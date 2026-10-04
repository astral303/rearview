//! A conversation's entries as the viewer, the terminal printout, exports and
//! the clipboard copy read them. Search and the agent commands read the
//! `agent_progress` shape unconverted, since they index and print it.

use super::{MalformedLine, Source, TranscriptEntries, normalized_session, sniffed_session};
use crate::error::Result;
use crate::log_entry::{
    LogEntry, SubagentIdentity, agent_progress_identity, convert_agent_progress,
};
use std::path::{Path, PathBuf};

/// The entries the viewer, the terminal printout, exports and the clipboard
/// copy read. Each `agent_progress` record, spliced from a sub-agent
/// transcript or recorded inline, is converted into the sub-agent message it
/// carries, so these outputs handle one shape for a sub-agent's messages. Keep
/// one entry per record: the viewer and the clipboard copy address an entry by
/// its position.
pub struct DisplayEntries {
    pub entries: Vec<LogEntry>,
    /// The identity each `agent_progress` record names, by its agent id and
    /// in record order. The converted message no longer carries it.
    pub subagent_identities: Vec<(String, SubagentIdentity)>,
    /// The lines of a Claude session file that did not parse, which the
    /// terminal printout reports under `--debug`.
    pub(crate) malformed_lines: Vec<MalformedLine>,
}

impl From<Vec<(usize, LogEntry)>> for DisplayEntries {
    fn from(normalized: Vec<(usize, LogEntry)>) -> Self {
        let mut subagent_identities = Vec::new();
        let entries = normalized
            .into_iter()
            .map(|(_, entry)| {
                if let LogEntry::Progress { data, .. } = &entry
                    && let Some((agent_id, identity)) = agent_progress_identity(data)
                {
                    subagent_identities.push((agent_id.to_owned(), identity));
                }
                convert_agent_progress(entry)
            })
            .collect();
        Self {
            entries,
            subagent_identities,
            malformed_lines: Vec::new(),
        }
    }
}

impl From<TranscriptEntries> for DisplayEntries {
    fn from(session: TranscriptEntries) -> Self {
        Self {
            malformed_lines: session.malformed_lines,
            ..session.entries.into()
        }
    }
}

/// The session's [`normalized_session`] as the viewer, the terminal printout,
/// exports and the clipboard copy read it.
pub fn display_log_entries(
    source: Source,
    path: &Path,
    subagents: &[PathBuf],
) -> Result<DisplayEntries> {
    Ok(normalized_session(source, path, subagents)?.into())
}

/// A bare file's [`sniffed_session`] as the viewer and the terminal printout
/// read it.
pub fn sniffed_display_log_entries(path: &Path) -> Result<DisplayEntries> {
    Ok(sniffed_session(path)?.into())
}
