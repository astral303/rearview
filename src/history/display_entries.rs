//! A conversation's entries as the viewer, the terminal printout, exports and
//! the clipboard copy read them. Search and the agent commands read the
//! `agent_progress` shape unconverted, since they index and print it.

use super::format::RecordLine;
use super::sqlite_cache::{
    CachedCopy, Currency, HeldCopy, InteractiveCache, InteractiveRead, Publication, SessionAddress,
};
use super::{MalformedLine, Source, TranscriptEntries, sniffed_session};
use crate::error::Result;
use crate::log_entry::{
    LogEntry, SubagentIdentity, agent_progress_identity, convert_agent_progress,
};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

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
#[derive(Clone)]
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

/// The time the terminal printout tries for refresh ownership before it
/// reads the cache as it is, as agent commands do.
const PRINTOUT_PATIENCE: std::time::Duration = std::time::Duration::from_secs(1);

/// `source`'s session at `path` as the terminal printout after the TUI reads
/// it, with the preview `show_last` selects for its row: refreshed under
/// refresh ownership when its cached copy is out of date or missing, then
/// read from `environment`'s cache. While another rearview keeps the cache
/// past [`PRINTOUT_PATIENCE`], or once the refresh's writes stop, an older
/// copy prints as it is, and one line on `notices` names it; a session the
/// refresh parsed whole prints as parsed. The TUI reads through
/// [`display_log_entries_interactively`].
pub fn printed_log_entries(
    environment: &super::Environment,
    source: Source,
    path: &Path,
    show_last: bool,
    notices: &mut dyn std::io::Write,
) -> Result<DisplayEntries> {
    let operation = super::OperationScope::new(
        environment.cache_base(),
        super::Patience::AtMost(PRINTOUT_PATIENCE),
        Arc::default(),
    );
    let printed = printed_session(environment, source, path, show_last, &operation);
    let finished = operation.finish().fail_if_low_space();
    let printed = printed?;
    finished?;
    if let Some(reason) = printed.older_copy_reason {
        writeln!(
            notices,
            "Showing an older cached copy of {}: {reason}",
            path.display()
        )?;
    }
    Ok(printed.entries)
}

/// The entries the printout shows, and why they are an older copy's.
struct PrintedSession {
    entries: DisplayEntries,
    /// `None` when the entries are current.
    older_copy_reason: Option<&'static str>,
}

/// The session at `path` the printout shows, refreshed and read under
/// `operation`: the entries the refresh parsed when its copy does not hold
/// them; otherwise the copy at the refreshed row's address, with a notice
/// when it is older than the row. Fails when neither exists.
fn printed_session(
    environment: &super::Environment,
    source: Source,
    path: &Path,
    show_last: bool,
    operation: &super::OperationScope,
) -> Result<PrintedSession> {
    let mut outcomes = super::provider::cache_sessions(
        environment,
        source,
        &[path],
        show_last,
        operation,
        super::load_meter::AgentMeter::UNMETERED,
    )?;
    // The printout is rearview's last output: no later refresh follows the
    // read's next step.
    let Some((super::provider::SessionRead::Listed(row), unwritten)) = outcomes
        .pop()
        .map(|outcome| outcome.into_read_and_unwritten())
    else {
        return Err(not_in_the_cache(path));
    };
    if let super::provider::UnwrittenParse::Entries(entries) = unwritten {
        return Ok(PrintedSession {
            entries: entries.into(),
            older_copy_reason: None,
        });
    }
    let Some(address) = &row.cache_address else {
        return Err(not_in_the_cache(path));
    };
    let read = super::sqlite_cache::read_copy_waiting(environment.cache_base(), address, |held| {
        Ok((
            held.currency_for_publisher(&row.publication)?,
            held.entries()?,
        ))
    });
    let Some((currency, entries)) = super::sqlite_cache::found(read)? else {
        return Err(not_in_the_cache(path));
    };
    Ok(PrintedSession {
        entries: entries.into(),
        older_copy_reason: match currency {
            Currency::Current | Currency::Adopt { .. } => None,
            Currency::Older => Some(match operation.stale_cause() {
                super::StaleCause::AnotherRearview => {
                    "another rearview is refreshing the session cache"
                }
                super::StaleCause::WritesStopped => {
                    "rearview could not update it in the session cache"
                }
            }),
        },
    })
}

/// The printout's failure for the session at `path` when the session is not
/// in the cache after its refresh.
fn not_in_the_cache(path: &Path) -> crate::error::AppError {
    crate::error::AppError::SessionCacheNotReady {
        session: path.display().to_string(),
        reason: "the session is not in the session cache".to_owned(),
    }
}

/// The entries of the session at `address` as exports and the clipboard copy
/// read them, through `cache`'s interactive read: the newest copy the cache
/// holds, current or not, for a row whose publication is `publication`. A
/// copy the read adopts in the row's place comes with its own publication.
pub fn display_log_entries_interactively(
    cache: &InteractiveCache,
    address: &SessionAddress,
    publication: &Publication,
) -> InteractiveRead<DisplayEntries> {
    cache.read(|reader| {
        let Some((entries, currency, ())) = newest_copy(reader, address, publication, |_| Ok(()))?
        else {
            return Ok(None);
        };
        Ok(Some(CachedCopy {
            value: DisplayEntries::from(entries),
            currency,
        }))
    })
}

/// A copy of a session as the viewer shows it.
pub struct ViewedCopy {
    pub entries: DisplayEntries,
    /// The copy's catalogue row, the header's source; `None` when the cache
    /// no longer lists the copy.
    pub row: Option<Box<super::Conversation>>,
}

/// [`display_log_entries_interactively`] with the copy's catalogue row, as
/// the viewer reads the session at `address`.
pub fn viewed_copy_interactively(
    cache: &InteractiveCache,
    address: &SessionAddress,
    publication: &Publication,
) -> InteractiveRead<ViewedCopy> {
    cache.read(|reader| {
        let viewed_row = |held: &HeldCopy<'_>| -> Result<Option<Box<super::Conversation>>> {
            let generation = Publication::Published {
                generation: held.generation(),
            };
            Ok(reader
                .catalogue_row_of(held)?
                .filter(|row| row.publication == generation)
                .map(Box::new))
        };
        let Some((entries, currency, row)) = newest_copy(reader, address, publication, viewed_row)?
        else {
            return Ok(None);
        };
        Ok(Some(CachedCopy {
            value: ViewedCopy {
                entries: DisplayEntries::from(entries),
                row,
            },
            currency,
        }))
    })
}

/// The newest copy `reader` holds of the session at `address`, its entries
/// read as it holds them, with its currency for a row whose publication is
/// `publication` and `also`'s read of it, in one read snapshot on `reader`'s
/// connection; `None` when the cache does not hold it ready.
fn newest_copy<T>(
    reader: &super::sqlite_cache::SqliteCache,
    address: &SessionAddress,
    publication: &Publication,
    also: impl FnOnce(&HeldCopy<'_>) -> Result<T>,
) -> Result<Option<(TranscriptEntries, Currency, T)>> {
    super::sqlite_cache::found(reader.read_copy_at(address, |held| {
        Ok((held.entries()?, held.currency(publication)?, also(held)?))
    }))
}

/// A bare file's [`sniffed_session`] as the viewer and the terminal printout
/// read it.
pub fn sniffed_display_log_entries(
    permit: super::ParsePermit<'_>,
    path: &Path,
    environment: &super::provider::RootEnvironment,
) -> Result<DisplayEntries> {
    Ok(sniffed_session(permit, path, environment)?.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::normalized_session;
    use std::collections::HashSet;
    use std::path::PathBuf;

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
    fn sub_agents_turn_keeps_its_origin_when_another_sub_agent_starts() {
        let dir = tempfile::tempdir().unwrap();
        let session = dir.path().join("session.jsonl");
        std::fs::write(&session, user_line("parent", 0) + "\n").unwrap();
        let first = explore_subagent(dir.path(), "agent-a");
        let second = explore_subagent(dir.path(), "agent-b");
        let displayed = |subagents: &[PathBuf]| -> DisplayEntries {
            normalized_session(
                crate::history::FOR_FIXTURES,
                Source::Claude,
                &session,
                subagents,
            )
            .unwrap()
            .into()
        };
        let before = displayed(&[first.clone(), second.clone()]);

        let started_since = explore_subagent(dir.path(), "agent-c");
        let after = displayed(&[first, started_since, second]);

        assert_eq!(origin_of(&after, "agent-b"), origin_of(&before, "agent-b"));
        let distinct: HashSet<&EntryOrigin> = after.origins.iter().collect();
        assert_eq!(distinct.len(), after.origins.len(), "{:?}", after.origins);
    }
}
