//! `agent read --tools --subagents` over each registered agent's every-record
//! session: each agent-searched marker shows, in a call's header, a launch
//! prompt or a sub-agent's message. Tool results stay hidden, so a marker in a
//! call's header cannot show through the call's result.

use crate::agent::protocol::{ProtocolOptions, ReadRequest, format_read};
use crate::agent::refs::{AgentConversationKey, resolved_conversation_for_key};
use crate::agent::transcript::AgentTranscript;
use crate::history::provider::contract_tests::{AgentSession, MarkerSearch, every_record_sessions};

const WITH_TOOLS_AND_SUBAGENTS: ProtocolOptions = ProtocolOptions {
    budget: None,
    tools: true,
    tool_results: false,
    thinking: false,
    subagents: true,
};

/// Each agent-searched marker of `session` missing from `agent read --tools
/// --subagents`.
fn unshown_agent_searched_markers(session: &AgentSession) -> Vec<String> {
    let row = &session.row;
    let transcript = AgentTranscript::load_owned(row.source, &row.path, &row.subagents).unwrap();
    let key = AgentConversationKey::from_conversation(row).unwrap();
    let resolved = resolved_conversation_for_key(std::slice::from_ref(&key), &key);
    let output = format_read(
        &[ReadRequest {
            resolved: &resolved,
            transcript: &transcript,
            range: None,
        }],
        None,
        None,
        WITH_TOOLS_AND_SUBAGENTS,
    )
    .unwrap();
    session
        .markers
        .iter()
        .filter(|marker| marker.search == MarkerSearch::AgentOnly)
        .filter(|marker| !output.contains(marker.text))
        .map(|marker| format!("{:?}: {}", row.source, marker.record))
        .collect()
}

#[test]
fn every_agents_read_with_tools_and_subagents_shows_each_agent_searched_marker() {
    let gaps: Vec<String> = every_record_sessions()
        .iter()
        .flat_map(unshown_agent_searched_markers)
        .collect();

    assert!(gaps.is_empty(), "{}", gaps.join("\n"));
}
