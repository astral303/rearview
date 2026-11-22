//! The places of a session's search hit messages, read from the message and
//! thread rows without the messages' bodies.

use crate::agent::address::HitPlaces;
use crate::agent::call_links::CallAddress;
use crate::agent::refs::Thread;
use crate::agent::subagents::{Handle, ThreadLaunch, handles_at_launch};
use crate::error::Result;
use rusqlite::{Connection, params};
use std::collections::{BTreeSet, HashMap};

/// The places of the messages of `session` numbered `ordinals`. Read the
/// session's launching calls only when one of `ordinals` is a sub-agent's,
/// because its address needs the handle a read gives it.
pub(super) fn read(connection: &Connection, session: i64, ordinals: &[usize]) -> Result<HitPlaces> {
    let mut numbers = BTreeSet::from([0]);
    numbers.extend(thread_numbers(connection, session, ordinals)?);
    let handles = if numbers.len() > 1 {
        handles(connection, session)?
    } else {
        HashMap::new()
    };
    let mut statement = connection.prepare_cached(
        "SELECT m.ordinal FROM threads t JOIN messages m ON m.thread = t.id \
         WHERE t.session = ?1 AND t.number = ?2 AND m.ordinal IS NOT NULL ORDER BY m.ordinal",
    )?;
    let mut threads = HashMap::new();
    for number in numbers {
        let thread = match number {
            0 => Thread::Parent,
            number => Thread::Subagent(handles[&number]),
        };
        let messages = statement
            .query_map(params![session, number], |row| {
                row.get::<_, i64>(0).map(|ordinal| ordinal as usize)
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        threads.insert(thread, messages);
    }
    Ok(HitPlaces::of(&threads, ordinals))
}

/// The thread number of each message of `session` numbered `ordinals`.
fn thread_numbers(connection: &Connection, session: i64, ordinals: &[usize]) -> Result<Vec<i64>> {
    let ordinals = serde_json::to_string(ordinals)?;
    Ok(connection
        .prepare_cached(
            "SELECT DISTINCT t.number FROM messages m JOIN threads t ON t.id = m.thread \
             WHERE m.session = ?1 AND m.ordinal IN (SELECT value FROM json_each(?2))",
        )?
        .query_map(params![session, ordinals], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?)
}

/// The handle of each sub-agent thread of `session`, by its thread number,
/// counted as [`handles_at_launch`] counts a read's.
fn handles(connection: &Connection, session: i64) -> Result<HashMap<i64, Handle>> {
    let mut statement = connection.prepare_cached(
        "SELECT t.number, t.launch_call, MIN(m.ordinal) FROM threads t \
         JOIN messages m ON m.thread = t.id \
         WHERE t.session = ?1 AND t.number > 0 AND m.ordinal IS NOT NULL GROUP BY t.number",
    )?;
    // Each thread's number, the id of the call that launched it, and its
    // first message. SQLite takes `launch_call` from the row of the first
    // message, as a read takes the thread's origin from its first message.
    let threads = statement
        .query_map([session], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, i64>(2)? as usize,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let launch_ids = threads
        .iter()
        .filter_map(|(_, launch_call, _)| launch_call.as_deref())
        .collect::<Vec<_>>();
    let calls = first_call_of_each_id(connection, session, &launch_ids)?;
    let thread_launches = threads
        .iter()
        .map(|(_, launch_call, first_message)| ThreadLaunch {
            first_message: *first_message,
            launch: launch_call
                .as_deref()
                .and_then(|id| calls.get(id))
                .map(|call| call.address),
        })
        .collect::<Vec<_>>();
    let launches = calls
        .values()
        .filter(|call| call.launches)
        .map(|call| call.address)
        .collect();
    let handles = handles_at_launch(&thread_launches, launches).threads;
    Ok(threads
        .iter()
        .map(|&(number, _, _)| number)
        .zip(handles)
        .collect())
}

/// A call [`first_call_of_each_id`] reads.
struct StoredCall {
    /// Its message and its place among the calls read of that message: it
    /// orders the calls of one message as their numbers do.
    address: CallAddress,
    /// True for a call of tool kind `Agent`.
    launches: bool,
}

/// The first call of each id in session order, of the calls of `session`
/// that launch a sub-agent or carry one of `ids`. A later call with the same
/// id is a copy and launches nothing.
fn first_call_of_each_id(
    connection: &Connection,
    session: i64,
    ids: &[&str],
) -> Result<HashMap<String, StoredCall>> {
    let ids = serde_json::to_string(ids)?;
    let mut statement = connection.prepare_cached(
        "SELECT m.ordinal, p.call_id, p.tool = 'agent' FROM parts p \
         JOIN entries e ON e.id = p.entry JOIN messages m ON m.id = e.message \
         WHERE p.session = ?1 AND p.kind = 'call' AND e.contributes AND m.ordinal IS NOT NULL \
         AND (p.tool = 'agent' OR p.call_id IN (SELECT value FROM json_each(?2))) \
         ORDER BY m.ordinal, e.sequence, p.part",
    )?;
    let mut rows = statement.query(params![session, ids])?;
    let mut calls = HashMap::new();
    let mut previous = CallAddress {
        ordinal: 0,
        number: 0,
    };
    while let Some(row) = rows.next()? {
        let ordinal = row.get::<_, i64>(0)? as usize;
        let number = if ordinal == previous.ordinal {
            previous.number + 1
        } else {
            1
        };
        previous = CallAddress { ordinal, number };
        calls
            .entry(row.get::<_, Option<String>>(1)?.unwrap_or_default())
            .or_insert(StoredCall {
                address: previous,
                launches: row.get::<_, Option<bool>>(2)?.unwrap_or(false),
            });
    }
    Ok(calls)
}
