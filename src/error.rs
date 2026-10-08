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
    /// wherever was readable would differ between launches. `reason` is the
    /// phrase the list shows; `detail` names the file and the failure.
    #[error("{reason}: {detail}")]
    SessionListUnreadable {
        reason: &'static str,
        detail: String,
    },

    /// Resuming or forking a session in `agent`, its display name, failed
    /// to start the agent or ended with a failure status.
    #[error("Failed to run {agent}: {detail}")]
    AgentLaunch { agent: &'static str, detail: String },

    #[error("Configuration error: {0}")]
    ConfigError(String),

    #[error("{0}")]
    UnsupportedCapability(String),

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

    /// The refresh thread ended without sending its outcome. The status bar
    /// shows it after `Refresh failed: `.
    #[error("it stopped before finishing")]
    RefreshStopped,
}

pub type Result<T> = std::result::Result<T, AppError>;
