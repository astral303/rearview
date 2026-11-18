//! Matches the search texts held in memory in chunks of about equal cost
//! across threads, whichever session holds them, then adds each session's
//! counts together, so one large session doesn't keep one thread busy while
//! the others wait.

use super::in_memory::SessionSearchTexts;
use super::raw::{ADJACENCY_LIMIT, Counts, PREFIX_LIMIT, RawQuery};
use rayon::prelude::*;
use std::collections::HashMap;
use std::ops::Range;

/// Chunks per thread: texts differ in size, so more chunks than threads
/// keep every thread busy until the last chunk ends.
const CHUNKS_PER_THREAD: usize = 4;
/// The least cost a chunk holds, so a search of little text is not split
/// into chunks costing more to schedule than to match.
const MIN_CHUNK_COST: usize = 256 * 1024;
/// A text's cost beyond its bytes, in bytes: counting a text of a few bytes
/// takes about as long as scanning 200 to 400 bytes of a long one, as
/// measured with the list's search counting words and quoted text over
/// sessions of long texts and of very short ones. Without it, a session of
/// many short texts fits in one chunk and keeps one thread busy after the
/// others finish.
const COST_PER_TEXT: usize = 256;

/// The cost of counting a text of `length` bytes, in bytes.
fn text_cost(length: usize) -> usize {
    length + COST_PER_TEXT
}

impl Counts {
    /// Adds `other`, the counts of other texts of the same session, as
    /// counting those texts after these would.
    pub(super) fn add(&mut self, other: &Counts) {
        for (found, other) in self.contains.iter_mut().zip(&other.contains) {
            *found |= *other;
        }
        for (count, other) in self.prefixes.iter_mut().zip(&other.prefixes) {
            *count = (*count + other).min(PREFIX_LIMIT);
        }
        self.adjacency = (self.adjacency + other.adjacency).min(ADJACENCY_LIMIT);
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
    let total_cost: usize = sessions
        .iter()
        .flat_map(|(_, texts)| texts.text_lengths())
        .map(text_cost)
        .sum();
    let chunk_cost =
        (total_cost / (rayon::current_num_threads() * CHUNKS_PER_THREAD)).max(MIN_CHUNK_COST);
    count_in_chunks(sessions, query, chunk_cost)
}

/// [`count_held`], each chunk holding texts costing at least `chunk_cost`,
/// or the rest.
fn count_in_chunks(
    sessions: &[(usize, &SessionSearchTexts)],
    query: &RawQuery<'_>,
    chunk_cost: usize,
) -> HashMap<usize, Counts> {
    let matcher = query.matcher();
    let mut counts: HashMap<usize, Counts> = sessions
        .iter()
        .map(|(index, _)| (*index, matcher.counts()))
        .collect();
    let chunk_counts = chunks(sessions, chunk_cost)
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

/// The texts of `sessions` in order, split into chunks once each contains
/// texts costing `chunk_cost`.
fn chunks<'t>(
    sessions: &[(usize, &'t SessionSearchTexts)],
    chunk_cost: usize,
) -> Vec<Vec<Run<'t>>> {
    let mut chunks = Vec::new();
    let mut chunk = Vec::new();
    let mut cost = 0;
    for &(index, texts) in sessions {
        let mut start = 0;
        let lengths = texts.text_lengths();
        let count = lengths.len();
        for (position, length) in lengths.enumerate() {
            cost += text_cost(length);
            if cost >= chunk_cost {
                chunk.push(Run {
                    index,
                    texts,
                    positions: start..position + 1,
                });
                chunks.push(std::mem::take(&mut chunk));
                cost = 0;
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
        let held =
            SessionSearchTexts::read(sessions.rows[0].cached_session.as_ref().unwrap()).unwrap();
        let literals = [Literal::new("a needle".to_owned())];
        let pairs = ["word word".to_owned()];
        let query = RawQuery::new(&["word", "needle", "missing"], &pairs, &literals);
        let sessions = [(7, &held)];

        let one_thread = summary(&count_in_chunks(&sessions, &query, usize::MAX)[&7]);
        assert_eq!(
            (&one_thread.1, one_thread.2),
            (&vec![PREFIX_LIMIT, 1, 0], ADJACENCY_LIMIT),
            "the session's counts reach their limits"
        );
        for chunk_cost in [1, 5, 12, 30, COST_PER_TEXT + 30, 3 * COST_PER_TEXT] {
            assert!(
                chunks(&sessions, chunk_cost).len() > 1,
                "the session spans several chunks costing {chunk_cost}"
            );
            assert_eq!(
                summary(&count_in_chunks(&sessions, &query, chunk_cost)[&7]),
                one_thread,
                "chunks costing {chunk_cost} give one thread's counts"
            );
        }
    }

    #[test]
    fn sessions_of_short_texts_spread_over_chunks_by_their_count_and_count_as_one_thread_does() {
        let short_texts = |word: &str| {
            (0..12)
                .map(|hour| claude_user_record(&format!("{word} {hour}"), october(1, hour)))
                .collect::<Vec<_>>()
        };
        let cached = cached_claude_sessions(&[
            ("first", short_texts("needle")),
            ("second", short_texts("haystack needle")),
        ]);
        let held = cached
            .rows
            .iter()
            .map(|row| SessionSearchTexts::read(row.cached_session.as_ref().unwrap()).unwrap())
            .collect::<Vec<_>>();
        let sessions = [(0, &held[0]), (1, &held[1])];
        let bytes: usize = held.iter().flat_map(|texts| texts.text_lengths()).sum();
        let chunk_cost = 4 * COST_PER_TEXT;
        assert!(
            bytes < chunk_cost && chunks(&sessions, chunk_cost).len() > 2,
            "{bytes} bytes of text, less than one chunk's cost, spread over several chunks by their count"
        );

        let literals = [Literal::new("needle 1".to_owned())];
        let pairs = ["haystack needle".to_owned()];
        for query in [
            RawQuery::new(&["needle", "haystack"], &pairs, &[]),
            RawQuery::new(&[], &[], &literals),
        ] {
            let one_thread = count_in_chunks(&sessions, &query, usize::MAX);
            let spread = count_in_chunks(&sessions, &query, chunk_cost);
            for index in [0, 1] {
                assert_eq!(
                    summary(&spread[&index]),
                    summary(&one_thread[&index]),
                    "session {index} counts as one thread counts it"
                );
            }
        }
    }
}
