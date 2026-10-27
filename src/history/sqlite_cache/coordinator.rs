//! A rearview's coordinator of one session cache base: it acquires refresh
//! ownership, publishes a wait for another rearview's hold on the cache,
//! and gives up its ownership at shutdown.

use super::base::CacheBase;
use super::observer::{self, Observation, OtherRearview};
use super::refresh::{self, RefreshOwnership, Release};
use super::scope::Cancellation;
use crate::error::Result;
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

    /// Who owns the cache's refresh now, without waiting or writing.
    pub(crate) fn observe(&self) -> Result<Observation> {
        observer::observe(&self.base)
    }

    /// Refresh ownership of the cache, waiting as long as another refresh
    /// keeps it and taking over one without a commit for 15 seconds.
    /// `status` names what each blocked attempt waits for, and clears when
    /// this returns. Fails on a database error other than a busy lock, and
    /// with [`crate::error::AppError::SessionCacheRefreshLost`] once
    /// `cancellation` is set.
    pub(crate) fn acquire(
        &self,
        status: &WaitStatus,
        cancellation: &Cancellation,
    ) -> Result<RefreshOwnership> {
        let acquired = RefreshOwnership::acquire(&self.base, status, cancellation);
        status.set(None);
        acquired
    }

    /// [`Self::acquire`], giving up after `limit` on the base's clock: `None`
    /// while another refresh still owns the cache then.
    pub(crate) fn acquire_within(
        &self,
        limit: Duration,
        status: &WaitStatus,
        cancellation: &Cancellation,
    ) -> Result<Option<RefreshOwnership>> {
        let acquired = RefreshOwnership::acquire_within(&self.base, limit, status, cancellation);
        status.set(None);
        acquired
    }

    /// End this rearview's use of the cache: revoke the write permission of
    /// every refresh ownership it holds, revoked ones included, and release
    /// each, waiting at most [`SHUTDOWN_RELEASE_WAIT`] for a busy writer
    /// lock. An ownership whose release fails stays this rearview's until
    /// its operation ends or another rearview takes it over.
    pub(crate) fn shutdown(&self) {
        let Ok(base) = self.base.canonical() else {
            return;
        };
        for identity in base.process().revoke_every_owner_of(base.path()) {
            if refresh::release(&base, &identity, SHUTDOWN_RELEASE_WAIT) != Release::Failed {
                base.process().forget(&identity);
            }
        }
    }
}

/// The longest [`Coordinator::shutdown`] waits to release one ownership.
const SHUTDOWN_RELEASE_WAIT: Duration = Duration::from_secs(1);
