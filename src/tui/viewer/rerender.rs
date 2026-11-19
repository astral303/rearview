//! One message's rows rendered again in place, after expanding or
//! collapsing one of its outputs, without rendering the rest of the
//! conversation: an entry's rows, or in summary mode a tool run's. The
//! connectors crossing the message's rows are painted again from the kept
//! calls.

use std::ops::Range;

use crate::log_entry::LogEntry;

use super::calls::{CallRanges, RenderedToolBlock};
use super::connectors::draw_connectors;
use super::entry::render_entry;
use super::summary::{classify_user_tool_entry, tool_only_assistant_summary};
use super::{
    CallRange, DetailCall, MessageRange, ParsedConversation, RenderOptions, RenderStepper,
    RenderedLine, ReplayedRun, ToolOutputId, message_range_excluding_trailing_blank,
};

/// Free row records a `Vec` of rows gets when a message rendered again, or
/// a tail, outgrows it: 256 KB of records. The next expands that add rows then
/// fit without copying every row record again.
pub(super) const SPARE_ROWS_AFTER_GROWTH: usize = 4096;

/// The rows and indexes of a render. A message rendered again changes them
/// in place.
pub struct ShownRows<'a> {
    pub lines: &'a mut Vec<RenderedLine>,
    pub messages: &'a mut Vec<MessageRange>,
    pub calls: &'a mut Vec<CallRange>,
    pub detail_calls: &'a mut Vec<DetailCall>,
}

/// The rows a message rendered again replaced: `new_len` rows replaced
/// `old`, starting where `old` started.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReplacedRows {
    pub old: Range<usize>,
    pub new_len: usize,
}

/// The outcome of [`render_message_in_place`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MessageInPlace {
    /// The message's new rows replaced its old ones.
    Replaced(ReplacedRows),
    /// Nothing to replace: the entry doesn't render alone with the options
    /// and doesn't open a tool run in summary mode, or its message doesn't
    /// have any rows, in the shown rows as in the new render.
    Unchanged,
    /// The shown rows or the new ones don't have the row extent
    /// [`render_message_in_place`] relies on; nothing was replaced.
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

/// The entry of the message whose rows in `lines` show `id`, among the
/// messages that start at or before the id's entry: in summary mode, the
/// first entry of the tool run whose summary row, or a call or result
/// inside it, shows `id`. A result that arrives after its run shows in a run
/// that starts before other messages. `None` when none of them shows it.
pub fn message_showing(
    lines: &[RenderedLine],
    messages: &[MessageRange],
    id: &ToolOutputId,
) -> Option<usize> {
    let entry_index = id.entry_index()?;
    let up_to_entry = messages.partition_point(|message| message.entry_index <= entry_index);
    messages[..up_to_entry]
        .iter()
        .rev()
        .find(|message| {
            lines.get(message.rows()).is_some_and(|rows| {
                rows.iter()
                    .any(|line| line.tool_output_id.as_ref() == Some(id))
            })
        })
        .map(|message| message.entry_index)
}

/// Render the message of the entry `entry_index` of `conversation` again
/// with `options`, in place of its rows in `shown`: the entry's rows when
/// it renders alone, or in summary mode the rows of the tool run it opens.
/// A render of `conversation` drew `shown` with `options` apart from the
/// expanded set.
///
/// Relies on the row extent of a message, an entry's or a tool run's: its
/// rows `start_line..end_line` start and end with a non-blank row, and one
/// blank row follows at `end_line`, the message's last row (see
/// `push_separator`). The message rendered alone must have it, and in
/// `shown` the message must end before the rows and before the next
/// message do. The blank row isn't checked in `shown`, where connectors
/// paint it: after a run whose last block has no rows, it carries the `↓`
/// of the connector to that block. When either check fails, `shown` stays
/// unchanged and a debug build panics.
pub fn render_message_in_place(
    conversation: &ParsedConversation,
    options: &RenderOptions,
    shown: ShownRows<'_>,
    entry_index: usize,
) -> MessageInPlace {
    let Some(entry) = conversation.entry(entry_index) else {
        return MessageInPlace::Unchanged;
    };
    let new_message = if renders_alone(entry, options) {
        render_entry_alone(conversation, options, entry_index, entry)
    } else if options.tool_display.is_summary() {
        render_run_alone(conversation, options, entry_index)
    } else {
        return MessageInPlace::Unchanged;
    };
    let shown_message = shown
        .messages
        .binary_search_by_key(&entry_index, |message| message.entry_index)
        .ok();
    let (message_index, new_message) = match (shown_message, new_message) {
        (None, None) => return MessageInPlace::Unchanged,
        (Some(message_index), Some(new_message))
            if has_shown_extent(&shown, message_index)
                && has_message_extent(&new_message.lines, &new_message.range) =>
        {
            (message_index, new_message)
        }
        _ => {
            debug_assert!(
                false,
                "entry {entry_index}'s message has the row extent of a message in the shown rows and alone"
            );
            return MessageInPlace::ExtentBroken;
        }
    };
    replace_message(shown, message_index, new_message, options.show_timing)
}

/// One message's rows rendered alone, its range, calls and tool blocks
/// counted from its first row.
struct RenderedMessage<'a> {
    lines: Vec<RenderedLine>,
    range: MessageRange,
    /// The calls of an expanded tool run.
    calls: Vec<CallRange>,
    /// An entry's tool calls and results, by which the detail modes' kept
    /// calls find their rows.
    tool_blocks: Vec<RenderedToolBlock<'a>>,
}

fn render_entry_alone<'a>(
    conversation: &ParsedConversation,
    options: &RenderOptions,
    entry_index: usize,
    entry: &'a LogEntry,
) -> Option<RenderedMessage<'a>> {
    let mut lines = Vec::new();
    let call_ranges = CallRanges::new(&conversation.lanes);
    let tool_blocks = render_entry(
        &mut lines,
        entry_index,
        entry,
        options,
        &conversation.links,
        &call_ranges,
    );
    let range = message_range_excluding_trailing_blank(&lines, 0, lines.len(), entry_index)?;
    Some(RenderedMessage {
        lines,
        range,
        calls: Vec::new(),
        tool_blocks,
    })
}

/// The rows of the tool run that the entry `entry_index` opens, as a whole
/// render gives them: stepped from that entry without a run open before it,
/// through the entries after it that don't render any rows, until an entry
/// closes the run. The closing entry's rows are dropped.
fn render_run_alone<'a>(
    conversation: &ParsedConversation,
    options: &RenderOptions,
    entry_index: usize,
) -> Option<RenderedMessage<'a>> {
    let entries = &conversation.entries;
    let first = entries
        .binary_search_by_key(&entry_index, |parsed| parsed.entry_index)
        .ok()?;
    let mut stepper = RenderStepper::replaying_run_from(
        entries,
        &conversation.links,
        &conversation.lanes,
        options,
        first,
    );
    stepper.step(first);
    if !stepper.is_run_open_from(first) {
        return None;
    }
    for parsed_idx in first + 1..entries.len() {
        stepper.step(parsed_idx);
        if !stepper.is_run_open_from(first) {
            break;
        }
    }
    let ReplayedRun {
        lines,
        range,
        calls,
    } = stepper.finish_run(first)?;
    Some(RenderedMessage {
        lines,
        range,
        calls,
        tool_blocks: Vec::new(),
    })
}

/// Replace the rows of the message `message_index` in `shown`, and the
/// blank row after them, with `message`'s, and move every later row index
/// by the change in rows. The calls of an expanded run in the old rows are
/// replaced by the message's. The connectors are painted again over the new
/// rows. Both messages have the row extent [`render_message_in_place`]
/// relies on.
fn replace_message(
    shown: ShownRows<'_>,
    message_index: usize,
    message: RenderedMessage<'_>,
    show_timing: bool,
) -> MessageInPlace {
    let old_message = &shown.messages[message_index];
    // The message's rows and the blank row after them.
    let old = old_message.start_line..old_message.end_line + 1;
    let RenderedMessage {
        lines,
        range,
        calls,
        tool_blocks,
    } = message;
    let replaced = ReplacedRows {
        old: old.clone(),
        new_len: lines.len(),
    };
    let first_row = old.start;

    let growth = replaced.new_len.saturating_sub(old.len());
    if shown.lines.capacity() - shown.lines.len() < growth {
        shown.lines.reserve_exact(growth + SPARE_ROWS_AFTER_GROWTH);
    }
    shown.lines.splice(old.clone(), lines);

    for message in &mut shown.messages[message_index + 1..] {
        message.start_line = replaced.moved(message.start_line);
        message.end_line = replaced.moved(message.end_line);
    }
    shown.messages[message_index] = MessageRange {
        start_line: first_row + range.start_line,
        end_line: first_row + range.end_line,
        ..range
    };

    let calls_in_old_rows = shown
        .calls
        .partition_point(|call| call.input.start_line < old.start)
        ..shown
            .calls
            .partition_point(|call| call.input.start_line < old.end);
    for call in &mut shown.calls[calls_in_old_rows.end..] {
        move_call(call, |row| replaced.moved(row));
    }
    shown.calls.splice(
        calls_in_old_rows,
        calls.into_iter().map(|mut call| {
            move_call(&mut call, |row| first_row + row);
            call
        }),
    );

    let block_rows = |location| {
        tool_blocks
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
        show_timing,
        replaced.new_rows(),
    );
    MessageInPlace::Replaced(replaced)
}

fn move_call(call: &mut CallRange, moved: impl Fn(usize) -> usize) {
    for area in std::iter::once(&mut call.input).chain(call.result.as_mut()) {
        area.start_line = moved(area.start_line);
        area.end_line = moved(area.end_line);
    }
}

/// True when `message`, the message of an entry or a tool run rendered
/// alone into `lines`, has the row extent [`render_message_in_place`]
/// relies on.
fn has_message_extent(lines: &[RenderedLine], message: &MessageRange) -> bool {
    let is_blank = |row: usize| lines.get(row).map(|line| line.spans.is_empty());
    message.start_line < message.end_line
        && message.end_line + 1 == lines.len()
        && is_blank(message.start_line) == Some(false)
        && is_blank(message.end_line - 1) == Some(false)
        && is_blank(message.end_line) == Some(true)
}

/// True when the message `message_index` of `shown` ends before the rows
/// and before the next message do, leaving its message's blank row to it.
fn has_shown_extent(shown: &ShownRows<'_>, message_index: usize) -> bool {
    let end_line = shown.messages[message_index].end_line;
    end_line < shown.lines.len()
        && shown
            .messages
            .get(message_index + 1)
            .is_none_or(|next| next.start_line > end_line)
}
