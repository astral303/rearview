//! Connectors from each call of an expanded tool run to its result.
//!
//! A parallel batch renders every call before the first result, so a result
//! can sit rows away from the call it answers. A thin line joins the two:
//! `┘` at the bottom of the input's rule, `┌─────` back along the input's
//! last row into the label column, `│` down, `↓` on the blank row above the
//! result, `┐` at the top of the result's rule. Rows keep their order; only
//! cells of the label column and the rule change.

use std::collections::BTreeMap;

use super::{CallRange, LineStyle, NAME_WIDTH, RenderedLine, TIMESTAMP_WIDTH, th};

/// A call issued alone runs its lane under the first letter of a six-letter
/// label; each further call of a parallel batch takes the next cell, so six
/// lanes fit under `Claude`.
const FIRST_LANE_CELL: usize = 3;
const RULE: &str = " │ ";
const RULE_LEAVING_INPUT: &str = "─┘ ";
/// A one-row input has its label on the anchor row, leaving no room for
/// `┌─────`; `┤` on the rule alone marks where the connector leaves.
const RULE_LEAVING_ONE_ROW_INPUT: &str = " ┤ ";
const RULE_ENTERING_RESULT: &str = " ┐ ";

/// The rows one call's connector runs through, and its lane cell.
struct Connector {
    lane: usize,
    /// The input's last row, which the connector leaves.
    anchor: usize,
    /// True when the input is one row, so its label sits on the anchor row.
    anchor_holds_label: bool,
    /// The blank row above the result, which carries `↓`.
    tip: usize,
    /// The result's first row, which the connector enters.
    result: usize,
}

impl Connector {
    /// `None` for a call without a result, a lane past the label column, or
    /// a result with no row between it and the input.
    fn of(call: &CallRange) -> Option<Self> {
        let result = call.result.as_ref()?;
        let lane = FIRST_LANE_CELL + call.batch_position.unwrap_or(0);
        let anchor = call.input.end_line - 1;
        let tip = result.start_line.checked_sub(1)?;
        (lane < NAME_WIDTH && tip > anchor).then_some(Self {
            lane,
            anchor,
            anchor_holds_label: call.input.end_line - call.input.start_line == 1,
            tip,
            result: result.start_line,
        })
    }

    fn add_to(&self, patches: &mut BTreeMap<usize, RowPatch>) {
        let anchor = patches.entry(self.anchor).or_default();
        if self.anchor_holds_label {
            anchor.rule = Some(RULE_LEAVING_ONE_ROW_INPUT);
        } else {
            anchor.cells[self.lane] = Some('┌');
            anchor.cells[self.lane + 1..].fill(Some('─'));
            anchor.rule = Some(RULE_LEAVING_INPUT);
        }
        for row in self.anchor + 1..self.tip {
            patches.entry(row).or_default().cells[self.lane] = Some('│');
        }
        patches.entry(self.tip).or_default().cells[self.lane] = Some('↓');
        patches.entry(self.result).or_default().rule = Some(RULE_ENTERING_RESULT);
    }
}

/// The glyphs every connector wants in one row: lane cells of the label
/// column, and the rule's text where a connector leaves or enters.
#[derive(Default)]
struct RowPatch {
    cells: [Option<char>; NAME_WIDTH],
    rule: Option<&'static str>,
}

pub(super) fn draw_connectors(lines: &mut [RenderedLine], calls: &[CallRange], show_timing: bool) {
    let mut patches: BTreeMap<usize, RowPatch> = BTreeMap::new();
    for connector in calls.iter().filter_map(Connector::of) {
        connector.add_to(&mut patches);
    }
    let style = LineStyle {
        fg: Some(th().text_muted),
        ..Default::default()
    };
    // A row's spans are `[timing?] [label column] [rule] [content…]`.
    let label_index = usize::from(show_timing);
    for (row, patch) in patches {
        paint_row(&mut lines[row], &patch, &style, label_index);
    }
}

fn paint_row(line: &mut RenderedLine, patch: &RowPatch, style: &LineStyle, label_index: usize) {
    if line.spans.is_empty() {
        // A blank row between blocks: give it the columns the lane needs.
        let timing_pad = (" ".repeat(TIMESTAMP_WIDTH), LineStyle::default());
        line.spans
            .extend(std::iter::repeat_n(timing_pad, label_index));
        line.spans.extend(label_column_spans(
            &[' '; NAME_WIDTH],
            &LineStyle::default(),
            &patch.cells,
            style,
        ));
        return;
    }
    if let Some(rule) = patch.rule
        && let Some(span) = line.spans.get_mut(label_index + 1)
        && span.0 == RULE
    {
        *span = (rule.to_string(), style.clone());
    }
    paint_label_column(line, label_index, &patch.cells, style);
}

/// Write lane glyphs into the row's label column where its cells are
/// blank; a label's own characters stay.
fn paint_label_column(
    line: &mut RenderedLine,
    label_index: usize,
    lane_cells: &[Option<char>; NAME_WIDTH],
    style: &LineStyle,
) {
    let Some((text, base)) = line.spans.get(label_index) else {
        return;
    };
    let chars: Vec<char> = text.chars().collect();
    let Ok(chars) = <[char; NAME_WIDTH]>::try_from(chars) else {
        return;
    };
    let base = base.clone();
    let replacement = label_column_spans(&chars, &base, lane_cells, style);
    line.spans.splice(label_index..=label_index, replacement);
}

fn label_column_spans(
    chars: &[char; NAME_WIDTH],
    base: &LineStyle,
    lane_cells: &[Option<char>; NAME_WIDTH],
    lane_style: &LineStyle,
) -> Vec<(String, LineStyle)> {
    let mut spans: Vec<(String, LineStyle)> = Vec::new();
    for (cell, &original) in chars.iter().enumerate() {
        let (glyph, style) = match lane_cells[cell] {
            Some(glyph) if original == ' ' => (glyph, lane_style),
            _ => (original, base),
        };
        match spans.last_mut() {
            Some((text, last)) if last == style => text.push(glyph),
            _ => spans.push((glyph.to_string(), style.clone())),
        }
    }
    spans
}
