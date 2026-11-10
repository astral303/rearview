//! A session's search texts and semantic turns, read from its stored parts
//! for [`crate::history::Conversation::load_held_content`].

use super::search_texts::visit_texts_within;
use super::{CachedSession, SearchedFields, entries};
use crate::error::Result;
use crate::history::parser::{normalize_full_text, normalize_whitespace};
use std::ops::ControlFlow;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SearchField {
    Full,
    Agent,
    Thinking,
    Supplement,
    Semantic,
}

pub(crate) struct SearchPart {
    pub(crate) ordinal: Option<usize>,
    pub(crate) field: SearchField,
    pub(crate) text: String,
}

impl CachedSession {
    /// Calls `visit` with each message's semantic text the copy stores, as
    /// [`visit_search_parts`] does, without the search texts.
    pub(crate) fn stored_semantic_turns(
        &self,
        mut visit: impl FnMut(SearchPart),
    ) -> Result<Option<()>> {
        self.read(|connection, session, _| visit_semantic_turns(connection, session, &mut visit))
    }
}

/// Calls `visit` with each search text the session row `session` stores in
/// the fields `agent search` reads, as the parse lays them out and
/// normalises them, then each message's semantic text. Text left empty is
/// dropped.
pub(super) fn visit_search_parts(
    connection: &rusqlite::Connection,
    session: i64,
    mut visit: impl FnMut(SearchPart),
) -> Result<()> {
    visit_agent_search_texts(connection, session, &mut visit)?;
    visit_semantic_turns(connection, session, &mut visit)
}

/// [`visit_search_parts`]'s search texts, without the semantic texts.
pub(super) fn visit_agent_search_texts(
    connection: &rusqlite::Connection,
    session: i64,
    visit: &mut impl FnMut(SearchPart),
) -> Result<()> {
    let mut texts = Vec::new();
    let unwindowed = crate::time_filter::TimeFilter::default();
    visit_texts_within(
        connection,
        session,
        SearchedFields::Agent,
        &unwindowed,
        |text| {
            let normalised = match text.field {
                SearchField::Supplement => normalize_whitespace(text.text),
                _ => normalize_full_text(text.text),
            };
            if !normalised.is_empty() {
                let part = SearchPart {
                    ordinal: text.ordinal,
                    field: text.field,
                    text: normalised,
                };
                texts.push((text.order, part));
            }
            ControlFlow::Continue(())
        },
    )?;
    texts.sort_by_key(|(order, _)| *order);
    for (_, part) in texts {
        visit(part);
    }
    Ok(())
}

pub(super) fn visit_semantic_turns(
    connection: &rusqlite::Connection,
    session: i64,
    visit: &mut impl FnMut(SearchPart),
) -> Result<()> {
    entries::read_semantic_texts(connection, session, |ordinal, text| {
        visit(SearchPart {
            ordinal: Some(ordinal),
            field: SearchField::Semantic,
            text,
        });
    })
}
