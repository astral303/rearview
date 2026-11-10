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
/// read of the cache orders the texts by it.
#[derive(Clone, Copy)]
pub(super) struct TextPlace {
    pub(super) field: SearchField,
    /// The field's rank in [`ENTRY_SEARCH_FIELDS`] in the high bits, the
    /// text's index among the field's texts in the low bits (see
    /// [`field_position_bits`]): a text appended to one field leaves the
    /// texts of a later field where they are.
    pub(super) position: i64,
}

/// The fields of an entry's search texts, in the order the parse lays them
/// out.
pub(super) const ENTRY_SEARCH_FIELDS: [SearchField; 3] =
    [SearchField::Full, SearchField::Agent, SearchField::Thinking];

/// The bits of a [`TextPlace::position`] holding a text's index in its field,
/// for an entry whose fields hold `texts`: 16, so a position fits SQLite's
/// 3-byte integer, or 32 when a field holds more than 65,536 texts.
fn field_position_bits(texts: &[Vec<String>]) -> u32 {
    const NARROW: u32 = 16;
    if texts.iter().all(|field| field.len() <= 1 << NARROW) {
        NARROW
    } else {
        u32::BITS
    }
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
    let bits = field_position_bits(&parts);
    let texts = (0_i64..)
        .zip(ENTRY_SEARCH_FIELDS)
        .zip(parts)
        .flat_map(|((rank, field), texts)| {
            (0_i64..)
                .zip(texts)
                .map(move |(index, text)| (field, (rank << bits) | index, text))
        });
    for (field, position, text) in texts {
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

#[cfg(test)]
mod tests {
    use super::field_position_bits;

    #[test]
    fn position_index_widens_to_32_bits_past_65536_texts_in_a_field() {
        let texts = |count: usize| vec![String::new(); count];

        assert_eq!(
            field_position_bits(&[texts(1 << 16), texts(1)]),
            16,
            "a field of 65,536 texts numbers them in 16 bits"
        );
        assert_eq!(
            field_position_bits(&[texts(1), texts((1 << 16) + 1)]),
            32,
            "one field of 65,537 texts widens every field's index to 32 bits"
        );
    }
}
