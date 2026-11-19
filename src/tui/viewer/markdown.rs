use super::output::{RowContinuation, RowWrap};
use super::text_blocks::{TextBlockRow, text_block_rows};
use super::{LineStyle, th};
use crate::markdown::layout::{LayoutDoc, LayoutEngine};

/// A line with styled spans from markdown rendering
pub(super) struct StyledLine {
    pub(super) spans: Vec<(String, LineStyle)>,
    pub(super) wrap: RowWrap,
    /// The block of a message's text the line shows.
    pub(super) text_block_row: Option<TextBlockRow>,
}

/// Render markdown text to styled lines for TUI display
pub(super) fn render_markdown_to_lines(input: &str, max_width: usize) -> Vec<StyledLine> {
    styled_lines(
        LayoutEngine::render(input, max_width),
        std::iter::repeat(None),
    )
}

/// [`render_markdown_to_lines`] for a message's text: each line carries the
/// top-level block it shows, which message navigation steps through.
pub(super) fn render_message_text_to_lines(input: &str, max_width: usize) -> Vec<StyledLine> {
    let doc = LayoutEngine::render_with_blocks(input, max_width);
    let text_block_rows = text_block_rows(input, &doc);
    styled_lines(doc, text_block_rows)
}

/// `doc`'s lines with `text_block_rows`, one per line, as styled lines.
fn styled_lines(
    doc: LayoutDoc,
    text_block_rows: impl IntoIterator<Item = Option<TextBlockRow>>,
) -> Vec<StyledLine> {
    doc.lines
        .into_iter()
        .zip(text_block_rows)
        // Drop fence-only lines — TUI signals code blocks via color instead.
        // Empty lines are kept (they're blank spacers, not fences).
        .filter(|(line, _)| line.runs.is_empty() || line.runs.iter().any(|r| !r.attrs.code_fence))
        .map(|(line, text_block_row)| StyledLine {
            spans: line
                .runs
                .into_iter()
                .filter(|run| !run.attrs.link_url && !run.attrs.heading_marker)
                .map(|run| (run.text, attrs_to_line_style(&run.attrs)))
                .collect(),
            wrap: RowWrap {
                // The layout drops nothing where it wraps: prose keeps its
                // spaces on the row above, and a code line is cut mid-word.
                continuation: if line.wrap.continues_above {
                    RowContinuation::Unbroken
                } else {
                    RowContinuation::None
                },
                indent_len: line.wrap.prefix_len,
            },
            text_block_row,
        })
        .collect()
}

fn attrs_to_line_style(attrs: &crate::markdown::layout::Attrs) -> LineStyle {
    let fg = if let Some(rgb) = attrs.fg {
        Some(rgb)
    } else if attrs.code_block_lang.is_some() || attrs.code {
        Some(th().code_color)
    } else if attrs.quote {
        Some(th().green)
    } else if attrs.link {
        Some(th().blue)
    } else if attrs.heading {
        Some(th().heading)
    } else {
        None
    };
    LineStyle {
        bold: attrs.bold || attrs.heading,
        italic: attrs.italic,
        dimmed: attrs.dimmed || attrs.strikethrough,
        fg,
    }
}

/// Apply italic and dimmed styling to thinking block content
pub(super) fn apply_thinking_style(styled_lines: Vec<StyledLine>) -> Vec<StyledLine> {
    styled_lines
        .into_iter()
        .map(|line| StyledLine {
            spans: line
                .spans
                .into_iter()
                .map(|(text, mut style)| {
                    style.italic = true;
                    style.fg = Some(th().thinking_text);
                    (text, style)
                })
                .collect(),
            wrap: line.wrap,
            text_block_row: line.text_block_row,
        })
        .collect()
}
