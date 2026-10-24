//! A session cache directory as one rearview reaches it.

use crate::clock::Clock;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

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
    /// The write permission every copy of the owner's token shares.
    writable: Arc<AtomicBool>,
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
    /// the write permission its tokens share.
    pub(crate) fn hold(&self, canonical_base: &Path, owner: &str, writable: Arc<AtomicBool>) {
        self.held().owners.push(HeldOwner {
            canonical_base: canonical_base.to_path_buf(),
            identity: owner.to_owned(),
            writable,
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
    /// `canonical_base`, and return their identities for the caller to
    /// release. They stay held until the caller forgets them.
    pub(crate) fn revoke_every_owner_of(&self, canonical_base: &Path) -> Vec<String> {
        self.held()
            .owners
            .iter()
            .filter(|owner| owner.canonical_base == canonical_base)
            .map(|owner| {
                owner.writable.store(false, Ordering::Release);
                owner.identity.clone()
            })
            .collect()
    }
}

/// The session cache directory as one rearview reaches it: with that
/// rearview's [`ProcessIdentity`], the clock its waits and commit times run
/// on, and the free space its writes check.
#[derive(Clone)]
pub struct CacheBase {
    path: PathBuf,
    process: ProcessIdentity,
    clock: Arc<dyn Clock>,
    disk: Arc<dyn DiskSpace>,
}

impl CacheBase {
    /// The base at `path`, its writes checking the system's free space.
    pub fn new(path: impl Into<PathBuf>, process: ProcessIdentity, clock: Arc<dyn Clock>) -> Self {
        Self {
            path: path.into(),
            process,
            clock,
            disk: Arc::new(SystemDiskSpace),
        }
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
            path: std::fs::canonicalize(&self.path)?,
            ..self.clone()
        })
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
