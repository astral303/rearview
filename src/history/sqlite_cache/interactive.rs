//! The session cache as the TUI's drawing thread reads it.
//!
//! The drawing thread reads the cache only through [`InteractiveCache`]: it
//! does not acquire refresh ownership or write any cache, and waits at most
//! [`INTERACTIVE_BUSY_TIMEOUT`](super::INTERACTIVE_BUSY_TIMEOUT) for another
//! connection's lock. A session the database does not hold as it is now is
//! read outside the database, from its transcripts, and
//! [`InteractiveCache::follow_up`] decides what caches it: the refresh
//! running now, or the TUI's background refresh thread.
//!
//! In debug builds, the waiting reader ([`SqliteCache::open_waiting`]),
//! refresh ownership and cache writers panic on a thread a
//! [`DrawingThread`] marks. The TUI's `App` marks its thread for as long as
//! it lives, so a test that drives a TUI path into any of them fails. A path
//! no test drives, and a release build, are not checked.

use super::refresh::{self, RefreshOwner};
use super::{CacheBase, ReaderWait, SqliteCache, is_busy_error};
use crate::error::Result;
use std::cell::Cell;
use std::marker::PhantomData;

/// The session cache base the TUI reads, and its one way to read it.
/// `Default` reads no cache.
#[derive(Clone, Debug, Default)]
pub struct InteractiveCache {
    base: Option<CacheBase>,
}

/// A TUI read that did not wait for the session cache.
pub struct InteractiveRead<T> {
    pub value: T,
    /// The follow-up of a session read outside the database; `None` when
    /// the database served the read, or nothing can cache the session.
    pub follow_up: Option<CacheFollowUp>,
}

/// The refresh that caches a session read outside the database: one running
/// now, or the TUI's background refresh thread.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CacheFollowUp {
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

impl<T> InteractiveRead<T> {
    /// `read`, with nothing to follow up.
    pub fn settled(read: T) -> Self {
        Self {
            value: read,
            follow_up: None,
        }
    }
}

impl InteractiveCache {
    /// The cache under `base`, or no cache without one.
    pub fn of(base: Option<CacheBase>) -> Self {
        Self { base }
    }

    /// The cache under `base`.
    pub fn at(base: CacheBase) -> Self {
        Self { base: Some(base) }
    }

    pub(crate) fn base(&self) -> Option<&CacheBase> {
        self.base.as_ref()
    }

    /// The cache's reader, waiting at most 150 ms for another connection's
    /// lock, as each read of a session it finds does; `None` without an
    /// admitted cache.
    pub(crate) fn open(&self) -> Result<Option<SqliteCache>> {
        match self.base() {
            Some(base) => ReaderWait::Interactive.open(base),
            None => Ok(None),
        }
    }

    /// `from_cache`'s read of the cache when it serves one; otherwise
    /// `from_transcripts`', with its [`Self::follow_up`].
    pub(crate) fn read<T>(
        &self,
        from_cache: impl FnOnce(&SqliteCache) -> Result<Option<T>>,
        from_transcripts: impl FnOnce() -> Result<T>,
    ) -> Result<InteractiveRead<T>> {
        let cached = self
            .open()
            .and_then(|cache| cache.map_or(Ok(None), |cache| from_cache(&cache)));
        let fallback = match cached {
            Ok(Some(read)) => return Ok(InteractiveRead::settled(read)),
            Err(error) if is_busy_error(&error) => Fallback::DatabaseBusy,
            Ok(None) | Err(_) => Fallback::NotServed,
        };
        Ok(InteractiveRead {
            value: from_transcripts()?,
            follow_up: self.follow_up(fallback),
        })
    }

    /// The follow-up for a session the drawing thread read outside the
    /// database after `fallback`: wait for [`Self::refresh_to_await`], or
    /// cache the session in the background when there is none. `None`
    /// without a cache to write: no base, or a base whose first build this
    /// process refused.
    pub(crate) fn follow_up(&self, fallback: Fallback) -> Option<CacheFollowUp> {
        if self.base().is_none_or(CacheBase::is_refused) {
            return None;
        }
        Some(self.refresh_to_await(fallback).map_or(
            CacheFollowUp::CacheInBackground,
            CacheFollowUp::AwaitRefresh,
        ))
    }

    /// The refresh a session read outside the database after `fallback`
    /// waits for: the one holding a busy database, or the one owning the
    /// cache now. Never waits.
    pub(crate) fn refresh_to_await(&self, fallback: Fallback) -> Option<RefreshOwner> {
        let base = self.base()?;
        match fallback {
            Fallback::DatabaseBusy => Some(refresh::owner_of_busy_database(base)),
            Fallback::NotServed => refresh::running_refresh(base).ok().flatten(),
        }
    }

    /// The process whose refresh owns this cache now; `None` while no
    /// refresh owns it, or when the cache cannot be read. Never waits.
    pub(crate) fn running_refresh(&self) -> Option<RefreshOwner> {
        refresh::running_refresh(self.base()?).ok().flatten()
    }
}

thread_local! {
    /// The number of [`DrawingThread`] marks on this thread.
    static DRAWING_THREAD_MARKS: Cell<usize> = const { Cell::new(0) };
}

/// Marks the current thread as the TUI's drawing thread until dropped.
pub struct DrawingThread {
    /// Keeps the mark on the thread that made it.
    _not_send: PhantomData<*const ()>,
}

impl DrawingThread {
    pub fn mark_this_thread() -> Self {
        DRAWING_THREAD_MARKS.with(|marks| marks.set(marks.get() + 1));
        Self {
            _not_send: PhantomData,
        }
    }
}

impl Drop for DrawingThread {
    fn drop(&mut self) {
        DRAWING_THREAD_MARKS.with(|marks| marks.set(marks.get() - 1));
    }
}

/// Panic in debug builds on a thread a [`DrawingThread`] marks, naming the
/// refused `operation`.
pub(super) fn refuse_the_drawing_thread(operation: &str) {
    debug_assert!(
        DRAWING_THREAD_MARKS.with(Cell::get) == 0,
        "the TUI's drawing thread tried to {operation}; read through InteractiveCache"
    );
}
