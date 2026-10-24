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
        commit_age: CommitAge,
    },
    /// The database is exclusively locked by another rearview, as during
    /// its rebuild, so its owner cannot be read.
    Unknown,
    /// This rearview's own rebuild holds the database's exclusive lock.
    RebuildingHere,
}

/// The age of a refresh's last commit against the takeover window, by the
/// observing rearview's wall clock.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CommitAge {
    Recent,
    /// Older than the takeover window: another rearview may take the
    /// refresh over, unless a batch still holds the writer lock.
    PastTakeover,
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
    /// cache, and while another rearview's last commit is past the takeover
    /// window: that rearview may have died, and only an acquisition, under
    /// the writer lock, can tell.
    pub fn awaited(self) -> Option<AwaitedRefresh> {
        match self {
            Self::Idle
            | Self::Running {
                owner: RefreshOwner::AnotherProcess,
                commit_age: CommitAge::PastTakeover,
                ..
            } => None,
            Self::Running {
                owner: RefreshOwner::ThisProcess,
                ..
            }
            | Self::RebuildingHere => Some(AwaitedRefresh::ThisProcess),
            Self::Running {
                owner: RefreshOwner::AnotherProcess,
                commit_age: CommitAge::Recent,
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
                commit_age: super::refresh::commit_age(base, last_commit_ms),
            },
            Some((None, _)) | None => Observation::Idle,
        })
    })();
    match observed {
        Err(AppError::SessionCache(error)) if is_busy(&error) => {
            let is_rebuilding_here = std::fs::canonicalize(base.path())
                .is_ok_and(|canonical| base.process().is_rebuilding(&canonical));
            Ok(if is_rebuilding_here {
                Observation::RebuildingHere
            } else {
                Observation::Unknown
            })
        }
        observed => observed,
    }
}
