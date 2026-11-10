//! Scores a session's stored search texts, read from the session cache or
//! held in memory, without normalising them.

use super::raw::{ADJACENCY_CAP, Counts, PREFIX_CAP, RawMatcher, RawQuery};
use super::*;
use crate::error::{AppError, Result};
use crate::history::sqlite_cache::{CachedSession, SearchText, SearchedFields};
use std::ops::ControlFlow;

impl Counts {
    pub(super) fn matches_literals(&self) -> bool {
        self.literals.iter().all(|found| *found)
    }
}

pub(super) fn collect(
    conversation: &Conversation,
    surface: SearchSurface<'_>,
    query: &RawQuery<'_>,
) -> Result<Counts> {
    let (fields, window) = match surface {
        SearchSurface::List(_) => (SearchedFields::List, TimeFilter::default()),
        SearchSurface::Agent(window) => (SearchedFields::Agent, *window),
    };
    let mut matcher = query.matcher();
    let mut counts = if let Some(texts) = surface.texts_in_memory(conversation) {
        count_in_memory(texts, &mut matcher)
    } else {
        let cached = conversation.cached_session.as_ref().expect("cached row");
        match count_stored(cached, fields, &window, &mut matcher) {
            Ok(Some(counts)) => counts,
            // The copy as the cache holds it, as the session's content loads.
            Ok(None) | Err(_) => count_stored(
                &cached.clone().ignoring_source_changes().waiting(),
                fields,
                &window,
                &mut matcher,
            )?
            .ok_or_else(|| AppError::SessionCacheNotReady {
                session: conversation.session_id.clone(),
                reason: "the session is not in the session cache".to_owned(),
            })?,
        }
    };
    if !window.is_active() {
        let project = conversation.project_name.as_deref().unwrap_or_default();
        for (found, literal) in counts.literals.iter_mut().zip(query.literals()) {
            *found |= literal.matches(project);
        }
    }
    Ok(counts)
}

/// `matcher`'s counts over `cached`'s search texts in `fields` inside
/// `window`, read until more text cannot change them; `None` when the cache
/// no longer holds the copy.
fn count_stored(
    cached: &CachedSession,
    fields: SearchedFields,
    window: &TimeFilter,
    matcher: &mut RawMatcher<'_>,
) -> Result<Option<Counts>> {
    let mut counts = matcher.counts();
    let read = cached.visit_search_texts(fields, window, |text: SearchText<'_>| {
        matcher.count(text.text, &mut counts);
        if matcher.is_complete(&counts) {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    })?;
    Ok(read.map(|()| counts))
}

/// `matcher`'s counts over `texts`, read until more text cannot change them.
fn count_in_memory(texts: &SessionSearchTexts, matcher: &mut RawMatcher<'_>) -> Counts {
    let mut counts = matcher.counts();
    for text in texts.texts() {
        matcher.count(text, &mut counts);
        if matcher.is_complete(&counts) {
            break;
        }
    }
    counts
}

pub(super) fn score(
    searchable: &SearchableConversation,
    body: Counts,
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
    use crate::history::sqlite_cache::{CatalogueSession, SessionRecord, SqliteCache};
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
            let root = SessionRoot::new(session.row.path.parent().unwrap());
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
            cache
                .write(
                    &SessionRecord::listed(
                        crate::history::FOR_FIXTURES,
                        storage,
                        session.row.source,
                        &root,
                        &stub,
                        &session.row,
                    )
                    .unwrap(),
                )
                .unwrap();
            let CatalogueSession::Listed(cached) = cache
                .catalogue_session(
                    storage,
                    session.row.source,
                    &root.path,
                    &stub.cache_key,
                    false,
                )
                .unwrap()
                .unwrap()
            else {
                panic!("listed fixture")
            };
            let mut original = session.row.clone();
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

    const QUERIES: [&str; 11] = [
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
