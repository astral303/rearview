//! `queued_command` attachments: messages that arrived while Claude was
//! working. A record's `timestamp` is when the message was queued, often
//! earlier than the record before it.

use serde_json::Value;

use crate::log_entry::{LogEntry, UserContent};

pub(super) const ATTACHMENT_TYPE: &str = "queued_command";

/// A queued command in `line` as a user message holding its `prompt`.
/// `None` for any other record, and when [`Sender::is_shown`] is false for
/// its sender.
pub(super) fn queued_message(line: &str) -> Option<LogEntry> {
    // Parse the line a second time only when it can hold a queued command.
    if !line.contains(ATTACHMENT_TYPE) {
        return None;
    }
    let raw = serde_json::from_str::<Value>(line).ok()?;
    let attachment = queued_attachment(&raw)?;
    if !Sender::of(attachment).is_shown() {
        return None;
    }
    let prompt = serde_json::from_value::<UserContent>(attachment.get("prompt")?.clone()).ok()?;
    let field = |name: &str| raw.get(name).and_then(Value::as_str).map(str::to_owned);
    Some(super::user_record(prompt, field))
}

pub(super) fn is_from_coordinator(raw: &Value) -> bool {
    queued_attachment(raw).is_some_and(|attachment| Sender::of(attachment) == Sender::Coordinator)
}

fn queued_attachment(raw: &Value) -> Option<&Value> {
    let attachment = raw.get("attachment")?;
    (attachment.get("type").and_then(Value::as_str) == Some(ATTACHMENT_TYPE)).then_some(attachment)
}

/// The sender of a queued command: its `origin.kind`, or its `commandMode` in a
/// record without an `origin`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Sender {
    Human,
    TaskNotification,
    /// A sub-agent handing back its report.
    Peer,
    /// The parent session, messaging a sub-agent.
    Coordinator,
    Unknown,
}

impl Sender {
    fn of(attachment: &Value) -> Self {
        let kind = attachment
            .pointer("/origin/kind")
            .or_else(|| attachment.get("commandMode"))
            .and_then(Value::as_str);
        match kind {
            Some("human" | "prompt") => Self::Human,
            Some("task-notification") => Self::TaskNotification,
            Some("peer") => Self::Peer,
            Some("coordinator") => Self::Coordinator,
            _ => Self::Unknown,
        }
    }

    /// False for a coordinator and an unknown sender. A coordinator's message
    /// repeats the parent's `SendMessage` call.
    fn is_shown(self) -> bool {
        matches!(self, Self::Human | Self::TaskNotification | Self::Peer)
    }
}
