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

use super::splice::{progress_entries, splice_by_timestamp};
use super::{SessionFormat, SessionHeader, SessionProjection};
use crate::agent::transcript::bounded_tool_result_text;
use crate::claude::{
    AssistantMessage, ContentBlock, LogEntry, TokenUsage, UserContent, UserMessage,
};
use crate::error::{AppError, Result};
use crate::history::Source;
use chrono::{DateTime, SecondsFormat, Utc};
use rusqlite::{Connection, OpenFlags};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub struct OpenCodeDbFormat;

pub static OPENCODE_DB: OpenCodeDbFormat = OpenCodeDbFormat;

impl SessionFormat for OpenCodeDbFormat {
    fn parse_transcript(&self, path: &Path) -> Result<Option<SessionProjection>> {
        let Some(reference) = decode_ref(path) else {
            return Ok(None);
        };
        let connection = open_read_only(&reference.database)?;
        project_session(&connection, &reference)
    }

    /// OpenCode records each sub-agent as a child session in the same
    /// database, so the view of a top-level session splices every
    /// descendant's dialogue in as `Progress` entries, ordered by timestamp.
    /// The view of a child session is its plain parse: the thread folds into
    /// the session at load and is read through it.
    fn parse_transcript_view(&self, path: &Path) -> Result<Option<SessionProjection>> {
        let Some(reference) = decode_ref(path) else {
            return Ok(None);
        };
        let connection = open_read_only(&reference.database)?;
        let Some(mut projection) = project_session(&connection, &reference)? else {
            return Ok(None);
        };
        if projection.parent_session_id.is_some() {
            return Ok(Some(projection));
        }
        let threads = descendant_sessions(&connection, &reference)?;
        projection.entries = splice_by_timestamp(projection.entries, progress_entries(threads));
        Ok(Some(projection))
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

/// Open the database read-only. A database that exists but cannot be opened
/// is an error, never "no session": a guard reading this as absence could
/// mistake a locked store for a deletable one.
pub(crate) fn open_read_only(database: &Path) -> Result<Connection> {
    let connection = Connection::open_with_flags(
        database,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|error| database_error(database, &error))?;
    configure(database, &connection)?;
    Ok(connection)
}

/// A running OpenCode holds the WAL writer; a short busy wait rides out its
/// checkpoints, the same 5000 ms OpenCode itself configures.
pub(crate) fn configure(database: &Path, connection: &Connection) -> Result<()> {
    connection
        .busy_timeout(Duration::from_millis(5000))
        .map_err(|error| database_error(database, &error))
}

pub(crate) fn database_error(database: &Path, error: &dyn std::fmt::Display) -> AppError {
    AppError::Io(std::io::Error::other(format!(
        "{}: {error}",
        database.display()
    )))
}

fn project_session(
    connection: &Connection,
    reference: &SessionRef,
) -> Result<Option<SessionProjection>> {
    let Some(session) = session_row(connection, reference)? else {
        return Ok(None);
    };

    let mut parts_by_message = parts_by_message(connection, reference)?;
    let mut sink = EntrySink::default();
    for (message_id, data) in messages(connection, reference)? {
        let parts = parts_by_message.remove(&message_id).unwrap_or_default();
        message_entries(&data, &parts, &mut sink);
    }
    let mut entries = sink.entries;

    // The title belongs to the session, so only a top-level session carries
    // it; a child's row folds away before a title could matter. OpenCode
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
        parent_session_id: session.parent_id,
        header: SessionHeader {
            version: 1,
            id: reference.session_id.clone(),
            timestamp: rfc3339_from_millis(session.time_created).unwrap_or_default(),
            cwd: PathBuf::from(session.directory),
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

fn session_row(connection: &Connection, reference: &SessionRef) -> Result<Option<SessionRow>> {
    let database = &reference.database;
    let mut statement = connection
        .prepare("SELECT parent_id, directory, title, time_created FROM session WHERE id = ?1")
        .map_err(|error| database_error(database, &error))?;
    let mut rows = statement
        .query([&reference.session_id])
        .map_err(|error| database_error(database, &error))?;
    let Some(row) = rows
        .next()
        .map_err(|error| database_error(database, &error))?
    else {
        return Ok(None);
    };
    let session = SessionRow {
        parent_id: row
            .get(0)
            .map_err(|error| database_error(database, &error))?,
        directory: row
            .get(1)
            .map_err(|error| database_error(database, &error))?,
        title: row
            .get(2)
            .map_err(|error| database_error(database, &error))?,
        time_created: row
            .get(3)
            .map_err(|error| database_error(database, &error))?,
    };
    Ok(Some(session))
}

/// The session's messages in conversation order: `(message id, decoded data)`.
/// A row whose JSON does not parse is skipped rather than failing the session.
fn messages(connection: &Connection, reference: &SessionRef) -> Result<Vec<(String, Value)>> {
    let database = &reference.database;
    let mut statement = connection
        .prepare("SELECT id, data FROM message WHERE session_id = ?1 ORDER BY time_created, id")
        .map_err(|error| database_error(database, &error))?;
    let rows = statement
        .query_map([&reference.session_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|error| database_error(database, &error))?;
    let mut messages = Vec::new();
    for row in rows {
        let (id, data) = row.map_err(|error| database_error(database, &error))?;
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
) -> Result<HashMap<String, Vec<Value>>> {
    let database = &reference.database;
    let mut statement = connection
        .prepare(
            "SELECT message_id, data FROM part WHERE session_id = ?1 ORDER BY time_created, id",
        )
        .map_err(|error| database_error(database, &error))?;
    let rows = statement
        .query_map([&reference.session_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|error| database_error(database, &error))?;
    let mut parts: HashMap<String, Vec<Value>> = HashMap::new();
    for row in rows {
        let (message_id, data) = row.map_err(|error| database_error(database, &error))?;
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
            vec![ContentBlock::ToolUse {
                id: call_id.clone(),
                name: "read".to_owned(),
                input,
            }],
            timestamp.clone(),
        ));
        let text = bounded_tool_result_text(&json!(contents.join("\n"))).unwrap_or_default();
        sink.push(LogEntry::User {
            message: UserMessage {
                role: "user".to_owned(),
                content: UserContent::Blocks(vec![ContentBlock::ToolResult {
                    tool_use_id: call_id,
                    content: Some(json!(text)),
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
/// never to rendering it inline.
fn injected_read_input(part: &Value) -> Option<Value> {
    let text = part.get("text").and_then(Value::as_str)?;
    if let Some(arguments) = text.strip_prefix("Called the Read tool with the following input: ") {
        return Some(
            serde_json::from_str(arguments).unwrap_or_else(|_| json!({ "input": arguments })),
        );
    }
    text.strip_prefix("Reading MCP resource: ")
        .map(|resource| json!({ "resource": resource }))
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
        vec![ContentBlock::ToolUse {
            id: call_id.clone(),
            name: part
                .get("tool")
                .and_then(Value::as_str)
                .unwrap_or(UNKNOWN)
                .to_owned(),
            input: part.pointer("/state/input").cloned().unwrap_or(json!({})),
        }],
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

/// Every descendant session, transitively, parsed in full and labelled with
/// its session id. A parent chain that loops back on itself stops at the
/// first repeated id.
fn descendant_sessions(
    connection: &Connection,
    reference: &SessionRef,
) -> Result<Vec<(String, SessionProjection)>> {
    let database = &reference.database;
    let mut statement = connection
        .prepare("SELECT id, parent_id FROM session WHERE parent_id IS NOT NULL ORDER BY id")
        .map_err(|error| database_error(database, &error))?;
    let rows = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|error| database_error(database, &error))?;
    let mut children_of: HashMap<String, Vec<String>> = HashMap::new();
    for row in rows {
        let (id, parent) = row.map_err(|error| database_error(database, &error))?;
        children_of.entry(parent).or_default().push(id);
    }

    let mut visited = HashSet::from([reference.session_id.clone()]);
    let mut queue = children_of
        .get(&reference.session_id)
        .cloned()
        .unwrap_or_default();
    let mut threads = Vec::new();
    while let Some(child_id) = queue.pop() {
        if !visited.insert(child_id.clone()) {
            continue;
        }
        queue.extend(children_of.get(&child_id).cloned().unwrap_or_default());
        let child = SessionRef {
            database: database.clone(),
            session_id: child_id.clone(),
        };
        if let Some(projection) = project_session(connection, &child)? {
            threads.push((child_id, projection));
        }
    }
    // Query order feeds a stack, so restore a stable order for the splice.
    threads.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(threads)
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
/// through 2026-06-22), restricted to the four tables this provider reads,
/// with their cascade behavior and WAL journaling. Transcribed rather than
/// approximated: the schema moves fast, and a drifted fixture would test a
/// database OpenCode no longer writes.
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
                INSERT INTO project (id, worktree, time_created, time_updated, sandboxes)
                VALUES ('proj_fixture', '/tmp/fixture', 1755000000000, 1755000000000, '[]');
                "#,
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
        assert_eq!(projection.parent_session_id, None);
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

        let conversation = crate::history::parser::process_session_file(
            session_ref(&database, "ses_tokens"),
            &OPENCODE_DB,
            None,
            None,
        )
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
    fn a_child_session_carries_its_parent_and_no_title() {
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

        assert_eq!(child.parent_session_id.as_deref(), Some("ses_parent"));
        assert_eq!(
            child.title, None,
            "a child folds away before a title could matter"
        );
        assert!(entry_json(&child).contains("child exploration prompt"));
    }

    #[test]
    fn the_view_splices_descendants_and_the_plain_parse_does_not() {
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
        let view = OPENCODE_DB
            .parse_transcript_view(&reference)
            .unwrap()
            .unwrap();

        let plain_rendered = entry_json(&plain);
        assert!(!plain_rendered.contains("child exploration"));
        let view_rendered = entry_json(&view);
        assert!(view_rendered.contains("child exploration"));
        assert!(
            view_rendered.contains("grandchild digging"),
            "descendants splice transitively"
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
