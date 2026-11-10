//! The semantic stage's read of many sessions' semantic turns, in tasks
//! across threads through [`scan_copies`]: each task reads the turns of one
//! run of a session's entry sequences.

use super::CachedSession;
use super::copies_scan::{CopiesScan, CopyOutcome, KeySplit, SessionsById, scan_copies};
use super::entries;
use super::search_parts::SemanticTurns;
use crate::error::{AppError, Result};
use rusqlite::Connection;
use std::collections::BTreeMap;
use std::ops::RangeInclusive;
use std::sync::{Mutex, MutexGuard, PoisonError};

/// The most entries one task reads, so the largest sessions spread their
/// entries across threads.
const ENTRIES_PER_TASK: i64 = 32_768;

/// The least and the greatest sequence of the session `?1`'s semantic turns,
/// each in its own subquery, read from their key.
const SESSION_SEQUENCE_BOUNDS: &str = "SELECT \
     (SELECT MIN(sequence) FROM semantic_turns WHERE session = ?1), \
     (SELECT MAX(sequence) FROM semantic_turns WHERE session = ?1)";

/// A scanned copy's semantic turns, as [`scan_semantic_turns`] read them.
#[derive(Debug)]
pub(crate) enum ScannedTurns {
    /// The copy's turns, as [`entries::read_semantic_turns`] reads them.
    Read(SemanticTurns),
    /// Another write replaced the copy before the scan ended; read it alone.
    Replaced,
    NoLongerCached,
    /// The copy's lookup or a read of its turns failed.
    Failed(AppError),
}

/// The semantic turns of each of `copies`, by position. Reads each copy as
/// the cache holds it, whether or not its transcripts changed since. Fails
/// when a cache base cannot be opened or read; a failed lookup or read of
/// one copy fails that copy alone.
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
    let builders = copies.iter().map(|_| Mutex::default()).collect::<Vec<_>>();
    let scanned = scan_copies(copies, |_, sessions| {
        Ok(SemanticScan {
            sessions,
            entries_per_task,
            before_task,
            builders: &builders,
        })
    })?;
    Ok(scanned
        .copies
        .into_iter()
        .zip(builders)
        .map(|(outcome, builder)| match outcome {
            CopyOutcome::Scanned(_) => {
                let builder = builder.into_inner().unwrap_or_else(PoisonError::into_inner);
                ScannedTurns::Read(builder.turns)
            }
            CopyOutcome::NoLongerCached => ScannedTurns::NoLongerCached,
            CopyOutcome::Replaced(_) => ScannedTurns::Replaced,
            CopyOutcome::Failed(error) => ScannedTurns::Failed(error),
        })
        .collect())
}

/// The scan of one cache base's copies. Each task's turns are added to its
/// copy's builder as the task ends.
struct SemanticScan<'h> {
    sessions: SessionsById,
    entries_per_task: i64,
    before_task: &'h (dyn Fn() + Sync),
    /// One per scanned copy, by position.
    builders: &'h [Mutex<CopyTurns>],
}

/// A run of one session's entries.
struct SemanticTask {
    session: i64,
    copy: usize,
    sequences: RangeInclusive<i64>,
}

/// A task's read: the ordinal and text of each turn of its entries.
struct TaskTurns {
    copy: usize,
    sequences: RangeInclusive<i64>,
    turns: Vec<(usize, String)>,
}

/// One copy's turns, built from its tasks' reads in sequence order whatever
/// order the tasks end in.
#[derive(Default)]
struct CopyTurns {
    turns: SemanticTurns,
    /// The first sequence of the task whose turns come next.
    next: i64,
    /// The turns of tasks that ended before the tasks ahead of them, by first
    /// sequence, with their last sequence.
    waiting: BTreeMap<i64, (i64, Vec<(usize, String)>)>,
    /// True once a task of the copy failed: its turns are dropped, and later
    /// reads are not added.
    failed: bool,
}

impl CopyTurns {
    /// Add `turns`, the read of the entries in `sequences`, and the waiting
    /// reads that follow them.
    fn add(&mut self, sequences: RangeInclusive<i64>, turns: Vec<(usize, String)>) {
        if self.failed {
            return;
        }
        if *sequences.start() != self.next {
            self.waiting
                .insert(*sequences.start(), (*sequences.end(), turns));
            return;
        }
        let mut next = Some((*sequences.end(), turns));
        while let Some((last, turns)) = next {
            for (ordinal, text) in turns {
                self.turns.push(ordinal, text);
            }
            self.next = last.saturating_add(1);
            next = self.waiting.remove(&self.next);
        }
    }

    fn drop_turns(&mut self) {
        *self = Self {
            failed: true,
            ..Self::default()
        };
    }
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
            if let Some((least, _)) = session_bounds {
                self.builder(copy).next = least;
            }
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

    fn copy_of(&self, task: &SemanticTask) -> Option<usize> {
        Some(task.copy)
    }

    fn read(&self, connection: &Connection, task: SemanticTask) -> Result<TaskTurns> {
        (self.before_task)();
        let mut turns = Vec::new();
        entries::read_semantic_turns_in(
            connection,
            task.session,
            task.sequences.clone(),
            |ordinal, text| turns.push((ordinal, text)),
        )?;
        Ok(TaskTurns {
            copy: task.copy,
            sequences: task.sequences,
            turns,
        })
    }

    fn task_ended(&self, read: TaskTurns) -> Option<TaskTurns> {
        self.builder(read.copy).add(read.sequences, read.turns);
        None
    }

    fn copy_failed(&self, copy: usize) {
        self.builder(copy).drop_turns();
    }
}

impl SemanticScan<'_> {
    fn builder(&self, copy: usize) -> MutexGuard<'_, CopyTurns> {
        self.builders[copy]
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests;
