//! Conversation viewer rendering for TUI display.
//!
//! This module renders conversation JSONL files to `Vec<RenderedLine>` for display
//! in the TUI viewer. It produces styled spans that ratatui can render directly,
//! without using ANSI escape codes.

use crate::clock::Zone;
use crate::history::{DisplayEntries, EntryOrigin};
use crate::log_entry::LogEntry;
use chrono::{DateTime, FixedOffset, NaiveDate};
use std::collections::BTreeSet;
use std::path::Path;

use crate::tui::theme::{self, Theme};

mod calls;
mod commands;
mod connectors;
mod days;
mod entry;

pub(crate) use commands::{local_command_output, user_text};
mod ledger;
mod markdown;
mod output;
mod relocation;
mod rerender;
mod roster;
mod search_rows;
mod style;
mod summary;
mod timing;
mod tools;

pub(crate) use days::{session_dates, short_date};
pub use output::{LineStyle, RenderedLine};
pub use relocation::EntryRelocation;
pub use rerender::{EntryInPlace, ShownRows, entry_rendering_alone, render_entry_in_place};
pub(crate) use roster::SubagentRoster;
#[cfg(test)]
pub use search_rows::find_matches;
pub use search_rows::{SearchMatch, find_matches_while, update_matches_in_replaced_rows};
pub(crate) use summary::format_coarse_duration;

use calls::{CallRanges, Lanes, top_level_tool_blocks};
use days::DayLabels;
use entry::render_entry;
use summary::{
    PendingToolSummary, RunAuthor, ToolOnlyReply, UserToolEntry, classify_user_tool_entry,
    flush_tool_summary, tool_only_assistant_summary,
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

/// Body lines a truncated tool call shows; a body one line over shows whole.
const TRUNCATED_BODY_LINES: usize = 3;
/// Lines a truncated tool result, task report or sub-agent reply shows; a
/// block one line over shows whole.
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
    /// False under `--render`: task reports and sub-agent replies render
    /// whole. Tool results ignore it.
    pub can_expand: bool,
    /// The local date the day labels name relative to.
    pub today: NaiveDate,
    /// The zone times and day labels are shown in.
    pub zone: Zone,
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

    pub fn rows(&self) -> ConnectorRows {
        ConnectorRows {
            input: self.input.rows(),
            result: self.result.as_ref().map(CallArea::rows),
            lane: self.lane,
        }
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
        self.rows().contains(&line_idx)
    }

    pub fn rows(&self) -> std::ops::Range<usize> {
        self.start_line..self.end_line
    }
}

/// A call's input rows, result rows and lane: what its connector and the
/// colour of its rule are drawn from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConnectorRows {
    pub input: std::ops::Range<usize>,
    pub result: Option<std::ops::Range<usize>>,
    pub lane: Option<usize>,
}

impl ConnectorRows {
    /// The rows the call's connector and rule colours draw on: from the
    /// input's first row to the end of the input or the result, whichever
    /// is later. A result without rows still covers its first row.
    pub fn touched_rows(&self) -> std::ops::Range<usize> {
        let end = self.result.as_ref().map_or(self.input.end, |result| {
            self.input.end.max(result.end).max(result.start + 1)
        });
        self.input.start..end
    }
}

/// A top-level call the detail modes drew: its rows, and the blocks of its
/// input and result. A render of one entry finds the call again by those
/// blocks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DetailCall {
    pub rows: ConnectorRows,
    pub input: BlockLocation,
    pub result: Option<BlockLocation>,
}

impl DetailCall {
    fn of(call: &CallRange) -> Self {
        Self {
            rows: call.rows(),
            input: call.input.location,
            result: call.result.as_ref().map(|result| result.location),
        }
    }
}

/// Result of rendering a conversation
pub struct RenderedConversation {
    pub lines: Vec<RenderedLine>,
    pub messages: Vec<MessageRange>,
    /// The calls of expanded runs, which `]` steps through; empty in the
    /// detail modes.
    pub calls: Vec<CallRange>,
    /// The top-level calls of the detail modes, in the order they were
    /// issued; empty in summary mode.
    pub detail_calls: Vec<DetailCall>,
}

/// Format an ISO 8601 timestamp to HH:MM in `zone`
fn format_timestamp(iso_timestamp: &str, zone: Zone) -> Option<String> {
    // Parse RFC 3339 timestamp (handles timezone offsets) and convert to `zone`
    DateTime::parse_from_rfc3339(iso_timestamp)
        .ok()
        .map(|dt| dt.with_timezone(&zone).format("%H:%M").to_string())
}

#[derive(Debug)]
pub struct RenderableEntry {
    pub entry_index: usize,
    entry: LogEntry,
}

/// A conversation as the viewer reads it: its entries and the
/// `SubagentRoster` built from them.
#[derive(Debug)]
pub struct ParsedConversation {
    entries: Vec<RenderableEntry>,
    /// Where each displayed entry was read, by its `entry_index`.
    origins: Vec<EntryOrigin>,
    roster: SubagentRoster,
    /// The lanes of the top-level calls, which the detail modes draw.
    lanes: Lanes,
    /// The earliest and latest message timestamps, for the header's dates.
    activity_span: Option<(DateTime<FixedOffset>, DateTime<FixedOffset>)>,
}

impl ParsedConversation {
    /// A conversation without entries: what the viewer shows of a session
    /// missing from the cache until its refresh finishes.
    pub fn empty() -> Self {
        Self {
            entries: Vec::new(),
            origins: Vec::new(),
            roster: SubagentRoster::from_identities(Vec::new()),
            lanes: Lanes::default(),
            activity_span: None,
        }
    }

    /// The earliest and latest user or assistant message times; `None` when
    /// no message carries one.
    pub fn activity_span(&self) -> Option<(DateTime<FixedOffset>, DateTime<FixedOffset>)> {
        self.activity_span
    }

    /// The entry numbered `entry_index` among the displayed entries.
    pub fn entry(&self, entry_index: usize) -> Option<&LogEntry> {
        let position = self
            .entries
            .binary_search_by_key(&entry_index, |parsed| parsed.entry_index)
            .ok()?;
        Some(&self.entries[position].entry)
    }
}

/// The day in `zone` of the first message with a row at or below `line`, the
/// row the viewer shows at its top, or of the next message after it that
/// carries a timestamp.
pub fn day_at_line(
    conversation: &ParsedConversation,
    messages: &[MessageRange],
    line: usize,
    zone: Zone,
) -> Option<chrono::NaiveDate> {
    let first_shown = messages.partition_point(|message| message.end_line <= line);
    messages[first_shown..].iter().find_map(|message| {
        let timestamp = conversation
            .entry(message.entry_index)?
            .activity_timestamp()?;
        days::day_in(timestamp, &zone)
    })
}

/// A copy of a session as the viewer shows it: its conversation, and the
/// copy's catalogue row the header shows.
pub struct ViewedConversation {
    pub conversation: ParsedConversation,
    /// `None` when the cache no longer lists the copy.
    pub row: Option<Box<crate::history::Conversation>>,
}

/// The conversation the cache keeps at `address`, its newest copy read
/// through `cache`'s interactive read, for a row whose publication is
/// `publication`.
pub fn parse_conversation_file_interactively(
    cache: &crate::history::InteractiveCache,
    address: &crate::history::sqlite_cache::SessionAddress,
    publication: &crate::history::sqlite_cache::Publication,
) -> crate::history::InteractiveRead<ViewedConversation> {
    crate::history::viewed_copy_interactively(cache, address, publication).map(|viewed| {
        ViewedConversation {
            conversation: parsed_conversation(viewed.entries),
            row: viewed.row,
        }
    })
}

/// A bare file the user names (`--render`, a direct path argument), read by
/// whichever registered format recognises it. The agent's roots come from
/// `environment`; `environment` canonicalises the file's directory and a
/// stored copy's.
pub fn parse_unattributed_conversation_file(
    permit: crate::history::ParsePermit<'_>,
    file_path: &Path,
    environment: &crate::history::provider::RootEnvironment,
) -> std::io::Result<ParsedConversation> {
    let displayed = crate::history::sniffed_display_log_entries(permit, file_path, environment)
        .map_err(|error| std::io::Error::other(error.to_string()))?;
    Ok(parsed_conversation(displayed))
}

/// Every entry but file-history snapshots, numbered by its place among the
/// displayed entries, with each sub-agent's identity recorded in the
/// `SubagentRoster`.
pub(crate) fn parsed_conversation(displayed: DisplayEntries) -> ParsedConversation {
    let mut roster = SubagentRoster::from_identities(displayed.subagent_identities);
    let origins = displayed.origins;
    let entries: Vec<RenderableEntry> = displayed
        .entries
        .into_iter()
        .enumerate()
        .filter_map(|(entry_index, entry)| {
            (!matches!(entry, LogEntry::FileHistorySnapshot { .. }))
                .then_some(RenderableEntry { entry_index, entry })
        })
        .collect();
    for parsed in &entries {
        roster.record_agent_calls(&parsed.entry);
    }
    let lanes = Lanes::of(top_level_tool_blocks(&entries));
    let activity_span = activity_span(&entries);
    ParsedConversation {
        entries,
        origins,
        roster,
        lanes,
        activity_span,
    }
}

fn activity_span(
    entries: &[RenderableEntry],
) -> Option<(DateTime<FixedOffset>, DateTime<FixedOffset>)> {
    let mut times = entries
        .iter()
        .filter_map(|parsed| DateTime::parse_from_rfc3339(parsed.entry.activity_timestamp()?).ok());
    let first = times.next()?;
    Some(times.fold((first, first), |(earliest, latest), time| {
        (earliest.min(time), latest.max(time))
    }))
}

/// The entries a render covers between two calls of its `keep_going`.
const ENTRIES_BETWEEN_STOP_CHECKS: usize = 256;

/// [`render_parsed_conversation_while`] to its end.
pub fn render_parsed_conversation(
    conversation: &ParsedConversation,
    options: &RenderOptions,
) -> RenderedConversation {
    render_parsed_conversation_while(conversation, options, &|| true)
        .expect("a render that keeps going finishes")
}

/// `conversation` rendered with `options`.
///
/// Calls `keep_going` before the first entry and every
/// [`ENTRIES_BETWEEN_STOP_CHECKS`] entries after it, then before drawing the
/// connectors over the finished rows; `None` once it returns false.
pub fn render_parsed_conversation_while(
    conversation: &ParsedConversation,
    options: &RenderOptions,
    keep_going: &dyn Fn() -> bool,
) -> Option<RenderedConversation> {
    render_entries(
        &conversation.entries,
        &conversation.roster,
        &conversation.lanes,
        options,
        keep_going,
    )
}

/// `lanes` are the lanes of `entries`' top-level calls.
fn render_entries(
    entries: &[RenderableEntry],
    roster: &SubagentRoster,
    lanes: &Lanes,
    options: &RenderOptions,
    keep_going: &dyn Fn() -> bool,
) -> Option<RenderedConversation> {
    let mut rendered = RenderedConversation {
        lines: Vec::new(),
        messages: Vec::new(),
        calls: Vec::new(),
        detail_calls: Vec::new(),
    };
    let mut pending_tool_summary: Option<PendingToolSummary> = None;
    // The detail modes pair every top-level call of the conversation here.
    // Summary mode pairs the calls of each expanded run as the run renders,
    // and its entries return no tool blocks.
    let mut call_ranges = CallRanges::new(lanes);

    // Only a collapsed run needs the session's agent.
    let session_agent = options
        .tool_display
        .is_summary()
        .then(|| session_agent(entries))
        .flatten();
    let mut day_labels = DayLabels::new(options.show_timing, options.today, options.zone);
    for (parsed_idx, parsed) in entries.iter().enumerate() {
        if parsed_idx % ENTRIES_BETWEEN_STOP_CHECKS == 0 && !keep_going() {
            return None;
        }
        if let Some(day) = day_labels.new_day_at(&parsed.entry) {
            // A run ends at midnight: the label sits between its rows and the
            // next day's.
            flush_tool_summary(
                &mut rendered,
                &mut pending_tool_summary,
                entries,
                roster,
                options,
            );
            day_labels.mark(&mut rendered.lines, day);
        }
        if options.tool_display.is_summary()
            && try_extend_or_start_pending_summary(
                &mut rendered,
                &mut pending_tool_summary,
                entries,
                roster,
                parsed_idx,
                options,
                session_agent,
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
            roster,
            &call_ranges,
        );
        // A tool block without rows still closes its call.
        if entry_lines.is_empty() && tool_blocks.is_empty() {
            continue;
        }

        flush_tool_summary(
            &mut rendered,
            &mut pending_tool_summary,
            entries,
            roster,
            options,
        );

        let first_row = rendered.lines.len();
        append_entry_with_range(
            &mut rendered.lines,
            &mut rendered.messages,
            parsed,
            entry_lines,
        );
        for block in tool_blocks {
            call_ranges.record(block.offset_by(first_row));
        }
    }

    flush_tool_summary(
        &mut rendered,
        &mut pending_tool_summary,
        entries,
        roster,
        options,
    );
    day_labels.drop_unfollowed(&mut rendered.lines);
    // `]` steps through the calls of an expanded run. Keep the detail modes'
    // calls apart, without ids: `]` steps through their messages.
    rendered.detail_calls = call_ranges
        .into_calls()
        .iter()
        .map(DetailCall::of)
        .collect();

    if !keep_going() {
        return None;
    }
    let all_rows = 0..rendered.lines.len();
    connectors::draw_connectors(
        &mut rendered.lines,
        rendered
            .calls
            .iter()
            .map(CallRange::rows)
            .chain(rendered.detail_calls.iter().map(|call| call.rows.clone())),
        options.show_timing,
        all_rows,
    );

    Some(rendered)
}

/// The agent whose session this is, named by the first top-level reply that
/// names one. A run of results the agent received carries no agent of its own,
/// so it borrows the session's; with no reply naming one, the run takes the
/// default label, as an unnamed assistant entry does.
fn session_agent(entries: &[RenderableEntry]) -> Option<&str> {
    entries.iter().find_map(|parsed| match &parsed.entry {
        LogEntry::Assistant {
            agent,
            parent_tool_use_id: None,
            ..
        } => agent.as_deref(),
        _ => None,
    })
}

/// Handle a parsed entry while in summary tool-display mode.
///
/// Returns `true` when the entry was absorbed into (or started) a pending
/// summary group and should be skipped by the normal render path.
fn try_extend_or_start_pending_summary(
    rendered: &mut RenderedConversation,
    pending: &mut Option<PendingToolSummary>,
    entries: &[RenderableEntry],
    roster: &SubagentRoster,
    parsed_idx: usize,
    options: &RenderOptions,
    session_agent: Option<&str>,
) -> bool {
    let parsed = &entries[parsed_idx];
    let entry_index = parsed.entry_index;
    let entry = &parsed.entry;

    if let Some(ToolOnlyReply {
        parent_id,
        agent,
        timestamp,
        summary,
    }) = tool_only_assistant_summary(entry, options)
    {
        extend_or_start(
            rendered,
            pending,
            entries,
            roster,
            options,
            PendingToolSummary::opening(
                RunAuthor::Agent(agent.map(str::to_string)),
                entry_index,
                parsed_idx,
                parent_id,
                timestamp,
                summary,
            ),
        );
        return true;
    }

    match classify_user_tool_entry(entry, options, session_agent) {
        Some(UserToolEntry::Opens {
            author,
            parent_id,
            timestamp,
            summary,
        }) => {
            extend_or_start(
                rendered,
                pending,
                entries,
                roster,
                options,
                PendingToolSummary::opening(
                    author,
                    entry_index,
                    parsed_idx,
                    parent_id,
                    timestamp,
                    summary,
                ),
            );
            true
        }
        Some(UserToolEntry::AnswersOpenRun { parent_id }) => {
            if let Some(run) = pending
                .as_mut()
                .filter(|run| run.absorbs_results_from(parent_id))
            {
                run.absorb(parsed_idx, entry.timestamp());
            }
            true
        }
        None => false,
    }
}

/// Extend the open run with `candidate`, or flush it and let `candidate` open
/// the next one.
fn extend_or_start(
    rendered: &mut RenderedConversation,
    pending: &mut Option<PendingToolSummary>,
    entries: &[RenderableEntry],
    roster: &SubagentRoster,
    options: &RenderOptions,
    candidate: PendingToolSummary,
) {
    match pending {
        Some(run) if run.extends(&candidate) => {
            run.absorb(candidate.last_parsed_idx, candidate.ended_at.as_deref());
            run.summary.merge(candidate.summary);
        }
        _ => {
            flush_tool_summary(rendered, pending, entries, roster, options);
            *pending = Some(candidate);
        }
    }
}

/// Append one parsed entry's rendered lines and, if the entry is a navigable
/// message, a `MessageRange` that excludes any trailing blank line.
fn append_entry_with_range(
    lines: &mut Vec<RenderedLine>,
    messages: &mut Vec<MessageRange>,
    parsed: &RenderableEntry,
    entry_lines: Vec<RenderedLine>,
) {
    let entry_index = parsed.entry_index;
    let is_message = matches!(
        parsed.entry,
        LogEntry::User { .. } | LogEntry::Assistant { .. }
    );

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

/// Push a blank row after `lines` unless they are empty or already end with
/// one. A block without rows then leaves one blank row between its
/// neighbours, and an entry's or a run's rows start with a non-blank row
/// and end with one blank row, because an entry renders again alone,
/// without the rows around it (see `render_entry_in_place`).
fn push_separator(lines: &mut Vec<RenderedLine>) {
    if lines.last().is_some_and(|line| !line.spans.is_empty()) {
        lines.push(RenderedLine::new(vec![]));
    }
}

#[cfg(test)]
mod tests;
