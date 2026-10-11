use super::ToolOutputId;
use crate::tui::theme::Rgb;

/// A single rendered line with its spans
#[derive(Clone, Debug)]
pub struct RenderedLine {
    pub spans: Vec<(String, LineStyle)>,
    pub tool_output_id: Option<ToolOutputId>,
    pub clickable: bool,
    /// How the row continues the row above, when both are rows of one
    /// wrapped line.
    pub continuation: RowContinuation,
    /// The bytes at the end of the row's text that continue the row above:
    /// its content after the ledger columns and any indent it repeats.
    /// Counted from the end, because connector lanes repaint the columns.
    pub continued_text_len: usize,
}

impl RenderedLine {
    pub fn new(spans: Vec<(String, LineStyle)>) -> Self {
        Self {
            spans,
            tool_output_id: None,
            clickable: false,
            continuation: RowContinuation::None,
            continued_text_len: 0,
        }
    }

    pub fn tool_output(
        spans: Vec<(String, LineStyle)>,
        tool_output_id: ToolOutputId,
        clickable: bool,
    ) -> Self {
        Self {
            tool_output_id: Some(tool_output_id),
            clickable,
            ..Self::new(spans)
        }
    }

    /// The row's spans joined.
    pub fn text(&self) -> String {
        self.spans.iter().map(|(text, _)| text.as_str()).collect()
    }
}

/// How a row continues the row above it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RowContinuation {
    /// The row starts a line of its own.
    #[default]
    None,
    /// The line wrapped at a space that neither row shows.
    AfterSpace,
    /// The line wrapped with nothing dropped: inside a word, or after a
    /// space the row above keeps.
    Unbroken,
}

/// A wrapped row's place in its line: how it continues the row above, and
/// how many bytes of indent open its content.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RowWrap {
    pub continuation: RowContinuation,
    pub indent_len: usize,
}

/// Style information for a span
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LineStyle {
    pub fg: Option<Rgb>,
    pub bold: bool,
    pub dimmed: bool,
    pub italic: bool,
}

impl LineStyle {
    /// `color` on an undimmed span. A dimmed span renders in `text_muted`
    /// whatever its `fg`, so a coloured span inside a dimmed run is built
    /// this way rather than with the run's `dimmed`.
    pub fn colored(color: Rgb) -> Self {
        Self {
            fg: Some(color),
            ..Default::default()
        }
    }
}

/// One span per run of consecutive cells that share a style.
pub(super) fn spans_of_cells(
    cells: impl IntoIterator<Item = (char, LineStyle)>,
) -> Vec<(String, LineStyle)> {
    let mut spans: Vec<(String, LineStyle)> = Vec::new();
    for (glyph, style) in cells {
        match spans.last_mut() {
            Some((text, last)) if *last == style => text.push(glyph),
            _ => spans.push((glyph.to_string(), style)),
        }
    }
    spans
}
