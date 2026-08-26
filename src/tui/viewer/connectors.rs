//! Connectors from each tool call to its result — inside an expanded run in
//! summary mode, and throughout the detail modes — and the colour of each
//! call's lane.
//!
//! A batch of interleaved calls renders every call before the first
//! result, so a result can sit rows away from the call it answers. A thin
//! line joins the two:
//! `┘` at the bottom of the input's rule, `┌─────` back along the input's
//! last row into the label column, `│` down, `↓` on the blank row above the
//! result, `┐` at the top of the result's rule. Each call open beside
//! another draws its connector and the rule of its input and result in its
//! lane's colour; a call open alone draws both in the rule's grey. Rows
//! keep their order; only cells of the label column and the rule change.

use std::collections::BTreeMap;
use std::ops::Range;

use crate::tui::theme::Rgb;

use super::{CallArea, CallRange, LineStyle, NAME_WIDTH, RenderedLine, TIMESTAMP_WIDTH, th};

/// A call open alone runs its lane under the first letter of a six-letter
/// label; a call issued beside open calls takes the cell right of their
/// lanes, so six lanes fit under `Claude`.
const FIRST_LANE_CELL: usize = 3;
const RULE: &str = " │ ";
const RULE_LEAVING_INPUT: &str = "─┘ ";
/// A one-row input has its label on the anchor row, leaving no room for
/// `┌─────`; `┤` on the rule alone marks where the connector leaves.
const RULE_LEAVING_ONE_ROW_INPUT: &str = " ┤ ";
const RULE_ENTERING_RESULT: &str = " ┐ ";

/// The colour of a call by its lane; `None` for a call open alone.
pub(super) fn lane_color(lane: Option<usize>) -> Option<Rgb> {
    let palette = &th().batch_call_colors;
    lane.map(|lane| palette[lane % palette.len()])
}

/// The colour of a call's rule and connector: its lane's colour, or the
/// rule's own grey for a call open alone.
fn call_color(call: &CallRange) -> Rgb {
    lane_color(call.lane).unwrap_or(th().border)
}

/// The cell of a call's lane in the label column; the first lane cell for a
/// call open alone.
fn lane_cell(call: &CallRange) -> usize {
    FIRST_LANE_CELL + call.lane.unwrap_or(0)
}

/// The rows one call's connector runs through, and its lane cell and colour.
struct Connector {
    lane_cell: usize,
    color: Rgb,
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
        let lane_cell = lane_cell(call);
        let anchor = call.input.end_line - 1;
        let tip = result.start_line.checked_sub(1)?;
        (lane_cell < NAME_WIDTH && tip > anchor).then_some(Self {
            lane_cell,
            color: call_color(call),
            anchor,
            anchor_holds_label: rows(&call.input).len() == 1,
            tip,
            result: result.start_line,
        })
    }

    fn add_to(&self, patches: &mut BTreeMap<usize, RowPatch>) {
        let anchor = patches.entry(self.anchor).or_default();
        if self.anchor_holds_label {
            anchor.rule = Some((RULE_LEAVING_ONE_ROW_INPUT, self.color));
        } else {
            anchor.cells[self.lane_cell] = Some(('┌', self.color));
            anchor.cells[self.lane_cell + 1..].fill(Some(('─', self.color)));
            anchor.rule = Some((RULE_LEAVING_INPUT, self.color));
        }
        for row in self.anchor + 1..self.tip {
            patches.entry(row).or_default().cells[self.lane_cell] = Some(('│', self.color));
        }
        patches.entry(self.tip).or_default().cells[self.lane_cell] = Some(('↓', self.color));
        patches.entry(self.result).or_default().rule = Some((RULE_ENTERING_RESULT, self.color));
    }
}

fn rows(area: &CallArea) -> Range<usize> {
    area.start_line..area.end_line
}

/// The glyphs every connector wants in one row: lane cells of the label
/// column, and the rule's text and colour where a connector leaves, enters,
/// or runs beside a call.
#[derive(Default)]
struct RowPatch {
    cells: [Option<(char, Rgb)>; NAME_WIDTH],
    rule: Option<(&'static str, Rgb)>,
}

pub(super) fn draw_connectors<'a>(
    lines: &mut [RenderedLine],
    calls: impl Iterator<Item = &'a CallRange>,
    show_timing: bool,
) {
    let mut patches: BTreeMap<usize, RowPatch> = BTreeMap::new();
    for call in calls {
        color_call_rules(call, &mut patches);
        if let Some(connector) = Connector::of(call) {
            connector.add_to(&mut patches);
        }
    }
    // A row's spans are `[timing?] [label column] [rule] [content…]`.
    let label_index = usize::from(show_timing);
    for (row, patch) in patches {
        paint_row(&mut lines[row], &patch, label_index);
    }
}

/// Colour the rule of every row of the call; its connector, added after,
/// replaces the rule on the rows it leaves and enters. A call open alone
/// is recoloured too: its input rows carry the run's dimmed rule, lighter
/// than the rule of its result and of the rows around the run, and its
/// connector is to match all of them.
fn color_call_rules(call: &CallRange, patches: &mut BTreeMap<usize, RowPatch>) {
    let color = call_color(call);
    for row in call.areas().flat_map(rows) {
        patches.entry(row).or_default().rule = Some((RULE, color));
    }
}

fn paint_row(line: &mut RenderedLine, patch: &RowPatch, label_index: usize) {
    if line.spans.is_empty() {
        // A blank row between blocks: give it the columns the lane needs.
        let timing_pad = (" ".repeat(TIMESTAMP_WIDTH), LineStyle::default());
        line.spans
            .extend(std::iter::repeat_n(timing_pad, label_index));
        line.spans.extend(label_column_spans(
            &[' '; NAME_WIDTH],
            &LineStyle::default(),
            &patch.cells,
        ));
        return;
    }
    if let Some((text, color)) = patch.rule
        && let Some(span) = line.spans.get_mut(label_index + 1)
        && span.0 == RULE
    {
        *span = (text.to_string(), LineStyle::colored(color));
    }
    paint_label_column(line, label_index, &patch.cells);
}

/// Write lane glyphs into the row's label column where its cells are
/// blank; a label's own characters stay.
fn paint_label_column(
    line: &mut RenderedLine,
    label_index: usize,
    lane_cells: &[Option<(char, Rgb)>; NAME_WIDTH],
) {
    let Some((text, base)) = line.spans.get(label_index) else {
        return;
    };
    let chars: Vec<char> = text.chars().collect();
    let Ok(chars) = <[char; NAME_WIDTH]>::try_from(chars) else {
        return;
    };
    let base = base.clone();
    let replacement = label_column_spans(&chars, &base, lane_cells);
    line.spans.splice(label_index..=label_index, replacement);
}

fn label_column_spans(
    chars: &[char; NAME_WIDTH],
    base: &LineStyle,
    lane_cells: &[Option<(char, Rgb)>; NAME_WIDTH],
) -> Vec<(String, LineStyle)> {
    let mut spans: Vec<(String, LineStyle)> = Vec::new();
    for (cell, &original) in chars.iter().enumerate() {
        let (glyph, style) = match lane_cells[cell] {
            Some((glyph, color)) if original == ' ' => (glyph, LineStyle::colored(color)),
            _ => (original, base.clone()),
        };
        match spans.last_mut() {
            Some((text, last)) if *last == style => text.push(glyph),
            _ => spans.push((glyph.to_string(), style)),
        }
    }
    spans
}
