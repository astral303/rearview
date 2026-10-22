//! Refresh batches hold SQLite's writer lock through their work and timestamp
//! their commits. A recent unfinished refresh keeps ownership between batches.

use super::base::CacheBase;
use super::connection::{self, Access};
use super::coordinator::WaitStatus;
use super::is_busy;
use super::observer::OtherRearview;
use crate::clock::{Clock, Moment};
use crate::error::{AppError, Result};
use rusqlite::{Connection, Transaction, TransactionBehavior, params};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

const STALE_AFTER: Duration = Duration::from_secs(15);
const POLL_EVERY: Duration = Duration::from_millis(200);

/// The longest SQLite's writer lock may stay busy before an owner's write
/// batch fails and revokes its write permission. It bounds the wait behind
/// another connection stalled inside a batch. Waits for the same owner's
/// other threads do not count, because they take turns through the owner's
/// [`RefreshToken`] before taking the lock.
const LOCK_WAIT_LIMIT: Duration = Duration::from_secs(30);

/// The process whose refresh owns a session cache.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RefreshOwner {
    ThisProcess,
    AnotherProcess,
}

impl RefreshOwner {
    /// The process `identity`, a stored owner identity of `base`'s cache,
    /// belongs to: this process's when this rearview holds it, another's
    /// otherwise. Every classification of a refresh's process goes through
    /// here.
    pub(super) fn of(base: &CacheBase, identity: &str) -> Self {
        if base.process().holds(identity) {
            Self::ThisProcess
        } else {
            Self::AnotherProcess
        }
    }
}

/// A refresh that owns a cache now, as one attempt at ownership found it.
#[derive(Debug)]
enum ActiveOwner {
    /// The owner identity `refresh_state.owner` stores.
    Stored(String),
    /// Another connection holds the writer lock, and the stored owner could
    /// not be read: an exclusive rebuild blocks readers, and a waiter holding
    /// the lock may not have stored its identity yet.
    Unreadable,
}

impl ActiveOwner {
    /// The other rearview a waiter blocked by this owner waits for; `None`
    /// for this process's own refresh.
    fn other_rearview(&self, base: &CacheBase) -> Option<OtherRearview> {
        match self {
            Self::Stored(identity) => match RefreshOwner::of(base, identity) {
                RefreshOwner::ThisProcess => None,
                RefreshOwner::AnotherProcess => Some(OtherRearview::Refreshing),
            },
            Self::Unreadable => Some(OtherRearview::Rebuilding),
        }
    }
}

/// The outcome of one attempt at refresh ownership that does not wait.
#[cfg(test)]
pub(crate) enum Acquisition {
    Acquired(RefreshOwnership),
    /// A refresh owns the cache now.
    HeldBy(RefreshOwner),
    /// Another connection holds the writer lock, and the owner cannot be
    /// read.
    Locked,
}

// NULL owner means completed; otherwise last_commit_ms starts the takeover
// window. This table survives content-schema rebuilds.
const CREATE: &str = "CREATE TABLE IF NOT EXISTS refresh_state (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    owner TEXT,
    last_commit_ms INTEGER NOT NULL
);
INSERT OR IGNORE INTO refresh_state VALUES (1, NULL, 0);";

/// This process's refresh ownership of a cache base, released on drop.
pub(crate) struct RefreshOwnership {
    token: RefreshToken,
}

impl RefreshOwnership {
    /// Wait for refresh ownership of `base` for as long as another refresh
    /// keeps it, taking over one without a commit for [`STALE_AFTER`].
    /// `status` names what each blocked attempt waits for. Fails only on a
    /// database error other than a busy lock. Never on the TUI's drawing
    /// thread.
    pub(super) fn acquire(base: &CacheBase, status: &WaitStatus) -> Result<Self> {
        loop {
            if let Some(ownership) = Self::acquire_within(base, Duration::MAX, status)? {
                return Ok(ownership);
            }
        }
    }

    /// [`Self::acquire`], giving up after `limit` on `base`'s clock: `None`
    /// while another refresh still owns the cache then.
    pub(super) fn acquire_within(
        base: &CacheBase,
        limit: Duration,
        status: &WaitStatus,
    ) -> Result<Option<Self>> {
        super::interactive::refuse_the_drawing_thread("acquire session cache refresh ownership");
        let base = base.canonical()?;
        let clock = base.clock().as_ref();
        let started = clock.now();
        let mut observed = None;
        loop {
            let owner = match try_acquire(&base, STALE_AFTER, &mut observed)? {
                Attempt::Acquired(token) => return Ok(Some(Self::register(token))),
                Attempt::Owned(identity) => ActiveOwner::Stored(identity),
                Attempt::WriterBusy(owner) => owner,
            };
            status.set(owner.other_rearview(&base));
            if clock.elapsed_since(started) >= limit {
                return Ok(None);
            }
            clock.sleep(POLL_EVERY);
        }
    }

    /// A wait for refresh ownership of `base` that a test plays as another
    /// rearview would, publishing nowhere.
    #[cfg(test)]
    pub(crate) fn acquire_for_test(base: &CacheBase) -> Result<Self> {
        Self::acquire(base, &WaitStatus::default())
    }

    /// Refresh ownership of `base` when no refresh owns it now; otherwise the
    /// process whose refresh does. Never waits.
    #[cfg(test)]
    pub(crate) fn try_acquire_now(base: &CacheBase) -> Result<Acquisition> {
        let base = base.canonical()?;
        Ok(match try_acquire(&base, STALE_AFTER, &mut None)? {
            Attempt::Acquired(token) => Acquisition::Acquired(Self::register(token)),
            Attempt::Owned(identity) | Attempt::WriterBusy(ActiveOwner::Stored(identity)) => {
                Acquisition::HeldBy(RefreshOwner::of(&base, &identity))
            }
            Attempt::WriterBusy(ActiveOwner::Unreadable) => Acquisition::Locked,
        })
    }

    /// `token`'s ownership, added to the identities its rearview holds until
    /// dropped.
    fn register(token: RefreshToken) -> Self {
        token
            .base
            .process()
            .hold(token.base.path(), &token.identity);
        Self { token }
    }

    pub(crate) fn token(&self) -> &RefreshToken {
        &self.token
    }
}

impl Drop for RefreshOwnership {
    /// Retry a busy release for up to [`STALE_AFTER`]: ownership left behind
    /// keeps every waiter out for that long, this process included.
    fn drop(&mut self) {
        if let Ok(connection) = connect(self.token.base.path(), STALE_AFTER) {
            let _ = connection.execute(
                "UPDATE refresh_state SET owner = NULL, last_commit_ms = ?1
                 WHERE singleton = 1 AND owner = ?2",
                params![now_ms(self.token.clock()), self.token.identity],
            );
        }
        self.token.forget();
    }
}

#[derive(Clone)]
pub(crate) struct RefreshToken {
    /// The canonical cache base this owner owns, as its rearview reaches it.
    base: CacheBase,
    identity: String,
    writable: Arc<AtomicBool>,
    /// The free bytes a write found below [`FREE_SPACE_FLOOR`]; `None` while
    /// no write has.
    out_of_space: Arc<Mutex<Option<u64>>>,
    /// Taken before SQLite's writer lock by each write batch for this owner,
    /// and released after its commit or rollback, so this owner's threads
    /// wait for each other without a time limit.
    turn: Arc<Mutex<()>>,
}

/// The least free space a write leaves on the cache's drive: 1 GB.
pub(crate) const FREE_SPACE_FLOOR: u64 = 1_000_000_000;

impl RefreshToken {
    /// `identity`'s ownership of `base`, a canonical cache base.
    fn new(base: &CacheBase, identity: String) -> Self {
        Self {
            base: base.clone(),
            identity,
            writable: Arc::new(AtomicBool::new(true)),
            out_of_space: Arc::new(Mutex::new(None)),
            turn: Arc::new(Mutex::new(())),
        }
    }

    /// This owner's turn: its other threads' write batches wait for it.
    pub(super) fn take_turn(&self) -> Turn<'_> {
        Turn {
            _guard: self.turn.lock().unwrap_or_else(PoisonError::into_inner),
        }
    }

    /// This owner's cache base as its rearview reaches it.
    pub(crate) fn cache_base(&self) -> CacheBase {
        self.base.clone()
    }

    fn clock(&self) -> &dyn Clock {
        self.base.clock().as_ref()
    }

    pub(crate) fn can_publish(&self) -> bool {
        self.writable.load(Ordering::Acquire)
    }

    /// Fail a write when the cache's drive has less than
    /// [`FREE_SPACE_FLOOR`] free: record the free bytes and revoke write
    /// permission for every copy of this token, so no write follows. A drive
    /// whose free space the system does not report passes.
    pub(crate) fn check_space(&self) -> Result<()> {
        let Some(free_bytes) = self
            .base
            .disk()
            .available_bytes(self.base.path())
            .filter(|&free| free < FREE_SPACE_FLOOR)
        else {
            return Ok(());
        };
        *self
            .out_of_space
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(free_bytes);
        self.revoke();
        Err(AppError::SessionCacheOutOfSpace {
            free_bytes,
            floor_bytes: FREE_SPACE_FLOOR,
        })
    }

    /// The free bytes a write of this owner found below the floor; `None`
    /// while no write has.
    pub(crate) fn out_of_space(&self) -> Option<u64> {
        *self
            .out_of_space
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Revoke write permission for every copy of this token. The identity
    /// stops counting as this process's at once, because the stored owner
    /// may already be another process's.
    pub(super) fn revoke(&self) {
        self.writable.store(false, Ordering::Release);
        self.forget();
    }

    /// Remove this identity from the ones its rearview holds.
    fn forget(&self) {
        self.base.process().forget(&self.identity);
    }

    /// Call inside the batch's immediate transaction, after taking its lock.
    pub(super) fn verify(&self, connection: &Connection) -> Result<()> {
        if !self.can_publish() {
            return Err(AppError::SessionCacheRefreshLost);
        }
        let current: bool = connection.query_row(
            "SELECT EXISTS (SELECT 1 FROM refresh_state WHERE singleton = 1 AND owner = ?1)",
            [&self.identity],
            |row| row.get(0),
        )?;
        if !current {
            return Err(AppError::SessionCacheRefreshLost);
        }
        Ok(())
    }

    /// Run `work` as one write batch: holding this owner's turn and SQLite's
    /// writer lock, then committing with the batch's end as
    /// `last_commit_ms`. Put reads and parsing inside `work` too: elapsed
    /// wall time cannot permit a takeover while the batch holds the writer
    /// lock. A failure revokes write permission for every copy of this
    /// token.
    pub(crate) fn write_batch<T>(&self, work: impl FnOnce() -> T) -> Result<T> {
        let result = (|| {
            if !self.can_publish() {
                return Err(AppError::SessionCacheRefreshLost);
            }
            let connection = connect(self.base.path(), POLL_EVERY)?;
            let batch = transaction(&connection, self)?;
            let result = work();
            commit(batch, self)?;
            Ok(result)
        })();
        if result.is_err() {
            self.revoke();
        }
        result
    }

    /// Run `read` holding the writer lock, and return its result even when
    /// cache coordination fails. A read that could not start under the lock
    /// runs without it. Failure revokes write permission for every copy of
    /// this token, including the one [`apply_writes`](super::apply_writes)
    /// holds.
    pub(crate) fn read<T>(&self, read: impl FnOnce() -> T) -> (T, Result<()>) {
        let mut read = Some(read);
        let mut result = None;
        let coordinated = self.write_batch(|| result = Some(read.take().unwrap()()));
        (
            result.unwrap_or_else(|| read.take().unwrap()()),
            coordinated,
        )
    }
}

/// `clock`'s wall time in epoch milliseconds, as `last_commit_ms` stores it.
fn now_ms(clock: &dyn Clock) -> i64 {
    clock.wall().timestamp_millis()
}

/// The outcome of one attempt to acquire refresh ownership.
enum Attempt {
    Acquired(RefreshToken),
    /// The refresh of this stored owner identity committed recently. The
    /// attempt held the writer lock.
    Owned(String),
    /// Another connection holds the writer lock.
    WriterBusy(ActiveOwner),
}

/// An owner's identity and last commit as one waiter saw them, and when it
/// first saw that pair on the waiter's clock.
struct ObservedRefresh {
    owner: String,
    last_commit_ms: i64,
    since: Moment,
}

/// One attempt at refresh ownership of `base`, a canonical cache base.
fn try_acquire(
    base: &CacheBase,
    stale_after: Duration,
    observed: &mut Option<ObservedRefresh>,
) -> Result<Attempt> {
    let clock = base.clock().as_ref();
    let attempt = (|| {
        let connection = connect_waiter(base.path())?;
        let Some(transaction) = begin_without_waiting(&connection)? else {
            return Ok(Attempt::WriterBusy(owner_behind_writer_lock(&connection)));
        };
        if !has_refresh_state(&transaction)? {
            transaction.execute_batch(CREATE)?;
        }
        if let Some(identity) = recent_owner(&transaction, stale_after, observed, clock)? {
            return Ok(Attempt::Owned(identity));
        }
        let identity: String =
            transaction.query_row("SELECT hex(randomblob(16))", [], |row| row.get(0))?;
        transaction.execute(
            "UPDATE refresh_state SET owner = ?1, last_commit_ms = ?2 WHERE singleton = 1",
            params![identity, now_ms(clock)],
        )?;
        transaction.commit()?;
        Ok(Attempt::Acquired(RefreshToken::new(base, identity)))
    })();
    match attempt {
        Err(AppError::SessionCache(error)) if is_busy(&error) => {
            Ok(Attempt::WriterBusy(ActiveOwner::Unreadable))
        }
        attempt => attempt,
    }
}

/// A waiter's connection, without a busy timeout. Close it before polling
/// again: an open WAL connection keeps an owner's exclusive rebuild waiting at
/// `BEGIN EXCLUSIVE`.
fn connect_waiter(base: &Path) -> Result<Connection> {
    let connection = connection::open(&super::database_path(base), Access::Create)?;
    connection.busy_timeout(Duration::ZERO)?;
    connection.pragma_update(None, "synchronous", "NORMAL")?;
    Ok(connection)
}

/// The writer lock, or `None` while another connection holds it.
fn begin_without_waiting(connection: &Connection) -> Result<Option<Transaction<'_>>> {
    match Transaction::new_unchecked(connection, TransactionBehavior::Immediate) {
        Err(error) if is_busy(&error) => Ok(None),
        transaction => Ok(Some(transaction?)),
    }
}

/// The identity of the refresh that owns the cache and is not stale; `None`
/// while no refresh does. Call holding the writer lock, so an owner inside a
/// long batch is never judged stale. Ownership goes stale 15 seconds after
/// its last commit by `clock`'s wall time, or after this waiter has seen the
/// same owner and commit for 15 seconds by `clock`'s monotonic time, so a
/// clock rollback cannot extend it.
fn recent_owner(
    transaction: &Transaction<'_>,
    stale_after: Duration,
    observed: &mut Option<ObservedRefresh>,
    clock: &dyn Clock,
) -> Result<Option<String>> {
    if !has_refresh_state(transaction)? {
        return Ok(None);
    }
    let (owner, last_commit_ms): (Option<String>, i64) = transaction.query_row(
        "SELECT owner, last_commit_ms FROM refresh_state WHERE singleton = 1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let Some(owner) = owner else {
        return Ok(None);
    };
    if observed
        .as_ref()
        .is_none_or(|previous| previous.owner != owner || previous.last_commit_ms != last_commit_ms)
    {
        *observed = Some(ObservedRefresh {
            owner: owner.clone(),
            last_commit_ms,
            since: clock.now(),
        });
    }
    let unchanged_for = observed.as_ref().map_or(Duration::ZERO, |observed| {
        clock.elapsed_since(observed.since)
    });
    let is_recent = now_ms(clock).saturating_sub(last_commit_ms) < stale_after.as_millis() as i64
        && unchanged_for < stale_after;
    Ok(is_recent.then_some(owner))
}

/// The refresh holding the writer lock another connection holds, as the
/// stored owner names it. In WAL mode the read does not need that lock.
fn owner_behind_writer_lock(connection: &Connection) -> ActiveOwner {
    let stored: rusqlite::Result<Option<String>> = connection.query_row(
        "SELECT owner FROM refresh_state WHERE singleton = 1",
        [],
        |row| row.get(0),
    );
    match stored {
        Ok(Some(identity)) => ActiveOwner::Stored(identity),
        Ok(None) | Err(_) => ActiveOwner::Unreadable,
    }
}

/// True once an acquisition has created the refresh state table.
pub(super) fn has_refresh_state(connection: &Connection) -> Result<bool> {
    Ok(connection.query_row(
        "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'refresh_state')",
        [],
        |row| row.get(0),
    )?)
}

/// Retry `operation` while the database is busy, for up to `limit` of
/// `clock`'s time.
fn retry_busy<T>(
    limit: Duration,
    clock: &dyn Clock,
    mut operation: impl FnMut() -> rusqlite::Result<T>,
) -> Result<T> {
    let started = clock.now();
    loop {
        match operation() {
            Err(error) if is_busy(&error) && clock.elapsed_since(started) < limit => {
                clock.sleep(POLL_EVERY);
            }
            result => return Ok(result?),
        }
    }
}

fn connect(base: &Path, timeout: Duration) -> Result<Connection> {
    let connection = connection::open(&super::database_path(base), Access::ReadWrite)?;
    connection.busy_timeout(timeout)?;
    connection.pragma_update(None, "synchronous", "NORMAL")?;
    Ok(connection)
}

/// One refresh owner's turn to write: held through a write batch, or through
/// a rebuild's batches and `VACUUM`.
pub(super) struct Turn<'a> {
    _guard: MutexGuard<'a, ()>,
}

/// A write transaction that holds its owner's turn until it commits or rolls
/// back, or runs inside a turn its caller holds.
pub(super) struct WriteBatch<'a> {
    // Declared before the turn, so a rollback on drop runs before the next
    // batch of the same owner can begin.
    transaction: Transaction<'a>,
    _turn: Option<Turn<'a>>,
}

impl<'a> std::ops::Deref for WriteBatch<'a> {
    type Target = Transaction<'a>;

    fn deref(&self) -> &Transaction<'a> {
        &self.transaction
    }
}

/// A write batch. Lock order: `owner`'s turn, then SQLite's writer lock,
/// then a check that `owner` still owns the refresh. Checking before taking
/// the writer lock would let another process take over between the check
/// and the batch. A failure revokes `owner`'s write permission.
pub(super) fn transaction<'a>(
    connection: &'a Connection,
    owner: &'a RefreshToken,
) -> Result<WriteBatch<'a>> {
    transaction_within(connection, owner, LOCK_WAIT_LIMIT)
}

/// [`transaction`], failing once the writer lock stays busy for
/// `lock_wait_limit` after `owner`'s turn is taken.
fn transaction_within<'a>(
    connection: &'a Connection,
    owner: &'a RefreshToken,
    lock_wait_limit: Duration,
) -> Result<WriteBatch<'a>> {
    let turn = owner.take_turn();
    let mut batch = transaction_during(connection, owner, &turn, lock_wait_limit)?;
    batch._turn = Some(turn);
    Ok(batch)
}

/// [`transaction`] inside `turn`, `owner`'s turn the caller holds.
pub(super) fn transaction_in_turn<'a>(
    connection: &'a Connection,
    owner: &'a RefreshToken,
    turn: &Turn<'_>,
) -> Result<WriteBatch<'a>> {
    transaction_during(connection, owner, turn, LOCK_WAIT_LIMIT)
}

fn transaction_during<'a>(
    connection: &'a Connection,
    owner: &'a RefreshToken,
    _turn: &Turn<'_>,
    lock_wait_limit: Duration,
) -> Result<WriteBatch<'a>> {
    let transaction = retry_busy(lock_wait_limit, owner.clock(), || {
        Transaction::new_unchecked(connection, TransactionBehavior::Immediate)
    })
    .inspect_err(|_| owner.revoke())?;
    owner.verify(&transaction).inspect_err(|_| owner.revoke())?;
    Ok(WriteBatch {
        transaction,
        _turn: None,
    })
}

/// A rebuild's write batch: SQLite's exclusive lock, taken inside `_turn`,
/// the owner's turn the rebuild holds.
pub(super) fn exclusive_transaction<'a>(
    connection: &'a Connection,
    _turn: &Turn<'_>,
) -> Result<WriteBatch<'a>> {
    let transaction = Transaction::new_unchecked(connection, TransactionBehavior::Exclusive)?;
    Ok(WriteBatch {
        transaction,
        _turn: None,
    })
}

/// Commit `batch` with `owner`'s commit time. A failure revokes `owner`'s
/// write permission.
pub(super) fn commit(batch: WriteBatch<'_>, owner: &RefreshToken) -> Result<()> {
    owner.verify(&batch).inspect_err(|_| owner.revoke())?;
    let changed = batch
        .execute(
            "UPDATE refresh_state SET last_commit_ms = ?1 WHERE singleton = 1 AND owner = ?2",
            params![now_ms(owner.clock()), owner.identity],
        )
        .inspect_err(|_| owner.revoke())?;
    if changed != 1 {
        owner.revoke();
        return Err(AppError::SessionCacheRefreshLost);
    }
    // Revoke before rollback releases the writer lock to queued writes.
    batch
        .execute_batch("COMMIT")
        .inspect_err(|_| owner.revoke())?;
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests;
