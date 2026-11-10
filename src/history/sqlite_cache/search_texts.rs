//! A session's stored search texts as lexical search reads them: each text
//! without terminal codes, with its message number and place in the
//! session, borrowed from SQLite. Each statement reads through an index on
//! the session, without joins.

use super::text_scan::{
    LayoutGroup, ReadOrder, StoredEntry, StoredMessage, StoredWindow, text_at, text_ordinal,
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

    /// True when a search text in `fields`, within `window` as
    /// [`Self::read_search_texts`] reads it, holds more than whitespace.
    pub(crate) fn holds_search_text(
        &self,
        fields: SearchedFields,
        window: &TimeFilter,
    ) -> Result<Option<bool>> {
        let mut holds = false;
        let read = self.read_search_texts(fields, window, |text| {
            holds = !text.text.trim().is_empty();
            if holds {
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        })?;
        Ok(read.map(|()| holds))
    }
}

const ENTRIES: &str = "SELECT id, message, contributes, sequence FROM entries WHERE session = ?1";
/// The session's messages with their times, which a window reads.
const MESSAGES_WITH_TIMES: &str =
    "SELECT id, ordinal, first_ms, last_ms FROM messages WHERE session = ?1";
/// The session's messages without their times, read from the index on
/// `(session, ordinal)` alone.
const MESSAGES: &str = "SELECT id, ordinal, NULL, NULL FROM messages WHERE session = ?1";
/// The session `?1`'s sub-agent metadata parts, outside every entry.
/// `ORDER BY part` makes SQLite read them through `parts_outside_entries`,
/// the index of those parts alone, not through the index of every part of
/// the session.
const SUBAGENT_METADATA_READ: &str =
    "SELECT part, text FROM parts WHERE session = ?1 AND entry IS NULL ORDER BY part";

/// The session `?1`'s parts holding search text in `fields`.
fn parts_query(fields: SearchedFields) -> String {
    format!(
        "SELECT entry, part, kind, text FROM parts \
         WHERE session = ?1 AND search_field IN ({})",
        fields.part_field_codes()
    )
}

/// A read of one session's search texts, with its entries and messages read
/// first to place each text.
struct SessionRead<'c> {
    connection: &'c Connection,
    session: i64,
    fields: SearchedFields,
    window: Option<StoredWindow>,
    entries: ById<StoredEntry>,
    messages: ById<StoredMessage>,
}

/// Rows by their id, sorted by it: a session's rows are read in another
/// order, and a hash map of a large session's rows took longer to build.
struct ById<T>(Vec<(i64, T)>);

impl<T: Copy> ById<T> {
    fn sorted(mut rows: Vec<(i64, T)>) -> Self {
        rows.sort_unstable_by_key(|(id, _)| *id);
        Self(rows)
    }

    fn get(&self, id: i64) -> Option<T> {
        let index = self.0.binary_search_by_key(&id, |(id, _)| *id).ok()?;
        Some(self.0[index].1)
    }
}

impl<'c> SessionRead<'c> {
    fn new(
        connection: &'c Connection,
        session: i64,
        fields: SearchedFields,
        window: Option<StoredWindow>,
    ) -> Result<Self> {
        let entries = connection
            .prepare_cached(ENTRIES)?
            .query_map([session], |row| {
                Ok((
                    row.get(0)?,
                    StoredEntry {
                        message: row.get(1)?,
                        contributes: row.get(2)?,
                        sequence: row.get(3)?,
                    },
                ))
            })?
            .collect::<rusqlite::Result<_>>()?;
        let messages = connection
            .prepare_cached(if window.is_some() {
                MESSAGES_WITH_TIMES
            } else {
                MESSAGES
            })?
            .query_map([session], |row| {
                Ok((
                    row.get(0)?,
                    StoredMessage {
                        ordinal: row.get(1)?,
                        first_ms: row.get(2)?,
                        last_ms: row.get(3)?,
                    },
                ))
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok(Self {
            connection,
            session,
            fields,
            window,
            entries: ById::sorted(entries),
            messages: ById::sorted(messages),
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
            let group = LayoutGroup::of_part(text_at(row, 2)?);
            let Some((ordinal, order)) = self.place(row.get(0)?, group, row.get(1)?) else {
                continue;
            };
            let text = SearchText {
                ordinal,
                order,
                text: text_at(row, 3)?,
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

    /// The message number and place of a text of `entry` in `group`, at
    /// `part` in the entry; `None` when the window leaves the text out, or
    /// the session doesn't hold `entry`.
    fn place(
        &self,
        entry: i64,
        group: LayoutGroup,
        part: i64,
    ) -> Option<(Option<usize>, ReadOrder)> {
        let stored = self.entries.get(entry)?;
        let message = stored
            .message
            .and_then(|message| self.messages.get(message));
        let ordinal = text_ordinal(stored, message, self.window)?;
        Some((
            ordinal,
            ReadOrder {
                group,
                sequence: stored.sequence,
                part,
            },
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One session's read costs what the session holds, not what the cache
    /// holds, only while each statement reads through an index. The sub-agent
    /// metadata read uses the index of those parts alone.
    #[test]
    fn each_statement_reads_through_an_index_without_scanning_a_table() {
        let connection = rusqlite::Connection::open_in_memory().unwrap();
        connection
            .execute_batch(super::super::schema::CREATE)
            .unwrap();
        for fields in [SearchedFields::List, SearchedFields::Agent] {
            for (query, index) in [
                (ENTRIES, None),
                (MESSAGES, None),
                (MESSAGES_WITH_TIMES, None),
                (parts_query(fields).as_str(), None),
                (SUBAGENT_METADATA_READ, Some("parts_outside_entries")),
            ] {
                let plan = connection
                    .prepare(&format!("EXPLAIN QUERY PLAN {query}"))
                    .unwrap()
                    .query_map([1], |row| row.get::<_, String>(3))
                    .unwrap()
                    .collect::<rusqlite::Result<Vec<_>>>()
                    .unwrap()
                    .join("\n");
                assert!(
                    plan.contains("SEARCH") && !plan.contains("SCAN"),
                    "{query}: the statement searches an index without scanning a table: {plan}"
                );
                if let Some(index) = index {
                    assert!(
                        plan.contains(&format!("INDEX {index} ")),
                        "{query}: the statement reads through {index}: {plan}"
                    );
                }
            }
        }
    }
}
