//! The stamp a cached session is checked against: its transcripts' size and
//! modification time when the cache recorded it.

use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// A file's stamp, taken just before a read of it. The session cache stores
/// it as the stamp of the content that read found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileStamp {
    pub path: PathBuf,
    /// `None` when the file could not be stat.
    pub fingerprint: Option<CachedFingerprint>,
}

impl FileStamp {
    pub fn before_read(path: &Path) -> Self {
        Self {
            path: path.to_path_buf(),
            fingerprint: CachedFingerprint::of_file(path).ok().flatten(),
        }
    }
}

/// The `(size, mtime)` stamp a cached session is validated against. A
/// cached session is reused while its transcripts still stamp the same.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CachedFingerprint {
    pub file_size: u64,
    pub mtime_secs: u64,
    pub mtime_nsecs: u32,
}

impl CachedFingerprint {
    pub fn of(file_size: u64, mtime: SystemTime) -> Self {
        let since_epoch = mtime.duration_since(UNIX_EPOCH).unwrap_or_default();
        Self {
            file_size,
            mtime_secs: since_epoch.as_secs(),
            mtime_nsecs: since_epoch.subsec_nanos(),
        }
    }

    /// The stamp of the file at `path`; `None` when no file is there, as for
    /// a locator inside a database file.
    pub fn of_file(path: &Path) -> io::Result<Option<Self>> {
        match std::fs::metadata(path) {
            Ok(metadata) => Ok(Some(Self::of(metadata.len(), metadata.modified()?))),
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

    /// True when the file is larger than it was at `before`. A change that
    /// does not grow a transcript counts as a rewrite: a reader's rewrite
    /// check sees only the bytes just before its cursor.
    pub fn has_grown_since(&self, before: &Self) -> bool {
        self.file_size > before.file_size
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// The stamp stands for one file's contents at one moment: a size or an
    /// mtime that moved must not match, or a changed transcript restores stale.
    #[test]
    fn fingerprint_matches_only_size_and_mtime_it_was_taken_from() {
        let mtime = UNIX_EPOCH + Duration::from_secs(1700000000) + Duration::from_nanos(123456789);
        let fingerprint = CachedFingerprint::of(500, mtime);

        assert_eq!(fingerprint, CachedFingerprint::of(500, mtime));
        assert_ne!(fingerprint, CachedFingerprint::of(501, mtime));
        assert_ne!(
            fingerprint,
            CachedFingerprint::of(500, mtime + Duration::from_secs(1))
        );
        assert_ne!(
            fingerprint,
            CachedFingerprint::of(500, UNIX_EPOCH + Duration::from_secs(1700000000)),
            "the sub-second part of the mtime is part of the stamp"
        );
    }
}
