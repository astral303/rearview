//! OMP sessions, stored under `~/.omp/agent/sessions/`.

use super::{SessionProvider, SourceLabels};
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
}
