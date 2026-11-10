use super::path_text;
use crate::error::{AppError, Result};
use crate::history::fingerprint::CachedFingerprint;
use crate::history::provider::walk::Canonicaliser;
use rusqlite::Connection;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The text two spellings of one session file share: its canonical path,
/// lowercased on a file system that ignores case.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct FileKey(String);

impl FileKey {
    /// The key of `path`, already canonical: a path the user named and
    /// rearview canonicalised, or a canonical directory joined with names
    /// beneath it.
    pub(super) fn of_canonical(path: &Path) -> Self {
        let text = path_text(path);
        Self(if IGNORES_CASE {
            text.to_lowercase()
        } else {
            text
        })
    }

    pub(super) fn as_str(&self) -> &str {
        &self.0
    }
}

/// Windows and macOS file systems ignore case by default; Linux's do not.
const IGNORES_CASE: bool = cfg!(any(windows, target_os = "macos"));

/// The keys of the session files one cache writer writes. Each directory
/// is canonicalised at most once: a session root for every file beneath it,
/// and the directory of a file outside the root.
pub(super) struct FileKeys {
    canonicaliser: Arc<dyn Canonicaliser>,
    /// Each directory's canonical path, or its spelling when it does not
    /// resolve, as a locator inside a database file does not.
    canonical_directories: HashMap<PathBuf, PathBuf>,
}

impl FileKeys {
    pub(super) fn new(canonicaliser: Arc<dyn Canonicaliser>) -> Self {
        Self {
            canonicaliser,
            canonical_directories: HashMap::new(),
        }
    }

    /// The key of `file`, as discovery under the session root `root` spells
    /// it: the root's canonical path joined with the file's path beneath
    /// the root. A file outside the root joins its directory's canonical
    /// path instead.
    pub(super) fn of_file_under(&mut self, root: &Path, file: &Path) -> FileKey {
        match file.strip_prefix(root) {
            Ok(beneath) => FileKey::of_canonical(&self.canonical(root).join(beneath)),
            Err(_) => match (file.parent(), file.file_name()) {
                (Some(directory), Some(name)) => {
                    FileKey::of_canonical(&self.canonical(directory).join(name))
                }
                _ => FileKey::of_canonical(file),
            },
        }
    }

    fn canonical(&mut self, directory: &Path) -> &Path {
        self.canonical_directories
            .entry(directory.to_path_buf())
            .or_insert_with(|| {
                self.canonicaliser
                    .canonicalise(directory)
                    .unwrap_or_else(|_| directory.to_path_buf())
            })
    }
}

/// Fails with [`AppError::SessionCacheSourceChanged`] when a file `session`
/// was read from no longer stamps as the cache recorded it.
pub(super) fn validate(connection: &Connection, session: i64) -> Result<()> {
    validate_unstamped(connection, session, &HashSet::new())
}

/// [`validate`] for the files of `session` outside `stamped`: files whose
/// discovery stamps the caller compared with the cache's, and that are not
/// stat again.
pub(super) fn validate_unstamped(
    connection: &Connection,
    session: i64,
    stamped: &HashSet<&Path>,
) -> Result<()> {
    let mut statement = connection.prepare(
        "SELECT path, file_size, mtime_secs, mtime_nsecs FROM session_files WHERE session = ?1",
    )?;
    let mut rows = statement.query([session])?;
    while let Some(row) = rows.next()? {
        let path = PathBuf::from(row.get::<_, String>(0)?);
        if stamped.contains(path.as_path()) {
            continue;
        }
        let expected = row
            .get::<_, Option<i64>>(1)?
            .map(|size| {
                Ok::<_, rusqlite::Error>(CachedFingerprint {
                    file_size: size as u64,
                    mtime_secs: row.get::<_, i64>(2)? as u64,
                    mtime_nsecs: row.get(3)?,
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
