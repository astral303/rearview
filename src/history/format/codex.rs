//! The Codex rollout format: JSONL lines of `{timestamp, ordinal?, type, payload}`.
//!
//! A rollout records far more than the dialogue — model context snapshots,
//! security scores, rate-limit telemetry — so this reader is a projection by
//! design: `response_item` lines carry the conversation, `token_count` events
//! carry usage, `turn_context` names the model, and everything else is skipped
//! without being an error. Unknown line types are expected; Codex adds them
//! freely between versions.

mod tools;

use super::{SessionFormat, SessionHeader, SessionProjection, append_exit_code, block_texts};
use crate::agent::sanitize::sanitize_agent_text;
use crate::agent::transcript::bounded_tool_result_text;
use crate::error::Result;
use crate::history::Source;
use crate::log_entry::{
    AssistantMessage, ContentBlock, LogEntry, TokenUsage, Tool, UserContent, UserMessage,
};
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::ffi::OsStr;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

pub struct CodexRolloutFormat;

pub static CODEX_ROLLOUT: CodexRolloutFormat = CodexRolloutFormat;

impl SessionFormat for CodexRolloutFormat {
    /// Reads the whole rollout, however large. The biggest sessions are the
    /// most valuable to search, so nothing is truncated: the reader streams one
    /// line at a time, and what stays resident is the extracted text, not the
    /// raw file.
    fn parse_transcript(&self, path: &Path) -> Result<Option<SessionProjection>> {
        let file = File::open(path)?;
        let Some(mut projection) = parse_reader(BufReader::new(file))? else {
            return Ok(None);
        };
        if let Some(title) = thread_title(path, &projection.header.id) {
            // Line 0: the title lives in the session index, not the rollout.
            projection.entries.insert(
                0,
                (
                    0,
                    LogEntry::CustomTitle {
                        custom_title: title.clone(),
                    },
                ),
            );
            projection.title = Some(title);
        }
        Ok(Some(projection))
    }
}

/// The session-level facts of the first `session_meta` line.
///
/// Only the first one: a sub-agent rollout carries the parent's `session_meta`
/// again further down, as inherited model context. `id` is the thread id — the
/// `session_id` field holds the *parent's* id in a sub-agent file, so it never
/// identifies the file it appears in.
struct RolloutHeader {
    id: String,
    timestamp: String,
    cwd: String,
    kind: ThreadKind,
    own_history_start: Option<u64>,
}

/// The thread a rollout's header records: a session, a sub-agent, or one
/// Codex ran for itself.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ThreadKind {
    /// A thread whose header names no parent.
    Session,
    /// A thread whose header names a parent: read through the session at
    /// the far end of its chain, or listed as a session when that parent has
    /// no rollout.
    Subagent { parent_thread_id: String },
    /// A Guardian review, compaction, memory consolidation or internal
    /// session: a thread Codex ran for itself and keeps off its own list.
    /// Not listed or read; deleted with the parent it names.
    Skipped { parent_thread_id: Option<String> },
}

impl ThreadKind {
    pub(crate) fn parent_thread_id(&self) -> Option<&str> {
        match self {
            Self::Session => None,
            Self::Subagent { parent_thread_id } => Some(parent_thread_id),
            Self::Skipped { parent_thread_id } => parent_thread_id.as_deref(),
        }
    }

    pub(crate) fn is_skipped(&self) -> bool {
        matches!(self, Self::Skipped { .. })
    }
}

fn rollout_header(value: &Value) -> Option<RolloutHeader> {
    let object = value.as_object()?;
    if object.get("type").and_then(Value::as_str) != Some("session_meta") {
        return None;
    }
    let payload = object.get("payload")?.as_object()?;
    let field = |name: &str| payload.get(name).and_then(Value::as_str).map(str::to_owned);
    Some(RolloutHeader {
        id: field("id")?,
        timestamp: field("timestamp")?,
        cwd: field("cwd")?,
        kind: thread_kind(payload),
        own_history_start: payload
            .get("subagent_history_start_ordinal")
            .and_then(Value::as_u64),
    })
}

fn thread_kind(payload: &Map<String, Value>) -> ThreadKind {
    let parent_thread_id = payload
        .get("parent_thread_id")
        .and_then(Value::as_str)
        .map(str::to_owned);
    thread_kind_of_source(payload.get("source"), parent_thread_id)
}

/// Classify a thread by its `source` — the `SessionSource` JSON a rollout
/// header carries and the `threads.source` column of Codex's state database
/// stores — and the parent recorded for it. `"cli"` and the other
/// interactive sources are strings, `{"subagent": …}` names a spawn variant,
/// `{"internal": …}` a session Codex ran for itself. Of the spawn variants
/// only `thread_spawn` is a thread the user's session started — a bare
/// `"thread_spawn"` in rollouts written before 0.150, an object carrying the
/// parent and depth since; `review`, `compact`, `memory_consolidation` and
/// `other` (`"guardian"` in the corpus) are Codex's own. With no `source`
/// (older rollouts) or a string one (`cli`), the parent alone decides, and a
/// `thread_spawn` thread with no parent recorded is a session.
pub(crate) fn thread_kind_of_source(
    source: Option<&Value>,
    parent_thread_id: Option<String>,
) -> ThreadKind {
    if let Some(Value::Object(source)) = source {
        if source.contains_key("internal") {
            return ThreadKind::Skipped { parent_thread_id };
        }
        if let Some(subagent) = source.get("subagent") {
            let started_by_a_thread = match subagent {
                Value::String(variant) => variant == "thread_spawn",
                Value::Object(variant) => variant.contains_key("thread_spawn"),
                _ => false,
            };
            if !started_by_a_thread {
                return ThreadKind::Skipped { parent_thread_id };
            }
        }
    }
    match parent_thread_id {
        Some(parent_thread_id) => ThreadKind::Subagent { parent_thread_id },
        None => ThreadKind::Session,
    }
}

/// The kind of thread a rollout records, read from its first line without
/// parsing the rest of the file. `None` for a file that does not open as a
/// rollout. The thread's id is in the file name, not read here.
pub(crate) fn thread_kind_of(path: &Path) -> Option<ThreadKind> {
    let file = File::open(path).ok()?;
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line).ok()? == 0 {
            return None;
        }
        if line.trim().is_empty() {
            continue;
        }
        let value = serde_json::from_str::<Value>(&line).ok()?;
        return Some(rollout_header(&value)?.kind);
    }
}

fn parse_reader(mut reader: impl BufRead) -> Result<Option<SessionProjection>> {
    let mut line = String::new();
    let mut line_number = 0usize;

    // The first line decides whether the file is a rollout at all.
    let header = loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            return Ok(None);
        }
        line_number += 1;
        if line.trim().is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            return Ok(None);
        };
        match rollout_header(&value) {
            Some(header) => break header,
            None => return Ok(None),
        }
    };

    let mut entries = Vec::new();
    let mut malformed_lines = Vec::new();
    let mut last_model = None;
    let mut own_history = match header.own_history_start {
        Some(boundary) => OwnHistory::Pending { boundary },
        None => OwnHistory::Started,
    };
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        line_number += 1;
        if line.trim().is_empty() {
            continue;
        }
        let parsed = serde_json::from_str::<Value>(&line).ok();
        let Some(object) = parsed.as_ref().and_then(Value::as_object) else {
            malformed_lines.push(line_number);
            continue;
        };
        if let OwnHistory::Pending { boundary } = own_history
            && !ordinal_below(object, boundary)
        {
            // The first own record: what was read so far is the parent's
            // history, the model it last named included.
            entries.clear();
            last_model = None;
            own_history = OwnHistory::Started;
        }
        if let Some(entry) = normalize_line(object, &mut last_model) {
            entries.push((line_number, entry));
        }
    }

    Ok(Some(SessionProjection {
        source: Source::Codex,
        header: SessionHeader {
            version: 1,
            id: header.id,
            timestamp: header.timestamp,
            cwd: PathBuf::from(header.cwd),
            thread_label: None,
        },
        title: None,
        entries,
        leaf_id: None,
        malformed_lines,
    }))
}

/// Ownership of the lines below `subagent_history_start_ordinal`, decided by
/// whether a record at or past it follows.
///
/// A live sub-agent thread copies a bounded parent context into its rollout
/// and sets the boundary at its first own record: the lines below are the
/// parent's history, and indexing them would count the parent's text and
/// tokens twice once the thread folds into it. Codex's legacy-to-paginated
/// migration sets the same field past the last record of every sub-agent
/// rollout it rewrites: nothing follows, and the whole file is the thread's
/// own. A live thread that ended before its first own record has the migrated
/// shape and shows its copied context as its own.
enum OwnHistory {
    /// Every line so far sits below the boundary. Its entries are kept until
    /// a record at or past the boundary shows them to be the parent's.
    Pending { boundary: u64 },
    /// Every line from here on is the thread's own.
    Started,
}

fn ordinal_below(object: &Map<String, Value>, boundary: u64) -> bool {
    object
        .get("ordinal")
        .and_then(Value::as_u64)
        .is_some_and(|ordinal| ordinal < boundary)
}

fn normalize_line(
    object: &Map<String, Value>,
    last_model: &mut Option<String>,
) -> Option<LogEntry> {
    let timestamp = object
        .get("timestamp")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let payload = object.get("payload")?.as_object()?;
    match object.get("type").and_then(Value::as_str)? {
        "response_item" => normalize_response_item(payload, timestamp),
        "compacted" => compaction_summary(payload, timestamp),
        "turn_context" => model_change(payload, timestamp, last_model),
        "event_msg" => token_usage(payload),
        // A later `session_meta` is inherited context; `world_state`,
        // `security_risk_score`, `inter_agent_communication*` and whatever
        // Codex adds next carry nothing the browser renders or indexes.
        _ => None,
    }
}

fn normalize_response_item(
    payload: &Map<String, Value>,
    timestamp: Option<String>,
) -> Option<LogEntry> {
    match payload.get("type").and_then(Value::as_str)? {
        "message" => normalize_message(payload, timestamp),
        "reasoning" => reasoning_summary(payload, timestamp),
        "custom_tool_call" => tool_call(payload, payload.get("input").cloned(), timestamp),
        "function_call" => {
            let arguments = payload.get("arguments").and_then(Value::as_str);
            let input = arguments.map(|raw| {
                serde_json::from_str(raw).unwrap_or_else(|_| json!({ "arguments": raw }))
            });
            tool_call(payload, input, timestamp)
        }
        "custom_tool_call_output" | "function_call_output" => tool_result(payload, timestamp),
        "agent_message" => inter_agent_message(payload, timestamp),
        _ => None,
    }
}

fn normalize_message(payload: &Map<String, Value>, timestamp: Option<String>) -> Option<LogEntry> {
    match payload.get("role").and_then(Value::as_str)? {
        "user" => {
            let texts = block_texts(payload.get("content"));
            // A shell command the user ran is one complete XML element, which
            // `is_injected_context` would otherwise read as context Codex
            // injected.
            if let [text] = texts.as_slice()
                && let Some(command) = UserShellCommand::parse(text)
            {
                return Some(user_shell_command_entry(payload, &command, timestamp));
            }
            let typed_by_the_user = texts
                .into_iter()
                .filter(|text| !is_injected_context(text))
                .collect::<Vec<_>>();
            if typed_by_the_user.is_empty() {
                return None;
            }
            Some(LogEntry::User {
                message: UserMessage {
                    role: "user".to_owned(),
                    content: UserContent::Blocks(
                        typed_by_the_user
                            .into_iter()
                            .map(|text| ContentBlock::Text { text })
                            .collect(),
                    ),
                },
                timestamp,
                uuid: None,
                cwd: None,
                parent_tool_use_id: None,
                usage: None,
            })
        }
        "assistant" => {
            let texts = block_texts(payload.get("content"));
            if texts.is_empty() {
                return None;
            }
            Some(assistant_entry(
                payload,
                texts
                    .into_iter()
                    .map(|text| ContentBlock::Text { text })
                    .collect(),
                timestamp,
            ))
        }
        // Developer messages are prompt plumbing — skills, permissions, task
        // wiring — not conversation.
        _ => None,
    }
}

/// A command the user ran through Codex, as its `<user_shell_command>` wrapper
/// records it. `Duration` is in the wrapper too, with no row to sit on.
struct UserShellCommand<'a> {
    command: &'a str,
    exit_code: i64,
    output: &'a str,
}

/// The literals Codex builds the wrapper from, in the order they appear.
const WRAPPER_OPEN: &str = "<user_shell_command>\n<command>\n";
const COMMAND_END: &str = "\n</command>\n<result>\n";
const EXIT_CODE_LABEL: &str = "Exit code: ";
const OUTPUT_LABEL: &str = "\nOutput:\n";
const WRAPPER_CLOSE: &str = "\n</result>\n</user_shell_command>";

impl<'a> UserShellCommand<'a> {
    /// Codex interpolates the command and the output into the wrapper raw, so
    /// a command that prints `</result>` leaves text no XML parser accepts.
    /// Anchoring on the wrapper's own literals reads each half whole: the
    /// closing anchor matches the last closing tag, the split the first
    /// opening one.
    ///
    /// A wrapper that fails an anchor falls through to `is_injected_context`,
    /// which drops it, so a format Codex changes costs a missing row rather
    /// than a wrong one.
    fn parse(text: &'a str) -> Option<Self> {
        let inner = text
            .trim()
            .strip_prefix(WRAPPER_OPEN)?
            .strip_suffix(WRAPPER_CLOSE)?;
        let (command, result) = inner.split_once(COMMAND_END)?;
        let (exit_line, _) = result.split_once('\n')?;
        Some(Self {
            command,
            exit_code: exit_line
                .strip_prefix(EXIT_CODE_LABEL)?
                .trim()
                .parse()
                .ok()?,
            // A command that printed nothing carries no `Output:` section.
            output: result.split_once(OUTPUT_LABEL).map_or("", |(_, out)| out),
        })
    }

    /// Builds the result body: what the command printed, with the exit line
    /// below it when the command failed, and the shell's terminal styling and
    /// `\r\n` endings stripped.
    fn result_text(&self) -> String {
        let mut text = sanitize_agent_text(self.output).trim_end().to_owned();
        if self.exit_code != 0 {
            append_exit_code(&mut text, self.exit_code);
        }
        bounded_tool_result_text(&json!(text)).unwrap_or_default()
    }
}

/// Builds one command's call-and-result pair, as the Pi reader builds it for
/// its own record.
fn user_shell_command_entry(
    payload: &Map<String, Value>,
    command: &UserShellCommand<'_>,
    timestamp: Option<String>,
) -> LogEntry {
    let call_id = string_field(payload, "id").unwrap_or_else(|| "unknown".to_owned());
    LogEntry::User {
        message: UserMessage {
            role: "user".to_owned(),
            content: UserContent::Blocks(vec![
                ContentBlock::ToolUse {
                    id: call_id.clone(),
                    name: "shell".to_owned(),
                    tool: Tool::UserShell,
                    input: json!({ "command": sanitize_agent_text(command.command) }),
                },
                ContentBlock::ToolResult {
                    tool_use_id: call_id,
                    content: Some(json!(command.result_text())),
                    standalone_tool_name: None,
                },
            ]),
        },
        timestamp,
        uuid: None,
        cwd: None,
        parent_tool_use_id: None,
        usage: None,
    }
}

/// Reasoning arrives encrypted with an optional plain-text summary; only the
/// summary is readable, and locally it is almost always absent.
fn reasoning_summary(payload: &Map<String, Value>, timestamp: Option<String>) -> Option<LogEntry> {
    let blocks = block_texts(payload.get("summary"))
        .into_iter()
        .map(|thinking| ContentBlock::Thinking {
            thinking,
            signature: String::new(),
        })
        .collect::<Vec<_>>();
    if blocks.is_empty() {
        return None;
    }
    Some(assistant_entry(payload, blocks, timestamp))
}

fn tool_call(
    payload: &Map<String, Value>,
    input: Option<Value>,
    timestamp: Option<String>,
) -> Option<LogEntry> {
    let call_id = string_field(payload, "call_id").unwrap_or_else(|| "unknown".to_owned());
    let name = string_field(payload, "name").unwrap_or_else(|| "unknown".to_owned());
    let blocks = tools::tool_use_blocks(&call_id, &name, &input.unwrap_or_else(|| json!({})));
    Some(assistant_entry(payload, blocks, timestamp))
}

/// Reads a tool-result payload into a user entry. With no `call_id` the result
/// answers no call: it takes the item's own `id`, or `"unknown"` when the
/// payload carries neither, and keeps the tool name the payload gives.
fn tool_result(payload: &Map<String, Value>, timestamp: Option<String>) -> Option<LogEntry> {
    // Output is either one string or content blocks typed `input_text`, which
    // the shared bounding helper would skip — so texts are gathered first and
    // only the length bound is delegated.
    let text = match payload.get("output") {
        Some(Value::String(text)) => text.clone(),
        output => block_texts(output).join("\n"),
    };
    let text = bounded_tool_result_text(&json!(text)).unwrap_or_default();
    // The id and the name are decided together: with no `call_id` the result
    // answers no call, so it takes the item's own id and the tool it names.
    let (tool_use_id, standalone_tool_name) = match string_field(payload, "call_id") {
        Some(call_id) => (call_id, None),
        None => (
            string_field(payload, "id").unwrap_or_else(|| "unknown".to_owned()),
            string_field(payload, "name"),
        ),
    };
    Some(LogEntry::User {
        message: UserMessage {
            role: "user".to_owned(),
            content: UserContent::Blocks(vec![ContentBlock::ToolResult {
                tool_use_id,
                content: Some(json!(text)),
                standalone_tool_name,
            }]),
        },
        timestamp,
        uuid: None,
        cwd: None,
        parent_tool_use_id: None,
        usage: None,
    })
}

/// A message between agents of a multi-agent run. Modelled as a tool call — the
/// same treatment Claude's Task dispatches get: visible in the viewer, absent
/// from the search index, while the sub-agent's own transcript carries the
/// searchable content.
fn inter_agent_message(
    payload: &Map<String, Value>,
    timestamp: Option<String>,
) -> Option<LogEntry> {
    let text = block_texts(payload.get("content")).join("\n");
    let block = ContentBlock::ToolUse {
        id: string_field(payload, "id").unwrap_or_else(|| "inter_agent".to_owned()),
        name: "inter_agent_message".to_owned(),
        tool: Tool::Other,
        input: json!({
            "author": string_field(payload, "author"),
            "recipient": string_field(payload, "recipient"),
            "message": text,
        }),
    };
    Some(assistant_entry(payload, vec![block], timestamp))
}

fn assistant_entry(
    payload: &Map<String, Value>,
    content: Vec<ContentBlock>,
    timestamp: Option<String>,
) -> LogEntry {
    LogEntry::Assistant {
        agent: Some("Codex".to_owned()),
        message: AssistantMessage {
            role: "assistant".to_owned(),
            content,
            model: None,
            usage: None,
            id: string_field(payload, "id"),
        },
        timestamp,
        uuid: None,
        parent_tool_use_id: None,
    }
}

fn compaction_summary(payload: &Map<String, Value>, timestamp: Option<String>) -> Option<LogEntry> {
    // `replacement_history` is deliberately unread: it restates earlier lines.
    let message = payload.get("message").and_then(Value::as_str)?;
    if message.is_empty() {
        return None;
    }
    Some(LogEntry::PiMetadata {
        label: "Compaction".to_owned(),
        text: message.to_owned(),
        timestamp,
        searchable: false,
        usage: None,
    })
}

fn model_change(
    payload: &Map<String, Value>,
    timestamp: Option<String>,
    last_model: &mut Option<String>,
) -> Option<LogEntry> {
    let model = payload.get("model").and_then(Value::as_str)?;
    if last_model.as_deref() == Some(model) {
        return None;
    }
    *last_model = Some(model.to_owned());
    Some(LogEntry::PiMetadata {
        label: "Model".to_owned(),
        text: model.to_owned(),
        timestamp,
        searchable: false,
        usage: None,
    })
}

/// Usage from a `token_count` event, as an invisible metadata entry.
///
/// The per-event `last_token_usage` values are summed because the running
/// `total_token_usage` cannot be trusted: it resets at compaction and inherits
/// the parent's context in a sub-agent thread — both observed in the corpus.
fn token_usage(payload: &Map<String, Value>) -> Option<LogEntry> {
    if payload.get("type").and_then(Value::as_str) != Some("token_count") {
        return None;
    }
    let last = payload.get("info")?.get("last_token_usage")?.as_object()?;
    let count = |field: &str| last.get(field).and_then(Value::as_u64).unwrap_or(0);
    let (input, cached) = (count("input_tokens"), count("cached_input_tokens"));
    let usage = TokenUsage {
        // Codex's input count includes cache reads; splitting them keeps the
        // total a plain sum of the four fields, as for every other source.
        input_tokens: input.saturating_sub(cached),
        output_tokens: count("output_tokens"),
        cache_creation_input_tokens: count("cache_write_input_tokens"),
        cache_read_input_tokens: cached,
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

fn string_field(payload: &Map<String, Value>, name: &str) -> Option<String> {
    payload.get(name).and_then(Value::as_str).map(str::to_owned)
}

/// Whether a user-role content block is context Codex injected rather than
/// something the user typed.
///
/// Codex wraps every detectable injected fragment in a marker pair — a
/// `<tag>…</tag>` wrapper, a bare placeholder tag, or the AGENTS.md pair — so
/// the test is structural rather than a list of tag names that would go stale.
/// The known cost: a prompt that is one complete XML element and nothing else
/// is misread as context, as is a `<user_shell_command>` record of a command
/// the user ran through the agent.
fn is_injected_context(text: &str) -> bool {
    let text = text.trim();
    if text.starts_with("# AGENTS.md instructions") && text.ends_with("</INSTRUCTIONS>") {
        return true;
    }
    let Some(rest) = text.strip_prefix('<') else {
        return false;
    };
    let Some(end) = rest.find('>') else {
        return false;
    };
    let name = &rest[..end];
    if name.is_empty() || name.starts_with('/') {
        return false;
    }
    // A lone tag with nothing after it is a placeholder such as
    // `<no retained transcript delta entries>`.
    if rest[end + 1..].trim().is_empty() {
        return true;
    }
    text.ends_with(&format!("</{name}>"))
}

/// A rollout filename, split into the fields the name encodes:
/// `rollout-<YYYY-MM-DDThh-mm-ss>-<thread_id>.jsonl`, with `_<rollout_id>`
/// spliced in before the extension when the thread was reverted and rewritten.
/// Mirrors Codex's own parser.
///
/// The timestamp digits and the UUIDv7 rollout id both order lexically, so
/// "which file is newest" is a plain string comparison.
pub(crate) struct RolloutFileName<'a> {
    pub(crate) thread_id: &'a str,
    pub(crate) timestamp: &'a str,
    pub(crate) rollout_id: &'a str,
}

impl<'a> RolloutFileName<'a> {
    pub(crate) fn parse(name: &'a str) -> Option<Self> {
        let core = name.strip_prefix("rollout-")?.strip_suffix(".jsonl")?;
        let timestamp = core.get(..19)?;
        if !is_rollout_timestamp(timestamp) || core.get(19..20)? != "-" {
            return None;
        }
        let ids = core.get(20..)?;
        let (thread_id, rollout_id) = ids.split_once('_').unwrap_or((ids, ids));
        (is_uuid(thread_id) && is_uuid(rollout_id)).then_some(Self {
            thread_id,
            timestamp,
            rollout_id,
        })
    }

    pub(crate) fn parse_path(path: &'a Path) -> Option<Self> {
        Self::parse(path.file_name()?.to_str()?)
    }
}

/// `YYYY-MM-DDThh-mm-ss`, the second-resolution stamp rollout names carry.
fn is_rollout_timestamp(text: &str) -> bool {
    let bytes = text.as_bytes();
    bytes.len() == 19
        && bytes.iter().enumerate().all(|(index, byte)| match index {
            4 | 7 | 13 | 16 => *byte == b'-',
            10 => *byte == b'T',
            _ => byte.is_ascii_digit(),
        })
}

fn is_uuid(text: &str) -> bool {
    let bytes = text.as_bytes();
    bytes.len() == 36
        && bytes.iter().enumerate().all(|(index, byte)| match index {
            8 | 13 | 18 | 23 => *byte == b'-',
            _ => byte.is_ascii_hexdigit(),
        })
}

/// A thread's newest rollout seen so far, carrying the name fields that order
/// rollouts of one thread.
struct NewestRollout<'a> {
    timestamp: &'a str,
    rollout_id: &'a str,
    path: &'a Path,
}

impl NewestRollout<'_> {
    /// Name timestamps carry seconds only; the UUIDv7 rollout id breaks the
    /// tie, the same way Codex picks the file it resumes.
    fn supersedes(&self, other: &Self) -> bool {
        (self.timestamp, self.rollout_id) > (other.timestamp, other.rollout_id)
    }
}

/// One file per thread: the newest, chosen the way Codex resolves a resume.
/// An undo leaves several rollouts of a thread on disk, and only the newest is
/// the thread's current content.
pub(crate) fn newest_rollouts_per_thread(files: &[PathBuf]) -> Vec<PathBuf> {
    let mut newest: HashMap<&str, NewestRollout<'_>> = HashMap::new();
    for path in files {
        let Some(name) = RolloutFileName::parse_path(path) else {
            continue;
        };
        let candidate = NewestRollout {
            timestamp: name.timestamp,
            rollout_id: name.rollout_id,
            path,
        };
        match newest.entry(name.thread_id) {
            Entry::Vacant(slot) => {
                slot.insert(candidate);
            }
            Entry::Occupied(mut kept) if candidate.supersedes(kept.get()) => {
                kept.insert(candidate);
            }
            Entry::Occupied(_) => {}
        }
    }
    let mut files = newest
        .into_values()
        .map(|rollout| rollout.path.to_path_buf())
        .collect::<Vec<_>>();
    files.sort();
    files
}

/// Rollouts sit exactly this many directory levels below the `sessions` tree,
/// in a directory per day: `YYYY/MM/DD/rollout-….jsonl`.
pub(crate) const SESSIONS_TREE_DEPTH: usize = 3;

/// The `sessions` tree holding `path`, or `None` when the file sits outside one.
pub(crate) fn sessions_tree_of(path: &Path) -> Option<&Path> {
    path.ancestors()
        .find(|ancestor| ancestor.file_name() == Some(OsStr::new("sessions")))
}

/// `session_index.jsonl` sits beside the `sessions` tree in the Codex home.
pub(crate) fn index_beside_sessions_tree(sessions_tree: &Path) -> Option<PathBuf> {
    sessions_tree
        .parent()
        .map(|home| home.join("session_index.jsonl"))
}

/// The session index of the sessions tree holding `transcript`, or `None` when
/// the file sits outside one.
pub(crate) fn session_index_path(transcript: &Path) -> Option<PathBuf> {
    sessions_tree_of(transcript).and_then(index_beside_sessions_tree)
}

/// The thread's user-visible name. Index records are append-only and the last
/// record for an id wins, so renames never rewrite the rollout itself.
fn thread_title(transcript: &Path, thread_id: &str) -> Option<String> {
    let index = session_index_path(transcript)?;
    index_titles(&index).remove(thread_id)
}

/// Every thread name the session index records, cleaned of line breaks, the
/// newest record per id winning. A missing or unreadable index is an empty map.
pub(crate) fn index_titles(index: &Path) -> HashMap<String, String> {
    let Ok(contents) = std::fs::read_to_string(index) else {
        return HashMap::new();
    };
    let mut titles = HashMap::new();
    for line in contents.lines() {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let Some(id) = value.get("id").and_then(Value::as_str) else {
            continue;
        };
        let Some(name) = value.get("thread_name").and_then(Value::as_str) else {
            continue;
        };
        let name = name.replace(['\r', '\n'], " ").trim().to_owned();
        if name.is_empty() {
            // The newest record has no usable name, so the thread has none.
            titles.remove(id);
        } else {
            titles.insert(id.to_owned(), name);
        }
    }
    titles
}

/// Rollouts written by hand for the tests here and in the provider: the
/// checked-in sub-agent fixture carries fixed ids, and a test tree needs ids
/// of its own.
#[cfg(test)]
pub(crate) mod test_support {
    use std::path::Path;

    /// Writes a rollout of `thread_id` at `path` whose header names
    /// `parent_thread_id` as the thread it ran under, followed by one
    /// assistant message, `sub-agent answer visible`.
    pub(crate) fn write_subagent_rollout(path: &Path, thread_id: &str, parent_thread_id: &str) {
        write_rollout_with_source(path, thread_id, parent_thread_id, None);
    }

    /// [`write_subagent_rollout`] with the header's `source` set to a
    /// Guardian review of `parent_thread_id`, as Codex 0.150 writes it.
    pub(crate) fn write_guardian_rollout(path: &Path, thread_id: &str, parent_thread_id: &str) {
        write_rollout_with_source(
            path,
            thread_id,
            parent_thread_id,
            Some(r#"{"subagent":{"other":"guardian"}}"#),
        );
    }

    fn write_rollout_with_source(
        path: &Path,
        thread_id: &str,
        parent_thread_id: &str,
        source: Option<&str>,
    ) {
        let source = source
            .map(|source| format!(",\"source\":{source}"))
            .unwrap_or_default();
        std::fs::write(
            path,
            format!(
                concat!(
                    "{{\"timestamp\":\"2026-08-02T10:00:05.000Z\",\"type\":\"session_meta\",",
                    "\"payload\":{{\"id\":\"{id}\",\"timestamp\":\"2026-08-02T10:00:05.000Z\",",
                    "\"cwd\":\"/tmp/project\",\"parent_thread_id\":\"{parent}\"{source}}}}}\n",
                    "{{\"timestamp\":\"2026-08-02T10:00:06.000Z\",\"type\":\"response_item\",",
                    "\"payload\":{{\"type\":\"message\",\"role\":\"assistant\",",
                    "\"content\":[{{\"type\":\"output_text\",\"text\":\"sub-agent answer visible\"}}]}}}}\n",
                ),
                id = thread_id,
                parent = parent_thread_id,
                source = source,
            ),
        )
        .unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::parser::process_conversation_file;

    fn fixture(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/codex")
            .join(name)
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
    fn projects_the_dialogue_and_drops_model_context() {
        let projection = CODEX_ROLLOUT
            .parse_transcript(&fixture("rollout.jsonl"))
            .unwrap()
            .unwrap();

        assert_eq!(projection.source, Source::Codex);
        assert_eq!(projection.header.id, "019f0000-0000-7000-8000-00000000000a");
        assert_eq!(projection.header.cwd, PathBuf::from("/tmp/project"));
        assert_eq!(projection.title, None);
        assert_eq!(projection.malformed_lines, vec![17]);

        let json = entry_json(&projection);
        for kept in [
            "active codex question",
            "codex answer searchable",
            "reasoning summary visible",
            "tool output searchable",
            "function output searchable",
            "TOOL_INPUT_VISIBLE",
            "INTER_AGENT_TASK_TEXT",
            "compaction summary text",
            "gpt-5.2-test",
        ] {
            assert!(json.contains(kept), "missing {kept:?}");
        }
        for dropped in [
            "BASE_INSTRUCTIONS_SENTINEL",
            "ENV_CONTEXT_SENTINEL",
            "AGENTS_MD_SENTINEL",
            "no retained transcript",
            "DEVELOPER_SENTINEL",
            "ENCRYPTED_SENTINEL",
            "EVENT_DUPLICATE_SENTINEL",
            "REPLACEMENT_SENTINEL",
            "WORLD_STATE_SENTINEL",
        ] {
            assert!(!json.contains(dropped), "leaked {dropped:?}");
        }
    }

    #[test]
    fn extracts_codex_conversation_metadata() {
        let conversation = process_conversation_file(fixture("rollout.jsonl"), None, None)
            .unwrap()
            .unwrap();

        assert_eq!(conversation.source, Source::Codex);
        assert_eq!(
            conversation.session_id,
            "019f0000-0000-7000-8000-00000000000a"
        );
        assert_eq!(conversation.model.as_deref(), Some("gpt-5.2-test"));
        // Two token_count events: (1100-1000)+1000+7+40 and 200+0+0+30.
        assert_eq!(conversation.total_tokens, 1377);
        assert_eq!(conversation.duration_minutes, Some(2));
        assert!(conversation.preview.starts_with("active codex question"));
        for searchable in ["active codex question", "tool output searchable"] {
            assert!(
                conversation.full_text.contains(searchable),
                "missing {searchable:?} from {:?}",
                conversation.full_text
            );
        }
        for hidden in [
            "compaction summary text",
            "INTER_AGENT_TASK_TEXT",
            "reasoning summary visible",
            "ENV_CONTEXT_SENTINEL",
        ] {
            assert!(
                !conversation.full_text.contains(hidden),
                "{hidden:?} does not belong in the search index"
            );
        }
    }

    /// The inherited prefix is the parent's history; indexing it would count
    /// the parent's text and tokens twice after the thread folds into it.
    #[test]
    fn a_sub_agent_rollout_keeps_only_its_own_history() {
        let projection = CODEX_ROLLOUT
            .parse_transcript(&fixture("subagent.jsonl"))
            .unwrap()
            .unwrap();

        assert_eq!(projection.header.id, "019f0000-0000-7000-8000-00000000000b");
        let json = entry_json(&projection);
        assert!(json.contains("child task question"));
        assert!(json.contains("child answer searchable"));
        assert!(!json.contains("INHERITED_PARENT_SENTINEL"));
        assert!(!json.contains("INHERITED_ANSWER_SENTINEL"));
        assert!(!json.contains("DEVELOPER_TASK_SENTINEL"));
        assert_eq!(
            json.matches("gpt-5.2-test").count(),
            1,
            "the copied context named the same model; the thread's own first turn_context is still a model change"
        );
    }

    /// Codex's legacy-to-paginated migration sets the boundary past the last
    /// record of every sub-agent rollout it rewrites; nothing follows it, so
    /// the whole file is the thread's own history, usage included.
    #[test]
    fn a_migrated_sub_agent_rollout_keeps_its_whole_history() {
        let projection = CODEX_ROLLOUT
            .parse_transcript(&fixture("subagent-migrated.jsonl"))
            .unwrap()
            .unwrap();

        assert_eq!(projection.header.id, MIGRATED_SUBAGENT_THREAD);
        let json = entry_json(&projection);
        assert!(json.contains("migrated task question"));
        assert!(json.contains("migrated answer searchable"));
        assert!(json.contains("\"input_tokens\":300"));
        assert!(!json.contains("PAGINATED_EVENT_SENTINEL"));
    }

    const PARENT_THREAD: &str = "019f0000-0000-7000-8000-00000000000a";
    const SUBAGENT_THREAD: &str = "019f0000-0000-7000-8000-00000000000b";
    const MIGRATED_SUBAGENT_THREAD: &str = "019f0000-0000-7000-8000-00000000000e";

    fn kind_of(name: &str) -> ThreadKind {
        thread_kind_of(&fixture(name)).unwrap()
    }

    #[test]
    fn a_header_naming_no_parent_records_a_session() {
        assert_eq!(kind_of("rollout.jsonl"), ThreadKind::Session);
    }

    /// The checked-in sub-agent fixtures predate the `source` field; the
    /// parent link alone makes them sub-agents.
    #[test]
    fn a_header_naming_a_parent_records_a_sub_agent() {
        for name in ["subagent.jsonl", "subagent-migrated.jsonl"] {
            assert_eq!(
                kind_of(name),
                ThreadKind::Subagent {
                    parent_thread_id: PARENT_THREAD.to_owned()
                },
                "{name}"
            );
        }
    }

    /// A Guardian review names the thread it reviewed as its parent, and its
    /// transcript restates that thread's conversation. Reading it as a
    /// sub-agent would count the parent's text and tokens twice.
    #[test]
    fn a_guardian_review_header_is_skipped() {
        assert_eq!(
            kind_of("guardian.jsonl"),
            ThreadKind::Skipped {
                parent_thread_id: Some(PARENT_THREAD.to_owned())
            }
        );
    }

    #[test]
    fn every_source_codex_runs_for_itself_is_skipped() {
        let kind = |source: Value| {
            let mut payload = json!({
                "id": SUBAGENT_THREAD,
                "timestamp": "2026-08-02T10:00:00.000Z",
                "cwd": "/tmp/project",
                "parent_thread_id": PARENT_THREAD,
            });
            payload["source"] = source;
            thread_kind(payload.as_object().unwrap())
        };

        for skipped in [
            json!({"subagent": {"other": "guardian"}}),
            json!({"subagent": "review"}),
            json!({"subagent": "compact"}),
            json!({"subagent": "memory_consolidation"}),
            json!({"internal": "guardian"}),
        ] {
            assert_eq!(
                kind(skipped.clone()),
                ThreadKind::Skipped {
                    parent_thread_id: Some(PARENT_THREAD.to_owned())
                },
                "{skipped}"
            );
        }
        for subagent_source in [
            json!({"subagent": {"thread_spawn": {"parent_thread_id": PARENT_THREAD, "depth": 1}}}),
            json!({"subagent": "thread_spawn"}),
        ] {
            assert_eq!(
                kind(subagent_source.clone()),
                ThreadKind::Subagent {
                    parent_thread_id: PARENT_THREAD.to_owned()
                },
                "{subagent_source}"
            );
        }
        assert_eq!(
            kind(json!("cli")),
            ThreadKind::Subagent {
                parent_thread_id: PARENT_THREAD.to_owned()
            },
            "an interactive source with a parent link is still a sub-agent"
        );
    }

    /// The state database records a sub-agent's parent as an edge, not in
    /// its `source`; a `thread_spawn` row no edge names is a session.
    #[test]
    fn a_source_with_no_parent_recorded_is_a_session_unless_skipped() {
        let source = json!({"subagent": {"thread_spawn": {"parent_thread_id": PARENT_THREAD}}});
        assert_eq!(
            thread_kind_of_source(Some(&source), None),
            ThreadKind::Session
        );
        assert_eq!(
            thread_kind_of_source(Some(&json!({"subagent": {"other": "guardian"}})), None),
            ThreadKind::Skipped {
                parent_thread_id: None
            }
        );
        assert_eq!(thread_kind_of_source(None, None), ThreadKind::Session);
    }

    #[test]
    fn a_file_that_is_not_a_rollout_has_no_thread_kind() {
        let pi =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/pi/v3-branched.jsonl");
        assert!(thread_kind_of(&pi).is_none());
        assert!(thread_kind_of(Path::new("/absent/rollout.jsonl")).is_none());
    }

    /// The parent rollout and the sub-agent fixture, copied into a tree.
    fn family_tree(home: &Path) -> (PathBuf, PathBuf) {
        let day = home.join("sessions/2026/08/01");
        std::fs::create_dir_all(&day).unwrap();
        let parent = day.join(format!("rollout-2026-08-01T10-00-00-{PARENT_THREAD}.jsonl"));
        std::fs::copy(fixture("rollout.jsonl"), &parent).unwrap();
        let subagent = day.join(format!(
            "rollout-2026-08-02T10-00-00-{SUBAGENT_THREAD}.jsonl"
        ));
        std::fs::copy(fixture("subagent.jsonl"), &subagent).unwrap();
        (parent, subagent)
    }

    #[test]
    fn the_view_splices_sub_agent_threads_in_as_progress_entries() {
        let home = tempfile::tempdir().unwrap();
        let (parent, subagent) = family_tree(home.path());

        let indexed = CODEX_ROLLOUT.parse_transcript(&parent).unwrap().unwrap();
        assert!(
            !entry_json(&indexed).contains("child answer searchable"),
            "the plain parse must not splice; the row merges whole threads instead"
        );

        let view = super::super::view_projection(&CODEX_ROLLOUT, &parent, &[subagent])
            .unwrap()
            .unwrap();
        let json = entry_json(&view);
        assert!(json.contains("child task question"));
        assert!(json.contains("child answer searchable"));
        assert!(
            !json.contains("INHERITED_PARENT_SENTINEL"),
            "a thread's inherited prefix is the parent's own history"
        );

        let (_, last) = view.entries.last().unwrap();
        let LogEntry::Progress { data, .. } = last else {
            panic!("child entries carry later timestamps, so they splice at the end: {last:?}");
        };
        let progress = crate::log_entry::parse_agent_progress(data)
            .expect("a spliced entry is shaped as Claude's agent_progress");
        assert_eq!(progress.agent_id, SUBAGENT_THREAD);
    }

    #[test]
    fn a_migrated_sub_agent_thread_splices_into_the_view() {
        let home = tempfile::tempdir().unwrap();
        let (parent, _) = family_tree(home.path());
        let migrated = home.path().join("sessions/2026/08/01").join(format!(
            "rollout-2026-08-02T11-00-00-{MIGRATED_SUBAGENT_THREAD}.jsonl"
        ));
        std::fs::copy(fixture("subagent-migrated.jsonl"), &migrated).unwrap();

        let view = super::super::view_projection(&CODEX_ROLLOUT, &parent, &[migrated])
            .unwrap()
            .unwrap();

        assert!(entry_json(&view).contains("migrated answer searchable"));
    }

    /// The row names nested threads with the rest, flattened; the view
    /// splices every one it is handed.
    #[test]
    fn a_sub_agent_of_a_sub_agent_splices_into_the_root_view() {
        let home = tempfile::tempdir().unwrap();
        let (parent, subagent) = family_tree(home.path());
        let nested = "019f0000-0000-7000-8000-00000000000d";
        let nested_rollout = home
            .path()
            .join("sessions/2026/08/01")
            .join(format!("rollout-2026-08-02T10-00-05-{nested}.jsonl"));
        test_support::write_subagent_rollout(&nested_rollout, nested, SUBAGENT_THREAD);

        let view =
            super::super::view_projection(&CODEX_ROLLOUT, &parent, &[subagent, nested_rollout])
                .unwrap()
                .unwrap();

        assert!(entry_json(&view).contains("sub-agent answer visible"));
        assert!(entry_json(&view).contains("child answer searchable"));
    }

    #[test]
    fn the_view_without_sub_agents_is_the_plain_parse() {
        let view = super::super::view_projection(&CODEX_ROLLOUT, &fixture("rollout.jsonl"), &[])
            .unwrap()
            .unwrap();

        assert!(entry_json(&view).contains("active codex question"));
        assert!(!entry_json(&view).contains("agent_progress"));
    }

    #[test]
    fn a_file_that_is_not_a_rollout_is_not_recognized() {
        let pi =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/pi/v3-branched.jsonl");
        assert!(CODEX_ROLLOUT.parse_transcript(&pi).unwrap().is_none());

        let directory = tempfile::tempdir().unwrap();
        let claude = directory.path().join("claude.jsonl");
        std::fs::write(&claude, "{\"type\":\"user\"}\n").unwrap();
        assert!(CODEX_ROLLOUT.parse_transcript(&claude).unwrap().is_none());
    }

    #[test]
    fn a_thread_named_in_the_session_index_gets_its_newest_name() {
        let home = tempfile::tempdir().unwrap();
        let day = home.path().join("sessions/2026/08/01");
        std::fs::create_dir_all(&day).unwrap();
        let transcript =
            day.join("rollout-2026-08-01T10-00-00-019f0000-0000-7000-8000-00000000000a.jsonl");
        std::fs::copy(fixture("rollout.jsonl"), &transcript).unwrap();
        std::fs::write(
            home.path().join("session_index.jsonl"),
            concat!(
                "{\"id\":\"019f0000-0000-7000-8000-00000000000a\",\"thread_name\":\"old name\",\"updated_at\":\"2026-08-01T11:00:00Z\"}\n",
                "{\"id\":\"019f0000-0000-7000-8000-00000000000f\",\"thread_name\":\"someone else\",\"updated_at\":\"2026-08-01T11:30:00Z\"}\n",
                "{\"id\":\"019f0000-0000-7000-8000-00000000000a\",\"thread_name\":\"newest\\nname\",\"updated_at\":\"2026-08-01T12:00:00Z\"}\n",
            ),
        )
        .unwrap();

        let projection = CODEX_ROLLOUT
            .parse_transcript(&transcript)
            .unwrap()
            .unwrap();

        assert_eq!(projection.title.as_deref(), Some("newest name"));
        assert!(matches!(
            &projection.entries[0].1,
            LogEntry::CustomTitle { custom_title } if custom_title == "newest name"
        ));
    }

    #[test]
    fn a_standalone_result_keeps_the_tool_it_names() {
        let projection = CODEX_ROLLOUT
            .parse_transcript(&fixture("rollout.jsonl"))
            .unwrap()
            .unwrap();

        let named = tool_results(&projection)
            .into_iter()
            .filter(|result| result.standalone_tool_name.is_some())
            .collect::<Vec<_>>();

        let [received] = named.as_slice() else {
            panic!("expected the one standalone result the fixture records: {named:?}");
        };
        assert_eq!(
            received.standalone_tool_name.as_deref(),
            Some("send_message_to_thread")
        );
        assert_eq!(
            received.tool_use_id, "fco_standalone",
            "with no call to answer, the result takes the item's own id"
        );
        assert_eq!(received.text.as_deref(), Some("delegated task searchable"));
    }

    /// One tool result of a projection, as the reader built it.
    #[derive(Debug)]
    struct ProjectedResult {
        tool_use_id: String,
        standalone_tool_name: Option<String>,
        text: Option<String>,
    }

    /// Every tool result the projection holds, in order.
    fn tool_results(projection: &SessionProjection) -> Vec<ProjectedResult> {
        projection
            .entries
            .iter()
            .filter_map(|(_, entry)| match entry {
                LogEntry::User { message, .. } => match &message.content {
                    UserContent::Blocks(blocks) => Some(blocks),
                    UserContent::String(_) => None,
                },
                _ => None,
            })
            .flatten()
            .filter_map(|block| match block {
                ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    standalone_tool_name,
                } => Some(ProjectedResult {
                    tool_use_id: tool_use_id.clone(),
                    standalone_tool_name: standalone_tool_name.clone(),
                    text: content.as_ref().and_then(Value::as_str).map(str::to_owned),
                }),
                _ => None,
            })
            .collect()
    }

    /// A result that answers a call carries no name of its own: the call above
    /// it names the tool.
    #[test]
    fn a_paired_result_names_no_tool() {
        let projection = CODEX_ROLLOUT
            .parse_transcript(&fixture("rollout.jsonl"))
            .unwrap()
            .unwrap();

        let paired = tool_results(&projection)
            .into_iter()
            .find(|result| result.tool_use_id == "call_1")
            .expect("the fixture holds a result answering call_1");

        assert_eq!(paired.standalone_tool_name, None, "{paired:?}");
    }

    /// Codex wraps a command the user ran in one XML element, which
    /// `is_injected_context` would otherwise drop as context Codex injected.
    #[test]
    fn a_user_run_command_reads_as_a_call_and_its_result() {
        let projection = CODEX_ROLLOUT
            .parse_transcript(&fixture("rollout.jsonl"))
            .unwrap()
            .unwrap();

        let commands = projection
            .entries
            .iter()
            .filter_map(ProjectedCommand::of)
            .collect::<Vec<_>>();

        let [succeeded, failed, printed_closing_tag] = commands.as_slice() else {
            panic!("expected the three commands the fixture records: {commands:?}");
        };

        assert_eq!(
            succeeded.call_id, "msg_shell_ok",
            "the call uses the message id"
        );
        assert_eq!(
            succeeded.result_id, succeeded.call_id,
            "the result answers that call"
        );
        assert_eq!(succeeded.command, "wc -l README.md");
        assert_eq!(succeeded.result, "shell output searchable");
        assert_eq!(
            succeeded.timestamp.as_deref(),
            Some("2026-08-01T10:02:05.000Z"),
            "the entry opens a run, which is stamped from the record"
        );

        assert_eq!(failed.command, "wc -l missing");
        assert_eq!(
            failed.result, "wc: no such file\nExit code: 1",
            "a failed command names its exit code, with no terminal styling"
        );

        assert_eq!(
            printed_closing_tag.command, r#"echo x; echo "</result>""#,
            "a command that prints the wrapper's closing tag is read whole"
        );
        assert_eq!(printed_closing_tag.result, "x\n</result>");
    }

    /// One command the user ran, as the projection records it.
    #[derive(Debug)]
    struct ProjectedCommand {
        call_id: String,
        result_id: String,
        command: String,
        result: String,
        timestamp: Option<String>,
    }

    impl ProjectedCommand {
        fn of((_, entry): &(usize, LogEntry)) -> Option<Self> {
            let LogEntry::User {
                message, timestamp, ..
            } = entry
            else {
                return None;
            };
            let UserContent::Blocks(blocks) = &message.content else {
                return None;
            };
            let [
                ContentBlock::ToolUse {
                    id, tool, input, ..
                },
                ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    ..
                },
            ] = blocks.as_slice()
            else {
                return None;
            };
            (*tool == Tool::UserShell).then(|| Self {
                call_id: id.clone(),
                result_id: tool_use_id.clone(),
                command: input["command"]
                    .as_str()
                    .expect("the call carries its command")
                    .to_owned(),
                result: content
                    .as_ref()
                    .and_then(Value::as_str)
                    .expect("the result carries the output")
                    .to_owned(),
                timestamp: timestamp.clone(),
            })
        }
    }

    /// Fills in the wrapper Codex writes for one command.
    fn wrapper(command: &str, exit_code: &str, output: &str) -> String {
        format!(
            "<user_shell_command>\n<command>\n{command}\n</command>\n<result>\n\
             Exit code: {exit_code}\nDuration: 0.1 seconds\nOutput:\n{output}\n\
             </result>\n</user_shell_command>"
        )
    }

    /// A wrapper the anchors do not fit is left to `is_injected_context`, which
    /// drops it — a missing row rather than a wrong one.
    #[test]
    fn a_wrapper_that_fails_an_anchor_is_not_read_as_a_command() {
        for unreadable in [
            "plain question".to_owned(),
            "<user_instructions>be brief</user_instructions>".to_owned(),
            wrapper("ls", "", "out"),
            wrapper("ls", "not a number", "out"),
            wrapper("ls", "0", "out").replace("\n</result>\n</user_shell_command>", ""),
        ] {
            assert!(
                UserShellCommand::parse(&unreadable).is_none(),
                "{unreadable:?}"
            );
        }
    }

    #[test]
    fn a_wrapper_is_read_through_the_whitespace_around_it() {
        let padded = format!("\n{}\n", wrapper("ls", "0", "out"));

        let command = UserShellCommand::parse(&padded).expect("the anchors sit inside the padding");

        assert_eq!(command.command, "ls");
        assert_eq!(command.result_text(), "out");
    }

    #[test]
    fn a_command_that_printed_nothing_keeps_its_exit_line() {
        let silent = wrapper("false", "1", "").replace("\nOutput:\n\n", "\n");

        let command = UserShellCommand::parse(&silent).expect("the wrapper still fits the anchors");

        assert_eq!(
            command.result_text(),
            "Exit code: 1",
            "with no output the exit line opens the result rather than following a blank row"
        );
    }

    #[test]
    fn injected_context_is_recognized_structurally() {
        for injected in [
            "<environment_context>\n<cwd>/tmp</cwd>\n</environment_context>",
            "<user_instructions>be brief</user_instructions>",
            "<no retained transcript delta entries>",
            "# AGENTS.md instructions for /tmp\n\n<INSTRUCTIONS>\nrules\n</INSTRUCTIONS>",
        ] {
            assert!(is_injected_context(injected), "{injected}");
        }
        for typed_by_a_user in [
            "why does <b>bold</b> not render?",
            "look at <this",
            "</closing> only",
            "plain question",
        ] {
            assert!(!is_injected_context(typed_by_a_user), "{typed_by_a_user}");
        }
    }
}
