//! A session cache directory as one rearview reaches it.

use crate::clock::Clock;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

/// The refresh owner identities one rearview holds, by canonical cache base.
/// Production makes one per process. A test makes one per rearview it plays,
/// so two rearviews in one test process classify each other's refreshes as
/// another process's.
#[derive(Clone, Debug, Default)]
pub struct ProcessIdentity {
    held: Arc<Mutex<Held>>,
}

#[derive(Debug, Default)]
struct Held {
    /// Each owner identity with the canonical base it owns.
    owners: Vec<(PathBuf, String)>,
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

    /// Record `owner` as this rearview's ownership of `canonical_base`.
    pub(crate) fn hold(&self, canonical_base: &Path, owner: &str) {
        self.held()
            .owners
            .push((canonical_base.to_path_buf(), owner.to_owned()));
    }

    /// Remove `owner` from the identities this rearview holds.
    pub(crate) fn forget(&self, owner: &str) {
        self.held().owners.retain(|(_, held)| held != owner);
    }

    /// True when `owner` is an identity this rearview holds.
    pub(crate) fn holds(&self, owner: &str) -> bool {
        self.held().owners.iter().any(|(_, held)| held == owner)
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
