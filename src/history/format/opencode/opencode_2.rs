//! OpenCode 2's session rows: one `session_message` row per message in `seq`
//! order, with its type in a column and the rest of the message as JSON in
//! `data`.
//!
//! | Message type                            | Projection                                |
//! |-----------------------------------------|-------------------------------------------|
//! | `user`                                  | user text and a line per attached file    |
//! | `assistant`                             | text, reasoning, tool calls and usage     |
//! | `shell`                                 | the user's shell command, with its output |
//! | `synthetic`                             | its injected reads, as `read` calls       |
//! | `compaction`, completed                 | its summary                               |
//! | `system`, `idle`, `skill`, `*-switched` | nothing                                   |
//! | any other                               | nothing                                   |
//!
//! An assistant message's usage comes from its `tokens`: OpenCode 2 writes one
//! assistant message per model request.

use super::{
    EntrySink, UNKNOWN, assistant_entry, compaction_summary, injected_read_input_from_text,
    nonempty_text, tool_result_block, tool_result_entry, tool_use_block, usage_entry, user_entry,
};
use crate::history::format::dropped::DroppedRecords;
use crate::history::format::{append_exit_code, rfc3339_from_millis};
use crate::log_entry::{ContentBlock, LogEntry, Tool};
use rusqlite::Connection;
use serde_json::{Value, json};

/// The session's entries, and the records dropped on the way, counted in
/// `dropped`. A row whose JSON does not parse is counted rather than failing
/// the session.
pub(super) fn entries(
    connection: &Connection,
    session_id: &str,
    dropped: DroppedRecords,
) -> rusqlite::Result<EntrySink> {
    let mut statement = connection
        .prepare("SELECT id, type, data FROM session_message WHERE session_id = ?1 ORDER BY seq")?;
    let rows = statement.query_map([session_id], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
        ))
    })?;
    let mut projection = Projection {
        sink: EntrySink {
            dropped,
            ..EntrySink::default()
        },
        line: 0,
    };
    for row in rows {
        let (message_id, message_type, data) = row?;
        match serde_json::from_str::<Value>(&data) {
            Ok(message) => projection.message(&message_id, &message_type, &message),
            Err(_) => projection.sink.dropped.failed("session_message.data"),
        }
    }
    Ok(projection.sink)
}

/// The projection under construction. Entries are numbered in order by the
/// message, or the assistant content item, they came from, so a call and its
/// result share a number as an OpenCode 1 tool part's do.
struct Projection {
    sink: EntrySink,
    line: usize,
}

impl Projection {
    fn next_line(&mut self) -> usize {
        self.line += 1;
        self.line
    }

    fn message(&mut self, message_id: &str, message_type: &str, message: &Value) {
        let timestamp = millis_timestamp(message.pointer("/time/created"));
        match message_type {
            "user" => self.user(message, timestamp),
            "synthetic" => self.injected_reads(message_id, message, timestamp),
            "assistant" => self.assistant(message_id, message, timestamp),
            "shell" => self.user_shell_command(message_id, message, timestamp),
            "compaction" => self.compaction(message, timestamp),
            "system" | "idle" | "skill" | "agent-switched" | "model-switched"
            | "location-switched" => self
                .sink
                .dropped
                .skipped("session_message.type", Some(message_type)),
            unknown => self
                .sink
                .dropped
                .unrecognised("session_message.type", Some(unknown)),
        }
    }

    /// The user's text, then one line per attached file.
    fn user(&mut self, message: &Value, timestamp: Option<String>) {
        let line = self.next_line();
        let blocks = nonempty_text(message, "text")
            .into_iter()
            .chain(attachment_lines(message))
            .map(|text| ContentBlock::Text { text })
            .collect::<Vec<_>>();
        if !blocks.is_empty() {
            self.sink.push(line, user_entry(blocks, timestamp));
        }
    }

    /// The reads a synthetic message narrates, as OpenCode 1's synthetic
    /// parts project. A migrated message joins its OpenCode 1 parts with a
    /// blank line: each paragraph narrating a read opens a call, and the
    /// paragraphs after it are that call's result. Paragraphs before the
    /// first narration are framing.
    fn injected_reads(&mut self, message_id: &str, message: &Value, timestamp: Option<String>) {
        let line = self.next_line();
        let Some(text) = message.get("text").and_then(Value::as_str) else {
            return;
        };
        let mut reads: Vec<(Value, Vec<&str>)> = Vec::new();
        for paragraph in text.split("\n\n") {
            match injected_read_input_from_text(paragraph) {
                Some(input) => reads.push((input, Vec::new())),
                None => {
                    if let Some((_, contents)) = reads.last_mut() {
                        contents.push(paragraph);
                    }
                }
            }
        }
        for (index, (input, contents)) in reads.into_iter().enumerate() {
            let call_id = format!("{message_id}:{index}");
            self.sink.push(
                line,
                assistant_entry(
                    Some(call_id.clone()),
                    vec![tool_use_block(call_id.clone(), "read", input)],
                    timestamp.clone(),
                ),
            );
            self.sink.push(
                line,
                tool_result_entry(call_id, &contents.join("\n\n"), timestamp.clone()),
            );
        }
    }

    /// An assistant message's content items, with a model marker when it
    /// names a model the previous one did not, and its usage. A message with
    /// no content yet shows nothing, as an OpenCode 1 message with no parts.
    fn assistant(&mut self, message_id: &str, message: &Value, timestamp: Option<String>) {
        let content = message
            .get("content")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default();
        if content.is_empty() {
            return;
        }
        if let Some(model) = message
            .pointer("/model/id")
            .and_then(Value::as_str)
            .filter(|model| !model.is_empty())
            && self.sink.last_model.as_deref() != Some(model)
        {
            self.sink.last_model = Some(model.to_owned());
            self.sink.push(
                self.line + 1,
                LogEntry::PiMetadata {
                    label: "Model".to_owned(),
                    text: model.to_owned(),
                    timestamp: timestamp.clone(),
                    searchable: false,
                    usage: None,
                },
            );
        }
        for (index, item) in content.iter().enumerate() {
            let line = self.next_line();
            // Content items carry no ids of their own; entries sharing an id
            // would collapse into one turn when the agent transcript
            // deduplicates repeated assistant ids.
            let entry_id = Some(format!("{message_id}:{index}"));
            let item_timestamp =
                millis_timestamp(item.pointer("/time/created")).or_else(|| timestamp.clone());
            match item.get("type").and_then(Value::as_str) {
                Some("text") => {
                    if let Some(text) = nonempty_text(item, "text") {
                        self.sink.push(
                            line,
                            assistant_entry(
                                entry_id,
                                vec![ContentBlock::Text { text }],
                                item_timestamp,
                            ),
                        );
                    }
                }
                Some("reasoning") => {
                    if let Some(thinking) = nonempty_text(item, "text") {
                        self.sink.push(
                            line,
                            assistant_entry(
                                entry_id,
                                vec![ContentBlock::Thinking {
                                    thinking,
                                    signature: String::new(),
                                }],
                                item_timestamp,
                            ),
                        );
                    }
                }
                Some("tool") => self.tool_call(line, entry_id, item, item_timestamp),
                unknown => self.sink.dropped.unrecognised("content.type", unknown),
            }
        }
        if let Some(usage) = usage_entry(message) {
            self.sink.push(self.line, usage);
        }
    }

    /// A tool content item: the call, and its result once the call has
    /// finished.
    fn tool_call(
        &mut self,
        line: usize,
        entry_id: Option<String>,
        item: &Value,
        timestamp: Option<String>,
    ) {
        let call_id = item
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or(UNKNOWN)
            .to_owned();
        // A call still streaming holds its input as unparsed text.
        let input = item
            .pointer("/state/input")
            .filter(|input| input.is_object())
            .cloned()
            .unwrap_or(json!({}));
        self.sink.push(
            line,
            assistant_entry(
                entry_id,
                vec![tool_use_block(
                    call_id.clone(),
                    item.get("name").and_then(Value::as_str).unwrap_or(UNKNOWN),
                    input,
                )],
                timestamp.clone(),
            ),
        );
        if let Some(output) = tool_output(item) {
            self.sink
                .push(line, tool_result_entry(call_id, &output, timestamp));
        }
    }

    /// A command the user ran in OpenCode, which the model did not call: its
    /// call and its output in one user turn.
    fn user_shell_command(&mut self, message_id: &str, message: &Value, timestamp: Option<String>) {
        let line = self.next_line();
        let Some(command) = message.get("command").and_then(Value::as_str) else {
            return;
        };
        let mut output = message
            .pointer("/output/output")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        if let Some(code) = message
            .get("exit")
            .and_then(Value::as_i64)
            .filter(|code| *code != 0)
        {
            append_exit_code(&mut output, code);
        }
        let call_id = message_id.to_owned();
        self.sink.push(
            line,
            user_entry(
                vec![
                    ContentBlock::ToolUse {
                        id: call_id.clone(),
                        name: "shell".to_owned(),
                        tool: Tool::UserShell,
                        input: json!({ "command": command }),
                    },
                    tool_result_block(call_id, &output),
                ],
                timestamp,
            ),
        );
    }

    fn compaction(&mut self, message: &Value, timestamp: Option<String>) {
        let line = self.next_line();
        let status = message.get("status").and_then(Value::as_str);
        if status != Some("completed") {
            self.sink.dropped.skipped("compaction.status", status);
            return;
        }
        if let Some(summary) = compaction_summary(message, timestamp) {
            self.sink.push(line, summary);
        }
    }
}

/// A line for each file attached to a user message, written as OpenCode 2
/// writes one when it summarises a session (`messageToText` in
/// `session/compaction.ts`). The file's contents are not shown: OpenCode 2
/// stores them inline as base64.
fn attachment_lines(message: &Value) -> impl Iterator<Item = String> + '_ {
    message
        .get("files")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|file| {
            let mime = file.get("mime").and_then(Value::as_str)?;
            let name = file
                .get("name")
                .and_then(Value::as_str)
                .or_else(|| {
                    (file.pointer("/source/type").and_then(Value::as_str) == Some("uri"))
                        .then(|| file.pointer("/source/uri").and_then(Value::as_str))
                        .flatten()
                })
                .unwrap_or(INLINE_ATTACHMENT);
            Some(format!("[Attached {mime}: {name}]"))
        })
}

/// OpenCode 2's label for an attachment with neither a name nor a URI.
const INLINE_ATTACHMENT: &str = "inline attachment";

/// The text a finished call returned: its text content, or, for a failed
/// call that returned none, its error message. A call still streaming or
/// running has returned nothing.
fn tool_output(item: &Value) -> Option<String> {
    let state = item.get("state")?;
    let text = state
        .get("content")
        .and_then(Value::as_array)
        .map(|content| {
            content
                .iter()
                .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|block| block.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default();
    match state.get("status").and_then(Value::as_str)? {
        "completed" => Some(text),
        "error" if !text.is_empty() => Some(text),
        "error" => state
            .pointer("/error/message")
            .and_then(Value::as_str)
            .map(str::to_owned),
        _ => None,
    }
}

fn millis_timestamp(millis: Option<&Value>) -> Option<String> {
    millis.and_then(Value::as_i64).and_then(rfc3339_from_millis)
}

#[cfg(test)]
mod tests {
    use super::super::fixture::{self, entry_json, opencode_2_projection};
    use super::super::{OPENCODE_DB, SessionRef, session_ref, subagent_session_ids};
    use super::*;
    use crate::history::format::dropped::DropOutcome;
    use crate::history::format::{SessionFormat, SessionProjection, view_projection};
    use crate::log_entry::{UserContent, UserMessage};
    use std::path::PathBuf;

    #[test]
    fn counting_names_skipped_and_unrecognised_opencode_2_message_types_and_items() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("opencode.db");
        let connection = fixture::create_opencode_2_database(&database);
        fixture::insert_opencode_2_session_row(&connection, "ses_v2_counted", None);
        let messages = [
            ("system", json!({ "text": "framing" })),
            (
                "teleport",
                json!({ "text": "a type OpenCode 2.0.26 does not write" }),
            ),
            (
                "assistant",
                json!({ "content": [
                    { "type": "hologram" },
                    { "type": "text", "text": "the reply" },
                ] }),
            ),
            ("compaction", json!({ "status": "pending" })),
        ];
        for (seq, (message_type, data)) in messages.iter().enumerate() {
            fixture::insert_opencode_2_message(
                &connection,
                "ses_v2_counted",
                seq as i64,
                message_type,
                data,
            );
        }
        connection
            .execute(
                "INSERT INTO session_message (id, session_id, type, seq, time_created,
                                              time_updated, data)
                 VALUES ('msg_unparsed', 'ses_v2_counted', 'user', 9, 1755000100000,
                         1755000100000, 'not json')",
                [],
            )
            .unwrap();
        drop(connection);

        let projection = OPENCODE_DB
            .parse_transcript_counting(
                &session_ref(&database, "ses_v2_counted"),
                DroppedRecords::counting(),
            )
            .unwrap()
            .unwrap();

        let version = Some("2.0.26");
        assert_eq!(
            projection.dropped.counts().collect::<Vec<_>>(),
            [
                (
                    version,
                    "compaction.status=pending",
                    DropOutcome::Skipped,
                    1
                ),
                (
                    version,
                    "content.type=hologram",
                    DropOutcome::Unrecognised,
                    1
                ),
                (version, "session_message.data", DropOutcome::Failed, 1),
                (
                    version,
                    "session_message.type=system",
                    DropOutcome::Skipped,
                    1
                ),
                (
                    version,
                    "session_message.type=teleport",
                    DropOutcome::Unrecognised,
                    1
                ),
            ]
        );
    }

    #[test]
    fn counting_leaves_an_opencode_2_projection_as_an_ordinary_parse_reads_it() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("opencode.db");
        fixture::standard_opencode_2_session(
            &fixture::create_opencode_2_database(&database),
            "ses_v2",
        );
        let reference = session_ref(&database, "ses_v2");

        let ordinary = OPENCODE_DB.parse_transcript(&reference).unwrap().unwrap();
        let counted = OPENCODE_DB
            .parse_transcript_counting(&reference, DroppedRecords::counting())
            .unwrap()
            .unwrap();

        assert_eq!(ordinary.dropped, DroppedRecords::default());
        assert_ne!(counted.dropped, DroppedRecords::default());
        assert_eq!(
            format!(
                "{:?}",
                SessionProjection {
                    dropped: DroppedRecords::default(),
                    ..counted
                }
            ),
            format!("{ordinary:?}")
        );
    }

    #[test]
    fn an_opencode_2_session_projects_its_dialogue_and_skips_framing() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("opencode.db");
        fixture::standard_opencode_2_session(
            &fixture::create_opencode_2_database(&database),
            "ses_v2",
        );

        let projection = opencode_2_projection(&database, "ses_v2");

        assert_eq!(projection.header.id, "ses_v2");
        assert_eq!(projection.header.cwd, PathBuf::from("/tmp/fixture"));
        assert_eq!(projection.title.as_deref(), Some("opencode 2 session"));
        let rendered = entry_json(&projection);
        for shown in [
            "find the opencode 2 defect",
            "weighing the opencode 2 layout",
            "fn opencode_2_defect() {}",
            "the opencode 2 defect hides in lib.rs",
            "gpt-6-luna",
            "OPENCODE_2_TOOL_ERROR",
            "ls opencode_2_dir",
            "OPENCODE_2_SHELL_OUTPUT",
            "Exit code: 2",
            "OPENCODE_2_COMPACTION_SUMMARY",
        ] {
            assert!(rendered.contains(shown), "{shown} missing: {rendered}");
        }
        assert!(
            !rendered.contains("OPENCODE_2_SYSTEM_FRAMING"),
            "a system message is framing, not dialogue"
        );
        let tools = projection
            .entries
            .iter()
            .flat_map(|(_, entry)| match entry {
                LogEntry::Assistant { message, .. } => message.content.clone(),
                LogEntry::User {
                    message:
                        UserMessage {
                            content: UserContent::Blocks(blocks),
                            ..
                        },
                    ..
                } => blocks.clone(),
                _ => Vec::new(),
            })
            .filter_map(|block| match block {
                ContentBlock::ToolUse { tool, input, .. } => Some((tool, input)),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            tools,
            [
                (Tool::Read, json!({ "file_path": "lib.rs" })),
                (Tool::Shell, json!({ "command": "cargo test" })),
                (Tool::UserShell, json!({ "command": "ls opencode_2_dir" })),
            ]
        );
    }

    #[test]
    fn an_opencode_2_reply_reaches_the_conversation_totals() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("opencode.db");
        fixture::standard_opencode_2_session(
            &fixture::create_opencode_2_database(&database),
            "ses_v2_tokens",
        );
        let stub = crate::history::provider::SessionStub {
            locator: session_ref(&database, "ses_v2_tokens"),
            subagents: Vec::new(),
            cache_key: "ses_v2_tokens".to_owned(),
            fingerprint: crate::history::provider::Fingerprint {
                size: 0,
                modified: None,
            },
        };

        let conversation =
            crate::history::parser::process_session_file(&stub, &OPENCODE_DB, None, &|| {})
                .unwrap()
                .unwrap();

        assert_eq!(conversation.total_tokens, 40 + 15 + 5 + 2);
        assert_eq!(conversation.model.as_deref(), Some("gpt-6-luna"));
    }

    /// OpenCode 2 joins a migrated user message's synthetic parts with a
    /// blank line into one synthetic message.
    #[test]
    fn an_opencode_2_synthetic_message_projects_the_reads_it_narrates() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("opencode.db");
        let connection = fixture::create_opencode_2_database(&database);
        fixture::insert_opencode_2_session_row(&connection, "ses_synthetic", None);
        let text = [
            "Summarize the task tool output above and continue with your task.",
            "Called the Read tool with the following input: {\"filePath\":\"/tmp/README.md\"}",
            "<content>",
            "INJECTED_README_SENTINEL",
            "</content>",
            "Reading MCP resource: docs://guide",
            "INJECTED_RESOURCE_SENTINEL",
        ]
        .join("\n\n");
        fixture::insert_opencode_2_message(
            &connection,
            "ses_synthetic",
            0,
            "synthetic",
            &json!({ "text": text }),
        );
        drop(connection);

        let projection = opencode_2_projection(&database, "ses_synthetic");
        let rendered = entry_json(&projection);

        assert!(rendered.contains("/tmp/README.md"));
        assert!(rendered.contains("INJECTED_README_SENTINEL"));
        assert!(rendered.contains("docs://guide"));
        assert!(rendered.contains("INJECTED_RESOURCE_SENTINEL"));
        assert!(!rendered.contains("Summarize the task tool output"));
        assert!(!rendered.contains("Called the Read tool"));
    }

    /// OpenCode 2 attaches an `@`-mentioned file to the user message, its
    /// contents inline as base64 (`Prompt.FileAttachment`).
    #[test]
    fn an_opencode_2_user_turn_names_each_attached_file() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("opencode.db");
        let connection = fixture::create_opencode_2_database(&database);
        fixture::insert_opencode_2_session_row(&connection, "ses_files", None);
        fixture::insert_opencode_2_message(
            &connection,
            "ses_files",
            0,
            "user",
            &json!({
                "text": "summarise @notes.txt and the screenshot",
                "files": [
                    { "data": "VGhlIHBlcml3aW5rbGUgbGVkZ2Vy", "mime": "text/plain",
                      "source": { "type": "inline" }, "name": "notes.txt" },
                    { "data": "iVBORw0KGgo=", "mime": "image/png",
                      "source": { "type": "uri", "uri": "file:///tmp/screenshot.png" } },
                    { "data": "AAAA", "mime": "application/octet-stream",
                      "source": { "type": "inline" } },
                ],
                "time": { "created": 1755000110000i64 },
            }),
        );
        drop(connection);

        let projection = opencode_2_projection(&database, "ses_files");
        let texts = projection
            .entries
            .iter()
            .flat_map(|(_, entry)| match entry {
                LogEntry::User {
                    message:
                        UserMessage {
                            content: UserContent::Blocks(blocks),
                            ..
                        },
                    ..
                } => blocks.clone(),
                _ => Vec::new(),
            })
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(text),
                _ => None,
            })
            .collect::<Vec<_>>();

        assert_eq!(
            texts,
            [
                "summarise @notes.txt and the screenshot",
                "[Attached text/plain: notes.txt]",
                "[Attached image/png: file:///tmp/screenshot.png]",
                "[Attached application/octet-stream: inline attachment]",
            ]
        );
    }

    #[test]
    fn the_view_splices_opencode_2_sub_agent_sessions() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("opencode.db");
        let connection = fixture::create_opencode_2_database(&database);
        fixture::standard_opencode_2_session(&connection, "ses_parent");
        fixture::insert_opencode_2_session_row(&connection, "ses_child", Some("ses_parent"));
        fixture::insert_opencode_2_message(
            &connection,
            "ses_child",
            0,
            "user",
            &json!({ "text": "OPENCODE_2_CHILD_PROMPT", "time": { "created": 1755000125000i64 } }),
        );
        drop(connection);

        let reference = session_ref(&database, "ses_parent");
        let view = view_projection(
            &OPENCODE_DB,
            &reference,
            &[session_ref(&database, "ses_child")],
        )
        .unwrap()
        .unwrap();

        assert!(entry_json(&view).contains("OPENCODE_2_CHILD_PROMPT"));
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
}
