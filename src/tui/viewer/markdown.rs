use super::output::{RowContinuation, RowWrap};
use super::{LineStyle, th};

/// A line with styled spans from markdown rendering
pub(super) struct StyledLine {
    pub(super) spans: Vec<(String, LineStyle)>,
    pub(super) wrap: RowWrap,
}

/// Render markdown text to styled lines for TUI display
pub(super) fn render_markdown_to_lines(input: &str, max_width: usize) -> Vec<StyledLine> {
    let doc = crate::markdown::layout::LayoutEngine::render(input, max_width);
    doc.lines
        .into_iter()
        // Drop fence-only lines — TUI signals code blocks via color instead.
        // Empty lines are kept (they're blank spacers, not fences).
        .filter(|line| line.runs.is_empty() || line.runs.iter().any(|r| !r.attrs.code_fence))
        .map(|line| StyledLine {
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
        })
        .collect()
}
