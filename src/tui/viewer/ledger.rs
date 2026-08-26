use super::markdown::StyledLine;
use super::timing::TimingSlot;
use textwrap::{Options, WordSeparator, WordSplitter, WrapAlgorithm};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::{LineStyle, NAME_WIDTH, RenderedLine, TIMESTAMP_WIDTH, ToolOutputId, th};

/// The rows of `text` at `width` columns, at least one. Rows break at
/// spaces, each filled before the next starts, so a hyphenated name or a
/// path stays whole; a word wider than a row is cut at the row width.
/// `width == 0` leaves the text as one row.
pub(super) fn wrap_row(text: &str, width: usize) -> Vec<String> {
    wrap_row_indented(text, width, "", "")
}

/// [`wrap_row`] with `first_indent` opening the first row and `rest_indent`
/// every later row, inside `width`: a header's prefix and a pad of its
/// width, or a body's pad. A width no wider than an indent leaves the text
/// as one row after `first_indent`.
pub(super) fn wrap_row_indented(
    text: &str,
    width: usize,
    first_indent: &str,
    rest_indent: &str,
) -> Vec<String> {
    let widest_indent = first_indent.width().max(rest_indent.width());
    if width <= widest_indent {
        return vec![format!("{first_indent}{text}")];
    }
    let options = Options::new(width)
        .initial_indent(first_indent)
        .subsequent_indent(rest_indent)
        .word_separator(WordSeparator::AsciiSpace)
        .word_splitter(WordSplitter::NoHyphenation)
        .wrap_algorithm(WrapAlgorithm::FirstFit);
    textwrap::wrap(text, options)
        .into_iter()
        .map(|row| row.into_owned())
        .collect()
}

/// The name column for a single ledger row.
pub(super) enum NameCol<'a> {
    /// First row of a block: a right-aligned label.
    Label {
        text: &'a str,
        color: (u8, u8, u8),
        bold: bool,
        dimmed: bool,
    },
    /// Continuation row: blank name, default style.
    BlankPlain,
    /// Continuation row: blank name, `dimmed: true` (no fg).
    BlankDim,
    /// Continuation row: blank name carrying the label color, `dimmed: true`.
    BlankColoredDim { color: (u8, u8, u8) },
}

/// Description of one ledger row's structural columns.
pub(super) struct LedgerRow<'a> {
    pub timing: TimingSlot<'a>,
    pub name: NameCol<'a>,
    /// Whether the " │ " separator span renders dimmed.
    pub separator_dimmed: bool,
    /// Optional tool-output id attached to the resulting `RenderedLine`.
    pub tool_output_id: Option<&'a ToolOutputId>,
    pub clickable: bool,
}

/// Low-level ledger writer: assembles the timestamp / name / separator
/// columns according to `row` and appends `content` spans after them.
///
/// All ledger rows in the viewer go through this single entry point so
/// that timestamp width, name alignment, separator styling, and tool
/// output id / clickable propagation stay consistent.
pub(super) fn fitted_name(text: &str) -> String {
    if UnicodeWidthStr::width(text) <= NAME_WIDTH {
        return text.to_owned();
    }

    let mut fitted = String::new();
    let mut width = 0;
    for character in text.chars() {
        let character_width = UnicodeWidthChar::width(character).unwrap_or(0);
        if width + character_width >= NAME_WIDTH {
            break;
        }
        fitted.push(character);
        width += character_width;
    }
    fitted.push('…');
    fitted
}

fn padded_name(text: &str) -> String {
    let fitted = fitted_name(text);
    let padding = NAME_WIDTH.saturating_sub(UnicodeWidthStr::width(fitted.as_str()));
    format!("{}{fitted}", " ".repeat(padding))
}

pub(super) fn push_row(
    lines: &mut Vec<RenderedLine>,
    row: LedgerRow<'_>,
    content: Vec<(String, LineStyle)>,
) {
    let mut spans = Vec::with_capacity(3 + content.len());

    match row.timing {
        TimingSlot::Disabled => {}
        TimingSlot::Pad => {
            spans.push((" ".repeat(TIMESTAMP_WIDTH), LineStyle::default()));
        }
        TimingSlot::Stamp(ts) => {
            spans.push((
                format!(" {} ", ts),
                LineStyle {
                    fg: Some((140, 140, 140)),
                    dimmed: false,
                    bold: false,
                    italic: false,
                },
            ));
        }
    }

    match row.name {
        NameCol::Label {
            text,
            color,
            bold,
            dimmed,
        } => {
            spans.push((
                padded_name(text),
                LineStyle {
                    fg: Some(color),
                    bold,
                    dimmed,
                    italic: false,
                },
            ));
        }
        NameCol::BlankPlain => {
            spans.push((" ".repeat(NAME_WIDTH), LineStyle::default()));
        }
        NameCol::BlankDim => {
            spans.push((
                " ".repeat(NAME_WIDTH),
                LineStyle {
                    dimmed: true,
                    ..Default::default()
                },
            ));
        }
        NameCol::BlankColoredDim { color } => {
            spans.push((
                " ".repeat(NAME_WIDTH),
                LineStyle {
                    fg: Some(color),
                    dimmed: true,
                    ..Default::default()
                },
            ));
        }
    }

    spans.push((
        " │ ".to_string(),
        LineStyle {
            fg: Some(th().border),
            dimmed: row.separator_dimmed,
            ..Default::default()
        },
    ));

    spans.extend(content);

    let line = match row.tool_output_id {
        Some(id) => RenderedLine::tool_output(spans, id.clone(), row.clickable),
        None => RenderedLine::new(spans),
    };
    lines.push(line);
}

/// Render ledger block with styled markdown lines.
///
/// `timing` describes the column for the first row. Continuation rows
/// inherit the column's presence: when `timing` is `Stamp`/`Pad`,
/// continuation rows render as `Pad`; when it is `Disabled`, every row
/// of the block renders without a timing column.
pub(super) fn render_ledger_block_styled(
    lines: &mut Vec<RenderedLine>,
    name: &str,
    color: (u8, u8, u8),
    bold: bool,
    styled_lines: Vec<StyledLine>,
    timing: TimingSlot<'_>,
) {
    if styled_lines.is_empty() {
        push_row(
            lines,
            LedgerRow {
                timing,
                name: NameCol::Label {
                    text: name,
                    color,
                    bold,
                    dimmed: false,
                },
                separator_dimmed: false,
                tool_output_id: None,
                clickable: false,
            },
            Vec::new(),
        );
        return;
    }

    let continuation = timing.continuation();
    for (i, styled_line) in styled_lines.iter().enumerate() {
        let row_timing = if i == 0 { timing } else { continuation };
        let name_col = if i == 0 {
            NameCol::Label {
                text: name,
                color,
                bold,
                dimmed: false,
            }
        } else {
            NameCol::BlankPlain
        };
        let content = styled_line
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
                tool_output_id: None,
                clickable: false,
            },
            content,
        );
    }
}

/// Render a truncation indicator line like "(N more lines...)", after
/// `indent`, the pad of the rows it counts.
pub(super) fn render_truncation_indicator(
    lines: &mut Vec<RenderedLine>,
    remaining: usize,
    dimmed: bool,
    timing: TimingSlot<'_>,
    tool_output_id: Option<&ToolOutputId>,
    indent: &str,
) {
    let content = vec![(
        format!("{indent}({remaining} more lines...)"),
        LineStyle {
            dimmed: true,
            ..Default::default()
        },
    )];
    push_row(
        lines,
        LedgerRow {
            timing,
            name: NameCol::BlankPlain,
            separator_dimmed: dimmed,
            tool_output_id,
            clickable: tool_output_id.is_some(),
        },
        content,
    );
}

/// Render ledger block with styled markdown lines (dimmed for subagents)
pub(super) fn render_ledger_block_styled_dimmed(
    lines: &mut Vec<RenderedLine>,
    name: &str,
    color: (u8, u8, u8),
    styled_lines: Vec<StyledLine>,
    timing: TimingSlot<'_>,
) {
    if styled_lines.is_empty() {
        push_row(
            lines,
            LedgerRow {
                timing,
                name: NameCol::Label {
                    text: name,
                    color,
                    bold: false,
                    dimmed: true,
                },
                separator_dimmed: true,
                tool_output_id: None,
                clickable: false,
            },
            Vec::new(),
        );
        return;
    }

    for (i, styled_line) in styled_lines.iter().enumerate() {
        let name_col = if i == 0 {
            NameCol::Label {
                text: name,
                color,
                bold: false,
                dimmed: true,
            }
        } else {
            NameCol::BlankColoredDim { color }
        };
        let content = styled_line
            .spans
            .iter()
            .cloned()
            .map(|(text, mut style)| {
                style.dimmed = true;
                (text, style)
            })
            .collect();
        push_row(
            lines,
            LedgerRow {
                timing,
                name: name_col,
                separator_dimmed: true,
                tool_output_id: None,
                clickable: false,
            },
            content,
        );
    }
}

/// Render ledger block with plain text (dimmed for subagents)
pub(super) fn render_ledger_block_plain_dimmed(
    lines: &mut Vec<RenderedLine>,
    name: &str,
    color: (u8, u8, u8),
    text: &str,
    timing: TimingSlot<'_>,
) {
    for (i, line_text) in text.lines().enumerate() {
        let name_col = if i == 0 {
            NameCol::Label {
                text: name,
                color,
                bold: false,
                dimmed: true,
            }
        } else {
            NameCol::BlankColoredDim { color }
        };
        let content = vec![(
            line_text.to_string(),
            LineStyle {
                dimmed: true,
                ..Default::default()
            },
        )];
        push_row(
            lines,
            LedgerRow {
                timing,
                name: name_col,
                separator_dimmed: true,
                tool_output_id: None,
                clickable: false,
            },
            content,
        );
    }
}

/// Render continuation rows (dimmed for subagents)
pub(super) fn render_continuation_dimmed(
    lines: &mut Vec<RenderedLine>,
    rows: &[String],
    timing: TimingSlot<'_>,
    tool_output_id: Option<&ToolOutputId>,
) {
    for row_text in rows {
        let content = vec![(
            row_text.clone(),
            LineStyle {
                dimmed: true,
                ..Default::default()
            },
        )];
        push_row(
            lines,
            LedgerRow {
                timing,
                name: NameCol::BlankDim,
                separator_dimmed: true,
                tool_output_id,
                clickable: tool_output_id.is_some(),
            },
            content,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_fit_the_ledger_column() {
        for name in ["You", "Branch summary", "扩展通知很长"] {
            let fitted = padded_name(name);
            assert_eq!(UnicodeWidthStr::width(fitted.as_str()), NAME_WIDTH);
        }
        assert_eq!(padded_name("Branch summary"), "Branch s…");
    }

    #[test]
    fn rows_break_at_spaces_and_fill_before_the_next_starts() {
        assert_eq!(
            wrap_row("git checkout delete-subagent-orphans", 30),
            ["git checkout", "delete-subagent-orphans"]
        );
        assert_eq!(
            wrap_row("one two three four five six", 14),
            ["one two three", "four five six"]
        );
    }

    #[test]
    fn a_word_wider_than_a_row_is_cut_at_the_row_width() {
        let path = "C:\\Users\\micro\\rearview\\src\\x.rs";
        assert_eq!(
            wrap_row(&format!("The file {path} is"), 20),
            ["The file", "C:\\Users\\micro\\rearv", "iew\\src\\x.rs is"]
        );
        let rows = wrap_row(&"x".repeat(100), 40);
        assert_eq!(
            rows.iter().map(String::len).collect::<Vec<_>>(),
            [40, 40, 20]
        );
    }

    #[test]
    fn an_empty_line_or_a_width_of_zero_is_one_row() {
        assert_eq!(wrap_row("", 40), [""]);
        assert_eq!(wrap_row(&"x".repeat(100), 0), ["x".repeat(100)]);
    }

    #[test]
    fn indents_open_the_rows_inside_the_width() {
        assert_eq!(
            wrap_row_indented("a b c", 5, "X: ", "   "),
            ["X: a", "   b", "   c"]
        );
        assert_eq!(
            wrap_row_indented(&"x".repeat(10), 8, "X: ", "   "),
            ["X: xxxxx", "   xxxxx"]
        );
        // An indent as wide as the row leaves the text whole.
        assert_eq!(wrap_row_indented("a b c", 3, "X: ", "   "), ["X: a b c"]);
    }
}
