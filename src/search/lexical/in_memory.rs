//! Each listed session's search texts in the session list's fields, read
//! from the session cache into memory, so a search of the list scans memory
//! instead of reading the cache. The texts stay as the cache stores them.

use crate::error::Result;
use crate::history::Conversation;
use crate::history::message_texts::PADDED_MESSAGE_SEPARATOR;
use crate::history::sqlite_cache::{CachedSession, CopyKey, SearchField, SearchedFields};
use crate::time_filter::TimeFilter;
use rayon::prelude::*;
use std::borrow::Borrow;
use std::collections::HashMap;
use std::ops::{ControlFlow, Range};
use std::sync::Arc;

/// One session's search texts in the session list's fields, as the session
/// cache stores them: the title and the summary, the text outside every
/// message, then each message's texts in message order, each after a
/// separator as `full_text` lays out its parts.
pub struct SessionSearchTexts {
    joined: Arc<str>,
    /// Each text's byte range in `joined`.
    texts: Box<[Range<usize>]>,
}

impl SessionSearchTexts {
    /// The texts of `cached`'s copy as the cache holds it, whether or not its
    /// transcripts changed since; `None` when the cache no longer holds it.
    pub(crate) fn read(cached: &CachedSession) -> Result<Option<Self>> {
        let copy = cached.clone().ignoring_source_changes().waiting();
        let mut read = TextsInReadOrder::default();
        let found =
            copy.visit_search_texts(SearchedFields::List, &TimeFilter::default(), |text| {
                read.push(Placement::of(text.field, text.ordinal), text.text);
                ControlFlow::Continue(())
            })?;
        Ok(found.map(|()| read.laid_out()))
    }

    /// Each text, in layout order.
    pub(super) fn texts(&self) -> impl Iterator<Item = &str> {
        self.texts.iter().map(|range| &self.joined[range.clone()])
    }

    /// Every text, each after a separator.
    pub fn joined(&self) -> &Arc<str> {
        &self.joined
    }
}

/// Where a text goes in the session's layout: the title and the summary
/// first, then the text outside every message, then each message's texts.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Placement {
    Leading,
    OutsideMessages,
    Message(usize),
}

impl Placement {
    fn of(field: SearchField, ordinal: Option<usize>) -> Self {
        match (field, ordinal) {
            (SearchField::Leading, _) => Self::Leading,
            (_, None) => Self::OutsideMessages,
            (_, Some(ordinal)) => Self::Message(ordinal),
        }
    }
}

/// The texts as the cache query returns them, one after another.
#[derive(Default)]
struct TextsInReadOrder {
    text: String,
    /// Each text's byte range in `text`.
    texts: Vec<(Placement, Range<usize>)>,
}

impl TextsInReadOrder {
    fn push(&mut self, placement: Placement, text: &str) {
        let start = self.text.len();
        self.text.push_str(text);
        self.texts.push((placement, start..self.text.len()));
    }

    /// The texts in layout order, keeping the query's order within a
    /// placement.
    fn laid_out(mut self) -> SessionSearchTexts {
        self.texts.sort_by_key(|(placement, _)| *placement);
        let separators = self.texts.len().saturating_sub(1) * PADDED_MESSAGE_SEPARATOR.len();
        let mut joined = String::with_capacity(self.text.len() + separators);
        let mut texts = Vec::with_capacity(self.texts.len());
        for (_, range) in self.texts {
            if !joined.is_empty() {
                joined.push_str(PADDED_MESSAGE_SEPARATOR);
            }
            let start = joined.len();
            joined.push_str(&self.text[range]);
            texts.push(start..joined.len());
        }
        SessionSearchTexts {
            joined: Arc::from(joined),
            texts: texts.into_boxed_slice(),
        }
    }
}

/// The search texts read into memory, by the copy of the session they were
/// read from.
#[derive(Default)]
pub struct SearchTextsInMemory {
    /// `None` while the copy's texts are being read.
    by_copy: HashMap<CopyKey, Option<Arc<SessionSearchTexts>>>,
}

impl SearchTextsInMemory {
    /// The texts of each of `conversations` the session cache holds, read in
    /// parallel. A session whose read fails, or whose copy the cache no
    /// longer holds, is left out: its search reads the cache, which reports
    /// the failure.
    pub fn read<C: Borrow<Conversation> + Sync>(conversations: &[C]) -> Self {
        let by_copy = conversations
            .par_iter()
            .filter_map(|conversation| {
                let cached = conversation.borrow().cached_session.as_ref()?;
                let texts = SessionSearchTexts::read(cached).ok().flatten()?;
                Some((cached.copy_key(), Some(Arc::new(texts))))
            })
            .collect();
        Self { by_copy }
    }

    /// The texts of `conversation`'s copy, once they are read.
    pub fn get(&self, conversation: &Conversation) -> Option<&SessionSearchTexts> {
        let cached = conversation.cached_session.as_ref()?;
        self.by_copy.get(&cached.copy_key())?.as_deref()
    }

    /// Keeps the texts of the copies `conversations` read and drops the
    /// rest. Returns the copies whose texts are neither held nor being read,
    /// now marked as being read.
    pub(crate) fn keep_copies_of<C: Borrow<Conversation>>(
        &mut self,
        conversations: &[C],
    ) -> Vec<CachedSession> {
        let mut kept = HashMap::with_capacity(conversations.len());
        let mut to_read = Vec::new();
        for conversation in conversations {
            let Some(cached) = &conversation.borrow().cached_session else {
                continue;
            };
            let key = cached.copy_key();
            let texts = self.by_copy.remove(&key).unwrap_or_else(|| {
                to_read.push(cached.clone());
                None
            });
            kept.insert(key, texts);
        }
        self.by_copy = kept;
        to_read
    }

    /// Holds `texts`, the read of `copy`, while a row still reads that copy.
    /// A failed read, `None`, is read again when the rows next change.
    pub(crate) fn finish_read(&mut self, copy: CopyKey, texts: Option<Arc<SessionSearchTexts>>) {
        match texts {
            Some(texts) => {
                if let Some(held) = self.by_copy.get_mut(&copy) {
                    *held = Some(texts);
                }
            }
            None => {
                self.by_copy.remove(&copy);
            }
        }
    }
}
