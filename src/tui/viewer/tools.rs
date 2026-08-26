use crate::log_entry::Tool;
use crate::tool_format::{self, DiffSide, ToolBody, ToolBodyKind};
use crate::tui::theme::Rgb;
use unicode_width::UnicodeWidthStr;

use super::ledger::{
    LedgerRow, NameCol, push_row, render_continuation_dimmed, render_ledger_block_plain_dimmed,
    render_truncation_indicator, wrap_row, wrap_row_indented,
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
    /// The colour of the header's tool word (`Edit:`) for an interleaved
    /// call; `None` leaves the header in one style.
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
    pub content_width: usize,
    pub timing: TimingSlot<'a>,
    pub tool_display: ToolDisplayMode,
    pub tool_output_id: &'a ToolOutputId,
    pub expanded: bool,
}
/// Render a formatted tool call, every row wrapped at `content_width`. The
/// header's value wraps under its prefix and shows whole. A shell command's
/// later rows sit under the value column with no blank row between, its
/// first line's wrapped rows included; other bodies sit flush left after a
/// blank row. In truncated display, body rows past `TRUNCATED_BODY_LINES`
/// show once the call is expanded.
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
    let formatted = tool_format::format_tool_call(name, tool, input, tool_format::NO_WRAP);
    let value_column = " ".repeat(formatted.prefix.width());
    let header_rows = wrap_row_indented(
        &formatted.value,
        content_width,
        &formatted.prefix,
        &value_column,
    );
    let (first_row, later_header_rows) = header_rows
        .split_first()
        .expect("wrap_row_indented yields at least one row");

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
        header_spans(first_row, name, tool_word_color, dimmed),
    );

    let continuation = timing.continuation();
    let push_header_row = |lines: &mut Vec<RenderedLine>, content: Vec<(String, LineStyle)>| {
        push_row(
            lines,
            LedgerRow {
                timing: continuation,
                name: NameCol::BlankPlain,
                separator_dimmed: dimmed,
                tool_output_id: Some(tool_output_id),
                clickable: false,
            },
            content,
        );
    };

    let (indent, rows) = if formatted.value_is_first_line {
        let mut rows: Vec<_> = later_header_rows
            .iter()
            .map(|row| (row.clone(), body_style(None)))
            .collect();
        if let Some(body) = &formatted.body {
            rows.extend(body_rows(body, content_width, &value_column));
        }
        (value_column, rows)
    } else {
        for row in later_header_rows {
            push_header_row(lines, vec![(row.clone(), header_style(dimmed))]);
        }
        let rows = match &formatted.body {
            Some(body) => {
                push_header_row(lines, Vec::new());
                body_rows(body, content_width, "")
            }
            None => Vec::new(),
        };
        (String::new(), rows)
    };

    let truncated = tool_display == ToolDisplayMode::Truncated
        && !expanded
        && rows.len() > TRUNCATED_BODY_LINES;
    let shown = if truncated {
        TRUNCATED_BODY_LINES
    } else {
        rows.len()
    };
    let hidden = rows.len() - shown;
    let clickable = tool_display == ToolDisplayMode::Truncated && (expanded || truncated);
    let id = clickable.then_some(tool_output_id);
    for (text, style) in rows.into_iter().take(shown) {
        push_row(
            lines,
            LedgerRow {
                timing: continuation,
                name: NameCol::BlankPlain,
                separator_dimmed: dimmed,
                tool_output_id: id,
                clickable,
            },
            vec![(text, style)],
        );
    }
    if truncated {
        render_truncation_indicator(
            lines,
            hidden,
            dimmed,
            continuation,
            Some(tool_output_id),
            &indent,
        );
    }
}

fn header_style(dimmed: bool) -> LineStyle {
    LineStyle {
        fg: Some(th().tool_text),
        dimmed,
        ..Default::default()
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
    match tool_word_color.zip(split_tool_word(header, name)) {
        Some((color, (word, rest))) => vec![
            (word.to_string(), LineStyle::colored(color)),
            (rest.to_string(), header_style(dimmed)),
        ],
        None => vec![(header.to_string(), header_style(dimmed))],
    }
}

/// The header's tool word and the rest, when the header opens with the
/// call's name and a colon (`Edit:` … ); a header shaped otherwise, such as
/// an agent's `Agent (scout):`, is left whole.
fn split_tool_word<'h>(header: &'h str, name: &str) -> Option<(&'h str, &'h str)> {
    let rest = header.strip_prefix(name)?.strip_prefix(':')?;
    Some((&header[..name.len() + 1], rest))
}

/// The body's rows at `content_width`, after `indent`, each with its style.
/// Only a diff body gets its added and removed lines coloured, on every row
/// a line wraps to; a diff line's later rows sit one more column in, under
/// its text rather than its sign.
fn body_rows(body: &ToolBody, content_width: usize, indent: &str) -> Vec<(String, LineStyle)> {
    let later_row_indent = match body.kind {
        ToolBodyKind::Diff => format!("{indent} "),
        ToolBodyKind::Plain => indent.to_string(),
    };
    body.text
        .lines()
        .flat_map(|line| {
            let style = body_style(body.kind.diff_side(line));
            wrap_row_indented(line, content_width, indent, &later_row_indent)
                .into_iter()
                .map(move |row| (row, style.clone()))
        })
        .collect()
}

fn body_style(diff_side: Option<DiffSide>) -> LineStyle {
    match diff_side {
        Some(DiffSide::Added) => LineStyle::colored(th().diff_add),
        Some(DiffSide::Removed) => LineStyle::colored(th().diff_remove),
        None => LineStyle {
            dimmed: true,
            ..Default::default()
        },
    }
}

/// Render tool result under its label, as markdown
pub(super) fn render_tool_result(lines: &mut Vec<RenderedLine>, spec: &ToolResultRenderSpec<'_>) {
    let ToolResultRenderSpec {
        text,
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
                text: "Result",
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
            "",
        );
    }
}

/// Render the dimmed body of a subagent tool result, wrapped at the content
/// width.
///
/// In truncated tool-display mode this emits at most `TRUNCATED_RESULT_LINES`
/// rows of the result followed by a clickable "(N more lines...)" indicator;
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
    let rows: Vec<String> = content_str
        .lines()
        .flat_map(|line| wrap_row(line, options.content_width))
        .collect();
    let truncated_mode = options.tool_display == ToolDisplayMode::Truncated;
    let truncated = truncated_mode && !expanded && rows.len() > TRUNCATED_RESULT_LINES;
    let shown = if truncated {
        TRUNCATED_RESULT_LINES
    } else {
        rows.len()
    };
    let clickable = truncated_mode && (expanded || truncated);
    render_continuation_dimmed(
        lines,
        &rows[..shown],
        timing,
        clickable.then_some(output_id),
    );
    if truncated {
        render_truncation_indicator(lines, rows.len() - shown, true, timing, Some(output_id), "");
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
