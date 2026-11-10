//! One user operation's hold on the session cache: a launch, a list
//! `Ctrl+R`, a session refresh, a rename or delete, or an agent command.

use super::base::CacheBase;
use super::coordinator::{Coordinator, WaitStatus};
use super::copy_state::{CopyMiss, CopyRead};
use super::observer::OtherRearview;
use super::refresh::{Batch, RefreshOwnership, RefreshToken};
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
/// drive low on space. Only [`OperationScope::acquire_access`] makes one.
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

    /// `parse` run with the permit to parse as one write batch (see
    /// [`RefreshToken::parse_batch`]); `None` once the ownership is no longer
    /// live, so nothing ran.
    pub(crate) fn parse_batch<T>(
        &self,
        parse: impl for<'p> FnOnce(super::ParsePermit<'p>) -> T,
    ) -> Option<Batch<T>> {
        if !self.is_live() {
            return None;
        }
        Some(self.token.parse_batch(parse))
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
        let mut operation = Self::new(base, Patience::UntilAcquired, status);
        *operation
            .held
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner) = Held::AsIs;
        operation
    }

    /// This operation, stopped by `cancellation`: once it is set, a wait for
    /// ownership fails, and the operation reads the cache as it is.
    pub fn cancelled_by(mut self, cancellation: &Cancellation) -> Self {
        self.cancellation = cancellation.clone();
        self
    }

    /// Acquire this operation's access to the cache now. The first call that
    /// needs refresh ownership waits for it as the operation's patience
    /// allows. An operation that owns the refresh reads the cache as it is
    /// once its write permission is revoked or it is cancelled, and keeps
    /// the ownership until [`Self::finish`]. Fails
    /// on a database error other than a busy lock, when cancelled while
    /// waiting for ownership, and once a write found the cache's drive low
    /// on space (see [`Self::finish`]).
    pub(crate) fn acquire_access(&self) -> Result<OperationAccess<'_>> {
        let coordinator = &self.coordinator;
        if let Some(error) = coordinator.base().low_space().error() {
            return Err(error);
        }
        let mut held = self.held.lock().unwrap_or_else(PoisonError::into_inner);
        match &*held {
            Held::Ownership(ownership) => return Ok(self.access_of(ownership.token())),
            Held::AsIs => return Ok(OperationAccess::AsIs),
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
            Held::Nothing | Held::AsIs => StaleCause::AnotherRearview,
        }
    }

    /// Acquire refresh ownership as [`Self::acquire_access`] does: the first
    /// call waits as long as the operation's [`Patience`] allows. While this
    /// operation is the live owner, returns `parse`'s result, run with the
    /// permit to parse transcripts as one write batch, so no other rearview
    /// takes the ownership over while it runs. `None` while the operation
    /// reads the cache as it is. Fails as `acquire_access` fails, and with
    /// the batch's own error when it could not start or commit.
    pub(crate) fn parse_as_owner<T>(
        &self,
        parse: impl for<'p> FnOnce(super::ParsePermit<'p>) -> T,
    ) -> Result<Option<T>> {
        Ok(match self.acquire_access()? {
            OperationAccess::Owner(owner) => match owner.parse_batch(parse) {
                Some(Batch::Ran(parsed, committed)) => {
                    committed?;
                    Some(parsed)
                }
                Some(Batch::NotStarted(error)) => return Err(error),
                None => None,
            },
            OperationAccess::AsIs => None,
        })
    }

    /// [`Self::parse_as_owner`], waiting until this operation acquires
    /// refresh ownership for as long as another rearview keeps the cache.
    /// Panics unless the operation's patience is [`Patience::UntilAcquired`].
    /// Fails as [`Self::parse_as_owner`] fails, and with
    /// [`AppError::SessionCacheRefreshLost`] once the ownership is no longer
    /// live.
    pub(crate) fn wait_to_parse_as_owner<T>(
        &self,
        parse: impl for<'p> FnOnce(super::ParsePermit<'p>) -> T,
    ) -> Result<T> {
        assert_eq!(
            self.patience,
            Patience::UntilAcquired,
            "only an operation that waits until it acquires ownership is sure to own the cache"
        );
        self.parse_as_owner(parse)?
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
/// and publish the rebuild on `status` until the lock is released. Fails on
/// any other error, and with the busy error once `cancellation` is set.
pub(crate) fn read_when_unlocked<T>(
    clock: &dyn crate::clock::Clock,
    status: &WaitStatus,
    cancellation: &Cancellation,
    read: impl FnMut() -> Result<T>,
) -> Result<T> {
    read_while_busy(clock, status, cancellation, read, super::is_busy_error)
}

/// [`read_when_unlocked`] for a read of one copy: a read error that is the
/// busy error waits as that error does.
pub(crate) fn read_copy_when_unlocked<T>(
    clock: &dyn crate::clock::Clock,
    status: &WaitStatus,
    cancellation: &Cancellation,
    read: impl FnMut() -> CopyRead<T>,
) -> CopyRead<T> {
    read_while_busy(
        clock,
        status,
        cancellation,
        read,
        |miss| matches!(miss, CopyMiss::ReadError(error) if super::is_busy_error(error)),
    )
}

/// `read`'s result once it no longer fails with an error `is_busy` names, as
/// [`read_when_unlocked`] waits.
fn read_while_busy<T, E>(
    clock: &dyn crate::clock::Clock,
    status: &WaitStatus,
    cancellation: &Cancellation,
    mut read: impl FnMut() -> std::result::Result<T, E>,
    is_busy: impl Fn(&E) -> bool,
) -> std::result::Result<T, E> {
    // The wait published before this read's, restored once it ends.
    let mut wait_before: Option<Option<OtherRearview>> = None;
    loop {
        match read() {
            Err(error) if is_busy(&error) && !cancellation.is_cancelled() => {
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
    /// End the operation, releasing its refresh ownership. Fails once a
    /// write to the cache found its drive with less than the floor free:
    /// the command reporting the error deletes the cache database (see
    /// [`super::low_space::run_deletion`]).
    pub fn finish(self) -> Result<()> {
        *self.held.lock().unwrap_or_else(PoisonError::into_inner) = Held::Nothing;
        match self.coordinator.base().low_space().error() {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

/// The interval between a read's attempts while the database is
/// exclusively locked.
const UNLOCK_POLL: Duration = Duration::from_millis(200);
