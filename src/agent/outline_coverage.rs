//! An outline's header counts the session's own and sub-agent messages, which
//! together are the list's message count, and shows its first and last time
//! as `agent read` shows them. Checked over
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
    let transcript =
        AgentTranscript::load_cached_for_test(agent, &row.path, &row.subagents).unwrap();
    let outline = format_outline(&resolved, &transcript, EVERY_PART);
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
        EVERY_PART,
    )
    .unwrap();
    let header = outline.lines().next().unwrap();
    let read_times = read_times(&read);

    let mut gaps = Vec::new();
    let count = |name| atom(header, name).map_or(0, |count| count.parse::<usize>().unwrap());
    let counted = count("messages") + count("subagent-messages");
    if counted != row.message_count {
        gaps.push(format!(
            "{agent:?}: the list counts {} messages, the outline header: {header}",
            row.message_count
        ));
    }
    match read_times {
        None => gaps.push(format!("{agent:?}: agent read did not show a time")),
        Some(read_times) if times(header, header) != Some(read_times) => gaps.push(format!(
            "{agent:?}: agent read shows {read_times:?}, the outline header: {header}"
        )),
        Some(_) => {}
    }
    gaps
}

#[test]
fn every_agents_outline_header_shows_sessions_message_count_and_times() {
    let gaps: Vec<String> = every_record_sessions()
        .iter()
        .flat_map(header_gaps)
        .collect();

    assert!(gaps.is_empty(), "{}", gaps.join("\n"));
}
