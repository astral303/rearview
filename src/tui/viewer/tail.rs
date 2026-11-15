//! The start of a render of a conversation's tail after a refresh of a
//! session with new messages: the first entry of a message the shown rows
//! contain before the change, early enough that no row before it changes.
#![cfg_attr(
    not(test),
    expect(dead_code, reason = "the viewer's reload doesn't render a tail yet")
)]

use std::collections::HashSet;

use crate::log_entry::{ContentBlock, LogEntry, UserContent};

use super::days::DayLabels;
use super::roster::RosterChanges;
use super::{MessageRange, ParsedConversation, RenderOptions, RenderableEntry, session_agent};

/// The rows the viewer shows of a conversation: the conversation, the
/// message ranges of its render, and the options the render drew them with.
pub(super) struct ShownRender<'a> {
    pub(super) conversation: &'a ParsedConversation,
    pub(super) messages: &'a [MessageRange],
    pub(super) options: &'a RenderOptions,
}

/// The first entry a render of a conversation's tail renders, and the row
/// of the shown rows its first row replaces.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct TailStart {
    pub(super) entry_index: usize,
    pub(super) row: usize,
}

/// The render of a changed conversation after the shown rows of an earlier
/// copy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum TailRender {
    /// Render the entries from the start's entry on, in place of the shown
    /// rows from its row on.
    Tail(TailStart),
    /// Render the whole conversation again.
    Whole,
}

/// The render of `conversation` with `options` after `shown`, the rows of a
/// copy with the same entries before the one numbered `first_changed`.
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
) -> TailRender {
    let roster_changes = RosterChanges::between(&shown.conversation.roster, &conversation.roster);
    if shown.options != options
        || (options.tool_display.is_summary()
            && session_agent(&shown.conversation.entries) != session_agent(&conversation.entries))
        || roster_changes.relabels_shown_sub_agent()
    {
        return TailRender::Whole;
    }
    let entries = &conversation.entries;
    let kept = entries.partition_point(|parsed| parsed.entry_index < first_changed);
    let start_before = first_roster_reader(&roster_changes, &entries[..kept])
        .map_or(first_changed, |reader| reader.entry_index + 1);
    let Some((message, first)) =
        start_answering_every_call(&shown, conversation, first_changed, start_before)
    else {
        return TailRender::Whole;
    };
    TailRender::Tail(TailStart {
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
    let shown_entries = &shown.conversation.entries;
    let kept = entries.partition_point(|parsed| parsed.entry_index < first_changed);
    let shown_kept = shown_entries.partition_point(|parsed| parsed.entry_index < first_changed);
    let mut later_results = LaterResults::of(&shown_entries[shown_kept..], &entries[kept..]);
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
    let blocks: &[ContentBlock] = match entry {
        LogEntry::Assistant { message, .. } => &message.content,
        LogEntry::User { message, .. } => match &message.content {
            UserContent::Blocks(blocks) => blocks,
            UserContent::String(_) => &[],
        },
        _ => &[],
    };
    blocks.iter().filter_map(|block| match block {
        ContentBlock::ToolUse { id, .. } => Some((id.as_str(), ToolBlockKind::Call)),
        ContentBlock::ToolResult {
            tool_use_id,
            standalone_tool_name: None,
            ..
        } => Some((tool_use_id.as_str(), ToolBlockKind::Result)),
        _ => None,
    })
}
