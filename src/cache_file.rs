//! Cache files on disk, for the session caches and the semantic embedding
//! cache alike: written through a temp file renamed into place, read only
//! when their header matches, and swept of the temp files an interrupted
//! write left behind.

use std::collections::HashSet;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime};

/// The prefix a cache write's temp file is created with. `tempfile` gives
/// its own temp files the same one, so leftovers from older releases match.
pub(crate) const TEMP_FILE_PREFIX: &str = ".tmp";

/// A temp file older than this is left over from a write that stopped
/// between creating it and renaming it into place. A write in progress keeps
/// its file's modification time current, so it is kept unless it stalls for
/// this long.
const STALE_TEMP_FILE_AGE: Duration = Duration::from_secs(60 * 60);

/// Write `data` to `path` through a temp file renamed into place, so a reader
/// never sees half a file. On failure the previous file, if any, stays.
pub(crate) fn write_atomically(path: &Path, data: &[u8]) {
    let Some(parent) = path.parent() else {
        return;
    };
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }
    sweep_stale_temp_files(parent);
    let Ok(mut temp) = tempfile::Builder::new()
        .prefix(TEMP_FILE_PREFIX)
        .tempfile_in(parent)
    else {
        return;
    };
    if temp.write_all(data).is_err() {
        return;
    }
    let _ = temp.persist(path);
}

/// The whole file at `path` when it opens with `header`; `None` when it is
/// absent, unreadable, or opens with anything else. Sweeps the file's
/// directory of stale temp files first.
pub(crate) fn read_if_header_matches(path: &Path, header: &[u8]) -> Option<Vec<u8>> {
    if let Some(parent) = path.parent() {
        sweep_stale_temp_files(parent);
    }
    read_matching(std::fs::File::open(path).ok()?, header)
}

/// Everything `reader` holds when it opens with `header`. A reader that opens
/// with anything else costs only the header's length, so a cache left by an
/// older release is not read in full only to be dropped.
fn read_matching(mut reader: impl Read, header: &[u8]) -> Option<Vec<u8>> {
    let mut data = vec![0; header.len()];
    reader.read_exact(&mut data).ok()?;
    if data != header {
        return None;
    }
    reader.read_to_end(&mut data).ok()?;
    Some(data)
}

/// Remove `directory`'s stale temp files the first time this process reads
/// or writes a cache file in it, so a directory of many cache files is
/// listed once, not once per file.
fn sweep_stale_temp_files(directory: &Path) {
    static SWEPT: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();
    let first_visit = SWEPT
        .get_or_init(Mutex::default)
        .lock()
        .is_ok_and(|mut swept| swept.insert(directory.to_path_buf()));
    if first_visit {
        remove_stale_temp_files(directory, SystemTime::now());
    }
}

/// Delete the files in `directory` whose names start with
/// [`TEMP_FILE_PREFIX`] and that are older than [`STALE_TEMP_FILE_AGE`].
///
/// A failed write removes its own temp file when it is dropped; a process
/// that stops mid-write cannot. Matching on the prefix alone relies on the
/// directory being rearview's own cache.
fn remove_stale_temp_files(directory: &Path, now: SystemTime) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let is_temp_file = entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.starts_with(TEMP_FILE_PREFIX));
        let is_stale = || {
            entry
                .metadata()
                .and_then(|metadata| metadata.modified())
                .ok()
                .and_then(|modified| now.duration_since(modified).ok())
                .is_some_and(|age| age > STALE_TEMP_FILE_AGE)
        };
        if is_temp_file && is_stale() {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Files created in a test cache directory, aged as a leftover temp file, a
/// write in progress, and an unrelated file, for the sweep tests of every
/// cache.
#[cfg(test)]
pub(crate) mod test_support {
    use super::*;

    pub(crate) struct TempFileFixture {
        pub(crate) leftover: PathBuf,
        pub(crate) in_progress: PathBuf,
        pub(crate) unrelated: PathBuf,
    }

    impl TempFileFixture {
        pub(crate) fn in_directory(directory: &Path) -> Self {
            std::fs::create_dir_all(directory).unwrap();
            let two_hours_ago = SystemTime::now() - Duration::from_secs(2 * 60 * 60);
            let file_aged = |name: &str, modified: Option<SystemTime>| {
                let path = directory.join(name);
                let file = std::fs::File::create(&path).unwrap();
                if let Some(modified) = modified {
                    file.set_modified(modified).unwrap();
                }
                path
            };
            Self {
                leftover: file_aged(".tmpLEFTOVER", Some(two_hours_ago)),
                in_progress: file_aged(".tmpWRITING", None),
                unrelated: file_aged("notes.bin", Some(two_hours_ago)),
            }
        }

        /// The leftover is gone; the write in progress and the unrelated
        /// file stay.
        pub(crate) fn assert_swept(&self) {
            assert!(!self.leftover.exists(), "a leftover temp file stays");
            assert!(
                self.in_progress.exists(),
                "a write in progress loses its file"
            );
            assert!(
                self.unrelated.exists(),
                "a file that is not a temp file is removed"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::TempFileFixture;
    use super::*;

    /// A reader that fails the test when read: a body read past a mismatched
    /// header is the full read the header check exists to skip.
    struct UnreadableBody;

    impl Read for UnreadableBody {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            panic!("the body of a cache file with another header was read");
        }
    }

    #[test]
    fn a_file_with_another_header_is_rejected_without_reading_past_it() {
        let read = read_matching(b"OLDHEADER".as_slice().chain(UnreadableBody), b"NEWHEADER");

        assert_eq!(read, None);
    }

    #[test]
    fn a_file_with_the_expected_header_is_read_whole() {
        let file = b"HEADERentries".to_vec();

        let read = read_matching(file.as_slice(), b"HEADER");

        assert_eq!(read, Some(file));
    }

    #[test]
    fn a_file_shorter_than_its_header_is_rejected() {
        assert_eq!(read_matching(b"HEAD".as_slice(), b"HEADER"), None);
    }

    #[test]
    fn a_write_sweeps_its_directory_of_leftover_temp_files() {
        let directory = tempfile::tempdir().unwrap();
        let fixture = TempFileFixture::in_directory(directory.path());

        write_atomically(&directory.path().join("cache.bin"), b"entries");

        fixture.assert_swept();
    }

    #[test]
    fn a_read_sweeps_its_directory_of_leftover_temp_files() {
        let directory = tempfile::tempdir().unwrap();
        let fixture = TempFileFixture::in_directory(directory.path());

        read_if_header_matches(&directory.path().join("cache.bin"), b"HEADER");

        fixture.assert_swept();
    }
}
