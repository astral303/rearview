//! The cache files of releases before the session cache database: a folder
//! per storage under the cache base, holding a folder per session root. A
//! load removes a root's folder once the database holds that root.

use super::provider::SessionCache;
use std::hash::{Hash, Hasher};
use std::path::Path;

/// Remove the cache files earlier releases kept under `base` for `storage`'s
/// root at the canonical path `canonical_root`, then `storage`'s folder once
/// it is empty. A failure leaves the files for the next load to remove.
pub(crate) fn remove(base: &Path, storage: SessionCache, canonical_root: &Path) {
    let storage_directory = base.join(storage.directory);
    let _ = std::fs::remove_dir_all(storage_directory.join(root_directory_name(canonical_root)));
    let _ = std::fs::remove_dir(&storage_directory);
}

/// The folder name those releases gave the root at `canonical_root`: a hash
/// of that path. `DefaultHasher::new()` computes it with fixed keys.
fn root_directory_name(canonical_root: &Path) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    canonical_root.hash(&mut hasher);
    format!("root-{:016x}", hasher.finish())
}
