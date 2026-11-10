use super::roots::{Places, StoredPath};
use crate::error::{AppError, Result};
use crate::history::fingerprint::CachedFingerprint;
use crate::history::path::path_key;
use rusqlite::Connection;
use std::collections::HashSet;
use std::path::Path;

/// The text two spellings of one session file share: its path below its
/// root, or its canonical path, lowercased on a file system that ignores
/// case.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(super) struct FileKey(String);

impl FileKey {
    /// The key of `path`, already canonical: a path the user named and
    /// rearview canonicalised, or a canonical directory joined with names
    /// beneath it.
    pub(super) fn of_canonical(path: &Path) -> Self {
        Self(path_key(path))
    }

    /// The key of `below`, a file's path below its root: two spellings of
    /// one file share it, as they share the root's key.
    pub(super) fn of_path_below_root(below: &Path) -> Self {
        Self::of_canonical(below)
    }

    pub(super) fn as_str(&self) -> &str {
        &self.0
    }
}

/// The text every spelling of one session root shares, as [`FileKey`] keys a
/// file: the root's canonical path, lowercased on a file system that ignores
/// case. The `roots` table keys each root by it: a root named through `..`
/// or through a link reads and writes the copy its canonical spelling does.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct RootKey(String);

impl RootKey {
    /// The key of the root whose canonical path is `canonical`.
    pub(super) fn of_canonical(canonical: &Path) -> Self {
        Self(FileKey::of_canonical(canonical).0)
    }

    pub(super) fn as_str(&self) -> &str {
        &self.0
    }
}

/// Fails with [`AppError::SessionCacheSourceChanged`] when a file `session`
/// was read from, at its path in `places`, no longer stamps as the cache
/// recorded it.
pub(super) fn validate(connection: &Connection, places: &Places, session: i64) -> Result<()> {
    validate_unstamped(connection, places, session, &HashSet::new())
}

/// [`validate`] for the files of `session` outside `stamped`: files whose
/// discovery stamps the caller compared with the cache's, and that are not
/// stat again.
pub(super) fn validate_unstamped(
    connection: &Connection,
    places: &Places,
    session: i64,
    stamped: &HashSet<&Path>,
) -> Result<()> {
    let mut statement = connection.prepare_cached(
        "SELECT root, path, file_size, mtime_secs, mtime_nsecs FROM session_files \
         WHERE session = ?1",
    )?;
    let mut rows = statement.query([session])?;
    while let Some(row) = rows.next()? {
        let path = places.path(StoredPath {
            root: row.get(0)?,
            path: row.get(1)?,
        })?;
        if stamped.contains(path.as_path()) {
            continue;
        }
        let expected = row
            .get::<_, Option<i64>>(2)?
            .map(|size| {
                Ok::<_, rusqlite::Error>(CachedFingerprint {
                    file_size: size as u64,
                    mtime_secs: row.get::<_, i64>(3)? as u64,
                    mtime_nsecs: row.get(4)?,
                })
            })
            .transpose()?;
        if CachedFingerprint::of_file(&path).ok() != Some(expected) {
            return Err(AppError::SessionCacheSourceChanged(path));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locator_inside_database_file_doesnt_become_filesystem_dependency() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("sessions.db");
        std::fs::write(&database, []).unwrap();
        assert_eq!(
            CachedFingerprint::of_file(&database.join("session.jsonl")).unwrap(),
            None
        );
    }
}
