//! The session list's token count over each registered agent's every-record
//! session. It equals the count for the session without its sub-agents plus
//! the fixture's hand count of sub-agent tokens. Sub-agent transcripts without
//! user text count too.

use crate::history::provider::contract_tests::{AgentSession, every_record_sessions, list_row};

/// `None` when `session`'s token count equals its own tokens plus its
/// fixture's sub-agent tokens; otherwise a message with each count.
fn token_count_gap(session: &AgentSession) -> Option<String> {
    let row = &session.row;
    let own_tokens = list_row(row.source, &row.path, &[]).total_tokens;
    let expected = own_tokens + session.subagent_tokens;
    (row.total_tokens != expected).then(|| {
        format!(
            "{:?}: the list counts {} tokens, expected {own_tokens} of the session's own and \
             {} of its sub-agents'",
            row.source, row.total_tokens, session.subagent_tokens
        )
    })
}

#[test]
fn every_agents_token_count_adds_each_sub_agent_transcripts_tokens() {
    let gaps: Vec<String> = every_record_sessions()
        .iter()
        .filter_map(token_count_gap)
        .collect();

    assert!(gaps.is_empty(), "{}", gaps.join("\n"));
}
