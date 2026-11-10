//! Matches the search texts held in memory in chunks of about equal size
//! across threads, whichever session holds them, then adds each session's
//! counts together: one large session no longer keeps one thread busy while
//! the others wait.

use super::in_memory::SessionSearchTexts;
use super::raw::{ADJACENCY_CAP, Counts, PREFIX_CAP, RawQuery};
use rayon::prelude::*;
use std::collections::HashMap;
use std::ops::Range;

/// Chunks per thread: texts differ in size, so more chunks than threads
/// keep every thread busy until the last chunk ends.
const CHUNKS_PER_THREAD: usize = 4;
/// The least text a chunk holds, so a search of little text is not split
/// into chunks costing more to schedule than to match.
const MIN_CHUNK_BYTES: usize = 256 * 1024;

impl Counts {
    /// Adds `other`, the counts of other texts of the same session, as
    /// counting those texts after these would.
    pub(super) fn add(&mut self, other: &Counts) {
        for (found, other) in self.contains.iter_mut().zip(&other.contains) {
            *found |= *other;
        }
        for (count, other) in self.prefixes.iter_mut().zip(&other.prefixes) {
            *count = (*count + other).min(PREFIX_CAP);
        }
        self.adjacency = (self.adjacency + other.adjacency).min(ADJACENCY_CAP);
        for (found, other) in self.literals.iter_mut().zip(&other.literals) {
            *found |= *other;
        }
        self.has_text |= other.has_text;
    }
}

/// `query`'s counts over the texts of each of `sessions`, by the index each
/// is paired with.
pub(super) fn count_held(
    sessions: &[(usize, &SessionSearchTexts)],
    query: &RawQuery<'_>,
) -> HashMap<usize, Counts> {
    let total_bytes: usize = sessions
        .iter()
        .map(|(_, texts)| texts.text_lengths().sum::<usize>())
        .sum();
    let chunk_bytes =
        (total_bytes / (rayon::current_num_threads() * CHUNKS_PER_THREAD)).max(MIN_CHUNK_BYTES);
    count_in_chunks(sessions, query, chunk_bytes)
}

/// [`count_held`], each chunk holding at least `chunk_bytes` of text, or the
/// rest.
fn count_in_chunks(
    sessions: &[(usize, &SessionSearchTexts)],
    query: &RawQuery<'_>,
    chunk_bytes: usize,
) -> HashMap<usize, Counts> {
    let matcher = query.matcher();
    let mut counts: HashMap<usize, Counts> = sessions
        .iter()
        .map(|(index, _)| (*index, matcher.counts()))
        .collect();
    let chunk_counts = chunks(sessions, chunk_bytes)
        .into_par_iter()
        .map(|chunk| {
            let mut matcher = query.matcher();
            chunk
                .into_iter()
                .map(|run| {
                    let mut counts = matcher.counts();
                    for text in run.texts.texts_at(run.positions) {
                        matcher.count(text, &mut counts);
                        if matcher.is_complete(&counts) {
                            break;
                        }
                    }
                    (run.index, counts)
                })
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    for (index, run_counts) in chunk_counts.into_iter().flatten() {
        if let Some(session) = counts.get_mut(&index) {
            session.add(&run_counts);
        }
    }
    counts
}

/// Consecutive texts of one session in a chunk.
struct Run<'t> {
    index: usize,
    texts: &'t SessionSearchTexts,
    positions: Range<usize>,
}

/// The texts of `sessions` in order, cut into chunks once each holds
/// `chunk_bytes` of text.
fn chunks<'t>(
    sessions: &[(usize, &'t SessionSearchTexts)],
    chunk_bytes: usize,
) -> Vec<Vec<Run<'t>>> {
    let mut chunks = Vec::new();
    let mut chunk = Vec::new();
    let mut bytes = 0;
    for &(index, texts) in sessions {
        let mut start = 0;
        let lengths = texts.text_lengths();
        let count = lengths.len();
        for (position, length) in lengths.enumerate() {
            bytes += length;
            if bytes >= chunk_bytes {
                chunk.push(Run {
                    index,
                    texts,
                    positions: start..position + 1,
                });
                chunks.push(std::mem::take(&mut chunk));
                bytes = 0;
                start = position + 1;
            }
        }
        if start < count {
            chunk.push(Run {
                index,
                texts,
                positions: start..count,
            });
        }
    }
    if !chunk.is_empty() {
        chunks.push(chunk);
    }
    chunks
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::literal::Literal;
    use crate::search::test_fixtures::{cached_claude_sessions, claude_user_record, october};

    type CountsSummary = (Vec<bool>, Vec<usize>, usize, Vec<bool>, bool);

    fn summary(counts: &Counts) -> CountsSummary {
        (
            counts.contains.clone(),
            counts.prefixes.clone(),
            counts.adjacency,
            counts.literals.clone(),
            counts.has_text,
        )
    }

    #[test]
    fn session_split_into_chunks_counts_as_one_thread_does() {
        let texts = [
            "word word",
            "a needle",
            "word",
            "word, word word",
            "word_word",
            "  ",
            "Word WORD",
            "words",
            "tail word",
        ];
        let records = texts
            .iter()
            .enumerate()
            .map(|(hour, text)| claude_user_record(text, october(1, hour as u32)))
            .collect();
        let sessions = cached_claude_sessions(&[("session", records)]);
        let held = SessionSearchTexts::read(sessions.rows[0].cached_session.as_ref().unwrap())
            .unwrap()
            .unwrap();
        let literals = [Literal::new("a needle".to_owned())];
        let pairs = ["word word".to_owned()];
        let query = RawQuery::new(&["word", "needle", "missing"], &pairs, &literals);
        let sessions = [(7, &held)];

        let one_thread = summary(&count_in_chunks(&sessions, &query, usize::MAX)[&7]);
        assert_eq!(
            (&one_thread.1, one_thread.2),
            (&vec![PREFIX_CAP, 1, 0], ADJACENCY_CAP),
            "the session's counts reach their caps"
        );
        for chunk_bytes in [1, 5, 12, 30] {
            assert!(
                chunks(&sessions, chunk_bytes).len() > 1,
                "the session spans several chunks of {chunk_bytes} bytes"
            );
            assert_eq!(
                summary(&count_in_chunks(&sessions, &query, chunk_bytes)[&7]),
                one_thread,
                "chunks of {chunk_bytes} bytes give one thread's counts"
            );
        }
    }
}
