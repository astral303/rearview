//! A rearview's coordinator of one session cache base: it acquires refresh
//! ownership, publishes a wait for another rearview's hold on the cache,
//! and gives up its ownership at shutdown.

use super::base::CacheBase;
use super::observer::{self, Observation, OtherRearview};
use super::refill::{self, DeletionRecord};
use super::refresh::{self, RefreshOwnership, Release};
use super::scope::Cancellation;
use crate::error::{AppError, Result};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

/// The wait one operation publishes while another rearview holds the session
/// cache, read by the thread that shows it.
#[derive(Debug, Default)]
pub struct WaitStatus(Mutex<Option<OtherRearview>>);

impl WaitStatus {
    /// The other rearview the operation waits for now; `None` while it does
    /// not wait.
    pub fn current(&self) -> Option<OtherRearview> {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn set(&self, wait: Option<OtherRearview>) {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = wait;
    }
}

/// One rearview's coordinator of one cache base.
#[derive(Clone, Debug)]
pub(crate) struct Coordinator {
    base: CacheBase,
}

impl Coordinator {
    pub(crate) fn of(base: &CacheBase) -> Self {
        Self { base: base.clone() }
    }

    pub(crate) fn base(&self) -> &CacheBase {
        &self.base
    }

    /// The current owner of the cache's refresh, observed without waiting or
    /// writing.
    pub(crate) fn observe(&self) -> Result<Observation> {
        observer::observe(&self.base)
    }

    /// Refresh ownership of the cache, waiting as long as another refresh
    /// keeps it and taking over one without a commit for 15 seconds.
    /// `status` names what each blocked attempt waits for, and clears when
    /// this returns. Fails on a database error other than a busy lock, and
    /// with [`crate::error::AppError::SessionCacheRefreshLost`] once
    /// `cancellation` is set. Fails with the low-space error, without
    /// waiting, when the cache's drive is low on space (see
    /// [`refresh::check_space_of`]), and when the ownership's write finds
    /// the disk full. Fails with the refill's refusal, without waiting for
    /// ownership, when the drive doesn't have room for the cache deleted by
    /// a low-space deletion (see [`refill::check`]).
    pub(crate) fn acquire(
        &self,
        status: &WaitStatus,
        cancellation: &Cancellation,
    ) -> Result<RefreshOwnership> {
        let deletion_record = self.check_space(status, cancellation)?;
        let acquired = RefreshOwnership::acquire(&self.base, status, cancellation)
            .map(|ownership| ownership.with_deletion_record(deletion_record))
            .map_err(|error| self.set_low_space_on_full_disk(error, cancellation));
        status.set(None);
        acquired
    }

    /// [`Self::acquire`], giving up after `limit` on the base's clock: `None`
    /// while another refresh still owns the cache then. The limit includes
    /// the refill check's wait for another rearview's exclusive lock.
    pub(crate) fn acquire_within(
        &self,
        limit: Duration,
        status: &WaitStatus,
        cancellation: &Cancellation,
    ) -> Result<Option<RefreshOwnership>> {
        let clock = self.base.clock();
        let started = clock.now();
        let deletion_record = self.check_space(status, cancellation)?;
        let rest = limit.saturating_sub(clock.elapsed_since(started));
        let acquired = RefreshOwnership::acquire_within(&self.base, rest, status, cancellation)
            .map(|ownership| {
                ownership.map(|ownership| ownership.with_deletion_record(deletion_record))
            })
            .map_err(|error| self.set_low_space_on_full_disk(error, cancellation));
        status.set(None);
        acquired
    }

    /// The refill check, then the floor check.
    fn check_space(
        &self,
        status: &WaitStatus,
        cancellation: &Cancellation,
    ) -> Result<DeletionRecord> {
        let deletion_record = refill::check(&self.base, status, cancellation)?;
        refresh::check_space_of(&self.base, || !cancellation.is_cancelled())?;
        Ok(deletion_record)
    }

    fn set_low_space_on_full_disk(&self, error: AppError, cancellation: &Cancellation) -> AppError {
        refresh::set_low_space_on_full_disk(&self.base, error, || !cancellation.is_cancelled())
    }

    /// End this rearview's use of the cache: revoke the write permission of
    /// every refresh ownership it holds, revoked ones included, and release
    /// each, waiting at most `busy_lock_wait` for a busy writer lock.
    /// - An ownership with its turn held by a write batch or a free-space
    ///   check is released in one attempt, without waiting: a write batch
    ///   holds the writer lock until it ends.
    /// - After a failed release, the ownership stays this rearview's until its
    ///   operation ends or another rearview takes it over.
    pub(crate) fn shutdown(&self, busy_lock_wait: Duration) {
        let Ok(base) = self.base.canonical() else {
            return;
        };
        for owner in base.process().revoke_every_owner_of(base.path()) {
            let wait = if owner.is_turn_held {
                Duration::ZERO
            } else {
                busy_lock_wait
            };
            if refresh::release(&base, &owner.identity, wait) != Release::Failed {
                base.process().forget(&owner.identity);
            }
        }
    }
}

/// The longest the TUI's exit waits to release one ownership while another
/// connection holds SQLite's writer lock.
pub(crate) const SHUTDOWN_RELEASE_WAIT: Duration = Duration::from_secs(1);
