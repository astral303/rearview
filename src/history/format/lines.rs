//! Line-by-line transcript parsing that spreads the per-line parse across the
//! rayon pool while the caller folds the results in file order, and the
//! resumable reading every line-delimited format shares: a cursor after the
//! last complete record, and a check that the bytes before it are unchanged.

use crate::error::Result;
use crate::history::provider::TranscriptRead;
use crate::parallel::ParallelThreshold;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::{self, BufRead, Read, Seek, SeekFrom};
use std::path::Path;

/// Lines of text gathered before their parses run together. Bounds the text
/// and parsed values held at once for a large transcript.
const LINE_BATCH_BYTES: usize = 8 << 20;

/// Lines per rayon task: a line's parse is short, so splitting finer costs
/// more than it saves.
const LINES_PER_TASK: usize = 64;

/// The bytes just before a cursor that its rewrite check hashes: about one
/// record's worth.
pub(crate) const REWRITE_CHECK_BYTES: u64 = 4096;

/// A place between two lines of a transcript: the byte just past a line
/// ending, and the number of the line that starts there.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct LineCursor {
    pub(crate) offset: u64,
    pub(crate) line: usize,
}

impl LineCursor {
    pub(crate) const START: Self = Self { offset: 0, line: 1 };

    /// The cursor as a [`TranscriptRead`] stores it.
    pub(crate) fn encode(self) -> Vec<u8> {
        serde_json::to_vec(&self).expect("a cursor serializes")
    }

    /// The cursor [`Self::encode`] stored, or `None` for bytes it did not
    /// write, such as the full read's empty cursor.
    pub(crate) fn decode(bytes: &[u8]) -> Option<Self> {
        serde_json::from_slice(bytes).ok()
    }
}

/// The end of a read of lines.
pub(crate) struct LinesRead {
    /// Past the last line that ends in a line ending.
    pub(crate) cursor: LineCursor,
    /// The last line, with its number, when it holds more than whitespace
    /// and doesn't have a line ending: a record still being written. It is
    /// not visited, so the caller can keep its state at `cursor` first.
    pub(crate) unterminated: Option<(usize, String)>,
}

/// Hands `visit` each line of `reader` that holds more than whitespace, in
/// file order: its line number, counted from `first_line_number`, the line
/// without its line ending, and what `parse` made of it. `parse` runs one
/// batch of lines at a time, on the rayon pool for a batch `parallel` admits;
/// `visit` runs on the calling thread, so it can carry state from line to
/// line.
pub(crate) fn visit_parsed_lines<T: Send>(
    reader: impl BufRead,
    first_line_number: usize,
    parallel: ParallelThreshold,
    parse: impl Fn(&str) -> T + Sync,
    mut visit: impl FnMut(usize, String, T),
) -> io::Result<()> {
    let from = LineCursor {
        offset: 0,
        line: first_line_number,
    };
    let read = visit_lines_from(reader, from, parallel, &parse, &mut visit)?;
    if let Some((number, line)) = read.unterminated {
        let value = parse(&line);
        visit(number, line, value);
    }
    Ok(())
}

/// [`visit_parsed_lines`] for `reader` positioned at `from`, except the last
/// line when it doesn't have a line ending: the result holds that line
/// instead, and the cursor past the last line that has one.
pub(crate) fn visit_lines_from<T: Send>(
    reader: impl BufRead,
    from: LineCursor,
    parallel: ParallelThreshold,
    parse: impl Fn(&str) -> T + Sync,
    mut visit: impl FnMut(usize, String, T),
) -> io::Result<LinesRead> {
    visit_line_spans_from(reader, from, parallel, parse, |span, line, value| {
        visit(span.start.line, line, value)
    })
}

/// A line's place in its transcript: the cursor at its start, and the
/// cursor past its line ending.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct LineSpan {
    pub(crate) start: LineCursor,
    pub(crate) end: LineCursor,
}

/// [`visit_lines_from`], handing `visit` each line's span instead of its
/// number.
pub(crate) fn visit_line_spans_from<T: Send>(
    mut reader: impl BufRead,
    from: LineCursor,
    parallel: ParallelThreshold,
    parse: impl Fn(&str) -> T + Sync,
    mut visit: impl FnMut(LineSpan, String, T),
) -> io::Result<LinesRead> {
    let mut batch: Vec<(LineSpan, String)> = Vec::new();
    let mut batch_bytes = 0;
    let mut cursor = from;
    let mut unterminated = None;
    let mut flush = |batch: &mut Vec<(LineSpan, String)>| {
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
        for ((span, line), value) in batch.drain(..).zip(parsed) {
            visit(span, line, value);
        }
    };
    loop {
        let mut line = String::new();
        let length = reader.read_line(&mut line)?;
        if length == 0 {
            break;
        }
        let start = cursor;
        let number = cursor.line;
        if line.ends_with('\n') {
            line.pop();
            if line.ends_with('\r') {
                line.pop();
            }
        } else {
            // `read_line` stops without a line ending only at the end.
            if !line.trim().is_empty() {
                unterminated = Some((number, line));
            }
            break;
        }
        cursor = LineCursor {
            offset: cursor.offset + length as u64,
            line: number + 1,
        };
        if line.trim().is_empty() {
            continue;
        }
        batch_bytes += line.len();
        batch.push((LineSpan { start, end: cursor }, line));
        if batch_bytes >= LINE_BATCH_BYTES {
            flush(&mut batch);
            batch_bytes = 0;
        }
    }
    flush(&mut batch);
    Ok(LinesRead {
        cursor,
        unterminated,
    })
}

/// The rewrite check of the transcript `file` holds, for a read that stopped
/// at `cursor`: a hash of the bytes just before it.
pub(crate) fn rewrite_check(
    file: &mut (impl Read + Seek),
    cursor: LineCursor,
) -> io::Result<Vec<u8>> {
    rewrite_check_after(file, 0, cursor)
}

/// [`rewrite_check`] of a transcript whose bytes before `from` the agent
/// rewrites in place: the hash leaves them out.
pub(crate) fn rewrite_check_after(
    file: &mut (impl Read + Seek),
    from: u64,
    cursor: LineCursor,
) -> io::Result<Vec<u8>> {
    let start = cursor
        .offset
        .saturating_sub(REWRITE_CHECK_BYTES)
        .max(from)
        .min(cursor.offset);
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::with_capacity((cursor.offset - start) as usize);
    file.take(cursor.offset - start).read_to_end(&mut bytes)?;
    Ok(blake3::hash(&bytes).as_bytes().to_vec())
}

/// The line `span` covers in the transcript at `source`, without its line
/// ending.
pub(crate) fn read_line_at(source: &Path, span: LineSpan) -> io::Result<String> {
    let mut file = File::open(source)?;
    file.seek(SeekFrom::Start(span.start.offset))?;
    let mut bytes = Vec::with_capacity((span.end.offset - span.start.offset) as usize);
    file.take(span.end.offset - span.start.offset)
        .read_to_end(&mut bytes)?;
    let mut line = String::from_utf8(bytes)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    if line.ends_with('\n') {
        line.pop();
        if line.ends_with('\r') {
            line.pop();
        }
    }
    Ok(line)
}

/// True when the transcript at `source` may no longer hold what `read` read
/// before its cursor: the file is shorter than the cursor, or the bytes the
/// rewrite check hashed differ. A same-size rewrite further back is not
/// detected. A cursor this reader did not write counts as rewritten.
pub(crate) fn is_rewritten(source: &Path, read: &TranscriptRead) -> Result<bool> {
    let Some(cursor) = LineCursor::decode(&read.cursor) else {
        return Ok(true);
    };
    let mut file = File::open(source)?;
    if file.metadata()?.len() < cursor.offset {
        return Ok(true);
    }
    Ok(rewrite_check(&mut file, cursor)? != read.rewrite_check)
}

/// The places a test splits `bytes`, a transcript, at: the start, each end
/// of a line, just before each line ending, and a place inside each line
/// after one.
#[cfg(test)]
pub(crate) fn record_splits(bytes: &[u8]) -> Vec<usize> {
    let mut splits = vec![0];
    let mut before_line_endings = Vec::new();
    for (index, _) in bytes.iter().enumerate().filter(|(_, byte)| **byte == b'\n') {
        splits.push(index + 1);
        before_line_endings.push(index);
    }
    let inside = splits
        .iter()
        .map(|split| split + 2)
        .filter(|split| *split < bytes.len())
        .collect::<Vec<_>>();
    splits.extend(inside);
    splits.extend(before_line_endings);
    splits
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::Source;

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

    /// The lines of `bytes` from `from`, read as from a transcript holding
    /// `bytes` from `from` on, and where the read stopped.
    fn lines_from(bytes: &[u8], from: LineCursor) -> (Vec<(usize, String)>, LinesRead) {
        let mut lines = Vec::new();
        let read = visit_lines_from(
            &bytes[from.offset as usize..],
            from,
            ParallelThreshold::DEFAULT,
            |_| (),
            |number, line, ()| lines.push((number, line)),
        )
        .unwrap();
        (lines, read)
    }

    /// Reads `bytes` cut at `split`, then resumes from the cursor over all
    /// of `bytes`, and asserts that the two reads hold what one read of
    /// `bytes` holds.
    fn assert_prefix_and_resumed_read_equal_full_read(name: &str, bytes: &[u8], split: usize) {
        let (full, full_read) = lines_from(bytes, LineCursor::START);
        let (prefix, prefix_read) = lines_from(&bytes[..split], LineCursor::START);
        assert!(
            prefix_read.cursor.offset as usize <= split,
            "{name} at {split}: the cursor stays inside the prefix"
        );

        let (resumed, resumed_read) = lines_from(bytes, prefix_read.cursor);

        let joined = prefix.into_iter().chain(resumed).collect::<Vec<_>>();
        assert_eq!(
            joined, full,
            "{name} at {split}: the prefix and the resumed read hold the full read's lines"
        );
        assert_eq!(
            (resumed_read.cursor, resumed_read.unterminated),
            (full_read.cursor, full_read.unterminated),
            "{name} at {split}: the resumed read stops where the full read stops"
        );
    }

    #[test]
    fn prefix_and_resumed_read_equal_full_read_at_every_split_for_every_log_format() {
        let mut tested = Vec::new();
        for session in crate::history::provider::contract_tests::every_record_sessions() {
            let transcripts = std::iter::once(&session.row.path).chain(&session.row.subagents);
            for path in transcripts.filter(|path| path.is_file()) {
                let bytes = std::fs::read(path).unwrap();
                let name = format!("{} {}", session.row.source.label(), path.display());
                for split in record_splits(&bytes) {
                    assert_prefix_and_resumed_read_equal_full_read(&name, &bytes, split);
                }
                tested.push(session.row.source);
            }
        }
        tested.dedup();
        assert_eq!(
            tested,
            [
                Source::Claude,
                Source::Codex,
                Source::Kimi,
                Source::Grok,
                Source::Pi,
                Source::Omp
            ],
            "every log format's every-record session is read"
        );
    }

    #[test]
    fn resumed_read_keeps_line_numbers_across_blank_lines_and_crlf_endings() {
        let bytes = b"first\r\n\r\n  \nsecond\nthird";
        for split in record_splits(bytes) {
            assert_prefix_and_resumed_read_equal_full_read("blank lines", bytes, split);
        }
    }

    #[test]
    fn unterminated_last_line_is_held_back_and_the_cursor_stays_before_it() {
        let (lines, read) = lines_from(b"first\nsecond", LineCursor::START);

        assert_eq!(lines, [(1, "first".to_owned())]);
        assert_eq!(read.cursor, LineCursor { offset: 6, line: 2 });
        assert_eq!(read.unterminated, Some((2, "second".to_owned())));
    }

    fn written(directory: &Path, bytes: &[u8]) -> std::path::PathBuf {
        let path = directory.join("transcript.jsonl");
        std::fs::write(&path, bytes).unwrap();
        path
    }

    /// The read of `bytes`, written to a file in `directory`, through to its
    /// cursor.
    fn read_through(directory: &Path, bytes: &[u8]) -> TranscriptRead {
        let path = written(directory, bytes);
        let (_, read) = lines_from(bytes, LineCursor::START);
        let mut file = File::open(&path).unwrap();
        TranscriptRead {
            cursor: read.cursor.encode(),
            rewrite_check: rewrite_check(&mut file, read.cursor).unwrap(),
            ..TranscriptRead::full_read()
        }
    }

    #[test]
    fn appended_records_dont_count_as_rewrite() {
        let directory = tempfile::tempdir().unwrap();
        let read = read_through(directory.path(), b"first\nsecond\npartial");

        let path = written(directory.path(), b"first\nsecond\npartial record\nthird\n");

        assert!(!is_rewritten(&path, &read).unwrap());
    }

    #[test]
    fn changed_byte_before_cursor_or_shrunk_file_counts_as_rewrite() {
        let directory = tempfile::tempdir().unwrap();
        let record = "x".repeat(100);
        let bytes = format!("{record}\n{record}\n");
        let read = read_through(directory.path(), bytes.as_bytes());

        let changed = bytes.replacen("xx", "xy", 1);
        let path = written(directory.path(), changed.as_bytes());
        assert!(
            is_rewritten(&path, &read).unwrap(),
            "a changed byte before the cursor is a rewrite"
        );

        let path = written(directory.path(), &bytes.as_bytes()[..bytes.len() - 1]);
        assert!(
            is_rewritten(&path, &read).unwrap(),
            "a file shorter than the cursor is a rewrite"
        );
    }

    #[test]
    fn full_reads_empty_cursor_counts_as_rewrite() {
        let directory = tempfile::tempdir().unwrap();
        let path = written(directory.path(), b"first\n");

        assert!(is_rewritten(&path, &TranscriptRead::full_read()).unwrap());
    }
}
