//! Cache files on disk, for the session caches and the semantic embedding
//! cache alike: kept under one cache directory, written through a temp file
//! renamed into place, read only when their header matches, and swept of the
//! temp files an interrupted write left behind.

use crate::clock::Clock;
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

/// Root of every cache this tool writes: `$REARVIEW_CACHE_DIR`, or
/// `~/.cache/rearview`, or `None` without a home directory.
///
/// The override exists so tests that spawn the binary keep cache writes out of
/// the user's real cache; it also lets a user relocate the cache outright.
pub(crate) fn user_cache_base() -> Option<PathBuf> {
    cache_base_from(std::env::var_os("REARVIEW_CACHE_DIR"), home::home_dir())
}

/// Root of the session caches: [`user_cache_base`]. A unit test build reads
/// only a session cache `$REARVIEW_CACHE_DIR` names, never the developer's,
/// which another rearview may be refreshing while the tests run.
pub(crate) fn session_cache_base() -> Option<PathBuf> {
    #[cfg(test)]
    return cache_base_from(std::env::var_os("REARVIEW_CACHE_DIR"), None);
    #[cfg(not(test))]
    user_cache_base()
}

fn cache_base_from(
    override_dir: Option<std::ffi::OsString>,
    home: Option<PathBuf>,
) -> Option<PathBuf> {
    match override_dir.filter(|value| !value.is_empty()) {
        Some(directory) => Some(PathBuf::from(directory)),
        None => Some(cache_base_under(&home?)),
    }
}

/// The cache root under `home` when `REARVIEW_CACHE_DIR` is unset.
pub(crate) fn cache_base_under(home: &Path) -> PathBuf {
    home.join(".cache").join(crate::APP_NAME)
}

/// The size of the files at or under `path`, following symbolic links and
/// counting each target once: a Hugging Face snapshot links to blobs in its
/// own model folder or in a shared `blobs` folder. An entry that cannot be
/// read fails with its path.
pub(crate) fn disk_usage(path: &Path) -> std::io::Result<u64> {
    add_disk_usage(path, &mut HashSet::new())
}

/// `disk_usage` of `path`, skipping files and folders in `counted`, the
/// canonical paths already counted, and adding those it counts.
fn add_disk_usage(path: &Path, counted: &mut HashSet<PathBuf>) -> std::io::Result<u64> {
    let with_path = |error: std::io::Error| {
        std::io::Error::new(error.kind(), format!("{}: {error}", path.display()))
    };
    let metadata = std::fs::metadata(path).map_err(with_path)?;
    if !counted.insert(std::fs::canonicalize(path).map_err(with_path)?) {
        return Ok(0);
    }
    if !metadata.is_dir() {
        return Ok(metadata.len());
    }
    let mut total = 0;
    for entry in std::fs::read_dir(path).map_err(with_path)? {
        total += add_disk_usage(&entry.map_err(with_path)?.path(), counted)?;
    }
    Ok(total)
}

/// Write `data` to `path` through a temp file renamed into place, so a reader
/// never sees half a file. On failure the previous file, if any, stays.
/// Sweeps the file's directory of temp files stale at `clock`'s time first.
pub(crate) fn write_atomically(path: &Path, data: &[u8], clock: &dyn Clock) {
    let Some(parent) = path.parent() else {
        return;
    };
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }
    sweep_stale_temp_files(parent, clock);
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
/// directory of temp files stale at `clock`'s time first.
pub(crate) fn read_if_header_matches(
    path: &Path,
    header: &[u8],
    clock: &dyn Clock,
) -> Option<Vec<u8>> {
    if let Some(parent) = path.parent() {
        sweep_stale_temp_files(parent, clock);
    }
    read_existing_if_header_matches(path, header)
}

/// [`read_if_header_matches`] without sweeping the directory's stale temp
/// files.
pub(crate) fn read_existing_if_header_matches(path: &Path, header: &[u8]) -> Option<Vec<u8>> {
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
pub(crate) fn sweep_stale_temp_files(directory: &Path, clock: &dyn Clock) {
    static SWEPT: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();
    let first_visit = SWEPT
        .get_or_init(Mutex::default)
        .lock()
        .is_ok_and(|mut swept| swept.insert(directory.to_path_buf()));
    if first_visit {
        remove_stale_temp_files(directory, SystemTime::from(clock.wall()));
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
    use crate::clock::TestClock;

    pub(crate) struct TempFileFixture {
        pub(crate) leftover: PathBuf,
        pub(crate) in_progress: PathBuf,
        pub(crate) unrelated: PathBuf,
    }

    impl TempFileFixture {
        /// The files in `directory`, modified relative to `clock`'s wall time.
        pub(crate) fn in_directory(directory: &Path, clock: &TestClock) -> Self {
            std::fs::create_dir_all(directory).unwrap();
            let now = SystemTime::from(clock.wall());
            let two_hours_ago = now - Duration::from_secs(2 * 60 * 60);
            let file_modified_at = |name: &str, modified: SystemTime| {
                let path = directory.join(name);
                let file = std::fs::File::create(&path).unwrap();
                file.set_modified(modified).unwrap();
                path
            };
            Self {
                leftover: file_modified_at(".tmpLEFTOVER", two_hours_ago),
                in_progress: file_modified_at(".tmpWRITING", now),
                unrelated: file_modified_at("notes.bin", two_hours_ago),
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
    use crate::clock::TestClock;

    /// A reader that fails the test when read: a body read past a mismatched
    /// header is the full read the header check exists to skip.
    struct UnreadableBody;

    impl Read for UnreadableBody {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            panic!("the body of a cache file with another header was read");
        }
    }

    #[test]
    fn cache_base_override_replaces_home_derived_default() {
        let home = PathBuf::from("/home/user");
        assert_eq!(
            cache_base_from(None, Some(home.clone())),
            Some(home.join(".cache").join("rearview"))
        );
        assert_eq!(
            cache_base_from(Some("/isolated/cache".into()), Some(home.clone())),
            Some(PathBuf::from("/isolated/cache")),
            "the override wins even when a home directory exists"
        );
        assert_eq!(
            cache_base_from(Some("".into()), Some(home.clone())),
            Some(home.join(".cache").join("rearview")),
            "an empty override means unset"
        );
        assert_eq!(cache_base_from(None, None), None);
    }

    #[test]
    fn file_with_another_header_is_rejected_without_reading_past_it() {
        let read = read_matching(b"OLDHEADER".as_slice().chain(UnreadableBody), b"NEWHEADER");

        assert_eq!(read, None);
    }

    #[test]
    fn file_with_expected_header_is_read_whole() {
        let file = b"HEADERentries".to_vec();

        let read = read_matching(file.as_slice(), b"HEADER");

        assert_eq!(read, Some(file));
    }

    #[test]
    fn file_shorter_than_its_header_is_rejected() {
        assert_eq!(read_matching(b"HEAD".as_slice(), b"HEADER"), None);
    }

    #[test]
    fn write_sweeps_its_directory_of_leftover_temp_files() {
        let directory = tempfile::tempdir().unwrap();
        let clock = TestClock::default();
        let fixture = TempFileFixture::in_directory(directory.path(), &clock);

        write_atomically(&directory.path().join("cache.bin"), b"entries", &clock);

        fixture.assert_swept();
    }

    #[test]
    fn read_sweeps_its_directory_of_leftover_temp_files() {
        let directory = tempfile::tempdir().unwrap();
        let clock = TestClock::default();
        let fixture = TempFileFixture::in_directory(directory.path(), &clock);

        read_if_header_matches(&directory.path().join("cache.bin"), b"HEADER", &clock);

        fixture.assert_swept();
    }

    /// Creates a symbolic link at `link` to the file `target`, a path relative
    /// to the link's folder. `false` where the platform refuses: Windows
    /// creates one only with Developer Mode or the privilege to create one.
    fn create_file_link(target: &Path, link: &Path) -> bool {
        #[cfg(unix)]
        let created = std::os::unix::fs::symlink(target, link);
        #[cfg(windows)]
        let created = std::os::windows::fs::symlink_file(target, link);
        created.is_ok()
    }

    /// `../../blobs/blob` with the platform's separator: Windows does not
    /// resolve a link target written with `/`.
    fn two_up_to_blob() -> PathBuf {
        ["..", "..", "blobs", "blob"].iter().collect()
    }

    /// Runs only where the platform lets the test create a symbolic link.
    #[test]
    fn disk_usage_counts_blob_linked_from_snapshot_once() {
        let model = tempfile::tempdir().unwrap();
        let blobs = model.path().join("blobs");
        std::fs::create_dir(&blobs).unwrap();
        std::fs::write(blobs.join("blob"), [0; 300]).unwrap();
        let snapshot = model.path().join("snapshots").join("snapshot");
        std::fs::create_dir_all(&snapshot).unwrap();
        if !create_file_link(&two_up_to_blob(), &snapshot.join("model.onnx")) {
            return;
        }

        assert_eq!(
            disk_usage(model.path()).unwrap(),
            300,
            "a blob a snapshot links to is not counted exactly once"
        );
    }

    /// Runs only where the platform lets the test create a symbolic link.
    #[test]
    fn disk_usage_counts_blob_linked_outside_model_folder() {
        let root = tempfile::tempdir().unwrap();
        let shared_blobs = root.path().join("blobs");
        std::fs::create_dir(&shared_blobs).unwrap();
        std::fs::write(shared_blobs.join("blob"), [0; 400]).unwrap();
        let model = root.path().join("models--owner--name");
        let model_blobs = model.join("blobs");
        std::fs::create_dir_all(&model_blobs).unwrap();
        if !create_file_link(&two_up_to_blob(), &model_blobs.join("blob")) {
            return;
        }

        assert_eq!(
            disk_usage(&model).unwrap(),
            400,
            "a blob linked outside the model folder is not counted"
        );
    }

    /// Runs only where the platform denies the test a folder it locks: not
    /// for the root user.
    #[cfg(unix)]
    #[test]
    fn disk_usage_of_unreadable_folder_fails_with_its_path() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let locked = root.path().join("locked");
        std::fs::create_dir(&locked).unwrap();
        std::fs::write(locked.join("file"), [0; 100]).unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        let readable_anyway = std::fs::read_dir(&locked).is_ok();

        let usage = disk_usage(root.path());

        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        if readable_anyway {
            return;
        }
        let error = usage.expect_err("an unreadable folder is counted as a size");
        assert!(
            error.to_string().contains(&locked.display().to_string()),
            "the disk usage error does not name the unreadable folder: {error}"
        );
    }

    #[test]
    fn a_write_stalled_past_the_stale_age_loses_its_temp_file() {
        let directory = tempfile::tempdir().unwrap();
        let clock = TestClock::default();
        let fixture = TempFileFixture::in_directory(directory.path(), &clock);
        clock.advance(STALE_TEMP_FILE_AGE + Duration::from_secs(1));

        read_if_header_matches(&directory.path().join("cache.bin"), b"HEADER", &clock);

        assert!(
            !fixture.in_progress.exists(),
            "a stalled write keeps its file"
        );
    }
}
