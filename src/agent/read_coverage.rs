//! `agent read` over each registered agent's every-record session.
//! - With `--tools --subagents`, each marker in a call's header, a launch
//!   prompt or a sub-agent's message shows. Tool results stay hidden, so a
//!   marker in a call's header cannot show through the call's result.
//! - With `--subagents --thinking`, each marker in a sub-agent's thinking
//!   shows; with `--subagents` alone, none does.
//! - With default flags, the header counts the hidden tool calls, and the
//!   read covers each message a read with every flag covers: a message, call
//!   or hidden record holds it. A read with every flag does not report a
//!   hidden part.
//! - With every flag and half its length as the budget, a read ends on a
//!   whole record, without being cut short at the budget, and covers the
//!   unbudgeted read's first messages, from the first, without a gap.
//! - With default flags and half its length as the budget, the reads a
//!   read's `continue read` record names, with its flags, show each message
//!   with text the budget omits.
//! - Each line of a read or an outline is a record of a kind the grammar
//!   defines, or content below a record opening with its address. A record
//!   carries its kind's atoms in the order the writer prints them, each
//!   value in its atom's form. Addresses count from 1. Runs of messages and
//!   calls, line ranges, a record's time span and an `at=`/`to=` pair are in
//!   order.
//! - A call and its result name each other, and a result in its call's own
//!   message prints after the call's record.
//! - Each record of a read or an outline holds one owner's messages, the
//!   session's or a sub-agent's, and names a sub-agent by the handle its
//!   legend record names. An outline segment names each sub-agent whose
//!   messages it shows, and no other.
//! - A read of one sub-agent shows exactly its messages, its launch and its
//!   reports. Each segment of its outline reads back exactly its messages.
//! - Handles number the sub-agents in the order they start: at their
//!   launch, else at their first message.
//! - When exact search finds a marker in a message, a read of that message
//!   alone with `--match <marker>` reports a hit.
//! - The `read ref=` recipe of each hit an exact `agent within` search for a
//!   marker prints reads the hit's `focus=` messages.

use crate::agent::address::MessageAddresses;
use crate::agent::protocol::{
    ProtocolOptions, ReadRequest, ReadSlice, format_outline, format_read,
};
use crate::agent::refs::{
    AgentConversationKey, MessageRange, ResolvedConversation, parse_messages, parse_read_ref,
    resolved_conversation_for_key,
};
use crate::agent::retrieval::{
    AgentRetrievalOptions, AgentTranscriptSearchTarget, retrieve_agent_hits_for_target,
};
use crate::agent::search::{
    AgentWithinRequest, exact_phrase_query, format_agent_output, run_within_search,
};
use crate::agent::test_support::{
    MessageKey, PositionalRecord, addressed_messages, atom, message_address, outline_records,
    read_records, segment_records,
};
use crate::agent::transcript::AgentTranscript;
use crate::history::provider::contract_tests::{AgentSession, every_record_sessions};
use crate::search::mode::SearchMode;
use crate::time_filter::TimeFilter;
use std::collections::{BTreeSet, HashMap};

const WITH_TOOLS_AND_SUBAGENTS: ProtocolOptions = ProtocolOptions {
    budget: None,
    tools: true,
    tool_results: false,
    thinking: false,
    subagents: true,
    window: TimeFilter {
        after: None,
        before: None,
    },
};

const DEFAULT_FLAGS: ProtocolOptions = ProtocolOptions {
    budget: None,
    tools: false,
    tool_results: false,
    thinking: false,
    subagents: false,
    window: TimeFilter {
        after: None,
        before: None,
    },
};

const EVERY_FLAG: ProtocolOptions = ProtocolOptions {
    budget: None,
    tools: true,
    tool_results: true,
    thinking: true,
    subagents: true,
    window: TimeFilter {
        after: None,
        before: None,
    },
};

const EVERY_FLAG_BUT_SUBAGENTS: ProtocolOptions = ProtocolOptions {
    subagents: false,
    ..EVERY_FLAG
};

const WITH_SUBAGENTS: ProtocolOptions = ProtocolOptions {
    subagents: true,
    ..DEFAULT_FLAGS
};

const WITH_SUBAGENTS_AND_THINKING: ProtocolOptions = ProtocolOptions {
    thinking: true,
    ..WITH_SUBAGENTS
};

const TOOL_RESULTS_ALONE: ProtocolOptions = ProtocolOptions {
    tools: false,
    tool_results: true,
    ..EVERY_FLAG
};

/// The key of `row`, listed in this test's own session cache.
fn key_of_listed(row: &crate::history::Conversation) -> AgentConversationKey {
    let mut row = row.clone();
    crate::history::environment::test_support::cache_rows(std::slice::from_mut(&mut row));
    AgentConversationKey::from_conversation(
        &row,
        &crate::history::path::CanonicalDirectories::of_system(),
    )
    .unwrap()
}

/// `agent outline` of `session`, with every part shown.
fn outline(session: &AgentSession) -> String {
    let row = &session.row;
    let transcript =
        AgentTranscript::load_cached_for_test(row.source, &row.path, &row.subagents).unwrap();
    let key = key_of_listed(row);
    let resolved = resolved_conversation_for_key(std::slice::from_ref(&key), &key);
    format_outline(&resolved, &transcript, EVERY_FLAG)
}

/// `agent read` of the whole of `session`, with its sub-agent transcripts.
fn read(session: &AgentSession, options: ProtocolOptions) -> String {
    let row = &session.row;
    let transcript =
        AgentTranscript::load_cached_for_test(row.source, &row.path, &row.subagents).unwrap();
    let key = key_of_listed(row);
    let resolved = resolved_conversation_for_key(std::slice::from_ref(&key), &key);
    format_read(
        &[ReadRequest {
            resolved: &resolved,
            transcript: &transcript,
            range: None,
            subagent: None,
            call: None,
            shortened_to: None,
        }],
        None,
        None,
        options,
    )
    .unwrap()
}

/// `agent read` of `reference`, `ch_…:m3..m5` or `ch_…:s3:m2`, a ref of
/// `session` a read printed.
fn read_ref(session: &AgentSession, reference: &str, options: ProtocolOptions) -> String {
    LoadedSession::of(session).read_ref(reference, options)
}

/// A session's transcript, loaded once for many reads of its refs.
struct LoadedSession {
    transcript: AgentTranscript,
    resolved: ResolvedConversation,
    addresses: MessageAddresses,
}

impl LoadedSession {
    fn of(session: &AgentSession) -> Self {
        let row = &session.row;
        let transcript =
            AgentTranscript::load_cached_for_test(row.source, &row.path, &row.subagents).unwrap();
        let key = key_of_listed(row);
        Self {
            resolved: resolved_conversation_for_key(std::slice::from_ref(&key), &key),
            addresses: MessageAddresses::of_transcript(&transcript),
            transcript,
        }
    }

    /// `agent read` of `reference`, as [`read_ref`] reads it.
    fn read_ref(&self, reference: &str, options: ProtocolOptions) -> String {
        let read_ref = parse_read_ref(reference).unwrap();
        let range = read_ref
            .range
            .map(|range| self.addresses.resolve(range).unwrap());
        format_read(
            &[ReadRequest {
                resolved: &self.resolved,
                transcript: &self.transcript,
                range: range.map(|range| range.range),
                subagent: read_ref.subagent.or(range.and_then(|range| range.subagent)),
                call: read_ref.call,
                shortened_to: range.and_then(|range| range.shortened_to),
            }],
            None,
            None,
            options,
        )
        .unwrap()
    }
}

/// Each marker of `session` in a call's header, a launch prompt or a
/// sub-agent's message that is missing from `agent read --tools
/// --subagents`.
fn unshown_call_and_subagent_markers(session: &AgentSession) -> Vec<String> {
    let output = read(session, WITH_TOOLS_AND_SUBAGENTS);
    session
        .markers
        .iter()
        .filter(|marker| marker.is_in_call_or_subagent)
        .filter(|marker| !output.contains(marker.text))
        .map(|marker| format!("{:?}: {}", session.row.source, marker.record))
        .collect()
}

/// The addresses of `session`'s messages, and the ordinal of each message by
/// the address its records print.
struct SessionAddresses {
    addresses: MessageAddresses,
    ordinals: HashMap<MessageKey, usize>,
    /// The thread of each sub-agent message, by ordinal: its label and
    /// transcript.
    threads: HashMap<usize, String>,
}

impl SessionAddresses {
    fn of(session: &AgentSession) -> Self {
        let row = &session.row;
        let transcript =
            AgentTranscript::load_cached_for_test(row.source, &row.path, &row.subagents).unwrap();
        let addresses = MessageAddresses::of_transcript(&transcript);
        let ordinals = transcript
            .messages
            .iter()
            .map(|message| {
                let address = addresses.message(message.ordinal);
                (addressed_messages(&address)[0], message.ordinal)
            })
            .collect();
        let threads = transcript
            .messages
            .iter()
            .filter_map(|message| {
                let (label, transcript) = message.subagent.as_deref()?.key();
                Some((message.ordinal, format!("{label} {transcript:?}")))
            })
            .collect();
        Self {
            addresses,
            ordinals,
            threads,
        }
    }

    /// The ordinals of the messages a record's `address` names.
    fn ordinals_of(&self, address: &str) -> Vec<usize> {
        addressed_messages(address)
            .into_iter()
            .map(|key| self.ordinals[&key])
            .collect()
    }

    /// The ordinals of the messages a segment's `address` covers.
    fn segment_ordinals(&self, address: &str) -> std::ops::RangeInclusive<usize> {
        let range = self
            .addresses
            .resolve(parse_messages(address).unwrap())
            .unwrap()
            .range;
        range.start..=range.end
    }
}

/// The ordinals of the messages a read covers: those of its message and call
/// records, and the ranges of its hidden records.
fn covered_messages(output: &str, addresses: &SessionAddresses) -> BTreeSet<usize> {
    read_records(output)
        .iter()
        .flat_map(|record| addresses.ordinals_of(record.address))
        .collect()
}

/// The ordinals of the messages a read's call records name in `result=`.
fn result_messages(output: &str, addresses: &SessionAddresses) -> BTreeSet<usize> {
    read_records(output)
        .iter()
        .filter_map(|record| record.atom("result"))
        .flat_map(|results| results.split(','))
        .flat_map(|address| addresses.ordinals_of(address))
        .collect()
}

/// Each message of `session` that a read with every flag covers and a read
/// with default flags does not.
fn uncovered_messages(session: &AgentSession) -> Vec<String> {
    let addresses = SessionAddresses::of(session);
    let covered = covered_messages(&read(session, DEFAULT_FLAGS), &addresses);
    covered_messages(&read(session, EVERY_FLAG), &addresses)
        .difference(&covered)
        .map(|&ordinal| {
            format!(
                "{:?}: {}",
                session.row.source,
                addresses.addresses.message(ordinal)
            )
        })
        .collect()
}

/// The launch, first and report messages a legend record names.
#[derive(Default)]
struct LegendLinks {
    launch: Option<MessageKey>,
    /// The first message of `span=`.
    first: Option<MessageKey>,
    reports: Vec<MessageKey>,
}

/// The handles a read's legend records name, with the launch and report
/// messages each one names.
fn legend(output: &str) -> HashMap<String, LegendLinks> {
    let ordinals = |values: &str| {
        values
            .split(',')
            .map(|address| addressed_messages(address)[0])
            .collect::<Vec<_>>()
    };
    output
        .lines()
        .filter_map(|line| line.strip_prefix("subagent "))
        .map(|record| {
            let (fields, _name) = record.split_once(" : ").unwrap();
            let mut fields = fields.split_whitespace();
            let handle = fields.next().unwrap().to_owned();
            let mut links = LegendLinks::default();
            for field in fields {
                if let Some(launch) = field.strip_prefix("launch=") {
                    links.launch = ordinals(launch).first().copied();
                } else if let Some(span) = field.strip_prefix("span=") {
                    links.first = Some(addressed_messages(span)[0]);
                } else if let Some(reports) = field.strip_prefix("report=") {
                    links.reports = ordinals(reports);
                }
            }
            (handle, links)
        })
        .collect()
}

/// The records of an output that open with an address: [`read_records`] or
/// [`outline_records`].
type RecordParser = fn(&str) -> Vec<PositionalRecord<'_>>;

/// The records of `session`'s reads with every flag and with default flags,
/// and of its outline, that name a sub-agent wrongly: a record holding
/// messages of two owners (the session or a sub-agent), a sub-agent
/// message's record without a handle, a handle on a record of the session's
/// own, a handle no legend record names, or one thread under two handles;
/// an outline segment whose handles do not name exactly the sub-agents of the
/// messages it shows; and how many handles the read with every flag names in
/// its legend, and how many the outline's segments name.
fn misnamed_subagents(session: &AgentSession) -> (Vec<String>, usize, usize) {
    let addresses = SessionAddresses::of(session);
    let threads = &addresses.threads;
    let outputs: [(String, RecordParser); 3] = [
        (read(session, EVERY_FLAG), read_records),
        (read(session, DEFAULT_FLAGS), read_records),
        (outline(session), outline_records),
    ];
    let mut gaps = Vec::new();
    for (output, records) in &outputs {
        let legend = legend(output);
        let mut handle_of_thread = HashMap::new();
        gaps.extend(
            records(output)
                .iter()
                .filter(|record| {
                    let owners = addresses
                        .ordinals_of(record.address)
                        .into_iter()
                        .map(|ordinal| threads.get(&ordinal))
                        .collect::<BTreeSet<_>>();
                    if owners.len() != 1 {
                        return true;
                    }
                    match (owners.first().copied().flatten(), record.subagent) {
                        (Some(thread), Some(handle)) => {
                            !legend.contains_key(handle)
                                || *handle_of_thread.entry(thread).or_insert(handle) != handle
                        }
                        (None, None) => false,
                        _ => true,
                    }
                })
                .map(|record| format!("{:?}: {}", session.row.source, record.address)),
        );
    }
    let (segment_gaps, segment_handles) =
        misnamed_segment_subagents(session, &addresses, &outputs[0].0, &outputs[2].0);
    gaps.extend(segment_gaps);
    (gaps, legend(&outputs[0].0).len(), segment_handles)
}

/// The segments of `outline` whose handles do not name exactly the
/// sub-agents of their messages shown in `read`, a read with every flag; and
/// how many handles the segments name. A handle names the thread of the
/// messages its records hold in `read`.
fn misnamed_segment_subagents(
    session: &AgentSession,
    addresses: &SessionAddresses,
    read: &str,
    outline: &str,
) -> (Vec<String>, usize) {
    let threads = &addresses.threads;
    let records = read_records(read);
    let shown = covered_messages(read, addresses);
    let thread_of_handle = records
        .iter()
        .filter_map(|record| {
            let ordinal = addresses.ordinals_of(record.address)[0];
            Some((record.subagent?, threads.get(&ordinal)?))
        })
        .collect::<HashMap<_, _>>();
    let outline_legend = legend(outline);
    let segments = segment_records(outline);
    let gaps = segments
        .iter()
        .filter(|segment| {
            let sub_agents_shown = addresses
                .segment_ordinals(segment.address)
                .filter(|ordinal| shown.contains(ordinal))
                .filter_map(|ordinal| threads.get(&ordinal))
                .collect::<BTreeSet<_>>();
            let named = segment
                .subagents
                .iter()
                .map(|handle| {
                    outline_legend
                        .contains_key(*handle)
                        .then(|| thread_of_handle.get(handle).copied())
                        .flatten()
                })
                .collect::<Option<BTreeSet<_>>>();
            named != Some(sub_agents_shown)
        })
        .map(|segment| format!("{:?}: seg {}", session.row.source, segment.address))
        .collect();
    let handles = segments.iter().map(|segment| segment.subagents.len()).sum();
    (gaps, handles)
}

#[test]
fn every_agents_records_name_one_owner_of_their_messages_in_legend() {
    let mut gaps = Vec::new();
    let (mut handles, mut segment_handles) = (0, 0);
    for session in every_record_sessions() {
        let (session_gaps, session_handles, session_segment_handles) = misnamed_subagents(&session);
        gaps.extend(session_gaps);
        handles += session_handles;
        segment_handles += session_segment_handles;
    }

    assert!(gaps.is_empty(), "{}", gaps.join("\n"));
    assert!(handles > 0, "no read names a sub-agent");
    assert!(segment_handles > 0, "no outline segment names a sub-agent");
}

/// The read of `session`'s sub-agent `handle` alone, with every flag.
fn read_subagent(session: &AgentSession, handle: usize) -> String {
    let row = &session.row;
    let transcript =
        AgentTranscript::load_cached_for_test(row.source, &row.path, &row.subagents).unwrap();
    let key = key_of_listed(row);
    let resolved = resolved_conversation_for_key(std::slice::from_ref(&key), &key);
    format_read(
        &[ReadRequest {
            resolved: &resolved,
            transcript: &transcript,
            range: None,
            subagent: Some(handle),
            call: None,
            shortened_to: None,
        }],
        None,
        None,
        EVERY_FLAG,
    )
    .unwrap()
}

/// The messages of the records of `output` carrying `handle`.
fn messages_of(output: &str, handle: &str) -> BTreeSet<MessageKey> {
    read_records(output)
        .iter()
        .filter(|record| record.subagent == Some(handle))
        .flat_map(|record| addressed_messages(record.address))
        .collect()
}

/// The messages a read covers: those of its message and call records, and
/// the ranges of its hidden records.
fn covered_addresses(output: &str) -> BTreeSet<MessageKey> {
    read_records(output)
        .iter()
        .flat_map(|record| addressed_messages(record.address))
        .collect()
}

/// Each sub-agent of `session` whose read alone does not show exactly its
/// messages in a read of the whole session, under its handle, then the launch
/// and the reports its legend names; and the legend's sub-agents, for the
/// count of launches and reports they name.
fn misread_subagents(session: &AgentSession) -> (Vec<String>, HashMap<String, LegendLinks>) {
    let whole = read(session, EVERY_FLAG);
    let legend = legend(&whole);
    let misread = legend
        .iter()
        .filter_map(|(handle, links)| {
            let number = handle.strip_prefix('s').unwrap().parse().unwrap();
            let alone = read_subagent(session, number);
            let own = messages_of(&whole, handle);
            let mut expected = own.clone();
            expected.extend(links.launch);
            expected.extend(&links.reports);
            let shown = covered_addresses(&alone);
            (messages_of(&alone, handle) != own || shown != expected).then(|| {
                format!(
                    "{:?}: {handle} shows {shown:?}, expected {expected:?} with its own {own:?}\n{alone}",
                    session.row.source
                )
            })
        })
        .collect();
    (misread, legend)
}

/// Each segment of the outline of each of `session`'s sub-agents, at two
/// messages a segment, whose address does not read back exactly the
/// messages it covers; and the count of segments. A segment of the
/// session's own messages reads back without `--subagents`: with it, `m5`
/// also shows the sub-agent messages written after m5, which the segment
/// does not cover.
fn segments_not_read_back(session: &AgentSession) -> (Vec<String>, usize) {
    let loaded = LoadedSession::of(session);
    let reference = loaded.resolved.reference.canonical();
    let mut misread = Vec::new();
    let mut segments = 0;
    for handle in legend(&read(session, EVERY_FLAG)).keys() {
        let number = handle.strip_prefix('s').unwrap().parse().unwrap();
        let request = ReadRequest {
            resolved: &loaded.resolved,
            transcript: &loaded.transcript,
            range: None,
            subagent: Some(number),
            call: None,
            shortened_to: None,
        };
        for (address, covered) in crate::agent::protocol::outline_segments(&request, 2) {
            segments += 1;
            let expected = covered
                .iter()
                .flat_map(|message| addressed_messages(message))
                .collect::<BTreeSet<_>>();
            let is_of_own_messages = addressed_messages(&address)[0].0.is_none();
            let read_ref = |options| {
                covered_addresses(&loaded.read_ref(&format!("{reference}:{address}"), options))
            };
            let read_back = if is_of_own_messages {
                read_ref(EVERY_FLAG_BUT_SUBAGENTS)
                    .into_iter()
                    .filter(|&(handle, _)| handle.is_none())
                    .collect()
            } else {
                read_ref(EVERY_FLAG)
            };
            if read_back != expected {
                misread.push(format!(
                    "{:?}: {handle}'s seg {address} covers {covered:?}, reads back {read_back:?}",
                    session.row.source
                ));
            }
        }
    }
    (misread, segments)
}

#[test]
fn every_agents_subagent_outline_segment_reads_back_its_messages() {
    let mut misread = Vec::new();
    let mut segments = 0;
    for session in every_record_sessions() {
        let (session_misread, session_segments) = segments_not_read_back(&session);
        misread.extend(session_misread);
        segments += session_segments;
    }

    assert!(misread.is_empty(), "{}", misread.join("\n"));
    assert!(segments > 0, "a sub-agent's outline has segments");
}

#[test]
fn every_agents_sub_agent_read_alone_shows_exactly_its_messages_launch_and_reports() {
    let mut misread = Vec::new();
    let (mut launches, mut reports) = (0, 0);
    for session in every_record_sessions() {
        let (session_misread, legend) = misread_subagents(&session);
        misread.extend(session_misread);
        launches += legend
            .values()
            .filter(|links| links.launch.is_some())
            .count();
        reports += legend
            .values()
            .map(|links| links.reports.len())
            .sum::<usize>();
    }

    assert!(misread.is_empty(), "{}", misread.join("\n"));
    assert!(launches > 0, "no legend names a launch");
    assert!(reports > 0, "no legend names a report");
}

/// Each pair of consecutive handles in `session`'s legend whose sub-agents
/// start out of order, a sub-agent starting at its launch, else at its first
/// message; and how many pairs there are.
fn subagents_out_of_launch_order(session: &AgentSession) -> (Vec<String>, usize) {
    let addresses = SessionAddresses::of(session);
    let legend = legend(&read(session, EVERY_FLAG));
    let mut starts = legend
        .iter()
        .map(|(handle, links)| {
            let number = handle.strip_prefix('s').unwrap().parse::<usize>().unwrap();
            let start = links.launch.or(links.first).unwrap();
            (number, addresses.ordinals[&start], start)
        })
        .collect::<Vec<_>>();
    starts.sort_unstable();
    let unordered = starts
        .windows(2)
        .filter(|pair| pair[0].1 > pair[1].1)
        .map(|pair| {
            format!(
                "{:?}: s{} starts at {}, after s{} at {}",
                session.row.source,
                pair[0].0,
                message_address(pair[0].2),
                pair[1].0,
                message_address(pair[1].2)
            )
        })
        .collect();
    (unordered, starts.len().saturating_sub(1))
}

#[test]
fn every_agents_handles_follow_order_of_subagents_starts() {
    let (mut unordered, mut pairs) = (Vec::new(), 0);
    for session in every_record_sessions() {
        let (session_unordered, session_pairs) = subagents_out_of_launch_order(&session);
        unordered.extend(session_unordered);
        pairs += session_pairs;
    }

    assert!(unordered.is_empty(), "{}", unordered.join("\n"));
    assert!(pairs > 0, "a session has two sub-agents");
}

#[test]
fn every_agents_read_with_tools_and_subagents_shows_each_call_and_subagent_marker() {
    let gaps: Vec<String> = every_record_sessions()
        .iter()
        .flat_map(unshown_call_and_subagent_markers)
        .collect();

    assert!(gaps.is_empty(), "{}", gaps.join("\n"));
}

/// Each marker of `session` in a sub-agent's thinking that `agent read
/// --subagents --thinking` doesn't show or `agent read --subagents` shows;
/// and how many such markers `session` has.
fn misread_subagent_thinking_markers(session: &AgentSession) -> (Vec<String>, usize) {
    let with_thinking = read(session, WITH_SUBAGENTS_AND_THINKING);
    let without_thinking = read(session, WITH_SUBAGENTS);
    let markers: Vec<_> = session
        .markers
        .iter()
        .filter(|marker| marker.is_in_subagent_thinking)
        .collect();
    let mut gaps = Vec::new();
    for marker in &markers {
        let source = session.row.source;
        if !with_thinking.contains(marker.text) {
            gaps.push(format!(
                "{source:?}: `--thinking` doesn't show {}",
                marker.text
            ));
        }
        if without_thinking.contains(marker.text) {
            gaps.push(format!(
                "{source:?}: a read without `--thinking` shows {}",
                marker.text
            ));
        }
    }
    (gaps, markers.len())
}

#[test]
fn every_agents_read_with_subagents_shows_subagents_thinking_only_with_thinking() {
    let mut gaps = Vec::new();
    let mut markers = 0;
    for session in every_record_sessions() {
        let (session_gaps, session_markers) = misread_subagent_thinking_markers(&session);
        gaps.extend(session_gaps);
        markers += session_markers;
    }

    assert!(gaps.is_empty(), "{}", gaps.join("\n"));
    assert!(
        markers > 0,
        "some agent's fixture contains a sub-agent's thinking"
    );
}

/// Each message `agent search` finds a marker of `session` in, with the
/// record of that message's read alone with every flag and
/// `--match <marker>`, when that read doesn't report a hit.
fn markers_a_read_of_their_message_doesnt_match(session: &AgentSession) -> Vec<String> {
    let row = &session.row;
    let transcript =
        AgentTranscript::load_cached_for_test(row.source, &row.path, &row.subagents).unwrap();
    let key = key_of_listed(row);
    let resolved = resolved_conversation_for_key(std::slice::from_ref(&key), &key);
    session
        .markers
        .iter()
        .flat_map(|marker| {
            retrieve_agent_hits_for_target(
                AgentTranscriptSearchTarget::whole(&transcript, None, None),
                &exact_phrase_query(marker.text),
                AgentRetrievalOptions {
                    limit: usize::MAX,
                    ..AgentRetrievalOptions::default()
                },
            )
            .into_iter()
            .map(move |hit| (marker, hit.focus_range.start))
        })
        .filter_map(|(marker, ordinal)| {
            let output = format_read(
                &[ReadRequest {
                    resolved: &resolved,
                    transcript: &transcript,
                    range: Some(MessageRange::single(ordinal)),
                    subagent: None,
                    call: None,
                    shortened_to: None,
                }],
                None,
                Some(&ReadSlice::Match {
                    query: marker.text.to_owned(),
                    context: 0,
                }),
                EVERY_FLAG,
            )
            .unwrap();
            let hits = output
                .lines()
                .find(|line| line.contains(" slice=match="))
                .and_then(|record| atom(record, "hits"));
            matches!(hits, None | Some("0"))
                .then(|| format!("{:?} {} in m{ordinal}: {output}", row.source, marker.record))
        })
        .collect()
}

#[test]
fn every_agents_marker_search_finds_is_hit_in_read_of_its_message_alone() {
    let unmatched: Vec<String> = every_record_sessions()
        .iter()
        .flat_map(markers_a_read_of_their_message_doesnt_match)
        .collect();

    assert!(unmatched.is_empty(), "{}", unmatched.join("\n"));
}

#[test]
fn every_agents_read_with_default_flags_covers_each_message_read_with_every_flag_covers() {
    let uncovered: Vec<String> = every_record_sessions()
        .iter()
        .flat_map(uncovered_messages)
        .collect();

    assert!(uncovered.is_empty(), "{}", uncovered.join("\n"));
}

#[test]
fn every_agents_read_with_default_flags_counts_its_hidden_tool_calls() {
    let uncounted: Vec<String> = every_record_sessions()
        .iter()
        .filter(|session| {
            let output = read(session, DEFAULT_FLAGS);
            !output.lines().next().unwrap().contains(" hidden-tools=")
        })
        .map(|session| format!("{:?}", session.row.source))
        .collect();

    assert!(uncounted.is_empty(), "{}", uncounted.join("\n"));
}

/// The read of `session` with every flag and half its length as the budget,
/// when it was cut short at the budget, misses the first message, or omits a
/// message before the last one it covers.
fn budget_gap(session: &AgentSession) -> Option<String> {
    let unbudgeted = read(session, EVERY_FLAG);
    let budget = unbudgeted.chars().count() / 2;
    let output = read(
        session,
        ProtocolOptions {
            budget: Some(budget),
            ..EVERY_FLAG
        },
    );
    let addresses = SessionAddresses::of(session);
    let covered = covered_messages(&output, &addresses);
    let leading = covered_messages(&unbudgeted, &addresses)
        .into_iter()
        .take(covered.len().max(1))
        .collect::<BTreeSet<_>>();
    let cut_short = !output.ends_with('\n');
    (cut_short || covered != leading)
        .then(|| format!("{:?}: covers {covered:?}\n{output}", session.row.source))
}

/// The messages of `session` that its read with `options` and half that
/// read's length as the budget omits, and that the reads its `continue read`
/// record names do not read exactly: a message they miss, one they read
/// twice, or one they read though the budgeted read covers it; and whether
/// the read printed such a record. A read of each ref in `refs=` uses the
/// record's flags; it reads a message it prints only as a call's `result=`,
/// such as an empty result, that the budgeted read's flags hid in a run.
fn messages_a_continue_read_misreads(
    session: &AgentSession,
    options: ProtocolOptions,
) -> (Vec<String>, bool) {
    let unbudgeted = read(session, options);
    let budgeted = read(
        session,
        ProtocolOptions {
            budget: Some(unbudgeted.chars().count() / 2),
            ..options
        },
    );
    let Some(record) = budgeted
        .lines()
        .find(|line| line.starts_with("continue read "))
    else {
        return (Vec::new(), false);
    };
    let flag = |name| atom(record, name) == Some("true");
    let flags = ProtocolOptions {
        tools: flag("tools"),
        tool_results: flag("tool-results"),
        thinking: flag("thinking"),
        subagents: flag("subagents"),
        ..DEFAULT_FLAGS
    };
    let addresses = SessionAddresses::of(session);
    let mut read_back = Vec::new();
    let mut results = BTreeSet::new();
    for reference in atom(record, "refs").unwrap().split(';') {
        let output = read_ref(session, reference, flags);
        read_back.extend(covered_messages(&output, &addresses));
        results.extend(result_messages(&output, &addresses));
    }
    let budgeted_covers = covered_messages(&budgeted, &addresses);
    let omitted = covered_messages(&unbudgeted, &addresses)
        .difference(&budgeted_covers)
        .copied()
        .collect::<BTreeSet<_>>();
    let mut read_once = read_back.iter().copied().collect::<BTreeSet<_>>();
    read_once.extend(results);
    let name = |ordinal: &usize| {
        let side = if omitted.contains(ordinal) {
            "omitted and not read back"
        } else {
            "read back and not omitted"
        };
        format!(
            "{:?}: {} {side}\n{record}",
            session.row.source,
            addresses.addresses.message(*ordinal)
        )
    };
    let mut misread = omitted
        .symmetric_difference(&read_once)
        .map(name)
        .collect::<Vec<_>>();
    if read_back.len() != read_back.iter().collect::<BTreeSet<_>>().len() {
        misread.push(format!(
            "{:?}: a message read twice\n{record}",
            session.row.source
        ));
    }
    (misread, true)
}

#[test]
fn every_agents_continue_read_reads_exactly_messages_its_budget_omits() {
    let mut misread = Vec::new();
    let mut continuations = 0;
    for session in every_record_sessions() {
        for options in [DEFAULT_FLAGS, WITH_SUBAGENTS] {
            let (session_misread, continues) = messages_a_continue_read_misreads(&session, options);
            misread.extend(session_misread);
            continuations += usize::from(continues);
        }
    }

    assert!(misread.is_empty(), "{}", misread.join("\n"));
    assert!(continuations > 0, "no budgeted read continues");
}

/// Each message of the session's own in `session` whose read as `mN`
/// differs from its read as `mN..mN`, with sub-agents shown or hidden; and
/// how many of those reads show a sub-agent's message.
fn own_messages_read_apart_from_their_closed_range(session: &AgentSession) -> (Vec<String>, usize) {
    let conversation = key_of_listed(&session.row).conversation_ref().canonical();
    let loaded = LoadedSession::of(session);
    let own_numbers = SessionAddresses::of(session)
        .ordinals
        .into_keys()
        .filter_map(|(handle, number)| handle.is_none().then_some(number))
        .collect::<BTreeSet<_>>();
    let mut differing = Vec::new();
    let mut showing_subagents = 0;
    for options in [WITH_SUBAGENTS, DEFAULT_FLAGS] {
        for number in &own_numbers {
            let single = loaded.read_ref(&format!("{conversation}:m{number}"), options);
            let closed = loaded.read_ref(&format!("{conversation}:m{number}..m{number}"), options);
            if single != closed {
                differing.push(format!(
                    "{:?}: m{number} reads\n{single}\nm{number}..m{number} reads\n{closed}",
                    session.row.source
                ));
            }
            if read_records(&single)
                .iter()
                .any(|record| record.subagent.is_some() && record.word != Some("hidden"))
            {
                showing_subagents += 1;
            }
        }
    }
    (differing, showing_subagents)
}

#[test]
fn every_agents_own_message_reads_as_its_closed_range_with_and_without_subagents() {
    let mut differing = Vec::new();
    let mut showing_subagents = 0;
    for session in every_record_sessions() {
        let (session_differing, session_showing) =
            own_messages_read_apart_from_their_closed_range(&session);
        differing.extend(session_differing);
        showing_subagents += session_showing;
    }

    assert!(differing.is_empty(), "{}", differing.join("\n"));
    assert!(
        showing_subagents > 0,
        "a read of one own message shows the sub-agent messages written after it"
    );
}

/// Each thread of `session`, the session's own and each sub-agent's, where
/// three reads from its second-last message differ: the range ending at its
/// last message, the range ending past it, and the open range. Also how
/// many sub-agent threads were read. The range ending past the last message
/// prints `last=` with that message.
fn ranges_to_thread_end_read_apart(session: &AgentSession) -> (Vec<String>, usize) {
    let conversation = key_of_listed(&session.row).conversation_ref().canonical();
    let loaded = LoadedSession::of(session);
    let mut lengths = HashMap::<Option<usize>, usize>::new();
    for (handle, number) in SessionAddresses::of(session).ordinals.into_keys() {
        let length = lengths.entry(handle).or_default();
        *length = (*length).max(number);
    }
    let mut misread = Vec::new();
    for (&handle, &length) in &lengths {
        let thread = handle.map_or_else(String::new, |handle| format!("s{handle}:"));
        let first = length.saturating_sub(1).max(1);
        let read = |messages: String| {
            loaded.read_ref(&format!("{conversation}:{thread}{messages}"), EVERY_FLAG)
        };
        let to_last = read(format!("m{first}..m{length}"));
        let past_last = read(format!("m{first}..m{}", length + 5));
        let open = read(format!("m{first}.."));
        let last = format!(" last={thread}m{length}");
        if !past_last
            .lines()
            .next()
            .unwrap()
            .contains(&format!("{last} "))
            || past_last.replacen(&last, "", 1) != to_last
            || open != to_last
        {
            misread.push(format!(
                "{:?}: {thread}m{first}..m{length} reads\n{to_last}\npast its end\n{past_last}\nopen\n{open}",
                session.row.source
            ));
        }
    }
    let subagent_threads = lengths.keys().filter(|handle| handle.is_some()).count();
    (misread, subagent_threads)
}

#[test]
fn every_agents_range_past_or_open_to_threads_end_reads_to_its_last_message() {
    let mut misread = Vec::new();
    let mut subagent_threads = 0;
    for session in every_record_sessions() {
        let (session_misread, session_threads) = ranges_to_thread_end_read_apart(&session);
        misread.extend(session_misread);
        subagent_threads += session_threads;
    }

    assert!(misread.is_empty(), "{}", misread.join("\n"));
    assert!(subagent_threads > 0, "a session has a sub-agent's thread");
}

/// Each address `session`'s reads with every flag and with default flags
/// print that a read of it alone with the same flags does not read back: a
/// message or call whose record the read lacks, or a run whose messages it
/// does not cover; and how many addresses were read. The addresses are those
/// of records, `result=`, `call=` and `same=`, and a legend's `launch=`,
/// `span=` and `report=`. A message a read prints without a record of its
/// own must still read without an error.
fn addresses_not_read_back(session: &AgentSession) -> (Vec<String>, usize) {
    let conversation = key_of_listed(&session.row).conversation_ref().canonical();
    let loaded = LoadedSession::of(session);
    let mut gaps = Vec::new();
    let mut count = 0;
    for options in [EVERY_FLAG, DEFAULT_FLAGS] {
        let output = read(session, options);
        let records = read_records(&output);
        let recorded = records
            .iter()
            .map(|record| record.address)
            .collect::<BTreeSet<_>>();
        let mut printed = recorded
            .iter()
            .map(|address| (*address).to_owned())
            .collect::<BTreeSet<_>>();
        for record in &records {
            for name in ["result", "call", "same"] {
                printed.extend(record.atom(name).map(linked_addresses).unwrap_or_default());
            }
        }
        for legend in output
            .lines()
            .filter_map(|line| line.strip_prefix("subagent "))
        {
            for field in legend.split(" : ").next().unwrap().split_whitespace() {
                for name in ["launch=", "span=", "report="] {
                    if let Some(values) = field.strip_prefix(name) {
                        printed.extend(values.split(',').map(str::to_owned));
                    }
                }
            }
        }
        for address in printed {
            count += 1;
            let alone = loaded.read_ref(&format!("{conversation}:{address}"), options);
            let read_back = if address.contains("..") {
                let covered = covered_addresses(&alone);
                addressed_messages(&address)
                    .iter()
                    .all(|message| covered.contains(message))
            } else {
                !recorded.contains(address.as_str())
                    || read_records(&alone)
                        .iter()
                        .any(|record| record.address == address)
            };
            if !read_back {
                gaps.push(format!("{:?}: {address}\n{alone}", session.row.source));
            }
        }
    }
    (gaps, count)
}

#[test]
fn every_agents_printed_address_reads_back_its_record() {
    let mut gaps = Vec::new();
    let mut addresses = 0;
    for session in every_record_sessions() {
        let (session_gaps, session_addresses) = addresses_not_read_back(&session);
        gaps.extend(session_gaps);
        addresses += session_addresses;
    }

    assert!(gaps.is_empty(), "{}", gaps.join("\n"));
    assert!(addresses > 0, "a read prints an address");
}

/// Each `read ref=` recipe that an exact `agent within` search of `session`
/// for one of its markers prints, and whose read with the recipe's flags
/// does not cover the hit's `focus=` messages; and how many recipes ran.
fn hit_recipes_not_read_back(session: &AgentSession) -> (Vec<String>, usize) {
    let row = &session.row;
    let loaded = LoadedSession::of(session);
    let addresses = SessionAddresses::of(session);
    let mut gaps = Vec::new();
    let mut recipes = 0;
    for marker in &session.markers {
        let request = AgentWithinRequest {
            query: exact_phrase_query(marker.text),
            top: usize::MAX,
            cli_mode: Some(SearchMode::Exact),
            config_mode: None,
            tui_semantic_search: None,
            budget: None,
            window: TimeFilter::default(),
        };
        let output = format_agent_output(&run_within_search(
            &request,
            row,
            &loaded.resolved,
            &loaded.transcript,
            &[],
        ));
        for recipe in output.lines().filter(|line| line.starts_with("read ref=")) {
            recipes += 1;
            let flag = |name| atom(recipe, name) == Some("true");
            let flags = ProtocolOptions {
                tools: flag("tools"),
                tool_results: flag("tool-results"),
                thinking: flag("thinking"),
                subagents: flag("subagents"),
                ..DEFAULT_FLAGS
            };
            let read = loaded.read_ref(atom(recipe, "ref").unwrap(), flags);
            let mut covered = covered_messages(&read, &addresses);
            covered.extend(result_messages(&read, &addresses));
            let focus = addresses.segment_ordinals(atom(recipe, "focus").unwrap());
            if !focus.clone().all(|ordinal| covered.contains(&ordinal)) {
                gaps.push(format!(
                    "{:?} {}: {recipe}\n{read}",
                    row.source, marker.record
                ));
            }
        }
    }
    (gaps, recipes)
}

#[test]
fn every_agents_within_hit_recipe_reads_its_focus_messages() {
    let mut gaps = Vec::new();
    let mut recipes = 0;
    for session in every_record_sessions() {
        let (session_gaps, session_recipes) = hit_recipes_not_read_back(&session);
        gaps.extend(session_gaps);
        recipes += session_recipes;
    }

    assert!(gaps.is_empty(), "{}", gaps.join("\n"));
    assert!(recipes > 0, "a marker search prints a recipe");
}

#[test]
fn every_agents_budgeted_read_covers_its_first_messages_without_being_cut_short() {
    let gaps: Vec<String> = every_record_sessions()
        .iter()
        .filter_map(budget_gap)
        .collect();

    assert!(gaps.is_empty(), "{}", gaps.join("\n"));
}

#[test]
fn every_agents_read_with_every_flag_doesnt_report_hidden_part() {
    let reporting: Vec<String> = every_record_sessions()
        .iter()
        .filter(|session| {
            let output = read(session, EVERY_FLAG);
            output.contains(" hidden-")
                || read_records(&output)
                    .iter()
                    .any(|record| record.word == Some("hidden"))
        })
        .map(|session| format!("{:?}", session.row.source))
        .collect();

    assert!(reporting.is_empty(), "{}", reporting.join("\n"));
}

/// [`read_records`] and [`outline_records`] fail on a line that is neither a
/// record nor content, content below no record, a time below no `day`
/// record, a record whose kind, address or word the grammar does not define,
/// atoms other than its kind's in order and form, and a range ending before
/// it starts.
#[test]
fn every_agents_read_and_outline_parse_as_positional_records() {
    for session in every_record_sessions() {
        for options in [DEFAULT_FLAGS, EVERY_FLAG] {
            let output = read(&session, options);
            assert!(
                !read_records(&output).is_empty(),
                "{:?}: {output}",
                session.row.source
            );
        }
        let outline = outline(&session);
        let message_records = outline_records(&outline);
        assert!(
            !message_records.is_empty() || outline.contains("\nseg "),
            "{:?}: {outline}",
            session.row.source
        );
    }
}

/// The `result=` and `call=` atoms of `session`'s read with every flag that
/// do not lead back to their record, and how many links the read holds.
/// Links to messages without a record are not checked: a message with empty
/// content has none.
fn one_way_links(session: &AgentSession) -> (Vec<String>, usize) {
    let output = read(session, EVERY_FLAG);
    let records = read_records(&output);
    let by_address = records
        .iter()
        .map(|record| (record.address, record))
        .collect::<HashMap<_, _>>();
    let names = |record: &PositionalRecord<'_>, atom: &str, address: &str| {
        record.atom(atom).is_some_and(|values| {
            linked_addresses(values)
                .iter()
                .any(|value| value == address)
        })
    };
    let mut gaps = Vec::new();
    let mut links = 0;
    for record in &records {
        for (atom, back) in [("result", "call"), ("call", "result")] {
            for target in record.atom(atom).map(linked_addresses).unwrap_or_default() {
                links += 1;
                if let Some(target_record) = by_address.get(target.as_str())
                    && !names(target_record, back, record.address)
                {
                    let address = record.address;
                    gaps.push(format!(
                        "{:?}: {address} {atom}={target}; target {target} lacks {back}={address}",
                        session.row.source
                    ));
                }
            }
        }
    }
    (gaps, links)
}

/// Each address in a `result=` or `call=` value: `m7.2..3` names `m7.2` and
/// `m7.3`, the parts of a split call.
fn linked_addresses(values: &str) -> Vec<String> {
    values
        .split(',')
        .flat_map(|value| match value.split_once("..") {
            Some((first, last)) => {
                let (message, first) = first.rsplit_once('.').unwrap();
                let (first, last) = (first.parse::<usize>().unwrap(), last.parse().unwrap());
                (first..=last)
                    .map(|part| format!("{message}.{part}"))
                    .collect()
            }
            None => vec![value.to_owned()],
        })
        .collect()
}

#[test]
fn every_agents_calls_and_results_link_both_ways() {
    let mut gaps = Vec::new();
    let mut links = 0;
    let mut split_calls = 0;
    for session in every_record_sessions() {
        let (session_gaps, session_links) = one_way_links(&session);
        gaps.extend(session_gaps);
        links += session_links;
        split_calls += read_records(&read(&session, EVERY_FLAG))
            .iter()
            .filter(|record| {
                record
                    .atom("call")
                    .is_some_and(|calls| calls.contains(".."))
            })
            .count();
    }

    assert!(gaps.is_empty(), "{}", gaps.join("\n"));
    assert!(links > 0, "no read links a call to its result");
    assert!(split_calls > 0, "no read links a split call to its result");
}

/// The records of results in their call's own message, `mN.K result`, in
/// `session`'s read with every flag that do not follow their call's record,
/// and how many such records the read holds.
fn misplaced_results_in_their_calls_message(session: &AgentSession) -> (Vec<String>, usize) {
    let output = read(session, EVERY_FLAG);
    let records = read_records(&output);
    let mut misplaced = Vec::new();
    let mut count = 0;
    for (index, record) in records.iter().enumerate() {
        if !is_result_in_its_calls_message(record) {
            continue;
        }
        count += 1;
        let follows_its_call = records[..index]
            .iter()
            .rev()
            .find(|earlier| earlier.word != Some("result"))
            .is_some_and(|call| call.address == record.address && call.word.is_none());
        if !follows_its_call {
            misplaced.push(format!(
                "{:?}: {} result",
                session.row.source, record.address
            ));
        }
    }
    (misplaced, count)
}

/// True for `m7.2 result`: a result in its call's own message.
fn is_result_in_its_calls_message(record: &PositionalRecord<'_>) -> bool {
    record.address.contains('.') && record.word == Some("result")
}

#[test]
fn every_agents_result_in_its_calls_message_prints_after_call() {
    let mut misplaced = Vec::new();
    let mut count = 0;
    for session in every_record_sessions() {
        let (session_misplaced, session_count) = misplaced_results_in_their_calls_message(&session);
        misplaced.extend(session_misplaced);
        count += session_count;
    }

    assert!(misplaced.is_empty(), "{}", misplaced.join("\n"));
    assert!(
        count > 0,
        "no read prints a result in its call's own message"
    );
}

/// The results of `session` that show their call's tool wrongly with or
/// without `tool=`: a read with every flag shows every call, and a read with
/// tool results alone shows none. A result names its call in `call=`, or in
/// its address when it is in the call's own message.
fn misnamed_tools(session: &AgentSession) -> Vec<String> {
    [(EVERY_FLAG, false), (TOOL_RESULTS_ALONE, true)]
        .into_iter()
        .flat_map(|(options, names_tools)| {
            let output = read(session, options);
            read_records(&output)
                .into_iter()
                .filter(|record| {
                    let is_result =
                        record.atom("call").is_some() || is_result_in_its_calls_message(record);
                    is_result && record.atom("tool").is_some() != names_tools
                })
                .map(|record| format!("{:?}: {}", session.row.source, record.address))
                .collect::<Vec<_>>()
        })
        .collect()
}

#[test]
fn every_agents_result_names_its_tool_only_when_read_hides_call() {
    let misnamed: Vec<String> = every_record_sessions()
        .iter()
        .flat_map(misnamed_tools)
        .collect();

    assert!(misnamed.is_empty(), "{}", misnamed.join("\n"));
}
