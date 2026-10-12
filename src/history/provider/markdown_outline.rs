//! A Markdown document's headings, top-level paragraphs, tables and top-level
//! list items, as plain text, for the tests that check documentation against
//! the registry.

use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag, TagEnd};

pub(super) enum Block {
    Heading { level: HeadingLevel, text: String },
    Paragraph(String),
    Table(Table),
    Item(ListItem),
}

#[derive(Debug, Default)]
pub(super) struct Table {
    pub(super) header: Vec<String>,
    pub(super) rows: Vec<Vec<String>>,
}

/// One top-level list item: its leading bold text, if any, and the text after
/// it, without its sub-items.
#[derive(Debug)]
pub(super) struct ListItem {
    pub(super) bold_lead: Option<String>,
    pub(super) text: String,
}

/// The blocks of `markdown`, in order. Links keep their Markdown, so a linked
/// version is not read as a bare one. A nested list item is detail under its
/// parent and adds no text.
pub(super) fn outline(markdown: &str) -> Vec<Block> {
    let mut blocks = Vec::new();
    let mut text = String::new();
    let mut table = Table::default();
    let mut row = Vec::new();
    let mut link_destinations = Vec::new();
    let mut item_bold_lead = None;
    let mut in_bold_lead = false;
    let mut item_depth = 0usize;
    for event in Parser::new_ext(markdown, Options::ENABLE_TABLES) {
        let in_nested_item = item_depth > 1;
        match event {
            Event::Start(Tag::Item) => {
                item_depth += 1;
                if item_depth == 1 {
                    text.clear();
                    item_bold_lead = None;
                }
            }
            Event::End(TagEnd::Item) => {
                if item_depth == 1 {
                    blocks.push(Block::Item(ListItem {
                        bold_lead: item_bold_lead.take(),
                        text: std::mem::take(&mut text),
                    }));
                }
                item_depth -= 1;
            }
            _ if in_nested_item => {}
            Event::Start(Tag::Paragraph) if item_depth == 0 => text.clear(),
            Event::End(TagEnd::Paragraph) if item_depth == 0 => {
                blocks.push(Block::Paragraph(
                    std::mem::take(&mut text).trim().to_owned(),
                ));
            }
            Event::Start(Tag::Heading { .. } | Tag::TableCell) => text.clear(),
            Event::Start(Tag::Strong)
                if item_depth == 1 && item_bold_lead.is_none() && text.trim().is_empty() =>
            {
                text.clear();
                in_bold_lead = true;
            }
            Event::End(TagEnd::Strong) if in_bold_lead => {
                item_bold_lead = Some(std::mem::take(&mut text));
                in_bold_lead = false;
            }
            Event::Start(Tag::Link { dest_url, .. }) => {
                text.push('[');
                link_destinations.push(dest_url);
            }
            Event::End(TagEnd::Link) => {
                if let Some(destination) = link_destinations.pop() {
                    text.push_str("](");
                    text.push_str(&destination);
                    text.push(')');
                }
            }
            Event::Text(fragment) | Event::Code(fragment) => text.push_str(&fragment),
            Event::SoftBreak | Event::HardBreak => text.push(' '),
            Event::End(TagEnd::Heading(level)) => blocks.push(Block::Heading {
                level,
                text: std::mem::take(&mut text).trim().to_owned(),
            }),
            Event::End(TagEnd::TableCell) => row.push(std::mem::take(&mut text).trim().to_owned()),
            Event::End(TagEnd::TableHead) => table.header = std::mem::take(&mut row),
            Event::End(TagEnd::TableRow) => table.rows.push(std::mem::take(&mut row)),
            Event::End(TagEnd::Table) => blocks.push(Block::Table(std::mem::take(&mut table))),
            _ => {}
        }
    }
    blocks
}
