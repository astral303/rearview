//! A session's stored search texts as lexical search reads them: each text
//! without terminal codes, with its message number and place in the
//! session, borrowed from SQLite. Each statement reads through an index on
//! the session, without joins.

use super::SearchField;
use super::text_scan::{
    LayoutGroup, MessagesInWindow, ReadOrder, StoredPlace, StoredWindow, text_at,
};
use crate::error::Result;
use crate::time_filter::TimeFilter;
use rusqlite::Connection;
use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ValueRef};
use std::ops::ControlFlow;

/// Each stored field with its number in the `search_field` columns. Semantic
/// text isn't stored in any field.
const STORED_FIELDS: [(SearchField, i64); 4] = [
    (SearchField::Full, 0),
    (SearchField::Agent, 1),
    (SearchField::Thinking, 2),
    (SearchField::Supplement, 4),
];

impl SearchField {
    /// The field's number in the `search_field` columns.
    pub(super) fn code(self) -> i64 {
        STORED_FIELDS
            .iter()
            .find_map(|&(field, code)| (field == self).then_some(code))
            .expect("every field but semantic text is stored")
    }
}

/// A `search_field` column's field.
impl FromSql for SearchField {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        let code = value.as_i64()?;
        STORED_FIELDS
            .iter()
            .find_map(|&(field, stored)| (stored == code).then_some(field))
            .ok_or(FromSqlError::OutOfRange(code))
    }
}

/// The fields a search reads.
#[derive(Clone, Copy)]
pub(crate) enum SearchedFields {
    /// The session list's: the session's text, title and summary.
    List,
    /// `agent search`'s: also sub-agents' text, thinking, tool calls and the
    /// text of sub-agent transcripts outside their turns.
    Agent,
}

impl SearchedFields {
    /// The fields read in the `search_field` column of `parts`.
    fn part_fields(self) -> &'static [SearchField] {
        match self {
            Self::List => &[SearchField::Full],
            Self::Agent => &[SearchField::Full, SearchField::Agent, SearchField::Thinking],
        }
    }

    /// The codes of [`Self::part_fields`], as an SQL list.
    pub(super) fn part_field_codes(self) -> String {
        self.part_fields()
            .iter()
            .map(|field| field.code().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// One stored search text.
pub(crate) struct SearchText<'a> {
    /// The number of the message the text belongs to; `None` for text
    /// outside every message.
    pub(crate) ordinal: Option<usize>,
    /// The text's place in its session.
    pub(crate) order: ReadOrder,
    pub(crate) field: SearchField,
    /// A text, or the text items of one tool result joined by line breaks.
    pub(crate) text: &'a str,
}

/// Calls `visit` with each search text in `fields` of the session row
/// `session`, each placed in the session by its `order`, until `visit`
/// breaks. With `window` active, reads only the texts of messages inside it,
/// without text outside every message. Leaves out the session's title and
/// summary: search reads them from the listed row, to match a name changed
/// outside the transcript.
pub(super) fn visit_texts_within(
    connection: &Connection,
    session: i64,
    fields: SearchedFields,
    window: &TimeFilter,
    mut visit: impl FnMut(SearchText<'_>) -> ControlFlow<()>,
) -> Result<()> {
    let read = SessionRead::new(connection, session, fields, StoredWindow::of(window))?;
    let _ = read.visit_texts(&mut visit)?;
    Ok(())
}

/// The session `?1`'s sub-agent metadata parts, outside every entry.
/// `ORDER BY part` makes SQLite read them through `parts_outside_entries`,
/// the index of those parts alone, not through the index of every part of
/// the session.
const SUBAGENT_METADATA_READ: &str =
    "SELECT part, text FROM parts WHERE session = ?1 AND entry IS NULL ORDER BY part";

/// The columns a session's read takes of each part holding search text.
const READ_PART_COLUMNS: &str =
    "entry_sequence, message_ordinal, search_position, search_field, text";

/// The session `?1`'s parts holding search text in `fields`.
fn parts_query(fields: SearchedFields) -> String {
    format!(
        "SELECT {READ_PART_COLUMNS} FROM parts WHERE session = ?1 AND search_field IN ({})",
        fields.part_field_codes()
    )
}

/// The session `?1`'s parts holding search text in `fields` of its messages
/// numbered from `?2` to `?3`.
fn parts_of_messages_query(fields: SearchedFields) -> String {
    format!(
        "SELECT {READ_PART_COLUMNS} FROM parts WHERE session = ?1 \
         AND message_ordinal BETWEEN ?2 AND ?3 AND search_field IN ({})",
        fields.part_field_codes()
    )
}

/// A read of one session's search texts, each placed by its part's row.
struct SessionRead<'c> {
    connection: &'c Connection,
    session: i64,
    fields: SearchedFields,
    window: Option<MessagesInWindow>,
}

impl<'c> SessionRead<'c> {
    fn new(
        connection: &'c Connection,
        session: i64,
        fields: SearchedFields,
        window: Option<StoredWindow>,
    ) -> Result<Self> {
        let window = window
            .map(|window| MessagesInWindow::of_session(connection, window, session))
            .transpose()?;
        Ok(Self {
            connection,
            session,
            fields,
            window,
        })
    }

    /// Visits each text, the entries' parts first, until `visit` breaks.
    fn visit_texts(
        &self,
        visit: &mut impl FnMut(SearchText<'_>) -> ControlFlow<()>,
    ) -> Result<ControlFlow<()>> {
        if self.visit_parts(visit)?.is_break() {
            return Ok(ControlFlow::Break(()));
        }
        if self.window.is_some() {
            return Ok(ControlFlow::Continue(()));
        }
        match self.fields {
            SearchedFields::List => Ok(ControlFlow::Continue(())),
            SearchedFields::Agent => self.visit_subagent_metadata(visit),
        }
    }

    /// Visits the parts holding search text, of every kind: with a window,
    /// those of each run of messages inside it.
    fn visit_parts(
        &self,
        visit: &mut impl FnMut(SearchText<'_>) -> ControlFlow<()>,
    ) -> Result<ControlFlow<()>> {
        let Some(window) = &self.window else {
            let mut statement = self.connection.prepare_cached(&parts_query(self.fields))?;
            return Self::visit_rows(statement.query([self.session])?, visit);
        };
        let mut statement = self
            .connection
            .prepare_cached(&parts_of_messages_query(self.fields))?;
        for run in window.runs_of(self.session) {
            let rows = statement.query([self.session, *run.start(), *run.end()])?;
            if Self::visit_rows(rows, visit)?.is_break() {
                return Ok(ControlFlow::Break(()));
            }
        }
        Ok(ControlFlow::Continue(()))
    }

    /// Visits the texts of `rows`, each a row of [`READ_PART_COLUMNS`].
    fn visit_rows(
        mut rows: rusqlite::Rows<'_>,
        visit: &mut impl FnMut(SearchText<'_>) -> ControlFlow<()>,
    ) -> Result<ControlFlow<()>> {
        while let Some(row) = rows.next()? {
            let place = StoredPlace::at(row, (0, 1, 2))?;
            let text = SearchText {
                ordinal: place.message(),
                order: place.order(),
                field: row.get(3)?,
                text: text_at(row, 4)?,
            };
            if visit(text).is_break() {
                return Ok(ControlFlow::Break(()));
            }
        }
        Ok(ControlFlow::Continue(()))
    }

    fn visit_subagent_metadata(
        &self,
        visit: &mut impl FnMut(SearchText<'_>) -> ControlFlow<()>,
    ) -> Result<ControlFlow<()>> {
        let mut statement = self.connection.prepare_cached(SUBAGENT_METADATA_READ)?;
        let mut rows = statement.query([self.session])?;
        while let Some(row) = rows.next()? {
            let text = SearchText {
                ordinal: None,
                order: ReadOrder {
                    group: LayoutGroup::SubagentMetadata,
                    sequence: 0,
                    position: row.get(0)?,
                },
                field: SearchField::Supplement,
                text: text_at(row, 1)?,
            };
            if visit(text).is_break() {
                return Ok(ControlFlow::Break(()));
            }
        }
        Ok(ControlFlow::Continue(()))
    }
}

#[cfg(test)]
mod tests {
    use super::super::text_scan::parts_of_messages;
    use super::*;

    /// One session's read, and a write moving its search places, cost what
    /// the session holds, not what the cache holds, only while each statement
    /// reads through an index, without sorting: those two through
    /// `parts_by_session`. The sub-agent metadata read uses the index of
    /// those parts alone; a windowed read or scan reads a run of messages'
    /// parts through `parts_by_message`.
    #[test]
    fn each_statement_on_one_sessions_parts_reads_through_an_index_without_sorting() {
        let connection = rusqlite::Connection::open_in_memory().unwrap();
        connection
            .execute_batch(super::super::schema::CREATE)
            .unwrap();
        for fields in [SearchedFields::List, SearchedFields::Agent] {
            for (query, index) in [
                (parts_query(fields), "parts_by_session"),
                (parts_of_messages_query(fields), "parts_by_message"),
                (SUBAGENT_METADATA_READ.to_owned(), "parts_outside_entries"),
                (parts_of_messages(fields), "parts_by_message"),
                (
                    super::super::entries::SHIFT_SEARCH_PLACES.to_owned(),
                    "parts_by_session",
                ),
            ] {
                let mut statement = connection
                    .prepare(&format!("EXPLAIN QUERY PLAN {query}"))
                    .unwrap();
                let parameters = vec![1; statement.parameter_count()];
                let plan = statement
                    .query_map(rusqlite::params_from_iter(parameters), |row| {
                        row.get::<_, String>(3)
                    })
                    .unwrap()
                    .collect::<rusqlite::Result<Vec<_>>>()
                    .unwrap()
                    .join("\n");
                assert!(
                    plan.contains(&format!("SEARCH parts USING INDEX {index} "))
                        && !plan.contains("SCAN")
                        && !plan.contains("TEMP B-TREE"),
                    "{query}: the statement reads through {index} without scanning or sorting: \
                     {plan}"
                );
            }
        }
    }
}
