//! Read-only access to an agent's SQLite database, for the providers whose
//! sessions or session list live in one: OpenCode's `opencode.db` and Codex's
//! `state_5.sqlite`.

use crate::cli::DebugLevel;
use crate::error::{AppError, Result};
use rusqlite::{Connection, ErrorCode, OpenFlags};
use std::path::Path;
use std::sync::Once;
use std::time::Duration;

/// A running agent holds the WAL writer; a short busy wait rides out its
/// checkpoints. 5000 ms is the wait OpenCode configures for itself.
pub(crate) const DEFAULT_BUSY_TIMEOUT: Duration = Duration::from_millis(5000);

/// Open an agent's session list read-only, waiting `busy_timeout` for a
/// lock. The failure is [`AppError::SessionListUnreadable`], so the list
/// reports why it holds none of the agent's sessions. A database that exists
/// but cannot be opened is an error, never "no session": a guard reading this
/// as absence could mistake a locked store for a deletable one.
pub(crate) fn open_session_list(database: &Path, busy_timeout: Duration) -> Result<Connection> {
    connect_read_only(database, busy_timeout)
        .map_err(|error| unusable_database(database, SESSION_DATABASE_CANNOT_BE_OPENED, &error))
}

fn connect_read_only(database: &Path, busy_timeout: Duration) -> rusqlite::Result<Connection> {
    let connection = Connection::open_with_flags(
        database,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    connection.busy_timeout(busy_timeout)?;
    Ok(connection)
}

/// The phrases the list shows for a present session list this reader could
/// not use, each followed by `: sessions not loaded`. Shared by every
/// provider whose session list is a database, so the list words one failure
/// one way.
pub(crate) const SESSION_DATABASE_LOCKED: &str = "session database locked";
pub(crate) const SESSION_DATABASE_CANNOT_BE_OPENED: &str = "session database cannot be opened";
pub(crate) const SESSION_DATABASE_CANNOT_BE_READ: &str = "session database cannot be read";

/// The failure to use a present session list, worded for the list: a lock
/// whichever step it surfaced at, else `stage`, the step that failed. A lock
/// is named apart because it most likely means the agent is writing, the one
/// reason a later launch clears on its own.
pub(crate) fn unusable_database(
    database: &Path,
    stage: &'static str,
    error: &rusqlite::Error,
) -> AppError {
    AppError::SessionListUnreadable {
        reason: if is_locked(error) {
            SESSION_DATABASE_LOCKED
        } else {
            stage
        },
        detail: format!("{}: {error}", database.display()),
    }
}

/// True when the failure is another connection's lock outlasting the busy
/// wait, the failure a running agent causes.
fn is_locked(error: &rusqlite::Error) -> bool {
    matches!(
        error.sqlite_error_code(),
        Some(ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked)
    )
}

/// A write statement that failed: the list still loads, so the failure
/// carries no reason worded for it.
pub(crate) fn database_error(database: &Path, error: &dyn std::fmt::Display) -> AppError {
    AppError::Io(std::io::Error::other(format!(
        "{}: {error}",
        database.display()
    )))
}

/// The newest schema migration a reader was developed against, beside the
/// journal the agent keeps of the migrations it has applied.
pub(crate) struct SchemaPin {
    /// Named in the warning, as the agent's journal names it.
    pub(crate) newest_verified: &'static (dyn std::fmt::Display + Sync),
    /// The newest migration applied beyond the pin, or `None` while the
    /// reader is current. A database without the journal table predates it —
    /// older than the pin, not newer — and any failure that matters
    /// resurfaces through the content queries, so an error reads as nothing
    /// to report.
    pub(crate) newest_unverified: fn(&Connection) -> Option<String>,
    /// One warning per process per reader.
    pub(crate) reported: Once,
}

impl SchemaPin {
    /// Warn once per process when `database` has applied a schema migration
    /// beyond the pin. Advisory only: additive migrations are the common
    /// case, and a column the reader misses already fails its query loudly.
    /// The warning explains a newer agent's sessions loading incompletely.
    pub(crate) fn warn_when_schema_outruns_reader(
        &self,
        database: &Path,
        debug_level: Option<DebugLevel>,
    ) {
        if debug_level.is_none() {
            return;
        }
        self.reported.call_once(|| {
            let Ok(connection) = connect_read_only(database, DEFAULT_BUSY_TIMEOUT) else {
                return;
            };
            if let Some(newest) = (self.newest_unverified)(&connection) {
                crate::debug::warn(
                    debug_level,
                    &format!(
                        "{}: schema migration {newest} is newer than the version this release \
                         was developed against ({}); sessions may load incompletely",
                        database.display(),
                        self.newest_verified
                    ),
                );
            }
        });
    }
}
