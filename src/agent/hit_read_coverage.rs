//! `agent search` reads only each ranked session's hit messages: those the
//! ranking scan found holding every term retrieval requires. Over each
//! registered agent's every-record session:
//! - the hit messages hold every message retrieval finds on the whole
//!   transcript, for every term of each kind drawn from the session's own
//!   segments, in lexical and exact mode, without a window and with windows
//!   over middle messages. The session's stored texts are counted in memory
//!   as the ranking scan counts them, and a sample of the cases runs through
//!   the scan itself;
//! - for a sample of those terms, the output from the hit messages is the
//!   output from the whole transcript.

use crate::agent::refs::AgentConversationKey;
use crate::agent::retrieval::{
    AgentRetrievalOptions, AgentTranscriptSearchTarget, retrieve_agent_hits_for_target,
    segment_texts,
};
use crate::agent::search::{
    AgentSearchRequest, AgentSearchScope, SearchedTranscript, SessionReader, exact_phrase_query,
    format_agent_output, run_global_lexical_search_reporting,
};
use crate::agent::service::{
    ScopedSessions, lexically_rank_scoped, read_hit_messages, retrieval_terms,
};
use crate::agent::time_span::TimeSpan;
use crate::agent::transcript::AgentTranscript;
use crate::clock::{Clock, TestClock};
use crate::history::provider::contract_tests::every_record_sessions;
use crate::history::{Conversation, WaitStatus};
use crate::search::lexical::hit_messages_of_texts;
use crate::search::mode::SearchMode;
use crate::time_filter::TimeFilter;
use chrono::TimeZone;
use rayon::prelude::*;
use std::collections::BTreeSet;
use std::sync::Mutex;

/// Queries drawn from each kind of term, at most this many of each.
const QUERIES_PER_KIND: usize = 10;
/// The most cases per session and window that are also run through the real
/// scan, to compare its hit messages with the counter's.
const SCANNED_CASES_PER_WINDOW: usize = 2;

#[test]
fn hit_messages_give_whole_transcripts_hits_for_every_agent() {
    let clock = TestClock::default();
    let base = tempfile::tempdir().unwrap();
    let sessions = every_record_sessions()
        .iter()
        .map(|session| {
            let cached = crate::history::sqlite_cache::tests::cached_row(base.path(), session);
            let copy = cached
                .cached_session
                .as_ref()
                .expect("the row reads its copy");
            let whole = AgentTranscript::from_stored(
                &cached.path,
                copy.read_copy(|held| held.numbered_entries(None)).unwrap(),
            )
            .unwrap();
            let key = AgentConversationKey::from_conversation(
                &cached,
                &crate::history::path::CanonicalDirectories::of_system(),
            )
            .unwrap();
            ([cached], key, whole)
        })
        .collect::<Vec<_>>();
    let cases = sessions
        .iter()
        .flat_map(|(conversations, key, whole)| {
            let queries = queries(whole);
            windows(&conversations[0])
                .into_iter()
                .flat_map(move |window| {
                    queries
                        .clone()
                        .into_iter()
                        .map(move |(query, mode)| (query, mode, window))
                })
                .map(move |(query, mode, window)| (conversations, key, whole, query, mode, window))
        })
        .collect::<Vec<_>>();
    let outcomes = cases
        .into_par_iter()
        .map(|(conversations, key, whole, query, mode, window)| {
            let label = conversations[0].source.label();
            let search = SearchCase {
                conversations,
                key,
                whole,
                clock: &clock,
                query: &query,
                mode,
                window,
            };
            let (from_hits, from_whole, read) = search.both_ways();
            let mut differing = Vec::new();
            if from_hits != from_whole {
                differing.push(format!(
                    "{label} {mode:?} {query:?} {window:?}:\nhit messages:\n{from_hits}\
                     whole transcript:\n{from_whole}"
                ));
            }
            let narrowed = read.is_some_and(|(messages, message_count)| {
                if message_count != whole.messages.len() {
                    differing.push(format!(
                        "{label}: the hit read's message count {message_count} is the \
                         transcript's {}",
                        whole.messages.len()
                    ));
                }
                messages < whole.messages.len()
            });
            (differing, narrowed)
        })
        .collect::<Vec<_>>();
    let differing = outcomes
        .iter()
        .flat_map(|(differing, _)| differing)
        .cloned()
        .collect::<Vec<_>>();
    assert!(differing.is_empty(), "{}", differing.join("\n\n"));
    assert!(
        outcomes.iter().any(|(_, narrowed)| *narrowed),
        "some query reads fewer messages than a whole read"
    );
}

/// A literal ending in a space matches a message's text ending in that
/// space, as retrieval matches it, though the session's ranking matches the
/// text without the whitespace at its edges.
#[test]
fn literal_ending_in_space_selects_message_whose_text_ends_in_it() {
    use crate::search::test_fixtures::{cached_claude_sessions, claude_user_record, october};
    let sessions = cached_claude_sessions(&[(
        "session",
        vec![
            claude_user_record("needle ", october(10, 12)),
            claude_user_record("needle tail", october(10, 13)),
        ],
    )]);
    let ranking = lexically_rank_scoped(
        &sessions.rows,
        "\"needle \"",
        ScopedSessions {
            indices: &[0],
            scope: AgentSearchScope::Global,
        },
        SearchMode::Exact,
        &TimeFilter::default(),
        &TestClock::default(),
    )
    .unwrap();
    assert_eq!(ranking.ranked, [0], "the second message ranks the session");
    assert_eq!(
        ranking.hit_messages.of(0).unwrap().ordinals,
        [1, 2],
        "both messages hold the literal"
    );
}

#[test]
fn hit_messages_hold_every_message_retrieval_finds_for_every_agent() {
    let clock = TestClock::default();
    let base = tempfile::tempdir().unwrap();
    let sessions = every_record_sessions()
        .iter()
        .map(|session| {
            let cached = crate::history::sqlite_cache::tests::cached_row(base.path(), session);
            let copy = cached
                .cached_session
                .as_ref()
                .expect("the row reads its copy");
            let whole = AgentTranscript::from_stored(
                &cached.path,
                copy.read_copy(|held| held.numbered_entries(None)).unwrap(),
            )
            .unwrap();
            ([cached], whole)
        })
        .collect::<Vec<_>>();
    // Each session's stored texts, read once per window, with every query
    // drawn from its segments inside the window.
    let cases = sessions
        .iter()
        .flat_map(|(conversations, whole)| {
            windows_over_middle_messages(whole)
                .into_iter()
                .map(move |window| {
                    let texts = stored_texts(&conversations[0], window);
                    let queries = every_drawn_query(whole, window);
                    (conversations, whole, window, texts, queries)
                })
        })
        .collect::<Vec<_>>();
    let counted = cases
        .par_iter()
        .flat_map(|(conversations, whole, window, texts, queries)| {
            let step = queries.len().div_ceil(SCANNED_CASES_PER_WINDOW).max(1);
            queries
                .par_iter()
                .enumerate()
                .map(move |(index, (query, mode))| {
                    let selected = hit_messages_of_texts(
                        query,
                        &retrieval_terms(query, *mode),
                        texts
                            .iter()
                            .map(|(ordinal, text)| (*ordinal, text.as_str())),
                    );
                    let retrieved = retrieved_messages(whole, query, *mode, *window);
                    let label = conversations[0].source.label();
                    let missed = retrieved
                        .iter()
                        .filter(|ordinal| !selected.contains(ordinal))
                        .collect::<Vec<_>>();
                    let miss = (!missed.is_empty()).then(|| {
                        format!(
                            "{label} {mode:?} {query:?} {window:?}: retrieval finds messages \
                             {missed:?} outside the hit messages {selected:?}"
                        )
                    });
                    let sample = (index % step == 0).then_some((
                        *conversations,
                        query,
                        *mode,
                        *window,
                        selected,
                    ));
                    (label, !retrieved.is_empty(), miss, sample)
                })
        })
        .collect::<Vec<_>>();
    let misses = counted
        .iter()
        .filter_map(|(_, _, miss, _)| miss.clone())
        .collect::<Vec<_>>();
    assert!(misses.is_empty(), "{}", misses.join("\n"));
    let retrieving = counted
        .iter()
        .filter(|(_, retrieves, _, _)| *retrieves)
        .map(|(label, _, _, _)| *label)
        .collect::<BTreeSet<_>>();
    let sampled = counted
        .into_iter()
        .filter_map(|(_, _, _, sample)| sample)
        .collect::<Vec<_>>();
    for (conversations, _) in &sessions {
        let label = conversations[0].source.label();
        assert!(
            retrieving.contains(label),
            "{label}: some query retrieves a message"
        );
    }
    let scanned_differently = sampled
        .into_par_iter()
        .filter_map(|(conversations, query, mode, window, selected)| {
            let ranking = lexically_rank_scoped(
                conversations,
                query,
                ScopedSessions {
                    indices: &[0],
                    scope: AgentSearchScope::Global,
                },
                mode,
                &window,
                &clock,
            )
            .unwrap();
            let scanned = ranking.hit_messages.of(0).map(|hits| hits.ordinals.clone());
            (!ranking.ranked.is_empty() && scanned.as_ref() != Some(&selected)).then(|| {
                format!(
                    "{} {mode:?} {query:?} {window:?}: the scan selects {scanned:?}, \
                     the counted texts {selected:?}",
                    conversations[0].source.label()
                )
            })
        })
        .collect::<Vec<_>>();
    assert!(
        scanned_differently.is_empty(),
        "{}",
        scanned_differently.join("\n")
    );
}

/// A semantic hit's anchors, times and `read` flags come from its focus
/// messages read alone; each must be the message a whole read holds.
#[test]
fn messages_read_alone_are_the_whole_transcripts_messages_for_every_agent() {
    let base = tempfile::tempdir().unwrap();
    let mut differing = Vec::new();
    for session in every_record_sessions() {
        let cached = crate::history::sqlite_cache::tests::cached_row(base.path(), &session);
        let copy = cached.cached_session.clone().unwrap();
        let whole = AgentTranscript::from_stored(
            &cached.path,
            copy.read_copy(|held| held.numbered_entries(None)).unwrap(),
        )
        .unwrap();
        let ordinals = whole
            .messages
            .iter()
            .map(|message| message.ordinal)
            .collect::<Vec<_>>();
        let selections = ordinals
            .iter()
            .map(|ordinal| vec![*ordinal])
            .chain(ordinals.windows(2).map(<[usize]>::to_vec))
            .chain(std::iter::once(
                ordinals.iter().step_by(2).copied().collect(),
            ));
        for selection in selections {
            let read = copy
                .read_copy(|held| held.agent_messages(&cached.path, &selection))
                .expect("the copy is read");
            let expected = whole
                .messages
                .iter()
                .filter(|message| selection.contains(&message.ordinal))
                .cloned()
                .collect::<Vec<_>>();
            if read.transcript.messages != expected {
                differing.push(format!(
                    "{} {selection:?}:\nread alone: {:#?}\nwhole: {expected:#?}",
                    session.row.source.label(),
                    read.transcript.messages
                ));
            }
        }
    }
    assert!(differing.is_empty(), "{}", differing.join("\n\n"));
}

#[test]
fn session_replaced_after_ranking_reads_hit_messages_of_its_new_copy() {
    let base = tempfile::tempdir().unwrap();
    let session = every_record_sessions().remove(0);
    let cached = crate::history::sqlite_cache::tests::cached_row(base.path(), &session);
    let copy = cached.cached_session.clone().unwrap();
    let whole = AgentTranscript::from_stored(
        &cached.path,
        copy.read_copy(|held| held.numbered_entries(None)).unwrap(),
    )
    .unwrap();
    let key = AgentConversationKey::from_conversation(
        &cached,
        &crate::history::path::CanonicalDirectories::of_system(),
    )
    .unwrap();
    let conversations = [cached];
    let clock = TestClock::default();
    let (word, _) = queries(&whole).remove(0);
    let ranking = lexically_rank_scoped(
        &conversations,
        &word,
        ScopedSessions {
            indices: &[0],
            scope: AgentSearchScope::Global,
        },
        SearchMode::Lexical,
        &TimeFilter::default(),
        &clock,
    )
    .unwrap();
    let hits = ranking.hit_messages.of(0).unwrap();
    assert!(!hits.ordinals.is_empty(), "the query finds hit messages");

    crate::history::sqlite_cache::tests::replace_copy(&copy);
    let read = read_hit_messages(
        &clock,
        &WaitStatus::default(),
        &ranking,
        &conversations[0],
        hits,
        &key,
    )
    .unwrap()
    .expect("a copy replaced after the ranking is read");

    let expected = whole
        .messages
        .iter()
        .filter(|message| hits.ordinals.contains(&message.ordinal))
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(
        read.transcript.messages, expected,
        "the new copy's hit messages are read"
    );
}

/// One query's search of one session.
struct SearchCase<'a> {
    conversations: &'a [Conversation; 1],
    key: &'a AgentConversationKey,
    whole: &'a AgentTranscript,
    clock: &'a TestClock,
    query: &'a str,
    mode: SearchMode,
    window: TimeFilter,
}

impl SearchCase<'_> {
    /// The output from the hit messages, the output from the whole
    /// transcript, and the hit read's message and message counts when it
    /// read.
    fn both_ways(&self) -> (String, String, Option<(usize, usize)>) {
        let request = AgentSearchRequest {
            query: self.query.to_owned(),
            top: 100,
            cli_mode: Some(self.mode),
            config_mode: None,
            tui_semantic_search: None,
            flat: true,
            hits_per_conversation: 100,
            retrieval_hits_per_conversation: None,
            all_hits: true,
            budget: None,
            session_offset: 0,
            window: self.window,
        };
        let ranking = lexically_rank_scoped(
            self.conversations,
            self.query,
            ScopedSessions {
                indices: &[0],
                scope: AgentSearchScope::Global,
            },
            request.mode(),
            &self.window,
            self.clock,
        )
        .unwrap();
        let keys = std::slice::from_ref(self.key);
        let read = Mutex::new(None);
        let wait = WaitStatus::default();
        let from_hits = run_global_lexical_search_reporting(
            &request,
            self.conversations,
            keys,
            &ranking.ranked,
            SessionReader {
                read: |index, key: &AgentConversationKey| {
                    let hits = ranking.hit_messages.of(index).expect("the scan counted it");
                    let searched = read_hit_messages(
                        self.clock as &dyn Clock,
                        &wait,
                        &ranking,
                        &self.conversations[index],
                        hits,
                        key,
                    )?
                    .expect("the cache holds the session");
                    *read.lock().unwrap() =
                        Some((searched.transcript.messages.len(), searched.message_count));
                    Ok(Some(searched))
                },
                bytes: |_, key: &AgentConversationKey| key.transcript_bytes,
            },
            None,
        )
        .unwrap();
        let from_whole = run_global_lexical_search_reporting(
            &request,
            self.conversations,
            keys,
            &ranking.ranked,
            SessionReader {
                read: |_, _: &AgentConversationKey| {
                    Ok(Some(SearchedTranscript::from(self.whole.clone())))
                },
                bytes: |_, key: &AgentConversationKey| key.transcript_bytes,
            },
            None,
        )
        .unwrap();
        (
            format_agent_output(&from_hits.output),
            format_agent_output(&from_whole.output),
            read.into_inner().unwrap(),
        )
    }
}

/// No window, and a window on the second of the session's first message.
fn windows(row: &Conversation) -> Vec<TimeFilter> {
    let mut windows = vec![TimeFilter::default()];
    if let Some(span) = row.message_times.span(1) {
        let second = crate::clock::Zone::System
            .timestamp_opt(span.first_second(), 0)
            .single()
            .unwrap();
        windows.push(TimeFilter {
            after: Some(second),
            before: Some(second),
        });
    }
    windows
}

/// Words, adjacent word pairs, quoted phrases and identifiers from the
/// segments `transcript`'s search reads, each in lexical and exact mode.
fn queries(transcript: &AgentTranscript) -> Vec<(String, SearchMode)> {
    let segments = segment_texts(transcript, TimeFilter::default());
    let mut words = BTreeSet::new();
    let mut pairs = BTreeSet::new();
    let mut phrases = BTreeSet::new();
    let mut identifiers = BTreeSet::new();
    for segment in &segments {
        let segment_words = segment
            .split(|ch: char| !ch.is_alphanumeric())
            .filter(|word| word.chars().count() >= 2)
            .collect::<Vec<_>>();
        words.extend(segment_words.iter().map(|word| (*word).to_owned()));
        pairs.extend(
            segment_words
                .windows(2)
                .map(|pair| format!("{} {}", pair[0], pair[1])),
        );
        let tokens = segment.split_whitespace().collect::<Vec<_>>();
        phrases.extend(
            tokens
                .windows(2)
                .filter(|pair| pair.iter().all(|token| !token.contains('"')))
                .map(|pair| format!("\"{} {}\"", pair[0], pair[1])),
        );
        identifiers.extend(
            tokens
                .iter()
                .filter(|token| token.contains('_') && !token.contains('"'))
                .map(|token| (*token).to_owned()),
        );
    }
    [words, pairs, phrases, identifiers]
        .into_iter()
        .flat_map(spread)
        .flat_map(|query| {
            [SearchMode::Lexical, SearchMode::Exact].map(|mode| (query.clone(), mode))
        })
        .collect()
}

/// Every query of each kind drawn from the segments `transcript`'s search
/// reads within `window`, each in the modes it reaches retrieval through:
/// - words and identifiers, in lexical and exact mode;
/// - the first half of each word, adjacent word pairs, a quoted token beside
///   a bare one, and a bare token before an unfinished quote, in lexical
///   mode. In exact mode the last two retrieve the phrase of both tokens.
/// - a word without its first and last characters, in exact mode;
/// - adjacent tokens as one phrase, and a segment's first token quoted with
///   a space before it and its last with a space after it, which run in
///   exact mode whatever the mode asked.
fn every_drawn_query(
    transcript: &AgentTranscript,
    window: TimeFilter,
) -> Vec<(String, SearchMode)> {
    // Each query with whether it runs in exact mode.
    let mut queries = BTreeSet::<(String, bool)>::new();
    let mut in_both_modes = |query: String| {
        queries.insert((query.clone(), false));
        queries.insert((query, true));
    };
    let mut lexical_only = Vec::new();
    let mut exact_only = Vec::new();
    for segment in segment_texts(transcript, window) {
        let words = segment
            .split(|ch: char| !ch.is_alphanumeric())
            .filter(|word| !word.is_empty())
            .collect::<Vec<_>>();
        for word in &words {
            in_both_modes((*word).to_owned());
            let chars = word.chars().collect::<Vec<_>>();
            if chars.len() >= 2 {
                lexical_only.push(chars[..chars.len().div_ceil(2)].iter().collect::<String>());
            }
            if chars.len() >= 3 {
                exact_only.push(chars[1..chars.len() - 1].iter().collect::<String>());
            }
        }
        for pair in words.windows(2) {
            lexical_only.push(format!("{} {}", pair[0], pair[1]));
        }
        let tokens = segment
            .split_whitespace()
            .filter(|token| !token.contains('"'))
            .collect::<Vec<_>>();
        for token in &tokens {
            if token.contains('_') {
                in_both_modes((*token).to_owned());
            }
        }
        if let (Some(first), Some(last)) = (tokens.first(), tokens.last()) {
            lexical_only.push(format!("\" {first}\""));
            lexical_only.push(format!("\"{last} \""));
        }
        for pair in tokens.windows(2) {
            lexical_only.push(format!("\"{} {}\"", pair[0], pair[1]));
            lexical_only.push(format!("\"{}\" {}", pair[0], pair[1]));
            lexical_only.push(format!("{} \"{}", pair[0], pair[1]));
        }
    }
    queries.extend(lexical_only.into_iter().map(|query| (query, false)));
    queries.extend(exact_only.into_iter().map(|query| (query, true)));
    queries
        .into_iter()
        .map(|(query, exact)| {
            let mode = if exact {
                SearchMode::Exact
            } else {
                SearchMode::Lexical
            };
            (query, mode)
        })
        .collect()
}

/// `row`'s stored search texts within `window`, each with its message
/// number, as `agent search` reads them.
fn stored_texts(row: &Conversation, window: TimeFilter) -> Vec<(Option<usize>, String)> {
    let mut texts = Vec::new();
    row.cached_session
        .clone()
        .expect("the row reads its copy")
        .read_copy(|held| {
            held.visit_search_texts(
                crate::history::sqlite_cache::SearchedFields::Agent,
                &window,
                |text| {
                    texts.push((text.ordinal, text.text.to_owned()));
                    std::ops::ControlFlow::Continue(())
                },
            )
        })
        .expect("the cache holds the copy");
    texts
}

/// The messages retrieval finds for `query` in `mode` on the whole
/// `transcript` within `window`, every one of them.
fn retrieved_messages(
    transcript: &AgentTranscript,
    query: &str,
    mode: SearchMode,
    window: TimeFilter,
) -> BTreeSet<usize> {
    let query = match mode {
        SearchMode::Exact => exact_phrase_query(query),
        _ => query.to_owned(),
    };
    retrieve_agent_hits_for_target(
        AgentTranscriptSearchTarget::whole(transcript, None, None),
        &query,
        AgentRetrievalOptions {
            limit: usize::MAX,
            window,
            ..AgentRetrievalOptions::default()
        },
    )
    .into_iter()
    .map(|hit| hit.focus_range.start)
    .collect()
}

/// No window, a window over the middle third of `transcript`'s dated
/// messages, and one over its middle dated message alone.
fn windows_over_middle_messages(transcript: &AgentTranscript) -> Vec<TimeFilter> {
    let spans = transcript
        .messages
        .iter()
        .filter_map(TimeSpan::of_message)
        .collect::<Vec<_>>();
    let at = |second| {
        crate::clock::Zone::System
            .timestamp_opt(second, 0)
            .single()
            .unwrap()
    };
    let mut windows = vec![TimeFilter::default()];
    if let (Some(first), Some(last), Some(middle)) = (
        spans.get(spans.len() / 3),
        spans.get(spans.len() * 2 / 3),
        spans.get(spans.len() / 2),
    ) {
        windows.push(TimeFilter {
            after: Some(at(first.first_second())),
            before: Some(at(last.last_second())),
        });
        windows.push(TimeFilter {
            after: Some(at(middle.first_second())),
            before: Some(at(middle.last_second())),
        });
    }
    windows
}

/// At most [`QUERIES_PER_KIND`] of `terms`, spread across their order.
fn spread(terms: BTreeSet<String>) -> Vec<String> {
    let step = terms.len().div_ceil(QUERIES_PER_KIND).max(1);
    terms.into_iter().step_by(step).collect()
}
