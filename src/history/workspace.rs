//! Scoping sessions to a directory.

use super::path::{convert_path_to_project_dir_name, is_same_project};
use super::{Conversation, Source};
use crate::error::Result;
use std::path::PathBuf;

/// A directory to scope sessions to.
#[derive(Clone, Debug)]
pub struct Workspace {
    /// Canonical when the directory exists, so a session that recorded the
    /// directory through a symlink still matches.
    canonical_dir: PathBuf,
    /// Claude's encoding of the directory. It groups a repository with its
    /// worktrees.
    project_dir_name: String,
}

impl Workspace {
    pub fn current() -> Result<Self> {
        Ok(Self::at(std::env::current_dir()?))
    }

    pub fn at(dir: PathBuf) -> Self {
        let project_dir_name = convert_path_to_project_dir_name(&dir);
        let canonical_dir = dir.canonicalize().unwrap_or(dir);
        Self {
            canonical_dir,
            project_dir_name,
        }
    }

    /// True when `conversation` was recorded in this workspace.
    ///
    /// A Claude session matches by encoded project directory name, so a
    /// repository's worktrees match with it. Any other agent's session matches
    /// by the directory it recorded; that is all those agents store.
    pub fn contains(&self, conversation: &Conversation) -> bool {
        if conversation.source != Source::Claude {
            return conversation
                .project_path
                .as_ref()
                .or(conversation.cwd.as_ref())
                .is_some_and(|dir| {
                    dir.canonicalize().unwrap_or_else(|_| dir.clone()) == self.canonical_dir
                });
        }
        conversation
            .path
            .parent()
            .and_then(|parent| parent.file_name())
            .is_some_and(|name| is_same_project(&name.to_string_lossy(), &self.project_dir_name))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::test_fixtures::one_message_conversation;
    use std::path::Path;

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
