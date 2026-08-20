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

use super::{
    SessionFormat, SessionHeader, SessionProjection, block_texts, progress_entries,
    splice_by_timestamp,
};
use crate::agent::transcript::bounded_tool_result_text;
use crate::claude::{
    AssistantMessage, ContentBlock, LogEntry, TokenUsage, UserContent, UserMessage,
};
use crate::error::Result;
use crate::history::Source;
use chrono::{DateTime, SecondsFormat, Utc};
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

pub struct KimiWireFormat;

pub static KIMI_WIRE: KimiWireFormat = KimiWireFormat;

impl SessionFormat for KimiWireFormat {
    fn parse_transcript(&self, path: &Path) -> Result<Option<SessionProjection>> {
        let file = File::open(path)?;
        let Some(wire) = parse_reader(BufReader::new(file))? else {
            return Ok(None);
        };
        let location = wire_location(path);
        let state = session_state(&location.session_dir);
        let session_id = directory_name(&location.session_dir);

        let (id, parent_session_id) = if state.agent_is_main(&location.agent_id) {
            (session_id, None)
        } else {
            let parent = match state.parent_agent_of(&location.agent_id) {
                Some(parent) => format!("{session_id}#{parent}"),
                None => session_id.clone(),
            };
            (format!("{session_id}#{}", location.agent_id), Some(parent))
        };

        let mut entries = wire.entries;
        let mut title = None;
        // The title belongs to the session, so only its main wire carries it;
        // a sub-agent's row folds away before a title could matter.
        if parent_session_id.is_none()
            && let Some(name) = state.title.clone()
        {
            let entry = if state.title_is_custom {
                LogEntry::CustomTitle {
                    custom_title: name.clone(),
                }
            } else {
                LogEntry::AiTitle {
                    ai_title: name.clone(),
                }
            };
            entries.insert(0, (1, entry));
            title = Some(name);
        }

        Ok(Some(SessionProjection {
            source: Source::Kimi,
            parent_session_id,
            header: SessionHeader {
                version: 1,
                id,
                timestamp: wire
                    .created_at_ms
                    .or(state.created_at_ms)
                    .and_then(rfc3339_from_millis)
                    .unwrap_or_default(),
                cwd: state.cwd.unwrap_or_default(),
            },
            title,
            entries,
            leaf_id: None,
            malformed_lines: wire.malformed_lines,
        }))
    }

    /// Kimi records each sub-agent as a wire of its own inside the same session
    /// directory, so the view of the main wire splices every sibling agent's
    /// dialogue in as `Progress` entries, ordered by timestamp. The view of a
    /// sub-agent's wire is its plain parse: the thread folds into the session
    /// at load and is read through it.
    fn parse_transcript_view(&self, path: &Path) -> Result<Option<SessionProjection>> {
        let Some(mut projection) = self.parse_transcript(path)? else {
            return Ok(None);
        };
        if projection.parent_session_id.is_some() {
            return Ok(Some(projection));
        }
        let threads = sibling_agent_wires(path);
        projection.entries = splice_by_timestamp(projection.entries, progress_entries(threads));
        Ok(Some(projection))
    }
}

/// Every other agent's wire in the session directory holding `path`, parsed in
/// full and labelled with its agent directory name.
fn sibling_agent_wires(path: &Path) -> Vec<(String, SessionProjection)> {
    let location = wire_location(path);
    let state = session_state(&location.session_dir);
    let Ok(agent_directories) = std::fs::read_dir(location.session_dir.join("agents")) else {
        return Vec::new();
    };
    let mut threads = Vec::new();
    for entry in agent_directories.flatten() {
        let agent_id = entry.file_name().to_string_lossy().into_owned();
        if agent_id == location.agent_id || state.agent_is_main(&agent_id) {
            continue;
        }
        if let Ok(Some(thread)) = KIMI_WIRE.parse_transcript(&entry.path().join("wire.jsonl")) {
            threads.push((agent_id, thread));
        }
    }
    // Directory enumeration order varies by platform; agent ids settle ties
    // the timestamp sort leaves.
    threads.sort_by(|left, right| left.0.cmp(&right.0));
    threads
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
    parent_agent_id: Option<String>,
}

impl SessionState {
    /// An agent absent from the state is judged by its name, so a session with
    /// no readable state still keeps its main wire on top.
    fn agent_is_main(&self, agent_id: &str) -> bool {
        match self.agents.get(agent_id) {
            Some(agent) => agent.is_main,
            None => agent_id == "main",
        }
    }

    /// The sub-agent's parent within the session, or `None` when the parent is
    /// the main agent — also the fallback when the state names no parent, so a
    /// thread with incomplete state folds into the session rather than
    /// surfacing as a row.
    fn parent_agent_of(&self, agent_id: &str) -> Option<String> {
        self.agents
            .get(agent_id)?
            .parent_agent_id
            .clone()
            .filter(|parent| !self.agent_is_main(parent))
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
                            parent_agent_id: agent
                                .get("parentAgentId")
                                .and_then(Value::as_str)
                                .map(str::to_owned),
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
    created_at_ms: Option<i64>,
    entries: Vec<(usize, LogEntry)>,
    malformed_lines: Vec<usize>,
}

fn parse_reader(mut reader: impl BufRead) -> Result<Option<ParsedWire>> {
    let mut line = String::new();
    let mut line_number = 0usize;

    // The first line decides whether the file is a wire log at all.
    let created_at_ms = loop {
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
        let Some(object) = value.as_object() else {
            return Ok(None);
        };
        if object.get("type").and_then(Value::as_str) != Some("metadata")
            || !object.contains_key("protocol_version")
        {
            return Ok(None);
        }
        break object.get("created_at").and_then(Value::as_i64);
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
        if object.get("type").and_then(Value::as_str) == Some("usage.record") {
            for entry in usage_record_entries(object, &mut last_model) {
                entries.push((line_number, entry));
            }
        } else if let Some(entry) = normalize_line(object) {
            entries.push((line_number, entry));
        }
    }

    Ok(Some(ParsedWire {
        created_at_ms,
        entries,
        malformed_lines,
    }))
}

fn normalize_line(object: &Map<String, Value>) -> Option<LogEntry> {
    let timestamp = entry_time(object);
    match object.get("type").and_then(Value::as_str)? {
        // `turn.prompt` and `turn.steer` restate what `context.append_message`
        // already recorded, so reading them too would double every prompt.
        "context.append_message" => user_message(object, timestamp),
        "context.append_loop_event" => loop_event(object.get("event")?.as_object()?, timestamp),
        "context.apply_compaction" => compaction_summary(object, timestamp),
        _ => None,
    }
}

fn user_message(object: &Map<String, Value>, timestamp: Option<String>) -> Option<LogEntry> {
    let message = object.get("message")?.as_object()?;
    if message.get("role").and_then(Value::as_str) != Some("user") {
        return None;
    }
    let texts = block_texts(message.get("content"))
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

fn loop_event(event: &Map<String, Value>, timestamp: Option<String>) -> Option<LogEntry> {
    match event.get("type").and_then(Value::as_str)? {
        "content.part" => content_part(event, timestamp),
        "tool.call" => tool_call(event, timestamp),
        "tool.result" => tool_result(event, timestamp),
        // `step.begin` / `step.end` are request framing; `usage.record`
        // carries the tokens.
        _ => None,
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
    Some(assistant_entry(event, vec![block], timestamp))
}

fn tool_call(event: &Map<String, Value>, timestamp: Option<String>) -> Option<LogEntry> {
    let block = ContentBlock::ToolUse {
        id: string_field(event, "toolCallId").unwrap_or_else(|| "unknown".to_owned()),
        name: string_field(event, "name").unwrap_or_else(|| "unknown".to_owned()),
        input: event.get("args").cloned().unwrap_or_else(|| json!({})),
    };
    Some(assistant_entry(event, vec![block], timestamp))
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
    let text = bounded_tool_result_text(&json!(text)).unwrap_or_default();
    Some(LogEntry::User {
        message: UserMessage {
            role: "user".to_owned(),
            content: UserContent::Blocks(vec![ContentBlock::ToolResult {
                tool_use_id: string_field(event, "toolCallId")
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

fn assistant_entry(
    event: &Map<String, Value>,
    content: Vec<ContentBlock>,
    timestamp: Option<String>,
) -> LogEntry {
    LogEntry::Assistant {
        agent: Some("Kimi".to_owned()),
        message: AssistantMessage {
            role: "assistant".to_owned(),
            content,
            model: None,
            usage: None,
            id: string_field(event, "uuid"),
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

/// Whether a user-role content block is context Kimi injected rather than
/// something the user typed. Kimi wraps every injected fragment — plan-mode
/// instructions, task reminders — in a `<system-reminder>` element.
fn is_injected_context(text: &str) -> bool {
    let text = text.trim();
    text.starts_with("<system-reminder>") && text.ends_with("</system-reminder>")
}

/// Wire events stamp milliseconds since the epoch; entries carry RFC 3339 so
/// timestamps compare and render like every other provider's.
fn entry_time(object: &Map<String, Value>) -> Option<String> {
    object
        .get("time")
        .and_then(Value::as_i64)
        .and_then(rfc3339_from_millis)
}

fn rfc3339_from_millis(millis: i64) -> Option<String> {
    DateTime::<Utc>::from_timestamp_millis(millis)
        .map(|timestamp| timestamp.to_rfc3339_opts(SecondsFormat::Millis, true))
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
    use crate::history::parser::process_conversation_file;

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
    fn projects_the_dialogue_and_drops_runtime_noise() {
        let home = tempfile::tempdir().unwrap();
        let wire = family_session(home.path());

        let projection = KIMI_WIRE.parse_transcript(&wire).unwrap().unwrap();

        assert_eq!(projection.source, Source::Kimi);
        assert_eq!(projection.header.id, SESSION);
        assert_eq!(projection.header.cwd, PathBuf::from("/tmp/kimi-project"));
        assert_eq!(projection.header.timestamp, "2026-08-06T10:00:00.000Z");
        assert_eq!(projection.parent_session_id, None);
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
    fn a_sub_agent_wire_carries_the_parent_link_and_no_title() {
        let home = tempfile::tempdir().unwrap();
        let main_wire = family_session(home.path());
        let sub_wire = main_wire
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("agent-0/wire.jsonl");

        let projection = KIMI_WIRE.parse_transcript(&sub_wire).unwrap().unwrap();

        assert_eq!(projection.header.id, format!("{SESSION}#agent-0"));
        assert_eq!(
            projection.parent_session_id.as_deref(),
            Some(SESSION),
            "the parent link is what lets the load loop fold this thread"
        );
        assert_eq!(projection.title, None);
        assert!(entry_json(&projection).contains("kimi child answer searchable"));
    }

    #[test]
    fn the_view_splices_sub_agent_wires_in_as_progress_entries() {
        let home = tempfile::tempdir().unwrap();
        let wire = family_session(home.path());

        let indexed = KIMI_WIRE.parse_transcript(&wire).unwrap().unwrap();
        assert!(
            !entry_json(&indexed).contains("kimi child answer searchable"),
            "the index parse must not splice; the loader folds whole threads instead"
        );

        let view = KIMI_WIRE.parse_transcript_view(&wire).unwrap().unwrap();
        let json = entry_json(&view);
        assert!(json.contains("kimi child task question"));
        assert!(json.contains("kimi child answer searchable"));

        let progress = view
            .entries
            .iter()
            .find_map(|(_, entry)| match entry {
                LogEntry::Progress { data, .. } => crate::claude::parse_agent_progress(data),
                _ => None,
            })
            .expect("a spliced entry is shaped as Claude's agent_progress");
        assert_eq!(progress.agent_id, "agent-0");
    }

    #[test]
    fn the_view_of_a_sub_agent_wire_is_its_plain_parse() {
        let home = tempfile::tempdir().unwrap();
        let main_wire = family_session(home.path());
        let sub_wire = main_wire
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("agent-0/wire.jsonl");

        let view = KIMI_WIRE.parse_transcript_view(&sub_wire).unwrap().unwrap();

        assert!(
            !entry_json(&view).contains("active kimi question"),
            "a sub-agent's view must not pull the session in around it"
        );
    }

    /// The legacy layout keeps one wire at the session directory root; its
    /// state uses `workDir` and predates `titleKind`.
    #[test]
    fn a_legacy_session_reads_its_state_and_counts_as_main() {
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

        let projection = KIMI_WIRE.parse_transcript(&wire).unwrap().unwrap();

        assert_eq!(
            projection.header.id,
            "session_1e000000-0000-4000-8000-000000000002"
        );
        assert_eq!(projection.parent_session_id, None);
        assert_eq!(projection.header.cwd, PathBuf::from("/tmp/legacy-project"));
        assert!(matches!(
            &projection.entries[0].1,
            LogEntry::CustomTitle { custom_title } if custom_title == "chosen by hand"
        ));
    }

    /// A wire without any session directory around it — a fixture, a stray
    /// copy — still parses; it just has no state to name it.
    #[test]
    fn a_wire_outside_a_session_directory_is_the_plain_dialogue() {
        let projection = KIMI_WIRE
            .parse_transcript(&fixture("wire.jsonl"))
            .unwrap()
            .unwrap();

        assert_eq!(projection.header.id, "kimi");
        assert_eq!(projection.title, None);
        assert!(entry_json(&projection).contains("active kimi question"));
    }

    #[test]
    fn a_file_that_is_not_a_kimi_wire_is_not_recognized() {
        for foreign in [
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/pi/v3-branched.jsonl"),
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/codex/rollout.jsonl"),
        ] {
            assert!(
                KIMI_WIRE.parse_transcript(&foreign).unwrap().is_none(),
                "{} is not a Kimi wire",
                foreign.display()
            );
        }

        let directory = tempfile::tempdir().unwrap();
        let claude = directory.path().join("claude.jsonl");
        std::fs::write(&claude, "{\"type\":\"user\"}\n").unwrap();
        assert!(KIMI_WIRE.parse_transcript(&claude).unwrap().is_none());
    }

    #[test]
    fn injected_context_is_recognized_by_its_reminder_wrapper() {
        assert!(is_injected_context(
            "<system-reminder>\nPlan mode is active.\n</system-reminder>"
        ));
        for typed_by_a_user in [
            "what does <system-reminder> mean?",
            "plain question",
            "<b>bold</b>",
        ] {
            assert!(!is_injected_context(typed_by_a_user), "{typed_by_a_user}");
        }
    }
}
