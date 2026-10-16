//! An outline's header shows the session's message count as the session list
//! does, and its first and last time as `agent read` shows them. Checked over
//! each registered agent's every-record session, read with its sub-agent
//! transcripts.

use crate::agent::protocol::{ReadRequest, format_outline, format_read};
use crate::agent::test_support::{EVERY_PART, atom, read_times, resolved_conversation, times};
use crate::agent::transcript::AgentTranscript;
use crate::history::provider::contract_tests::{AgentSession, every_record_sessions};

/// The differences between `session`'s outline header and the list's message
/// count or the times `agent read` shows.
fn header_gaps(session: &AgentSession) -> Vec<String> {
    let row = &session.row;
    let agent = row.source;
    let resolved = resolved_conversation(row);
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
    let read_times = read_times(&read);

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
