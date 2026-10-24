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

use super::dropped::DroppedRecords;
use super::lines::visit_parsed_lines;
use super::{
    EarlierTurns, SessionFormat, SessionHeader, SessionProjection, append_exit_code, at_own_lines,
};
use super::{rename_key, rfc3339_from_millis};
use crate::error::Result;
use crate::history::Source;
use crate::history::provider::SessionTitle;
use crate::log_entry::{
    AssistantMessage, ContentBlock, LogEntry, RECAP_LABEL, TokenUsage, Tool, UserContent,
    UserMessage, replacement_diff,
};
use crate::parallel::ParallelThreshold;
use chrono::{DateTime, SecondsFormat, Utc};
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs::File;
use std::io::{BufRead, BufReader, ErrorKind};
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
        let updates = match File::open(path) {
            Ok(file) => parse_updates(BufReader::new(file), ParallelThreshold::DEFAULT, dropped)?,
            Err(error) if error.kind() == ErrorKind::NotFound => ParsedUpdates {
                dropped,
                ..ParsedUpdates::default()
            },
            Err(error) => return Err(error.into()),
        };
        Ok(Some(projection(summary, updates)))
    }
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

fn projection(summary: SessionSummary, updates: ParsedUpdates) -> SessionProjection {
    let mut entries = updates.entries;
    // Use the summary's model when no completed turn named one.
    if !updates.named_a_model
        && let Some(model) = summary.current_model_id.clone()
    {
        entries.insert(0, (0, model_entry(model, None)));
    }
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
        // Line 0: the title lives in summary.json, not the transcript.
        entries.insert(0, (0, entry));
        title = Some(text);
    }
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

fn parse_updates(
    reader: impl BufRead,
    parallel: ParallelThreshold,
    dropped: DroppedRecords,
) -> Result<ParsedUpdates> {
    let mut projector = Projector {
        dropped,
        ..Projector::default()
    };
    visit_parsed_lines(
        reader,
        1,
        parallel,
        |line| serde_json::from_str::<Value>(line).ok(),
        |line_number, _, record| match record {
            Some(Value::Object(record)) => projector.record(line_number, &record),
            _ => projector.malformed_lines.push(line_number),
        },
    )?;
    Ok(projector.finish())
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
#[derive(Default)]
struct Projector {
    entries: Vec<(usize, LogEntry)>,
    malformed_lines: Vec<usize>,
    /// The run of chunk records not yet emitted.
    run: Option<ChunkRun>,
    /// Each tool call's index in `entries`, by call id, so a later update can
    /// complete its input.
    calls: HashMap<String, usize>,
    /// Each sub-agent's type by its session id, from its spawn record.
    subagent_types: HashMap<String, String>,
    last_model: Option<String>,
    named_a_model: bool,
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
        self.calls.insert(id, self.entries.len());
        self.entries
            .push((line, assistant_entry(vec![block], timestamp)));
    }

    /// An update carries the call's full input, its result, or both. If the
    /// call's opening record is missing, the update opens the call.
    fn update_call(
        &mut self,
        line: usize,
        update: &Map<String, Value>,
        meta: Option<&Map<String, Value>>,
        timestamp: Option<String>,
    ) {
        let id = call_id(update);
        if !self.calls.contains_key(&id) {
            self.open_call(line, update, meta, timestamp.clone());
        } else if update.get("rawInput").is_some_and(Value::is_object) {
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
        if let Some(query) = backend_search_query(update)
            && let Some(ContentBlock::ToolUse { input, .. }) = self.call_block(&id)
            && let Some(arguments) = input.as_object_mut()
        {
            arguments
                .entry("query")
                .or_insert_with(|| Value::String(query));
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
        let Some(usage) = update.get("usage").and_then(Value::as_object) else {
            return;
        };
        if let Some(models) = usage.get("modelUsage").and_then(Value::as_object) {
            for model in models.keys() {
                if self.last_model.as_deref() != Some(model) {
                    self.last_model = Some(model.clone());
                    self.named_a_model = true;
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
            self.subagent_types.insert(child, subagent_type);
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
            .and_then(|child| self.subagent_types.get(&child).cloned());
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
            named_a_model: self.named_a_model,
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
}
