//! A session's catalogue row, built from what a load read of it, and the
//! record a load hands the database writer.

use super::entries::path_text;
use crate::history::fingerprint::CachedFingerprint;
use crate::history::provider::{RecordedSession, SessionCache, SessionRoot, SessionStub};
use crate::history::{Conversation, ParseError, Source, TranscriptEntries};
use std::collections::BTreeSet;
use std::path::Path;

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
    /// `None` for a session that is not listed; the reason `session_entries`
    /// failed for a listed one.
    pub(super) entries: Option<Result<TranscriptEntries, String>>,
    pub(super) search_parts: Vec<SubagentSearchPart>,
}

pub(super) struct SubagentSearchPart {
    pub(super) file_position: usize,
    pub(super) sanitise_text: bool,
    pub(super) text: String,
}

impl SessionRecord {
    /// The record of `conversation`, `storage`'s session at `stub` under
    /// `root`, with its entries as `session_entries` reads them. `None` when
    /// the stub has no fingerprint to validate the record against.
    pub(crate) fn listed(
        storage: SessionCache,
        source: Source,
        root: &SessionRoot,
        stub: &SessionStub,
        conversation: &Conversation,
    ) -> Option<Self> {
        let mut catalogue =
            CatalogueRow::of_stub(storage, source, root, stub, SessionStatus::Listed)?;
        catalogue.describe(stub, conversation);
        let mut search_parts = Vec::new();
        let entries = catalogue
            .read_entries(source, &stub.locator, &conversation.subagents)
            .map(|(entries, parts)| {
                search_parts = parts;
                entries
            })
            .map_err(|error| error.to_string());
        Some(Self {
            catalogue,
            entries: Some(entries),
            search_parts,
        })
    }

    /// The record of `storage`'s session at `stub` under `root`, read as
    /// `status`, which is not [`SessionStatus::Listed`].
    pub(crate) fn unlisted(
        storage: SessionCache,
        source: Source,
        root: &SessionRoot,
        stub: &SessionStub,
        status: SessionStatus,
    ) -> Option<Self> {
        let mut catalogue = CatalogueRow::of_stub(storage, source, root, stub, status)?;
        catalogue.handle = handle(source, stub, None);
        Some(Self {
            catalogue,
            entries: None,
            search_parts: Vec::new(),
        })
    }
}

/// The search parts and source transcripts of each sub-agent transcript at
/// `subagents` that reads. An unreadable sub-agent transcript is left out,
/// as the session's entries leave it out.
fn read_subagent_search_parts(
    source: Source,
    subagents: &[std::path::PathBuf],
) -> crate::error::Result<(Vec<SubagentSearchPart>, BTreeSet<std::path::PathBuf>)> {
    let mut parts = Vec::new();
    let mut sources = BTreeSet::new();
    for (index, path) in subagents.iter().enumerate() {
        if let Ok(Some(projection)) = source.provider().format().parse_transcript(path) {
            sources.extend(
                projection
                    .entries
                    .iter()
                    .filter_map(|(record, _)| record.transcript.as_deref().map(Path::to_path_buf)),
            );
            parts.extend(
                crate::history::parser::SubagentContribution::search_parts(
                    projection.entries.iter().map(|(_, entry)| entry),
                )
                .into_iter()
                .map(|(full_text, text)| SubagentSearchPart {
                    file_position: index + 1,
                    sanitise_text: full_text,
                    text,
                }),
            );
        }
    }
    Ok((parts, sources))
}

/// A session's row in the `sessions` table, with the rows of
/// `session_files` and `parse_errors` that belong to it.
pub(super) struct CatalogueRow {
    pub(super) storage: &'static str,
    pub(super) source: &'static str,
    pub(super) root: String,
    pub(super) cache_key: String,
    pub(super) locator: String,
    pub(super) canonical_path: String,
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
    pub(super) path: String,
    pub(super) fingerprint: Option<CachedFingerprint>,
}

impl CatalogueRow {
    /// The row's identity and files, before anything the read found. `None`
    /// when the stub has no fingerprint.
    fn of_stub(
        storage: SessionCache,
        source: Source,
        root: &SessionRoot,
        stub: &SessionStub,
        status: SessionStatus,
    ) -> Option<Self> {
        let fingerprint = stub.fingerprint.stamp()?;
        let canonical_path = stub
            .locator
            .canonicalize()
            .unwrap_or_else(|_| stub.locator.clone());
        Some(Self {
            storage: storage.directory,
            source: source.label(),
            root: path_text(&root.path),
            cache_key: stub.cache_key.clone(),
            locator: path_text(&stub.locator),
            canonical_path: path_text(&canonical_path),
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
            files: std::iter::once(&stub.locator)
                .chain(&stub.subagents)
                .enumerate()
                .map(|(index, file)| SessionFile {
                    kind: if index == 0 { "locator" } else { "subagent" },
                    path: path_text(&super::files::key(file)),
                    fingerprint: file_fingerprint(file),
                })
                .collect(),
            parse_errors: Vec::new(),
        })
    }

    fn read_entries(
        &mut self,
        source: Source,
        path: &Path,
        subagents: &[std::path::PathBuf],
    ) -> crate::error::Result<(TranscriptEntries, Vec<SubagentSearchPart>)> {
        use crate::error::AppError;
        let mut entries = crate::history::normalized_session(source, path, subagents)?;
        let (mut search_parts, mut child_sources) = read_subagent_search_parts(source, subagents)?;
        let mut added_dependency = false;
        for path in transcript_paths(&entries)
            .into_iter()
            .chain(child_sources.iter().map(std::path::PathBuf::as_path))
        {
            let key = path_text(&super::files::key(path));
            if !self.files.iter().any(|file| file.path == key) {
                let stamp = super::files::fingerprint(path)?
                    .ok_or_else(|| AppError::SessionCacheSourceChanged(path.to_path_buf()))?;
                self.files.push(SessionFile {
                    kind: "continuation",
                    path: key,
                    fingerprint: Some(stamp),
                });
                added_dependency = true;
            }
        }
        // Discover continuation dependencies before the validated parse.
        if added_dependency {
            entries = crate::history::normalized_session(source, path, subagents)?;
            (search_parts, child_sources) = read_subagent_search_parts(source, subagents)?;
        }
        for path in transcript_paths(&entries)
            .into_iter()
            .chain(child_sources.iter().map(std::path::PathBuf::as_path))
        {
            let key = path_text(&super::files::key(path));
            if !self.files.iter().any(|file| file.path == key) {
                return Err(AppError::SessionCacheSourceChanged(path.to_path_buf()));
            }
        }
        for file in &self.files {
            if super::files::fingerprint(Path::new(&file.path))? != file.fingerprint {
                return Err(AppError::SessionCacheSourceChanged((&file.path).into()));
            }
        }
        Ok((entries, search_parts))
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
        self.files.retain(|file| file.kind != "subagent");
        self.files
            .extend(conversation.subagents.iter().map(|file| SessionFile {
                kind: "subagent",
                path: path_text(&super::files::key(file)),
                fingerprint: file_fingerprint(file),
            }));
    }
}

/// `restored`, the sub-agent transcripts of a row the database restored for
/// the session at `stub`, spelled as `stub` lists them: the database keys
/// each by its canonical path. A transcript `stub` does not list keeps the
/// database's spelling.
pub(crate) fn subagents_as_listed(
    stub: &SessionStub,
    restored: Vec<std::path::PathBuf>,
) -> Vec<std::path::PathBuf> {
    restored
        .into_iter()
        .map(|key| {
            stub.subagents
                .iter()
                .find(|listed| super::files::key(listed) == key)
                .cloned()
                .unwrap_or(key)
        })
        .collect()
}

fn transcript_paths(entries: &TranscriptEntries) -> BTreeSet<&Path> {
    entries
        .entries
        .iter()
        .filter_map(|(record, _)| record.transcript.as_deref())
        .collect()
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
    super::files::fingerprint(path).ok().flatten()
}
