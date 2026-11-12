//! A session's search texts and semantic turns, read from its stored parts
//! for [`crate::history::Conversation::load_held_content`].

use super::search_texts::visit_texts_within;
use super::{SearchedFields, entries};
use crate::agent::refs::MessageRange;
use crate::error::Result;
use crate::history::parser::{normalize_full_text, normalize_whitespace};
use std::collections::HashMap;
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

/// Calls `visit` with each search text the session row `session` stores in
/// the fields `agent search` reads, as the parse lays them out and
/// normalises them, then each semantic turn. Text left empty is dropped.
pub(super) fn visit_search_parts(
    connection: &rusqlite::Connection,
    session: i64,
    mut visit: impl FnMut(SearchPart),
) -> Result<()> {
    visit_agent_search_texts(connection, session, &mut visit)?;
    visit_semantic_turns(connection, session, &mut visit)
}

/// [`visit_search_parts`]'s search texts, without the semantic turns.
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
    entries::read_semantic_turns(connection, session, |ordinal, text| {
        visit(SearchPart {
            ordinal: Some(ordinal),
            field: SearchField::Semantic,
            text,
        });
    })
}

/// A copy's semantic turns as the parse lays them out: one turn per message,
/// its entries' turns joined by a space, in the order of each message's
/// first entry turn.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct SemanticTurns {
    texts: Vec<String>,
    ranges: Vec<MessageRange>,
    /// Each ordinal's place in `texts`, built when an ordinal is pushed below
    /// the one before it. `None` until then: a repeated ordinal continues the
    /// last turn, and a greater one starts the next.
    places: Option<HashMap<usize, usize>>,
}

impl SemanticTurns {
    /// Joins `text` to the turn of the message of `ordinal`, or starts that
    /// turn.
    pub(crate) fn push(&mut self, ordinal: usize, text: String) {
        let last = self.ranges.last().map(|range| range.start);
        if self.places.is_none() && last.is_some_and(|last| ordinal < last) {
            self.places = Some(
                self.ranges
                    .iter()
                    .enumerate()
                    .map(|(place, range)| (range.start, place))
                    .collect(),
            );
        }
        let place = match &self.places {
            Some(places) => places.get(&ordinal).copied(),
            None => (last == Some(ordinal)).then(|| self.texts.len() - 1),
        };
        match place {
            Some(place) => {
                let turn = &mut self.texts[place];
                turn.push(' ');
                turn.push_str(&text);
            }
            None => {
                if let Some(places) = &mut self.places {
                    places.insert(ordinal, self.texts.len());
                }
                self.texts.push(text);
                self.ranges.push(MessageRange::single(ordinal));
            }
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.texts.is_empty()
    }

    /// The turns' texts and message ranges, in order.
    pub(crate) fn into_parts(self) -> (Vec<String>, Vec<MessageRange>) {
        (self.texts, self.ranges)
    }
}
