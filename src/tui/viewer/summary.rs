use crate::log_entry::{ContentBlock, LogEntry, Tool, UserContent};

use super::calls::{CallRanges, EntryToolBlock, RenderedToolBlock, ToolBlock, entry_tool_blocks};
use super::connectors::batch_color;
use super::ledger::{LedgerRow, NameCol, push_row, wrap_row};
use super::style::assistant_label;
use super::timing::TimingSlot;
use super::tools::{
    ToolCallRenderSpec, ToolOutputKind, ToolResultRenderSpec, make_tool_output_id,
    render_tool_call, render_tool_result, tool_result_display_text,
};
use super::*;

pub(super) struct PendingToolSummary {
    pub(super) id: ToolOutputId,
    pub(super) first_entry_index: usize,
    pub(super) first_parsed_idx: usize,
    pub(super) last_parsed_idx: usize,
    pub(super) parent_id: Option<String>,
    pub(super) agent: Option<String>,
    /// Raw timestamp of the entry that opened the run; it also fills the
    /// stamp column.
    pub(super) started_at: Option<String>,
    /// Raw timestamp of the last absorbed entry that carried one.
    pub(super) ended_at: Option<String>,
    pub(super) summary: ToolActivitySummary,
}

impl PendingToolSummary {
    /// Extend the run to the entry at `parsed_idx`. An entry without a
    /// timestamp leaves the recorded end where it is, so the run still ends
    /// at the last entry that was stamped.
    pub(super) fn absorb(&mut self, parsed_idx: usize, timestamp: Option<&str>) {
        self.last_parsed_idx = parsed_idx;
        if let Some(timestamp) = timestamp {
            self.ended_at = Some(timestamp.to_string());
        }
    }
}

#[derive(Default)]
pub(super) struct ToolActivitySummary {
    searched_patterns: usize,
    searched_file_patterns: usize,
    read_files: usize,
    shell_commands: usize,
    edited_files: usize,
    wrote_files: usize,
    agents: usize,
    agent_messages: usize,
    waits: usize,
    task_list_updates: usize,
    fetched_urls: usize,
    web_searches: usize,
    other_tools: usize,
}

impl ToolActivitySummary {
    fn add_call(&mut self, tool: Tool) {
        match tool {
            Tool::Shell => self.shell_commands += 1,
            Tool::Read => self.read_files += 1,
            Tool::Grep => self.searched_patterns += 1,
            Tool::Glob => self.searched_file_patterns += 1,
            Tool::Edit => self.edited_files += 1,
            Tool::Write => self.wrote_files += 1,
            Tool::Agent => self.agents += 1,
            Tool::AgentMessage => self.agent_messages += 1,
            Tool::Wait => self.waits += 1,
            Tool::TaskList => self.task_list_updates += 1,
            Tool::WebFetch => self.fetched_urls += 1,
            Tool::WebSearch => self.web_searches += 1,
            Tool::Other => self.other_tools += 1,
        }
    }

    pub(super) fn merge(&mut self, other: Self) {
        self.searched_patterns += other.searched_patterns;
        self.searched_file_patterns += other.searched_file_patterns;
        self.read_files += other.read_files;
        self.shell_commands += other.shell_commands;
        self.edited_files += other.edited_files;
        self.wrote_files += other.wrote_files;
        self.agents += other.agents;
        self.agent_messages += other.agent_messages;
        self.waits += other.waits;
        self.task_list_updates += other.task_list_updates;
        self.fetched_urls += other.fetched_urls;
        self.web_searches += other.web_searches;
        self.other_tools += other.other_tools;
    }

    pub(super) fn is_empty(&self) -> bool {
        self.searched_patterns
            + self.searched_file_patterns
            + self.read_files
            + self.shell_commands
            + self.edited_files
            + self.wrote_files
            + self.agents
            + self.agent_messages
            + self.waits
            + self.task_list_updates
            + self.fetched_urls
            + self.web_searches
            + self.other_tools
            == 0
    }

    pub(super) fn sentence(&self) -> String {
        let mut parts = Vec::new();
        push_summary_item(
            &mut parts,
            self.searched_patterns,
            "Searched for",
            "pattern",
        );
        push_summary_item(
            &mut parts,
            self.searched_file_patterns,
            "Searched for",
            "file pattern",
        );
        push_summary_item(&mut parts, self.read_files, "read", "file");
        push_summary_item(&mut parts, self.shell_commands, "ran", "shell command");
        push_summary_item(&mut parts, self.edited_files, "edited", "file");
        push_summary_item(&mut parts, self.wrote_files, "wrote", "file");
        push_summary_item(&mut parts, self.agents, "started", "agent");
        push_summary_item(&mut parts, self.agent_messages, "messaged", "agent");
        push_summary_item(&mut parts, self.waits, "waited", "time");
        push_summary_item(
            &mut parts,
            self.task_list_updates,
            "updated the task list",
            "time",
        );
        push_summary_item(&mut parts, self.fetched_urls, "fetched", "URL");
        push_summary_item(&mut parts, self.web_searches, "searched", "web");
        push_summary_item(&mut parts, self.other_tools, "called", "tool");
        capitalize_first(parts.join(", "))
    }
}

fn capitalize_first(text: String) -> String {
    let mut chars = text.chars();
    let Some(first) = chars.next() else {
        return text;
    };
    first.to_uppercase().chain(chars).collect()
}

fn push_summary_item(parts: &mut Vec<String>, count: usize, verb: &str, noun: &str) {
    if count == 0 {
        return;
    }
    let suffix = if count == 1 { "" } else { "s" };
    parts.push(format!("{verb} {count} {noun}{suffix}"));
}

/// One run's heading: its summary sentence under the run's label.
pub(super) struct SummaryRowSpec<'a> {
    pub label: &'a str,
    pub label_color: (u8, u8, u8),
    pub dimmed: bool,
    pub timing: TimingSlot<'a>,
    pub text: &'a str,
    pub content_width: usize,
    /// The run's id, on every row of the heading so hover and click cover
    /// the whole of it; `None` where the heading toggles nothing.
    pub tool_output_id: Option<&'a ToolOutputId>,
}

/// Render the run's heading, wrapped at the content width.
pub(super) fn render_tool_activity_summary(
    lines: &mut Vec<RenderedLine>,
    spec: &SummaryRowSpec<'_>,
) {
    let SummaryRowSpec {
        label,
        label_color,
        dimmed,
        timing,
        text,
        content_width,
        tool_output_id,
    } = *spec;
    let continuation = timing.continuation();
    for (i, row) in wrap_row(text, content_width).into_iter().enumerate() {
        let name = if i == 0 {
            NameCol::Label {
                text: label,
                color: label_color,
                bold: false,
                dimmed,
            }
        } else {
            NameCol::BlankPlain
        };
        let content = vec![(
            row,
            LineStyle {
                fg: Some(th().tool_text),
                dimmed: true,
                ..Default::default()
            },
        )];
        push_row(
            lines,
            LedgerRow {
                timing: if i == 0 { timing } else { continuation },
                name,
                separator_dimmed: dimmed,
                tool_output_id,
                clickable: tool_output_id.is_some(),
            },
            content,
        );
    }
}

pub(super) fn summarize_tool_calls(blocks: &[ContentBlock]) -> ToolActivitySummary {
    let mut summary = ToolActivitySummary::default();
    for block in blocks {
        if let ContentBlock::ToolUse { tool, .. } = block {
            summary.add_call(*tool);
        }
    }
    summary
}

fn assistant_blocks_are_tool_only(blocks: &[ContentBlock], show_thinking: bool) -> bool {
    blocks.iter().all(|block| {
        matches!(block, ContentBlock::ToolUse { .. })
            || (!show_thinking && matches!(block, ContentBlock::Thinking { .. }))
    })
}

pub(super) fn tool_only_assistant_summary<'a>(
    entry: &'a LogEntry,
    options: &RenderOptions,
) -> Option<(
    Option<&'a str>,
    Option<&'a str>,
    Option<&'a str>,
    ToolActivitySummary,
)> {
    let LogEntry::Assistant {
        message,
        agent,
        timestamp,
        parent_tool_use_id,
        ..
    } = entry
    else {
        return None;
    };

    if parent_tool_use_id.is_some() && !options.show_thinking {
        return None;
    }
    if message.content.is_empty()
        || !assistant_blocks_are_tool_only(&message.content, options.show_thinking)
    {
        return None;
    }

    let summary = summarize_tool_calls(&message.content);
    (!summary.is_empty()).then_some((
        parent_tool_use_id.as_deref(),
        agent.as_deref(),
        timestamp.as_deref(),
        summary,
    ))
}

pub(super) fn user_entry_is_only_tool_results(entry: &LogEntry, options: &RenderOptions) -> bool {
    let LogEntry::User {
        message,
        parent_tool_use_id,
        ..
    } = entry
    else {
        return false;
    };

    if parent_tool_use_id.is_some() && !options.show_thinking {
        return false;
    }

    let UserContent::Blocks(blocks) = &message.content else {
        return false;
    };
    !blocks.is_empty()
        && blocks
            .iter()
            .all(|block| matches!(block, ContentBlock::ToolResult { .. }))
}

fn render_summary_group_details(
    lines: &mut Vec<RenderedLine>,
    calls: &mut Vec<CallRange>,
    entries: &[RenderableEntry],
    pending: &PendingToolSummary,
    options: &RenderOptions,
) {
    let mut rendered_any = false;
    let mut call_ranges =
        CallRanges::new(run_tool_blocks(entries, pending).map(|entry_block| entry_block.block));
    let pad_timing = TimingSlot::from_show_timing(options.show_timing);
    let parent_id = pending.parent_id.as_deref();
    let label = assistant_label(parent_id, pending.agent.as_deref());
    for EntryToolBlock {
        parsed,
        block_index,
        block,
    } in run_tool_blocks(entries, pending)
    {
        if rendered_any {
            lines.push(RenderedLine::new(vec![]));
        }
        rendered_any = true;
        let location = BlockLocation {
            entry_index: parsed.entry_index,
            block_index,
        };
        let start_line = lines.len();
        match block {
            ToolBlock::Call {
                id,
                name,
                tool,
                input,
            } => {
                let output_id = make_tool_output_id(
                    parsed.entry_index,
                    parent_id,
                    block_index,
                    ToolOutputKind::ToolCall,
                    Some(id),
                );
                let expanded = options.expanded_tool_outputs.contains(&output_id);
                render_tool_call(
                    lines,
                    &ToolCallRenderSpec {
                        name,
                        tool,
                        input,
                        label: &label,
                        label_color: th().accent_dim,
                        dimmed: true,
                        tool_word_color: batch_color(call_ranges.batch_position(id)),
                        content_width: options.content_width,
                        timing: pad_timing,
                        tool_display: ToolDisplayMode::Truncated,
                        tool_output_id: &output_id,
                        expanded,
                    },
                );
                call_ranges.record(RenderedToolBlock {
                    kind: ToolOutputKind::ToolCall,
                    tool_use_id: id,
                    area: CallArea {
                        id: output_id,
                        location,
                        start_line,
                        end_line: lines.len(),
                    },
                });
            }
            ToolBlock::Result {
                tool_use_id,
                content,
            } => {
                let output_id = make_tool_output_id(
                    parsed.entry_index,
                    parent_id,
                    block_index,
                    ToolOutputKind::ToolResult,
                    Some(tool_use_id),
                );
                let expanded = options.expanded_tool_outputs.contains(&output_id);
                let content_str = tool_result_display_text(content);
                render_tool_result(
                    lines,
                    &ToolResultRenderSpec {
                        text: &content_str,
                        content_width: options.content_width,
                        timing: pad_timing,
                        tool_display: ToolDisplayMode::Truncated,
                        tool_output_id: &output_id,
                        expanded,
                    },
                );
                call_ranges.record(RenderedToolBlock {
                    kind: ToolOutputKind::ToolResult,
                    tool_use_id,
                    area: CallArea {
                        id: output_id,
                        location,
                        start_line,
                        end_line: lines.len(),
                    },
                });
            }
        }
    }
    calls.extend(call_ranges.into_calls());
}

/// The tool calls and results of the run in file order: the blocks of every
/// entry the run absorbed that shares its parent, other blocks skipped.
fn run_tool_blocks<'a>(
    entries: &'a [RenderableEntry],
    pending: &'a PendingToolSummary,
) -> impl Iterator<Item = EntryToolBlock<'a>> + 'a {
    entry_tool_blocks(
        &entries[pending.first_parsed_idx..=pending.last_parsed_idx],
        pending.parent_id.as_deref(),
    )
}

/// How long a run took, from the entry that opened it to the last absorbed
/// entry that carried a timestamp. `None` when either end is missing or is
/// not an RFC 3339 timestamp, or when the end precedes the start.
fn format_run_duration(started_at: Option<&str>, ended_at: Option<&str>) -> Option<String> {
    use chrono::DateTime;

    let started = DateTime::parse_from_rfc3339(started_at?).ok()?;
    let ended = DateTime::parse_from_rfc3339(ended_at?).ok()?;
    let seconds = ended.signed_duration_since(started).num_seconds();
    (seconds >= 0).then(|| format_coarse_duration(seconds as u64))
}

/// Whole units, truncated: `1h 5m` from an hour, `2m` from a minute, `40s`
/// below that.
fn format_coarse_duration(seconds: u64) -> String {
    let minutes = seconds / 60;
    if minutes >= 60 {
        return format!("{}h {}m", minutes / 60, minutes % 60);
    }
    if minutes >= 1 {
        return format!("{minutes}m");
    }
    format!("{seconds}s")
}

/// What the run did, how long it took, and whether its calls are listed
/// below it.
fn summary_row_text(pending: &PendingToolSummary, expanded: bool, show_timing: bool) -> String {
    let mut text = pending.summary.sentence();
    if show_timing
        && let Some(duration) =
            format_run_duration(pending.started_at.as_deref(), pending.ended_at.as_deref())
    {
        text.push_str(" · ");
        text.push_str(&duration);
    }
    if expanded {
        text.push_str(" (expanded):");
    }
    text
}

pub(super) fn flush_tool_summary(
    lines: &mut Vec<RenderedLine>,
    messages: &mut Vec<MessageRange>,
    calls: &mut Vec<CallRange>,
    pending: &mut Option<PendingToolSummary>,
    entries: &[RenderableEntry],
    options: &RenderOptions,
) {
    let Some(pending) = pending.take() else {
        return;
    };

    let start_line = lines.len();
    let label = assistant_label(pending.parent_id.as_deref(), pending.agent.as_deref());
    let ts = if options.show_timing {
        pending.started_at.as_deref().and_then(format_timestamp)
    } else {
        None
    };
    let timing = match ts.as_deref() {
        Some(ts) => TimingSlot::Stamp(ts),
        None => TimingSlot::Disabled,
    };
    // The heading's rows are the only rows carrying the run's id, collapsed
    // or expanded, so hovering it highlights the heading alone and clicking
    // it toggles the run; the detail rows keep their own ids for their own
    // toggles.
    let expanded = options.expanded_tool_outputs.contains(&pending.id);
    render_tool_activity_summary(
        lines,
        &SummaryRowSpec {
            label: &label,
            label_color: th().accent_dim,
            dimmed: pending.parent_id.is_some(),
            timing,
            text: &summary_row_text(&pending, expanded, options.show_timing),
            content_width: options.content_width,
            tool_output_id: Some(&pending.id),
        },
    );
    if expanded {
        render_summary_group_details(lines, calls, entries, &pending, options);
    }

    let end_line = lines.len();
    if end_line > start_line {
        messages.push(MessageRange {
            entry_index: pending.first_entry_index,
            start_line,
            end_line,
        });
        lines.push(RenderedLine::new(vec![]));
    }
}

#[cfg(test)]
mod tests {
    use super::{format_coarse_duration, format_run_duration};

    #[test]
    fn durations_read_in_whole_truncated_units() {
        assert_eq!(format_coarse_duration(0), "0s");
        assert_eq!(format_coarse_duration(59), "59s");
        assert_eq!(format_coarse_duration(60), "1m");
        assert_eq!(format_coarse_duration(119), "1m");
        assert_eq!(format_coarse_duration(3600), "1h 0m");
        assert_eq!(format_coarse_duration(3900), "1h 5m");
    }

    #[test]
    fn a_run_ending_before_it_starts_has_no_duration() {
        assert_eq!(
            format_run_duration(Some("2026-02-04T12:32:10Z"), Some("2026-02-04T12:30:00Z")),
            None
        );
    }

    #[test]
    fn a_run_missing_or_unparsable_timestamps_has_no_duration() {
        let stamp = "2026-02-04T12:30:00Z";
        assert_eq!(format_run_duration(None, Some(stamp)), None);
        assert_eq!(format_run_duration(Some(stamp), None), None);
        assert_eq!(format_run_duration(Some("yesterday"), Some(stamp)), None);
    }
}
