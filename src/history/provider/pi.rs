//! Pi coding agent sessions, stored under `~/.pi/agent/sessions/`.

use super::{
    PathResumeLauncher, SessionCache, SessionLauncher, SessionProvider, SessionRoot,
    SessionStorage, SourceLabels,
};
use crate::cli::DebugLevel;
use crate::error::Result;
use crate::history::format::{SessionFormat, pi_log};
use crate::history::{Conversation, Source, parser, pi_loader};
use std::path::PathBuf;
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
}

static LAUNCHER: PathResumeLauncher = PathResumeLauncher {
    program: "pi",
    resume_flag: "--session",
    fork_flag: "--fork",
};

struct PiStorage;

impl SessionStorage for PiStorage {
    fn cache(&self) -> SessionCache {
        SessionCache {
            directory: "pi",
            magic: *b"PIHIST01",
            schema_version: 1,
        }
    }

    fn roots(&self) -> Result<Vec<SessionRoot>> {
        Ok(vec![pi_loader::session_root()?])
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
