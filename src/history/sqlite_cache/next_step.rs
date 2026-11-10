//! The TUI's next step with a session it read: nothing, a second read once a
//! refresh completes, or a session refresh on the background worker.

use super::CacheBase;
use super::refresh::{self, RefreshOwner};

/// The TUI's next step with a session it read.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NextStep {
    /// The database served the read or holds what it read, or a session
    /// refresh would find nothing to cache.
    Settled,
    /// A refresh owns the cache, in this process or another: read the
    /// session again once it completes.
    AwaitRefresh(RefreshOwner),
    /// No refresh owns the cache: the TUI's background refresh thread reads
    /// the session under refresh ownership and caches it.
    CacheInBackground,
}

/// The way a read fell back from the database to the transcripts.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Fallback {
    /// The database stayed busy past the reader's timeout.
    DatabaseBusy,
    /// The database does not hold the session as it is now, or failed.
    NotServed,
}

impl NextStep {
    /// The next step of a session the TUI's drawing thread read outside the
    /// database under `base` after `fallback`: wait for the refresh holding
    /// the cache, or cache the session in the background while none does.
    /// Settled without a base, or with a base whose first build this process
    /// refused. Never waits.
    pub(crate) fn after_drawing_thread_read(base: Option<&CacheBase>, fallback: Fallback) -> Self {
        let Some(base) = base.filter(|base| !base.is_refused()) else {
            return Self::Settled;
        };
        refresh_to_await(base, fallback).map_or(Self::CacheInBackground, Self::AwaitRefresh)
    }

    /// The next step of a session read outside the database under `base`
    /// after `fallback`, by a read that waited for the cache and could not
    /// write it: wait for the refresh holding the cache; settled while none
    /// does. Never waits.
    pub(crate) fn after_read_without_writing(base: Option<&CacheBase>, fallback: Fallback) -> Self {
        base.and_then(|base| refresh_to_await(base, fallback))
            .map_or(Self::Settled, Self::AwaitRefresh)
    }
}

/// The refresh a session read outside `base`'s database after `fallback`
/// waits for: the one holding a busy database, or the one owning the cache
/// now. Never waits.
fn refresh_to_await(base: &CacheBase, fallback: Fallback) -> Option<RefreshOwner> {
    match fallback {
        Fallback::DatabaseBusy => Some(refresh::owner_of_busy_database(base)),
        Fallback::NotServed => refresh::running_refresh(base).ok().flatten(),
    }
}
