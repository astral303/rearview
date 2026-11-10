//! The stamp a cached session is checked against: its transcripts' size and
//! modification time when the cache recorded it.

use std::time::{SystemTime, UNIX_EPOCH};

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
