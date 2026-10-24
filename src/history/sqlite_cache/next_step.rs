//! The TUI's next step with a session it read: nothing, a second read once a
//! refresh completes, or a session refresh on the background worker.

use super::CacheBase;
use super::observer::{self, AwaitedRefresh};

/// The TUI's next step with a session it read.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
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
    /// The next step of a session the TUI's drawing thread read outside the
    /// database under `base`: wait for the refresh the cache's observation
    /// names, or cache the session in the background while none owns it.
    /// Never waits.
    pub(crate) fn after_drawing_thread_read(base: &CacheBase) -> Self {
        awaited_refresh(base).map_or(Self::CacheInBackground, Self::AwaitRefresh)
    }

    /// The next step of a session read outside the database under `base` by
    /// a read that could not write it: wait for the refresh the cache's
    /// observation names; settled while none owns it. Never waits.
    pub(crate) fn after_read_without_writing(base: &CacheBase) -> Self {
        awaited_refresh(base).map_or(Self::Settled, Self::AwaitRefresh)
    }
}

/// The refresh a reader of `base`'s cache waits for now. An observation that
/// fails names none: the next read of the cache meets the failure itself.
fn awaited_refresh(base: &CacheBase) -> Option<AwaitedRefresh> {
    observer::observe(base).ok()?.awaited()
}
