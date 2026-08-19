//! Where a provider keeps its session transcripts, and how to enumerate them.

use crate::error::Result;
use std::fs::read_dir;
use std::path::{Path, PathBuf};

/// How transcripts are arranged beneath a [`SessionRoot`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionLayout {
    /// Transcripts sit directly in the root, as when the user pins a session
    /// directory and every project shares it.
    Flat,
    /// Transcripts sit one level down, in a directory per project.
    ChildDirectories,
}

/// Where a root came from, which decides whether a transcript in it that names
/// no agent belongs to the provider that listed the root.
///
/// Only the code that resolves a root knows this. It cannot be recovered from
/// the path afterwards: a home directory can be named after an agent, and a
/// redirected directory can sit anywhere.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RootOrigin {
    /// The agent's own installation tree. A transcript here is that agent's even
    /// when the file itself says nothing about which agent wrote it.
    AgentTree,
    /// A directory the user pointed the agent at, through configuration or the
    /// environment. Pi-family agents share one wire format and can be aimed at
    /// the same directory, so an unmarked transcript here names no owner.
    Redirected,
}

/// A directory a provider stores sessions under, together with the arrangement
/// of transcripts inside it.
///
/// Roots are [`RootOrigin::Redirected`] unless [`SessionRoot::in_agent_tree`]
/// says otherwise. That default is the cautious one: a provider that does not
/// declare a root as its own cannot claim transcripts that name no agent.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionRoot {
    pub path: PathBuf,
    layout: SessionLayout,
    origin: RootOrigin,
}

impl SessionRoot {
    pub fn flat(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            layout: SessionLayout::Flat,
            origin: RootOrigin::Redirected,
        }
    }

    pub fn child_directories(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            layout: SessionLayout::ChildDirectories,
            origin: RootOrigin::Redirected,
        }
    }

    /// Mark this root as part of the agent's own tree rather than a directory
    /// the user redirected it to.
    #[must_use]
    pub fn in_agent_tree(self) -> Self {
        Self {
            origin: RootOrigin::AgentTree,
            ..self
        }
    }

    pub fn origin(&self) -> RootOrigin {
        self.origin
    }

    /// Transcript paths under this root, sorted so successive runs agree.
    ///
    /// A root that does not exist yields no files rather than an error: an agent
    /// the user has not installed is an absence, not a failure.
    pub fn discover_files(&self) -> Result<Vec<PathBuf>> {
        if !self.path.exists() {
            return Ok(Vec::new());
        }
        let mut files = Vec::new();
        match self.layout {
            SessionLayout::Flat => collect_transcripts(&self.path, &mut files)?,
            SessionLayout::ChildDirectories => {
                for entry in read_dir(&self.path)? {
                    let path = entry?.path();
                    // `is_dir` follows symlinks, so a project directory linked
                    // into the root is still searched.
                    if path.is_dir() {
                        collect_transcripts(&path, &mut files)?;
                    }
                }
            }
        }
        files.sort();
        Ok(files)
    }
}

fn collect_transcripts(directory: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    for entry in read_dir(directory)? {
        let path = entry?.path();
        if path.extension().and_then(|extension| extension.to_str()) == Some("jsonl") {
            files.push(path);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_transcript(path: &Path) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "{}\n").unwrap();
    }

    #[test]
    fn missing_root_yields_no_files() {
        let directory = tempfile::tempdir().unwrap();
        let root = SessionRoot::flat(directory.path().join("never-created"));
        assert!(root.discover_files().unwrap().is_empty());
    }

    #[test]
    fn flat_layout_takes_only_jsonl_beside_the_root() {
        let directory = tempfile::tempdir().unwrap();
        write_transcript(&directory.path().join("session.jsonl"));
        write_transcript(&directory.path().join("notes.txt"));
        write_transcript(&directory.path().join("nested/deeper.jsonl"));

        let files = SessionRoot::flat(directory.path())
            .discover_files()
            .unwrap();
        assert_eq!(files, vec![directory.path().join("session.jsonl")]);
    }

    #[test]
    fn child_directory_layout_skips_transcripts_in_the_root_itself() {
        let directory = tempfile::tempdir().unwrap();
        write_transcript(&directory.path().join("stray.jsonl"));
        write_transcript(&directory.path().join("project-b/second.jsonl"));
        write_transcript(&directory.path().join("project-a/first.jsonl"));

        let files = SessionRoot::child_directories(directory.path())
            .discover_files()
            .unwrap();
        assert_eq!(
            files,
            vec![
                directory.path().join("project-a/first.jsonl"),
                directory.path().join("project-b/second.jsonl"),
            ],
            "child directories are searched, the root is not, and results are sorted"
        );
    }

    #[cfg(unix)]
    #[test]
    fn child_directory_layout_follows_symlinked_project_directories() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("elsewhere");
        let root = directory.path().join("sessions");
        write_transcript(&target.join("session.jsonl"));
        std::fs::create_dir_all(&root).unwrap();
        symlink(&target, root.join("linked-project")).unwrap();

        let files = SessionRoot::child_directories(&root)
            .discover_files()
            .unwrap();
        assert_eq!(files, vec![root.join("linked-project/session.jsonl")]);
    }
}
