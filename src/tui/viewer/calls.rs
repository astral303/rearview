//! Each tool call's rows paired with the rows of the result answering it,
//! and the lane of each call open beside another.

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
        standalone_tool_name: Option<&'a str>,
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
                standalone_tool_name,
            } => Some(Self::Result {
                tool_use_id,
                content: content.as_ref(),
                standalone_tool_name: standalone_tool_name.as_deref(),
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
    lanes: HashMap<&'a str, usize>,
    call_by_tool_use_id: HashMap<&'a str, usize>,
    calls: Vec<CallRange>,
}

impl<'a> CallRanges<'a> {
    pub(super) fn new(blocks: impl Iterator<Item = ToolBlock<'a>>) -> Self {
        let blocks: Vec<_> = blocks.collect();
        Self {
            lanes: lanes(&blocks),
            ..Default::default()
        }
    }

    /// `None` for a call open alone, or never answered.
    pub(super) fn lane(&self, tool_use_id: &str) -> Option<usize> {
        self.lanes.get(tool_use_id).copied()
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
                    lane: self.lane(tool_use_id),
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

/// Each call's lane, counted from the first lane cell, by `tool_use_id`. A
/// call is open from its call block to its result; a call takes the first
/// lane right of every open one, so its connector crosses none of theirs,
/// and its result frees the lane. A call open alone, or never answered, is
/// absent.
fn lanes<'a>(blocks: &[ToolBlock<'a>]) -> HashMap<&'a str, usize> {
    let answered: HashSet<&str> = blocks
        .iter()
        .filter_map(|block| match block {
            ToolBlock::Result { tool_use_id, .. } => Some(*tool_use_id),
            ToolBlock::Call { .. } => None,
        })
        .collect();
    let mut lanes = HashMap::new();
    let mut open: Vec<OpenCall<'a>> = Vec::new();
    for block in blocks {
        match block {
            ToolBlock::Call { id, .. } if answered.contains(id) => open_beside(&mut open, id),
            ToolBlock::Call { .. } => {}
            ToolBlock::Result { tool_use_id, .. } => {
                lanes.extend(
                    free(&mut open, tool_use_id).and_then(OpenCall::lane_if_beside_another),
                );
            }
        }
    }
    lanes
}

/// Opens `id` in the first lane right of every open call; each of them, and
/// `id`, is now open beside another.
fn open_beside<'a>(open: &mut Vec<OpenCall<'a>>, id: &'a str) {
    let lane = open.iter().map(|call| call.lane + 1).max().unwrap_or(0);
    let beside_another = !open.is_empty();
    for call in open.iter_mut() {
        call.beside_another = true;
    }
    open.push(OpenCall {
        id,
        lane,
        beside_another,
    });
}

/// Removes the call `tool_use_id` answers from `open`; `None` for a result
/// answering no open call.
fn free<'a>(open: &mut Vec<OpenCall<'a>>, tool_use_id: &str) -> Option<OpenCall<'a>> {
    let index = open.iter().position(|call| call.id == tool_use_id)?;
    Some(open.remove(index))
}

/// A call awaiting its result, and whether another call has been open at
/// the same time.
struct OpenCall<'a> {
    id: &'a str,
    lane: usize,
    beside_another: bool,
}

impl<'a> OpenCall<'a> {
    /// The call's lane, keyed by its id, for a call open beside another;
    /// `None` for a call open alone.
    fn lane_if_beside_another(self) -> Option<(&'a str, usize)> {
        self.beside_another.then_some((self.id, self.lane))
    }
}
