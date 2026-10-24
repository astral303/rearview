//! OpenCode 1's session rows: `message` rows in conversation order, each with
//! its `part` rows, both as JSON in `data`.
//!
//! | Part                                           | Projection       |
//! |------------------------------------------------|------------------|
//! | `text`, `reasoning`, `tool`                    | the conversation |
//! | `step-finish`                                  | usage            |
//! | `compaction`                                   | its summary      |
//! | synthetic user text narrating an injected read | a `read` call    |
//! | any other                                      | nothing          |

use super::{
    EntrySink, UNKNOWN, assistant_entry, compaction_summary, injected_read_input_from_text,
    nonempty_text, tool_result_entry, tool_use_block, usage_entry, user_entry,
};
use crate::history::format::dropped::DroppedRecords;
use crate::history::format::rfc3339_from_millis;
use crate::log_entry::{ContentBlock, LogEntry};
use rusqlite::Connection;
use serde_json::{Value, json};
use std::collections::HashMap;

/// An OpenCode 1 session's entries, each numbered by the part it came from,
/// and the records dropped on the way, counted in `dropped`.
pub(super) fn entries(
    connection: &Connection,
    session_id: &str,
    mut dropped: DroppedRecords,
) -> rusqlite::Result<EntrySink> {
    let mut parts_by_message = parts_by_message(connection, session_id, &mut dropped)?;
    let messages = messages(connection, session_id, &mut dropped)?;
    let mut sink = EntrySink {
        dropped,
        ..EntrySink::default()
    };
    for (message_id, data) in messages {
        let parts = parts_by_message.remove(&message_id).unwrap_or_default();
        message_entries(&data, &parts, &mut sink);
    }
    Ok(sink)
}

/// The session's messages in conversation order: `(message id, decoded data)`.
/// A row whose JSON does not parse is skipped rather than failing the session,
/// and counted in `dropped`.
fn messages(
    connection: &Connection,
    session_id: &str,
    dropped: &mut DroppedRecords,
) -> rusqlite::Result<Vec<(String, Value)>> {
    let mut statement = connection
        .prepare("SELECT id, data FROM message WHERE session_id = ?1 ORDER BY time_created, id")?;
    let rows = statement.query_map([session_id], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut messages = Vec::new();
    for row in rows {
        let (id, data) = row?;
        match serde_json::from_str::<Value>(&data) {
            Ok(data) => messages.push((id, data)),
            Err(_) => dropped.failed("message.data"),
        }
    }
    Ok(messages)
}

/// One row of the session's `part` table: its decoded data, and its
/// position among the session's parts, from 1.
struct Part {
    position: usize,
    data: Value,
}

/// Every part of the session, grouped by message, in part order. A row whose
/// JSON does not parse is counted in `dropped`.
fn parts_by_message(
    connection: &Connection,
    session_id: &str,
    dropped: &mut DroppedRecords,
) -> rusqlite::Result<HashMap<String, Vec<Part>>> {
    let mut statement = connection.prepare(
        "SELECT message_id, data FROM part WHERE session_id = ?1 ORDER BY time_created, id",
    )?;
    let rows = statement.query_map([session_id], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut parts: HashMap<String, Vec<Part>> = HashMap::new();
    for (index, row) in rows.enumerate() {
        let (message_id, data) = row?;
        match serde_json::from_str::<Value>(&data) {
            Ok(data) => parts.entry(message_id).or_default().push(Part {
                position: index + 1,
                data,
            }),
            Err(_) => dropped.failed("part.data"),
        }
    }
    Ok(parts)
}

/// Project one message's parts into `sink`.
///
/// A user message becomes one `User` entry holding all its text blocks — one
/// turn, however many blocks carried it. An assistant message becomes one
/// entry per conversational part, so tool calls and results interleave in
/// part order, plus usage from its `step-finish` parts and a model marker
/// when the message names a model the previous one did not.
fn message_entries(message: &Value, parts: &[Part], sink: &mut EntrySink) {
    let role = message.get("role").and_then(Value::as_str).unwrap_or("");
    let message_timestamp = message
        .pointer("/time/created")
        .and_then(Value::as_i64)
        .and_then(rfc3339_from_millis);
    // A message with no parts yet shows nothing: its model marker waits for
    // the first part and takes that part's line, never another message's.
    let Some(first_line) = parts.first().map(|part| part.position) else {
        return;
    };

    match role {
        "user" => {
            let texts = parts
                .iter()
                .map(|part| &part.data)
                .filter(|part| part.get("type").and_then(Value::as_str) == Some("text"))
                .filter(|part| !is_synthetic(part) || is_comment_note(part))
                .filter_map(|part| part.get("text").and_then(Value::as_str))
                .filter(|text| !text.is_empty())
                .map(str::to_owned)
                .collect::<Vec<_>>();
            if !texts.is_empty() {
                let timestamp = parts
                    .first()
                    .and_then(|part| part_timestamp(&part.data))
                    .or_else(|| message_timestamp.clone());
                sink.push(
                    first_line,
                    user_entry(
                        texts
                            .into_iter()
                            .map(|text| ContentBlock::Text { text })
                            .collect(),
                        timestamp,
                    ),
                );
            }
            injected_read_entries(parts, &message_timestamp, sink);
        }
        "assistant" => {
            if let Some(model) = model_name(message)
                && sink.last_model.as_deref() != Some(model)
            {
                sink.last_model = Some(model.to_owned());
                sink.push(
                    first_line,
                    LogEntry::PiMetadata {
                        label: "Model".to_owned(),
                        text: model.to_owned(),
                        timestamp: message_timestamp.clone(),
                        searchable: false,
                        usage: None,
                    },
                );
            }
            for part in parts {
                let timestamp = part_timestamp(&part.data).or_else(|| message_timestamp.clone());
                for entry in assistant_part_entries(&part.data, timestamp, &mut sink.dropped) {
                    sink.push(part.position, entry);
                }
            }
        }
        _ => sink.dropped.unrecognised("message.role", Some(role)),
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
fn injected_read_entries(parts: &[Part], message_timestamp: &Option<String>, sink: &mut EntrySink) {
    let mut index = 0;
    while index < parts.len() {
        let Part {
            position,
            data: part,
        } = &parts[index];
        index += 1;
        if !is_synthetic_text(part) {
            continue;
        }
        let Some(input) = injected_read_input(part) else {
            continue;
        };
        let mut contents = Vec::new();
        while let Some(Part { data: next, .. }) = parts.get(index)
            && is_synthetic_text(next)
            && !is_comment_note(next)
            && injected_read_input(next).is_none()
        {
            if let Some(text) = next.get("text").and_then(Value::as_str) {
                contents.push(text);
            }
            index += 1;
        }
        let part_id = part.get("id").and_then(Value::as_str);
        let call_id = part_id.unwrap_or("injected-read").to_owned();
        let timestamp = part_timestamp(part).or_else(|| message_timestamp.clone());
        sink.push(
            *position,
            assistant_entry(
                part_id.map(str::to_owned),
                vec![tool_use_block(call_id.clone(), "read", input)],
                timestamp.clone(),
            ),
        );
        sink.push(
            *position,
            tool_result_entry(call_id, &contents.join("\n"), timestamp),
        );
    }
}

/// The input a narration part states, or `None` for a synthetic part that
/// narrates no read.
fn injected_read_input(part: &Value) -> Option<Value> {
    injected_read_input_from_text(part.get("text").and_then(Value::as_str)?)
}

/// One assistant part's entries. Synthetic parts, `step-start`, snapshots,
/// `file` and `patch` attachments, and unknown types yield nothing —
/// projection, not parsing. The named and unknown types count in `dropped`.
fn assistant_part_entries(
    part: &Value,
    timestamp: Option<String>,
    dropped: &mut DroppedRecords,
) -> Vec<LogEntry> {
    if is_synthetic(part) {
        return Vec::new();
    }
    let part_id = part.get("id").and_then(Value::as_str).map(str::to_owned);
    let kind = part.get("type").and_then(Value::as_str);
    match kind {
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
        Some("step-finish") => usage_entry(part).into_iter().collect(),
        Some("compaction") => compaction_summary(part, timestamp).into_iter().collect(),
        Some("step-start" | "snapshot" | "file" | "patch") => {
            dropped.skipped("part.type", kind);
            Vec::new()
        }
        _ => {
            dropped.unrecognised("part.type", kind);
            Vec::new()
        }
    }
}

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
        entries.push(tool_result_entry(call_id, output, timestamp));
    }
    entries
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

#[cfg(test)]
mod tests {
    use super::super::fixture::{self, SessionSpec, database_in, entry_json};
    use super::super::{OPENCODE_DB, session_ref};
    use super::*;
    use crate::history::Source;
    use crate::history::format::SessionFormat;
    use crate::history::format::dropped::DropOutcome;
    use std::path::PathBuf;

    #[test]
    fn counting_names_skipped_and_unrecognised_part_types_and_roles() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("opencode.db");
        let connection = fixture::create_database(&database);
        fixture::insert_session(
            &connection,
            &SessionSpec {
                id: "ses_counted",
                parent_id: None,
                directory: "/tmp/opencode-project",
                title: "counted",
                created_ms: 1755000100000,
                updated_ms: 1755000100000,
                archived_ms: None,
            },
        );
        for (message, role) in [("msg_a", "assistant"), ("msg_b", "system")] {
            fixture::insert_message(
                &connection,
                message,
                "ses_counted",
                1755000100000,
                &json!({"role": role}),
            );
        }
        for (part, message, kind) in [
            ("prt_a1", "msg_a", "hologram"),
            ("prt_a2", "msg_a", "step-start"),
            ("prt_b1", "msg_b", "text"),
        ] {
            fixture::insert_part(
                &connection,
                part,
                message,
                "ses_counted",
                1755000100000,
                &json!({"type": kind, "text": "x"}),
            );
        }
        drop(connection);

        let projection = OPENCODE_DB
            .parse_transcript_counting(
                crate::history::FOR_FIXTURES,
                &session_ref(&database, "ses_counted"),
                DroppedRecords::counting(),
            )
            .unwrap()
            .unwrap();

        let version = Some("1.0.0-fixture");
        assert_eq!(
            projection.dropped.counts().collect::<Vec<_>>(),
            [
                (version, "message.role=system", DropOutcome::Unrecognised, 1),
                (version, "part.type=hologram", DropOutcome::Unrecognised, 1),
                (version, "part.type=step-start", DropOutcome::Skipped, 1),
            ]
        );
    }

    #[test]
    fn session_projects_dialogue_and_skips_request_framing() {
        let directory = tempfile::tempdir().unwrap();
        let database = database_in(directory.path());
        let connection = Connection::open(&database).unwrap();
        fixture::standard_session(&connection, "ses_standard");
        drop(connection);

        let projection = OPENCODE_DB
            .parse_transcript(
                crate::history::FOR_FIXTURES,
                &session_ref(&database, "ses_standard"),
            )
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

    /// A reply that switches models is written as a message row before any
    /// of its parts, while the previous reply's call may still be running.
    #[test]
    fn model_marker_waits_for_its_replys_first_part() {
        let directory = tempfile::tempdir().unwrap();
        let database = database_in(directory.path());
        let connection = Connection::open(&database).unwrap();
        fixture::insert_session(
            &connection,
            &SessionSpec {
                id: "ses_switch",
                parent_id: None,
                directory: "/tmp/switch",
                title: "",
                created_ms: 1_755_000_000_000,
                updated_ms: 1_755_000_000_000,
                archived_ms: None,
            },
        );
        let reply = |id: &str, model: &str, time_ms: i64| {
            fixture::insert_message(
                &connection,
                id,
                "ses_switch",
                time_ms,
                &serde_json::json!({
                    "role": "assistant",
                    "modelID": model,
                    "time": { "created": time_ms },
                }),
            );
        };
        reply("msg_first", "model-a", 1_755_000_001_000);
        fixture::insert_part(
            &connection,
            "prt_call",
            "msg_first",
            "ses_switch",
            1_755_000_002_000,
            &serde_json::json!({
                "type": "tool",
                "tool": "bash",
                "callID": "call_running",
                "state": { "status": "running", "input": { "command": "ls" } },
            }),
        );
        reply("msg_second", "model-b", 1_755_000_003_000);
        let marker_lines = || {
            OPENCODE_DB
                .parse_transcript(
                    crate::history::FOR_FIXTURES,
                    &session_ref(&database, "ses_switch"),
                )
                .unwrap()
                .unwrap()
                .entries
                .into_iter()
                .filter_map(|(record, entry)| {
                    matches!(&entry, LogEntry::PiMetadata { text, .. } if text == "model-b")
                        .then_some(record.line)
                })
                .collect::<Vec<_>>()
        };

        assert_eq!(
            marker_lines(),
            Vec::<usize>::new(),
            "a reply with no part yet shows no marker"
        );

        fixture::insert_part(
            &connection,
            "prt_reply",
            "msg_second",
            "ses_switch",
            1_755_000_004_000,
            &serde_json::json!({ "type": "text", "text": "done" }),
        );
        assert_eq!(marker_lines(), vec![2], "the marker takes its reply's part");
    }

    #[test]
    fn step_finish_tokens_reach_conversation_totals() {
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
        let conversation = crate::history::parser::process_session_file(
            crate::history::FOR_FIXTURES,
            &stub,
            &OPENCODE_DB,
            None,
            &|| {},
            &crate::clock::TestClock::default(),
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
    fn injected_read_is_read_of_file_or_resource_it_names() {
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
}
