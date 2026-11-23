//! The places of a session's search hit messages, and the number of its
//! sub-agent handles, read from the message, thread and launching-call rows
//! without the messages' bodies.

use crate::agent::address::HitPlaces;
use crate::agent::refs::Thread;
use crate::agent::subagents::{
    CallPosition, Handle, LaunchOrderHandles, ThreadLaunch, handles_at_launch,
};
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
    let mut threads = HashMap::new();
    for number in numbers {
        let thread = match number {
            0 => Thread::Parent,
            number => Thread::Subagent(handles[&number]),
        };
        threads.insert(thread, thread_ordinals(connection, session, number)?);
    }
    Ok(HitPlaces::of(&threads, ordinals))
}

/// The ordinals of the messages of `session`'s thread `number`, in order:
/// thread 0 is the session's own.
fn thread_ordinals(connection: &Connection, session: i64, number: i64) -> Result<Vec<usize>> {
    Ok(connection
        .prepare_cached(
            "SELECT m.ordinal FROM threads t JOIN messages m ON m.thread = t.id \
             WHERE t.session = ?1 AND t.number = ?2 AND m.ordinal IS NOT NULL ORDER BY m.ordinal",
        )?
        .query_map(params![session, number], |row| {
            row.get::<_, i64>(0).map(|ordinal| ordinal as usize)
        })?
        .collect::<rusqlite::Result<_>>()?)
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

/// The number of `session`'s sub-agent handles, counted as
/// [`handles_at_launch`] counts a read's: one per thread, and one per
/// launching call that has a handle without a thread.
pub(super) fn count_subagents(connection: &Connection, session: i64) -> Result<usize> {
    let handles = launch_order(connection, session)?.handles;
    Ok(handles.threads.len() + handles.threadless.len())
}

/// The handle of each sub-agent thread of `session`, by its thread number,
/// counted as [`handles_at_launch`] counts a read's.
fn handles(connection: &Connection, session: i64) -> Result<HashMap<i64, Handle>> {
    let SessionLaunchOrder {
        thread_numbers,
        handles,
    } = launch_order(connection, session)?;
    Ok(thread_numbers.into_iter().zip(handles.threads).collect())
}

/// The handles [`handles_at_launch`] gives a session's sub-agents, and the
/// number of each thread, in the order of `handles.threads`.
struct SessionLaunchOrder {
    thread_numbers: Vec<i64>,
    handles: LaunchOrderHandles,
}

/// The launch order of `session`'s sub-agent threads and launching calls.
fn launch_order(connection: &Connection, session: i64) -> Result<SessionLaunchOrder> {
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
    let calls = launches_by_id(connection, session)?;
    let thread_launches = threads
        .iter()
        .map(|(_, launch_call, first_message)| ThreadLaunch {
            first_message: *first_message,
            launch: launch_call.as_deref().and_then(|id| calls.get(id)).copied(),
        })
        .collect::<Vec<_>>();
    let launches = calls.values().copied().collect();
    Ok(SessionLaunchOrder {
        thread_numbers: threads.iter().map(|&(number, _, _)| number).collect(),
        handles: handles_at_launch(&thread_launches, launches),
    })
}

/// The first launching call of each id of `session` in session order, read
/// through `parts_launching_calls`. A later call with the same id is a copy
/// and launches nothing.
fn launches_by_id(connection: &Connection, session: i64) -> Result<HashMap<String, CallPosition>> {
    let mut statement = connection.prepare_cached(
        "SELECT m.ordinal, p.call_id FROM parts p \
         JOIN entries e ON e.id = p.entry JOIN messages m ON m.id = e.message \
         WHERE p.session = ?1 AND p.kind = 'call' AND p.tool = 'agent' \
         AND e.contributes AND m.ordinal IS NOT NULL \
         ORDER BY m.ordinal, e.sequence, p.part",
    )?;
    let mut rows = statement.query([session])?;
    let mut calls = HashMap::new();
    let mut previous = CallPosition {
        ordinal: 0,
        rank: 0,
    };
    while let Some(row) = rows.next()? {
        let ordinal = row.get::<_, i64>(0)? as usize;
        let rank = if ordinal == previous.ordinal {
            previous.rank + 1
        } else {
            1
        };
        previous = CallPosition { ordinal, rank };
        calls
            .entry(row.get::<_, Option<String>>(1)?.unwrap_or_default())
            .or_insert(previous);
    }
    Ok(calls)
}
