//! The session cache database's deletion after a write found its drive
//! short of space. The write sets its base's low-space latch, and the
//! operation stops and releases its ownership. Once no write of this
//! rearview can set the latch, the command reporting the error runs the
//! deletion and prints the deletion's outcome.

use super::base::CacheBase;
use super::coordinator::WaitStatus;
use super::refresh::{self, ExclusiveWait, FREE_SPACE_FLOOR, RefreshOwnership};
use super::scope::Cancellation;
use crate::error::{AppError, CacheDeletion};
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

/// A base's finding that its drive is short of space, and the deletion it
/// asks for, shared by the base's clones.
pub(crate) struct Latch {
    state: Mutex<State>,
    /// Held while the deletion runs, so it runs once.
    deleting: Mutex<()>,
    checks: Mutex<Checks>,
    check_ended: Condvar,
    /// Runs once the next [`Latch::wait_for_checks`] begins, for a test of
    /// what a check does during that wait.
    #[cfg(test)]
    on_wait_start: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

/// The free-space checks that may still set the latch.
#[derive(Default)]
struct Checks {
    in_flight: usize,
    /// True once [`Latch::wait_for_checks`] gave up on a check: no check sets
    /// the latch from then on.
    sealed: bool,
}

/// One free-space check that may set the latch, counted until it drops.
pub(super) struct SpaceCheck<'a> {
    latch: &'a Latch,
}

impl SpaceCheck<'_> {
    /// Set the latch with the check's finding of `free_bytes` free; false,
    /// leaving the latch as it is, once the latch is sealed.
    pub(super) fn publish(&self, free_bytes: u64) -> bool {
        let checks = self.latch.checks();
        if checks.sealed {
            return false;
        }
        self.latch.set(free_bytes);
        true
    }
}

impl Drop for SpaceCheck<'_> {
    fn drop(&mut self) {
        let mut checks = self.latch.checks();
        checks.in_flight -= 1;
        self.latch.check_ended.notify_all();
    }
}

#[derive(Clone, Debug, Default)]
enum State {
    #[default]
    Clear,
    /// A write found `free_bytes` free, below [`FREE_SPACE_FLOOR`]; the
    /// deletion has not run.
    Set { free_bytes: u64 },
    Ran {
        free_bytes: u64,
        outcome: CacheDeletion,
    },
}

impl Default for Latch {
    fn default() -> Self {
        Self {
            state: Mutex::default(),
            deleting: Mutex::default(),
            checks: Mutex::default(),
            check_ended: Condvar::new(),
            #[cfg(test)]
            on_wait_start: Mutex::default(),
        }
    }
}

impl Latch {
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn checks(&self) -> MutexGuard<'_, Checks> {
        self.checks.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Count a free-space check while `can_publish` holds and the latch is
    /// not sealed; `None` otherwise. After a write permission's revocation,
    /// later checks of its owner cannot start.
    pub(super) fn begin_check(&self, can_publish: impl FnOnce() -> bool) -> Option<SpaceCheck<'_>> {
        let mut checks = self.checks();
        if checks.sealed || !can_publish() {
            return None;
        }
        checks.in_flight += 1;
        Some(SpaceCheck { latch: self })
    }

    /// Wait up to `limit` for the free-space checks begun before this call
    /// to end, then seal the latch should one still run: its finding is
    /// dropped, and no later check starts. Call once every write permission
    /// of the base is revoked. A check by an ownership acquired after this
    /// call can still set a latch left unsealed.
    pub(crate) fn wait_for_checks(&self, limit: Duration) {
        #[cfg(test)]
        if let Some(hook) = self
            .on_wait_start
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        {
            hook();
        }
        let deadline = Instant::now() + limit;
        let mut checks = self.checks();
        while checks.in_flight > 0 {
            let Some(left) = deadline.checked_duration_since(Instant::now()) else {
                checks.sealed = true;
                return;
            };
            checks = self
                .check_ended
                .wait_timeout(checks, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }

    /// Set the latch: a write found `free_bytes` free on the drive, below
    /// the floor. A set latch keeps the first write's free bytes.
    fn set(&self, free_bytes: u64) {
        let mut state = self.state();
        if matches!(*state, State::Clear) {
            *state = State::Set { free_bytes };
        }
    }

    /// True once a write found the drive short of space.
    pub(crate) fn is_set(&self) -> bool {
        !matches!(*self.state(), State::Clear)
    }

    /// The low-space error the latch reports, with the deletion's outcome
    /// once the deletion ran; `None` while the latch is clear.
    pub(crate) fn error(&self) -> Option<AppError> {
        let (free_bytes, outcome) = match &*self.state() {
            State::Clear => return None,
            State::Set { free_bytes } => (*free_bytes, CacheDeletion::Pending),
            State::Ran {
                free_bytes,
                outcome,
            } => (*free_bytes, outcome.clone()),
        };
        Some(AppError::SessionCacheLowSpace {
            free_bytes,
            floor_bytes: FREE_SPACE_FLOOR,
            outcome,
        })
    }

    /// The free bytes the write found while the deletion has not run;
    /// `None` once it ran, and while the latch is clear.
    pub(crate) fn free_bytes_awaiting_deletion(&self) -> Option<u64> {
        match *self.state() {
            State::Set { free_bytes } => Some(free_bytes),
            State::Clear | State::Ran { .. } => None,
        }
    }

    /// Run `hook` once the next [`Self::wait_for_checks`] begins.
    #[cfg(test)]
    pub(crate) fn on_wait_start(&self, hook: impl FnOnce() + Send + 'static) {
        *self
            .on_wait_start
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(Box::new(hook));
    }
}

/// Run `base`'s low-space deletion after a write found its drive short of
/// space, and return the deletion's outcome; `None` while the latch is
/// clear. A later call returns the first call's outcome without deleting.
/// Call once no write of this rearview can set the latch: after its workers
/// on the base stopped, or after the TUI's shutdown and
/// [`Latch::wait_for_checks`]. The deletion waits for this rearview's open
/// connections to close.
pub(crate) fn run_deletion(base: &CacheBase) -> Option<CacheDeletion> {
    let latch = base.low_space();
    let _deleting = latch
        .deleting
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    let free_bytes = match &*latch.state() {
        State::Clear => return None,
        State::Ran { outcome, .. } => return Some(outcome.clone()),
        State::Set { free_bytes } => *free_bytes,
    };
    let outcome = delete(base, base.low_space_wait());
    *latch.state() = State::Ran {
        free_bytes,
        outcome: outcome.clone(),
    };
    Some(outcome)
}

/// Delete the contents of `base`'s cache database once this rearview's
/// connections to it close, under refresh ownership and SQLite's exclusive
/// lock (see [`RefreshOwnership::delete_database`]), each wait lasting at
/// most `wait`. [`CacheDeletion::Deferred`] when a connection of this
/// rearview stays open, or another rearview keeps the ownership or the
/// database, past its wait.
fn delete(base: &CacheBase, wait: Duration) -> CacheDeletion {
    let refused = base.refuse_connections(wait);
    if !refused.all_closed {
        return CacheDeletion::Deferred;
    }
    let database = super::database_path(base.path());
    // An acquisition creates a missing database.
    if !database.is_file() {
        return refresh::remove_side_files(&database);
    }
    let ownership = RefreshOwnership::acquire_within(
        base,
        wait,
        &WaitStatus::default(),
        &Cancellation::default(),
    );
    match ownership {
        Ok(Some(mut ownership)) => ownership.delete_database(ExclusiveWait::of(wait)),
        Ok(None) => CacheDeletion::Deferred,
        Err(error) => refresh::deletion_failure(&database, error),
    }
}
