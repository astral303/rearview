//! Scoping sessions to a directory.

use super::Conversation;
use crate::error::Result;
use std::path::{Path, PathBuf};

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
