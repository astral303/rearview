//! Every part of every message carries an RFC 3339 time for `at=` and `to=`.
//! Checked over each registered agent's every-record session, read with its
//! sub-agent transcripts.

use crate::agent::transcript::AgentTranscript;
use crate::history::provider::contract_tests::every_record_sessions;
use chrono::DateTime;

#[test]
fn every_agents_message_parts_carry_the_time_they_were_written() {
    let untimed: Vec<String> = every_record_sessions()
        .iter()
        .flat_map(|session| {
            let row = &session.row;
            let transcript =
                AgentTranscript::load_owned(row.source, &row.path, &row.subagents).unwrap();
            transcript
                .messages
                .into_iter()
                .flat_map(|message| {
                    message
                        .parts
                        .iter()
                        .enumerate()
                        .filter(|(_, part)| {
                            part.source()
                                .timestamp
                                .as_deref()
                                .is_none_or(|time| DateTime::parse_from_rfc3339(time).is_err())
                        })
                        .map(|(index, _)| {
                            format!("{:?}: m{} part {index}", row.source, message.ordinal)
                        })
                        .collect::<Vec<_>>()
                })
                .collect::<Vec<_>>()
        })
        .collect();

    assert!(
        untimed.is_empty(),
        "parts without a time:\n{}",
        untimed.join("\n")
    );
}
