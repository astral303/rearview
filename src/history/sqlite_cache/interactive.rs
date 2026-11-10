//! The session cache as the TUI's drawing thread reads it.
//!
//! The drawing thread reads the cache only through [`InteractiveCache`]: it
//! does not acquire refresh ownership, write any cache or parse a
//! transcript, and waits at most
//! [`INTERACTIVE_BUSY_TIMEOUT`](super::INTERACTIVE_BUSY_TIMEOUT) for another
//! connection's lock. For a session the database does not hold as its
//! transcripts are now, a read returns the older copy or no copy, and its
//! [`NextStep::after_drawing_thread_read`] names what refreshes it: the
//! refresh running now, or a session refresh on the TUI's background worker.
//! Without a cache base, a read parses the transcripts: there is no cache to
//! read.
//!
//! In debug builds, the waiting reader ([`SqliteCache::open_waiting`]),
//! refresh ownership, cache writers and a transcript parse beside a session
//! cache panic on a thread a [`DrawingThread`] marks. The TUI's `App` marks its thread for as long as
//! it lives, so a test that drives a TUI path into any of them fails. A path
//! no test drives, and a release build, are not checked.

use super::next_step::NextStep;
use super::observer::{self, AwaitedRefresh};
use super::{CacheBase, ReaderWait, SqliteCache};
use crate::error::Result;
use std::cell::Cell;
use std::marker::PhantomData;

/// The session cache base the TUI's drawing thread reads, and its one way to
/// read it. `Default` reads no cache.
#[derive(Clone, Debug, Default)]
pub struct InteractiveCache {
    base: Option<CacheBase>,
}

/// A drawing-thread read of a session, and the TUI's next step for it.
pub struct InteractiveRead<T> {
    /// The session as read; `None` when it is missing from the cache, or the
    /// cache could not be read within the drawing thread's wait.
    pub value: Option<T>,
    pub next: NextStep,
}

/// A session's copy in the cache, as one read found it.
pub(crate) struct CachedCopy<T> {
    pub(crate) value: T,
    /// True when the copy matches the session's transcripts as they are now.
    pub(crate) is_current: bool,
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

    /// `from_cache`'s copy of a session: settled when current, otherwise
    /// with its [`NextStep::after_drawing_thread_read`], as is a session
    /// missing from the cache. Without a cache base, `from_transcripts`'
    /// read, settled.
    pub(crate) fn read<T>(
        &self,
        from_cache: impl FnOnce(&SqliteCache) -> Result<Option<CachedCopy<T>>>,
        from_transcripts: impl FnOnce() -> Result<T>,
    ) -> Result<InteractiveRead<T>> {
        let Some(base) = self.base() else {
            return Ok(InteractiveRead {
                value: Some(from_transcripts()?),
                next: NextStep::Settled,
            });
        };
        let copy = self
            .open()
            .and_then(|cache| cache.map_or(Ok(None), |cache| from_cache(&cache)))
            .ok()
            .flatten();
        let next = match &copy {
            Some(copy) if copy.is_current => NextStep::Settled,
            Some(_) | None => NextStep::after_drawing_thread_read(Some(base)),
        };
        Ok(InteractiveRead {
            value: copy.map(|copy| copy.value),
            next,
        })
    }

    /// The refresh a reader of this cache waits for now; `None` while no
    /// refresh owns it, or when the cache's refresh state cannot be read.
    /// Never waits and never takes SQLite's writer lock.
    pub(crate) fn awaited_refresh(&self) -> Option<AwaitedRefresh> {
        observer::observe(self.base()?).ok()?.awaited()
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
pub(crate) fn refuse_the_drawing_thread(operation: &str) {
    debug_assert!(
        DRAWING_THREAD_MARKS.with(Cell::get) == 0,
        "the TUI's drawing thread tried to {operation}; read through InteractiveCache"
    );
}
