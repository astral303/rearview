//! The TUI's next step with a session it read: nothing, a second read once a
//! refresh completes, or a session refresh on the background worker.

use super::CacheBase;
use super::observer::{self, AwaitedRefresh};

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
    /// No refresh owns the cache: a session refresh on the TUI's background
    /// worker reads the session under refresh ownership and caches it.
    CacheInBackground,
}

impl NextStep {
    /// The next step of a session whose copy in the database under `base` is
    /// out of date or missing, read by the drawing thread or by a read that
    /// could not write: wait for the refresh the cache's observation names,
    /// or cache the session in the background while none owns it. Never
    /// waits.
    pub(crate) fn after_read_without_writing(base: &CacheBase) -> Self {
        awaited_refresh(base).map_or(Self::CacheInBackground, Self::AwaitRefresh)
    }
}

/// The refresh a reader of `base`'s cache waits for now. An observation that
/// fails names none: the next read of the cache meets the failure itself.
fn awaited_refresh(base: &CacheBase) -> Option<AwaitedRefresh> {
    observer::observe(base).ok()?.awaited()
}
