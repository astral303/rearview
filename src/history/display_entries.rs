//! A conversation's entries as the viewer, the terminal printout, exports and
//! the clipboard copy read them. Search and the agent commands read the
//! `agent_progress` shape unconverted, since they index and print it.

use super::format::RecordLine;
use super::{MalformedLine, Source, TranscriptEntries, normalized_session, sniffed_session};
use crate::error::Result;
use crate::log_entry::{
    LogEntry, SubagentIdentity, agent_progress_identity, convert_agent_progress,
};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// The record an entry was read from, and which of that record's entries it
/// is. An entry keeps its origin when the session is read again, wherever
/// sub-agent turns spliced in since then move it: agents append to transcript
/// files, and OpenCode's line is the database part an entry came from, which
/// it updates in place.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct EntryOrigin {
    record: RecordLine,
    /// Which of the entries read from one record this is: a Kimi usage record
    /// becomes several.
    part: usize,
}

/// The entries the viewer, the terminal printout, exports and the clipboard
/// copy read. Each `agent_progress` record, spliced from a sub-agent
/// transcript or recorded inline, is converted into the sub-agent message it
/// carries, so these outputs handle one shape for a sub-agent's messages. Keep
/// one entry per record: the viewer and the clipboard copy address an entry by
/// its position.
pub struct DisplayEntries {
    pub entries: Vec<LogEntry>,
    /// Where each entry of `entries` was read, at the same position.
    pub origins: Vec<EntryOrigin>,
    /// The identity each `agent_progress` record names, by its agent id and
    /// in record order. The converted message no longer carries it.
    pub subagent_identities: Vec<(String, SubagentIdentity)>,
    /// The lines of the session file that did not parse. The terminal
    /// printout reports under `--debug` the ones whose text the format kept.
    pub(crate) malformed_lines: Vec<MalformedLine>,
}

impl From<Vec<(RecordLine, LogEntry)>> for DisplayEntries {
    fn from(normalized: Vec<(RecordLine, LogEntry)>) -> Self {
        let mut subagent_identities = Vec::new();
        let mut parts_read: HashMap<RecordLine, usize> = HashMap::new();
        let mut entries = Vec::with_capacity(normalized.len());
        let mut origins = Vec::with_capacity(normalized.len());
        for (record, entry) in normalized {
            if let LogEntry::Progress { data, .. } = &entry
                && let Some((agent_id, identity)) = agent_progress_identity(data)
            {
                subagent_identities.push((agent_id.to_owned(), identity));
            }
            let part = parts_read.entry(record.clone()).or_insert(0);
            origins.push(EntryOrigin {
                record,
                part: *part,
            });
            *part += 1;
            entries.push(convert_agent_progress(entry));
        }
        Self {
            entries,
            origins,
            subagent_identities,
            malformed_lines: Vec::new(),
        }
    }
}

/// Entries read from the session's own transcript, at their lines in it.
#[cfg(test)]
impl From<Vec<(usize, LogEntry)>> for DisplayEntries {
    fn from(normalized: Vec<(usize, LogEntry)>) -> Self {
        super::format::at_own_lines(normalized).into()
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn user_line(text: &str, second: usize) -> String {
        serde_json::json!({
            "type": "user",
            "timestamp": format!("2024-01-01T00:00:{second:02}Z"),
            "message": {"role": "user", "content": text}
        })
        .to_string()
    }

    /// A sub-agent transcript of one turn sent at second 1, typed `Explore`.
    fn explore_subagent(dir: &Path, stem: &str) -> PathBuf {
        let transcript = dir.join(format!("session/subagents/{stem}.jsonl"));
        std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
        std::fs::write(&transcript, user_line(stem, 1) + "\n").unwrap();
        std::fs::write(
            transcript.with_extension("meta.json"),
            r#"{"agentType":"Explore"}"#,
        )
        .unwrap();
        transcript
    }

    /// The origin of the entry whose text is `text`.
    fn origin_of(displayed: &DisplayEntries, text: &str) -> EntryOrigin {
        let position = displayed
            .entries
            .iter()
            .position(|entry| serde_json::to_string(entry).unwrap().contains(text))
            .unwrap();
        displayed.origins[position].clone()
    }

    #[test]
    fn a_sub_agents_turn_keeps_its_origin_when_another_sub_agent_starts() {
        let dir = tempfile::tempdir().unwrap();
        let session = dir.path().join("session.jsonl");
        std::fs::write(&session, user_line("parent", 0) + "\n").unwrap();
        let first = explore_subagent(dir.path(), "agent-a");
        let second = explore_subagent(dir.path(), "agent-b");
        let before =
            display_log_entries(Source::Claude, &session, &[first.clone(), second.clone()])
                .unwrap();

        let started_since = explore_subagent(dir.path(), "agent-c");
        let after =
            display_log_entries(Source::Claude, &session, &[first, started_since, second]).unwrap();

        assert_eq!(origin_of(&after, "agent-b"), origin_of(&before, "agent-b"));
        let distinct: HashSet<&EntryOrigin> = after.origins.iter().collect();
        assert_eq!(distinct.len(), after.origins.len(), "{:?}", after.origins);
    }
}
