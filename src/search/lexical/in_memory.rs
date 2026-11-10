//! Each listed session's search texts in the session list's fields, read
//! from the session cache into memory, so a search of the list scans memory
//! instead of reading the cache. The texts stay as the cache stores them.

use crate::error::Result;
use crate::history::Conversation;
use crate::history::message_texts::PADDED_MESSAGE_SEPARATOR;
use crate::history::sqlite_cache::{
    CachedSession, CopyKey, ReadOrder, ScannedRow, SearchedFields, TextVisitor, scan_search_texts,
};
use crate::time_filter::TimeFilter;
use rayon::prelude::*;
use std::borrow::Borrow;
use std::collections::{HashMap, HashSet};
use std::ops::{ControlFlow, Range};
use std::sync::Arc;

/// One session's search texts in the session list's fields, as the session
/// cache stores them: the text outside every message, then each message's
/// texts in message order, each after a separator as `full_text` lays out
/// its parts. The title and the summary are the row's (see
/// [`Conversation::title_and_summary_texts`]).
pub struct SessionSearchTexts {
    joined: Arc<str>,
    /// Each text's byte range in `joined`.
    texts: Box<[Range<usize>]>,
}

impl SessionSearchTexts {
    /// The texts of `cached`'s copy as the cache holds it, whether or not its
    /// transcripts changed since; `None` when the cache no longer holds it.
    pub(crate) fn read(cached: &CachedSession) -> Result<Option<Self>> {
        let copy = cached.clone().ignoring_source_changes().waiting();
        let mut read = TextsInReadOrder::default();
        let found =
            copy.read_search_texts(SearchedFields::List, &TimeFilter::default(), |text| {
                let placement = Placement::of(text.ordinal);
                read.push((placement, text.order), text.text);
                ControlFlow::Continue(())
            })?;
        Ok(found.map(|()| laid_out(vec![read])))
    }

    /// The texts at `positions` in layout order.
    pub(super) fn texts_at(&self, positions: Range<usize>) -> impl Iterator<Item = &str> {
        self.texts[positions]
            .iter()
            .map(|range| &self.joined[range.clone()])
    }

    /// The byte length of each text, in layout order.
    pub(super) fn text_lengths(&self) -> impl ExactSizeIterator<Item = usize> + '_ {
        self.texts.iter().map(|range| range.len())
    }

    /// Every text, each after a separator.
    pub fn joined(&self) -> &Arc<str> {
        &self.joined
    }
}

/// A text's place in the session's layout: the text outside every message,
/// then each message's texts.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Placement {
    OutsideMessages,
    Message(usize),
}

impl Placement {
    fn of(ordinal: Option<usize>) -> Self {
        match ordinal {
            None => Self::OutsideMessages,
            Some(ordinal) => Self::Message(ordinal),
        }
    }
}

/// A text's place in the session's layout: its placement, then its place in
/// the order a read of the session alone returns it.
type LayoutKey = (Placement, ReadOrder);

/// Texts of one session, one after another, each with its layout key.
#[derive(Default)]
struct TextsInReadOrder {
    text: String,
    /// Each text's byte range in `text`.
    texts: Vec<(LayoutKey, Range<usize>)>,
}

impl TextsInReadOrder {
    fn push(&mut self, key: LayoutKey, text: &str) {
        let start = self.text.len();
        self.text.push_str(text);
        self.texts.push((key, start..self.text.len()));
    }
}

/// The texts of `pieces`, all of one session, in layout order. Texts of
/// equal keys keep their order in `pieces`. Sorts and copies across threads.
fn laid_out(pieces: Vec<TextsInReadOrder>) -> SessionSearchTexts {
    let mut ordered = pieces
        .iter()
        .flat_map(|piece| {
            piece
                .texts
                .iter()
                .map(|(key, range)| (*key, &piece.text[range.clone()]))
        })
        .collect::<Vec<_>>();
    ordered.par_sort_by_key(|(key, _)| *key);
    let texts_in_order = ordered.iter().map(|(_, text)| *text).collect::<Vec<_>>();
    joined(&texts_in_order)
}

/// `texts`, in their order, each after a separator once the joined text
/// holds a byte.
fn joined(texts: &[&str]) -> SessionSearchTexts {
    let mut end = 0;
    let ranges = texts
        .iter()
        .map(|text| {
            let start = if end == 0 {
                0
            } else {
                end + PADDED_MESSAGE_SEPARATOR.len()
            };
            end = start + text.len();
            start..end
        })
        .collect::<Box<[_]>>();
    SessionSearchTexts {
        joined: joined_once(texts, &ranges),
        texts: ranges,
    }
}

/// Bytes of texts a thread copies at once.
const BYTES_PER_COPY_TASK: usize = 1 << 20;

/// `texts` copied once, across threads, each to its range in `ranges`, with
/// the separator filling each gap between two ranges. The first range starts
/// at 0, and each gap is empty or as long as the separator.
fn joined_once(texts: &[&str], ranges: &[Range<usize>]) -> Arc<str> {
    let length = ranges.last().map_or(0, |range| range.end);
    let mut joined = Arc::<[u8]>::new_uninit_slice(length);
    let mut unfilled = Arc::get_mut(&mut joined).expect("a new allocation has one owner");
    // Each task: its bytes, the offset of its first byte in the joined text,
    // and the positions of the texts it copies.
    let mut tasks = Vec::new();
    let mut task_start = 0;
    let mut copied = 0;
    for (index, range) in ranges.iter().enumerate() {
        if range.end - copied >= BYTES_PER_COPY_TASK || index + 1 == ranges.len() {
            let (bytes, rest) = unfilled.split_at_mut(range.end - copied);
            tasks.push((bytes, copied, task_start..index + 1));
            unfilled = rest;
            task_start = index + 1;
            copied = range.end;
        }
    }
    tasks
        .into_par_iter()
        .for_each(|(bytes, offset, positions)| {
            let mut filled = 0;
            for position in positions {
                let range = &ranges[position];
                let text_start = range.start - offset;
                bytes[filled..text_start].write_copy_of_slice(
                    &PADDED_MESSAGE_SEPARATOR.as_bytes()[..text_start - filled],
                );
                bytes[text_start..range.end - offset]
                    .write_copy_of_slice(texts[position].as_bytes());
                filled = range.end - offset;
            }
        });
    // SAFETY: the tasks' slices cover every byte, and each task writes every
    // byte of its slice: each text's range, and the gap before it.
    let joined = unsafe { joined.assume_init() };
    // SAFETY: the bytes are texts and separators, each valid UTF-8, joined
    // whole; `str` and `[u8]` share their layout.
    unsafe { Arc::from_raw(Arc::into_raw(joined) as *const str) }
}

/// The texts of each of `copies` as the cache holds it, by its key; `None`
/// for a copy whose read failed or the cache no longer holds. Each session
/// is read on its own, through the cache's indexes on the session.
fn read_copies(copies: &[CachedSession]) -> Vec<TextsRead> {
    copies
        .par_iter()
        .map(|cached| {
            let texts = SessionSearchTexts::read(cached).ok().flatten();
            (cached.copy_key(), texts.map(Arc::new))
        })
        .collect()
}

/// [`read_copies`] for the copies of every row of the list, in one scan of
/// each table holding search text. A scan reads the whole cache whatever the
/// copies, and reading every row one session at a time takes longer.
fn read_every_row_copy(copies: &[CachedSession]) -> Vec<TextsRead> {
    let references = copies.iter().collect::<Vec<_>>();
    let scanned = scan_search_texts(
        &references,
        SearchedFields::List,
        &TimeFilter::default(),
        TextCollector::default,
    );
    let Ok(scanned) = scanned else {
        return copies
            .iter()
            .map(|cached| (cached.copy_key(), None))
            .collect();
    };
    let mut pieces = copies.iter().map(|_| Vec::new()).collect::<Vec<_>>();
    for collector in scanned.visitors {
        for (copy, texts) in collector.by_copy {
            pieces[copy].push(texts);
        }
    }
    let missing = scanned
        .missing
        .into_iter()
        .map(|(copy, _)| copy)
        .collect::<HashSet<_>>();
    // The row of a copy another write replaced holds the new copy's texts
    // until the rows next change and name that copy.
    let replaced = scanned.replaced.into_iter().collect::<HashMap<_, _>>();
    pieces
        .into_par_iter()
        .enumerate()
        .map(|(copy, pieces)| {
            let cached = &copies[copy];
            let texts = match replaced.get(&copy) {
                Some(&generation) => {
                    SessionSearchTexts::read(&cached.clone().of_generation(generation))
                        .ok()
                        .flatten()
                        .map(Arc::new)
                }
                None => (!missing.contains(&copy)).then(|| Arc::new(laid_out(pieces))),
            };
            (cached.copy_key(), texts)
        })
        .collect()
}

/// A read of a copy's search texts; `None` when the read failed or the cache
/// no longer holds the copy.
pub(crate) type TextsRead = (CopyKey, Option<Arc<SessionSearchTexts>>);

/// The copies of the list's rows whose texts are to be read.
pub(crate) struct CopiesToRead {
    pub(crate) copies: Vec<CachedSession>,
    /// True when no row's texts were held or being read: at launch, or once
    /// every row's copy changed.
    pub(crate) are_every_row: bool,
}

impl CopiesToRead {
    /// The texts of each copy: in one scan when the copies are every row's,
    /// else one session at a time.
    pub(crate) fn read(&self) -> Vec<TextsRead> {
        if self.are_every_row {
            read_every_row_copy(&self.copies)
        } else {
            read_copies(&self.copies)
        }
    }
}

/// One row range's texts, by the position of their session in the scanned
/// copies.
#[derive(Default)]
struct TextCollector {
    by_copy: HashMap<usize, TextsInReadOrder>,
}

impl TextVisitor for TextCollector {
    fn examine(&mut self, _text: &str) -> bool {
        true
    }

    fn keep(&mut self, row: &ScannedRow, text: &str) {
        let placement = Placement::of(row.ordinal);
        self.by_copy
            .entry(row.copy)
            .or_default()
            .push((placement, row.order), text);
    }
}

/// The search texts read into memory, by the copy of the session they were
/// read from.
#[derive(Default)]
pub struct SearchTextsInMemory {
    /// `None` while the copy's texts are being read.
    by_copy: HashMap<CopyKey, Option<Arc<SessionSearchTexts>>>,
}

impl SearchTextsInMemory {
    /// The texts of each of `conversations` the session cache holds. A
    /// session whose read fails, or whose copy the cache no longer holds, is
    /// left out: its search reads the cache, which reports the failure.
    pub fn read<C: Borrow<Conversation> + Sync>(conversations: &[C]) -> Self {
        let copies = conversations
            .iter()
            .filter_map(|conversation| conversation.borrow().cached_session.clone())
            .collect::<Vec<_>>();
        let by_copy = read_every_row_copy(&copies)
            .into_iter()
            .filter_map(|(copy, texts)| Some((copy, Some(texts?))))
            .collect();
        Self { by_copy }
    }

    /// The texts of `conversation`'s copy, once they are read.
    pub fn get(&self, conversation: &Conversation) -> Option<&SessionSearchTexts> {
        let cached = conversation.cached_session.as_ref()?;
        self.by_copy.get(&cached.copy_key())?.as_deref()
    }

    /// Keeps the texts of the copies `conversations` read and drops the
    /// rest. Returns the copies whose texts are neither held nor being read,
    /// now marked as being read.
    pub(crate) fn keep_copies_of<C: Borrow<Conversation>>(
        &mut self,
        conversations: &[C],
    ) -> CopiesToRead {
        let mut kept = HashMap::with_capacity(conversations.len());
        let mut copies = Vec::new();
        for conversation in conversations {
            let Some(cached) = &conversation.borrow().cached_session else {
                continue;
            };
            let key = cached.copy_key();
            let texts = self.by_copy.remove(&key).unwrap_or_else(|| {
                copies.push(cached.clone());
                None
            });
            kept.insert(key, texts);
        }
        let are_every_row = copies.len() == kept.len();
        self.by_copy = kept;
        CopiesToRead {
            copies,
            are_every_row,
        }
    }

    /// Holds `texts`, the read of `copy`, while a row still reads that copy.
    /// A failed read, `None`, is read again when the rows next change.
    pub(crate) fn finish_read(&mut self, copy: CopyKey, texts: Option<Arc<SessionSearchTexts>>) {
        match texts {
            Some(texts) => {
                if let Some(held) = self.by_copy.get_mut(&copy) {
                    *held = Some(texts);
                }
            }
            None => {
                self.by_copy.remove(&copy);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `texts` pushed one by one onto a string, each after a separator once
    /// the string holds a byte.
    fn pushed_one_by_one(texts: &[&str]) -> String {
        let mut joined = String::new();
        for text in texts {
            if !joined.is_empty() {
                joined.push_str(PADDED_MESSAGE_SEPARATOR);
            }
            joined.push_str(text);
        }
        joined
    }

    #[test]
    fn joined_texts_equal_texts_pushed_one_by_one_across_copy_tasks() {
        let long = "é".repeat(BYTES_PER_COPY_TASK / 3);
        let longer = "x".repeat(BYTES_PER_COPY_TASK + 1);
        for texts in [
            vec![],
            vec![""],
            vec!["", "", "first", "", "second"],
            vec![
                long.as_str(),
                "",
                "short",
                longer.as_str(),
                "",
                long.as_str(),
            ],
            vec![longer.as_str(), longer.as_str()],
        ] {
            let joined = joined(&texts);
            assert_eq!(
                &*joined.joined,
                pushed_one_by_one(&texts),
                "the texts join as pushed one by one: {} texts",
                texts.len()
            );
            assert_eq!(
                joined.texts_at(0..texts.len()).collect::<Vec<_>>(),
                texts,
                "each text's range holds the text"
            );
        }
    }
}
