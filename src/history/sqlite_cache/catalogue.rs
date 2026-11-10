//! A session's catalogue row, built from what a load read of it, and the
//! record a load hands the database writer.

use super::entries::path_text;
use super::files::{FileKey, RootKey};
use super::positional::PositionalRow;
use crate::history::fingerprint::{CachedFingerprint, FileStamp};
use crate::history::parser::{SessionTranscripts, SubagentSearchPart};
use crate::history::provider::{
    RecordedSession, SessionCache, SessionRoot, SessionStub, TranscriptRead,
};
use crate::history::resumed::SessionCheckpoint;
use crate::history::{Conversation, ParseError, Source, TranscriptEntries};
use rusqlite::types::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SessionStatus {
    Listed,
    /// Read cleanly, and holds no conversation the provider lists.
    Empty,
    /// The read hit a permanent error.
    Unreadable,
}

impl SessionStatus {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Listed => "listed",
            Self::Empty => "empty",
            Self::Unreadable => "unreadable",
        }
    }
}

/// The status a session's rows carry until every one of them is written. A
/// row with it is not a current one.
pub(super) const WRITING: &str = "writing";

/// The SQL condition on a `sessions` row that holds unless its entries failed
/// to read: an unlisted row's `entries_error` holds the reason it is not
/// listed, not a failure.
pub(super) const ENTRIES_NOT_FAILED: &str = "(status != 'listed' OR entries_error IS NULL)";

/// One session as a load read it, for the database writer: its catalogue
/// row and, for a listed session, its entries.
pub(crate) struct SessionRecord {
    pub(super) catalogue: CatalogueRow,
    /// `None` for a session that is not listed; the reason `session_entries`
    /// failed for a listed one.
    pub(super) entries: Option<Result<TranscriptEntries, String>>,
    /// The warning an agent command prints for a session that is not
    /// listed, as [`crate::agent::diagnostic::AgentWarning::to_recorded`]
    /// writes it; `None` for a listed one.
    pub(super) unlisted_reason: Option<String>,
    pub(super) search_parts: Vec<SubagentSearchPart>,
    /// Where a resumed read of the session starts, with the agent's ledger
    /// of its threads' links; `None` for a session whose next change is read
    /// whole.
    pub(super) checkpoint: Option<(SessionCheckpoint, Vec<u8>)>,
    /// For a record of a resumed read: what changed, written over the
    /// stored session in place of its whole entries.
    pub(super) update: Option<Box<super::window::WindowUpdate>>,
}

/// A session's transcript files as its stub names them, the locator first,
/// each with its stamp. A listed record stores these stamps, so take them
/// before the parse: a file that changes during the parse then fails the
/// record's check.
pub(crate) struct StubStamps(Vec<FileStamp>);

impl StubStamps {
    /// The stub's files as they stamp now.
    pub(crate) fn current(stub: &SessionStub) -> Self {
        Self(
            std::iter::once(&stub.locator)
                .chain(&stub.subagents)
                .map(|path| FileStamp::before_read(path))
                .collect(),
        )
    }

    /// The stamp of the stub's file at `path`.
    pub(crate) fn fingerprint_of(&self, path: &Path) -> Option<CachedFingerprint> {
        self.0
            .iter()
            .find(|stamp| stamp.path == path)
            .and_then(|stamp| stamp.fingerprint)
    }
}

impl SessionRecord {
    /// The record of `conversation`, `storage`'s session at `stub` under
    /// `root`, with the entries of `transcripts`, the parse that built it.
    /// `stamps` are the stub's files stamped before that parse. `None` when
    /// the stub has no fingerprint to validate the record against.
    pub(crate) fn listed(
        storage: SessionCache,
        source: Source,
        root: &SessionRoot,
        stub: &SessionStub,
        stamps: StubStamps,
        conversation: &Conversation,
        mut transcripts: SessionTranscripts,
    ) -> Option<Self> {
        let mut catalogue =
            CatalogueRow::of_stub(storage, source, root, stub, stamps, SessionStatus::Listed)?;
        catalogue.describe(stub, conversation);
        let added = catalogue.add_transcripts_read(&transcripts);
        catalogue.add_reads(&transcripts);
        let checkpoint = transcripts
            .checkpoint
            .take()
            .map(|checkpoint| (checkpoint, std::mem::take(&mut transcripts.ledger)));
        let (entries, search_parts) = match added {
            Ok(()) => {
                let (entries, search_parts) = transcripts.into_entries_and_search_parts();
                (Ok(entries), search_parts)
            }
            Err(error) => (Err(error.to_string()), Vec::new()),
        };
        Some(Self {
            catalogue,
            entries: Some(entries),
            unlisted_reason: None,
            search_parts,
            checkpoint,
            update: None,
        })
    }

    /// The record of `conversation`, a resumed read of `storage`'s session
    /// at `stub` under `root`, whose files were stamped as `stamps` before
    /// the read. `reads` gives each file read again what its read leaves
    /// for the next and its share of the row, `stored` each other file's.
    /// `None` when a file changed after its stamp was taken, or the stub
    /// has no fingerprint.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn resumed(
        storage: SessionCache,
        source: Source,
        root: &SessionRoot,
        stub: &SessionStub,
        stamps: StubStamps,
        conversation: &Conversation,
        reads: HashMap<PathBuf, (TranscriptRead, Vec<u8>)>,
        entries: TranscriptEntries,
        search_parts: Vec<SubagentSearchPart>,
        checkpoint: Option<(SessionCheckpoint, Vec<u8>)>,
        update: super::window::WindowUpdate,
    ) -> Option<Self> {
        let mut catalogue =
            CatalogueRow::of_stub(storage, source, root, stub, stamps, SessionStatus::Listed)?;
        catalogue.describe(stub, conversation);
        for file in &mut catalogue.files {
            if CachedFingerprint::of_file(&file.path).ok()? != file.fingerprint {
                return None;
            }
            if let Some((read, fold)) = reads.get(&file.path) {
                file.read = read.clone();
                file.fold = fold.clone();
            }
        }
        Some(Self {
            catalogue,
            entries: Some(Ok(entries)),
            unlisted_reason: None,
            search_parts,
            checkpoint,
            update: Some(Box::new(update)),
        })
    }

    /// The record of `storage`'s session at `stub` under `root`, read as
    /// `status`, which is not [`SessionStatus::Listed`], with `reason`, the
    /// warning an agent command prints for it.
    pub(crate) fn unlisted(
        storage: SessionCache,
        source: Source,
        root: &SessionRoot,
        stub: &SessionStub,
        status: SessionStatus,
        reason: String,
    ) -> Option<Self> {
        let stamps = StubStamps::current(stub);
        let mut catalogue = CatalogueRow::of_stub(storage, source, root, stub, stamps, status)?;
        catalogue.handle = handle(source, stub, None);
        Some(Self {
            catalogue,
            entries: None,
            unlisted_reason: Some(reason),
            search_parts: Vec::new(),
            checkpoint: None,
            update: None,
        })
    }
}

/// A session's row in the `sessions` table, with the rows of
/// `session_files` and `parse_errors` that belong to it.
pub(super) struct CatalogueRow {
    pub(super) storage: &'static str,
    pub(super) source: &'static str,
    pub(super) root: SessionRoot,
    pub(super) cache_key: String,
    pub(super) locator: String,
    pub(super) status: SessionStatus,
    pub(super) fingerprint: CachedFingerprint,
    pub(super) session_id: Option<String>,
    pub(super) session_id_key: Option<String>,
    pub(super) handle: Option<String>,
    pub(super) project_path: Option<String>,
    pub(super) project_name: Option<String>,
    pub(super) cwd: Option<String>,
    pub(super) timestamp_ms: Option<i64>,
    pub(super) preview_first: Option<String>,
    pub(super) preview_last: Option<String>,
    pub(super) summary: Option<String>,
    pub(super) custom_title: Option<String>,
    pub(super) model: Option<String>,
    pub(super) total_tokens: Option<i64>,
    pub(super) duration_minutes: Option<i64>,
    pub(super) message_count: Option<i64>,
    pub(super) assistant_messages: Option<i64>,
    pub(super) is_missing_earlier_turns: Option<bool>,
    /// The locator, sub-agent files and contributing continuation files,
    /// with fingerprints for the locators that name files.
    pub(super) files: Vec<SessionFile>,
    pub(super) parse_errors: Vec<ParseError>,
}

pub(super) struct SessionFile {
    pub(super) kind: &'static str,
    /// As discovery, or the transcript that names the file, spells it.
    pub(super) path: PathBuf,
    pub(super) fingerprint: Option<CachedFingerprint>,
    /// What the parse's read of the file leaves for the next read: the full
    /// read's values for a file read through the default, or not read.
    pub(super) read: TranscriptRead,
    /// A sub-agent transcript's encoded [`crate::history::resumed::ThreadFold`];
    /// empty for any other file.
    pub(super) fold: Vec<u8>,
}

impl SessionFile {
    pub(super) fn new(
        kind: &'static str,
        path: PathBuf,
        fingerprint: Option<CachedFingerprint>,
    ) -> Self {
        Self {
            kind,
            path,
            fingerprint,
            read: TranscriptRead::full_read(),
            fold: Vec::new(),
        }
    }
}

/// The [`RootKey`] of a row's root, and the [`FileKey`] of its locator and
/// of each of its files, in order.
pub(super) struct CatalogueKeys {
    pub(super) root: RootKey,
    pub(super) locator: FileKey,
    pub(super) files: Vec<FileKey>,
}

/// The `sessions` columns [`CatalogueRow::values`] gives, after `storage`,
/// `root_key` and `cache_key`.
const COLUMNS: &[&str] = &[
    "root",
    "source",
    "locator",
    "file_key",
    "status",
    "file_size",
    "mtime_secs",
    "mtime_nsecs",
    "session_id",
    "session_id_key",
    "handle",
    "project_path",
    "project_name",
    "cwd",
    "timestamp_ms",
    "preview_first",
    "preview_last",
    "summary",
    "custom_title",
    "model",
    "total_tokens",
    "duration_minutes",
    "message_count",
    "assistant_messages",
    "is_missing_earlier_turns",
    "entries_error",
    "generation",
];

/// `INSERT` of a `sessions` row: `storage`, `root_key` and `cache_key` (`?1`
/// to `?3`), then [`CatalogueRow::values`].
pub(super) fn insert_sql() -> String {
    let parameters: Vec<String> = (4..COLUMNS.len() + 4)
        .map(|index| format!("?{index}"))
        .collect();
    format!(
        "INSERT INTO sessions (storage, root_key, cache_key, {}) VALUES (?1, ?2, ?3, {})",
        COLUMNS.join(", "),
        parameters.join(", ")
    )
}

/// `UPDATE` of a `sessions` row to [`CatalogueRow::values`], then the row's
/// id.
pub(super) fn update_sql() -> String {
    let assignments: Vec<String> = COLUMNS
        .iter()
        .enumerate()
        .map(|(index, column)| format!("{column} = ?{}", index + 1))
        .collect();
    format!(
        "UPDATE sessions SET {} WHERE id = ?{}",
        assignments.join(", "),
        COLUMNS.len() + 1
    )
}

/// The key a stored `session_files` row matches a new one by: its kind and
/// path.
pub(super) fn file_key_of(values: &[Value]) -> (String, String) {
    let text = |value: &Value| match value {
        Value::Text(text) => text.clone(),
        _ => String::new(),
    };
    (text(&values[0]), text(&values[1]))
}

impl CatalogueRow {
    /// The row's `sessions` columns, as [`COLUMNS`] lists them, with `keys`,
    /// the keys of its files, `status`, `message_count`, `entries_error` and
    /// `generation`.
    pub(super) fn values(
        &self,
        keys: &CatalogueKeys,
        status: &str,
        message_count: Option<i64>,
        entries_error: Option<&str>,
        generation: i64,
    ) -> Vec<Value> {
        let text = |text: &Option<String>| Value::from(text.clone());
        vec![
            path_text(&self.root.path).into(),
            self.source.to_owned().into(),
            self.locator.clone().into(),
            keys.locator.as_str().to_owned().into(),
            status.to_owned().into(),
            (self.fingerprint.file_size as i64).into(),
            (self.fingerprint.mtime_secs as i64).into(),
            self.fingerprint.mtime_nsecs.into(),
            text(&self.session_id),
            text(&self.session_id_key),
            text(&self.handle),
            text(&self.project_path),
            text(&self.project_name),
            text(&self.cwd),
            self.timestamp_ms.into(),
            text(&self.preview_first),
            text(&self.preview_last),
            text(&self.summary),
            text(&self.custom_title),
            text(&self.model),
            self.total_tokens.into(),
            self.duration_minutes.into(),
            message_count.into(),
            self.assistant_messages.into(),
            self.is_missing_earlier_turns.into(),
            entries_error.map(str::to_owned).into(),
            generation.into(),
        ]
    }

    /// The row's `session_files` rows, with `keys`, the keys of its files.
    pub(super) fn file_rows(&self, keys: &CatalogueKeys) -> Vec<PositionalRow<(String, String)>> {
        self.files
            .iter()
            .zip(&keys.files)
            .map(|(file, key)| {
                let path = path_text(&file.path);
                PositionalRow {
                    key: (file.kind.to_owned(), path.clone()),
                    values: vec![
                        file.kind.to_owned().into(),
                        path.into(),
                        key.as_str().to_owned().into(),
                        file.fingerprint
                            .map(|fingerprint| fingerprint.file_size as i64)
                            .into(),
                        file.fingerprint
                            .map(|fingerprint| fingerprint.mtime_secs as i64)
                            .into(),
                        file.fingerprint
                            .map(|fingerprint| fingerprint.mtime_nsecs)
                            .into(),
                    ],
                }
            })
            .collect()
    }

    /// The row's `parse_errors` rows, keyed by line number.
    pub(super) fn parse_error_rows(&self) -> Vec<PositionalRow<i64>> {
        let json =
            |lines: &[String]| serde_json::to_string(lines).expect("a list of strings serializes");
        self.parse_errors
            .iter()
            .map(|error| PositionalRow {
                key: error.line_number as i64,
                values: vec![
                    (error.line_number as i64).into(),
                    error.line_content.clone().into(),
                    error.error_message.clone().into(),
                    json(&error.context_before).into(),
                    json(&error.context_after).into(),
                ],
            })
            .collect()
    }

    /// The row's sub-agent transcripts' `subagent_search_parts` rows of
    /// `parts`, keyed by the path of the transcript holding each.
    pub(super) fn search_part_rows(
        &self,
        parts: &[SubagentSearchPart],
    ) -> Vec<PositionalRow<String>> {
        parts
            .iter()
            .map(|part| PositionalRow {
                key: self
                    .files
                    .get(part.file_position)
                    .map(|file| path_text(&file.path))
                    .unwrap_or_default(),
                values: vec![
                    (part.file_position as i64).into(),
                    // Only search reads these texts, without terminal codes,
                    // so their codes are dropped.
                    crate::agent::sanitize::split_terminal_codes(&part.text)
                        .text
                        .into_owned()
                        .into(),
                ],
            })
            .collect()
    }
    /// The row's identity and files, the stub's files with `stamps`, before
    /// anything the read found. `None` when the stub has no fingerprint.
    fn of_stub(
        storage: SessionCache,
        source: Source,
        root: &SessionRoot,
        stub: &SessionStub,
        stamps: StubStamps,
        status: SessionStatus,
    ) -> Option<Self> {
        let fingerprint = stub.fingerprint.stamp()?;
        Some(Self {
            storage: storage.directory,
            source: source.label(),
            root: root.clone(),
            cache_key: stub.cache_key.clone(),
            locator: path_text(&stub.locator),
            status,
            fingerprint,
            session_id: None,
            session_id_key: None,
            handle: None,
            project_path: None,
            project_name: None,
            cwd: None,
            timestamp_ms: None,
            preview_first: None,
            preview_last: None,
            summary: None,
            custom_title: None,
            model: None,
            total_tokens: None,
            duration_minutes: None,
            message_count: None,
            assistant_messages: None,
            is_missing_earlier_turns: None,
            files: stamps
                .0
                .into_iter()
                .enumerate()
                .map(|(index, stamp)| {
                    let kind = if index == 0 { "locator" } else { "subagent" };
                    SessionFile::new(kind, stamp.path, stamp.fingerprint)
                })
                .collect(),
            parse_errors: Vec::new(),
        })
    }

    /// Add each continuation file `transcripts` read to the row's files,
    /// stamped as before its read. Fails with `SessionCacheSourceChanged`
    /// when a file's stamp now differs from the row's: the file changed
    /// after its stamp was taken, so the parse may hold either version.
    fn add_transcripts_read(
        &mut self,
        transcripts: &SessionTranscripts,
    ) -> crate::error::Result<()> {
        use crate::error::AppError;
        for path in transcripts.transcripts_read() {
            if !self.lists_file(path) {
                let fingerprint = transcripts
                    .stamp_before_read(path)
                    .and_then(|stamp| stamp.fingerprint)
                    .ok_or_else(|| AppError::SessionCacheSourceChanged(path.to_path_buf()))?;
                self.files.push(SessionFile::new(
                    "continuation",
                    path.to_path_buf(),
                    Some(fingerprint),
                ));
            }
        }
        for file in &self.files {
            if CachedFingerprint::of_file(&file.path)? != file.fingerprint {
                return Err(AppError::SessionCacheSourceChanged(file.path.clone()));
            }
        }
        Ok(())
    }

    /// True when the row's files hold `path`, as the parse that names it
    /// spells it.
    fn lists_file(&self, path: &Path) -> bool {
        self.files.iter().any(|file| file.path == path)
    }

    fn describe(&mut self, stub: &SessionStub, conversation: &Conversation) {
        let source = conversation.source;
        let project_path = conversation
            .project_path
            .clone()
            .or_else(|| conversation.cwd.clone())
            .unwrap_or_else(|| "unknown".into());
        let recorded = RecordedSession {
            project_dir: conversation.cwd.clone(),
            session_id: conversation.session_id.clone(),
        };
        self.handle = handle(source, stub, Some(&recorded));
        self.session_id_key =
            Some(crate::search::session_id_for_lookup(&conversation.session_id).into_owned());
        self.session_id = Some(conversation.session_id.clone());
        self.project_name = Some(crate::history::format_short_name_from_path(&project_path));
        self.project_path = Some(path_text(&project_path));
        self.cwd = conversation.cwd.as_deref().map(path_text);
        self.timestamp_ms = Some(conversation.timestamp.timestamp_millis());
        self.preview_first = Some(conversation.preview_first.clone());
        self.preview_last = Some(conversation.preview_last.clone());
        self.summary.clone_from(&conversation.summary);
        self.custom_title.clone_from(&conversation.custom_title);
        self.model.clone_from(&conversation.model);
        self.total_tokens = Some(conversation.total_tokens as i64);
        self.duration_minutes = conversation.duration_minutes.map(|minutes| minutes as i64);
        self.message_count = Some(conversation.message_count as i64);
        self.assistant_messages = Some(conversation.assistant_messages as i64);
        self.is_missing_earlier_turns = Some(conversation.is_missing_earlier_turns);
        self.parse_errors.clone_from(&conversation.parse_errors);
        // The row's sub-agents, not the stub's: a row leaves out the
        // sub-agents of discarded turns. The stub's fingerprint still spans
        // every sub-agent transcript.
        let mut stamped: HashMap<PathBuf, Option<CachedFingerprint>> = HashMap::new();
        self.files.retain(|file| {
            if file.kind != "subagent" {
                return true;
            }
            stamped.insert(file.path.clone(), file.fingerprint);
            false
        });
        self.files.extend(conversation.subagents.iter().map(|file| {
            let fingerprint = stamped
                .get(file)
                .copied()
                .unwrap_or_else(|| file_fingerprint(file));
            SessionFile::new("subagent", file.clone(), fingerprint)
        }));
    }

    /// Give each of the row's files what `transcripts`' read of it leaves
    /// for the next read. A continuation file is read whole.
    fn add_reads(&mut self, transcripts: &SessionTranscripts) {
        for file in &mut self.files {
            let read = match file.kind {
                "locator" => Some(transcripts.own_read()),
                "subagent" => transcripts.subagent_read(&file.path),
                _ => None,
            };
            if let Some(read) = read {
                file.read = read.clone();
            }
            if file.kind == "subagent"
                && let Some(fold) = transcripts.subagent_fold(&file.path)
            {
                file.fold = fold.to_vec();
            }
        }
    }

    /// The row's `transcript_reads` rows, one per file at its position,
    /// keyed by the file's path.
    pub(super) fn read_rows(&self) -> Vec<PositionalRow<String>> {
        self.files
            .iter()
            .map(|file| {
                let path = path_text(&file.path);
                let read = &file.read;
                PositionalRow {
                    key: path.clone(),
                    values: vec![
                        path.into(),
                        read.cursor.clone().into(),
                        read.stable_cursor.clone().into(),
                        read.identity.clone().into(),
                        read.rewrite_check.clone().into(),
                        read.parser_state.clone().into(),
                        read.stable_line.map(|line| line as i64).into(),
                        file.fold.clone().into(),
                    ],
                }
            })
            .collect()
    }
}

/// The full digest of the `ch_` handle the agent commands give the session
/// at `stub`, by the same rule as their keys; `None` when nothing names the
/// session.
fn handle(
    source: Source,
    stub: &SessionStub,
    recorded: Option<&RecordedSession>,
) -> Option<String> {
    let key = crate::agent::refs::loaded_session_key(source.provider(), stub, recorded)?;
    Some(key.conversation_ref().digest_hex().to_owned())
}

/// The fingerprint of the file at `path`; `None` for a locator that names no
/// file, as an OpenCode session's does.
fn file_fingerprint(path: &Path) -> Option<CachedFingerprint> {
    CachedFingerprint::of_file(path).ok().flatten()
}
