//! A session's stored search texts as lexical search reads them: each text
//! without terminal codes, with its message number and place in the
//! session, borrowed from SQLite. Each statement reads through an index on
//! the session, without joins.

use super::text_scan::{
    LayoutGroup, MessagesInWindow, ReadOrder, StoredPlace, StoredWindow, text_at,
};
use super::{CachedSession, SearchField};
use crate::error::Result;
use crate::time_filter::TimeFilter;
use rusqlite::Connection;
use std::ops::ControlFlow;

impl SearchField {
    /// The field's number in the `search_field` columns.
    pub(super) fn code(self) -> i64 {
        match self {
            Self::Full => 0,
            Self::Agent => 1,
            Self::Thinking => 2,
            Self::Supplement => 4,
            Self::Semantic => unreachable!("semantic text has no stored field"),
        }
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
    /// A text, or the text items of one tool result joined by line breaks.
    pub(crate) text: &'a str,
}

impl CachedSession {
    /// Calls `visit` with each search text in `fields`, each placed in the
    /// session by its `order`, until `visit` breaks. With `window` active,
    /// reads only the texts of messages inside it, without text outside every
    /// message. Leaves out the session's title and summary: search reads them
    /// from the listed row, to match a name changed outside the transcript.
    pub(crate) fn read_search_texts(
        &self,
        fields: SearchedFields,
        window: &TimeFilter,
        mut visit: impl FnMut(SearchText<'_>) -> ControlFlow<()>,
    ) -> Result<Option<()>> {
        self.read(|connection, session, _| {
            let read = SessionRead::new(connection, session, fields, StoredWindow::of(window))?;
            let _ = read.visit_texts(&mut visit)?;
            Ok(())
        })
    }
}

/// The session `?1`'s sub-agent metadata parts, outside every entry.
/// `ORDER BY part` makes SQLite read them through `parts_outside_entries`,
/// the index of those parts alone, not through the index of every part of
/// the session.
const SUBAGENT_METADATA_READ: &str =
    "SELECT part, text FROM parts WHERE session = ?1 AND entry IS NULL ORDER BY part";

/// The session `?1`'s parts holding search text in `fields`.
fn parts_query(fields: SearchedFields) -> String {
    format!(
        "SELECT part, kind, entry_sequence, message_ordinal, text FROM parts \
         WHERE session = ?1 AND search_field IN ({})",
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

    /// Visits the parts holding search text, of every kind.
    fn visit_parts(
        &self,
        visit: &mut impl FnMut(SearchText<'_>) -> ControlFlow<()>,
    ) -> Result<ControlFlow<()>> {
        let mut statement = self.connection.prepare_cached(&parts_query(self.fields))?;
        let mut rows = statement.query([self.session])?;
        while let Some(row) = rows.next()? {
            let place = StoredPlace::at(row, (2, 3))?;
            if let Some(window) = &self.window
                && !window.holds(self.session, place.ordinal)
            {
                continue;
            }
            let text = SearchText {
                ordinal: place.message(),
                order: ReadOrder {
                    group: LayoutGroup::of_part(text_at(row, 1)?),
                    sequence: place.sequence,
                    part: row.get(0)?,
                },
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
                    part: row.get(0)?,
                },
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
    /// reads through an index, without sorting. The sub-agent metadata read
    /// uses the index of those parts alone; a windowed scan reads one
    /// message's parts through `parts_by_message`.
    #[test]
    fn each_statement_on_one_sessions_parts_reads_through_an_index_without_sorting() {
        let connection = rusqlite::Connection::open_in_memory().unwrap();
        connection
            .execute_batch(super::super::schema::CREATE)
            .unwrap();
        for fields in [SearchedFields::List, SearchedFields::Agent] {
            for (query, index) in [
                (parts_query(fields), "parts_by_message"),
                (SUBAGENT_METADATA_READ.to_owned(), "parts_outside_entries"),
                (parts_of_messages(fields), "parts_by_message"),
                (
                    super::super::entries::SHIFT_SEARCH_PLACES.to_owned(),
                    "parts_by_message",
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
