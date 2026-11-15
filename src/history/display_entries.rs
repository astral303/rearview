//! A conversation's entries as the viewer, the terminal printout, exports and
//! the clipboard copy read them. Search and the agent commands read the
//! `agent_progress` shape unconverted, since they index and print it.

use super::format::RecordLine;
use super::sqlite_cache::{
    CachedCopy, Currency, EntryHashes, HeldCopy, InteractiveCache, InteractiveRead, Publication,
    SequencedHashes, SessionAddress,
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

/// The identity one `agent_progress` record gives the sub-agent `agent_id`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordedIdentity {
    /// The index of the record's entry among the displayed entries.
    pub entry_index: usize,
    pub agent_id: String,
    pub identity: SubagentIdentity,
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
    /// The identity each `agent_progress` record gives its sub-agent, in
    /// record order. The converted message no longer carries it.
    pub subagent_identities: Vec<RecordedIdentity>,
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
        for (entry_index, (record, entry)) in normalized.into_iter().enumerate() {
            if let LogEntry::Progress { data, .. } = &entry
                && let Some((agent_id, identity)) = agent_progress_identity(data)
            {
                subagent_identities.push(RecordedIdentity {
                    entry_index,
                    agent_id: agent_id.to_owned(),
                    identity,
                });
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
    /// One hash per entry of `entries`, read in the same snapshot.
    pub entry_hashes: EntryHashes,
    /// The copy's catalogue row, the header's source; `None` when the cache
    /// no longer lists the copy.
    pub row: Option<Box<super::Conversation>>,
}

/// [`display_log_entries_interactively`] with the copy's entry hashes and
/// catalogue row, as the viewer reads the session at `address`.
pub fn viewed_copy_interactively(
    cache: &InteractiveCache,
    address: &SessionAddress,
    publication: &Publication,
) -> InteractiveRead<ViewedCopy> {
    cache.read(|reader| {
        let hashes_and_row =
            |held: &HeldCopy<'_>| Ok((held.entry_hashes()?, viewed_row(reader, held)?));
        let Some((entries, currency, (entry_hashes, row))) =
            newest_copy(reader, address, publication, hashes_and_row)?
        else {
            return Ok(None);
        };
        Ok(Some(CachedCopy {
            value: ViewedCopy {
                entries: DisplayEntries::from(entries),
                entry_hashes,
                row,
            },
            currency,
        }))
    })
}

/// The catalogue row `reader` lists for the copy `held`; `None` when the
/// cache no longer lists that copy.
fn viewed_row(
    reader: &super::sqlite_cache::SqliteCache,
    held: &HeldCopy<'_>,
) -> Result<Option<Box<super::Conversation>>> {
    let generation = Publication::Published {
        generation: held.generation(),
    };
    Ok(reader
        .catalogue_row_of(held)?
        .filter(|row| row.publication == generation)
        .map(Box::new))
}

/// A reload's read of a session's copy in the cache, for a viewer that
/// shows an earlier copy: the copy's display entries, whole or from the
/// first changed entry on, with its entry hashes and catalogue row.
pub enum RereadCopy {
    /// Every entry of the copy, as [`viewed_copy_interactively`] reads it.
    Whole(ViewedCopy),
    Tail(CopyTail),
}

/// A copy's entries from the first changed entry on: the first entry whose
/// hash differs from the shown copy's.
pub struct CopyTail {
    /// The index of the first changed entry. It is the copy's entry count
    /// when the copy has the shown copy's entries, or the first of them
    /// alone; `entries` is then empty.
    pub first_changed: usize,
    /// The entries from the first changed entry on, the only ones the read
    /// decoded. Each origin's part and each identity's entry index count
    /// from the first of them.
    pub entries: DisplayEntries,
    /// One hash per entry of the whole copy, read in the same snapshot.
    pub entry_hashes: EntryHashes,
    /// The copy's catalogue row, as [`ViewedCopy::row`].
    pub row: Option<Box<super::Conversation>>,
}

/// The newest copy of the session at `address`, read for a viewer that
/// shows a copy whose entry hashes are `shown`: in one read snapshot, the
/// copy's hashes, then its entries from the first changed entry on.
///
/// The read decodes the whole copy, as [`viewed_copy_interactively`] does,
/// when the first changed entry is the first entry.
pub fn reread_copy_interactively(
    cache: &InteractiveCache,
    address: &SessionAddress,
    publication: &Publication,
    shown: &EntryHashes,
) -> InteractiveRead<RereadCopy> {
    cache.read(|reader| {
        let read = reader.read_copy_at(address, |held| {
            let hashes = held.sequenced_entry_hashes()?;
            let tail = tail_entries(held, shown, &hashes)?;
            let currency = held.currency(publication)?;
            let row = viewed_row(reader, held)?;
            let copy = match tail {
                Some((first_changed, entries)) => RereadCopy::Tail(CopyTail {
                    first_changed,
                    entries: entries.into(),
                    entry_hashes: hashes.hashes,
                    row,
                }),
                None => RereadCopy::Whole(ViewedCopy {
                    entries: held.entries()?.into(),
                    entry_hashes: hashes.hashes,
                    row,
                }),
            };
            Ok((copy, currency))
        });
        let Some((value, currency)) = super::sqlite_cache::found(read)? else {
            return Ok(None);
        };
        Ok(Some(CachedCopy { value, currency }))
    })
}

/// The index of the first entry of the copy `held` whose hash in `hashes`
/// differs from `shown`'s, and the copy's entries from it on; `None` when
/// the whole copy is to be read (see [`reread_copy_interactively`]).
///
/// The count check guards the cache's constraint of one entry per
/// `sequence` in a session, which gives one entry read per hash from the
/// first changed entry on. A copy in the cache doesn't fail it.
fn tail_entries(
    held: &HeldCopy<'_>,
    shown: &EntryHashes,
    hashes: &SequencedHashes,
) -> Result<Option<(usize, TranscriptEntries)>> {
    let copy_len = hashes.hashes.len();
    let first_changed = shown.first_change_in(&hashes.hashes).unwrap_or(copy_len);
    if first_changed == 0 {
        return Ok(None);
    }
    let Some(first_sequence) = hashes.sequence_at(first_changed) else {
        let unread = TranscriptEntries {
            entries: Vec::new(),
            malformed_lines: Vec::new(),
        };
        return Ok(Some((first_changed, unread)));
    };
    let entries = held.entries_from(first_sequence)?;
    if entries.entries.len() != copy_len - first_changed {
        return Ok(None);
    }
    Ok(Some((first_changed, entries)))
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

    /// A Claude session of one prompt per text, written to `path` and
    /// cached; returns its row.
    fn cache_prompts(path: &Path, texts: &[&str]) -> crate::history::Conversation {
        let lines: String = texts
            .iter()
            .enumerate()
            .map(|(second, text)| user_line(text, second) + "\n")
            .collect();
        std::fs::write(path, lines).unwrap();
        crate::history::environment::test_support::cache_session(Source::Claude, path, &[])
    }

    fn test_cache() -> InteractiveCache {
        InteractiveCache::at(crate::history::environment::test_support::test_cache_base())
    }

    /// The whole copy of `row`'s session, as the viewer's open reads it.
    fn whole_copy(row: &crate::history::Conversation) -> ViewedCopy {
        crate::history::environment::test_support::current_copy(viewed_copy_interactively(
            &test_cache(),
            row.cache_address.as_ref().unwrap(),
            &row.publication,
        ))
    }

    /// The copy of `row`'s session, read for a viewer that shows `shown`.
    fn reread_after(row: &crate::history::Conversation, shown: &ViewedCopy) -> RereadCopy {
        crate::history::environment::test_support::current_copy(reread_copy_interactively(
            &test_cache(),
            row.cache_address.as_ref().unwrap(),
            &row.publication,
            &shown.entry_hashes,
        ))
    }

    /// The tail a reread returned; panics for a whole copy.
    fn tail_of(reread: RereadCopy, case: &str) -> CopyTail {
        match reread {
            RereadCopy::Tail(tail) => tail,
            RereadCopy::Whole(_) => panic!("{case}: the read returns a tail"),
        }
    }

    #[test]
    fn reread_decodes_entries_from_first_changed_entry_on() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let shown = whole_copy(&cache_prompts(&path, &["a", "b", "c", "d"]));
        let changes: [(&str, &[&str], usize); 2] = [
            ("two prompts appended", &["a", "b", "c", "d", "e", "f"], 4),
            ("third prompt edited", &["a", "b", "c, edited", "d"], 2),
        ];

        for (case, texts, first_changed) in changes {
            let row = cache_prompts(&path, texts);
            let whole = whole_copy(&row);
            let tail = tail_of(reread_after(&row, &shown), case);

            assert_eq!(
                (tail.first_changed, tail.entries.entries.len()),
                (first_changed, texts.len() - first_changed),
                "{case}: the read decodes the entries from the first changed entry on alone"
            );
            assert_eq!(
                format!("{:?}", (&tail.entries.entries, &tail.entries.origins)),
                format!(
                    "{:?}",
                    (
                        &whole.entries.entries[first_changed..],
                        &whole.entries.origins[first_changed..]
                    )
                ),
                "{case}: the tail's entries are the whole read's from the first changed entry on"
            );
            assert_eq!(
                tail.entry_hashes, whole.entry_hashes,
                "{case}: the tail comes with one hash per entry of the whole copy"
            );
        }
    }

    #[test]
    fn reread_of_copy_with_equal_kept_entries_doesnt_decode_any_entry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let shown = whole_copy(&cache_prompts(&path, &["a", "b", "c"]));

        let mut transcript = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        std::io::Write::write_all(&mut transcript, b"\n").unwrap();
        drop(transcript);
        let row =
            crate::history::environment::test_support::cache_session(Source::Claude, &path, &[]);
        let equal = tail_of(reread_after(&row, &shown), "an equal copy");
        let shorter = tail_of(
            reread_after(&cache_prompts(&path, &["a", "b"]), &shown),
            "a shorter copy",
        );

        assert_eq!(
            (equal.first_changed, equal.entries.entries.len()),
            (3, 0),
            "a copy with the shown entries: the first changed entry is its entry count, and \
             no entry is decoded"
        );
        assert_eq!(
            (
                shorter.first_changed,
                shorter.entries.entries.len(),
                shorter.entry_hashes.len()
            ),
            (2, 0, 2),
            "a copy with the first two shown entries alone: no entry is decoded"
        );
    }

    #[test]
    fn reread_of_copy_changed_at_first_entry_reads_whole_copy() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let shown = whole_copy(&cache_prompts(&path, &["a", "b", "c"]));

        let row = cache_prompts(&path, &["a, edited", "b", "c"]);
        let reread = reread_after(&row, &shown);

        let RereadCopy::Whole(copy) = reread else {
            panic!("a copy changed at its first entry is read whole");
        };
        assert_eq!(
            (copy.entries.entries.len(), copy.entry_hashes.len()),
            (3, 3),
            "the whole read decodes every entry and returns one hash per entry"
        );
    }

    #[test]
    fn sub_agents_identity_is_recorded_with_its_entrys_index() {
        let progress = |nickname: &str| LogEntry::Progress {
            data: serde_json::json!({
                "type": "agent_progress",
                "agentId": "thread-a",
                "identity": {"nickname": nickname},
                "message": {
                    "type": "user",
                    "message": {"role": "user", "content": [{"type": "text", "text": "turn"}]}
                }
            }),
            extra: serde_json::json!({}),
        };
        let parent: LogEntry = serde_json::from_str(&user_line("parent", 0)).unwrap();

        let displayed = DisplayEntries::from(vec![
            (1, parent.clone()),
            (2, progress("Lorentz")),
            (3, parent),
            (4, progress("Hubble")),
        ]);

        let recorded: Vec<(usize, &str, Option<&str>)> = displayed
            .subagent_identities
            .iter()
            .map(|recorded| {
                (
                    recorded.entry_index,
                    recorded.agent_id.as_str(),
                    recorded.identity.nickname.as_deref(),
                )
            })
            .collect();
        assert_eq!(
            recorded,
            [
                (1, "thread-a", Some("Lorentz")),
                (3, "thread-a", Some("Hubble"))
            ],
            "each identity is recorded with the index of its record's entry"
        );
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
