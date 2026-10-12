//! OpenCode 2's session rows: one `session_message` row per message, its
//! type in a column and the rest of the message as JSON in `data`, in `seq`
//! order.
//!
//! The projection matches OpenCode 1's: user text, assistant text, reasoning
//! and tool calls carry the conversation, and each assistant message's
//! `tokens` carry usage, since OpenCode 2 writes one assistant message per
//! model request. A file attached to a user message projects as one line in
//! that turn, a command the user ran (`shell`) as one, an injected read in a
//! `synthetic` message as a `read` call, and a completed compaction as its
//! summary. The other message types (`system`, `idle`, `skill`, the
//! `*-switched` markers) are framing and yield nothing.

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
