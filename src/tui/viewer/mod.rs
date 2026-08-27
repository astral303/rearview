//! Conversation viewer rendering for TUI display.
//!
//! This module renders conversation JSONL files to `Vec<RenderedLine>` for display
//! in the TUI viewer. It produces styled spans that ratatui can render directly,
//! without using ANSI escape codes.

use crate::log_entry::LogEntry;
use std::collections::BTreeSet;
use std::path::Path;

use crate::tui::theme::{self, Theme};

mod calls;
mod commands;
mod connectors;
mod entry;

pub(crate) use commands::process_command_message;
mod ledger;
mod markdown;
mod output;
mod style;
mod summary;
mod timing;
mod tools;

pub use output::{LineStyle, RenderedLine};

use calls::{CallRanges, top_level_tool_blocks};
use entry::render_entry;
use summary::{
    PendingToolSummary, flush_tool_summary, tool_only_assistant_summary,
    user_entry_is_only_tool_results,
};
use tools::make_tool_summary_output_id;

/// Width of the focus gutter indicator (▌ + space)
pub const GUTTER_WIDTH: usize = 2;

const NAME_WIDTH: usize = 9;
/// Width of timestamp prefix when timing is enabled (space + HH:MM + space)
const TIMESTAMP_WIDTH: usize = 7;
/// Width of the ` │ ` between the name column and the content
const SEPARATOR_WIDTH: usize = 3;

/// The columns a row has left for text once the ledger columns — the gutter,
/// the timestamp column while timing is shown, the name and the separator —
/// are taken from `frame_width`. Every wrap in the viewer uses this width.
pub fn content_width(frame_width: usize, show_timing: bool) -> usize {
    let timestamp = if show_timing { TIMESTAMP_WIDTH } else { 0 };
    frame_width.saturating_sub(GUTTER_WIDTH + timestamp + NAME_WIDTH + SEPARATOR_WIDTH)
}

/// Get the current theme (cached after first detection)
fn th() -> &'static Theme {
    theme::detect_theme()
}

/// Maximum body lines shown in truncated tool call mode
const TRUNCATED_BODY_LINES: usize = 3;
/// Maximum result lines shown in truncated tool result mode
const TRUNCATED_RESULT_LINES: usize = 4;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ToolOutputId(pub String);

/// Controls how tool calls and results are displayed
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ToolDisplayMode {
    #[default]
    Hidden,
    Truncated,
    Full,
}

impl ToolDisplayMode {
    /// Cycle to the next mode: Summary → Truncated → Full → Summary
    pub fn next(self) -> Self {
        match self {
            Self::Hidden => Self::Truncated,
            Self::Truncated => Self::Full,
            Self::Full => Self::Hidden,
        }
    }

    pub fn is_summary(self) -> bool {
        matches!(self, Self::Hidden)
    }

    /// Whether full or truncated tool details should be rendered
    pub fn shows_details(self) -> bool {
        !matches!(self, Self::Hidden)
    }

    /// Whether tools should be included in exported text
    pub fn is_visible(self) -> bool {
        self.shows_details()
    }

    /// Fixed-width label for the status bar (3 chars each)
    pub fn status_label(self) -> &'static str {
        match self {
            Self::Hidden => "sum",
            Self::Truncated => "trn",
            Self::Full => "all",
        }
    }
}

/// Options for rendering a conversation
pub struct RenderOptions {
    pub tool_display: ToolDisplayMode,
    pub show_thinking: bool,
    pub show_timing: bool,
    pub content_width: usize,
    pub expanded_tool_outputs: BTreeSet<ToolOutputId>,
}

/// Tracks the line range of a single message (User or Assistant entry) in the rendered output
#[derive(Clone, Debug)]
pub struct MessageRange {
    /// Index of the JSONL entry (line number in the file, 0-based, counting only parsed entries)
    pub entry_index: usize,
    /// Start line in rendered output (inclusive)
    pub start_line: usize,
    /// End line in rendered output (exclusive, excludes trailing blank)
    pub end_line: usize,
}

impl MessageRange {
    pub fn rows(&self) -> std::ops::Range<usize> {
        self.start_line..self.end_line
    }
}

/// One tool call's input rows and, when a result answers it, its result
/// rows. The two areas need not be adjacent: interleaved calls render every
/// input before the first result.
#[derive(Clone, Debug)]
pub struct CallRange {
    pub input: CallArea,
    pub result: Option<CallArea>,
    /// The lane of a call open beside another (issued while an earlier
    /// call awaited its result, or awaiting its own when a later call was
    /// issued), counted from the first lane cell. `None` for a call open
    /// alone, or never answered.
    pub lane: Option<usize>,
}

impl CallRange {
    pub fn areas(&self) -> impl DoubleEndedIterator<Item = &CallArea> {
        std::iter::once(&self.input).chain(self.result.as_ref())
    }

    /// Other calls' rows may sit here; empty without a result.
    pub fn input_to_result_gap(&self) -> std::ops::Range<usize> {
        let end = self
            .result
            .as_ref()
            .map_or(self.input.end_line, |result| result.start_line);
        self.input.end_line..end
    }

    pub fn contains_line(&self, line_idx: usize) -> bool {
        self.areas().any(|area| area.contains_line(line_idx))
    }

    /// Whether any of the call's rows fall inside `rows`. Input and result are
    /// separate areas, with other calls' rows possibly between them.
    pub fn overlaps(&self, rows: &std::ops::Range<usize>) -> bool {
        self.areas()
            .any(|area| area.start_line < rows.end && area.end_line > rows.start)
    }
}

/// The rows one tool-output id was rendered to, `end_line` exclusive.
#[derive(Clone, Debug)]
pub struct CallArea {
    pub id: ToolOutputId,
    pub location: BlockLocation,
    pub start_line: usize,
    pub end_line: usize,
}

/// A content block's place in the conversation: the entry's index among the
/// parsed entries and the block's index within that entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlockLocation {
    pub entry_index: usize,
    pub block_index: usize,
}

impl CallArea {
    pub fn contains_line(&self, line_idx: usize) -> bool {
        (self.start_line..self.end_line).contains(&line_idx)
    }
}

/// Result of rendering a conversation
pub struct RenderedConversation {
    pub lines: Vec<RenderedLine>,
    pub messages: Vec<MessageRange>,
    /// The calls of expanded runs, which `]` steps through; empty in the
    /// detail modes.
    pub calls: Vec<CallRange>,
}

/// Format an ISO 8601 timestamp to HH:MM local time
fn format_timestamp(iso_timestamp: &str) -> Option<String> {
    use chrono::{DateTime, Local};
    // Parse RFC 3339 timestamp (handles timezone offsets) and convert to local time
    DateTime::parse_from_rfc3339(iso_timestamp)
        .ok()
        .map(|dt| dt.with_timezone(&Local).format("%H:%M").to_string())
}

#[derive(Debug)]
pub struct RenderableEntry {
    pub entry_index: usize,
    entry: LogEntry,
}

/// A conversation the list already attributed to a source: only that
/// provider's format reads the file.
pub fn parse_conversation_file(
    source: crate::history::Source,
    file_path: &Path,
) -> std::io::Result<Vec<RenderableEntry>> {
    let normalized = crate::history::normalized_log_entries(source, file_path)
        .map_err(|error| std::io::Error::other(error.to_string()))?;
    Ok(renderable_entries(normalized))
}

/// A bare file the user handed us (`--render`, a direct path argument), read
/// by whichever registered format recognizes it.
pub fn parse_unattributed_conversation_file(
    file_path: &Path,
) -> std::io::Result<Vec<RenderableEntry>> {
    let normalized = crate::history::sniffed_log_entries(file_path)
        .map_err(|error| std::io::Error::other(error.to_string()))?;
    Ok(renderable_entries(normalized))
}

fn renderable_entries(normalized: Vec<(usize, LogEntry)>) -> Vec<RenderableEntry> {
    normalized
        .into_iter()
        .map(|(_, entry)| entry)
        .enumerate()
        .filter_map(|(entry_index, entry)| {
            (!matches!(entry, LogEntry::FileHistorySnapshot { .. }))
                .then_some(RenderableEntry { entry_index, entry })
        })
        .collect()
}

pub fn render_parsed_conversation(
    entries: &[RenderableEntry],
    options: &RenderOptions,
) -> RenderedConversation {
    let mut lines = Vec::new();
    let mut messages = Vec::new();
    let mut calls = Vec::new();
    let mut pending_tool_summary: Option<PendingToolSummary> = None;
    // Summary mode pairs the calls of each expanded run as the run renders;
    // the detail modes pair every top-level call of the conversation.
    let mut call_ranges = if options.tool_display.shows_details() {
        CallRanges::new(top_level_tool_blocks(entries))
    } else {
        CallRanges::default()
    };

    for (parsed_idx, parsed) in entries.iter().enumerate() {
        if options.tool_display.is_summary()
            && try_extend_or_start_pending_summary(
                &mut lines,
                &mut messages,
                &mut calls,
                &mut pending_tool_summary,
                entries,
                parsed_idx,
                options,
            )
        {
            continue;
        }

        // Rendering before the flush keeps a tool run open across entries
        // that produce no output under the current options (Codex usage
        // metadata, thinking-only entries with thinking off); otherwise each
        // such entry would split the run into one summary row per call.
        let mut entry_lines = Vec::new();
        let tool_blocks = render_entry(
            &mut entry_lines,
            parsed.entry_index,
            &parsed.entry,
            options,
            &call_ranges,
        );
        if entry_lines.is_empty() {
            continue;
        }

        flush_tool_summary(
            &mut lines,
            &mut messages,
            &mut calls,
            &mut pending_tool_summary,
            entries,
            options,
        );

        let first_row = lines.len();
        append_entry_with_range(&mut lines, &mut messages, parsed, entry_lines, options);
        for block in tool_blocks {
            call_ranges.record(block.offset_by(first_row));
        }
    }

    flush_tool_summary(
        &mut lines,
        &mut messages,
        &mut calls,
        &mut pending_tool_summary,
        entries,
        options,
    );
    // `]` steps through the calls of an expanded run; in the detail modes it
    // steps through messages, so their calls are drawn but not returned.
    let mut detail_calls = call_ranges.into_calls();

    postprocess_blank_lines(
        &mut lines,
        &mut messages,
        calls.iter_mut().chain(&mut detail_calls),
    );
    connectors::draw_connectors(
        &mut lines,
        calls.iter().chain(&detail_calls),
        options.show_timing,
    );

    RenderedConversation {
        lines,
        messages,
        calls,
    }
}

/// Handle a parsed entry while in summary tool-display mode.
///
/// Returns `true` when the entry was absorbed into (or started) a pending
/// summary group and should be skipped by the normal render path.
fn try_extend_or_start_pending_summary(
    lines: &mut Vec<RenderedLine>,
    messages: &mut Vec<MessageRange>,
    calls: &mut Vec<CallRange>,
    pending: &mut Option<PendingToolSummary>,
    entries: &[RenderableEntry],
    parsed_idx: usize,
    options: &RenderOptions,
) -> bool {
    let parsed = &entries[parsed_idx];
    let entry_index = parsed.entry_index;
    let entry = &parsed.entry;

    if let Some((parent_id, agent, timestamp, summary)) =
        tool_only_assistant_summary(entry, options)
    {
        match pending {
            Some(p) if p.parent_id.as_deref() == parent_id && p.agent.as_deref() == agent => {
                p.absorb(parsed_idx, timestamp);
                p.summary.merge(summary);
            }
            _ => {
                flush_tool_summary(lines, messages, calls, pending, entries, options);
                *pending = Some(PendingToolSummary {
                    id: make_tool_summary_output_id(entry_index, parent_id),
                    first_entry_index: entry_index,
                    first_parsed_idx: parsed_idx,
                    last_parsed_idx: parsed_idx,
                    parent_id: parent_id.map(str::to_string),
                    agent: agent.map(str::to_string),
                    started_at: timestamp.map(str::to_string),
                    ended_at: timestamp.map(str::to_string),
                    summary,
                });
            }
        }
        return true;
    }

    if user_entry_is_only_tool_results(entry, options) {
        if let Some(p) = pending {
            p.absorb(parsed_idx, entry.timestamp());
        }
        return true;
    }

    false
}

/// Append one parsed entry's rendered lines and, if the entry is a navigable
/// message, a `MessageRange` that excludes any trailing blank line.
fn append_entry_with_range(
    lines: &mut Vec<RenderedLine>,
    messages: &mut Vec<MessageRange>,
    parsed: &RenderableEntry,
    entry_lines: Vec<RenderedLine>,
    options: &RenderOptions,
) {
    let entry_index = parsed.entry_index;
    let entry = &parsed.entry;
    let is_message = matches!(entry, LogEntry::User { .. } | LogEntry::Assistant { .. })
        || matches!(entry, LogEntry::Progress { data, .. }
            if options.show_thinking && crate::log_entry::parse_agent_progress(data).is_some());

    let start_line = lines.len();
    lines.extend(entry_lines);
    let end_line = lines.len();

    if !is_message {
        return;
    }
    if let Some(range) =
        message_range_excluding_trailing_blank(lines, start_line, end_line, entry_index)
    {
        messages.push(range);
    }
}

/// If the rendered slice produced any non-blank lines, return a
/// `MessageRange` whose `end_line` excludes a trailing blank.
fn message_range_excluding_trailing_blank(
    lines: &[RenderedLine],
    start_line: usize,
    end_line: usize,
    entry_index: usize,
) -> Option<MessageRange> {
    if end_line <= start_line {
        return None;
    }
    let effective_end = if lines.get(end_line - 1).is_some_and(|l| l.spans.is_empty()) {
        end_line - 1
    } else {
        end_line
    };
    if effective_end <= start_line {
        return None;
    }
    Some(MessageRange {
        entry_index,
        start_line,
        end_line: effective_end,
    })
}

/// Collapse consecutive blank rendered lines and remap message and call
/// ranges so they continue to point at their original visible content.
///
/// Multiple render helpers each push a trailing blank line, which can
/// produce adjacent blanks when a tool result emits empty output. The
/// dedup pass removes any blank line whose immediate predecessor is also
/// blank, and the remap pass shifts every range start/end onto the new
/// line indices, clamping ranges that ended on a removed blank.
fn postprocess_blank_lines<'a>(
    lines: &mut Vec<RenderedLine>,
    messages: &mut Vec<MessageRange>,
    calls: impl Iterator<Item = &'a mut CallRange>,
) {
    let mut removed = vec![false; lines.len()];
    let mut i = 1;
    while i < lines.len() {
        if lines[i].spans.is_empty() && lines[i - 1].spans.is_empty() {
            removed[i] = true;
        }
        i += 1;
    }

    // Build index mapping: old line index -> new line index. Removed
    // entries get the index they would collapse onto; they are never
    // dereferenced for surviving ranges because the remap below walks
    // backward off any removed terminator first.
    let mut new_index = Vec::with_capacity(lines.len());
    let mut offset = 0usize;
    for (idx, &is_removed) in removed.iter().enumerate() {
        if is_removed {
            new_index.push(idx - offset);
            offset += 1;
        } else {
            new_index.push(idx - offset);
        }
    }
    let total_after = lines.len() - offset;

    // Compact in place.
    let mut write = 0;
    for (read, &is_removed) in removed.iter().enumerate() {
        if !is_removed {
            if write != read {
                lines.swap(write, read);
            }
            write += 1;
        }
    }
    lines.truncate(total_after);

    let remap = |start_line: usize, end_line: usize| {
        remapped_line_range(start_line, end_line, &new_index, &removed, total_after)
    };
    for msg in messages.iter_mut() {
        (msg.start_line, msg.end_line) = remap(msg.start_line, msg.end_line);
    }
    for area in calls.flat_map(|call| std::iter::once(&mut call.input).chain(call.result.as_mut()))
    {
        (area.start_line, area.end_line) = remap(area.start_line, area.end_line);
    }

    messages.retain(|m| m.start_line < m.end_line);
}

/// Where a `start_line..end_line` range lands once the lines flagged in
/// `removed` are gone. An exclusive end that sat on a removed blank moves
/// back to the last surviving line before it.
fn remapped_line_range(
    start_line: usize,
    end_line: usize,
    new_index: &[usize],
    removed: &[bool],
    total_after: usize,
) -> (usize, usize) {
    let new_start = new_index[start_line];
    let new_end = if end_line > 0 && end_line <= new_index.len() {
        let mut last = end_line - 1;
        while last > start_line && removed[last] {
            last -= 1;
        }
        new_index[last] + 1
    } else if end_line == new_index.len() {
        total_after
    } else {
        end_line
    };
    let new_end = new_end.min(total_after);
    (new_start.min(new_end), new_end)
}

/// Render a bare conversation file to lines — the `--render` path, where the
/// file arrives with no source attached.
pub fn render_conversation(
    file_path: &Path,
    options: &RenderOptions,
) -> std::io::Result<RenderedConversation> {
    let entries = parse_unattributed_conversation_file(file_path)?;
    Ok(render_parsed_conversation(&entries, options))
}

#[cfg(test)]
mod tests;
