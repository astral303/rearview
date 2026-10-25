//! `agent read` over each registered agent's every-record session.
//! - With `--tools --subagents`, each agent-searched marker shows, in a
//!   call's header, a launch prompt or a sub-agent's message. Tool results
//!   stay hidden, so a marker in a call's header cannot show through the
//!   call's result.
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
//!   reports.

use crate::agent::protocol::{ProtocolOptions, ReadRequest, format_outline, format_read};
use crate::agent::refs::{
    AgentConversationKey, MessageRange, parse_read_ref, resolved_conversation_for_key,
};
use crate::agent::test_support::{
    PositionalRecord, atom, outline_records, read_records, segment_records,
};
use crate::agent::transcript::AgentTranscript;
use crate::history::provider::contract_tests::{AgentSession, MarkerSearch, every_record_sessions};
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

const TOOL_RESULTS_ALONE: ProtocolOptions = ProtocolOptions {
    tools: false,
    tool_results: true,
    ..EVERY_FLAG
};

/// `agent outline` of `session`, with every part shown.
fn outline(session: &AgentSession) -> String {
    let row = &session.row;
    let transcript = AgentTranscript::load_owned(row.source, &row.path, &row.subagents).unwrap();
    let key = AgentConversationKey::from_conversation(row).unwrap();
    let resolved = resolved_conversation_for_key(std::slice::from_ref(&key), &key);
    format_outline(&resolved, &transcript, EVERY_FLAG)
}

/// `agent read` of the whole of `session`, with its sub-agent transcripts.
fn read(session: &AgentSession, options: ProtocolOptions) -> String {
    read_range(session, None, options)
}

/// `agent read` of `range`, or the whole, of `session`.
fn read_range(
    session: &AgentSession,
    range: Option<MessageRange>,
    options: ProtocolOptions,
) -> String {
    let row = &session.row;
    let transcript = AgentTranscript::load_owned(row.source, &row.path, &row.subagents).unwrap();
    let key = AgentConversationKey::from_conversation(row).unwrap();
    let resolved = resolved_conversation_for_key(std::slice::from_ref(&key), &key);
    format_read(
        &[ReadRequest {
            resolved: &resolved,
            transcript: &transcript,
            range,
            subagent: None,
            call: None,
        }],
        None,
        None,
        options,
    )
    .unwrap()
}

/// Each agent-searched marker of `session` missing from `agent read --tools
/// --subagents`.
fn unshown_agent_searched_markers(session: &AgentSession) -> Vec<String> {
    let output = read(session, WITH_TOOLS_AND_SUBAGENTS);
    session
        .markers
        .iter()
        .filter(|marker| marker.search == MarkerSearch::AgentOnly)
        .filter(|marker| !output.contains(marker.text))
        .map(|marker| format!("{:?}: {}", session.row.source, marker.record))
        .collect()
}

/// The message numbers a read covers: those of its message and call
/// records, and the ranges of its hidden records.
fn covered_messages(output: &str) -> BTreeSet<usize> {
    read_records(output)
        .iter()
        .flat_map(|record| addressed_messages(record.address))
        .collect()
}

/// `7..=7` for `m7` and `m7.2`; `7..=9` for `m7..m9`.
fn addressed_messages(address: &str) -> std::ops::RangeInclusive<usize> {
    let ordinal = |address: &str| {
        let message = address.strip_prefix('m').unwrap();
        let message = message
            .split_once('.')
            .map_or(message, |(message, _)| message);
        message.parse::<usize>().unwrap()
    };
    let (first, last) = address.split_once("..").unwrap_or((address, address));
    ordinal(first)..=ordinal(last)
}

/// Each message of `session` that a read with every flag covers and a read
/// with default flags does not.
fn uncovered_messages(session: &AgentSession) -> Vec<String> {
    let covered = covered_messages(&read(session, DEFAULT_FLAGS));
    covered_messages(&read(session, EVERY_FLAG))
        .difference(&covered)
        .map(|ordinal| format!("{:?}: m{ordinal}", session.row.source))
        .collect()
}

/// The launch and report messages a legend record names.
#[derive(Default)]
struct LegendLinks {
    launch: Option<usize>,
    reports: Vec<usize>,
}

/// The handles a read's legend records name, with the launch and report
/// messages each one names.
fn legend(output: &str) -> HashMap<String, LegendLinks> {
    let ordinals = |values: &str| {
        values
            .split(',')
            .map(|address| address.strip_prefix('m').unwrap().parse::<usize>().unwrap())
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
                } else if let Some(reports) = field.strip_prefix("report=") {
                    links.reports = ordinals(reports);
                }
            }
            (handle, links)
        })
        .collect()
}

/// The thread of each of `session`'s sub-agent messages, by ordinal: its
/// label and transcript.
fn subagent_threads(session: &AgentSession) -> HashMap<usize, String> {
    let row = &session.row;
    AgentTranscript::load_owned(row.source, &row.path, &row.subagents)
        .unwrap()
        .messages
        .iter()
        .filter_map(|message| {
            let (label, transcript) = message.subagent.as_deref()?.key();
            Some((message.ordinal, format!("{label} {transcript:?}")))
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
    let threads = subagent_threads(session);
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
                    let owners = addressed_messages(record.address)
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
        misnamed_segment_subagents(session, &threads, &outputs[0].0, &outputs[2].0);
    gaps.extend(segment_gaps);
    (gaps, legend(&outputs[0].0).len(), segment_handles)
}

/// The segments of `outline` whose handles do not name exactly the
/// sub-agents of their messages shown in `read`, a read with every flag; and
/// how many handles the segments name. A handle names the thread of the
/// messages its records hold in `read`; `threads` gives each sub-agent
/// message's thread.
fn misnamed_segment_subagents(
    session: &AgentSession,
    threads: &HashMap<usize, String>,
    read: &str,
    outline: &str,
) -> (Vec<String>, usize) {
    let records = read_records(read);
    let shown = covered_messages(read);
    let thread_of_handle = records
        .iter()
        .filter_map(|record| {
            let ordinal = *addressed_messages(record.address).start();
            Some((record.subagent?, threads.get(&ordinal)?))
        })
        .collect::<HashMap<_, _>>();
    let outline_legend = legend(outline);
    let segments = segment_records(outline);
    let gaps = segments
        .iter()
        .filter(|segment| {
            let sub_agents_shown = addressed_messages(segment.address)
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
    let transcript = AgentTranscript::load_owned(row.source, &row.path, &row.subagents).unwrap();
    let key = AgentConversationKey::from_conversation(row).unwrap();
    let resolved = resolved_conversation_for_key(std::slice::from_ref(&key), &key);
    format_read(
        &[ReadRequest {
            resolved: &resolved,
            transcript: &transcript,
            range: None,
            subagent: Some(handle),
            call: None,
        }],
        None,
        None,
        EVERY_FLAG,
    )
    .unwrap()
}

/// The ordinals of the records of `output` carrying `handle`.
fn messages_of(output: &str, handle: &str) -> BTreeSet<usize> {
    read_records(output)
        .iter()
        .filter(|record| record.subagent == Some(handle))
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
            let shown = covered_messages(&alone);
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

#[test]
fn every_agents_read_with_tools_and_subagents_shows_each_agent_searched_marker() {
    let gaps: Vec<String> = every_record_sessions()
        .iter()
        .flat_map(unshown_agent_searched_markers)
        .collect();

    assert!(gaps.is_empty(), "{}", gaps.join("\n"));
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
    let covered = covered_messages(&output);
    let leading = covered_messages(&unbudgeted)
        .into_iter()
        .take(covered.len().max(1))
        .collect::<BTreeSet<_>>();
    let cut_short = !output.ends_with('\n');
    (cut_short || covered != leading)
        .then(|| format!("{:?}: covers {covered:?}\n{output}", session.row.source))
}

/// Each message of `session` with text that its read with default flags and
/// half that read's length as the budget omits, and that the reads its
/// `continue read` record names do not show; and whether the read printed
/// such a record. A read of each ref in `refs=` uses the record's flags.
fn messages_a_continue_read_doesnt_show(session: &AgentSession) -> (Vec<String>, bool) {
    let unbudgeted = read(session, DEFAULT_FLAGS);
    let budgeted = read(
        session,
        ProtocolOptions {
            budget: Some(unbudgeted.chars().count() / 2),
            ..DEFAULT_FLAGS
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
    let shown = atom(record, "refs")
        .unwrap()
        .split(';')
        .flat_map(|reference| {
            let range = parse_read_ref(reference).unwrap().range;
            let output = read_range(session, range, flags);
            read_records(&output)
                .iter()
                .filter(|record| record.word != Some("hidden"))
                .flat_map(|record| addressed_messages(record.address))
                .collect::<Vec<_>>()
        })
        .collect::<BTreeSet<_>>();
    let budgeted_covers = covered_messages(&budgeted);
    let unshown = covered_messages(&read(session, EVERY_FLAG))
        .intersection(&covered_messages(&unbudgeted))
        .filter(|ordinal| !budgeted_covers.contains(ordinal) && !shown.contains(ordinal))
        .map(|ordinal| format!("{:?}: m{ordinal}\n{record}", session.row.source))
        .collect();
    (unshown, true)
}

#[test]
fn every_agents_continue_read_shows_each_message_its_budget_omits() {
    let mut unshown = Vec::new();
    let mut continuations = 0;
    for session in every_record_sessions() {
        let (session_unshown, continues) = messages_a_continue_read_doesnt_show(&session);
        unshown.extend(session_unshown);
        continuations += usize::from(continues);
    }

    assert!(unshown.is_empty(), "{}", unshown.join("\n"));
    assert!(continuations > 0, "no budgeted read continues");
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
