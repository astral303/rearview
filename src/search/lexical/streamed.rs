//! Scores cached parts without retaining a session-sized search string.

use super::*;
use crate::error::Result;
use crate::history::sqlite_cache::{SearchField, SearchPart};

pub(super) struct BodyCounts {
    contains: Vec<bool>,
    prefixes: Vec<usize>,
    adjacency: usize,
    literals: Vec<bool>,
    has_text: bool,
}

impl BodyCounts {
    fn new(words: &[&str], literals: &[Literal]) -> Self {
        Self {
            contains: vec![false; words.len()],
            prefixes: vec![0; words.len()],
            adjacency: 0,
            literals: vec![false; literals.len()],
            has_text: false,
        }
    }

    fn push(&mut self, text: &str, words: &[&str], pairs: &[String], literals: &[Literal]) {
        self.has_text |= !text.is_empty();
        let normalized = normalize_for_search(text);
        for (index, word) in words.iter().enumerate() {
            self.contains[index] |= normalized.contains(word);
            self.prefixes[index] =
                (self.prefixes[index] + count_prefix_matches(&normalized, word, 10)).min(10);
        }
        self.adjacency = (self.adjacency + count_adjacent_pairs(&normalized, pairs, 3)).min(3);
        for (found, literal) in self.literals.iter_mut().zip(literals) {
            *found |= literal.matches(text);
        }
    }

    pub(super) fn matches_literals(&self) -> bool {
        self.literals.iter().all(|found| *found)
    }
}

pub(super) fn collect(
    conversation: &Conversation,
    surface: SearchSurface<'_>,
    words: &[&str],
    pairs: &[String],
    literals: &[Literal],
) -> Result<BodyCounts> {
    let window = match surface {
        SearchSurface::List => TimeFilter::default(),
        SearchSurface::Agent(window) => *window,
    };
    let mut counts = BodyCounts::new(words, literals);
    let read = conversation
        .cached_session
        .as_ref()
        .expect("cached row")
        .search_parts_within(conversation, &window, |part: SearchPart| {
            let visible = match part.field {
                SearchField::Full | SearchField::Leading => true,
                SearchField::Agent | SearchField::Thinking | SearchField::Supplement => {
                    matches!(surface, SearchSurface::Agent(_))
                }
                SearchField::Semantic => false,
            };
            let in_window = !window.is_active()
                || part.ordinal.is_some_and(|ordinal| {
                    window.contains_message(conversation.message_times.span(ordinal))
                });
            if visible && in_window {
                counts.push(&part.text, words, pairs, literals);
            }
        });
    if !matches!(read, Ok(Some(()))) {
        let content = conversation.load_content()?;
        counts = BodyCounts::new(words, literals);
        if window.is_active() {
            for text in content.texts_within(&window).into_array() {
                counts.push(&text, words, pairs, literals);
            }
        } else {
            counts.push(&content.full_text, words, pairs, literals);
            if matches!(surface, SearchSurface::Agent(_)) {
                counts.push(&content.agent_search_text, words, pairs, literals);
                counts.push(&content.thinking_and_tool_call_text, words, pairs, literals);
            }
        }
    }
    if !window.is_active() {
        let project = conversation.project_name.as_deref().unwrap_or_default();
        for (found, literal) in counts.literals.iter_mut().zip(literals) {
            *found |= literal.matches(project);
        }
    }
    Ok(counts)
}

pub(super) fn score(
    searchable: &SearchableConversation,
    body: BodyCounts,
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
                .map(|word| count_prefix_matches(text, word, 10))
                .collect::<Vec<_>>();
            fields.push(field_score(
                name,
                weight,
                words,
                &counts,
                count_adjacent_pairs(text, pairs, 3),
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

    #[test]
    fn cached_parts_preserve_scores_literals_and_windows_for_every_provider() {
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
                    &SessionRecord::listed(storage, session.row.source, &root, &stub, &session.row)
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
            let second = Local.timestamp_opt(first_second, 0).single().unwrap();
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
            for window in &windows {
                for surface in [SearchSurface::List, SearchSurface::Agent(window)] {
                    let original_texts = match surface {
                        SearchSurface::List => precompute_search_text(&original),
                        SearchSurface::Agent(_) => {
                            precompute_agent_search_text(&original, window).searchable
                        }
                    };
                    let cached_texts = match surface {
                        SearchSurface::List => precompute_search_text(&cached),
                        SearchSurface::Agent(_) => {
                            precompute_agent_search_text(&cached, window).searchable
                        }
                    };
                    for query in [
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
                    ] {
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
        }
    }
}
