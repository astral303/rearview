//! Content is loaded when a consumer needs it, keeping catalogue loads small.

use super::Conversation;
use super::message_texts::{MessageTextBuilder, push_separated};
use super::sqlite_cache::{SearchField, SearchPart};
use crate::agent::refs::MessageRange;
use crate::error::{AppError, Result};
use std::borrow::Cow;
use std::collections::HashMap;

impl Conversation {
    /// This row with its content: the copy the session cache holds, as it
    /// holds it, whether or not the transcript changed since. Only the
    /// refresh owner parses transcripts, so a copy missing from the cache, or
    /// replaced since the row was read, is a miss.
    pub(crate) fn load_content(&self) -> Result<Cow<'_, Self>> {
        let Some(cached) = &self.cached_session else {
            return Ok(Cow::Borrowed(self));
        };
        let mut parts = ContentBuilder::default();
        let copy = cached.clone().ignoring_source_changes().waiting();
        match copy.search_parts(self, |part| parts.push(part))? {
            Some(()) => {
                let mut row = self.clone();
                parts.finish(&mut row);
                Ok(Cow::Owned(row))
            }
            None => Err(AppError::SessionCacheNotReady {
                session: self.session_id.clone(),
                reason: "the session is not in the session cache".to_owned(),
            }),
        }
    }
}

#[derive(Default)]
struct ContentBuilder {
    full: MessageTextBuilder,
    agent: MessageTextBuilder,
    thinking: MessageTextBuilder,
    leading: Vec<String>,
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
            SearchField::Leading => self.leading.push(part.text),
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
        (row.full_text, row.message_text_starts.full_text) = self.full.finish(
            self.leading.iter().map(String::as_str),
            row.message_count,
            identity,
        );
        (
            row.agent_search_text,
            row.message_text_starts.agent_search_text,
        ) = self.agent.finish([], row.message_count, identity);
        (
            row.thinking_and_tool_call_text,
            row.message_text_starts.thinking_and_tool_call_text,
        ) = self.thinking.finish([], row.message_count, identity);
        for text in self.supplement {
            push_separated(&mut row.agent_search_text, &text);
        }
        row.search_text_lower = crate::search::normalize_for_search(&row.full_text);
        row.semantic_route_text =
            super::semantic_route_text(&row.full_text, &row.agent_search_text);
        row.semantic_turns = self.semantic;
        row.semantic_turn_ranges = self.semantic_ranges;
        row.cached_session = None;
    }
}
