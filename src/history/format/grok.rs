//! The Grok Build transcript: `updates.jsonl`, the append-only ACP update
//! stream Grok keeps in each session directory, with `summary.json` beside it
//! for the session's id, working directory, title and kind.
//!
//! Every line is an envelope, `{timestamp, method, params: {update, _meta}}`.
//! The reader is a projection: message and thought chunks, tool calls and
//! their results, turn usage and recaps; every other update kind is skipped
//! without being an error, since Grok adds kinds between versions.
//!
//! The sibling `summary.json` naming an `info.id` claims the transcript,
//! because the transcript does not name its agent. A session closed before
//! its first prompt has a summary and no transcript; it reads as a session
//! with no entries.
//!
//! Grok appends to the transcript and rewrites `summary.json`, so a read of
//! a changed transcript resumes from its stable cursor and reads
//! `summary.json` again.

use super::dropped::DroppedRecords;
use super::lines::{self, LineCursor, visit_line_spans_from};
use super::{
    EarlierTurns, RecordLine, SessionFormat, SessionHeader, SessionProjection, append_exit_code,
    at_own_lines,
};
use super::{rename_key, rfc3339_from_millis};
use crate::error::Result;
use crate::history::provider::{
    ResumedWindow, Resumption, SessionTitle, TranscriptRead, TranscriptReader,
};
use crate::history::{MalformedLine, ParsePermit, Source};
use crate::log_entry::{
    AssistantMessage, ContentBlock, LogEntry, RECAP_LABEL, TokenUsage, Tool, UserContent,
    UserMessage, replacement_diff,
};
use crate::parallel::ParallelThreshold;
use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::ffi::OsStr;
use std::fs::File;
use std::io::{self, BufRead, BufReader, ErrorKind, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// The transcript's file name in a session directory, and the locator's last
/// component.
pub(crate) const UPDATES_FILE: &str = "updates.jsonl";

/// The session header and title beside the transcript.
pub(crate) const SUMMARY_FILE: &str = "summary.json";

pub struct GrokUpdatesFormat;

pub static GROK_UPDATES: GrokUpdatesFormat = GrokUpdatesFormat;

impl SessionFormat for GrokUpdatesFormat {
    /// The dropped-records report shows every Grok session's version as
    /// unknown: Grok writes no release version into a session's files.
    fn read_transcript(
        &self,
        _admitted: crate::history::Admitted,
        path: &Path,
        dropped: DroppedRecords,
    ) -> Result<Option<SessionProjection>> {
        if path.file_name() != Some(OsStr::new(UPDATES_FILE)) {
            return Ok(None);
        }
        let Some(summary) = session_summary(path.parent().unwrap_or_else(|| Path::new(""))) else {
            return Ok(None);
        };
        let (updates, transcript_read) = match File::open(path) {
            Ok(file) => {
                let mut reader = BufReader::new(file);
                let mut projector =
                    Projector::new(LineCursor::START, StreamState::default(), dropped);
                let cursor =
                    projector.read(&mut reader, LineCursor::START, ParallelThreshold::DEFAULT)?;
                let transcript_read = match projector.first_line {
                    Some(first_line) => projector
                        .end(cursor)
                        .transcript_read(stream_identity(first_line, &summary), reader.get_mut())?,
                    None => TranscriptRead::full_read(),
                };
                (projector.finish(), transcript_read)
            }
            Err(error) if error.kind() == ErrorKind::NotFound => (
                ParsedUpdates {
                    dropped,
                    ..ParsedUpdates::default()
                },
                TranscriptRead::full_read(),
            ),
            Err(error) => return Err(error.into()),
        };
        Ok(Some(projection(summary, updates, transcript_read)))
    }
}

/// Resumes a read of a Grok transcript from its stable cursor: the last
/// record boundary where no message is still streaming and no call the
/// current turn opened waits for its result, since a later update completes
/// a call's input. The identity is the transcript's first line with what the
/// read takes from `summary.json` besides the title and the model; every
/// read takes those again.
impl TranscriptReader for GrokUpdatesFormat {
    fn identity(&self, source: &Path) -> Result<Vec<u8>> {
        if source.file_name() != Some(OsStr::new(UPDATES_FILE)) {
            return Ok(Vec::new());
        }
        let Some(summary) = session_summary(source.parent().unwrap_or_else(|| Path::new("")))
        else {
            return Ok(Vec::new());
        };
        let mut reader = BufReader::new(File::open(source)?);
        let mut line = String::new();
        loop {
            line.clear();
            if reader.read_line(&mut line)? == 0 || !line.ends_with('\n') {
                return Ok(Vec::new());
            }
            line.pop();
            if line.ends_with('\r') {
                line.pop();
            }
            if !line.trim().is_empty() {
                return Ok(stream_identity(blake3::hash(line.as_bytes()), &summary));
            }
        }
    }

    fn is_rewritten(&self, source: &Path, read: &TranscriptRead) -> Result<bool> {
        lines::is_rewritten(source, read)
    }

    fn read_from(
        &self,
        permit: ParsePermit<'_>,
        source: &Path,
        read: &TranscriptRead,
    ) -> Result<Resumption> {
        permit.admit();
        let (Some(stable_cursor), Ok(state)) = (
            LineCursor::decode(&read.stable_cursor),
            serde_json::from_slice::<StreamRead>(&read.parser_state),
        ) else {
            return Ok(Resumption::FullRead);
        };
        let mut reader = BufReader::new(File::open(source)?);
        reader.seek(SeekFrom::Start(stable_cursor.offset))?;
        let mut projector =
            Projector::new(stable_cursor, state.at_stable, DroppedRecords::default());
        let cursor = projector.read(&mut reader, stable_cursor, ParallelThreshold::DEFAULT)?;
        if projector.changes_earlier_entry {
            return Ok(Resumption::ChangesEarlierEntry);
        }
        let bytes_read = reader
            .get_ref()
            .metadata()?
            .len()
            .saturating_sub(stable_cursor.offset);
        let next_read = projector
            .end(cursor)
            .transcript_read(read.identity.clone(), reader.get_mut())?;
        let updates = projector.finish();
        Ok(Resumption::Window(ResumedWindow {
            start_line: stable_cursor.line,
            entries: at_own_lines(updates.entries),
            malformed_lines: updates
                .malformed_lines
                .into_iter()
                .map(|line_number| MalformedLine {
                    line_number,
                    detail: None,
                })
                .collect(),
            read: next_read,
            bytes_read,
        }))
    }

    fn read_whole(
        &self,
        permit: ParsePermit<'_>,
        source: &Path,
    ) -> Result<Option<SessionProjection>> {
        self.parse_transcript(permit, source)
    }

    /// The entries `summary.json` adds: the session's title, and the
    /// summary's model while the transcript, as `read` read it, names none.
    fn leading_entries(
        &self,
        source: &Path,
        read: &TranscriptRead,
    ) -> Result<Vec<(RecordLine, LogEntry)>> {
        let Some(summary) = session_summary(source.parent().unwrap_or_else(|| Path::new("")))
        else {
            return Ok(Vec::new());
        };
        let names_a_model = serde_json::from_slice::<StreamRead>(&read.parser_state)
            .is_ok_and(|stream| stream.names_a_model);
        let (entries, _) = summary_entries(&summary, names_a_model);
        Ok(entries
            .into_iter()
            .map(|entry| (RecordLine::from(0), entry))
            .collect())
    }
}

/// The transcript's identity: a hash of its first line, given as
/// `first_line`'s hash, and of what a read takes from `summary` besides the
/// title and the model.
fn stream_identity(first_line: blake3::Hash, summary: &SessionSummary) -> Vec<u8> {
    let facts = serde_json::to_vec(&(
        first_line.to_hex().as_str(),
        &summary.id,
        summary.cwd.as_deref().map(Path::to_string_lossy),
        &summary.created_at,
        &summary.session_kind,
        &summary.agent_name,
    ))
    .expect("a transcript's identity facts serialize");
    blake3::hash(&facts).as_bytes().to_vec()
}

/// The `summary.json` fields this reader uses. Only `info.id` is required:
/// Grok's own tests write a placeholder summary, and a session directory must
/// not stop being Grok's because a field this reader does not need is absent.
#[derive(Clone, Debug, Default)]
pub(crate) struct SessionSummary {
    pub(crate) id: String,
    cwd: Option<PathBuf>,
    created_at: Option<String>,
    generated_title: Option<String>,
    title_is_manual: bool,
    session_summary: Option<String>,
    session_kind: Option<String>,
    agent_name: Option<String>,
    current_model_id: Option<String>,
}

impl SessionSummary {
    /// The title the user chose, else the one Grok generated, else Grok's
    /// one-line summary of the session.
    pub(crate) fn title(&self) -> Option<SessionTitle> {
        match &self.generated_title {
            Some(title) if self.title_is_manual => Some(SessionTitle::Custom(title.clone())),
            Some(title) => Some(SessionTitle::Generated(title.clone())),
            None => self.session_summary.clone().map(SessionTitle::Generated),
        }
    }

    /// True for a sub-agent's session. It is read through its parent's row
    /// and carries no title of its own.
    fn is_subagent(&self) -> bool {
        self.session_kind
            .as_deref()
            .is_some_and(|kind| kind.starts_with("subagent"))
    }
}

/// The summary in `session_dir`, or `None` when it is missing, unreadable or
/// names no session id.
pub(crate) fn session_summary(session_dir: &Path) -> Option<SessionSummary> {
    let contents = std::fs::read_to_string(session_dir.join(SUMMARY_FILE)).ok()?;
    let value = serde_json::from_str::<Value>(&contents).ok()?;
    let id = value
        .pointer("/info/id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())?;
    let text = |field: &str| {
        value
            .get(field)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_owned)
    };
    Some(SessionSummary {
        id: id.to_owned(),
        cwd: value
            .pointer("/info/cwd")
            .and_then(Value::as_str)
            .map(PathBuf::from),
        created_at: text("created_at"),
        generated_title: text("generated_title"),
        title_is_manual: value
            .get("title_is_manual")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        session_summary: text("session_summary"),
        session_kind: text("session_kind"),
        agent_name: text("agent_name"),
        current_model_id: text("current_model_id"),
    })
}

/// The entries `summary` adds before the transcript's, at line 0, and the
/// title's text: the session's title, then the summary's model when the
/// transcript names none, as no completed turn did when `named_a_model` is
/// false.
fn summary_entries(
    summary: &SessionSummary,
    named_a_model: bool,
) -> (Vec<LogEntry>, Option<String>) {
    let mut entries = Vec::new();
    let mut title = None;
    if !summary.is_subagent()
        && let Some(name) = summary.title()
    {
        let (entry, text) = match name {
            SessionTitle::Custom(text) => (
                LogEntry::CustomTitle {
                    custom_title: text.clone(),
                },
                text,
            ),
            SessionTitle::Generated(text) => (
                LogEntry::AiTitle {
                    ai_title: text.clone(),
                },
                text,
            ),
        };
        entries.push(entry);
        title = Some(text);
    }
    if !named_a_model && let Some(model) = summary.current_model_id.clone() {
        entries.push(model_entry(model, None));
    }
    (entries, title)
}

fn projection(
    summary: SessionSummary,
    updates: ParsedUpdates,
    transcript_read: TranscriptRead,
) -> SessionProjection {
    // Line 0: the title and the model live in summary.json, not the
    // transcript.
    let (leading, title) = summary_entries(&summary, updates.named_a_model);
    let entries = leading
        .into_iter()
        .map(|entry| (0, entry))
        .chain(updates.entries)
        .collect();
    let thread_label = summary
        .is_subagent()
        .then(|| summary.agent_name.clone())
        .flatten();
    SessionProjection {
        source: Source::Grok,
        header: SessionHeader {
            version: 1,
            timestamp: summary
                .created_at
                .as_deref()
                .map(rfc3339_millis)
                .unwrap_or_default(),
            id: summary.id,
            cwd: summary.cwd.unwrap_or_default(),
            thread_label,
            subagent_identity: Default::default(),
        },
        title,
        entries: at_own_lines(entries),
        leaf_id: None,
        malformed_lines: updates.malformed_lines,
        dropped: updates.dropped,
        earlier_turns: EarlierTurns::Read,
        earlier_transcripts: Vec::new(),
        transcript_read,
    }
}

/// `timestamp` reformatted to milliseconds; a value that does not parse is
/// kept as written.
fn rfc3339_millis(timestamp: &str) -> String {
    DateTime::parse_from_rfc3339(timestamp)
        .map(|parsed| {
            parsed
                .with_timezone(&Utc)
                .to_rfc3339_opts(SecondsFormat::Millis, true)
        })
        .unwrap_or_else(|_| timestamp.to_owned())
}

#[derive(Default)]
struct ParsedUpdates {
    entries: Vec<(usize, LogEntry)>,
    malformed_lines: Vec<usize>,
    named_a_model: bool,
    dropped: DroppedRecords,
}

#[cfg(test)]
fn parse_updates(
    reader: impl BufRead,
    parallel: ParallelThreshold,
    dropped: DroppedRecords,
) -> Result<ParsedUpdates> {
    let mut projector = Projector::new(LineCursor::START, StreamState::default(), dropped);
    projector.read(reader, LineCursor::START, parallel)?;
    Ok(projector.finish())
}

/// What a read of the update stream carries from one record to the next, as
/// the next read restarts with it.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct StreamState {
    last_model: Option<String>,
    named_a_model: bool,
    /// Each sub-agent's type by its session id, from its spawn record.
    subagent_types: BTreeMap<String, String>,
    /// The id of each call opened before the place the state stands at. An
    /// update completing one's input changes an entry before that place.
    earlier_calls: BTreeSet<String>,
}

/// What a read of the update stream leaves for the next, as its
/// [`TranscriptRead::parser_state`].
#[derive(Debug, Deserialize, Serialize)]
struct StreamRead {
    /// The state at the stable cursor.
    at_stable: StreamState,
    /// True when a completed turn named a model by the stream's end: the
    /// summary's model then adds no entry.
    names_a_model: bool,
}

/// Where a read of the update stream stopped, and what it leaves for the
/// next.
struct StreamReadEnd {
    cursor: LineCursor,
    stable_cursor: LineCursor,
    read: StreamRead,
}

impl StreamReadEnd {
    /// What this read leaves for the next, for the transcript `file` holds,
    /// whose identity is `identity`.
    fn transcript_read(&self, identity: Vec<u8>, file: &mut File) -> Result<TranscriptRead> {
        Ok(TranscriptRead {
            cursor: self.cursor.encode(),
            stable_cursor: self.stable_cursor.encode(),
            identity,
            rewrite_check: lines::rewrite_check(file, self.cursor)?,
            parser_state: serde_json::to_vec(&self.read).expect("a stream read serializes"),
            stable_line: Some(self.stable_cursor.line),
        })
    }
}

/// A record boundary a read can restart from: the state there, but for the
/// earlier calls, and how many calls the read had opened.
struct StablePlace {
    cursor: LineCursor,
    state: StreamState,
    opened: usize,
}

/// The update kinds the reader drops on purpose: plans, task lifecycle,
/// compaction, retries and memory. Any other kind it does not read is
/// unrecognised.
const DROPPED_UPDATE_KINDS: &[&str] = &[
    "plan",
    "task_backgrounded",
    "task_completed",
    "background_tasks",
    "auto_compact_started",
    "auto_compact_completed",
    "compaction_checkpoint",
    "retry_state",
    "memory_dream_queued",
    "memory_dream_started",
    "memory_dream_completed",
    "memory_flush_started",
    "memory_flush_completed",
];

/// The update stream folded into entries, one record at a time.
struct Projector {
    entries: Vec<(usize, LogEntry)>,
    malformed_lines: Vec<usize>,
    /// The run of chunk records not yet emitted.
    run: Option<ChunkRun>,
    /// Each tool call this read opened, by call id, with its index in
    /// `entries`, so a later update can complete its input.
    calls: HashMap<String, usize>,
    /// The state the read started with, its earlier calls those opened
    /// before the read, moved on record by record.
    state: StreamState,
    /// The id of each call this read opened, in order.
    opened: Vec<String>,
    /// The calls the current turn opened that wait for their result. A
    /// prompt or a completed turn ends the turn.
    open_turn_calls: HashSet<String>,
    /// True once an update would complete the input of a call opened before
    /// the read.
    changes_earlier_entry: bool,
    /// The last record boundary where no message streamed and no call of
    /// the current turn waited.
    stable: StablePlace,
    /// The hash of the first line holding more than whitespace that ends in
    /// a line ending.
    first_line: Option<blake3::Hash>,
    dropped: DroppedRecords,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ChunkKind {
    User,
    Message,
    Thought,
}

/// The fields adjacent chunk records share when they are one message.
#[derive(PartialEq, Eq)]
struct ChunkKey {
    prompt_id: Option<String>,
    stream_start_ms: Option<i64>,
    prompt_index: Option<i64>,
}

struct ChunkRun {
    kind: ChunkKind,
    key: ChunkKey,
    line: usize,
    timestamp: Option<String>,
    text: String,
}

impl Projector {
    /// A projector reading on from `from` with `state`.
    fn new(from: LineCursor, state: StreamState, dropped: DroppedRecords) -> Self {
        let mut projector = Self {
            entries: Vec::new(),
            malformed_lines: Vec::new(),
            run: None,
            calls: HashMap::new(),
            state,
            opened: Vec::new(),
            open_turn_calls: HashSet::new(),
            changes_earlier_entry: false,
            stable: StablePlace {
                cursor: from,
                state: StreamState::default(),
                opened: 0,
            },
            first_line: None,
            dropped,
        };
        projector.stable = projector.place(from);
        projector
    }

    /// Read `reader`, standing at `from`, to its end, and return the cursor
    /// past the last line with a line ending. A last line without one is
    /// read too, after the stable place is kept.
    fn read(
        &mut self,
        reader: impl BufRead,
        from: LineCursor,
        parallel: ParallelThreshold,
    ) -> io::Result<LineCursor> {
        let parse = |line: &str| serde_json::from_str::<Value>(line).ok();
        let read = visit_line_spans_from(reader, from, parallel, parse, |span, line, record| {
            if self.first_line.is_none() {
                self.first_line = Some(blake3::hash(line.as_bytes()));
            }
            self.add(span.start.line, record);
            if self.run.is_none() && self.open_turn_calls.is_empty() {
                self.stable = self.place(span.end);
            }
        })?;
        if let Some((line_number, line)) = read.unterminated {
            self.add(line_number, parse(&line));
        }
        Ok(read.cursor)
    }

    fn add(&mut self, line: usize, record: Option<Value>) {
        match record {
            Some(Value::Object(record)) => self.record(line, &record),
            _ => self.malformed_lines.push(line),
        }
    }

    /// `cursor` as a place the next read can restart from, with the state
    /// now.
    fn place(&self, cursor: LineCursor) -> StablePlace {
        StablePlace {
            cursor,
            state: StreamState {
                last_model: self.state.last_model.clone(),
                named_a_model: self.state.named_a_model,
                subagent_types: self.state.subagent_types.clone(),
                earlier_calls: BTreeSet::new(),
            },
            opened: self.opened.len(),
        }
    }

    /// The read's end, stopped at `cursor`.
    fn end(&self, cursor: LineCursor) -> StreamReadEnd {
        let mut at_stable = self.stable.state.clone();
        at_stable.earlier_calls = self
            .state
            .earlier_calls
            .iter()
            .chain(&self.opened[..self.stable.opened])
            .cloned()
            .collect();
        StreamReadEnd {
            cursor,
            stable_cursor: self.stable.cursor,
            read: StreamRead {
                at_stable,
                names_a_model: self.state.named_a_model,
            },
        }
    }

    fn record(&mut self, line: usize, record: &Map<String, Value>) {
        let params = record.get("params").and_then(Value::as_object);
        let meta = params
            .and_then(|params| params.get("_meta"))
            .and_then(Value::as_object);
        let Some(update) = params
            .and_then(|params| params.get("update"))
            .and_then(Value::as_object)
        else {
            self.end_run();
            return;
        };
        let timestamp = record_time(record, meta);
        let update_kind = update.get("sessionUpdate").and_then(Value::as_str);
        let chunk_kind = match update_kind {
            Some("user_message_chunk") => Some(ChunkKind::User),
            Some("agent_message_chunk") => Some(ChunkKind::Message),
            Some("agent_thought_chunk") => Some(ChunkKind::Thought),
            _ => None,
        };
        if let Some(chunk_kind) = chunk_kind {
            self.chunk(chunk_kind, line, update, meta, timestamp);
            return;
        }
        self.end_run();
        match update_kind {
            Some("tool_call") => self.open_call(line, update, meta, timestamp),
            Some("tool_call_update") => self.update_call(line, update, meta, timestamp),
            Some("turn_completed") => self.turn_completed(line, update, timestamp),
            Some("session_recap") => self.recap(line, update, timestamp),
            Some("subagent_spawned") => self.subagent_spawned(update),
            Some("subagent_finished") => self.subagent_finished(line, update, timestamp),
            // The `todo_write` call carries plans and the tool results carry
            // task lifecycle; compaction, retries and memory are runtime
            // state.
            Some(kind) if DROPPED_UPDATE_KINDS.contains(&kind) => {
                self.dropped.skipped("sessionUpdate", update_kind);
            }
            _ => self.dropped.unrecognised("sessionUpdate", update_kind),
        }
    }

    /// Join adjacent chunks of one kind into one message when they share a
    /// prompt, a stream start and a prompt index; Grok coalesces most of them
    /// on disk already.
    fn chunk(
        &mut self,
        kind: ChunkKind,
        line: usize,
        update: &Map<String, Value>,
        meta: Option<&Map<String, Value>>,
        timestamp: Option<String>,
    ) {
        let text = update
            .get("content")
            .and_then(|content| content.get("text"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        let key = ChunkKey {
            prompt_id: meta
                .and_then(|meta| meta.get("promptId"))
                .and_then(Value::as_str)
                .map(str::to_owned),
            stream_start_ms: meta
                .and_then(|meta| meta.get("streamStartMs"))
                .and_then(Value::as_i64),
            prompt_index: update
                .get("_meta")
                .and_then(|meta| meta.get("promptIndex"))
                .and_then(Value::as_i64),
        };
        if kind == ChunkKind::User {
            self.open_turn_calls.clear();
        }
        if let Some(run) = &mut self.run
            && run.kind == kind
            && run.key == key
        {
            run.text.push_str(text);
            return;
        }
        self.end_run();
        self.run = Some(ChunkRun {
            kind,
            key,
            line,
            timestamp,
            text: text.to_owned(),
        });
    }

    fn end_run(&mut self) {
        let Some(run) = self.run.take() else {
            return;
        };
        if run.text.trim().is_empty() {
            return;
        }
        let entry = match run.kind {
            ChunkKind::User => {
                user_entry(vec![ContentBlock::Text { text: run.text }], run.timestamp)
            }
            ChunkKind::Message => {
                assistant_entry(vec![ContentBlock::Text { text: run.text }], run.timestamp)
            }
            ChunkKind::Thought => assistant_entry(
                vec![ContentBlock::Thinking {
                    thinking: run.text,
                    signature: String::new(),
                }],
                run.timestamp,
            ),
        };
        self.entries.push((run.line, entry));
    }

    fn open_call(
        &mut self,
        line: usize,
        update: &Map<String, Value>,
        meta: Option<&Map<String, Value>>,
        timestamp: Option<String>,
    ) {
        let id = call_id(update);
        let (name, tool) = tool_identity(update, meta);
        let block = ContentBlock::ToolUse {
            id: id.clone(),
            name,
            tool,
            input: call_input(update, tool),
        };
        self.calls.insert(id.clone(), self.entries.len());
        self.opened.push(id.clone());
        self.open_turn_calls.insert(id);
        self.entries
            .push((line, assistant_entry(vec![block], timestamp)));
    }

    /// An update carries the call's full input, its result, or both. If the
    /// call's opening record is missing, the update opens the call. An update
    /// completing the input of a call opened before the read changes an entry
    /// before it.
    fn update_call(
        &mut self,
        line: usize,
        update: &Map<String, Value>,
        meta: Option<&Map<String, Value>>,
        timestamp: Option<String>,
    ) {
        let id = call_id(update);
        let is_earlier = !self.calls.contains_key(&id) && self.state.earlier_calls.contains(&id);
        if !self.calls.contains_key(&id) && !is_earlier {
            self.open_call(line, update, meta, timestamp.clone());
        } else if update.get("rawInput").is_some_and(Value::is_object) {
            if is_earlier {
                self.changes_earlier_entry = true;
            }
            let (name, tool) = tool_identity(update, meta);
            if let Some(ContentBlock::ToolUse {
                name: call_name,
                tool: call_tool,
                input,
                ..
            }) = self.call_block(&id)
            {
                *input = call_input(update, tool);
                if update
                    .get("_meta")
                    .and_then(|meta| meta.get("x.ai/tool"))
                    .is_some()
                {
                    *call_name = name;
                    *call_tool = tool;
                }
            }
        }
        let status = update.get("status").and_then(Value::as_str);
        if !matches!(status, Some("completed" | "failed")) {
            return;
        }
        self.open_turn_calls.remove(&id);
        if let Some(query) = backend_search_query(update) {
            if is_earlier {
                self.changes_earlier_entry = true;
            } else if let Some(ContentBlock::ToolUse { input, .. }) = self.call_block(&id)
                && let Some(arguments) = input.as_object_mut()
            {
                arguments
                    .entry("query")
                    .or_insert_with(|| Value::String(query));
            }
        }
        let text = result_text(update, status == Some("failed"));
        self.entries.push((
            line,
            user_entry(
                vec![ContentBlock::ToolResult {
                    tool_use_id: id,
                    content: text.map(Value::String),
                    standalone_tool_name: None,
                }],
                timestamp,
            ),
        ));
    }

    fn call_block(&mut self, id: &str) -> Option<&mut ContentBlock> {
        let index = *self.calls.get(id)?;
        match &mut self.entries.get_mut(index)?.1 {
            LogEntry::Assistant { message, .. } => message.content.first_mut(),
            _ => None,
        }
    }

    /// The turn's model, as a Model entry when it differs from the last one
    /// seen, then its tokens.
    fn turn_completed(
        &mut self,
        line: usize,
        update: &Map<String, Value>,
        timestamp: Option<String>,
    ) {
        self.open_turn_calls.clear();
        let Some(usage) = update.get("usage").and_then(Value::as_object) else {
            return;
        };
        if let Some(models) = usage.get("modelUsage").and_then(Value::as_object) {
            for model in models.keys() {
                if self.state.last_model.as_deref() != Some(model) {
                    self.state.last_model = Some(model.clone());
                    self.state.named_a_model = true;
                    self.entries
                        .push((line, model_entry(model.clone(), timestamp.clone())));
                }
            }
        }
        if let Some(entry) = usage_entry(usage) {
            self.entries.push((line, entry));
        }
    }

    fn recap(&mut self, line: usize, update: &Map<String, Value>, timestamp: Option<String>) {
        let Some(summary) = update
            .get("summary")
            .and_then(Value::as_str)
            .filter(|summary| !summary.trim().is_empty())
        else {
            return;
        };
        self.entries.push((
            line,
            LogEntry::PiMetadata {
                label: RECAP_LABEL.to_owned(),
                text: summary.to_owned(),
                timestamp,
                searchable: true,
                usage: None,
            },
        ));
    }

    fn subagent_spawned(&mut self, update: &Map<String, Value>) {
        if let (Some(child), Some(subagent_type)) = (
            string(update, "child_session_id"),
            string(update, "subagent_type"),
        ) {
            self.state.subagent_types.insert(child, subagent_type);
        }
    }

    /// Show a row when a sub-agent is cancelled or fails, because the thread
    /// alone does not show why it stopped. Skip a completed sub-agent's
    /// record: its output repeats the thread's last reply.
    fn subagent_finished(
        &mut self,
        line: usize,
        update: &Map<String, Value>,
        timestamp: Option<String>,
    ) {
        let Some(status) = string(update, "status").filter(|status| status != "completed") else {
            return;
        };
        let subagent_type = string(update, "child_session_id")
            .and_then(|child| self.state.subagent_types.get(&child).cloned());
        let mut text = match subagent_type {
            Some(subagent_type) => format!("{subagent_type} {status}"),
            None => status.clone(),
        };
        // Leave off a cancellation's error: it repeats the status.
        if status != "cancelled"
            && let Some(error) = string(update, "error")
        {
            text.push_str(": ");
            text.push_str(&error);
        }
        self.entries.push((
            line,
            LogEntry::PiMetadata {
                label: "Sub-agent".to_owned(),
                text,
                timestamp,
                searchable: true,
                usage: None,
            },
        ));
    }

    fn finish(mut self) -> ParsedUpdates {
        self.end_run();
        ParsedUpdates {
            entries: self.entries,
            malformed_lines: self.malformed_lines,
            named_a_model: self.state.named_a_model,
            dropped: self.dropped,
        }
    }
}

/// The agent's own timestamp in milliseconds, else the envelope's seconds.
/// Treat zero as absent: a session pulled from Grok's backend writes it on
/// every line.
fn record_time(record: &Map<String, Value>, meta: Option<&Map<String, Value>>) -> Option<String> {
    meta.and_then(|meta| meta.get("agentTimestampMs"))
        .and_then(Value::as_i64)
        .filter(|millis| *millis > 0)
        .and_then(rfc3339_from_millis)
        .or_else(|| {
            record
                .get("timestamp")
                .and_then(Value::as_i64)
                .filter(|seconds| *seconds > 0)
                .and_then(|seconds| rfc3339_from_millis(seconds.saturating_mul(1000)))
        })
}

fn call_id(update: &Map<String, Value>) -> String {
    update
        .get("toolCallId")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_owned()
}

/// The call's name and canonical tool: from Grok's `x.ai/tool` metadata when
/// the record carries it, else from a backend call's `rawInput.variant`, else
/// from the ACP kind.
fn tool_identity(update: &Map<String, Value>, meta: Option<&Map<String, Value>>) -> (String, Tool) {
    if let Some(tool) = update
        .get("_meta")
        .and_then(|meta| meta.get("x.ai/tool"))
        .and_then(Value::as_object)
    {
        let name = tool
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_owned();
        let kind = tool.get("kind").and_then(Value::as_str);
        let canonical = canonical_tool(&name, kind);
        return (name, canonical);
    }
    match update
        .get("rawInput")
        .and_then(|input| input.get("variant"))
        .and_then(Value::as_str)
    {
        Some("WebSearch") => ("web_search".to_owned(), Tool::WebSearch),
        Some("XSearch") => ("x_search".to_owned(), Tool::WebSearch),
        _ => {
            let acp_kind = update.get("kind").and_then(Value::as_str).or_else(|| {
                meta.and_then(|meta| meta.get("updateParams"))
                    .and_then(|params| params.get("kind"))
                    .and_then(Value::as_str)
            });
            let name = update
                .get("title")
                .and_then(Value::as_str)
                .map(|title| title.trim().trim_end_matches(':').trim())
                .filter(|title| !title.is_empty())
                .unwrap_or("unknown")
                .to_owned();
            (name, acp_tool(acp_kind))
        }
    }
}

/// A Grok tool by its name, or by its `x.ai/tool` kind for a name not
/// listed: an MCP tool, or a tool Grok renamed for the model.
fn canonical_tool(name: &str, kind: Option<&str>) -> Tool {
    match name {
        "run_terminal_command" => Tool::Shell,
        "read_file" => Tool::Read,
        "search_replace" => Tool::Edit,
        "write" => Tool::Write,
        "grep" => Tool::Grep,
        "glob" => Tool::Glob,
        "list_dir" => Tool::DirectoryListing,
        "spawn_subagent" => Tool::Agent,
        "get_command_or_subagent_output" | "wait_commands_or_subagents" | "monitor" => Tool::Wait,
        "kill_command_or_subagent" => Tool::Other,
        "todo_write" => Tool::TaskList,
        "web_fetch" => Tool::WebFetch,
        "web_search" => Tool::WebSearch,
        _ => match kind {
            Some("read") => Tool::Read,
            Some("edit") => Tool::Edit,
            Some("write") => Tool::Write,
            Some("execute") => Tool::Shell,
            Some("search") => Tool::Grep,
            Some("list") | Some("list_dir") => Tool::DirectoryListing,
            Some("plan") => Tool::TaskList,
            Some("task") => Tool::Agent,
            Some("background_task_action") => Tool::Wait,
            Some("web_fetch") => Tool::WebFetch,
            Some("web_search") => Tool::WebSearch,
            _ => Tool::Other,
        },
    }
}

/// A call with no Grok metadata by its ACP kind.
fn acp_tool(kind: Option<&str>) -> Tool {
    match kind {
        Some("read") => Tool::Read,
        Some("edit") => Tool::Edit,
        Some("execute") => Tool::Shell,
        Some("search") => Tool::Grep,
        Some("fetch") => Tool::WebFetch,
        _ => Tool::Other,
    }
}

/// The call's `rawInput` in the canonical shape for `tool`, without the
/// `variant` and `backend` markers Grok adds. The `x.ai/tool` input map is
/// not used: it keeps fewer arguments (an edit's path, not its strings).
fn call_input(update: &Map<String, Value>, tool: Tool) -> Value {
    let mut arguments = update
        .get("rawInput")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    arguments.remove("variant");
    arguments.remove("backend");
    match tool {
        Tool::Read => rename_key(&mut arguments, "target_file", "file_path"),
        // Grok's own `list_dir` names the directory `target_directory`; the
        // `list_dir` of its Codex tool set names it `dir_path`.
        Tool::DirectoryListing => {
            rename_key(&mut arguments, "target_directory", "path");
            rename_key(&mut arguments, "dir_path", "path");
        }
        _ => {}
    }
    Value::Object(arguments)
}

/// A backend web search's query. Only the result records it.
fn backend_search_query(update: &Map<String, Value>) -> Option<String> {
    update
        .get("rawOutput")
        .and_then(|output| output.pointer("/action/query"))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

/// The result's text: the model-facing `rawOutput`, else the display
/// `content` beside it. A failed call reads its error from either.
fn result_text(update: &Map<String, Value>, failed: bool) -> Option<String> {
    update
        .get("rawOutput")
        .and_then(Value::as_object)
        .and_then(|output| raw_output_text(output, failed))
        .or_else(|| content_text(update))
        .filter(|text| !text.trim().is_empty())
}

/// `rawOutput`'s text by its `type`. The type does not follow the tool's
/// name: `write` and `search_replace` both produce `SearchReplace`. A type
/// not listed is a result with no text.
fn raw_output_text(output: &Map<String, Value>, failed: bool) -> Option<String> {
    let Some(kind) = output.get("type").and_then(Value::as_str) else {
        return backend_search_text(output);
    };
    let text = match kind {
        "ReadFile" => nested_text(output, "FileContent", "content"),
        "ListDir" | "WebFetch" => nested_text(output, "Content", "content"),
        "SearchReplace" => output
            .get("EditsApplied")
            .and_then(Value::as_object)
            .map(|edits| {
                replacement_diff(
                    edits
                        .get("old_string")
                        .and_then(Value::as_str)
                        .unwrap_or_default(),
                    edits
                        .get("new_string")
                        .and_then(Value::as_str)
                        .unwrap_or_default(),
                )
            }),
        "GrepSearch" => byte_text(output.get("stdout"))
            .filter(|text| !text.is_empty())
            .or_else(|| byte_text(output.get("stderr"))),
        "Bash" => bash_text(output),
        "Todo" => nested_text(output, "TodosUpdated", "summary_for_prompt"),
        "TaskOutput" => nested_text(output, "Result", "output"),
        "KillTask" => {
            nested_text(output, "Result", "message").or_else(|| string(output, "TaskNotFound"))
        }
        "BackgroundTaskStarted" => {
            let parts: Vec<String> = ["task_id", "command"]
                .into_iter()
                .filter_map(|field| string(output, field))
                .collect();
            (!parts.is_empty()).then(|| parts.join("\n"))
        }
        "Text" => string(output, "text"),
        "SearchTool" => string(output, "content"),
        // `Monitor` is a wait receipt, `{taskId, timeoutMs, persistent}`.
        _ => None,
    };
    text.or_else(|| failed.then(|| error_text(output)).flatten())
}

/// A failed call's error variant beside `type`: a message string
/// (`FileNotFound`, `NotFound`), or an object holding one
/// (`NoMatchesFound.message`).
fn error_text(output: &Map<String, Value>) -> Option<String> {
    output
        .iter()
        .filter(|(key, _)| key.as_str() != "type")
        .find_map(|(_, value)| match value {
            Value::String(text) if !text.is_empty() => Some(text.clone()),
            Value::Object(object) => string(object, "message"),
            _ => None,
        })
}

/// `output_for_prompt`, else the byte-array `output`, with the exit code
/// below it when the command failed and the output does not end with it.
fn bash_text(output: &Map<String, Value>) -> Option<String> {
    let mut text =
        string(output, "output_for_prompt").or_else(|| byte_text(output.get("output")))?;
    if let Some(code) = output.get("exit_code").and_then(Value::as_i64)
        && code != 0
        && !text
            .trim_end()
            .lines()
            .last()
            .is_some_and(|line| line.trim_start().starts_with("exit:"))
    {
        append_exit_code(&mut text, code);
    }
    Some(text)
}

/// A backend web search's query and the sources it read, one per line.
fn backend_search_text(output: &Map<String, Value>) -> Option<String> {
    let action = output.get("action")?;
    let mut lines: Vec<String> = action
        .get("query")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .into_iter()
        .collect();
    lines.extend(
        action
            .get("sources")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|source| source.get("url").and_then(Value::as_str))
            .map(str::to_owned),
    );
    (!lines.is_empty()).then(|| lines.join("\n"))
}

/// The display `content`: its text parts, and an edit's diff parts as a
/// replacement diff.
fn content_text(update: &Map<String, Value>) -> Option<String> {
    let parts: Vec<String> = update
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|part| match part.get("type").and_then(Value::as_str)? {
            "content" => part
                .pointer("/content/text")
                .and_then(Value::as_str)
                .map(str::to_owned),
            "diff" => Some(replacement_diff(
                part.get("oldText")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
                part.get("newText")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
            )),
            _ => None,
        })
        .collect();
    (!parts.is_empty()).then(|| parts.join("\n"))
}

/// Bytes Grok stores as a JSON array of numbers, decoded as UTF-8 with
/// replacement characters; a string is kept as it is.
fn byte_text(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(text) => Some(text.clone()),
        Value::Array(numbers) => {
            let bytes: Vec<u8> = numbers
                .iter()
                .filter_map(Value::as_u64)
                .filter_map(|byte| u8::try_from(byte).ok())
                .collect();
            Some(String::from_utf8_lossy(&bytes).into_owned())
        }
        _ => None,
    }
}

fn nested_text(output: &Map<String, Value>, variant: &str, field: &str) -> Option<String> {
    output
        .get(variant)
        .and_then(Value::as_object)
        .and_then(|object| string(object, field))
}

fn string(object: &Map<String, Value>, field: &str) -> Option<String> {
    object
        .get(field)
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
}

/// The turn's tokens, as an invisible metadata entry. Subtract cached and
/// cache-written tokens from `inputTokens`; Grok counts them in it.
fn usage_entry(usage: &Map<String, Value>) -> Option<LogEntry> {
    let count = |field: &str| usage.get(field).and_then(Value::as_u64).unwrap_or(0);
    let cache_read = count("cachedReadTokens");
    let cache_creation = count("cacheCreationTokens");
    let tokens = TokenUsage {
        input_tokens: count("inputTokens")
            .saturating_sub(cache_read)
            .saturating_sub(cache_creation),
        output_tokens: count("outputTokens"),
        cache_creation_input_tokens: cache_creation,
        cache_read_input_tokens: cache_read,
    };
    let total = tokens.input_tokens
        + tokens.output_tokens
        + tokens.cache_creation_input_tokens
        + tokens.cache_read_input_tokens;
    (total > 0).then(|| LogEntry::PiMetadata {
        label: "Usage".to_owned(),
        text: String::new(),
        timestamp: None,
        searchable: false,
        usage: Some(tokens),
    })
}

fn model_entry(model: String, timestamp: Option<String>) -> LogEntry {
    LogEntry::PiMetadata {
        label: "Model".to_owned(),
        text: model,
        timestamp,
        searchable: false,
        usage: None,
    }
}

fn user_entry(content: Vec<ContentBlock>, timestamp: Option<String>) -> LogEntry {
    LogEntry::User {
        message: UserMessage {
            role: "user".to_owned(),
            content: UserContent::Blocks(content),
        },
        timestamp,
        uuid: None,
        cwd: None,
        parent_tool_use_id: None,
        source_tool_use_id: None,
        usage: None,
    }
}

fn assistant_entry(content: Vec<ContentBlock>, timestamp: Option<String>) -> LogEntry {
    LogEntry::Assistant {
        agent: Some("Grok".to_owned()),
        message: AssistantMessage {
            role: "assistant".to_owned(),
            content,
            model: None,
            usage: None,
            id: None,
        },
        timestamp,
        uuid: None,
        parent_tool_use_id: None,
    }
}

/// Grok sessions written in code, shaped like Grok's own records with
/// invented text, for the format's and the provider's tests.
#[cfg(test)]
pub(crate) mod fixture {
    use super::{SUMMARY_FILE, UPDATES_FILE};
    use serde_json::{Value, json};
    use std::path::{Path, PathBuf};

    /// One record's envelope at `millis`, carrying `update` and `meta`
    /// beside `agentTimestampMs`.
    pub(crate) fn envelope(millis: i64, update: Value, mut meta: Value) -> Value {
        meta["agentTimestampMs"] = json!(millis);
        json!({
            "timestamp": millis / 1000,
            "method": "session/update",
            "params": {"sessionId": "s", "update": update, "_meta": meta},
        })
    }

    pub(crate) fn user(millis: i64, text: &str, prompt_index: i64) -> Value {
        envelope(
            millis,
            json!({
                "sessionUpdate": "user_message_chunk",
                "content": {"type": "text", "text": text},
                "_meta": {"promptIndex": prompt_index},
            }),
            json!({}),
        )
    }

    /// An agent chunk of `kind` in the stream `prompt` started at `stream`.
    pub(crate) fn agent_chunk(
        millis: i64,
        kind: &str,
        text: &str,
        prompt: &str,
        stream: i64,
    ) -> Value {
        envelope(
            millis,
            json!({"sessionUpdate": kind, "content": {"type": "text", "text": text}}),
            json!({"promptId": prompt, "streamStartMs": stream}),
        )
    }

    pub(crate) fn reply(millis: i64, text: &str, prompt: &str) -> Value {
        agent_chunk(millis, "agent_message_chunk", text, prompt, millis)
    }

    /// A call's three records: opened, its input, and its terminal update.
    pub(crate) fn call(
        millis: i64,
        id: &str,
        name: &str,
        kind: &str,
        input: Value,
        status: &str,
        raw_output: Value,
    ) -> Vec<Value> {
        let tool = json!({"version": 1, "name": name, "kind": kind, "namespace": "builtin"});
        let mut full_input = input.clone();
        full_input["variant"] = json!("Variant");
        vec![
            envelope(
                millis,
                json!({"sessionUpdate": "tool_call", "toolCallId": id, "title": name,
                       "rawInput": input, "_meta": {"x.ai/tool": tool}}),
                json!({"promptId": "p"}),
            ),
            envelope(
                millis + 1,
                json!({"sessionUpdate": "tool_call_update", "toolCallId": id, "title": name,
                       "rawInput": full_input, "_meta": {"x.ai/tool": tool}}),
                json!({"promptId": "p"}),
            ),
            envelope(
                millis + 2,
                json!({"sessionUpdate": "tool_call_update", "toolCallId": id, "status": status,
                       "rawOutput": raw_output}),
                json!({"promptId": "p"}),
            ),
        ]
    }

    pub(crate) fn turn_completed(millis: i64, model: &str, input: u64, output: u64) -> Value {
        envelope(
            millis,
            json!({
                "sessionUpdate": "turn_completed",
                "prompt_id": "p",
                "usage": {
                    "inputTokens": input,
                    "outputTokens": output,
                    "cachedReadTokens": 0,
                    "cacheCreationTokens": 0,
                    "modelUsage": {model: {}},
                },
            }),
            json!({}),
        )
    }

    /// The `summary.json` of `id`, recorded in `cwd`, with `extra` fields.
    pub(crate) fn summary(id: &str, cwd: &str, extra: Value) -> Value {
        let mut summary = json!({
            "info": {"id": id, "cwd": cwd},
            "created_at": "2026-08-26T15:04:16.048797100Z",
            "current_model_id": "grok-4.6",
            "chat_format_version": 1,
            "session_summary": "summary text",
            "unknown_field": {"kept": true},
        });
        for (key, value) in extra.as_object().cloned().unwrap_or_default() {
            summary[key] = value;
        }
        summary
    }

    /// A session directory `<sessions>/<cwd_dir>/<id>/` with `summary`, and
    /// `records` as its transcript when there are any; returns the locator.
    pub(crate) fn write_session(
        sessions: &Path,
        cwd_dir: &str,
        id: &str,
        summary: &Value,
        records: &[Value],
    ) -> PathBuf {
        let directory = sessions.join(cwd_dir).join(id);
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join(SUMMARY_FILE), summary.to_string()).unwrap();
        let locator = directory.join(UPDATES_FILE);
        if !records.is_empty() {
            let lines: String = records.iter().map(|record| format!("{record}\n")).collect();
            std::fs::write(&locator, lines).unwrap();
        }
        locator
    }

    /// The parent's `subagents/<child>/meta.json` naming `child`.
    pub(crate) fn write_subagent_meta(parent_dir: &Path, child: &str, subagent_type: &str) {
        let directory = parent_dir.join("subagents").join(child);
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(
            directory.join("meta.json"),
            json!({
                "parent_session_id": parent_dir.file_name().unwrap().to_string_lossy(),
                "child_session_id": child,
                "subagent_type": subagent_type,
                "description": "a sub-agent task",
                "prompt": "the spawn prompt",
                "status": "completed",
            })
            .to_string(),
        )
        .unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::fixture::*;
    use super::*;
    use crate::history::format::dropped::DropOutcome;
    use crate::history::parser::RowDating;
    use serde_json::json;

    #[test]
    fn counting_names_skipped_and_unrecognised_update_kinds_without_version() {
        let lines = ["hologram", "hologram", "plan", "memory_dream_started"]
            .map(|kind| json!({"timestamp": 1, "params": {"update": {"sessionUpdate": kind}}}))
            .map(|record| record.to_string())
            .join("\n");

        let updates = parse_updates(
            lines.as_bytes(),
            ParallelThreshold::NEVER,
            DroppedRecords::counting(),
        )
        .unwrap();

        assert_eq!(
            updates.dropped.counts().collect::<Vec<_>>(),
            [
                (None, "sessionUpdate=hologram", DropOutcome::Unrecognised, 2),
                (
                    None,
                    "sessionUpdate=memory_dream_started",
                    DropOutcome::Skipped,
                    1
                ),
                (None, "sessionUpdate=plan", DropOutcome::Skipped, 1),
            ]
        );
    }

    const SESSION: &str = "019a0000-0000-7000-8000-000000000001";

    fn parse(records: &[Value]) -> SessionProjection {
        parse_with(records, json!({}))
    }

    fn parse_with(records: &[Value], extra_summary: Value) -> SessionProjection {
        let home = tempfile::tempdir().unwrap();
        let locator = write_session(
            home.path(),
            "%2Ftmp%2Fgrok-project",
            SESSION,
            &summary(SESSION, "/tmp/grok-project", extra_summary),
            records,
        );
        GROK_UPDATES
            .parse_transcript(crate::history::FOR_FIXTURES, &locator)
            .unwrap()
            .unwrap()
    }

    fn dialogue(projection: &SessionProjection) -> Vec<&LogEntry> {
        projection
            .entries
            .iter()
            .map(|(_, entry)| entry)
            .filter(|entry| matches!(entry, LogEntry::User { .. } | LogEntry::Assistant { .. }))
            .collect()
    }

    fn texts(projection: &SessionProjection) -> Vec<String> {
        dialogue(projection)
            .into_iter()
            .flat_map(|entry| match entry {
                LogEntry::User { message, .. } => match &message.content {
                    UserContent::Blocks(blocks) => blocks.clone(),
                    UserContent::String(text) => vec![ContentBlock::Text { text: text.clone() }],
                },
                LogEntry::Assistant { message, .. } => message.content.clone(),
                _ => Vec::new(),
            })
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(text),
                ContentBlock::Thinking { thinking, .. } => Some(format!("thought: {thinking}")),
                _ => None,
            })
            .collect()
    }

    /// The text of the result answering `id`, `None` when it has none.
    fn result_of(projection: &SessionProjection, id: &str) -> Option<Option<String>> {
        projection
            .entries
            .iter()
            .find_map(|(_, entry)| match entry {
                LogEntry::User { message, .. } => match &message.content {
                    UserContent::Blocks(blocks) => blocks.iter().find_map(|block| match block {
                        ContentBlock::ToolResult {
                            tool_use_id,
                            content,
                            ..
                        } if tool_use_id == id => {
                            Some(content.as_ref().and_then(Value::as_str).map(str::to_owned))
                        }
                        _ => None,
                    }),
                    _ => None,
                },
                _ => None,
            })
    }

    fn call_of(projection: &SessionProjection, id: &str) -> (String, Tool, Value) {
        projection
            .entries
            .iter()
            .find_map(|(_, entry)| match entry {
                LogEntry::Assistant { message, .. } => {
                    message.content.iter().find_map(|block| match block {
                        ContentBlock::ToolUse {
                            id: call,
                            name,
                            tool,
                            input,
                        } if call == id => Some((name.clone(), *tool, input.clone())),
                        _ => None,
                    })
                }
                _ => None,
            })
            .unwrap()
    }

    #[test]
    fn summary_supplies_header_and_manual_title() {
        let projection = parse_with(
            &[user(1_786_000_000_000, "hello", 0)],
            json!({"generated_title": "Chosen name", "title_is_manual": true}),
        );

        assert_eq!(projection.source, Source::Grok);
        assert_eq!(projection.header.id, SESSION);
        assert_eq!(projection.header.cwd, PathBuf::from("/tmp/grok-project"));
        assert_eq!(projection.header.timestamp, "2026-08-26T15:04:16.048Z");
        assert_eq!(projection.title.as_deref(), Some("Chosen name"));
        assert!(matches!(
            &projection.entries[0].1,
            LogEntry::CustomTitle { custom_title } if custom_title == "Chosen name"
        ));
    }

    #[test]
    fn generated_title_then_one_line_summary_name_session() {
        let generated = parse_with(
            &[user(1, "hello", 0)],
            json!({"generated_title": "Generated"}),
        );
        assert!(matches!(
            &generated.entries[0].1,
            LogEntry::AiTitle { ai_title } if ai_title == "Generated"
        ));
        let summarized = parse(&[user(1, "hello", 0)]);
        assert_eq!(summarized.title.as_deref(), Some("summary text"));
    }

    #[test]
    fn file_without_summary_beside_it_isnt_grok() {
        let directory = tempfile::tempdir().unwrap();
        let locator = directory.path().join(UPDATES_FILE);
        std::fs::write(&locator, format!("{}\n", user(1, "hello", 0))).unwrap();
        assert!(
            GROK_UPDATES
                .parse_transcript(crate::history::FOR_FIXTURES, &locator)
                .unwrap()
                .is_none()
        );

        std::fs::write(directory.path().join(SUMMARY_FILE), "{}").unwrap();
        assert!(
            GROK_UPDATES
                .parse_transcript(crate::history::FOR_FIXTURES, &locator)
                .unwrap()
                .is_none(),
            "a summary naming no session id does not claim the file"
        );
    }

    /// The Claude reader comes before Grok's in registration order and must
    /// not claim Grok's envelopes.
    #[test]
    fn grok_transcript_is_recognized_as_grok_by_scan() {
        let home = tempfile::tempdir().unwrap();
        let locator = write_session(
            home.path(),
            "cwd",
            SESSION,
            &summary(SESSION, "/tmp", json!({})),
            &[user(1, "hello", 0), reply(2, "hi", "p")],
        );
        assert_eq!(
            super::super::parse_transcript(crate::history::FOR_FIXTURES, &locator)
                .unwrap()
                .map(|projection| projection.source),
            Some(Source::Grok)
        );
    }

    #[test]
    fn session_with_no_transcript_reads_as_empty() {
        let home = tempfile::tempdir().unwrap();
        let locator = write_session(
            home.path(),
            "cwd",
            SESSION,
            &summary(SESSION, "/tmp", json!({})),
            &[],
        );
        assert!(!locator.exists());

        let projection = GROK_UPDATES
            .parse_transcript(crate::history::FOR_FIXTURES, &locator)
            .unwrap()
            .unwrap();
        assert!(dialogue(&projection).is_empty());
        assert!(
            GROK_UPDATES
                .parse_conversation(
                    &locator,
                    RowDating::new(None, &crate::clock::TestClock::default()),
                    None,
                )
                .unwrap()
                .is_none(),
            "a session with no conversation lists no row"
        );
    }

    #[test]
    fn adjacent_chunks_of_one_stream_merge_and_new_stream_starts_message() {
        let projection = parse(&[
            user(1_000, "question", 0),
            agent_chunk(2_000, "agent_thought_chunk", "think ", "p1", 2_000),
            agent_chunk(2_001, "agent_thought_chunk", "more", "p1", 2_000),
            agent_chunk(2_002, "agent_message_chunk", "first ", "p1", 2_002),
            agent_chunk(2_003, "agent_message_chunk", "part", "p1", 2_002),
            agent_chunk(2_004, "agent_message_chunk", "second", "p1", 2_004),
        ]);

        assert_eq!(
            texts(&projection),
            vec!["question", "thought: think more", "first part", "second"]
        );
    }

    #[test]
    fn user_messages_with_their_own_prompt_index_stay_apart() {
        let projection = parse(&[user(1, "one", 0), user(2, "two", 1)]);
        assert_eq!(texts(&projection), vec!["one", "two"]);
    }

    #[test]
    fn back_to_back_user_messages_without_prompt_index_merge() {
        let without_index = |millis, text: &str| {
            envelope(
                millis,
                json!({"sessionUpdate": "user_message_chunk",
                       "content": {"type": "text", "text": text}}),
                json!({}),
            )
        };
        let projection = parse(&[without_index(1, "one "), without_index(2, "two")]);
        assert_eq!(texts(&projection), vec!["one two"]);
    }

    #[test]
    fn record_with_zero_timestamp_and_no_meta_projects_without_timestamp() {
        let bare = json!({
            "timestamp": 0,
            "method": "session/update",
            "params": {"update": {"sessionUpdate": "user_message_chunk",
                                  "content": {"type": "text", "text": "pulled"}}},
        });
        let projection = parse(&[bare]);

        assert_eq!(texts(&projection), vec!["pulled"]);
        assert_eq!(dialogue(&projection)[0].timestamp(), None);
    }

    #[test]
    fn agents_millisecond_timestamp_dates_entry() {
        let projection = parse(&[user(1_786_000_000_123, "hello", 0)]);
        assert_eq!(
            dialogue(&projection)[0].timestamp(),
            Some("2026-08-06T07:06:40.123Z")
        );
    }

    #[test]
    fn line_that_is_not_json_is_reported_as_malformed() {
        let home = tempfile::tempdir().unwrap();
        let locator = write_session(
            home.path(),
            "cwd",
            SESSION,
            &summary(SESSION, "/tmp", json!({})),
            &[user(1, "hello", 0)],
        );
        let mut contents = std::fs::read_to_string(&locator).unwrap();
        contents.push_str("{not json\n");
        std::fs::write(&locator, contents).unwrap();

        let projection = GROK_UPDATES
            .parse_transcript(crate::history::FOR_FIXTURES, &locator)
            .unwrap()
            .unwrap();
        assert_eq!(projection.malformed_lines, vec![2]);
    }

    #[test]
    fn calls_map_onto_canonical_tools_with_their_input_reshaped() {
        let mut records = Vec::new();
        records.extend(call(
            10,
            "read",
            "read_file",
            "read",
            json!({"target_file": "/tmp/a.rs"}),
            "completed",
            json!({"type": "ReadFile", "FileContent": {"content": "file body"}}),
        ));
        records.extend(call(
            20,
            "list",
            "list_dir",
            "list",
            json!({"target_directory": "/tmp"}),
            "completed",
            json!({"type": "ListDir", "Content": {"content": "a.rs\nb.rs"}}),
        ));
        records.extend(call(
            30,
            "shell",
            "run_terminal_command",
            "execute",
            json!({"command": "ls"}),
            "completed",
            json!({"type": "Bash", "output_for_prompt": "a.rs", "exit_code": 0}),
        ));
        records.extend(call(
            40,
            "codex-list",
            "list_dir",
            "list",
            json!({"dir_path": "/tmp", "depth": 2}),
            "completed",
            json!({"type": "ListDir", "Content": {"content": "a.rs"}}),
        ));
        let projection = parse(&records);

        let (name, tool, input) = call_of(&projection, "read");
        assert_eq!((name.as_str(), tool), ("read_file", Tool::Read));
        assert_eq!(
            input,
            json!({"file_path": "/tmp/a.rs"}),
            "variant is dropped"
        );
        assert_eq!(
            call_of(&projection, "list"),
            (
                "list_dir".to_owned(),
                Tool::DirectoryListing,
                json!({"path": "/tmp"})
            )
        );
        assert_eq!(
            call_of(&projection, "codex-list").2,
            json!({"path": "/tmp", "depth": 2})
        );
        assert_eq!(call_of(&projection, "shell").1, Tool::Shell);
        assert_eq!(
            result_of(&projection, "read"),
            Some(Some("file body".to_owned()))
        );
    }

    #[test]
    fn unknown_tool_name_falls_through_to_its_kind() {
        let records = call(
            10,
            "mcp",
            "some_mcp_reader",
            "read",
            json!({"path": "/tmp/a"}),
            "completed",
            json!({"type": "Text", "text": "read"}),
        );
        assert_eq!(call_of(&parse(&records), "mcp").1, Tool::Read);

        let listing = call(
            10,
            "mcp-list",
            "some_mcp_lister",
            "list",
            json!({"path": "/tmp"}),
            "completed",
            json!({"type": "Text", "text": "a.rs"}),
        );
        assert_eq!(
            call_of(&parse(&listing), "mcp-list").1,
            Tool::DirectoryListing
        );

        let renamed_listing = call(
            10,
            "renamed-list",
            "cursor_ls",
            "list_dir",
            json!({"target_directory": "/tmp"}),
            "completed",
            json!({"type": "ListDir", "Content": {"content": "a.rs"}}),
        );
        assert_eq!(
            call_of(&parse(&renamed_listing), "renamed-list"),
            (
                "cursor_ls".to_owned(),
                Tool::DirectoryListing,
                json!({"path": "/tmp"})
            )
        );

        let unknown_kind = call(
            10,
            "memory",
            "memory_search",
            "memory_search",
            json!({"query": "q"}),
            "completed",
            json!({"type": "Text", "text": "found"}),
        );
        assert_eq!(call_of(&parse(&unknown_kind), "memory").1, Tool::Other);
    }

    #[test]
    fn glob_call_with_list_kind_doesnt_count_as_directory_listing() {
        let records = call(
            10,
            "glob",
            "glob",
            "list",
            json!({"pattern": "**/*.rs", "path": "src"}),
            "completed",
            json!({"type": "Text", "text": "src/main.rs"}),
        );
        assert_eq!(
            call_of(&parse(&records), "glob"),
            (
                "glob".to_owned(),
                Tool::Glob,
                json!({"pattern": "**/*.rs", "path": "src"})
            )
        );
    }

    /// Each `rawOutput` type the reader names, with the text it reads.
    #[test]
    fn each_raw_output_type_reads_its_text() {
        let bytes = |text: &str| json!(text.bytes().collect::<Vec<u8>>());
        let cases = [
            (
                json!({"type": "ReadFile", "FileContent": {"content": "body"}}),
                Some("body"),
            ),
            (
                json!({"type": "ListDir", "Content": {"content": "entries"}}),
                Some("entries"),
            ),
            (
                json!({"type": "WebFetch", "Content": {"content": "page"}}),
                Some("page"),
            ),
            (
                json!({"type": "SearchReplace", "EditsApplied": {"old_string": "a", "new_string": "b"}}),
                Some("-a\n+b"),
            ),
            (
                json!({"type": "GrepSearch", "stdout": bytes("match\n"), "stderr": []}),
                Some("match\n"),
            ),
            (
                json!({"type": "Bash", "output_for_prompt": "out", "exit_code": 0}),
                Some("out"),
            ),
            (
                json!({"type": "Todo", "TodosUpdated": {"summary_for_prompt": "2 todos"}}),
                Some("2 todos"),
            ),
            (
                json!({"type": "TaskOutput", "Result": {"output": "progress"}}),
                Some("progress"),
            ),
            (
                json!({"type": "KillTask", "Result": {"message": "killed"}}),
                Some("killed"),
            ),
            (
                json!({"type": "KillTask", "TaskNotFound": "no such task"}),
                Some("no such task"),
            ),
            (
                json!({"type": "BackgroundTaskStarted", "task_id": "t1", "command": "make"}),
                Some("t1\nmake"),
            ),
            (json!({"type": "Text", "text": "started"}), Some("started")),
            (
                json!({"type": "SearchTool", "content": "tools"}),
                Some("tools"),
            ),
            (
                json!({"type": "Monitor", "taskId": "t1", "timeoutMs": 5}),
                None,
            ),
            (json!({"type": "SomethingNew", "field": "ignored"}), None),
        ];
        for (index, (raw_output, expected)) in cases.into_iter().enumerate() {
            let id = format!("call{index}");
            let records = call(
                10,
                &id,
                "some_tool",
                "other",
                json!({}),
                "completed",
                raw_output.clone(),
            );
            assert_eq!(
                result_of(&parse(&records), &id),
                Some(expected.map(str::to_owned)),
                "{raw_output}"
            );
        }
    }

    #[test]
    fn byte_arrays_decode_and_failed_command_shows_its_exit_code() {
        let output: Vec<u8> = "partial \u{fffd}".bytes().chain([0xff]).collect();
        let records = call(
            10,
            "bash",
            "run_terminal_command",
            "execute",
            json!({"command": "false"}),
            "failed",
            json!({"type": "Bash", "output": output, "exit_code": 2}),
        );
        let text = result_of(&parse(&records), "bash").unwrap().unwrap();
        assert!(text.starts_with("partial \u{fffd}\u{fffd}"), "{text:?}");
        assert!(text.ends_with("Exit code: 2"), "{text:?}");
    }

    #[test]
    fn failed_call_reads_its_error_variant_or_its_content() {
        let not_found = call(
            10,
            "read",
            "read_file",
            "read",
            json!({"target_file": "/missing"}),
            "failed",
            json!({"type": "ReadFile", "FileNotFound": "no such file"}),
        );
        assert_eq!(
            result_of(&parse(&not_found), "read"),
            Some(Some("no such file".to_owned()))
        );

        let no_match = call(
            10,
            "edit",
            "search_replace",
            "edit",
            json!({}),
            "failed",
            json!({"type": "SearchReplace", "NoMatchesFound": {"message": "old_string not found"}}),
        );
        assert_eq!(
            result_of(&parse(&no_match), "edit"),
            Some(Some("old_string not found".to_owned()))
        );

        let mut content_only = call(10, "c", "grep", "search", json!({}), "failed", json!(null));
        content_only[2]["params"]["update"]
            .as_object_mut()
            .unwrap()
            .remove("rawOutput");
        content_only[2]["params"]["update"]["content"] =
            json!([{"type": "content", "content": {"type": "text", "text": "grep failed"}}]);
        assert_eq!(
            result_of(&parse(&content_only), "c"),
            Some(Some("grep failed".to_owned()))
        );
    }

    #[test]
    fn call_with_no_terminal_update_doesnt_have_result() {
        let mut records = call(
            10,
            "open",
            "read_file",
            "read",
            json!({}),
            "completed",
            json!({}),
        );
        records.pop();
        let projection = parse(&records);
        assert_eq!(call_of(&projection, "open").1, Tool::Read);
        assert_eq!(result_of(&projection, "open"), None);
    }

    #[test]
    fn backend_web_search_maps_on_its_variant_and_reads_its_query_from_result() {
        let records = [
            envelope(
                10,
                json!({"sessionUpdate": "tool_call", "toolCallId": "ws_1", "title": "Web search:",
                       "kind": "search", "rawInput": {"variant": "WebSearch", "backend": true}}),
                json!({}),
            ),
            envelope(
                11,
                json!({"sessionUpdate": "tool_call_update", "toolCallId": "ws_1", "status": "completed",
                       "rawOutput": {"action": {"type": "search", "query": "rust cache",
                                                "sources": [{"url": "https://example.com/a"}]},
                                     "id": "ws_1", "status": "completed"}}),
                json!({}),
            ),
        ];
        let projection = parse(&records);
        let (name, tool, input) = call_of(&projection, "ws_1");
        assert_eq!((name.as_str(), tool), ("web_search", Tool::WebSearch));
        assert_eq!(input, json!({"query": "rust cache"}));
        assert_eq!(
            result_of(&projection, "ws_1"),
            Some(Some("rust cache\nhttps://example.com/a".to_owned()))
        );
    }

    #[test]
    fn token_usage_totals_across_turns_and_turn_without_record_doesnt_add_any() {
        let home = tempfile::tempdir().unwrap();
        let locator = write_session(
            home.path(),
            "cwd",
            SESSION,
            &summary(SESSION, "/tmp", json!({})),
            &[
                user(1_000, "one", 0),
                reply(1_100, "first", "p1"),
                turn_completed(1_200, "grok-4.6-build", 100, 10),
                user(2_000, "two", 1),
                reply(2_100, "second", "p2"),
                turn_completed(2_200, "grok-4.6-build", 50, 5),
                user(3_000, "three", 2),
                reply(3_100, "no turn record", "p3"),
            ],
        );
        let conversation = GROK_UPDATES
            .parse_conversation(
                &locator,
                RowDating::new(None, &crate::clock::TestClock::default()),
                None,
            )
            .unwrap()
            .unwrap();
        assert_eq!(conversation.total_tokens, 165);
        assert_eq!(conversation.model.as_deref(), Some("grok-4.6-build"));
    }

    #[test]
    fn cached_tokens_are_subtracted_from_input_count() {
        let mut record = turn_completed(10, "m", 100, 10);
        record["params"]["update"]["usage"]["cachedReadTokens"] = json!(60);
        record["params"]["update"]["usage"]["cacheCreationTokens"] = json!(15);
        let projection = parse(&[record]);
        let usage = projection
            .entries
            .iter()
            .find_map(|(_, entry)| match entry {
                LogEntry::PiMetadata {
                    usage: Some(usage), ..
                } => Some(usage.clone()),
                _ => None,
            });
        let usage = usage.unwrap();
        assert_eq!(
            (
                usage.input_tokens,
                usage.cache_read_input_tokens,
                usage.cache_creation_input_tokens,
                usage.output_tokens
            ),
            (25, 60, 15, 10)
        );
    }

    #[test]
    fn session_with_two_models_records_each_change() {
        let projection = parse(&[
            turn_completed(1, "grok-a", 1, 1),
            turn_completed(2, "grok-a", 1, 1),
            turn_completed(3, "grok-b", 1, 1),
        ]);
        let models: Vec<&str> = projection
            .entries
            .iter()
            .filter_map(|(_, entry)| match entry {
                LogEntry::PiMetadata { label, text, .. } if label == "Model" => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(models, vec!["grok-a", "grok-b"]);
    }

    #[test]
    fn summarys_model_names_session_with_no_completed_turn() {
        let projection = parse(&[user(1, "hello", 0)]);
        assert!(projection.entries.iter().any(|(_, entry)| matches!(
            entry,
            LogEntry::PiMetadata { label, text, .. } if label == "Model" && text == "grok-4.6"
        )));
    }

    #[test]
    fn recap_is_searchable_text() {
        let recap = envelope(
            10,
            json!({"sessionUpdate": "session_recap", "summary": "recap words", "auto": true}),
            json!({}),
        );
        let projection = parse(&[recap]);
        assert!(projection.entries.iter().any(|(_, entry)| matches!(
            entry,
            LogEntry::PiMetadata { label, text, searchable: true, .. }
                if label == RECAP_LABEL && text == "recap words"
        )));
    }

    fn subagent_rows(projection: &SessionProjection) -> Vec<&str> {
        projection
            .entries
            .iter()
            .filter_map(|(_, entry)| match entry {
                LogEntry::PiMetadata {
                    label,
                    text,
                    searchable: true,
                    ..
                } if label == "Sub-agent" => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn cancelled_or_failed_sub_agent_shows_row_and_completed_one_doesnt() {
        let spawned = |millis, child: &str| {
            envelope(
                millis,
                json!({"sessionUpdate": "subagent_spawned", "subagent_id": child,
                       "child_session_id": child, "subagent_type": "explore",
                       "description": "d"}),
                json!({}),
            )
        };
        let finished = |millis, child: &str, status: &str, extra: Value| {
            let mut update = json!({"sessionUpdate": "subagent_finished", "subagent_id": child,
                                    "child_session_id": child, "status": status,
                                    "tool_calls": 1, "turns": 1, "duration_ms": 10});
            for (key, value) in extra.as_object().cloned().unwrap_or_default() {
                update[key] = value;
            }
            envelope(millis, update, json!({}))
        };
        let projection = parse(&[
            spawned(1, "c1"),
            spawned(2, "c2"),
            spawned(3, "c3"),
            finished(
                4,
                "c1",
                "cancelled",
                json!({"error": "Subagent was cancelled"}),
            ),
            finished(5, "c2", "failed", json!({"error": "model unavailable"})),
            finished(6, "c3", "completed", json!({"output": "the last reply"})),
            finished(7, "unspawned", "cancelled", json!({})),
        ]);

        assert_eq!(
            subagent_rows(&projection),
            vec![
                "explore cancelled",
                "explore failed: model unavailable",
                "cancelled"
            ]
        );
    }

    #[test]
    fn sub_agent_session_uses_its_type_as_its_label_and_has_no_title() {
        let projection = parse_with(
            &[user(1, "the spawn prompt", 0)],
            json!({"session_kind": "subagent", "agent_name": "explore", "generated_title": "t"}),
        );
        assert_eq!(projection.header.thread_label(), "explore");
        assert_eq!(projection.title, None);
    }

    #[test]
    fn skipped_update_kinds_are_neither_entries_nor_malformed() {
        let skipped = [
            "plan",
            "retry_state",
            "memory_dream_started",
            "compaction_checkpoint",
            "a_future_kind",
        ]
        .map(|kind| envelope(10, json!({"sessionUpdate": kind}), json!({})));
        let projection = parse(&skipped);
        assert!(dialogue(&projection).is_empty());
        assert!(projection.malformed_lines.is_empty());
    }

    #[test]
    fn parse_is_same_on_pool_and_on_one_thread() {
        let mut records = vec![user(1, "question", 0)];
        for index in 0..300 {
            records.push(agent_chunk(10 + index, "agent_message_chunk", "x", "p", 10));
        }
        let lines: String = records.iter().map(|record| format!("{record}\n")).collect();
        let parsed = |parallel| {
            let updates =
                parse_updates(lines.as_bytes(), parallel, DroppedRecords::counting()).unwrap();
            format!("{:?} {:?}", updates.entries, updates.dropped)
        };
        assert_eq!(
            parsed(ParallelThreshold::ALWAYS),
            parsed(ParallelThreshold::NEVER)
        );
    }

    mod resumed_reads {
        use super::*;
        use crate::history::format::lines::record_splits;
        use crate::history::provider::contract_tests::every_record_sessions;
        use crate::history::provider::transcript_reads::resume;

        fn reader() -> &'static dyn TranscriptReader {
            Source::Grok.provider().storage().transcript_reader()
        }

        fn read_whole(path: &Path) -> SessionProjection {
            GROK_UPDATES
                .parse_transcript(crate::history::FOR_FIXTURES, path)
                .unwrap()
                .expect("a Grok transcript")
        }

        /// `entries` as text a test compares.
        fn listed(entries: &[(RecordLine, LogEntry)]) -> Vec<String> {
            entries
                .iter()
                .map(|(record, entry)| format!("{} {entry:?}", record.line))
                .collect()
        }

        /// Asserts that `prefix`, a read of a transcript's beginning, up to
        /// the window's first line, and then `window`, the read resumed from
        /// it over the whole transcript, hold what `full`, one read of the
        /// whole transcript, holds, and leave what it leaves. The entries at
        /// line 0 come from `summary.json`, which every read takes again.
        fn assert_window_completes(
            name: &str,
            path: &Path,
            prefix: &SessionProjection,
            window: &ResumedWindow,
            full: &SessionProjection,
        ) {
            let kept = prefix
                .entries
                .iter()
                .filter(|(record, _)| record.line > 0 && record.line < window.start_line)
                .chain(&window.entries)
                .cloned()
                .collect::<Vec<_>>();
            let full_entries = full
                .entries
                .iter()
                .filter(|(record, _)| record.line > 0)
                .cloned()
                .collect::<Vec<_>>();
            assert_eq!(
                listed(&kept),
                listed(&full_entries),
                "{name}: the prefix and the window hold the full read's entries"
            );
            let malformed = prefix
                .malformed_lines
                .iter()
                .filter(|line| **line < window.start_line)
                .copied()
                .chain(window.malformed_lines.iter().map(|line| line.line_number))
                .collect::<Vec<_>>();
            assert_eq!(
                malformed, full.malformed_lines,
                "{name}: the prefix and the window hold the full read's malformed lines"
            );
            assert_eq!(
                window.read, full.transcript_read,
                "{name}: the window leaves what the full read leaves"
            );
            let leading = reader().leading_entries(path, &window.read).unwrap();
            assert_eq!(
                listed(&leading),
                listed(
                    &full
                        .entries
                        .iter()
                        .filter(|(record, _)| record.line == 0)
                        .cloned()
                        .collect::<Vec<_>>()
                ),
                "{name}: the leading entries after the window are the full read's"
            );
        }

        /// The three records of a call `id` opened at `millis`: opened, its
        /// input, and its result.
        fn call_records(millis: i64, id: &str) -> [Value; 3] {
            call(
                millis,
                id,
                "read_file",
                "read",
                json!({"target_file": format!("/tmp/{id}.rs")}),
                "completed",
                json!({"type": "ReadFile", "FileContent": {"content": format!("body of {id}")}}),
            )
            .try_into()
            .unwrap()
        }

        /// Three turns: streamed thoughts and replies, two calls completing
        /// in the other order, a call still open when the next prompt
        /// arrives, a sub-agent that fails, and two models.
        fn interleaved_turns() -> Vec<Value> {
            let [open_a, input_a, done_a] = call_records(1_300, "a");
            let [open_b, input_b, done_b] = call_records(1_310, "b");
            let [open_c, input_c, done_c] = call_records(1_500, "c");
            vec![
                user(1_000, "first question", 0),
                agent_chunk(1_100, "agent_thought_chunk", "think ", "p1", 1_100),
                agent_chunk(1_101, "agent_thought_chunk", "more", "p1", 1_100),
                reply(1_200, "first answer", "p1"),
                turn_completed(1_250, "grok-4.6", 10, 5),
                user(1_260, "second question", 1),
                reply(1_270, "second answer", "p2"),
                open_a,
                input_a,
                open_b,
                input_b,
                done_b,
                done_a,
                envelope(
                    1_350,
                    json!({"sessionUpdate": "subagent_spawned", "child_session_id": "child-1",
                           "subagent_type": "explore"}),
                    json!({}),
                ),
                open_c,
                input_c,
                user(2_000, "third question", 2),
                done_c,
                reply(2_100, "third answer", "p3"),
                envelope(
                    2_150,
                    json!({"sessionUpdate": "subagent_finished", "child_session_id": "child-1",
                           "status": "failed", "error": "model unavailable"}),
                    json!({}),
                ),
                turn_completed(2_200, "grok-4.7", 20, 6),
            ]
        }

        /// `records` as lines, with a malformed line after the second.
        fn with_malformed_line(records: &[Value]) -> String {
            let mut lines = records.iter().map(Value::to_string).collect::<Vec<_>>();
            lines.insert(2, "{not json".to_owned());
            lines.join("\n") + "\n"
        }

        /// Copies the session directory holding `locator` into `into`, and
        /// returns the copy's locator.
        fn copied_session(locator: &Path, into: &Path) -> PathBuf {
            let directory = into.join(locator.parent().unwrap().file_name().unwrap());
            std::fs::create_dir_all(&directory).unwrap();
            for file in [UPDATES_FILE, SUMMARY_FILE] {
                std::fs::copy(locator.with_file_name(file), directory.join(file)).unwrap();
            }
            directory.join(UPDATES_FILE)
        }

        #[test]
        fn prefix_and_resumed_read_equal_full_read_at_every_split_of_every_grok_transcript() {
            let home = tempfile::tempdir().unwrap();
            let every_record = every_record_sessions()
                .into_iter()
                .find(|session| session.row.source == Source::Grok)
                .unwrap();
            let mut transcripts = std::iter::once(&every_record.row.path)
                .chain(&every_record.row.subagents)
                .map(|locator| copied_session(locator, home.path()))
                .collect::<Vec<_>>();
            let generated = write_session(
                home.path(),
                "cwd",
                SESSION,
                &summary(
                    SESSION,
                    "/tmp/grok-project",
                    json!({"generated_title": "Generated"}),
                ),
                &[],
            );
            std::fs::write(&generated, with_malformed_line(&interleaved_turns())).unwrap();
            transcripts.push(generated);
            for path in transcripts {
                let bytes = std::fs::read(&path).unwrap();
                let full = read_whole(&path);
                for split in record_splits(&bytes) {
                    let name = format!("{} at {split}", path.display());
                    std::fs::write(&path, &bytes[..split]).unwrap();
                    let prefix = read_whole(&path);
                    std::fs::write(&path, &bytes).unwrap();

                    match resume(
                        reader(),
                        crate::history::FOR_FIXTURES,
                        &path,
                        &prefix.transcript_read,
                    )
                    .unwrap()
                    {
                        Resumption::Window(window) => {
                            assert_window_completes(&name, &path, &prefix, &window, &full)
                        }
                        other => assert!(
                            matches!(other, Resumption::FullRead)
                                && prefix.transcript_read == TranscriptRead::full_read(),
                            "{name}: only a transcript without a first line ending reads whole: {other:?}"
                        ),
                    }
                }
            }
        }

        #[test]
        fn streaming_message_or_waiting_call_of_current_turn_holds_stable_cursor_before_it() {
            let records = interleaved_turns();
            let lines_through = |count: usize| {
                records[..count]
                    .iter()
                    .map(|record| format!("{record}\n"))
                    .collect::<String>()
            };
            let home = tempfile::tempdir().unwrap();
            let path = write_session(
                home.path(),
                "cwd",
                SESSION,
                &summary(SESSION, "/tmp", json!({})),
                &[],
            );
            let stable_line = |count: usize| {
                std::fs::write(&path, lines_through(count)).unwrap();
                read_whole(&path).transcript_read.stable_line
            };

            for (records_read, expected, reason) in [
                (5, 6, "a completed turn is a place to restart"),
                (
                    9,
                    6,
                    "the streamed reply, then the call it opened, await what follows",
                ),
                (
                    12,
                    6,
                    "the first call awaits its result after the second completes",
                ),
                (13, 14, "every call of the turn has its result"),
                (16, 15, "an open call restarts at its opening record"),
                (17, 15, "a streaming prompt follows the open call"),
                (18, 19, "the prompt ended the turn of the call still open"),
                (19, 19, "a streaming reply restarts at its first chunk"),
            ] {
                assert_eq!(
                    stable_line(records_read),
                    Some(expected),
                    "after {records_read} records: {reason}"
                );
            }
        }

        #[test]
        fn input_completed_for_call_before_stable_cursor_changes_earlier_entry() {
            let [open, _, _] = call_records(1_000, "late");
            let records = [
                user(900, "question", 0),
                open,
                turn_completed(2_100, "grok-4.6", 1, 1),
            ];
            let home = tempfile::tempdir().unwrap();
            let path = write_session(
                home.path(),
                "cwd",
                SESSION,
                &summary(SESSION, "/tmp", json!({})),
                &records,
            );
            let read = read_whole(&path).transcript_read;
            assert_eq!(
                read.stable_line,
                Some(4),
                "the completed turn ended the turn of the open call"
            );
            let late_input = envelope(
                2_200,
                json!({"sessionUpdate": "tool_call_update", "toolCallId": "late",
                       "rawInput": {"target_file": "/tmp/late-input.rs"}}),
                json!({}),
            );
            let mut bytes = std::fs::read_to_string(&path).unwrap();
            bytes.push_str(&format!("{late_input}\n"));
            std::fs::write(&path, &bytes).unwrap();

            assert!(matches!(
                resume(reader(), crate::history::FOR_FIXTURES, &path, &read).unwrap(),
                Resumption::ChangesEarlierEntry
            ));
        }

        #[test]
        fn summarys_model_leads_until_a_turn_names_one() {
            let home = tempfile::tempdir().unwrap();
            let path = write_session(
                home.path(),
                "cwd",
                SESSION,
                &summary(SESSION, "/tmp", json!({})),
                &[user(1_000, "question", 0)],
            );
            let unnamed = read_whole(&path).transcript_read;
            let model_rows = |read: &TranscriptRead| {
                listed(&reader().leading_entries(&path, read).unwrap())
                    .concat()
                    .matches("Model")
                    .count()
            };
            assert_eq!(model_rows(&unnamed), 1, "the summary's model leads");

            let mut bytes = std::fs::read_to_string(&path).unwrap();
            bytes.push_str(&format!("{}\n", turn_completed(2_000, "grok-4.7", 1, 1)));
            std::fs::write(&path, &bytes).unwrap();
            let Resumption::Window(window) =
                resume(reader(), crate::history::FOR_FIXTURES, &path, &unnamed).unwrap()
            else {
                panic!("the transcript resumes");
            };

            assert_eq!(
                model_rows(&window.read),
                0,
                "a named model replaces the summary's"
            );
        }

        #[test]
        fn transcript_with_another_first_line_or_working_directory_reads_whole() {
            let home = tempfile::tempdir().unwrap();
            let path = write_session(
                home.path(),
                "cwd",
                SESSION,
                &summary(
                    SESSION,
                    "/tmp/grok-project",
                    json!({"generated_title": "First"}),
                ),
                &[user(1_000, "question", 0), reply(1_100, "answer", "p1")],
            );
            let read = read_whole(&path).transcript_read;
            let summary_path = path.with_file_name(SUMMARY_FILE);
            let original = std::fs::read_to_string(&summary_path).unwrap();
            assert_eq!(reader().identity(&path).unwrap(), read.identity);

            std::fs::write(&summary_path, original.replace("\"First\"", "\"Renamed\"")).unwrap();
            assert_eq!(
                reader().identity(&path).unwrap(),
                read.identity,
                "the title is not part of the identity: every read takes it again"
            );

            std::fs::write(
                &summary_path,
                original.replace("/tmp/grok-project", "/tmp/elsewhere"),
            )
            .unwrap();
            assert_ne!(reader().identity(&path).unwrap(), read.identity);
            assert!(matches!(
                resume(reader(), crate::history::FOR_FIXTURES, &path, &read).unwrap(),
                Resumption::FullRead
            ));

            std::fs::write(&summary_path, &original).unwrap();
            let bytes = std::fs::read_to_string(&path).unwrap();
            std::fs::write(&path, bytes.replacen("question", "QUESTION", 1)).unwrap();
            assert_ne!(
                reader().identity(&path).unwrap(),
                read.identity,
                "the first line is the identity"
            );
        }
    }
}
