//! Claude Code sessions, stored under `~/.claude/projects/<encoded-cwd>/`.

use super::{SessionProvider, SessionStorage, SourceLabels};
use crate::history::Source;
use crate::history::format::SessionFormat;

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

    /// Claude's transcripts are partitioned by project directory rather than
    /// gathered under a session root: discovery excludes `agent-*.jsonl`, caching
    /// is per project, and loading streams project batches to the TUI. None of
    /// that fits [`SessionStorage`], so Claude keeps its own loader.
    fn storage(&self) -> Option<&dyn SessionStorage> {
        None
    }

    /// Claude writes [`LogEntry`](crate::claude::LogEntry) records directly, with
    /// no session header stating an id, start time or cwd. There is nothing to
    /// project, so a file no other format claims is read as a Claude transcript.
    fn format(&self) -> Option<&dyn SessionFormat> {
        None
    }
}
