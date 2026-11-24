//! Agent commands composed from the addresses and flags other commands
//! print, over each registered agent's every-record session:
//! - `agent read --match` of a whole session, with default flags and with
//!   every flag, shows exactly the messages a read of each one alone with
//!   `--match` finds a line in, each with the records and lines that read
//!   prints.

use crate::agent::protocol::{MatchContext, ProtocolOptions, ReadSlice};
use crate::agent::read_coverage::{DEFAULT_FLAGS, EVERY_FLAG, LoadedSession};
use crate::agent::test_support::atom;
use crate::history::provider::contract_tests::{AgentSession, every_record_sessions};
use std::collections::BTreeMap;

/// The `--match` queries each session is read with: its markers and a common
/// word.
fn match_queries(session: &AgentSession) -> Vec<String> {
    session
        .markers
        .iter()
        .map(|marker| marker.text.to_owned())
        .chain(["the".to_owned()])
        .collect()
}

/// Each message's lines in a read, by the message's address: its record,
/// its calls' records and the content lines below them. The header and the
/// `conversation`, `subagent`, `day`, `read` and `continue` records are left
/// out, and so is a record's `tool=`: it names the tool of a call whose
/// record doesn't print above, which depends on the messages the read shows.
fn message_blocks(read: &str) -> BTreeMap<String, Vec<String>> {
    let mut blocks: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut current = None;
    for line in read.lines().skip(1) {
        if line.starts_with('|') {
            if let Some(message) = &current {
                blocks
                    .get_mut(message)
                    .expect("a content line follows its record")
                    .push(line.to_owned());
            }
            continue;
        }
        let address = line.split(' ').next().unwrap_or_default();
        if [
            "protocol",
            "conversation",
            "subagent",
            "day",
            "read",
            "continue",
        ]
        .contains(&address)
        {
            continue;
        }
        let message = message_of(address).to_owned();
        let record = line
            .split(' ')
            .filter(|field| !field.starts_with("tool="))
            .collect::<Vec<_>>()
            .join(" ");
        blocks.entry(message.clone()).or_default().push(record);
        current = Some(message);
    }
    blocks
}

/// The address of the message `address` names or holds: `m7` for `m7` and
/// for its call `m7.2`.
fn message_of(address: &str) -> &str {
    match address.rsplit_once('.') {
        Some((message, call)) if call.bytes().all(|byte| byte.is_ascii_digit()) => message,
        _ => address,
    }
}

/// The messages whose `--match <query>` read of the whole of `session`
/// differs from their reads alone with `options`, and how many messages
/// matched.
fn match_read_differences(
    session: &AgentSession,
    loaded: &LoadedSession,
    query: &str,
    options: ProtocolOptions,
) -> (Vec<String>, usize) {
    let slice = ReadSlice::Match {
        query: query.to_owned(),
        context: MatchContext {
            before: 1,
            after: 1,
        },
    };
    let reference = loaded.resolved.reference.canonical();
    let whole = loaded.read_sliced(&reference, Some(&slice), options);
    let shown = message_blocks(&whole);
    let mut expected = BTreeMap::new();
    for message in &loaded.transcript.messages {
        let address = loaded.addresses.message(message.ordinal);
        let alone = loaded.read_sliced(&format!("{reference}:{address}"), Some(&slice), options);
        let mut blocks = message_blocks(&alone);
        let matched = blocks
            .get(&address)
            .is_some_and(|block| atom(&block[0], "matches").is_some_and(|count| count != "0"));
        // A read alone shows a sub-agent's message that the session's read
        // with `options` hides.
        let shown_by_options = options.subagents || message.subagent.is_none();
        if matched && shown_by_options {
            expected.insert(address.clone(), blocks.remove(&address).unwrap());
        }
    }
    let matched = expected.len();
    let mut differences = Vec::new();
    for address in shown.keys().chain(expected.keys()) {
        if shown.get(address) != expected.get(address) {
            differences.push(format!(
                "{:?} --match {query:?} tools={}: {address}\nwhole read: {:#?}\nalone: {:#?}",
                session.row.source,
                options.tools,
                shown.get(address),
                expected.get(address)
            ));
        }
    }
    let header_count = whole
        .lines()
        .next()
        .and_then(|header| atom(header, "matched-messages"))
        .map(str::to_owned);
    if header_count != Some(matched.to_string()) {
        differences.push(format!(
            "{:?} --match {query:?}: matched-messages={header_count:?}, {matched} matched alone",
            session.row.source
        ));
    }
    (differences, matched)
}

#[test]
fn every_agents_match_read_of_whole_session_shows_what_reads_of_each_message_alone_match() {
    let mut differences = Vec::new();
    let mut matched = 0;
    for session in every_record_sessions() {
        let loaded = LoadedSession::of(&session);
        for query in match_queries(&session) {
            for options in [DEFAULT_FLAGS, EVERY_FLAG] {
                let (found, count) = match_read_differences(&session, &loaded, &query, options);
                differences.extend(found);
                matched += count;
            }
        }
    }

    assert!(differences.is_empty(), "{}", differences.join("\n\n"));
    assert!(matched > 0, "a query matches a message");
}
