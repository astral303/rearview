//! Message navigation inside a message's text: the blocks `→` focuses from
//! a message stop, the items it focuses from a list, the stops `J` and `K`
//! step through there, and the text `y` copies.

use std::ops::Range;
use std::sync::Arc;

use super::view_state::NO_TEXT_TO_COPY;
use super::{App, AppMode, TextBlockFocus, ViewState};
use crate::tui::viewer::{BlockRows, TextBlock, text_blocks_in};

/// The focused block or item, and the rows the focus marks.
pub(crate) struct FocusedText {
    pub(crate) block: Arc<TextBlock>,
    pub(crate) item: Option<usize>,
    pub(crate) rows: Range<usize>,
}

impl ViewState {
    /// The text blocks of the message at `message_index`, in row order.
    pub(super) fn text_blocks_of(&self, message_index: usize) -> Vec<BlockRows> {
        match self.message_ranges.get(message_index) {
            Some(message) => text_blocks_in(&self.rendered_lines, message.rows()),
            None => Vec::new(),
        }
    }

    /// `None` while no block is focused, or when the shown rows don't have
    /// the focused block or item.
    pub(crate) fn focused_text(&self) -> Option<FocusedText> {
        let text_block = self.focused_text_block()?;
        let block = self
            .text_blocks_of(self.focused_message()?)
            .into_iter()
            .nth(text_block.block)?;
        let rows = match text_block.item {
            None => block.rows.clone(),
            Some(item) => {
                block
                    .items(&self.rendered_lines)
                    .into_iter()
                    .find(|shown| shown.item == item)?
                    .rows
            }
        };
        Some(FocusedText {
            block: block.block,
            item: text_block.item,
            rows,
        })
    }

    /// The texts the blocks of the message at `message_index` were rendered
    /// from, in row order: equal lists show the message's text unchanged.
    pub(super) fn message_texts(&self, message_index: usize) -> Vec<Arc<str>> {
        let mut texts: Vec<Arc<str>> = Vec::new();
        for shown in self.text_blocks_of(message_index) {
            let text = shown.block.text();
            if !texts.last().is_some_and(|last| Arc::ptr_eq(last, text)) {
                texts.push(Arc::clone(text));
            }
        }
        texts
    }

    /// `text_block` in the message at `message_index` as the shown rows have
    /// it: an item they don't show becomes its list (see [`list_of_item`]),
    /// and a block they don't show `None`.
    pub(super) fn shown_text_block(
        &self,
        message_index: usize,
        text_block: TextBlockFocus,
    ) -> Option<TextBlockFocus> {
        let blocks = self.text_blocks_of(message_index);
        let block = blocks.get(text_block.block)?;
        match text_block.item {
            Some(item)
                if !block
                    .items(&self.rendered_lines)
                    .iter()
                    .any(|shown| shown.item == item) =>
            {
                list_of_item(&blocks, text_block)
            }
            _ => Some(text_block),
        }
    }

    /// `←` from the item `text_block` of the message at `message_index`.
    pub(super) fn text_stop_outward(
        &self,
        message_index: usize,
        text_block: TextBlockFocus,
    ) -> Option<TextBlockFocus> {
        match text_block.item {
            Some(_) => list_of_item(&self.text_blocks_of(message_index), text_block),
            None => None,
        }
    }

    /// The stop `→` focuses in the message at `message_index`: its first
    /// block. In a message of one block, a list's first item, a code block
    /// or a quote; `None` for a lone paragraph, heading or table, because
    /// its copy equals the message's, and for a message without blocks.
    pub(super) fn first_text_stop(&self, message_index: usize) -> Option<TextBlockFocus> {
        let first_block = TextBlockFocus {
            block: 0,
            item: None,
        };
        match self.text_blocks_of(message_index).as_slice() {
            [] => None,
            [only] if only.block.is_list() => self.first_item_stop(message_index, first_block),
            [only] if only.block.copies_as_markdown() => None,
            _ => Some(first_block),
        }
    }

    /// The first item of the list at `text_block`, the stop `→` focuses from
    /// the list; `None` for any other block.
    pub(super) fn first_item_stop(
        &self,
        message_index: usize,
        text_block: TextBlockFocus,
    ) -> Option<TextBlockFocus> {
        let blocks = self.text_blocks_of(message_index);
        let list = blocks
            .get(text_block.block)
            .filter(|shown| shown.block.is_list())?;
        let first = list.items(&self.rendered_lines).first()?.item;
        Some(TextBlockFocus {
            item: Some(first),
            ..text_block
        })
    }

    /// The stop `J` moves to from `text_block` inside the message at
    /// `message_index`: the next item of its list, otherwise the next block.
    /// `None` past the message's last block.
    pub(super) fn text_stop_after(
        &self,
        message_index: usize,
        text_block: TextBlockFocus,
    ) -> Option<TextBlockFocus> {
        let blocks = self.text_blocks_of(message_index);
        if let Some(item) = text_block.item {
            let items = blocks.get(text_block.block)?.items(&self.rendered_lines);
            let next = items
                .iter()
                .position(|shown| shown.item == item)
                .and_then(|position| items.get(position + 1));
            if let Some(next) = next {
                return Some(TextBlockFocus {
                    item: Some(next.item),
                    ..text_block
                });
            }
        }
        let next_block = text_block.block + 1;
        (next_block < blocks.len()).then_some(TextBlockFocus {
            block: next_block,
            item: None,
        })
    }

    /// The stop `K` moves to from `text_block`: the previous item of its
    /// list, otherwise the previous block. `None` before the message's first
    /// block.
    pub(super) fn text_stop_before(
        &self,
        message_index: usize,
        text_block: TextBlockFocus,
    ) -> Option<TextBlockFocus> {
        if let Some(item) = text_block.item {
            let blocks = self.text_blocks_of(message_index);
            let items = blocks.get(text_block.block)?.items(&self.rendered_lines);
            let previous = items
                .iter()
                .position(|shown| shown.item == item)
                .and_then(|position| position.checked_sub(1))
                .map(|position| items[position].item);
            if previous.is_some() {
                return Some(TextBlockFocus {
                    item: previous,
                    ..text_block
                });
            }
        }
        Some(TextBlockFocus {
            block: text_block.block.checked_sub(1)?,
            item: None,
        })
    }
}

/// The list of the focused item `text_block` among a message's `blocks`.
/// `None` when the list is the message's only block: the message is
/// focused instead, because the list's rows are the message's.
pub(super) fn list_of_item(
    blocks: &[BlockRows],
    text_block: TextBlockFocus,
) -> Option<TextBlockFocus> {
    (blocks.len() > 1).then_some(TextBlockFocus {
        item: None,
        ..text_block
    })
}

impl App {
    /// Copy the focused block or item. False when no block is focused.
    pub(super) fn copy_focused_text(&mut self) -> bool {
        let AppMode::View(state) = &self.app_mode else {
            return false;
        };
        let Some(copy) = state
            .focused_text()
            .and_then(|focused| focused.block.copy(focused.item))
        else {
            return false;
        };
        if copy.text.is_empty() {
            self.set_status(NO_TEXT_TO_COPY.to_owned());
        } else {
            self.copy_to_clipboard(copy.name, &copy.text);
        }
        true
    }
}
