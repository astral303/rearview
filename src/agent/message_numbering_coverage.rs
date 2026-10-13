//! The session list numbers each agent's messages as `agent read` does. Over
//! each registered agent's every-record session, the row's message count
//! equals `agent read`'s, and the reply after the agent's records without text
//! carries the message number the fixture names, in the list and in
//! `agent read`. The sessions are read without their sub-agent transcripts.

use crate::agent::transcript::{AgentTranscript, agent_part_text};
use crate::history::provider::contract_tests::{AgentSession, every_record_sessions, list_row};

/// The differences between the list's numbering of `session`, `agent read`'s,
/// and the reply number the fixture names.
fn numbering_gaps(session: &AgentSession) -> Vec<String> {
    let agent = session.row.source;
    let Some(reply) = &session.reply_after_records_without_text else {
        return vec![format!(
            "{agent:?}: the fixture names no reply after records without text"
        )];
    };
    let row = list_row(agent, &session.row.path, &[]);
    let transcript = AgentTranscript::load_owned(agent, &session.row.path, &[]).unwrap();
    let mut gaps = Vec::new();
    if row.message_count != transcript.messages.len() {
        gaps.push(format!(
            "{agent:?}: the list counts {} messages, agent read {}",
            row.message_count,
            transcript.messages.len()
        ));
    }
    let read_number = transcript
        .messages
        .iter()
        .find(|message| {
            message
                .parts
                .iter()
                .filter_map(agent_part_text)
                .any(|text| text.contains(reply.marker))
        })
        .map(|message| message.ordinal);
    let listed_number = row
        .semantic_turns
        .iter()
        .position(|turn| turn.contains(reply.marker))
        .map(|turn| row.semantic_turn_ranges[turn].start);
    let expected_number = Some(reply.number);
    if (listed_number, read_number) != (expected_number, expected_number) {
        gaps.push(format!(
            "{agent:?}: the list numbers the reply {listed_number:?}, agent read \
             {read_number:?}, the fixture {expected_number:?}"
        ));
    }
    gaps
}

#[test]
fn every_agents_list_numbers_its_messages_as_agent_read_does() {
    let gaps: Vec<String> = every_record_sessions()
        .iter()
        .flat_map(numbering_gaps)
        .collect();

    assert!(gaps.is_empty(), "{}", gaps.join("\n"));
}
