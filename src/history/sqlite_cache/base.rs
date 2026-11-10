//! A session cache directory as one rearview reaches it.

use crate::clock::Clock;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

/// The refresh owner identities one rearview holds, by canonical cache base,
/// and the bases it shut down. Production makes one per process. A test
/// makes one per rearview it plays, so two rearviews in one test process
/// classify each other's refreshes as another process's.
#[derive(Clone, Debug, Default)]
pub struct ProcessIdentity {
    held: Arc<Mutex<Held>>,
}

#[derive(Debug, Default)]
struct Held {
    owners: Vec<HeldOwner>,
    /// Canonical bases this rearview shut down: it acquires no ownership of
    /// them again.
    shut_down: Vec<PathBuf>,
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

    /// Revoke the write permission of every ownership this rearview holds of
    /// `canonical_base`, forget them, and refuse it any later ownership of
    /// that base. Returns the identities revoked, for the caller to release.
    pub(crate) fn shut_down(&self, canonical_base: &Path) -> Vec<String> {
        let mut held = self.held();
        held.shut_down.push(canonical_base.to_path_buf());
        let (revoked, kept) = std::mem::take(&mut held.owners)
            .into_iter()
            .partition::<Vec<_>, _>(|owner| owner.canonical_base == canonical_base);
        held.owners = kept;
        revoked
            .into_iter()
            .map(|owner| {
                owner.writable.store(false, Ordering::Release);
                owner.identity
            })
            .collect()
    }

    /// True once this rearview shut `canonical_base` down.
    pub(crate) fn is_shut_down(&self, canonical_base: &Path) -> bool {
        self.held()
            .shut_down
            .iter()
            .any(|base| base == canonical_base)
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
        Self {
            disk: Arc::new(FixedFreeSpace(free_bytes)),
            ..self
        }
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
