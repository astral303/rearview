//! The viewer's `/` search over its rendered rows.

use super::RenderedLine;
use super::output::RowContinuation;
use super::rerender::ReplacedRows;
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

    fn move_rows(&mut self, moved: impl Fn(usize) -> usize) {
        for range in &mut self.rows {
            range.row = moved(range.row);
        }
    }
}

/// The rows a search covers between two calls of its `keep_going`.
const ROWS_BETWEEN_STOP_CHECKS: usize = 1024;

/// [`find_matches_while`] to its end.
pub fn find_matches(lines: &[RenderedLine], query: &str) -> Vec<SearchMatch> {
    find_matches_while(lines, query, &|| true).expect("a search that keeps going finishes")
}

/// Update `matches`, the matches of `query` in the rows before `replaced`,
/// to the matches in `lines`, the rows after it: drop the matches in the
/// old rows, move the later ones by the change in rows, and search the new
/// rows alone. Expects a wrapped line to start at the first replaced row
/// and at the row after the last, as at an entry's rows: a match across
/// either edge isn't updated.
pub fn update_matches_in_replaced_rows(
    matches: &mut Vec<SearchMatch>,
    lines: &[RenderedLine],
    query: &str,
    replaced: &ReplacedRows,
) {
    let old_first = matches.partition_point(|found| found.first_row() < replaced.old.start);
    let old_end = matches.partition_point(|found| found.first_row() < replaced.old.end);
    for found in &mut matches[old_end..] {
        found.move_rows(|row| replaced.moved(row));
    }
    let new_rows = replaced.new_rows();
    let new_first_row = new_rows.start;
    let new_matches = find_matches(&lines[new_rows], query)
        .into_iter()
        .map(|mut found| {
            found.move_rows(|row| new_first_row + row);
            found
        });
    matches.splice(old_first..old_end, new_matches);
}

/// Every match of `query` in `lines`, top to bottom. The rows of a wrapped
/// line are searched as the line they were cut from, so one match can cover
/// several rows. Case is ignored, and a run of whitespace in `query` matches
/// any run of whitespace in the text.
///
/// Calls `keep_going` before the first row, then at the first wrapped line
/// starting [`ROWS_BETWEEN_STOP_CHECKS`] rows or more after the last call;
/// `None` once it returns false.
pub fn find_matches_while(
    lines: &[RenderedLine],
    query: &str,
    keep_going: &dyn Fn() -> bool,
) -> Option<Vec<SearchMatch>> {
    let literal = Literal::ignoring_case(query.to_string());
    let mut matches = Vec::new();
    let mut line = WrappedLine::default();
    let mut start = 0;
    let mut next_check = 0;
    while start < lines.len() {
        if start >= next_check {
            if !keep_going() {
                return None;
            }
            next_check = start + ROWS_BETWEEN_STOP_CHECKS;
        }
        let end = wrapped_line_end(lines, start);
        line.join(lines, start..end);
        matches.extend(
            literal
                .match_ranges(&line.text)
                .into_iter()
                .filter_map(|(match_start, match_end)| line.match_rows(match_start..match_end)),
        );
        start = end;
    }
    Some(matches)
}

/// The row after the wrapped line that starts at row `start`.
fn wrapped_line_end(lines: &[RenderedLine], start: usize) -> usize {
    (start + 1..lines.len())
        .find(|&row| lines[row].continuation == RowContinuation::None)
        .unwrap_or(lines.len())
}

/// The rows of one wrapped line joined back into its text.
#[derive(Default)]
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
    /// Replaces the line with `rows` of `lines` joined: the first row whole,
    /// then each later row's continued text, after a space where the line
    /// broke at one.
    fn join(&mut self, lines: &[RenderedLine], rows: Range<usize>) {
        self.text.clear();
        self.pieces.clear();
        for row in rows.clone() {
            let line = &lines[row];
            let continued_from = if row == rows.start {
                0
            } else {
                if line.continuation == RowContinuation::AfterSpace {
                    self.text.push(' ');
                }
                line.text_len().saturating_sub(line.continued_text_len)
            };
            let start = self.text.len();
            let row_start = push_row_text_from(&mut self.text, line, continued_from);
            self.pieces.push(Piece {
                row,
                start,
                row_start,
                len: self.text.len() - start,
            });
        }
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

/// Appends `row`'s text to `text` from byte `from`, or from the start of
/// the character `from` falls inside. Returns the byte offset in the row's
/// text that the appended text starts at.
fn push_row_text_from(text: &mut String, row: &RenderedLine, from: usize) -> usize {
    let mut span_start = 0;
    let mut row_start = None;
    for (span, _) in &row.spans {
        let span_end = span_start + span.len();
        if span_end > from {
            let skipped = span.floor_char_boundary(from.saturating_sub(span_start));
            row_start.get_or_insert(span_start + skipped);
            text.push_str(&span[skipped..]);
        }
        span_start = span_end;
    }
    row_start.unwrap_or(span_start)
}

/// The matches of `query` with each wrapped line joined from its rows'
/// [`RenderedLine::text`], for tests to compare [`find_matches`] with.
#[cfg(test)]
pub(super) fn find_matches_by_row_text(lines: &[RenderedLine], query: &str) -> Vec<SearchMatch> {
    let literal = Literal::ignoring_case(query.to_string());
    let mut matches = Vec::new();
    let mut start = 0;
    while start < lines.len() {
        let end = wrapped_line_end(lines, start);
        let line = join_row_texts(lines, start..end);
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

#[cfg(test)]
fn join_row_texts(lines: &[RenderedLine], rows: Range<usize>) -> WrappedLine {
    let mut joined = WrappedLine::default();
    for row in rows.clone() {
        let line = &lines[row];
        let row_text = line.text();
        let row_start = if row == rows.start {
            0
        } else {
            if line.continuation == RowContinuation::AfterSpace {
                joined.text.push(' ');
            }
            row_text.floor_char_boundary(row_text.len().saturating_sub(line.continued_text_len))
        };
        joined.pieces.push(Piece {
            row,
            start: joined.text.len(),
            row_start,
            len: row_text.len() - row_start,
        });
        joined.text.push_str(&row_text[row_start..]);
    }
    joined
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

    fn row_of_spans(texts: &[&str]) -> RenderedLine {
        RenderedLine::new(
            texts
                .iter()
                .map(|text| (text.to_string(), LineStyle::default()))
                .collect(),
        )
    }

    /// A row of `texts` continuing the row above: its last
    /// `continued_text_len` bytes continue it.
    fn continued_spans(
        texts: &[&str],
        continued_text_len: usize,
        continuation: RowContinuation,
    ) -> RenderedLine {
        let mut line = row_of_spans(texts);
        line.continuation = continuation;
        line.continued_text_len = continued_text_len;
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
    fn phrase_broken_at_space_matches_across_two_rows() {
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
    fn word_cut_inside_matches_whole_across_two_rows() {
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
    fn space_kept_on_row_above_joins_once() {
        let lines = [
            row("cache "),
            continued("> ", "rebuild", RowContinuation::Unbroken),
        ];
        assert_eq!(covered(&lines, "cache rebuild").len(), 1);
    }

    #[test]
    fn next_line_doesnt_continue_line_above() {
        assert!(find_matches(&[row("cach"), row("e")], "cache").is_empty());
    }

    #[test]
    fn phrase_matches_across_rows_of_several_spans_each() {
        let lines = [
            row_of_spans(&[" You ", "│ ", "run the ", "cac", "he"]),
            continued_spans(
                &["     ", "│ re", "bui", "ld now"],
                "rebuild now".len(),
                RowContinuation::AfterSpace,
            ),
            continued_spans(
                &["     │ ", "and ", "again"],
                "and again".len(),
                RowContinuation::AfterSpace,
            ),
        ];
        assert_eq!(
            covered(&lines, "cache rebuild now and"),
            [vec![
                (0, "cache".to_string()),
                (1, "rebuild now".to_string()),
                (2, "and".to_string()),
            ]]
        );
        assert!(
            find_matches(&lines, "│ re").is_empty(),
            "the columns sharing a span with the continued text are not searched"
        );
    }

    #[test]
    fn continued_row_cut_inside_character_continues_from_its_start() {
        let continued_text = "ébc";
        for spans in [&["│ ", continued_text][..], &["│ ébc"], &["│", " é", "bc"]] {
            let lines = [
                row("caf"),
                continued_spans(spans, continued_text.len() - 1, RowContinuation::Unbroken),
            ];
            assert_eq!(
                covered(&lines, "café"),
                [vec![(0, "caf".to_string()), (1, "é".to_string())]],
                "the continued text starts at the cut character: {spans:?}"
            );
        }
    }

    /// A search over `rows` lines of one row each, whose `keep_going` returns
    /// each of `answers` in turn and then true: the count of `keep_going`
    /// calls, and whether the search finished.
    fn stop_checks(rows: usize, answers: &[bool]) -> (usize, bool) {
        let lines = vec![row("cache"); rows];
        let calls = std::cell::Cell::new(0);
        let finished = find_matches_while(&lines, "cache", &|| {
            let call = calls.get();
            calls.set(call + 1);
            answers.get(call).copied().unwrap_or(true)
        })
        .is_some();
        (calls.get(), finished)
    }

    #[test]
    fn search_checks_whether_to_keep_going_before_each_interval_of_rows() {
        assert_eq!(
            stop_checks(3 * ROWS_BETWEEN_STOP_CHECKS + 1, &[]),
            (4, true),
            "one check before the first row, then one per interval"
        );
    }

    #[test]
    fn stopped_search_doesnt_search_past_next_interval() {
        assert_eq!(
            stop_checks(3 * ROWS_BETWEEN_STOP_CHECKS + 1, &[true, false]),
            (2, false),
            "the search ends at the check that answers false"
        );
        assert_eq!(
            stop_checks(1, &[false]),
            (1, false),
            "a search whose first check answers false ends before its first row"
        );
    }

    #[test]
    fn match_over_no_rows_isnt_built() {
        assert_eq!(SearchMatch::covering(Vec::new()), None);
    }

    #[test]
    fn phrase_across_line_break_doesnt_match() {
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
    fn columns_of_continued_row_are_not_searched() {
        let lines = [
            row("first"),
            continued(" 12:00 ", "second", RowContinuation::AfterSpace),
        ];
        assert!(find_matches(&lines, "12:00").is_empty());
        assert_eq!(covered(&[row(" 12:00 first")], "12:00").len(), 1);
    }
}
