//! The top-level Markdown blocks of a message's text, as the rows they laid
//! out carry them: message navigation steps between them and copies them
//! without parsing the Markdown again.

use std::ops::Range;
use std::sync::Arc;

use crate::markdown::block_content::{list_item_content, quote_content};
use crate::markdown::layout::{BlockKind, LayoutDoc};

use super::RenderedLine;

/// One top-level block of a message's Markdown text.
#[derive(Clone, PartialEq)]
pub struct TextBlock {
    /// The whole text the block was rendered from, shared by its blocks.
    text: Arc<str>,
    /// Bytes of `text`.
    source: Range<usize>,
    kind: BlockKind,
    /// Bytes of `text`, one range per item of a list.
    items: Vec<Range<usize>>,
}

/// Leaves out the text: every block of a message holds the same one.
impl std::fmt::Debug for TextBlock {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TextBlock")
            .field("source", &self.source)
            .field("kind", &self.kind)
            .field("items", &self.items)
            .finish_non_exhaustive()
    }
}

/// The text block a row shows, and the list item when the block is a list.
#[derive(Clone, Debug, PartialEq)]
pub struct TextBlockRow {
    pub block: Arc<TextBlock>,
    pub item: Option<usize>,
}

/// A copy of a block or of a list item: the text the clipboard gets, and
/// the name the status line gives it.
#[derive(Debug, PartialEq)]
pub struct BlockCopy {
    pub name: &'static str,
    pub text: String,
}

impl TextBlock {
    /// The whole text the block was rendered from.
    pub fn text(&self) -> &Arc<str> {
        &self.text
    }

    pub fn is_list(&self) -> bool {
        self.kind == BlockKind::List
    }

    /// True when the block copies as its Markdown: a paragraph, heading,
    /// list or table. A code block copies without its fences, and a quote
    /// without its `>` marks.
    pub fn copies_as_markdown(&self) -> bool {
        !matches!(self.kind, BlockKind::CodeBlock { .. } | BlockKind::Quote)
    }

    /// The copy of the block, or of its item `item` without its bullet or
    /// number. `None` when the block doesn't have item `item`.
    pub fn copy(&self, item: Option<usize>) -> Option<BlockCopy> {
        let Some(item) = item else {
            return Some(self.copy_of_block());
        };
        let source = self.items.get(item)?;
        let line_start = self.text[..source.start]
            .rfind('\n')
            .map_or(0, |newline| newline + 1);
        Some(BlockCopy {
            name: "List item",
            text: list_item_content(self.text[line_start..source.end].trim_end()),
        })
    }

    fn copy_of_block(&self) -> BlockCopy {
        let markdown = self.text[self.source.clone()].trim_end();
        let (name, text) = match &self.kind {
            BlockKind::Paragraph => ("Paragraph", markdown.to_owned()),
            BlockKind::Heading => ("Heading", markdown.to_owned()),
            BlockKind::CodeBlock { code } => (
                "Code block",
                code.strip_suffix('\n').unwrap_or(code).to_owned(),
            ),
            BlockKind::Quote => ("Quote", quote_content(markdown)),
            BlockKind::List => ("List", markdown.to_owned()),
            BlockKind::Table => ("Table", markdown.to_owned()),
        };
        BlockCopy { name, text }
    }
}

/// The block and item each line of `doc` shows, `None` for a line between
/// blocks; `doc` is a render of `text` that recorded its blocks.
pub(super) fn text_block_rows(text: &str, doc: &LayoutDoc) -> Vec<Option<TextBlockRow>> {
    let mut rows = vec![None; doc.lines.len()];
    if doc.blocks.is_empty() {
        return rows;
    }
    let text: Arc<str> = Arc::from(text);
    for laid_out in &doc.blocks {
        let block = Arc::new(TextBlock {
            text: Arc::clone(&text),
            source: laid_out.source.clone(),
            kind: laid_out.kind.clone(),
            items: laid_out
                .items
                .iter()
                .map(|item| item.source.clone())
                .collect(),
        });
        let mut items = laid_out.items.iter().enumerate().peekable();
        for line in laid_out.lines.clone() {
            while items.next_if(|(_, item)| item.lines.end <= line).is_some() {}
            let item = items
                .peek()
                .filter(|(_, item)| item.lines.contains(&line))
                .map(|&(index, _)| index);
            rows[line] = Some(TextBlockRow {
                block: Arc::clone(&block),
                item,
            });
        }
    }
    rows
}

/// A text block's rows among a message's rows.
#[derive(Clone, Debug)]
pub struct BlockRows {
    pub block: Arc<TextBlock>,
    pub rows: Range<usize>,
}

/// A list item's rows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ItemRows {
    pub item: usize,
    pub rows: Range<usize>,
}

/// The text blocks among `lines[rows]`, in row order. A block's rows are
/// the consecutive rows that show it.
pub fn text_blocks_in(lines: &[RenderedLine], rows: Range<usize>) -> Vec<BlockRows> {
    let mut blocks: Vec<BlockRows> = Vec::new();
    for row in rows {
        let Some(text_block_row) = lines.get(row).and_then(|line| line.text_block_row.as_ref())
        else {
            continue;
        };
        match blocks.last_mut() {
            Some(last)
                if last.rows.end == row && Arc::ptr_eq(&last.block, &text_block_row.block) =>
            {
                last.rows.end = row + 1;
            }
            _ => blocks.push(BlockRows {
                block: Arc::clone(&text_block_row.block),
                rows: row..row + 1,
            }),
        }
    }
    blocks
}

impl BlockRows {
    /// The items of a list block that have rows, in row order.
    pub fn items(&self, lines: &[RenderedLine]) -> Vec<ItemRows> {
        let mut items: Vec<ItemRows> = Vec::new();
        for row in self.rows.clone() {
            let Some(item) = lines[row]
                .text_block_row
                .as_ref()
                .and_then(|text_block_row| text_block_row.item)
            else {
                continue;
            };
            match items.last_mut() {
                Some(last) if last.item == item => last.rows.end = row + 1,
                _ => items.push(ItemRows {
                    item,
                    rows: row..row + 1,
                }),
            }
        }
        items
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::markdown::layout::LayoutEngine;

    /// The blocks of `markdown`, in order.
    fn blocks_of(markdown: &str) -> Vec<Arc<TextBlock>> {
        let doc = LayoutEngine::render_with_blocks(markdown, 80);
        let mut blocks: Vec<Arc<TextBlock>> = Vec::new();
        for text_block_row in text_block_rows(markdown, &doc).into_iter().flatten() {
            if !blocks
                .iter()
                .any(|block| Arc::ptr_eq(block, &text_block_row.block))
            {
                blocks.push(text_block_row.block);
            }
        }
        blocks
    }

    /// The name the status line gives a copy of `markdown`'s block `index`,
    /// or of its item `item`, and the text it copies.
    fn copy_of(markdown: &str, index: usize, item: Option<usize>) -> (&'static str, String) {
        let copy = blocks_of(markdown)[index]
            .copy(item)
            .expect("the block has the item");
        (copy.name, copy.text)
    }

    #[test]
    fn code_block_copies_its_code_without_fences_or_indent() {
        assert_eq!(
            copy_of("Run:\n\n```sh\ncargo test\nmise run check\n```\n", 1, None),
            ("Code block", "cargo test\nmise run check".to_owned())
        );
        assert_eq!(
            copy_of("Run:\n\n    cargo test\n      indented\n", 1, None),
            ("Code block", "cargo test\n  indented".to_owned())
        );
    }

    #[test]
    fn quote_copies_its_markdown_without_quote_marks() {
        assert_eq!(
            copy_of("Note:\n\n> one\n> two\n", 1, None),
            ("Quote", "one\ntwo".to_owned())
        );
    }

    #[test]
    fn paragraph_heading_list_and_table_copy_their_markdown() {
        let markdown = "## Steps\n\nRun **this**\nthen that.\n\n- one\n- two\n\n| a | b |\n|---|---|\n| 1 | 2 |\n";
        assert_eq!(
            copy_of(markdown, 0, None),
            ("Heading", "## Steps".to_owned())
        );
        assert_eq!(
            copy_of(markdown, 1, None),
            ("Paragraph", "Run **this**\nthen that.".to_owned())
        );
        assert_eq!(
            copy_of(markdown, 2, None),
            ("List", "- one\n- two".to_owned())
        );
        assert_eq!(
            copy_of(markdown, 3, None),
            ("Table", "| a | b |\n|---|---|\n| 1 | 2 |".to_owned())
        );
    }

    #[test]
    fn list_item_copies_its_markdown_without_bullet_and_unindented() {
        let markdown =
            "1. Build:\n\n   ```sh\n   cargo build\n   ```\n2. Check\n   - fmt\n   - clippy\n";
        assert_eq!(
            copy_of(markdown, 0, Some(1)),
            ("List item", "Check\n- fmt\n- clippy".to_owned())
        );
        assert_eq!(
            copy_of("  - indented\n    bullet\n", 0, Some(0)),
            ("List item", "indented\nbullet".to_owned()),
            "the indent before the bullet counts towards the content's indent"
        );
    }

    #[test]
    fn missing_item_doesnt_copy() {
        assert_eq!(blocks_of("- one\n- two\n")[0].copy(Some(2)), None);
    }

    #[test]
    fn rows_between_blocks_and_of_a_thematic_break_dont_carry_block() {
        let markdown = "First.\n\n---\n\nSecond.";
        let doc = LayoutEngine::render_with_blocks(markdown, 80);
        let in_block: Vec<bool> = text_block_rows(markdown, &doc)
            .iter()
            .map(Option::is_some)
            .collect();
        assert_eq!(in_block, [true, false, false, false, true]);
    }
}
