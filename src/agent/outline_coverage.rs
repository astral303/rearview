//! An outline's header counts the session's own and sub-agent messages, which
//! together are the list's message count, shows its first and last time
//! as `agent read` shows them, and counts the parts its flags hide as
//! `agent read` counts them. Checked over each registered agent's
//! every-record session, read with its sub-agent transcripts.

use crate::agent::address::MessageAddresses;
use crate::agent::protocol::{
    ProtocolOptions, ReadRequest, format_outline, format_outline_with_warnings, format_read,
};
use crate::agent::refs::{ResolvedConversation, parse_read_ref};
use crate::agent::subagents::SubagentHandles;
use crate::agent::test_support::{
    EVERY_PART, atom, read_times, resolved_conversation, times, written_at,
};
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
            shortened_to: None,
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

/// The `hidden-` atoms of an output's header: `hidden-tools=3`.
fn hidden_atoms(output: &str) -> Vec<&str> {
    output
        .lines()
        .next()
        .unwrap()
        .split(' ')
        .filter(|atom| atom.starts_with("hidden-"))
        .collect()
}

#[test]
fn every_agents_outline_header_counts_what_its_flags_hide_as_read_does() {
    let without_flags = ProtocolOptions {
        tools: false,
        tool_results: false,
        thinking: false,
        subagents: false,
        ..EVERY_PART
    };
    let with_tools = ProtocolOptions {
        tools: true,
        ..without_flags
    };
    let mut differing = Vec::new();
    let mut outlines_hiding_parts = 0;
    let mut subagent_outlines = 0;
    for session in every_record_sessions() {
        let row = &session.row;
        let resolved = resolved_conversation(row);
        let transcript =
            AgentTranscript::load_cached_for_test(row.source, &row.path, &row.subagents).unwrap();
        let has_subagent = SubagentHandles::of(&transcript).count() > 0;
        subagent_outlines += usize::from(has_subagent);
        let mut parts = vec![None, Some("m2..m3")];
        if has_subagent {
            parts.push(Some("s1"));
        }
        for part in parts {
            let request = request_of(&resolved, &transcript, part);
            for options in [without_flags, with_tools, EVERY_PART] {
                let outline =
                    format_outline_with_warnings(&request, options, &[], written_at()).unwrap();
                let read =
                    format_read(std::slice::from_ref(&request), None, None, options).unwrap();
                if hidden_atoms(&outline) != hidden_atoms(&read) {
                    differing.push(format!(
                        "{:?} {part:?} {options:?}:\n{outline}\n{read}",
                        row.source
                    ));
                }
                outlines_hiding_parts += usize::from(!hidden_atoms(&outline).is_empty());
            }
        }
    }

    assert!(differing.is_empty(), "{}", differing.join("\n\n"));
    assert!(
        outlines_hiding_parts > 0,
        "an outline without every flag hides a part"
    );
    assert!(
        subagent_outlines > 0,
        "a session has a sub-agent to outline"
    );
}

/// The request `agent outline` and `agent read` make of `part`, the ref's
/// part after the conversation's (`m2..m3`, `s1`); the whole session for
/// `None`.
fn request_of<'a>(
    resolved: &'a ResolvedConversation,
    transcript: &'a AgentTranscript,
    part: Option<&str>,
) -> ReadRequest<'a> {
    let read_ref = parse_read_ref(&format!("ch_123456789abc{}", suffix_of(part))).unwrap();
    let range = read_ref.range.map(|range| {
        MessageAddresses::of_transcript(transcript)
            .resolve(range)
            .unwrap()
    });
    ReadRequest {
        resolved,
        transcript,
        range: range.map(|range| range.range),
        subagent: read_ref.subagent.or(range.and_then(|range| range.subagent)),
        call: read_ref.call,
        shortened_to: range.and_then(|range| range.shortened_to),
    }
}

fn suffix_of(part: Option<&str>) -> String {
    part.map_or_else(String::new, |part| format!(":{part}"))
}
