//! Pi coding agent sessions, stored under `~/.pi/agent/sessions/`.

use super::{SessionCache, SessionProvider, SourceLabels};
use crate::history::Source;

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

    fn session_cache(&self) -> Option<SessionCache> {
        Some(SessionCache {
            directory: "pi",
            magic: *b"PIHIST01",
            schema_version: 1,
        })
    }
}
