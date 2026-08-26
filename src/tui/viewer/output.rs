use super::ToolOutputId;

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
    pub fg: Option<(u8, u8, u8)>,
    pub bold: bool,
    pub dimmed: bool,
    pub italic: bool,
}
