//! A session's catalogue row, built from what a load read of it, and the
//! record a load hands the database writer.

use super::SessionAddress;
use super::entries::path_text;
use super::positional::{PositionalRow, PositionalTable, SESSION_FILES, TRANSCRIPT_READS};
use super::roots::{FilePlace, Places, SessionRoots, StoredPath, WrittenPlaces};
use crate::history::fingerprint::{CachedFingerprint, FileStamp};
use crate::history::parser::{SessionTranscripts, SubagentMetadataPart};
use crate::history::path::CanonicalDirectories;
use crate::history::provider::{
    RecordedSession, SessionCache, SessionStub, ThreadLinker, TranscriptRead,
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

/// One session as a load read it, for the database writer: its catalogue
/// row and, for a listed session, its entries.
pub(crate) struct SessionRecord {
    pub(super) catalogue: CatalogueRow,
    /// `None` for a session that is not listed.
    pub(super) entries: Option<RecordEntries>,
    /// The warning an agent command prints for a session that is not
    /// listed, as [`crate::agent::diagnostic::AgentWarning::to_recorded`]
    /// writes it; `None` for a listed one.
    pub(super) unlisted_reason: Option<String>,
    pub(super) subagent_metadata: Vec<SubagentMetadataPart>,
    /// The checkpoint a resumed read of the session starts from; `None` for
    /// a session whose next change is read whole.
    pub(super) checkpoint: Option<CheckpointRecord>,
    /// For a record of a resumed read: what changed, written over the
    /// stored session in place of its whole entries.
    pub(super) update: Option<Box<super::window::WindowUpdate>>,
}

/// A listed session's entries, as its record carries them to the writer.
pub(crate) enum RecordEntries {
    /// Written into the copy with the record.
    Written(TranscriptEntries),
    /// Kept out of the copy, which stores `reason` as its entries error:
    /// a file changed after its stamp. `entries` are the parse's.
    Unwritten {
        reason: String,
        entries: TranscriptEntries,
    },
}

impl RecordEntries {
    /// The entries the copy is written with; `None` when they are kept out.
    pub(super) fn written(&self) -> Option<&TranscriptEntries> {
        match self {
            Self::Written(entries) => Some(entries),
            Self::Unwritten { .. } => None,
        }
    }

    fn into_entries(self) -> TranscriptEntries {
        match self {
            Self::Written(entries) | Self::Unwritten { entries, .. } => entries,
        }
    }
}

/// The checkpoint a resumed read of a session starts from and the agent's
/// ledger of its threads' links, each naming transcripts by their full paths,
/// with the linker that reads the ledger.
pub(crate) struct CheckpointRecord {
    checkpoint: SessionCheckpoint,
    ledger: Vec<u8>,
    linker: &'static dyn ThreadLinker,
}

impl CheckpointRecord {
    /// The checkpoint and the ledger as the cache stores them: each
    /// transcript named by the `roots` row of its root, as `places` holds
    /// it, and its path below that root. `None` when a transcript is outside
    /// every root `places` holds; the session's next change is then read
    /// whole.
    pub(super) fn stored_in(&self, places: &Places) -> Option<(SessionCheckpoint, Vec<u8>)> {
        let rename = |path: &Path| places.saved_name(path);
        Some((
            self.checkpoint.clone().with_transcripts_renamed(&rename)?,
            self.linker
                .with_transcripts_renamed(&self.ledger, &rename)?,
        ))
    }
}

/// A listed session's parse, as [`SessionRecord::listed`] records it.
pub(crate) enum ListedRecord {
    Built(Box<SessionRecord>),
    /// The stub does not have a fingerprint to validate a record against;
    /// the parse's entries stay with the read.
    WithoutFingerprint(TranscriptEntries),
    /// Building the record failed with `error`, as for a file outside every
    /// root; the parse's entries stay with the read.
    Failed {
        error: crate::error::AppError,
        entries: TranscriptEntries,
    },
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

    /// The stamp of `path`.
    pub(crate) fn fingerprint_of(&self, path: &Path) -> Option<CachedFingerprint> {
        self.0
            .iter()
            .find(|stamp| stamp.path == path)
            .and_then(|stamp| stamp.fingerprint)
    }
}

/// The storage and agent that keep one root's sessions, the root and the
/// directories beside it, and the directories the reading load canonicalised
/// for the records' references: every record of the root's sessions shares
/// them.
#[derive(Clone)]
pub(crate) struct RecordPlace<'a> {
    pub(crate) storage: SessionCache,
    pub(crate) source: Source,
    pub(crate) roots: SessionRoots,
    pub(crate) directories: &'a CanonicalDirectories,
    /// The agent's linker, which names the transcripts its ledger holds.
    pub(crate) linker: &'static dyn ThreadLinker,
}

impl RecordPlace<'_> {
    /// The address of the session `stub` names under this place's root,
    /// with the roots' keys as this load canonicalised them.
    pub(crate) fn address_of(&self, stub: &SessionStub) -> SessionAddress {
        SessionAddress::new(
            self.storage,
            self.source,
            self.roots.clone(),
            stub.cache_key.clone(),
        )
    }
}

impl SessionRecord {
    /// The record of `conversation`, the session at `stub` under `place`'s
    /// root, with the entries of `transcripts`, the parse that built it.
    /// `stamps` are the stub's files stamped before that parse.
    pub(crate) fn listed(
        place: &RecordPlace<'_>,
        stub: &SessionStub,
        stamps: StubStamps,
        conversation: &Conversation,
        mut transcripts: SessionTranscripts,
    ) -> ListedRecord {
        let described = (|| {
            let Some(mut catalogue) =
                CatalogueRow::of_stub(place, stub, stamps, SessionStatus::Listed)?
            else {
                return Ok(None);
            };
            catalogue.describe(place, stub, conversation)?;
            catalogue.add_continuation_files(&transcripts)?;
            Ok(Some(catalogue))
        })();
        let mut catalogue = match described {
            Ok(Some(catalogue)) => catalogue,
            Ok(None) => {
                return ListedRecord::WithoutFingerprint(
                    transcripts.into_entries_and_subagent_metadata().0,
                );
            }
            Err(error) => {
                return ListedRecord::Failed {
                    error,
                    entries: transcripts.into_entries_and_subagent_metadata().0,
                };
            }
        };
        let added = catalogue.check_stamps();
        catalogue.add_reads(&transcripts);
        let checkpoint = transcripts
            .checkpoint
            .take()
            .map(|checkpoint| CheckpointRecord {
                checkpoint,
                ledger: std::mem::take(&mut transcripts.ledger),
                linker: place.linker,
            });
        let (entries, subagent_metadata) = transcripts.into_entries_and_subagent_metadata();
        let (entries, subagent_metadata) = match added {
            Ok(()) => (RecordEntries::Written(entries), subagent_metadata),
            Err(error) => (
                RecordEntries::Unwritten {
                    reason: error.to_string(),
                    entries,
                },
                Vec::new(),
            ),
        };
        ListedRecord::Built(Box::new(Self {
            catalogue,
            entries: Some(entries),
            unlisted_reason: None,
            subagent_metadata,
            checkpoint,
            update: None,
        }))
    }

    /// True when a copy written from this record holds its entries.
    pub(crate) fn writes_its_entries(&self) -> bool {
        matches!(self.entries, Some(RecordEntries::Written(_)))
    }

    /// The entries of this record's whole parse, written or not; `None` for
    /// a session that is not listed and for a resumed read, whose entries
    /// are the stored copy's tail.
    pub(crate) fn into_parsed_entries(self) -> Option<TranscriptEntries> {
        if self.update.is_some() {
            return None;
        }
        self.entries.map(RecordEntries::into_entries)
    }

    /// The record of `conversation`, a resumed read of the session at `stub`
    /// under `place`'s root, whose files were stamped as `stamps` before
    /// the read. `reads` gives each file read again what its read leaves
    /// for the next and its share of the row, `stored` each other file's.
    /// `None` when a file changed after its stamp was taken, or the stub
    /// does not have a fingerprint. Fails for a file of the session outside
    /// every root of `place`.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn resumed(
        place: &RecordPlace<'_>,
        stub: &SessionStub,
        stamps: StubStamps,
        conversation: &Conversation,
        reads: HashMap<PathBuf, (TranscriptRead, Vec<u8>)>,
        entries: TranscriptEntries,
        subagent_metadata: Vec<SubagentMetadataPart>,
        checkpoint: Option<(SessionCheckpoint, Vec<u8>)>,
        update: super::window::WindowUpdate,
    ) -> crate::error::Result<Option<Self>> {
        let Some(mut catalogue) =
            CatalogueRow::of_stub(place, stub, stamps, SessionStatus::Listed)?
        else {
            return Ok(None);
        };
        catalogue.describe(place, stub, conversation)?;
        for file in &mut catalogue.files {
            if CachedFingerprint::of_file(&file.path).ok() != Some(file.fingerprint) {
                return Ok(None);
            }
            if let Some((read, fold)) = reads.get(&file.path) {
                file.read = read.clone();
                file.fold = fold.clone();
            }
        }
        let checkpoint = checkpoint.map(|(checkpoint, ledger)| CheckpointRecord {
            checkpoint,
            ledger,
            linker: place.linker,
        });
        Ok(Some(Self {
            catalogue,
            entries: Some(RecordEntries::Written(entries)),
            unlisted_reason: None,
            subagent_metadata,
            checkpoint,
            update: Some(Box::new(update)),
        }))
    }

    /// The record of the session at `stub` under `place`'s root, read as
    /// `status`, which is not [`SessionStatus::Listed`], with `reason`, the
    /// warning an agent command prints for it. Fails for a file of the
    /// session outside every root of `place`.
    pub(crate) fn unlisted(
        place: &RecordPlace<'_>,
        stub: &SessionStub,
        status: SessionStatus,
        reason: String,
    ) -> crate::error::Result<Option<Self>> {
        let stamps = StubStamps::current(stub);
        let Some(mut catalogue) = CatalogueRow::of_stub(place, stub, stamps, status)? else {
            return Ok(None);
        };
        catalogue.handle = handle(place, stub, None);
        Ok(Some(Self {
            catalogue,
            entries: None,
            unlisted_reason: Some(reason),
            subagent_metadata: Vec::new(),
            checkpoint: None,
            update: None,
        }))
    }
}

/// A session's row in the `sessions` table, with the rows of
/// `session_files` and `parse_errors` that belong to it.
pub(super) struct CatalogueRow {
    pub(super) storage: &'static str,
    pub(super) source: &'static str,
    pub(super) roots: SessionRoots,
    pub(super) cache_key: String,
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
    /// The file's path as discovery spells it, or as the transcript naming
    /// the file spells it.
    pub(super) path: PathBuf,
    /// `path` below the session's roots.
    pub(super) place: FilePlace,
    pub(super) fingerprint: Option<CachedFingerprint>,
    /// The state the parse's read of the file leaves for the next read: the
    /// full read's values for a file read through the default, or not read.
    pub(super) read: TranscriptRead,
    /// A sub-agent transcript's encoded [`crate::history::resumed::ThreadFold`];
    /// empty for any other file.
    pub(super) fold: Vec<u8>,
}

/// A row's place in a write: the `roots` row of the root it lists under,
/// and the stored path of each of its files, in order.
pub(super) struct CatalogueKeys {
    pub(super) root: i64,
    pub(super) files: Vec<StoredPath>,
    /// The rows of the roots the row's files are stored below. They give the
    /// transcripts of the row's entries their stored paths.
    pub(super) places: WrittenPlaces,
}

/// The place of a stored row of `table`, from its `root` and `path`
/// columns. Fails for a column of another type than the schema gives it.
fn stored_path_of(table: &PositionalTable, values: &[Value]) -> crate::error::Result<StoredPath> {
    Ok(StoredPath {
        root: table.integer(values, "root")?,
        path: table.text(values, "path")?,
    })
}

/// The `sessions` columns [`CatalogueRow::values`] gives, after `storage`,
/// `root` and `cache_key`.
const COLUMNS: &[&str] = &[
    "source",
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

/// `INSERT` of a `sessions` row: `storage`, `root` and `cache_key` (`?1` to
/// `?3`), then [`CatalogueRow::values`].
pub(super) fn insert_sql() -> String {
    let parameters: Vec<String> = (4..COLUMNS.len() + 4)
        .map(|index| format!("?{index}"))
        .collect();
    format!(
        "INSERT INTO sessions (storage, root, cache_key, {}) VALUES (?1, ?2, ?3, {})",
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
/// place.
pub(super) fn file_key_of(values: &[Value]) -> crate::error::Result<(String, StoredPath)> {
    Ok((
        SESSION_FILES.text(values, "kind")?,
        stored_path_of(&SESSION_FILES, values)?,
    ))
}

/// The place of a stored `session_files` row's file.
pub(super) fn file_place_of(values: &[Value]) -> crate::error::Result<StoredPath> {
    stored_path_of(&SESSION_FILES, values)
}

/// The key a stored `transcript_reads` row matches a new one by: its file's
/// place.
pub(super) fn read_key_of(values: &[Value]) -> crate::error::Result<StoredPath> {
    stored_path_of(&TRANSCRIPT_READS, values)
}

impl CatalogueRow {
    /// The row's `sessions` columns, as [`COLUMNS`] lists them, with
    /// `status`, `message_count`, `entries_error` and `generation`.
    pub(super) fn values(
        &self,
        status: &str,
        message_count: Option<i64>,
        entries_error: Option<&str>,
        generation: i64,
    ) -> Vec<Value> {
        let text = |text: &Option<String>| Value::from(text.clone());
        vec![
            self.source.to_owned().into(),
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

    /// The row's `session_files` rows, with `keys`, the places of its files.
    pub(super) fn file_rows(
        &self,
        keys: &CatalogueKeys,
    ) -> Vec<PositionalRow<(String, StoredPath)>> {
        self.files
            .iter()
            .zip(&keys.files)
            .map(|(file, place)| PositionalRow {
                key: (file.kind.to_owned(), place.clone()),
                values: vec![
                    file.kind.to_owned().into(),
                    place.root.into(),
                    place.path.clone().into(),
                    place.key().as_str().to_owned().into(),
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

    /// The sub-agent metadata parts of `parts`, each keyed by its
    /// transcript's file in `keys`.
    pub(super) fn subagent_metadata_rows(
        keys: &CatalogueKeys,
        parts: &[SubagentMetadataPart],
    ) -> Vec<PositionalRow<Option<StoredPath>>> {
        parts
            .iter()
            .map(|part| {
                let (kind, text) =
                    super::entries::SearchOnlyKind::SubagentMetadata.columns(&part.text);
                PositionalRow {
                    key: keys.files.get(part.file_position).cloned(),
                    values: vec![
                        kind.to_owned().into(),
                        super::SearchField::Supplement.code().into(),
                        (part.file_position as i64).into(),
                        text.into_owned().into(),
                    ],
                }
            })
            .collect()
    }
    /// The row's identity and files, the stub's files with `stamps`, before
    /// anything the read found. `None` when the stub does not have a
    /// fingerprint. Fails for a file outside every root of `place`.
    fn of_stub(
        place: &RecordPlace<'_>,
        stub: &SessionStub,
        stamps: StubStamps,
        status: SessionStatus,
    ) -> crate::error::Result<Option<Self>> {
        let Some(fingerprint) = stub.fingerprint.stamp() else {
            return Ok(None);
        };
        let mut row = Self {
            storage: place.storage.directory,
            source: place.source.label(),
            roots: place.roots.clone(),
            cache_key: stub.cache_key.clone(),
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
            files: Vec::with_capacity(stamps.0.len()),
            parse_errors: Vec::new(),
        };
        for (index, stamp) in stamps.0.into_iter().enumerate() {
            let kind = if index == 0 { "locator" } else { "subagent" };
            row.add_file(kind, stamp.path, stamp.fingerprint)?;
        }
        Ok(Some(row))
    }

    /// Add the file at `path` to the row's files, placed below the row's
    /// roots. Fails for a file outside them all.
    fn add_file(
        &mut self,
        kind: &'static str,
        path: PathBuf,
        fingerprint: Option<CachedFingerprint>,
    ) -> crate::error::Result<()> {
        let session = self.files.first().map_or(path.as_path(), |own| &own.path);
        let place = self.roots.place(session, &path)?;
        self.files.push(SessionFile {
            kind,
            path,
            place,
            fingerprint,
            read: TranscriptRead::full_read(),
            fold: Vec::new(),
        });
        Ok(())
    }

    /// [`Self::add_file`] for a test that builds a row by hand.
    #[cfg(test)]
    pub(super) fn add_test_file(
        &mut self,
        kind: &'static str,
        path: PathBuf,
        fingerprint: Option<CachedFingerprint>,
    ) {
        self.add_file(kind, path, fingerprint)
            .expect("the test's file lies below its row's roots");
    }

    /// Add each continuation file `transcripts` read to the row's files,
    /// stamped as before its read; a file without that stamp is stamped
    /// `None` and fails [`Self::check_stamps`]. Fails for a file below none
    /// of the row's roots.
    fn add_continuation_files(
        &mut self,
        transcripts: &SessionTranscripts,
    ) -> crate::error::Result<()> {
        for path in transcripts.transcripts_read() {
            if !self.lists_file(path) {
                let fingerprint = transcripts
                    .stamp_before_read(path)
                    .and_then(|stamp| stamp.fingerprint);
                self.add_file("continuation", path.to_path_buf(), fingerprint)?;
            }
        }
        Ok(())
    }

    /// Fails with `SessionCacheSourceChanged` for a continuation file
    /// without a stamp, and for a file whose stamp now differs from the
    /// row's, because the parse may hold either version of a file that
    /// changed after its stamp was taken.
    fn check_stamps(&self) -> crate::error::Result<()> {
        use crate::error::AppError;
        for file in &self.files {
            if file.kind == "continuation" && file.fingerprint.is_none() {
                return Err(AppError::SessionCacheSourceChanged(file.path.clone()));
            }
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

    /// Fill the row from `conversation`, and take the row's sub-agents for
    /// the stub's. Fails for a sub-agent outside every root of the row.
    fn describe(
        &mut self,
        place: &RecordPlace<'_>,
        stub: &SessionStub,
        conversation: &Conversation,
    ) -> crate::error::Result<()> {
        let project_path = conversation
            .project_path
            .clone()
            .or_else(|| conversation.cwd.clone())
            .unwrap_or_else(|| "unknown".into());
        let recorded = RecordedSession {
            project_dir: conversation.cwd.clone(),
            session_id: conversation.session_id.clone(),
        };
        self.handle = handle(place, stub, Some(&recorded));
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
        for file in &conversation.subagents {
            let fingerprint = stamped
                .get(file)
                .copied()
                .unwrap_or_else(|| file_fingerprint(file));
            self.add_file("subagent", file.clone(), fingerprint)?;
        }
        Ok(())
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
    /// keyed by the file's place, which `keys` gives.
    pub(super) fn read_rows(&self, keys: &CatalogueKeys) -> Vec<PositionalRow<StoredPath>> {
        self.files
            .iter()
            .zip(&keys.files)
            .map(|(file, place)| {
                let read = &file.read;
                PositionalRow {
                    key: place.clone(),
                    values: vec![
                        place.root.into(),
                        place.path.clone().into(),
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
/// session. The recorded directory is canonicalised through `place`'s
/// directories.
fn handle(
    place: &RecordPlace<'_>,
    stub: &SessionStub,
    recorded: Option<&RecordedSession>,
) -> Option<String> {
    let key = crate::agent::refs::loaded_session_key(
        place.source.provider(),
        stub,
        recorded,
        place.directories,
        place.address_of(stub),
    )?;
    Some(key.conversation_ref().digest_hex().to_owned())
}

/// The fingerprint of the file at `path`; `None` for a locator that names no
/// file, as an OpenCode session's does.
fn file_fingerprint(path: &Path) -> Option<CachedFingerprint> {
    CachedFingerprint::of_file(path).ok().flatten()
}
