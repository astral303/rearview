//! Read-only access to an agent's SQLite database, for the providers whose
//! sessions or session list live in one: OpenCode's `opencode.db` and Codex's
//! `state_5.sqlite`.

use crate::cli::DebugLevel;
use crate::error::{AppError, Result};
use rusqlite::{Connection, OpenFlags};
use std::path::Path;
use std::sync::Once;
use std::time::Duration;

/// Open the database read-only. A database that exists but cannot be opened
/// is an error, never "no session": a guard reading this as absence could
/// mistake a locked store for a deletable one.
pub(crate) fn open_read_only(database: &Path) -> Result<Connection> {
    let connection = Connection::open_with_flags(
        database,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|error| database_error(database, &error))?;
    configure(database, &connection)?;
    Ok(connection)
}

/// A running agent holds the WAL writer; a short busy wait rides out its
/// checkpoints. 5000 ms is the wait OpenCode configures for itself.
pub(crate) fn configure(database: &Path, connection: &Connection) -> Result<()> {
    connection
        .busy_timeout(Duration::from_millis(5000))
        .map_err(|error| database_error(database, &error))
}

pub(crate) fn database_error(database: &Path, error: &dyn std::fmt::Display) -> AppError {
    AppError::Io(std::io::Error::other(format!(
        "{}: {error}",
        database.display()
    )))
}

/// The newest schema migration a reader was verified against, beside the
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
    /// this reader was never verified against — the one drift signal the
    /// agent records. Advisory only: additive migrations are the common case,
    /// and a column the reader misses already fails its query loudly. The
    /// warning explains a newer agent's sessions loading incompletely.
    pub(crate) fn warn_when_schema_outruns_reader(
        &self,
        database: &Path,
        debug_level: Option<DebugLevel>,
    ) {
        if debug_level.is_none() {
            return;
        }
        self.reported.call_once(|| {
            let Ok(connection) = open_read_only(database) else {
                return;
            };
            if let Some(newest) = (self.newest_unverified)(&connection) {
                crate::debug::warn(
                    debug_level,
                    &format!(
                        "{}: schema migration {newest} is newer than this reader was verified \
                         against ({}); sessions may load incompletely",
                        database.display(),
                        self.newest_verified
                    ),
                );
            }
        });
    }
}
