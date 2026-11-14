//! One entry's rows rendered again in place, after expanding or collapsing
//! one of its outputs, without rendering the rest of the conversation. The
//! connectors crossing the entry's rows are painted again from the kept
//! calls.

use std::ops::Range;

use crate::log_entry::LogEntry;

use super::calls::CallRanges;
use super::connectors::draw_connectors;
use super::entry::render_entry;
use super::summary::{classify_user_tool_entry, tool_only_assistant_summary};
use super::{
    CallRange, DetailCall, MessageRange, ParsedConversation, RenderOptions, RenderedLine,
    ToolOutputId, message_range_excluding_trailing_blank,
};

/// Free row records a `Vec` of rows gets when an entry rendered again
/// outgrows it: 256 KB of records. The next expands that add rows then
/// fit without copying every row record again.
const SPARE_ROWS_AFTER_GROWTH: usize = 4096;

/// The rows and indexes of a render. An entry rendered again changes them
/// in place.
pub struct ShownRows<'a> {
    pub lines: &'a mut Vec<RenderedLine>,
    pub messages: &'a mut Vec<MessageRange>,
    pub calls: &'a mut Vec<CallRange>,
    pub detail_calls: &'a mut Vec<DetailCall>,
}

/// The rows an entry rendered again replaced: `new_len` rows replaced
/// `old`, starting where `old` started.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReplacedRows {
    pub old: Range<usize>,
    pub new_len: usize,
}

/// The outcome of [`render_entry_in_place`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EntryInPlace {
    /// The entry's new rows replaced its old ones.
    Replaced(ReplacedRows),
    /// Nothing to replace: the entry doesn't render alone with the options,
    /// or doesn't have any rows, in the shown rows as in the new render.
    Unchanged,
    /// The shown rows or the new ones don't have the row extent
    /// [`render_entry_in_place`] relies on; nothing was replaced.
    ExtentBroken,
}

impl ReplacedRows {
    pub fn new_rows(&self) -> Range<usize> {
        self.old.start..self.old.start + self.new_len
    }

    /// The index of a row outside `old` after the replacement: a row at or
    /// after `old`'s end moves by the difference in length.
    pub fn moved(&self, row: usize) -> usize {
        if row >= self.old.end {
            row - self.old.len() + self.new_len
        } else {
            row
        }
    }
}

/// The entry that expanding or collapsing `id` renders again with
/// `options`, when it renders alone: a message of its own, not a call or
/// result a tool run in summary mode collapses. `None` otherwise, and for an
/// entry missing from `conversation`.
pub fn entry_rendering_alone(
    conversation: &ParsedConversation,
    options: &RenderOptions,
    id: &ToolOutputId,
) -> Option<usize> {
    let entry_index = id.entry_index()?;
    renders_alone(conversation.entry(entry_index)?, options).then_some(entry_index)
}

fn renders_alone(entry: &LogEntry, options: &RenderOptions) -> bool {
    let is_message = matches!(entry, LogEntry::User { .. } | LogEntry::Assistant { .. });
    let is_in_run = options.tool_display.is_summary()
        && (tool_only_assistant_summary(entry, options).is_some()
            || classify_user_tool_entry(entry, options, None).is_some());
    is_message && !is_in_run
}

/// Render the entry `entry_index` of `conversation` again with `options`,
/// in place of its rows in `shown`. A render of `conversation` drew
/// `shown` with `options` apart from the expanded set.
///
/// Relies on the row extent of an entry's message: its rows
/// `start_line..end_line` start and end with a non-blank row, and one blank
/// row follows at `end_line`, the entry's last row (see `push_separator`).
/// The entry rendered alone must have it, and in `shown` the message must
/// end before the rows and before the next message do; the blank row isn't
/// checked there, where connectors paint it. When either check fails,
/// `shown` stays unchanged and a debug build panics.
pub fn render_entry_in_place(
    conversation: &ParsedConversation,
    options: &RenderOptions,
    shown: ShownRows<'_>,
    entry_index: usize,
) -> EntryInPlace {
    let Some(entry) = conversation
        .entry(entry_index)
        .filter(|entry| renders_alone(entry, options))
    else {
        return EntryInPlace::Unchanged;
    };
    let mut lines = Vec::new();
    let call_ranges = CallRanges::new(&conversation.lanes);
    let blocks = render_entry(
        &mut lines,
        entry_index,
        entry,
        options,
        &conversation.roster,
        &call_ranges,
    );
    let new_message = message_range_excluding_trailing_blank(&lines, 0, lines.len(), entry_index);
    let shown_message = shown
        .messages
        .binary_search_by_key(&entry_index, |message| message.entry_index)
        .ok();
    let (message_index, new_message) = match (shown_message, new_message) {
        (None, None) => return EntryInPlace::Unchanged,
        (Some(message_index), Some(new_message))
            if has_shown_extent(&shown, message_index)
                && has_entry_extent(&lines, &new_message) =>
        {
            (message_index, new_message)
        }
        _ => {
            debug_assert!(
                false,
                "entry {entry_index} has the row extent of a message in the shown rows and alone"
            );
            return EntryInPlace::ExtentBroken;
        }
    };
    let message = &shown.messages[message_index];
    let old = message.start_line..message.end_line + 1;
    let replaced = ReplacedRows {
        old: old.clone(),
        new_len: lines.len(),
    };
    let first_row = old.start;

    let growth = replaced.new_len.saturating_sub(old.len());
    if shown.lines.capacity() - shown.lines.len() < growth {
        shown.lines.reserve_exact(growth + SPARE_ROWS_AFTER_GROWTH);
    }
    shown.lines.splice(old, lines);

    for message in &mut shown.messages[message_index + 1..] {
        message.start_line = replaced.moved(message.start_line);
        message.end_line = replaced.moved(message.end_line);
    }
    shown.messages[message_index] = MessageRange {
        start_line: first_row + new_message.start_line,
        end_line: first_row + new_message.end_line,
        ..new_message
    };
    for area in shown
        .calls
        .iter_mut()
        .flat_map(|call| std::iter::once(&mut call.input).chain(call.result.as_mut()))
    {
        area.start_line = replaced.moved(area.start_line);
        area.end_line = replaced.moved(area.end_line);
    }

    let block_rows = |location| {
        blocks
            .iter()
            .find(|block| block.area.location == location)
            .map(|block| first_row + block.area.start_line..first_row + block.area.end_line)
    };
    let moved = |rows: &Range<usize>| replaced.moved(rows.start)..replaced.moved(rows.end);
    for call in shown.detail_calls.iter_mut() {
        call.rows.input = block_rows(call.input).unwrap_or_else(|| moved(&call.rows.input));
        if let (Some(location), Some(rows)) = (call.result, call.rows.result.as_mut()) {
            *rows = block_rows(location).unwrap_or_else(|| moved(rows));
        }
    }

    draw_connectors(
        shown.lines,
        shown
            .calls
            .iter()
            .map(CallRange::rows)
            .chain(shown.detail_calls.iter().map(|call| call.rows.clone())),
        options.show_timing,
        replaced.new_rows(),
    );
    EntryInPlace::Replaced(replaced)
}

/// True when `message`, the message of an entry rendered alone into
/// `lines`, has the row extent [`render_entry_in_place`] relies on.
fn has_entry_extent(lines: &[RenderedLine], message: &MessageRange) -> bool {
    let is_blank = |row: usize| lines.get(row).map(|line| line.spans.is_empty());
    message.start_line < message.end_line
        && message.end_line + 1 == lines.len()
        && is_blank(message.start_line) == Some(false)
        && is_blank(message.end_line - 1) == Some(false)
        && is_blank(message.end_line) == Some(true)
}

/// True when the message `message_index` of `shown` ends before the rows
/// and before the next message do, leaving its entry's blank row to it.
fn has_shown_extent(shown: &ShownRows<'_>, message_index: usize) -> bool {
    let end_line = shown.messages[message_index].end_line;
    end_line < shown.lines.len()
        && shown
            .messages
            .get(message_index + 1)
            .is_none_or(|next| next.start_line > end_line)
}
