//! The search field of each search text of an entry, decided as the entry is
//! written: a block whose stored text search reads as written carries the
//! field, and every other search text is stored once beside the entry. The
//! texts come from the parser, so the stored ones match what search read
//! when it built them from the entry.

use super::parts;
use crate::agent::sanitize::sanitize_agent_text;
use crate::agent::transcript::tool_result_item_text;
use crate::history::parser::{normalize_full_text, search_parts_of_entry};
use crate::history::sqlite_cache::SearchField;
use crate::log_entry::{ContentBlock, LogEntry};
use serde_json::Value;
use std::borrow::Cow;

/// One entry's search texts.
pub(super) struct EntrySearch {
    /// For each of the entry's blocks, the field search reads its stored
    /// text in.
    pub(super) blocks: Vec<Option<SearchField>>,
    /// The texts no block holds as written, each with its field.
    pub(super) derived: Vec<DerivedText>,
}

pub(super) struct DerivedText {
    pub(super) field: SearchField,
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
    let mut block_texts: Vec<Option<Option<String>>> = vec![None; blocks.len()];
    let fields = [SearchField::Full, SearchField::Agent, SearchField::Thinking];
    for (field, texts) in fields
        .into_iter()
        .zip(search_parts_of_entry(entry, spliced))
    {
        for text in texts {
            let searched = normalize_full_text(&text);
            if searched.is_empty() {
                continue;
            }
            let holder = (0..blocks.len()).find(|&index| {
                search.blocks[index].is_none()
                    && block_texts[index]
                        .get_or_insert_with(|| searched_block_text(&blocks[index]))
                        .as_deref()
                        == Some(searched.as_str())
            });
            match holder {
                Some(index) => search.blocks[index] = Some(field),
                None => search.derived.push(DerivedText { field, text }),
            }
        }
    }
    search
}

/// The text search reads from `block`'s stored text, without terminal codes
/// and with its whitespace collapsed: a text, a thinking block, a result's
/// string, or the text items of a result whose items the row stores, joined
/// by line breaks. `None` for a block without stored text.
fn searched_block_text(block: &ContentBlock) -> Option<String> {
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
        } if parts::stores_items(items) => Cow::Owned(
            items
                .iter()
                .filter_map(tool_result_item_text)
                .map(sanitize_agent_text)
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        _ => return None,
    };
    Some(normalize_full_text(&stored))
}
