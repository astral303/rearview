use crate::log_entry::Tool;
use crate::tool_format::{self, DiffSide, ToolBodyKind};
use crate::tui::theme::Rgb;

use super::ledger::{
    LedgerRow, NameCol, push_row, render_continuation_dimmed, render_ledger_block_plain_dimmed,
    render_truncation_indicator,
};
use super::markdown::render_markdown_to_lines;
use super::timing::TimingSlot;
use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ToolOutputKind {
    ToolCall,
    ToolResult,
}

impl ToolOutputKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::ToolCall => "call",
            Self::ToolResult => "result",
        }
    }
}
pub(super) fn make_tool_output_id(
    entry_index: usize,
    parent_id: Option<&str>,
    block_index: usize,
    kind: ToolOutputKind,
    raw_id: Option<&str>,
) -> ToolOutputId {
    let parent = parent_id.unwrap_or("top");
    let raw = raw_id.unwrap_or("none");
    ToolOutputId(format!(
        "entry:{entry_index}:parent:{parent}:block:{block_index}:kind:{}:id:{raw}",
        kind.as_str()
    ))
}

pub(super) fn make_tool_summary_output_id(
    entry_index: usize,
    parent_id: Option<&str>,
) -> ToolOutputId {
    let parent = parent_id.unwrap_or("top");
    ToolOutputId(format!("entry:{entry_index}:parent:{parent}:kind:summary"))
}
/// Extract text content from tool result for markdown rendering.
/// Returns Some(text) if content is a string or array of text blocks.
/// Returns None for JSON structures that should be pretty-printed instead.
pub(super) fn extract_tool_result_text(content: Option<&serde_json::Value>) -> Option<String> {
    match content {
        Some(serde_json::Value::String(s)) => Some(s.clone()),
        Some(serde_json::Value::Array(arr)) => {
            // Handle array of content blocks (e.g., [{type: "text", text: "..."}])
            let texts: Vec<&str> = arr
                .iter()
                .filter_map(|item| item.get("text").and_then(|t| t.as_str()))
                .collect();
            if !texts.is_empty() {
                Some(texts.join("\n\n"))
            } else {
                None // Array without text blocks - render as JSON
            }
        }
        _ => None, // Objects, null, etc. - render as JSON
    }
}

/// Format tool result content to a string for display (non-text content)
pub(super) fn format_tool_result_content(content: Option<&serde_json::Value>) -> String {
    match content {
        Some(value) => {
            if let Ok(formatted) = serde_json::to_string_pretty(value) {
                formatted
            } else {
                "<invalid content>".to_string()
            }
        }
        None => "<no content>".to_string(),
    }
}

/// Pick the display text for a tool result: prefer extracted text content,
/// fall back to a JSON pretty-print for objects, null, or text-less arrays.
pub(super) fn tool_result_display_text(content: Option<&serde_json::Value>) -> String {
    extract_tool_result_text(content).unwrap_or_else(|| format_tool_result_content(content))
}

/// Descriptor for one tool-call rendering pass shared by entry and
/// summary-expansion render paths.
pub(super) struct ToolCallRenderSpec<'a> {
    pub name: &'a str,
    pub tool: Tool,
    pub input: &'a serde_json::Value,
    pub label: &'a str,
    pub label_color: Rgb,
    pub dimmed: bool,
    /// The colour of the header's tool word (`Edit:`) for a call issued
    /// together with others; `None` leaves the header in one style.
    pub tool_word_color: Option<Rgb>,
    pub content_width: usize,
    pub timing: TimingSlot<'a>,
    pub tool_display: ToolDisplayMode,
    pub tool_output_id: &'a ToolOutputId,
    pub expanded: bool,
}

/// Descriptor for one tool-result rendering pass shared by entry and
/// summary-expansion render paths.
pub(super) struct ToolResultRenderSpec<'a> {
    pub text: &'a str,
    /// The first row's name column: `↳ Result` where the result follows its
    /// call, `Result` inside an expanded run, where a connector joins them.
    pub label: &'a str,
    pub content_width: usize,
    pub timing: TimingSlot<'a>,
    pub tool_display: ToolDisplayMode,
    pub tool_output_id: &'a ToolOutputId,
    pub expanded: bool,
}
/// Render a formatted tool call with proper styling
pub(super) fn render_tool_call(lines: &mut Vec<RenderedLine>, spec: &ToolCallRenderSpec<'_>) {
    let ToolCallRenderSpec {
        name,
        tool,
        input,
        label,
        label_color,
        dimmed,
        tool_word_color,
        content_width,
        timing,
        tool_display,
        tool_output_id,
        expanded,
    } = *spec;
    let formatted = tool_format::format_tool_call(name, tool, input, content_width);

    let header_content = header_spans(&formatted.header, name, tool_word_color, dimmed);
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
            tool_output_id: Some(tool_output_id),
            clickable: false,
        },
        header_content,
    );

    // Render the body if present, with empty line separator
    if let Some(body) = formatted.body {
        let body_timing = timing.continuation();

        // Empty line between header and body
        push_row(
            lines,
            LedgerRow {
                timing: body_timing,
                name: NameCol::BlankPlain,
                separator_dimmed: dimmed,
                tool_output_id: Some(tool_output_id),
                clickable: false,
            },
            Vec::new(),
        );

        if tool_display == ToolDisplayMode::Truncated && !expanded {
            let body_lines: Vec<&str> = body.text.lines().collect();
            let total = body_lines.len();
            if total > TRUNCATED_BODY_LINES {
                let truncated = body_lines[..TRUNCATED_BODY_LINES].join("\n");
                render_tool_body(
                    lines,
                    &truncated,
                    body.kind,
                    dimmed,
                    body_timing,
                    Some(tool_output_id),
                    true,
                );
                render_truncation_indicator(
                    lines,
                    total - TRUNCATED_BODY_LINES,
                    dimmed,
                    body_timing,
                    Some(tool_output_id),
                );
            } else {
                render_tool_body(
                    lines,
                    &body.text,
                    body.kind,
                    dimmed,
                    body_timing,
                    None,
                    false,
                );
            }
        } else {
            let id = (tool_display == ToolDisplayMode::Truncated).then_some(tool_output_id);
            render_tool_body(
                lines,
                &body.text,
                body.kind,
                dimmed,
                body_timing,
                id,
                id.is_some(),
            );
        }
    }
}

/// The header in the tool text style, or, given `tool_word_color`, its tool
/// word in that colour and the rest in the tool text style.
fn header_spans(
    header: &str,
    name: &str,
    tool_word_color: Option<Rgb>,
    dimmed: bool,
) -> Vec<(String, LineStyle)> {
    let header_style = LineStyle {
        fg: Some(th().tool_text),
        dimmed,
        ..Default::default()
    };
    match tool_word_color.zip(split_tool_word(header, name)) {
        Some((color, (word, rest))) => vec![
            (word.to_string(), LineStyle::colored(color)),
            (rest.to_string(), header_style),
        ],
        None => vec![(header.to_string(), header_style)],
    }
}

/// The header's tool word and the rest, when the header opens with the
/// call's name and a colon (`Edit:` … ); a header shaped otherwise, such as
/// an agent's `Agent (scout):`, is left whole.
fn split_tool_word<'h>(header: &'h str, name: &str) -> Option<(&'h str, &'h str)> {
    let rest = header.strip_prefix(name)?.strip_prefix(':')?;
    Some((&header[..name.len() + 1], rest))
}

/// Render tool body lines; only a diff body gets its added and removed lines
/// coloured.
fn render_tool_body(
    lines: &mut Vec<RenderedLine>,
    text: &str,
    kind: ToolBodyKind,
    dimmed: bool,
    timing: TimingSlot<'_>,
    tool_output_id: Option<&ToolOutputId>,
    clickable: bool,
) {
    for line in text.lines() {
        let style = match kind.diff_side(line) {
            Some(DiffSide::Added) => LineStyle::colored(th().diff_add),
            Some(DiffSide::Removed) => LineStyle::colored(th().diff_remove),
            None => LineStyle {
                dimmed: true,
                ..Default::default()
            },
        };
        push_row(
            lines,
            LedgerRow {
                timing,
                name: NameCol::BlankPlain,
                separator_dimmed: dimmed,
                tool_output_id,
                clickable,
            },
            vec![(line.to_string(), style)],
        );
    }
}

/// Render tool result under its label, as markdown
pub(super) fn render_tool_result(lines: &mut Vec<RenderedLine>, spec: &ToolResultRenderSpec<'_>) {
    let ToolResultRenderSpec {
        text,
        label,
        content_width,
        timing,
        tool_display,
        tool_output_id,
        expanded,
    } = *spec;
    // Fence plain text tool results to prevent markdown misinterpretation.
    // If the result already contains fenced code blocks, assume it's intentional markdown.
    let text = if text.contains("```") {
        text.to_string()
    } else {
        format!("```text\n{}\n```", text)
    };
    // Render markdown
    let styled_lines = render_markdown_to_lines(&text, content_width);

    let total = styled_lines.len();
    let limit = if tool_display == ToolDisplayMode::Truncated
        && !expanded
        && total > TRUNCATED_RESULT_LINES
    {
        TRUNCATED_RESULT_LINES
    } else {
        total
    };

    let continuation = timing.continuation();
    for (i, styled_line) in styled_lines.iter().take(limit).enumerate() {
        let row_timing = if i == 0 { timing } else { continuation };
        let name_col = if i == 0 {
            NameCol::Label {
                text: label,
                color: th().tool_text,
                bold: false,
                dimmed: false,
            }
        } else {
            NameCol::BlankPlain
        };
        let content: Vec<_> = styled_line
            .spans
            .iter()
            .map(|(t, s)| (t.clone(), s.clone()))
            .collect();
        let clickable = tool_display == ToolDisplayMode::Truncated && (expanded || limit < total);
        let id = clickable.then_some(tool_output_id);
        push_row(
            lines,
            LedgerRow {
                timing: row_timing,
                name: name_col,
                separator_dimmed: false,
                tool_output_id: id,
                clickable,
            },
            content,
        );
    }

    if limit < total {
        render_truncation_indicator(
            lines,
            total - limit,
            false,
            continuation,
            Some(tool_output_id),
        );
    }
}

/// Render the dimmed body of a subagent tool result.
///
/// In truncated tool-display mode this emits at most `TRUNCATED_RESULT_LINES`
/// of the result followed by a clickable "(N more lines...)" indicator;
/// otherwise it renders the full result as a continuation block. Used by
/// both the user-message subagent branch and the agent-progress user
/// branch.
pub(super) fn render_dimmed_tool_result_body(
    lines: &mut Vec<RenderedLine>,
    options: &RenderOptions,
    output_id: &ToolOutputId,
    expanded: bool,
    content_str: &str,
    timing: TimingSlot<'_>,
) {
    let truncated_mode = options.tool_display == ToolDisplayMode::Truncated;
    if truncated_mode && !expanded {
        let content_lines: Vec<&str> = content_str.lines().collect();
        let total = content_lines.len();
        if total > TRUNCATED_RESULT_LINES {
            let truncated = content_lines[..TRUNCATED_RESULT_LINES].join("\n");
            render_continuation_dimmed(lines, &truncated, timing, Some(output_id));
            render_truncation_indicator(
                lines,
                total - TRUNCATED_RESULT_LINES,
                true,
                timing,
                Some(output_id),
            );
        } else {
            render_continuation_dimmed(lines, content_str, timing, None);
        }
    } else {
        let id = truncated_mode.then_some(output_id);
        render_continuation_dimmed(lines, content_str, timing, id);
    }
}

/// Render the "  ↳ Tool │ <Result>" header that introduces a dimmed
/// subagent tool result block.
pub(super) fn render_subagent_tool_result_header(
    lines: &mut Vec<RenderedLine>,
    timing: TimingSlot<'_>,
) {
    render_ledger_block_plain_dimmed(lines, "  ↳ Tool", th().accent_dim, "<Result>", timing);
}
