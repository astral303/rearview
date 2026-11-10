//! The statements that write a session's entry rows, for a first write and
//! for the changes a later write makes.

use super::parts;
use super::rows::{EntryRow, SessionRows};
use crate::agent::sanitize::split_terminal_codes;
use crate::history::sqlite_cache::SearchField;
use rusqlite::{CachedStatement, Connection, ToSql, params};

/// The database id of each of a session's rows, by its index in
/// [`SessionRows`]; `None` until the row is written or matched with a
/// stored one.
pub(super) struct RowIds {
    pub(super) transcripts: Vec<Option<i64>>,
    pub(super) threads: Vec<Option<i64>>,
    pub(super) messages: Vec<Option<i64>>,
}

impl RowIds {
    pub(super) fn new(rows: &SessionRows) -> Self {
        Self {
            transcripts: vec![None; rows.transcripts.len()],
            threads: vec![None; rows.threads.len()],
            messages: vec![None; rows.messages.len()],
        }
    }
}

/// The columns an entry's place in its session sets.
const ENTRY_PLACE_COLUMNS: &[&str] = &[
    "message",
    "contributes",
    "position",
    "sequence",
    "record_transcript",
    "search_field",
];

/// The columns [`EntryRow::digest`] hashes, the digest last.
const ENTRY_CONTENT_COLUMNS: &[&str] = &[
    "record_line",
    "kind",
    "timestamp_ms",
    "content",
    "text",
    "text_codes",
    "label",
    "uuid",
    "usage_input",
    "usage_output",
    "usage_cache_creation",
    "usage_cache_read",
    "level",
    "duration_ms",
    "parent_uuid",
    "flag",
    "digest",
];

const MESSAGE_COLUMNS: &[&str] = &[
    "thread",
    "ordinal",
    "role",
    "reply_id",
    "first_ms",
    "last_ms",
    "model",
    "agent",
    "cwd",
    "parent_tool_use_id",
    "source_tool_use_id",
    "report_has_text",
    "report_task_id",
    "report_launch_call",
    "usage_input",
    "usage_output",
    "usage_cache_creation",
    "usage_cache_read",
    "digest",
];

/// `INSERT` of `session` (`?1`) and `columns` (`?2` on).
fn insert_sql(table: &str, columns: &[&[&str]]) -> String {
    let columns: Vec<&str> = columns.concat();
    let parameters: Vec<String> = (2..=columns.len() + 1)
        .map(|index| format!("?{index}"))
        .collect();
    format!(
        "INSERT INTO {table} (session, {}) VALUES (?1, {})",
        columns.join(", "),
        parameters.join(", ")
    )
}

/// `UPDATE` of `columns` (`?2` on) on the row of `session` (`?1`) with the id
/// after them.
fn update_sql(table: &str, columns: &[&[&str]]) -> String {
    let columns: Vec<&str> = columns.concat();
    let assignments: Vec<String> = columns
        .iter()
        .enumerate()
        .map(|(index, column)| format!("{column} = ?{}", index + 2))
        .collect();
    format!(
        "UPDATE {table} SET {} WHERE id = ?{} AND session = ?1",
        assignments.join(", "),
        columns.len() + 2
    )
}

/// An entry's place in its session, as database ids.
pub(super) struct EntryPlace {
    pub(super) sequence: i64,
    pub(super) message: Option<i64>,
    pub(super) transcript: Option<i64>,
}

impl EntryPlace {
    /// `row`'s place at `sequence`, with the ids `ids` gives its message and
    /// transcript, which are written already.
    pub(super) fn of(row: &EntryRow, sequence: usize, ids: &RowIds) -> Self {
        Self {
            sequence: sequence as i64,
            message: row
                .message
                .map(|message| ids.messages[message].expect("an entry's message is written first")),
            transcript: row.transcript.map(|transcript| {
                ids.transcripts[transcript].expect("an entry's transcript is written first")
            }),
        }
    }
}

pub(super) struct RowWriter<'connection> {
    connection: &'connection Connection,
    session: i64,
    insert_entry: CachedStatement<'connection>,
    update_entry: CachedStatement<'connection>,
    insert_search_text: CachedStatement<'connection>,
    parts: parts::Writer<'connection>,
}

impl<'connection> RowWriter<'connection> {
    pub(super) fn new(connection: &'connection Connection, session: i64) -> rusqlite::Result<Self> {
        Ok(Self {
            connection,
            session,
            insert_entry: connection.prepare_cached(&insert_sql(
                "entries",
                &[ENTRY_PLACE_COLUMNS, ENTRY_CONTENT_COLUMNS],
            ))?,
            update_entry: connection.prepare_cached(&update_sql(
                "entries",
                &[ENTRY_PLACE_COLUMNS, ENTRY_CONTENT_COLUMNS],
            ))?,
            // A search text is read only by search, which reads it without
            // terminal codes, so its codes are dropped.
            insert_search_text: connection.prepare_cached(
                "INSERT INTO search_texts (session, entry, search_field, text) VALUES (?1, ?2, ?3, ?4)",
            )?,
            parts: parts::Writer::new(connection)?,
        })
    }

    /// The id of the transcript at `index`, inserting it when `ids` has none.
    pub(super) fn transcript(
        &self,
        rows: &SessionRows,
        ids: &mut RowIds,
        index: usize,
    ) -> rusqlite::Result<i64> {
        if let Some(id) = ids.transcripts[index] {
            return Ok(id);
        }
        self.connection
            .prepare_cached("INSERT INTO transcripts (session, path) VALUES (?1, ?2)")?
            .execute(params![
                self.session,
                super::path_text(&rows.transcripts[index])
            ])?;
        let id = self.connection.last_insert_rowid();
        ids.transcripts[index] = Some(id);
        Ok(id)
    }

    /// The id of the thread at `index`, inserting it when `ids` has none.
    pub(super) fn thread(
        &self,
        rows: &SessionRows,
        ids: &mut RowIds,
        index: usize,
    ) -> rusqlite::Result<i64> {
        if let Some(id) = ids.threads[index] {
            return Ok(id);
        }
        let thread = &rows.threads[index];
        let origin = &thread.origin;
        let transcript = match thread.transcript {
            Some(index) => Some(self.transcript(rows, ids, index)?),
            None => None,
        };
        self.connection
            .prepare_cached(
                "INSERT INTO threads (session, number, transcript, label, nickname, role, \
                 agent_path, agent_id, launch_call) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            )?
            .execute(params![
                self.session,
                thread.number,
                transcript,
                origin.label,
                origin.nickname,
                origin.role,
                origin.agent_path,
                origin.agent_id,
                origin.launch_call
            ])?;
        let id = self.connection.last_insert_rowid();
        ids.threads[index] = Some(id);
        Ok(id)
    }

    /// The id of the message at `index`, inserting it when `ids` has none.
    pub(super) fn message(
        &self,
        rows: &SessionRows,
        ids: &mut RowIds,
        index: usize,
    ) -> rusqlite::Result<i64> {
        if let Some(id) = ids.messages[index] {
            return Ok(id);
        }
        let message = &rows.messages[index];
        let thread = self.thread(rows, ids, message.thread)?;
        let mut statement = self
            .connection
            .prepare_cached(&insert_sql("messages", &[MESSAGE_COLUMNS]))?;
        statement.raw_bind_parameter(1, self.session)?;
        statement.raw_bind_parameter(2, thread)?;
        for (offset, value) in message.values().iter().enumerate() {
            statement.raw_bind_parameter(offset + 3, value)?;
        }
        statement.raw_execute()?;
        let id = self.connection.last_insert_rowid();
        ids.messages[index] = Some(id);
        Ok(id)
    }

    /// Sets every column of the stored message `id` to the message at
    /// `index`'s, whose thread `ids` holds.
    pub(super) fn update_message(
        &self,
        rows: &SessionRows,
        ids: &RowIds,
        index: usize,
        id: i64,
    ) -> rusqlite::Result<()> {
        let message = &rows.messages[index];
        let mut statement = self
            .connection
            .prepare_cached(&update_sql("messages", &[MESSAGE_COLUMNS]))?;
        statement.raw_bind_parameter(1, self.session)?;
        statement.raw_bind_parameter(2, ids.threads[message.thread])?;
        let values = message.values();
        for (offset, value) in values.iter().enumerate() {
            statement.raw_bind_parameter(offset + 3, value)?;
        }
        statement.raw_bind_parameter(values.len() + 3, id)?;
        statement.raw_execute()?;
        Ok(())
    }

    /// Inserts the entry at `sequence` with its parts and search texts,
    /// first inserting its message, thread and transcript when `ids` has
    /// none for them.
    pub(super) fn insert_entry(
        &mut self,
        rows: &SessionRows,
        ids: &mut RowIds,
        sequence: usize,
    ) -> rusqlite::Result<i64> {
        let row = &rows.entries[sequence];
        if let Some(message) = row.message {
            self.message(rows, ids, message)?;
        }
        if let Some(transcript) = row.transcript {
            self.transcript(rows, ids, transcript)?;
        }
        let place = EntryPlace::of(row, sequence, ids);
        bind_entry(&mut self.insert_entry, self.session, row, &place)?;
        self.insert_entry.raw_execute()?;
        let id = self.connection.last_insert_rowid();
        self.insert_children(row, id)?;
        Ok(id)
    }

    /// Sets every column of the stored entry `id` to `row`'s, at `place`,
    /// and replaces its parts and search texts.
    pub(super) fn update_entry(
        &mut self,
        row: &EntryRow,
        place: &EntryPlace,
        id: i64,
    ) -> rusqlite::Result<()> {
        bind_entry(&mut self.update_entry, self.session, row, place)?;
        self.update_entry.raw_bind_parameter(
            ENTRY_PLACE_COLUMNS.len() + ENTRY_CONTENT_COLUMNS.len() + 2,
            id,
        )?;
        self.update_entry.raw_execute()?;
        self.connection
            .prepare_cached("DELETE FROM parts WHERE entry = ?1")?
            .execute([id])?;
        self.connection
            .prepare_cached("DELETE FROM search_texts WHERE entry = ?1")?
            .execute([id])?;
        self.insert_children(row, id)
    }

    /// Moves the stored entry `id` to `place`, and sets whether search reads
    /// it as the session's title or summary.
    pub(super) fn place_entry(
        &self,
        row: &EntryRow,
        place: &EntryPlace,
        id: i64,
    ) -> rusqlite::Result<()> {
        let mut statement = self
            .connection
            .prepare_cached(&update_sql("entries", &[ENTRY_PLACE_COLUMNS]))?;
        bind_place(&mut statement, self.session, row, place)?;
        statement.raw_bind_parameter(ENTRY_PLACE_COLUMNS.len() + 2, id)?;
        statement.raw_execute()?;
        Ok(())
    }

    fn insert_children(&mut self, row: &EntryRow, id: i64) -> rusqlite::Result<()> {
        self.parts.write(
            self.connection,
            id,
            row.columns.parts.blocks(),
            &row.search.blocks,
        )?;
        for derived in &row.search.derived {
            self.insert_search_text.execute(params![
                self.session,
                id,
                derived.field.code(),
                split_terminal_codes(&derived.text).text
            ])?;
        }
        Ok(())
    }
}

/// Binds `session` and `row`'s [`ENTRY_PLACE_COLUMNS`] at `place`.
fn bind_place(
    statement: &mut CachedStatement,
    session: i64,
    row: &EntryRow,
    place: &EntryPlace,
) -> rusqlite::Result<()> {
    let search_field = row.is_leading.then(|| SearchField::Leading.code());
    let values: [&dyn ToSql; 7] = [
        &session,
        &place.message,
        &row.contributes,
        &row.position,
        &place.sequence,
        &place.transcript,
        &search_field,
    ];
    for (offset, value) in values.into_iter().enumerate() {
        statement.raw_bind_parameter(offset + 1, value)?;
    }
    Ok(())
}

/// Binds `session` and every column of `row` at `place`.
fn bind_entry(
    statement: &mut CachedStatement,
    session: i64,
    row: &EntryRow,
    place: &EntryPlace,
) -> rusqlite::Result<()> {
    bind_place(statement, session, row, place)?;
    let columns = &row.columns;
    let text = columns.text.map(split_terminal_codes);
    let usage = row.metadata_usage();
    let count =
        |count: fn(&crate::log_entry::TokenUsage) -> u64| usage.map(|usage| count(usage) as i64);
    let values: [&dyn ToSql; 17] = [
        &(row.record.line as i64),
        &columns.kind,
        &columns.timestamp_ms,
        &columns.content,
        &text.as_ref().map(|text| &*text.text),
        &text.as_ref().and_then(|text| text.codes.as_deref()),
        &columns.label,
        &columns.uuid,
        &count(|usage| usage.input_tokens),
        &count(|usage| usage.output_tokens),
        &count(|usage| usage.cache_creation_input_tokens),
        &count(|usage| usage.cache_read_input_tokens),
        &columns.level,
        &columns.duration_ms.map(|duration| duration as i64),
        &columns.parent_uuid,
        &columns.flag,
        &row.digest,
    ];
    for (offset, value) in values.into_iter().enumerate() {
        statement.raw_bind_parameter(ENTRY_PLACE_COLUMNS.len() + 2 + offset, value)?;
    }
    Ok(())
}
