//! Each message's slice of the cached search texts holds the markers
//! `agent read` shows in that message, and no other message's markers.
//! Checked over each registered agent's every-record session, read with its
//! sub-agent transcripts.

use crate::agent::refs::MessageRange;
use crate::agent::transcript::{AgentTranscript, agent_part_text};
use crate::history::Conversation;
use crate::history::provider::contract_tests::{MarkerSearch, every_record_sessions};
use crate::search::literal::Literal;
use std::collections::BTreeSet;

fn messages_holding_in_cache(row: &Conversation, literal: &Literal) -> BTreeSet<usize> {
    (1..=row.message_count)
        .filter(|ordinal| {
            row.texts_of(&[MessageRange::single(*ordinal)])
                .into_array()
                .iter()
                .any(|text| literal.matches(text))
        })
        .collect()
}

fn messages_holding_in_agent_read(
    transcript: &AgentTranscript,
    literal: &Literal,
) -> BTreeSet<usize> {
    transcript
        .messages
        .iter()
        .filter(|message| {
            message
                .parts
                .iter()
                .filter_map(agent_part_text)
                .any(|text| literal.matches(&text))
        })
        .map(|message| message.ordinal)
        .collect()
}

#[test]
fn each_markers_cached_text_is_in_the_message_agent_read_shows_it_in() {
    let misplaced: Vec<String> = every_record_sessions()
        .iter()
        .flat_map(|session| {
            let row = &session.row;
            let transcript =
                AgentTranscript::load_owned(row.source, &row.path, &row.subagents).unwrap();
            session
                .markers
                .iter()
                .filter(|marker| marker.search != MarkerSearch::Unchecked)
                .filter_map(|marker| {
                    let literal = Literal::new(marker.text.to_owned());
                    let cached = messages_holding_in_cache(row, &literal);
                    let shown = messages_holding_in_agent_read(&transcript, &literal);
                    (cached != shown).then(|| {
                        format!(
                            "{:?} {}: cached in {cached:?}, agent read shows it in {shown:?}",
                            row.source, marker.record
                        )
                    })
                })
                .collect::<Vec<_>>()
        })
        .collect();

    assert!(misplaced.is_empty(), "{}", misplaced.join("\n"));
}
