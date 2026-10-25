//! The OpenCode session store: one SQLite database holding every session.
//!
//! | OpenCode | Session table | Message tables    | Read in        |
//! |----------|---------------|-------------------|----------------|
//! | 1        | `session`     | `message`, `part` | [`opencode_1`] |
//! | 2        | `session_v2`  | `session_message` | [`opencode_2`] |
//!
//! - An OpenCode 2 upgrade leaves both tables in the database. A session in
//!   both is read from `session_v2`: OpenCode 2 copies each OpenCode 1 session
//!   there under the same id and writes new activity only to the copy.
//! - A session's locator is `<database-file>/<session-id>.jsonl`, a path to no
//!   file: its parent is the database and its stem the session id.
//! - The locator meets the two requirements on
//!   [`SessionStub`](crate::history::provider::SessionStub): a stable final
//!   component and a printable `--show-path` value. No other code decodes it.
//! - Each projection keeps the dialogue and skips request framing, snapshot
//!   ids and event queues.
//! - Unknown part and message types are not errors.
//! - Every query names its columns, because OpenCode migrates its schema
//!   between versions.

use super::dropped::DroppedRecords;
use super::{
    EarlierTurns, SessionFormat, SessionHeader, SessionProjection, at_own_lines, rename_key,
    rfc3339_from_millis,
};
use crate::agent::transcript::tool_result_text;
use crate::error::{Result, SessionDatabaseFailure};
use crate::history::Source;
use crate::history::provider::sqlite::{
    DEFAULT_BUSY_TIMEOUT, open_session_list, unusable_database,
};
use crate::history::provider::subagents::SubagentForest;
use crate::log_entry::{
    AssistantMessage, ContentBlock, LogEntry, TokenUsage, Tool, UserContent, UserMessage,
};
use rusqlite::Connection;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

mod opencode_1;
mod opencode_2;

pub struct OpenCodeDbFormat;

pub static OPENCODE_DB: OpenCodeDbFormat = OpenCodeDbFormat;

impl SessionFormat for OpenCodeDbFormat {
    fn parse_transcript_counting(
        &self,
        path: &Path,
        dropped: DroppedRecords,
    ) -> Result<Option<SessionProjection>> {
        let Some(reference) = decode_ref(path) else {
            return Ok(None);
        };
        let connection = open_session_list(&reference.database, DEFAULT_BUSY_TIMEOUT)?;
        project_session(&connection, &reference, dropped)
    }
}

/// Every OpenCode session id starts with this. The locator codec here and
/// the launcher's stem check both rely on it to tell session stems apart
/// from other files' names. A stem the database itself listed needs no more
/// than the prefix; a typed query is held to the full shape by the provider's
/// `is_session_id_shape`, so the two checks stay separate.
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
pub(crate) const NEWEST_VERIFIED_MIGRATION: &str = "20261007190000_azure_cli_external_credential";

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
    mut dropped: DroppedRecords,
) -> Result<Option<SessionProjection>> {
    let cannot_be_read = |error: rusqlite::Error| {
        unusable_database(
            &reference.database,
            SessionDatabaseFailure::CannotBeRead,
            &error,
        )
    };
    let tables = SessionTables::of(connection).map_err(cannot_be_read)?;
    let mut found = None;
    for table in tables.reading_order() {
        if let Some(session) =
            session_row(connection, table, &reference.session_id).map_err(cannot_be_read)?
        {
            found = Some((table, session));
            break;
        }
    }
    let Some((table, session)) = found else {
        return Ok(None);
    };
    dropped.record_version(|| {
        connection
            .query_row(
                &format!("SELECT version FROM {} WHERE id = ?1", table.name()),
                [&reference.session_id],
                |row| row.get::<_, String>(0),
            )
            .ok()
    });
    let EntrySink {
        mut entries,
        dropped,
        ..
    } = match table {
        SessionTable::OpenCode1 => opencode_1::entries(connection, &reference.session_id, dropped),
        SessionTable::OpenCode2 => opencode_2::entries(connection, &reference.session_id, dropped),
    }
    .map_err(cannot_be_read)?;

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
            subagent_identity: Default::default(),
        },
        title,
        entries: at_own_lines(entries),
        leaf_id: None,
        malformed_lines: Vec::new(),
        dropped,
        earlier_turns: EarlierTurns::Read,
    }))
}

/// The two tables a session row can live in.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SessionTable {
    /// `session`, with `message` and `part` rows.
    OpenCode1,
    /// `session_v2`, with `session_message` rows.
    OpenCode2,
}

impl SessionTable {
    pub(crate) fn name(self) -> &'static str {
        match self {
            SessionTable::OpenCode1 => "session",
            SessionTable::OpenCode2 => "session_v2",
        }
    }
}

/// The session tables a database holds: OpenCode 1's, OpenCode 2's, or both
/// in a database OpenCode 2 upgraded.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SessionTables {
    opencode_1: bool,
    opencode_2: bool,
}

impl SessionTables {
    pub(crate) fn of(connection: &Connection) -> rusqlite::Result<Self> {
        Ok(Self {
            opencode_1: table_exists(connection, SessionTable::OpenCode1.name())?,
            opencode_2: table_exists(connection, SessionTable::OpenCode2.name())?,
        })
    }

    /// The tables the database holds, OpenCode 2's first.
    pub(crate) fn present(self) -> impl Iterator<Item = SessionTable> {
        [
            (self.opencode_2, SessionTable::OpenCode2),
            (self.opencode_1, SessionTable::OpenCode1),
        ]
        .into_iter()
        .filter_map(|(present, table)| present.then_some(table))
    }

    /// The tables to look a session up in, OpenCode 2's first. A database
    /// with neither is looked up in OpenCode 1's table, so its query fails
    /// and reports the database unreadable.
    fn reading_order(self) -> Vec<SessionTable> {
        if self.opencode_1 || self.opencode_2 {
            self.present().collect()
        } else {
            vec![SessionTable::OpenCode1]
        }
    }

    /// One query over every session: a session in both tables from
    /// `opencode_2`, every other from the table holding it. `opencode_1` and
    /// `opencode_2` select the same columns and each ends with
    /// `FROM <table> s`. A database with neither runs `opencode_1`, which
    /// fails.
    pub(crate) fn select_sessions(self, opencode_1: &str, opencode_2: &str) -> String {
        match (self.opencode_1, self.opencode_2) {
            (true, true) => format!(
                "{opencode_2} UNION ALL {opencode_1} WHERE s.id NOT IN (SELECT id FROM session_v2)"
            ),
            (false, true) => opencode_2.to_owned(),
            (_, false) => opencode_1.to_owned(),
        }
    }
}

struct SessionRow {
    parent_id: Option<String>,
    directory: String,
    title: String,
    time_created: i64,
}

fn session_row(
    connection: &Connection,
    table: SessionTable,
    session_id: &str,
) -> rusqlite::Result<Option<SessionRow>> {
    let mut statement = connection.prepare(&format!(
        "SELECT parent_id, directory, COALESCE(title, ''), time_created FROM {} WHERE id = ?1",
        table.name()
    ))?;
    let mut rows = statement.query([session_id])?;
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

/// The projection under construction, accumulated across messages. OpenCode
/// numbers an entry by the database part it came from; a finishing call
/// updates its part in place, so the call keeps its origin when an earlier
/// call's result is added. The last model named suppresses repeated model
/// markers.
#[derive(Default)]
struct EntrySink {
    entries: Vec<(usize, LogEntry)>,
    last_model: Option<String>,
    dropped: DroppedRecords,
}

impl EntrySink {
    fn push(&mut self, line: usize, entry: LogEntry) {
        self.entries.push((line, entry));
    }
}

/// The input `text` narrates, or `None` when it narrates no read. The
/// prefixes are OpenCode's own literals (session/prompt.ts); a rewording
/// upstream degrades to skipping the run, never to rendering it inline. An
/// MCP resource is addressed as the `file_path` of the read, which is what
/// the header shows.
fn injected_read_input_from_text(text: &str) -> Option<Value> {
    if let Some(arguments) = text.strip_prefix("Called the Read tool with the following input: ") {
        return Some(
            serde_json::from_str(arguments).unwrap_or_else(|_| json!({ "input": arguments })),
        );
    }
    text.strip_prefix("Reading MCP resource: ")
        .map(|resource| json!({ "file_path": resource }))
}

/// What a tool part that names neither its call nor its tool renders as —
/// the viewer needs something to show, and dropping the part would hide a
/// call the session made.
const UNKNOWN: &str = "unknown";

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

fn tool_result_entry(call_id: String, output: &str, timestamp: Option<String>) -> LogEntry {
    user_entry(vec![tool_result_block(call_id, output)], timestamp)
}

fn tool_result_block(call_id: String, output: &str) -> ContentBlock {
    ContentBlock::ToolResult {
        tool_use_id: call_id,
        content: Some(json!(tool_result_text(&json!(output)).unwrap_or_default())),
        standalone_tool_name: None,
    }
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

/// OpenCode 2 renamed `bash` to `shell` and `task` to `subagent`; a session
/// OpenCode 2 migrated keeps the OpenCode 1 names.
fn canonical_tool(name: &str) -> Tool {
    match name {
        "bash" | "shell" => Tool::Shell,
        "read" => Tool::Read,
        "edit" => Tool::Edit,
        "write" => Tool::Write,
        "grep" => Tool::Grep,
        "glob" => Tool::Glob,
        // OpenCode removed `list` in 1.0.201; older sessions hold its calls.
        "list" => Tool::DirectoryListing,
        "webfetch" => Tool::WebFetch,
        "todowrite" => Tool::TaskList,
        "task" | "subagent" => Tool::Agent,
        "skill" => Tool::Skill,
        _ => Tool::Other,
    }
}

/// Renames the keys the headers and diff bodies read to their canonical names;
/// every other key passes through. Each pair is OpenCode 1's key, then
/// OpenCode 2's for the same argument.
fn canonicalize_input(tool: Tool, input: &mut Value) {
    let Some(arguments) = input.as_object_mut() else {
        return;
    };
    if tool.takes_file_path() {
        rename_key(arguments, "filePath", "file_path");
        rename_key(arguments, "path", "file_path");
    }
    match tool {
        Tool::Edit => {
            rename_key(arguments, "oldString", "old_string");
            rename_key(arguments, "newString", "new_string");
        }
        Tool::Grep => rename_key(arguments, "include", "glob"),
        Tool::Skill => {
            rename_key(arguments, "name", "skill");
            rename_key(arguments, "id", "skill");
        }
        Tool::Agent => rename_key(arguments, "agent", "subagent_type"),
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

/// One model request's tokens, from the `tokens` object of an OpenCode 1
/// `step-finish` part or an OpenCode 2 assistant message, as an invisible
/// metadata entry. Each is the per-request grain; OpenCode 1's message- and
/// session-level token columns restate these totals and reading them too
/// would double-count.
fn usage_entry(part: &Value) -> Option<LogEntry> {
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

/// Every session and the parent it names, from both tables, as the forest
/// discovery and delete read sub-agent sessions from. The failure is
/// SQLite's own, so the caller can word it for the list.
pub(crate) fn session_forest(connection: &Connection) -> rusqlite::Result<SubagentForest<String>> {
    let sql = SessionTables::of(connection)?.select_sessions(
        "SELECT s.id, s.parent_id FROM session s",
        "SELECT s.id, s.parent_id FROM session_v2 s",
    );
    let mut statement = connection.prepare(&sql)?;
    let rows = statement.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
    })?;
    let sessions = rows.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(SubagentForest::new(sessions))
}

fn table_exists(connection: &Connection, table: &str) -> rusqlite::Result<bool> {
    connection.query_row(
        "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
        [table],
        |row| row.get(0),
    )
}

/// The id of every sub-agent row of `reference`, nested ones included,
/// in id order.
pub(crate) fn subagent_session_ids(
    connection: &Connection,
    reference: &SessionRef,
) -> rusqlite::Result<Vec<String>> {
    Ok(session_forest(connection)?.subagents_of(&reference.session_id))
}

fn nonempty_text(part: &Value, field: &str) -> Option<String> {
    part.get(field)
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
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
    use super::{OPENCODE_DB, SessionFormat, SessionProjection, session_ref};
    use rusqlite::Connection;
    use std::path::{Path, PathBuf};

    pub(crate) fn database_in(directory: &Path) -> PathBuf {
        let database = directory.join("opencode.db");
        create_database(&database);
        database
    }

    pub(crate) fn entry_json(projection: &SessionProjection) -> String {
        projection
            .entries
            .iter()
            .map(|(_, entry)| serde_json::to_string(entry).unwrap())
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub(crate) fn opencode_2_projection(database: &Path, session_id: &str) -> SessionProjection {
        OPENCODE_DB
            .parse_transcript(&session_ref(database, session_id))
            .unwrap()
            .unwrap()
    }

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

    /// A database as OpenCode 2 creates it: `session_v2` and
    /// `session_message`, and no `session`, `message` or `part` table.
    pub(crate) fn create_opencode_2_database(path: &Path) -> Connection {
        let connection = Connection::open(path).unwrap();
        add_opencode_2_tables(&connection);
        connection
    }

    /// The two OpenCode 2 session tables, transcribed from v2.0.26's
    /// `packages/core/src/database/schema.gen.ts`, keeping the `NOT NULL`
    /// columns without a default and the ones these tests read. OpenCode 2
    /// adds them beside OpenCode 1's tables when it upgrades a database.
    pub(crate) fn add_opencode_2_tables(connection: &Connection) {
        connection
            .execute_batch(include_str!(
                "../../../tests/fixtures/opencode/v2-schema.sql"
            ))
            .unwrap();
    }

    /// An OpenCode 2 session row with one user message.
    pub(crate) fn insert_opencode_2_session(
        connection: &Connection,
        id: &str,
        parent_id: Option<&str>,
    ) {
        insert_opencode_2_session_row(connection, id, parent_id);
        insert_opencode_2_message(
            connection,
            id,
            0,
            "user",
            &serde_json::json!({ "text": "opencode 2 question" }),
        );
    }

    /// An OpenCode 2 session row, with no messages.
    pub(crate) fn insert_opencode_2_session_row(
        connection: &Connection,
        id: &str,
        parent_id: Option<&str>,
    ) {
        connection
            .execute(
                "INSERT INTO session_v2 (id, project_id, parent_id, slug, directory, title,
                                         version, time_created, time_updated)
                 VALUES (?1, 'proj_fixture', ?2, ?1, '/tmp/fixture', 'opencode 2 session',
                         '2.0.26', 1755000100000, 1755000100000)",
                rusqlite::params![id, parent_id],
            )
            .unwrap();
    }

    /// The `seq`th message of an OpenCode 2 session: its type in a column and
    /// `data`, the rest of the message, as JSON. Its row times come from
    /// `data`'s `time.created`, as OpenCode 2 writes them.
    pub(crate) fn insert_opencode_2_message(
        connection: &Connection,
        session_id: &str,
        seq: i64,
        message_type: &str,
        data: &serde_json::Value,
    ) {
        let created = data
            .pointer("/time/created")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(1755000100000);
        connection
            .execute(
                "INSERT INTO session_message (id, session_id, type, seq, time_created,
                                              time_updated, data)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?5, ?6)",
                rusqlite::params![
                    format!("msg_{session_id}_{seq}"),
                    session_id,
                    message_type,
                    seq,
                    created,
                    data.to_string()
                ],
            )
            .unwrap();
    }

    /// A top-level OpenCode 2 session in the shapes OpenCode 2.0.26 writes:
    /// a user turn; an assistant reply with reasoning, a completed `read`
    /// (OpenCode 2's `path` argument), a failed `shell` call and text, with
    /// its tokens; framing to skip (`system`, `idle`); a command the user ran
    /// that exited non-zero; and a completed compaction.
    pub(crate) fn standard_opencode_2_session(connection: &Connection, session_id: &str) {
        insert_opencode_2_session_row(connection, session_id, None);
        let model =
            serde_json::json!({ "id": "gpt-6-luna", "providerID": "openai", "variant": "default" });
        let messages = [
            (
                "user",
                serde_json::json!({
                    "text": "find the opencode 2 defect",
                    "time": { "created": 1755000110000i64 },
                }),
            ),
            (
                "assistant",
                serde_json::json!({
                    "agent": "build",
                    "model": model,
                    "content": [
                        { "type": "reasoning", "text": "weighing the opencode 2 layout",
                          "time": { "created": 1755000120000i64 } },
                        { "type": "tool", "id": "call_read", "name": "read",
                          "state": { "status": "completed", "input": { "path": "lib.rs" },
                                     "content": [{ "type": "text", "text": "fn opencode_2_defect() {}" }] },
                          "time": { "created": 1755000121000i64 } },
                        { "type": "tool", "id": "call_shell", "name": "shell",
                          "state": { "status": "error", "input": { "command": "cargo test" },
                                     "error": { "type": "tool.execution",
                                                "message": "OPENCODE_2_TOOL_ERROR" } },
                          "time": { "created": 1755000122000i64 } },
                        { "type": "text", "text": "the opencode 2 defect hides in lib.rs" },
                    ],
                    "finish": "stop",
                    "tokens": { "input": 40, "output": 15, "reasoning": 5,
                                "cache": { "read": 2, "write": 0 } },
                    "time": { "created": 1755000120000i64, "completed": 1755000130000i64 },
                }),
            ),
            (
                "system",
                serde_json::json!({
                    "text": "OPENCODE_2_SYSTEM_FRAMING",
                    "time": { "created": 1755000130000i64 },
                }),
            ),
            (
                "idle",
                serde_json::json!({ "outcome": "succeeded", "time": { "created": 1755000131000i64 } }),
            ),
            (
                "shell",
                serde_json::json!({
                    "shellID": "sh_1",
                    "command": "ls opencode_2_dir",
                    "status": "exited",
                    "exit": 2,
                    "output": { "output": "OPENCODE_2_SHELL_OUTPUT", "truncated": false },
                    "time": { "created": 1755000140000i64 },
                }),
            ),
            (
                "compaction",
                serde_json::json!({
                    "status": "completed",
                    "reason": "auto",
                    "summary": "OPENCODE_2_COMPACTION_SUMMARY",
                    "recent": "",
                    "time": { "created": 1755000150000i64 },
                }),
            ),
        ];
        for (seq, (message_type, data)) in messages.iter().enumerate() {
            insert_opencode_2_message(connection, session_id, seq as i64, message_type, data);
        }
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
    use fixture::{SessionSpec, database_in, entry_json, opencode_2_projection};

    /// A session OpenCode 2 migrated keeps its OpenCode 1 rows, frozen at the
    /// upgrade; what came after is only in its OpenCode 2 copy.
    #[test]
    fn migrated_session_reads_from_its_opencode_2_copy() {
        let directory = tempfile::tempdir().unwrap();
        let database = database_in(directory.path());
        let connection = Connection::open(&database).unwrap();
        fixture::standard_session(&connection, "ses_migrated");
        fixture::add_opencode_2_tables(&connection);
        fixture::insert_opencode_2_session_row(&connection, "ses_migrated", None);
        fixture::insert_opencode_2_message(
            &connection,
            "ses_migrated",
            0,
            "user",
            &json!({ "text": "CONTINUED_AFTER_UPGRADE", "time": { "created": 1755000900000i64 } }),
        );
        drop(connection);

        let rendered = entry_json(&opencode_2_projection(&database, "ses_migrated"));

        assert!(rendered.contains("CONTINUED_AFTER_UPGRADE"));
        assert!(
            !rendered.contains("find the fixture defect"),
            "the OpenCode 1 rows are not read beside the copy"
        );
    }

    #[test]
    fn row_numbers_messages_as_agent_read_after_empty_outputs() {
        const REPLY: &str = "Both commands printed nothing, so the notes file is empty.";
        let directory = tempfile::tempdir().unwrap();
        let database = database_in(directory.path());
        let connection = Connection::open(&database).unwrap();
        fixture::add_opencode_2_tables(&connection);
        fixture::insert_opencode_2_session_row(&connection, "ses_numbering", None);
        let messages = [
            (
                "user",
                json!({ "text": "Is the notes file empty?", "time": { "created": 1755000110000i64 } }),
            ),
            (
                "assistant",
                json!({
                    "agent": "build",
                    "content": [
                        { "type": "tool", "id": "call_shell", "name": "shell",
                          "state": { "status": "completed", "input": { "command": "cat notes.txt" },
                                     "content": [] },
                          "time": { "created": 1755000121000i64 } },
                    ],
                    "time": { "created": 1755000120000i64, "completed": 1755000122000i64 },
                }),
            ),
            (
                "shell",
                json!({
                    "shellID": "sh_1",
                    "command": "cat notes.txt",
                    "status": "exited",
                    "exit": 0,
                    "output": { "output": "", "truncated": false },
                    "time": { "created": 1755000130000i64 },
                }),
            ),
            (
                "assistant",
                json!({
                    "agent": "build",
                    "content": [{ "type": "text", "text": REPLY }],
                    "time": { "created": 1755000140000i64, "completed": 1755000141000i64 },
                }),
            ),
        ];
        for (seq, (message_type, data)) in messages.iter().enumerate() {
            fixture::insert_opencode_2_message(
                &connection,
                "ses_numbering",
                seq as i64,
                message_type,
                data,
            );
        }
        drop(connection);

        crate::agent::test_support::assert_row_numbers_messages_as_agent_read(
            Source::OpenCode,
            &session_ref(&database, "ses_numbering"),
            5,
            &[REPLY],
        );
    }

    #[test]
    fn sub_agent_session_carries_no_title() {
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
    fn view_splices_subagent_sessions_and_plain_parse_does_not() {
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
    fn migration_journal_flags_only_databases_beyond_pin() {
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
    fn unreadable_database_fails_rather_than_answering_absent() {
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
            ("list", Tool::DirectoryListing),
            ("webfetch", Tool::WebFetch),
            ("todowrite", Tool::TaskList),
            ("task", Tool::Agent),
            ("skill", Tool::Skill),
            ("question", Tool::Other),
            ("shell", Tool::Shell),
            ("subagent", Tool::Agent),
        ];
        for (name, tool) in expected {
            assert_eq!(tool_use(name, json!({})).0, tool, "{name}");
        }
    }

    #[test]
    fn opencode_keys_are_renamed_to_canonical_ones_and_rest_kept() {
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
        assert_eq!(
            tool_use("skill", json!({"name": "pdf-tools"})).1,
            json!({"skill": "pdf-tools"})
        );
        assert_eq!(tool_use("read", json!({})), (Tool::Read, json!({})));
    }

    #[test]
    fn opencode_2_keys_are_renamed_to_canonical_ones() {
        assert_eq!(
            tool_use("read", json!({"path": "src/lib.rs"})).1,
            json!({"file_path": "src/lib.rs"})
        );
        assert_eq!(
            tool_use(
                "subagent",
                json!({"agent": "general", "description": "scout", "prompt": "Look."})
            )
            .1,
            json!({"subagent_type": "general", "description": "scout", "prompt": "Look."})
        );
        assert_eq!(
            tool_use("skill", json!({"id": "pdf-tools"})).1,
            json!({"skill": "pdf-tools"})
        );
        assert_eq!(
            tool_use("glob", json!({"pattern": "*.rs", "path": "src"})).1,
            json!({"pattern": "*.rs", "path": "src"}),
            "a directory to search keeps its key"
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
