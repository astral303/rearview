//! OMP sessions, stored under `~/.omp/agent/sessions/`.

use super::{
    PathResumeLauncher, SessionCache, SessionLauncher, SessionProvider, SessionRoot,
    SessionStorage, SourceLabels,
};
use crate::cli::DebugLevel;
use crate::error::Result;
use crate::history::format::{SessionFormat, pi_log};
use crate::history::{Conversation, Source, omp_loader, parser};
use std::path::PathBuf;
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
}

static LAUNCHER: PathResumeLauncher = PathResumeLauncher {
    program: "omp",
    resume_flag: "--resume",
    fork_flag: "--fork",
};

struct OmpStorage;

impl SessionStorage for OmpStorage {
    fn cache(&self) -> SessionCache {
        SessionCache {
            directory: "omp",
            magic: *b"OMHIST01",
            schema_version: 1,
        }
    }

    fn roots(&self) -> Result<Vec<SessionRoot>> {
        Ok(vec![omp_loader::session_root()?])
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
        if root_is_omp_owned(root) {
            parser::process_session_file(path, &pi_log::OMP_LOG, modified, debug_level)
        } else {
            parser::process_conversation_file(path, modified, debug_level)
        }
    }

    fn max_session_bytes(&self) -> Option<u64> {
        None
    }
}

fn root_is_omp_owned(root: &SessionRoot) -> bool {
    root.path
        .components()
        .any(|component| matches!(component.as_os_str().to_str(), Some(".omp" | "omp")))
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

    /// A transcript with no OMP title slot cannot say which agent wrote it, so the
    /// tree holding it decides. Attributing it wrongly would also mislabel every
    /// assistant turn in the viewer.
    #[test]
    fn an_untitled_transcript_belongs_to_the_tree_that_holds_it() {
        let directory = tempfile::tempdir().unwrap();
        assert_eq!(
            parsed_source(SessionRoot::flat(
                directory.path().join(".omp/agent/sessions")
            )),
            Source::Omp
        );
        assert_eq!(
            parsed_source(SessionRoot::flat(directory.path().join("redirected"))),
            Source::Pi
        );
    }
}
