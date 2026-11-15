//! A render of a conversation's tail after a refresh of the open session.
//! The tail starts at the first entry of a message the shown rows contain
//! before the first changed entry, early enough that no row before it
//! changes. Its rows replace the shown rows from the start on.

use std::collections::HashSet;

use crate::history::sqlite_cache::EntryHashes;
use crate::log_entry::{ContentBlock, LogEntry};

use super::days::DayLabels;
use super::rerender::{ReplacedRows, SPARE_ROWS_AFTER_GROWTH, ShownRows};
use super::roster::{RosterChanges, SubagentRoster};
use super::{
    CallRange, MessageRange, ParsedConversation, RenderOptions, RenderableEntry,
    RenderedConversation, RenderedLine, session_agent,
};

/// The rows the viewer shows of a conversation: the conversation, the
/// message ranges of its render, and the options the render drew them with.
pub(crate) struct ShownCopy<'a> {
    pub(crate) conversation: &'a ParsedConversation,
    pub(crate) messages: &'a [MessageRange],
    pub(crate) options: &'a RenderOptions,
}

impl<'a> ShownCopy<'a> {
    /// The shown rows before a newer copy, changed from the entry numbered
    /// `first_changed` on.
    pub(super) fn changed_from(&self, first_changed: usize) -> ShownRender<'a> {
        let entries = &self.conversation.entries;
        let kept = entries.partition_point(|parsed| parsed.entry_index < first_changed);
        ShownRender {
            replaced: &entries[kept..],
            roster: &self.conversation.roster,
            messages: self.messages,
            options: self.options,
        }
    }
}

/// The rows the viewer shows of an earlier copy of a conversation, and the
/// values of that copy the newer copy doesn't have: its entries from the
/// first changed entry on, and its roster. The entries before the first
/// changed entry are the newer copy's.
pub(crate) struct ShownRender<'a> {
    pub(super) replaced: &'a [RenderableEntry],
    pub(super) roster: &'a SubagentRoster,
    /// The message ranges of the shown rows.
    pub(super) messages: &'a [MessageRange],
    /// The options the shown rows were rendered with.
    pub(super) options: &'a RenderOptions,
}

/// The first entry a render of a conversation's tail renders, and the row
/// of the shown rows its first row replaces.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TailStart {
    pub(crate) entry_index: usize,
    pub(crate) row: usize,
}

/// The rows a render of a conversation draws for the viewer: its tail after
/// the shown rows of an earlier copy, or every entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RowsToRender {
    /// Render the entries from the start's entry on, in place of the shown
    /// rows from its row on.
    Tail(TailStart),
    /// Render the whole conversation again.
    Whole,
}

/// [`tail_render_start`] for `conversation`, a newer read of the shown
/// conversation, from the first entry whose hash in `hashes` differs from
/// the shown copy's `shown_hashes`. A read whose entries are equal renders
/// the last shown message again. The whole conversation renders again when
/// either read has another count of hashes than of entries.
pub(crate) fn tail_after_read(
    shown: ShownCopy<'_>,
    shown_hashes: &EntryHashes,
    conversation: &ParsedConversation,
    hashes: &EntryHashes,
    options: &RenderOptions,
) -> RowsToRender {
    if shown_hashes.len() != shown.conversation.origins.len()
        || hashes.len() != conversation.origins.len()
    {
        return RowsToRender::Whole;
    }
    let first_changed = shown_hashes.first_change_in(hashes).unwrap_or(hashes.len());
    tail_render_start(
        shown.changed_from(first_changed),
        conversation,
        first_changed,
        options,
    )
}

/// The shown rows a tail replaced: their range with the tail's row count,
/// and the rows themselves.
pub(crate) struct SplicedTail {
    pub(crate) replaced: ReplacedRows,
    pub(crate) removed: Vec<RenderedLine>,
}

/// Put `tail`, a tail's render counted from its first row, in place of
/// `shown`'s rows from `row` on, and its messages and calls, moved down by
/// `row`, in place of those starting at or after `row`.
///
/// The row records before `row` stay where they are while the capacity
/// allows, and gain spare room when it doesn't, as a message rendered again
/// in place does. The tail's connectors are drawn over its own rows: no kept
/// call reaches `row` (see [`tail_render_start`]).
pub(crate) fn splice_tail(
    shown: ShownRows<'_>,
    tail: RenderedConversation,
    row: usize,
) -> SplicedTail {
    let removed = shown.lines.split_off(row);
    let replaced = ReplacedRows {
        old: row..row + removed.len(),
        new_len: tail.lines.len(),
    };
    if shown.lines.capacity() - shown.lines.len() < tail.lines.len() {
        shown
            .lines
            .reserve_exact(tail.lines.len() + SPARE_ROWS_AFTER_GROWTH);
    }
    shown.lines.extend(tail.lines);

    let kept = shown
        .messages
        .partition_point(|message| message.start_line < row);
    shown.messages.truncate(kept);
    shown
        .messages
        .extend(tail.messages.into_iter().map(|message| MessageRange {
            start_line: message.start_line + row,
            end_line: message.end_line + row,
            ..message
        }));

    let kept = shown
        .calls
        .partition_point(|call| call.input.start_line < row);
    shown.calls.truncate(kept);
    shown.calls.extend(tail.calls.into_iter().map(|mut call| {
        move_call_down(&mut call, row);
        call
    }));

    let kept = shown
        .detail_calls
        .partition_point(|call| call.rows.input.start < row);
    shown.detail_calls.truncate(kept);
    shown
        .detail_calls
        .extend(tail.detail_calls.into_iter().map(|mut call| {
            call.rows.input = call.rows.input.start + row..call.rows.input.end + row;
            if let Some(result) = call.rows.result.as_mut() {
                *result = result.start + row..result.end + row;
            }
            call
        }));
    SplicedTail { replaced, removed }
}

fn move_call_down(call: &mut CallRange, rows: usize) {
    for area in std::iter::once(&mut call.input).chain(call.result.as_mut()) {
        area.start_line += rows;
        area.end_line += rows;
    }
}

/// The render of `conversation` with `options` after `shown`, the rows of a
/// copy with the same entries before the one numbered `first_changed`. The
/// session agent the shown rows borrowed is the first that the entries before
/// the first changed entry name, or else the first of the replaced entries.
///
/// The start is the first entry of the last message shown before the first
/// changed entry, then of an earlier message while a row before it reads a
/// value the change alters:
/// - a call answered at or after the start, in either copy: its lane, its
///   connector and its result, and a tool run's late results;
/// - a sub-agent label new in the changed roster, or a task path whose
///   sub-agent differs in it.
///
/// The whole conversation renders again when the options differ from the
/// shown render's, when summary mode's runs of received results would show
/// another session agent, when a sub-agent the shown roster labels gets
/// another label, and when the start is the first entry.
pub(super) fn tail_render_start(
    shown: ShownRender<'_>,
    conversation: &ParsedConversation,
    first_changed: usize,
    options: &RenderOptions,
) -> RowsToRender {
    let roster_changes = RosterChanges::between(shown.roster, &conversation.roster);
    let entries = &conversation.entries;
    let kept = entries.partition_point(|parsed| parsed.entry_index < first_changed);
    let shown_session_agent =
        session_agent(&entries[..kept]).or_else(|| session_agent(shown.replaced));
    if shown.options != options
        || (options.tool_display.is_summary() && shown_session_agent != session_agent(entries))
        || roster_changes.relabels_shown_sub_agent()
    {
        return RowsToRender::Whole;
    }
    let start_before = first_roster_reader(&roster_changes, &entries[..kept])
        .map_or(first_changed, |reader| reader.entry_index + 1);
    let Some((message, first)) =
        start_answering_every_call(&shown, conversation, first_changed, start_before)
    else {
        return RowsToRender::Whole;
    };
    RowsToRender::Tail(TailStart {
        entry_index: message.entry_index,
        row: message.start_line - day_label_rows_at(entries, first, options),
    })
}

/// The first of `kept` whose rows read a roster value in `changes`.
fn first_roster_reader<'a>(
    changes: &RosterChanges,
    kept: &'a [RenderableEntry],
) -> Option<&'a RenderableEntry> {
    if changes.is_empty() {
        return None;
    }
    kept.iter().find(|parsed| changes.reaches(&parsed.entry))
}

/// The last message shown before the entry numbered `start_before`, then an
/// earlier one while a call before its first entry is answered at or after
/// it, in either copy; with that first entry's position in `conversation`'s
/// entries. `None` when the shown rows don't have such a message, or when
/// the message starts at the conversation's first entry.
fn start_answering_every_call<'a>(
    shown: &ShownRender<'a>,
    conversation: &'a ParsedConversation,
    first_changed: usize,
    mut start_before: usize,
) -> Option<(&'a MessageRange, usize)> {
    let entries = &conversation.entries;
    let kept = entries.partition_point(|parsed| parsed.entry_index < first_changed);
    let mut later_results = LaterResults::of(shown.replaced, &entries[kept..]);
    let mut results_from = kept;
    loop {
        let message = shown
            .messages
            .partition_point(|message| message.entry_index < start_before)
            .checked_sub(1)
            .map(|message| &shown.messages[message])?;
        let first = entries.partition_point(|parsed| parsed.entry_index < message.entry_index);
        if first == 0 {
            return None;
        }
        later_results.move_back_over(&entries[first..results_from]);
        results_from = first;
        match later_results.earliest_call_in(&entries[..first]) {
            Some(call) => start_before = entries[call].entry_index + 1,
            None => return Some((message, first)),
        }
    }
}

/// The rows of the day label a render adds before the entry at `first`: one
/// while the timing column shows and the entry starts a day, none otherwise.
fn day_label_rows_at(entries: &[RenderableEntry], first: usize, options: &RenderOptions) -> usize {
    let mut day_labels = DayLabels::new(options.show_timing, options.today, options.zone);
    day_labels.resume_after(entries[..first].iter().map(|parsed| &parsed.entry));
    usize::from(day_labels.new_day_at(&entries[first].entry).is_some())
}

/// The call ids that results from a position on answer, in each copy, for
/// calls before that position.
struct LaterResults<'a> {
    shown: HashSet<&'a str>,
    changed: HashSet<&'a str>,
}

impl<'a> LaterResults<'a> {
    /// The results of the two copies' entries after the ones they share.
    fn of(shown_tail: &'a [RenderableEntry], changed_tail: &'a [RenderableEntry]) -> Self {
        let mut results = Self {
            shown: HashSet::new(),
            changed: HashSet::new(),
        };
        move_back_over(&mut results.shown, shown_tail);
        move_back_over(&mut results.changed, changed_tail);
        results
    }

    /// Move the position back over `shared`, entries both copies contain
    /// just before it.
    fn move_back_over(&mut self, shared: &'a [RenderableEntry]) {
        move_back_over(&mut self.shown, shared);
        move_back_over(&mut self.changed, shared);
    }

    /// The position in `before` of the earliest call these results answer.
    /// Walks back from its end until every answered call is found.
    fn earliest_call_in(&self, before: &[RenderableEntry]) -> Option<usize> {
        let mut calls: HashSet<&str> = self.shown.union(&self.changed).copied().collect();
        let mut earliest = None;
        for (position, parsed) in before.iter().enumerate().rev() {
            if calls.is_empty() {
                break;
            }
            for (id, kind) in tool_blocks(&parsed.entry) {
                if kind == ToolBlockKind::Call && calls.remove(id) {
                    earliest = Some(position);
                }
            }
        }
        earliest
    }
}

/// Walk `entries` from the last block back: a result adds the call id it
/// answers to `ids`, and a call removes its own.
fn move_back_over<'a>(ids: &mut HashSet<&'a str>, entries: &'a [RenderableEntry]) {
    for parsed in entries.iter().rev() {
        for (id, kind) in tool_blocks(&parsed.entry).rev() {
            match kind {
                ToolBlockKind::Call => ids.remove(id),
                ToolBlockKind::Result => ids.insert(id),
            };
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ToolBlockKind {
    Call,
    Result,
}

/// The id of each call `entry` contains, and of each call a result it
/// contains answers, whoever's entry it is. A standalone result answers no
/// call and is left out.
fn tool_blocks(entry: &LogEntry) -> impl DoubleEndedIterator<Item = (&str, ToolBlockKind)> {
    entry
        .content_blocks()
        .iter()
        .filter_map(|block| match block {
            ContentBlock::ToolUse { id, .. } => Some((id.as_str(), ToolBlockKind::Call)),
            ContentBlock::ToolResult {
                tool_use_id,
                standalone_tool_name: None,
                ..
            } => Some((tool_use_id.as_str(), ToolBlockKind::Result)),
            _ => None,
        })
}
