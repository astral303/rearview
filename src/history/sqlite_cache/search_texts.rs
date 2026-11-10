//! A session's stored search texts as lexical search reads them: each text
//! without terminal codes, with its search field and message number, borrowed
//! from SQLite, in one query.

use super::{CachedSession, SearchField};
use crate::error::Result;
use crate::time_filter::TimeFilter;
use rusqlite::types::Type;
use rusqlite::{Row, params};
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
    fn of_code(code: i64) -> Option<Self> {
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

/// One stored search text.
pub(crate) struct SearchText<'a> {
    pub(crate) field: SearchField,
    /// The number of the message the text belongs to; `None` for the title,
    /// the summary and other text outside every message.
    pub(crate) ordinal: Option<usize>,
    /// A text, or the text items of one tool result joined by line breaks.
    pub(crate) text: &'a str,
}

impl CachedSession {
    /// Calls `visit` with each search text in `fields`; with `window` active,
    /// only the texts of messages inside it, without the title, the summary
    /// or text outside every message. Stops when `visit` breaks.
    pub(crate) fn visit_search_texts(
        &self,
        fields: SearchedFields,
        window: &TimeFilter,
        mut visit: impl FnMut(SearchText<'_>) -> ControlFlow<()>,
    ) -> Result<Option<()>> {
        self.read(|connection, session| {
            let mut statement = connection.prepare_cached(&query(fields, window.is_active()))?;
            let mut rows = if window.is_active() {
                let after = window
                    .after
                    .map_or(i64::MIN, |after| after.timestamp().saturating_mul(1000));
                let before = window.before.map_or(i64::MAX, |before| {
                    before.timestamp().saturating_mul(1000).saturating_add(999)
                });
                statement.query(params![session, after, before])?
            } else {
                statement.query([session])?
            };
            let mut items = ResultItems::default();
            while let Some(row) = rows.next()? {
                let text = row.get_ref(2)?.as_str().map_err(|error| {
                    rusqlite::Error::FromSqlConversionFailure(2, Type::Text, Box::new(error))
                })?;
                let flow = match row.get::<_, Option<i64>>(3)? {
                    Some(part) if items.part == Some(part) => {
                        items.push(text);
                        ControlFlow::Continue(())
                    }
                    Some(part) => {
                        let flow = items.visit(&mut visit);
                        items.start(part, Metadata::of(row)?, text);
                        flow
                    }
                    None => {
                        let metadata = Metadata::of(row)?;
                        if items.visit(&mut visit).is_break() {
                            ControlFlow::Break(())
                        } else {
                            visit(metadata.text(text))
                        }
                    }
                };
                if flow.is_break() {
                    return Ok(());
                }
            }
            let _ = items.visit(&mut visit);
            Ok(())
        })
    }
}

/// The query reading a session's search texts in `fields`, inside the
/// window `?2` to `?3` when `windowed`: rows of field, message number, text,
/// and for a result's text item, the result's part.
fn query(fields: SearchedFields, windowed: bool) -> String {
    let field_codes = match fields {
        SearchedFields::List => format!("= {}", SearchField::Full.code()),
        SearchedFields::Agent => format!(
            "IN ({}, {}, {})",
            SearchField::Full.code(),
            SearchField::Agent.code(),
            SearchField::Thinking.code()
        ),
    };
    let (ordinal, messages, inside) = if windowed {
        (
            "m.ordinal",
            "JOIN messages m ON m.id = e.message",
            " AND e.contributes AND m.ordinal IS NOT NULL AND m.last_ms >= ?2 AND m.first_ms <= ?3",
        )
    } else {
        (
            "CASE WHEN e.contributes THEN m.ordinal END",
            "LEFT JOIN messages m ON m.id = e.message",
            "",
        )
    };
    let mut sql = format!(
        "SELECT p.search_field, {ordinal}, p.text, NULL FROM entries e \
         JOIN parts p ON p.entry = e.id {messages} \
         WHERE e.session = ?1 AND p.search_field {field_codes} AND p.text IS NOT NULL{inside} \
         UNION ALL \
         SELECT p.search_field, {ordinal}, i.text, p.id FROM entries e \
         JOIN parts p ON p.entry = e.id JOIN result_items i ON i.part = p.id {messages} \
         WHERE e.session = ?1 AND p.search_field {field_codes} AND p.text IS NULL \
         AND i.searched{inside} \
         UNION ALL \
         SELECT s.search_field, {ordinal}, s.text, NULL FROM search_texts s \
         JOIN entries e ON e.id = s.entry {messages} \
         WHERE s.session = ?1 AND s.search_field {field_codes}{inside}"
    );
    if !windowed {
        sql.push_str(
            " UNION ALL SELECT search_field, NULL, text, NULL FROM entries \
             WHERE session = ?1 AND search_field IS NOT NULL AND text IS NOT NULL",
        );
        if matches!(fields, SearchedFields::Agent) {
            sql.push_str(&format!(
                " UNION ALL SELECT {}, NULL, text, NULL \
                 FROM subagent_search_parts WHERE session = ?1",
                SearchField::Supplement.code()
            ));
        }
    }
    sql
}

/// A row's field and message number.
#[derive(Clone, Copy)]
struct Metadata {
    field: SearchField,
    ordinal: Option<usize>,
}

impl Metadata {
    fn of(row: &Row) -> rusqlite::Result<Self> {
        let code: i64 = row.get(0)?;
        let field = SearchField::of_code(code).ok_or_else(|| {
            rusqlite::Error::FromSqlConversionFailure(
                0,
                Type::Integer,
                format!("unknown search field {code}").into(),
            )
        })?;
        Ok(Self {
            field,
            ordinal: row
                .get::<_, Option<i64>>(1)?
                .and_then(|ordinal| usize::try_from(ordinal).ok()),
        })
    }

    fn text(self, text: &str) -> SearchText<'_> {
        SearchText {
            field: self.field,
            ordinal: self.ordinal,
            text,
        }
    }
}

/// The text items of one tool result, joined by line breaks as search reads
/// them. The buffer is reused from one result to the next.
#[derive(Default)]
struct ResultItems {
    part: Option<i64>,
    metadata: Option<Metadata>,
    text: String,
}

impl ResultItems {
    fn start(&mut self, part: i64, metadata: Metadata, text: &str) {
        self.part = Some(part);
        self.metadata = Some(metadata);
        self.text.clear();
        self.text.push_str(text);
    }

    fn push(&mut self, text: &str) {
        self.text.push('\n');
        self.text.push_str(text);
    }

    /// Visits the result being joined, if any, and empties the buffers.
    fn visit(
        &mut self,
        visit: &mut impl FnMut(SearchText<'_>) -> ControlFlow<()>,
    ) -> ControlFlow<()> {
        self.part = None;
        match self.metadata.take() {
            Some(metadata) => visit(metadata.text(&self.text)),
            None => ControlFlow::Continue(()),
        }
    }
}
