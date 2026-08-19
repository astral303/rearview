//! OMP sessions, stored under `~/.omp/agent/sessions/`.

use super::{SessionCache, SessionProvider, SourceLabels};
use crate::history::Source;

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

    fn session_cache(&self) -> Option<SessionCache> {
        Some(SessionCache {
            directory: "omp",
            magic: *b"OMHIST01",
            schema_version: 1,
        })
    }
}
