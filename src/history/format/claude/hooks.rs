//! Hooks' messages and errors. The `hook_system_message` and
//! `hook_non_blocking_error` attachments each carry the `toolUseID` of their
//! hook's call; Stop, UserPromptSubmit and SessionStart hooks carry a random
//! one. A `stop_hook_summary` record lists the Stop hooks' errors.

use std::collections::{HashMap, HashSet};

use serde_json::{Value, json};

use super::ClaudeRecord;
use crate::history::format::RecordLine;
use crate::log_entry::{ContentBlock, HOOK_LABEL, LogEntry, UserContent, UserMessage};

const MESSAGE_ATTACHMENT_TYPE: &str = "hook_system_message";
pub(super) const ERROR_ATTACHMENT_TYPE: &str = "hook_non_blocking_error";
const STOP_HOOK_SUMMARY_SUBTYPE: &str = "stop_hook_summary";

pub(super) struct HookAttachment {
    tool_use_id: String,
    hook_name: String,
    report: HookReport,
    timestamp: Option<String>,
    uuid: Option<String>,
    cwd: Option<String>,
}

enum HookReport {
    Message(String),
    /// The error's stderr, empty when the hook wrote none.
    Error(String),
}

impl HookAttachment {
    /// The hook attachment in `line`. `None` for any other record, for an
    /// attachment without a `toolUseID` or a `hookName`, and for a Stop or
    /// SubagentStop hook's error.
    pub(super) fn parse(line: &str) -> Option<Self> {
        if !line.contains(MESSAGE_ATTACHMENT_TYPE) && !line.contains(ERROR_ATTACHMENT_TYPE) {
            return None;
        }
        let record = serde_json::from_str::<Value>(line).ok()?;
        if is_stop_hook_error(&record) {
            return None;
        }
        let attachment = record.get("attachment")?;
        let field = |name: &str| attachment.get(name).and_then(Value::as_str);
        let report = match field("type")? {
            MESSAGE_ATTACHMENT_TYPE => HookReport::Message(field("content")?.to_owned()),
            ERROR_ATTACHMENT_TYPE => HookReport::Error(field("stderr").unwrap_or("").to_owned()),
            _ => return None,
        };
        let record_field = |name: &str| record.get(name).and_then(Value::as_str).map(str::to_owned);
        Some(Self {
            tool_use_id: field("toolUseID")?.to_owned(),
            hook_name: field("hookName")?.to_owned(),
            report,
            timestamp: record_field("timestamp"),
            uuid: record_field("uuid"),
            cwd: record_field("cwd"),
        })
    }

    /// `<hookName> hook: <content>`, or `<hookName> hook error: <stderr>`.
    fn result_text(&self) -> String {
        self.report.text(&format!("{} hook", self.hook_name))
    }

    pub(super) fn into_result_entry(self) -> LogEntry {
        let result = ContentBlock::ToolResult {
            content: Some(Value::String(self.result_text())),
            tool_use_id: self.tool_use_id,
            standalone_tool_name: None,
        };
        let (timestamp, uuid, cwd) = (self.timestamp, self.uuid, self.cwd);
        super::user_record(UserContent::Blocks(vec![result]), |name| match name {
            "timestamp" => timestamp.clone(),
            "uuid" => uuid.clone(),
            "cwd" => cwd.clone(),
            _ => None,
        })
    }

    fn into_hook_row(self) -> LogEntry {
        hook_row(self.report.text(&self.hook_name), self.timestamp)
    }
}

impl HookReport {
    /// `<subject>: <message>`, or `<subject> error: <stderr>`.
    fn text(&self, subject: &str) -> String {
        match self {
            HookReport::Message(message) => format!("{subject}: {}", message.trim_end()),
            HookReport::Error(stderr) if stderr.trim().is_empty() => format!("{subject} error"),
            HookReport::Error(stderr) => format!("{subject} error: {}", stderr.trim_end()),
        }
    }
}

fn hook_row(text: String, timestamp: Option<String>) -> LogEntry {
    LogEntry::PiMetadata {
        label: HOOK_LABEL.to_owned(),
        text,
        timestamp,
        searchable: true,
        usage: None,
    }
}

/// True for a Stop or SubagentStop hook's error attachment. The
/// `stop_hook_summary` record after it lists the same error, and
/// [`stop_hook_rows`] reads it there.
pub(super) fn is_stop_hook_error(record: &Value) -> bool {
    let attachment_field = |name: &str| {
        record
            .pointer(&format!("/attachment/{name}"))
            .and_then(Value::as_str)
    };
    attachment_field("type") == Some(ERROR_ATTACHMENT_TYPE)
        && matches!(attachment_field("hookEvent"), Some("Stop" | "SubagentStop"))
}

/// A `stop_hook_summary` record's errors as `Hook` rows, one per error, each
/// under the record's `hookLabel` or else `Stop`, as Claude Code shows them.
/// `None` for any other record, and for a summary listing no error.
pub(super) fn stop_hook_rows(entry: &LogEntry) -> Option<Vec<LogEntry>> {
    let LogEntry::System { subtype, extra, .. } = entry else {
        return None;
    };
    if subtype != STOP_HOOK_SUMMARY_SUBTYPE {
        return None;
    }
    let label = extra
        .get("hookLabel")
        .and_then(Value::as_str)
        .unwrap_or("Stop");
    let timestamp = extra
        .get("timestamp")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let rows: Vec<LogEntry> = extra
        .get("hookErrors")?
        .as_array()?
        .iter()
        .filter_map(Value::as_str)
        .map(|error| {
            hook_row(
                HookReport::Error(error.to_owned()).text(label),
                timestamp.clone(),
            )
        })
        .collect();
    (!rows.is_empty()).then_some(rows)
}

/// The transcript's records as entries, with each hook attachment's text in
/// its call's result: before the result's content when the attachment comes
/// first in the file, after it otherwise.
///
/// - A call with no result in the transcript takes its first hook attachment
///   as its result.
/// - A hook attachment with no call in the transcript becomes a `Hook` row in
///   its place.
pub(super) fn place_hook_attachments(
    records: Vec<(RecordLine, ClaudeRecord)>,
) -> Vec<(RecordLine, LogEntry)> {
    let holders = result_holders(&records);
    let mut texts: HashMap<(usize, String), Surrounding> = HashMap::new();
    for (index, (_, record)) in records.iter().enumerate() {
        let ClaudeRecord::HookAttachment(hook) = record else {
            continue;
        };
        if let Some(&holder) = holders.get(&hook.tool_use_id)
            && holder != index
        {
            let surrounding = texts.entry((holder, hook.tool_use_id.clone())).or_default();
            let side = if index < holder {
                &mut surrounding.before
            } else {
                &mut surrounding.after
            };
            side.push(hook.result_text());
        }
    }
    let mut entries = Vec::with_capacity(records.len());
    for (index, (line, record)) in records.into_iter().enumerate() {
        match record {
            ClaudeRecord::Entry(entry) => {
                entries.push((line, with_hook_texts(entry, index, &mut texts)));
            }
            ClaudeRecord::Entries(rows) => {
                entries.extend(rows.into_iter().map(|row| (line.clone(), row)));
            }
            ClaudeRecord::HookAttachment(hook) => match holders.get(&hook.tool_use_id) {
                Some(&holder) if holder == index => {
                    let result = hook.into_result_entry();
                    entries.push((line, with_hook_texts(result, index, &mut texts)));
                }
                Some(_) => {}
                None => entries.push((line, hook.into_hook_row())),
            },
        }
    }
    entries
}

/// Each hooked call's result holder, by call ID: the first entry holding a
/// result for the call, else the call's first hook attachment. A hook
/// attachment with no call in `records` has no holder.
fn result_holders(records: &[(RecordLine, ClaudeRecord)]) -> HashMap<String, usize> {
    let hooked: HashSet<&str> = records
        .iter()
        .filter_map(|(_, record)| match record {
            ClaudeRecord::HookAttachment(hook) => Some(hook.tool_use_id.as_str()),
            ClaudeRecord::Entry(_) | ClaudeRecord::Entries(_) => None,
        })
        .collect();
    if hooked.is_empty() {
        return HashMap::new();
    }
    let mut holders = HashMap::new();
    let mut calls = HashSet::new();
    for (index, (_, record)) in records.iter().enumerate() {
        let ClaudeRecord::Entry(entry) = record else {
            continue;
        };
        for block in entry_blocks(entry) {
            match block {
                ContentBlock::ToolUse { id, .. } => {
                    calls.insert(id.as_str());
                }
                ContentBlock::ToolResult { tool_use_id, .. }
                    if hooked.contains(tool_use_id.as_str()) =>
                {
                    holders.entry(tool_use_id.clone()).or_insert(index);
                }
                _ => {}
            }
        }
    }
    for (index, (_, record)) in records.iter().enumerate() {
        if let ClaudeRecord::HookAttachment(hook) = record
            && calls.contains(hook.tool_use_id.as_str())
        {
            holders.entry(hook.tool_use_id.clone()).or_insert(index);
        }
    }
    holders
}

fn entry_blocks(entry: &LogEntry) -> &[ContentBlock] {
    match entry {
        LogEntry::Assistant { message, .. } => &message.content,
        LogEntry::User {
            message:
                UserMessage {
                    content: UserContent::Blocks(blocks),
                    ..
                },
            ..
        } => blocks,
        _ => &[],
    }
}

/// `entry`, the record at `index`, with its results' hook texts from `texts`
/// added around their content.
fn with_hook_texts(
    mut entry: LogEntry,
    index: usize,
    texts: &mut HashMap<(usize, String), Surrounding>,
) -> LogEntry {
    if texts.is_empty() {
        return entry;
    }
    if let LogEntry::User {
        message:
            UserMessage {
                content: UserContent::Blocks(blocks),
                ..
            },
        ..
    } = &mut entry
    {
        for block in blocks {
            if let ContentBlock::ToolResult {
                tool_use_id,
                content,
                ..
            } = block
                && let Some(surrounding) = texts.remove(&(index, tool_use_id.clone()))
            {
                *content = Some(surrounding.around(content.take()));
            }
        }
    }
    entry
}

/// Hook texts before and after one result's content.
#[derive(Default)]
struct Surrounding {
    before: Vec<String>,
    after: Vec<String>,
}

impl Surrounding {
    /// `content` with the hook texts added: as lines of a text result, as
    /// text items of a result array.
    fn around(self, content: Option<Value>) -> Value {
        let Self { before, after } = self;
        match content {
            Some(Value::String(text)) if !text.is_empty() => Value::String(
                before
                    .into_iter()
                    .chain(std::iter::once(text))
                    .chain(after)
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
            None | Some(Value::Null) | Some(Value::String(_)) => Value::String(
                before
                    .into_iter()
                    .chain(after)
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
            Some(Value::Array(items)) => Value::Array(text_items(before, items, after)),
            Some(other) => Value::Array(text_items(before, vec![other], after)),
        }
    }
}

fn text_items(before: Vec<String>, items: Vec<Value>, after: Vec<String>) -> Vec<Value> {
    let text_item = |text: String| json!({ "type": "text", "text": text });
    before
        .into_iter()
        .map(text_item)
        .chain(items)
        .chain(after.into_iter().map(text_item))
        .collect()
}
