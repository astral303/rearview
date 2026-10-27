//! The session cache as the TUI's drawing thread reads it.
//!
//! The drawing thread reads the cache only through [`InteractiveCache`]: it
//! does not acquire refresh ownership, write any cache or parse a
//! transcript, and waits at most
//! [`INTERACTIVE_BUSY_TIMEOUT`](super::INTERACTIVE_BUSY_TIMEOUT) for another
//! connection's lock. For a session whose cached copy is out of date or
//! missing, a read returns that copy or a miss, and its
//! [`NextStep::after_read_without_writing`] names what refreshes it: the
//! refresh running now, or a session refresh on the TUI's background worker.
//!
//! In debug builds, the waiting reader ([`SqliteCache::open_waiting`]),
//! refresh ownership, cache writers and the load's transcript parse panic on
//! a thread a [`DrawingThread`] marks. The TUI's `App` marks its thread for
//! as long as it lives, so a test that drives a TUI path into any of them
//! fails. A path no test drives, and a release build, are not checked.

use super::next_step::NextStep;
use super::observer::{self, AwaitedRefresh};
use super::{CacheBase, ReaderWait, SqliteCache};
use crate::error::Result;
use std::cell::Cell;
use std::marker::PhantomData;

/// The session cache base the TUI's drawing thread reads, and its one way to
/// read it.
#[derive(Clone, Debug)]
pub struct InteractiveCache {
    base: CacheBase,
}

/// A drawing-thread read of a session. Its copy comes out only through
/// [`Self::into_read`], with the TUI's next step for it.
pub struct InteractiveRead<T> {
    read: CachedRead<T>,
}

/// A drawing-thread read's copy of a session, and the TUI's next step for it
/// when the copy is out of date or missing.
pub enum CachedRead<T> {
    Current(CurrentCopy<T>),
    Older {
        copy: T,
        next: NextStep,
    },
    /// Missing from the cache, or the cache could not be read within the
    /// drawing thread's wait.
    Missing {
        next: NextStep,
    },
}

/// A copy that matched the session's transcripts when the drawing thread
/// read it. Only [`InteractiveCache::read`] makes one.
pub struct CurrentCopy<T>(T);

impl<T> InteractiveRead<T> {
    /// This read with its copy converted by `convert`.
    pub fn map<U>(self, convert: impl FnOnce(T) -> U) -> InteractiveRead<U> {
        let read = match self.read {
            CachedRead::Current(CurrentCopy(copy)) => {
                CachedRead::Current(CurrentCopy(convert(copy)))
            }
            CachedRead::Older { copy, next } => CachedRead::Older {
                copy: convert(copy),
                next,
            },
            CachedRead::Missing { next } => CachedRead::Missing { next },
        };
        InteractiveRead { read }
    }

    pub fn into_read(self) -> CachedRead<T> {
        self.read
    }
}

impl<T> CurrentCopy<T> {
    pub fn into_value(self) -> T {
        self.0
    }
}

/// A session's copy in the cache, as one read found it.
pub(crate) struct CachedCopy<T> {
    pub(crate) value: T,
    /// True when the copy matches the session's transcripts as they are now.
    pub(crate) is_current: bool,
}

impl InteractiveCache {
    /// The cache under `base`.
    pub fn at(base: CacheBase) -> Self {
        Self { base }
    }

    /// The cache's reader, waiting at most 150 ms for another connection's
    /// lock, as each read of a session it finds does; `None` without an
    /// admitted cache.
    pub(crate) fn open(&self) -> Result<Option<SqliteCache>> {
        ReaderWait::Interactive.open(&self.base)
    }

    /// `from_cache`'s copy of a session: current, or out of date with its
    /// [`NextStep::after_read_without_writing`], as is a session missing
    /// from the cache or a cache that could not be read.
    pub(crate) fn read<T>(
        &self,
        from_cache: impl FnOnce(&SqliteCache) -> Result<Option<CachedCopy<T>>>,
    ) -> InteractiveRead<T> {
        let copy = self
            .open()
            .and_then(|cache| cache.map_or(Ok(None), |cache| from_cache(&cache)));
        let read = match copy {
            Ok(Some(copy)) if copy.is_current => CachedRead::Current(CurrentCopy(copy.value)),
            Ok(Some(copy)) => CachedRead::Older {
                copy: copy.value,
                next: NextStep::after_read_without_writing(&self.base),
            },
            Ok(None) => CachedRead::Missing {
                next: NextStep::after_read_without_writing(&self.base),
            },
            Err(error) => CachedRead::Missing {
                next: NextStep::after_failed_interactive_read(&self.base, &error),
            },
        };
        InteractiveRead { read }
    }

    /// The refresh a reader of this cache waits for now; `None` while no
    /// refresh owns it, or when the cache's refresh state cannot be read.
    /// Waits up to 100 ms for the database's exclusive lock and never takes
    /// SQLite's writer lock.
    pub(crate) fn awaited_refresh(&self) -> Option<AwaitedRefresh> {
        observer::observe(&self.base).ok()?.awaited()
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
