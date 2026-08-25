//! File-tree helpers for providers whose sessions are transcript files.
//!
//! The core never walks a directory itself — each file-backed provider states
//! its own tree shape by composing these from its
//! [`discover`](super::SessionStorage::discover). A new file-based agent is
//! these calls plus a format.

use super::{Fingerprint, SessionRoot, SessionStub};
use crate::error::Result;
use std::fs::read_dir;
use std::path::{Path, PathBuf};

/// A resolved session root whose transcripts are plain files `depth` directory
/// levels below it: 0 beside the root itself, 1 in a directory per project,
/// 3 in Codex's `YYYY/MM/DD` tree.
///
/// The pairing lives with the code that resolved the root; the core only ever
/// sees the [`SessionRoot`] inside.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileRoot {
    pub root: SessionRoot,
    pub depth: usize,
}

/// The transcripts at one depth of a tree: the plain ones to read, and a
/// count of the ones an agent compressed.
#[derive(Debug, Default, Eq, PartialEq)]
pub struct Transcripts {
    /// The `.jsonl` files, sorted so successive runs agree.
    pub plain: Vec<PathBuf>,
    /// The `.jsonl.zst` files: transcripts an agent compressed in place.
    /// Nothing here decodes them, so they are counted for the user rather
    /// than listed.
    pub compressed_count: usize,
}

/// Every transcript exactly `depth` directory levels below `directory`,
/// plain or compressed.
///
/// Fixed depth rather than recursion, so a symlink cycle inside the tree
/// cannot make the walk unbounded; `is_dir` follows symlinks, so a project
/// directory linked into the tree is still searched. A directory that does
/// not exist yields no files rather than an error: an agent the user has not
/// installed is an absence, not a failure.
pub fn transcripts_at_depth(directory: &Path, depth: usize) -> Result<Transcripts> {
    let mut transcripts = Transcripts::default();
    if !directory.exists() {
        return Ok(transcripts);
    }
    let mut parents = vec![directory.to_path_buf()];
    for _ in 0..depth {
        let mut children = Vec::new();
        for parent in &parents {
            children.extend(subdirectories(parent)?);
        }
        parents = children;
    }
    for parent in &parents {
        collect_transcripts(parent, &mut transcripts)?;
    }
    transcripts.plain.sort();
    Ok(transcripts)
}

/// The `.jsonl` files exactly `depth` directory levels below `directory`,
/// sorted so successive runs agree.
pub fn jsonl_files_at_depth(directory: &Path, depth: usize) -> Result<Vec<PathBuf>> {
    Ok(transcripts_at_depth(directory, depth)?.plain)
}

/// Stat each file into a [`SessionStub`], keyed by its path relative to the
/// root. A file that cannot be stat — vanished between enumeration and here —
/// is skipped rather than failing the whole root.
pub fn file_stubs(root: &SessionRoot, files: Vec<PathBuf>) -> Vec<SessionStub> {
    files
        .into_iter()
        .filter_map(|path| {
            let metadata = std::fs::metadata(&path).ok()?;
            // Keys are relative to the root, so a root that moves misses
            // cleanly rather than colliding with its old contents.
            let cache_key = path
                .strip_prefix(&root.path)
                .unwrap_or(&path)
                .to_string_lossy()
                .into_owned();
            Some(SessionStub {
                fingerprint: Fingerprint {
                    size: metadata.len(),
                    modified: metadata.modified().ok(),
                },
                locator: path,
                cache_key,
            })
        })
        .collect()
}

/// The transcripts directly inside `directory`: `.jsonl` files listed,
/// `.jsonl.zst` files counted.
fn collect_transcripts(directory: &Path, transcripts: &mut Transcripts) -> Result<()> {
    for entry in read_dir(directory)? {
        let path = entry?.path();
        if is_transcript(&path) {
            transcripts.plain.push(path);
        } else if is_compressed_transcript(&path) {
            transcripts.compressed_count += 1;
        }
    }
    Ok(())
}

/// `name.jsonl`.
fn is_transcript(path: &Path) -> bool {
    has_extension(path, "jsonl")
}

/// `name.jsonl.zst`: a transcript compressed in place, as Codex does.
fn is_compressed_transcript(path: &Path) -> bool {
    has_extension(path, "zst")
        && path
            .file_stem()
            .is_some_and(|stem| is_transcript(Path::new(stem)))
}

fn has_extension(path: &Path, extension: &str) -> bool {
    path.extension().and_then(|found| found.to_str()) == Some(extension)
}

pub fn subdirectories(parent: &Path) -> Result<Vec<PathBuf>> {
    let mut directories = Vec::new();
    for entry in read_dir(parent)? {
        let path = entry?.path();
        if path.is_dir() {
            directories.push(path);
        }
    }
    Ok(directories)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_transcript(path: &Path) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "{}\n").unwrap();
    }

    #[test]
    fn a_missing_directory_yields_no_files() {
        let directory = tempfile::tempdir().unwrap();
        let files = jsonl_files_at_depth(&directory.path().join("never-created"), 0).unwrap();
        assert!(files.is_empty());
    }

    #[test]
    fn depth_zero_takes_only_jsonl_beside_the_root() {
        let directory = tempfile::tempdir().unwrap();
        write_transcript(&directory.path().join("session.jsonl"));
        write_transcript(&directory.path().join("notes.txt"));
        write_transcript(&directory.path().join("nested/deeper.jsonl"));

        let files = jsonl_files_at_depth(directory.path(), 0).unwrap();
        assert_eq!(files, vec![directory.path().join("session.jsonl")]);
    }

    #[test]
    fn depth_one_skips_transcripts_in_the_root_itself() {
        let directory = tempfile::tempdir().unwrap();
        write_transcript(&directory.path().join("stray.jsonl"));
        write_transcript(&directory.path().join("project-b/second.jsonl"));
        write_transcript(&directory.path().join("project-a/first.jsonl"));

        let files = jsonl_files_at_depth(directory.path(), 1).unwrap();
        assert_eq!(
            files,
            vec![
                directory.path().join("project-a/first.jsonl"),
                directory.path().join("project-b/second.jsonl"),
            ],
            "child directories are searched, the root is not, and results are sorted"
        );
    }

    #[test]
    fn depth_three_takes_only_jsonl_exactly_three_levels_down() {
        let directory = tempfile::tempdir().unwrap();
        let listed = directory.path().join("2026/08/19/session.jsonl");
        write_transcript(&listed);
        write_transcript(&directory.path().join("2026/08/too-shallow.jsonl"));
        write_transcript(&directory.path().join("2026/08/19/deeper/too-deep.jsonl"));
        write_transcript(&directory.path().join("2026/08/19/notes.txt"));

        let files = jsonl_files_at_depth(directory.path(), 3).unwrap();
        assert_eq!(files, vec![listed]);
    }

    /// Codex compresses old rollouts to `.jsonl.zst` in place. They are not
    /// transcripts to read, and the walk counts them so the list can show why
    /// it holds less than the tree does.
    #[test]
    fn compressed_transcripts_are_counted_and_not_listed() {
        let directory = tempfile::tempdir().unwrap();
        let listed = directory.path().join("2026/08/19/session.jsonl");
        write_transcript(&listed);
        write_transcript(&directory.path().join("2026/08/19/older.jsonl.zst"));
        write_transcript(&directory.path().join("2026/08/too-shallow.jsonl.zst"));
        write_transcript(&directory.path().join("2026/08/19/archive.zst"));

        let transcripts = transcripts_at_depth(directory.path(), 3).unwrap();

        assert_eq!(
            transcripts,
            Transcripts {
                plain: vec![listed],
                compressed_count: 1,
            },
            "only a .jsonl.zst at the walked depth counts"
        );
    }

    #[cfg(unix)]
    #[test]
    fn depth_one_follows_symlinked_project_directories() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("elsewhere");
        let root = directory.path().join("sessions");
        write_transcript(&target.join("session.jsonl"));
        std::fs::create_dir_all(&root).unwrap();
        symlink(&target, root.join("linked-project")).unwrap();

        let files = jsonl_files_at_depth(&root, 1).unwrap();
        assert_eq!(files, vec![root.join("linked-project/session.jsonl")]);
    }

    #[test]
    fn file_stubs_key_by_location_within_the_root_and_skip_vanished_files() {
        let directory = tempfile::tempdir().unwrap();
        let present = directory.path().join("project").join("session.jsonl");
        write_transcript(&present);

        let root = SessionRoot::new(directory.path());
        let stubs = file_stubs(
            &root,
            vec![present.clone(), directory.path().join("vanished.jsonl")],
        );

        assert_eq!(stubs.len(), 1, "a file that cannot be stat is not a stub");
        assert_eq!(stubs[0].locator, present);
        assert_eq!(
            stubs[0].cache_key,
            PathBuf::from("project")
                .join("session.jsonl")
                .to_string_lossy()
        );
        assert_eq!(stubs[0].fingerprint.size, 3);
        assert!(stubs[0].fingerprint.modified.is_some());
    }
}
