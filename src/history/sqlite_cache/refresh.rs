//! Refresh batches hold SQLite's writer lock through their work and timestamp
//! their commits. A recent unfinished refresh keeps ownership between batches.

use super::base::CacheBase;
use super::connection::{self, Access, CacheConnection};
use super::coordinator::WaitStatus;
use super::is_busy;
use super::observer::{CommitAge, OtherRearview, REBUILD_LOCK_THRESHOLD};
use super::scope::Cancellation;
use crate::clock::{Clock, Moment};
use crate::error::{AppError, CacheDeletion, Result};
use crate::history::path::without_verbatim_prefix;
use rusqlite::{Connection, Transaction, TransactionBehavior, params};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

const STALE_AFTER: Duration = Duration::from_secs(15);
const POLL_EVERY: Duration = Duration::from_millis(200);

/// The longest an owner's write batch waits for its connection to open, and
/// then for SQLite's writer lock, before it fails and revokes its write
/// permission. Opening waits for a connection that holds the database's
/// exclusive lock, as a connection closing last holds it through its
/// checkpoint. The writer lock wait bounds the wait behind another
/// connection stalled inside a batch. Waits for the same owner's other
/// threads do not count, because they take turns through the owner's
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
    /// for this process's own refresh or rebuild. `base` is canonical.
    fn other_rearview(&self, base: &CacheBase) -> Option<OtherRearview> {
        match self {
            Self::Stored(identity) => match RefreshOwner::of(base, identity) {
                RefreshOwner::ThisProcess => None,
                RefreshOwner::AnotherProcess => Some(OtherRearview::Refreshing),
            },
            Self::Unreadable if base.process().is_rebuilding(base.path()) => None,
            Self::Unreadable => Some(OtherRearview::Rebuilding),
        }
    }
}

/// The age of a commit made at `last_commit_ms` against the takeover window,
/// by `base`'s wall clock.
pub(super) fn commit_age(base: &CacheBase, last_commit_ms: i64) -> CommitAge {
    let age_ms = now_ms(base.clock().as_ref()).saturating_sub(last_commit_ms);
    if age_ms < STALE_AFTER.as_millis() as i64 {
        CommitAge::Recent
    } else {
        CommitAge::PastTakeover
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
    /// `status` names what each blocked attempt waits for. Fails on a
    /// database error other than a busy lock, and with
    /// [`AppError::SessionCacheRefreshLost`] once `cancellation` is set.
    /// Never on the TUI's drawing thread.
    pub(super) fn acquire(
        base: &CacheBase,
        status: &WaitStatus,
        cancellation: &Cancellation,
    ) -> Result<Self> {
        loop {
            if let Some(ownership) =
                Self::acquire_within(base, Duration::MAX, status, cancellation)?
            {
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
        cancellation: &Cancellation,
    ) -> Result<Option<Self>> {
        super::interactive::refuse_the_drawing_thread("acquire session cache refresh ownership");
        let base = base.canonical()?;
        let clock = base.clock().as_ref();
        let started = clock.now();
        let mut observed = None;
        loop {
            refuse_once_cancelled(cancellation)?;
            let owner = match try_acquire(&base, STALE_AFTER, &mut observed)? {
                Attempt::Acquired(token) => {
                    let ownership = Self::register(token);
                    // A shutdown that cancelled this wait after the check
                    // above missed this ownership; dropping it releases it.
                    refuse_once_cancelled(cancellation)?;
                    return Ok(Some(ownership));
                }
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
        Self::acquire(base, &WaitStatus::default(), &Cancellation::default())
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
    /// released or replaced.
    fn register(token: RefreshToken) -> Self {
        token.base.process().hold(
            token.base.path(),
            &token.identity,
            Arc::clone(&token.access),
        );
        Self { token }
    }

    pub(crate) fn token(&self) -> &RefreshToken {
        &self.token
    }

    /// Delete the cache database's contents under SQLite's exclusive lock
    /// (see [`take_exclusive_lock`]), waiting as `wait` allows: drop every
    /// table but `refresh_state`, shrink the file and leave WAL mode, then
    /// remove the files SQLite keeps beside it. Keep the database file: a
    /// reader opening it meanwhile is then neither unlinked under nor a
    /// cause of a failed removal. Other rearviews cannot write the emptied
    /// cache until the ownership drops.
    pub(super) fn delete_database(&mut self, wait: ExclusiveWait) -> CacheDeletion {
        let turn = self.token.take_turn();
        let database = super::database_path(self.token.base.path());
        if !database.exists() {
            return remove_side_files(&database);
        }
        let deletion = self
            .empty_database(&turn, &database, wait)
            .unwrap_or_else(|error| deletion_failure(&database, error));
        #[cfg(test)]
        if let Some(after_lock_ends) = AFTER_DELETION_LOCK_ENDS.take() {
            after_lock_ends();
        }
        deletion
    }

    /// Drop every table of `database` but `refresh_state`, shrink its file,
    /// leave WAL mode and remove the files SQLite keeps beside it, under
    /// SQLite's exclusive lock. The connection holds the lock until it
    /// closes on return: once the lock ends, another rearview may own the
    /// cache and write those files again.
    fn empty_database(
        &self,
        turn: &Turn<'_>,
        database: &Path,
        wait: ExclusiveWait,
    ) -> Result<CacheDeletion> {
        let connection = connect_writable(&self.token.base, wait.total)?;
        take_exclusive_lock(&connection, &self.token, turn, wait, |batch| {
            record_commit(&batch, &self.token)?;
            batch.transaction.commit()?;
            Ok(())
        })?;
        // Empty the database before leaving WAL mode: leaving it copies the
        // log into the database only up to the database's size at its last
        // commit, so none of a load's pages in the log grow the file.
        let batch = connection.unchecked_transaction()?;
        super::drop_session_tables(&batch)?;
        batch.commit()?;
        connection.execute_batch("VACUUM")?;
        // Commit the ownership again before the lock ends, so a waiter
        // finds it held after an emptying that outlasts the takeover window,
        // and takes over only once the side files are removed.
        let batch = connection.unchecked_transaction()?;
        record_commit(&batch, &self.token)?;
        batch.commit()?;
        leave_wal(&connection)?;
        // Hold the rollback journal in memory before removing the side
        // files: in exclusive locking mode the connection keeps the journal
        // file open, and Windows refuses to remove an open file. The mode
        // lasts only for this connection.
        connection.query_row("PRAGMA journal_mode = MEMORY", [], |_| Ok(()))?;
        Ok(remove_side_files(database))
    }
}

#[cfg(test)]
thread_local! {
    /// Runs on the deleting thread once a low-space deletion's exclusive
    /// lock ends, for a test of what another rearview may do then.
    pub(super) static AFTER_DELETION_LOCK_ENDS: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::RefCell::new(None) };
}

/// The outcome of a low-space deletion of `database` that failed with
/// `error`: deferred when another connection kept the database busy.
pub(super) fn deletion_failure(database: &Path, error: AppError) -> CacheDeletion {
    if super::is_busy_error(&error) {
        return CacheDeletion::Deferred;
    }
    CacheDeletion::Failed {
        file: without_verbatim_prefix(database).display().to_string(),
        error: error.to_string(),
    }
}

/// Record a commit of `owner`'s ownership now on `connection`, whether or
/// not its write permission stands, so a rearview waiting for the ownership
/// finds it held. Fails with [`AppError::SessionCacheRefreshLost`] once
/// another identity owns the cache.
fn record_commit(connection: &Connection, owner: &RefreshToken) -> Result<()> {
    let changed = connection.execute(
        "UPDATE refresh_state SET last_commit_ms = ?1 WHERE singleton = 1 AND owner = ?2",
        params![now_ms(owner.clock()), owner.identity],
    )?;
    if changed != 1 {
        return Err(AppError::SessionCacheRefreshLost);
    }
    Ok(())
}

/// A wait for SQLite's exclusive lock: `total` in all, in attempts of at
/// most `attempt`. The owner commits again after each attempt that fails.
#[derive(Clone, Copy, Debug)]
pub(super) struct ExclusiveWait {
    pub(super) total: Duration,
    pub(super) attempt: Duration,
}

/// The attempt of an [`ExclusiveWait::of`]. Keep it well under
/// [`STALE_AFTER`]: a rearview waiting for the ownership between the
/// owner's commits must not find it stale.
const EXCLUSIVE_LOCK_ATTEMPT: Duration = Duration::from_secs(3);

impl ExclusiveWait {
    /// A wait of `total` in attempts of [`EXCLUSIVE_LOCK_ATTEMPT`].
    pub(super) const fn of(total: Duration) -> Self {
        Self {
            total,
            attempt: EXCLUSIVE_LOCK_ATTEMPT,
        }
    }

    /// The busy timeout of one attempt.
    fn attempt_timeout(self) -> Duration {
        self.total.min(self.attempt)
    }

    fn attempts(self) -> u128 {
        let attempt = self.attempt_timeout().as_millis().max(1);
        self.total.as_millis().div_ceil(attempt).max(1)
    }
}

/// Take SQLite's exclusive lock on `connection`'s database inside `turn`,
/// and commit its first batch with `commit`; the lock lasts until the
/// connection closes. Waits for every other connection to close as `wait`
/// allows, committing `owner`'s ownership after each attempt that fails.
/// Fails with a busy error once the wait ends, and with
/// [`AppError::SessionCacheRefreshLost`] once another identity owns the
/// cache.
pub(super) fn take_exclusive_lock(
    connection: &Connection,
    owner: &RefreshToken,
    turn: &Turn<'_>,
    wait: ExclusiveWait,
    commit: impl FnOnce(WriteBatch<'_>) -> Result<()>,
) -> Result<()> {
    // With exclusive locking in WAL mode, BEGIN EXCLUSIVE waits up to the
    // busy timeout for every other connection to close, and keeps the lock
    // across commits and VACUUM until this connection closes. A failed
    // attempt keeps this connection's shared lock and doesn't take any
    // other, and the commit between attempts writes through it: in WAL mode
    // the pager asks for the exclusive lock from the shared one, and both
    // the Unix and the Windows VFS take the pending byte only on the way
    // from a reserved lock.
    connection.pragma_update(None, "locking_mode", "EXCLUSIVE")?;
    connection.busy_timeout(wait.attempt_timeout())?;
    let attempts = wait.attempts();
    let mut attempt = 1;
    let batch = loop {
        match exclusive_transaction(connection, turn) {
            Ok(batch) => break batch,
            Err(error) if super::is_busy_error(&error) && attempt < attempts => {
                keep_ownership(owner, wait.attempt)?;
                attempt += 1;
            }
            Err(error) => return Err(error),
        }
    };
    connection.busy_timeout(wait.total)?;
    commit(batch)
}

/// Switch `connection`'s database, held under SQLite's exclusive lock, out
/// of WAL mode, deleting its write-ahead log. Fails with
/// [`AppError::SessionCacheStuckInWal`] when the database keeps another
/// mode.
pub(super) fn leave_wal(connection: &Connection) -> Result<()> {
    // Delete the log on the way out: SQLite opens a `-wal` file it finds
    // beside a database out of WAL mode as its log again.
    connection::set_persistent_wal(connection, false)?;
    let mode: String =
        connection.query_row("PRAGMA journal_mode = DELETE", [], |row| row.get(0))?;
    if !mode.eq_ignore_ascii_case("delete") {
        return Err(AppError::SessionCacheStuckInWal { mode });
    }
    #[cfg(test)]
    if let Some(database) = connection.path() {
        let size = std::fs::metadata(database).map_or(0, |metadata| metadata.len());
        SIZES_AFTER_LEAVING_WAL.with_borrow_mut(|sizes| sizes.push(size));
    }
    Ok(())
}

#[cfg(test)]
thread_local! {
    /// The database file's size each time a connection on this thread left
    /// WAL mode, for a test of what leaving it copies.
    static SIZES_AFTER_LEAVING_WAL: std::cell::RefCell<Vec<u64>> = const {
        std::cell::RefCell::new(Vec::new())
    };
}

/// [`record_commit`] on a connection of its own, waiting up to `wait` for
/// the writer lock.
fn keep_ownership(owner: &RefreshToken, wait: Duration) -> Result<()> {
    let connection = connect_writable(&owner.base, wait)?;
    record_commit(&connection, owner)
}

/// Remove the files SQLite keeps beside `database`.
pub(super) fn remove_side_files(database: &Path) -> CacheDeletion {
    for suffix in super::DATABASE_SIDE_FILE_SUFFIXES {
        let mut path = database.to_path_buf().into_os_string();
        path.push(suffix);
        let path = std::path::PathBuf::from(path);
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return CacheDeletion::Failed {
                    file: without_verbatim_prefix(&path).display().to_string(),
                    error: error.to_string(),
                };
            }
        }
    }
    CacheDeletion::Deleted
}

impl Drop for RefreshOwnership {
    /// Revoke every copy's write permission, and retry a busy release for up
    /// to [`STALE_AFTER`]: ownership left behind keeps every waiter out for
    /// that long, this process included. A release that fails keeps the
    /// identity this process's, so its shutdown can release it.
    fn drop(&mut self) {
        self.token.revoke();
        let released = release(&self.token.base, &self.token.identity, STALE_AFTER);
        if released != Release::Failed {
            self.token.forget();
        }
    }
}

/// The outcome of one release of an owner identity.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Release {
    /// The identity owned the cache, and no longer does.
    Released,
    /// Another identity owns the cache, or none does.
    NotOwner,
    /// The database could not be written: the identity may still own it.
    Failed,
}

/// Clear `identity`'s ownership of `base`, a canonical cache base, when it
/// still owns the cache, retrying a busy writer lock for up to `wait`. A
/// release that fails leaves the ownership to the takeover window. A missing
/// database file has no owner to clear.
pub(super) fn release(base: &CacheBase, identity: &str, wait: Duration) -> Release {
    if !super::database_path(base.path()).is_file() {
        return Release::NotOwner;
    }
    let released = connect_writable(base, wait).and_then(|connection| {
        connection.execute(
            "UPDATE refresh_state SET owner = NULL, last_commit_ms = ?1
             WHERE singleton = 1 AND owner = ?2",
            params![now_ms(base.clock().as_ref()), identity],
        )
    });
    match released {
        Ok(1) => Release::Released,
        Ok(_) => Release::NotOwner,
        Err(_) => Release::Failed,
    }
}

/// An owner's write permission and turn, shared by every copy of its token
/// and by its rearview's record of the ownership.
#[derive(Debug)]
pub(crate) struct WriteAccess {
    writable: AtomicBool,
    /// Taken before SQLite's writer lock by each write batch for this owner,
    /// and released after its commit or rollback, so this owner's threads
    /// wait for each other without a time limit. A free-space check takes it
    /// too.
    turn: Mutex<()>,
}

impl WriteAccess {
    fn new() -> Self {
        Self {
            writable: AtomicBool::new(true),
            turn: Mutex::new(()),
        }
    }

    /// Revoke write permission for every copy of the owner's token.
    pub(crate) fn revoke(&self) {
        self.writable.store(false, Ordering::Release);
    }

    /// True while a write batch or a free-space check holds the owner's
    /// turn: a write batch may then hold SQLite's writer lock.
    pub(crate) fn is_turn_held(&self) -> bool {
        matches!(
            self.turn.try_lock(),
            Err(std::sync::TryLockError::WouldBlock)
        )
    }
}

#[derive(Clone)]
pub(crate) struct RefreshToken {
    /// The canonical cache base this owner owns, as its rearview reaches it.
    base: CacheBase,
    identity: String,
    access: Arc<WriteAccess>,
    /// True once a write found another identity owning the cache.
    replaced: Arc<AtomicBool>,
    /// The thread running a parse of [`Self::parse_batch`] inside the turn.
    /// While it is set, debug builds panic on a take of the turn from that
    /// thread or from any rayon thread: a parse spreads over rayon's pool,
    /// and a take from the parse waits forever behind its own batch.
    #[cfg(debug_assertions)]
    parsing: Arc<Mutex<Option<std::thread::ThreadId>>>,
    /// The threads waiting in [`Self::take_turn`], for tests that order a
    /// step after a thread starts waiting.
    #[cfg(test)]
    waiting_for_turn: Arc<std::sync::atomic::AtomicUsize>,
}

/// The least free space a write leaves on the cache's drive: 1 GB.
pub(crate) const FREE_SPACE_FLOOR: u64 = 1_000_000_000;

impl RefreshToken {
    /// `identity`'s ownership of `base`, a canonical cache base.
    fn new(base: &CacheBase, identity: String) -> Self {
        Self {
            base: base.clone(),
            identity,
            access: Arc::new(WriteAccess::new()),
            replaced: Arc::new(AtomicBool::new(false)),
            #[cfg(debug_assertions)]
            parsing: Arc::default(),
            #[cfg(test)]
            waiting_for_turn: Arc::default(),
        }
    }

    /// This owner's turn: its other threads' write batches wait for it.
    pub(super) fn take_turn(&self) -> Turn<'_> {
        #[cfg(debug_assertions)]
        {
            let parsing = *self.parsing.lock().unwrap_or_else(PoisonError::into_inner);
            assert!(
                parsing.is_none_or(|parser| {
                    parser != std::thread::current().id() && rayon::current_thread_index().is_none()
                }),
                "a parse inside the owner's batch took the owner's turn, which its batch holds"
            );
        }
        #[cfg(test)]
        self.waiting_for_turn.fetch_add(1, Ordering::SeqCst);
        let guard = self
            .access
            .turn
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        #[cfg(test)]
        self.waiting_for_turn.fetch_sub(1, Ordering::SeqCst);
        Turn { _guard: guard }
    }

    /// The threads waiting for this owner's turn.
    #[cfg(test)]
    pub(super) fn threads_waiting_for_turn(&self) -> usize {
        self.waiting_for_turn.load(Ordering::SeqCst)
    }

    /// This owner's cache base as its rearview reaches it.
    pub(crate) fn cache_base(&self) -> CacheBase {
        self.base.clone()
    }

    fn clock(&self) -> &dyn Clock {
        self.base.clock().as_ref()
    }

    pub(crate) fn can_publish(&self) -> bool {
        self.access.writable.load(Ordering::Acquire)
    }

    /// True once a write found another identity owning the cache.
    pub(crate) fn is_replaced(&self) -> bool {
        self.replaced.load(Ordering::Acquire)
    }

    /// Fail a write when the cache's drive has less than
    /// [`FREE_SPACE_FLOOR`] free: set the base's low-space latch and revoke
    /// write permission for every copy of this token, so no write follows.
    /// The command reporting the error deletes the cache database once it
    /// reads the latch. A drive whose free space the system does not report
    /// passes.
    /// Fail without checking once the write permission is revoked, as when
    /// the TUI quits, including while this check waits for the turn; fail
    /// without setting the latch once the TUI's exit gave up waiting for the
    /// check (see [`super::low_space::Latch::wait_for_checks`]). Call
    /// outside this owner's turn: the check takes it, so a parse group of
    /// this owner runs wholly before the check or finds the write permission
    /// revoked.
    pub(crate) fn check_space(&self) -> Result<()> {
        if !self.can_publish() {
            return Err(AppError::SessionCacheRefreshLost);
        }
        let _turn = self.take_turn();
        let Some(check) = self.base.low_space().begin_check(|| self.can_publish()) else {
            return Err(AppError::SessionCacheRefreshLost);
        };
        let Some(free_bytes) = self
            .base
            .disk()
            .available_bytes(self.base.path())
            .filter(|&free| free < FREE_SPACE_FLOOR)
        else {
            return Ok(());
        };
        if !check.publish(free_bytes) {
            return Err(AppError::SessionCacheRefreshLost);
        }
        self.revoke();
        Err(self
            .base
            .low_space()
            .error()
            .expect("the low-space latch was just set"))
    }

    /// True once a write of this owner's base found its drive short of
    /// space.
    pub(crate) fn is_low_space(&self) -> bool {
        self.base.low_space().is_set()
    }

    /// Revoke write permission for every copy of this token. The identity
    /// stays this process's until its release or a confirmed replacement:
    /// the stored owner still names it.
    pub(crate) fn revoke(&self) {
        self.access.revoke();
    }

    /// Revoke write permission, as a write that found another identity
    /// owning the cache does, and stop counting this identity as this
    /// process's.
    fn replaced(&self) {
        self.revoke();
        self.replaced.store(true, Ordering::Release);
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
            self.replaced();
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
            let connection = retry_busy(LOCK_WAIT_LIMIT, self.clock(), || {
                connect_writable(&self.base, POLL_EVERY)
            })?;
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

    /// Run `work` as one write batch, as [`Self::write_batch`] does, and
    /// return what became of it.
    pub(crate) fn batch_returning<T>(&self, work: impl FnOnce() -> T) -> Batch<T> {
        let mut result = None;
        let coordinated = self.write_batch(|| result = Some(work()));
        match (result, coordinated) {
            (Some(result), committed) => Batch::Ran(result, committed),
            (None, Err(error)) => Batch::NotStarted(error),
            (None, Ok(())) => unreachable!("a committed batch ran its work"),
        }
    }

    /// Run `parse` with the permit to parse transcripts as one write batch,
    /// as [`Self::batch_returning`] does. Each parse of this owner then
    /// holds the writer lock: a takeover cannot commit until it ends. The
    /// permit does not outlive `parse`.
    pub(crate) fn parse_batch<T>(
        &self,
        parse: impl for<'p> FnOnce(super::ParsePermit<'p>) -> T,
    ) -> Batch<T> {
        self.batch_returning(|| {
            #[cfg(debug_assertions)]
            let _parsing = Parsing::during(&self.parsing);
            parse(super::permit::ParsePermit::owner())
        })
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

/// The outcome of one write batch of [`RefreshToken::batch_returning`].
#[derive(Debug)]
pub(crate) enum Batch<T> {
    /// The batch could not start with this error; its work did not run.
    NotStarted(AppError),
    /// The work returned this, and the batch committed or failed as the
    /// result reports.
    Ran(T, Result<()>),
}

/// [`RefreshToken::parsing`] naming this thread until this drops.
#[cfg(debug_assertions)]
struct Parsing<'a>(&'a Mutex<Option<std::thread::ThreadId>>);

#[cfg(debug_assertions)]
impl<'a> Parsing<'a> {
    fn during(parsing: &'a Mutex<Option<std::thread::ThreadId>>) -> Self {
        *parsing.lock().unwrap_or_else(PoisonError::into_inner) = Some(std::thread::current().id());
        Self(parsing)
    }
}

#[cfg(debug_assertions)]
impl Drop for Parsing<'_> {
    fn drop(&mut self) {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = None;
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
        let connection = connect_waiter(base)?;
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

/// Fail once `cancellation` is set.
fn refuse_once_cancelled(cancellation: &Cancellation) -> Result<()> {
    if cancellation.is_cancelled() {
        return Err(AppError::SessionCacheRefreshLost);
    }
    Ok(())
}

/// A waiter's connection: opening waits up to [`REBUILD_LOCK_THRESHOLD`] for
/// the database's exclusive lock, and its later statements do not wait. Close it
/// before polling again: an open WAL connection keeps an owner's exclusive
/// rebuild waiting at `BEGIN EXCLUSIVE`.
fn connect_waiter(base: &CacheBase) -> Result<CacheConnection> {
    let connection = connection::open_cache(base, Access::Create, REBUILD_LOCK_THRESHOLD)?;
    connection.pragma_update(None, "synchronous", "NORMAL")?;
    connection.busy_timeout(Duration::ZERO)?;
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

/// A refresh connection to the database in `base` without this rearview's
/// low-space close setting, as another rearview's connection opens.
#[cfg(test)]
fn connect(base: &Path, timeout: Duration) -> rusqlite::Result<Connection> {
    let connection = connection::open(&super::database_path(base), Access::ReadWrite, timeout)?;
    connection.pragma_update(None, "synchronous", "NORMAL")?;
    Ok(connection)
}

/// A refresh connection to `base`'s database, as this rearview's refreshes
/// open it.
fn connect_writable(base: &CacheBase, timeout: Duration) -> rusqlite::Result<CacheConnection> {
    let connection = connection::open_cache(base, Access::ReadWrite, timeout)?;
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
    #[cfg(debug_assertions)]
    _live: LiveBatch,
}

#[cfg(debug_assertions)]
thread_local! {
    /// The write batches open on this thread.
    static LIVE_BATCHES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// One write batch counted in [`LIVE_BATCHES`] until it drops.
#[cfg(debug_assertions)]
struct LiveBatch;

#[cfg(debug_assertions)]
impl LiveBatch {
    fn count() -> Self {
        LIVE_BATCHES.with(|live| live.set(live.get() + 1));
        Self
    }
}

#[cfg(debug_assertions)]
impl Drop for LiveBatch {
    fn drop(&mut self) {
        LIVE_BATCHES.with(|live| live.set(live.get() - 1));
    }
}

/// True while a write batch is open on this thread.
#[cfg(debug_assertions)]
pub(super) fn is_batch_open_on_this_thread() -> bool {
    LIVE_BATCHES.with(|live| live.get() > 0)
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
    // A revoked owner, as after the TUI quits, does not wait for the lock.
    if !owner.can_publish() {
        return Err(AppError::SessionCacheRefreshLost);
    }
    let transaction = retry_busy(lock_wait_limit, owner.clock(), || {
        Transaction::new_unchecked(connection, TransactionBehavior::Immediate)
    })
    .inspect_err(|_| owner.revoke())?;
    owner.verify(&transaction).inspect_err(|_| owner.revoke())?;
    Ok(WriteBatch {
        transaction,
        _turn: None,
        #[cfg(debug_assertions)]
        _live: LiveBatch::count(),
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
        #[cfg(debug_assertions)]
        _live: LiveBatch::count(),
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
        owner.replaced();
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
