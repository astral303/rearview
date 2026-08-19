//! Pi coding agent sessions, stored under `~/.pi/agent/sessions/`.

use super::{SessionProvider, SourceLabels};
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
}
