use crate::error::Result;
use crate::history::Conversation;
use crate::history::message_texts::{PADDED_MESSAGE_SEPARATOR, push_separated};
use crate::search::literal::{
    Literal, LiteralCorpusEntry, build_agent_literal_corpus, build_literal_corpus,
    matches_all_literals,
};
use crate::search::query::ParsedQuery;
pub use crate::text_match::normalize_for_search;
use crate::time_filter::TimeFilter;
use chrono::{DateTime, Duration, Local, TimeZone};
use rayon::prelude::*;
use std::borrow::{Borrow, Cow};
use std::sync::Arc;

mod streamed;

/// Precomputed search data for a conversation
#[derive(Clone)]
pub struct SearchableConversation {
    /// The text Stage 1 rejects a session on when a query word is missing
    /// from it: the body, then the project name. Under a time window it holds
    /// the body alone. Shared, so a copy of the search data handed to a
    /// worker does not copy each session's text.
    pub text_lower: Arc<str>,
    /// Normalized custom_title only (small, typically <100 chars)
    pub title_lower: String,
    /// Normalized summary only (small, typically <500 chars)
    pub summary_lower: String,
    /// Normalized project_name only (small, typically <50 chars)
    pub project_lower: String,
    /// Original conversation index
    pub index: usize,
    /// The length of the body at the start of `text_lower`: all of it under
    /// a time window.
    body_len: usize,
}

impl SearchableConversation {
    /// The normalized body the body field scores against: `text_lower`
    /// without the project name.
    pub fn body_lower(&self) -> &str {
        &self.text_lower[..self.body_len]
    }
}

/// Check if a query looks like a UUID (e.g., e7d318b1-4274-4ee2-a341-e94893b5df49)
pub fn is_uuid(query: &str) -> bool {
    let q = query.trim();
    if q.len() != 36 {
        return false;
    }
    let parts: Vec<&str> = q.split('-').collect();
    if parts.len() != 5 {
        return false;
    }
    let expected_lens = [8, 4, 4, 4, 12];
    parts
        .iter()
        .zip(expected_lens.iter())
        .all(|(part, &len)| part.len() == len && part.chars().all(|c| c.is_ascii_hexdigit()))
}

/// True when `query` names the session stored as `stored`. Hex UUIDs match
/// whatever case each is written in; every other shape must match exactly,
/// because OpenCode builds ids from a base62 alphabet where `a` and `A` are
/// two ids.
pub fn session_id_matches(stored: &str, query: &str) -> bool {
    if is_uuid(stored) && is_uuid(query) {
        stored.eq_ignore_ascii_case(query)
    } else {
        stored == query
    }
}

/// The form to probe a file name, directory or index with: a UUID lowercased,
/// every other id as typed.
pub fn session_id_for_lookup(query: &str) -> Cow<'_, str> {
    if is_uuid(query) {
        Cow::Owned(query.to_ascii_lowercase())
    } else {
        Cow::Borrowed(query)
    }
}

/// Build searchable index from conversations using pre-normalized search text.
/// Only appends the (small) project name — the expensive full_text normalization
/// was already done during parsing/cache load.
pub fn precompute_search_text(conversations: &[Conversation]) -> Vec<SearchableConversation> {
    precompute_search_text_with(conversations, false)
}

/// `agent search`'s texts for each session, built for one time window. With
/// the window active, a session matches only when every query word is in the
/// text of its messages inside the window; its title, summary and project
/// name then add to its score.
pub struct AgentSearchTexts {
    searchable: Vec<SearchableConversation>,
    window: TimeFilter,
}

pub fn precompute_agent_search_text(
    conversations: &[Conversation],
    window: &TimeFilter,
) -> AgentSearchTexts {
    let searchable = if window.is_active() {
        conversations
            .par_iter()
            .enumerate()
            .map(|(index, conv)| searchable_conversation_within(conv, index, window))
            .collect()
    } else {
        precompute_search_text_with(conversations, true)
    };
    AgentSearchTexts {
        searchable,
        window: *window,
    }
}

fn searchable_conversation_within(
    conv: &Conversation,
    index: usize,
    window: &TimeFilter,
) -> SearchableConversation {
    let texts = conv.texts_within(window);
    let mut text_lower = normalize_for_search(&texts.full_text);
    for agent_text in [&texts.agent_search_text, &texts.thinking_and_tool_call_text] {
        push_separated(&mut text_lower, &normalize_for_search(agent_text));
    }
    let body_len = text_lower.len();
    SearchableConversation {
        text_lower: Arc::from(text_lower),
        title_lower: normalized_field(conv.custom_title.as_deref()),
        summary_lower: normalized_field(conv.summary.as_deref()),
        project_lower: normalized_field(conv.project_name.as_deref()),
        index,
        body_len,
    }
}

fn normalized_field(field: Option<&str>) -> String {
    field.map(normalize_for_search).unwrap_or_default()
}

fn precompute_search_text_with(
    conversations: &[Conversation],
    include_agent_text: bool,
) -> Vec<SearchableConversation> {
    conversations
        .par_iter()
        .enumerate()
        .map(|(idx, conv)| searchable_conversation_with(conv, idx, include_agent_text))
        .collect()
}

/// The search data of the conversation at `index`, as
/// [`precompute_search_text`] builds it for each conversation.
pub fn searchable_conversation(conv: &Conversation, index: usize) -> SearchableConversation {
    searchable_conversation_with(conv, index, false)
}

fn searchable_conversation_with(
    conv: &Conversation,
    index: usize,
    include_agent_text: bool,
) -> SearchableConversation {
    let title_lower = normalized_field(conv.custom_title.as_deref());
    let summary_lower = normalized_field(conv.summary.as_deref());
    let project_lower = normalized_field(conv.project_name.as_deref());
    let agent_texts = if include_agent_text {
        vec![
            normalize_for_search(&conv.agent_search_text),
            normalize_for_search(&conv.thinking_and_tool_call_text),
        ]
    } else {
        Vec::new()
    };

    // Stage 1 reads the body and the project name as one text; the body field
    // scores the part before the project name.
    let mut text_lower = String::with_capacity(
        conv.search_text_lower.len()
            + agent_texts
                .iter()
                .map(|text| text.len() + PADDED_MESSAGE_SEPARATOR.len())
                .sum::<usize>()
            + project_lower.len()
            + 1,
    );
    text_lower.push_str(&conv.search_text_lower);
    for agent_text in &agent_texts {
        push_separated(&mut text_lower, agent_text);
    }
    let body_len = text_lower.len();
    if !project_lower.is_empty() {
        text_lower.push(' ');
        text_lower.push_str(&project_lower);
    }

    SearchableConversation {
        text_lower: Arc::from(text_lower),
        title_lower,
        summary_lower,
        project_lower,
        index,
        body_len,
    }
}

/// Filter and score conversations based on query
/// Returns indices into the original conversations vec, sorted by score descending
#[cfg(test)]
pub fn search<C: Borrow<Conversation> + Sync>(
    conversations: &[C],
    searchable: &[SearchableConversation],
    query: &str,
    now: DateTime<Local>,
) -> Vec<usize> {
    try_search(conversations, searchable, query, now).expect("in-memory search")
}

pub fn try_search<C: Borrow<Conversation> + Sync>(
    conversations: &[C],
    searchable: &[SearchableConversation],
    query: &str,
    now: DateTime<Local>,
) -> Result<Vec<usize>> {
    search_with_surface(conversations, searchable, query, now, SearchSurface::List)
}

#[cfg(test)]
pub fn agent_search(
    conversations: &[Conversation],
    texts: &AgentSearchTexts,
    query: &str,
    now: DateTime<Local>,
) -> Vec<usize> {
    try_agent_search(conversations, texts, query, now).expect("in-memory search")
}

pub fn try_agent_search(
    conversations: &[Conversation],
    texts: &AgentSearchTexts,
    query: &str,
    now: DateTime<Local>,
) -> Result<Vec<usize>> {
    search_with_surface(
        conversations,
        &texts.searchable,
        query,
        now,
        SearchSurface::Agent(&texts.window),
    )
}

/// The texts a search matches.
#[derive(Clone, Copy)]
enum SearchSurface<'w> {
    /// The session list's: the conversation's text and its project name.
    List,
    /// `agent search`'s: also sub-agents' text, thinking and tool calls; with
    /// the window active, only the text of the messages inside it.
    Agent(&'w TimeFilter),
}

impl SearchSurface<'_> {
    fn literal_corpus<C: Borrow<Conversation> + Sync>(
        self,
        conversations: &[C],
    ) -> Vec<LiteralCorpusEntry> {
        match self {
            Self::List => build_literal_corpus(conversations),
            Self::Agent(window) => build_agent_literal_corpus(conversations, window),
        }
    }

    /// The time a session's freshness and ties read. With the window active,
    /// its last message inside the window, so activity after `--before`
    /// does not raise its rank.
    fn ranked_time(self, conversation: &Conversation) -> DateTime<Local> {
        match self {
            Self::Agent(window) if window.is_active() => conversation
                .message_times
                .last_second_within(window)
                .and_then(|second| Local.timestamp_opt(second, 0).single())
                .unwrap_or(conversation.timestamp),
            _ => conversation.timestamp,
        }
    }
}

fn search_with_surface<C: Borrow<Conversation> + Sync>(
    conversations: &[C],
    searchable: &[SearchableConversation],
    query: &str,
    now: DateTime<Local>,
    surface: SearchSurface<'_>,
) -> Result<Vec<usize>> {
    Ok(
        debug_search_with_surface(conversations, searchable, query, now, surface, |_| true)?
            .results
            .into_iter()
            .map(|(index, _)| index)
            .collect(),
    )
}

pub struct LexicalDebugSearch {
    pub parsed: ParsedQuery,
    pub results: Vec<(usize, ScoreDebug)>,
}

#[cfg(test)]
pub fn debug_search(
    conversations: &[Conversation],
    searchable: &[SearchableConversation],
    query: &str,
    now: DateTime<Local>,
    scope: impl Fn(usize) -> bool + Sync,
) -> LexicalDebugSearch {
    try_debug_search(conversations, searchable, query, now, scope).expect("in-memory search")
}

pub fn try_debug_search(
    conversations: &[Conversation],
    searchable: &[SearchableConversation],
    query: &str,
    now: DateTime<Local>,
    scope: impl Fn(usize) -> bool + Sync,
) -> Result<LexicalDebugSearch> {
    debug_search_with_surface(
        conversations,
        searchable,
        query,
        now,
        SearchSurface::List,
        scope,
    )
}

fn debug_search_with_surface<C: Borrow<Conversation> + Sync>(
    conversations: &[C],
    searchable: &[SearchableConversation],
    query: &str,
    now: DateTime<Local>,
    surface: SearchSurface<'_>,
    scope: impl Fn(usize) -> bool + Sync,
) -> Result<LexicalDebugSearch> {
    let parsed = ParsedQuery::parse(query);
    let results = search_debug_with_query(conversations, searchable, &parsed, now, surface, scope)?;
    Ok(LexicalDebugSearch { parsed, results })
}

fn exact_debug_results<C: Borrow<Conversation> + Sync>(
    conversations: &[C],
    literals: &[Literal],
    now: DateTime<Local>,
    surface: SearchSurface<'_>,
    scope: impl Fn(usize) -> bool + Sync,
) -> Result<Vec<(usize, ScoreDebug)>> {
    let corpus = surface.literal_corpus(conversations);
    let ranked_time = |index: usize| surface.ranked_time(conversations[index].borrow());
    let mut matching = conversations
        .par_iter()
        .enumerate()
        .map(|(index, conversation)| {
            if !scope(index) {
                return Ok(None);
            }
            let row = conversation.borrow();
            let matches = if row.cached_session.is_some() {
                streamed::collect(row, surface, &[], &[], literals)?.matches_literals()
            } else {
                matches_all_literals(&corpus[index].text, literals)
            };
            Ok(matches.then_some(index))
        })
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    matching.sort_unstable_by_key(|index| std::cmp::Reverse(ranked_time(*index)));
    Ok(matching
        .into_iter()
        .map(|index| (index, freshness_only(ranked_time(index), now)))
        .collect())
}

fn browse_debug_results<C: Borrow<Conversation>>(
    conversations: &[C],
    now: DateTime<Local>,
    surface: SearchSurface<'_>,
    scope: impl Fn(usize) -> bool + Sync,
) -> Vec<(usize, ScoreDebug)> {
    conversations
        .iter()
        .enumerate()
        .filter(|(index, _)| scope(*index))
        .map(|(index, conversation)| {
            let time = surface.ranked_time(conversation.borrow());
            (index, freshness_only(time, now))
        })
        .collect()
}

fn freshness_only(time: DateTime<Local>, now: DateTime<Local>) -> ScoreDebug {
    let fresh = freshness_bonus(time, now);
    ScoreDebug {
        total: fresh,
        freshness: fresh,
        fields: vec![],
    }
}

fn identifier_literals(query: &str) -> Vec<Literal> {
    query
        .split_whitespace()
        .filter(|term| term.contains('_'))
        .map(|term| Literal::new(term.to_string()))
        .collect()
}

fn normalized_query_words(query: &str) -> String {
    normalize_for_search(
        &query
            .split_whitespace()
            .filter(|term| !term.contains('_'))
            .collect::<Vec<_>>()
            .join(" "),
    )
}

fn search_debug_with_query<C: Borrow<Conversation> + Sync>(
    conversations: &[C],
    searchable: &[SearchableConversation],
    parsed: &ParsedQuery,
    now: DateTime<Local>,
    surface: SearchSurface<'_>,
    scope: impl Fn(usize) -> bool + Sync,
) -> Result<Vec<(usize, ScoreDebug)>> {
    let intent = parsed.lexical_text().trim();
    if parsed.is_effectively_empty() {
        return Ok(browse_debug_results(conversations, now, surface, scope));
    }

    if parsed.is_quoted_only() {
        return exact_debug_results(conversations, parsed.literals(), now, surface, scope);
    }

    let query_lower = normalized_query_words(intent);
    let query_words: Vec<&str> = query_lower.split_whitespace().collect();
    let identifier_literals = identifier_literals(intent);
    let literal_filters = parsed
        .literals()
        .iter()
        .cloned()
        .chain(identifier_literals)
        .collect::<Vec<_>>();
    if query_words.is_empty() {
        if literal_filters.is_empty() {
            return Ok(browse_debug_results(conversations, now, surface, scope));
        }
        return exact_debug_results(conversations, &literal_filters, now, surface, scope);
    }

    let corpus = (!literal_filters.is_empty()).then(|| surface.literal_corpus(conversations));

    let adjacent_pairs: Vec<String> = if query_words.len() > 1 {
        query_words
            .windows(2)
            .map(|w| format!("{} {}", w[0], w[1]))
            .collect()
    } else {
        vec![]
    };

    let mut scored: Vec<(usize, ScoreDebug, DateTime<Local>)> = searchable
        .par_iter()
        .map(|s| -> Result<_> {
            if !scope(s.index) {
                return Ok(None);
            }
            let row = conversations[s.index].borrow();
            let time = surface.ranked_time(row);
            if row.cached_session.is_some() {
                let counts = streamed::collect(
                    row,
                    surface,
                    &query_words,
                    &adjacent_pairs,
                    &literal_filters,
                )?;
                let include_project =
                    !matches!(surface, SearchSurface::Agent(window) if window.is_active());
                return Ok(streamed::score(
                    s,
                    counts,
                    &query_words,
                    &adjacent_pairs,
                    time,
                    now,
                    include_project,
                )
                .map(|debug| (s.index, debug, time)));
            }
            if corpus.as_ref().is_some_and(|corpus| {
                !matches_all_literals(&corpus[s.index].text, &literal_filters)
            }) {
                return Ok(None);
            }
            let time = surface.ranked_time(conversations[s.index].borrow());
            Ok(
                score_text_debug(s, s.body_lower(), &query_words, &adjacent_pairs, time, now)
                    .map(|debug| (s.index, debug, time)),
            )
        })
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .flatten()
        .collect();

    scored.sort_unstable_by(|a, b| {
        b.1.total
            .partial_cmp(&a.1.total)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.2.cmp(&a.2))
    });

    Ok(scored
        .into_iter()
        .map(|(idx, debug, _)| (idx, debug))
        .collect())
}

/// Field weights for scoring
const WEIGHT_TITLE: f64 = 5.0;
const WEIGHT_SUMMARY: f64 = 3.0;
const WEIGHT_PROJECT: f64 = 4.0;
const WEIGHT_BODY: f64 = 1.0;

/// Debug breakdown of a search score
pub struct ScoreDebug {
    pub total: f64,
    pub freshness: f64,
    pub fields: Vec<FieldDebug>,
}

pub struct FieldDebug {
    pub name: &'static str,
    pub weight: f64,
    pub tf_score: f64,
    pub adjacency_score: f64,
    /// Per query-word: (word, tf_count, ln_score)
    pub word_details: Vec<(String, usize, f64)>,
}

/// Core scoring implementation used by both score_text and score_text_debug.
///
/// Stage 1: Fast rejection using combined text (AND logic, prefix matching).
/// Stage 2: Per-field scoring with log-saturated TF, adjacency bonuses, field weights.
/// Returns None if Stage 1 rejects the conversation.
fn score_impl(
    s: &SearchableConversation,
    body_lower: &str,
    query_words: &[&str],
    adjacent_pairs: &[String],
    timestamp: DateTime<Local>,
    now: DateTime<Local>,
) -> Option<ScoreDebug> {
    if query_words.is_empty() {
        return None;
    }

    // Stage 1: Fast rejection — all query words must exist as substrings
    for &qw in query_words {
        if !s.text_lower.contains(qw) {
            return None;
        }
    }

    // Stage 1: Prefix matching on combined text (AND logic).
    // If any word has 0 prefix matches in text_lower, reject.
    for &qw in query_words {
        if count_prefix_matches(&s.text_lower, qw, 1) == 0 {
            // CJK fallback: substring matching is acceptable for CJK text
            let has_cjk = query_words
                .iter()
                .any(|w| w.chars().any(|c| ('\u{4E00}'..='\u{9FFF}').contains(&c)));
            if has_cjk {
                let fresh = freshness_bonus(timestamp, now);
                let flat = (query_words.len() as f64) * 0.5;
                return Some(ScoreDebug {
                    total: flat + fresh,
                    freshness: fresh,
                    fields: vec![],
                });
            }
            return None;
        }
    }

    // Stage 2: Field-aware scoring
    let fields: &[(&str, f64, &'static str)] = &[
        (&s.title_lower, WEIGHT_TITLE, "title"),
        (&s.summary_lower, WEIGHT_SUMMARY, "summary"),
        (&s.project_lower, WEIGHT_PROJECT, "project"),
        (body_lower, WEIGHT_BODY, "body"),
    ];

    let mut base_score = 0.0;
    let mut field_debugs = Vec::new();

    for &(field, weight, name) in fields {
        if field.is_empty() {
            continue;
        }

        // Per-word log-saturated TF
        let mut field_tf_score = 0.0;
        let mut word_details = Vec::new();
        for &qw in query_words {
            let tf = count_prefix_matches(field, qw, 10); // cap at 10
            let ln_score = if tf > 0 { ((1 + tf) as f64).ln() } else { 0.0 };
            field_tf_score += ln_score;
            word_details.push((qw.to_string(), tf, ln_score));
        }
        let weighted_tf = weight * field_tf_score;
        base_score += weighted_tf;

        // Adjacency bonus using precomputed pairs
        let adj_count = if !adjacent_pairs.is_empty() {
            count_adjacent_pairs(field, adjacent_pairs, 3)
        } else {
            0
        };
        let weighted_adj = weight * 2.0 * adj_count as f64;
        base_score += weighted_adj;

        field_debugs.push(FieldDebug {
            name,
            weight,
            tf_score: weighted_tf,
            adjacency_score: weighted_adj,
            word_details,
        });
    }

    let fresh = freshness_bonus(timestamp, now);

    Some(ScoreDebug {
        total: base_score + fresh,
        freshness: fresh,
        fields: field_debugs,
    })
}

/// Score with full debug breakdown. Returns None if Stage 1 rejects.
pub fn score_text_debug(
    s: &SearchableConversation,
    body_lower: &str,
    query_words: &[&str],
    adjacent_pairs: &[String],
    timestamp: DateTime<Local>,
    now: DateTime<Local>,
) -> Option<ScoreDebug> {
    score_impl(s, body_lower, query_words, adjacent_pairs, timestamp, now)
}

/// Returns true if `pos` in `text` is at the start of a word (i.e. preceded by
/// a non-alphanumeric character or is the start of the string). This treats
/// markdown punctuation (`*`, `(`, `:`, `.`, etc.) as word boundaries, so a
/// phrase like `**media pipeline**` is matched the same as `media pipeline`.
fn is_word_start(text: &str, pos: usize) -> bool {
    pos == 0
        || text[..pos]
            .chars()
            .next_back()
            .is_some_and(|c| !c.is_alphanumeric())
}

/// Count prefix matches of `word` in `text`, up to `max_count`.
fn count_prefix_matches(text: &str, word: &str, max_count: usize) -> usize {
    let mut start = 0;
    let mut count = 0;
    while let Some(pos) = text[start..].find(word) {
        let actual_pos = start + pos;
        if is_word_start(text, actual_pos) {
            count += 1;
            if count >= max_count {
                break;
            }
        }
        start = actual_pos + word.len().max(1);
    }
    count
}

/// Count how many precomputed adjacent pairs appear in text.
/// Returns count capped at `max_count`.
fn count_adjacent_pairs(text: &str, adjacent_pairs: &[String], max_count: usize) -> usize {
    let mut count = 0;
    for combined in adjacent_pairs {
        let mut start = 0;
        while let Some(pos) = text[start..].find(combined.as_str()) {
            let actual_pos = start + pos;
            if is_word_start(text, actual_pos) {
                count += 1;
                if count >= max_count {
                    return count;
                }
            }
            start = actual_pos + combined.len().max(1);
        }
    }
    count
}

/// Additive freshness bonus with continuous exponential decay.
/// Max bonus: 2.0 (brand new), half-life: 7 days.
fn freshness_bonus(timestamp: DateTime<Local>, now: DateTime<Local>) -> f64 {
    let age = now.signed_duration_since(timestamp);
    if age < Duration::zero() {
        return 2.0; // future timestamp edge case
    }
    let age_days = age.num_seconds() as f64 / 86_400.0;
    2.0 * 2_f64.powf(-age_days / 7.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::test_fixtures::{
        october, one_message_conversation, timed_messages_conversation, window_on_october_10,
        with_agent_texts,
    };

    /// Create a test conversation with optional metadata.
    /// Rebuilds search_text_lower to match production behavior:
    /// custom_title + summary are prepended to body text before normalization.
    fn make_conv_full(
        text: &str,
        project: Option<&str>,
        title: Option<&str>,
        summary: Option<&str>,
        timestamp: DateTime<Local>,
    ) -> Conversation {
        one_message_conversation(text, timestamp, summary, title, project)
    }

    fn make_conv(text: &str, timestamp: DateTime<Local>) -> Conversation {
        make_conv_full(text, None, None, None, timestamp)
    }

    fn make_conv_with_project(
        text: &str,
        project: &str,
        timestamp: DateTime<Local>,
    ) -> Conversation {
        make_conv_full(text, Some(project), None, None, timestamp)
    }

    #[test]
    fn empty_quoted_query_browses_results() {
        let now = Local::now();
        let convs = vec![
            make_conv("older text", now),
            make_conv("newer text", now - Duration::days(1)),
        ];
        let searchable = precompute_search_text(&convs);

        let results = search(&convs, &searchable, "\"\"", now);

        assert_eq!(results, vec![0, 1]);
    }

    #[test]
    fn quoted_identifier_matches_exact_literal_only() {
        let now = Local::now();
        let convs = vec![
            make_conv("literal RESTAURANT_SIGNALS identifier", now),
            make_conv("restaurant signals normalized words only", now),
        ];
        let searchable = precompute_search_text(&convs);

        let results = search(&convs, &searchable, "\"RESTAURANT_SIGNALS\"", now);

        assert_eq!(results, vec![0]);
    }

    #[test]
    fn mixed_query_scores_unquoted_intent_and_requires_literal() {
        let now = Local::now();
        let convs = vec![
            make_conv_full(
                "alpha alpha alpha RESTAURANT_SIGNALS",
                None,
                None,
                None,
                now - Duration::hours(2),
            ),
            make_conv_full("alpha alpha alpha alpha alpha", None, None, None, now),
            make_conv_full("beta RESTAURANT_SIGNALS", None, None, None, now),
        ];
        let searchable = precompute_search_text(&convs);

        let results = search(&convs, &searchable, "alpha \"RESTAURANT_SIGNALS\"", now);

        assert_eq!(results, vec![0]);
    }

    #[test]
    fn quoted_only_query_uses_exact_fallback_newest_first() {
        let now = Local::now();
        let convs = vec![
            make_conv_full("needle_literal", None, None, None, now - Duration::days(1)),
            make_conv_full("needle_literal", None, None, None, now),
            make_conv_full("needle literal", None, None, None, now - Duration::hours(1)),
        ];
        let searchable = precompute_search_text(&convs);

        let results = search(&convs, &searchable, "\"needle_literal\"", now);

        assert_eq!(results, vec![1, 0]);
    }

    #[test]
    fn quoted_phrase_with_run_of_spaces_matches_and_highlights_in_list() {
        let now = Local::now();
        let convs = vec![
            make_conv("deploy, then retry backoff window", now),
            make_conv("retrybackoff written without a space", now),
        ];
        let searchable = precompute_search_text(&convs);
        let query = "\"retry   backoff\"";

        assert_eq!(search(&convs, &searchable, query, now), vec![0]);
        let preview = "retry\t\tbackoff";
        assert_eq!(
            crate::search::preview::HighlightQuery::parse(query).match_ranges(preview),
            vec![(0, preview.len())]
        );
    }

    #[test]
    fn quoted_literals_use_smart_case_in_lexical_search() {
        let now = Local::now();
        let convs = vec![
            make_conv("RESTAURANT_SIGNALS uppercase", now - Duration::hours(1)),
            make_conv("restaurant_signals lowercase", now),
        ];
        let searchable = precompute_search_text(&convs);

        let insensitive = search(&convs, &searchable, "\"restaurant_signals\"", now);
        let sensitive = search(&convs, &searchable, "\"RESTAURANT_SIGNALS\"", now);

        assert_eq!(insensitive, vec![1, 0]);
        assert_eq!(sensitive, vec![0]);
    }

    #[test]
    fn unquoted_identifier_requires_exact_underscore_match() {
        let now = Local::now();
        let convs = vec![
            make_conv("restaurant signals normalized words only", now),
            make_conv("restaurant_signals identifier", now - Duration::hours(1)),
            make_conv(
                "restaurant API_SIGNALS mixed identifier",
                now - Duration::hours(2),
            ),
        ];
        let searchable = precompute_search_text(&convs);

        let results = search(&convs, &searchable, "restaurant_signals", now);

        assert_eq!(results, vec![1]);
    }

    #[test]
    fn debug_search_enforces_literal_filters() {
        let now = Local::now();
        let convs = vec![
            make_conv("alpha RESTAURANT_SIGNALS", now),
            make_conv("alpha restaurant signals", now),
        ];
        let searchable = precompute_search_text(&convs);

        let debug = debug_search(
            &convs,
            &searchable,
            "alpha \"RESTAURANT_SIGNALS\"",
            now,
            |_| true,
        );

        assert_eq!(
            debug
                .results
                .iter()
                .map(|(idx, _)| *idx)
                .collect::<Vec<_>>(),
            vec![0]
        );
        assert_eq!(debug.parsed.unquoted(), "alpha");
        assert_eq!(debug.parsed.literals()[0].text(), "RESTAURANT_SIGNALS");
    }

    #[test]
    fn search_matches_underscore_separated() {
        let now = Local::now();
        let convs = vec![make_conv("HARDENED_RUNTIME config", now)];
        let searchable = precompute_search_text(&convs);
        let results = search(&convs, &searchable, "harden runtime", now);
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn search_matches_different_case() {
        let now = Local::now();
        let convs = vec![make_conv("Hardened Runtime enabled", now)];
        let searchable = precompute_search_text(&convs);
        let results = search(&convs, &searchable, "harden runtime", now);
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn search_prefix_matches_words() {
        let now = Local::now();
        let convs = vec![make_conv("hardened security", now)];
        let searchable = precompute_search_text(&convs);
        let results = search(&convs, &searchable, "harden", now);
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn search_requires_all_words() {
        let now = Local::now();
        let convs = vec![make_conv("hardened security", now)];
        let searchable = precompute_search_text(&convs);
        let results = search(&convs, &searchable, "harden runtime", now);
        assert_eq!(results.len(), 0); // "runtime" not present
    }

    #[test]
    fn search_with_underscore_in_query_requires_whole_identifier() {
        let now = Local::now();
        let convs = vec![
            make_conv("hardened runtime enabled", now),
            make_conv("hardened_runtime enabled", now - Duration::hours(1)),
            make_conv("hardened other_runtime enabled", now - Duration::hours(2)),
        ];
        let searchable = precompute_search_text(&convs);

        let results = search(&convs, &searchable, "hardened_runtime", now);

        assert_eq!(results, vec![1]);
    }

    #[test]
    fn freshness_decays_over_time() {
        let now = Local::now();
        let fresh = freshness_bonus(now - Duration::hours(1), now);
        let week_old = freshness_bonus(now - Duration::days(7), now);
        let month_old = freshness_bonus(now - Duration::days(30), now);
        assert!(fresh > week_old, "fresh should score higher than week-old");
        assert!(
            week_old > month_old,
            "week-old should score higher than month-old"
        );
        assert!(fresh <= 2.0, "freshness bonus should not exceed 2.0");
        assert!(
            month_old > 0.0,
            "old conversations should still get some bonus"
        );
    }

    #[test]
    fn future_timestamp_gets_max_freshness() {
        let now = Local::now();
        let timestamp = now + Duration::hours(1);
        assert_eq!(freshness_bonus(timestamp, now), 2.0);
    }

    #[test]
    fn continuous_freshness_no_cliff() {
        let now = Local::now();
        let score_23h = freshness_bonus(now - Duration::hours(23), now);
        let score_25h = freshness_bonus(now - Duration::hours(25), now);
        let diff = (score_23h - score_25h).abs();
        assert!(
            diff < 0.1,
            "no dramatic cliff at 24h boundary: 23h={:.3} 25h={:.3}",
            score_23h,
            score_25h
        );
    }

    #[test]
    fn search_matches_project_name() {
        let now = Local::now();
        let convs = vec![make_conv_with_project(
            "some conversation",
            "workmux/main-worktree-fix",
            now,
        )];
        let searchable = precompute_search_text(&convs);

        // Match worktree name
        let results = search(&convs, &searchable, "main-worktree-fix", now);
        assert_eq!(results.len(), 1);

        // Match with project prefix
        let results = search(&convs, &searchable, "workmux", now);
        assert_eq!(results.len(), 1);

        // Match project/worktree combined
        let results = search(&convs, &searchable, "workmux main worktree", now);
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn project_name_is_lexical_search_metadata_only() {
        let now = Local::now();
        let convs = vec![make_conv_full(
            "visible dialogue sentinel",
            Some("project lexical sentinel"),
            None,
            None,
            now,
        )];

        let searchable = precompute_search_text(&convs);

        assert!(
            searchable[0]
                .text_lower
                .contains("project lexical sentinel")
        );
        assert_eq!(searchable[0].project_lower, "project lexical sentinel");
        assert!(search(&convs, &searchable, "project lexical", now).contains(&0));
        assert!(
            !convs[0]
                .semantic_turns
                .join(" ")
                .contains("project lexical sentinel")
        );
    }

    #[test]
    fn body_field_holds_agent_text_but_not_project_name() {
        let mut conv =
            one_message_conversation("visible dialogue", Local::now(), None, None, Some("proj"));
        conv.agent_search_text = "Sub-Agent text".to_string();

        let searchable = precompute_agent_search_text(&[conv], &TimeFilter::default())
            .searchable
            .remove(0);

        assert_eq!(
            searchable.body_lower(),
            "visible dialogue \u{1e} sub agent text"
        );
        assert_eq!(
            &*searchable.text_lower,
            "visible dialogue \u{1e} sub agent text proj"
        );
    }

    #[test]
    fn agent_search_text_includes_progress_without_polluting_full_text() {
        let now = Local::now();
        let mut conv = make_conv("visible dialogue", now);
        conv.agent_search_text = "subagent_progress_needle".to_string();
        let convs = vec![conv];
        let normal_searchable = precompute_search_text(&convs);
        let agent_searchable = precompute_agent_search_text(&convs, &TimeFilter::default());

        assert!(!convs[0].full_text.contains("subagent_progress_needle"));
        assert_eq!(
            search(&convs, &normal_searchable, "subagent_progress_needle", now),
            Vec::<usize>::new()
        );
        assert_eq!(
            agent_search(&convs, &agent_searchable, "subagent_progress_needle", now),
            vec![0]
        );
        assert_eq!(
            agent_search(
                &convs,
                &agent_searchable,
                "\"subagent_progress_needle\"",
                now,
            ),
            vec![0]
        );
    }

    #[test]
    fn agent_search_finds_session_matching_only_in_thinking() {
        let now = Local::now();
        let mut conv = make_conv("visible dialogue", now);
        conv.thinking_and_tool_call_text = "weighing the stale index".to_string();
        let convs = vec![conv];
        let normal_searchable = precompute_search_text(&convs);
        let agent_searchable = precompute_agent_search_text(&convs, &TimeFilter::default());

        assert_eq!(
            search(&convs, &normal_searchable, "stale", now),
            Vec::<usize>::new()
        );
        assert_eq!(
            agent_search(&convs, &agent_searchable, "stale", now),
            vec![0]
        );
        assert_eq!(
            agent_search(&convs, &agent_searchable, "\"stale index\"", now),
            vec![0]
        );
    }

    #[test]
    fn agent_search_finds_session_matching_only_in_tool_name() {
        let now = Local::now();
        let mut conv = make_conv("visible dialogue", now);
        conv.thinking_and_tool_call_text = "WebFetch: https://example.com".to_string();
        let convs = vec![conv];
        let normal_searchable = precompute_search_text(&convs);
        let agent_searchable = precompute_agent_search_text(&convs, &TimeFilter::default());

        assert_eq!(
            search(&convs, &normal_searchable, "webfetch", now),
            Vec::<usize>::new()
        );
        assert_eq!(
            agent_search(&convs, &agent_searchable, "webfetch", now),
            vec![0]
        );
        assert_eq!(
            agent_search(&convs, &agent_searchable, "\"WebFetch\"", now),
            vec![0]
        );
    }

    /// `convs` in `agent search`'s order for `query` within `window`.
    fn agent_ranking(convs: &[Conversation], query: &str, window: &TimeFilter) -> Vec<usize> {
        let texts = precompute_agent_search_text(convs, window);
        agent_search(convs, &texts, query, october(10, 23))
    }

    #[test]
    fn strong_match_inside_window_ranks_above_old_matches_with_weak_one_inside() {
        let mut old_matches = vec![("the flaky test failed again", october(1, 9)); 10];
        old_matches.push(("one flaky test", october(10, 12)));
        let convs = [
            timed_messages_conversation(&old_matches, None),
            timed_messages_conversation(
                &[
                    ("lunch plans", october(1, 9)),
                    ("the flaky test failed", october(10, 11)),
                    ("the flaky test failed on retry", october(10, 11)),
                    ("the flaky test is fixed", october(10, 12)),
                ],
                None,
            ),
        ];

        assert_eq!(
            agent_ranking(&convs, "flaky test", &TimeFilter::default()),
            [0, 1]
        );
        assert_eq!(
            agent_ranking(&convs, "flaky test", &window_on_october_10()),
            [1, 0]
        );
    }

    #[test]
    fn session_matching_only_outside_window_isnt_ranked_within_it() {
        let convs = [timed_messages_conversation(
            &[
                ("the flaky test failed", october(1, 9)),
                ("lunch plans", october(10, 12)),
            ],
            None,
        )];

        assert_eq!(
            agent_ranking(&convs, "flaky test", &TimeFilter::default()),
            [0]
        );
        assert_eq!(
            agent_ranking(&convs, "flaky test", &window_on_october_10()),
            Vec::<usize>::new()
        );
        assert_eq!(
            agent_ranking(&convs, "\"flaky test\"", &window_on_october_10()),
            Vec::<usize>::new()
        );
    }

    #[test]
    fn quoted_text_inside_window_matches_within_it() {
        let convs = [timed_messages_conversation(
            &[
                ("lunch plans", october(1, 9)),
                ("the flaky test failed", october(10, 12)),
            ],
            None,
        )];

        assert_eq!(
            agent_ranking(&convs, "\"flaky test\"", &window_on_october_10()),
            [0]
        );
    }

    /// A session with one message before the window and one inside it,
    /// neither holding the query word in its `full_text`.
    fn session_across_the_window_start() -> Conversation {
        timed_messages_conversation(
            &[
                ("lunch plans", october(1, 9)),
                ("checking the logs", october(10, 12)),
            ],
            None,
        )
    }

    #[test]
    fn tool_call_inside_window_ranks_its_session() {
        let convs = [
            with_agent_texts(
                session_across_the_window_start(),
                &[],
                &["", "Bash: cargo test --flaky"],
            ),
            with_agent_texts(
                session_across_the_window_start(),
                &[],
                &["Bash: cargo test --flaky", ""],
            ),
        ];

        assert_eq!(
            agent_ranking(&convs, "flaky", &TimeFilter::default()).len(),
            2
        );
        assert_eq!(agent_ranking(&convs, "flaky", &window_on_october_10()), [0]);
    }

    #[test]
    fn subagent_message_inside_window_ranks_its_session() {
        let convs = [
            with_agent_texts(
                session_across_the_window_start(),
                &["", "the flaky test failed"],
                &[],
            ),
            with_agent_texts(
                session_across_the_window_start(),
                &["the flaky test failed", ""],
                &[],
            ),
        ];

        assert_eq!(
            agent_ranking(&convs, "flaky", &TimeFilter::default()).len(),
            2
        );
        assert_eq!(agent_ranking(&convs, "flaky", &window_on_october_10()), [0]);
    }

    #[test]
    fn session_list_doesnt_match_quoted_text_across_two_messages() {
        let convs = [timed_messages_conversation(
            &[
                ("the build was flaky", october(10, 11)),
                ("test runs passed", october(10, 12)),
            ],
            None,
        )];
        let searchable = precompute_search_text(&convs);
        let list_search = |query| search(&convs, &searchable, query, october(10, 23));

        assert_eq!(list_search("flaky test"), [0]);
        assert!(list_search("\"flaky test\"").is_empty());
    }

    #[test]
    fn title_adds_to_match_inside_window_but_doesnt_match_alone() {
        let convs = [
            timed_messages_conversation(
                &[("lunch plans", october(10, 12))],
                Some("flaky test tracker"),
            ),
            timed_messages_conversation(&[("the flaky test failed", october(10, 12))], None),
            timed_messages_conversation(
                &[("the flaky test failed", october(10, 12))],
                Some("flaky test tracker"),
            ),
        ];

        assert_eq!(
            agent_ranking(&convs, "flaky test", &window_on_october_10()),
            [2, 1]
        );
        assert_eq!(
            agent_ranking(&convs, "flaky test", &TimeFilter::default()),
            [2, 0, 1]
        );
    }

    #[test]
    fn session_active_after_window_ranks_by_its_last_message_inside_it() {
        let before_october_1_at_23 = TimeFilter {
            after: None,
            before: Some(october(1, 23)),
        };
        let convs = [
            timed_messages_conversation(
                &[
                    ("one flaky test", october(1, 9)),
                    ("lunch plans", october(10, 22)),
                ],
                None,
            ),
            timed_messages_conversation(
                &[
                    ("the flaky test failed", october(1, 8)),
                    ("flaky again", october(1, 10)),
                ],
                None,
            ),
        ];

        assert_eq!(
            agent_ranking(&convs, "flaky test", &TimeFilter::default()),
            [0, 1]
        );
        assert_eq!(
            agent_ranking(&convs, "flaky test", &before_october_1_at_23),
            [1, 0]
        );
        assert_eq!(
            agent_ranking(&convs, "\"flaky test\"", &before_october_1_at_23),
            [1, 0]
        );
    }

    #[test]
    fn two_words_in_two_search_texts_dont_get_phrase_bonus() {
        let conv = with_agent_texts(
            timed_messages_conversation(&[("the build was flaky", october(10, 12))], None),
            &["test runs passed"],
            &[],
        );

        for window in [TimeFilter::default(), window_on_october_10()] {
            let texts = precompute_agent_search_text(std::slice::from_ref(&conv), &window);
            let searchable = &texts.searchable[0];
            let debug = score_text_debug(
                searchable,
                searchable.body_lower(),
                &["flaky", "test"],
                &["flaky test".to_owned()],
                october(10, 12),
                october(10, 23),
            )
            .expect("both words are in the body");
            let body = debug.fields.iter().find(|field| field.name == "body");

            assert_eq!(body.map(|field| field.adjacency_score), Some(0.0));
        }
    }

    #[test]
    fn search_matches_hyphenated_words() {
        let now = Local::now();
        let convs = vec![make_conv("main-worktree-fix discussion", now)];
        let searchable = precompute_search_text(&convs);
        let results = search(&convs, &searchable, "worktree fix", now);
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn is_uuid_valid() {
        assert!(is_uuid("e7d318b1-4274-4ee2-a341-e94893b5df49"));
        assert!(is_uuid("00000000-0000-0000-0000-000000000000"));
        assert!(is_uuid("ABCDEF01-2345-6789-abcd-ef0123456789"));
    }

    #[test]
    fn is_uuid_invalid() {
        assert!(!is_uuid(""));
        assert!(!is_uuid("not-a-uuid"));
        assert!(!is_uuid("e7d318b1-4274-4ee2-a341")); // too short
        assert!(!is_uuid("e7d318b1-4274-4ee2-a341-e94893b5df49x")); // too long
        assert!(!is_uuid("e7d318b14274-4ee2-a341-e94893b5df49-")); // wrong grouping
        assert!(!is_uuid("g7d318b1-4274-4ee2-a341-e94893b5df49")); // non-hex char
    }

    #[test]
    fn is_uuid_with_whitespace() {
        assert!(is_uuid("  e7d318b1-4274-4ee2-a341-e94893b5df49  "));
    }

    #[test]
    fn uuid_matches_its_session_whatever_case_it_is_typed_in() {
        assert!(session_id_matches(
            "e7d318b1-4274-4ee2-a341-e94893b5df49",
            "E7D318B1-4274-4EE2-A341-E94893B5DF49"
        ));
        assert!(!session_id_matches(
            "e7d318b1-4274-4ee2-a341-e94893b5df49",
            "e7d318b1-4274-4ee2-a341-e94893b5df48"
        ));
    }

    /// OpenCode ids are base62, so a flipped letter is another id; a Pi id
    /// the user chose is whatever they typed.
    #[test]
    fn id_that_is_not_uuid_matches_only_itself() {
        assert!(session_id_matches(
            "ses_fdf25a01fffe5Xd4qwaJHrqnZy",
            "ses_fdf25a01fffe5Xd4qwaJHrqnZy"
        ));
        assert!(!session_id_matches(
            "ses_fdf25a01fffe5Xd4qwaJHrqnZy",
            "ses_fdf25a01fffe5xd4qwajhrqnzy"
        ));
        assert!(session_id_matches("custom_v1_id", "custom_v1_id"));
        assert!(!session_id_matches("custom_v1_id", "CUSTOM_V1_ID"));
    }

    #[test]
    fn uuid_is_looked_up_lowercased_and_any_other_id_as_typed() {
        assert_eq!(
            session_id_for_lookup("E7D318B1-4274-4EE2-A341-E94893B5DF49"),
            "e7d318b1-4274-4ee2-a341-e94893b5df49"
        );
        assert_eq!(
            session_id_for_lookup("ses_fdf25a01fffe5Xd4qwaJHrqnZy"),
            "ses_fdf25a01fffe5Xd4qwaJHrqnZy"
        );
    }

    #[test]
    fn search_matches_chinese_text_with_punctuation() {
        let now = Local::now();
        let convs = vec![make_conv(
            "\u{9000}\u{51FA}\u{7801} 143 \u{5C31}\u{662F} SIGTERM\u{FF0C}\u{5C5E}\u{4E8E}\u{9884}\u{671F}\u{884C}\u{4E3A}\u{3002}\u{5F53}\u{524D}\u{65B0}\u{8FDB}",
            now,
        )];
        let searchable = precompute_search_text(&convs);

        // Should match Chinese text across CJK punctuation boundaries
        let results = search(&convs, &searchable, "\u{5C5E}\u{4E8E}\u{9884}\u{671F}", now);
        assert_eq!(results.len(), 1);

        // Should match text before punctuation
        let results = search(&convs, &searchable, "\u{9000}\u{51FA}\u{7801}", now);
        assert_eq!(results.len(), 1);

        // Should match mixed Chinese and English
        let results = search(&convs, &searchable, "SIGTERM \u{9884}\u{671F}", now);
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn search_matches_chinese_substring_within_token() {
        let now = Local::now();
        let convs = vec![make_conv(
            "\u{8FD9}\u{662F}\u{4E00}\u{4E2A}\u{6D4B}\u{8BD5}\u{4F1A}\u{8BDD}\u{5185}\u{5BB9}",
            now,
        )];
        let searchable = precompute_search_text(&convs);

        // Should find substring even without word boundaries
        let results = search(&convs, &searchable, "\u{6D4B}\u{8BD5}\u{4F1A}\u{8BDD}", now);
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn cjk_punctuation_treated_as_separator() {
        assert_eq!(
            normalize_for_search("SIGTERM\u{FF0C}\u{5C5E}\u{4E8E}\u{9884}\u{671F}"),
            "sigterm \u{5C5E}\u{4E8E}\u{9884}\u{671F}"
        );
        assert_eq!(
            normalize_for_search("\u{884C}\u{4E3A}\u{3002}\u{5F53}\u{524D}"),
            "\u{884C}\u{4E3A} \u{5F53}\u{524D}"
        );
    }

    #[test]
    fn exact_project_match_beats_recent_body_mention() {
        let now = Local::now();
        let old_exact = make_conv_full(
            "discussion about agents config",
            Some("workmux/agents-config"),
            None,
            None,
            now - Duration::hours(22),
        );
        let new_incidental = make_conv_full(
            "updated agents and changed config files",
            Some("workmux/other-project"),
            None,
            None,
            now - Duration::hours(1),
        );
        let convs = vec![old_exact, new_incidental];
        let searchable = precompute_search_text(&convs);
        let results = search(&convs, &searchable, "agents-config", now);
        assert_eq!(results[0], 0, "exact project match should rank first");
    }

    #[test]
    fn title_match_beats_body_only() {
        let now = Local::now();
        let with_title = make_conv_full(
            "some body text about agents and config",
            None,
            Some("agents config setup"),
            None,
            now,
        );
        let body_only = make_conv_full(
            "discussed agents and config in detail agents config agents",
            None,
            None,
            None,
            now,
        );
        let convs = vec![with_title, body_only];
        let searchable = precompute_search_text(&convs);
        let results = search(&convs, &searchable, "agents config", now);
        assert_eq!(results[0], 0, "title match should rank higher");
    }

    #[test]
    fn repeated_term_beats_single_mention() {
        let now = Local::now();
        let repeated = make_conv_full(
            "config config config setup config again",
            None,
            None,
            None,
            now,
        );
        let single = make_conv_full("config was mentioned once here", None, None, None, now);
        let convs = vec![repeated, single];
        let searchable = precompute_search_text(&convs);
        let results = search(&convs, &searchable, "config", now);
        assert_eq!(results[0], 0, "repeated mentions should score higher");
    }

    #[test]
    fn adjacent_terms_beat_separated() {
        let now = Local::now();
        let adjacent = make_conv_full("the agents config is important", None, None, None, now);
        let separated = make_conv_full(
            "the agents did something and later we changed config",
            None,
            None,
            None,
            now,
        );
        let convs = vec![adjacent, separated];
        let searchable = precompute_search_text(&convs);
        let results = search(&convs, &searchable, "agents config", now);
        assert_eq!(results[0], 0, "adjacent terms should score higher");
    }

    #[test]
    fn adjacency_detected_inside_markdown_bold() {
        // Markdown punctuation (`*`, parens, dots) should be treated as a
        // word boundary for both prefix matching and adjacency detection,
        // so `**media pipeline**` scores the same as `media pipeline`.
        let now = Local::now();
        let bolded = make_conv_full("the **media pipeline** is the gap.", None, None, None, now);
        let plain_separated = make_conv_full(
            "media is mentioned. then later pipeline is mentioned separately.",
            None,
            None,
            None,
            now,
        );
        let convs = vec![bolded, plain_separated];
        let searchable = precompute_search_text(&convs);
        let results = search(&convs, &searchable, "media pipeline", now);
        assert_eq!(
            results[0], 0,
            "adjacency in **media pipeline** must beat distant terms"
        );
    }

    #[test]
    fn prefix_match_after_markdown_punctuation() {
        // Word starting after `*`, `(`, `:` etc. should still count toward
        // prefix matches — they're word boundaries, not just whitespace.
        let now = Local::now();
        let convs = vec![make_conv("look at *media* and (pipeline) and `media`", now)];
        let searchable = precompute_search_text(&convs);
        let results = search(&convs, &searchable, "media pipeline", now);
        assert_eq!(
            results.len(),
            1,
            "media and pipeline after punctuation must match"
        );
    }

    #[test]
    fn freshness_does_not_overpower_relevance() {
        let now = Local::now();
        // Old but highly relevant (project name match + body mentions)
        let old_relevant = make_conv_full(
            "agents config agents config agents config",
            Some("workmux/agents-config"),
            Some("agents config"),
            None,
            now - Duration::days(7),
        );
        // Brand new but barely relevant (single mention in body only)
        let new_weak = make_conv_full(
            "something about config in passing",
            Some("workmux/unrelated"),
            None,
            None,
            now - Duration::minutes(5),
        );
        let convs = vec![old_relevant, new_weak];
        let searchable = precompute_search_text(&convs);
        let results = search(&convs, &searchable, "agents config", now);
        assert_eq!(results[0], 0, "strong relevance should beat freshness");
    }
}
