//! The read of many sessions' copies in tasks across threads, each rayon
//! work split on a read connection of its own, for the lexical scan
//! ([`super::text_scan`]) and the semantic one ([`super::semantic_scan`]):
//! [`scan_copies`] runs the tasks and rechecks the sessions read.

use super::{CacheBase, CachedSession, SqliteCache};
use crate::error::{AppError, Result};
use rayon::prelude::*;
use rusqlite::Connection;
use std::collections::HashMap;
use std::ops::RangeInclusive;

/// Row ranges per thread that a scan of each large table is split into.
/// Many small ranges keep every thread busy until the scan ends, as rows
/// differ in size by orders of magnitude.
const RANGES_PER_THREAD: i64 = 16;
/// The fewest rows a range holds, so a small table is one range.
const MIN_ROWS_PER_RANGE: i64 = 4096;

/// A scan's kept reads, at most one per task, and what it found of each
/// copy.
pub(crate) struct ScannedCopies<R> {
    pub(crate) reads: Vec<R>,
    /// One outcome per scanned copy, by the copy's position.
    pub(crate) copies: Vec<CopyOutcome>,
}

/// A scanned copy's outcome.
#[derive(Debug)]
pub(crate) enum CopyOutcome {
    /// The reads hold the copy's content, read from the session row of this
    /// id; they don't hold any content of a copy without tasks.
    Scanned(i64),
    NoLongerCached,
    /// Another write replaced the copy before the scan ended; the cache now
    /// holds the copy of this generation. The reads may hold content of
    /// either copy: callers drop it.
    Replaced(i64),
    /// The copy's lookup failed, as when a write holds its session, or one of
    /// its tasks failed. The reads may hold part of its content: callers drop
    /// it.
    Failed(AppError),
}

impl CopyOutcome {
    /// The outcome of `copy`'s lookup in `connection`'s snapshot.
    pub(super) fn of_lookup(connection: &Connection, copy: &CachedSession) -> Self {
        match copy.listed_in(connection) {
            Ok(Some(stored)) if stored.generation == copy.generation => Self::Scanned(stored.id),
            Ok(Some(stored)) => Self::Replaced(stored.generation),
            Ok(None) => Self::NoLongerCached,
            Err(error) => Self::Failed(error),
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
/// - A failed lookup of a copy, or a failed task of one copy, fails that
///   copy alone.
/// - A task finding the database gone gets `NoLongerCached` for its copy, or
///   for every copy of the base when the task reads several sessions.
/// - Fails when a cache base cannot be opened or read, or a task of several
///   sessions fails.
/// - Once every task ends, each scanned session is looked up again: a copy
///   replaced by another write during the scan gets `Replaced`, and a failed
///   lookup gets `Failed`.
pub(super) fn scan_copies<S: CopiesScan>(
    copies: &[&CachedSession],
    plan: impl Fn(&Connection, SessionsById) -> Result<S>,
) -> Result<ScannedCopies<S::Read>> {
    let mut by_base: HashMap<&std::path::Path, Vec<usize>> = HashMap::new();
    for (position, copy) in copies.iter().enumerate() {
        by_base.entry(copy.base.path()).or_default().push(position);
    }
    let mut outcomes = copies.iter().map(|_| None).collect::<Vec<_>>();
    let mut reads = Vec::new();
    for positions in by_base.into_values() {
        let base = &copies[positions[0]].base;
        let Some(cache) = SqliteCache::open_waiting(base)? else {
            for position in positions {
                outcomes[position] = Some(CopyOutcome::NoLongerCached);
            }
            continue;
        };
        let planned = cache.with_read_snapshot(|connection| {
            let mut sessions = SessionsById::new();
            for &position in &positions {
                let copy = copies[position];
                let outcome = CopyOutcome::of_lookup(connection, copy);
                if let CopyOutcome::Scanned(session) = outcome {
                    let earlier = sessions.insert(session, (position, copy.generation));
                    debug_assert!(earlier.is_none(), "a scan holds one copy per session");
                }
                outcomes[position] = Some(outcome);
            }
            if sessions.is_empty() {
                return Ok(None);
            }
            let scan = plan(connection, sessions)?;
            let tasks = scan.tasks(connection)?;
            Ok(Some((scan, tasks)))
        })?;
        let Some((scan, tasks)) = planned else {
            continue;
        };
        let task_ends = tasks
            .into_par_iter()
            .map_init(
                || None,
                |reader: &mut Option<SqliteCache>, task| {
                    let copy = scan.copy_of(&task);
                    let end = match open_once(reader, base) {
                        Ok(Some(cache)) => match cache
                            .with_read_snapshot(|connection| scan.read(connection, task))
                        {
                            Ok(read) => TaskEnd::Succeeded(scan.task_ended(read)),
                            Err(error) => TaskEnd::Failed(error),
                        },
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
        for (copy, end) in task_ends {
            match (copy, end) {
                (_, TaskEnd::Succeeded(read)) => reads.extend(read),
                (Some(copy), TaskEnd::DatabaseGone) => {
                    outcomes[copy] = Some(CopyOutcome::NoLongerCached);
                }
                (None, TaskEnd::DatabaseGone) => {
                    for &position in &positions {
                        outcomes[position] = Some(CopyOutcome::NoLongerCached);
                    }
                }
                (Some(copy), TaskEnd::Failed(error)) => {
                    outcomes[copy] = Some(CopyOutcome::Failed(error));
                }
                (None, TaskEnd::Failed(error)) => return Err(error),
            }
        }
        cache.with_read_snapshot(|connection| {
            for &position in &positions {
                if matches!(outcomes[position], Some(CopyOutcome::Scanned(_))) {
                    outcomes[position] = Some(CopyOutcome::of_lookup(connection, copies[position]));
                }
            }
            Ok(())
        })?;
    }
    Ok(ScannedCopies {
        reads,
        copies: outcomes
            .into_iter()
            .map(|outcome| outcome.expect("every copy's cache base is scanned"))
            .collect(),
    })
}

/// The read connection of this rayon work split, opened on its first task;
/// `None` when `base`'s database is gone.
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
    /// The split of `table`'s `key`, the first column of an index. An empty
    /// table has no range.
    pub(super) fn of(connection: &Connection, table: &str, key: &str) -> Result<Self> {
        let bounds = connection.query_row(&Self::bounds_query(table, key), [], |row| {
            Ok(row.get::<_, Option<i64>>(0)?.zip(row.get(1)?))
        })?;
        Ok(Self::spanning(bounds))
    }

    /// The split of the keys from the least to the greatest of `bounds`; no
    /// range without bounds.
    pub(super) fn spanning(bounds: Option<(i64, i64)>) -> Self {
        let span = bounds.map_or(0, |(least, greatest)| {
            (greatest - least).saturating_add(1).max(0)
        });
        let threads = i64::try_from(rayon::current_num_threads()).unwrap_or(1);
        let count = (span / MIN_ROWS_PER_RANGE)
            .clamp(1, threads * RANGES_PER_THREAD)
            .min(span.max(1));
        Self::of_width(bounds, (span + count - 1) / count)
    }

    /// The split of the keys from the least to the greatest of `bounds`
    /// into ranges of at most `width` keys; no range without bounds.
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
    fn tasks_finding_the_database_gone_leave_their_copies_no_longer_cached() {
        for task_per_copy in [true, false] {
            let base = tempfile::tempdir().unwrap();
            let copies = every_record_copies(base.path());
            let references = copies.iter().collect::<Vec<_>>();
            let failed = std::sync::Arc::default();

            let scanned = scan_copies(&references, |_, sessions| {
                Ok(DatabaseGoneScan {
                    database: super::super::database_path(copies[0].base.path()),
                    sessions,
                    task_per_copy,
                    failed: std::sync::Arc::clone(&failed),
                })
            })
            .unwrap();

            assert!(
                scanned
                    .copies
                    .iter()
                    .all(|outcome| matches!(outcome, CopyOutcome::NoLongerCached)),
                "task per copy {task_per_copy}: every copy is no longer in the session cache: \
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
                scan_copies(&references, |_, sessions| {
                    Ok(FirstTaskFailingScan {
                        sessions,
                        events: std::sync::Arc::clone(&events),
                    })
                })
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
            matches!(scanned.copies[0], CopyOutcome::Failed(_)),
            "the copy fails: {:?}",
            scanned.copies[0]
        );
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
        KeySplit::of(&connection, "texts", "id").unwrap()
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
                assert!(ranges.is_empty(), "{case}: an empty table has no range");
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
