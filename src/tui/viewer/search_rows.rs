//! The viewer's `/` search over its rendered rows.

use super::RenderedLine;
use super::output::RowContinuation;
use crate::search::literal::Literal;
use std::ops::Range;

/// One match of the viewer's search: the part of each row it covers, in
/// row order. It covers at least one row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchMatch {
    rows: Vec<RowRange>,
}

/// The bytes of one row's text that a match covers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RowRange {
    pub row: usize,
    pub bytes: Range<usize>,
}

impl SearchMatch {
    /// The match covering `rows`; `None` when it covers none.
    fn covering(rows: Vec<RowRange>) -> Option<Self> {
        (!rows.is_empty()).then_some(Self { rows })
    }

    pub fn rows(&self) -> &[RowRange] {
        &self.rows
    }

    /// The row `n` and `N` move to.
    pub fn first_row(&self) -> usize {
        self.rows[0].row
    }

    pub fn last_row(&self) -> usize {
        self.rows[self.rows.len() - 1].row
    }
}

/// Every match of `query` in `lines`, top to bottom. The rows of a wrapped
/// line are searched as the line they were cut from, so one match can cover
/// several rows. Case is ignored, and a run of whitespace in `query` matches
/// any run of whitespace in the text.
pub fn find_matches(lines: &[RenderedLine], query: &str) -> Vec<SearchMatch> {
    let literal = Literal::ignoring_case(query.to_string());
    let mut matches = Vec::new();
    let mut start = 0;
    while start < lines.len() {
        let end = wrapped_line_end(lines, start);
        let line = WrappedLine::join(lines, start..end);
        matches.extend(
            literal
                .match_ranges(&line.text)
                .into_iter()
                .filter_map(|(match_start, match_end)| line.match_rows(match_start..match_end)),
        );
        start = end;
    }
    matches
}

/// The row after the wrapped line that starts at row `start`.
fn wrapped_line_end(lines: &[RenderedLine], start: usize) -> usize {
    (start + 1..lines.len())
        .find(|&row| lines[row].continuation == RowContinuation::None)
        .unwrap_or(lines.len())
}

/// The rows of one wrapped line joined back into its text.
struct WrappedLine {
    text: String,
    pieces: Vec<Piece>,
}

/// A row's part of a wrapped line: `len` bytes of the line's text from
/// `start`, which are the row's text from `row_start`.
struct Piece {
    row: usize,
    start: usize,
    row_start: usize,
    len: usize,
}

impl WrappedLine {
    /// The first row whole, then each later row's continued text, after a
    /// space where the line broke at one.
    fn join(lines: &[RenderedLine], rows: Range<usize>) -> Self {
        let mut text = String::new();
        let mut pieces = Vec::new();
        for row in rows.clone() {
            let line = &lines[row];
            let row_text = line.text();
            let row_start = if row == rows.start {
                0
            } else {
                if line.continuation == RowContinuation::AfterSpace {
                    text.push(' ');
                }
                char_boundary_at_or_before(
                    &row_text,
                    row_text.len().saturating_sub(line.continued_text_len),
                )
            };
            pieces.push(Piece {
                row,
                start: text.len(),
                row_start,
                len: row_text.len() - row_start,
            });
            text.push_str(&row_text[row_start..]);
        }
        Self { text, pieces }
    }

    /// The match covering `bytes` of the line's text, by row. `None` when
    /// it covers only a space the rows do not show.
    fn match_rows(&self, bytes: Range<usize>) -> Option<SearchMatch> {
        let rows: Vec<RowRange> = self
            .pieces
            .iter()
            .filter_map(|piece| {
                let start = bytes.start.max(piece.start);
                let end = bytes.end.min(piece.start + piece.len);
                (start < end).then(|| RowRange {
                    row: piece.row,
                    bytes: piece.row_start + start - piece.start
                        ..piece.row_start + end - piece.start,
                })
            })
            .collect();
        SearchMatch::covering(rows)
    }
}

fn char_boundary_at_or_before(text: &str, mut index: usize) -> usize {
    while !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::viewer::LineStyle;

    fn row(text: &str) -> RenderedLine {
        RenderedLine::new(vec![(text.to_string(), LineStyle::default())])
    }

    /// A row continuing the one above: `columns` stand in for the ledger
    /// columns and indent, which the joined line leaves out.
    fn continued(columns: &str, text: &str, continuation: RowContinuation) -> RenderedLine {
        let mut line = RenderedLine::new(vec![
            (columns.to_string(), LineStyle::default()),
            (text.to_string(), LineStyle::default()),
        ]);
        line.continuation = continuation;
        line.continued_text_len = text.len();
        line
    }

    fn covered(lines: &[RenderedLine], query: &str) -> Vec<Vec<(usize, String)>> {
        find_matches(lines, query)
            .into_iter()
            .map(|found| {
                found
                    .rows()
                    .iter()
                    .map(|range| {
                        (
                            range.row,
                            lines[range.row].text()[range.bytes.clone()].to_string(),
                        )
                    })
                    .collect()
            })
            .collect()
    }

    #[test]
    fn a_phrase_broken_at_a_space_matches_across_the_two_rows() {
        let lines = [
            row(" You │ run the cache"),
            continued("     │ ", "rebuild now", RowContinuation::AfterSpace),
        ];
        assert_eq!(
            covered(&lines, "cache rebuild"),
            [vec![(0, "cache".to_string()), (1, "rebuild".to_string())]]
        );
    }

    #[test]
    fn a_word_cut_inside_matches_whole_across_the_two_rows() {
        let lines = [
            row("C:\\Users\\micro\\rearv"),
            continued("  ", "iew\\src\\x.rs", RowContinuation::Unbroken),
        ];
        assert_eq!(
            covered(&lines, "rearview\\src"),
            [vec![(0, "rearv".to_string()), (1, "iew\\src".to_string())]]
        );
    }

    #[test]
    fn a_space_kept_on_the_row_above_joins_once() {
        let lines = [
            row("cache "),
            continued("> ", "rebuild", RowContinuation::Unbroken),
        ];
        assert_eq!(covered(&lines, "cache rebuild").len(), 1);
    }

    #[test]
    fn a_match_over_no_rows_isnt_built() {
        assert_eq!(SearchMatch::covering(Vec::new()), None);
    }

    #[test]
    fn a_phrase_across_a_line_break_doesnt_match() {
        let lines = [row("run the cache"), row("rebuild now")];
        assert!(find_matches(&lines, "cache rebuild").is_empty());
    }

    #[test]
    fn each_hit_on_one_row_is_its_own_match_in_order() {
        let lines = [row("cache and cache"), row("Cache")];
        let rows: Vec<usize> = find_matches(&lines, "cache")
            .iter()
            .map(SearchMatch::first_row)
            .collect();
        assert_eq!(rows, [0, 0, 1]);
    }

    #[test]
    fn the_columns_of_a_continued_row_are_not_searched() {
        let lines = [
            row("first"),
            continued(" 12:00 ", "second", RowContinuation::AfterSpace),
        ];
        assert!(find_matches(&lines, "12:00").is_empty());
        assert_eq!(covered(&[row(" 12:00 first")], "12:00").len(), 1);
    }
}
