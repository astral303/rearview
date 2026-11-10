//! Session-wide reference facts for a bounded read, without result bodies.

use super::{threads, times};
use crate::agent::transcript::{
    AgentMessage, AgentMessagePart, AgentMessageRole, AgentPartSource, AgentTranscript,
};
use crate::error::Result;
use crate::history::sqlite_cache::roots::Places;
use crate::history::task_notification::ReportSender;
use crate::log_entry::Tool;
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use std::path::Path;

/// Each call and result of the session `?1`'s numbered messages, without
/// its input or content. Keep those columns after the ones read: the read
/// then doesn't reach any of their overflow pages.
pub(in crate::history::sqlite_cache) const CALLS_AND_RESULTS: &str = "SELECT m.ordinal, p.kind, p.call_id, p.name, p.tool, p.call_context \
     FROM messages m JOIN entries e ON e.message = m.id JOIN parts p ON p.entry = e.id \
     WHERE m.session = ?1 AND m.ordinal IS NOT NULL AND e.contributes \
     AND p.kind IN ('call', 'result') ORDER BY m.ordinal, e.sequence, p.part";

pub(super) fn read(
    connection: &Connection,
    session: i64,
    places: &Places,
    path: &Path,
) -> Result<AgentTranscript> {
    let threads = threads::read(connection, session, places)?;
    let mut messages = Vec::new();
    let mut statement = connection.prepare(
        "SELECT ordinal, role, reply_id, parent_tool_use_id, thread, first_ms, last_ms, report_has_text, report_task_id, report_launch_call \
         FROM messages WHERE session = ?1 AND ordinal IS NOT NULL ORDER BY ordinal",
    )?;
    let mut rows = statement.query([session])?;
    while let Some(row) = rows.next()? {
        let role: Option<String> = row.get(1)?;
        let thread = &threads.threads[&row.get::<_, i64>(4)?];
        let subagent = thread.origin();
        let report_has_text: Option<bool> = row.get(7)?;
        let mut message = AgentMessage {
            ordinal: row.get::<_, i64>(0)? as usize,
            role: if role.as_deref() == Some("assistant") {
                AgentMessageRole::Assistant
            } else {
                AgentMessageRole::User
            },
            is_report: report_has_text.is_some(),
            report_sender: ReportSender {
                task_id: row.get(8)?,
                launch_call: row.get(9)?,
            },
            report_has_text: report_has_text.unwrap_or(false),
            assistant_message_id: row.get(2)?,
            parent_tool_use_id: row
                .get::<_, Option<String>>(3)?
                .or_else(|| subagent.as_ref().map(|origin| origin.label.clone())),
            subagent,
            parts: Vec::new(),
        };
        // Headers and handle ordering still see every message's span.
        // These empty parts are context only; selected bodies replace them.
        for millis in [row.get::<_, Option<i64>>(5)?, row.get::<_, Option<i64>>(6)?]
            .into_iter()
            .flatten()
        {
            message.parts.push(AgentMessagePart::Text {
                text: String::new(),
                source: source(&message, Some(times::text(millis)?)),
            });
        }
        messages.push(message);
    }
    let mut statement = connection.prepare(CALLS_AND_RESULTS)?;
    let mut rows = statement.query([session])?;
    while let Some(row) = rows.next()? {
        let ordinal = row.get::<_, i64>(0)? as usize;
        let message = &mut messages[ordinal - 1];
        let kind: String = row.get(1)?;
        match kind.as_str() {
            "call" => {
                let input = match row.get::<_, Option<String>>(5)? {
                    Some(stored) => serde_json::from_str(&stored)?,
                    None => json!({}),
                };
                message.parts.push(AgentMessagePart::ToolUse {
                    id: row.get::<_, Option<String>>(2)?.unwrap_or_default(),
                    name: row.get::<_, Option<String>>(3)?.unwrap_or_default(),
                    tool: row
                        .get::<_, Option<String>>(4)?
                        .and_then(|tool| {
                            serde_json::from_value(serde_json::Value::String(tool)).ok()
                        })
                        .unwrap_or_default(),
                    input,
                    source: source(message, None),
                });
            }
            "result" => message.parts.push(AgentMessagePart::ToolResult {
                tool_use_id: row.get::<_, Option<String>>(2)?.unwrap_or_default(),
                content: None,
                source: source(message, None),
            }),
            _ => {}
        }
    }
    let malformed_lines = connection
        .prepare("SELECT line_number FROM malformed_lines WHERE session = ?1 ORDER BY position")?
        .query_map([session], |row| {
            row.get::<_, i64>(0).map(|line| line as usize)
        })?
        .collect::<rusqlite::Result<_>>()?;
    let (summary, custom_title) = connection.query_row(
        "SELECT summary, custom_title FROM sessions WHERE id = ?1",
        params![session],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    Ok(AgentTranscript {
        path: path.to_path_buf(),
        messages,
        malformed_lines,
        summary,
        custom_title,
    })
}

/// The `call_context` a write stores for a call of `tool` with `input`: the
/// fields of the input the sub-agent links read. The links read them from
/// `Agent` and `AgentMessage` calls alone. `None` for any other call, and
/// for an input without any of the fields; a bounded read gives such a
/// call an empty context.
pub(super) fn stored_call_context(tool: Tool, input: &Value) -> Option<String> {
    if !matches!(tool, Tool::Agent | Tool::AgentMessage) {
        return None;
    }
    let mut context = serde_json::Map::new();
    for key in [
        "description",
        "recipient",
        "run_in_background",
        "subagent_type",
    ] {
        if let Some(value) = input.get(key) {
            context.insert(key.to_owned(), value.clone());
        }
    }
    if let Some(agent_type) = input.pointer("/launch/agent_type") {
        context.insert("launch".to_owned(), json!({ "agent_type": agent_type }));
    }
    (!context.is_empty()).then(|| Value::Object(context).to_string())
}

fn source(message: &AgentMessage, timestamp: Option<String>) -> AgentPartSource {
    AgentPartSource {
        role: message.role,
        timestamp,
        jsonl_line: 0,
        part_index: 0,
        assistant_message_id: message.assistant_message_id.clone(),
        parent_tool_use_id: message.parent_tool_use_id.clone(),
        tool_name: None,
    }
}
