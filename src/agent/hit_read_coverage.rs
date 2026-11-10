//! `agent search` reads only each ranked session's hit messages: those the
//! ranking scan found holding every term retrieval requires. Over each
//! registered agent's every-record session, its hits from those messages are
//! the hits from the whole transcript, for words, adjacent words, phrases and
//! identifiers drawn from the session's own segments, in lexical and exact
//! mode, with and without a time window.

use crate::agent::refs::AgentConversationKey;
use crate::agent::retrieval::segment_texts;
use crate::agent::search::{
    AgentSearchRequest, SearchedTranscript, format_agent_output,
    run_global_lexical_search_reporting,
};
use crate::agent::service::{lexically_rank_scoped, read_hit_messages};
use crate::agent::transcript::AgentTranscript;
use crate::clock::{Clock, TestClock};
use crate::history::provider::contract_tests::every_record_sessions;
use crate::history::{Conversation, WaitStatus};
use crate::search::mode::SearchMode;
use crate::time_filter::TimeFilter;
use chrono::TimeZone;
use rayon::prelude::*;
use std::collections::BTreeSet;
use std::sync::Mutex;

/// Queries drawn from each kind of term, at most this many of each.
const QUERIES_PER_KIND: usize = 10;

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
                copy.numbered_entries(None).unwrap().unwrap(),
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
            copy.numbered_entries(None).unwrap().unwrap(),
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
            let Some(crate::history::sqlite_cache::CopyRead::Read(read)) =
                copy.agent_messages(&cached.path, &selection).unwrap()
            else {
                panic!("the copy is read");
            };
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
    let whole =
        AgentTranscript::from_stored(&cached.path, copy.numbered_entries(None).unwrap().unwrap())
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
        &[0],
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
            &[0],
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
            |index, key| {
                let hits = ranking.hit_messages.of(index).expect("the scan counted it");
                let searched = read_hit_messages(
                    self.clock as &dyn Clock,
                    &wait,
                    &ranking,
                    &self.conversations[index],
                    hits,
                    key,
                )?;
                *read.lock().unwrap() =
                    Some((searched.transcript.messages.len(), searched.message_count));
                Ok(searched)
            },
            None,
        )
        .unwrap();
        let from_whole = run_global_lexical_search_reporting(
            &request,
            self.conversations,
            keys,
            &ranking.ranked,
            |_, _| Ok(SearchedTranscript::from(self.whole.clone())),
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
    let segments = segment_texts(transcript);
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

/// At most [`QUERIES_PER_KIND`] of `terms`, spread across their order.
fn spread(terms: BTreeSet<String>) -> Vec<String> {
    let step = terms.len().div_ceil(QUERIES_PER_KIND).max(1);
    terms.into_iter().step_by(step).collect()
}
