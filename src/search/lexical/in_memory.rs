//! Each listed session's search texts, read from the session cache into
//! memory, so a search of the list scans memory instead of reading the
//! cache. The texts stay as the cache stores them.

use crate::error::Result;
use crate::history::Conversation;
use crate::history::message_texts::PADDED_MESSAGE_SEPARATOR;
use crate::history::sqlite_cache::{
    CachedSession, CopyKey, CopyMiss, CopyRead, HeldCopy, ReadOrder, ScannedCopy, ScannedRow,
    SearchField, SessionKey, StoredAppend, TextVisitor, scan_search_texts_in_waves,
};
use crate::time_filter::TimeFilter;
use rayon::prelude::*;
use std::borrow::Borrow;
use std::collections::HashMap;
use std::ops::{ControlFlow, Range};
use std::sync::Arc;

/// One session's search texts, as the session cache stores them: the
/// dialogue, then sub-agents' text, the sub-agent metadata, and thinking and
/// tool calls. Each field holds the text outside every message, then each
/// message's texts in message order, each after a separator as `full_text`
/// lays out its parts. The title and the summary are the row's (see
/// [`Conversation::title_and_summary_texts`]).
pub struct SessionSearchTexts {
    joined: Arc<str>,
    /// Each text's byte range in `joined`.
    texts: Box<[Range<usize>]>,
    /// The length of the dialogue at the start of `joined`.
    dialogue_len: usize,
    /// The generation of the copy the texts were read from.
    copy_generation: i64,
    /// Each text's placement in its field group, by its position in `texts`.
    placements: Box<[Placement]>,
    /// The position in `texts` after the last text of each of
    /// [`LAID_OUT_GROUPS`].
    group_ends: [usize; LAID_OUT_GROUPS.len()],
}

impl SessionSearchTexts {
    /// The texts of `cached`'s copy as the cache holds it, whether or not its
    /// transcripts changed since, or of the copy another write replaced it
    /// with. The row of a replaced copy holds the new copy's texts until the
    /// rows next change and name that copy.
    pub(crate) fn read(cached: &CachedSession) -> CopyRead<Self> {
        let copy = cached.clone().waiting();
        let (generation, read) = copy.read_copy(generation_and_texts)?;
        Ok(laid_out(generation, vec![read]))
    }

    /// [`Self::read`] of `cached`, given `held`, the texts of an earlier
    /// read. Returns `held` when the cache holds its copy. When an append
    /// wrote the cached copy over that copy, reads the append's texts and
    /// merges them into `held`; otherwise reads the copy whole.
    pub(crate) fn read_after(cached: &CachedSession, held: &Arc<Self>) -> CopyRead<Arc<Self>> {
        let copy = cached.clone().waiting();
        let read = copy.read_copy(|copy| {
            let generation = copy.generation();
            if generation == held.copy_generation {
                return Ok(CopyTexts::Held);
            }
            Ok(match copy.append_on(held.copy_generation)? {
                Some(append) => CopyTexts::Appended {
                    generation,
                    append,
                    texts: appended_texts(copy, &append)?,
                },
                None => CopyTexts::Whole {
                    generation,
                    texts: texts_in_read_order(copy)?,
                },
            })
        })?;
        Ok(match read {
            CopyTexts::Held => Arc::clone(held),
            CopyTexts::Appended {
                generation,
                append,
                texts,
            } => Arc::new(held.with_append(generation, &append, texts)),
            CopyTexts::Whole { generation, texts } => Arc::new(laid_out(generation, vec![texts])),
        })
    }

    /// Merges `added` into these texts: `append` wrote `added` in the copy
    /// `generation` and kept the rest. Each field group lays out the kept
    /// texts outside every message, the added ones, the kept texts of
    /// messages, then the added ones. Every write rewrites the sub-agent
    /// metadata: `added` alone contains it.
    fn with_append(&self, generation: i64, append: &StoredAppend, added: TextsInReadOrder) -> Self {
        let mut added_in_order = added
            .texts
            .iter()
            .map(|(key, range)| (*key, &added.text[range.clone()]))
            .collect::<Vec<_>>();
        added_in_order.sort_by_key(|(key, _)| *key);
        let mut added_in_order = added_in_order.into_iter().peekable();
        let mut texts = Vec::with_capacity(self.texts.len() + added.texts.len());
        let mut group_ends = [0; LAID_OUT_GROUPS.len()];
        let mut group_start = 0;
        for (index, group) in LAID_OUT_GROUPS.into_iter().enumerate() {
            let held = group_start..self.group_ends[index];
            group_start = held.end;
            let mut added_to_group = Vec::new();
            while let Some(((_, placement, _), text)) =
                added_in_order.next_if(|((added, ..), _)| *added == group)
            {
                added_to_group.push((text, placement));
            }
            let added_outside =
                added_to_group.partition_point(|(_, placement)| placement.is_outside_messages());
            let held_outside = held.start
                + self.placements[held.clone()]
                    .partition_point(|placement| placement.is_outside_messages());
            let kept = |positions: Range<usize>| {
                positions
                    .filter(|&position| self.placements[position].is_kept_by(group, append))
                    .map(|position| {
                        (
                            &self.joined[self.texts[position].clone()],
                            self.placements[position],
                        )
                    })
            };
            texts.extend(kept(held.start..held_outside));
            texts.extend(added_to_group.drain(..added_outside));
            texts.extend(kept(held_outside..held.end));
            texts.extend(added_to_group);
            group_ends[index] = texts.len();
        }
        Self::of_texts(generation, &texts, group_ends)
    }

    /// `texts`, in their layout order, each with its placement: the texts of
    /// the copy `generation`, the last text of each of [`LAID_OUT_GROUPS`]
    /// before its `group_ends`.
    fn of_texts(
        generation: i64,
        texts: &[(&str, Placement)],
        group_ends: [usize; LAID_OUT_GROUPS.len()],
    ) -> Self {
        let (texts, placements): (Vec<&str>, Vec<Placement>) = texts.iter().copied().unzip();
        let (joined, ranges) = joined(&texts);
        Self {
            joined,
            dialogue_len: ranges[..group_ends[0]].last().map_or(0, |range| range.end),
            texts: ranges,
            copy_generation: generation,
            placements: placements.into_boxed_slice(),
            group_ends,
        }
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

    /// The dialogue's texts, each after a separator: the start of
    /// [`Self::joined`].
    pub fn dialogue(&self) -> &str {
        &self.joined[..self.dialogue_len]
    }
}

/// A text's field group, in the order a parsed row joins its texts:
/// `full_text`, then `agent_search_text` (sub-agents' text, ending with the
/// sub-agent metadata), then `thinking_and_tool_call_text`. A row read from
/// the cache then shows the context line a parsed row shows.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum FieldGroup {
    Dialogue,
    SubagentText,
    SubagentMetadata,
    ThinkingAndToolCalls,
    /// Semantic turns are not search texts; a read of search texts doesn't
    /// return any.
    SemanticTurns,
}

/// The field groups of search texts, in layout order.
const LAID_OUT_GROUPS: [FieldGroup; 4] = [
    FieldGroup::Dialogue,
    FieldGroup::SubagentText,
    FieldGroup::SubagentMetadata,
    FieldGroup::ThinkingAndToolCalls,
];

impl FieldGroup {
    fn of(field: SearchField) -> Self {
        match field {
            SearchField::Full => Self::Dialogue,
            SearchField::Agent => Self::SubagentText,
            SearchField::Supplement => Self::SubagentMetadata,
            SearchField::Thinking => Self::ThinkingAndToolCalls,
            SearchField::Semantic => Self::SemanticTurns,
        }
    }
}

/// A text's place among its field group's texts: the texts outside every
/// message by their entry's sequence, then each message's texts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Placement {
    OutsideMessages { entry: i64 },
    Message(usize),
}

impl Placement {
    fn of(ordinal: Option<usize>, order: ReadOrder) -> Self {
        match ordinal {
            None => Self::OutsideMessages {
                entry: order.entry_sequence(),
            },
            Some(ordinal) => Self::Message(ordinal),
        }
    }

    fn is_outside_messages(self) -> bool {
        matches!(self, Self::OutsideMessages { .. })
    }

    /// True when `append` kept the text of `group` at this placement. Every
    /// write rewrites the sub-agent metadata.
    fn is_kept_by(self, group: FieldGroup, append: &StoredAppend) -> bool {
        if group == FieldGroup::SubagentMetadata {
            return false;
        }
        match (self, append.first_message) {
            (Self::OutsideMessages { entry }, _) => entry < append.first_entry,
            (Self::Message(_), None) => true,
            (Self::Message(ordinal), Some(first)) => {
                i64::try_from(ordinal).is_ok_and(|ordinal| ordinal < first)
            }
        }
    }
}

/// A text's place in the session's layout: its field group, its placement,
/// then its place in the order a read of the session alone returns it.
type LayoutKey = (FieldGroup, Placement, ReadOrder);

/// The layout key of a text of `field` in the message `ordinal`, at `order`
/// in its session.
fn layout_key(field: SearchField, ordinal: Option<usize>, order: ReadOrder) -> LayoutKey {
    (FieldGroup::of(field), Placement::of(ordinal, order), order)
}

/// A copy's texts as a read after an earlier read finds them.
enum CopyTexts {
    /// The held texts are the copy's.
    Held,
    /// An append wrote the copy of `generation` over the held texts' copy;
    /// `texts` are the append's.
    Appended {
        generation: i64,
        append: StoredAppend,
        texts: TextsInReadOrder,
    },
    /// Every text of the copy of `generation`.
    Whole {
        generation: i64,
        texts: TextsInReadOrder,
    },
}

/// The search texts of `held`, in the order its read returns them.
fn texts_in_read_order(held: &HeldCopy<'_>) -> Result<TextsInReadOrder> {
    let mut read = TextsInReadOrder::default();
    held.visit_search_texts(&TimeFilter::default(), |text| {
        read.push(layout_key(text.field, text.ordinal, text.order), text.text);
        ControlFlow::Continue(())
    })?;
    Ok(read)
}

/// [`texts_in_read_order`], with the generation of `held`'s copy.
fn generation_and_texts(held: &HeldCopy<'_>) -> Result<(i64, TextsInReadOrder)> {
    Ok((held.generation(), texts_in_read_order(held)?))
}

/// Reads `append`'s search texts in `held`'s copy, in the order its read
/// returns them.
fn appended_texts(held: &HeldCopy<'_>, append: &StoredAppend) -> Result<TextsInReadOrder> {
    let mut read = TextsInReadOrder::default();
    held.visit_appended_search_texts(append, |text| {
        read.push(layout_key(text.field, text.ordinal, text.order), text.text);
        ControlFlow::Continue(())
    })?;
    Ok(read)
}

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

/// The texts of `pieces`, all of one session's copy of `generation`, in
/// layout order. Texts of equal keys keep their order in `pieces`. Sorts and
/// copies across threads.
fn laid_out(generation: i64, pieces: Vec<TextsInReadOrder>) -> SessionSearchTexts {
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
    let group_ends = LAID_OUT_GROUPS
        .map(|group| ordered.partition_point(|((text_group, ..), _)| *text_group <= group));
    let texts = ordered
        .iter()
        .map(|((_, placement, _), text)| (*text, *placement))
        .collect::<Vec<_>>();
    SessionSearchTexts::of_texts(generation, &texts, group_ends)
}

/// `texts`, in their order, each after a separator once the joined text
/// contains a byte, and each text's range in the joined text.
fn joined(texts: &[&str]) -> (Arc<str>, Box<[Range<usize>]>) {
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
    (joined_once(texts, &ranges), ranges)
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

/// Reads the texts of each of `copies` as the cache holds it, and hands them
/// to `reply` with the copy's key as its read ends. Reads each session on
/// its own, through the cache's indexes on the session. With texts in
/// `held`, reads only an append's texts on them.
fn read_copies(
    copies: &[CachedSession],
    held: &HashMap<SessionKey, Arc<SessionSearchTexts>>,
    reply: BatchReply<'_>,
) {
    copies.par_iter().for_each(|cached| {
        let key = cached.copy_key();
        let read = read_texts(cached, held.get(&key.session));
        reply(vec![(key, read)]);
    });
}

/// [`SessionSearchTexts::read`] of `cached`, or
/// [`SessionSearchTexts::read_after`] the texts `held`, or the cause of its
/// miss.
fn read_texts(cached: &CachedSession, held: Option<&Arc<SessionSearchTexts>>) -> TextsReadResult {
    let read = match held {
        Some(held) => SessionSearchTexts::read_after(cached, held),
        None => SessionSearchTexts::read(cached).map(Arc::new),
    };
    read.map_err(ReadFailure::of_miss)
}

/// The most waves the first read of every row's texts is scanned in. The
/// search bar's count of sessions loading decreases as each wave ends.
const FIRST_READ_WAVES: usize = 16;

/// [`read_copies`] for the copies of every row of the list, in one scan of
/// each table holding search text, in waves of row ranges. `reply` receives
/// each copy after the wave that reads its last row, laid out while the next
/// wave scans. A scan reads the whole cache whatever the copies, and reading
/// every row one session at a time takes longer.
fn read_every_row_copy(copies: &[CachedSession], reply: BatchReply<'_>) {
    let references = copies.iter().collect::<Vec<_>>();
    let mut pieces = copies.iter().map(|_| Vec::new()).collect::<Vec<_>>();
    let mut is_replied = vec![false; copies.len()];
    let scanned = rayon::scope(|layouts| {
        scan_search_texts_in_waves(
            &references,
            FIRST_READ_WAVES,
            TextCollector::default,
            |_, held| generation_and_texts(held),
            |wave| {
                for collector in wave.reads {
                    for (copy, texts) in collector.by_copy {
                        pieces[copy].push(texts);
                    }
                }
                let done = wave
                    .copies
                    .into_iter()
                    .map(|(copy, scanned)| {
                        is_replied[copy] = true;
                        (copy, scanned, std::mem::take(&mut pieces[copy]))
                    })
                    .collect::<Vec<_>>();
                if !done.is_empty() {
                    layouts.spawn(move |_| reply(laid_out_copies(copies, done)));
                }
            },
        )
    });
    if let Err(error) = scanned {
        let failure = ReadFailure::of(&error);
        reply(
            copies
                .iter()
                .zip(is_replied)
                .filter(|(_, is_replied)| !is_replied)
                .map(|(cached, _)| (cached.copy_key(), Err(failure.clone())))
                .collect(),
        );
    }
}

/// The texts of each of `done`, copies of `copies` by position, laid out
/// from what the scan found of it.
fn laid_out_copies(
    copies: &[CachedSession],
    done: Vec<(usize, ScannedCopyTexts, Vec<TextsInReadOrder>)>,
) -> Vec<TextsRead> {
    done.into_par_iter()
        .map(|(copy, scanned, pieces)| {
            let cached = &copies[copy];
            let texts = match scanned {
                Ok(ScannedCopy::Scanned(())) => Ok(Arc::new(laid_out(cached.generation(), pieces))),
                Ok(ScannedCopy::ReadAlone((generation, read))) => {
                    Ok(Arc::new(laid_out(generation, vec![read])))
                }
                Err(miss) => Err(ReadFailure::of_miss(miss)),
            };
            (cached.copy_key(), texts)
        })
        .collect()
}

/// A scan's result for one copy: its texts in the scan's pieces, its texts
/// read alone with their copy's generation, or the cause of its miss.
type ScannedCopyTexts = CopyRead<ScannedCopy<(), (i64, TextsInReadOrder)>>;

/// Receives the reads of each batch of copies as the batch's read ends.
pub(crate) type BatchReply<'a> = &'a (dyn Fn(Vec<TextsRead>) + Sync);

/// A read of a copy's search texts.
pub(crate) type TextsRead = (CopyKey, TextsReadResult);

/// The texts a read of a copy found, or the cause of its failure.
pub(crate) type TextsReadResult = std::result::Result<Arc<SessionSearchTexts>, ReadFailure>;

/// The reason a read of a copy's search texts didn't return any, as the TUI
/// names it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReadFailure {
    /// The session is no longer in the session cache.
    NoLongerCached,
    /// The session's cached copy is not ready yet: the search bar counts it
    /// as loading.
    NotReady,
    Error(Arc<str>),
}

impl ReadFailure {
    pub(crate) fn of(error: &crate::error::AppError) -> Self {
        Self::Error(error.to_string().into())
    }

    /// The TUI's name for `miss`.
    pub(crate) fn of_miss(miss: CopyMiss) -> Self {
        match miss {
            CopyMiss::NotThere => Self::NoLongerCached,
            CopyMiss::NotReady => Self::NotReady,
            CopyMiss::ReadError(error) => Self::of(&error),
        }
    }

    /// True when the session counts as loading, not unreadable.
    pub(crate) fn is_loading(&self) -> bool {
        matches!(self, Self::NotReady)
    }
}

impl std::fmt::Display for ReadFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoLongerCached => formatter.write_str("no longer in the session cache"),
            Self::NotReady => formatter.write_str("its cached copy is not ready yet"),
            Self::Error(error) => formatter.write_str(error),
        }
    }
}

/// The copies of the list's rows whose texts are to be read, and how.
pub(crate) enum CopiesToRead {
    /// Every row's copy, none with held texts an appended reread starts
    /// from: at launch. One scan of the cache reads them, in waves.
    EveryRow(Vec<CachedSession>),
    /// The changed rows' copies, each session read on its own.
    EachSession {
        copies: Vec<CachedSession>,
        /// The texts of earlier copies of the sessions, by session: an
        /// appended reread merges an append's texts into them.
        held: HashMap<SessionKey, Arc<SessionSearchTexts>>,
    },
}

impl CopiesToRead {
    pub(crate) fn copies(&self) -> &[CachedSession] {
        match self {
            Self::EveryRow(copies) | Self::EachSession { copies, .. } => copies,
        }
    }

    /// The texts of each copy, handed to `reply` a batch at a time as each
    /// batch's read ends.
    pub(crate) fn read_in_batches(&self, reply: BatchReply<'_>) {
        match self {
            Self::EveryRow(copies) => read_every_row_copy(copies, reply),
            Self::EachSession { copies, held } => read_copies(copies, held, reply),
        }
    }

    /// The texts of each copy, in the copies' order, once every batch's read
    /// ends.
    pub(crate) fn read(&self) -> Vec<TextsRead> {
        let reads = std::sync::Mutex::new(HashMap::with_capacity(self.copies().len()));
        self.read_in_batches(&|batch| {
            reads
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .extend(batch);
        });
        let mut reads = reads
            .into_inner()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.copies()
            .iter()
            .map(|cached| {
                let key = cached.copy_key();
                let read = reads
                    .remove(&key)
                    .expect("the read of every copy replies with the copy");
                (key, read)
            })
            .collect()
    }

    /// A read of each copy that failed with `failure`.
    pub(crate) fn failed(&self, failure: &ReadFailure) -> Vec<TextsRead> {
        self.copies()
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
        self.by_copy
            .entry(row.copy)
            .or_default()
            .push(layout_key(row.field, row.ordinal, row.order), text);
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
    /// The generation of the copy the read was asked for. The texts can be
    /// of a later copy ([`SessionSearchTexts::copy_generation`]).
    requested_generation: i64,
    texts: Arc<SessionSearchTexts>,
}

/// The read of the copy a session's row reads.
#[derive(Clone, PartialEq, Eq)]
enum CopyReadState {
    Held,
    Reading,
    /// The read didn't return the copy's texts. While none of the session's
    /// earlier texts are held, the search bar counts a copy that is not
    /// ready as loading, and any other as unreadable. The copy is read again
    /// when the rows next change.
    Missed(ReadFailure),
}

/// The sessions without texts in memory, by cause.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SessionsWithoutTexts {
    /// Sessions whose first read is running, or whose cached copy is not
    /// ready yet.
    pub loading: usize,
    /// Sessions whose read failed, or that are no longer in the session
    /// cache.
    pub unreadable: usize,
}

impl SessionsWithoutTexts {
    /// Counts a session whose read missed with `failure`.
    pub(crate) fn count(&mut self, failure: &ReadFailure) {
        if failure.is_loading() {
            self.loading += 1;
        } else {
            self.unreadable += 1;
        }
    }
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
        for (copy, read) in CopiesToRead::EveryRow(copies).read() {
            let (read, texts) = match read {
                Ok(texts) => (
                    CopyReadState::Held,
                    Some(HeldTexts {
                        requested_generation: copy.generation,
                        texts,
                    }),
                ),
                Err(failure) => (CopyReadState::Missed(failure), None),
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

    /// The reason the read of `conversation`'s copy didn't return its texts;
    /// `None` unless that read missed.
    pub fn read_failure(&self, conversation: &Conversation) -> Option<&ReadFailure> {
        match &self.held_session(conversation)?.read {
            CopyReadState::Missed(failure) => Some(failure),
            CopyReadState::Held | CopyReadState::Reading => None,
        }
    }

    fn held_session(&self, conversation: &Conversation) -> Option<&HeldSession> {
        let cached = conversation.cached_session.as_ref()?;
        self.by_session.get(&cached.copy_key().session)
    }

    /// The sessions without texts in memory: being read for the first time
    /// or not ready yet, or whose read failed.
    pub fn sessions_without_texts(&self) -> SessionsWithoutTexts {
        let mut without = SessionsWithoutTexts::default();
        for session in self.by_session.values().filter(|held| held.texts.is_none()) {
            match &session.read {
                CopyReadState::Reading => without.loading += 1,
                CopyReadState::Missed(failure) => without.count(failure),
                CopyReadState::Held => {}
            }
        }
        without
    }

    /// The count of sessions being read again; search uses their held texts
    /// until each read ends.
    pub fn sessions_updating(&self) -> usize {
        self.by_session
            .values()
            .filter(|held| held.texts.is_some() && held.read == CopyReadState::Reading)
            .count()
    }

    /// Keeps the texts of the sessions `conversations` list and drops the
    /// rest. Returns the copies whose texts are neither held nor being read,
    /// now marked as being read: copies new to their rows, and copies whose
    /// read missed; with the texts held of their sessions.
    pub(crate) fn keep_copies_of<C: Borrow<Conversation>>(
        &mut self,
        conversations: &[C],
    ) -> CopiesToRead {
        let mut kept = HashMap::with_capacity(conversations.len());
        let mut copies = Vec::new();
        let mut held_texts = HashMap::new();
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
                        && !matches!(held.read, CopyReadState::Missed(_)) =>
                {
                    held
                }
                held => {
                    copies.push(cached.clone());
                    let texts = held.and_then(|held| held.texts);
                    if let Some(texts) = &texts {
                        held_texts.insert(session.clone(), Arc::clone(&texts.texts));
                    }
                    HeldSession {
                        generation,
                        read: CopyReadState::Reading,
                        texts,
                    }
                }
            };
            kept.insert(session, held);
        }
        let are_every_row = copies.len() == kept.len() && held_texts.is_empty();
        self.by_session = kept;
        if are_every_row {
            CopiesToRead::EveryRow(copies)
        } else {
            CopiesToRead::EachSession {
                copies,
                held: held_texts,
            }
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
                    .is_some_and(|held| held.requested_generation > copy.generation)
                {
                    return;
                }
                held.texts = Some(HeldTexts {
                    requested_generation: copy.generation,
                    texts,
                });
                if is_row_copy {
                    held.read = CopyReadState::Held;
                }
            }
            Err(failure) if is_row_copy => held.read = CopyReadState::Missed(failure),
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

    /// `texts`, the first `dialogue_texts` of them the dialogue, the rest
    /// sub-agents' text, each its own message's.
    fn session_of(texts: &[&str], dialogue_texts: usize) -> SessionSearchTexts {
        let placed = texts
            .iter()
            .enumerate()
            .map(|(ordinal, text)| (*text, Placement::Message(ordinal)))
            .collect::<Vec<_>>();
        let mut group_ends = [texts.len(); LAID_OUT_GROUPS.len()];
        group_ends[0] = dialogue_texts;
        SessionSearchTexts::of_texts(1, &placed, group_ends)
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
            let joined = session_of(&texts, texts.len());
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

    #[test]
    fn dialogue_is_its_texts_joined_without_the_other_fields_texts() {
        let texts = ["asked", "", "answered", "tool call", "thinking"];
        for dialogue_texts in 0..=texts.len() {
            assert_eq!(
                session_of(&texts, dialogue_texts).dialogue(),
                pushed_one_by_one(&texts[..dialogue_texts]),
                "the dialogue is the first {dialogue_texts} texts, joined"
            );
        }
    }
}
