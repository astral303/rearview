//! OMP sessions, stored under `~/.omp/agent/sessions/`.

use super::{
    PathResumeLauncher, RefNamespaces, RootOrigin, SessionCache, SessionLauncher, SessionProvider,
    SessionRoot, SessionStorage, SessionStub, SourceLabels, walk,
};
use crate::cli::DebugLevel;
use crate::error::Result;
use crate::history::format::{self, SessionFormat, pi_log};
use crate::history::{Conversation, Source, omp_loader, parser};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

pub struct OmpProvider;

impl SessionProvider for OmpProvider {
    fn source(&self) -> Source {
        Source::Omp
    }

    fn labels(&self) -> SourceLabels {
        SourceLabels {
            name: "omp",
            list: "OMP",
            display: "OMP",
        }
    }

    fn ref_namespaces(&self) -> RefNamespaces {
        RefNamespaces {
            conversation: Some("agent-omp-v1"),
            project: "agent-omp-project-v1",
        }
    }

    fn storage(&self) -> Option<&dyn SessionStorage> {
        Some(&OmpStorage)
    }

    fn format(&self) -> Option<&dyn SessionFormat> {
        Some(&pi_log::OMP_LOG)
    }

    fn launcher(&self) -> &dyn SessionLauncher {
        &LAUNCHER
    }

    fn rename_session(&self, path: &Path, title: &str) -> Result<()> {
        pi_log::append_omp_session_rename(path, title)
    }

    /// OMP keeps a session's tool results in a directory named after the
    /// transcript, so deleting the transcript alone would orphan it.
    fn delete_session(&self, path: &Path) -> Result<usize> {
        format::require_owned_transcript(Source::Omp, path)?;
        std::fs::remove_file(path)?;
        let artifacts = path.with_extension("");
        if artifacts.is_dir() {
            std::fs::remove_dir_all(artifacts)?;
        }
        Ok(1)
    }

    /// An OMP session states its id in its header, not its file name, as a Pi
    /// session does. OMP sessions resolve by id only once listed.
    fn resolve_session_id(&self, _session_id: &str) -> Result<Option<PathBuf>> {
        Ok(None)
    }

    /// Read from the header, as Pi's is, and just as capable of repeating
    /// across two logs in one project.
    fn find_sessions_by_id(&self, session_id: &str) -> Result<Vec<PathBuf>> {
        let root = omp_loader::session_root()?;
        pi_log::sessions_with_id(&root.root.path, root.depth, session_id)
    }
}

static LAUNCHER: PathResumeLauncher = PathResumeLauncher {
    program: "omp",
    resume_flag: "--resume",
    fork_flag: "--fork",
};

struct OmpStorage;

impl SessionStorage for OmpStorage {
    fn source(&self) -> Source {
        Source::Omp
    }

    fn cache(&self) -> SessionCache {
        SessionCache {
            directory: "omp",
            magic: *b"OMHIST01",
            schema_version: 4,
        }
    }

    fn roots(&self) -> Result<Vec<SessionRoot>> {
        Ok(vec![omp_loader::session_root()?.root])
    }

    /// The walk depth belongs to the resolution that produced the root, so it
    /// is re-resolved here rather than guessed from the path.
    fn discover(&self, root: &SessionRoot) -> Result<Vec<SessionStub>> {
        let depth = omp_loader::session_root()?.depth;
        Ok(walk::file_stubs(
            root,
            walk::jsonl_files_at_depth(&root.path, depth)?,
        ))
    }

    /// Everything under OMP's own tree is OMP's, title slot or not. A redirected
    /// session directory belongs to no one in particular, so its files are left to
    /// the registry to attribute rather than claimed outright.
    fn parse_session(
        &self,
        path: PathBuf,
        root: &SessionRoot,
        modified: Option<SystemTime>,
        debug_level: Option<DebugLevel>,
    ) -> Result<Option<Conversation>> {
        match root.origin() {
            RootOrigin::AgentTree => {
                parser::process_session_file(path, &pi_log::OMP_LOG, modified, debug_level)
            }
            RootOrigin::Redirected => {
                parser::process_conversation_file(path, modified, debug_level)
            }
        }
    }

    fn max_session_bytes(&self) -> Option<u64> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed_source(root: SessionRoot) -> Source {
        std::fs::create_dir_all(&root.path).unwrap();
        let path = root.path.join("session.jsonl");
        std::fs::copy(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/pi/v3-branched.jsonl"),
            &path,
        )
        .unwrap();
        OmpStorage
            .parse_session(path, &root, None, None)
            .unwrap()
            .unwrap()
            .source
    }

    #[test]
    fn deleting_a_session_takes_its_artifact_directory_with_it() {
        let directory = tempfile::tempdir().unwrap();
        let session = directory.path().join("session.jsonl");
        std::fs::copy(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/omp/v3.jsonl"),
            &session,
        )
        .unwrap();
        let artifacts = directory.path().join("session");
        std::fs::create_dir_all(artifacts.join("tool-results")).unwrap();
        std::fs::write(artifacts.join("tool-results/output.txt"), "result").unwrap();

        OmpProvider.delete_session(&session).unwrap();

        assert!(!session.exists());
        assert!(
            !artifacts.exists(),
            "tool results named after the transcript would be orphaned"
        );
    }

    /// A transcript with no OMP title slot cannot say which agent wrote it, so the
    /// tree holding it decides. Attributing it wrongly would also mislabel every
    /// assistant turn in the viewer.
    #[test]
    fn an_untitled_transcript_belongs_to_the_tree_that_holds_it() {
        let directory = tempfile::tempdir().unwrap();
        assert_eq!(
            parsed_source(SessionRoot::new(directory.path().join("own-tree")).in_agent_tree()),
            Source::Omp
        );
        assert_eq!(
            parsed_source(SessionRoot::new(directory.path().join("redirected"))),
            Source::Pi,
            "a directory the user redirected OMP to can hold Pi's transcripts"
        );
    }
}
