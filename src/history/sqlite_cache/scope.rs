//! One user operation's hold on the session cache: a launch, a list
//! `Ctrl+R`, a session's caching in the background, or an agent command.

use super::base::CacheBase;
use super::coordinator::{Coordinator, WaitStatus};
use super::observer::OtherRearview;
use super::refresh::{RefreshOwnership, RefreshToken};
use crate::error::Result;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

/// An operation's limit on its wait for refresh ownership.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Patience {
    /// As long as another refresh keeps the cache.
    UntilAcquired,
    /// At most this long; then the operation reads the cache as it is.
    AtMost(Duration),
}

/// An operation's access to the session cache.
#[derive(Clone)]
pub(crate) enum OperationAccess {
    /// Read it, and write it for this owner while its write permission
    /// stands.
    Owner(RefreshToken),
    /// Read it as it is, parsing nothing: another rearview kept refresh
    /// ownership past the operation's patience.
    AsIs,
}

/// One operation's hold on the session cache. It acquires refresh ownership
/// through the base's [`Coordinator`] the first time the operation needs to
/// write, publishes its wait on its [`WaitStatus`] from the first blocked
/// attempt, and keeps ownership until it drops.
pub struct OperationScope {
    coordinator: Coordinator,
    patience: Patience,
    status: Arc<WaitStatus>,
    held: Mutex<Held>,
}

enum Held {
    Nothing,
    Ownership(RefreshOwnership),
    AsIs,
    /// A write found this many bytes free on the cache's drive, below the
    /// floor, and the cache database was deleted.
    OutOfSpace(u64),
}

impl Held {
    /// When a write of the held ownership found the drive short of space:
    /// release the ownership and delete the cache database under `base`.
    fn abort_when_out_of_space(&mut self, base: &CacheBase) {
        let Self::Ownership(ownership) = self else {
            return;
        };
        let Some(free_bytes) = ownership.token().out_of_space() else {
            return;
        };
        *self = Self::OutOfSpace(free_bytes);
        super::delete_database(base.path());
    }
}

fn out_of_space(free_bytes: u64) -> crate::error::AppError {
    crate::error::AppError::SessionCacheOutOfSpace {
        free_bytes,
        floor_bytes: super::refresh::FREE_SPACE_FLOOR,
    }
}

impl OperationScope {
    /// The operation on the cache under `base`, waiting as `patience` allows
    /// and publishing its wait on `status`.
    pub fn new(base: &CacheBase, patience: Patience, status: Arc<WaitStatus>) -> Self {
        Self {
            coordinator: Coordinator::of(base),
            patience,
            status,
            held: Mutex::new(Held::Nothing),
        }
    }

    /// An operation that reads the cache under `base` as it is and never
    /// acquires ownership.
    pub fn as_is(base: &CacheBase, status: Arc<WaitStatus>) -> Self {
        Self {
            held: Mutex::new(Held::AsIs),
            ..Self::new(base, Patience::UntilAcquired, status)
        }
    }

    /// This operation's access to the cache now, acquiring refresh ownership
    /// on the first call that needs it. Fails on a database error
    /// other than a busy lock, and once a write found the cache's drive
    /// short of space (see [`Self::finish`]).
    pub(crate) fn access(&self) -> Result<OperationAccess> {
        let coordinator = &self.coordinator;
        let mut held = self.held.lock().unwrap_or_else(PoisonError::into_inner);
        held.abort_when_out_of_space(coordinator.base());
        match &*held {
            Held::Ownership(ownership) => {
                return Ok(OperationAccess::Owner(ownership.token().clone()));
            }
            Held::AsIs => return Ok(OperationAccess::AsIs),
            Held::OutOfSpace(free_bytes) => return Err(out_of_space(*free_bytes)),
            Held::Nothing => {}
        }
        let ownership = match self.patience {
            Patience::UntilAcquired => Some(coordinator.acquire(&self.status)?),
            Patience::AtMost(limit) => coordinator.acquire_within(limit, &self.status)?,
        };
        Ok(match ownership {
            Some(ownership) => {
                let token = ownership.token().clone();
                *held = Held::Ownership(ownership);
                OperationAccess::Owner(token)
            }
            None => {
                *held = Held::AsIs;
                OperationAccess::AsIs
            }
        })
    }

    /// True once a failed write batch revoked the write permission of this
    /// operation's refresh ownership: what it read after that is missing
    /// from the cache.
    pub(crate) fn has_lost_write_permission(&self) -> bool {
        matches!(
            &*self.held.lock().unwrap_or_else(PoisonError::into_inner),
            Held::Ownership(ownership) if !ownership.token().can_publish()
        )
    }

    /// `read`'s result once the database is readable: while `read` finds it
    /// exclusively locked, as during another rearview's rebuild, wait and
    /// publish the rebuild until the lock goes. Fails on any other error.
    pub(crate) fn read_when_unlocked<T>(&self, mut read: impl FnMut() -> Result<T>) -> Result<T> {
        let clock = self.coordinator.base().clock();
        // The wait published before this read's, restored once it ends.
        let mut wait_before: Option<Option<OtherRearview>> = None;
        loop {
            match read() {
                Err(error) if super::is_busy_error(&error) => {
                    wait_before.get_or_insert_with(|| self.status.current());
                    self.status.set(Some(OtherRearview::Rebuilding));
                    clock.sleep(UNLOCK_POLL);
                }
                result => {
                    if let Some(earlier) = wait_before {
                        self.status.set(earlier);
                    }
                    return result;
                }
            }
        }
    }
}

impl OperationScope {
    /// End the operation, releasing its refresh ownership. Fails when one of
    /// its writes found the cache's drive with less than the floor free: the
    /// cache database is then deleted, and the operation's command reports
    /// the free space.
    pub fn finish(self) -> Result<()> {
        let mut held = self.held.lock().unwrap_or_else(PoisonError::into_inner);
        held.abort_when_out_of_space(self.coordinator.base());
        if let Held::OutOfSpace(free_bytes) = *held {
            return Err(out_of_space(free_bytes));
        }
        *held = Held::Nothing;
        Ok(())
    }
}

/// The interval between a read's attempts while the database is
/// exclusively locked.
const UNLOCK_POLL: Duration = Duration::from_millis(200);
