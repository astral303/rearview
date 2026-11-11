//! Short project names from paths, the project names
//! `tui.exclude_projects` and `agent.exclude_projects` hide, and paths
//! spelled with the platform's separator.

use super::Conversation;
use super::provider::walk::Canonicaliser;
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

/// `path` with each separator spelled as the platform's own: on Windows,
/// every `/` becomes `\`; elsewhere `path` is returned as it is. The change
/// is a string operation and doesn't ask the file system. Environment
/// variables, the home, current and temporary directories, session roots and
/// a path an agent command names pass through it, so that a session path
/// built below one doesn't mix separators. Paths read from transcripts don't
/// pass through it.
pub(crate) fn with_platform_separators(path: impl Into<OsString>) -> PathBuf {
    let path = path.into();
    #[cfg(windows)]
    {
        use std::os::windows::ffi::{OsStrExt, OsStringExt};
        const SLASH: u16 = b'/' as u16;
        const BACKSLASH: u16 = b'\\' as u16;
        if path.encode_wide().any(|unit| unit == SLASH) {
            let units: Vec<u16> = path
                .encode_wide()
                .map(|unit| if unit == SLASH { BACKSLASH } else { unit })
                .collect();
            return PathBuf::from(OsString::from_wide(&units));
        }
    }
    PathBuf::from(path)
}

/// A Windows path without the `\\?\` or `\\?\UNC\` prefix `canonicalize`
/// adds. OMP writes paths without it.
pub(crate) fn without_verbatim_prefix(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    if let Some(share) = text.strip_prefix(r"\\?\UNC\") {
        PathBuf::from(format!(r"\\{share}"))
    } else if let Some(local) = text.strip_prefix(r"\\?\") {
        PathBuf::from(local)
    } else {
        path.to_path_buf()
    }
}

/// Windows and macOS file systems ignore case by default; Linux's do not.
const FILE_NAMES_IGNORE_CASE: bool = cfg!(any(windows, target_os = "macos"));

/// The text two spellings of `path` share when both are canonical, or both
/// lie below one directory as spelled: lowercased where file names ignore
/// case.
pub(crate) fn path_key(path: &Path) -> String {
    let text = path.to_string_lossy();
    if FILE_NAMES_IGNORE_CASE {
        text.to_lowercase()
    } else {
        text.into_owned()
    }
}

/// A file's path below a root, both as spelled: the names below the root,
/// joined with the platform's separator whichever separators the path
/// mixes. Every comparison of a path with a root passes through it.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct PathBelowRoot(String);

impl PathBelowRoot {
    /// `path` below `root`; `None` when `path` does not begin with `root`,
    /// or steps through `..` below it, because only the file system
    /// canonicalises `..`. `root` itself is the empty path below it.
    pub(crate) fn of(path: &Path, root: &Path) -> Option<Self> {
        let below = path.strip_prefix(root).ok()?;
        let mut text = OsString::new();
        for component in below.components() {
            let std::path::Component::Normal(name) = component else {
                return None;
            };
            if !text.is_empty() {
                text.push(std::path::MAIN_SEPARATOR_STR);
            }
            text.push(name);
        }
        Some(Self(text.to_string_lossy().into_owned()))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }

    pub(crate) fn into_string(self) -> String {
        self.0
    }

    /// The text two spellings of the file below one root share: the
    /// [`path_key`] of the path below it.
    pub(crate) fn key(&self) -> String {
        path_key(Path::new(&self.0))
    }
}

/// The text every spelling of one file shares: the [`path_key`] of the
/// file's path with its directory canonical. Two rows with one identity
/// name one file.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct FileIdentity(String);

impl FileIdentity {
    /// The identity of `path`, whose directory is canonical: a canonical
    /// root or directory joined with the names below it.
    pub(crate) fn of_canonical(path: &Path) -> Self {
        Self(path_key(path))
    }
}

/// The canonical path of each directory asked for, canonicalised once through
/// the canonicaliser, for one command, load, lookup or workspace. Shared
/// across threads.
pub struct CanonicalDirectories {
    canonicaliser: Arc<dyn Canonicaliser>,
    /// `None` for a directory that didn't canonicalise.
    canonical_by_directory: Mutex<HashMap<PathBuf, Option<PathBuf>>>,
}

impl CanonicalDirectories {
    pub(crate) fn new(canonicaliser: Arc<dyn Canonicaliser>) -> Self {
        Self {
            canonicaliser,
            canonical_by_directory: Mutex::new(HashMap::new()),
        }
    }

    /// Directories the file system canonicalises, for a test that doesn't
    /// count canonicalisations.
    #[cfg(test)]
    pub(crate) fn of_system() -> Self {
        Self::new(super::provider::walk::SystemCanonicaliser::shared())
    }

    /// `directory`'s canonical path; `None` when it doesn't canonicalise.
    pub(crate) fn canonical(&self, directory: &Path) -> Option<PathBuf> {
        let mut canonical_by_directory = self
            .canonical_by_directory
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        canonical_by_directory
            .entry(directory.to_path_buf())
            .or_insert_with(|| self.canonicaliser.canonicalise(directory).ok())
            .clone()
    }

    /// The identity of the file at `path`, named without its root: its
    /// directory's canonical path joined with its name, or `path` as spelled
    /// when the directory doesn't canonicalise. A bare file name sits in the
    /// current directory.
    pub(crate) fn file_identity(&self, path: &Path) -> FileIdentity {
        let (Some(directory), Some(name)) = (path.parent(), path.file_name()) else {
            return FileIdentity::of_canonical(path);
        };
        let directory = if directory.as_os_str().is_empty() {
            Path::new(".")
        } else {
            directory
        };
        match self.canonical(directory) {
            Some(canonical) => FileIdentity::of_canonical(&canonical.join(name)),
            None => FileIdentity::of_canonical(path),
        }
    }
}

impl std::fmt::Debug for CanonicalDirectories {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CanonicalDirectories")
            .finish_non_exhaustive()
    }
}

/// Format a path into a short display name.
///
/// For worktree paths like `/Users/raine/code/claude-history__worktrees/claude-search`,
/// returns `claude-history/claude-search` to show both the main project and worktree name.
///
/// For regular paths, returns just the folder name.
pub fn format_short_name_from_path(path: &Path) -> String {
    let path_str = path.to_string_lossy();

    // Check for worktree pattern in the path
    if let Some(wt_pos) = path_str
        .find("__worktrees/")
        .or_else(|| path_str.find("/.worktrees/"))
    {
        let is_hidden = path_str[wt_pos..].starts_with("/.");
        let separator_len = if is_hidden {
            "/.worktrees/".len()
        } else {
            "__worktrees/".len()
        };

        // Get main project (folder before __worktrees)
        let before = &path_str[..wt_pos];
        let main_project = Path::new(before)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();

        // Get worktree name (folder after __worktrees/)
        let after = &path_str[wt_pos + separator_len..];
        let worktree = after.split('/').next().unwrap_or("");

        if !main_project.is_empty() && !worktree.is_empty() {
            return format!("{}/{}", main_project, worktree);
        }
    }

    // Not a worktree, just return the folder name
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path_str.into_owned())
}

/// The project names `tui.exclude_projects` and `agent.exclude_projects` hide.
/// A name matches the list's project name exactly, and a parent such as
/// `repo` also hides its worktree rows, such as `repo/feature`.
#[derive(Clone, Debug, Default)]
pub struct ExcludedProjects(HashSet<String>);

impl ExcludedProjects {
    /// True when `conversation`'s project name matches. A row without a
    /// project name is never excluded.
    pub fn excludes(&self, conversation: &Conversation) -> bool {
        conversation
            .project_name
            .as_deref()
            .is_some_and(|project_name| self.excludes_name(project_name))
    }

    fn excludes_name(&self, project_name: &str) -> bool {
        self.0.contains(project_name)
            || project_name
                .split_once('/')
                .is_some_and(|(parent, _)| self.0.contains(parent))
    }
}

impl FromIterator<String> for ExcludedProjects {
    fn from_iter<I: IntoIterator<Item = String>>(names: I) -> Self {
        Self(names.into_iter().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_below_root_joins_names_with_platforms_separator() {
        let root = Path::new("root");
        let below = |path: &str| PathBelowRoot::of(Path::new(path), root);

        let expected = ["2025", "01", "rollout.jsonl"].join(std::path::MAIN_SEPARATOR_STR);
        assert_eq!(
            below(&format!("root{}{expected}", std::path::MAIN_SEPARATOR)).map(|place| place.0),
            Some(expected.clone()),
            "a path below the root keeps its names"
        );
        #[cfg(windows)]
        assert_eq!(
            below(r"root\2025/01\rollout.jsonl").map(|place| place.0),
            Some(expected),
            "a path mixing separators is below the root as the platform spells it"
        );
        assert_eq!(
            below("root").map(|place| place.0),
            Some(String::new()),
            "the root is the empty path below itself"
        );
    }

    #[test]
    fn path_below_root_refuses_paths_the_file_system_canonicalises() {
        let root = Path::new("root");

        for path in ["root/../other/session.jsonl", "other/root"] {
            assert_eq!(
                PathBelowRoot::of(Path::new(path), root),
                None,
                "{path} is not below the root as spelled"
            );
        }
    }

    // === format_project_short_name tests (worktree display) ===

    #[test]
    fn format_short_name_extracts_worktree_pattern() {
        // Test the worktree pattern detection in decoded paths
        let path = "/Users/raine/code/WalkingMate__worktrees/template-engine";

        // Check for worktree pattern
        assert!(path.contains("__worktrees/"));

        // Extract main project
        let wt_pos = path.find("__worktrees/").unwrap();
        let before = &path[..wt_pos];
        let main_project = Path::new(before)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap();
        assert_eq!(main_project, "WalkingMate");

        // Extract worktree name
        let after = &path[wt_pos + "__worktrees/".len()..];
        let worktree = after.split('/').next().unwrap();
        assert_eq!(worktree, "template-engine");

        // Combined display
        let display = format!("{}/{}", main_project, worktree);
        assert_eq!(display, "WalkingMate/template-engine");
    }

    #[test]
    fn format_short_name_hidden_worktrees() {
        // Test .worktrees pattern (hidden worktrees folder)
        let path = "/Users/raine/code/workmux/.worktrees/uncommitted";

        // Check for hidden worktree pattern
        assert!(path.contains("/.worktrees/"));

        let wt_pos = path.find("/.worktrees/").unwrap();
        let before = &path[..wt_pos];
        let main_project = Path::new(before)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap();
        assert_eq!(main_project, "workmux");

        let after = &path[wt_pos + "/.worktrees/".len()..];
        let worktree = after.split('/').next().unwrap();
        assert_eq!(worktree, "uncommitted");
    }

    // === Excluded project names ===

    fn row_in_project(project_name: Option<&str>) -> Conversation {
        crate::search::test_fixtures::one_message_conversation(
            "hello",
            chrono::Local::now(),
            None,
            None,
            project_name,
        )
    }

    fn excluded(names: &[&str]) -> ExcludedProjects {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    #[test]
    fn excluded_name_matches_its_project_exactly() {
        let excluded = excluded(&["repo"]);

        assert!(excluded.excludes(&row_in_project(Some("repo"))));
        assert!(!excluded.excludes(&row_in_project(Some("Repo"))));
        assert!(!excluded.excludes(&row_in_project(Some("repo-two"))));
    }

    #[test]
    fn excluded_parent_hides_its_worktree_rows() {
        let excluded = excluded(&["repo"]);

        assert!(excluded.excludes(&row_in_project(Some("repo/feature"))));
        assert!(!excluded.excludes(&row_in_project(Some("other/repo"))));
    }

    #[test]
    fn row_without_project_name_is_not_excluded() {
        assert!(!excluded(&["repo"]).excludes(&row_in_project(None)));
    }

    #[test]
    fn platform_separators_replace_forward_slashes_on_windows_alone() {
        let spelled = with_platform_separators("C:/Users/x/.claude\\projects");

        let expected = if cfg!(windows) {
            r"C:\Users\x\.claude\projects"
        } else {
            "C:/Users/x/.claude\\projects"
        };
        assert_eq!(
            spelled.as_os_str(),
            expected,
            "forward slashes become the platform's separator on Windows, and stay elsewhere"
        );
    }
}
