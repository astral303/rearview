//! A session cache directory as one rearview reaches it.

use crate::clock::Clock;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

/// The refresh owner identities one rearview holds, by canonical cache base,
/// and the cache bases whose first build it refused. Production makes one
/// per process. A test makes one per rearview it plays, so two rearviews in
/// one test process classify each other's refreshes as another process's.
#[derive(Clone, Debug, Default)]
pub struct ProcessIdentity {
    held: Arc<Mutex<Held>>,
}

#[derive(Debug, Default)]
struct Held {
    /// Each owner identity with the canonical base it owns.
    owners: Vec<(PathBuf, String)>,
    refused_bases: Vec<PathBuf>,
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

    /// True when this rearview holds an owner identity of `canonical_base`.
    pub(crate) fn holds_any_owner_of(&self, canonical_base: &Path) -> bool {
        self.held()
            .owners
            .iter()
            .any(|(base, _)| base == canonical_base)
    }

    /// Record that this rearview refused the first build of `base`.
    pub(crate) fn refuse(&self, base: &Path) {
        self.held().refused_bases.push(base.to_path_buf());
    }

    /// True when this rearview refused the first build of `base`.
    pub(crate) fn has_refused(&self, base: &Path) -> bool {
        self.held()
            .refused_bases
            .iter()
            .any(|refused| refused == base)
    }
}

/// The session cache directory as one rearview reaches it: with that
/// rearview's [`ProcessIdentity`], and the clock its waits and commit times
/// run on.
#[derive(Clone)]
pub struct CacheBase {
    path: PathBuf,
    process: ProcessIdentity,
    clock: Arc<dyn Clock>,
}

impl CacheBase {
    pub fn new(path: impl Into<PathBuf>, process: ProcessIdentity, clock: Arc<dyn Clock>) -> Self {
        Self {
            path: path.into(),
            process,
            clock,
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

    /// This base with its directory created and its path canonical, as
    /// refresh ownership records it.
    pub(crate) fn canonical(&self) -> crate::error::Result<Self> {
        std::fs::create_dir_all(&self.path)?;
        Ok(Self {
            path: std::fs::canonicalize(&self.path)?,
            ..self.clone()
        })
    }

    /// True when this rearview refused the first build of this base.
    pub(crate) fn is_refused(&self) -> bool {
        self.process.has_refused(&self.path)
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
