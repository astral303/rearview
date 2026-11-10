//! The session cache as the TUI's drawing thread reads it.
//!
//! The drawing thread reads the cache only through [`InteractiveCache`]: it
//! does not acquire refresh ownership or write any cache, and waits at most
//! [`INTERACTIVE_BUSY_TIMEOUT`](super::INTERACTIVE_BUSY_TIMEOUT) for another
//! connection's lock. A session the database does not hold as it is now is
//! read outside the database, from its transcripts, and its
//! [`NextStep::after_drawing_thread_read`] names what caches it: the refresh
//! running now, or the TUI's background refresh thread.
//!
//! In debug builds, the waiting reader ([`SqliteCache::open_waiting`]),
//! refresh ownership and cache writers panic on a thread a
//! [`DrawingThread`] marks. The TUI's `App` marks its thread for as long as
//! it lives, so a test that drives a TUI path into any of them fails. A path
//! no test drives, and a release build, are not checked.

use super::next_step::{Fallback, NextStep};
use super::refresh::{self, RefreshOwner};
use super::{CacheBase, ReaderWait, SqliteCache, is_busy_error};
use crate::error::Result;
use std::cell::Cell;
use std::marker::PhantomData;

/// The session cache base the TUI's drawing thread reads, and its one way to
/// read it. `Default` reads no cache.
#[derive(Clone, Debug, Default)]
pub struct InteractiveCache {
    base: Option<CacheBase>,
}

/// The value a drawing-thread read returned without waiting for the session
/// cache, and the TUI's next step for the session.
pub struct InteractiveRead<T> {
    pub value: T,
    pub next: NextStep,
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

    /// `from_cache`'s read of the cache when it serves one, settled;
    /// otherwise `from_transcripts`', with its
    /// [`NextStep::after_drawing_thread_read`].
    pub(crate) fn read<T>(
        &self,
        from_cache: impl FnOnce(&SqliteCache) -> Result<Option<T>>,
        from_transcripts: impl FnOnce() -> Result<T>,
    ) -> Result<InteractiveRead<T>> {
        let cached = self
            .open()
            .and_then(|cache| cache.map_or(Ok(None), |cache| from_cache(&cache)));
        let fallback = match cached {
            Ok(Some(read)) => {
                return Ok(InteractiveRead {
                    value: read,
                    next: NextStep::Settled,
                });
            }
            Err(error) if is_busy_error(&error) => Fallback::DatabaseBusy,
            Ok(None) | Err(_) => Fallback::NotServed,
        };
        Ok(InteractiveRead {
            value: from_transcripts()?,
            next: NextStep::after_drawing_thread_read(self.base(), fallback),
        })
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
