//! A resumed read of a changed session: what it starts from, read from the
//! stored session, and its write, which changes the stored rows from the
//! checkpoint on and leaves every row before it as stored, but for the
//! rows the read of a changed sub-agent transcript changes in place.

use super::SqliteCache;
use super::entries::{self, ReadPurpose, Selection};
use super::roots::{Places, StoredPath};
use crate::error::{AppError, Result};
use crate::history::MalformedLine;
use crate::history::fingerprint::CachedFingerprint;
use crate::history::format::RecordLine;
use crate::history::provider::{SessionRoot, SessionStorage, TranscriptRead};
use crate::history::resumed::{SessionCheckpoint, WholeRead};
use crate::log_entry::LogEntry;
use rusqlite::{Connection, OptionalExtension, params};
use std::path::{Path, PathBuf};

/// What a resumed read of a stored session starts from.
pub(crate) struct StoredResume {
    pub(crate) session: i64,
    pub(crate) generation: i64,
    /// The session's files in position order, each with what its last read
    /// left.
    pub(crate) files: Vec<StoredFile>,
    pub(crate) checkpoint: SessionCheckpoint,
    pub(crate) ledger: Vec<u8>,
    pub(crate) malformed_lines: Vec<MalformedLine>,
    /// Each stored sub-agent metadata part: the path of its transcript and
    /// its text.
    pub(crate) subagent_metadata: Vec<(PathBuf, String)>,
    /// The stored rows of the session's leading entries, at sequence 0 on.
    pub(crate) leading_rows: usize,
    /// The session's roots, which place its stored paths.
    places: Places,
}

pub(crate) struct StoredFile {
    pub(crate) kind: String,
    pub(crate) path: PathBuf,
    pub(crate) fingerprint: Option<CachedFingerprint>,
    pub(crate) read: TranscriptRead,
    pub(crate) fold: Vec<u8>,
}

/// A stored entry from a checkpoint on, read back.
pub(crate) struct StoredTailEntry {
    pub(crate) record: RecordLine,
    pub(crate) entry: LogEntry,
    pub(crate) splice_key: Option<String>,
    pub(crate) is_child: bool,
}

/// A stored sub-agent turn's place and numbering.
pub(crate) struct StoredTurn {
    pub(crate) line: usize,
    pub(crate) sequence: i64,
    pub(crate) splice_key: Option<String>,
    pub(crate) is_message: bool,
    pub(crate) reply_id: Option<String>,
    pub(crate) digest: i64,
    pub(crate) id: i64,
    pub(crate) message: Option<i64>,
}

/// A session's id and generation, with its checkpoint's sequence, encoded
/// checkpoint and ledger.
type StoredCheckpointRow = (i64, i64, i64, Vec<u8>, Vec<u8>);

impl SqliteCache {
    /// The start of a resumed read of the listed session `storage` found at
    /// `cache_key` under `root`, when the stored copy is the one of
    /// `generation`; else the reason the session is read whole: the cache
    /// does not hold a checkpoint for it, the checkpoint or the ledger does
    /// not decode or names a transcript outside every root of the session,
    /// or the cache holds another copy. The checkpoint and the ledger name each
    /// transcript by its full path in this run's spelling of the roots.
    pub(crate) fn stored_resume(
        &self,
        storage: &dyn SessionStorage,
        root: &SessionRoot,
        cache_key: &str,
        generation: i64,
    ) -> Result<std::result::Result<StoredResume, WholeRead>> {
        let linker = storage.thread_linker();
        let storage = storage.cache();
        let roots = self.session_roots(root);
        self.with_read_snapshot(|connection| {
            let places = Places::stored(connection, &roots)?;
            let row: Option<StoredCheckpointRow> = connection
                .query_row(
                    "SELECT s.id, s.generation, c.sequence, c.checkpoint, c.ledger \
                     FROM sessions s JOIN session_checkpoints c ON c.session = s.id \
                     WHERE s.storage = ?1 AND s.root = ?2 AND s.cache_key = ?3 \
                     AND s.status = 'listed' AND s.entries_error IS NULL",
                    params![storage.directory, places.listing_root(), cache_key],
                    |row| {
                        Ok((
                            row.get(0)?,
                            row.get(1)?,
                            row.get(2)?,
                            row.get(3)?,
                            row.get(4)?,
                        ))
                    },
                )
                .optional()?;
            let Some((session, stored_generation, sequence, checkpoint, ledger)) = row else {
                return Ok(Err(WholeRead::NoCheckpoint));
            };
            if stored_generation != generation {
                return Ok(Err(WholeRead::StoredCopyReplaced));
            }
            let path_saved_as = |name: &Path| places.path_saved_as(name);
            let Some(checkpoint) = SessionCheckpoint::decode(&checkpoint)
                .filter(|checkpoint| checkpoint.sequence as i64 == sequence)
                .and_then(|checkpoint| checkpoint.with_transcripts_renamed(&path_saved_as))
            else {
                return Ok(Err(WholeRead::CheckpointUnreadable));
            };
            let Some(ledger) = linker.with_transcripts_renamed(&ledger, &path_saved_as) else {
                return Ok(Err(WholeRead::CheckpointUnreadable));
            };
            Ok(Ok(StoredResume {
                session,
                generation,
                files: stored_files(connection, &places, session)?,
                checkpoint,
                ledger,
                malformed_lines: entries::read_malformed_lines(connection, session)?,
                subagent_metadata: stored_subagent_metadata(connection, &places, session)?,
                leading_rows: connection.query_row(
                    "SELECT COUNT(*) FROM entries WHERE session = ?1 AND record_line = 0 \
                     AND child IS NULL AND record_transcript IS NULL AND sequence < ?2",
                    params![session, sequence],
                    |row| row.get::<_, i64>(0),
                )? as usize,
                places,
            }))
        })
    }

    /// The stored entries of `resume`'s session from `sequence` on, read
    /// back with the timestamps the transcripts wrote.
    pub(crate) fn stored_tail(
        &self,
        resume: &StoredResume,
        sequence: usize,
    ) -> Result<Vec<StoredTailEntry>> {
        self.with_read_snapshot(|connection| {
            check_generation(connection, resume)?;
            let selected = Selection::of(
                "SELECT id FROM entries WHERE session = ?1 AND sequence >= ?2",
                vec![resume.session.into(), (sequence as i64).into()],
            );
            let read = entries::read_selected(
                connection,
                resume.session,
                &resume.places,
                &selected,
                ReadPurpose::Display,
            )?;
            let mut statement = connection.prepare_cached(
                "SELECT splice_key, child IS NOT NULL FROM entries \
                 WHERE session = ?1 AND sequence >= ?2 ORDER BY sequence",
            )?;
            let places = statement
                .query_map(params![resume.session, sequence as i64], |row| {
                    Ok((row.get::<_, Option<String>>(0)?, row.get(1)?))
                })?
                .collect::<rusqlite::Result<Vec<(Option<String>, bool)>>>()?;
            Ok(read
                .transcript
                .entries
                .into_iter()
                .zip(places)
                .map(|((record, mut entry), (splice_key, is_child))| {
                    restore_timestamp(&mut entry, splice_key.as_deref(), is_child);
                    StoredTailEntry {
                        record,
                        entry,
                        splice_key,
                        is_child,
                    }
                })
                .collect())
        })
    }

    /// The stored turns of the sub-agent transcript at `path` in `resume`'s
    /// session, in sequence order.
    pub(crate) fn stored_turns(
        &self,
        resume: &StoredResume,
        path: &Path,
    ) -> Result<Vec<StoredTurn>> {
        self.with_read_snapshot(|connection| {
            check_generation(connection, resume)?;
            let mut statement = connection.prepare_cached(
                "SELECT e.record_line, e.sequence, e.splice_key, \
                 m.ordinal IS NOT NULL AND e.contributes, m.reply_id, e.digest, e.id, e.message \
                 FROM entries e JOIN transcripts t ON t.id = e.child \
                 LEFT JOIN messages m ON m.id = e.message \
                 WHERE e.session = ?1 AND t.session = ?1 AND t.root = ?2 AND t.path = ?3 \
                 ORDER BY e.sequence",
            )?;
            // A transcript outside the session's roots holds no stored turn.
            let Some(place) = resume.places.place(path) else {
                return Ok(Vec::new());
            };
            Ok(statement
                .query_map(params![resume.session, place.root, place.path], |row| {
                    Ok(StoredTurn {
                        line: row.get::<_, i64>(0)? as usize,
                        sequence: row.get(1)?,
                        splice_key: row.get(2)?,
                        is_message: row.get::<_, Option<bool>>(3)?.unwrap_or(false),
                        reply_id: row.get(4)?,
                        digest: row.get(5)?,
                        id: row.get(6)?,
                        message: row.get(7)?,
                    })
                })?
                .collect::<rusqlite::Result<_>>()?)
        })
    }

    /// True when a message of `resume`'s session whose first entry stands
    /// before `sequence` carries one of `reply_ids`.
    pub(crate) fn continues_stored_reply(
        &self,
        resume: &StoredResume,
        reply_ids: &[&str],
        sequence: usize,
    ) -> Result<bool> {
        self.with_read_snapshot(|connection| {
            let mut statement = connection.prepare_cached(
                "SELECT EXISTS (SELECT 1 FROM messages m JOIN entries e ON e.message = m.id \
                 WHERE m.session = ?1 AND m.reply_id = ?2 AND e.sequence < ?3)",
            )?;
            for reply_id in reply_ids {
                let exists: bool = statement
                    .query_row(params![resume.session, reply_id, sequence as i64], |row| {
                        row.get(0)
                    })?;
                if exists {
                    return Ok(true);
                }
            }
            Ok(false)
        })
    }
}

/// Fails when `resume`'s session no longer holds the copy it was read from.
fn check_generation(connection: &Connection, resume: &StoredResume) -> Result<()> {
    let generation: Option<i64> = connection
        .query_row(
            "SELECT generation FROM sessions WHERE id = ?1",
            [resume.session],
            |row| row.get(0),
        )
        .optional()?;
    if generation != Some(resume.generation) {
        return Err(AppError::SessionCacheSuperseded(resume.session));
    }
    Ok(())
}

/// Give `entry`, read back from a row whose splice key is `splice_key`, the
/// timestamp text its transcript wrote: a read gives it from the row's
/// milliseconds.
fn restore_timestamp(entry: &mut LogEntry, splice_key: Option<&str>, is_child: bool) {
    match entry {
        LogEntry::User { timestamp, .. }
        | LogEntry::Assistant { timestamp, .. }
        | LogEntry::PiMetadata { timestamp, .. } => {
            *timestamp = splice_key.map(str::to_owned);
        }
        LogEntry::Progress { extra, .. } if is_child => {
            if let Some(extra) = extra.as_object_mut() {
                match splice_key.filter(|key| !key.is_empty()) {
                    Some(key) => {
                        extra.insert("timestamp".to_owned(), key.into());
                    }
                    None => {
                        extra.remove("timestamp");
                    }
                }
            }
        }
        _ => {}
    }
}

fn stored_files(connection: &Connection, places: &Places, session: i64) -> Result<Vec<StoredFile>> {
    let mut statement = connection.prepare_cached(
        "SELECT f.kind, f.root, f.path, f.file_size, f.mtime_secs, f.mtime_nsecs, r.cursor, \
         r.stable_cursor, r.identity, r.rewrite_check, r.parser_state, r.stable_line, r.fold_state \
         FROM session_files f LEFT JOIN transcript_reads r \
         ON r.session = f.session AND r.position = f.position AND r.root = f.root \
         AND r.path = f.path WHERE f.session = ?1 ORDER BY f.position",
    )?;
    let mut rows = statement.query([session])?;
    let mut files = Vec::new();
    while let Some(row) = rows.next()? {
        let fingerprint = match row.get::<_, Option<i64>>(3)? {
            Some(file_size) => Some(CachedFingerprint {
                file_size: file_size as u64,
                mtime_secs: row.get::<_, i64>(4)? as u64,
                mtime_nsecs: row.get(5)?,
            }),
            None => None,
        };
        let blob = |index| {
            row.get::<_, Option<Vec<u8>>>(index)
                .map(Option::unwrap_or_default)
        };
        files.push(StoredFile {
            kind: row.get(0)?,
            path: places.path(StoredPath {
                root: row.get(1)?,
                path: row.get(2)?,
            })?,
            fingerprint,
            read: TranscriptRead {
                cursor: blob(6)?,
                stable_cursor: blob(7)?,
                identity: blob(8)?,
                rewrite_check: blob(9)?,
                parser_state: blob(10)?,
                stable_line: row.get::<_, Option<i64>>(11)?.map(|line| line as usize),
            },
            fold: blob(12)?,
        });
    }
    Ok(files)
}

fn stored_subagent_metadata(
    connection: &Connection,
    places: &Places,
    session: i64,
) -> Result<Vec<(PathBuf, String)>> {
    let mut statement = connection.prepare_cached(
        "SELECT f.root, f.path, p.text FROM parts p \
         JOIN session_files f ON f.session = p.session AND f.position = p.file_position \
         WHERE p.session = ?1 AND p.entry IS NULL ORDER BY p.part",
    )?;
    let mut rows = statement.query([session])?;
    let mut parts = Vec::new();
    while let Some(row) = rows.next()? {
        let path = places.path(StoredPath {
            root: row.get(0)?,
            path: row.get(1)?,
        })?;
        parts.push((path, row.get(2)?));
    }
    Ok(parts)
}

/// What a resumed read changed in a stored session.
pub(crate) struct WindowUpdate {
    pub(crate) session: i64,
    pub(crate) generation: i64,
    /// How many stored rows held the session's leading entries, which the
    /// record's leading entries replace.
    pub(crate) stored_leading_rows: usize,
    pub(crate) leading: Vec<(RecordLine, LogEntry)>,
    /// The checkpoint the tail starts after, before the leading rows move.
    pub(crate) checkpoint: SessionCheckpoint,
    /// Rows before the checkpoint the read of a changed sub-agent transcript
    /// changed in place: each entry, with the stored row's id, sequence and
    /// message.
    pub(crate) in_place: Vec<InPlaceRow>,
    /// The call that launched each sub-agent thread, by its transcript.
    pub(crate) launch_calls: Vec<(PathBuf, Option<String>)>,
}

pub(crate) struct InPlaceRow {
    pub(crate) record: RecordLine,
    pub(crate) entry: LogEntry,
    pub(crate) id: i64,
    pub(crate) message: Option<i64>,
    /// The stored row's digest: an equal row is not written.
    pub(crate) digest: i64,
}
