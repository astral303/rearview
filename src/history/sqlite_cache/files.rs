use crate::error::{AppError, Result};
use crate::history::cache::CachedFingerprint;
use rusqlite::Connection;
use std::io;
use std::path::{Path, PathBuf};

pub(super) fn key(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

pub(super) fn fingerprint(path: &Path) -> io::Result<Option<CachedFingerprint>> {
    match std::fs::metadata(path) {
        Ok(metadata) => Ok(Some(CachedFingerprint::of(
            metadata.len(),
            metadata.modified()?,
        ))),
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
            ) =>
        {
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

pub(super) fn validate(connection: &Connection, session: i64) -> Result<()> {
    let mut statement = connection.prepare(
        "SELECT path, file_size, mtime_secs, mtime_nsecs FROM session_files WHERE session = ?1",
    )?;
    let mut rows = statement.query([session])?;
    while let Some(row) = rows.next()? {
        let path = PathBuf::from(row.get::<_, String>(0)?);
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
        if fingerprint(&path).ok() != Some(expected) {
            return Err(AppError::SessionCacheSourceChanged(path));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_locator_inside_a_database_file_doesnt_become_a_filesystem_dependency() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("sessions.db");
        std::fs::write(&database, []).unwrap();
        assert_eq!(fingerprint(&database.join("session.jsonl")).unwrap(), None);
    }
}
