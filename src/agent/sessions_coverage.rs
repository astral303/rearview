//! A `session` record shows a session's message count and times as `agent
//! outline`'s header shows them. With a time window, it shows the count and
//! times of the messages inside it as a windowed `agent read` shows them.

use crate::agent::protocol::{ProtocolOptions, ReadRequest, format_outline, format_read};
use crate::agent::refs::ResolvedConversation;
use crate::agent::sessions::{format_sessions, list_sessions};
use crate::agent::test_support::{
    EVERY_PART, RecordTimes, atom, read_times, resolved_conversation, times,
};
use crate::agent::transcript::AgentTranscript;
use crate::clock::Zone;
use crate::history::Conversation;
use crate::history::provider::contract_tests::{AgentSession, every_record_sessions};
use crate::time_filter::TimeFilter;
use chrono::TimeZone;

/// `row`'s `session` record, and the times it shows below its header.
fn session_record(
    row: &Conversation,
    resolved: &ResolvedConversation,
    window: &TimeFilter,
) -> (String, Option<RecordTimes>) {
    let listing = list_sessions([(row, resolved)], window, 0, 1);
    let output = format_sessions(&listing, None);
    let header = output.lines().next().unwrap_or_default();
    let record = output
        .lines()
        .find(|line| line.starts_with("session "))
        .unwrap_or_default();
    (record.to_owned(), times(record, header))
}

/// The window from the first time of `row`'s middle message to its last.
fn window_around_middle_message(row: &Conversation) -> Option<TimeFilter> {
    let span = row.message_times.span(row.message_count.div_ceil(2))?;
    let local = |second| Zone::System.timestamp_opt(second, 0).single();
    Some(TimeFilter {
        after: local(span.first_second()),
        before: local(span.last_second()),
    })
}

/// The differences between `session`'s `session` record and what `agent
/// outline` and a windowed `agent read` show of it.
fn record_gaps(session: &AgentSession) -> Vec<String> {
    let row = &session.row;
    let agent = row.source;
    let resolved = resolved_conversation(row);
    let transcript =
        AgentTranscript::load_cached_for_test(agent, &row.path, &row.subagents).unwrap();
    let mut gaps = Vec::new();

    let outline = format_outline(&resolved, &transcript, EVERY_PART);
    let header = outline.lines().next().unwrap();
    let (record, record_times) = session_record(row, &resolved, &TimeFilter::default());
    if atom(&record, "messages") != atom(header, "messages")
        || record_times != times(header, header)
    {
        gaps.push(format!(
            "{agent:?}: agent sessions lists {record}, the outline header: {header}"
        ));
    }

    let Some(window) = window_around_middle_message(row) else {
        gaps.push(format!(
            "{agent:?}: the middle message does not have a time"
        ));
        return gaps;
    };
    let read = format_read(
        &[ReadRequest {
            resolved: &resolved,
            transcript: &transcript,
            range: None,
            subagent: None,
            call: None,
        }],
        None,
        None,
        ProtocolOptions {
            window,
            ..EVERY_PART
        },
    )
    .unwrap();
    let read_header = read.lines().next().unwrap();
    let (record, record_times) = session_record(row, &resolved, &window);
    if atom(&record, "messages-in-window") != atom(read_header, "messages-in-window") {
        gaps.push(format!(
            "{agent:?}: agent sessions lists {record}, the windowed read header: {read_header}"
        ));
    }
    if record_times != read_times(&read) {
        gaps.push(format!(
            "{agent:?}: agent sessions lists {record}, a windowed read shows {:?}",
            read_times(&read)
        ));
    }
    gaps
}

#[test]
fn every_agents_session_record_matches_its_outline_and_windowed_read() {
    let gaps: Vec<String> = every_record_sessions()
        .iter()
        .flat_map(record_gaps)
        .collect();

    assert!(gaps.is_empty(), "{}", gaps.join("\n"));
}
