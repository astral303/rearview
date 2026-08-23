use crate::claude::{ContentBlock, LogEntry, Tool, UserContent};

use super::ledger::{LedgerRow, NameCol, push_row};
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
    pub(super) timestamp: Option<String>,
    pub(super) summary: ToolActivitySummary,
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

    pub(super) fn expanded_heading(&self) -> String {
        format!("{} (expanded):", self.sentence())
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

pub(super) fn render_tool_activity_summary(
    lines: &mut Vec<RenderedLine>,
    label: &str,
    label_color: (u8, u8, u8),
    dimmed: bool,
    timing: TimingSlot<'_>,
    row_text: String,
    tool_output_id: Option<&ToolOutputId>,
) {
    let content = vec![(
        row_text,
        LineStyle {
            fg: Some(th().tool_text),
            dimmed: true,
            ..Default::default()
        },
    )];
    push_row(
        lines,
        LedgerRow {
            timing,
            name: NameCol::Label {
                text: label,
                color: label_color,
                bold: false,
                dimmed,
            },
            separator_dimmed: dimmed,
            tool_output_id,
            clickable: tool_output_id.is_some(),
        },
        content,
    );
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
    entries: &[RenderableEntry],
    pending: &PendingToolSummary,
    options: &RenderOptions,
) {
    let mut rendered_any = false;
    let pad_timing = TimingSlot::from_show_timing(options.show_timing);
    let label = assistant_label(pending.parent_id.as_deref(), pending.agent.as_deref());
    for parsed in &entries[pending.first_parsed_idx..=pending.last_parsed_idx] {
        match &parsed.entry {
            LogEntry::Assistant {
                message,
                parent_tool_use_id,
                ..
            } if parent_tool_use_id.as_deref() == pending.parent_id.as_deref() => {
                for (block_idx, block) in message.content.iter().enumerate() {
                    if let ContentBlock::ToolUse {
                        id,
                        name,
                        tool,
                        input,
                    } = block
                    {
                        if rendered_any {
                            lines.push(RenderedLine::new(vec![]));
                        }
                        let output_id = make_tool_output_id(
                            parsed.entry_index,
                            parent_tool_use_id.as_deref(),
                            block_idx,
                            ToolOutputKind::ToolCall,
                            Some(id),
                        );
                        let expanded = options.expanded_tool_outputs.contains(&output_id);
                        render_tool_call(
                            lines,
                            &ToolCallRenderSpec {
                                name,
                                tool: *tool,
                                input,
                                label: &label,
                                label_color: th().accent_dim,
                                dimmed: true,
                                content_width: options.content_width,
                                timing: pad_timing,
                                tool_display: ToolDisplayMode::Truncated,
                                tool_output_id: &output_id,
                                expanded,
                            },
                        );
                        rendered_any = true;
                    }
                }
            }
            LogEntry::User {
                message,
                parent_tool_use_id,
                ..
            } if parent_tool_use_id.as_deref() == pending.parent_id.as_deref() => {
                let UserContent::Blocks(blocks) = &message.content else {
                    continue;
                };
                for (block_idx, block) in blocks.iter().enumerate() {
                    if let ContentBlock::ToolResult {
                        content,
                        tool_use_id,
                        ..
                    } = block
                    {
                        if rendered_any {
                            lines.push(RenderedLine::new(vec![]));
                        }
                        let output_id = make_tool_output_id(
                            parsed.entry_index,
                            parent_tool_use_id.as_deref(),
                            block_idx,
                            ToolOutputKind::ToolResult,
                            Some(tool_use_id),
                        );
                        let expanded = options.expanded_tool_outputs.contains(&output_id);
                        let content_str = tool_result_display_text(content.as_ref());
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
                        rendered_any = true;
                    }
                }
            }
            _ => {}
        }
    }
}

pub(super) fn flush_tool_summary(
    lines: &mut Vec<RenderedLine>,
    messages: &mut Vec<MessageRange>,
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
        pending.timestamp.as_deref().and_then(format_timestamp)
    } else {
        None
    };
    let timing = match ts.as_deref() {
        Some(ts) => TimingSlot::Stamp(ts),
        None => TimingSlot::Disabled,
    };
    // The summary row is the only row carrying the run's id, folded or
    // expanded, so hovering it highlights one row and clicking it toggles the
    // run; the detail rows keep their own ids for their own toggles.
    let expanded = options.expanded_tool_outputs.contains(&pending.id);
    let row_text = if expanded {
        pending.summary.expanded_heading()
    } else {
        pending.summary.sentence()
    };
    render_tool_activity_summary(
        lines,
        &label,
        th().accent_dim,
        pending.parent_id.is_some(),
        timing,
        row_text,
        Some(&pending.id),
    );
    if expanded {
        render_summary_group_details(lines, entries, &pending, options);
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
