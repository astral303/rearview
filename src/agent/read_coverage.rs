//! `agent read` over each registered agent's every-record session.
//! - With `--tools --subagents`, each agent-searched marker shows, in a
//!   call's header, a launch prompt or a sub-agent's message. Tool results
//!   stay hidden, so a marker in a call's header cannot show through the
//!   call's result.
//! - With default flags, the header counts the hidden tool calls, and the
//!   read covers each message a read with every flag covers: a `message` or
//!   a `hidden` record holds it. A read with every flag does not report a
//!   hidden part.
//! - With every flag and half its length as the budget, a read ends on a
//!   whole record, without being cut short at the budget, and covers the
//!   unbudgeted read's first messages, from the first, without a gap.

use crate::agent::protocol::{ProtocolOptions, ReadRequest, format_read};
use crate::agent::refs::{AgentConversationKey, resolved_conversation_for_key};
use crate::agent::transcript::AgentTranscript;
use crate::history::provider::contract_tests::{AgentSession, MarkerSearch, every_record_sessions};
use crate::time_filter::TimeFilter;
use std::collections::BTreeSet;

const WITH_TOOLS_AND_SUBAGENTS: ProtocolOptions = ProtocolOptions {
    budget: None,
    tools: true,
    tool_results: false,
    thinking: false,
    subagents: true,
    window: TimeFilter {
        after: None,
        before: None,
    },
};

const DEFAULT_FLAGS: ProtocolOptions = ProtocolOptions {
    budget: None,
    tools: false,
    tool_results: false,
    thinking: false,
    subagents: false,
    window: TimeFilter {
        after: None,
        before: None,
    },
};

const EVERY_FLAG: ProtocolOptions = ProtocolOptions {
    budget: None,
    tools: true,
    tool_results: true,
    thinking: true,
    subagents: true,
    window: TimeFilter {
        after: None,
        before: None,
    },
};

/// `agent read` of the whole of `session`, with its sub-agent transcripts.
fn read(session: &AgentSession, options: ProtocolOptions) -> String {
    let row = &session.row;
    let transcript = AgentTranscript::load_owned(row.source, &row.path, &row.subagents).unwrap();
    let key = AgentConversationKey::from_conversation(row).unwrap();
    let resolved = resolved_conversation_for_key(std::slice::from_ref(&key), &key);
    format_read(
        &[ReadRequest {
            resolved: &resolved,
            transcript: &transcript,
            range: None,
        }],
        None,
        None,
        options,
    )
    .unwrap()
}

/// Each agent-searched marker of `session` missing from `agent read --tools
/// --subagents`.
fn unshown_agent_searched_markers(session: &AgentSession) -> Vec<String> {
    let output = read(session, WITH_TOOLS_AND_SUBAGENTS);
    session
        .markers
        .iter()
        .filter(|marker| marker.search == MarkerSearch::AgentOnly)
        .filter(|marker| !output.contains(marker.text))
        .map(|marker| format!("{:?}: {}", session.row.source, marker.record))
        .collect()
}

/// The message numbers a read covers: those of its `message` records, and
/// the ranges of its `hidden` records.
fn covered_messages(output: &str) -> BTreeSet<usize> {
    let ordinal = |atom: &str| atom.strip_prefix('m')?.parse::<usize>().ok();
    output
        .lines()
        .filter_map(|line| {
            let rest = line
                .strip_prefix("message ")
                .or_else(|| line.strip_prefix("hidden "))?;
            let range = rest.split_whitespace().next()?;
            let (first, last) = range.split_once("..").unwrap_or((range, range));
            Some(ordinal(first)?..=ordinal(last)?)
        })
        .flatten()
        .collect()
}

/// Each message of `session` that a read with every flag covers and a read
/// with default flags does not.
fn uncovered_messages(session: &AgentSession) -> Vec<String> {
    let covered = covered_messages(&read(session, DEFAULT_FLAGS));
    covered_messages(&read(session, EVERY_FLAG))
        .difference(&covered)
        .map(|ordinal| format!("{:?}: m{ordinal}", session.row.source))
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

#[test]
fn every_agents_read_with_default_flags_covers_each_message_a_read_with_every_flag_covers() {
    let uncovered: Vec<String> = every_record_sessions()
        .iter()
        .flat_map(uncovered_messages)
        .collect();

    assert!(uncovered.is_empty(), "{}", uncovered.join("\n"));
}

#[test]
fn every_agents_read_with_default_flags_counts_its_hidden_tool_calls() {
    let uncounted: Vec<String> = every_record_sessions()
        .iter()
        .filter(|session| {
            let output = read(session, DEFAULT_FLAGS);
            !output.lines().next().unwrap().contains(" hidden-tools=")
        })
        .map(|session| format!("{:?}", session.row.source))
        .collect();

    assert!(uncounted.is_empty(), "{}", uncounted.join("\n"));
}

/// The read of `session` with every flag and half its length as the budget,
/// when it was cut short at the budget, misses the first message, or omits a
/// message before the last one it covers.
fn budget_gap(session: &AgentSession) -> Option<String> {
    let unbudgeted = read(session, EVERY_FLAG);
    let budget = unbudgeted.chars().count() / 2;
    let output = read(
        session,
        ProtocolOptions {
            budget: Some(budget),
            ..EVERY_FLAG
        },
    );
    let covered = covered_messages(&output);
    let leading = covered_messages(&unbudgeted)
        .into_iter()
        .take(covered.len().max(1))
        .collect::<BTreeSet<_>>();
    let cut_short = !output.ends_with('\n');
    (cut_short || covered != leading)
        .then(|| format!("{:?}: covers {covered:?}\n{output}", session.row.source))
}

#[test]
fn every_agents_budgeted_read_covers_its_first_messages_without_being_cut_short() {
    let gaps: Vec<String> = every_record_sessions()
        .iter()
        .filter_map(budget_gap)
        .collect();

    assert!(gaps.is_empty(), "{}", gaps.join("\n"));
}

#[test]
fn every_agents_read_with_every_flag_doesnt_report_a_hidden_part() {
    let reporting: Vec<String> = every_record_sessions()
        .iter()
        .filter(|session| {
            let output = read(session, EVERY_FLAG);
            output.contains(" hidden-") || output.lines().any(|line| line.starts_with("hidden "))
        })
        .map(|session| format!("{:?}", session.row.source))
        .collect();

    assert!(reporting.is_empty(), "{}", reporting.join("\n"));
}
