//! A session's stored search texts as lexical search reads them: each text
//! without terminal codes, with its search field, message number and place
//! in the session, borrowed from SQLite. Each table holding search text is
//! read through its index on the session, without joins.

use super::text_scan::{
    ReadOrder, StoredEntry, StoredMessage, StoredWindow, Table, field_of, text_at, text_ordinal,
};
use super::{CachedSession, SearchField};
use crate::error::Result;
use crate::time_filter::TimeFilter;
use rusqlite::Connection;
use rusqlite::types::ValueRef;
use std::ops::ControlFlow;

impl SearchField {
    /// The field's number in the `search_field` columns.
    pub(super) fn code(self) -> i64 {
        match self {
            Self::Full => 0,
            Self::Agent => 1,
            Self::Thinking => 2,
            Self::Leading => 3,
            Self::Supplement => 4,
            Self::Semantic => unreachable!("semantic text has no stored field"),
        }
    }

    /// The field numbered `code` in the `search_field` columns.
    pub(super) fn of_code(code: i64) -> Option<Self> {
        [
            Self::Full,
            Self::Agent,
            Self::Thinking,
            Self::Leading,
            Self::Supplement,
        ]
        .into_iter()
        .find(|field| field.code() == code)
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
    /// The fields read in the `search_field` columns of `parts` and
    /// `search_texts`.
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

    /// True when `field` is one of [`Self::part_fields`].
    pub(super) fn reads_part_field(self, field: SearchField) -> bool {
        self.part_fields().contains(&field)
    }
}

/// One stored search text.
pub(crate) struct SearchText<'a> {
    pub(crate) field: SearchField,
    /// The number of the message the text belongs to; `None` for the title,
    /// the summary and other text outside every message.
    pub(crate) ordinal: Option<usize>,
    /// The text's place in its session.
    pub(crate) order: ReadOrder,
    /// A text, or the text items of one tool result joined by line breaks.
    pub(crate) text: &'a str,
}

impl CachedSession {
    /// Calls `visit` with each search text in `fields`; with `window` active,
    /// only the texts of messages inside it, without the title, the summary
    /// or text outside every message. The texts come table by table, each
    /// placed in the session by its `order`. Stops when `visit` breaks.
    pub(crate) fn read_search_texts(
        &self,
        fields: SearchedFields,
        window: &TimeFilter,
        mut visit: impl FnMut(SearchText<'_>) -> ControlFlow<()>,
    ) -> Result<Option<()>> {
        self.read(|connection, session| {
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
/// The searched text items of the result part `?1`, in order.
const RESULT_ITEMS: &str =
    "SELECT text FROM result_items WHERE part = ?1 AND searched ORDER BY position";
/// The titles, summaries and other texts entries hold themselves.
const ENTRY_TEXTS: &str = "SELECT search_field, sequence, text FROM entries \
     WHERE session = ?1 AND search_field IS NOT NULL AND text IS NOT NULL";
const SUBAGENT_PARTS: &str = "SELECT position, text FROM subagent_search_parts WHERE session = ?1";

/// The session `?1`'s parts in `fields`: those holding text, and results,
/// whose text `result_items` holds.
fn parts_query(fields: SearchedFields) -> String {
    format!(
        "SELECT id, entry, search_field, part, text FROM parts \
         WHERE session = ?1 AND search_field IN ({})",
        fields.part_field_codes()
    )
}

fn search_texts_query(fields: SearchedFields) -> String {
    format!(
        "SELECT id, entry, search_field, text FROM search_texts \
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

/// A result part whose text `result_items` holds, placed in the session.
struct ResultPart {
    id: i64,
    field: SearchField,
    ordinal: Option<usize>,
    order: ReadOrder,
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

    /// Visits each text, table by table, until `visit` breaks.
    fn visit_texts(
        &self,
        visit: &mut impl FnMut(SearchText<'_>) -> ControlFlow<()>,
    ) -> Result<ControlFlow<()>> {
        let ControlFlow::Continue(results) = self.visit_parts(visit)? else {
            return Ok(ControlFlow::Break(()));
        };
        if self.visit_results(&results, visit)?.is_break()
            || self.visit_search_texts(visit)?.is_break()
        {
            return Ok(ControlFlow::Break(()));
        }
        if self.window.is_some() {
            return Ok(ControlFlow::Continue(()));
        }
        if self.visit_entry_texts(visit)?.is_break() {
            return Ok(ControlFlow::Break(()));
        }
        match self.fields {
            SearchedFields::List => Ok(ControlFlow::Continue(())),
            SearchedFields::Agent => self.visit_subagent_parts(visit),
        }
    }

    /// Visits the parts holding text; returns the results among the parts.
    fn visit_parts(
        &self,
        visit: &mut impl FnMut(SearchText<'_>) -> ControlFlow<()>,
    ) -> Result<ControlFlow<(), Vec<ResultPart>>> {
        let mut statement = self.connection.prepare_cached(&parts_query(self.fields))?;
        let mut rows = statement.query([self.session])?;
        let mut results = Vec::new();
        while let Some(row) = rows.next()? {
            let field = field_of(row.get(2)?)?;
            let Some((ordinal, order)) = self.place(row.get(1)?, Table::Parts, row.get(3)?, 0)
            else {
                continue;
            };
            match row.get_ref(4)? {
                ValueRef::Null => results.push(ResultPart {
                    id: row.get(0)?,
                    field,
                    ordinal,
                    order: ReadOrder {
                        table: Table::ResultItems,
                        ..order
                    },
                }),
                _ => {
                    let text = SearchText {
                        field,
                        ordinal,
                        order,
                        text: text_at(row, 4)?,
                    };
                    if visit(text).is_break() {
                        return Ok(ControlFlow::Break(()));
                    }
                }
            }
        }
        Ok(ControlFlow::Continue(results))
    }

    /// Visits the searched text items of each of `results` that holds any,
    /// joined by line breaks as search reads them.
    fn visit_results(
        &self,
        results: &[ResultPart],
        visit: &mut impl FnMut(SearchText<'_>) -> ControlFlow<()>,
    ) -> Result<ControlFlow<()>> {
        let mut statement = self.connection.prepare_cached(RESULT_ITEMS)?;
        let mut joined = String::new();
        for result in results {
            joined.clear();
            let mut items = statement.query([result.id])?;
            let mut holds_items = false;
            while let Some(item) = items.next()? {
                if holds_items {
                    joined.push('\n');
                }
                joined.push_str(text_at(item, 0)?);
                holds_items = true;
            }
            if holds_items {
                let text = SearchText {
                    field: result.field,
                    ordinal: result.ordinal,
                    order: result.order,
                    text: &joined,
                };
                if visit(text).is_break() {
                    return Ok(ControlFlow::Break(()));
                }
            }
        }
        Ok(ControlFlow::Continue(()))
    }

    fn visit_search_texts(
        &self,
        visit: &mut impl FnMut(SearchText<'_>) -> ControlFlow<()>,
    ) -> Result<ControlFlow<()>> {
        let mut statement = self
            .connection
            .prepare_cached(&search_texts_query(self.fields))?;
        let mut rows = statement.query([self.session])?;
        while let Some(row) = rows.next()? {
            let Some((ordinal, order)) =
                self.place(row.get(1)?, Table::SearchTexts, 0, row.get(0)?)
            else {
                continue;
            };
            let text = SearchText {
                field: field_of(row.get(2)?)?,
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

    /// Visits the titles, summaries and other texts outside every message
    /// that entries hold themselves.
    fn visit_entry_texts(
        &self,
        visit: &mut impl FnMut(SearchText<'_>) -> ControlFlow<()>,
    ) -> Result<ControlFlow<()>> {
        let mut statement = self.connection.prepare_cached(ENTRY_TEXTS)?;
        let mut rows = statement.query([self.session])?;
        while let Some(row) = rows.next()? {
            let text = SearchText {
                field: field_of(row.get(0)?)?,
                ordinal: None,
                order: ReadOrder {
                    table: Table::Entries,
                    sequence: row.get(1)?,
                    part: 0,
                    item: 0,
                },
                text: text_at(row, 2)?,
            };
            if visit(text).is_break() {
                return Ok(ControlFlow::Break(()));
            }
        }
        Ok(ControlFlow::Continue(()))
    }

    fn visit_subagent_parts(
        &self,
        visit: &mut impl FnMut(SearchText<'_>) -> ControlFlow<()>,
    ) -> Result<ControlFlow<()>> {
        let mut statement = self.connection.prepare_cached(SUBAGENT_PARTS)?;
        let mut rows = statement.query([self.session])?;
        while let Some(row) = rows.next()? {
            let text = SearchText {
                field: SearchField::Supplement,
                ordinal: None,
                order: ReadOrder {
                    table: Table::SubagentSearchParts,
                    sequence: 0,
                    part: 0,
                    item: row.get(0)?,
                },
                text: text_at(row, 1)?,
            };
            if visit(text).is_break() {
                return Ok(ControlFlow::Break(()));
            }
        }
        Ok(ControlFlow::Continue(()))
    }

    /// The message number and place of a text of `entry` in `table`, at
    /// `part` and `item` in the entry; `None` when the window leaves the text
    /// out, or the session holds no such entry.
    fn place(
        &self,
        entry: i64,
        table: Table,
        part: i64,
        item: i64,
    ) -> Option<(Option<usize>, ReadOrder)> {
        let stored = self.entries.get(entry)?;
        let message = stored
            .message
            .and_then(|message| self.messages.get(message));
        let ordinal = text_ordinal(stored, message, self.window)?;
        Some((
            ordinal,
            ReadOrder {
                table,
                sequence: stored.sequence,
                part,
                item,
            },
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One session's read costs what the session holds, not what the cache
    /// holds, only while each statement reads through an index.
    #[test]
    fn each_statement_reads_through_an_index_without_scanning_a_table() {
        let connection = rusqlite::Connection::open_in_memory().unwrap();
        connection
            .execute_batch(super::super::schema::CREATE)
            .unwrap();
        for fields in [SearchedFields::List, SearchedFields::Agent] {
            for query in [
                ENTRIES,
                MESSAGES,
                MESSAGES_WITH_TIMES,
                &parts_query(fields),
                RESULT_ITEMS,
                &search_texts_query(fields),
                ENTRY_TEXTS,
                SUBAGENT_PARTS,
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
                    "{query}: the statement searches an index and scans no table: {plan}"
                );
            }
        }
    }
}
