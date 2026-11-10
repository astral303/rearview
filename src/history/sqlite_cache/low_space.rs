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
    /// The longest each step of the deletion waits.
    wait: Duration,
    /// The free-space checks that may still set the latch.
    checks_in_flight: Mutex<usize>,
    check_ended: Condvar,
}

/// One free-space check that may set the latch, counted until it drops.
pub(super) struct SpaceCheck<'a> {
    latch: &'a Latch,
}

impl Drop for SpaceCheck<'_> {
    fn drop(&mut self) {
        let mut checks = self.latch.checks_in_flight();
        *checks -= 1;
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
            wait: super::WAITING_BUSY_TIMEOUT,
            checks_in_flight: Mutex::default(),
            check_ended: Condvar::new(),
        }
    }
}

impl Latch {
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn checks_in_flight(&self) -> MutexGuard<'_, usize> {
        self.checks_in_flight
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Count a free-space check while `can_publish` holds; `None` when it
    /// does not. A write permission revoked before
    /// [`Self::wait_for_checks`] then lets no later check set the latch.
    pub(super) fn begin_check(&self, can_publish: impl FnOnce() -> bool) -> Option<SpaceCheck<'_>> {
        let mut checks = self.checks_in_flight();
        if !can_publish() {
            return None;
        }
        *checks += 1;
        Some(SpaceCheck { latch: self })
    }

    /// Wait up to `limit` for the free-space checks begun before this call
    /// to end; false when one still runs then. Call once every write
    /// permission of the base is revoked: the latch then stays as it is.
    pub(crate) fn wait_for_checks(&self, limit: Duration) -> bool {
        let deadline = Instant::now() + limit;
        let mut checks = self.checks_in_flight();
        while *checks > 0 {
            let Some(left) = deadline.checked_duration_since(Instant::now()) else {
                return false;
            };
            checks = self
                .check_ended
                .wait_timeout(checks, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        true
    }

    /// Set the latch: a write found `free_bytes` free on the drive, below
    /// the floor. A set latch keeps the first write's free bytes.
    pub(super) fn set(&self, free_bytes: u64) {
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

    /// Once the latch is set, turn off the checkpoint SQLite runs when
    /// `connection`, about to close, is the last connection: copying the
    /// write-ahead log into the database needs the space the drive lacks.
    pub(super) fn keep_log_on_close(&self, connection: &rusqlite::Connection) {
        if !self.is_set() {
            return;
        }
        // SQLite fails the call only for an option it does not know.
        let kept = connection.set_db_config(
            rusqlite::config::DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE,
            true,
        );
        debug_assert!(kept.is_ok(), "SQLite turns the checkpoint on close off");
    }

    #[cfg(test)]
    pub(super) fn set_wait(&mut self, wait: Duration) {
        self.wait = wait;
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
    let outcome = delete(base, latch.wait);
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
