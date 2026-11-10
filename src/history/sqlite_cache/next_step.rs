//! The TUI's next step with a session it read: nothing, a second read once a
//! refresh completes, or a session refresh on the background worker.

use super::observer::{self, AwaitedRefresh, Observation};
use super::{CacheBase, is_busy_error};
use crate::error::{AppError, Result};

/// The TUI's next step with a session it read.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[must_use = "a session left without its next step is never refreshed"]
pub enum NextStep {
    /// The database served the read or holds what it read, or a session
    /// refresh would find nothing to cache.
    Settled,
    /// A refresh owns the cache, in this process or another: read the
    /// session again once it completes.
    AwaitRefresh(AwaitedRefresh),
    /// No refresh owns the cache, or its refresh state could not be read: a
    /// session refresh on the TUI's background worker reads the session under
    /// refresh ownership and caches it.
    CacheInBackground,
}

impl NextStep {
    /// The next step of a session whose copy in the database under `base` is
    /// out of date or missing, read by the drawing thread or by a read that
    /// could not write: wait for the refresh the cache's observation names,
    /// or request a session refresh while none owns it. The
    /// observation waits up to 100 ms for the database's exclusive lock.
    pub(crate) fn after_read_without_writing(base: &CacheBase) -> Self {
        Self::after_observation(observer::observe(base))
    }

    /// [`Self::after_read_without_writing`] for a drawing-thread read of the
    /// database under `base` that failed with `error`. After a busy error,
    /// observe without waiting: the read already waited 150 ms for the lock,
    /// longer than the observation's 100 ms.
    pub(crate) fn after_failed_interactive_read(base: &CacheBase, error: &AppError) -> Self {
        if is_busy_error(error) {
            Self::after_observation(observer::observe_after_locked_read(base))
        } else {
            Self::after_read_without_writing(base)
        }
    }

    fn after_observation(observation: Result<Observation>) -> Self {
        observation
            .ok()
            .and_then(Observation::awaited)
            .map_or(Self::CacheInBackground, Self::AwaitRefresh)
    }
}
