//! The session list numbers each agent's messages as `agent read` does. Over
//! each registered agent's every-record session, read with its sub-agent
//! transcripts:
//!
//! - the row's message count equals `agent read`'s;
//! - the reply after the agent's records without text carries the number the
//!   fixture names, in the list and in `agent read`;
//! - each semantic turn holding a marker carries the number of an `agent
//!   read` message holding that marker;
//! - an agent declaring `subagents` Capable has such a turn after a
//!   sub-agent message: only such a turn moves with the splice.

use crate::agent::transcript::{AgentMessage, AgentTranscript, agent_part_text};
use crate::history::Conversation;
use crate::history::environment::test_support::test_cache_base;
use crate::history::provider::contract_tests::{AgentSession, every_record_sessions};

/// The differences between the list's numbering of `session`, `agent read`'s,
/// and the reply number the fixture names.
fn numbering_gaps(session: &AgentSession) -> Vec<String> {
    let row = &session.row;
    let agent = row.source;
    let Some(reply) = &session.reply_after_records_without_text else {
        return vec![format!(
            "{agent:?}: the fixture names no reply after records without text"
        )];
    };
    let transcript =
        AgentTranscript::load_owned(&test_cache_base(), agent, &row.path, &row.subagents).unwrap();
    let mut gaps = Vec::new();
    if row.message_count != transcript.messages.len() {
        gaps.push(format!(
            "{agent:?}: the list counts {} messages, agent read {}",
            row.message_count,
            transcript.messages.len()
        ));
    }
    let listed_reply = listed_number(row, reply.marker);
    let read_reply = read_number(&transcript, reply.marker);
    let expected_reply = Some(reply.number);
    if (listed_reply, read_reply) != (expected_reply, expected_reply) {
        gaps.push(format!(
            "{agent:?}: the list numbers the reply {listed_reply:?}, agent read \
             {read_reply:?}, the fixture {expected_reply:?}"
        ));
    }
    let first_subagent_message = transcript
        .messages
        .iter()
        .find(|message| message.parent_tool_use_id.is_some())
        .map(|message| message.ordinal);
    let mut checks_a_marker_after_a_subagent = false;
    for marker in &session.markers {
        let Some(listed) = listed_number(row, marker.text) else {
            continue;
        };
        checks_a_marker_after_a_subagent |=
            first_subagent_message.is_some_and(|subagent| listed > subagent);
        let is_read_there = transcript
            .messages
            .iter()
            .find(|message| message.ordinal == listed)
            .is_some_and(|message| holds(message, marker.text));
        if !is_read_there {
            gaps.push(format!(
                "{agent:?}: the list numbers the {} {listed}, but agent read's message \
                 {listed} does not hold it",
                marker.record
            ));
        }
    }
    if agent.provider().subagents().is_capable() && !checks_a_marker_after_a_subagent {
        gaps.push(format!(
            "{agent:?}: the agent declares subagents Capable, but no semantic turn holding a \
             marker follows a sub-agent message"
        ));
    }
    gaps
}

/// The message number of the list's semantic turn holding `marker`.
fn listed_number(row: &Conversation, marker: &str) -> Option<usize> {
    row.semantic_turns
        .iter()
        .position(|turn| turn.contains(marker))
        .map(|turn| row.semantic_turn_ranges[turn].start)
}

/// The number of the first message `agent read` shows holding `marker`.
fn read_number(transcript: &AgentTranscript, marker: &str) -> Option<usize> {
    transcript
        .messages
        .iter()
        .find(|message| holds(message, marker))
        .map(|message| message.ordinal)
}

/// True when a part of `message` holds `marker`.
fn holds(message: &AgentMessage, marker: &str) -> bool {
    message
        .parts
        .iter()
        .filter_map(agent_part_text)
        .any(|text| text.contains(marker))
}

#[test]
fn every_agents_list_numbers_its_messages_as_agent_read_does() {
    let gaps: Vec<String> = every_record_sessions()
        .iter()
        .flat_map(numbering_gaps)
        .collect();

    assert!(gaps.is_empty(), "{}", gaps.join("\n"));
}
