//! A read-only look at who owns a session cache's refresh. It never takes
//! SQLite's writer lock and never writes, so the TUI's drawing thread may
//! call it.

use super::base::CacheBase;
use super::connection::{self, Access};
use super::is_busy;
use super::refresh::RefreshOwner;
use crate::error::{AppError, Result};
use rusqlite::OptionalExtension;
use std::time::Duration;

/// One look at a cache's refresh state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Observation {
    /// No refresh owns the cache.
    Idle,
    /// A refresh owns the cache. An old `last_commit_ms` alone does not mean
    /// it completed: a long batch holds the writer lock past the takeover
    /// window.
    Running {
        owner: RefreshOwner,
        last_commit_ms: i64,
    },
    /// The database is exclusively locked, as during a rebuild, so its owner
    /// cannot be read.
    Unknown,
}

/// Another rearview's work on the session cache, as a waiter names it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OtherRearview {
    /// Its refresh owns the cache.
    Refreshing,
    /// The database is exclusively locked, as during its rebuild.
    Rebuilding,
}

/// The refresh a reader waits for.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AwaitedRefresh {
    ThisProcess,
    AnotherRearview(OtherRearview),
}

impl Observation {
    /// The refresh a reader waits for now; `None` while no refresh owns the
    /// cache.
    pub fn awaited(self) -> Option<AwaitedRefresh> {
        match self {
            Self::Idle => None,
            Self::Running {
                owner: RefreshOwner::ThisProcess,
                ..
            } => Some(AwaitedRefresh::ThisProcess),
            Self::Running {
                owner: RefreshOwner::AnotherProcess,
                ..
            } => Some(AwaitedRefresh::AnotherRearview(OtherRearview::Refreshing)),
            Self::Unknown => Some(AwaitedRefresh::AnotherRearview(OtherRearview::Rebuilding)),
        }
    }
}

/// The refresh state of `base`'s cache, read through a read-only connection
/// without waiting. A stored owner identity this rearview holds is this
/// process's refresh; any other is another process's.
pub(crate) fn observe(base: &CacheBase) -> Result<Observation> {
    let path = super::database_path(base.path());
    if !path.is_file() {
        return Ok(Observation::Idle);
    }
    let observed = (|| {
        let connection = connection::open(&path, Access::ReadOnly)?;
        connection.busy_timeout(Duration::ZERO)?;
        if !super::refresh::has_refresh_state(&connection)? {
            return Ok(Observation::Idle);
        }
        let stored: Option<(Option<String>, i64)> = connection
            .query_row(
                "SELECT owner, last_commit_ms FROM refresh_state WHERE singleton = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        Ok(match stored {
            Some((Some(identity), last_commit_ms)) => Observation::Running {
                owner: RefreshOwner::of(base, &identity),
                last_commit_ms,
            },
            Some((None, _)) | None => Observation::Idle,
        })
    })();
    match observed {
        Err(AppError::SessionCache(error)) if is_busy(&error) => Ok(Observation::Unknown),
        observed => observed,
    }
}
