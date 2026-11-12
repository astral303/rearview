//! File-tree helpers for providers whose sessions are transcript files.
//!
//! The core never walks a directory itself — each file-backed provider states
//! its own tree shape by composing these from its
//! [`discover`](super::SessionStorage::discover). A new file-based agent is
//! these calls plus a format.
//!
//! Every provider lists directories through a [`DirectoryLister`]: discovery
//! through a [`DiscoveryLister`] over the one its caller passes, so a test
//! can make one listing fail under any provider; other reads through
//! [`SystemLister`].

use super::storage::UnreadableDirectory;
use super::{DiscoveredSessions, Fingerprint, SessionRoot, SessionStub};
use crate::error::Result;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::{Mutex, PoisonError};

/// Lists the entries of one directory.
pub trait DirectoryLister: Send + Sync {
    /// The path of every entry directly inside `directory`, in the order the
    /// file system gives them.
    fn list(&self, directory: &Path) -> std::io::Result<Vec<PathBuf>>;
}

/// The file system's own listing.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemLister;

impl DirectoryLister for SystemLister {
    fn list(&self, directory: &Path) -> std::io::Result<Vec<PathBuf>> {
        std::fs::read_dir(directory)?
            .map(|entry| entry.map(|entry| entry.path()))
            .collect()
    }
}

/// Canonicalises a path: links followed, `.` and `..` removed, and on
/// Windows the verbatim `\\?\` form.
pub trait Canonicaliser: Send + Sync {
    fn canonicalise(&self, path: &Path) -> std::io::Result<PathBuf>;
}

/// The file system's canonicalisation. [`Environment::new`] and the
/// [`RootEnvironment`] constructors make one; other code canonicalises a
/// path through its environment's canonicaliser, which a test replaces with
/// its own.
///
/// [`Environment::new`]: crate::history::Environment::new
/// [`RootEnvironment`]: super::RootEnvironment
#[derive(Clone, Copy, Debug)]
pub struct SystemCanonicaliser(());

impl SystemCanonicaliser {
    pub(in crate::history) fn of_process() -> Arc<dyn Canonicaliser> {
        Arc::new(Self(()))
    }

    /// The file system's canonicalisation, for a test.
    #[cfg(test)]
    pub(crate) fn shared() -> Arc<dyn Canonicaliser> {
        Self::of_process()
    }
}

impl Canonicaliser for SystemCanonicaliser {
    // Every canonicalisation in the environment calls this method; the lint
    // refuses `canonicalize` everywhere else.
    #[allow(clippy::disallowed_methods)]
    fn canonicalise(&self, path: &Path) -> std::io::Result<PathBuf> {
        std::fs::canonicalize(path)
    }
}

/// The lister one discovery walks a session root through. An error listing
/// the root fails the discovery. A directory beneath the root that cannot be
/// listed, such as one an agent is deleting on Windows, lists as empty and is
/// recorded, so the walk continues and hides only the sessions inside it.
pub struct DiscoveryLister<'a> {
    lister: &'a dyn DirectoryLister,
    root: &'a Path,
    unreadable_directories: Mutex<Vec<UnreadableDirectory>>,
}

impl<'a> DiscoveryLister<'a> {
    /// A discovery of the root at `root`, listing through `lister`.
    pub fn beneath(lister: &'a dyn DirectoryLister, root: &'a Path) -> Self {
        Self {
            lister,
            root,
            unreadable_directories: Mutex::default(),
        }
    }

    /// The sessions this discovery found, `stubs`, with every directory it
    /// could not list.
    pub fn discovered(self, stubs: Vec<SessionStub>) -> DiscoveredSessions {
        DiscoveredSessions {
            unreadable_directories: self.into_unreadable_directories(),
            ..DiscoveredSessions::complete(stubs)
        }
    }

    /// Every directory this discovery could not list, in the order it tried
    /// them.
    pub fn into_unreadable_directories(self) -> Vec<UnreadableDirectory> {
        self.unreadable_directories
            .into_inner()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

impl DirectoryLister for DiscoveryLister<'_> {
    fn list(&self, directory: &Path) -> std::io::Result<Vec<PathBuf>> {
        match self.lister.list(directory) {
            Err(error) if directory != self.root => {
                self.unreadable_directories
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push(UnreadableDirectory {
                        path: directory.to_path_buf(),
                        error: error.to_string(),
                    });
                Ok(Vec::new())
            }
            listed => listed,
        }
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use std::sync::Mutex;

    /// Makes `link` a link to the directory `target`: a symbolic link on
    /// Unix, a junction on Windows, because creating a junction doesn't
    /// require administrator rights.
    pub(crate) fn link_directory(target: &Path, link: &Path) {
        #[cfg(unix)]
        std::os::unix::fs::symlink(target, link).unwrap();
        #[cfg(windows)]
        {
            let status = std::process::Command::new("cmd")
                .arg("/C")
                .arg("mklink")
                .arg("/J")
                .arg(link)
                .arg(target)
                .stdout(std::process::Stdio::null())
                .status()
                .unwrap();
            assert!(status.success(), "mklink /J makes the junction");
        }
    }

    /// Removes the link [`link_directory`] made at `link`; its target stays.
    pub(crate) fn unlink_directory(link: &Path) {
        #[cfg(unix)]
        std::fs::remove_file(link).unwrap();
        #[cfg(windows)]
        std::fs::remove_dir(link).unwrap();
    }

    /// The file system's listing, recording each directory it lists,
    /// running the actions a test schedules before a listing, and refusing
    /// the directories a test names, as a directory the user cannot read
    /// refuses.
    #[derive(Default)]
    pub(crate) struct ScriptedLister {
        /// Each refused directory with the number of its first refused
        /// listing.
        refused: Mutex<Vec<(PathBuf, usize)>>,
        actions: Mutex<Vec<ListingAction>>,
        listed: Mutex<Vec<PathBuf>>,
    }

    /// An action a test runs before the `listing`th listing of `directory`.
    struct ListingAction {
        directory: PathBuf,
        listing: usize,
        action: Arc<dyn Fn() + Send + Sync>,
    }

    impl ScriptedLister {
        /// Refuse every later listing of `directory` with a permission error.
        pub(crate) fn refuse(&self, directory: &Path) {
            self.refuse_from_listing(directory, self.times_listed(directory) + 1);
        }

        /// Refuse the `listing`th listing of `directory`, counted from the
        /// first, and every one after it.
        pub(crate) fn refuse_from_listing(&self, directory: &Path, listing: usize) {
            self.refused
                .lock()
                .unwrap()
                .push((directory.to_path_buf(), listing));
        }

        /// Run `action` before the `listing`th listing of `directory`,
        /// counted from the first.
        pub(crate) fn run_before_listing(
            &self,
            directory: &Path,
            listing: usize,
            action: impl Fn() + Send + Sync + 'static,
        ) {
            self.actions.lock().unwrap().push(ListingAction {
                directory: directory.to_path_buf(),
                listing,
                action: Arc::new(action),
            });
        }

        /// The number of times `directory` was listed, refused listings
        /// included.
        pub(crate) fn times_listed(&self, directory: &Path) -> usize {
            self.listed
                .lock()
                .unwrap()
                .iter()
                .filter(|listed| listed.as_path() == directory)
                .count()
        }
    }

    /// The file system's canonicalisation, recording each path it
    /// canonicalises.
    #[derive(Default)]
    pub(crate) struct CountingCanonicaliser {
        canonicalised: Mutex<Vec<PathBuf>>,
    }

    impl CountingCanonicaliser {
        /// Every path canonicalised so far, in order and with repeats.
        pub(crate) fn canonicalised(&self) -> Vec<PathBuf> {
            self.canonicalised.lock().unwrap().clone()
        }
    }

    /// Asserts that `actual` canonicalisations are `expected`, in order and
    /// with repeats. On a mismatch the failure lists each expected call with
    /// its purpose.
    pub(crate) fn assert_canonicalised(actual: &[PathBuf], expected: &[(&Path, &str)]) {
        let matches = actual.len() == expected.len()
            && actual
                .iter()
                .zip(expected)
                .all(|(path, (expected_path, _))| path == expected_path);
        let expected_calls: Vec<String> = expected
            .iter()
            .map(|(path, purpose)| format!("{} ({purpose})", path.display()))
            .collect();
        assert!(
            matches,
            "expected these canonicalisations, each with its purpose:\n{}\nactual:\n{actual:#?}",
            expected_calls.join("\n")
        );
    }

    /// A canonicalisation a test expects: the path, and what it is for.
    pub(crate) type ExpectedCall<'a> = (&'a Path, &'a str);

    /// The canonicalisations of opening one session by its path through
    /// `agent read` and the printout, each from a cache its own load filled.
    pub(crate) struct OpeningCanonicalisations<'a> {
        /// Key discovery's: the roots, then each directory a session
        /// recorded that the agent's keys canonicalise.
        pub(crate) key_discovery: Vec<ExpectedCall<'a>>,
        /// The canonical path's directory, compared with each key's file;
        /// `None` when the canonical path is spelled as the path typed.
        pub(crate) canonical_directory: Option<ExpectedCall<'a>>,
        /// The roots `agent read`'s refresh canonicalises again: those key
        /// discovery could not canonicalise.
        pub(crate) canonicalised_again: Vec<ExpectedCall<'a>>,
        /// The printout's: the roots its record of the session is placed
        /// below.
        pub(crate) record_placement: Vec<ExpectedCall<'a>>,
    }

    impl<'a> OpeningCanonicalisations<'a> {
        /// `agent read` of the path as typed.
        pub(crate) fn agent_read(&self) -> Vec<ExpectedCall<'a>> {
            [&self.key_discovery, &self.canonicalised_again]
                .into_iter()
                .flatten()
                .copied()
                .collect()
        }

        /// `agent read` of the canonical path.
        pub(crate) fn agent_read_by_canonical_path(&self) -> Vec<ExpectedCall<'a>> {
            self.key_discovery
                .iter()
                .chain(&self.canonical_directory)
                .chain(&self.canonicalised_again)
                .copied()
                .collect()
        }

        /// The printout of a path the cache finds by its text: the record
        /// placement.
        pub(crate) fn printout(&self) -> Vec<ExpectedCall<'a>> {
            self.record_placement.clone()
        }
    }

    impl Canonicaliser for CountingCanonicaliser {
        fn canonicalise(&self, path: &Path) -> std::io::Result<PathBuf> {
            self.canonicalised.lock().unwrap().push(path.to_path_buf());
            SystemCanonicaliser(()).canonicalise(path)
        }
    }

    impl DirectoryLister for ScriptedLister {
        fn list(&self, directory: &Path) -> std::io::Result<Vec<PathBuf>> {
            let listing = {
                let mut listed = self.listed.lock().unwrap();
                listed.push(directory.to_path_buf());
                listed.iter().filter(|listed| *listed == directory).count()
            };
            // Cloned out so an action can list or schedule without a deadlock.
            let actions: Vec<_> = self
                .actions
                .lock()
                .unwrap()
                .iter()
                .filter(|scheduled| {
                    scheduled.directory == directory && scheduled.listing == listing
                })
                .map(|scheduled| Arc::clone(&scheduled.action))
                .collect();
            for action in actions {
                action();
            }
            if self
                .refused
                .lock()
                .unwrap()
                .iter()
                .any(|(refused, from)| refused == directory && listing >= *from)
            {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "access denied",
                ));
            }
            SystemLister.list(directory)
        }
    }
}

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
/// plain or compressed, listed through `lister`.
///
/// Fixed depth rather than recursion, so a symlink cycle inside the tree
/// cannot make the walk unbounded; `is_dir` follows symlinks, so a project
/// directory linked into the tree is still searched. A directory that does
/// not exist yields no files rather than an error: an agent the user has not
/// installed is an absence, not a failure.
pub fn transcripts_at_depth(
    lister: &dyn DirectoryLister,
    directory: &Path,
    depth: usize,
) -> Result<Transcripts> {
    let mut transcripts = Transcripts::default();
    if !directory.exists() {
        return Ok(transcripts);
    }
    let mut parents = vec![directory.to_path_buf()];
    for _ in 0..depth {
        let mut children = Vec::new();
        for parent in &parents {
            children.extend(subdirectories(lister, parent)?);
        }
        parents = children;
    }
    for parent in &parents {
        collect_transcripts(lister, parent, &mut transcripts)?;
    }
    transcripts.plain.sort();
    Ok(transcripts)
}

/// The `.jsonl` files exactly `depth` directory levels below `directory`,
/// listed through `lister`, sorted so successive runs agree.
pub fn jsonl_files_at_depth(
    lister: &dyn DirectoryLister,
    directory: &Path,
    depth: usize,
) -> Result<Vec<PathBuf>> {
    Ok(transcripts_at_depth(lister, directory, depth)?.plain)
}

/// Stat each file into a [`SessionStub`] with no sub-agent transcripts, keyed
/// by its path relative to the root. A file that cannot be stat, vanished
/// between enumeration and here, is left out.
pub fn file_stubs(root: &SessionRoot, files: Vec<PathBuf>) -> Vec<SessionStub> {
    session_stubs(
        root,
        files
            .into_iter()
            .map(|transcript| SessionFiles {
                transcript,
                subagents: Vec::new(),
            })
            .collect(),
    )
}

/// One session's files: its own transcript and the sub-agent transcripts
/// its agent wrote beside it, in the order the viewer shows them.
pub struct SessionFiles {
    pub transcript: PathBuf,
    pub subagents: Vec<PathBuf>,
}

/// Stat each session and its sub-agent transcripts into one [`SessionStub`]
/// whose fingerprint spans them all, keyed by the transcript's path relative
/// to the root. A session whose transcript cannot be stat is left out; a
/// sub-agent transcript that cannot be is left out of its session, since
/// the agent deleted it between enumeration and here.
pub fn session_stubs(root: &SessionRoot, sessions: Vec<SessionFiles>) -> Vec<SessionStub> {
    sessions
        .into_iter()
        .filter_map(|session| {
            let transcript = fingerprint_of(&session.transcript)?;
            let (subagents, subagent_fingerprints): (Vec<_>, Vec<_>) = session
                .subagents
                .into_iter()
                .filter_map(|path| {
                    let fingerprint = fingerprint_of(&path)?;
                    Some((path, fingerprint))
                })
                .unzip();
            // Keys are relative to the root, so a root that moves misses
            // cleanly rather than colliding with its old contents.
            let cache_key = session
                .transcript
                .strip_prefix(&root.path)
                .unwrap_or(&session.transcript)
                .to_string_lossy()
                .into_owned();
            Some(SessionStub {
                fingerprint: Fingerprint::spanning(transcript, subagent_fingerprints),
                locator: session.transcript,
                subagents,
                cache_key,
            })
        })
        .collect()
}

fn fingerprint_of(path: &Path) -> Option<Fingerprint> {
    let metadata = std::fs::metadata(path).ok()?;
    Some(Fingerprint {
        size: metadata.len(),
        modified: metadata.modified().ok(),
    })
}

/// The transcripts directly inside `directory`: `.jsonl` files listed,
/// `.jsonl.zst` files counted.
fn collect_transcripts(
    lister: &dyn DirectoryLister,
    directory: &Path,
    transcripts: &mut Transcripts,
) -> Result<()> {
    for path in lister.list(directory)? {
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

/// The directories directly inside `parent`, listed through `lister`.
pub fn subdirectories(lister: &dyn DirectoryLister, parent: &Path) -> Result<Vec<PathBuf>> {
    Ok(lister
        .list(parent)?
        .into_iter()
        .filter(|path| path.is_dir())
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_transcript(path: &Path) {
        write_bytes(path, "{}\n");
    }

    fn write_bytes(path: &Path, contents: &str) -> u64 {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
        contents.len() as u64
    }

    #[test]
    fn missing_directory_yields_no_files() {
        let directory = tempfile::tempdir().unwrap();
        let files = jsonl_files_at_depth(&SystemLister, &directory.path().join("never-created"), 0)
            .unwrap();
        assert!(files.is_empty());
    }

    #[test]
    fn depth_zero_takes_only_jsonl_beside_root() {
        let directory = tempfile::tempdir().unwrap();
        write_transcript(&directory.path().join("session.jsonl"));
        write_transcript(&directory.path().join("notes.txt"));
        write_transcript(&directory.path().join("nested/deeper.jsonl"));

        let files = jsonl_files_at_depth(&SystemLister, directory.path(), 0).unwrap();
        assert_eq!(files, vec![directory.path().join("session.jsonl")]);
    }

    #[test]
    fn depth_one_skips_transcripts_in_root_itself() {
        let directory = tempfile::tempdir().unwrap();
        write_transcript(&directory.path().join("stray.jsonl"));
        write_transcript(&directory.path().join("project-b/second.jsonl"));
        write_transcript(&directory.path().join("project-a/first.jsonl"));

        let files = jsonl_files_at_depth(&SystemLister, directory.path(), 1).unwrap();
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

        let files = jsonl_files_at_depth(&SystemLister, directory.path(), 3).unwrap();
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

        let transcripts = transcripts_at_depth(&SystemLister, directory.path(), 3).unwrap();

        assert_eq!(
            transcripts,
            Transcripts {
                plain: vec![listed],
                compressed_count: 1,
            },
            "only a .jsonl.zst at the walked depth counts"
        );
    }

    /// The contract tests cover a directory beneath the root, for every
    /// provider.
    #[test]
    fn discovery_fails_when_its_root_cannot_be_listed() {
        let directory = tempfile::tempdir().unwrap();
        write_transcript(&directory.path().join("project/session.jsonl"));
        let lister = test_support::ScriptedLister::default();
        lister.refuse(directory.path());

        let discovery = DiscoveryLister::beneath(&lister, directory.path());

        assert!(jsonl_files_at_depth(&discovery, directory.path(), 1).is_err());
        assert!(discovery.into_unreadable_directories().is_empty());
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

        let files = jsonl_files_at_depth(&SystemLister, &root, 1).unwrap();
        assert_eq!(files, vec![root.join("linked-project/session.jsonl")]);
    }

    #[test]
    fn file_stubs_key_by_location_within_root_and_skip_vanished_files() {
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
        assert!(stubs[0].subagents.is_empty());
    }

    /// An agent can delete a sub-agent transcript between enumeration and
    /// the stat; the session it belonged to still lists, without it.
    #[test]
    fn session_stubs_span_sub_agents_that_still_exist() {
        let directory = tempfile::tempdir().unwrap();
        let session = directory.path().join("session.jsonl");
        let subagent = directory.path().join("agents/agent-0/wire.jsonl");
        let session_bytes = write_bytes(&session, "{}\n");
        let subagent_bytes = write_bytes(&subagent, "{}\n{}\n");

        let root = SessionRoot::new(directory.path());
        let stubs = session_stubs(
            &root,
            vec![SessionFiles {
                transcript: session.clone(),
                subagents: vec![
                    subagent.clone(),
                    directory.path().join("agents/gone/wire.jsonl"),
                ],
            }],
        );

        assert_eq!(stubs.len(), 1);
        assert_eq!(stubs[0].locator, session);
        assert_eq!(
            stubs[0].subagents,
            vec![subagent],
            "the vanished sub-agent is dropped"
        );
        assert_eq!(stubs[0].cache_key, "session.jsonl");
        assert_eq!(
            stubs[0].fingerprint.size,
            session_bytes + subagent_bytes,
            "the fingerprint spans the session and its sub-agents"
        );
    }
}
