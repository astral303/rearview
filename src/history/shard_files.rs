//! The cache files releases before the session cache database kept: a
//! folder per storage under the cache base, holding a folder per session
//! root. A load removes a root's folder once the database holds that root.

use super::provider::SessionCache;
use std::hash::{Hash, Hasher};
use std::path::Path;

/// Remove the cache files releases before the database kept for `root` of
/// `storage` under `base`, then `storage`'s folder once it is empty. A
/// failure leaves the files for the next load to remove.
pub(crate) fn remove(base: &Path, storage: SessionCache, root: &Path) {
    let storage_directory = base.join(storage.directory);
    let _ = std::fs::remove_dir_all(storage_directory.join(root_directory_name(root)));
    let _ = std::fs::remove_dir(&storage_directory);
}

/// The folder name those releases gave `root`: a hash of its canonical
/// path, which `DefaultHasher::new()` computes with fixed keys.
fn root_directory_name(root: &Path) -> String {
    let resolved = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    resolved.hash(&mut hasher);
    format!("root-{:016x}", hasher.finish())
}
