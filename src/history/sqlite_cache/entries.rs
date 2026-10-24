//! Cached entries preserve rendered and searchable content, every tool
//! result included; sub-agent wrappers are reconstructed from message
//! columns and shared thread metadata.

mod context;
mod messages;
mod parts;
mod threads;
mod times;

use crate::error::{AppError, Result};
use crate::history::format::RecordLine;
use crate::history::{MalformedLine, MalformedLineDetail, TranscriptEntries};
use crate::log_entry::{
    AssistantMessage, ContentBlock, LogEntry, TokenUsage, UserContent, UserMessage,
};
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde_json::Value;
use std::ops::RangeInclusive;
use std::path::Path;
use threads::Numbering;

/// Entries written per transaction, so a session of hundreds of thousands of
/// entries does not hold the database's write lock while all of it is
/// written.
const ENTRIES_PER_TRANSACTION: usize = 20_000;

/// Writes `entries`, the entries of the session with id `session`.
pub(super) fn write(
    connection: &mut Connection,
    session: i64,
    entries: &TranscriptEntries,
    check_space: &dyn Fn() -> Result<()>,
    owner: &super::refresh::RefreshToken,
) -> Result<()> {
    let mut numbering = Numbering::default();
    let mut message_writer = messages::Writer::new(session);
    for (chunk_index, chunk) in entries.entries.chunks(ENTRIES_PER_TRANSACTION).enumerate() {
        check_space()?;
        let transaction = super::session_transaction(connection, session, owner)?;
        {
            let mut entry_statement = transaction.prepare_cached(INSERT_ENTRY)?;
            let mut part_writer = parts::Writer::new(&transaction)?;
            for (offset, (record, entry)) in chunk.iter().enumerate() {
                let sequence = (chunk_index * ENTRIES_PER_TRANSACTION + offset) as i64;
                let placement = numbering.place(&transaction, session, record, entry)?;
                let columns = EntryColumns::of(entry);
                let membership = message_writer.write(&transaction, &placement, entry, &columns)?;
                let message = membership.map(|membership| membership.id);
                let metadata_usage = message.is_none().then_some(columns.usage).flatten();
                entry_statement.execute(params![
                    session,
                    message,
                    membership.is_some_and(|membership| membership.contributes),
                    placement.position,
                    sequence,
                    placement.transcript,
                    record.line as i64,
                    columns.kind,
                    columns.timestamp_ms,
                    columns.content,
                    columns.text,
                    columns.label,
                    columns.uuid,
                    metadata_usage.map(|usage| usage.input_tokens as i64),
                    metadata_usage.map(|usage| usage.output_tokens as i64),
                    metadata_usage.map(|usage| usage.cache_creation_input_tokens as i64),
                    metadata_usage.map(|usage| usage.cache_read_input_tokens as i64),
                    columns.level,
                    columns.duration_ms.map(|duration| duration as i64),
                    columns.parent_uuid,
                    columns.flag,
                ])?;
                part_writer.write(
                    &transaction,
                    transaction.last_insert_rowid(),
                    columns.parts.blocks(),
                )?;
            }
        }
        super::refresh::commit(transaction, owner)?;
        check_space()?;
    }
    check_space()?;
    let transaction = super::session_transaction(connection, session, owner)?;
    transaction.execute(
        "UPDATE sessions SET message_count = (SELECT COUNT(*) FROM messages \
         WHERE session = ?1 AND ordinal IS NOT NULL) WHERE id = ?1",
        [session],
    )?;
    {
        let mut statement = transaction.prepare_cached(
            "INSERT INTO malformed_lines (session, position, line_number, line_content, \
             error_message, context_before, context_after) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        )?;
        for (position, malformed) in entries.malformed_lines.iter().enumerate() {
            let detail = malformed.detail.as_ref();
            statement.execute(params![
                session,
                position as i64,
                malformed.line_number as i64,
                detail.map(|detail| detail.line_content.as_str()),
                detail.map(|detail| detail.error_message.as_str()),
                detail.map(|detail| json_text(&detail.context_before)),
                detail.map(|detail| json_text(&detail.context_after)),
            ])?;
        }
    }
    super::refresh::commit(transaction, owner)?;
    Ok(())
}

const INSERT_ENTRY: &str = "INSERT INTO entries (session, message, contributes, position, sequence, \
    record_transcript, record_line, kind, timestamp_ms, content, text, label, uuid, \
    usage_input, usage_output, usage_cache_creation, usage_cache_read, level, duration_ms, \
    parent_uuid, flag) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, \
    ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21)";

#[derive(Default)]
enum Parts<'a> {
    #[default]
    None,
    /// The entry's content blocks.
    Borrowed(&'a [ContentBlock]),
    /// A user message recorded as one string: one text part.
    Text(ContentBlock),
    /// A sub-agent turn's blocks, read out of its JSON payload.
    Owned(Vec<ContentBlock>),
}

impl Parts<'_> {
    fn blocks(&self) -> &[ContentBlock] {
        match self {
            Self::None => &[],
            Self::Borrowed(blocks) => blocks,
            Self::Text(block) => std::slice::from_ref(block),
            Self::Owned(blocks) => blocks,
        }
    }
}

/// An entry's columns. Every field is `None` unless the entry's kind has it.
#[derive(Default)]
struct EntryColumns<'a> {
    kind: &'static str,
    timestamp_ms: Option<i64>,
    role: Option<&'a str>,
    content: Option<&'static str>,
    text: Option<&'a str>,
    label: Option<&'a str>,
    uuid: Option<&'a str>,
    cwd: Option<&'a str>,
    parent_tool_use_id: Option<&'a str>,
    source_tool_use_id: Option<&'a str>,
    message_id: Option<&'a str>,
    model: Option<&'a str>,
    agent: Option<&'a str>,
    usage: Option<&'a TokenUsage>,
    level: Option<&'a str>,
    duration_ms: Option<u64>,
    parent_uuid: Option<&'a str>,
    flag: Option<bool>,
    parts: Parts<'a>,
}

const CONTENT_STRING: &str = "string";
const CONTENT_BLOCKS: &str = "blocks";

impl<'a> EntryColumns<'a> {
    fn of(entry: &'a LogEntry) -> Self {
        match entry {
            LogEntry::Summary { summary } => Self {
                kind: "summary",
                text: Some(summary),
                ..Self::default()
            },
            LogEntry::User {
                message,
                timestamp,
                uuid,
                cwd,
                parent_tool_use_id,
                source_tool_use_id,
                usage,
            } => {
                let (content, parts) = match &message.content {
                    UserContent::String(text) => (
                        CONTENT_STRING,
                        Parts::Text(ContentBlock::Text { text: text.clone() }),
                    ),
                    UserContent::Blocks(blocks) => (CONTENT_BLOCKS, Parts::Borrowed(blocks)),
                };
                Self {
                    kind: "user",
                    timestamp_ms: times::millis(timestamp.as_deref()),
                    role: Some(&message.role),
                    content: Some(content),
                    uuid: uuid.as_deref(),
                    cwd: cwd.as_deref(),
                    parent_tool_use_id: parent_tool_use_id.as_deref(),
                    source_tool_use_id: source_tool_use_id.as_deref(),
                    usage: usage.as_ref(),
                    parts,
                    ..Self::default()
                }
            }
            LogEntry::Assistant {
                message,
                agent,
                timestamp,
                uuid,
                parent_tool_use_id,
            } => Self {
                kind: "assistant",
                timestamp_ms: times::millis(timestamp.as_deref()),
                role: Some(&message.role),
                content: Some(CONTENT_BLOCKS),
                uuid: uuid.as_deref(),
                parent_tool_use_id: parent_tool_use_id.as_deref(),
                message_id: message.id.as_deref(),
                model: message.model.as_deref(),
                agent: agent.as_deref(),
                usage: message.usage.as_ref(),
                parts: Parts::Borrowed(&message.content),
                ..Self::default()
            },
            LogEntry::FileHistorySnapshot { .. } => Self {
                kind: "file-history-snapshot",
                ..Self::default()
            },
            LogEntry::Progress { data, extra } => {
                let timestamp = extra.get("timestamp").and_then(Value::as_str);
                match progress_blocks(data) {
                    Some(blocks) => Self {
                        kind: "progress",
                        timestamp_ms: times::millis(timestamp),
                        content: Some(CONTENT_BLOCKS),
                        role: data.pointer("/message/type").and_then(Value::as_str),
                        message_id: data.pointer("/message/message/id").and_then(Value::as_str),
                        parts: Parts::Owned(blocks),
                        ..Self::default()
                    },
                    None => Self {
                        kind: "progress",
                        timestamp_ms: times::millis(timestamp),
                        ..Self::default()
                    },
                }
            }
            LogEntry::System {
                subtype,
                level,
                duration_ms,
                parent_uuid,
                ..
            } => Self {
                kind: "system",
                label: Some(subtype),
                level: level.as_deref(),
                duration_ms: *duration_ms,
                parent_uuid: parent_uuid.as_deref(),
                ..Self::default()
            },
            LogEntry::CustomTitle { custom_title } => Self {
                kind: "custom-title",
                text: Some(custom_title),
                ..Self::default()
            },
            LogEntry::AiTitle { ai_title } => Self {
                kind: "ai-title",
                text: Some(ai_title),
                ..Self::default()
            },
            LogEntry::AgentName { agent_name } => Self {
                kind: "agent-name",
                text: Some(agent_name),
                ..Self::default()
            },
            LogEntry::PermissionMode { .. } => Self {
                kind: "permission-mode",
                ..Self::default()
            },
            LogEntry::PiMetadata {
                label,
                text,
                timestamp,
                searchable,
                usage,
            } => Self {
                kind: "pi-metadata",
                label: Some(label),
                text: Some(text),
                timestamp_ms: times::millis(timestamp.as_deref()),
                flag: Some(*searchable),
                usage: usage.as_ref(),
                ..Self::default()
            },
            LogEntry::Unknown => Self {
                kind: "unknown",
                ..Self::default()
            },
        }
    }
}

fn progress_blocks(data: &Value) -> Option<Vec<ContentBlock>> {
    if data.get("type").and_then(Value::as_str) != Some("agent_progress") {
        return None;
    }
    let content = data.pointer("/message/message/content")?;
    serde_json::from_value(content.clone()).ok()
}

fn json_text(lines: &[String]) -> String {
    serde_json::to_string(lines).expect("a list of strings serializes")
}

pub(super) fn path_text(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

pub(super) struct Selection {
    ids: &'static str,
    parameters: Vec<rusqlite::types::Value>,
}

impl Selection {
    fn new(session: i64, messages: Option<RangeInclusive<u64>>) -> Self {
        let mut parameters = vec![session.into()];
        let ids = if let Some(messages) = messages {
            parameters.push(i64::try_from(*messages.start()).unwrap_or(i64::MAX).into());
            parameters.push(i64::try_from(*messages.end()).unwrap_or(i64::MAX).into());
            "SELECT e.id FROM messages m JOIN entries e ON e.message = m.id \
             WHERE m.session = ?1 AND m.ordinal BETWEEN ?2 AND ?3"
        } else {
            "SELECT id FROM entries WHERE session = ?1"
        };
        Self { ids, parameters }
    }
}

pub(super) fn read(
    connection: &Connection,
    session: i64,
    messages: Option<RangeInclusive<u64>>,
) -> Result<TranscriptEntries> {
    let selected = Selection::new(session, messages);
    Ok(read_selected(connection, session, &selected, ReadPurpose::Display)?.transcript)
}

pub(crate) struct StoredEntries {
    pub(crate) transcript: TranscriptEntries,
    pub(crate) numbers: Vec<Option<crate::agent::message_numbering::Numbered>>,
    pub(super) last_sequence: Option<i64>,
}

pub(super) enum ReadPurpose {
    Display,
    Search,
}

pub(super) fn read_numbered(
    connection: &Connection,
    session: i64,
    messages: Option<RangeInclusive<u64>>,
) -> Result<StoredEntries> {
    read_selected(
        connection,
        session,
        &Selection::new(session, messages),
        ReadPurpose::Display,
    )
}

/// Render the returned transcript only within `messages`; other bodies
/// contain reference context, never an empty successful content read.
pub(super) fn read_range(
    connection: &Connection,
    session: i64,
    path: &Path,
    messages: RangeInclusive<u64>,
) -> Result<crate::agent::transcript::AgentTranscript> {
    let mut transcript = context::read(connection, session, path)?;
    let selected = crate::agent::transcript::AgentTranscript::from_stored(
        path,
        read_numbered(connection, session, Some(messages))?,
    )?;
    for message in selected.messages {
        let index = transcript
            .messages
            .binary_search_by_key(&message.ordinal, |message| message.ordinal)
            .expect("selected message is in the context");
        transcript.messages[index] = message;
    }
    Ok(transcript)
}

pub(super) fn read_search_batch(
    connection: &Connection,
    session: i64,
    after: i64,
    window: &crate::time_filter::TimeFilter,
) -> Result<StoredEntries> {
    let selected = if window.is_active() {
        Selection {
            ids: "SELECT e.id FROM messages m JOIN entries e ON e.message = m.id \
              WHERE m.session = ?1 AND e.sequence > ?2 AND m.ordinal IS NOT NULL AND e.contributes \
              AND m.last_ms >= ?3 AND m.first_ms <= ?4 ORDER BY e.sequence LIMIT 512",
            parameters: vec![
                session.into(),
                after.into(),
                window
                    .after
                    .map_or(i64::MIN, |after| after.timestamp().saturating_mul(1000))
                    .into(),
                window
                    .before
                    .map_or(i64::MAX, |before| {
                        before.timestamp().saturating_mul(1000).saturating_add(999)
                    })
                    .into(),
            ],
        }
    } else {
        Selection {
            ids: "SELECT id FROM entries WHERE session = ?1 AND sequence > ?2 ORDER BY sequence LIMIT 512",
            parameters: vec![session.into(), after.into()],
        }
    };
    read_selected(connection, session, &selected, ReadPurpose::Search)
}

fn read_selected(
    connection: &Connection,
    session: i64,
    selected: &Selection,
    purpose: ReadPurpose,
) -> Result<StoredEntries> {
    let stored = threads::read(connection, session)?;
    let mut parts = parts::read(connection, selected)?;
    let sql = format!(
        "SELECT m.thread, e.id, e.record_transcript, e.record_line, e.kind, e.timestamp_ms, m.role, \
         e.content, e.text, e.label, e.uuid, m.cwd, m.parent_tool_use_id, m.source_tool_use_id, \
         m.reply_id, m.model, m.agent, COALESCE(e.usage_input, m.usage_input), \
         COALESCE(e.usage_output, m.usage_output), \
         COALESCE(e.usage_cache_creation, m.usage_cache_creation), \
         COALESCE(e.usage_cache_read, m.usage_cache_read), e.level, e.duration_ms, e.parent_uuid, e.flag, \
         m.ordinal, e.contributes, e.sequence \
         FROM entries e LEFT JOIN messages m ON m.id = e.message WHERE e.id IN ({}) \
         ORDER BY e.sequence",
        selected.ids,
    );
    let mut statement = connection.prepare(&sql)?;
    let mut rows = statement.query(rusqlite::params_from_iter(&selected.parameters))?;
    let mut entries = Vec::new();
    let mut numbers = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut last_sequence = None;
    while let Some(row) = rows.next()? {
        let ordinal = row
            .get::<_, Option<i64>>(25)?
            .map(|ordinal| ordinal as usize);
        let contributes: bool = row.get(26)?;
        numbers.push(ordinal.filter(|_| contributes).map(|ordinal| {
            crate::agent::message_numbering::Numbered {
                ordinal,
                starts_message: seen.insert(ordinal),
            }
        }));
        last_sequence = Some(row.get(27)?);
        let thread = row.get::<_, Option<i64>>(0)?.map(|id| &stored.threads[&id]);
        let record = RecordLine {
            transcript: row
                .get::<_, Option<i64>>(2)?
                .map(|number| stored.transcripts[&number].clone())
                .or_else(|| thread.and_then(|thread| thread.transcript.clone())),
            line: row.get::<_, i64>(3)? as usize,
        };
        let entry = row.get::<_, i64>(1)?;
        let blocks = parts.remove(&entry).unwrap_or_default();
        entries.push((record, entry_of(row, thread, blocks)?));
    }
    let malformed_lines = if matches!(purpose, ReadPurpose::Display) {
        connection
            .prepare(
                "SELECT line_number, line_content, error_message, context_before, context_after \
             FROM malformed_lines WHERE session = ?1 ORDER BY position",
            )?
            .query_map([session], |row| {
                let line_content: Option<String> = row.get(1)?;
                let detail = match line_content {
                    Some(line_content) => Some(MalformedLineDetail {
                        line_content,
                        error_message: row.get(2)?,
                        context_before: lines_of(&row.get::<_, String>(3)?),
                        context_after: lines_of(&row.get::<_, String>(4)?),
                    }),
                    None => None,
                };
                Ok(MalformedLine {
                    line_number: row.get::<_, i64>(0)? as usize,
                    detail,
                })
            })?
            .collect::<rusqlite::Result<_>>()?
    } else {
        Vec::new()
    };
    Ok(StoredEntries {
        transcript: TranscriptEntries {
            entries,
            malformed_lines,
        },
        numbers,
        last_sequence,
    })
}

fn entry_of(
    row: &Row,
    thread: Option<&threads::Thread>,
    blocks: Vec<ContentBlock>,
) -> rusqlite::Result<LogEntry> {
    let kind: String = row.get(4)?;
    let text = |index| row.get::<_, Option<String>>(index);
    let timestamp = row.get::<_, Option<i64>>(5)?.map(times::text).transpose()?;
    let usage = || -> rusqlite::Result<Option<TokenUsage>> {
        let Some(input_tokens) = row.get::<_, Option<i64>>(17)? else {
            return Ok(None);
        };
        Ok(Some(TokenUsage {
            input_tokens: input_tokens as u64,
            output_tokens: row.get::<_, i64>(18)? as u64,
            cache_creation_input_tokens: row.get::<_, i64>(19)? as u64,
            cache_read_input_tokens: row.get::<_, i64>(20)? as u64,
        }))
    };
    Ok(match kind.as_str() {
        "summary" => LogEntry::Summary {
            summary: text(8)?.unwrap_or_default(),
        },
        "user" => LogEntry::User {
            message: UserMessage {
                role: text(6)?.unwrap_or_default(),
                content: if text(7)?.as_deref() == Some(CONTENT_STRING) {
                    UserContent::String(match blocks.into_iter().next() {
                        Some(ContentBlock::Text { text }) => text,
                        _ => String::new(),
                    })
                } else {
                    UserContent::Blocks(blocks)
                },
            },
            timestamp,
            uuid: text(10)?,
            cwd: text(11)?,
            parent_tool_use_id: text(12)?,
            source_tool_use_id: text(13)?,
            usage: usage()?,
        },
        "assistant" => LogEntry::Assistant {
            message: AssistantMessage {
                role: text(6)?.unwrap_or_default(),
                content: blocks,
                model: text(15)?,
                usage: usage()?,
                id: text(14)?,
            },
            agent: text(16)?,
            timestamp,
            uuid: text(10)?,
            parent_tool_use_id: text(12)?,
        },
        "file-history-snapshot" => LogEntry::FileHistorySnapshot {
            message_id: String::new(),
            snapshot: Value::Null,
            is_snapshot_update: false,
        },
        "progress" => match thread {
            Some(thread) => thread.progress(text(6)?, text(14)?, timestamp, blocks),
            None => LogEntry::Progress {
                data: Value::Null,
                extra: Value::Null,
            },
        },
        "system" => LogEntry::System {
            subtype: text(9)?.unwrap_or_default(),
            level: text(21)?,
            duration_ms: row
                .get::<_, Option<i64>>(22)?
                .map(|duration| duration as u64),
            parent_uuid: text(23)?,
            extra: Value::Null,
        },
        "custom-title" => LogEntry::CustomTitle {
            custom_title: text(8)?.unwrap_or_default(),
        },
        "ai-title" => LogEntry::AiTitle {
            ai_title: text(8)?.unwrap_or_default(),
        },
        "agent-name" => LogEntry::AgentName {
            agent_name: text(8)?.unwrap_or_default(),
        },
        "permission-mode" => LogEntry::PermissionMode { extra: Value::Null },
        "pi-metadata" => LogEntry::PiMetadata {
            label: text(9)?.unwrap_or_default(),
            text: text(8)?.unwrap_or_default(),
            timestamp,
            searchable: row.get::<_, Option<bool>>(24)?.unwrap_or(true),
            usage: usage()?,
        },
        _ => LogEntry::Unknown,
    })
}

fn json_value(text: Option<&str>) -> rusqlite::Result<Option<Value>> {
    text.map(|text| {
        serde_json::from_str(text).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                0,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })
    })
    .transpose()
}

fn lines_of(json: &str) -> Vec<String> {
    serde_json::from_str(json).unwrap_or_default()
}

/// The id of the listed session `storage` found at `cache_key` under `root`.
pub(super) fn session_id_of(
    connection: &Connection,
    storage: &str,
    root: &str,
    cache_key: &str,
) -> Result<Option<i64>> {
    let row: Option<(i64, String, Option<String>)> = connection
        .query_row(
            "SELECT id, status, CASE WHEN is_missing_earlier_turns = 1 \
             THEN 'earlier session history needs rechecking' ELSE entries_error END FROM sessions \
             WHERE storage = ?1 AND root = ?2 AND cache_key = ?3",
            params![storage, root, cache_key],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let Some((id, status, error)) = row else {
        return Ok(None);
    };
    if status == super::catalogue::WRITING || error.is_some() {
        return Err(AppError::SessionCacheNotReady {
            session: cache_key.to_owned(),
            reason: error.unwrap_or_else(|| "refresh in progress".to_owned()),
        });
    }
    Ok(Some(id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_message_range_uses_message_and_entry_indexes() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(super::super::schema::CREATE)
            .unwrap();
        let selected = Selection::new(1, Some(2..=4));
        let plan = connection
            .prepare(&format!("EXPLAIN QUERY PLAN {}", selected.ids))
            .unwrap()
            .query_map(rusqlite::params_from_iter(&selected.parameters), |row| {
                row.get::<_, String>(3)
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
            .join("\n");
        assert!(plan.contains("sqlite_autoindex_messages_1"), "{plan}");
        assert!(plan.contains("entries_by_message"), "{plan}");
    }
}
