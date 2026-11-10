//! Scores sessions' stored search texts, read from the session cache or held
//! in memory, without normalising them.

use super::hit_messages::MessageTerms;
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
/// memory, or else from the cache. Without a time window, the counts also
/// hold the row's title and summary, and a literal in a conversation's
/// project name counts as found. With `retrieval`, also the hit messages of
/// each conversation read from the cache.
pub(super) fn collect_all<C: Borrow<Conversation> + Sync>(
    conversations: &[C],
    indices: &[usize],
    surface: SearchSurface<'_>,
    query: &RawQuery<'_>,
    retrieval: Option<RetrievalTerms>,
) -> Result<(HashMap<usize, Counts>, HitMessages)> {
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
    let (stored_counts, hit_messages) =
        count_stored(conversations, &stored, surface, query, retrieval)?;
    counts.extend(stored_counts);
    let (_, window) = surface.stored_fields();
    if !window.is_active() {
        let mut matcher = query.matcher();
        for (index, counts) in &mut counts {
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
    Ok((counts, hit_messages))
}

/// `query`'s counts over the search texts of each conversation at `indices`,
/// read from the session cache in one scan, by index. A conversation's
/// counts hold the texts holding some of the query, and `has_text` is true
/// when any text it holds has more than whitespace.
fn count_stored<C: Borrow<Conversation>>(
    conversations: &[C],
    indices: &[usize],
    surface: SearchSurface<'_>,
    query: &RawQuery<'_>,
    retrieval: Option<RetrievalTerms>,
) -> Result<(HashMap<usize, Counts>, HitMessages)> {
    let matcher = query.matcher();
    let mut counts: HashMap<usize, Counts> = indices
        .iter()
        .map(|&index| (index, matcher.counts()))
        .collect();
    let mut hit_messages = HitMessages::default();
    if indices.is_empty() {
        return Ok((counts, hit_messages));
    }
    let copies = indices
        .iter()
        .map(|&index| cached_copy(conversations[index].borrow()))
        .collect::<Vec<_>>();
    let (fields, window) = surface.stored_fields();
    let scanned = scan_search_texts(&copies, fields, &window, || {
        BodyCounter::new(query, retrieval.is_some())
    })?;
    if let Some((_, error)) = scanned.missing.into_iter().next() {
        return Err(error);
    }
    let replaced = scanned.replaced.into_iter().collect::<HashMap<_, _>>();
    let mut message_terms: HashMap<usize, HashMap<usize, MessageTerms>> = HashMap::new();
    for counter in scanned.visitors {
        for (copy, found) in counter.bodies {
            if !replaced.contains_key(&copy)
                && let Some(session) = counts.get_mut(&indices[copy])
            {
                session.add(&found);
            }
        }
        for ((copy, ordinal), terms) in counter.messages.into_iter().flatten() {
            if !replaced.contains_key(&copy) {
                message_terms
                    .entry(copy)
                    .or_default()
                    .entry(ordinal)
                    .or_default()
                    .add(terms);
            }
        }
    }
    if let Some(retrieval) = retrieval {
        let required = query.required_terms(retrieval);
        for (copy, &copy_read) in copies.iter().enumerate() {
            if !replaced.contains_key(&copy) {
                let terms = message_terms.remove(&copy).unwrap_or_default();
                hit_messages.0.insert(
                    indices[copy],
                    SessionHitMessages::holding(copy_read.generation(), &terms, required),
                );
            }
        }
    }
    for (copy, generation) in replaced {
        let conversation = conversations[indices[copy]].borrow();
        let (counts_alone, messages) = count_alone(
            cached_copy(conversation).clone().of_generation(generation),
            surface,
            query,
        )?
        .ok_or_else(|| not_in_cache(conversation))?;
        counts.insert(indices[copy], counts_alone);
        if let Some(retrieval) = retrieval {
            hit_messages.0.insert(
                indices[copy],
                SessionHitMessages::holding(generation, &messages, query.required_terms(retrieval)),
            );
        }
    }
    Ok((counts, hit_messages))
}

fn not_in_cache(conversation: &Conversation) -> AppError {
    AppError::SessionCacheNotReady {
        session: conversation.session_id.clone(),
        reason: "the session is not in the session cache".to_owned(),
    }
}

/// `query`'s counts over `copy`'s search texts, read alone, and the terms
/// each message's texts hold: a copy another write gave the session after a
/// scan started. `None` when the cache no longer holds the copy.
fn count_alone(
    copy: CachedSession,
    surface: SearchSurface<'_>,
    query: &RawQuery<'_>,
) -> Result<Option<(Counts, HashMap<usize, MessageTerms>)>> {
    let (fields, window) = surface.stored_fields();
    let mut matcher = query.matcher();
    let mut counts = matcher.counts();
    let mut text_counts = matcher.counts();
    let mut messages: HashMap<usize, MessageTerms> = HashMap::new();
    let read =
        copy.ignoring_source_changes()
            .waiting()
            .read_search_texts(fields, &window, |text| {
                text_counts.clear();
                matcher.count(text.text, &mut text_counts);
                counts.add(&text_counts);
                if let Some(ordinal) = text.ordinal {
                    messages
                        .entry(ordinal)
                        .or_default()
                        .add(matcher.message_terms(&text_counts));
                }
                std::ops::ControlFlow::Continue(())
            })?;
    Ok(read.map(|()| (counts, messages)))
}

/// The hit messages of `copy` for `query` under `retrieval`, its texts read
/// alone: a copy another write gave a ranked session after the ranking scan.
/// `None` when the cache no longer holds the copy.
pub(super) fn hit_messages_alone(
    copy: CachedSession,
    window: &TimeFilter,
    query: &RawQuery<'_>,
    retrieval: RetrievalTerms,
) -> Result<Option<SessionHitMessages>> {
    let generation = copy.generation();
    Ok(
        count_alone(copy, SearchSurface::Agent(window), query)?.map(|(_, messages)| {
            SessionHitMessages::holding(generation, &messages, query.required_terms(retrieval))
        }),
    )
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
    /// When hit messages are wanted, the terms each message's kept texts
    /// hold, by the position of its session and its number.
    messages: Option<HashMap<(usize, usize), MessageTerms>>,
}

impl<'q> BodyCounter<'q> {
    fn new(query: &'q RawQuery<'q>, counts_messages: bool) -> Self {
        let matcher = query.matcher();
        let text = matcher.counts();
        Self {
            matcher,
            text,
            bodies: HashMap::new(),
            messages: counts_messages.then(HashMap::new),
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
        body_of(&mut self.bodies, &self.matcher, row.copy).add(&self.text);
        if let (Some(messages), Some(ordinal)) = (&mut self.messages, row.ordinal) {
            messages
                .entry((row.copy, ordinal))
                .or_default()
                .add(self.matcher.message_terms(&self.text));
        }
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
                &SearchTextsInMemory::default(),
                "zebrafish swim",
                october(20, 12),
                |_| true,
            )
            .map(|search| {
                search
                    .results
                    .into_iter()
                    .map(|(index, score)| (index, score.total.to_bits()))
                    .collect::<Vec<_>>()
            })
        };
        let before = ranked().unwrap();
        crate::history::sqlite_cache::tests::replace_copy(rows[0].cached_session.as_ref().unwrap());
        assert_eq!(
            ranked().expect("a replaced session doesn't fail the search"),
            before,
            "the replaced session ranks on its new copy's texts"
        );
    }

    #[test]
    fn session_matching_only_its_project_name_scores_its_body_as_held_texts_do() {
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
                let names = score
                    .fields
                    .iter()
                    .map(|field| field.name)
                    .collect::<Vec<_>>();
                (index, names)
            })
            .collect::<Vec<_>>()
        };
        let from_cache = fields(&SearchTextsInMemory::default());
        assert_eq!(
            from_cache,
            fields(&SearchTextsInMemory::read(&rows)),
            "the scan's sessions score the fields their held texts score"
        );
        assert!(
            from_cache.contains(&(0, vec!["project", "body"])),
            "a session holding text scores its body: {from_cache:?}"
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

    /// The context line a row 80 columns wide shows for `evidence`.
    fn context_line(
        conversation: &Conversation,
        evidence: Option<LexicalEvidence>,
    ) -> Option<String> {
        let evidence = evidence?;
        crate::search::preview::build_context_segments_from_ranges(
            &evidence.texts(conversation),
            &evidence.context_ranges,
            80,
        )
    }
}
