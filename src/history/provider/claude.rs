//! Claude Code sessions, stored under `~/.claude/projects/<encoded-cwd>/`.

use super::{SessionCache, SessionProvider, SourceLabels};
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

    /// Claude's transcripts are already partitioned by project directory and are
    /// cached that way, so there is nothing to cache per session root.
    fn session_cache(&self) -> Option<SessionCache> {
        None
    }
}
