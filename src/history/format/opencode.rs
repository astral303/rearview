//! The OpenCode session store: one SQLite database holding every session's
//! messages and parts as JSON rows.
//!
//! There is no transcript file. A session is addressed by a locator of the
//! form `<database-file>/<session-id>.jsonl` — the session id as a
//! `.jsonl`-suffixed component under the database file itself. Decoding is
//! self-contained: the locator's parent is the database, its stem the session
//! id. No code outside this provider interprets the locator, so the shape
//! only has to satisfy the two residues documented on
//! [`SessionStub`](crate::history::provider::SessionStub): a stable final
//! component, and something honest for `--show-path` to print.
//!
//! The store records far more than the dialogue — request framing, snapshot
//! ids, event queues — so this reader is a projection by design: `text`,
//! `reasoning` and `tool` parts carry the conversation, `step-finish` parts
//! carry usage, and everything else is skipped without being an error.
//! Unknown part types are expected; OpenCode migrates its schema freely
//! between versions, so every query names its columns explicitly.

use super::{SessionFormat, SessionHeader, SessionProjection, rename_key};
use crate::agent::transcript::bounded_tool_result_text;
use crate::error::Result;
use crate::history::Source;
use crate::history::provider::sqlite::{
    DEFAULT_BUSY_TIMEOUT, SESSION_DATABASE_CANNOT_BE_READ, open_session_list, unusable_database,
};
use crate::history::provider::subagents::SubagentForest;
use crate::log_entry::{
    AssistantMessage, ContentBlock, LogEntry, TokenUsage, Tool, UserContent, UserMessage,
};
use chrono::{DateTime, SecondsFormat, Utc};
use rusqlite::Connection;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub struct OpenCodeDbFormat;

pub static OPENCODE_DB: OpenCodeDbFormat = OpenCodeDbFormat;

impl SessionFormat for OpenCodeDbFormat {
    fn parse_transcript(&self, path: &Path) -> Result<Option<SessionProjection>> {
        let Some(reference) = decode_ref(path) else {
            return Ok(None);
        };
        let connection = open_session_list(&reference.database, DEFAULT_BUSY_TIMEOUT)?;
        project_session(&connection, &reference)
    }
}

/// Every OpenCode session id starts with this. The locator codec here and
/// the launcher's stem check both rely on it to tell session stems apart
/// from other files' names.
pub(crate) const SESSION_ID_PREFIX: &str = "ses_";

/// A decoded locator: which database, which session inside it.
pub(crate) struct SessionRef {
    pub(crate) database: PathBuf,
    pub(crate) session_id: String,
}

/// The locator for `session_id` inside `database`.
pub(crate) fn session_ref(database: &Path, session_id: &str) -> PathBuf {
    database.join(format!("{session_id}.jsonl"))
}

/// Decode a locator, or `None` when `path` is not one — a real transcript
/// file's parent is a directory, never a file, so other providers' paths
/// fall through here without being opened.
pub(crate) fn decode_ref(path: &Path) -> Option<SessionRef> {
    if path.extension().and_then(|extension| extension.to_str()) != Some("jsonl") {
        return None;
    }
    let session_id = path.file_stem()?.to_str()?;
    if !session_id.starts_with(SESSION_ID_PREFIX) {
        return None;
    }
    let database = path.parent()?;
    if !database.is_file() {
        return None;
    }
    Some(SessionRef {
        database: database.to_path_buf(),
        session_id: session_id.to_owned(),
    })
}

/// The newest schema migration this reader was developed against. OpenCode
/// journals every migration it applies in a `migration` table, with
/// timestamp-prefixed ids that order lexicographically. Move the pin forward
/// after re-verifying the queries and payload shapes against the migrations
/// beyond it.
pub(crate) const NEWEST_VERIFIED_MIGRATION: &str = "20260622202450_simplify_session_input";

/// The newest migration the database has applied beyond
/// [`NEWEST_VERIFIED_MIGRATION`], or `None` while the reader is current.
///
/// A database without the journal table predates it — older than the pin,
/// not newer — and any failure that matters resurfaces through the content
/// queries, so errors here read as "nothing to report".
pub(crate) fn newest_unverified_migration(connection: &Connection) -> Option<String> {
    connection
        .query_row(
            "SELECT MAX(id) FROM migration WHERE id > ?1",
            [NEWEST_VERIFIED_MIGRATION],
            |row| row.get::<_, Option<String>>(0),
        )
        .ok()
        .flatten()
}

/// The session is its database: a query that fails is the failure the list
/// reports, worded the same way.
fn project_session(
    connection: &Connection,
    reference: &SessionRef,
) -> Result<Option<SessionProjection>> {
    let cannot_be_read = |error: rusqlite::Error| {
        unusable_database(&reference.database, SESSION_DATABASE_CANNOT_BE_READ, &error)
    };
    let Some(session) = session_row(connection, reference).map_err(cannot_be_read)? else {
        return Ok(None);
    };

    let mut parts_by_message = parts_by_message(connection, reference).map_err(cannot_be_read)?;
    let mut sink = EntrySink::default();
    for (message_id, data) in messages(connection, reference).map_err(cannot_be_read)? {
        let parts = parts_by_message.remove(&message_id).unwrap_or_default();
        message_entries(&data, &parts, &mut sink);
    }
    let mut entries = sink.entries;

    // The title belongs to the session, so only a top-level session carries
    // it; a sub-agent row is read through its parent and is not listed. OpenCode
    // autogenerates titles and stores no custom-title marker; they are the
    // session's one display name, so they project as titles, the treatment
    // Pi titles get. Ordinal 0: the title lives in the session row, not
    // among the messages.
    let mut title = None;
    if session.parent_id.is_none() {
        let trimmed = session.title.trim();
        if !trimmed.is_empty() {
            title = Some(trimmed.to_owned());
            entries.insert(
                0,
                (
                    0,
                    LogEntry::CustomTitle {
                        custom_title: trimmed.to_owned(),
                    },
                ),
            );
        }
    }

    Ok(Some(SessionProjection {
        source: Source::OpenCode,
        header: SessionHeader {
            version: 1,
            id: reference.session_id.clone(),
            timestamp: rfc3339_from_millis(session.time_created).unwrap_or_default(),
            cwd: PathBuf::from(session.directory),
            thread_label: None,
        },
        title,
        entries,
        leaf_id: None,
        malformed_lines: Vec::new(),
    }))
}

struct SessionRow {
    parent_id: Option<String>,
    directory: String,
    title: String,
    time_created: i64,
}

fn session_row(
    connection: &Connection,
    reference: &SessionRef,
) -> rusqlite::Result<Option<SessionRow>> {
    let mut statement = connection
        .prepare("SELECT parent_id, directory, title, time_created FROM session WHERE id = ?1")?;
    let mut rows = statement.query([&reference.session_id])?;
    let Some(row) = rows.next()? else {
        return Ok(None);
    };
    Ok(Some(SessionRow {
        parent_id: row.get(0)?,
        directory: row.get(1)?,
        title: row.get(2)?,
        time_created: row.get(3)?,
    }))
}

/// The session's messages in conversation order: `(message id, decoded data)`.
/// A row whose JSON does not parse is skipped rather than failing the session.
fn messages(
    connection: &Connection,
    reference: &SessionRef,
) -> rusqlite::Result<Vec<(String, Value)>> {
    let mut statement = connection
        .prepare("SELECT id, data FROM message WHERE session_id = ?1 ORDER BY time_created, id")?;
    let rows = statement.query_map([&reference.session_id], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut messages = Vec::new();
    for row in rows {
        let (id, data) = row?;
        if let Ok(data) = serde_json::from_str::<Value>(&data) {
            messages.push((id, data));
        }
    }
    Ok(messages)
}

/// Every part of the session, grouped by message, in part order.
fn parts_by_message(
    connection: &Connection,
    reference: &SessionRef,
) -> rusqlite::Result<HashMap<String, Vec<Value>>> {
    let mut statement = connection.prepare(
        "SELECT message_id, data FROM part WHERE session_id = ?1 ORDER BY time_created, id",
    )?;
    let rows = statement.query_map([&reference.session_id], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut parts: HashMap<String, Vec<Value>> = HashMap::new();
    for row in rows {
        let (message_id, data) = row?;
        if let Ok(data) = serde_json::from_str::<Value>(&data) {
            parts.entry(message_id).or_default().push(data);
        }
    }
    Ok(parts)
}

/// The projection under construction, accumulated across messages. Each
/// entry's ordinal stands in for the line number a transcript file would
/// have; the last model named suppresses repeated model markers.
#[derive(Default)]
struct EntrySink {
    entries: Vec<(usize, LogEntry)>,
    last_model: Option<String>,
}

impl EntrySink {
    fn push(&mut self, entry: LogEntry) {
        self.entries.push((self.entries.len() + 1, entry));
    }
}

/// Project one message's parts into `sink`.
///
/// A user message becomes one `User` entry holding all its text blocks — one
/// turn, however many blocks carried it. An assistant message becomes one
/// entry per conversational part, so tool calls and results interleave in
/// part order, plus usage from its `step-finish` parts and a model marker
/// when the message names a model the previous one did not.
fn message_entries(message: &Value, parts: &[Value], sink: &mut EntrySink) {
    let role = message.get("role").and_then(Value::as_str).unwrap_or("");
    let message_timestamp = message
        .pointer("/time/created")
        .and_then(Value::as_i64)
        .and_then(rfc3339_from_millis);

    match role {
        "user" => {
            let texts = parts
                .iter()
                .filter(|part| part.get("type").and_then(Value::as_str) == Some("text"))
                .filter(|part| !is_synthetic(part) || is_comment_note(part))
                .filter_map(|part| part.get("text").and_then(Value::as_str))
                .filter(|text| !text.is_empty())
                .map(str::to_owned)
                .collect::<Vec<_>>();
            if !texts.is_empty() {
                let timestamp = parts
                    .first()
                    .and_then(part_timestamp)
                    .or_else(|| message_timestamp.clone());
                sink.push(LogEntry::User {
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
                });
            }
            injected_read_entries(parts, &message_timestamp, sink);
        }
        "assistant" => {
            if let Some(model) = model_name(message)
                && sink.last_model.as_deref() != Some(model)
            {
                sink.last_model = Some(model.to_owned());
                sink.push(LogEntry::PiMetadata {
                    label: "Model".to_owned(),
                    text: model.to_owned(),
                    timestamp: message_timestamp.clone(),
                    searchable: false,
                    usage: None,
                });
            }
            for part in parts {
                let timestamp = part_timestamp(part).or_else(|| message_timestamp.clone());
                for entry in assistant_part_entries(part, timestamp) {
                    sink.push(entry);
                }
            }
        }
        _ => {}
    }
}

/// A part OpenCode marks `synthetic` is prompt plumbing, not the user's own
/// words. OpenCode itself treats a user message whose parts are all
/// synthetic as not written by the user.
fn is_synthetic(part: &Value) -> bool {
    part.get("synthetic").and_then(Value::as_bool) == Some(true)
}

fn is_synthetic_text(part: &Value) -> bool {
    is_synthetic(part) && part.get("type").and_then(Value::as_str) == Some("text")
}

/// A comment the user made on a file in OpenCode's review UI. Flagged
/// synthetic because the app captures the file and line range, but the
/// comment is the user's own words, and its stored text already reads as
/// prose ("The user made the following comment regarding lines 40 through
/// 68 of errorHandler.ts: …"). It stays in the dialogue.
fn is_comment_note(part: &Value) -> bool {
    part.pointer("/metadata/opencodeComment").is_some()
}

/// Project a user message's synthetic runs as the reads they narrate.
///
/// An @-file mention or MCP resource stores a narration part with the
/// injected contents behind it. Both become a `read` tool call and result:
/// they ride the tools toggle and index bounded, like the same file read by
/// the model through a real tool. A synthetic part narrating no read (a
/// continue reminder, editor context) is framing and yields nothing —
/// except a comment note, which stays in the dialogue as user text.
fn injected_read_entries(
    parts: &[Value],
    message_timestamp: &Option<String>,
    sink: &mut EntrySink,
) {
    let mut index = 0;
    while index < parts.len() {
        let part = &parts[index];
        index += 1;
        if !is_synthetic_text(part) {
            continue;
        }
        let Some(input) = injected_read_input(part) else {
            continue;
        };
        let mut contents = Vec::new();
        while index < parts.len()
            && is_synthetic_text(&parts[index])
            && !is_comment_note(&parts[index])
            && injected_read_input(&parts[index]).is_none()
        {
            if let Some(text) = parts[index].get("text").and_then(Value::as_str) {
                contents.push(text);
            }
            index += 1;
        }
        let part_id = part.get("id").and_then(Value::as_str);
        let call_id = part_id.unwrap_or("injected-read").to_owned();
        let timestamp = part_timestamp(part).or_else(|| message_timestamp.clone());
        sink.push(assistant_entry(
            part_id.map(str::to_owned),
            vec![tool_use_block(call_id.clone(), "read", input)],
            timestamp.clone(),
        ));
        let text = bounded_tool_result_text(&json!(contents.join("\n"))).unwrap_or_default();
        sink.push(LogEntry::User {
            message: UserMessage {
                role: "user".to_owned(),
                content: UserContent::Blocks(vec![ContentBlock::ToolResult {
                    tool_use_id: call_id,
                    content: Some(json!(text)),
                    standalone_tool_name: None,
                }]),
            },
            timestamp,
            uuid: None,
            cwd: None,
            parent_tool_use_id: None,
            usage: None,
        });
    }
}

/// The input a narration part states, or `None` for a synthetic part that
/// narrates no read. The prefixes are OpenCode's own literals
/// (session/prompt.ts); a rewording upstream degrades to skipping the run,
/// never to rendering it inline. An MCP resource is addressed as the
/// `file_path` of the read, which is what the header shows.
fn injected_read_input(part: &Value) -> Option<Value> {
    let text = part.get("text").and_then(Value::as_str)?;
    if let Some(arguments) = text.strip_prefix("Called the Read tool with the following input: ") {
        return Some(
            serde_json::from_str(arguments).unwrap_or_else(|_| json!({ "input": arguments })),
        );
    }
    text.strip_prefix("Reading MCP resource: ")
        .map(|resource| json!({ "file_path": resource }))
}

/// One assistant part's entries. Synthetic parts, `step-start`, snapshots,
/// `file` and `patch` attachments, and unknown types yield nothing —
/// projection, not parsing.
fn assistant_part_entries(part: &Value, timestamp: Option<String>) -> Vec<LogEntry> {
    if is_synthetic(part) {
        return Vec::new();
    }
    let part_id = part.get("id").and_then(Value::as_str).map(str::to_owned);
    match part.get("type").and_then(Value::as_str) {
        Some("text") => match nonempty_text(part, "text") {
            Some(text) => vec![assistant_entry(
                part_id,
                vec![ContentBlock::Text { text }],
                timestamp,
            )],
            None => Vec::new(),
        },
        Some("reasoning") => match nonempty_text(part, "text") {
            Some(thinking) => vec![assistant_entry(
                part_id,
                vec![ContentBlock::Thinking {
                    thinking,
                    signature: String::new(),
                }],
                timestamp,
            )],
            None => Vec::new(),
        },
        Some("tool") => tool_entries(part, timestamp),
        Some("step-finish") => step_usage(part).into_iter().collect(),
        Some("compaction") => compaction_summary(part, timestamp).into_iter().collect(),
        _ => Vec::new(),
    }
}

/// What a tool part that names neither its call nor its tool renders as —
/// the viewer needs something to show, and dropping the part would hide a
/// call the session made.
const UNKNOWN: &str = "unknown";

/// A `tool` part carries the call and, once completed, its result in one
/// record: `{tool, callID, state: {status, input, output}}`.
fn tool_entries(part: &Value, timestamp: Option<String>) -> Vec<LogEntry> {
    let call_id = part
        .get("callID")
        .and_then(Value::as_str)
        .or_else(|| part.get("id").and_then(Value::as_str))
        .unwrap_or(UNKNOWN)
        .to_owned();
    let mut entries = vec![assistant_entry(
        part.get("id").and_then(Value::as_str).map(str::to_owned),
        vec![tool_use_block(
            call_id.clone(),
            part.get("tool").and_then(Value::as_str).unwrap_or(UNKNOWN),
            part.pointer("/state/input").cloned().unwrap_or(json!({})),
        )],
        timestamp.clone(),
    )];
    if part.pointer("/state/status").and_then(Value::as_str) == Some("completed") {
        let output = part
            .pointer("/state/output")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let text = bounded_tool_result_text(&json!(output)).unwrap_or_default();
        entries.push(LogEntry::User {
            message: UserMessage {
                role: "user".to_owned(),
                content: UserContent::Blocks(vec![ContentBlock::ToolResult {
                    tool_use_id: call_id,
                    content: Some(json!(text)),
                    standalone_tool_name: None,
                }]),
            },
            timestamp,
            uuid: None,
            cwd: None,
            parent_tool_use_id: None,
            usage: None,
        });
    }
    entries
}

fn tool_use_block(call_id: String, name: &str, mut input: Value) -> ContentBlock {
    let tool = canonical_tool(name);
    canonicalize_input(tool, &mut input);
    ContentBlock::ToolUse {
        id: call_id,
        name: name.to_owned(),
        tool,
        input,
    }
}

fn canonical_tool(name: &str) -> Tool {
    match name {
        "bash" => Tool::Shell,
        "read" => Tool::Read,
        "edit" => Tool::Edit,
        "write" => Tool::Write,
        "grep" => Tool::Grep,
        "glob" => Tool::Glob,
        "webfetch" => Tool::WebFetch,
        "todowrite" => Tool::TaskList,
        "task" => Tool::Agent,
        _ => Tool::Other,
    }
}

/// OpenCode names its keys in camel case; the ones the headers and diff
/// bodies read are renamed, every other key passes through.
fn canonicalize_input(tool: Tool, input: &mut Value) {
    let Some(arguments) = input.as_object_mut() else {
        return;
    };
    if tool.takes_file_path() {
        rename_key(arguments, "filePath", "file_path");
    }
    match tool {
        Tool::Edit => {
            rename_key(arguments, "oldString", "old_string");
            rename_key(arguments, "newString", "new_string");
        }
        Tool::Grep => rename_key(arguments, "include", "glob"),
        _ => {}
    }
}

fn assistant_entry(
    id: Option<String>,
    content: Vec<ContentBlock>,
    timestamp: Option<String>,
) -> LogEntry {
    LogEntry::Assistant {
        agent: Some("OpenCode".to_owned()),
        message: AssistantMessage {
            role: "assistant".to_owned(),
            content,
            model: None,
            usage: None,
            // The part id, not the message id: entries sharing an id would
            // collapse into one turn when the agent transcript deduplicates
            // repeated assistant ids.
            id,
        },
        timestamp,
        uuid: None,
        parent_tool_use_id: None,
    }
}

/// One step's tokens, as an invisible metadata entry. `step-finish` is the
/// per-request grain; the message- and session-level token columns restate
/// these totals and reading them too would double-count.
fn step_usage(part: &Value) -> Option<LogEntry> {
    let count = |pointer: &str| part.pointer(pointer).and_then(Value::as_u64).unwrap_or(0);
    let usage = TokenUsage {
        input_tokens: count("/tokens/input"),
        // Reasoning tokens are output-side spend; folding them into output
        // keeps the four-field usage shape every provider shares.
        output_tokens: count("/tokens/output") + count("/tokens/reasoning"),
        cache_creation_input_tokens: count("/tokens/cache/write"),
        cache_read_input_tokens: count("/tokens/cache/read"),
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

fn compaction_summary(part: &Value, timestamp: Option<String>) -> Option<LogEntry> {
    let summary = part.get("summary").and_then(Value::as_str)?;
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

/// Every session row and the parent it names, as the forest discovery and
/// delete read sub-agent rows from. The failure is SQLite's own, so the
/// caller can word it for the list.
pub(crate) fn session_forest(connection: &Connection) -> rusqlite::Result<SubagentForest<String>> {
    let mut statement = connection.prepare("SELECT id, parent_id FROM session")?;
    let rows = statement.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
    })?;
    let sessions = rows.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(SubagentForest::new(sessions))
}

/// The id of every sub-agent row of `reference`, nested ones included,
/// in id order.
pub(crate) fn subagent_session_ids(
    connection: &Connection,
    reference: &SessionRef,
) -> rusqlite::Result<Vec<String>> {
    Ok(session_forest(connection)?.subagents_of(&reference.session_id))
}

fn model_name(message: &Value) -> Option<&str> {
    message
        .get("modelID")
        .and_then(Value::as_str)
        .or_else(|| message.pointer("/model/modelID").and_then(Value::as_str))
        .filter(|model| !model.is_empty())
}

fn part_timestamp(part: &Value) -> Option<String> {
    part.pointer("/time/start")
        .and_then(Value::as_i64)
        .and_then(rfc3339_from_millis)
}

fn nonempty_text(part: &Value, field: &str) -> Option<String> {
    part.get(field)
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
}

/// Rows stamp milliseconds since the epoch; entries carry RFC 3339 so
/// timestamps compare and render like every other provider's. A deliberate
/// twin of kimi.rs's copy; a third is the cue to extract one.
fn rfc3339_from_millis(millis: i64) -> Option<String> {
    DateTime::<Utc>::from_timestamp_millis(millis)
        .map(|timestamp| timestamp.to_rfc3339_opts(SecondsFormat::Millis, true))
}

/// Builders for synthetic OpenCode databases, shared with the provider's
/// tests. The schema is transcribed from
/// `../opencode/packages/core/src/database/schema.gen.ts` (migrations
/// through 2026-06-22), restricted to the four content tables this provider
/// reads, with their cascade behavior and WAL journaling — plus the
/// migration journal `migration.ts` creates, stamped at
/// [`NEWEST_VERIFIED_MIGRATION`](super::NEWEST_VERIFIED_MIGRATION) as in a
/// database written by the OpenCode this reader was developed against.
/// Transcribed rather than approximated: the schema moves fast, and a
/// drifted fixture would test a database OpenCode no longer writes.
#[cfg(test)]
pub(crate) mod fixture {
    use rusqlite::Connection;
    use std::path::Path;

    pub(crate) fn create_database(path: &Path) -> Connection {
        let connection = Connection::open(path).unwrap();
        connection
            .pragma_update(None, "journal_mode", "WAL")
            .unwrap();
        connection
            .execute_batch(
                r#"
                CREATE TABLE `project` (
                  `id` text PRIMARY KEY,
                  `worktree` text NOT NULL,
                  `vcs` text,
                  `name` text,
                  `icon_url` text,
                  `icon_url_override` text,
                  `icon_color` text,
                  `time_created` integer NOT NULL,
                  `time_updated` integer NOT NULL,
                  `time_initialized` integer,
                  `sandboxes` text NOT NULL,
                  `commands` text
                );
                CREATE TABLE `session` (
                  `id` text PRIMARY KEY,
                  `project_id` text NOT NULL,
                  `workspace_id` text,
                  `parent_id` text,
                  `slug` text NOT NULL,
                  `directory` text NOT NULL,
                  `path` text,
                  `title` text NOT NULL,
                  `version` text NOT NULL,
                  `share_url` text,
                  `summary_additions` integer,
                  `summary_deletions` integer,
                  `summary_files` integer,
                  `summary_diffs` text,
                  `metadata` text,
                  `cost` real DEFAULT 0 NOT NULL,
                  `tokens_input` integer DEFAULT 0 NOT NULL,
                  `tokens_output` integer DEFAULT 0 NOT NULL,
                  `tokens_reasoning` integer DEFAULT 0 NOT NULL,
                  `tokens_cache_read` integer DEFAULT 0 NOT NULL,
                  `tokens_cache_write` integer DEFAULT 0 NOT NULL,
                  `revert` text,
                  `permission` text,
                  `agent` text,
                  `model` text,
                  `time_created` integer NOT NULL,
                  `time_updated` integer NOT NULL,
                  `time_compacting` integer,
                  `time_archived` integer,
                  CONSTRAINT `fk_session_project_id_project_id_fk` FOREIGN KEY (`project_id`) REFERENCES `project`(`id`) ON DELETE CASCADE
                );
                CREATE TABLE `message` (
                  `id` text PRIMARY KEY,
                  `session_id` text NOT NULL,
                  `time_created` integer NOT NULL,
                  `time_updated` integer NOT NULL,
                  `data` text NOT NULL,
                  CONSTRAINT `fk_message_session_id_session_id_fk` FOREIGN KEY (`session_id`) REFERENCES `session`(`id`) ON DELETE CASCADE
                );
                CREATE TABLE `part` (
                  `id` text PRIMARY KEY,
                  `message_id` text NOT NULL,
                  `session_id` text NOT NULL,
                  `time_created` integer NOT NULL,
                  `time_updated` integer NOT NULL,
                  `data` text NOT NULL,
                  CONSTRAINT `fk_part_message_id_message_id_fk` FOREIGN KEY (`message_id`) REFERENCES `message`(`id`) ON DELETE CASCADE
                );
                CREATE TABLE `migration` (
                  `id` text PRIMARY KEY,
                  `time_completed` integer NOT NULL
                );
                INSERT INTO project (id, worktree, time_created, time_updated, sandboxes)
                VALUES ('proj_fixture', '/tmp/fixture', 1755000000000, 1755000000000, '[]');
                "#,
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO migration (id, time_completed) VALUES (?1, 1755000000000)",
                [super::NEWEST_VERIFIED_MIGRATION],
            )
            .unwrap();
        connection
    }

    pub(crate) struct SessionSpec<'a> {
        pub id: &'a str,
        pub parent_id: Option<&'a str>,
        pub directory: &'a str,
        pub title: &'a str,
        pub created_ms: i64,
        pub updated_ms: i64,
        pub archived_ms: Option<i64>,
    }

    pub(crate) fn insert_session(connection: &Connection, session: &SessionSpec) {
        connection
            .execute(
                "INSERT INTO session (id, project_id, parent_id, slug, directory, title,
                                      version, time_created, time_updated, time_archived)
                 VALUES (?1, 'proj_fixture', ?2, ?3, ?4, ?5, '1.0.0-fixture', ?6, ?7, ?8)",
                rusqlite::params![
                    session.id,
                    session.parent_id,
                    session.id,
                    session.directory,
                    session.title,
                    session.created_ms,
                    session.updated_ms,
                    session.archived_ms,
                ],
            )
            .unwrap();
    }

    pub(crate) fn insert_message(
        connection: &Connection,
        id: &str,
        session_id: &str,
        time_ms: i64,
        data: &serde_json::Value,
    ) {
        connection
            .execute(
                "INSERT INTO message (id, session_id, time_created, time_updated, data)
                 VALUES (?1, ?2, ?3, ?3, ?4)",
                rusqlite::params![id, session_id, time_ms, data.to_string()],
            )
            .unwrap();
    }

    pub(crate) fn insert_part(
        connection: &Connection,
        id: &str,
        message_id: &str,
        session_id: &str,
        time_ms: i64,
        data: &serde_json::Value,
    ) {
        connection
            .execute(
                "INSERT INTO part (id, message_id, session_id, time_created, time_updated, data)
                 VALUES (?1, ?2, ?3, ?4, ?4, ?5)",
                rusqlite::params![id, message_id, session_id, time_ms, data.to_string()],
            )
            .unwrap();
    }

    /// A top-level session with one user turn and one multi-part assistant
    /// turn: text, a synthetic reminder to skip, a synthetic @-file
    /// injection that projects as a read, a review-UI comment that stays
    /// user text, reasoning, a completed tool call, request framing to
    /// skip, and a `step-finish` with tokens. Content shapes mirror
    /// OpenCode's own stress fixture
    /// (`packages/app/e2e/performance/timeline/session-timeline-stress.fixture.ts`)
    /// and the live rows the plan surveyed.
    pub(crate) fn standard_session(connection: &Connection, session_id: &str) {
        insert_session(
            connection,
            &SessionSpec {
                id: session_id,
                parent_id: None,
                directory: "/tmp/opencode-project",
                title: "fixture generated title",
                created_ms: 1755000100000i64,
                updated_ms: 1755000400000i64,
                archived_ms: None,
            },
        );
        let row = |prefix: &str| format!("{prefix}_{session_id}");
        insert_message(
            connection,
            &row("msg_user"),
            session_id,
            1755000100000i64,
            &serde_json::json!({
                "role": "user",
                "time": { "created": 1755000100000i64 },
                "agent": "build",
            }),
        );
        insert_part(
            connection,
            &row("prt_user"),
            &row("msg_user"),
            session_id,
            1755000100000i64,
            &serde_json::json!({
                "type": "text",
                "text": "find the fixture defect",
                "time": { "start": 1755000100000i64, "end": 1755000100000i64 },
            }),
        );
        // Synthetic parts in prompt.ts's stored shapes: a reminder that
        // narrates no read, then an @-file injection as its narration part
        // and the file contents behind it. Ids keep this insertion order
        // under the (time_created, id) sort.
        insert_part(
            connection,
            &row("prt_user_a_note"),
            &row("msg_user"),
            session_id,
            1755000100000i64,
            &serde_json::json!({
                "type": "text",
                "synthetic": true,
                "text": "Summarize the task tool output above and continue with your task.",
            }),
        );
        insert_part(
            connection,
            &row("prt_user_b_call"),
            &row("msg_user"),
            session_id,
            1755000100000i64,
            &serde_json::json!({
                "type": "text",
                "synthetic": true,
                "text": "Called the Read tool with the following input: {\"filePath\":\"/tmp/opencode-project/README.md\"}",
            }),
        );
        insert_part(
            connection,
            &row("prt_user_c_body"),
            &row("msg_user"),
            session_id,
            1755000100000i64,
            &serde_json::json!({
                "type": "text",
                "synthetic": true,
                "text": "<content>SYNTHETIC_INJECTION_SENTINEL fn defect() {}</content>",
            }),
        );
        // A review-UI comment: synthetic, but the user's own words. Its
        // position right after the injection pins that a comment is never
        // swallowed into a preceding read's output.
        insert_part(
            connection,
            &row("prt_user_d_comment"),
            &row("msg_user"),
            session_id,
            1755000100000i64,
            &serde_json::json!({
                "type": "text",
                "synthetic": true,
                "text": "The user made the following comment regarding lines 40 through 68 of errorHandler.ts: COMMENT_SENTINEL this looks messy",
                "metadata": { "opencodeComment": {
                    "path": "errorHandler.ts",
                    "comment": "COMMENT_SENTINEL this looks messy",
                } },
            }),
        );
        insert_message(
            connection,
            &row("msg_asst"),
            session_id,
            1755000200000i64,
            &serde_json::json!({
                "role": "assistant",
                "time": { "created": 1755000200000i64 },
                "parentID": row("msg_user"),
                "modelID": "claude-opus-4-6",
                "providerID": "anthropic",
                "tokens": {
                    "total": 62, "input": 40, "output": 15, "reasoning": 5,
                    "cache": { "read": 2, "write": 0 },
                },
            }),
        );
        for (part, time_ms, data) in [
            (
                "prt_asst_0001",
                1755000200000i64,
                serde_json::json!({ "type": "step-start", "snapshot": "snap_1" }),
            ),
            (
                "prt_asst_0002",
                1755000210000i64,
                serde_json::json!({
                    "type": "reasoning",
                    "text": "considering the fixture layout",
                    "time": { "start": 1755000210000i64, "end": 1755000215000i64 },
                }),
            ),
            (
                "prt_asst_0003",
                1755000220000i64,
                serde_json::json!({
                    "type": "tool",
                    "tool": "read",
                    "callID": "call_0001",
                    "state": {
                        "status": "completed",
                        "input": { "filePath": "/tmp/opencode-project/lib.rs" },
                        "output": "fn defect() {}",
                        "time": { "start": 1755000220000i64, "end": 1755000221000i64 },
                    },
                }),
            ),
            (
                "prt_asst_0004",
                1755000230000i64,
                serde_json::json!({
                    "type": "text",
                    "text": "the defect hides in lib.rs",
                    "time": { "start": 1755000230000i64, "end": 1755000231000i64 },
                }),
            ),
            (
                "prt_asst_0005",
                1755000240000i64,
                serde_json::json!({
                    "type": "step-finish",
                    "reason": "stop",
                    "snapshot": "snap_2",
                    "cost": 0.01,
                    "tokens": {
                        "total": 62, "input": 40, "output": 15, "reasoning": 5,
                        "cache": { "read": 2, "write": 0 },
                    },
                }),
            ),
        ] {
            insert_part(
                connection,
                &format!("{part}_{session_id}"),
                &row("msg_asst"),
                session_id,
                time_ms,
                &data,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fixture::SessionSpec;

    fn database_in(directory: &Path) -> PathBuf {
        let database = directory.join("opencode.db");
        fixture::create_database(&database);
        database
    }

    fn entry_json(projection: &SessionProjection) -> String {
        projection
            .entries
            .iter()
            .map(|(_, entry)| serde_json::to_string(entry).unwrap())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn a_session_projects_dialogue_and_skips_request_framing() {
        let directory = tempfile::tempdir().unwrap();
        let database = database_in(directory.path());
        let connection = Connection::open(&database).unwrap();
        fixture::standard_session(&connection, "ses_standard");
        drop(connection);

        let projection = OPENCODE_DB
            .parse_transcript(&session_ref(&database, "ses_standard"))
            .unwrap()
            .unwrap();

        assert_eq!(projection.source, Source::OpenCode);
        assert_eq!(projection.header.id, "ses_standard");
        assert_eq!(
            projection.header.cwd,
            PathBuf::from("/tmp/opencode-project")
        );
        assert_eq!(projection.title.as_deref(), Some("fixture generated title"));

        let rendered = entry_json(&projection);
        assert!(rendered.contains("find the fixture defect"));
        assert!(rendered.contains("considering the fixture layout"));
        assert!(rendered.contains("the defect hides in lib.rs"));
        assert!(rendered.contains("call_0001"));
        assert!(rendered.contains("fn defect() {}"));
        assert!(rendered.contains("claude-opus-4-6"));
        assert!(
            !rendered.contains("snap_1"),
            "step-start request framing is not dialogue"
        );
        assert!(
            rendered.contains("SYNTHETIC_INJECTION_SENTINEL"),
            "@-file contents render as a tool result, behind the tools toggle"
        );
        assert!(
            rendered.contains("/tmp/opencode-project/README.md"),
            "the narrated read input becomes the tool call's input"
        );
        assert!(
            !rendered.contains("Called the Read tool"),
            "the narration is consumed into the call, not shown as the user's words"
        );
        assert!(
            !rendered.contains("Summarize the task tool output"),
            "a synthetic reminder narrating no read is not dialogue"
        );
        assert!(
            rendered.contains("COMMENT_SENTINEL"),
            "a review-UI comment is the user's own words and stays in the dialogue"
        );
    }

    #[test]
    fn step_finish_tokens_reach_the_conversation_totals() {
        let directory = tempfile::tempdir().unwrap();
        let database = database_in(directory.path());
        let connection = Connection::open(&database).unwrap();
        fixture::standard_session(&connection, "ses_tokens");
        drop(connection);

        let stub = crate::history::provider::SessionStub {
            locator: session_ref(&database, "ses_tokens"),
            subagents: Vec::new(),
            cache_key: "ses_tokens".to_owned(),
            fingerprint: crate::history::provider::Fingerprint {
                size: 0,
                modified: None,
            },
        };
        let conversation = crate::history::parser::process_session_file(&stub, &OPENCODE_DB, None)
            .unwrap()
            .unwrap();

        assert_eq!(
            conversation.total_tokens,
            40 + 15 + 5 + 2,
            "input + output + reasoning + cache read, counted once"
        );
        assert_eq!(conversation.model.as_deref(), Some("claude-opus-4-6"));
        assert_eq!(
            conversation.custom_title.as_deref(),
            Some("fixture generated title"),
            "OpenCode titles are autogenerated but project as titles"
        );
    }

    #[test]
    fn a_sub_agent_session_carries_no_title() {
        let directory = tempfile::tempdir().unwrap();
        let database = database_in(directory.path());
        let connection = Connection::open(&database).unwrap();
        fixture::standard_session(&connection, "ses_parent");
        fixture::insert_session(
            &connection,
            &SessionSpec {
                id: "ses_child",
                parent_id: Some("ses_parent"),
                directory: "/tmp/opencode-project",
                title: "Explore fixtures (@explore subagent)",
                created_ms: 1755000205000i64,
                updated_ms: 1755000205000i64,
                archived_ms: None,
            },
        );
        fixture::insert_message(
            &connection,
            "msg_child_0001",
            "ses_child",
            1755000205000i64,
            &serde_json::json!({ "role": "user", "time": { "created": 1755000205000i64 } }),
        );
        fixture::insert_part(
            &connection,
            "prt_child_0001",
            "msg_child_0001",
            "ses_child",
            1755000205000i64,
            &serde_json::json!({ "type": "text", "text": "child exploration prompt" }),
        );
        drop(connection);

        let child = OPENCODE_DB
            .parse_transcript(&session_ref(&database, "ses_child"))
            .unwrap()
            .unwrap();

        assert_eq!(
            child.title, None,
            "a sub-agent row is read through its parent and carries no title"
        );
        assert!(entry_json(&child).contains("child exploration prompt"));
        assert_eq!(
            subagent_session_ids(
                &Connection::open(&database).unwrap(),
                &SessionRef {
                    database: database.clone(),
                    session_id: "ses_parent".to_owned(),
                },
            )
            .unwrap(),
            vec!["ses_child"]
        );
    }

    #[test]
    fn the_view_splices_subagent_sessions_and_the_plain_parse_does_not() {
        let directory = tempfile::tempdir().unwrap();
        let database = database_in(directory.path());
        let connection = Connection::open(&database).unwrap();
        fixture::standard_session(&connection, "ses_parent");
        for (id, parent, prompt, at) in [
            (
                "ses_child",
                "ses_parent",
                "child exploration",
                1755000205000i64,
            ),
            (
                "ses_grandchild",
                "ses_child",
                "grandchild digging",
                1755000206000i64,
            ),
        ] {
            fixture::insert_session(
                &connection,
                &SessionSpec {
                    id,
                    parent_id: Some(parent),
                    directory: "/tmp/opencode-project",
                    title: "",
                    created_ms: at,
                    updated_ms: at,
                    archived_ms: None,
                },
            );
            let message_id = format!("msg_{id}");
            fixture::insert_message(
                &connection,
                &message_id,
                id,
                at,
                &serde_json::json!({ "role": "user", "time": { "created": at } }),
            );
            fixture::insert_part(
                &connection,
                &format!("prt_{id}"),
                &message_id,
                id,
                at,
                &serde_json::json!({ "type": "text", "text": prompt }),
            );
        }
        drop(connection);

        let reference = session_ref(&database, "ses_parent");
        let plain = OPENCODE_DB.parse_transcript(&reference).unwrap().unwrap();
        let view = super::super::view_projection(
            &OPENCODE_DB,
            &reference,
            &[
                session_ref(&database, "ses_child"),
                session_ref(&database, "ses_grandchild"),
            ],
        )
        .unwrap()
        .unwrap();

        let plain_rendered = entry_json(&plain);
        assert!(!plain_rendered.contains("child exploration"));
        let view_rendered = entry_json(&view);
        assert!(view_rendered.contains("child exploration"));
        assert!(
            view_rendered.contains("grandchild digging"),
            "a sub-agent's own sub-agent splices too"
        );
        assert!(
            view_rendered.contains("agent_progress"),
            "spliced threads take Claude's native progress shape"
        );
    }

    #[test]
    fn foreign_files_and_missing_sessions_are_not_claimed() {
        let directory = tempfile::tempdir().unwrap();

        let pi = directory.path().join("session.jsonl");
        std::fs::copy(
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/pi/v1.jsonl"),
            &pi,
        )
        .unwrap();
        assert!(
            OPENCODE_DB.parse_transcript(&pi).unwrap().is_none(),
            "a transcript file's parent is a directory, so it decodes as no ref"
        );

        let database = database_in(directory.path());
        assert!(
            OPENCODE_DB
                .parse_transcript(&session_ref(&database, "ses_absent"))
                .unwrap()
                .is_none(),
            "a ref to a session the database does not hold is not claimed"
        );
    }

    /// The journal check is advisory: it names a database migrated beyond
    /// the pin and stays quiet otherwise — including for a database too old
    /// to carry the journal at all.
    #[test]
    fn the_migration_journal_flags_only_databases_beyond_the_pin() {
        let directory = tempfile::tempdir().unwrap();
        let database = database_in(directory.path());
        let connection = Connection::open(&database).unwrap();

        assert_eq!(
            newest_unverified_migration(&connection),
            None,
            "a database at the pin has nothing to report"
        );

        connection
            .execute(
                "INSERT INTO migration (id, time_completed)
                 VALUES ('20991231000000_from_the_future', 1)",
                [],
            )
            .unwrap();
        assert_eq!(
            newest_unverified_migration(&connection).as_deref(),
            Some("20991231000000_from_the_future")
        );

        connection.execute("DROP TABLE migration", []).unwrap();
        assert_eq!(
            newest_unverified_migration(&connection),
            None,
            "a database from before the journal is older than the pin, not newer"
        );
    }

    #[test]
    fn an_unreadable_database_fails_rather_than_answering_absent() {
        let directory = tempfile::tempdir().unwrap();
        let not_a_database = directory.path().join("garbage.bin");
        std::fs::write(&not_a_database, "not sqlite at all").unwrap();

        let result = OPENCODE_DB.parse_transcript(&session_ref(&not_a_database, "ses_x"));

        assert!(
            result.is_err(),
            "a store that cannot be read must not read as an absent session"
        );
    }

    fn tool_use(name: &str, input: Value) -> (Tool, Value) {
        match tool_use_block("call_1".to_owned(), name, input) {
            ContentBlock::ToolUse {
                id,
                name: kept,
                tool,
                input,
            } => {
                assert_eq!(id, "call_1");
                assert_eq!(kept, name);
                (tool, input)
            }
            other => panic!("not a tool use: {other:?}"),
        }
    }

    #[test]
    fn every_opencode_tool_name_lands_in_its_bucket() {
        let expected = [
            ("bash", Tool::Shell),
            ("read", Tool::Read),
            ("edit", Tool::Edit),
            ("write", Tool::Write),
            ("grep", Tool::Grep),
            ("glob", Tool::Glob),
            ("webfetch", Tool::WebFetch),
            ("todowrite", Tool::TaskList),
            ("task", Tool::Agent),
            ("skill", Tool::Other),
            ("question", Tool::Other),
        ];
        for (name, tool) in expected {
            assert_eq!(tool_use(name, json!({})).0, tool, "{name}");
        }
    }

    #[test]
    fn opencode_keys_are_renamed_to_the_canonical_ones_and_the_rest_kept() {
        assert_eq!(
            tool_use(
                "read",
                json!({"filePath": "src/lib.rs", "offset": 10, "limit": 5})
            )
            .1,
            json!({"file_path": "src/lib.rs", "offset": 10, "limit": 5})
        );
        assert_eq!(
            tool_use(
                "edit",
                json!({"filePath": "src/lib.rs", "oldString": "a", "newString": "b", "replaceAll": true})
            )
            .1,
            json!({"file_path": "src/lib.rs", "old_string": "a", "new_string": "b", "replaceAll": true})
        );
        assert_eq!(
            tool_use("write", json!({"filePath": "NEW.md", "content": "# New"})).1,
            json!({"file_path": "NEW.md", "content": "# New"})
        );
        assert_eq!(
            tool_use(
                "grep",
                json!({"pattern": "fn main", "path": "src", "include": "*.rs"})
            )
            .1,
            json!({"pattern": "fn main", "path": "src", "glob": "*.rs"})
        );
        assert_eq!(
            tool_use("glob", json!({"pattern": "**/*.rs", "path": "src"})).1,
            json!({"pattern": "**/*.rs", "path": "src"})
        );
        assert_eq!(
            tool_use(
                "task",
                json!({"description": "scout", "prompt": "Look.", "subagent_type": "explore"})
            )
            .1,
            json!({"description": "scout", "prompt": "Look.", "subagent_type": "explore"})
        );
        assert_eq!(tool_use("read", json!({})), (Tool::Read, json!({})));
    }

    #[test]
    fn an_injected_read_is_a_read_of_the_file_or_resource_it_names() {
        let narration = |text: &str| json!({ "type": "text", "synthetic": true, "text": text });

        assert_eq!(
            injected_read_input(&narration(
                "Called the Read tool with the following input: {\"filePath\":\"/tmp/README.md\"}"
            )),
            Some(json!({ "filePath": "/tmp/README.md" }))
        );
        assert_eq!(
            injected_read_input(&narration("Reading MCP resource: docs://guide")),
            Some(json!({ "file_path": "docs://guide" }))
        );
        assert_eq!(injected_read_input(&narration("Continue.")), None);
    }

    #[test]
    fn ref_encoding_round_trips() {
        let directory = tempfile::tempdir().unwrap();
        let database = database_in(directory.path());

        let reference = session_ref(&database, "ses_round_trip");
        let decoded = decode_ref(&reference).unwrap();

        assert_eq!(decoded.database, database);
        assert_eq!(decoded.session_id, "ses_round_trip");
        assert!(
            decode_ref(&directory.path().join("ses_no_database.jsonl")).is_none(),
            "a ref decodes only under an existing database file"
        );
    }
}
