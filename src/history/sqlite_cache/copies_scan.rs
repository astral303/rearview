//! The read of many sessions' copies in tasks across threads, each rayon
//! work split on a read connection of its own, for the lexical scan
//! ([`super::text_scan`]) and the semantic one ([`super::semantic_scan`]):
//! [`scan_copies`] runs the tasks, rechecks the sessions read and reads a
//! replaced copy alone.

use super::cached_session::read_copy_in;
use super::copy_state::{CopyMiss, CopyRead, RowState, unless_every_read_failed};
use super::{CacheBase, CachedSession, HeldCopy, SqliteCache};
use crate::error::{AppError, Result};
use rayon::prelude::*;
use rusqlite::Connection;
use std::collections::HashMap;
use std::ops::RangeInclusive;
use std::sync::{Mutex, PoisonError};

/// Row ranges per thread that a scan of each large table is split into.
/// Many small ranges keep every thread busy until the scan ends, as rows
/// differ in size by orders of magnitude.
pub(super) const RANGES_PER_THREAD: i64 = 16;
/// The fewest rows a range holds, so a small table is one range.
const MIN_ROWS_PER_RANGE: i64 = 4096;

/// A scan's kept reads, at most one per task, and what it found of each
/// copy: the copy's data, or the reason the scan missed it. A copy's miss
/// leaves the reads holding some, all or none of its content: callers drop
/// it.
pub(crate) struct ScannedCopies<R, A> {
    pub(crate) reads: Vec<R>,
    /// One per scanned copy, by the copy's position.
    pub(crate) copies: Vec<CopyRead<ScannedCopy<(), A>>>,
}

/// The data a scan found of one copy.
#[derive(Debug)]
pub(crate) enum ScannedCopy<S, A> {
    /// The scan read the copy: its reads hold the copy's content, and `S`
    /// holds what the caller built of it.
    Scanned(S),
    /// Another write replaced the copy before the scan ended. The scan read
    /// the copy the cache then held alone, and `A` holds that read; the
    /// reads may hold content of either copy, and callers drop it.
    ReadAlone(A),
}

/// A copy's state as the scan finds it in one snapshot.
#[derive(Debug)]
pub(super) enum Lookup {
    /// The cache holds the scanned copy, in the session row of this id.
    Scanned(i64),
    /// The cache holds another copy of the session: read it alone.
    Replaced,
    Missed(CopyMiss),
}

impl Lookup {
    /// The state of `copy` in `connection`'s snapshot.
    pub(super) fn of(connection: &Connection, copy: &CachedSession) -> Self {
        match RowState::at(connection, &copy.address).map(RowState::listed) {
            Ok(Ok(stored)) if stored.generation == copy.generation => Self::Scanned(stored.id),
            Ok(Ok(_)) => Self::Replaced,
            Ok(Err(miss)) => Self::Missed(miss),
            Err(error) => Self::Missed(CopyMiss::ReadError(error)),
        }
    }
}

/// One cache base's part of a scan of many sessions' copies, split into
/// tasks that threads read, each in a snapshot of its own.
pub(super) trait CopiesScan: Sync {
    type Task: Send;
    type Read: Send;
    /// The scan's tasks, planned on `connection` in the snapshot that found
    /// the scanned sessions.
    fn tasks(&self, connection: &Connection) -> Result<Vec<Self::Task>>;
    /// The position of the one copy `task` reads; `None` for a task of
    /// several sessions. A failure of such a task fails the scan.
    fn copy_of(&self, task: &Self::Task) -> Option<usize>;
    /// `task`'s read on `connection`, inside a snapshot of its own.
    fn read(&self, connection: &Connection, task: Self::Task) -> Result<Self::Read>;
    /// Takes a task's `read` as the task ends, on the task's thread; the scan
    /// keeps what this returns in [`ScannedCopies::reads`].
    fn task_ended(&self, read: Self::Read) -> Option<Self::Read> {
        Some(read)
    }
    /// Called on the task's thread when a task of the copy at `copy` fails or
    /// finds the database gone, before the scan ends: an implementation
    /// holding reads of the copy can drop them here.
    fn copy_failed(&self, _copy: usize) {}
    /// Maps each scanned session's row id to the position in `tasks` of its
    /// last task, read on `connection` in the snapshot that planned `tasks`.
    /// [`scan_copies_in_waves`] delivers a session missing from the map with
    /// the last wave.
    fn last_tasks(
        &self,
        _connection: &Connection,
        _tasks: &[Self::Task],
    ) -> Result<HashMap<i64, usize>> {
        Ok(HashMap::new())
    }
}

/// A task's outcome.
enum TaskEnd<R> {
    /// The task's read, when [`CopiesScan::task_ended`] keeps it.
    Succeeded(Option<R>),
    /// The cache base's database is gone.
    DatabaseGone,
    Failed(AppError),
}

/// The scanned copies by their session's row id: each copy's position and
/// generation.
pub(super) type SessionsById = HashMap<i64, (usize, i64)>;

/// Reads the tasks `plan` makes of `copies`' sessions on the rayon pool, each
/// in a snapshot of its own. Each rayon work split opens one read connection.
/// `plan` receives the sessions the cache holds as each copy read them; the
/// other copies are not scanned. Reads each copy as the cache holds it,
/// whether or not its transcripts changed since.
///
/// `copies` holds at most one copy of each session: the commands and the TUI
/// pass one copy per listed row, and a load lists each session once whatever
/// its root's spelling.
///
/// - A miss in a copy's lookup, or a failed task of one copy, misses that
///   copy alone.
/// - A task finding the database gone leaves its copy not there, or every
///   copy of the base when the task reads several sessions.
/// - Fails when a cache base cannot be opened or read, a task of several
///   sessions fails, or the read of every copy fails: the first failure is
///   the cache's, not one session's.
/// - Once every task ends, each scanned session is looked up again. A copy
///   another write replaced, or found not ready, is read alone by
///   `read_alone`, given its position, from the copy the cache holds in that
///   snapshot.
pub(super) fn scan_copies<S: CopiesScan, A>(
    copies: &[&CachedSession],
    plan: impl Fn(&Connection, SessionsById) -> Result<S>,
    read_alone: impl Fn(usize, &HeldCopy<'_>) -> Result<A>,
) -> Result<ScannedCopies<S::Read, A>> {
    let mut reads = Vec::new();
    let mut scanned = copies.iter().map(|_| None).collect::<Vec<_>>();
    scan_copies_in_waves(copies, Waves::ONE, plan, read_alone, |wave| {
        reads.extend(wave.reads);
        for (position, found) in wave.copies {
            scanned[position] = Some(found);
        }
    })?;
    Ok(ScannedCopies {
        reads,
        copies: unless_every_read_failed(
            scanned
                .into_iter()
                .map(|copy| copy.expect("every copy's cache base is scanned"))
                .collect(),
        )?,
    })
}

/// Looks the copy at `position` up again in `connection`'s snapshot once its
/// tasks ended, and returns the scan's result for it. Reads the copy alone
/// through `read_alone` when another write replaced it or it is not ready.
fn recheck<A>(
    connection: &Connection,
    copies: &[&CachedSession],
    position: usize,
    lookups: &mut [Option<Lookup>],
    read_alone: &impl Fn(usize, &HeldCopy<'_>) -> Result<A>,
) -> CopyRead<ScannedCopy<(), A>> {
    let copy = copies[position];
    let lookup = match lookups[position].take() {
        Some(Lookup::Scanned(_)) => Lookup::of(connection, copy),
        lookup => lookup.expect("every copy of the base is looked up"),
    };
    match lookup {
        Lookup::Scanned(_) => Ok(ScannedCopy::Scanned(())),
        Lookup::Replaced | Lookup::Missed(CopyMiss::NotReady) => {
            read_copy_in(connection, &copy.address, |held| read_alone(position, held))
                .map(ScannedCopy::ReadAlone)
        }
        Lookup::Missed(miss) => Err(miss),
    }
}

/// One wave of [`scan_copies_in_waves`]: its tasks' kept reads, and the
/// scan's result for each copy due in the wave, by the copy's position. A
/// copy is due in the wave that runs its last task.
pub(crate) struct ScannedWave<R, A> {
    pub(crate) reads: Vec<R>,
    pub(crate) copies: Vec<(usize, CopyRead<ScannedCopy<(), A>>)>,
}

/// The waves a scan reads its tasks in.
#[derive(Clone, Copy, Debug)]
pub(super) struct Waves {
    pub(super) most: usize,
    /// The fewest tasks of every wave but the last: a wave of fewer tasks
    /// than threads leaves threads idle.
    pub(super) least_tasks: usize,
}

impl Waves {
    /// Every task in one wave.
    pub(super) const ONE: Self = Self {
        most: 1,
        least_tasks: 1,
    };

    /// The tasks of every wave but the last, for a scan of `tasks` tasks.
    fn tasks_per_wave(self, tasks: usize) -> usize {
        tasks
            .div_ceil(self.most.max(1))
            .max(self.least_tasks)
            .max(1)
    }
}

/// [`scan_copies`] in `waves` of the tasks `plan` makes, in their order,
/// each wave's tasks across threads. `deliver` receives each wave as it
/// ends: its reads, and the copies whose last task
/// ([`CopiesScan::last_tasks`]) it ran, each rechecked as [`scan_copies`]
/// rechecks every copy. A copy the plan didn't find is delivered with the
/// first wave.
///
/// Fails as [`scan_copies`] does, once the waves before the failure are
/// delivered; a failure of every copy is delivered copy by copy.
pub(super) fn scan_copies_in_waves<S: CopiesScan, A>(
    copies: &[&CachedSession],
    waves: Waves,
    plan: impl Fn(&Connection, SessionsById) -> Result<S>,
    read_alone: impl Fn(usize, &HeldCopy<'_>) -> Result<A>,
    mut deliver: impl FnMut(ScannedWave<S::Read, A>),
) -> Result<()> {
    let mut by_base: HashMap<&std::path::Path, Vec<usize>> = HashMap::new();
    for (position, copy) in copies.iter().enumerate() {
        by_base.entry(copy.base.path()).or_default().push(position);
    }
    let mut lookups = copies.iter().map(|_| None).collect::<Vec<_>>();
    for positions in by_base.into_values() {
        let base = &copies[positions[0]].base;
        let Some(cache) = SqliteCache::open_waiting(base)? else {
            deliver(ScannedWave {
                reads: Vec::new(),
                copies: positions
                    .iter()
                    .map(|&position| (position, Err(CopyMiss::NotThere)))
                    .collect(),
            });
            continue;
        };
        let planned = cache.with_read_snapshot(|connection| {
            let mut sessions = SessionsById::new();
            for &position in &positions {
                let copy = copies[position];
                let lookup = Lookup::of(connection, copy);
                if let Lookup::Scanned(session) = lookup {
                    let earlier = sessions.insert(session, (position, copy.generation));
                    debug_assert!(earlier.is_none(), "a scan holds one copy per session");
                }
                lookups[position] = Some(lookup);
            }
            if sessions.is_empty() {
                return Ok(None);
            }
            let scan = plan(connection, sessions)?;
            let tasks = scan.tasks(connection)?;
            let tasks_per_wave = waves.tasks_per_wave(tasks.len());
            let last_tasks = if tasks_per_wave < tasks.len() {
                scan.last_tasks(connection, &tasks)?
            } else {
                HashMap::new()
            };
            Ok(Some((scan, tasks, tasks_per_wave, last_tasks)))
        })?;
        let Some((scan, tasks, tasks_per_wave, last_tasks)) = planned else {
            let rechecked = cache.with_read_snapshot(|connection| {
                Ok(positions
                    .iter()
                    .map(|&position| {
                        let found =
                            recheck(connection, copies, position, &mut lookups, &read_alone);
                        (position, found)
                    })
                    .collect())
            })?;
            deliver(ScannedWave {
                reads: Vec::new(),
                copies: rechecked,
            });
            continue;
        };
        let last_wave = tasks.len().saturating_sub(1) / tasks_per_wave;
        let wave_of = |position: usize, lookups: &[Option<Lookup>]| match &lookups[position] {
            Some(Lookup::Scanned(session)) => last_tasks
                .get(session)
                .map_or(last_wave, |task| task / tasks_per_wave),
            _ => 0,
        };
        let due_waves = positions
            .iter()
            .map(|&position| (position, wave_of(position, &lookups)))
            .collect::<Vec<_>>();
        let mut remaining = tasks.into_iter();
        let readers = OpenReaders::default();
        for wave in 0..=last_wave {
            let wave_tasks = remaining.by_ref().take(tasks_per_wave).collect();
            let reads = read_tasks(&scan, wave_tasks, base, &readers, &positions, &mut lookups)?;
            let rechecked = cache.with_read_snapshot(|connection| {
                Ok(due_waves
                    .iter()
                    .filter(|(_, due)| *due == wave)
                    .map(|&(position, _)| {
                        let found =
                            recheck(connection, copies, position, &mut lookups, &read_alone);
                        (position, found)
                    })
                    .collect())
            })?;
            deliver(ScannedWave {
                reads,
                copies: rechecked,
            });
        }
    }
    Ok(())
}

/// The reads of `scan`'s `tasks` of the copies at `positions`, on the rayon
/// pool, each in a snapshot of its own on `base`. A task's failure or a
/// database found gone is recorded in `lookups`; a failure of a task of
/// several sessions fails the scan.
fn read_tasks<S: CopiesScan>(
    scan: &S,
    tasks: Vec<S::Task>,
    base: &CacheBase,
    readers: &OpenReaders,
    positions: &[usize],
    lookups: &mut [Option<Lookup>],
) -> Result<Vec<S::Read>> {
    let task_ends = tasks
        .into_par_iter()
        .map_init(
            || readers.lend(),
            |reader: &mut LentReader<'_>, task| {
                let copy = scan.copy_of(&task);
                let end = match open_once(&mut reader.reader, base) {
                    Ok(Some(cache)) => {
                        match cache.with_read_snapshot(|connection| scan.read(connection, task)) {
                            Ok(read) => TaskEnd::Succeeded(scan.task_ended(read)),
                            Err(error) => TaskEnd::Failed(error),
                        }
                    }
                    Ok(None) => TaskEnd::DatabaseGone,
                    Err(error) => TaskEnd::Failed(error),
                };
                if let (Some(copy), TaskEnd::DatabaseGone | TaskEnd::Failed(_)) = (copy, &end) {
                    scan.copy_failed(copy);
                }
                (copy, end)
            },
        )
        .collect::<Vec<_>>();
    let mut reads = Vec::new();
    for (copy, end) in task_ends {
        match (copy, end) {
            (_, TaskEnd::Succeeded(read)) => reads.extend(read),
            (Some(copy), TaskEnd::DatabaseGone) => {
                lookups[copy] = Some(Lookup::Missed(CopyMiss::NotThere));
            }
            (None, TaskEnd::DatabaseGone) => {
                for &position in positions {
                    lookups[position] = Some(Lookup::Missed(CopyMiss::NotThere));
                }
            }
            (Some(copy), TaskEnd::Failed(error)) => {
                lookups[copy] = Some(Lookup::Missed(CopyMiss::ReadError(error)));
            }
            (None, TaskEnd::Failed(error)) => return Err(error),
        }
    }
    Ok(reads)
}

/// Read connections a scan's work splits hand back as they end, for the
/// splits of its later waves: a wave doesn't open a connection per split.
#[derive(Default)]
struct OpenReaders(Mutex<Vec<SqliteCache>>);

impl OpenReaders {
    /// A connection a split ended with, or none yet.
    fn lend(&self) -> LentReader<'_> {
        let reader = self.0.lock().unwrap_or_else(PoisonError::into_inner).pop();
        LentReader {
            readers: self,
            reader,
        }
    }
}

/// A work split's read connection, handed back to its [`OpenReaders`] as the
/// split ends.
struct LentReader<'r> {
    readers: &'r OpenReaders,
    reader: Option<SqliteCache>,
}

impl Drop for LentReader<'_> {
    fn drop(&mut self) {
        if let Some(reader) = self.reader.take() {
            self.readers
                .0
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(reader);
        }
    }
}

/// The read connection of this rayon work split: the one [`OpenReaders`]
/// lent it, or else one opened here on its first task; `None` when `base`'s
/// database is gone.
fn open_once<'r>(
    reader: &'r mut Option<SqliteCache>,
    base: &CacheBase,
) -> Result<Option<&'r SqliteCache>> {
    if reader.is_none() {
        *reader = SqliteCache::open_waiting(base)?;
    }
    Ok(reader.as_ref())
}

/// A key, from its least to its greatest value, split into ranges of equal
/// width: a table's across threads, or one session's rows.
pub(super) struct KeySplit {
    least: i64,
    greatest: i64,
    width: i64,
}

impl KeySplit {
    /// The split of `table`'s `key`, the first column of an index, into at
    /// most `ranges_per_thread` ranges per thread. The split of an empty
    /// table is empty.
    pub(super) fn of(
        connection: &Connection,
        table: &str,
        key: &str,
        ranges_per_thread: i64,
    ) -> Result<Self> {
        let bounds = connection.query_row(&Self::bounds_query(table, key), [], |row| {
            Ok(row.get::<_, Option<i64>>(0)?.zip(row.get(1)?))
        })?;
        Ok(Self::spanning(bounds, ranges_per_thread))
    }

    /// The split of the keys from the least to the greatest of `bounds` into
    /// at most `ranges_per_thread` ranges per thread; empty without bounds.
    pub(super) fn spanning(bounds: Option<(i64, i64)>, ranges_per_thread: i64) -> Self {
        let span = bounds.map_or(0, |(least, greatest)| {
            (greatest - least).saturating_add(1).max(0)
        });
        let threads = i64::try_from(rayon::current_num_threads()).unwrap_or(1);
        let count = (span / MIN_ROWS_PER_RANGE)
            .clamp(1, threads * ranges_per_thread)
            .min(span.max(1));
        Self::of_width(bounds, (span + count - 1) / count)
    }

    /// The split of the keys from the least to the greatest of `bounds`
    /// into ranges of at most `width` keys; empty without bounds.
    pub(super) fn of_width(bounds: Option<(i64, i64)>, width: i64) -> Self {
        let (least, greatest) = bounds.unwrap_or((1, 0));
        Self {
            least,
            greatest,
            width: width.max(1),
        }
    }

    /// The least and the greatest `key` of `table`, each in its own subquery:
    /// SQLite reads a lone MIN or MAX from one end of an index, but scans the
    /// whole table for the two together.
    fn bounds_query(table: &str, key: &str) -> String {
        format!("SELECT (SELECT MIN({key}) FROM {table}), (SELECT MAX({key}) FROM {table})")
    }

    pub(super) fn ranges(&self) -> impl Iterator<Item = RangeInclusive<i64>> + use<> {
        let (greatest, width) = (self.greatest, self.width);
        let first = Some(self.least).filter(|least| *least <= greatest);
        std::iter::successors(first, move |start| {
            start.checked_add(width).filter(|next| *next <= greatest)
        })
        .map(move |start| start..=start.saturating_add(width - 1).min(greatest))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::sqlite_cache::tests::every_record_copies;

    /// A scan whose planning stamps the database with another schema
    /// version, so that every task finds the cache gone: one task per copy,
    /// or one task of every copy. `failed` records each copy
    /// [`CopiesScan::copy_failed`] is called for.
    struct DatabaseGoneScan {
        database: std::path::PathBuf,
        sessions: SessionsById,
        task_per_copy: bool,
        failed: std::sync::Arc<std::sync::Mutex<Vec<usize>>>,
    }

    impl CopiesScan for DatabaseGoneScan {
        type Task = Option<usize>;
        type Read = ();

        fn tasks(&self, _: &Connection) -> Result<Vec<Option<usize>>> {
            Connection::open(&self.database)?.execute(
                "UPDATE meta SET value = 'another' WHERE key = 'schema_version'",
                [],
            )?;
            Ok(if self.task_per_copy {
                self.sessions
                    .values()
                    .map(|&(copy, _)| Some(copy))
                    .collect()
            } else {
                vec![None]
            })
        }

        fn copy_of(&self, task: &Option<usize>) -> Option<usize> {
            *task
        }

        fn read(&self, _: &Connection, _: Option<usize>) -> Result<()> {
            panic!("no task reads a database that is gone")
        }

        fn copy_failed(&self, copy: usize) {
            self.failed.lock().unwrap().push(copy);
        }
    }

    #[test]
    fn tasks_finding_the_database_gone_leave_their_copies_not_there() {
        for task_per_copy in [true, false] {
            let base = tempfile::tempdir().unwrap();
            let copies = every_record_copies(base.path());
            let references = copies.iter().collect::<Vec<_>>();
            let failed = std::sync::Arc::default();

            let scanned = scan_copies(
                &references,
                |_, sessions| {
                    Ok(DatabaseGoneScan {
                        database: super::super::database_path(copies[0].base.path()),
                        sessions,
                        task_per_copy,
                        failed: std::sync::Arc::clone(&failed),
                    })
                },
                |_, _| Ok(()),
            )
            .unwrap();

            assert!(
                scanned
                    .copies
                    .iter()
                    .all(|outcome| matches!(outcome, Err(CopyMiss::NotThere))),
                "task per copy {task_per_copy}: every copy is not there: \
                 {:?}",
                scanned.copies
            );
            let mut failed = failed.lock().unwrap().clone();
            failed.sort_unstable();
            let expected = if task_per_copy {
                (0..copies.len()).collect()
            } else {
                Vec::new()
            };
            assert_eq!(
                failed, expected,
                "task per copy {task_per_copy}: the scan calls `copy_failed` for each task's copy"
            );
        }
    }

    /// A scan of one task per copy, in copy order, whose task of the first
    /// copy fails; `events` records each task's read and each copy
    /// [`CopiesScan::copy_failed`] is called for, in order.
    struct FirstTaskFailingScan {
        sessions: SessionsById,
        events: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    }

    impl CopiesScan for FirstTaskFailingScan {
        type Task = usize;
        type Read = usize;

        fn tasks(&self, _: &Connection) -> Result<Vec<usize>> {
            let mut copies = self
                .sessions
                .values()
                .map(|&(copy, _)| copy)
                .collect::<Vec<_>>();
            copies.sort_unstable();
            Ok(copies)
        }

        fn copy_of(&self, task: &usize) -> Option<usize> {
            Some(*task)
        }

        fn read(&self, _: &Connection, task: usize) -> Result<usize> {
            self.events.lock().unwrap().push(format!("read {task}"));
            if task == 0 {
                Err(AppError::SessionCache(rusqlite::Error::InvalidQuery))
            } else {
                Ok(task)
            }
        }

        fn copy_failed(&self, copy: usize) {
            self.events.lock().unwrap().push(format!("failed {copy}"));
        }
    }

    /// One thread reads the tasks in order, so the hook runs before the
    /// next task's read only when the failing task's thread calls it.
    #[test]
    fn failed_task_reports_its_copy_failed_before_the_next_task_reads() {
        let base = tempfile::tempdir().unwrap();
        let copies = every_record_copies(base.path());
        let references = copies.iter().collect::<Vec<_>>();
        let events = std::sync::Arc::default();
        let one_thread = rayon::ThreadPoolBuilder::new()
            .num_threads(1)
            .build()
            .unwrap();

        let scanned = one_thread
            .install(|| {
                scan_copies(
                    &references,
                    |_, sessions| {
                        Ok(FirstTaskFailingScan {
                            sessions,
                            events: std::sync::Arc::clone(&events),
                        })
                    },
                    |_, _| Ok(()),
                )
            })
            .unwrap();

        assert_eq!(
            events.lock().unwrap()[..3],
            ["read 0", "failed 0", "read 1"],
            "the scan calls `copy_failed` for the failing task's copy as that task ends"
        );
        assert_eq!(
            events
                .lock()
                .unwrap()
                .iter()
                .filter(|event| event.starts_with("failed"))
                .count(),
            1,
            "the scan calls `copy_failed` for the failing task's copy alone"
        );
        assert!(
            matches!(scanned.copies[0], Err(CopyMiss::ReadError(_))),
            "the copy's read fails: {:?}",
            scanned.copies[0]
        );
    }

    /// A scan of one task per scanned copy, in copy order; each task reads
    /// its copy's position and is its copy's last task.
    struct TaskPerCopyScan {
        sessions: SessionsById,
    }

    impl CopiesScan for TaskPerCopyScan {
        type Task = usize;
        type Read = usize;

        fn tasks(&self, _: &Connection) -> Result<Vec<usize>> {
            let mut copies = self
                .sessions
                .values()
                .map(|&(copy, _)| copy)
                .collect::<Vec<_>>();
            copies.sort_unstable();
            Ok(copies)
        }

        fn copy_of(&self, task: &usize) -> Option<usize> {
            Some(*task)
        }

        fn read(&self, _: &Connection, task: usize) -> Result<usize> {
            Ok(task)
        }

        fn last_tasks(&self, _: &Connection, tasks: &[usize]) -> Result<HashMap<i64, usize>> {
            Ok(self
                .sessions
                .iter()
                .map(|(&session, &(copy, _))| {
                    let task = tasks.iter().position(|task| *task == copy).unwrap();
                    (session, task)
                })
                .collect())
        }
    }

    #[test]
    fn scan_in_waves_delivers_each_copy_once_with_the_wave_of_its_last_task() {
        let base = tempfile::tempdir().unwrap();
        let copies = every_record_copies(base.path());
        let references = copies.iter().collect::<Vec<_>>();
        let replaced = copies.len() - 1;
        super::super::tests::replace_copy(&copies[replaced]);
        let scanned = copies.len() - 1;
        let mut waves = Vec::new();

        scan_copies_in_waves(
            &references,
            Waves {
                most: scanned,
                least_tasks: 1,
            },
            |_, sessions| Ok(TaskPerCopyScan { sessions }),
            |position, _| Ok(position),
            |wave| {
                let delivered = wave
                    .copies
                    .iter()
                    .map(|(copy, found)| (*copy, matches!(found, Ok(ScannedCopy::Scanned(())))))
                    .collect::<Vec<_>>();
                waves.push((wave.reads, delivered));
            },
        )
        .unwrap();

        let mut expected = (0..scanned)
            .map(|copy| (vec![copy], vec![(copy, true)]))
            .collect::<Vec<_>>();
        expected[0].1.push((replaced, false));
        for (_, delivered) in &mut waves {
            delivered.sort_unstable();
        }
        assert_eq!(
            waves, expected,
            "each wave runs one task and delivers the copy that task ends; the copy replaced \
             before the plan is delivered, read alone, with the first wave"
        );
    }

    #[test]
    fn scan_in_waves_runs_at_least_least_tasks_in_each_wave_but_last() {
        let base = tempfile::tempdir().unwrap();
        let copies = every_record_copies(base.path());
        let references = copies.iter().collect::<Vec<_>>();
        for least_tasks in [2, copies.len()] {
            let mut wave_tasks = Vec::new();

            scan_copies_in_waves(
                &references,
                Waves {
                    most: copies.len(),
                    least_tasks,
                },
                |_, sessions| Ok(TaskPerCopyScan { sessions }),
                |position, _| Ok(position),
                |wave| wave_tasks.push(wave.reads.len()),
            )
            .unwrap();

            let mut expected = vec![least_tasks; copies.len() / least_tasks];
            if !copies.len().is_multiple_of(least_tasks) {
                expected.push(copies.len() % least_tasks);
            }
            assert_eq!(
                wave_tasks,
                expected,
                "{} tasks in waves of at least {least_tasks}",
                copies.len()
            );
        }
    }

    /// The split of a table holding rows of `ids`.
    fn split_of_rows(ids: &[i64]) -> KeySplit {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch("CREATE TABLE texts (id INTEGER PRIMARY KEY)")
            .unwrap();
        let mut insert = connection
            .prepare("INSERT INTO texts (id) VALUES (?1)")
            .unwrap();
        for id in ids {
            insert.execute([id]).unwrap();
        }
        KeySplit::of(&connection, "texts", "id", RANGES_PER_THREAD).unwrap()
    }

    #[test]
    fn row_ranges_cover_each_row_once_with_gaps_in_ids_and_with_single_row_or_none() {
        let deleted_sessions =
            (1..=40_000).filter(|id| !(5_000..12_000).contains(id) && id % 7 != 0);
        for (case, ids) in [
            ("gaps", deleted_sessions.collect::<Vec<_>>()),
            ("single row", vec![42]),
            ("no row", Vec::new()),
        ] {
            let ranges = split_of_rows(&ids).ranges().collect::<Vec<_>>();
            if case == "gaps" {
                assert!(
                    ranges.len() > 1,
                    "{case}: the table splits into several ranges"
                );
            }
            for id in &ids {
                let covering = ranges.iter().filter(|range| range.contains(id)).count();
                assert_eq!(covering, 1, "{case}: one range covers row {id}: {ranges:?}");
            }
            if ids.is_empty() {
                assert!(
                    ranges.is_empty(),
                    "{case}: an empty table doesn't have a range"
                );
            }
        }
    }

    #[test]
    fn ranges_of_a_width_near_i64_max_end_at_the_greatest_key() {
        let width = i64::MAX - 1;
        let split = KeySplit::of_width(Some((i64::MIN + 1, i64::MAX)), width);

        let ranges = split.ranges().collect::<Vec<_>>();

        assert_eq!(
            ranges,
            vec![
                i64::MIN + 1..=-2,
                -1..=i64::MAX - 3,
                i64::MAX - 2..=i64::MAX,
            ],
            "the ranges cover the keys once each and stop at i64::MAX"
        );
    }

    #[test]
    fn split_bounds_read_each_scanned_tables_index_without_scanning_it() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(super::super::schema::CREATE)
            .unwrap();
        for (table, key) in [("parts", "id"), ("entries", "id"), ("messages", "id")] {
            let query = KeySplit::bounds_query(table, key);
            let plan = connection
                .prepare(&format!("EXPLAIN QUERY PLAN {query}"))
                .unwrap()
                .query_map([], |row| row.get::<_, String>(3))
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap()
                .join("\n");
            assert!(
                plan.contains(&format!("SEARCH {table}"))
                    && !plan.contains(&format!("SCAN {table}")),
                "{query}: the bounds come from the ends of an index, without a scan: {plan}"
            );
        }
    }
}
