use crate::history::Conversation;
use crate::search::byte_search::{AsciiCaselessFinder, NonAsciiCapitals};
use crate::search::lexical::{SearchTextsInMemory, SessionSearchTexts};
use crate::search::literal::Literal;
use crate::search::query::ParsedQuery;
use crate::search::separated_texts::SeparatedTexts;
use crate::text_match::{is_word_start, normalize_for_search};
use std::borrow::Borrow;
use std::collections::HashMap;
use std::ops::Range;
use std::sync::Arc;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LexicalEvidence {
    /// The row's title and summary as the evidence read them: a refresh can
    /// retitle the row before the evidence is built again.
    title_and_summary: Vec<String>,
    /// The session's other texts, shared with the texts held in memory;
    /// `None` for the conversation's `full_text`.
    body: Option<Arc<str>>,
    /// Ranges in `leading`, then `body`, each after a separator.
    pub context_ranges: Vec<(usize, usize)>,
}

impl LexicalEvidence {
    /// The texts `context_ranges` index, `conversation` being the row the
    /// evidence was built for.
    pub(crate) fn texts<'a>(&'a self, conversation: &'a Conversation) -> SeparatedTexts<'a> {
        let body = self.body.as_deref().unwrap_or(&conversation.full_text);
        SeparatedTexts::new(
            self.title_and_summary
                .iter()
                .map(String::as_str)
                .chain([body]),
        )
    }
}

#[derive(Clone, Copy, Debug)]
struct TermHit {
    start: usize,
    end: usize,
    term_idx: usize,
}

#[derive(Clone, Debug)]
struct HitCluster {
    start: usize,
    end: usize,
    unique_terms: u64,
    missing_terms: u64,
    adjacent_pairs: u32,
    last_hit_end: usize,
    last_term_idx: usize,
}

impl HitCluster {
    fn span(&self) -> usize {
        self.end.saturating_sub(self.start)
    }

    fn unique_count(&self) -> u32 {
        self.unique_terms.count_ones()
    }

    fn missing_count(&self) -> u32 {
        self.missing_terms.count_ones()
    }
}

#[derive(Clone)]
enum EvidenceSpec {
    Normalized(String),
    Literal(Literal),
}

impl EvidenceSpec {
    fn key(&self) -> (&str, bool) {
        match self {
            Self::Normalized(term) => (term.as_str(), false),
            Self::Literal(literal) => (literal.text(), true),
        }
    }

    fn has_match(&self, text: &str) -> bool {
        match self {
            Self::Normalized(term) => find_first_normalized_match(text, term).is_some(),
            Self::Literal(literal) => literal.matches(text),
        }
    }

    fn ranges(&self, text: &str) -> Vec<(usize, usize)> {
        match self {
            Self::Normalized(term) => find_normalized_term_ranges(text, term),
            Self::Literal(literal) => literal.match_ranges(text),
        }
    }
}

/// The number of top-ranked results the session list builds evidence for.
pub const MAX_EVIDENCE_ROWS: usize = 200;

/// The evidence the session list shows under its first [`MAX_EVIDENCE_ROWS`]
/// search results, keyed by conversation index. `ranked_results` holds the
/// result indices in rank order; `texts` holds the search texts read into
/// memory so far.
pub fn build_session_list_evidence<C: Borrow<Conversation>>(
    conversations: &[C],
    texts: &SearchTextsInMemory,
    ranked_results: &[usize],
    parsed: &ParsedQuery,
) -> HashMap<usize, LexicalEvidence> {
    ranked_results
        .iter()
        .take(MAX_EVIDENCE_ROWS)
        .filter_map(|&index| {
            let conversation = conversations[index].borrow();
            build_lexical_evidence(conversation, texts.get(conversation), parsed)
                .map(|evidence| (index, evidence))
        })
        .collect()
}

/// The evidence for `conversation` in its search texts: its row's title and
/// summary, then `texts`, those held in memory. A conversation the cache
/// doesn't hold reads its `full_text` in place of `texts`; a cached one
/// without texts held reads its title and summary alone.
pub fn build_lexical_evidence(
    conversation: &Conversation,
    texts: Option<&SessionSearchTexts>,
    parsed: &ParsedQuery,
) -> Option<LexicalEvidence> {
    let specs = evidence_specs(parsed);
    if specs.is_empty() {
        return None;
    }
    let mut evidence = LexicalEvidence {
        title_and_summary: conversation
            .title_and_summary_texts()
            .map(str::to_owned)
            .collect(),
        body: texts.map(|texts| Arc::clone(texts.joined())),
        context_ranges: Vec::new(),
    };
    let ranges =
        select_context_ranges(&evidence.texts(conversation), &conversation.preview, &specs)?;
    evidence.context_ranges = ranges;
    Some(evidence)
}

fn evidence_specs(parsed: &ParsedQuery) -> Vec<EvidenceSpec> {
    let unquoted = parsed.unquoted();
    let unquoted_terms = unquoted.split_whitespace().collect::<Vec<_>>();
    let identifier_literals = unquoted_terms
        .iter()
        .copied()
        .filter(|term| term.contains('_'))
        .map(|term| EvidenceSpec::Literal(Literal::new(term.to_string())));
    let normalized_query = unquoted_terms
        .iter()
        .copied()
        .filter(|term| !term.contains('_'))
        .collect::<Vec<_>>()
        .join(" ");
    let normalized = normalize_for_search(&normalized_query)
        .split_whitespace()
        .map(|term| EvidenceSpec::Normalized(term.to_string()))
        .collect::<Vec<_>>();

    dedupe_specs(
        &normalized
            .into_iter()
            .chain(identifier_literals)
            .chain(parsed.literals().iter().cloned().map(EvidenceSpec::Literal))
            .collect::<Vec<_>>(),
    )
}

fn dedupe_specs(specs: &[EvidenceSpec]) -> Vec<EvidenceSpec> {
    let mut deduped = Vec::new();
    for spec in specs {
        let (text, is_literal) = spec.key();
        if !deduped.iter().any(|existing: &EvidenceSpec| {
            let (existing_text, existing_is_literal) = existing.key();
            existing_is_literal == is_literal && existing_text.eq_ignore_ascii_case(text)
        }) {
            deduped.push(spec.clone());
            if deduped.len() == 64 {
                break;
            }
        }
    }
    deduped
}

fn select_context_ranges(
    full_text: &SeparatedTexts<'_>,
    preview: &str,
    specs: &[EvidenceSpec],
) -> Option<Vec<(usize, usize)>> {
    let mut missing_mask: u64 = 0;
    let mut missing_count = 0u32;
    for (i, spec) in specs.iter().enumerate() {
        if !spec.has_match(preview) {
            missing_mask |= 1 << i;
            missing_count += 1;
        }
    }

    let all_hits = find_all_spec_hits(full_text, specs);
    if all_hits.is_empty() {
        return None;
    }

    if missing_count == 0 {
        let preview_hit_count = find_all_spec_hits(&SeparatedTexts::new([preview]), specs).len();
        if all_hits.len() <= preview_hit_count {
            return None;
        }
    }

    let hit_spans: Vec<(usize, usize, usize)> = all_hits
        .iter()
        .map(|hit| (hit.start, hit.end, hit.term_idx))
        .collect();
    select_hidden_context_ranges(full_text, &hit_spans, missing_mask, missing_count)
}

pub(crate) fn select_hidden_context_ranges(
    full_text: &SeparatedTexts<'_>,
    hits: &[(usize, usize, usize)],
    missing_mask: u64,
    missing_count: u32,
) -> Option<Vec<(usize, usize)>> {
    let merge_gap_bytes: usize = 50;
    let max_cluster_span_bytes: usize = 200;
    let max_clusters: usize = 3;

    if hits.is_empty() {
        return None;
    }

    let mut clusters: Vec<HitCluster> = Vec::new();
    for &(start, end, term_idx) in hits {
        let term_bit: u64 = 1u64 << term_idx;
        let is_missing = (missing_mask & term_bit) != 0;
        let mut extended = false;

        if let Some(last) = clusters.last_mut() {
            let close_enough = start <= last.end.saturating_add(merge_gap_bytes);
            let new_end = last.end.max(end);
            let new_span = new_end.saturating_sub(last.start);
            if close_enough && new_span <= max_cluster_span_bytes {
                if term_idx != last.last_term_idx && start >= last.last_hit_end {
                    let gap = full_text.slice(last.last_hit_end..start);
                    if !gap.is_empty() && gap.chars().all(|c| !c.is_alphanumeric()) {
                        last.adjacent_pairs += 1;
                    }
                }

                last.end = new_end;
                last.unique_terms |= term_bit;
                if is_missing {
                    last.missing_terms |= term_bit;
                }
                last.last_hit_end = end;
                last.last_term_idx = term_idx;
                extended = true;
            }
        }

        if !extended {
            clusters.push(HitCluster {
                start,
                end,
                unique_terms: term_bit,
                missing_terms: if is_missing { term_bit } else { 0 },
                adjacent_pairs: 0,
                last_hit_end: end,
                last_term_idx: term_idx,
            });
        }
    }

    if missing_count > 0 {
        clusters.retain(|cluster| cluster.missing_count() > 0);
    }
    if clusters.is_empty() {
        return None;
    }

    clusters.sort_unstable_by(|a, b| {
        b.missing_count()
            .cmp(&a.missing_count())
            .then_with(|| b.adjacent_pairs.cmp(&a.adjacent_pairs))
            .then_with(|| b.unique_count().cmp(&a.unique_count()))
            .then_with(|| a.span().cmp(&b.span()))
            .then_with(|| a.start.cmp(&b.start))
    });

    let mut selected: Vec<HitCluster> = Vec::new();
    let mut covered_missing: u64 = 0;

    for cluster in &clusters {
        if selected.len() >= max_clusters {
            break;
        }
        let new_missing = cluster.missing_terms & !covered_missing;
        if new_missing != 0 {
            covered_missing |= cluster.missing_terms;
            selected.push(cluster.clone());
        }
    }

    for cluster in &clusters {
        if selected.len() >= max_clusters {
            break;
        }
        if !selected
            .iter()
            .any(|selected| selected.start == cluster.start && selected.end == cluster.end)
        {
            selected.push(cluster.clone());
        }
    }

    selected.sort_unstable_by_key(|cluster| cluster.start);
    Some(
        selected
            .into_iter()
            .map(|cluster| (cluster.start, cluster.end))
            .collect(),
    )
}

/// Every spec's hits in `texts`, sorted by start, each text searched on its
/// own: a hit doesn't cross a separator.
fn find_all_spec_hits(texts: &SeparatedTexts<'_>, specs: &[EvidenceSpec]) -> Vec<TermHit> {
    let mut all_hits = Vec::new();
    for (term_idx, spec) in specs.iter().enumerate() {
        for (offset, text) in texts.texts() {
            all_hits.extend(spec.ranges(text).into_iter().map(|(start, end)| TermHit {
                start: offset + start,
                end: offset + end,
                term_idx,
            }));
        }
    }
    all_hits.sort_unstable_by_key(|hit| hit.start);
    all_hits
}

fn find_first_normalized_match(text: &str, term: &str) -> Option<(usize, usize)> {
    find_normalized_term_ranges(text, term).into_iter().next()
}

/// The byte range of each match of `term` in `text`, left to right and not
/// overlapping. Each text character matches `term`'s character it
/// normalises to. A term starting with an alphanumeric character matches
/// only at the start of a word.
///
/// A term of ASCII characters that normalise to themselves is found by a
/// byte search, except in the whitespace-bounded span around each character
/// outside ASCII that normalises to one of the term's letters: that span is
/// read character by character. The ranges are those of reading the whole
/// text character by character, because such a term holds no whitespace, so
/// no match crosses a span's edge, and a span starts after whitespace, so its
/// first character starts a word as it does in the whole text.
fn find_normalized_term_ranges(text: &str, term: &str) -> Vec<(usize, usize)> {
    let is_found_by_bytes = term.bytes().all(|byte| {
        byte.is_ascii_graphic() && normalize_evidence_char(char::from(byte)) == char::from(byte)
    });
    if !is_found_by_bytes {
        return find_term_ranges_each_char(text, term);
    }
    let capitals = NonAsciiCapitals::matchable_by(term.as_bytes());
    let mut ranges = Vec::new();
    let mut from = 0;
    while let Some(capital) = capitals.first_in(&text[from..]).map(|at| from + at) {
        let span = whitespace_bounded_span(text, from, capital);
        push_ranges_at(
            &mut ranges,
            from,
            find_ascii_term_ranges(&text[from..span.start], term),
        );
        push_ranges_at(
            &mut ranges,
            span.start,
            find_term_ranges_each_char(&text[span.clone()], term),
        );
        from = span.end;
    }
    push_ranges_at(
        &mut ranges,
        from,
        find_ascii_term_ranges(&text[from..], term),
    );
    ranges
}

/// The span of `text` around byte `at` without whitespace: from just after
/// the last whitespace character between `from` and `at`, or from `from`,
/// to the next whitespace character, or the end.
fn whitespace_bounded_span(text: &str, from: usize, at: usize) -> Range<usize> {
    let start = text[from..at]
        .char_indices()
        .rev()
        .find(|(_, ch)| ch.is_whitespace())
        .map_or(from, |(index, ch)| from + index + ch.len_utf8());
    let end = text[at..]
        .find(char::is_whitespace)
        .map_or(text.len(), |index| at + index);
    start..end
}

/// Adds `found`, ranges in a slice starting at byte `offset`, to `ranges`.
fn push_ranges_at(ranges: &mut Vec<(usize, usize)>, offset: usize, found: Vec<(usize, usize)>) {
    ranges.extend(
        found
            .into_iter()
            .map(|(start, end)| (offset + start, offset + end)),
    );
}

/// [`find_normalized_term_ranges`] for a term of ASCII characters that
/// normalise to themselves, in a text without a character outside ASCII
/// that normalises to one of them: a byte search ignoring ASCII case.
fn find_ascii_term_ranges(text: &str, term: &str) -> Vec<(usize, usize)> {
    let finder = AsciiCaselessFinder::new(term.as_bytes());
    let starts_alphanumeric = term.starts_with(|ch: char| ch.is_alphanumeric());
    let mut ranges = Vec::new();
    let mut from = 0;
    while let Some(start) = finder.find(text.as_bytes(), from) {
        if starts_alphanumeric && !is_word_start(text, start) {
            from = start + 1;
            continue;
        }
        let end = start + term.len();
        ranges.push((start, end));
        from = end;
    }
    ranges
}

fn find_term_ranges_each_char(text: &str, term: &str) -> Vec<(usize, usize)> {
    let term_chars: Vec<char> = term.chars().collect();
    if term_chars.is_empty() {
        return Vec::new();
    }

    let query_starts_alnum = term_chars[0].is_alphanumeric();
    let mut ranges = Vec::new();
    let mut prev_is_alnum = false;
    let mut iter = text.char_indices().peekable();

    while let Some(&(byte_start, ch)) = iter.peek() {
        let norm_ch = normalize_evidence_char(ch);
        let is_alnum = ch.is_alphanumeric();
        let valid_start = !query_starts_alnum || !prev_is_alnum;

        if valid_start && norm_ch == term_chars[0] {
            let mut lookahead = iter.clone();
            lookahead.next();
            let mut matched = true;
            let mut end_byte = byte_start + ch.len_utf8();

            for &query_char in term_chars.iter().skip(1) {
                if let Some(&(_, next_ch)) = lookahead.peek() {
                    let next_norm = normalize_evidence_char(next_ch);
                    end_byte += next_ch.len_utf8();
                    lookahead.next();
                    if next_norm != query_char {
                        matched = false;
                        break;
                    }
                } else {
                    matched = false;
                    break;
                }
            }

            if matched {
                ranges.push((byte_start, end_byte));
                for _ in 0..term_chars.len().saturating_sub(1) {
                    iter.next();
                }
                prev_is_alnum = term_chars.last().is_some_and(|c| c.is_alphanumeric());
                iter.next();
                continue;
            }
        }

        prev_is_alnum = is_alnum;
        iter.next();
    }

    ranges
}

fn normalize_evidence_char(ch: char) -> char {
    if ch == '_' || ch == '-' || ch == '/' {
        ' '
    } else {
        ch.to_lowercase().next().unwrap_or(ch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Local, TimeZone};
    use std::path::PathBuf;

    fn conversation(preview: &str, full_text: &str) -> Conversation {
        Conversation {
            source: crate::history::Source::Claude,
            subagents: Vec::new(),
            session_id: "session".to_owned(),
            path: PathBuf::from("/tmp/session.jsonl"),
            index: 0,
            timestamp: Local.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap(),
            preview: preview.to_string(),
            preview_first: preview.to_string(),
            preview_last: preview.to_string(),
            full_text: full_text.to_string(),
            agent_search_text: String::new(),
            thinking_and_tool_call_text: String::new(),
            semantic_turns: vec![full_text.to_string()],
            semantic_turn_ranges: vec![crate::agent::refs::MessageRange::single(1)],
            search_text_lower: normalize_for_search(full_text),
            project_name: None,
            project_path: None,
            cwd: None,
            message_count: 1,
            message_times: crate::history::MessageTimes::default(),
            message_text_starts: crate::history::MessageTextStarts::default(),
            assistant_messages: 1,
            parse_errors: Vec::new(),
            summary: None,
            custom_title: None,
            external_title: None,
            model: None,
            total_tokens: 0,
            duration_minutes: None,
            fingerprint: None,
            has_transient_read_error: false,
            is_missing_earlier_turns: false,
            cached_session: None,
            cache_address: None,
            publication: crate::history::sqlite_cache::Publication::Unpublished {
                discovered: Vec::new(),
            },
        }
    }

    const TERMS: [&str; 14] = [
        "cache", "i", "k", "kit", "is", "a", "1st", ".rs", "c++", "über", "x", "session", "tool",
        "answer",
    ];

    fn assert_term_ranges_match_reading_each_character(text: &str, context: &str) {
        for term in TERMS {
            assert_eq!(
                find_normalized_term_ranges(text, term),
                find_term_ranges_each_char(text, term),
                "{term:?} in {context}: the ranges are those reading each character finds"
            );
        }
    }

    #[test]
    fn term_ranges_are_those_reading_each_character_finds() {
        let texts = [
            "",
            "x",
            "cache",
            "CACHE",
            "mycache cache_x Cache-y /cache cAcHe",
            "Caché cache中 中cache éCACHE cache",
            "\u{130}s is IS i\u{307}s",
            "\u{212A}it kit KIT k",
            "a.rs b.RS .rs",
            "c++ C++ cc++",
            "über ÜBER uber",
            "1st 21st 1ST",
            "aaa a-a a_a a/a",
        ];
        for text in texts {
            assert_term_ranges_match_reading_each_character(text, &format!("{text:?}"));
        }
    }

    #[test]
    fn term_ranges_in_every_providers_fixture_are_those_reading_each_character_finds() {
        for session in crate::history::provider::contract_tests::every_record_sessions() {
            let row = &session.row;
            for text in [
                &row.full_text,
                &row.preview,
                &row.agent_search_text,
                &row.thinking_and_tool_call_text,
            ] {
                assert_term_ranges_match_reading_each_character(text, row.source.label());
            }
        }
    }

    #[test]
    fn term_ranges_around_capitals_outside_ascii_are_those_reading_each_character_finds() {
        let separator = crate::history::message_texts::PADDED_MESSAGE_SEPARATOR;
        let bodies = [
            "\u{130}s is IS kit cache".to_owned(),
            "cache kit is \u{130}".to_owned(),
            "cache kit\u{212A} is k".to_owned(),
            format!("cache is{separator}\u{130}s kit"),
            format!("is \u{212A}it{separator}kit cache"),
            "x\u{130}s\u{212A}it\u{130}cache\u{212A} is kit".to_owned(),
            "\u{130}\u{130} \u{212A}\u{212A} session tool \u{130}s".to_owned(),
            "session.rs\u{130}cache-kit_\u{212A}it".to_owned(),
            "is kit cache session answer 1st".to_owned(),
        ];
        for body in &bodies {
            assert_term_ranges_match_reading_each_character(body, &format!("{body:?}"));
        }
    }

    #[test]
    fn every_providers_fixture_with_capitals_outside_ascii_gives_ranges_reading_each_character_finds()
     {
        for session in crate::history::provider::contract_tests::every_record_sessions() {
            let text = &session.row.full_text;
            let middle = (text.len() / 2..=text.len())
                .find(|index| text.is_char_boundary(*index))
                .unwrap_or(text.len());
            let body = format!(
                "\u{130}s {}\u{212A}it\u{130}{} kit\u{212A}",
                &text[..middle],
                &text[middle..]
            );
            assert_term_ranges_match_reading_each_character(&body, session.row.source.label());
        }
    }

    #[test]
    fn titled_rows_context_line_is_that_of_its_title_and_summary_joined_ahead_of_its_text() {
        let body = "a body that mentions deepgram, then audio_generation and plans";
        let mut titled = conversation("unrelated preview", body);
        titled.custom_title = Some("Deepgram plans".to_owned());
        titled.summary = Some("audio generation notes".to_owned());
        let mut joined_text = String::new();
        for text in ["Deepgram plans", "audio generation notes", body] {
            crate::history::message_texts::push_separated(&mut joined_text, text);
        }
        let joined = conversation("unrelated preview", &joined_text);
        let line = |row: &Conversation, query: &str| {
            let evidence = build_lexical_evidence(row, None, &ParsedQuery::parse(query))?;
            crate::search::preview::build_context_segments_from_ranges(
                &evidence.texts(row),
                &evidence.context_ranges,
                40,
            )
        };

        for query in [
            "deepgram",
            "plans",
            "notes body",
            "generation",
            "\"audio_generation\"",
            "plans audio",
        ] {
            let expected = line(&joined, query);
            assert!(expected.is_some(), "{query:?} finds a context line");
            assert_eq!(
                line(&titled, query),
                expected,
                "{query:?}: the context line reads the titles ahead of the text"
            );
        }
    }

    #[test]
    fn unquoted_body_match_produces_context_range() {
        let parsed = ParsedQuery::parse("deepgram");
        let conversation = conversation(
            "unrelated preview",
            "unrelated preview followed by hidden deepgram evidence",
        );

        let evidence = build_lexical_evidence(&conversation, None, &parsed).unwrap();

        assert_eq!(evidence.context_ranges.len(), 1);
        let (start, end) = evidence.context_ranges[0];
        assert!(conversation.full_text[start..end].contains("deepgram"));
    }

    #[test]
    fn quoted_literal_preserves_exact_punctuation() {
        let parsed = ParsedQuery::parse("\"audio_generation\"");
        let conversation = conversation(
            "audio generation normalized preview",
            "audio generation normalized preview and exact audio_generation evidence",
        );

        let evidence = build_lexical_evidence(&conversation, None, &parsed).unwrap();

        let (start, end) = evidence.context_ranges[0];
        assert_eq!(&conversation.full_text[start..end], "audio_generation");
    }

    #[test]
    fn unquoted_identifier_evidence_preserves_exact_punctuation() {
        let parsed = ParsedQuery::parse("api_key");
        let conversation = conversation(
            "api key normalized preview",
            "api key normalized preview and exact api_key evidence",
        );

        let evidence = build_lexical_evidence(&conversation, None, &parsed).unwrap();

        let snippets = evidence
            .context_ranges
            .iter()
            .map(|(start, end)| &conversation.full_text[*start..*end])
            .collect::<Vec<_>>();
        assert!(snippets.iter().any(|snippet| snippet.contains("api_key")));
    }

    #[test]
    fn mixed_query_can_surface_unquoted_and_literal_ranges() {
        let parsed = ParsedQuery::parse("hidden_unquoted \"exact_literal\"");
        let full_text = format!("hidden_unquoted {} exact_literal", "x ".repeat(120));
        let conversation = conversation("visible preview", &full_text);

        let evidence = build_lexical_evidence(&conversation, None, &parsed).unwrap();
        let snippets = evidence
            .context_ranges
            .iter()
            .map(|(start, end)| &conversation.full_text[*start..*end])
            .collect::<Vec<_>>();

        assert!(
            snippets
                .iter()
                .any(|snippet| snippet.contains("hidden_unquoted"))
        );
        assert!(
            snippets
                .iter()
                .any(|snippet| snippet.contains("exact_literal"))
        );
    }
}
