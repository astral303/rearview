use std::borrow::Cow;

use crate::history::{TASK_LABEL, TaskReport, user_task_report};
use crate::log_entry::{ContentBlock, LogEntry, UserContent};
use crate::tui::theme::Rgb;

use super::RenderedLine;

use super::calls::{CallRanges, RenderedToolBlock};
use super::commands::process_command_message;
use super::connectors::lane_color;
use super::ledger::{render_ledger_block_styled, render_ledger_block_styled_dimmed};
use super::markdown::{apply_thinking_style, render_markdown_to_lines};
use super::style::{USER_LABEL, subagent_label};
use super::summary::{SummaryRowSpec, render_tool_activity_summary, summarize_tool_activity};
use super::timing::{RowTiming, TimingSlot};
use super::tools::{
    SubagentReplyRenderSpec, TaskReportRenderSpec, ToolCallRenderSpec, ToolOutputKind,
    ToolResultRenderSpec, make_reply_output_id, make_tool_output_id, render_subagent_reply,
    render_task_report, render_tool_call, render_tool_result, tool_result_display_text,
};
use super::*;

/// Classify a log entry and dispatch to the matching focused render
/// function. This is the only entry point used by the rest of the
/// viewer; per-kind rendering lives in `render_user_message` and
/// `render_assistant_message`.
///
/// Returns the entry's tool calls and results with their rows, counted from
/// the entry's first row, for the connectors to join; `call_ranges` holds
/// the batch position of each call.
pub(super) fn render_entry<'a>(
    lines: &mut Vec<RenderedLine>,
    entry_index: usize,
    entry: &'a LogEntry,
    options: &RenderOptions,
    call_ranges: &CallRanges<'_>,
) -> Vec<RenderedToolBlock<'a>> {
    match entry {
        LogEntry::Summary { .. }
        | LogEntry::FileHistorySnapshot { .. }
        | LogEntry::System { .. }
        | LogEntry::CustomTitle { .. }
        | LogEntry::AiTitle { .. }
        | LogEntry::AgentName { .. }
        | LogEntry::PermissionMode { .. }
        | LogEntry::Progress { .. }
        | LogEntry::PiMetadata {
            searchable: false, ..
        }
        | LogEntry::Unknown => Vec::new(),
        LogEntry::PiMetadata {
            label,
            text,
            timestamp,
            searchable: true,
            ..
        } => {
            let content = UserContent::String(text.clone());
            let ctx = EntryCtx {
                style: MessageStyle::for_pi_metadata(label),
                parent_id: None,
                entry_index,
                options,
                call_ranges,
            };
            let ts = entry_timestamp(options, timestamp.as_deref());
            // Text alone: the message renders no tool block.
            render_user_message(
                lines,
                &ctx,
                RowTiming::new(options.show_timing, ts.as_deref()),
                &content,
            );
            Vec::new()
        }
        LogEntry::User {
            message,
            timestamp,
            parent_tool_use_id,
            ..
        } => {
            if parent_tool_use_id.is_some() && !options.show_thinking {
                return Vec::new();
            }
            let parent_id = parent_tool_use_id.as_deref();
            let style = MessageStyle::for_user(parent_id, &message.content);
            let ctx = EntryCtx {
                style,
                parent_id,
                entry_index,
                options,
                call_ranges,
            };
            let ts = entry_timestamp(options, timestamp.as_deref());
            let timing = RowTiming::new(options.show_timing, ts.as_deref());
            let tool_blocks = render_user_message(lines, &ctx, timing, &message.content);
            joinable_blocks(&ctx, tool_blocks)
        }
        LogEntry::Assistant {
            message,
            agent,
            timestamp,
            parent_tool_use_id,
            ..
        } => {
            if parent_tool_use_id.is_some() && !options.show_thinking {
                return Vec::new();
            }
            let parent_id = parent_tool_use_id.as_deref();
            let style = MessageStyle::for_assistant(parent_id, agent.as_deref());
            let ctx = EntryCtx {
                style,
                parent_id,
                entry_index,
                options,
                call_ranges,
            };
            let ts = entry_timestamp(options, timestamp.as_deref());
            let timing = RowTiming::new(options.show_timing, ts.as_deref());
            let tool_blocks = render_assistant_message(lines, &ctx, timing, &message.content);
            joinable_blocks(&ctx, tool_blocks)
        }
    }
}

/// The blocks a connector may join. A sub-agent's message contributes none:
/// the detail modes lay out lanes for top-level calls alone.
fn joinable_blocks<'a>(
    ctx: &EntryCtx<'_>,
    tool_blocks: Vec<RenderedToolBlock<'a>>,
) -> Vec<RenderedToolBlock<'a>> {
    if ctx.style.is_subagent {
        Vec::new()
    } else {
        tool_blocks
    }
}

fn entry_timestamp(options: &RenderOptions, raw: Option<&str>) -> Option<String> {
    if options.show_timing {
        raw.and_then(format_timestamp)
    } else {
        None
    }
}

/// The label, colours and nesting of one message: top-level or sub-agent
/// user or assistant, or Pi metadata.
struct MessageStyle<'a> {
    label: Cow<'a, str>,
    label_color: Rgb,
    /// The colour of the label beside this message's tool calls. An agent's
    /// call rows are dimmer than its own label; a user's match theirs.
    call_label_color: Rgb,
    /// Whether the label and content render dimmed (subagent / skill).
    dimmed: bool,
    /// Whether the first text-block label is bold.
    bold: bool,
    /// True when the message renders as a sub-agent's: it dims its tool
    /// results, skips thinking blocks, truncates its replies, and keeps its
    /// tool blocks out of the connectors. Distinct from `dimmed` because
    /// skill-mode user messages are dimmed but not nested.
    is_subagent: bool,
    /// The background task's report the user message holds, parsed once
    /// here: a top-level one renders as the `Task` row, a sub-agent's as its
    /// dimmed text.
    task_report: Option<TaskReport>,
}

impl<'a> MessageStyle<'a> {
    fn for_user(parent_id: Option<&'a str>, content: &UserContent) -> Self {
        let task_report = user_task_report(content);
        if let Some(p) = parent_id {
            return Self {
                label: Cow::Owned(subagent_label(p)),
                label_color: th().text_primary,
                call_label_color: th().text_primary,
                dimmed: true,
                bold: false,
                is_subagent: true,
                task_report,
            };
        }
        if task_report.is_some() {
            return Self {
                label: Cow::Borrowed(TASK_LABEL),
                label_color: th().text_primary,
                call_label_color: th().text_primary,
                dimmed: false,
                bold: true,
                is_subagent: false,
                task_report,
            };
        }
        let is_skill = match content {
            UserContent::String(s) => s.trim().starts_with("Base directory for this skill:"),
            UserContent::Blocks(blocks) => blocks.iter().any(|block| {
                matches!(block, ContentBlock::Text { text }
                    if text.trim().starts_with("Base directory for this skill:"))
            }),
        };
        Self {
            label: Cow::Borrowed(USER_LABEL),
            label_color: th().text_primary,
            call_label_color: th().text_primary,
            dimmed: is_skill,
            bold: !is_skill,
            is_subagent: false,
            task_report: None,
        }
    }

    fn for_assistant(parent_id: Option<&'a str>, agent: Option<&'a str>) -> Self {
        match parent_id {
            Some(p) => Self {
                label: Cow::Owned(subagent_label(p)),
                label_color: th().accent,
                call_label_color: th().accent_dim,
                dimmed: true,
                bold: false,
                is_subagent: true,
                task_report: None,
            },
            None => Self {
                label: Cow::Borrowed(agent.unwrap_or("Claude")),
                label_color: th().accent,
                call_label_color: th().accent_dim,
                dimmed: false,
                bold: true,
                is_subagent: false,
                task_report: None,
            },
        }
    }

    fn for_pi_metadata(label: &'a str) -> Self {
        Self {
            label: Cow::Borrowed(label),
            label_color: th().text_secondary,
            call_label_color: th().accent_dim,
            dimmed: true,
            bold: false,
            is_subagent: false,
            task_report: None,
        }
    }
}

/// Per-entry rendering context: style + identity + immutable options.
/// Held by reference so the focused render functions read it without
/// taking ownership of the style and so the per-row timing cursor can
/// be mutated independently.
struct EntryCtx<'a> {
    style: MessageStyle<'a>,
    parent_id: Option<&'a str>,
    entry_index: usize,
    options: &'a RenderOptions,
    /// The detail modes' call ranges, read for each call's batch position;
    /// empty in summary mode, where each expanded run keeps its own.
    call_ranges: &'a CallRanges<'a>,
}

// ---------------------------------------------------------------------
// Focused per-kind render functions.
//
// These replace the previous `MessageRenderer` orchestrator. Each one
// owns the block-pipeline template for one entry kind and pushes a
// trailing blank only when something rendered. The assistant template
// ordering (text → tool summary → tool calls → thinking) is the
// documented compatibility contract — it does not match raw JSON block
// order.
// ---------------------------------------------------------------------

/// User template: text, then the calls the user made themselves, then tool
/// results. A user call keeps the entry's own label colour, so the row reads
/// as the user's rather than as the agent's.
fn render_user_message<'a>(
    lines: &mut Vec<RenderedLine>,
    ctx: &EntryCtx<'_>,
    mut timing: RowTiming<'_>,
    content: &'a UserContent,
) -> Vec<RenderedToolBlock<'a>> {
    let mut tool_blocks = Vec::new();
    let mut printed = step_user_text(lines, ctx, &mut timing, content);
    if ctx.options.tool_display.shows_details()
        && let UserContent::Blocks(blocks) = content
    {
        step_tool_calls(lines, ctx, &mut timing, blocks, &mut tool_blocks);
        step_user_tool_results(lines, ctx, &mut timing, blocks, &mut tool_blocks);
        printed |= !tool_blocks.is_empty();
    }
    if printed {
        lines.push(RenderedLine::new(vec![]));
    }
    tool_blocks
}

/// Assistant template: text → tool summary → tool calls → thinking.
/// Order is intentional and matches pre-refactor behavior, not the
/// raw JSON block order.
fn render_assistant_message<'a>(
    lines: &mut Vec<RenderedLine>,
    ctx: &EntryCtx<'_>,
    mut timing: RowTiming<'_>,
    blocks: &'a [ContentBlock],
) -> Vec<RenderedToolBlock<'a>> {
    let mut tool_blocks = Vec::new();
    let mut printed = step_assistant_text(lines, ctx, &mut timing, blocks);
    printed |= step_tool_summary(lines, ctx, &mut timing, blocks);
    if ctx.options.tool_display.shows_details() {
        step_tool_calls(lines, ctx, &mut timing, blocks, &mut tool_blocks);
        printed |= !tool_blocks.is_empty();
    }
    printed |= step_thinking(lines, ctx, &mut timing, blocks);
    if printed {
        lines.push(RenderedLine::new(vec![]));
    }
    tool_blocks
}

// ---- block-pipeline steps ----------------------------------------------

fn step_user_text(
    lines: &mut Vec<RenderedLine>,
    ctx: &EntryCtx<'_>,
    timing: &mut RowTiming<'_>,
    content: &UserContent,
) -> bool {
    if let Some(report) = &ctx.style.task_report
        && !ctx.style.dimmed
    {
        let output_id = make_tool_output_id(
            ctx.entry_index,
            ctx.parent_id,
            0,
            ToolOutputKind::ToolResult,
            Some("task-report"),
        );
        render_task_report(
            lines,
            &TaskReportRenderSpec {
                report,
                label: &ctx.style.label,
                label_color: ctx.style.label_color,
                content_width: ctx.options.content_width,
                timing: timing.take_once(),
                tool_display: ctx.options.tool_display,
                tool_output_id: &output_id,
                expanded: ctx.options.expanded_tool_outputs.contains(&output_id),
                can_expand: ctx.options.can_expand,
            },
        );
        return true;
    }
    let text = match (&ctx.style.task_report, content) {
        (Some(report), _) => Some(report.display_text()),
        (None, UserContent::String(s)) => process_command_message(s),
        (None, UserContent::Blocks(blocks)) => {
            let texts: Vec<String> = blocks
                .iter()
                .filter_map(|block| {
                    if let ContentBlock::Text { text } = block {
                        process_command_message(text)
                    } else {
                        None
                    }
                })
                .collect();
            if texts.is_empty() {
                None
            } else {
                Some(texts.join("\n\n"))
            }
        }
    };
    let Some(text) = text else { return false };
    let md_lines = render_markdown_to_lines(&text, ctx.options.content_width);
    if ctx.style.dimmed {
        render_ledger_block_styled_dimmed(
            lines,
            &ctx.style.label,
            ctx.style.label_color,
            md_lines,
            timing.pad(),
            None,
        );
    } else {
        render_ledger_block_styled(
            lines,
            &ctx.style.label,
            ctx.style.label_color,
            ctx.style.bold,
            md_lines,
            timing.take_once(),
        );
    }
    // Top-level slot is now spent regardless of the branch taken.
    let _ = timing.take_once();
    true
}

fn step_assistant_text(
    lines: &mut Vec<RenderedLine>,
    ctx: &EntryCtx<'_>,
    timing: &mut RowTiming<'_>,
    blocks: &[ContentBlock],
) -> bool {
    let mut printed = false;
    for (block_index, block) in blocks.iter().enumerate() {
        let ContentBlock::Text { text } = block else {
            continue;
        };
        if text.trim().is_empty() {
            continue;
        }
        if ctx.style.is_subagent {
            let output_id = make_reply_output_id(ctx.entry_index, ctx.parent_id, block_index);
            render_subagent_reply(
                lines,
                &SubagentReplyRenderSpec {
                    text,
                    label: &ctx.style.label,
                    label_color: ctx.style.label_color,
                    content_width: ctx.options.content_width,
                    timing: timing.pad(),
                    tool_display: ctx.options.tool_display,
                    expanded: ctx.options.expanded_tool_outputs.contains(&output_id),
                    tool_output_id: &output_id,
                    can_expand: ctx.options.can_expand,
                },
            );
            let _ = timing.take_once();
        } else {
            render_ledger_block_styled(
                lines,
                &ctx.style.label,
                ctx.style.label_color,
                ctx.style.bold,
                render_markdown_to_lines(text, ctx.options.content_width),
                timing.take_once(),
            );
        }
        printed = true;
    }
    printed
}

fn step_tool_summary(
    lines: &mut Vec<RenderedLine>,
    ctx: &EntryCtx<'_>,
    timing: &mut RowTiming<'_>,
    blocks: &[ContentBlock],
) -> bool {
    if !ctx.options.tool_display.is_summary() {
        return false;
    }
    let summary = summarize_tool_activity(blocks);
    if summary.is_empty() {
        return false;
    }
    render_tool_activity_summary(
        lines,
        &SummaryRowSpec {
            label: &ctx.style.label,
            label_color: th().accent_dim,
            dimmed: ctx.style.is_subagent,
            timing: timing.consume(),
            text: &summary.sentence(),
            content_width: ctx.options.content_width,
            tool_output_id: None,
        },
    );
    true
}

/// Renders every call of `blocks`, appending each with its rows to
/// `tool_blocks`. A blank row separates it from whatever the entry rendered
/// before it, as an expanded run separates its blocks, so each connector has
/// a row for its `↓`.
fn step_tool_calls<'a>(
    lines: &mut Vec<RenderedLine>,
    ctx: &EntryCtx<'_>,
    timing: &mut RowTiming<'_>,
    blocks: &'a [ContentBlock],
    tool_blocks: &mut Vec<RenderedToolBlock<'a>>,
) {
    for (block_index, block) in blocks.iter().enumerate() {
        let ContentBlock::ToolUse {
            id,
            name,
            tool,
            input,
        } = block
        else {
            continue;
        };
        if !tool_blocks.is_empty() {
            lines.push(RenderedLine::new(vec![]));
        }
        let output_id = make_tool_output_id(
            ctx.entry_index,
            ctx.parent_id,
            block_index,
            ToolOutputKind::ToolCall,
            Some(id),
        );
        let expanded = ctx.options.expanded_tool_outputs.contains(&output_id);
        let row_timing = if ctx.style.is_subagent {
            timing.pad()
        } else {
            timing.consume()
        };
        let first_row = lines.len();
        render_tool_call(
            lines,
            &ToolCallRenderSpec {
                name,
                tool: *tool,
                input,
                label: &ctx.style.label,
                label_color: ctx.style.call_label_color,
                dimmed: ctx.style.dimmed,
                tool_word_color: lane_color(ctx.call_ranges.lane(id)),
                content_width: ctx.options.content_width,
                timing: row_timing,
                tool_display: ctx.options.tool_display,
                tool_output_id: &output_id,
                expanded,
            },
        );
        tool_blocks.push(RenderedToolBlock {
            kind: ToolOutputKind::ToolCall,
            tool_use_id: id,
            area: CallArea {
                id: output_id,
                location: BlockLocation {
                    entry_index: ctx.entry_index,
                    block_index,
                },
                start_line: first_row,
                end_line: lines.len(),
            },
        });
    }
}

fn step_thinking(
    lines: &mut Vec<RenderedLine>,
    ctx: &EntryCtx<'_>,
    timing: &mut RowTiming<'_>,
    blocks: &[ContentBlock],
) -> bool {
    if !ctx.options.show_thinking || ctx.style.is_subagent {
        return false;
    }
    let mut printed = false;
    for block in blocks {
        let ContentBlock::Thinking { thinking, .. } = block else {
            continue;
        };
        if thinking.is_empty() {
            continue;
        }
        let md_lines = render_markdown_to_lines(thinking, ctx.options.content_width);
        let styled_lines = apply_thinking_style(md_lines);
        render_ledger_block_styled(
            lines,
            "Thinking",
            th().accent_dim,
            false,
            styled_lines,
            timing.consume(),
        );
        printed = true;
    }
    printed
}

/// Renders every result of `blocks`, appending each with its rows to
/// `tool_blocks`, separated as [`step_tool_calls`] separates its own.
fn step_user_tool_results<'a>(
    lines: &mut Vec<RenderedLine>,
    ctx: &EntryCtx<'_>,
    timing: &mut RowTiming<'_>,
    blocks: &'a [ContentBlock],
    tool_blocks: &mut Vec<RenderedToolBlock<'a>>,
) {
    for row in collect_tool_result_rows(ctx, blocks) {
        if !tool_blocks.is_empty() {
            lines.push(RenderedLine::new(vec![]));
        }
        let row_timing = if ctx.style.is_subagent {
            timing.pad()
        } else {
            timing.consume()
        };
        let first_row = lines.len();
        render_tool_result_row(lines, ctx, &row, row_timing);
        tool_blocks.push(RenderedToolBlock {
            kind: ToolOutputKind::ToolResult,
            tool_use_id: row.tool_use_id,
            area: CallArea {
                id: row.output_id,
                location: BlockLocation {
                    entry_index: ctx.entry_index,
                    block_index: row.block_index,
                },
                start_line: first_row,
                end_line: lines.len(),
            },
        });
    }
}

struct ToolResultRenderRow<'a> {
    tool_use_id: &'a str,
    /// The tool a standalone result names for itself.
    standalone_tool_name: Option<&'a str>,
    block_index: usize,
    output_id: ToolOutputId,
    expanded: bool,
    content: String,
}

fn collect_tool_result_rows<'a>(
    ctx: &EntryCtx<'_>,
    blocks: &'a [ContentBlock],
) -> Vec<ToolResultRenderRow<'a>> {
    let mut rows = Vec::new();
    for (block_index, block) in blocks.iter().enumerate() {
        let ContentBlock::ToolResult {
            content,
            tool_use_id,
            standalone_tool_name,
        } = block
        else {
            continue;
        };
        let output_id = make_tool_output_id(
            ctx.entry_index,
            ctx.parent_id,
            block_index,
            ToolOutputKind::ToolResult,
            Some(tool_use_id),
        );
        rows.push(ToolResultRenderRow {
            tool_use_id,
            standalone_tool_name: standalone_tool_name.as_deref(),
            block_index,
            expanded: ctx.options.expanded_tool_outputs.contains(&output_id),
            output_id,
            content: tool_result_display_text(content.as_ref()),
        });
    }
    rows
}

fn render_tool_result_row(
    lines: &mut Vec<RenderedLine>,
    ctx: &EntryCtx<'_>,
    row: &ToolResultRenderRow<'_>,
    timing: TimingSlot<'_>,
) {
    render_tool_result(
        lines,
        &ToolResultRenderSpec {
            text: &row.content,
            standalone_tool_name: row.standalone_tool_name,
            dimmed: ctx.style.is_subagent,
            content_width: ctx.options.content_width,
            timing,
            tool_display: ctx.options.tool_display,
            tool_output_id: &row.output_id,
            expanded: row.expanded,
        },
    );
}
