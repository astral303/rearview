use crate::history::TaskReport;
use crate::log_entry::Tool;
use crate::tool_format::{self, DiffSide, FormattedToolCall, ToolBody, ToolBodyKind};
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
    /// The tool a standalone result names for itself. A result answering a
    /// call sits under it and is labelled `Result`.
    pub standalone_tool_name: Option<&'a str>,
    pub content_width: usize,
    pub timing: TimingSlot<'a>,
    pub tool_display: ToolDisplayMode,
    pub tool_output_id: &'a ToolOutputId,
    pub expanded: bool,
}
/// Render a formatted tool call: its header rows, then its body rows under
/// the truncation rule.
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
    let CallRows {
        header,
        blank_row_after_header,
        indent,
        body,
    } = call_rows(&formatted, content_width);
    let (first_row, later_header_rows) = header
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
    let later_header_contents = later_header_rows
        .iter()
        .map(|row| vec![(row.clone(), header_style(dimmed))])
        .chain(blank_row_after_header.then(Vec::new));
    for content in later_header_contents {
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
    }

    let truncation =
        Truncation::of_tool_result(body.len(), TRUNCATED_BODY_LINES, tool_display, expanded);
    let id = truncation.clickable.then_some(tool_output_id);
    for (text, style) in body.into_iter().take(truncation.shown) {
        push_row(
            lines,
            LedgerRow {
                timing: continuation,
                name: NameCol::BlankPlain,
                separator_dimmed: dimmed,
                tool_output_id: id,
                clickable: truncation.clickable,
            },
            vec![(text, style)],
        );
    }
    if truncation.hidden > 0 {
        render_truncation_indicator(
            lines,
            truncation.hidden,
            dimmed,
            continuation,
            Some(tool_output_id),
            &indent,
        );
    }
}

/// The rows of one call at the content width, without the ledger columns.
struct CallRows {
    /// The header's rows: the first opens with the prefix, the rest are
    /// padded to the value column.
    header: Vec<String>,
    /// True when a blank row separates the header from the body.
    blank_row_after_header: bool,
    /// The pad the body rows and the truncation indicator sit after.
    indent: String,
    /// The rows truncation counts, each with its style.
    body: Vec<(String, LineStyle)>,
}

/// Wrap the call at `content_width`. The header's value wraps under its
/// prefix. A shell command's rows after the first, its first line's wrapped
/// rows included, are body rows under the value column with no blank row
/// between, so a long command truncates as a long body does; another call's
/// header shows whole and its body sits flush left after a blank row.
fn call_rows(formatted: &FormattedToolCall, content_width: usize) -> CallRows {
    let value_column = " ".repeat(formatted.prefix.width());
    let mut header = wrap_row_indented(
        &formatted.value,
        content_width,
        &formatted.prefix,
        &value_column,
    );
    if formatted.value_is_first_line {
        let mut body: Vec<_> = header
            .drain(1..)
            .map(|row| (row, plain_body_style()))
            .collect();
        if let Some(later_lines) = &formatted.body {
            body.extend(body_rows(later_lines, content_width, &value_column));
        }
        return CallRows {
            header,
            blank_row_after_header: false,
            indent: value_column,
            body,
        };
    }
    CallRows {
        header,
        blank_row_after_header: formatted.body.is_some(),
        indent: String::new(),
        body: formatted
            .body
            .as_ref()
            .map_or_else(Vec::new, |body| body_rows(body, content_width, "")),
    }
}

mod truncation {
    use super::ToolDisplayMode;

    /// The rows a block shows of its lines: `shown` rows render, `hidden`
    /// rows sit behind the `(N more lines...)` row, and `clickable` is true
    /// while a click on the block's rows toggles between the two, which is
    /// while the block is truncated or was expanded.
    pub(super) struct Truncation {
        pub(super) shown: usize,
        pub(super) hidden: usize,
        pub(super) clickable: bool,
    }

    impl Truncation {
        /// A tool result: truncated in `tools·trn` alone.
        pub(super) fn of_tool_result(
            total: usize,
            limit: usize,
            tool_display: ToolDisplayMode,
            expanded: bool,
        ) -> Self {
            Self::from_line_budget(
                tool_display == ToolDisplayMode::Truncated,
                total,
                limit,
                expanded,
            )
        }

        /// A task report: truncated in every mode but `tools·full`.
        pub(super) fn of_task_report(
            total: usize,
            limit: usize,
            tool_display: ToolDisplayMode,
            expanded: bool,
        ) -> Self {
            Self::from_line_budget(
                tool_display != ToolDisplayMode::Full,
                total,
                limit,
                expanded,
            )
        }

        /// Every row shown and none clickable.
        pub(super) fn whole(total: usize) -> Self {
            Self::from_line_budget(false, total, total, false)
        }

        fn from_line_budget(
            is_truncatable: bool,
            total_lines: usize,
            line_limit: usize,
            is_expanded: bool,
        ) -> Self {
            let is_truncated = is_truncatable && !is_expanded && total_lines > line_limit;
            let shown = if is_truncated {
                line_limit
            } else {
                total_lines
            };
            Self {
                shown,
                hidden: total_lines - shown,
                clickable: is_truncatable && (is_expanded || is_truncated),
            }
        }
    }
}

use truncation::Truncation;

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
        None => plain_body_style(),
    }
}

fn plain_body_style() -> LineStyle {
    LineStyle {
        dimmed: true,
        ..Default::default()
    }
}

/// Render tool result under its label, as markdown
pub(super) fn render_tool_result(lines: &mut Vec<RenderedLine>, spec: &ToolResultRenderSpec<'_>) {
    let ToolResultRenderSpec {
        text,
        standalone_tool_name,
        content_width,
        timing,
        tool_display,
        tool_output_id,
        expanded,
    } = *spec;
    // Fence plain text tool results to prevent markdown misinterpretation.
    // If the result already contains fenced code blocks, assume it's intentional markdown.
    let markdown = if text.contains("```") {
        text.to_string()
    } else {
        format!("```text\n{text}\n```")
    };
    // Render markdown
    let styled_lines = render_markdown_to_lines(&markdown, content_width);

    let truncation = Truncation::of_tool_result(
        styled_lines.len(),
        TRUNCATED_RESULT_LINES,
        tool_display,
        expanded,
    );
    let id = truncation.clickable.then_some(tool_output_id);
    let continuation = timing.continuation();
    let result_label = NameCol::Label {
        text: "Result",
        color: th().tool_text,
        bold: false,
        dimmed: false,
    };

    // A standalone result names its tool on a row of its own, as a call heads
    // its body with the tool it invoked. That leaves the body its full width
    // and its whole truncation budget.
    if let Some(tool) = standalone_tool_name {
        push_row(
            lines,
            LedgerRow {
                timing,
                name: result_label,
                separator_dimmed: false,
                tool_output_id: id,
                clickable: truncation.clickable,
            },
            vec![(tool.to_owned(), LineStyle::colored(th().tool_text))],
        );
    }

    for (i, styled_line) in styled_lines.iter().take(truncation.shown).enumerate() {
        let heads_the_result = i == 0 && standalone_tool_name.is_none();
        let row_timing = if heads_the_result {
            timing
        } else {
            continuation
        };
        let name_col = if heads_the_result {
            result_label
        } else {
            NameCol::BlankPlain
        };
        let content: Vec<_> = styled_line
            .spans
            .iter()
            .map(|(t, s)| (t.clone(), s.clone()))
            .collect();
        push_row(
            lines,
            LedgerRow {
                timing: row_timing,
                name: name_col,
                separator_dimmed: false,
                tool_output_id: id,
                clickable: truncation.clickable,
            },
            content,
        );
    }

    if truncation.hidden > 0 {
        render_truncation_indicator(
            lines,
            truncation.hidden,
            false,
            continuation,
            Some(tool_output_id),
            "",
        );
    }
}

pub(super) struct TaskReportRenderSpec<'a> {
    pub report: &'a TaskReport,
    pub label: &'a str,
    pub label_color: Rgb,
    pub content_width: usize,
    pub timing: TimingSlot<'a>,
    pub tool_display: ToolDisplayMode,
    pub tool_output_id: &'a ToolOutputId,
    pub expanded: bool,
    /// True when no gesture can expand the report, as under `--render`.
    pub whole: bool,
}

/// The summary row toggles the body, as a collapsed run's row does, so `→`
/// reaches it from the message stop.
pub(super) fn render_task_report(lines: &mut Vec<RenderedLine>, spec: &TaskReportRenderSpec<'_>) {
    let TaskReportRenderSpec {
        report,
        label,
        label_color,
        content_width,
        timing,
        tool_display,
        tool_output_id,
        expanded,
        whole,
    } = *spec;
    let body = report
        .body
        .as_deref()
        .map(|markdown| render_markdown_to_lines(markdown, content_width))
        .unwrap_or_default();
    let truncation = if whole {
        Truncation::whole(body.len())
    } else {
        Truncation::of_task_report(body.len(), TRUNCATED_RESULT_LINES, tool_display, expanded)
    };
    let id = truncation.clickable.then_some(tool_output_id);
    let continuation = timing.continuation();

    for (i, row) in wrap_row(&report.summary, content_width)
        .into_iter()
        .enumerate()
    {
        let (row_timing, name) = if i == 0 {
            (
                timing,
                NameCol::Label {
                    text: label,
                    color: label_color,
                    bold: true,
                    dimmed: false,
                },
            )
        } else {
            (continuation, NameCol::BlankPlain)
        };
        push_row(
            lines,
            task_report_row(row_timing, name, id, truncation.clickable),
            vec![(row, LineStyle::default())],
        );
    }
    if let Some(usage) = &report.usage {
        for row in wrap_row(&usage.line(), content_width) {
            push_row(
                lines,
                task_report_row(continuation, NameCol::BlankPlain, id, truncation.clickable),
                vec![(row, plain_body_style())],
            );
        }
    }
    for styled_line in body.iter().take(truncation.shown) {
        push_row(
            lines,
            task_report_row(continuation, NameCol::BlankPlain, id, truncation.clickable),
            styled_line.spans.clone(),
        );
    }
    if truncation.hidden > 0 {
        render_truncation_indicator(
            lines,
            truncation.hidden,
            false,
            continuation,
            Some(tool_output_id),
            "",
        );
    }
}

fn task_report_row<'a>(
    timing: TimingSlot<'a>,
    name: NameCol<'a>,
    tool_output_id: Option<&'a ToolOutputId>,
    clickable: bool,
) -> LedgerRow<'a> {
    LedgerRow {
        timing,
        name,
        separator_dimmed: false,
        tool_output_id,
        clickable,
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
    let truncation = Truncation::of_tool_result(
        rows.len(),
        TRUNCATED_RESULT_LINES,
        options.tool_display,
        expanded,
    );
    render_continuation_dimmed(
        lines,
        &rows[..truncation.shown],
        timing,
        truncation.clickable.then_some(output_id),
    );
    if truncation.hidden > 0 {
        render_truncation_indicator(lines, truncation.hidden, true, timing, Some(output_id), "");
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
