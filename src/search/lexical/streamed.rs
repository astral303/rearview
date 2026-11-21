//! Scores sessions' stored search texts, read from the session cache or held
//! in memory, without normalising them.

use super::hit_messages::MessageTerms;
use super::raw::{ADJACENCY_LIMIT, Counts, PREFIX_LIMIT, RawMatcher, RawQuery};
use super::*;
use crate::error::Result;
use crate::history::sqlite_cache::{
    CachedSession, HeldCopy, ScannedCopy, ScannedRow, TextVisitor, found, scan_search_texts,
};
use std::collections::{HashMap, HashSet};

impl Counts {
    pub(super) fn matches_literals(&self) -> bool {
        self.literals.iter().all(|found| *found)
    }

    /// Resets every count to not found, keeping the lengths.
    fn clear(&mut self) {
        self.contains.fill(false);
        self.prefixes.fill(0);
        self.adjacency = 0;
        self.literals.fill(false);
        self.has_text = false;
    }
}

/// `query`'s counts over the search texts of each conversation at `indices`,
/// each of which the session cache holds, by index: for the session list,
/// from the texts held in memory, and none for a session without texts
/// held; for `agent search`, from the cache. Without a time window, the
/// counts also hold the row's title and summary, and a literal in a
/// conversation's project name counts as found. For `agent search`, also
/// the hit messages of each conversation read from the cache.
pub(super) fn collect_all<C: Borrow<Conversation> + Sync>(
    conversations: &[C],
    indices: &[usize],
    surface: SearchSurface<'_>,
    query: &RawQuery<'_>,
) -> Result<StoredCounts> {
    let (held, stored): (Vec<_>, Vec<_>) = indices
        .iter()
        .map(|&index| {
            let texts = surface.texts_in_memory(conversations[index].borrow());
            (index, texts)
        })
        .partition(|(_, texts)| texts.is_some());
    let held = held
        .into_iter()
        .filter_map(|(index, texts)| Some((index, texts?)))
        .collect::<Vec<_>>();
    let mut counts = chunked::count_held(&held, query);
    let stored = stored
        .into_iter()
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    let mut stored = match surface {
        SearchSurface::List(_) => {
            let matcher = query.matcher();
            counts.extend(stored.iter().map(|&index| (index, matcher.counts())));
            StoredCounts {
                counts,
                hit_messages: HitMessages::default(),
                no_longer_cached: HashSet::new(),
                failed: Vec::new(),
            }
        }
        SearchSurface::Agent { retrieval, .. } => {
            let mut found = count_stored(conversations, &stored, surface, query, retrieval)?;
            found.counts.extend(counts);
            found
        }
    };
    if !surface.window().is_active() {
        let mut matcher = query.matcher();
        for (index, counts) in &mut stored.counts {
            let conversation = conversations[*index].borrow();
            for text in conversation.title_and_summary_texts() {
                matcher.count(text, counts);
            }
            let project = conversation.project_name.as_deref().unwrap_or_default();
            for (found, literal) in counts.literals.iter_mut().zip(query.literals()) {
                *found |= literal.matches(project);
            }
        }
    }
    Ok(stored)
}

/// The results of a count of sessions' stored search texts, by the index of
/// each conversation counted.
pub(super) struct StoredCounts {
    pub(super) counts: HashMap<usize, Counts>,
    pub(super) hit_messages: HitMessages,
    /// The conversations whose session was no longer in the session cache,
    /// or whose cached copy was not ready, while it was counted: another
    /// rearview removed it, or was writing it again. Nothing of them is
    /// counted.
    pub(super) no_longer_cached: HashSet<usize>,
    /// The conversations whose read failed, each with its error, in index
    /// order. Nothing of them is counted.
    pub(super) failed: Vec<(usize, crate::error::AppError)>,
}

impl StoredCounts {
    /// True when the conversation at `index` is left out of the search: no
    /// longer in the session cache, not ready, or its read failed.
    pub(super) fn leaves_out(&self, index: usize) -> bool {
        self.no_longer_cached.contains(&index)
            || self.failed.iter().any(|(failed, _)| *failed == index)
    }
}

/// `query`'s counts over the search texts of each conversation at `indices`,
/// read from the session cache in one scan, by index. A conversation's
/// counts hold the texts holding some of the query, and `has_text` is true
/// when any text it holds has more than whitespace. Also each conversation's
/// messages holding every term `retrieval` requires. A session another write
/// replaced during the scan is counted again alone, from the copy the cache
/// then holds. Fails when the scan fails, or the read of every conversation
/// fails.
fn count_stored<C: Borrow<Conversation>>(
    conversations: &[C],
    indices: &[usize],
    surface: SearchSurface<'_>,
    query: &RawQuery<'_>,
    retrieval: &RetrievalTerms,
) -> Result<StoredCounts> {
    let matcher = query.matcher();
    let mut stored = StoredCounts {
        counts: indices
            .iter()
            .map(|&index| (index, matcher.counts()))
            .collect(),
        hit_messages: HitMessages::default(),
        no_longer_cached: HashSet::new(),
        failed: Vec::new(),
    };
    if indices.is_empty() {
        return Ok(stored);
    }
    let copies = indices
        .iter()
        .map(|&index| cached_copy(conversations[index].borrow()))
        .collect::<Vec<_>>();
    let window = surface.window();
    let scanned = scan_search_texts(
        &copies,
        &window,
        surface.scan_scope(),
        || BodyCounter::new(query),
        |_, held| Ok((held.generation(), count_alone(held, &window, query)?)),
    )?;
    let counts = &mut stored.counts;
    let required = query.required_terms(retrieval);
    let mut scanned_copies = HashSet::new();
    for (copy, scanned) in scanned.copies.into_iter().enumerate() {
        let index = indices[copy];
        match found(scanned) {
            Ok(Some(ScannedCopy::Scanned(()))) => {
                scanned_copies.insert(copy);
            }
            Ok(Some(ScannedCopy::ReadAlone((generation, (counts_alone, messages))))) => {
                counts.insert(index, counts_alone);
                stored.hit_messages.0.insert(
                    index,
                    SessionHitMessages::holding(generation, &messages, required),
                );
            }
            Ok(None) => {
                counts.remove(&index);
                stored.no_longer_cached.insert(index);
            }
            Err(error) => {
                counts.remove(&index);
                stored.failed.push((index, error));
            }
        }
    }
    let mut message_terms: HashMap<usize, HashMap<usize, MessageTerms>> = HashMap::new();
    for counter in scanned.reads {
        for (copy, found) in counter.bodies {
            if scanned_copies.contains(&copy)
                && let Some(session) = counts.get_mut(&indices[copy])
            {
                session.add(&found);
            }
        }
        for ((copy, ordinal), terms) in counter.messages {
            if scanned_copies.contains(&copy) {
                message_terms
                    .entry(copy)
                    .or_default()
                    .entry(ordinal)
                    .or_default()
                    .add(terms);
            }
        }
    }
    for copy in scanned_copies {
        let terms = message_terms.remove(&copy).unwrap_or_default();
        stored.hit_messages.0.insert(
            indices[copy],
            SessionHitMessages::holding(copies[copy].generation(), &terms, required),
        );
    }
    Ok(stored)
}

/// `query`'s counts over the search texts of `held`, read alone, and the
/// terms each message's texts hold.
fn count_alone(
    held: &HeldCopy<'_>,
    window: &TimeFilter,
    query: &RawQuery<'_>,
) -> Result<(Counts, HashMap<usize, MessageTerms>)> {
    let mut counter = BodyCounter::new(query);
    held.visit_search_texts(window, |text| {
        counter.count(ALONE, text.ordinal, text.text);
        std::ops::ControlFlow::Continue(())
    })?;
    Ok(counter.into_session(ALONE))
}

/// The position of a session read alone, as [`BodyCounter`] counts it.
const ALONE: usize = 0;

/// The hit messages `agent search`'s ranking scan finds for `query` under
/// `retrieval` among one session's `texts`, each with its message number,
/// counted as the scan counts each text it reads.
#[cfg(test)]
pub(crate) fn hit_messages_of_texts<'t>(
    query: &str,
    retrieval: &RetrievalTerms,
    texts: impl IntoIterator<Item = (Option<usize>, &'t str)>,
) -> Vec<usize> {
    let window = TimeFilter::default();
    let surface = SearchSurface::Agent {
        window: &window,
        retrieval,
        scope: ScanScope::AllWorkspaces,
    };
    let terms = CountedTerms::of(&crate::search::query::ParsedQuery::parse(query), surface);
    let words = terms.words();
    let query = RawQuery::new(&words, &[], &terms.literals);
    let mut counter = BodyCounter::new(&query);
    for (ordinal, text) in texts {
        counter.count(ALONE, ordinal, text);
    }
    let (_, messages) = counter.into_session(ALONE);
    SessionHitMessages::holding(0, &messages, query.required_terms(retrieval)).ordinals
}

/// The hit messages of `held` for `query` under `retrieval` within
/// `window`, its texts read alone.
pub(super) fn hit_messages_in(
    held: &HeldCopy<'_>,
    window: &TimeFilter,
    retrieval: &RetrievalTerms,
    query: &RawQuery<'_>,
) -> Result<SessionHitMessages> {
    let (_, messages) = count_alone(held, window, query)?;
    Ok(SessionHitMessages::holding(
        held.generation(),
        &messages,
        query.required_terms(retrieval),
    ))
}

fn cached_copy(conversation: &Conversation) -> &CachedSession {
    conversation.cached_session.as_ref().expect("cached row")
}

/// One row range's counts of the texts holding some of the query, by the
/// position of their session in the scanned copies.
struct BodyCounter<'q> {
    matcher: RawMatcher<'q>,
    /// The counts of the text examined last.
    text: Counts,
    /// The terms the text examined last holds as a message's part.
    text_terms: MessageTerms,
    bodies: HashMap<usize, Counts>,
    /// The terms each message's kept texts hold, by the position of its
    /// session and its number.
    messages: HashMap<(usize, usize), MessageTerms>,
}

impl<'q> BodyCounter<'q> {
    fn new(query: &'q RawQuery<'q>) -> Self {
        let matcher = query.matcher();
        let text = matcher.counts();
        Self {
            matcher,
            text,
            text_terms: MessageTerms::default(),
            bodies: HashMap::new(),
            messages: HashMap::new(),
        }
    }

    /// Counts `text` of the session at `copy`, in the message `ordinal`, as
    /// a scan counts each text it reads.
    fn count(&mut self, copy: usize, ordinal: Option<usize>, text: &str) {
        if !text.trim().is_empty() {
            self.holds_text(copy);
        }
        if self.examine(text) {
            self.keep_examined(copy, ordinal);
        }
    }

    /// Adds the text examined last to the session at `copy`, in the message
    /// `ordinal`.
    fn keep_examined(&mut self, copy: usize, ordinal: Option<usize>) {
        body_of(&mut self.bodies, &self.matcher, copy).add(&self.text);
        if let Some(ordinal) = ordinal {
            self.messages
                .entry((copy, ordinal))
                .or_default()
                .add(self.text_terms);
        }
    }

    /// The counts of the session at `copy`, and the terms each of its
    /// messages holds.
    fn into_session(mut self, copy: usize) -> (Counts, HashMap<usize, MessageTerms>) {
        let counts = self
            .bodies
            .remove(&copy)
            .unwrap_or_else(|| self.matcher.counts());
        let messages = self
            .messages
            .into_iter()
            .filter(|((of_copy, _), _)| *of_copy == copy)
            .map(|((_, ordinal), terms)| (ordinal, terms))
            .collect();
        (counts, messages)
    }
}

impl TextVisitor for BodyCounter<'_> {
    fn examine(&mut self, text: &str) -> bool {
        self.text.clear();
        self.matcher.count(text, &mut self.text);
        self.text_terms = self.matcher.message_terms(text, &self.text);
        self.text_terms.literals != 0
            || self
                .text
                .contains
                .iter()
                .chain(&self.text.literals)
                .any(|found| *found)
    }

    fn keep(&mut self, row: &ScannedRow, _text: &str) {
        self.keep_examined(row.copy, row.ordinal);
    }

    fn holds_text(&mut self, copy: usize) {
        body_of(&mut self.bodies, &self.matcher, copy).has_text = true;
    }
}

/// The counts in `bodies` of the session at `copy` in the scanned copies.
fn body_of<'b>(
    bodies: &'b mut HashMap<usize, Counts>,
    matcher: &RawMatcher<'_>,
    copy: usize,
) -> &'b mut Counts {
    bodies.entry(copy).or_insert_with(|| matcher.counts())
}

/// `searchable`'s score from `body`, its counts; `None` when it does not
/// match. The body field scores only when `body` holds text.
pub(super) fn score(
    searchable: &SearchableConversation,
    body: &Counts,
    words: &[&str],
    pairs: &[String],
    time: DateTime<Local>,
    now: DateTime<Local>,
    include_project: bool,
) -> Option<ScoreDebug> {
    if !body.matches_literals() {
        return None;
    }
    let project = &searchable.project_lower;
    for (index, word) in words.iter().enumerate() {
        if !body.contains[index] && !(include_project && project.contains(word)) {
            return None;
        }
    }
    for (index, word) in words.iter().enumerate() {
        if body.prefixes[index] == 0
            && !(include_project && count_prefix_matches(project, word, 1) > 0)
        {
            if words.iter().any(|word| {
                word.chars()
                    .any(|ch| ('\u{4E00}'..='\u{9FFF}').contains(&ch))
            }) {
                let fresh = freshness_bonus(time, now);
                return Some(ScoreDebug {
                    total: words.len() as f64 * 0.5 + fresh,
                    freshness: fresh,
                    fields: Vec::new(),
                });
            }
            return None;
        }
    }
    let mut fields = Vec::new();
    for (text, weight, name) in [
        (&searchable.title_lower, WEIGHT_TITLE, "title"),
        (&searchable.summary_lower, WEIGHT_SUMMARY, "summary"),
        (project, WEIGHT_PROJECT, "project"),
    ] {
        if !text.is_empty() {
            let counts = words
                .iter()
                .map(|word| count_prefix_matches(text, word, PREFIX_LIMIT))
                .collect::<Vec<_>>();
            fields.push(field_score(
                name,
                weight,
                words,
                &counts,
                count_adjacent_pairs(text, pairs, ADJACENCY_LIMIT),
            ));
        }
    }
    if body.has_text {
        fields.push(field_score(
            "body",
            WEIGHT_BODY,
            words,
            &body.prefixes,
            body.adjacency,
        ));
    }
    let fresh = freshness_bonus(time, now);
    let total = fields.iter().fold(0.0, |total, field| {
        total + field.tf_score + field.adjacency_score
    }) + fresh;
    Some(ScoreDebug {
        total,
        freshness: fresh,
        fields,
    })
}

fn field_score(
    name: &'static str,
    weight: f64,
    words: &[&str],
    counts: &[usize],
    adjacency: usize,
) -> FieldDebug {
    let word_details = words
        .iter()
        .zip(counts)
        .map(|(word, &count)| {
            let score = if count > 0 {
                ((1 + count) as f64).ln()
            } else {
                0.0
            };
            ((*word).to_owned(), count, score)
        })
        .collect::<Vec<_>>();
    let tf_score = weight * word_details.iter().map(|(_, _, score)| score).sum::<f64>();
    FieldDebug {
        name,
        weight,
        tf_score,
        adjacency_score: weight * 2.0 * adjacency as f64,
        word_details,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::provider::contract_tests::{AgentSession, every_record_sessions};
    use crate::history::sqlite_cache::tests::parsed_and_cached_row;
    use crate::search::evidence::{LexicalEvidence, build_lexical_evidence};

    #[test]
    fn cache_and_memory_preserve_scores_evidence_literals_and_windows_for_every_provider() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-10-01T16:00:00Z")
            .unwrap()
            .with_timezone(&Local);
        for session in every_record_sessions() {
            let base = tempfile::tempdir().unwrap();
            let (row, cached) = parsed_and_cached_row(base.path(), &session);
            let mut original = row;
            original.project_name.clone_from(&cached.project_name);
            let first_second = original
                .message_times
                .span(1)
                .map(|span| span.first_second())
                .unwrap_or(now.timestamp());
            let second = crate::clock::Zone::System
                .timestamp_opt(first_second, 0)
                .single()
                .unwrap();
            let windows = [
                TimeFilter::default(),
                TimeFilter {
                    after: Some(second),
                    before: Some(second),
                },
                TimeFilter {
                    after: None,
                    before: Some(second),
                },
            ];
            let original = [original];
            let cached = [cached];
            let in_memory = SearchTextsInMemory::read(&cached);
            let held = in_memory
                .get(&cached[0])
                .expect("the texts are read into memory");
            for window in &windows {
                for surface in [
                    SearchSurface::List(&in_memory),
                    SearchSurface::Agent {
                        window,
                        retrieval: &RetrievalTerms::WordsAndLiterals,
                        scope: ScanScope::AllWorkspaces,
                    },
                    SearchSurface::Agent {
                        window,
                        retrieval: &RetrievalTerms::WordsAndLiterals,
                        scope: ScanScope::Project,
                    },
                ] {
                    let original_texts = match surface {
                        SearchSurface::List(_) => precompute_search_text(&original),
                        SearchSurface::Agent { .. } => {
                            precompute_agent_search_text(&original, window).searchable
                        }
                    };
                    let cached_texts = match surface {
                        SearchSurface::List(_) => precompute_search_text(&cached),
                        SearchSurface::Agent { .. } => {
                            precompute_agent_search_text(&cached, window).searchable
                        }
                    };
                    for query in QUERIES {
                        let expected = debug_search_with_surface(
                            &original,
                            &original_texts,
                            query,
                            now,
                            surface,
                            |_| true,
                        )
                        .unwrap();
                        let actual = debug_search_with_surface(
                            &cached,
                            &cached_texts,
                            query,
                            now,
                            surface,
                            |_| true,
                        )
                        .unwrap();
                        assert_eq!(
                            scores_and_word_counts(actual.results),
                            scores_and_word_counts(expected.results),
                            "{} {query:?}, window {window:?}",
                            session.row.source.label()
                        );
                    }
                }
            }
            let mut queries_with_evidence = 0;
            for query in QUERIES {
                let parsed = ParsedQuery::parse(query);
                let read_from_memory = build_lexical_evidence(&cached[0], Some(held), &parsed);
                queries_with_evidence += usize::from(read_from_memory.is_some());
                assert_eq!(
                    context_line(&cached[0], read_from_memory, 80),
                    context_line(
                        &original[0],
                        build_lexical_evidence(&original[0], None, &parsed),
                        80
                    ),
                    "{} {query:?}: the context line is the parsed session's",
                    session.row.source.label()
                );
            }
            assert!(
                queries_with_evidence > 0,
                "{}: some query finds evidence",
                session.row.source.label()
            );
        }
    }

    /// A result's index and total score, and each of its fields' name, term
    /// score and adjacency score.
    type ResultScores = (usize, u64, Vec<(&'static str, u64, u64)>);

    /// [`ResultScores`], with each field's per-word counts and scores.
    type ResultScoresAndWordCounts = (
        usize,
        u64,
        Vec<(&'static str, u64, u64, Vec<(String, usize, f64)>)>,
    );

    fn scores_and_word_counts(results: Vec<(usize, ScoreDebug)>) -> Vec<ResultScoresAndWordCounts> {
        results
            .into_iter()
            .map(|(index, score)| {
                let fields = score.fields.into_iter().map(|field| {
                    (
                        field.name,
                        field.tf_score.to_bits(),
                        field.adjacency_score.to_bits(),
                        field.word_details,
                    )
                });
                (index, score.total.to_bits(), fields.collect())
            })
            .collect()
    }

    fn scores(results: Vec<(usize, ScoreDebug)>) -> Vec<ResultScores> {
        results
            .into_iter()
            .map(|(index, score)| {
                let fields = score.fields.iter().map(|field| {
                    (
                        field.name,
                        field.tf_score.to_bits(),
                        field.adjacency_score.to_bits(),
                    )
                });
                (index, score.total.to_bits(), fields.collect())
            })
            .collect()
    }

    /// The count of `queries` with results. Fails unless the lexical search
    /// of `rows` gives each query the same results, order and scores reading
    /// the session list's texts in memory as reading `agent search`'s texts
    /// from the session cache. `agent search`'s own ranking is not compared.
    fn queries_scored_alike_from_memory_and_cache(
        rows: &[Conversation],
        queries: &[String],
    ) -> usize {
        let searchable = precompute_search_text(rows);
        let in_memory = SearchTextsInMemory::read(rows);
        let window = TimeFilter::default();
        let from_cache = SearchSurface::Agent {
            window: &window,
            retrieval: &RetrievalTerms::WordsAndLiterals,
            scope: ScanScope::AllWorkspaces,
        };
        let now = crate::search::test_fixtures::october(20, 12);
        let mut queries_with_results = 0;
        for query in queries {
            let ranked = |surface| {
                let searched =
                    debug_search_with_surface(rows, &searchable, query, now, surface, |_| true);
                scores(searched.unwrap().results)
            };
            let from_memory = ranked(SearchSurface::List(&in_memory));
            queries_with_results += usize::from(!from_memory.is_empty());
            assert_eq!(
                from_memory,
                ranked(from_cache),
                "{:?} {query:?}: the texts in memory give the results, order and scores the \
                 session cache's texts give",
                rows[0].source
            );
        }
        queries_with_results
    }

    /// The session list holds the dialogue ahead of the other fields' texts,
    /// and `agent search` reads the texts in the cache's order: this test
    /// fails when a score depends on the order of a session's texts. Each
    /// session has a cache of its own, because the every-record sessions
    /// share one cache key.
    #[test]
    fn list_search_scores_every_providers_session_from_memory_as_from_cache() {
        for session in every_record_sessions() {
            let base = tempfile::tempdir().unwrap();
            let rows = [crate::history::sqlite_cache::tests::cached_row(
                base.path(),
                &session,
            )];
            let markers = session.markers.iter().map(|marker| marker.text);
            let (first, last) = (
                session.markers[0].text,
                session.markers.last().unwrap().text,
            );
            let queries = QUERIES
                .map(str::to_owned)
                .into_iter()
                .chain(markers.flat_map(|text| [text.to_owned(), format!("\"{text}\"")]))
                .chain([format!("{first} {last}"), format!("{last} \"{first}\"")])
                .collect::<Vec<_>>();

            let queries_with_results = queries_scored_alike_from_memory_and_cache(&rows, &queries);

            assert!(
                queries_with_results > session.markers.len(),
                "{:?}: the queries find the session: {queries_with_results}",
                session.row.source
            );
        }
    }

    #[test]
    fn list_search_scores_sessions_matching_in_different_fields_from_memory_as_from_cache() {
        use crate::search::test_fixtures::{cached_claude_sessions, claude_user_record, october};
        let at = |hour| october(10, hour).to_rfc3339();
        let asking = |text: &str| claude_user_record(text, october(10, 10));
        let assistant = |block: serde_json::Value| {
            serde_json::json!({"type": "assistant", "timestamp": at(11),
                "message": {"role": "assistant", "content": [block]}})
        };
        let sessions = cached_claude_sessions(&[
            (
                "dialogue",
                vec![asking("rotate the ledger keys"), asking("ledger again")],
            ),
            (
                "tool-call",
                vec![
                    asking("check the keys"),
                    assistant(serde_json::json!({"type": "tool_use", "id": "call",
                        "name": "Bash", "input": {"command": "cat ledger.txt ledger.bak"}})),
                ],
            ),
            (
                "thinking",
                vec![
                    asking("check the ledger"),
                    assistant(serde_json::json!({"type": "thinking",
                        "thinking": "the ledger keys rotate; the ledger keys expire",
                        "signature": "sig"})),
                ],
            ),
            (
                "sub-agent",
                vec![
                    asking("delegate the work"),
                    serde_json::json!({"type": "progress", "timestamp": at(12),
                        "parentToolUseID": "toolu_task",
                        "data": {"type": "agent_progress", "agentId": "a7777777",
                            "prompt": "Read the ledger",
                            "message": {"type": "assistant", "message": {"role": "assistant",
                                "content": [{"type": "text", "text": "ledger keys found"}]}}}}),
                ],
            ),
            ("unrelated", vec![asking("water the plants")]),
        ]);
        let queries = [
            "ledger",
            "ledger keys",
            "\"ledger keys\"",
            "keys ledger rotate",
        ]
        .map(str::to_owned);

        let queries_with_results =
            queries_scored_alike_from_memory_and_cache(&sessions.rows, &queries);

        assert_eq!(
            queries_with_results,
            queries.len(),
            "each query finds a session"
        );
        let listed = search_with_texts(
            &sessions.rows,
            &precompute_search_text(&sessions.rows),
            &SearchTextsInMemory::read(&sessions.rows),
            "ledger",
            october(20, 12),
        );
        assert_eq!(
            listed.len(),
            4,
            "the session list finds the session of each field holding the word"
        );
    }

    #[test]
    fn scan_matches_each_table_and_window_keeps_only_messages_inside_it() {
        use crate::search::test_fixtures::{cached_claude_sessions, october, window_on_october_10};
        let at = |day, hour| october(day, hour).to_rfc3339();
        let records = vec![
            serde_json::json!({"type": "custom-title", "customTitle": "titleword"}),
            serde_json::json!({"type": "user", "timestamp": at(1, 12),
                "message": {"role": "user", "content": "outsideword"}}),
            serde_json::json!({"type": "user", "timestamp": at(10, 12),
                "message": {"role": "user", "content": "insideword"}}),
            serde_json::json!({"type": "assistant", "timestamp": at(10, 13),
                "message": {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "call", "name": "Bash",
                     "input": {"command": "headerword"}}]}}),
            serde_json::json!({"type": "user", "timestamp": at(10, 14),
                "message": {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "call",
                     "content": [{"type": "text", "text": "resultword"}]}]}}),
        ];
        let sessions = cached_claude_sessions(&[("session", records)]);
        let rows = &sessions.rows;
        let now = october(20, 12);
        let finds = |window: &TimeFilter, query: &str| {
            !agent_search(
                rows,
                &precompute_agent_search_text(rows, window),
                query,
                now,
            )
            .is_empty()
        };
        let without_window = TimeFilter::default();
        let window = window_on_october_10();
        for query in [
            "titleword",
            "outsideword",
            "insideword",
            "headerword",
            "resultword",
        ] {
            assert!(
                finds(&without_window, query),
                "{query:?} is found without a window"
            );
        }
        for query in ["insideword", "headerword", "resultword"] {
            assert!(
                finds(&window, query),
                "{query:?} is found inside the window"
            );
        }
        for query in ["outsideword", "titleword"] {
            assert!(
                !finds(&window, query),
                "{query:?} is left out by the window"
            );
        }
    }

    #[test]
    fn session_replaced_since_its_row_is_ranked_on_the_copy_the_cache_holds() {
        use crate::search::test_fixtures::{cached_claude_sessions, claude_user_record, october};
        let sessions = cached_claude_sessions(&[
            (
                "first",
                vec![claude_user_record("zebrafish swim", october(10, 12))],
            ),
            (
                "second",
                vec![claude_user_record("zebrafish rest", october(10, 13))],
            ),
        ]);
        let rows = &sessions.rows;
        let searchable = precompute_search_text(rows);
        let ranked = || {
            try_debug_search(
                rows,
                &searchable,
                &SearchTextsInMemory::read(rows),
                "zebrafish swim",
                october(20, 12),
                |_| true,
            )
            .unwrap()
            .results
            .into_iter()
            .map(|(index, score)| (index, score.total.to_bits()))
            .collect::<Vec<_>>()
        };
        let before = ranked();
        crate::history::sqlite_cache::tests::replace_copy(rows[0].cached_session.as_ref().unwrap());
        assert!(!before.is_empty(), "the query finds the sessions");
        assert_eq!(
            ranked(),
            before,
            "the replaced session ranks on its new copy's texts"
        );
    }

    #[test]
    fn session_matching_only_its_project_name_scores_its_body_once_its_texts_are_held() {
        use crate::search::test_fixtures::{cached_claude_sessions, claude_user_record, october};
        let sessions = cached_claude_sessions(&[(
            "talk",
            vec![claude_user_record("plain words", october(10, 12))],
        )]);
        let mut rows = sessions.rows.clone();
        for row in &mut rows {
            row.project_name = Some("zebrafish".to_owned());
        }
        let searchable = precompute_search_text(&rows);
        let fields = |texts: &SearchTextsInMemory| {
            try_debug_search(
                &rows,
                &searchable,
                texts,
                "zebrafish",
                october(20, 12),
                |_| true,
            )
            .unwrap()
            .results
            .into_iter()
            .map(|(index, score)| {
                let names = score.fields.iter().map(|field| field.name).collect();
                (index, names)
            })
            .collect::<Vec<(usize, Vec<&str>)>>()
        };

        assert_eq!(
            fields(&SearchTextsInMemory::read(&rows)),
            [(0, vec!["project", "body"])],
            "a session holding texts scores its body"
        );
        assert_eq!(
            fields(&SearchTextsInMemory::default()),
            [(0, vec!["project"])],
            "a session without texts held scores its project name alone"
        );
    }

    /// A Claude session containing runs of whitespace: paragraphs apart by
    /// blank lines, a line wrapped onto an indented line, a code block with
    /// indented and blank lines, and an edit whose diff contains indented
    /// lines. Its first messages, the row's preview, don't contain any of
    /// them.
    fn whitespace_runs_records() -> Vec<serde_json::Value> {
        use crate::search::test_fixtures::{claude_user_record, october};
        let paragraphs = "The retry loop now waits before each attempt.\n\n\n\nEach wait doubles the backoff\n        until the\t\tlimit.";
        let reply = "Here is the patch for the lexer:\n\n```rust\nfn parse(input: &str) {\n        let tokens = lex(input)\n                .filter(|token| !token.is_blank());\n\n\n        for token in tokens {\n                emit(token);\n        }\n}\n```\n\nThe filter drops blank tokens before emit sees them.";
        vec![
            claude_user_record("start the review", october(10, 8)),
            claude_user_record("check the build first", october(10, 9)),
            claude_user_record("then read the patch", october(10, 9)),
            serde_json::json!({"type": "assistant", "timestamp": october(10, 9).to_rfc3339(),
                "message": {"id": "paragraphs", "role": "assistant", "content": [
                    {"type": "text", "text": paragraphs}]}}),
            claude_user_record("review the lexer change", october(10, 10)),
            serde_json::json!({"type": "assistant", "timestamp": october(10, 11).to_rfc3339(),
            "message": {"id": "reply", "role": "assistant", "content": [
                {"type": "text", "text": reply},
                {"type": "tool_use", "id": "edit", "name": "Edit", "input": {
                    "file_path": "src/lexer.rs",
                    "old_string": "    let tokens = lex(input);\n\n    for token in tokens {",
                    "new_string": "    let tokens = lex(input)\n        .filter(|token| !token.is_blank());\n\n    for token in tokens {"
                }}]}}),
            serde_json::json!({"type": "user", "timestamp": october(10, 12).to_rfc3339(),
                "message": {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "edit",
                     "content": "The file src/lexer.rs has been updated."}]}}),
            claude_user_record("thanks, that reads well", october(10, 13)),
            claude_user_record("commit it", october(10, 14)),
            claude_user_record("and open the pull request", october(10, 15)),
        ]
    }

    /// Writes `records` as a Claude transcript in a new directory, parses
    /// it, writes its record to a new session cache, and returns the parsed
    /// row, the row the cache lists, and both directories.
    fn parsed_and_cached_rows(
        records: &[serde_json::Value],
    ) -> (Conversation, Conversation, [tempfile::TempDir; 2]) {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("session.jsonl");
        let lines = records
            .iter()
            .map(serde_json::Value::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&path, lines).unwrap();
        let session =
            AgentSession::new(crate::history::Source::Claude, path, Vec::new(), Vec::new());
        let base = tempfile::tempdir().unwrap();
        let (parsed, cached) = parsed_and_cached_row(base.path(), &session);
        (parsed, cached, [home, base])
    }

    /// A cached row shows the parsed row's context line when the session's
    /// texts contain runs of whitespace. A row with a failed cache write
    /// keeps its parse's texts; a row read from the cache reads its copy's.
    /// The context line's window counts each character of a run kept as the
    /// cache stores it, and the run's bytes keep two hits from joining one
    /// snippet.
    #[test]
    fn cached_row_shows_parsed_rows_context_line_for_runs_of_whitespace() {
        let (parsed, cached, _directories) = parsed_and_cached_rows(&whitespace_runs_records());
        let cached = [cached];
        let in_memory = SearchTextsInMemory::read(&cached);
        let held = in_memory
            .get(&cached[0])
            .expect("the texts are read into memory");
        let mut differing = Vec::new();
        for query in [
            "attempt doubles",
            "tokens emit",
            "emit",
            "filter",
            "for token",
            "parse input",
            "\"is_blank\"",
            "\"lex(input) .filter\"",
        ] {
            let parsed_query = ParsedQuery::parse(query);
            for width in [40, 80, 120] {
                let line = |row: &Conversation, texts| {
                    context_line(
                        row,
                        build_lexical_evidence(row, texts, &parsed_query),
                        width,
                    )
                };
                let from_cache = line(&cached[0], Some(held));
                assert!(
                    from_cache.is_some(),
                    "{query:?} at {width} columns: the cached copy shows a context line"
                );
                let from_parse = line(&parsed, None);
                if from_parse != from_cache {
                    differing.push(format!(
                        "{query:?} at {width} columns:\n  parsed: {from_parse:?}\n  cached: \
                         {from_cache:?}"
                    ));
                }
            }
        }
        assert!(
            differing.is_empty(),
            "the cached row shows the parsed row's context line:\n{}",
            differing.join("\n")
        );
    }

    /// A row parsed in this process and its cached copy give each query the
    /// same score, and each field the same counts of words and adjacent
    /// pairs, when runs of whitespace separate the words: on the
    /// session list's texts in memory and on `agent search`'s texts in the
    /// cache. The order of results follows the scores.
    #[test]
    fn parsed_row_and_its_cached_copy_score_runs_of_whitespace_alike() {
        let (mut parsed, cached, _directories) = parsed_and_cached_rows(&whitespace_runs_records());
        parsed.project_name.clone_from(&cached.project_name);
        let parsed = [parsed];
        let cached = [cached];
        let in_memory = SearchTextsInMemory::read(&cached);
        let without_texts = SearchTextsInMemory::default();
        let window = TimeFilter::default();
        let now = crate::search::test_fixtures::october(20, 12);
        let ranked = |rows: &[Conversation], query: &str, surface| {
            let searchable = precompute_search_text(rows);
            let searched =
                debug_search_with_surface(rows, &searchable, query, now, surface, |_| true);
            scores_and_word_counts(searched.unwrap().results)
        };
        let mut pairs_scored = 0;
        for query in [
            "backoff until",
            "the limit",
            "\"backoff until\"",
            "attempt each",
            "tokens emit",
            "token tokens",
            "filter token",
            "\"lex(input) .filter\"",
        ] {
            let from_parse = ranked(&parsed, query, SearchSurface::List(&without_texts));
            assert!(!from_parse.is_empty(), "{query:?} finds the parsed row");
            pairs_scored += usize::from(from_parse.iter().any(|(_, _, fields)| {
                fields.iter().any(|(name, _, adjacency, _)| {
                    *name == "body" && f64::from_bits(*adjacency) > 0.0
                })
            }));
            for (surface, name) in [
                (
                    SearchSurface::List(&in_memory),
                    "the session list's texts in memory",
                ),
                (
                    SearchSurface::Agent {
                        window: &window,
                        retrieval: &RetrievalTerms::WordsAndLiterals,
                        scope: ScanScope::AllWorkspaces,
                    },
                    "`agent search`'s texts in the cache",
                ),
            ] {
                assert_eq!(
                    ranked(&cached, query, surface),
                    from_parse,
                    "{query:?}: {name} score the cached copy as the parse scores its row"
                );
            }
        }
        assert!(
            pairs_scored > 1,
            "some queries score adjacent words across a run of whitespace"
        );
    }

    const QUERIES: [&str; 13] = [
        "t",
        "e",
        "session",
        "question",
        "answer",
        "tool",
        "steps",
        "searchable",
        "question answer",
        "child tool",
        "\"child answer searchable\"",
        "tasked_subagent_sentinel",
        "unknown_needle",
    ];

    /// The context line a row `width` columns wide shows for `evidence`.
    fn context_line(
        conversation: &Conversation,
        evidence: Option<LexicalEvidence>,
        width: usize,
    ) -> Option<String> {
        let evidence = evidence?;
        crate::search::preview::build_context_segments_from_ranges(
            &evidence.texts(conversation),
            &evidence.context_ranges,
            width,
        )
    }
}
