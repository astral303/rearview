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

    #[error("Session cache entries need refreshing for {session}: {reason}")]
    SessionCacheNotReady { session: String, reason: String },

    #[error("Session cache source changed or cannot be validated: {}", .0.display())]
    SessionCacheSourceChanged(std::path::PathBuf),

    /// A write to the session cache found less than the cache's floor free
    /// on its drive: the write stopped and the database was deleted.
    #[error(
        "not enough disk space for the session cache ({} GB free, needs at least {} GB). \
         Free some space and run rearview again.",
        gigabytes_down_to_a_tenth(*.free_bytes),
        .floor_bytes / 1_000_000_000
    )]
    SessionCacheOutOfSpace { free_bytes: u64, floor_bytes: u64 },
}

/// `bytes` in gigabytes, rounded down to one decimal place, so a drive
/// short of the floor never reads as holding it.
fn gigabytes_down_to_a_tenth(bytes: u64) -> String {
    let tenths = bytes / 100_000_000;
    format!("{}.{}", tenths / 10, tenths % 10)
}

impl AppError {
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
