//! The record a low-space deletion leaves: the size the session cache had
//! reached when the deletion emptied it. An acquisition reads it and
//! refuses to write the cache again while its drive doesn't have room for
//! that size and the floor, before the operation parses anything. A full
//! load that ends as the live owner, without low space, clears it.

use super::base::CacheBase;
use super::connection::{self, Access};
use super::coordinator::WaitStatus;
use super::observer::REBUILD_LOCK_THRESHOLD;
use super::refresh::FREE_SPACE_FLOOR;
use super::scope::{Cancellation, read_when_unlocked};
use crate::error::{AppError, Result};
use rusqlite::{Connection, OptionalExtension};
use std::path::Path;

/// The record's table. It is not one of `schema::TABLES`, which the
/// deletion and a layout rebuild drop: the record outlives both.
const CREATE: &str = "CREATE TABLE IF NOT EXISTS low_space_deletion (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    cache_bytes INTEGER NOT NULL
)";

/// A low-space deletion's record in the cache database, as [`check`] found
/// it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum DeletionRecord {
    Absent,
    Present,
}

/// The bytes of the cache database at `database` and its write-ahead log.
pub(super) fn cache_bytes(database: &Path) -> u64 {
    let mut log = database.to_path_buf().into_os_string();
    log.push("-wal");
    [database.as_os_str(), &log]
        .into_iter()
        .map(|path| std::fs::metadata(path).map_or(0, |metadata| metadata.len()))
        .sum()
}

/// Record on `connection`, in the deletion's batch, that the deletion
/// emptied a cache of `cache_bytes`. Keep the larger of the stored and the
/// new size, because a later deletion can find a smaller cache.
pub(super) fn record(connection: &Connection, cache_bytes: u64) -> rusqlite::Result<()> {
    connection.execute_batch(CREATE)?;
    connection.execute(
        "INSERT INTO low_space_deletion (singleton, cache_bytes) VALUES (1, ?1) \
         ON CONFLICT (singleton) DO UPDATE SET \
         cache_bytes = max(cache_bytes, excluded.cache_bytes)",
        [cache_bytes as i64],
    )?;
    Ok(())
}

/// Clear the record on `connection`, the database it was found in.
pub(super) fn clear(connection: &Connection) -> rusqlite::Result<()> {
    connection.execute("DELETE FROM low_space_deletion", [])?;
    Ok(())
}

/// Refuse to write `base`'s deleted cache again while its drive has less
/// free than the record's size, less the cache's current size, plus
/// [`FREE_SPACE_FLOOR`]: set the base's low-space latch to the refusal and
/// fail with its error. Waits, publishing on `status`, while another
/// rearview holds the database exclusively locked. A drive that doesn't
/// report its free space passes. Returns whether the record is there, for
/// a full load to clear.
///
/// The cache's current size is subtracted from the record's, so the answer
/// stays the same while another rearview fills the cache and after a fill
/// the user stopped. A cache at or past the record's size is left to the
/// floor check.
pub(super) fn check(
    base: &CacheBase,
    status: &WaitStatus,
    cancellation: &Cancellation,
) -> Result<DeletionRecord> {
    let recorded = read_when_unlocked(base.clock().as_ref(), status, cancellation, || {
        recorded_bytes(base)
    })?;
    let Some(recorded) = recorded else {
        return Ok(DeletionRecord::Absent);
    };
    let current = cache_bytes(&super::database_path(base.path()));
    if recorded <= current {
        return Ok(DeletionRecord::Present);
    }
    let needed_bytes = recorded - current + FREE_SPACE_FLOOR;
    let Some(check) = base
        .low_space()
        .begin_check(|| !cancellation.is_cancelled())
    else {
        return Err(AppError::SessionCacheRefreshLost);
    };
    let Some(free_bytes) = base
        .disk()
        .available_bytes(base.path())
        .filter(|&free| free < needed_bytes)
    else {
        return Ok(DeletionRecord::Present);
    };
    if !check.publish_refill_refusal(free_bytes, needed_bytes) {
        return Err(AppError::SessionCacheRefreshLost);
    }
    Err(base
        .low_space()
        .error()
        .expect("the low-space latch was just set"))
}

/// The cache size in the record in `base`'s database; `None` without a
/// record.
fn recorded_bytes(base: &CacheBase) -> Result<Option<u64>> {
    if !super::database_path(base.path()).is_file() {
        return Ok(None);
    }
    let connection = connection::open_cache(base, Access::ReadOnly, REBUILD_LOCK_THRESHOLD)?;
    Ok(recorded_in(&connection)?)
}

/// The cache size in the record in the database `connection` reads; `None`
/// without a record.
fn recorded_in(connection: &Connection) -> rusqlite::Result<Option<u64>> {
    let has_record_table: bool = connection.query_row(
        "SELECT EXISTS (SELECT 1 FROM sqlite_master \
         WHERE type = 'table' AND name = 'low_space_deletion')",
        [],
        |row| row.get(0),
    )?;
    if !has_record_table {
        return Ok(None);
    }
    let recorded: Option<i64> = connection
        .query_row(
            "SELECT cache_bytes FROM low_space_deletion WHERE singleton = 1",
            [],
            |row| row.get(0),
        )
        .optional()?;
    Ok(recorded.map(|bytes| bytes as u64))
}

/// The cache size in the record in the database under the cache base
/// directory `base`; `None` without a record. Reads without writing, so the
/// cache's files keep their sizes.
#[cfg(test)]
pub(crate) fn recorded_cache_bytes(base: &Path) -> Option<u64> {
    let connection = Connection::open_with_flags(
        super::database_path(base),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    recorded_in(&connection).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::TestClock;
    use crate::history::ProcessIdentity;
    use crate::history::provider::walk::SystemCanonicaliser;
    use std::sync::Arc;

    /// A cache base in `directory`, whose database has a record of the
    /// database's own size plus `bytes_past_cache`, on a drive with
    /// `free_bytes` free.
    fn base_with_record(directory: &Path, bytes_past_cache: u64, free_bytes: u64) -> CacheBase {
        let database = super::super::database_path(directory);
        {
            let connection = Connection::open(&database).unwrap();
            record(&connection, 0).unwrap();
        }
        let current = cache_bytes(&database);
        Connection::open(&database)
            .unwrap()
            .execute(
                "UPDATE low_space_deletion SET cache_bytes = ?1",
                [(current + bytes_past_cache) as i64],
            )
            .unwrap();
        assert_eq!(
            cache_bytes(&database),
            current,
            "updating the record doesn't change the cache's size"
        );
        CacheBase::new(
            directory,
            ProcessIdentity::default(),
            Arc::new(TestClock::default()),
            SystemCanonicaliser::shared(),
        )
        .with_free_space(free_bytes)
    }

    fn check_of(base: &CacheBase) -> Result<DeletionRecord> {
        check(base, &WaitStatus::default(), &Cancellation::default())
    }

    /// A cache that reached the record's size passes, even on a drive below
    /// the floor: the floor check after this one decides.
    #[test]
    fn cache_at_recorded_size_passes_below_floor() {
        let directory = tempfile::tempdir().unwrap();
        let base = base_with_record(directory.path(), 0, FREE_SPACE_FLOOR - 1);

        let checked = check_of(&base);

        assert!(
            matches!(checked, Ok(DeletionRecord::Present)),
            "the check passes and finds the record: {checked:?}"
        );
        assert!(!base.low_space().is_set(), "the latch stays clear");
    }

    /// A cache one byte short of the record's size needs that byte and the
    /// floor: the check refuses a drive with the floor free, and passes one
    /// with a byte more.
    #[test]
    fn cache_short_of_recorded_size_needs_rest_of_record_and_floor() {
        let refused_directory = tempfile::tempdir().unwrap();
        let refused = base_with_record(refused_directory.path(), 1, FREE_SPACE_FLOOR);
        let passed_directory = tempfile::tempdir().unwrap();
        let passed = base_with_record(passed_directory.path(), 1, FREE_SPACE_FLOOR + 1);

        let refusal = check_of(&refused);
        let pass = check_of(&passed);

        assert!(
            matches!(
                refusal,
                Err(AppError::SessionCacheRefillLowSpace {
                    free_bytes: FREE_SPACE_FLOOR,
                    needed_bytes,
                }) if needed_bytes == FREE_SPACE_FLOOR + 1
            ),
            "the check refuses the drive with the floor free: {refusal:?}"
        );
        assert!(
            matches!(pass, Ok(DeletionRecord::Present)),
            "the check passes the drive with a byte more: {pass:?}"
        );
    }
}
