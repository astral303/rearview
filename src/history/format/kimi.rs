//! The Kimi Code wire format: JSONL event logs named `wire.jsonl`, one per
//! agent, inside a directory per session.
//!
//! A wire records everything the agent runtime did — full LLM request payloads,
//! permission prompts, plugin lifecycles — so this reader is a projection by
//! design: `context.append_message` carries the user's turns, loop events carry
//! the assistant's text and tool traffic, `usage.record` carries tokens and the
//! model, and everything else is skipped without being an error. Unknown event
//! types are expected; Kimi adds them freely between versions.
//!
//! The file itself names no session: identity comes from the directory holding
//! it, and the title, working directory and agent parentage come from the
//! session directory's `state.json`. Messages removed by a later `context.undo`
//! are still shown — the wire keeps them, and the view is what happened.
//!
//! A read of a changed wire resumes from its stable cursor and reads
//! `state.json` again, because Kimi appends to a wire and rewrites
//! `state.json`.

mod message_origin;

use self::message_origin::{Appended, ShellCall, appended_message};
use super::dropped::DroppedRecords;
use super::lines::{self, LineCursor, visit_line_spans_from};
use super::{
    EarlierTurns, RecordLine, SessionFormat, SessionHeader, SessionProjection, at_own_lines,
    rename_key, rfc3339_from_millis,
};
use crate::agent::transcript::tool_result_text;
use crate::error::Result;
use crate::history::provider::{ResumedWindow, Resumption, TranscriptRead, TranscriptReader};
use crate::history::{MalformedLine, ParsePermit, Source};
use crate::log_entry::{
    AssistantMessage, ContentBlock, LogEntry, TokenUsage, Tool, UserContent, UserMessage,
};
use crate::parallel::ParallelThreshold;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};

pub struct KimiWireFormat;

pub static KIMI_WIRE: KimiWireFormat = KimiWireFormat;

impl SessionFormat for KimiWireFormat {
    fn read_transcript(
        &self,
        _admitted: crate::history::Admitted,
        path: &Path,
        dropped: DroppedRecords,
    ) -> Result<Option<SessionProjection>> {
        let mut reader = BufReader::new(File::open(path)?);
        let Some(wire) = parse_reader(&mut reader, dropped)? else {
            return Ok(None);
        };
        let location = wire_location(path);
        let state = session_state(&location.session_dir);
        let session_id = directory_name(&location.session_dir);

        let is_main = state.agent_is_main(&location.agent_id);
        let transcript_read = match &wire.end {
            Some(end) => end.transcript_read(
                wire_identity(&wire.opening_line, &state, is_main),
                reader.get_mut(),
            )?,
            None => TranscriptRead::full_read(),
        };
        // The session part of the qualified id would repeat on every thread;
        // see `SessionHeader::thread_label`.
        let (id, thread_label) = if is_main {
            (session_id, None)
        } else {
            (
                qualified_agent_id(&session_id, &location.agent_id),
                Some(location.agent_id.clone()),
            )
        };

        let mut entries = wire.entries;
        let mut title = None;
        if let Some((entry, name)) = title_entry(&state, is_main) {
            // Line 0: the title lives in state.json, not the wire.
            entries.insert(0, (0, entry));
            title = Some(name);
        }

        Ok(Some(SessionProjection {
            source: Source::Kimi,
            header: SessionHeader {
                version: 1,
                id,
                timestamp: wire
                    .created_at_ms
                    .or(state.created_at_ms)
                    .and_then(rfc3339_from_millis)
                    .unwrap_or_default(),
                cwd: state.cwd.unwrap_or_default(),
                thread_label,
                subagent_identity: Default::default(),
            },
            title,
            entries: at_own_lines(entries),
            leaf_id: None,
            malformed_lines: wire.malformed_lines,
            dropped: wire.dropped,
            earlier_turns: EarlierTurns::Read,
            earlier_transcripts: Vec::new(),
            transcript_read,
        }))
    }
}

/// Resumes a read of a Kimi wire from its stable cursor: the last record
/// boundary where a `!` command before it isn't waiting for its output,
/// because that output becomes the command's call's result. The identity is
/// a hash of the wire's metadata line and of the `state.json` fields a read
/// uses besides the title; every read rereads the title.
impl TranscriptReader for KimiWireFormat {
    fn identity(&self, source: &Path) -> Result<Vec<u8>> {
        let mut reader = BufReader::new(File::open(source)?);
        let Some(opening) = read_opening(&mut reader)?.filter(|opening| opening.is_terminated)
        else {
            return Ok(Vec::new());
        };
        let location = wire_location(source);
        let state = session_state(&location.session_dir);
        let is_main = state.agent_is_main(&location.agent_id);
        Ok(wire_identity(&opening.line, &state, is_main))
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
        permit.admit(source);
        let (Some(stable_cursor), Ok(state)) = (
            LineCursor::decode(&read.stable_cursor),
            serde_json::from_slice::<WireState>(&read.parser_state),
        ) else {
            return Ok(Resumption::FullRead);
        };
        let mut reader = BufReader::new(File::open(source)?);
        reader.seek(SeekFrom::Start(stable_cursor.offset))?;
        let mut projector = WireProjector::new(stable_cursor, state, DroppedRecords::default());
        let cursor = projector.read(&mut reader, stable_cursor, ParallelThreshold::DEFAULT)?;
        let bytes_read = reader
            .get_ref()
            .metadata()?
            .len()
            .saturating_sub(stable_cursor.offset);
        let end = projector.end(cursor);
        Ok(Resumption::Window(ResumedWindow {
            start_line: stable_cursor.line,
            read: end.transcript_read(read.identity.clone(), reader.get_mut())?,
            entries: at_own_lines(projector.entries),
            malformed_lines: projector
                .malformed_lines
                .into_iter()
                .map(|line_number| MalformedLine {
                    line_number,
                    detail: None,
                })
                .collect(),
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

    /// The session's title, from `state.json`, on its main wire.
    fn leading_entries(
        &self,
        source: &Path,
        _read: &TranscriptRead,
    ) -> Result<Vec<(RecordLine, LogEntry)>> {
        let location = wire_location(source);
        let state = session_state(&location.session_dir);
        let is_main = state.agent_is_main(&location.agent_id);
        Ok(title_entry(&state, is_main)
            .map(|(entry, _)| (RecordLine::from(0), entry))
            .into_iter()
            .collect())
    }
}

/// The wire's identity: a hash of its metadata line, `opening_line`, and of
/// the session `state` fields a read uses besides the title, for the main
/// wire when `is_main`.
fn wire_identity(opening_line: &str, state: &SessionState, is_main: bool) -> Vec<u8> {
    let facts = serde_json::to_vec(&(
        opening_line,
        state.cwd.as_deref().map(Path::to_string_lossy),
        state.created_at_ms,
        is_main,
    ))
    .expect("a wire's identity facts serialize");
    blake3::hash(&facts).as_bytes().to_vec()
}

/// The session's title entry and its text, for the main wire only: the
/// title belongs to the session, and a sub-agent wire is read through the
/// session and doesn't have a row.
fn title_entry(state: &SessionState, is_main: bool) -> Option<(LogEntry, String)> {
    if !is_main {
        return None;
    }
    let name = state.title.clone()?;
    let entry = if state.title_is_custom {
        LogEntry::CustomTitle {
            custom_title: name.clone(),
        }
    } else {
        LogEntry::AiTitle {
            ai_title: name.clone(),
        }
    };
    Some((entry, name))
}

/// Where a wire sits: the session directory that owns it and the agent within
/// the session that wrote it. `agents/<agent>/wire.jsonl` is the current
/// layout; a wire directly in the session directory is the legacy one, which
/// is the main agent's. A file outside both shapes is treated as a main wire
/// owned by whatever directory holds it, so a stray copy still parses.
pub(crate) struct WireLocation {
    pub(crate) session_dir: PathBuf,
    pub(crate) agent_id: String,
}

pub(crate) fn wire_location(path: &Path) -> WireLocation {
    let parent = path.parent().unwrap_or_else(|| Path::new(""));
    if let Some(agents) = parent.parent()
        && agents.file_name() == Some(OsStr::new("agents"))
        && let Some(session_dir) = agents.parent()
        && let Some(agent_id) = parent.file_name().and_then(OsStr::to_str)
    {
        return WireLocation {
            session_dir: session_dir.to_path_buf(),
            agent_id: agent_id.to_owned(),
        };
    }
    WireLocation {
        session_dir: parent.to_path_buf(),
        agent_id: "main".to_owned(),
    }
}

/// The id of a sub-agent's thread: the session's id qualified by the agent's
/// directory name, which repeats across sessions and cannot stand alone.
fn qualified_agent_id(session_id: &str, agent_id: &str) -> String {
    format!("{session_id}#{agent_id}")
}

fn directory_name(directory: &Path) -> String {
    directory
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// The session-level facts `state.json` records. Everything is optional: a
/// session whose state is missing or unreadable still lists, it just carries
/// no title or working directory.
#[derive(Default)]
pub(crate) struct SessionState {
    pub(crate) title: Option<String>,
    pub(crate) title_is_custom: bool,
    cwd: Option<PathBuf>,
    created_at_ms: Option<i64>,
    agents: HashMap<String, AgentEntry>,
}

struct AgentEntry {
    is_main: bool,
}

impl SessionState {
    /// An agent absent from the state is judged by its name, so a session with
    /// no readable state still keeps its main wire on top.
    pub(crate) fn agent_is_main(&self, agent_id: &str) -> bool {
        match self.agents.get(agent_id) {
            Some(agent) => agent.is_main,
            None => agent_id == "main",
        }
    }
}

pub(crate) fn session_state(session_dir: &Path) -> SessionState {
    let Ok(contents) = std::fs::read_to_string(session_dir.join("state.json")) else {
        return SessionState::default();
    };
    let Ok(value) = serde_json::from_str::<Value>(&contents) else {
        return SessionState::default();
    };
    parse_state(&value)
}

fn parse_state(value: &Value) -> SessionState {
    let agents = value
        .get("agents")
        .and_then(Value::as_object)
        .map(|agents| {
            agents
                .iter()
                .map(|(agent_id, agent)| {
                    (
                        agent_id.clone(),
                        AgentEntry {
                            is_main: agent.get("type").and_then(Value::as_str) == Some("main"),
                        },
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    SessionState {
        title: value
            .get("title")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|title| !title.is_empty())
            .map(str::to_owned),
        // `titleKind` is the current field, `isCustomTitle` the legacy one;
        // files in transition carry both, agreeing.
        title_is_custom: value
            .get("titleKind")
            .and_then(Value::as_str)
            .map(|kind| kind == "custom")
            .or_else(|| value.get("isCustomTitle").and_then(Value::as_bool))
            .unwrap_or(false),
        cwd: value
            .get("cwd")
            .or_else(|| value.get("workDir"))
            .and_then(Value::as_str)
            .map(PathBuf::from),
        created_at_ms: value.get("createdAt").and_then(Value::as_i64),
        agents,
    }
}

struct ParsedWire {
    /// The metadata line as written, without its line ending.
    opening_line: String,
    created_at_ms: Option<i64>,
    entries: Vec<(usize, LogEntry)>,
    malformed_lines: Vec<usize>,
    dropped: DroppedRecords,
    /// The read's end and the next read's start; `None` while the metadata
    /// line doesn't have a line ending.
    end: Option<WireReadEnd>,
}

/// The metadata record a wire opens with, which decides whether the file is
/// a wire at all.
struct WireOpening {
    /// The line as written, without its line ending.
    line: String,
    created_at_ms: Option<i64>,
    /// The wire protocol the line names: a wire records the protocol it
    /// speaks, not the Kimi Code release that wrote it.
    protocol: Option<String>,
    /// Past the line.
    records_start: LineCursor,
    /// False while the line doesn't have a line ending: Kimi is writing it.
    is_terminated: bool,
}

/// The metadata record `reader` opens with; `None` when the first line
/// holding more than whitespace is not one.
fn read_opening(reader: &mut impl BufRead) -> io::Result<Option<WireOpening>> {
    let mut line = String::new();
    let mut records_start = LineCursor::START;
    loop {
        line.clear();
        let length = reader.read_line(&mut line)?;
        if length == 0 {
            return Ok(None);
        }
        records_start = LineCursor {
            offset: records_start.offset + length as u64,
            line: records_start.line + 1,
        };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(Value::Object(object)) = serde_json::from_str::<Value>(&line) else {
            return Ok(None);
        };
        if object.get("type").and_then(Value::as_str) != Some("metadata")
            || !object.contains_key("protocol_version")
        {
            return Ok(None);
        }
        return Ok(Some(WireOpening {
            created_at_ms: object.get("created_at").and_then(Value::as_i64),
            protocol: object.get("protocol_version").map(|version| match version {
                Value::String(text) => format!("wire protocol {text}"),
                other => format!("wire protocol {other}"),
            }),
            is_terminated: line.ends_with('\n'),
            line: line.trim_end_matches(['\n', '\r']).to_owned(),
            records_start,
        }));
    }
}

fn parse_reader(
    mut reader: impl BufRead,
    mut dropped: DroppedRecords,
) -> Result<Option<ParsedWire>> {
    let Some(opening) = read_opening(&mut reader)? else {
        return Ok(None);
    };
    dropped.record_version(|| opening.protocol.clone());
    let start = opening.records_start;
    let mut projector = WireProjector::new(start, WireState::default(), dropped);
    let cursor = projector.read(reader, start, ParallelThreshold::DEFAULT)?;
    let end = opening.is_terminated.then(|| projector.end(cursor));
    Ok(Some(ParsedWire {
        opening_line: opening.line,
        created_at_ms: opening.created_at_ms,
        entries: projector.entries,
        malformed_lines: projector.malformed_lines,
        dropped: projector.dropped,
        end,
    }))
}

/// The state a read of a wire carries from one record to the next, kept at
/// each boundary where a `!` command isn't waiting for its output.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct WireState {
    /// The model the last turn-scoped usage record named.
    last_model: Option<String>,
}

/// The end of a read of a wire, and the cursor and state the next read
/// restarts from.
struct WireReadEnd {
    cursor: LineCursor,
    stable_cursor: LineCursor,
    stable_state: WireState,
}

impl WireReadEnd {
    /// The [`TranscriptRead`] the next read resumes from, for the wire with
    /// the identity `identity` that `file` holds.
    fn transcript_read(&self, identity: Vec<u8>, file: &mut File) -> Result<TranscriptRead> {
        Ok(TranscriptRead {
            cursor: self.cursor.encode(),
            stable_cursor: self.stable_cursor.encode(),
            identity,
            rewrite_check: lines::rewrite_check(file, self.cursor)?,
            parser_state: serde_json::to_vec(&self.stable_state).expect("a wire state serializes"),
            stable_line: Some(self.stable_cursor.line),
        })
    }
}

/// A wire's records folded into entries, one record at a time.
struct WireProjector {
    entries: Vec<(usize, LogEntry)>,
    malformed_lines: Vec<usize>,
    state: WireState,
    /// The index in `entries` of the last `!` command's call, until its
    /// output arrives.
    open_shell_call: Option<(usize, ShellCall)>,
    dropped: DroppedRecords,
    /// The last record boundary where a `!` command wasn't waiting for its
    /// output, and the state there.
    stable: (LineCursor, WireState),
}

impl WireProjector {
    /// A projector reading on from `from` with `state`.
    fn new(from: LineCursor, state: WireState, dropped: DroppedRecords) -> Self {
        Self {
            entries: Vec::new(),
            malformed_lines: Vec::new(),
            stable: (from, state.clone()),
            state,
            open_shell_call: None,
            dropped,
        }
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
        let read = visit_line_spans_from(reader, from, parallel, parse, |span, _, record| {
            self.record(span.start.line, record);
            if self.open_shell_call.is_none() {
                self.stable = (span.end, self.state.clone());
            }
        })?;
        if let Some((line_number, line)) = read.unterminated {
            self.record(line_number, parse(&line));
        }
        Ok(read.cursor)
    }

    /// The read's end, stopped at `cursor`.
    fn end(&self, cursor: LineCursor) -> WireReadEnd {
        WireReadEnd {
            cursor,
            stable_cursor: self.stable.0,
            stable_state: self.stable.1.clone(),
        }
    }

    fn record(&mut self, line_number: usize, record: Option<Value>) {
        let Some(Value::Object(object)) = record else {
            self.malformed_lines.push(line_number);
            return;
        };
        match object.get("type").and_then(Value::as_str) {
            Some("usage.record") => {
                for entry in usage_record_entries(&object, &mut self.state.last_model) {
                    self.entries.push((line_number, entry));
                }
            }
            Some("context.append_message") => {
                let timestamp = entry_time(&object);
                let appended =
                    object
                        .get("message")
                        .and_then(Value::as_object)
                        .and_then(|message| {
                            appended_message(message, timestamp.clone(), &mut self.dropped)
                        });
                if let Some(appended) = appended {
                    push_appended(
                        &mut self.entries,
                        &mut self.open_shell_call,
                        line_number,
                        appended,
                        timestamp,
                    );
                }
            }
            _ => {
                if let Some(entry) = normalize_line(&object, &mut self.dropped) {
                    self.entries.push((line_number, entry));
                }
            }
        }
    }
}

/// `open_shell_call` holds the index of the last `!` command's call; the
/// command's output becomes that call's result.
fn push_appended(
    entries: &mut Vec<(usize, LogEntry)>,
    open_shell_call: &mut Option<(usize, ShellCall)>,
    line_number: usize,
    appended: Appended,
    timestamp: Option<String>,
) {
    match appended {
        Appended::Entry(entry) => entries.push((line_number, *entry)),
        Appended::ShellCommand(command) => {
            let call = ShellCall::at_line(line_number);
            let entry = call.entry(&command, timestamp);
            *open_shell_call = Some((entries.len(), call));
            entries.push((line_number, entry));
        }
        Appended::ShellOutput(output) => match open_shell_call.take() {
            Some((index, call)) => {
                if let Some((_, entry)) = entries.get_mut(index) {
                    call.add_result(entry, output);
                }
            }
            None => {
                let call = ShellCall::at_line(line_number);
                let mut entry = call.entry("", timestamp);
                call.add_result(&mut entry, output);
                entries.push((line_number, entry));
            }
        },
    }
}

fn normalize_line(object: &Map<String, Value>, dropped: &mut DroppedRecords) -> Option<LogEntry> {
    let timestamp = entry_time(object);
    let kind = object.get("type").and_then(Value::as_str);
    match kind {
        Some("context.append_loop_event") => {
            loop_event(object.get("event")?.as_object()?, timestamp, dropped)
        }
        Some("context.apply_compaction") => compaction_summary(object, timestamp),
        // `turn.prompt` and `turn.steer` restate what `context.append_message`
        // already recorded, so reading them too would double every prompt.
        Some("turn.prompt" | "turn.steer") => {
            dropped.skipped("type", kind);
            None
        }
        _ => {
            dropped.unrecognised("type", kind);
            None
        }
    }
}

fn loop_event(
    event: &Map<String, Value>,
    timestamp: Option<String>,
    dropped: &mut DroppedRecords,
) -> Option<LogEntry> {
    let kind = event.get("type").and_then(Value::as_str);
    match kind {
        Some("content.part") => content_part(event, timestamp),
        Some("tool.call") => tool_call(event, timestamp),
        Some("tool.result") => tool_result(event, timestamp),
        // `step.begin` / `step.end` are request framing; `usage.record`
        // carries the tokens.
        Some("step.begin" | "step.end") => {
            dropped.skipped("event.type", kind);
            None
        }
        _ => {
            dropped.unrecognised("event.type", kind);
            None
        }
    }
}

fn content_part(event: &Map<String, Value>, timestamp: Option<String>) -> Option<LogEntry> {
    let part = event.get("part")?.as_object()?;
    let block = match part.get("type").and_then(Value::as_str)? {
        "text" => ContentBlock::Text {
            text: nonempty_string(part, "text")?,
        },
        "think" => ContentBlock::Thinking {
            thinking: nonempty_string(part, "think")?,
            signature: String::new(),
        },
        _ => return None,
    };
    Some(assistant_entry(vec![block], timestamp))
}

fn tool_call(event: &Map<String, Value>, timestamp: Option<String>) -> Option<LogEntry> {
    let name = string_field(event, "name").unwrap_or_else(|| "unknown".to_owned());
    let tool = canonical_tool(&name);
    let mut input = event.get("args").cloned().unwrap_or_else(|| json!({}));
    canonicalize_input(tool, &mut input);
    let block = ContentBlock::ToolUse {
        id: string_field(event, "toolCallId").unwrap_or_else(|| "unknown".to_owned()),
        name,
        tool,
        input,
    };
    Some(assistant_entry(vec![block], timestamp))
}

fn canonical_tool(name: &str) -> Tool {
    match name {
        "Bash" => Tool::Shell,
        "Read" | "ReadMediaFile" => Tool::Read,
        "Edit" => Tool::Edit,
        "Write" => Tool::Write,
        "Grep" => Tool::Grep,
        "Glob" => Tool::Glob,
        "Skill" => Tool::Skill,
        "Agent" => Tool::Agent,
        "TodoList" => Tool::TaskList,
        "FetchURL" => Tool::WebFetch,
        "TaskOutput" => Tool::Wait,
        _ => Tool::Other,
    }
}

fn canonicalize_input(tool: Tool, input: &mut Value) {
    if !tool.takes_file_path() {
        return;
    }
    let Some(arguments) = input.as_object_mut() else {
        return;
    };
    rename_key(arguments, "path", "file_path");
}

fn tool_result(event: &Map<String, Value>, timestamp: Option<String>) -> Option<LogEntry> {
    // The result is `{output, note}` — the note is runtime plumbing wrapped in
    // `<system>…</system>`, so only the output is kept.
    let text = match event.get("result") {
        Some(Value::String(text)) => text.clone(),
        Some(value) => value
            .get("output")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        None => String::new(),
    };
    let text = tool_result_text(&Value::String(text)).unwrap_or_default();
    Some(LogEntry::User {
        message: UserMessage {
            role: "user".to_owned(),
            content: UserContent::Blocks(vec![ContentBlock::ToolResult {
                tool_use_id: string_field(event, "toolCallId")
                    .unwrap_or_else(|| "unknown".to_owned()),
                content: Some(json!(text)),
                standalone_tool_name: None,
            }]),
        },
        timestamp,
        uuid: None,
        cwd: None,
        parent_tool_use_id: None,
        source_tool_use_id: None,
        usage: None,
    })
}

fn assistant_entry(content: Vec<ContentBlock>, timestamp: Option<String>) -> LogEntry {
    LogEntry::Assistant {
        agent: Some("Kimi".to_owned()),
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

/// Everything a turn-scoped `usage.record` says: the model it names — emitted
/// as a Model entry when it differs from the last one seen — followed by the
/// turn's token counts. Session-scoped records restate running totals and
/// would double-count, so they yield nothing.
fn usage_record_entries(
    object: &Map<String, Value>,
    last_model: &mut Option<String>,
) -> Vec<LogEntry> {
    if object.get("usageScope").and_then(Value::as_str) != Some("turn") {
        return Vec::new();
    }
    let mut entries = Vec::new();
    if let Some(model) = object.get("model").and_then(Value::as_str)
        && last_model.as_deref() != Some(model)
    {
        *last_model = Some(model.to_owned());
        entries.push(LogEntry::PiMetadata {
            label: "Model".to_owned(),
            text: model.to_owned(),
            timestamp: entry_time(object),
            searchable: false,
            usage: None,
        });
    }
    entries.extend(token_usage(object));
    entries
}

/// The turn's tokens, as an invisible metadata entry.
fn token_usage(object: &Map<String, Value>) -> Option<LogEntry> {
    let recorded = object.get("usage")?.as_object()?;
    let count = |field: &str| recorded.get(field).and_then(Value::as_u64).unwrap_or(0);
    let usage = TokenUsage {
        input_tokens: count("inputOther"),
        output_tokens: count("output"),
        cache_creation_input_tokens: count("inputCacheCreation"),
        cache_read_input_tokens: count("inputCacheRead"),
    };
    let total = usage.input_tokens
        + usage.output_tokens
        + usage.cache_creation_input_tokens
        + usage.cache_read_input_tokens;
    if total == 0 {
        return None;
    }
    Some(LogEntry::PiMetadata {
        label: "Usage".to_owned(),
        text: String::new(),
        timestamp: None,
        searchable: false,
        usage: Some(usage),
    })
}

fn compaction_summary(object: &Map<String, Value>, timestamp: Option<String>) -> Option<LogEntry> {
    let summary = object.get("summary").and_then(Value::as_str)?;
    if summary.is_empty() {
        return None;
    }
    Some(LogEntry::PiMetadata {
        label: "Compaction".to_owned(),
        text: summary.to_owned(),
        timestamp,
        searchable: false,
        usage: None,
    })
}

/// Wire events stamp milliseconds since the epoch; entries carry RFC 3339 so
/// timestamps compare and render like every other provider's.
fn entry_time(object: &Map<String, Value>) -> Option<String> {
    object
        .get("time")
        .and_then(Value::as_i64)
        .and_then(rfc3339_from_millis)
}

fn nonempty_string(object: &Map<String, Value>, field: &str) -> Option<String> {
    string_field(object, field).filter(|text| !text.is_empty())
}

fn string_field(object: &Map<String, Value>, field: &str) -> Option<String> {
    object.get(field).and_then(Value::as_str).map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::format::dropped::DropOutcome;
    use crate::history::parser::process_conversation_file;

    #[test]
    fn counting_names_skipped_and_unrecognised_wire_and_loop_event_types() {
        let wire = [
            r#"{"type":"metadata","protocol_version":"1.5","created_at":1755000000000}"#,
            r#"{"type":"teleport"}"#,
            r#"{"type":"context.append_loop_event","event":{"type":"hologram"}}"#,
            r#"{"type":"turn.prompt"}"#,
            r#"{"type":"context.append_loop_event","event":{"type":"step.begin"}}"#,
        ]
        .join("\n");

        let parsed = parse_reader(
            std::io::Cursor::new(wire.as_bytes()),
            DroppedRecords::counting(),
        )
        .unwrap()
        .expect("a wire");

        let version = Some("wire protocol 1.5");
        assert_eq!(
            parsed.dropped.counts().collect::<Vec<_>>(),
            [
                (version, "event.type=hologram", DropOutcome::Unrecognised, 1),
                (version, "event.type=step.begin", DropOutcome::Skipped, 1),
                (version, "type=teleport", DropOutcome::Unrecognised, 1),
                (version, "type=turn.prompt", DropOutcome::Skipped, 1),
            ]
        );
    }

    const SESSION: &str = "session_0f000000-0000-4000-8000-000000000001";

    fn fixture(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/kimi")
            .join(name)
    }

    /// A session directory in the current layout — `state.json`, a main wire
    /// and one sub-agent wire — returning the main wire's path.
    fn family_session(home: &Path) -> PathBuf {
        let session_dir = home.join("sessions/wd_kimi-project_abc123").join(SESSION);
        std::fs::create_dir_all(session_dir.join("agents/main")).unwrap();
        std::fs::create_dir_all(session_dir.join("agents/agent-0")).unwrap();
        std::fs::write(
            session_dir.join("state.json"),
            format!(
                concat!(
                    "{{\"id\":\"{id}\",\"version\":2,\"cwd\":\"/tmp/kimi-project\",",
                    "\"createdAt\":1786010400000,\"updatedAt\":1786010521000,",
                    "\"agents\":{{\"main\":{{\"type\":\"main\",\"parentAgentId\":null}},",
                    "\"agent-0\":{{\"type\":\"sub\",\"parentAgentId\":\"main\"}}}},",
                    "\"custom\":{{}},\"title\":\"kimi generated title\",",
                    "\"isCustomTitle\":false,\"titleKind\":\"replaceable\"}}",
                ),
                id = SESSION
            ),
        )
        .unwrap();
        let main_wire = session_dir.join("agents/main/wire.jsonl");
        std::fs::copy(fixture("wire.jsonl"), &main_wire).unwrap();
        std::fs::copy(
            fixture("subagent-wire.jsonl"),
            session_dir.join("agents/agent-0/wire.jsonl"),
        )
        .unwrap();
        main_wire
    }

    fn entry_json(projection: &SessionProjection) -> String {
        projection
            .entries
            .iter()
            .map(|(_, entry)| serde_json::to_string(entry).unwrap())
            .collect::<Vec<_>>()
            .join(" ")
    }

    #[test]
    fn projects_dialogue_and_drops_runtime_noise() {
        let home = tempfile::tempdir().unwrap();
        let wire = family_session(home.path());

        let projection = KIMI_WIRE
            .parse_transcript(crate::history::FOR_FIXTURES, &wire)
            .unwrap()
            .unwrap();

        assert_eq!(projection.source, Source::Kimi);
        assert_eq!(projection.header.id, SESSION);
        assert_eq!(projection.header.cwd, PathBuf::from("/tmp/kimi-project"));
        assert_eq!(projection.header.timestamp, "2026-08-06T10:00:00.000Z");
        assert_eq!(projection.title.as_deref(), Some("kimi generated title"));
        assert!(
            matches!(
                &projection.entries[0].1,
                LogEntry::AiTitle { ai_title } if ai_title == "kimi generated title"
            ),
            "a title Kimi generated is a summary, not a user rename"
        );
        assert_eq!(projection.malformed_lines, vec![14]);

        let json = entry_json(&projection);
        for kept in [
            "active kimi question",
            "kimi answer searchable",
            "THINKING_VISIBLE kimi reasoning",
            "kimi tool output searchable",
            "TOOL_INPUT_VISIBLE",
            "steer follow-up question",
            "final kimi reply",
            "COMPACTION_SUMMARY_TEXT",
            "kimi-k3-test",
        ] {
            assert!(json.contains(kept), "missing {kept:?}");
        }
        for dropped in [
            "SYSTEM_REMINDER_SENTINEL",
            "NOTE_SENTINEL",
            "UNKNOWN_EVENT_SENTINEL",
            "LLM_REQUEST_SENTINEL",
            "kimi-k3-ignored",
        ] {
            assert!(!json.contains(dropped), "leaked {dropped:?}");
        }
        assert_eq!(
            json.matches("active kimi question").count(),
            1,
            "turn.prompt restates the appended message and must not double it"
        );
        assert_eq!(
            json.matches("steer follow-up question").count(),
            1,
            "turn.steer restates the appended message and must not double it"
        );
        assert_eq!(
            json.matches("\"Model\"").count(),
            2,
            "the model is emitted on change only"
        );
    }

    #[test]
    fn extracts_kimi_conversation_metadata() {
        let home = tempfile::tempdir().unwrap();
        let wire = family_session(home.path());

        let conversation = process_conversation_file(wire, None, None)
            .unwrap()
            .unwrap();

        assert_eq!(conversation.source, Source::Kimi);
        assert_eq!(conversation.session_id, SESSION);
        assert_eq!(
            conversation.model.as_deref(),
            Some("kimi-k3-newmodel"),
            "the newest model named by a turn is the conversation's"
        );
        // Two turn-scoped usage records: 100+40+10+50 and 10+5+0+0; the
        // session-scoped record restates totals and must not count.
        assert_eq!(conversation.total_tokens, 215);
        assert_eq!(conversation.duration_minutes, Some(2));
        assert_eq!(
            conversation.summary.as_deref(),
            Some("kimi generated title")
        );
        assert!(conversation.preview.starts_with("active kimi question"));
        for searchable in ["active kimi question", "kimi tool output searchable"] {
            assert!(
                conversation.full_text.contains(searchable),
                "missing {searchable:?} from {:?}",
                conversation.full_text
            );
        }
        for hidden in [
            "COMPACTION_SUMMARY_TEXT",
            "THINKING_VISIBLE",
            "SYSTEM_REMINDER_SENTINEL",
        ] {
            assert!(
                !conversation.full_text.contains(hidden),
                "{hidden:?} does not belong in the search index"
            );
        }
        assert_eq!(
            conversation
                .full_text
                .matches("active kimi question")
                .count(),
            1,
            "a prompt indexed twice would rank twice"
        );
    }

    #[test]
    fn sub_agent_wire_carries_qualified_id_and_no_title() {
        let home = tempfile::tempdir().unwrap();
        let main_wire = family_session(home.path());
        let sub_wire = main_wire
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("agent-0/wire.jsonl");

        let projection = KIMI_WIRE
            .parse_transcript(crate::history::FOR_FIXTURES, &sub_wire)
            .unwrap()
            .unwrap();

        assert_eq!(projection.header.id, format!("{SESSION}#agent-0"));
        assert_eq!(projection.header.thread_label(), "agent-0");
        assert_eq!(projection.title, None);
        assert!(entry_json(&projection).contains("kimi child answer searchable"));
    }

    #[test]
    fn view_splices_sub_agent_wires_in_as_progress_entries() {
        let home = tempfile::tempdir().unwrap();
        let wire = family_session(home.path());
        let sub_wire = wire
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("agent-0/wire.jsonl");

        let indexed = KIMI_WIRE
            .parse_transcript(crate::history::FOR_FIXTURES, &wire)
            .unwrap()
            .unwrap();
        assert!(
            !entry_json(&indexed).contains("kimi child answer searchable"),
            "the plain parse must not splice; the row merges whole threads instead"
        );

        let view = super::super::view_projection(
            &KIMI_WIRE,
            crate::history::FOR_FIXTURES,
            &wire,
            &[sub_wire],
        )
        .unwrap()
        .unwrap();
        let json = entry_json(&view);
        assert!(json.contains("kimi child task question"));
        assert!(json.contains("kimi child answer searchable"));

        let progress = view
            .entries
            .iter()
            .find_map(|(_, entry)| match entry {
                LogEntry::Progress { data, .. } => crate::log_entry::parse_agent_progress(data),
                _ => None,
            })
            .expect("a spliced entry is shaped as Claude's agent_progress");
        assert_eq!(progress.agent_id, "agent-0");
    }

    #[test]
    fn view_leaves_out_sub_agents_opening_prompt() {
        let home = tempfile::tempdir().unwrap();
        let wire = family_session(home.path());
        let sub_wire = wire
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("agent-0/wire.jsonl");
        let lines = [
            json!({"type": "metadata", "protocol_version": "1.5"}),
            json!({
                "type": "context.append_message",
                "message": {
                    "role": "user",
                    "content": [{"type": "text", "text": "SUB_AGENT_PROMPT research the part numbers"}],
                    "origin": {"kind": "system_trigger", "name": "subagent"}
                }
            }),
            json!({
                "type": "context.append_loop_event",
                "event": {"type": "content.part", "part": {"type": "text", "text": "kimi child answer searchable"}}
            }),
        ];
        std::fs::write(&sub_wire, lines.map(|line| line.to_string()).join("\n")).unwrap();

        let view = super::super::view_projection(
            &KIMI_WIRE,
            crate::history::FOR_FIXTURES,
            &wire,
            &[sub_wire],
        )
        .unwrap()
        .unwrap();

        let json = entry_json(&view);
        assert!(json.contains("kimi child answer searchable"));
        assert!(!json.contains("SUB_AGENT_PROMPT"), "{json}");
    }

    #[test]
    fn search_finds_system_text_as_it_shows() {
        let directory = tempfile::tempdir().unwrap();
        let wire = directory.path().join("wire.jsonl");
        let message = |text: &str, origin: Value| {
            json!({
                "type": "context.append_message",
                "message": {"role": "user", "content": [{"type": "text", "text": text}], "origin": origin}
            })
        };
        let lines = [
            json!({"type": "metadata", "protocol_version": "1.5"}),
            message("the user's prompt", json!({"kind": "user"})),
            message(
                "<notification id=\"task:bash-1:completed\" category=\"task\" type=\"task.completed\" source_kind=\"background_task\" source_id=\"bash-1\">\nTitle: Background process completed\nSeverity: info\nHeadless trace completed.\n</notification>",
                json!({"kind": "task", "taskId": "bash-1", "status": "completed"}),
            ),
            message(
                "<hook_result hook_event=\"UserPromptSubmit\">\nPrompt quality: C.\n</hook_result>",
                json!({"kind": "hook_result", "event": "UserPromptSubmit"}),
            ),
            message(
                "SKILL_FILE_TEXT for the model",
                json!({"kind": "skill_activation", "skillName": "docs", "trigger": "model-tool"}),
            ),
        ];
        std::fs::write(&wire, lines.map(|line| line.to_string()).join("\n")).unwrap();

        let conversation = process_conversation_file(wire, None, None)
            .unwrap()
            .unwrap();

        for shown in [
            "the user's prompt",
            "Headless trace completed.",
            "Prompt quality: C.",
        ] {
            assert!(
                conversation.full_text.contains(shown),
                "missing {shown:?} from {:?}",
                conversation.full_text
            );
        }
        for left_out in ["SKILL_FILE_TEXT", "Severity:", "<hook_result"] {
            assert!(
                !conversation.full_text.contains(left_out),
                "{left_out:?} reached the search text: {:?}",
                conversation.full_text
            );
        }
    }

    /// A sub-agent wire that cannot be read is left out of the view; the
    /// session's own entries still render.
    #[test]
    fn view_leaves_out_sub_agent_wire_it_cannot_read() {
        let home = tempfile::tempdir().unwrap();
        let wire = family_session(home.path());
        let sub_wire = wire
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("agent-0/wire.jsonl");
        std::fs::write(&sub_wire, [0xff, 0xfe]).unwrap();

        let view = super::super::view_projection(
            &KIMI_WIRE,
            crate::history::FOR_FIXTURES,
            &wire,
            &[sub_wire],
        )
        .unwrap()
        .unwrap();

        let json = entry_json(&view);
        assert!(json.contains("active kimi question"));
        assert!(!json.contains("agent_progress"));
    }

    /// The legacy layout keeps one wire at the session directory root; its
    /// state uses `workDir` and predates `titleKind`.
    #[test]
    fn legacy_session_reads_its_state_and_counts_as_main() {
        let home = tempfile::tempdir().unwrap();
        let session_dir = home
            .path()
            .join("sessions/wd_old_1234")
            .join("session_1e000000-0000-4000-8000-000000000002");
        std::fs::create_dir_all(&session_dir).unwrap();
        std::fs::write(
            session_dir.join("state.json"),
            concat!(
                "{\"createdAt\":1786010400000,\"updatedAt\":1786010500000,",
                "\"workDir\":\"/tmp/legacy-project\",\"agents\":{},",
                "\"title\":\"chosen by hand\",\"isCustomTitle\":true}",
            ),
        )
        .unwrap();
        let wire = session_dir.join("wire.jsonl");
        std::fs::copy(fixture("wire.jsonl"), &wire).unwrap();

        let projection = KIMI_WIRE
            .parse_transcript(crate::history::FOR_FIXTURES, &wire)
            .unwrap()
            .unwrap();

        assert_eq!(
            projection.header.id,
            "session_1e000000-0000-4000-8000-000000000002"
        );
        assert_eq!(projection.header.cwd, PathBuf::from("/tmp/legacy-project"));
        assert!(matches!(
            &projection.entries[0].1,
            LogEntry::CustomTitle { custom_title } if custom_title == "chosen by hand"
        ));
    }

    /// A wire without any session directory around it — a fixture, a stray
    /// copy — still parses; it just has no state to name it.
    #[test]
    fn wire_outside_session_directory_is_plain_dialogue() {
        let projection = KIMI_WIRE
            .parse_transcript(crate::history::FOR_FIXTURES, &fixture("wire.jsonl"))
            .unwrap()
            .unwrap();

        assert_eq!(projection.header.id, "kimi");
        assert_eq!(projection.title, None);
        assert!(entry_json(&projection).contains("active kimi question"));
    }

    #[test]
    fn file_that_is_not_kimi_wire_is_not_recognized() {
        for foreign in [
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/pi/v3-branched.jsonl"),
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/codex/rollout.jsonl"),
        ] {
            assert!(
                KIMI_WIRE
                    .parse_transcript(crate::history::FOR_FIXTURES, &foreign)
                    .unwrap()
                    .is_none(),
                "{} is not a Kimi wire",
                foreign.display()
            );
        }

        let directory = tempfile::tempdir().unwrap();
        let claude = directory.path().join("claude.jsonl");
        std::fs::write(&claude, "{\"type\":\"user\"}\n").unwrap();
        assert!(
            KIMI_WIRE
                .parse_transcript(crate::history::FOR_FIXTURES, &claude)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn tool_calls_sharing_uuid_in_different_turns_are_separate_messages() {
        let directory = tempfile::tempdir().unwrap();
        let wire = directory.path().join("wire.jsonl");
        let call = |turn: &str, path: &str| {
            json!({
                "type": "context.append_loop_event",
                "event": {
                    "type": "tool.call", "uuid": "Read_0", "turnId": turn, "step": 1,
                    "toolCallId": "Read_0", "name": "Read", "args": {"path": path}
                }
            })
            .to_string()
        };
        let prompt = |text: &str| {
            json!({
                "type": "context.append_message",
                "message": {"role": "user", "content": [{"type": "text", "text": text}]}
            })
            .to_string()
        };
        let lines = [
            json!({"type": "metadata", "protocol_version": "1.5"}).to_string(),
            prompt("first question"),
            call("0", "first_turn_file.md"),
            prompt("second question"),
            call("1", "second_turn_file.md"),
        ];
        std::fs::write(&wire, lines.join("\n")).unwrap();

        let transcript = crate::agent::transcript::AgentTranscript::parse_uncached(
            crate::history::FOR_FIXTURES,
            Source::Kimi,
            &wire,
            &[],
        )
        .unwrap();

        let read_paths = transcript
            .messages
            .iter()
            .flat_map(|message| &message.parts)
            .filter_map(|part| match part {
                crate::agent::transcript::AgentMessagePart::ToolUse { input, .. } => {
                    input.get("file_path").and_then(Value::as_str)
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(read_paths, ["first_turn_file.md", "second_turn_file.md"]);
        assert_eq!(transcript.messages.len(), 4);
    }

    fn tool_use(name: &str, args: Value) -> (Tool, Value) {
        let event = json!({"toolCallId": "call_1", "name": name, "args": args});
        let entry = tool_call(event.as_object().unwrap(), None).unwrap();
        let LogEntry::Assistant { message, .. } = entry else {
            panic!("not an assistant entry: {entry:?}");
        };
        match message.content.into_iter().next() {
            Some(ContentBlock::ToolUse {
                name: kept,
                tool,
                input,
                ..
            }) => {
                assert_eq!(kept, name);
                (tool, input)
            }
            other => panic!("not a tool use: {other:?}"),
        }
    }

    #[test]
    fn every_kimi_tool_name_lands_in_its_bucket() {
        let expected = [
            ("Bash", Tool::Shell),
            ("Read", Tool::Read),
            ("ReadMediaFile", Tool::Read),
            ("Edit", Tool::Edit),
            ("Write", Tool::Write),
            ("Grep", Tool::Grep),
            ("Glob", Tool::Glob),
            ("Skill", Tool::Skill),
            ("Agent", Tool::Agent),
            ("TodoList", Tool::TaskList),
            ("FetchURL", Tool::WebFetch),
            ("TaskOutput", Tool::Wait),
            ("TaskList", Tool::Other),
            ("TaskStop", Tool::Other),
            ("AskUserQuestion", Tool::Other),
            ("ExitPlanMode", Tool::Other),
            ("mcp__rustrover__ide_find_references", Tool::Other),
        ];
        for (name, tool) in expected {
            assert_eq!(tool_use(name, json!({})).0, tool, "{name}");
        }
    }

    #[test]
    fn file_tools_address_file_as_file_path_and_keep_rest() {
        assert_eq!(
            tool_use("Read", json!({"path": "src/lib.rs", "offset": 10})).1,
            json!({"file_path": "src/lib.rs", "offset": 10})
        );
        assert_eq!(
            tool_use("ReadMediaFile", json!({"path": "shot.png"})).1,
            json!({"file_path": "shot.png"})
        );
        assert_eq!(
            tool_use(
                "Edit",
                json!({"path": "src/lib.rs", "old_string": "a", "new_string": "b", "replace_all": true})
            )
            .1,
            json!({"file_path": "src/lib.rs", "old_string": "a", "new_string": "b", "replace_all": true})
        );
        assert_eq!(
            tool_use("Write", json!({"path": "NEW.md", "content": "# New"})).1,
            json!({"file_path": "NEW.md", "content": "# New"})
        );
        assert_eq!(
            tool_use("Grep", json!({"pattern": "fn main", "path": "src"})).1,
            json!({"pattern": "fn main", "path": "src"})
        );
        assert_eq!(tool_use("Read", json!({})), (Tool::Read, json!({})));
    }

    mod resumed_reads {
        use super::*;
        use crate::history::format::lines::record_splits;
        use crate::history::provider::contract_tests::every_record_sessions;
        use crate::history::provider::transcript_reads::resume;

        fn reader() -> &'static dyn TranscriptReader {
            Source::Kimi.provider().storage().transcript_reader()
        }

        fn read_whole(path: &Path) -> Option<SessionProjection> {
            KIMI_WIRE
                .parse_transcript(crate::history::FOR_FIXTURES, path)
                .unwrap()
        }

        /// `entries` as text a test compares.
        fn listed(entries: &[(RecordLine, LogEntry)]) -> Vec<String> {
            entries
                .iter()
                .map(|(record, entry)| format!("{} {entry:?}", record.line))
                .collect()
        }

        /// Asserts that `prefix`, a read of a wire's beginning, up to the
        /// window's first line, and then `window`, the read resumed from it
        /// over the whole wire, hold what `full`, one read of the whole wire,
        /// holds, and that `window` leaves the same [`TranscriptRead`] as
        /// `full`.
        fn assert_window_completes(
            name: &str,
            prefix: &SessionProjection,
            window: &ResumedWindow,
            full: &SessionProjection,
        ) {
            let kept = prefix
                .entries
                .iter()
                .filter(|(record, _)| record.line < window.start_line)
                .chain(&window.entries)
                .cloned()
                .collect::<Vec<_>>();
            assert_eq!(
                listed(&kept),
                listed(&full.entries),
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
                "{name}: the window leaves the full read's `TranscriptRead`"
            );
        }

        fn appended(text: &str, origin: Value) -> String {
            json!({
                "type": "context.append_message",
                "message": {"role": "user", "content": [{"type": "text", "text": text}], "origin": origin},
                "time": 1786010700000_i64,
            })
            .to_string()
        }

        fn usage(model: &str) -> String {
            json!({
                "type": "usage.record", "usageScope": "turn", "model": model,
                "usage": {"inputOther": 10, "output": 5}, "time": 1786010701000_i64,
            })
            .to_string()
        }

        /// Two `!` commands, the second answered after a prompt and a usage
        /// record naming another model, then a malformed line and an output
        /// without a command before it.
        fn shell_commands() -> String {
            let input = |command: &str| {
                appended(
                    &format!("<bash-input>\n{command}\n</bash-input>"),
                    json!({"kind": "shell_command", "phase": "input"}),
                )
            };
            let output = |text: &str| {
                appended(
                    &format!("<bash-stdout>{text}\n</bash-stdout>"),
                    json!({"kind": "shell_command", "phase": "output"}),
                )
            };
            [
                json!({"type": "metadata", "protocol_version": "1.5", "created_at": 1786010400000_i64})
                    .to_string(),
                usage("kimi-k3-first"),
                input("ls"),
                output("a.rs"),
                input("cargo test"),
                appended("is it done?", json!({"kind": "user"})),
                usage("kimi-k3-second"),
                output("ok"),
                "{not json".to_owned(),
                output("stray"),
            ]
            .join("\n")
                + "\n"
        }

        #[test]
        fn prefix_and_resumed_read_equal_full_read_at_every_split_of_every_kimi_fixture() {
            let home = tempfile::tempdir().unwrap();
            let family = family_session(home.path());
            let shell = home.path().join("shell/wire.jsonl");
            std::fs::create_dir_all(shell.parent().unwrap()).unwrap();
            std::fs::write(&shell, shell_commands()).unwrap();
            let every_record = every_record_sessions()
                .into_iter()
                .find(|session| session.row.source == Source::Kimi)
                .unwrap();
            let copies = std::iter::once(&every_record.row.path)
                .chain(&every_record.row.subagents)
                .chain([&fixture("wire.jsonl"), &fixture("subagent-wire.jsonl")])
                .enumerate()
                .map(|(index, path)| {
                    let copy = home.path().join(format!("copy-{index}/wire.jsonl"));
                    std::fs::create_dir_all(copy.parent().unwrap()).unwrap();
                    std::fs::copy(path, &copy).unwrap();
                    copy
                })
                .collect::<Vec<_>>();
            let sub_agent = family
                .parent()
                .unwrap()
                .with_file_name("agent-0")
                .join("wire.jsonl");
            for path in copies.into_iter().chain([family, sub_agent, shell]) {
                let bytes = std::fs::read(&path).unwrap();
                let full = read_whole(&path).unwrap();
                for split in record_splits(&bytes) {
                    let name = format!("{} at {split}", path.display());
                    std::fs::write(&path, &bytes[..split]).unwrap();
                    let prefix = read_whole(&path);
                    std::fs::write(&path, &bytes).unwrap();
                    let Some(prefix) = prefix else {
                        continue;
                    };

                    match resume(
                        reader(),
                        crate::history::FOR_FIXTURES,
                        &path,
                        &prefix.transcript_read,
                    )
                    .unwrap()
                    {
                        Resumption::Window(window) => assert_window_completes(
                            &format!("{name} from line {}", window.start_line),
                            &prefix,
                            &window,
                            &full,
                        ),
                        other => assert!(
                            matches!(other, Resumption::FullRead)
                                && prefix.transcript_read == TranscriptRead::full_read(),
                            "{name}: only a wire with a metadata line that doesn't have a line ending yet reads whole: {other:?}"
                        ),
                    }
                }
            }
        }

        #[test]
        fn shell_command_awaiting_its_output_holds_stable_cursor_before_it() {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("wire.jsonl");
            let bytes = shell_commands();
            let through_second_command = bytes.split_inclusive('\n').take(5).collect::<String>();
            std::fs::write(&path, &through_second_command).unwrap();

            let prefix = read_whole(&path).unwrap();

            assert_eq!(
                prefix.transcript_read.stable_line,
                Some(5),
                "the next read restarts at the command awaiting its output"
            );
            assert_eq!(
                LineCursor::decode(&prefix.transcript_read.cursor).map(|cursor| cursor.line),
                Some(6),
                "the read stops past the command"
            );
        }

        #[test]
        fn wire_with_another_metadata_line_or_working_directory_reads_whole() {
            let home = tempfile::tempdir().unwrap();
            let wire = family_session(home.path());
            let read = read_whole(&wire).unwrap().transcript_read;
            let state = wire
                .parent()
                .unwrap()
                .parent()
                .unwrap()
                .with_file_name("state.json");
            let original_state = std::fs::read_to_string(&state).unwrap();
            assert_eq!(reader().identity(&wire).unwrap(), read.identity);

            std::fs::write(
                &state,
                original_state.replace("/tmp/kimi-project", "/tmp/elsewhere"),
            )
            .unwrap();
            assert_ne!(
                reader().identity(&wire).unwrap(),
                read.identity,
                "the working directory state.json names is part of the identity"
            );
            assert!(matches!(
                resume(reader(), crate::history::FOR_FIXTURES, &wire, &read).unwrap(),
                Resumption::FullRead
            ));

            std::fs::write(
                &state,
                original_state.replace("kimi generated title", "another title"),
            )
            .unwrap();
            assert_eq!(
                reader().identity(&wire).unwrap(),
                read.identity,
                "the title is not part of the identity: every read rereads it"
            );

            let bytes = std::fs::read_to_string(&wire).unwrap();
            std::fs::write(&wire, bytes.replacen("1786010400000", "1786010400001", 1)).unwrap();
            assert_ne!(
                reader().identity(&wire).unwrap(),
                read.identity,
                "the metadata line is part of the identity"
            );
        }
    }
}
