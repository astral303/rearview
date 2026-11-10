use super::{EntryColumns, threads::Placement};
use crate::agent::message_numbering::MessageNumbering;
use crate::agent::transcript::{AgentMessageRole, MessageReport};
use crate::log_entry::LogEntry;
use rusqlite::{Connection, params};
use std::collections::HashMap;

pub(super) struct Writer {
    session: i64,
    numbering: MessageNumbering,
    replies: HashMap<(i64, &'static str), HashMap<String, i64>>,
}

#[derive(Clone, Copy)]
pub(super) struct Membership {
    pub(super) id: i64,
    pub(super) contributes: bool,
}

impl Writer {
    pub(super) fn new(session: i64) -> Self {
        Self {
            session,
            numbering: MessageNumbering::default(),
            replies: HashMap::new(),
        }
    }

    pub(super) fn write(
        &mut self,
        connection: &Connection,
        placement: &Placement,
        entry: &LogEntry,
        columns: &EntryColumns<'_>,
    ) -> rusqlite::Result<Option<Membership>> {
        let role = columns.role.or_else(|| {
            (columns.kind == "pi-metadata" && columns.flag == Some(true)).then_some("user")
        });
        if role.is_none() && columns.kind != "progress" {
            return Ok(None);
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
        if let Some(id) = stored {
            // Usage snapshots can change while a reply streams. Match the
            // catalogue's last-record rule rather than summing the copies.
            connection.prepare_cached(
                "UPDATE messages SET ordinal = COALESCE(ordinal, ?2), \
                 first_ms = CASE WHEN ?3 IS NULL THEN first_ms \
                     WHEN first_ms IS NULL THEN ?3 ELSE MIN(first_ms, ?3) END, \
                 last_ms = CASE WHEN ?3 IS NULL THEN last_ms \
                     WHEN last_ms IS NULL THEN ?3 ELSE MAX(last_ms, ?3) END, \
                 model = COALESCE(?4, model), agent = COALESCE(?5, agent), \
                 cwd = COALESCE(?6, cwd), parent_tool_use_id = COALESCE(?7, parent_tool_use_id), \
                 source_tool_use_id = COALESCE(?8, source_tool_use_id), \
                 usage_input = COALESCE(?9, usage_input), usage_output = COALESCE(?10, usage_output), \
                 usage_cache_creation = COALESCE(?11, usage_cache_creation), \
                 usage_cache_read = COALESCE(?12, usage_cache_read), \
                 report_has_text = CASE WHEN ordinal IS NULL AND ?2 IS NOT NULL THEN ?13 ELSE report_has_text END, \
                 report_task_id = CASE WHEN ordinal IS NULL AND ?2 IS NOT NULL THEN ?14 ELSE report_task_id END, \
                 report_launch_call = CASE WHEN ordinal IS NULL AND ?2 IS NOT NULL THEN ?15 ELSE report_launch_call END WHERE id = ?1",
            )?.execute(params![
                id, ordinal, time, columns.model, columns.agent, columns.cwd,
                columns.parent_tool_use_id, columns.source_tool_use_id,
                columns.usage.map(|usage| usage.input_tokens as i64),
                columns.usage.map(|usage| usage.output_tokens as i64),
                columns.usage.map(|usage| usage.cache_creation_input_tokens as i64),
                columns.usage.map(|usage| usage.cache_read_input_tokens as i64),
                report.is_report.then_some(report.has_text), report.sender.task_id, report.sender.launch_call,
            ])?;
            return Ok(Some(Membership {
                id,
                contributes: numbered.is_some(),
            }));
        }
        connection.prepare_cached(
            "INSERT INTO messages (session, thread, ordinal, role, reply_id, first_ms, last_ms, \
             model, agent, cwd, parent_tool_use_id, source_tool_use_id, usage_input, usage_output, \
             usage_cache_creation, usage_cache_read, report_has_text, report_task_id, report_launch_call) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)",
        )?.execute(params![
            self.session, placement.thread, ordinal, role, columns.message_id, time,
            columns.model, columns.agent, columns.cwd, columns.parent_tool_use_id,
            columns.source_tool_use_id,
            columns.usage.map(|usage| usage.input_tokens as i64),
            columns.usage.map(|usage| usage.output_tokens as i64),
            columns.usage.map(|usage| usage.cache_creation_input_tokens as i64),
            columns.usage.map(|usage| usage.cache_read_input_tokens as i64),
            report.is_report.then_some(report.has_text), report.sender.task_id, report.sender.launch_call,
        ])?;
        let id = connection.last_insert_rowid();
        if let Some(reply) = columns.message_id {
            self.replies
                .entry(key)
                .or_default()
                .insert(reply.to_owned(), id);
        }
        Ok(Some(Membership {
            id,
            contributes: numbered.is_some(),
        }))
    }
}
