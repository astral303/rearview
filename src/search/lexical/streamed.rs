//! Scores sessions' stored search texts, read from the session cache or held
//! in memory, without normalising them.

use super::raw::{ADJACENCY_CAP, Counts, PREFIX_CAP, RawMatcher, RawQuery};
use super::*;
use crate::error::{AppError, Result};
use crate::history::sqlite_cache::{CachedSession, ScannedRow, TextVisitor, scan_search_texts};
use std::collections::HashMap;

impl Counts {
    pub(super) fn matches_literals(&self) -> bool {
        self.literals.iter().all(|found| *found)
    }

    /// Counts nothing found, keeping the lengths.
    fn clear(&mut self) {
        self.contains.fill(false);
        self.prefixes.fill(0);
        self.adjacency = 0;
        self.literals.fill(false);
        self.has_text = false;
    }
}

/// `query`'s counts over the search texts of each conversation at `indices`,
/// each of which the session cache holds, by index: from the texts held in
/// memory, or else from the cache. Without a time window, a literal in a
/// conversation's project name counts as found.
pub(super) fn collect_all<C: Borrow<Conversation> + Sync>(
    conversations: &[C],
    indices: &[usize],
    surface: SearchSurface<'_>,
    query: &RawQuery<'_>,
) -> Result<HashMap<usize, Counts>> {
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
    counts.extend(count_stored(conversations, &stored, surface, query)?);
    let (_, window) = surface.stored_fields();
    if !window.is_active() {
        for (index, counts) in &mut counts {
            let conversation = conversations[*index].borrow();
            let project = conversation.project_name.as_deref().unwrap_or_default();
            for (found, literal) in counts.literals.iter_mut().zip(query.literals()) {
                *found |= literal.matches(project);
            }
        }
    }
    Ok(counts)
}

/// `query`'s counts over the search texts of each conversation at `indices`,
/// read from the session cache in one scan, by index. A conversation's
/// counts hold only the texts holding some of the query: `has_text` is false
/// when none does, whatever other text it holds (see [`body_has_text`]).
fn count_stored<C: Borrow<Conversation>>(
    conversations: &[C],
    indices: &[usize],
    surface: SearchSurface<'_>,
    query: &RawQuery<'_>,
) -> Result<HashMap<usize, Counts>> {
    let matcher = query.matcher();
    let mut counts: HashMap<usize, Counts> = indices
        .iter()
        .map(|&index| (index, matcher.counts()))
        .collect();
    if indices.is_empty() {
        return Ok(counts);
    }
    let copies = indices
        .iter()
        .map(|&index| cached_copy(conversations[index].borrow()))
        .collect::<Vec<_>>();
    let (fields, window) = surface.stored_fields();
    let scanned = scan_search_texts(&copies, fields, &window, || BodyCounter::new(query))?;
    if let Some((_, error)) = scanned.missing.into_iter().next() {
        return Err(error);
    }
    for counter in scanned.visitors {
        for (copy, found) in counter.bodies {
            if let Some(session) = counts.get_mut(&indices[copy]) {
                session.add(&found);
            }
        }
    }
    Ok(counts)
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
    bodies: HashMap<usize, Counts>,
}

impl<'q> BodyCounter<'q> {
    fn new(query: &'q RawQuery<'q>) -> Self {
        let matcher = query.matcher();
        let text = matcher.counts();
        Self {
            matcher,
            text,
            bodies: HashMap::new(),
        }
    }
}

impl TextVisitor for BodyCounter<'_> {
    fn examine(&mut self, text: &str) -> bool {
        self.text.clear();
        self.matcher.count(text, &mut self.text);
        self.text
            .contains
            .iter()
            .chain(&self.text.literals)
            .any(|found| *found)
    }

    fn keep(&mut self, row: &ScannedRow, _text: &str) {
        let matcher = &self.matcher;
        self.bodies
            .entry(row.copy)
            .or_insert_with(|| matcher.counts())
            .add(&self.text);
    }
}

/// Whether `conversation`'s body holds text, whose counts are `body`: read
/// from the session cache when `body` came from a scan that found none of
/// the query.
pub(super) fn body_has_text(
    conversation: &Conversation,
    surface: SearchSurface<'_>,
    body: &Counts,
) -> Result<bool> {
    if body.has_text || surface.texts_in_memory(conversation).is_some() {
        return Ok(body.has_text);
    }
    let (fields, window) = surface.stored_fields();
    cached_copy(conversation)
        .clone()
        .ignoring_source_changes()
        .waiting()
        .holds_search_text(fields, &window)?
        .ok_or_else(|| AppError::SessionCacheNotReady {
            session: conversation.session_id.clone(),
            reason: "the session is not in the session cache".to_owned(),
        })
}

/// `searchable`'s score from `body`, its counts; `None` when it does not
/// match. `body_has_text` is read only for a session that matches.
#[allow(clippy::too_many_arguments)]
pub(super) fn score(
    searchable: &SearchableConversation,
    body: &Counts,
    body_has_text: impl FnOnce() -> Result<bool>,
    words: &[&str],
    pairs: &[String],
    time: DateTime<Local>,
    now: DateTime<Local>,
    include_project: bool,
) -> Result<Option<ScoreDebug>> {
    if !body.matches_literals() {
        return Ok(None);
    }
    let project = &searchable.project_lower;
    for (index, word) in words.iter().enumerate() {
        if !body.contains[index] && !(include_project && project.contains(word)) {
            return Ok(None);
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
                return Ok(Some(ScoreDebug {
                    total: words.len() as f64 * 0.5 + fresh,
                    freshness: fresh,
                    fields: Vec::new(),
                }));
            }
            return Ok(None);
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
                .map(|word| count_prefix_matches(text, word, PREFIX_CAP))
                .collect::<Vec<_>>();
            fields.push(field_score(
                name,
                weight,
                words,
                &counts,
                count_adjacent_pairs(text, pairs, ADJACENCY_CAP),
            ));
        }
    }
    if body_has_text()? {
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
    Ok(Some(ScoreDebug {
        total,
        freshness: fresh,
        fields,
    }))
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
    use crate::history::provider::contract_tests::every_record_sessions;
    use crate::history::provider::{Fingerprint, SessionRoot, SessionStub};
    use crate::history::sqlite_cache::tests::parsed_record_and_row;
    use crate::history::sqlite_cache::{CatalogueSession, SqliteCache};
    use crate::search::evidence::{LexicalEvidence, build_lexical_evidence};

    #[test]
    fn cache_and_memory_preserve_scores_evidence_literals_and_windows_for_every_provider() {
        let base = tempfile::tempdir().unwrap();
        let mut cache = SqliteCache::open(&crate::history::sqlite_cache::tests::rearview_at(
            base.path(),
        ))
        .unwrap();
        cache.mark_admitted().unwrap();
        let now = chrono::DateTime::parse_from_rfc3339("2026-10-01T16:00:00Z")
            .unwrap()
            .with_timezone(&Local);
        for session in every_record_sessions() {
            let storage = session.row.source.provider().storage().cache();
            cache.admit_storage(storage).unwrap();
            let root = SessionRoot::new(
                crate::history::environment::test_support::directory_holding_files(
                    &session.row.path,
                    &session.row.subagents,
                ),
            );
            let stub = SessionStub {
                locator: session.row.path.clone(),
                subagents: session.row.subagents.clone(),
                cache_key: "session".to_owned(),
                fingerprint: Fingerprint {
                    size: 1000,
                    modified: Some(std::time::UNIX_EPOCH),
                },
            };
            let stub = if session.row.source == crate::history::Source::OpenCode {
                let stamp = session
                    .row
                    .source
                    .provider()
                    .storage()
                    .current_fingerprint(&root, &session.row.path)
                    .unwrap()
                    .unwrap();
                SessionStub {
                    fingerprint: Fingerprint {
                        size: stamp.file_size,
                        modified: Some(
                            std::time::UNIX_EPOCH
                                + std::time::Duration::new(stamp.mtime_secs, stamp.mtime_nsecs),
                        ),
                    },
                    ..stub
                }
            } else {
                stub
            };
            let (record, row) = parsed_record_and_row(session.row.source, &root, &stub);
            cache.write(&record).unwrap();
            let CatalogueSession::Listed(cached) = cache
                .catalogue_session(storage, session.row.source, &root, &stub.cache_key, false)
                .unwrap()
                .unwrap()
            else {
                panic!("listed fixture")
            };
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
            let cached = [*cached];
            let not_read = SearchTextsInMemory::default();
            let in_memory = SearchTextsInMemory::read(&cached);
            let held = in_memory
                .get(&cached[0])
                .expect("the texts are read into memory");
            for window in &windows {
                for surface in [
                    SearchSurface::List(&not_read),
                    SearchSurface::List(&in_memory),
                    SearchSurface::Agent(window),
                ] {
                    let original_texts = match surface {
                        SearchSurface::List(_) => precompute_search_text(&original),
                        SearchSurface::Agent(_) => {
                            precompute_agent_search_text(&original, window).searchable
                        }
                    };
                    let cached_texts = match surface {
                        SearchSurface::List(_) => precompute_search_text(&cached),
                        SearchSurface::Agent(_) => {
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
                        let signature = |results: Vec<(usize, ScoreDebug)>| {
                            results
                                .into_iter()
                                .map(|(index, score)| {
                                    (
                                        index,
                                        score.total.to_bits(),
                                        score
                                            .fields
                                            .into_iter()
                                            .map(|field| {
                                                (
                                                    field.name,
                                                    field.tf_score.to_bits(),
                                                    field.adjacency_score.to_bits(),
                                                    field.word_details,
                                                )
                                            })
                                            .collect::<Vec<_>>(),
                                    )
                                })
                                .collect::<Vec<_>>()
                        };
                        assert_eq!(
                            signature(actual.results),
                            signature(expected.results),
                            "{} {query:?}, window {window:?}",
                            session.row.source.label()
                        );
                    }
                }
            }
            let mut queries_with_evidence = 0;
            for query in QUERIES {
                let parsed = ParsedQuery::parse(query);
                let read_from_cache = build_lexical_evidence(&cached[0], None, &parsed);
                let read_from_memory = build_lexical_evidence(&cached[0], Some(held), &parsed);
                queries_with_evidence += usize::from(read_from_memory.is_some());
                assert_eq!(
                    read_from_memory,
                    read_from_cache,
                    "{} {query:?}: the evidence in memory is the evidence in the cache",
                    session.row.source.label()
                );
                assert_eq!(
                    context_line(&cached[0], read_from_memory),
                    context_line(
                        &original[0],
                        build_lexical_evidence(&original[0], None, &parsed)
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

    /// The context line a row 80 columns wide shows for `evidence`.
    fn context_line(
        conversation: &Conversation,
        evidence: Option<LexicalEvidence>,
    ) -> Option<String> {
        let evidence = evidence?;
        crate::search::preview::build_context_segments_from_ranges(
            evidence
                .source_text
                .as_deref()
                .unwrap_or(&conversation.full_text),
            &evidence.context_ranges,
            80,
        )
    }
}
