//! The Codex rollout format: JSONL lines of `{timestamp, ordinal?, type, payload}`.
//!
//! A rollout records far more than the dialogue — model context snapshots,
//! security scores, rate-limit telemetry — so this reader is a projection by
//! design: `response_item` lines carry the conversation, `token_count` events
//! carry usage, `turn_context` names the model, and everything else is skipped
//! without being an error. Unknown line types are expected; Codex adds them
//! freely between versions.

mod tools;

use super::splice::{progress_entries, splice_by_timestamp};
use super::{SessionFormat, SessionHeader, SessionProjection, block_texts};
use crate::agent::transcript::bounded_tool_result_text;
use crate::error::Result;
use crate::history::Source;
use crate::history::provider::walk;
use crate::log_entry::{
    AssistantMessage, ContentBlock, LogEntry, TokenUsage, Tool, UserContent, UserMessage,
};
use serde_json::{Map, Value, json};
use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
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

    /// Codex records each sub-agent thread as a rollout file of its own, so the
    /// view reads every sub-agent thread in the sessions tree and splices its
    /// dialogue in as `Progress` entries, ordered by timestamp — synthesizing
    /// what Claude writes natively. Threads splice in full: their tails are as
    /// worth reading and anchoring hits to as anything else.
    ///
    /// The cost is one first-line read per rollout in the tree, paid per view.
    fn parse_transcript_view(&self, path: &Path) -> Result<Option<SessionProjection>> {
        let Some(mut projection) = self.parse_transcript(path)? else {
            return Ok(None);
        };
        let threads = subagent_projections(path, &projection.header.id)
            .into_iter()
            .map(|thread| (thread.header.id.clone(), thread))
            .collect();
        projection.entries = splice_by_timestamp(projection.entries, progress_entries(threads));
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
    parent_session_id: Option<String>,
    own_history_start: Option<u64>,
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
        parent_session_id: field("parent_thread_id"),
        own_history_start: payload
            .get("subagent_history_start_ordinal")
            .and_then(Value::as_u64),
    })
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
        if inherited_from_parent(object, header.own_history_start) {
            continue;
        }
        if let Some(entry) = normalize_line(object, &mut last_model) {
            entries.push((line_number, entry));
        }
    }

    Ok(Some(SessionProjection {
        source: Source::Codex,
        parent_session_id: header.parent_session_id,
        header: SessionHeader {
            version: 1,
            id: header.id,
            timestamp: header.timestamp,
            cwd: PathBuf::from(header.cwd),
        },
        title: None,
        entries,
        leaf_id: None,
        malformed_lines,
    }))
}

/// Lines below `subagent_history_start_ordinal` are the parent's history,
/// copied into a sub-agent's rollout as model context. Indexing them would
/// count the parent's text and tokens twice once the thread folds into it.
fn inherited_from_parent(object: &Map<String, Value>, own_history_start: Option<u64>) -> bool {
    let Some(start) = own_history_start else {
        return false;
    };
    object
        .get("ordinal")
        .and_then(Value::as_u64)
        .is_some_and(|ordinal| ordinal < start)
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
            let texts = block_texts(payload.get("content"))
                .into_iter()
                .filter(|text| !is_injected_context(text))
                .collect::<Vec<_>>();
            if texts.is_empty() {
                return None;
            }
            Some(LogEntry::User {
                message: UserMessage {
                    role: "user".to_owned(),
                    content: UserContent::Blocks(
                        texts
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

fn tool_result(payload: &Map<String, Value>, timestamp: Option<String>) -> Option<LogEntry> {
    // Output is either one string or content blocks typed `input_text`, which
    // the shared bounding helper would skip — so texts are gathered first and
    // only the length bound is delegated.
    let text = match payload.get("output") {
        Some(Value::String(text)) => text.clone(),
        output => block_texts(output).join("\n"),
    };
    let text = bounded_tool_result_text(&json!(text)).unwrap_or_default();
    Some(LogEntry::User {
        message: UserMessage {
            role: "user".to_owned(),
            content: UserContent::Blocks(vec![ContentBlock::ToolResult {
                tool_use_id: string_field(payload, "call_id")
                    .unwrap_or_else(|| "unknown".to_owned()),
                content: Some(json!(text)),
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

/// Every sub-agent thread of `thread_id` in the sessions tree holding
/// `transcript`, nested ones included, parsed in full.
fn subagent_projections(transcript: &Path, thread_id: &str) -> Vec<SessionProjection> {
    let Some(sessions_root) = sessions_tree_of(transcript) else {
        return Vec::new();
    };
    let Ok(rollouts) = walk::jsonl_files_at_depth(sessions_root, SESSIONS_TREE_DEPTH) else {
        return Vec::new();
    };
    subagent_threads(&rollouts, thread_id)
        .into_iter()
        .filter_map(|thread| {
            let rollout = File::open(thread.newest_rollout).ok()?;
            parse_reader(BufReader::new(rollout)).ok().flatten()
        })
        .collect()
}

/// A sub-agent thread and the rollout Codex would resume it from.
pub(crate) struct SubagentThread {
    pub(crate) thread_id: String,
    pub(crate) newest_rollout: PathBuf,
}

/// Every sub-agent thread of `thread_id` among `rollouts`, nested ones
/// included.
///
/// Parentage is read from the first line of each thread's newest rollout —
/// the only way to learn it, since a filename names its own thread but not
/// its parent. A chain that loops back on itself stops at the first repeated
/// id.
pub(crate) fn subagent_threads(rollouts: &[PathBuf], thread_id: &str) -> Vec<SubagentThread> {
    let mut subagents_of: HashMap<String, Vec<SubagentThread>> = HashMap::new();
    for rollout in newest_rollouts_per_thread(rollouts) {
        let Some(name) = RolloutFileName::parse_path(&rollout) else {
            continue;
        };
        if name.thread_id == thread_id {
            continue;
        }
        let Some(parent) = rollout_parent_of(&rollout) else {
            continue;
        };
        let thread = SubagentThread {
            thread_id: name.thread_id.to_owned(),
            newest_rollout: rollout,
        };
        subagents_of.entry(parent).or_default().push(thread);
    }

    let mut visited = HashSet::from([thread_id.to_owned()]);
    let mut frontier = vec![thread_id.to_owned()];
    let mut threads = Vec::new();
    while let Some(id) = frontier.pop() {
        for thread in subagents_of.remove(&id).unwrap_or_default() {
            if !visited.insert(thread.thread_id.clone()) {
                continue;
            }
            frontier.push(thread.thread_id.clone());
            threads.push(thread);
        }
    }
    threads
}

/// The parent thread named by the rollout's first line, read without parsing
/// the rest of the file.
fn rollout_parent_of(path: &Path) -> Option<String> {
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
        return rollout_header(&value)?.parent_session_id;
    }
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
        std::fs::write(
            path,
            format!(
                concat!(
                    "{{\"timestamp\":\"2026-08-02T10:00:05.000Z\",\"type\":\"session_meta\",",
                    "\"payload\":{{\"id\":\"{id}\",\"timestamp\":\"2026-08-02T10:00:05.000Z\",",
                    "\"cwd\":\"/tmp/project\",\"parent_thread_id\":\"{parent}\"}}}}\n",
                    "{{\"timestamp\":\"2026-08-02T10:00:06.000Z\",\"type\":\"response_item\",",
                    "\"payload\":{{\"type\":\"message\",\"role\":\"assistant\",",
                    "\"content\":[{{\"type\":\"output_text\",\"text\":\"sub-agent answer visible\"}}]}}}}\n",
                ),
                id = thread_id,
                parent = parent_thread_id,
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
        assert_eq!(projection.parent_session_id, None);
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
        assert_eq!(
            projection.parent_session_id.as_deref(),
            Some("019f0000-0000-7000-8000-00000000000a"),
            "the parent link is what lets the load loop fold this thread"
        );
        let json = entry_json(&projection);
        assert!(json.contains("child task question"));
        assert!(json.contains("child answer searchable"));
        assert!(!json.contains("INHERITED_PARENT_SENTINEL"));
        assert!(!json.contains("INHERITED_ANSWER_SENTINEL"));
        assert!(!json.contains("DEVELOPER_TASK_SENTINEL"));
    }

    const PARENT_THREAD: &str = "019f0000-0000-7000-8000-00000000000a";
    const SUBAGENT_THREAD: &str = "019f0000-0000-7000-8000-00000000000b";

    /// A sessions tree holding the parent rollout and the sub-agent fixture,
    /// returning the parent's transcript path.
    fn family_tree(home: &Path) -> PathBuf {
        let day = home.join("sessions/2026/08/01");
        std::fs::create_dir_all(&day).unwrap();
        let parent = day.join(format!("rollout-2026-08-01T10-00-00-{PARENT_THREAD}.jsonl"));
        std::fs::copy(fixture("rollout.jsonl"), &parent).unwrap();
        std::fs::copy(
            fixture("subagent.jsonl"),
            day.join(format!(
                "rollout-2026-08-02T10-00-00-{SUBAGENT_THREAD}.jsonl"
            )),
        )
        .unwrap();
        parent
    }

    #[test]
    fn the_view_splices_sub_agent_threads_in_as_progress_entries() {
        let home = tempfile::tempdir().unwrap();
        let parent = family_tree(home.path());

        let indexed = CODEX_ROLLOUT.parse_transcript(&parent).unwrap().unwrap();
        assert!(
            !entry_json(&indexed).contains("child answer searchable"),
            "the index parse must not splice; the loader folds whole threads instead"
        );

        let view = CODEX_ROLLOUT
            .parse_transcript_view(&parent)
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

    /// An undo leaves superseded copies of a thread on disk; splicing one
    /// would repeat the whole thread in the view.
    #[test]
    fn a_superseded_child_rollout_does_not_splice_twice() {
        let home = tempfile::tempdir().unwrap();
        let parent = family_tree(home.path());
        std::fs::copy(
            fixture("subagent.jsonl"),
            home.path().join("sessions/2026/08/01").join(format!(
                "rollout-2026-08-03T10-00-00-{SUBAGENT_THREAD}_019f0000-0000-7000-8000-000000000001.jsonl"
            )),
        )
        .unwrap();

        let view = CODEX_ROLLOUT
            .parse_transcript_view(&parent)
            .unwrap()
            .unwrap();

        assert_eq!(
            entry_json(&view).matches("child answer searchable").count(),
            1
        );
    }

    #[test]
    fn a_sub_agent_of_a_sub_agent_splices_into_the_root_view() {
        let home = tempfile::tempdir().unwrap();
        let parent = family_tree(home.path());
        let nested = "019f0000-0000-7000-8000-00000000000d";
        test_support::write_subagent_rollout(
            &home
                .path()
                .join("sessions/2026/08/01")
                .join(format!("rollout-2026-08-02T10-00-05-{nested}.jsonl")),
            nested,
            SUBAGENT_THREAD,
        );

        let view = CODEX_ROLLOUT
            .parse_transcript_view(&parent)
            .unwrap()
            .unwrap();

        assert!(entry_json(&view).contains("sub-agent answer visible"));
    }

    /// A rollout outside a `sessions` tree — a copied file, a fixture — still
    /// parses; there is simply nowhere to look for its threads.
    #[test]
    fn the_view_of_a_rollout_outside_a_sessions_tree_is_the_plain_parse() {
        let view = CODEX_ROLLOUT
            .parse_transcript_view(&fixture("rollout.jsonl"))
            .unwrap()
            .unwrap();

        assert!(entry_json(&view).contains("active codex question"));
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
