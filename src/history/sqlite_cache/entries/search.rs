//! The search field of each search text of an entry, decided as the entry is
//! written: a block whose stored text search reads as written carries the
//! field, and every other search text is stored once beside the entry. The
//! texts come from the parser, so the stored ones match what search read
//! when it built them from the entry.

use super::parts;
use crate::history::parser::{normalize_full_text, search_parts_of_entry};
use crate::history::sqlite_cache::SearchField;
use crate::log_entry::{ContentBlock, LogEntry};
use serde_json::Value;
use std::borrow::Cow;

/// One entry's search texts.
pub(super) struct EntrySearch {
    /// For each of the entry's blocks, where search reads its stored text.
    pub(super) blocks: Vec<Option<TextPlace>>,
    /// The texts no block holds as written, each with its place.
    pub(super) derived: Vec<DerivedText>,
}

/// The place of one of an entry's search texts: its field, and its position
/// among the entry's search texts in the order the parse lays them out. A
/// read of the cache lays them out by it as the parse does.
#[derive(Clone, Copy)]
pub(super) struct TextPlace {
    pub(super) field: SearchField,
    pub(super) position: i64,
}

pub(super) struct DerivedText {
    pub(super) place: TextPlace,
    pub(super) text: String,
}

/// `entry`'s search texts; `blocks` are the blocks its row stores. A
/// sub-agent turn read from a transcript the session splices in is
/// `spliced`.
pub(super) fn entry_search(
    entry: &LogEntry,
    blocks: &[ContentBlock],
    spliced: bool,
) -> EntrySearch {
    let mut search = EntrySearch {
        blocks: vec![None; blocks.len()],
        derived: Vec::new(),
    };
    let parts = search_parts_of_entry(entry, spliced);
    if parts.iter().all(Vec::is_empty) {
        return search;
    }
    let block_texts: Vec<Option<BlockText>> = blocks.iter().map(BlockText::of).collect();
    let fields = [SearchField::Full, SearchField::Agent, SearchField::Thinking];
    let texts = fields
        .into_iter()
        .zip(parts)
        .flat_map(|(field, texts)| texts.into_iter().map(move |text| (field, text)));
    for (position, (field, text)) in (0..).zip(texts) {
        let searched = searched_text(&text, &block_texts);
        if searched.is_empty() {
            continue;
        }
        let place = TextPlace { field, position };
        let holder = block_texts
            .iter()
            .zip(&search.blocks)
            .position(|(block, held)| {
                held.is_none()
                    && block
                        .as_ref()
                        .is_some_and(|block| block.searched == searched)
            });
        match holder {
            Some(index) => search.blocks[index] = Some(place),
            None => search.derived.push(DerivedText { place, text }),
        }
    }
    search
}

/// `text` as search reads it. A block storing `text` as written holds it
/// already normalised.
fn searched_text<'a>(text: &str, block_texts: &'a [Option<BlockText>]) -> Cow<'a, str> {
    block_texts
        .iter()
        .flatten()
        .find(|block| block.stored == text)
        .map_or_else(
            || Cow::Owned(normalize_full_text(text)),
            |block| Cow::Borrowed(block.searched.as_str()),
        )
}

/// A block's stored text, and the text search reads from it: without
/// terminal codes and with its whitespace collapsed.
struct BlockText<'a> {
    stored: Cow<'a, str>,
    searched: String,
}

impl<'a> BlockText<'a> {
    /// The stored text of `block`: a text, a thinking block, a result's
    /// string, or the text items of a result whose items the row stores,
    /// joined by line breaks. `None` for a block without stored text.
    fn of(block: &'a ContentBlock) -> Option<Self> {
        let stored = match block {
            ContentBlock::Text { text } => Cow::Borrowed(text.as_str()),
            ContentBlock::Thinking { thinking, .. } => Cow::Borrowed(thinking.as_str()),
            ContentBlock::ToolResult {
                content: Some(Value::String(text)),
                ..
            } => Cow::Borrowed(text.as_str()),
            ContentBlock::ToolResult {
                content: Some(Value::Array(items)),
                ..
            } if parts::stores_items(items) => Cow::Owned(parts::searched_items_text(items)),
            _ => return None,
        };
        let searched = normalize_full_text(&stored);
        Some(Self { stored, searched })
    }
}
