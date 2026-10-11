//! Line-by-line transcript parsing that spreads the per-line parse across the
//! rayon pool while the caller folds the results in file order.

use crate::parallel::ParallelThreshold;
use rayon::prelude::*;
use std::io::{self, BufRead};

/// Lines of text gathered before their parses run together. Bounds the text
/// and parsed values held at once for a large transcript.
const LINE_BATCH_BYTES: usize = 8 << 20;

/// Lines per rayon task: a line's parse is short, so splitting finer costs
/// more than it saves.
const LINES_PER_TASK: usize = 64;

/// Hands `visit` each line of `reader` that holds more than whitespace, in
/// file order: its line number, counted from `first_line_number`, the line
/// without its line ending, and what `parse` made of it. `parse` runs one
/// batch of lines at a time, on the rayon pool for a batch `parallel` admits;
/// `visit` runs on the calling thread, so it can carry state from line to
/// line.
pub(crate) fn visit_parsed_lines<T: Send>(
    mut reader: impl BufRead,
    first_line_number: usize,
    parallel: ParallelThreshold,
    parse: impl Fn(&str) -> T + Sync,
    mut visit: impl FnMut(usize, String, T),
) -> io::Result<()> {
    let mut batch: Vec<(usize, String)> = Vec::new();
    let mut batch_bytes = 0;
    let mut line_number = first_line_number;
    let mut flush = |batch: &mut Vec<(usize, String)>| {
        let parsed = if parallel.runs_in_parallel(batch.len()) {
            batch
                .par_iter()
                .with_min_len(LINES_PER_TASK)
                .map(|(_, line)| parse(line))
                .collect::<Vec<_>>()
        } else {
            batch
                .iter()
                .map(|(_, line)| parse(line))
                .collect::<Vec<_>>()
        };
        for ((number, line), value) in batch.drain(..).zip(parsed) {
            visit(number, line, value);
        }
    };
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        let number = line_number;
        line_number += 1;
        if line.ends_with('\n') {
            line.pop();
            if line.ends_with('\r') {
                line.pop();
            }
        }
        if line.trim().is_empty() {
            continue;
        }
        batch_bytes += line.len();
        batch.push((number, line));
        if batch_bytes >= LINE_BATCH_BYTES {
            flush(&mut batch);
            batch_bytes = 0;
        }
    }
    flush(&mut batch);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn visited(text: &str, first_line_number: usize) -> Vec<(usize, String, usize)> {
        let mut lines = Vec::new();
        visit_parsed_lines(
            text.as_bytes(),
            first_line_number,
            ParallelThreshold::DEFAULT,
            str::len,
            |number, line, length| {
                lines.push((number, line, length));
            },
        )
        .unwrap();
        lines
    }

    #[test]
    fn lines_arrive_in_file_order_with_their_numbers_and_parses() {
        let text = (0..1000)
            .map(|index| format!("line {index}\n"))
            .collect::<String>();

        let lines = visited(&text, 1);

        assert_eq!(lines.len(), 1000);
        for (index, (number, line, length)) in lines.iter().enumerate() {
            assert_eq!(*number, index + 1);
            assert_eq!(*line, format!("line {index}"));
            assert_eq!(*length, line.len());
        }
    }

    #[test]
    fn blank_lines_are_skipped_but_still_numbered() {
        let lines = visited("first\n\n  \nsecond\r\nthird", 5);

        assert_eq!(
            lines,
            [
                (5, "first".to_owned(), 5),
                (8, "second".to_owned(), 6),
                (9, "third".to_owned(), 5),
            ]
        );
    }

    #[test]
    fn lines_beyond_one_batch_keep_their_order() {
        let line = "x".repeat(1 << 20);
        let text = (0..20)
            .map(|index| format!("{index}{line}\n"))
            .collect::<String>();

        let lines = visited(&text, 1);

        let firsts = lines
            .iter()
            .map(|(_, line, _)| line.trim_end_matches('x').to_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            firsts,
            (0..20).map(|index| index.to_string()).collect::<Vec<_>>()
        );
    }
}
