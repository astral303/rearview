//! Content is loaded when a consumer needs it, keeping catalogue loads small.

use super::Conversation;
use super::message_texts::{MessageTextBuilder, push_separated};
use super::sqlite_cache::{
    self, CachedSession, HeldCopy, RouteKey, ScannedTurns, SearchField, SearchPart, SemanticRoute,
};
use crate::agent::refs::MessageRange;
use crate::error::Result;
use rayon::prelude::*;
use std::borrow::Cow;
use std::collections::HashMap;

impl Conversation {
    /// This row with the content of the copy the session cache holds at its
    /// address now, whichever generation it is, as it holds it; `None` when
    /// the session is no longer in the session cache. A row without a copy
    /// is its own content.
    pub(crate) fn load_held_content(&self) -> Result<Option<HeldContent<'_>>> {
        let Some(cached) = &self.cached_session else {
            return Ok(Some(HeldContent {
                row: Cow::Borrowed(self),
                generation: None,
            }));
        };
        let read = self.read_held_parts(cached, |held, parts| {
            held.stored_search_parts(|part| parts.push(part))
        })?;
        Ok(read.map(|held| HeldContent {
            row: Cow::Owned(held.row),
            generation: Some(held.generation),
        }))
    }

    /// This row with the semantic turns of the copy the cache holds at its
    /// address now, whichever generation it is, as
    /// [`Self::load_held_content`] reads it, and the agent route's text
    /// (see [`Self::semantic_route_text`]); the search texts stay empty.
    /// The route is the one stored with the copy for the row's title and
    /// summary, otherwise computed from the copy's texts and returned to
    /// store. `None` when the session is no longer in the session cache.
    pub(crate) fn load_semantic_content(&self) -> Result<Option<SemanticContent<'_>>> {
        let Some(cached) = &self.cached_session else {
            return Ok(Some(SemanticContent {
                route_text: (!self.semantic_turns.is_empty()).then(|| self.semantic_route_text()),
                held: HeldContent {
                    row: Cow::Borrowed(self),
                    generation: None,
                },
                computed_route: None,
            }));
        };
        let (title, summary) = (self.custom_title.as_deref(), self.summary.as_deref());
        let read = self.read_held_parts(cached, |held, parts| {
            held.stored_semantic_turns(|part| parts.push(part))?;
            if parts.semantic.is_empty() {
                return Ok(None);
            }
            Ok(Some(match held.stored_semantic_route(title, summary)? {
                Some(text) => RouteRead::Stored(text),
                None => {
                    held.stored_agent_search_texts(|part| parts.push(part))?;
                    RouteRead::Unstored(held.semantic_route_key())
                }
            }))
        })?;
        Ok(read.map(|held| {
            let HeldParts {
                mut row,
                generation,
                result: route,
            } = held;
            let (route_text, computed_route) = match route {
                None => (None, None),
                Some(RouteRead::Stored(text)) => (Some(text), None),
                Some(RouteRead::Unstored(key)) => {
                    let text = row.semantic_route_text();
                    (Some(text.clone()), Some(key.route(title, summary, text)))
                }
            };
            row.clear_search_texts();
            SemanticContent {
                held: HeldContent {
                    row: Cow::Owned(row),
                    generation: Some(generation),
                },
                route_text,
                computed_route,
            }
        }))
    }

    fn clear_search_texts(&mut self) {
        self.full_text = String::new();
        self.agent_search_text = String::new();
        self.thinking_and_tool_call_text = String::new();
        self.search_text_lower = String::new();
        self.message_text_starts = Default::default();
    }

    /// Each of `rows`, each a row and its copy, with the semantic turns of the
    /// copy the cache holds at its address now, whichever generation it is,
    /// as [`Self::load_held_content`] reads them; the search texts stay
    /// empty. One result per row, in order; `None` when the session is no
    /// longer in the session cache.
    ///
    /// The rows are read together in tasks across threads
    /// ([`sqlite_cache::scan_semantic_turns`]); a row whose copy another
    /// write replaced is read alone. Fails when the cache cannot be opened or
    /// read before any row's turns; a failed read of one row's turns fails
    /// that row alone.
    pub(crate) fn load_held_semantic_turns_of(
        rows: &[(&Conversation, &CachedSession)],
    ) -> Result<Vec<Result<Option<CopyContent>>>> {
        let copies = rows.iter().map(|&(_, cached)| cached).collect::<Vec<_>>();
        let scanned = sqlite_cache::scan_semantic_turns(&copies)?;
        Ok(rows
            .par_iter()
            .zip(scanned)
            .map(|(&(row, cached), scanned)| match scanned {
                ScannedTurns::Read(turns) => {
                    let mut parts = ContentBuilder::default();
                    for (ordinal, text) in turns {
                        parts.push(SearchPart {
                            ordinal: Some(ordinal),
                            field: SearchField::Semantic,
                            text,
                        });
                    }
                    let mut loaded = row.clone();
                    parts.finish(&mut loaded);
                    Ok(Some(CopyContent {
                        row: loaded,
                        generation: cached.generation(),
                    }))
                }
                ScannedTurns::Replaced => {
                    Ok(row
                        .read_held_semantic_turns(cached)?
                        .map(|held| CopyContent {
                            row: held.row,
                            generation: held.generation,
                        }))
                }
                ScannedTurns::NoLongerCached => Ok(None),
                ScannedTurns::Failed(error) => Err(error),
            })
            .collect())
    }

    /// This row with the semantic turns of `cached`'s copy the cache holds
    /// now; `None` when the session is no longer in the session cache.
    fn read_held_semantic_turns(&self, cached: &CachedSession) -> Result<Option<HeldParts<()>>> {
        self.read_held_parts(cached, |held, parts| {
            held.stored_semantic_turns(|part| parts.push(part))
        })
    }

    /// This row with the parts `read_parts` reads of `cached`'s copy the
    /// cache holds now; `None` when the session is no longer in the session
    /// cache. Messages are counted and timed as that copy holds them when it
    /// replaced the row's.
    fn read_held_parts<T>(
        &self,
        cached: &CachedSession,
        read_parts: impl FnOnce(&HeldCopy<'_>, &mut ContentBuilder) -> Result<T>,
    ) -> Result<Option<HeldParts<T>>> {
        let copy = cached.clone().ignoring_source_changes().waiting();
        let read = copy.read_held_copy(|held| {
            let mut parts = ContentBuilder::default();
            let result = read_parts(held, &mut parts)?;
            // The row's own copy keeps the row's times, read with the row.
            let replaced_times = if held.generation() == cached.generation() {
                None
            } else {
                Some(held.message_times()?)
            };
            Ok((parts, result, replaced_times, held.generation()))
        })?;
        Ok(read.map(|(parts, result, replaced_times, generation)| {
            let mut row = self.clone();
            if let Some((message_count, message_times)) = replaced_times {
                row.message_count = message_count;
                row.message_times = message_times;
            }
            parts.finish(&mut row);
            HeldParts {
                row,
                generation,
                result,
            }
        }))
    }
}

/// A read of [`Conversation::read_held_parts`].
struct HeldParts<T> {
    /// The row with the parts read.
    row: Conversation,
    /// The generation of the copy read.
    generation: i64,
    /// The result of the read's `read_parts`.
    result: T,
}

/// A row's content, read by [`Conversation::load_held_content`].
pub(crate) struct HeldContent<'r> {
    pub(crate) row: Cow<'r, Conversation>,
    /// The generation of the copy the content was read from; `None` for a
    /// row without a copy.
    pub(crate) generation: Option<i64>,
}

/// A cached row with the content of the copy the cache holds, read by
/// [`Conversation::load_held_semantic_turns_of`].
pub(crate) struct CopyContent {
    pub(crate) row: Conversation,
    /// The generation of the copy read.
    pub(crate) generation: i64,
}

/// A row's semantic content, read by [`Conversation::load_semantic_content`].
pub(crate) struct SemanticContent<'r> {
    pub(crate) held: HeldContent<'r>,
    /// The agent route's text; `None` for a copy without semantic turns,
    /// which has no route.
    pub(crate) route_text: Option<String>,
    /// The route text computed from the copy's texts, to store with it;
    /// `None` when it was read as stored, or the row has no copy.
    pub(crate) computed_route: Option<SemanticRoute>,
}

/// The route text a copy's read found.
enum RouteRead {
    Stored(String),
    /// None stored: computed from the texts read, stored under this key.
    Unstored(RouteKey),
}

#[derive(Default)]
struct ContentBuilder {
    full: MessageTextBuilder,
    agent: MessageTextBuilder,
    thinking: MessageTextBuilder,
    supplement: Vec<String>,
    semantic: Vec<String>,
    semantic_ranges: Vec<MessageRange>,
    semantic_indices: HashMap<usize, usize>,
}

impl ContentBuilder {
    fn push(&mut self, part: SearchPart) {
        match part.field {
            SearchField::Full => self.full.push(part.ordinal, part.text),
            SearchField::Agent => self.agent.push(part.ordinal, part.text),
            SearchField::Thinking => self.thinking.push(part.ordinal, part.text),
            SearchField::Supplement => self.supplement.push(part.text),
            SearchField::Semantic => {
                let ordinal = part.ordinal.expect("a semantic turn has a message");
                if let Some(&index) = self.semantic_indices.get(&ordinal) {
                    self.semantic[index].push(' ');
                    self.semantic[index].push_str(&part.text);
                } else {
                    self.semantic_indices.insert(ordinal, self.semantic.len());
                    self.semantic.push(part.text);
                    self.semantic_ranges.push(MessageRange::single(ordinal));
                }
            }
        }
    }

    fn finish(self, row: &mut Conversation) {
        let identity = |text: &str| text.to_owned();
        (row.full_text, row.message_text_starts.full_text) =
            self.full.finish(row.message_count, identity);
        (
            row.agent_search_text,
            row.message_text_starts.agent_search_text,
        ) = self.agent.finish(row.message_count, identity);
        (
            row.thinking_and_tool_call_text,
            row.message_text_starts.thinking_and_tool_call_text,
        ) = self.thinking.finish(row.message_count, identity);
        for text in self.supplement {
            push_separated(&mut row.agent_search_text, &text);
        }
        row.search_text_lower = crate::search::normalize_for_search(&row.full_text);
        row.semantic_turns = self.semantic;
        row.semantic_turn_ranges = self.semantic_ranges;
        row.cached_session = None;
    }
}
