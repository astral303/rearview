//! Pi coding agent sessions, stored under `~/.pi/agent/sessions/`.

use super::{
    PathResumeLauncher, RefNamespaces, SessionCache, SessionLauncher, SessionProvider, SessionRoot,
    SessionStorage, SessionStub, SourceLabels, walk,
};
use crate::cli::DebugLevel;
use crate::error::Result;
use crate::history::format::{self, SessionFormat, pi_log};
use crate::history::{Conversation, Source, parser, pi_loader};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

pub struct PiProvider;

impl SessionProvider for PiProvider {
    fn source(&self) -> Source {
        Source::Pi
    }

    fn labels(&self) -> SourceLabels {
        SourceLabels {
            name: "pi",
            list: "Pi",
            display: "Pi",
        }
    }

    fn ref_namespaces(&self) -> RefNamespaces {
        RefNamespaces {
            conversation: Some("agent-pi-v1"),
            project: "agent-pi-project-v1",
        }
    }

    fn storage(&self) -> Option<&dyn SessionStorage> {
        Some(&PiStorage)
    }

    fn format(&self) -> Option<&dyn SessionFormat> {
        Some(&pi_log::PI_LOG)
    }

    fn launcher(&self) -> &dyn SessionLauncher {
        &LAUNCHER
    }

    fn rename_session(&self, path: &Path, title: &str) -> Result<()> {
        pi_log::append_session_rename(path, title)
    }

    fn delete_session(&self, path: &Path) -> Result<usize> {
        format::require_owned_transcript(Source::Pi, path)?;
        std::fs::remove_file(path)?;
        Ok(1)
    }

    /// A Pi session states its id in its header, not its file name, and two
    /// logs in one project may state the same id. Pi sessions resolve by id
    /// only once listed.
    fn resolve_session_id(&self, _session_id: &str) -> Result<Option<PathBuf>> {
        Ok(None)
    }

    /// The id is in the header, so finding a session means reading the header
    /// of every log under the root — affordable once, not per keystroke. Two
    /// logs in one project may state the same id, so more than one can match.
    fn find_sessions_by_id(&self, session_id: &str) -> Result<Vec<PathBuf>> {
        let root = pi_loader::session_root()?;
        pi_log::sessions_with_id(&root.root.path, root.depth, session_id)
    }
}

static LAUNCHER: PathResumeLauncher = PathResumeLauncher {
    program: "pi",
    resume_flag: "--session",
    fork_flag: "--fork",
};

struct PiStorage;

impl SessionStorage for PiStorage {
    fn source(&self) -> Source {
        Source::Pi
    }

    fn cache(&self) -> SessionCache {
        SessionCache {
            directory: "pi",
            magic: *b"PIHIST01",
            schema_version: 5,
        }
    }

    fn roots(&self) -> Result<Vec<SessionRoot>> {
        Ok(vec![pi_loader::session_root()?.root])
    }

    /// The walk depth belongs to the resolution that produced the root, so it
    /// is re-resolved here rather than guessed from the path.
    fn discover(&self, root: &SessionRoot) -> Result<Vec<SessionStub>> {
        let depth = pi_loader::session_root()?.depth;
        Ok(walk::file_stubs(
            root,
            walk::jsonl_files_at_depth(&root.path, depth)?,
        ))
    }

    fn parse_session(
        &self,
        path: PathBuf,
        _root: &SessionRoot,
        modified: Option<SystemTime>,
        debug_level: Option<DebugLevel>,
    ) -> Result<Option<Conversation>> {
        parser::process_session_file(path, &pi_log::PI_LOG, modified, debug_level)
    }

    fn max_session_bytes(&self) -> Option<u64> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delete_removes_only_the_transcript_pi_owns() {
        let directory = tempfile::tempdir().unwrap();
        let session = directory.path().join("session.jsonl");
        std::fs::copy(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/pi/v1.jsonl"),
            &session,
        )
        .unwrap();
        let sibling = directory.path().join("sibling.txt");
        std::fs::write(&sibling, "keep").unwrap();

        PiProvider.delete_session(&session).unwrap();

        assert!(!session.exists());
        assert!(sibling.exists());
        assert!(
            PiProvider.delete_session(&sibling).is_err(),
            "a file Pi does not own must survive a delete aimed at it"
        );
    }
}
