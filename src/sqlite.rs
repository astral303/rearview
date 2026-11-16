//! SQLite error classes the session cache and the passage store share.

/// True for SQLite's refusal while another connection holds a lock the
/// statement needs.
pub(crate) fn is_busy(error: &rusqlite::Error) -> bool {
    matches!(error, rusqlite::Error::SqliteFailure(error, _)
        if matches!(error.code, rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked))
}
