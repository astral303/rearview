use super::digest::Digest;
use super::{EntryColumns, threads::Placement};
use crate::agent::message_numbering::{MessageNumbering, NumberingCheckpoint};
use crate::agent::transcript::{AgentMessageRole, MessageReport};
use crate::log_entry::{LogEntry, TokenUsage};
use std::collections::HashMap;

/// A `messages` row: the shared fields of the entries that make up one
/// logical message.
pub(super) struct MessageRow<'a> {
    /// The index of the message's thread in the session's threads.
    pub(super) thread: usize,
    pub(super) ordinal: Option<i64>,
    /// The index of the entry that started the message: its first.
    pub(super) first_entry: usize,
    role: Option<&'a str>,
    reply_id: Option<&'a str>,
    first_ms: Option<i64>,
    last_ms: Option<i64>,
    model: Option<&'a str>,
    agent: Option<&'a str>,
    cwd: Option<&'a str>,
    parent_tool_use_id: Option<&'a str>,
    source_tool_use_id: Option<&'a str>,
    report_has_text: Option<bool>,
    report_task_id: Option<String>,
    report_launch_call: Option<String>,
    usage: Option<&'a TokenUsage>,
}

impl MessageRow<'_> {
    /// The row's columns after `thread`, in the order the writer's
    /// `MESSAGE_COLUMNS` lists them.
    pub(super) fn values(&self) -> [rusqlite::types::Value; 18] {
        use rusqlite::types::Value;
        let usage = |count: fn(&TokenUsage) -> u64| {
            Value::from(self.usage.map(|usage| count(usage) as i64))
        };
        [
            self.ordinal.into(),
            self.role.map(str::to_owned).into(),
            self.reply_id.map(str::to_owned).into(),
            self.first_ms.into(),
            self.last_ms.into(),
            self.model.map(str::to_owned).into(),
            self.agent.map(str::to_owned).into(),
            self.cwd.map(str::to_owned).into(),
            self.parent_tool_use_id.map(str::to_owned).into(),
            self.source_tool_use_id.map(str::to_owned).into(),
            self.report_has_text.into(),
            self.report_task_id.clone().into(),
            self.report_launch_call.clone().into(),
            usage(|usage| usage.input_tokens),
            usage(|usage| usage.output_tokens),
            usage(|usage| usage.cache_creation_input_tokens),
            usage(|usage| usage.cache_read_input_tokens),
            self.digest().into(),
        ]
    }

    /// A hash of every column but `thread` and `ordinal`.
    pub(super) fn digest(&self) -> i64 {
        let mut digest = Digest::default();
        for text in [
            self.role,
            self.reply_id,
            self.model,
            self.agent,
            self.cwd,
            self.parent_tool_use_id,
            self.source_tool_use_id,
            self.report_task_id.as_deref(),
            self.report_launch_call.as_deref(),
        ] {
            digest.text(text);
        }
        digest.number(self.first_ms);
        digest.number(self.last_ms);
        digest.flag(self.report_has_text);
        digest.usage(self.usage);
        digest.finish()
    }
}

/// Groups a session's entries into messages, in entry order: an entry
/// carrying a reply id already seen in its thread joins that reply's
/// message.
#[derive(Default)]
pub(super) struct Messages<'a> {
    numbering: MessageNumbering,
    replies: HashMap<(i64, &'static str), HashMap<&'a str, usize>>,
    pub(super) rows: Vec<MessageRow<'a>>,
}

#[derive(Clone, Copy)]
pub(super) struct Membership {
    /// The index of the message in [`Messages::rows`].
    pub(super) message: usize,
    pub(super) contributes: bool,
}

impl<'a> Messages<'a> {
    /// The grouping after the entries `numbering` was checkpointed past,
    /// holding no message yet.
    pub(super) fn resumed(numbering: &NumberingCheckpoint) -> Self {
        Self {
            numbering: MessageNumbering::resumed(numbering),
            ..Self::default()
        }
    }

    /// Adds `entry`, the entry at `index` whose columns are `columns`, to its
    /// message; `None` for an entry outside every message.
    pub(super) fn add(
        &mut self,
        index: usize,
        placement: &Placement,
        entry: &LogEntry,
        columns: &EntryColumns<'a>,
    ) -> Option<Membership> {
        let role = columns.role.or_else(|| {
            (columns.kind == "pi-metadata" && columns.flag == Some(true)).then_some("user")
        });
        if role.is_none() && columns.kind != "progress" {
            return None;
        }
        let numbered = if columns.kind == "progress" {
            role.and_then(AgentMessageRole::of_progress)
                .and_then(|role| {
                    self.numbering.number_progress_in_thread(
                        role,
                        columns.parts.blocks(),
                        columns.message_id,
                        placement.logical_thread,
                    )
                })
        } else {
            self.numbering
                .number_in_thread(entry, placement.logical_thread)
        };
        let ordinal = numbered.map(|numbered| numbered.ordinal as i64);
        let time = numbered.and(columns.timestamp_ms);
        let report = if numbered.is_some_and(|numbered| numbered.starts_message) {
            role.and_then(AgentMessageRole::of_progress)
                .map(|role| MessageReport::of_blocks(role, columns.parts.blocks()))
                .unwrap_or_default()
        } else {
            MessageReport::default()
        };
        let key = (placement.logical_thread, columns.kind);
        let stored = columns.message_id.and_then(|reply| {
            self.replies
                .get(&key)
                .and_then(|replies| replies.get(reply))
                .copied()
        });
        let contributes = numbered.is_some();
        if let Some(message) = stored {
            let row = &mut self.rows[message];
            // Usage snapshots can change while a reply streams. Match the
            // catalogue's last-record rule rather than summing the copies.
            if row.ordinal.is_none() && ordinal.is_some() {
                row.report_has_text = report.is_report.then_some(report.has_text);
                row.report_task_id = report.sender.task_id;
                row.report_launch_call = report.sender.launch_call;
            }
            row.ordinal = row.ordinal.or(ordinal);
            if let Some(time) = time {
                row.first_ms = Some(row.first_ms.map_or(time, |first| first.min(time)));
                row.last_ms = Some(row.last_ms.map_or(time, |last| last.max(time)));
            }
            row.model = columns.model.or(row.model);
            row.agent = columns.agent.or(row.agent);
            row.cwd = columns.cwd.or(row.cwd);
            row.parent_tool_use_id = columns.parent_tool_use_id.or(row.parent_tool_use_id);
            row.source_tool_use_id = columns.source_tool_use_id.or(row.source_tool_use_id);
            row.usage = columns.usage.or(row.usage);
            return Some(Membership {
                message,
                contributes,
            });
        }
        self.rows.push(MessageRow {
            thread: placement.thread,
            ordinal,
            first_entry: index,
            role,
            reply_id: columns.message_id,
            first_ms: time,
            last_ms: time,
            model: columns.model,
            agent: columns.agent,
            cwd: columns.cwd,
            parent_tool_use_id: columns.parent_tool_use_id,
            source_tool_use_id: columns.source_tool_use_id,
            report_has_text: report.is_report.then_some(report.has_text),
            report_task_id: report.sender.task_id,
            report_launch_call: report.sender.launch_call,
            usage: columns.usage,
        });
        let message = self.rows.len() - 1;
        if let Some(reply) = columns.message_id {
            self.replies.entry(key).or_default().insert(reply, message);
        }
        Some(Membership {
            message,
            contributes,
        })
    }
}
