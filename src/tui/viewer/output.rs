use super::ToolOutputId;
use crate::tui::theme::Rgb;

/// A single rendered line with its spans
#[derive(Clone, Debug)]
pub struct RenderedLine {
    pub spans: Vec<(String, LineStyle)>,
    pub tool_output_id: Option<ToolOutputId>,
    pub clickable: bool,
}

impl RenderedLine {
    pub fn new(spans: Vec<(String, LineStyle)>) -> Self {
        Self {
            spans,
            tool_output_id: None,
            clickable: false,
        }
    }

    pub fn tool_output(
        spans: Vec<(String, LineStyle)>,
        tool_output_id: ToolOutputId,
        clickable: bool,
    ) -> Self {
        Self {
            spans,
            tool_output_id: Some(tool_output_id),
            clickable,
        }
    }
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
