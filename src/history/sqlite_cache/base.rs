//! A session cache directory as one rearview reaches it.

use super::low_space;
use super::refresh::WriteAccess;
use crate::clock::Clock;
use crate::error::AppError;
use crate::history::provider::walk::Canonicaliser;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

/// The refresh owner identities one rearview holds, by canonical cache base,
/// from acquisition until release or a confirmed replacement, write
/// permission or not. Production makes one per process. A test makes one
/// per rearview it plays, so two rearviews in one test process classify
/// each other's refreshes as another process's.
#[derive(Clone, Debug, Default)]
pub struct ProcessIdentity {
    held: Arc<Mutex<Held>>,
}

#[derive(Debug, Default)]
struct Held {
    owners: Vec<HeldOwner>,
    /// Canonical bases this rearview's own rebuild holds exclusively locked.
    rebuilding: Vec<PathBuf>,
}

/// The state a [`CacheBase`] shares with its clones: the base's low-space
/// latch and the connections this rearview has open through it. Each
/// [`CacheBase::new`] makes its own, keeping apart two bases a test makes in
/// one process.
#[derive(Default)]
struct BaseState {
    low_space: low_space::Latch,
    connections: Mutex<Connections>,
    /// Notified whenever a connection of [`Self::connections`] closes.
    closed: Condvar,
}

/// This rearview's open connections to a base, as [`CacheBase::open_connection`]
/// counts them. Two kinds of connection aren't counted, each opened and
/// closed within one call:
/// - refresh ownership's own connections, the deletion's among them
/// - the observer's read
///
/// The low-space deletion doesn't wait for them, so a thread holding one
/// doesn't wait for itself.
#[derive(Default)]
struct Connections {
    open: usize,
    /// True while the low-space deletion refuses new connections. The
    /// deletion's own connections are not counted.
    refusing: bool,
    /// The thread holding each of the `open` connections, by its serial:
    /// the thread that opened it, or the last one that used it (see
    /// [`OpenConnection::held_on_this_thread`]).
    #[cfg(debug_assertions)]
    holders: std::collections::HashMap<u64, std::thread::ThreadId>,
    #[cfg(debug_assertions)]
    next_serial: u64,
}

impl BaseState {
    fn connections(&self) -> MutexGuard<'_, Connections> {
        self.connections
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

/// One session cache connection this rearview has open, counted until it
/// drops.
pub(crate) struct OpenConnection {
    state: Arc<BaseState>,
    #[cfg(debug_assertions)]
    serial: u64,
    #[cfg(debug_assertions)]
    holder: std::cell::Cell<std::thread::ThreadId>,
}

impl OpenConnection {
    /// Record this thread as the connection's holder. A connection moved to
    /// another thread is recorded there at its first use: until then, the
    /// thread it left still holds it for the low-space deletion's debug
    /// check.
    #[cfg(debug_assertions)]
    pub(super) fn held_on_this_thread(&self) {
        let current = std::thread::current().id();
        if self.holder.replace(current) != current {
            self.state
                .connections()
                .holders
                .insert(self.serial, current);
        }
    }
}

impl Drop for OpenConnection {
    fn drop(&mut self) {
        let mut connections = self.state.connections();
        connections.open -= 1;
        #[cfg(debug_assertions)]
        connections.holders.remove(&self.serial);
        drop(connections);
        self.state.closed.notify_all();
    }
}

/// The low-space deletion's refusal of this rearview's new connections to a
/// base, until it drops.
pub(super) struct RefusedConnections {
    state: Arc<BaseState>,
    /// True when every connection open at the refusal closed within its
    /// wait.
    pub(super) all_closed: bool,
}

impl Drop for RefusedConnections {
    fn drop(&mut self) {
        self.state.connections().refusing = false;
    }
}

/// This rearview's rebuild of one cache base, marked from before it takes
/// SQLite's exclusive lock until it drops.
pub(crate) struct RebuildMark {
    process: ProcessIdentity,
    canonical_base: PathBuf,
}

impl Drop for RebuildMark {
    fn drop(&mut self) {
        let mut held = self.process.held();
        if let Some(index) = held
            .rebuilding
            .iter()
            .position(|base| *base == self.canonical_base)
        {
            held.rebuilding.swap_remove(index);
        }
    }
}

/// One refresh ownership this rearview holds.
#[derive(Debug)]
struct HeldOwner {
    canonical_base: PathBuf,
    identity: String,
    access: Arc<WriteAccess>,
}

/// An ownership [`ProcessIdentity::revoke_every_owner_of`] revoked.
pub(crate) struct RevokedOwner {
    pub(crate) identity: String,
    /// True when a write batch or a free-space check held the owner's turn
    /// at the revocation; a write batch may then hold SQLite's writer lock.
    pub(crate) is_turn_held: bool,
}

/// The free space on the drive a path is on, as rearview asks for it.
pub trait DiskSpace: Send + Sync {
    /// The bytes this user may still write on the drive holding `path`;
    /// `None` when the system does not answer.
    fn available_bytes(&self, path: &Path) -> Option<u64>;
}

/// The system's answer for free space.
pub struct SystemDiskSpace;

impl DiskSpace for SystemDiskSpace {
    fn available_bytes(&self, path: &Path) -> Option<u64> {
        super::free_space::available_bytes(path)
    }
}

/// A drive whose free space stays at the bytes it holds.
#[cfg(test)]
struct FixedFreeSpace(u64);

#[cfg(test)]
impl DiskSpace for FixedFreeSpace {
    fn available_bytes(&self, _path: &Path) -> Option<u64> {
        Some(self.0)
    }
}

/// A drive with room for the first `roomy_answers` free-space checks and
/// 0.9 GB free from then on, calling `on_running_out` before each of those
/// later answers.
#[cfg(test)]
pub(crate) struct DriveRunningOut {
    roomy_answers: usize,
    answers: std::sync::atomic::AtomicUsize,
    on_running_out: Box<dyn Fn() + Send + Sync>,
}

#[cfg(test)]
impl DriveRunningOut {
    pub(crate) fn after(
        roomy_answers: usize,
        on_running_out: impl Fn() + Send + Sync + 'static,
    ) -> Self {
        Self {
            roomy_answers,
            answers: std::sync::atomic::AtomicUsize::new(0),
            on_running_out: Box::new(on_running_out),
        }
    }

    /// The free-space checks this drive answered.
    pub(crate) fn answers(&self) -> usize {
        self.answers.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// The `roomy_answers` that run the drive out at the first, the middle
    /// and the last of `checks` free-space checks.
    pub(crate) fn check_points(checks: usize) -> Vec<usize> {
        let mut points = vec![0, checks / 2, checks - 1];
        points.dedup();
        points
    }
}

#[cfg(test)]
impl DiskSpace for DriveRunningOut {
    fn available_bytes(&self, _path: &Path) -> Option<u64> {
        let answer = self
            .answers
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if answer < self.roomy_answers {
            return Some(10 * super::refresh::FREE_SPACE_FLOOR);
        }
        (self.on_running_out)();
        Some(900_000_000)
    }
}

impl ProcessIdentity {
    fn held(&self) -> MutexGuard<'_, Held> {
        self.held.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Record `owner` as this rearview's ownership of `canonical_base`, with
    /// the write access its tokens share.
    pub(crate) fn hold(&self, canonical_base: &Path, owner: &str, access: Arc<WriteAccess>) {
        self.held().owners.push(HeldOwner {
            canonical_base: canonical_base.to_path_buf(),
            identity: owner.to_owned(),
            access,
        });
    }

    /// Remove `owner` from the identities this rearview holds.
    pub(crate) fn forget(&self, owner: &str) {
        self.held().owners.retain(|held| held.identity != owner);
    }

    /// True when `owner` is an identity this rearview holds.
    pub(crate) fn holds(&self, owner: &str) -> bool {
        self.held().owners.iter().any(|held| held.identity == owner)
    }

    /// Mark this rearview's rebuild of `canonical_base` until the mark drops.
    pub(crate) fn mark_rebuild(&self, canonical_base: &Path) -> RebuildMark {
        self.held().rebuilding.push(canonical_base.to_path_buf());
        RebuildMark {
            process: self.clone(),
            canonical_base: canonical_base.to_path_buf(),
        }
    }

    /// True while this rearview's own rebuild of `canonical_base` runs.
    pub(crate) fn is_rebuilding(&self, canonical_base: &Path) -> bool {
        self.held()
            .rebuilding
            .iter()
            .any(|base| base == canonical_base)
    }

    /// Revoke the write permission of every ownership this rearview holds of
    /// `canonical_base`, and return them for the caller to release. They
    /// stay held until the caller forgets them.
    pub(crate) fn revoke_every_owner_of(&self, canonical_base: &Path) -> Vec<RevokedOwner> {
        self.held()
            .owners
            .iter()
            .filter(|owner| owner.canonical_base == canonical_base)
            .map(|owner| {
                owner.access.revoke();
                RevokedOwner {
                    identity: owner.identity.clone(),
                    is_turn_held: owner.access.is_turn_held(),
                }
            })
            .collect()
    }
}

/// The session cache directory as one rearview reaches it: with that
/// rearview's [`ProcessIdentity`], the clock its waits and commit times run
/// on, the free space its writes check, and the canonicaliser its reads and
/// writes key session roots and files with.
#[derive(Clone)]
pub struct CacheBase {
    path: PathBuf,
    process: ProcessIdentity,
    clock: Arc<dyn Clock>,
    disk: Arc<dyn DiskSpace>,
    canonicaliser: Arc<dyn Canonicaliser>,
    /// `path` canonical, once it was asked for. Held while it canonicalises,
    /// so concurrent first callers canonicalise it once.
    canonical_path: Arc<Mutex<Option<PathBuf>>>,
    /// The longest each step of the low-space deletion waits. Tests shorten
    /// it with `with_low_space_wait`: the steps wait in real time, and a
    /// deferred deletion would otherwise take 30 s.
    low_space_wait: Duration,
    /// Made by [`Self::new`] and kept by clones and [`Self::canonical`].
    state: Arc<BaseState>,
}

impl CacheBase {
    /// The base at `path`, its writes checking the system's free space, and
    /// its reads and writes keying session roots and files through
    /// `canonicaliser`.
    pub fn new(
        path: impl Into<PathBuf>,
        process: ProcessIdentity,
        clock: Arc<dyn Clock>,
        canonicaliser: Arc<dyn Canonicaliser>,
    ) -> Self {
        Self {
            path: path.into(),
            process,
            clock,
            disk: Arc::new(SystemDiskSpace),
            canonicaliser,
            canonical_path: Arc::default(),
            low_space_wait: super::WAITING_BUSY_TIMEOUT,
            state: Arc::default(),
        }
    }

    /// Count a new connection of this rearview to this base's database until
    /// the result drops. Fails with [`AppError::SessionCacheBeingDeleted`]
    /// while the low-space deletion refuses new connections.
    pub(crate) fn open_connection(&self) -> crate::error::Result<OpenConnection> {
        let mut connections = self.state.connections();
        if connections.refusing {
            return Err(AppError::SessionCacheBeingDeleted);
        }
        connections.open += 1;
        #[cfg(debug_assertions)]
        let (serial, holder) = {
            let serial = connections.next_serial;
            connections.next_serial += 1;
            let holder = std::thread::current().id();
            connections.holders.insert(serial, holder);
            (serial, holder)
        };
        Ok(OpenConnection {
            state: Arc::clone(&self.state),
            #[cfg(debug_assertions)]
            serial,
            #[cfg(debug_assertions)]
            holder: std::cell::Cell::new(holder),
        })
    }

    /// Refuse this rearview's new connections to this base (see
    /// [`Self::open_connection`]) until the result drops, and wait up to
    /// `wait` for the open ones to close. Call on a thread that doesn't hold
    /// any of them: the wait would end only at `wait`.
    pub(super) fn refuse_connections(&self, wait: Duration) -> RefusedConnections {
        let mut connections = self.state.connections();
        #[cfg(debug_assertions)]
        {
            let current = std::thread::current().id();
            assert!(
                !connections
                    .holders
                    .values()
                    .any(|holder| *holder == current),
                "the thread refusing connections doesn't hold any of them open"
            );
        }
        connections.refusing = true;
        let deadline = Instant::now() + wait;
        while connections.open > 0 {
            let Some(left) = deadline.checked_duration_since(Instant::now()) else {
                break;
            };
            connections = self
                .state
                .closed
                .wait_timeout(connections, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        RefusedConnections {
            state: Arc::clone(&self.state),
            all_closed: connections.open == 0,
        }
    }

    /// This base's low-space latch: set once a write of the base found its
    /// drive low on space (see [`low_space::run_deletion`]).
    pub(crate) fn low_space(&self) -> &low_space::Latch {
        &self.state.low_space
    }

    /// This base, each step of its low-space deletion waiting at most
    /// `wait`. Clones made earlier keep their wait.
    #[cfg(test)]
    pub(crate) fn with_low_space_wait(self, wait: Duration) -> Self {
        Self {
            low_space_wait: wait,
            ..self
        }
    }

    /// The longest each step of this base's low-space deletion waits.
    pub(super) fn low_space_wait(&self) -> Duration {
        self.low_space_wait
    }

    /// This base, keying session roots and files through `canonicaliser`.
    #[cfg(test)]
    pub(crate) fn with_canonicaliser(self, canonicaliser: Arc<dyn Canonicaliser>) -> Self {
        Self {
            canonicaliser,
            ..self
        }
    }

    pub(crate) fn canonicaliser(&self) -> &Arc<dyn Canonicaliser> {
        &self.canonicaliser
    }

    /// This base on a drive with `free_bytes` free, whatever its writes add.
    #[cfg(test)]
    pub(crate) fn with_free_space(self, free_bytes: u64) -> Self {
        self.with_disk_space(Arc::new(FixedFreeSpace(free_bytes)))
    }

    /// This base on a drive whose free space `disk` answers.
    #[cfg(test)]
    pub(crate) fn with_disk_space(self, disk: Arc<dyn DiskSpace>) -> Self {
        Self { disk, ..self }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn process(&self) -> &ProcessIdentity {
        &self.process
    }

    pub(crate) fn clock(&self) -> &Arc<dyn Clock> {
        &self.clock
    }

    pub(crate) fn disk(&self) -> &Arc<dyn DiskSpace> {
        &self.disk
    }

    /// This base with its directory created and its path canonical, as
    /// refresh ownership records it.
    pub(crate) fn canonical(&self) -> crate::error::Result<Self> {
        std::fs::create_dir_all(&self.path)?;
        Ok(Self {
            path: self.canonical_path()?,
            ..self.clone()
        })
    }

    /// This base's directory, canonical.
    pub(crate) fn canonical_path(&self) -> std::io::Result<PathBuf> {
        let mut memo = self
            .canonical_path
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(canonical) = memo.as_ref() {
            return Ok(canonical.clone());
        }
        let canonical = self.canonicaliser.canonicalise(&self.path)?;
        *memo = Some(canonical.clone());
        Ok(canonical)
    }
}

impl std::fmt::Debug for CacheBase {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CacheBase")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::TestClock;
    use crate::history::provider::walk::test_support::CountingCanonicaliser;
    use std::sync::Barrier;

    #[test]
    fn concurrent_first_callers_canonicalise_cache_directory_once() {
        let directory = tempfile::tempdir().unwrap();
        let canonicaliser = Arc::new(CountingCanonicaliser::default());
        let base = CacheBase::new(
            directory.path(),
            ProcessIdentity::default(),
            Arc::new(TestClock::default()),
            canonicaliser.clone(),
        );
        let callers = 8;
        let start = Barrier::new(callers);

        let canonical_paths: Vec<PathBuf> = std::thread::scope(|scope| {
            let threads: Vec<_> = (0..callers)
                .map(|_| {
                    let base = base.clone();
                    let start = &start;
                    scope.spawn(move || {
                        start.wait();
                        base.canonical_path().unwrap()
                    })
                })
                .collect();
            threads
                .into_iter()
                .map(|thread| thread.join().unwrap())
                .collect()
        });

        assert_eq!(
            canonicaliser.canonicalised(),
            vec![directory.path().to_path_buf()],
            "the first callers canonicalise the directory once between them"
        );
        assert!(
            canonical_paths
                .iter()
                .all(|path| *path == canonical_paths[0]),
            "every caller gets the one canonical path"
        );
    }
}
