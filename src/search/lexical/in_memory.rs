//! Each listed session's search texts in the session list's fields, read
//! from the session cache into memory, so a search of the list scans memory
//! instead of reading the cache. The texts stay as the cache stores them.

use crate::error::Result;
use crate::history::Conversation;
use crate::history::message_texts::PADDED_MESSAGE_SEPARATOR;
use crate::history::sqlite_cache::{
    CachedSession, CopyKey, CopyOutcome, ReadOrder, ScanScope, ScannedRow, SearchedFields,
    SessionKey, TextVisitor, scan_search_texts,
};
use crate::time_filter::TimeFilter;
use rayon::prelude::*;
use std::borrow::Borrow;
use std::collections::HashMap;
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
    /// transcripts changed since, or of the copy another write replaced it
    /// with; `None` when the session is no longer in the session cache. The
    /// row of a replaced copy holds the new copy's texts until the rows next
    /// change and name that copy.
    pub(crate) fn read(cached: &CachedSession) -> Result<Option<Self>> {
        let copy = cached.clone().waiting();
        let mut read = TextsInReadOrder::default();
        let found = copy.read_held_copy(|held| {
            held.visit_search_texts(SearchedFields::List, &TimeFilter::default(), |text| {
                let placement = Placement::of(text.ordinal);
                read.push((placement, text.order), text.text);
                ControlFlow::Continue(())
            })
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

/// The texts of each of `copies` as the cache holds it, by its key. Each
/// session is read on its own, through the cache's indexes on the session.
fn read_copies(copies: &[CachedSession]) -> Vec<TextsRead> {
    copies
        .par_iter()
        .map(|cached| (cached.copy_key(), read_texts(cached)))
        .collect()
}

/// [`SessionSearchTexts::read`] of `cached`, or the cause of its failure.
fn read_texts(cached: &CachedSession) -> TextsReadResult {
    match SessionSearchTexts::read(cached) {
        Ok(Some(texts)) => Ok(Arc::new(texts)),
        Ok(None) => Err(ReadFailure::NoLongerCached),
        Err(error) => Err(ReadFailure::of(&error)),
    }
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
        ScanScope::AllWorkspaces,
        TextCollector::default,
    );
    let scanned = match scanned {
        Ok(scanned) => scanned,
        Err(error) => {
            let failure = ReadFailure::of(&error);
            return copies
                .iter()
                .map(|cached| (cached.copy_key(), Err(failure.clone())))
                .collect();
        }
    };
    let mut pieces = copies.iter().map(|_| Vec::new()).collect::<Vec<_>>();
    for collector in scanned.reads {
        for (copy, texts) in collector.by_copy {
            pieces[copy].push(texts);
        }
    }
    pieces
        .into_par_iter()
        .zip(scanned.copies)
        .enumerate()
        .map(|(copy, (pieces, outcome))| {
            let cached = &copies[copy];
            let texts = match outcome {
                CopyOutcome::Scanned(_) => Ok(Arc::new(laid_out(pieces))),
                CopyOutcome::NoLongerCached => Err(ReadFailure::NoLongerCached),
                CopyOutcome::Replaced(_) => read_texts(cached),
                CopyOutcome::Failed(error) => Err(ReadFailure::of(&error)),
            };
            (cached.copy_key(), texts)
        })
        .collect()
}

/// A read of a copy's search texts.
pub(crate) type TextsRead = (CopyKey, TextsReadResult);

/// The texts a read of a copy found, or the cause of its failure.
pub(crate) type TextsReadResult = std::result::Result<Arc<SessionSearchTexts>, ReadFailure>;

/// The cause of a failed read of a copy's search texts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReadFailure {
    /// The session is no longer in the session cache.
    NoLongerCached,
    Error(Arc<str>),
}

impl ReadFailure {
    pub(crate) fn of(error: &crate::error::AppError) -> Self {
        Self::Error(error.to_string().into())
    }
}

impl std::fmt::Display for ReadFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoLongerCached => formatter.write_str("no longer in the session cache"),
            Self::Error(error) => formatter.write_str(error),
        }
    }
}

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

    /// A read of each copy that failed with `failure`.
    pub(crate) fn failed(&self, failure: &ReadFailure) -> Vec<TextsRead> {
        self.copies
            .iter()
            .map(|cached| (cached.copy_key(), Err(failure.clone())))
            .collect()
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

/// The search texts read into memory, by session: for each session a row
/// lists, the texts of the newest copy read, and the read of the copy the
/// row reads.
#[derive(Default)]
pub struct SearchTextsInMemory {
    by_session: HashMap<SessionKey, HeldSession>,
}

/// One session's texts in memory.
struct HeldSession {
    /// The generation of the copy the session's row reads.
    generation: i64,
    read: CopyReadState,
    /// The texts search reads: the row's copy's once read, until then those
    /// of the newest earlier copy read.
    texts: Option<HeldTexts>,
}

/// The texts read for one copy of a session.
struct HeldTexts {
    /// The generation of the copy the texts were read for.
    generation: i64,
    texts: Arc<SessionSearchTexts>,
}

/// The read of the copy a session's row reads.
#[derive(Clone, PartialEq, Eq)]
enum CopyReadState {
    Held,
    Reading,
    /// The read failed, or the session is no longer in the session cache.
    /// While none of the session's earlier texts are held, the search bar
    /// counts it as unreadable. The copy is read again when the rows next
    /// change.
    Unreadable(ReadFailure),
}

/// The sessions without texts in memory, by why.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SessionsWithoutTexts {
    /// Sessions whose first read is running.
    pub loading: usize,
    /// Sessions whose read failed.
    pub unreadable: usize,
}

impl SearchTextsInMemory {
    /// The texts of each of `conversations` the session cache holds. A
    /// session whose read fails, or whose copy is no longer in the session
    /// cache, doesn't get any.
    pub fn read<C: Borrow<Conversation> + Sync>(conversations: &[C]) -> Self {
        let copies = conversations
            .iter()
            .filter_map(|conversation| conversation.borrow().cached_session.clone())
            .collect::<Vec<_>>();
        let mut in_memory = Self::default();
        for (copy, read) in read_every_row_copy(&copies) {
            let (read, texts) = match read {
                Ok(texts) => (
                    CopyReadState::Held,
                    Some(HeldTexts {
                        generation: copy.generation,
                        texts,
                    }),
                ),
                Err(failure) => (CopyReadState::Unreadable(failure), None),
            };
            in_memory.by_session.insert(
                copy.session,
                HeldSession {
                    generation: copy.generation,
                    read,
                    texts,
                },
            );
        }
        in_memory
    }

    /// The texts search reads for `conversation`'s session: its copy's, or
    /// until they are read, those of the newest earlier copy read.
    pub fn get(&self, conversation: &Conversation) -> Option<&SessionSearchTexts> {
        let held = self.held_session(conversation)?;
        held.texts.as_ref().map(|held| &*held.texts)
    }

    /// The cause of the failed read of `conversation`'s copy; `None` unless
    /// that read failed.
    pub fn read_failure(&self, conversation: &Conversation) -> Option<&ReadFailure> {
        match &self.held_session(conversation)?.read {
            CopyReadState::Unreadable(failure) => Some(failure),
            CopyReadState::Held | CopyReadState::Reading => None,
        }
    }

    fn held_session(&self, conversation: &Conversation) -> Option<&HeldSession> {
        let cached = conversation.cached_session.as_ref()?;
        self.by_session.get(&cached.copy_key().session)
    }

    /// The sessions without texts in memory: being read for the first time,
    /// or whose read failed.
    pub fn sessions_without_texts(&self) -> SessionsWithoutTexts {
        let mut without = SessionsWithoutTexts::default();
        for session in self.by_session.values().filter(|held| held.texts.is_none()) {
            match session.read {
                CopyReadState::Reading => without.loading += 1,
                CopyReadState::Unreadable(_) => without.unreadable += 1,
                CopyReadState::Held => {}
            }
        }
        without
    }

    /// Keeps the texts of the sessions `conversations` list and drops the
    /// rest. Returns the copies whose texts are neither held nor being read,
    /// now marked as being read: copies new to their rows, and unreadable
    /// copies.
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
            let CopyKey {
                session,
                generation,
            } = cached.copy_key();
            let held = match self.by_session.remove(&session) {
                Some(held)
                    if held.generation == generation
                        && !matches!(held.read, CopyReadState::Unreadable(_)) =>
                {
                    held
                }
                held => {
                    copies.push(cached.clone());
                    HeldSession {
                        generation,
                        read: CopyReadState::Reading,
                        texts: held.and_then(|held| held.texts),
                    }
                }
            };
            kept.insert(session, held);
        }
        let are_every_row = copies.len() == kept.len();
        self.by_session = kept;
        CopiesToRead {
            copies,
            are_every_row,
        }
    }

    /// Holds the texts of `read`, a read of `copy`, when they are newer than
    /// the texts its session holds. A read of the copy the row reads ends that read;
    /// the session stays being read after a read of an earlier copy. A failed
    /// read keeps the texts held before it.
    pub(crate) fn finish_read(&mut self, copy: CopyKey, read: TextsReadResult) {
        let Some(held) = self.by_session.get_mut(&copy.session) else {
            return;
        };
        let is_row_copy = copy.generation == held.generation;
        if is_row_copy && held.read != CopyReadState::Reading {
            return;
        }
        match read {
            Ok(texts) => {
                if held
                    .texts
                    .as_ref()
                    .is_some_and(|held| held.generation > copy.generation)
                {
                    return;
                }
                held.texts = Some(HeldTexts {
                    generation: copy.generation,
                    texts,
                });
                if is_row_copy {
                    held.read = CopyReadState::Held;
                }
            }
            Err(failure) if is_row_copy => held.read = CopyReadState::Unreadable(failure),
            Err(_) => {}
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
