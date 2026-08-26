//! Each tool call's rows paired with the rows of the result answering it,
//! and each interleaved call's position in its batch.

use std::collections::{HashMap, HashSet};

use crate::log_entry::{ContentBlock, LogEntry, Tool, UserContent};

use super::tools::ToolOutputKind;
use super::{CallArea, CallRange, RenderableEntry};

/// One tool block of an entry: the entry holding it, the block's index among
/// that entry's blocks, and the block's fields.
pub(super) struct EntryToolBlock<'a> {
    pub(super) parsed: &'a RenderableEntry,
    pub(super) block_index: usize,
    pub(super) block: ToolBlock<'a>,
}

pub(super) enum ToolBlock<'a> {
    Call {
        id: &'a str,
        name: &'a str,
        tool: Tool,
        input: &'a serde_json::Value,
    },
    Result {
        tool_use_id: &'a str,
        content: Option<&'a serde_json::Value>,
    },
}

impl<'a> ToolBlock<'a> {
    fn of(block: &'a ContentBlock) -> Option<Self> {
        match block {
            ContentBlock::ToolUse {
                id,
                name,
                tool,
                input,
            } => Some(Self::Call {
                id,
                name,
                tool: *tool,
                input,
            }),
            ContentBlock::ToolResult {
                tool_use_id,
                content,
            } => Some(Self::Result {
                tool_use_id,
                content: content.as_ref(),
            }),
            _ => None,
        }
    }
}

/// The tool calls and results of `entries` in file order: the blocks of
/// every entry whose parent is `parent_id`, other blocks skipped.
pub(super) fn entry_tool_blocks<'a>(
    entries: &'a [RenderableEntry],
    parent_id: Option<&'a str>,
) -> impl Iterator<Item = EntryToolBlock<'a>> + 'a {
    entries.iter().flat_map(move |parsed| {
        let blocks: &[ContentBlock] = match &parsed.entry {
            LogEntry::Assistant {
                message,
                parent_tool_use_id,
                ..
            } if parent_tool_use_id.as_deref() == parent_id => &message.content,
            LogEntry::User {
                message,
                parent_tool_use_id,
                ..
            } if parent_tool_use_id.as_deref() == parent_id => match &message.content {
                UserContent::Blocks(blocks) => blocks,
                UserContent::String(_) => &[],
            },
            _ => &[],
        };
        blocks
            .iter()
            .enumerate()
            .filter_map(move |(block_index, block)| {
                Some(EntryToolBlock {
                    parsed,
                    block_index,
                    block: ToolBlock::of(block)?,
                })
            })
    })
}

/// The tool calls and results of the conversation's top-level entries, in
/// file order.
pub(super) fn top_level_tool_blocks(
    entries: &[RenderableEntry],
) -> impl Iterator<Item = ToolBlock<'_>> {
    entry_tool_blocks(entries, None).map(|entry_block| entry_block.block)
}

/// A tool call or result and the rows it rendered to.
pub(super) struct RenderedToolBlock<'a> {
    pub(super) kind: ToolOutputKind,
    pub(super) tool_use_id: &'a str,
    pub(super) area: CallArea,
}

impl RenderedToolBlock<'_> {
    /// The same block `first_row` rows down: an entry's rows become the
    /// conversation's.
    pub(super) fn offset_by(mut self, first_row: usize) -> Self {
        self.area.start_line += first_row;
        self.area.end_line += first_row;
        self
    }
}

/// The `CallRange`s of one sequence of tool blocks — an expanded run, or
/// every top-level block of the conversation in the detail modes — built as
/// the blocks render. A call opens a range; the result carrying its
/// `tool_use_id` closes it.
#[derive(Default)]
pub(super) struct CallRanges<'a> {
    batch_positions: HashMap<&'a str, usize>,
    call_by_tool_use_id: HashMap<&'a str, usize>,
    calls: Vec<CallRange>,
}

impl<'a> CallRanges<'a> {
    pub(super) fn new(blocks: impl Iterator<Item = ToolBlock<'a>>) -> Self {
        let blocks: Vec<_> = blocks.collect();
        Self {
            batch_positions: batch_positions(&blocks),
            ..Default::default()
        }
    }

    /// `None` for a call issued alone, or never answered.
    pub(super) fn batch_position(&self, tool_use_id: &str) -> Option<usize> {
        self.batch_positions.get(tool_use_id).copied()
    }

    pub(super) fn record(&mut self, block: RenderedToolBlock<'a>) {
        let RenderedToolBlock {
            kind,
            tool_use_id,
            area,
        } = block;
        match kind {
            ToolOutputKind::ToolCall => {
                self.call_by_tool_use_id
                    .insert(tool_use_id, self.calls.len());
                self.calls.push(CallRange {
                    input: area,
                    result: None,
                    batch_position: self.batch_position(tool_use_id),
                });
            }
            ToolOutputKind::ToolResult => {
                if let Some(&call) = self.call_by_tool_use_id.get(tool_use_id) {
                    self.calls[call].result = Some(area);
                }
            }
        }
    }

    pub(super) fn into_calls(self) -> Vec<CallRange> {
        self.calls
    }
}

/// Each call's position in its batch of interleaved calls, by
/// `tool_use_id`. A batch is two or more answered calls, across entries,
/// with no result between them; a call issued alone, or never answered, is
/// absent.
fn batch_positions<'a>(blocks: &[ToolBlock<'a>]) -> HashMap<&'a str, usize> {
    let answered: HashSet<&str> = blocks
        .iter()
        .filter_map(|block| match block {
            ToolBlock::Result { tool_use_id, .. } => Some(*tool_use_id),
            ToolBlock::Call { .. } => None,
        })
        .collect();
    let mut positions = HashMap::new();
    let mut open: Vec<&'a str> = Vec::new();
    for block in blocks {
        match block {
            ToolBlock::Call { id, .. } if answered.contains(id) => open.push(id),
            ToolBlock::Call { .. } => {}
            ToolBlock::Result { .. } => close_batch(&mut open, &mut positions),
        }
    }
    close_batch(&mut open, &mut positions);
    positions
}

fn close_batch<'a>(open: &mut Vec<&'a str>, positions: &mut HashMap<&'a str, usize>) {
    if open.len() >= 2 {
        positions.extend(
            open.iter()
                .enumerate()
                .map(|(position, &id)| (id, position)),
        );
    }
    open.clear();
}
