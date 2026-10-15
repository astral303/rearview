//! Scoping sessions to a directory.

use super::Conversation;
use crate::error::Result;
use std::path::{Component, Path, PathBuf};

/// A directory to scope sessions to.
#[derive(Clone, Debug)]
pub struct Workspace {
    directory: PathBuf,
    /// Canonical when the directory exists, so a session that recorded the
    /// directory through a symlink still matches.
    canonical_dir: PathBuf,
}

impl Workspace {
    pub fn current() -> Result<Self> {
        Ok(Self::at(std::env::current_dir()?))
    }

    pub fn at(directory: PathBuf) -> Self {
        let canonical_dir = directory
            .canonicalize()
            .unwrap_or_else(|_| directory.clone());
        Self {
            directory,
            canonical_dir,
        }
    }

    /// The directory as given, not canonicalized: an agent that names a
    /// folder after the directory it ran in names it after this spelling.
    pub fn directory(&self) -> &Path {
        &self.directory
    }

    /// True when `dir` names this workspace's directory, however each was
    /// spelled.
    pub fn is_directory(&self, dir: &Path) -> bool {
        dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf()) == self.canonical_dir
    }

    /// True when `conversation` was recorded in this workspace, by the rule
    /// of the agent that recorded it.
    pub fn contains(&self, conversation: &Conversation) -> bool {
        conversation.source.provider().is_in_workspace(
            self,
            &conversation.path,
            conversation
                .project_path
                .as_deref()
                .or(conversation.cwd.as_deref()),
        )
    }
}

/// The directories whose sessions belong to `directory`, nearest first:
/// `directory`, each parent up to the repository's top level, and from a
/// linked worktree the main checkout's top level. The top level is the
/// nearest directory holding a `.git` directory or file. Outside a
/// repository, `directory` alone: walking further up would reach the home
/// directory's sessions.
pub fn candidate_directories(directory: &Path) -> Vec<PathBuf> {
    let Some(top_level) = directory
        .ancestors()
        .find(|ancestor| ancestor.join(".git").exists())
    else {
        return vec![directory.to_path_buf()];
    };
    let mut candidates: Vec<PathBuf> = directory
        .ancestors()
        .take_while(|ancestor| *ancestor != top_level)
        .map(Path::to_path_buf)
        .collect();
    candidates.push(top_level.to_path_buf());
    candidates.extend(main_checkout_of(top_level));
    candidates
}

/// The main checkout's top level when `top_level` is a linked worktree:
/// its `.git` file names the worktree's git directory, and that directory's
/// `commondir` names the repository's shared git directory. Read from the
/// files, without running git. A submodule's git directory has no
/// `commondir`.
fn main_checkout_of(top_level: &Path) -> Option<PathBuf> {
    let git_file = std::fs::read_to_string(top_level.join(".git")).ok()?;
    let git_dir = top_level.join(git_file.lines().next()?.strip_prefix("gitdir:")?.trim());
    let common_dir = std::fs::read_to_string(git_dir.join("commondir")).ok()?;
    checkout_of_git_dir(&lexically_normal(&git_dir.join(common_dir.trim())))
}

/// The checkout whose `.git` directory is `git_dir`. `None` for a bare
/// repository: its git directory is not named `.git`, and no checkout holds
/// it.
fn checkout_of_git_dir(git_dir: &Path) -> Option<PathBuf> {
    if git_dir.file_name()? != ".git" {
        return None;
    }
    git_dir.parent().map(Path::to_path_buf)
}

/// `path` with `.` and `..` components resolved by their names, and its
/// separators in the platform's spelling.
fn lexically_normal(path: &Path) -> PathBuf {
    let mut normal = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normal.pop();
            }
            other => normal.push(other),
        }
    }
    normal
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::Source;
    use crate::history::provider::claude::convert_path_to_project_dir_name;
    use crate::search::test_fixtures::one_message_conversation;

    fn session(source: Source, path: &str, cwd: Option<&str>) -> Conversation {
        let mut conversation =
            one_message_conversation("hello", chrono::Local::now(), None, None, None);
        conversation.source = source;
        conversation.path = PathBuf::from(path);
        conversation.cwd = cwd.map(PathBuf::from);
        conversation
    }

    fn claude_session_under(project_dir: &Path) -> Conversation {
        let project_dir_name = convert_path_to_project_dir_name(project_dir);
        session(
            Source::Claude,
            &format!("/claude/projects/{project_dir_name}/session.jsonl"),
            None,
        )
    }

    #[test]
    fn a_subdirectorys_candidates_run_up_to_the_top_level() {
        let repository = tempfile::tempdir().unwrap();
        std::fs::create_dir(repository.path().join(".git")).unwrap();
        let subdirectory = repository.path().join("src").join("bin");
        std::fs::create_dir_all(&subdirectory).unwrap();

        assert_eq!(
            candidate_directories(&subdirectory),
            vec![
                subdirectory.clone(),
                repository.path().join("src"),
                repository.path().to_path_buf(),
            ]
        );
    }

    /// Git for Windows writes `gitdir:` with forward slashes.
    #[test]
    fn a_linked_worktrees_candidates_end_with_the_main_checkout() {
        let checkouts = tempfile::tempdir().unwrap();
        let main = checkouts.path().join("main");
        let worktree_git_dir = main.join(".git").join("worktrees").join("lane");
        std::fs::create_dir_all(&worktree_git_dir).unwrap();
        std::fs::write(worktree_git_dir.join("commondir"), "../..\n").unwrap();
        let worktree = checkouts.path().join("lane");
        std::fs::create_dir(&worktree).unwrap();
        std::fs::write(
            worktree.join(".git"),
            format!(
                "gitdir: {}\n",
                worktree_git_dir.display().to_string().replace('\\', "/")
            ),
        )
        .unwrap();

        assert_eq!(candidate_directories(&worktree), vec![worktree, main]);
    }

    /// A submodule's `.git` file names its git directory inside the
    /// superproject's, which holds no `commondir`.
    #[test]
    fn a_submodules_candidates_end_at_its_own_top_level() {
        let superproject = tempfile::tempdir().unwrap();
        let module_git_dir = superproject.path().join(".git").join("modules").join("lib");
        std::fs::create_dir_all(&module_git_dir).unwrap();
        let submodule = superproject.path().join("lib");
        std::fs::create_dir(&submodule).unwrap();
        std::fs::write(
            submodule.join(".git"),
            format!("gitdir: {}\n", module_git_dir.display()),
        )
        .unwrap();

        assert_eq!(candidate_directories(&submodule), vec![submodule]);
    }

    #[test]
    fn outside_a_repository_the_directory_is_the_only_candidate() {
        let parent = tempfile::tempdir().unwrap();
        let directory = parent.path().join("project");
        std::fs::create_dir(&directory).unwrap();

        assert_eq!(candidate_directories(&directory), vec![directory]);
    }

    #[test]
    fn a_claude_worktree_session_is_in_its_repositorys_workspace() {
        let workspace = Workspace::at(PathBuf::from("/Users/raine/code/project"));
        let session = claude_session_under(Path::new("/Users/raine/code/project/.worktrees/fix"));

        assert!(workspace.contains(&session));
    }

    #[test]
    fn a_claude_session_from_another_project_is_not() {
        let workspace = Workspace::at(PathBuf::from("/Users/raine/code/project"));
        let session = claude_session_under(Path::new("/Users/raine/code/elsewhere"));

        assert!(!workspace.contains(&session));
    }

    #[test]
    fn another_agents_session_is_in_the_workspace_it_recorded() {
        let workspace = Workspace::at(PathBuf::from("/Users/raine/code/project"));

        assert!(workspace.contains(&session(
            Source::Codex,
            "/codex/rollout.jsonl",
            Some("/Users/raine/code/project"),
        )));
        assert!(!workspace.contains(&session(
            Source::Codex,
            "/codex/rollout.jsonl",
            Some("/Users/raine/elsewhere"),
        )));
        assert!(!workspace.contains(&session(Source::Codex, "/codex/rollout.jsonl", None)));
    }
}
