//! An outline's header shows the session's message count as the session list
//! does, and its first and last time as `agent read` shows them. Checked over
//! each registered agent's every-record session, read with its sub-agent
//! transcripts.

use crate::agent::protocol::{ProtocolOptions, ReadRequest, format_outline, format_read};
use crate::agent::refs::{AgentConversationKey, ResolvedConversation};
use crate::agent::transcript::AgentTranscript;
use crate::history::provider::contract_tests::{AgentSession, every_record_sessions};
use chrono::{DateTime, FixedOffset};

const EVERY_PART: ProtocolOptions = ProtocolOptions {
    budget: None,
    tools: true,
    tool_results: true,
    thinking: true,
    subagents: true,
};

type Times = (DateTime<FixedOffset>, DateTime<FixedOffset>);

/// The value of a record line's `name=` atom.
fn atom<'a>(line: &'a str, name: &str) -> Option<&'a str> {
    line.split_whitespace()
        .find_map(|field| field.strip_prefix(name)?.strip_prefix('='))
}

/// A record line's `at=` time, and its `to=` time or else its `at=` time.
fn times(line: &str) -> Option<Times> {
    let time = |name| atom(line, name).map(|time| DateTime::parse_from_rfc3339(time).unwrap());
    let at = time("at")?;
    Some((at, time("to").unwrap_or(at)))
}

/// The differences between `session`'s outline header and the list's message
/// count or the times `agent read` shows.
fn header_gaps(session: &AgentSession) -> Vec<String> {
    let row = &session.row;
    let agent = row.source;
    let file_name = row.path.file_name().unwrap().to_string_lossy();
    let key = AgentConversationKey::new("project", file_name, row.path.clone());
    let resolved = ResolvedConversation {
        reference: key.conversation_ref(),
        key,
    };
    let transcript = AgentTranscript::load_owned(agent, &row.path, &row.subagents).unwrap();
    let outline = format_outline(&resolved, &transcript, EVERY_PART);
    let read = format_read(
        &[ReadRequest {
            resolved: &resolved,
            transcript: &transcript,
            range: None,
        }],
        None,
        None,
        EVERY_PART,
    )
    .unwrap();
    let header = outline.lines().next().unwrap();
    let read_times = read
        .lines()
        .filter(|line| line.starts_with("message m"))
        .filter_map(times)
        .reduce(|(first, last), (at, to)| (first.min(at), last.max(to)));

    let mut gaps = Vec::new();
    let listed = row.message_count.to_string();
    if atom(header, "messages") != Some(listed.as_str()) {
        gaps.push(format!(
            "{agent:?}: the list counts {listed} messages, the outline header: {header}"
        ));
    }
    match read_times {
        None => gaps.push(format!("{agent:?}: agent read did not show a time")),
        Some(read_times) if times(header) != Some(read_times) => gaps.push(format!(
            "{agent:?}: agent read shows {read_times:?}, the outline header: {header}"
        )),
        Some(_) => {}
    }
    gaps
}

#[test]
fn every_agents_outline_header_shows_the_sessions_message_count_and_times() {
    let gaps: Vec<String> = every_record_sessions()
        .iter()
        .flat_map(header_gaps)
        .collect();

    assert!(gaps.is_empty(), "{}", gaps.join("\n"));
}
