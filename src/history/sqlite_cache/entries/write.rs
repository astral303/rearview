//! The statements that write a session's entry rows, for a first write and
//! for the changes a later write makes.

use super::parts::{self, EntryIds};
use super::rows::{EntryRow, SessionRows};
use super::semantic;
use crate::agent::sanitize::split_terminal_codes;
use crate::history::sqlite_cache::roots::WrittenPlaces;
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
    "child",
];

/// The columns [`EntryRow::columns_digest`] hashes, then the two digests.
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
    "splice_key",
    "columns_digest",
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
    pub(super) child: Option<i64>,
}

impl EntryPlace {
    /// `row`'s place at `sequence`, with the ids `ids` gives its message and
    /// transcripts. Write the message and transcripts first.
    pub(super) fn of(row: &EntryRow, sequence: usize, ids: &RowIds) -> Self {
        let transcript =
            |index: usize| ids.transcripts[index].expect("an entry's transcript is written first");
        Self {
            sequence: sequence as i64,
            message: row
                .message
                .map(|message| ids.messages[message].expect("an entry's message is written first")),
            transcript: row.transcript.map(transcript),
            child: row.child.map(transcript),
        }
    }
}

/// The place search reads an entry's texts at, stored on each part search
/// reads: the entry's sequence, and its message's ordinal when the entry
/// contributes to the message.
#[derive(Clone, Copy)]
pub(super) struct SearchPlace {
    pub(super) sequence: i64,
    pub(super) ordinal: Option<i64>,
}

impl SearchPlace {
    /// The place of the entry at `index` in `rows`.
    pub(super) fn of(rows: &SessionRows, index: usize) -> Self {
        let row = &rows.entries[index];
        Self {
            sequence: rows.sequence_of(index) as i64,
            ordinal: row
                .message
                .filter(|_| row.contributes)
                .and_then(|message| rows.messages[message].ordinal),
        }
    }
}

pub(super) struct RowWriter<'connection> {
    connection: &'connection Connection,
    session: i64,
    /// The session's roots, to place each transcript the writer inserts.
    places: &'connection WrittenPlaces,
    insert_entry: CachedStatement<'connection>,
    parts: parts::Writer<'connection>,
}

impl<'connection> RowWriter<'connection> {
    pub(super) fn new(
        connection: &'connection Connection,
        session: i64,
        places: &'connection WrittenPlaces,
    ) -> rusqlite::Result<Self> {
        Ok(Self {
            connection,
            session,
            places,
            insert_entry: connection.prepare_cached(&insert_sql(
                "entries",
                &[ENTRY_PLACE_COLUMNS, ENTRY_CONTENT_COLUMNS],
            ))?,
            parts: parts::Writer::new(connection)?,
        })
    }

    /// The id of the transcript at `index`, inserting the transcript when
    /// `ids` doesn't have its id.
    pub(super) fn transcript(
        &self,
        rows: &SessionRows,
        ids: &mut RowIds,
        index: usize,
    ) -> crate::error::Result<i64> {
        if let Some(id) = ids.transcripts[index] {
            return Ok(id);
        }
        let place = self.places.place_written(&rows.transcripts[index])?;
        self.connection
            .prepare_cached("INSERT INTO transcripts (session, root, path) VALUES (?1, ?2, ?3)")?
            .execute(params![self.session, place.root, place.path])?;
        let id = self.connection.last_insert_rowid();
        ids.transcripts[index] = Some(id);
        Ok(id)
    }

    /// The id of the thread at `index`, inserting the thread when `ids`
    /// doesn't have its id.
    pub(super) fn thread(
        &self,
        rows: &SessionRows,
        ids: &mut RowIds,
        index: usize,
    ) -> crate::error::Result<i64> {
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

    /// The id of the message at `index`, inserting the message when `ids`
    /// doesn't have its id.
    pub(super) fn message(
        &self,
        rows: &SessionRows,
        ids: &mut RowIds,
        index: usize,
    ) -> crate::error::Result<i64> {
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

    /// Inserts the entry at `index` with its parts, first inserting its
    /// message, thread and transcripts when `ids` doesn't have their ids.
    pub(super) fn insert_entry(
        &mut self,
        rows: &SessionRows,
        ids: &mut RowIds,
        index: usize,
    ) -> crate::error::Result<i64> {
        let row = &rows.entries[index];
        if let Some(message) = row.message {
            self.message(rows, ids, message)?;
        }
        for transcript in [row.transcript, row.child].into_iter().flatten() {
            self.transcript(rows, ids, transcript)?;
        }
        let place = EntryPlace::of(row, rows.sequence_of(index), ids);
        bind_entry(&mut self.insert_entry, self.session, row, &place)?;
        self.insert_entry.raw_execute()?;
        let id = self.connection.last_insert_rowid();
        let search = SearchPlace::of(rows, index);
        self.parts.write(
            EntryIds {
                session: self.session,
                entry: id,
            },
            &row.build_parts(),
            search,
        )?;
        if let Some(turn) = &row.semantic_turn {
            semantic::insert(
                self.connection,
                self.session,
                place.sequence,
                search.ordinal,
                turn,
            )?;
        }
        Ok(id)
    }

    /// Writes `row`'s content over the stored entry `id`, keeping its place:
    /// the columns when their digest changed, else the digest alone, the
    /// parts through [`parts::Writer::update`], a changed or new part at
    /// `search`, and the semantic turn when it changed.
    pub(super) fn update_content(
        &mut self,
        row: &EntryRow,
        id: i64,
        search: SearchPlace,
    ) -> rusqlite::Result<()> {
        let stored_columns: i64 = self
            .connection
            .prepare_cached("SELECT columns_digest FROM entries WHERE id = ?1")?
            .query_row([id], |stored| stored.get(0))?;
        if stored_columns == row.columns_digest {
            self.connection
                .prepare_cached("UPDATE entries SET digest = ?2 WHERE id = ?1")?
                .execute(params![id, row.digest()])?;
        } else {
            let mut statement = self
                .connection
                .prepare_cached(&update_sql("entries", &[ENTRY_CONTENT_COLUMNS]))?;
            statement.raw_bind_parameter(1, self.session)?;
            bind_content(&mut statement, row, 2)?;
            statement.raw_bind_parameter(ENTRY_CONTENT_COLUMNS.len() + 2, id)?;
            statement.raw_execute()?;
        }
        self.parts.update(
            EntryIds {
                session: self.session,
                entry: id,
            },
            &row.build_parts(),
            search,
        )?;
        semantic::replace_of_entry(
            self.connection,
            id,
            search.ordinal,
            row.semantic_turn.as_deref(),
        )
    }

    /// [`Self::update_content`] at the place the stored entry `id` and its
    /// message give. The row keeps its place and its message.
    pub(super) fn rewrite_content(&mut self, row: &EntryRow, id: i64) -> rusqlite::Result<()> {
        let search = self
            .connection
            .prepare_cached(
                "SELECT e.sequence, CASE WHEN e.contributes THEN m.ordinal END FROM entries e \
                 LEFT JOIN messages m ON m.id = e.message WHERE e.id = ?1",
            )?
            .query_row([id], |stored| {
                Ok(SearchPlace {
                    sequence: stored.get(0)?,
                    ordinal: stored.get(1)?,
                })
            })?;
        self.update_content(row, id, search)
    }

    /// Sets every column of the stored message `id` but its thread and its
    /// ordinal to the message at `index`'s.
    pub(super) fn rewrite_message(
        &self,
        rows: &SessionRows,
        index: usize,
        id: i64,
    ) -> rusqlite::Result<()> {
        let columns = &MESSAGE_COLUMNS[2..];
        let mut statement = self
            .connection
            .prepare_cached(&update_sql("messages", &[columns]))?;
        statement.raw_bind_parameter(1, self.session)?;
        let values = rows.messages[index].values();
        for (offset, value) in values[1..].iter().enumerate() {
            statement.raw_bind_parameter(offset + 2, value)?;
        }
        statement.raw_bind_parameter(columns.len() + 2, id)?;
        statement.raw_execute()?;
        Ok(())
    }

    /// Moves the stored entry `id` and its semantic turn to `place`. Sets
    /// `search` on the parts search reads, and `search`'s ordinal on the turn.
    pub(super) fn place_entry(
        &self,
        row: &EntryRow,
        place: &EntryPlace,
        search: SearchPlace,
        id: i64,
    ) -> rusqlite::Result<()> {
        semantic::place(
            self.connection,
            self.session,
            id,
            place.sequence,
            search.ordinal,
        )?;
        let mut statement = self
            .connection
            .prepare_cached(&update_sql("entries", &[ENTRY_PLACE_COLUMNS]))?;
        bind_place(&mut statement, self.session, row, place)?;
        statement.raw_bind_parameter(ENTRY_PLACE_COLUMNS.len() + 2, id)?;
        statement.raw_execute()?;
        self.connection
            .prepare_cached(
                "UPDATE parts SET entry_sequence = ?2, message_ordinal = ?3 \
                 WHERE entry = ?1 AND search_field IS NOT NULL",
            )?
            .execute(params![id, search.sequence, search.ordinal])?;
        Ok(())
    }

    /// Moves the stored entry `id` and its semantic turn to the free sequence
    /// `-id`, out of the way of the entries moving after it.
    pub(super) fn move_aside(&self, id: i64) -> rusqlite::Result<()> {
        semantic::move_to(self.connection, self.session, id, -id)?;
        self.connection
            .prepare_cached("UPDATE entries SET sequence = -id WHERE id = ?1")?
            .execute([id])?;
        Ok(())
    }

    /// Sets `ordinal`, the new ordinal of `message`, on the search places of
    /// the parts of the stored entries contributing to it, and on their
    /// semantic turns. An entry moving to another message, or changing
    /// whether it contributes, gets its place from [`Self::place_entry`].
    pub(super) fn renumber_search_places(
        &self,
        message: i64,
        ordinal: Option<i64>,
    ) -> rusqlite::Result<()> {
        self.connection
            .prepare_cached(
                "UPDATE parts SET message_ordinal = ?2 WHERE entry IN \
                 (SELECT id FROM entries WHERE message = ?1 AND contributes) \
                 AND entry_sequence IS NOT NULL",
            )?
            .execute(params![message, ordinal])?;
        semantic::renumber(self.connection, message, ordinal)
    }
}

/// Binds `session` and `row`'s [`ENTRY_PLACE_COLUMNS`] at `place`.
fn bind_place(
    statement: &mut CachedStatement,
    session: i64,
    row: &EntryRow,
    place: &EntryPlace,
) -> rusqlite::Result<()> {
    let values: [&dyn ToSql; 7] = [
        &session,
        &place.message,
        &row.contributes,
        &row.position,
        &place.sequence,
        &place.transcript,
        &place.child,
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
    bind_content(statement, row, ENTRY_PLACE_COLUMNS.len() + 2)
}

/// Binds `row`'s [`ENTRY_CONTENT_COLUMNS`] from parameter `first` on.
fn bind_content(
    statement: &mut CachedStatement,
    row: &EntryRow,
    first: usize,
) -> rusqlite::Result<()> {
    let columns = &row.columns;
    let text = columns.text.map(split_terminal_codes);
    let usage = row.metadata_usage();
    let count =
        |count: fn(&crate::log_entry::TokenUsage) -> u64| usage.map(|usage| count(usage) as i64);
    let values: [&dyn ToSql; 19] = [
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
        &row.splice_key,
        &row.columns_digest,
        &row.digest(),
    ];
    for (offset, value) in values.into_iter().enumerate() {
        statement.raw_bind_parameter(first + offset, value)?;
    }
    Ok(())
}
