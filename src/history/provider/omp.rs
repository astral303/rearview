//! OMP sessions, stored under `~/.omp/agent/sessions/`.

use super::{SessionCache, SessionProvider, SessionRoot, SessionStorage, SourceLabels};
use crate::cli::DebugLevel;
use crate::error::Result;
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
}

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

    /// OMP's own tree holds OMP transcripts, but a redirected session directory
    /// may hold Pi-format files, which have to be sniffed rather than assumed.
    fn parse_session(
        &self,
        path: PathBuf,
        root: &SessionRoot,
        modified: Option<SystemTime>,
        debug_level: Option<DebugLevel>,
    ) -> Result<Option<Conversation>> {
        if root_is_omp_owned(root) {
            parser::process_omp_conversation_file(path, modified, debug_level)
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
