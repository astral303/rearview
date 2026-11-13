//! The class of a session cache write error, and whether the next write of
//! any session can fail the same way.

use super::is_busy;
use crate::error::AppError;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WriteErrorClass {
    /// A write found less than the floor free on the cache's drive, or
    /// SQLite or the system reported the disk full. The write returns the
    /// low-space error once the base's low-space latch is set. Also an
    /// acquisition's refusal to write a deleted cache again
    /// ([`AppError::SessionCacheRefillLowSpace`]).
    LowSpace,
    /// Another rearview owns the cache, or this owner's write permission was
    /// revoked.
    LostOwnership,
    /// SQLite refused a lock the statement needed: another connection held
    /// it past the wait (`SQLITE_BUSY`), or a statement of the same
    /// connection or a connection sharing its cache held a table
    /// (`SQLITE_LOCKED`).
    Busy,
    /// A failure of the database, its file or a statement, not of the
    /// write's own rows: an I/O error, a read-only file, corruption,
    /// a refused commit, a database that is not admitted or being deleted,
    /// or a statement SQLite rejects whatever its values.
    Io,
    /// A failure of the write's own rows: a constraint failure, a value too
    /// long or of the wrong type for its column, a value rusqlite cannot
    /// convert, a stored copy the write no longer matches, or a session file
    /// or root the cache cannot place. No write raises an error from outside
    /// the session cache; each such error is classed `Other` too.
    Other,
}

impl WriteErrorClass {
    pub(crate) fn of(error: &AppError) -> Self {
        match error {
            AppError::SessionCacheLowSpace { .. } | AppError::SessionCacheRefillLowSpace { .. } => {
                Self::LowSpace
            }
            AppError::SessionCacheRefreshLost => Self::LostOwnership,
            AppError::SessionCache(error) => Self::of_sqlite(error),
            AppError::Io(error) if error.kind() == std::io::ErrorKind::StorageFull => {
                Self::LowSpace
            }
            AppError::Io(_)
            | AppError::SessionCacheBeingDeleted
            | AppError::SessionCacheStuckInWal { .. }
            | AppError::SessionCacheNotReady { .. }
            | AppError::NoSessionCacheDirectory => Self::Io,
            AppError::SessionCacheSuperseded(_)
            | AppError::SessionFileNotPlaced { .. }
            | AppError::SessionRootNotCanonicalised { .. }
            | AppError::SessionCacheSourceChanged(_)
            | AppError::StoredRootUnknown { .. }
            | AppError::Json(_) => Self::Other,
            AppError::NoHistoryFound(_)
            | AppError::UnrecognizedTranscript(_)
            | AppError::SelectionCancelled
            | AppError::SessionNotFound(_)
            | AppError::AmbiguousSessionId(_)
            | AppError::SessionListUnreadable { .. }
            | AppError::AgentLaunch { .. }
            | AppError::ConfigError(_)
            | AppError::NotCapable { .. }
            | AppError::DeleteRefused(_)
            | AppError::AgentStoresNoSessions { .. }
            | AppError::NoAgentStoresSessions(_)
            | AppError::UpdateError(_)
            | AppError::Agent(_)
            | AppError::AgentProtocol(_)
            | AppError::TimeFilter(_)
            | AppError::SemanticSearchCancelled
            | AppError::InvalidEmbeddings(_)
            | AppError::RefreshStopped => Self::Other,
        }
    }

    pub(super) fn of_sqlite(error: &rusqlite::Error) -> Self {
        if is_busy(error) {
            return Self::Busy;
        }
        let Some(failure) = error.sqlite_error() else {
            return Self::Other;
        };
        match failure.code {
            // A commit hook refuses the whole commit, not a row.
            rusqlite::ErrorCode::ConstraintViolation => {
                if failure.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_COMMITHOOK {
                    Self::Io
                } else {
                    Self::Other
                }
            }
            rusqlite::ErrorCode::TooBig | rusqlite::ErrorCode::TypeMismatch => Self::Other,
            rusqlite::ErrorCode::DiskFull => Self::LowSpace,
            _ => Self::Io,
        }
    }

    /// True for every class but `Other`: the next write of any session can
    /// fail the same way.
    pub(crate) fn stops_writes(self) -> bool {
        self != Self::Other
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::CacheDeletion;
    use rusqlite::Connection;

    fn sqlite_class(error: rusqlite::Error) -> WriteErrorClass {
        WriteErrorClass::of(&AppError::SessionCache(error))
    }

    fn database_with_table(directory: &std::path::Path) -> std::path::PathBuf {
        let path = directory.join("classified.sqlite3");
        Connection::open(&path)
            .unwrap()
            .execute_batch("CREATE TABLE rows (id INTEGER PRIMARY KEY)")
            .unwrap();
        path
    }

    fn connection_with_table() -> Connection {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch("CREATE TABLE rows (id INTEGER PRIMARY KEY)")
            .unwrap();
        connection
    }

    #[test]
    fn writer_lock_held_by_another_connection_is_busy() {
        let directory = tempfile::tempdir().unwrap();
        let path = database_with_table(directory.path());
        let holder = Connection::open(&path).unwrap();
        holder.execute_batch("BEGIN IMMEDIATE").unwrap();
        let waiter = Connection::open(&path).unwrap();
        waiter.busy_timeout(std::time::Duration::ZERO).unwrap();

        let error = waiter.execute_batch("BEGIN IMMEDIATE").unwrap_err();

        assert_eq!(
            sqlite_class(error),
            WriteErrorClass::Busy,
            "a lock another connection holds is busy"
        );
    }

    #[test]
    fn write_to_read_only_database_is_io() {
        let directory = tempfile::tempdir().unwrap();
        let path = database_with_table(directory.path());
        let reader =
            Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();

        let error = reader
            .execute("INSERT INTO rows (id) VALUES (1)", [])
            .unwrap_err();

        assert_eq!(
            sqlite_class(error),
            WriteErrorClass::Io,
            "a write to a read-only database is an I/O failure"
        );
    }

    #[test]
    fn write_past_database_size_limit_is_low_space() {
        let connection = connection_with_table();
        let pages: i64 = connection
            .query_row("PRAGMA page_count", [], |row| row.get(0))
            .unwrap();
        connection
            .query_row(&format!("PRAGMA max_page_count = {pages}"), [], |_| Ok(()))
            .unwrap();

        let error = connection
            .execute(
                "CREATE TABLE texts AS SELECT randomblob(100000) AS text",
                [],
            )
            .unwrap_err();

        assert_eq!(
            error.sqlite_error_code(),
            Some(rusqlite::ErrorCode::DiskFull),
            "the database at its size limit reports a full disk: {error}"
        );
        assert_eq!(
            sqlite_class(error),
            WriteErrorClass::LowSpace,
            "a full database is classed low space"
        );
    }

    #[test]
    fn file_that_isnt_database_is_io() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("not-a-database.sqlite3");
        std::fs::write(&path, [0x5a_u8; 4096]).unwrap();
        let connection = Connection::open(&path).unwrap();

        let error = connection
            .query_row("SELECT count(*) FROM sqlite_master", [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap_err();

        assert_eq!(
            sqlite_class(error),
            WriteErrorClass::Io,
            "a file that is not a database is an I/O failure"
        );
    }

    #[test]
    fn refused_commit_is_io() {
        let mut connection = connection_with_table();
        connection.commit_hook(Some(|| true)).unwrap();

        let error = connection
            .execute_batch("BEGIN; INSERT INTO rows (id) VALUES (1); COMMIT")
            .unwrap_err();

        assert_eq!(
            sqlite_class(error),
            WriteErrorClass::Io,
            "a commit the commit hook refuses is an I/O failure"
        );
    }

    #[test]
    fn commit_leaving_row_without_its_reference_is_other() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "PRAGMA foreign_keys = ON;
                 CREATE TABLE parents (id INTEGER PRIMARY KEY);
                 CREATE TABLE children (parent INTEGER REFERENCES parents (id)
                     DEFERRABLE INITIALLY DEFERRED);",
            )
            .unwrap();

        let error = connection
            .execute_batch("BEGIN; INSERT INTO children (parent) VALUES (1); COMMIT")
            .unwrap_err();

        assert_eq!(
            sqlite_class(error),
            WriteErrorClass::Other,
            "a foreign-key failure at the commit is classed Other: the write left a row \
             without its reference"
        );
    }

    #[test]
    fn statement_a_trigger_aborts_is_other() {
        let connection = connection_with_table();
        connection
            .execute_batch(
                "CREATE TRIGGER refuse BEFORE INSERT ON rows
                 BEGIN SELECT RAISE(ABORT, 'refused'); END;",
            )
            .unwrap();

        let error = connection
            .execute("INSERT INTO rows (id) VALUES (1)", [])
            .unwrap_err();

        assert_eq!(
            sqlite_class(error),
            WriteErrorClass::Other,
            "a statement a trigger aborts for its rows is classed Other"
        );
    }

    #[test]
    fn duplicate_key_is_other() {
        let connection = connection_with_table();
        connection
            .execute("INSERT INTO rows VALUES (1)", [])
            .unwrap();

        let error = connection
            .execute("INSERT INTO rows (id) VALUES (1)", [])
            .unwrap_err();

        assert_eq!(
            sqlite_class(error),
            WriteErrorClass::Other,
            "a unique constraint failure is classed Other: another row holds the key"
        );
    }

    #[test]
    fn value_over_length_limit_is_other() {
        let connection = connection_with_table();

        let error = connection
            .query_row("SELECT zeroblob(2000000000)", [], |_| Ok(()))
            .unwrap_err();

        assert_eq!(
            error.sqlite_error_code(),
            Some(rusqlite::ErrorCode::TooBig),
            "a blob past SQLite's length limit is too big: {error}"
        );
        assert_eq!(
            sqlite_class(error),
            WriteErrorClass::Other,
            "a value over the length limit is classed Other"
        );
    }

    #[test]
    fn value_of_wrong_type_for_its_column_is_other() {
        let connection = connection_with_table();

        let error = connection
            .execute("INSERT INTO rows (id) VALUES ('text')", [])
            .unwrap_err();

        assert_eq!(
            error.sqlite_error_code(),
            Some(rusqlite::ErrorCode::TypeMismatch),
            "text as an integer key is a type mismatch: {error}"
        );
        assert_eq!(
            sqlite_class(error),
            WriteErrorClass::Other,
            "a value of the wrong type for its column is classed Other"
        );
    }

    #[test]
    fn rusqlite_error_without_sqlite_failure_is_other() {
        let connection = connection_with_table();

        let error = connection
            .query_row("SELECT id FROM rows", [], |row| row.get::<_, i64>(0))
            .unwrap_err();

        assert!(
            matches!(error, rusqlite::Error::QueryReturnedNoRows),
            "a row read from an empty table is missing: {error}"
        );
        assert_eq!(
            sqlite_class(error),
            WriteErrorClass::Other,
            "an error rusqlite raises itself is classed Other"
        );
    }

    #[test]
    fn crate_errors_are_classed() {
        let classes = [
            (
                AppError::SessionCacheLowSpace {
                    free_bytes: 0,
                    floor_bytes: 1,
                    outcome: CacheDeletion::Pending,
                },
                WriteErrorClass::LowSpace,
            ),
            (
                AppError::SessionCacheRefreshLost,
                WriteErrorClass::LostOwnership,
            ),
            (
                AppError::Io(std::io::Error::other("disk failed")),
                WriteErrorClass::Io,
            ),
            (
                AppError::Io(std::io::Error::from(std::io::ErrorKind::StorageFull)),
                WriteErrorClass::LowSpace,
            ),
            (
                AppError::Io(std::io::Error::from(std::io::ErrorKind::QuotaExceeded)),
                WriteErrorClass::Io,
            ),
            (AppError::SessionCacheBeingDeleted, WriteErrorClass::Io),
            (
                AppError::SessionCacheStuckInWal {
                    mode: "wal".to_owned(),
                },
                WriteErrorClass::Io,
            ),
            (
                AppError::SessionCacheNotReady {
                    session: "session".to_owned(),
                    reason: "the database is not admitted".to_owned(),
                },
                WriteErrorClass::Io,
            ),
            (AppError::SessionCacheSuperseded(1), WriteErrorClass::Other),
            (
                AppError::SessionFileNotPlaced {
                    session: "session.jsonl".into(),
                    file: "elsewhere/file.jsonl".into(),
                    reason: "lies below none of its roots",
                },
                WriteErrorClass::Other,
            ),
            (
                AppError::SessionNotFound("session".to_owned()),
                WriteErrorClass::Other,
            ),
        ];
        for (error, class) in classes {
            assert_eq!(
                WriteErrorClass::of(&error),
                class,
                "{error:?} is classed {class:?}"
            );
        }
    }

    #[test]
    fn only_failure_of_writes_own_rows_lets_later_writes_run() {
        let classes = [
            WriteErrorClass::LowSpace,
            WriteErrorClass::LostOwnership,
            WriteErrorClass::Busy,
            WriteErrorClass::Io,
            WriteErrorClass::Other,
        ];
        for class in classes {
            // A new class doesn't build until this match names it; add it to
            // `classes` above as well.
            let stops = match class {
                WriteErrorClass::LowSpace
                | WriteErrorClass::LostOwnership
                | WriteErrorClass::Busy
                | WriteErrorClass::Io => true,
                WriteErrorClass::Other => false,
            };
            assert_eq!(
                class.stops_writes(),
                stops,
                "{class:?} stops later writes: {stops}"
            );
        }
    }
}
