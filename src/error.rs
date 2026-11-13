use crate::history::provider::{DenialDetail, Operation, refusal_message};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum AppError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON parsing error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("No conversation history found in {0}")]
    NoHistoryFound(String),

    /// No registered format reads the file, or not the format of the agent
    /// it was attributed to.
    #[error("Not a recognized session transcript: {0}")]
    UnrecognizedTranscript(String),

    #[error("User cancelled selection")]
    SelectionCancelled,

    #[error("Session not found: {0}")]
    SessionNotFound(String),

    /// Pi and OMP take a session's id from a record inside the log, and a
    /// branch copied within a project keeps the id it came from. Deleting on
    /// such an id would take a session the user did not name.
    #[error("{0}")]
    AmbiguousSessionId(String),

    /// An agent's own list of its sessions is present but could not be
    /// read, so none of that agent's sessions load this launch. Nothing is
    /// listed from another source in its place: a list that came from
    /// wherever was readable would differ between launches. `detail` names
    /// the file and the failure.
    #[error("{reason}: {detail}")]
    SessionListUnreadable {
        reason: SessionDatabaseFailure,
        detail: String,
    },

    /// Resuming or forking a session in `agent`, its display name, failed
    /// to start the agent or ended with a failure status.
    #[error("Failed to run {agent}: {detail}")]
    AgentLaunch { agent: &'static str, detail: String },

    #[error("Configuration error: {0}")]
    ConfigError(String),

    /// The user asked `agent`, its display name, for `operation`. Its
    /// provider declares that operation Not Capable, and nothing ran.
    #[error("{}", refusal_message(.agent, .operation, .detail))]
    NotCapable {
        agent: &'static str,
        operation: Operation,
        detail: Option<DenialDetail>,
    },

    /// A delete that removed nothing because another session may show turns
    /// from the one named.
    #[error("{0}")]
    DeleteRefused(String),

    /// `--show-dir`: no sessions directory of the agent holds a session for
    /// the directory or its repository.
    #[error("{agent} stores no sessions for {directory} or its repository")]
    AgentStoresNoSessions {
        agent: &'static str,
        directory: String,
    },

    /// `--show-all-dirs`: no agent's sessions directory holds a session for
    /// the directory or its repository.
    #[error("No agent stores sessions for {0} or its repository")]
    NoAgentStoresSessions(String),

    #[error("Update error: {0}")]
    UpdateError(String),

    #[error("Agent command error: {0}")]
    Agent(#[from] crate::agent::diagnostic::AgentError),

    #[error("{0}")]
    AgentProtocol(String),

    #[error("Invalid time range: {0}")]
    TimeFilter(#[from] crate::time_filter::TimeFilterError),

    #[error("Semantic search cancelled")]
    SemanticSearchCancelled,

    /// Embedding output that cannot be scored: the wrong number of
    /// embeddings, embeddings of differing or zero length, or a non-finite
    /// value. It is neither cached nor ranked.
    #[error("Invalid semantic embeddings: {0}")]
    InvalidEmbeddings(String),

    /// The refresh thread ended without sending its outcome. The status bar
    /// shows it after `Refresh failed: `.
    #[error("it stopped before finishing")]
    RefreshStopped,

    /// The session cache database could not be opened, read or written.
    #[error("Session cache database error: {0}")]
    SessionCache(#[from] rusqlite::Error),

    #[error("Session cache refresh was superseded: {0}")]
    SessionCacheSuperseded(i64),

    #[error("Session cache refresh ownership was lost")]
    SessionCacheRefreshLost,

    /// Under SQLite's exclusive lock, the database kept the journal mode
    /// `mode` when switched out of WAL mode.
    #[error("Session cache could not leave WAL mode: {mode}")]
    SessionCacheStuckInWal { mode: String },

    #[error("Session cache entries need refreshing for {session}: {reason}")]
    SessionCacheNotReady { session: String, reason: String },

    /// A file the session cache cannot store below its session's roots.
    #[error("Session {}: its file {} {reason}", .session.display(), .file.display())]
    SessionFileNotPlaced {
        session: std::path::PathBuf,
        file: std::path::PathBuf,
        reason: &'static str,
    },

    /// A session root whose directory could not be canonicalised.
    #[error(
        "Session {}: its root {} could not be canonicalised",
        .session.display(),
        .root.display()
    )]
    SessionRootNotCanonicalised {
        session: std::path::PathBuf,
        root: std::path::PathBuf,
    },

    #[error("Session cache source changed or cannot be validated: {}", .0.display())]
    SessionCacheSourceChanged(std::path::PathBuf),

    /// A stored file of a session listed under `listing_root` lies below
    /// `roots` row `root`, and none of this run's roots matches that row: a
    /// directory beside the root can canonicalise to another path now.
    #[error(
        "Session cache source changed: a session under {listing_root} stores its file {file} below `roots` row {root}, and none of this run's roots matches that row"
    )]
    StoredRootUnknown {
        listing_root: String,
        root: i64,
        file: String,
    },

    /// A write to the session cache found less than the cache's floor free
    /// on its drive: the write stopped, and the cache base's low-space latch
    /// is set. `outcome` stays pending until the command reporting the error
    /// runs the cache database's deletion.
    #[error(
        "{}{}",
        low_space_lead(*.free_bytes, *.floor_bytes),
        low_space_rest(.outcome)
    )]
    SessionCacheLowSpace {
        free_bytes: u64,
        floor_bytes: u64,
        outcome: CacheDeletion,
    },

    /// The cache's drive has `free_bytes` free after a low-space deletion,
    /// and writing the deleted cache again with the floor kept free needs at
    /// least `needed_bytes`. The command stopped before it parsed anything
    /// and didn't delete anything.
    #[error("{}", refill_low_space_message(*.free_bytes, *.needed_bytes))]
    SessionCacheRefillLowSpace { free_bytes: u64, needed_bytes: u64 },

    /// The session cache's low-space deletion refused a new connection of
    /// this rearview.
    #[error("The session cache is being deleted")]
    SessionCacheBeingDeleted,

    /// Neither `REARVIEW_CACHE_DIR` nor a home directory names where the
    /// session cache goes: nothing runs.
    #[error(
        "no directory for the session cache. Set REARVIEW_CACHE_DIR to a writable directory \
         and run rearview again."
    )]
    NoSessionCacheDirectory,
}

/// The session cache database's deletion after a write found its drive
/// low on space.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CacheDeletion {
    /// Not run yet.
    Pending,
    Deleted,
    /// Another rearview, or a connection of this one, kept the database open
    /// past the deletion's wait.
    Deferred,
    /// Emptying the database `file` or removing the side file `file` failed
    /// with `error`; a lost refresh ownership fails the emptying.
    Failed {
        file: String,
        error: String,
    },
}

impl CacheDeletion {
    /// The sentence the low-space message gives this deletion, with its
    /// leading space; empty while the deletion is pending.
    fn sentence(&self) -> String {
        match self {
            Self::Pending => String::new(),
            Self::Deleted => " The session cache was deleted.".to_owned(),
            Self::Deferred => {
                " The session cache was not deleted: another rearview has it open.".to_owned()
            }
            Self::Failed { file, error } => {
                format!(" The session cache could not be deleted: {file}: {error}.")
            }
        }
    }
}

/// The low-space message's first sentence.
pub(crate) fn low_space_lead(free_bytes: u64, floor_bytes: u64) -> String {
    format!(
        "not enough disk space for the session cache ({} GB free, needs at least {} GB).",
        gigabytes_down_to_a_tenth(free_bytes),
        floor_bytes / 1_000_000_000
    )
}

/// The low-space message after its first sentence: the deletion's
/// `outcome`, then the remedy.
pub(crate) fn low_space_rest(outcome: &CacheDeletion) -> String {
    format!(
        "{} Free some space and run rearview again.",
        outcome.sentence()
    )
}

/// The message of a refill refused for `free_bytes` free when it needs
/// `needed_bytes`. The free space rounds down and the need rounds up, so
/// freeing their difference as shown passes the check. The need is a lower
/// bound: the record is the size the deleted cache had reached, not the
/// size of a full cache.
fn refill_low_space_message(free_bytes: u64, needed_bytes: u64) -> String {
    let free_tenths = free_bytes / BYTES_PER_TENTH_OF_GIGABYTE;
    let needed_tenths = needed_bytes.div_ceil(BYTES_PER_TENTH_OF_GIGABYTE);
    format!(
        "not enough disk space to rebuild the session cache ({} GB free, needs at least {} GB). \
         Free at least {} GB and run rearview again.",
        gigabytes_of_tenths(free_tenths),
        gigabytes_of_tenths(needed_tenths),
        gigabytes_of_tenths(needed_tenths.saturating_sub(free_tenths))
    )
}

const BYTES_PER_TENTH_OF_GIGABYTE: u64 = 100_000_000;

/// `bytes` in gigabytes, rounded down to one decimal place, so the message
/// doesn't show a drive below the floor as holding it.
fn gigabytes_down_to_a_tenth(bytes: u64) -> String {
    gigabytes_of_tenths(bytes / BYTES_PER_TENTH_OF_GIGABYTE)
}

fn gigabytes_of_tenths(tenths: u64) -> String {
    format!("{}.{}", tenths / 10, tenths % 10)
}

impl AppError {
    /// True when the session cache's copy of a session no longer matches
    /// the session's files as this run reads them: a file changed, or a
    /// stored file lies below a `roots` row that none of this run's roots
    /// matches.
    pub(crate) fn is_changed_source(&self) -> bool {
        matches!(
            self,
            Self::SessionCacheSourceChanged(_) | Self::StoredRootUnknown { .. }
        )
    }

    pub fn home_not_found() -> Self {
        Self::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "Could not determine home directory",
        ))
    }
}

/// The reason an agent's session database could not be used, displayed as
/// the phrase the list shows before `: sessions not loaded`. Shared by every
/// provider whose session list is a database, so the list words one failure
/// one way.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionDatabaseFailure {
    /// Another connection's lock outlasted the busy wait. The agent is most
    /// likely writing, so a later read can succeed.
    Locked,
    CannotBeOpened,
    CannotBeRead,
    /// Codex's database describes another sessions tree: none of its rows
    /// names a session file under the one being read.
    NamesNoSessionFile,
}

impl std::fmt::Display for SessionDatabaseFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Locked => "session database locked",
            Self::CannotBeOpened => "session database cannot be opened",
            Self::CannotBeRead => "session database cannot be read",
            Self::NamesNoSessionFile => "session database names no session file",
        })
    }
}

pub type Result<T> = std::result::Result<T, AppError>;

#[cfg(test)]
mod tests {
    use super::*;

    fn low_space_message(outcome: CacheDeletion) -> String {
        AppError::SessionCacheLowSpace {
            free_bytes: 950_000_000,
            floor_bytes: 1_000_000_000,
            outcome,
        }
        .to_string()
    }

    #[test]
    fn low_space_message_reports_a_completed_deletion() {
        assert_eq!(
            low_space_message(CacheDeletion::Deleted),
            "not enough disk space for the session cache (0.9 GB free, needs at least 1 GB). \
             The session cache was deleted. Free some space and run rearview again."
        );
    }

    #[test]
    fn low_space_message_reports_a_deferred_deletion() {
        assert_eq!(
            low_space_message(CacheDeletion::Deferred),
            "not enough disk space for the session cache (0.9 GB free, needs at least 1 GB). \
             The session cache was not deleted: another rearview has it open. Free some space \
             and run rearview again."
        );
    }

    /// The space to free is the difference of the amounts the message
    /// shows, the free space rounded down and the need rounded up.
    #[test]
    fn refill_message_asks_to_free_difference_of_amounts_it_shows() {
        let message = AppError::SessionCacheRefillLowSpace {
            free_bytes: 2_150_000_000,
            needed_bytes: 3_410_000_000,
        }
        .to_string();

        assert_eq!(
            message,
            "not enough disk space to rebuild the session cache (2.1 GB free, needs at least \
             3.5 GB). Free at least 1.4 GB and run rearview again."
        );
    }

    #[test]
    fn low_space_message_reports_a_failed_deletion_with_its_file_and_cause() {
        assert_eq!(
            low_space_message(CacheDeletion::Failed {
                file: r"C:\cache\history.sqlite3".to_owned(),
                error: "Access is denied. (os error 5)".to_owned(),
            }),
            "not enough disk space for the session cache (0.9 GB free, needs at least 1 GB). \
             The session cache could not be deleted: C:\\cache\\history.sqlite3: Access is \
             denied. (os error 5). Free some space and run rearview again."
        );
    }
}
