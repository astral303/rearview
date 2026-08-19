//! Claude Code sessions, stored under `~/.claude/projects/<encoded-cwd>/`.

use super::{SessionProvider, SourceLabels};
use crate::history::Source;

pub struct ClaudeProvider;

impl SessionProvider for ClaudeProvider {
    fn source(&self) -> Source {
        Source::Claude
    }

    fn labels(&self) -> SourceLabels {
        SourceLabels {
            name: "claude",
            list: "CC",
        }
    }
}
