//! One user operation's hold on the session cache: a launch, a list
//! `Ctrl+R`, a session refresh, a rename or delete, or an agent command.

use super::base::CacheBase;
use super::coordinator::{Coordinator, WaitStatus};
use super::observer::OtherRearview;
use super::refresh::{RefreshOwnership, RefreshToken};
use crate::error::{AppError, Result};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

/// A request, shared by every copy, that the operations holding it stop:
/// each acquires no ownership and parses no more once it is set.
#[derive(Clone, Debug, Default)]
pub struct Cancellation(Arc<AtomicBool>);

impl Cancellation {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

/// Why an operation reads the cache as it is.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StaleCause {
    /// Another rearview keeps refresh ownership, or took it over.
    AnotherRearview,
    /// This operation owns the refresh, and its writes stopped: a write
    /// batch failed, or the operation was cancelled.
    WritesStopped,
}

/// An operation's limit on its wait for refresh ownership.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Patience {
    /// As long as another refresh keeps the cache.
    UntilAcquired,
    /// At most this long; then the operation reads the cache as it is.
    AtMost(Duration),
}

/// An operation's access to the session cache.
pub(crate) enum OperationAccess<'s> {
    /// Read it, parse transcripts, and write it as the live refresh owner.
    Owner(LiveOwner<'s>),
    /// Read it as it is, parsing nothing, for the operation's
    /// [`OperationScope::stale_cause`].
    AsIs,
}

/// An operation's refresh ownership while it may write and parse: its write
/// permission stands, the operation is not cancelled, and no write found the
/// drive short of space. Only [`OperationScope::access`] makes one.
pub(crate) struct LiveOwner<'s> {
    scope: &'s OperationScope,
    token: RefreshToken,
}

impl<'s> LiveOwner<'s> {
    /// The ownership's token, for reads and writes under its writer lock.
    pub(crate) fn token(&self) -> &RefreshToken {
        &self.token
    }

    /// True while the ownership may still write and parse.
    pub(crate) fn is_live(&self) -> bool {
        self.scope.is_live(&self.token)
    }

    /// The permit for the transcripts one writer-locked batch parses;
    /// `None` once the ownership is no longer live.
    pub(crate) fn parse_permit(&self) -> Option<super::ParsePermit<'s>> {
        self.is_live().then(super::permit::ParsePermit::owner)
    }
}

/// One operation's hold on the session cache. It acquires refresh ownership
/// through the base's [`Coordinator`] the first time the operation needs to
/// write, publishes its wait on its [`WaitStatus`] from the first blocked
/// attempt, and keeps ownership until it drops, after its write permission
/// is revoked too.
pub struct OperationScope {
    coordinator: Coordinator,
    patience: Patience,
    status: Arc<WaitStatus>,
    cancellation: Cancellation,
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

fn out_of_space(free_bytes: u64) -> AppError {
    AppError::SessionCacheOutOfSpace {
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
            cancellation: Cancellation::default(),
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

    /// This operation, stopped by `cancellation`: once it is set, a wait for
    /// ownership fails, and the operation reads the cache as it is.
    pub fn cancelled_by(self, cancellation: &Cancellation) -> Self {
        Self {
            cancellation: cancellation.clone(),
            ..self
        }
    }

    /// This operation's access to the cache now, acquiring refresh ownership
    /// on the first call that needs it. An operation that owns the refresh
    /// reads the cache as it is once its write permission is revoked or it
    /// is cancelled, and keeps the ownership until [`Self::finish`]. Fails
    /// on a database error other than a busy lock, when cancelled while
    /// waiting for ownership, and once a write found the cache's drive short
    /// of space (see [`Self::finish`]).
    pub(crate) fn access(&self) -> Result<OperationAccess<'_>> {
        let coordinator = &self.coordinator;
        let mut held = self.held.lock().unwrap_or_else(PoisonError::into_inner);
        held.abort_when_out_of_space(coordinator.base());
        match &*held {
            Held::Ownership(ownership) => return Ok(self.access_of(ownership.token())),
            Held::AsIs => return Ok(OperationAccess::AsIs),
            Held::OutOfSpace(free_bytes) => return Err(out_of_space(*free_bytes)),
            Held::Nothing => {}
        }
        let ownership = match self.patience {
            Patience::UntilAcquired => Some(coordinator.acquire(&self.status, &self.cancellation)?),
            Patience::AtMost(limit) => {
                coordinator.acquire_within(limit, &self.status, &self.cancellation)?
            }
        };
        Ok(match ownership {
            Some(ownership) => {
                let access = self.access_of(ownership.token());
                *held = Held::Ownership(ownership);
                access
            }
            None => {
                *held = Held::AsIs;
                OperationAccess::AsIs
            }
        })
    }

    /// The access this operation's ownership `token` gives now.
    fn access_of(&self, token: &RefreshToken) -> OperationAccess<'_> {
        if !self.is_live(token) {
            return OperationAccess::AsIs;
        }
        OperationAccess::Owner(LiveOwner {
            scope: self,
            token: token.clone(),
        })
    }

    /// True while `token`, this operation's, may write and parse.
    fn is_live(&self, token: &RefreshToken) -> bool {
        token.can_publish() && !self.cancellation.is_cancelled()
    }

    /// Why the operation owning `token` reads the cache as it is.
    fn stale_cause_of(token: &RefreshToken) -> StaleCause {
        if token.is_replaced() {
            StaleCause::AnotherRearview
        } else {
            StaleCause::WritesStopped
        }
    }

    /// Why sessions this operation read from copies older than their
    /// transcripts are stale: its own writes stopped, or another rearview
    /// keeps or took over the refresh.
    pub(crate) fn stale_cause(&self) -> StaleCause {
        match &*self.held.lock().unwrap_or_else(PoisonError::into_inner) {
            Held::Ownership(ownership) => Self::stale_cause_of(ownership.token()),
            Held::OutOfSpace(_) => StaleCause::WritesStopped,
            Held::Nothing | Held::AsIs => StaleCause::AnotherRearview,
        }
    }

    /// The permit this operation parses transcripts with as the live
    /// refresh owner, acquiring ownership as [`Self::access`] does; `None`
    /// while it reads the cache as it is.
    pub(crate) fn parse_permit(&self) -> Result<Option<super::ParsePermit<'_>>> {
        Ok(match self.access()? {
            OperationAccess::Owner(_) => Some(super::permit::ParsePermit::owner()),
            OperationAccess::AsIs => None,
        })
    }

    /// [`Self::parse_permit`] of an operation that waits as long as another
    /// rearview keeps the cache, so it always acquires ownership. Fails as
    /// acquiring it fails, and with [`AppError::SessionCacheRefreshLost`]
    /// once the ownership is no longer live.
    pub(crate) fn acquired_parse_permit(&self) -> Result<super::ParsePermit<'_>> {
        assert_eq!(
            self.patience,
            Patience::UntilAcquired,
            "only an operation that waits until it acquires ownership is sure to own the cache"
        );
        self.parse_permit()?
            .ok_or(AppError::SessionCacheRefreshLost)
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

    /// [`read_when_unlocked`] on this operation's clock, publishing on its
    /// status, until the operation is cancelled.
    pub(crate) fn read_when_unlocked<T>(&self, read: impl FnMut() -> Result<T>) -> Result<T> {
        read_when_unlocked(
            self.coordinator.base().clock().as_ref(),
            &self.status,
            &self.cancellation,
            read,
        )
    }
}

/// `read`'s result once the database is readable: while `read` finds it
/// exclusively locked, as during another rearview's rebuild, wait on `clock`
/// and publish the rebuild on `status` until the lock goes. Fails on any
/// other error, and with the busy error once `cancellation` is set.
pub(crate) fn read_when_unlocked<T>(
    clock: &dyn crate::clock::Clock,
    status: &WaitStatus,
    cancellation: &Cancellation,
    mut read: impl FnMut() -> Result<T>,
) -> Result<T> {
    // The wait published before this read's, restored once it ends.
    let mut wait_before: Option<Option<OtherRearview>> = None;
    loop {
        match read() {
            Err(error) if super::is_busy_error(&error) && !cancellation.is_cancelled() => {
                wait_before.get_or_insert_with(|| status.current());
                status.set(Some(OtherRearview::Rebuilding));
                clock.sleep(UNLOCK_POLL);
            }
            result => {
                if let Some(earlier) = wait_before {
                    status.set(earlier);
                }
                return result;
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
