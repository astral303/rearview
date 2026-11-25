//! Agent commands composed from the addresses and flags other commands
//! print, over each registered agent's every-record session:
//! - `agent read --match` of a whole session, with default flags and with
//!   every flag, shows exactly the messages a read of each one alone with
//!   `--match` finds a line in, each with the records and lines that read
//!   prints.
//! - An exact `agent within` of an address, a range of the session's own
//!   messages or a sub-agent's, for each marker, returns the whole session's
//!   hits on the messages `agent read` of that address shows with every
//!   flag, and with `--top 1` one of them; some marker's hit, and some
//!   marker's best hit, is outside an address.
//! - `agent read <ref>:m7 --subagents --match <marker>` of a message with a
//!   sub-agent message written during it prints every message `agent within
//!   <ref>:m7` finds the marker in.

use crate::agent::protocol::{MatchContext, ProtocolOptions, ReadSlice};
use crate::agent::read_coverage::{DEFAULT_FLAGS, EVERY_FLAG, LoadedSession};
use crate::agent::refs::{ThreadRange, parse_messages, parse_within_ref};
use crate::agent::search::{
    AgentWithinRequest, WithinAddress, exact_phrase_query, run_within_search,
};
use crate::agent::service::within_address;
use crate::agent::test_support::atom;
use crate::history::provider::contract_tests::{AgentSession, every_record_sessions};
use crate::search::mode::SearchMode;
use crate::time_filter::TimeFilter;
use std::collections::{BTreeMap, BTreeSet};

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

/// The address of the message `address` names, or of the message
/// containing the call it names: `m7` for `m7` and for `m7.2`.
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

/// The addresses `session`'s `agent within` cases search: two ranges of
/// the session's own messages, each sub-agent and each sub-agent's
/// messages from its second on, those that name a message.
fn within_addresses(loaded: &LoadedSession) -> Vec<String> {
    let handles = loaded
        .transcript
        .messages
        .iter()
        .filter_map(|message| {
            let address = loaded.addresses.message(message.ordinal);
            let (handle, _) = address.split_once(':')?;
            Some(handle.to_owned())
        })
        .collect::<BTreeSet<_>>();
    ["m2..m4".to_owned(), "m3..".to_owned()]
        .into_iter()
        .chain(handles.iter().cloned())
        .chain(handles.iter().map(|handle| format!("{handle}:m2..")))
        .filter(|address| {
            parse_messages(address).map_or(true, |range| loaded.addresses.resolve(range).is_ok())
        })
        .collect()
}

/// The addresses of the messages `range`, a hit's `hit=`, names.
fn hit_messages(range: ThreadRange) -> Vec<String> {
    let last = range.last.unwrap_or(range.first);
    (range.first..=last)
        .map(|number| ThreadRange::one(range.thread, number).to_string())
        .collect()
}

/// How many hits the exact `agent within` cases of addresses returned.
#[derive(Default)]
struct WithinCounts {
    /// Hits on the messages at an address.
    at_address: usize,
    /// Whole-session hits outside an address.
    left_out: usize,
    /// Markers whose best whole-session hit is outside an address that
    /// contains another hit of theirs.
    best_outside: usize,
}

/// How an exact `agent within` of `address` for each marker of `session`
/// differs from the whole session's hits on the messages `agent read` of
/// the address shows with every flag, also at `--top 1`.
fn within_address_differences(
    session: &AgentSession,
    loaded: &LoadedSession,
    address: &str,
    counts: &mut WithinCounts,
) -> Vec<String> {
    let reference = format!("{}:{address}", loaded.resolved.reference.canonical());
    let read = loaded.read_sliced(&reference, None, EVERY_FLAG);
    let shown = message_blocks(&read).into_keys().collect::<BTreeSet<_>>();
    let searched = within_address(
        &parse_within_ref(&reference).unwrap(),
        &loaded.resolved,
        &loaded.transcript,
    )
    .unwrap();
    let sorted = |hits: &[ThreadRange]| {
        let mut hits = hits.iter().map(ToString::to_string).collect::<Vec<_>>();
        hits.sort_unstable();
        hits
    };
    let mut differences = Vec::new();
    for marker in &session.markers {
        let within = |searched, top| exact_within_hits(session, loaded, marker.text, searched, top);
        let whole = within(None, usize::MAX);
        let expected = whole
            .iter()
            .copied()
            .filter(|&hit| {
                hit_messages(hit)
                    .iter()
                    .all(|message| shown.contains(message))
            })
            .collect::<Vec<_>>();
        let found = within(searched.clone(), usize::MAX);
        counts.at_address += found.len();
        counts.left_out += whole.len() - expected.len();
        if sorted(&found) != sorted(&expected) {
            differences.push(format!(
                "{:?} within {address} {:?} returns the whole session's hits on the messages \
                 its read shows, {:?}, not {:?}:\n{read}",
                session.row.source,
                marker.text,
                sorted(&expected),
                sorted(&found)
            ));
        }
        if let Some(best) = whole.first().filter(|_| !expected.is_empty()) {
            counts.best_outside += usize::from(!expected.contains(best));
            let top = within(searched.clone(), 1);
            if top.len() != 1 {
                differences.push(format!(
                    "{:?} within {address} {:?} --top 1 returns one of its {} hits, not {top:?}",
                    session.row.source,
                    marker.text,
                    expected.len()
                ));
            }
        }
    }
    differences
}

#[test]
fn every_agents_within_address_returns_whole_session_hits_its_read_shows() {
    let mut differences = Vec::new();
    let mut counts = WithinCounts::default();
    for session in every_record_sessions() {
        let loaded = LoadedSession::of(&session);
        for address in within_addresses(&loaded) {
            differences.extend(within_address_differences(
                &session,
                &loaded,
                &address,
                &mut counts,
            ));
        }
    }

    assert!(differences.is_empty(), "{}", differences.join("\n\n"));
    assert!(
        counts.at_address > 0 && counts.left_out > 0 && counts.best_outside > 0,
        "an address contains a marker's hit, one leaves a hit out, and one leaves out a \
         best hit before --top: {} {} {}",
        counts.at_address,
        counts.left_out,
        counts.best_outside
    );
}

/// The addresses of the session's own messages with a sub-agent message
/// written during them: `m7` when `agent read <ref>:m7 --subagents` reads a
/// sub-agent's message too.
fn own_messages_with_subagent_messages(loaded: &LoadedSession) -> Vec<String> {
    let messages = &loaded.transcript.messages;
    messages
        .iter()
        .filter(|message| message.subagent.is_none())
        .map(|message| loaded.addresses.message(message.ordinal))
        .filter(|address| {
            let range = loaded
                .addresses
                .resolve(parse_messages(address).unwrap())
                .unwrap()
                .range;
            messages.iter().any(|message| {
                message.subagent.is_some()
                    && range.start <= message.ordinal
                    && message.ordinal <= range.end
            })
        })
        .collect()
}

/// The `hit=` of each hit an exact `agent within` for `marker` of
/// `searched`, the messages at an address, or the whole session, returns,
/// best first, at most `top` of them.
fn exact_within_hits(
    session: &AgentSession,
    loaded: &LoadedSession,
    marker: &str,
    searched: Option<WithinAddress>,
    top: usize,
) -> Vec<ThreadRange> {
    let request = AgentWithinRequest {
        query: exact_phrase_query(marker),
        top,
        cli_mode: Some(SearchMode::Exact),
        config_mode: None,
        tui_semantic_search: None,
        budget: None,
        window: TimeFilter::default(),
        address: searched,
    };
    run_within_search(
        &request,
        &session.row,
        &loaded.resolved,
        &loaded.transcript,
        &[],
    )
    .hits
    .iter()
    .map(|hit| {
        hit.addresses
            .expect("a hit on a read session has addresses")
            .hit
    })
    .collect()
}

#[test]
fn every_agents_match_read_of_own_message_searches_subagent_messages_written_during_it() {
    let mut missing = Vec::new();
    let mut subagent_hits = 0;
    for session in every_record_sessions() {
        let loaded = LoadedSession::of(&session);
        for address in own_messages_with_subagent_messages(&loaded) {
            let reference = format!("{}:{address}", loaded.resolved.reference.canonical());
            let searched = within_address(
                &parse_within_ref(&reference).unwrap(),
                &loaded.resolved,
                &loaded.transcript,
            )
            .unwrap();
            for marker in &session.markers {
                let slice = ReadSlice::Match {
                    query: marker.text.to_owned(),
                    context: MatchContext {
                        before: 0,
                        after: 0,
                    },
                };
                let read = loaded.read_sliced(&reference, Some(&slice), EVERY_FLAG);
                let printed = message_blocks(&read).into_keys().collect::<BTreeSet<_>>();
                let hits =
                    exact_within_hits(&session, &loaded, marker.text, searched.clone(), usize::MAX);
                for message in hits.into_iter().flat_map(hit_messages) {
                    subagent_hits += usize::from(message.starts_with('s'));
                    if !printed.contains(&message) {
                        missing.push(format!(
                            "{:?} read {address} --match {:?} doesn't print {message}, a hit \
                             of within {address}:\n{read}",
                            session.row.source, marker.text
                        ));
                    }
                }
            }
        }
    }

    assert!(missing.is_empty(), "{}", missing.join("\n\n"));
    assert!(
        subagent_hits > 0,
        "within of a message finds a marker in a sub-agent message written during it"
    );
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
