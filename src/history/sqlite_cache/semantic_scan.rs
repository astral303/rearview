//! The semantic stage's read of many sessions' semantic turns, in tasks
//! across threads through [`scan_copies`]: each task reads one run of a
//! session's entries.

use super::CachedSession;
use super::entries;
use super::text_scan::{CopiesScan, KeySplit, SessionsById, scan_copies};
use crate::error::{AppError, Result};
use rusqlite::Connection;
use std::ops::RangeInclusive;

/// The most entries one task reads, so the largest sessions spread their
/// entries across threads.
const ENTRIES_PER_TASK: i64 = 32_768;

/// The least and the greatest entry sequence of the session `?1`, each in
/// its own subquery, read from the index on `(session, sequence)`.
const SESSION_SEQUENCE_BOUNDS: &str = "SELECT \
     (SELECT MIN(sequence) FROM entries WHERE session = ?1), \
     (SELECT MAX(sequence) FROM entries WHERE session = ?1)";

/// A scanned copy's semantic turns, as [`scan_semantic_turns`] read them.
pub(crate) enum ScannedTurns {
    /// Each message number and semantic text of the copy, in its session's
    /// order, as [`entries::read_semantic_texts`] reads them.
    Read(Vec<(usize, String)>),
    /// Another write replaced the copy before the scan ended; read it alone.
    Replaced,
    NoLongerCached,
    /// A read of the copy's entries failed.
    Failed(AppError),
}

/// The semantic turns of each of `copies`, by position. Reads each copy as
/// the cache holds it, whether or not its transcripts changed since. Fails
/// when the cache cannot be opened or read before any copy's entries; a
/// failed read of one copy's entries fails that copy alone.
pub(crate) fn scan_semantic_turns(copies: &[&CachedSession]) -> Result<Vec<ScannedTurns>> {
    scan_in_tasks_of(copies, ENTRIES_PER_TASK, &|| {})
}

/// [`scan_semantic_turns`] in tasks of at most `entries_per_task` entries,
/// calling `before_task` before each task's read.
fn scan_in_tasks_of(
    copies: &[&CachedSession],
    entries_per_task: i64,
    before_task: &(dyn Fn() + Sync),
) -> Result<Vec<ScannedTurns>> {
    let scanned = scan_copies(copies, |_, sessions| {
        Ok(SemanticScan {
            sessions,
            entries_per_task,
            before_task,
        })
    })?;
    let mut tasks_by_copy = copies.iter().map(|_| Vec::new()).collect::<Vec<_>>();
    for task in scanned.reads {
        tasks_by_copy[task.copy].push(task);
    }
    let mut turns = tasks_by_copy
        .into_iter()
        .map(ScannedTurns::of_tasks)
        .collect::<Vec<_>>();
    for copy in scanned.no_longer_cached {
        turns[copy] = ScannedTurns::NoLongerCached;
    }
    for (copy, _) in scanned.replaced {
        turns[copy] = ScannedTurns::Replaced;
    }
    Ok(turns)
}

impl ScannedTurns {
    /// The turns `tasks`, every task read of one copy, hold together, in
    /// its session's order; the first failure fails them.
    fn of_tasks(mut tasks: Vec<TaskTurns>) -> Self {
        tasks.sort_by_key(|task| task.first_sequence);
        let mut turns = Vec::new();
        for task in tasks {
            match task.turns {
                Ok(read) => turns.extend(read),
                Err(error) => return Self::Failed(error),
            }
        }
        Self::Read(turns)
    }
}

/// The scan of one cache base's copies.
struct SemanticScan<'h> {
    sessions: SessionsById,
    entries_per_task: i64,
    before_task: &'h (dyn Fn() + Sync),
}

/// A run of one session's entries.
struct SemanticTask {
    session: i64,
    copy: usize,
    sequences: RangeInclusive<i64>,
}

/// One task's read: the turns of its run of entries, or the failure of
/// their read.
struct TaskTurns {
    copy: usize,
    first_sequence: i64,
    turns: Result<Vec<(usize, String)>>,
}

impl CopiesScan for SemanticScan<'_> {
    type Task = SemanticTask;
    type Read = TaskTurns;

    /// Each session's entries, from its least to its greatest sequence, in
    /// runs of at most `entries_per_task`.
    fn tasks(&self, connection: &Connection) -> Result<Vec<SemanticTask>> {
        let mut bounds = connection.prepare_cached(SESSION_SEQUENCE_BOUNDS)?;
        let mut tasks = Vec::new();
        for (&session, &(copy, _)) in &self.sessions {
            let session_bounds = bounds.query_row([session], |row| {
                Ok(row.get::<_, Option<i64>>(0)?.zip(row.get(1)?))
            })?;
            tasks.extend(
                KeySplit::of_width(session_bounds, self.entries_per_task)
                    .ranges()
                    .map(|sequences| SemanticTask {
                        session,
                        copy,
                        sequences,
                    }),
            );
        }
        Ok(tasks)
    }

    fn read(&self, connection: &Connection, task: SemanticTask) -> Result<TaskTurns> {
        (self.before_task)();
        let mut turns = Vec::new();
        let read = entries::read_semantic_texts_of_entries(
            connection,
            task.session,
            task.sequences.clone(),
            |ordinal, text| turns.push((ordinal, text)),
        );
        Ok(TaskTurns {
            copy: task.copy,
            first_sequence: *task.sequences.start(),
            turns: read.map(|()| turns),
        })
    }
}

#[cfg(test)]
mod tests;
