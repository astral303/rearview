//! A changed session read from where its last read stopped: each changed
//! transcript resumed through its agent's reader, the windows linked,
//! spliced after the checkpoint and folded into the session row, and the
//! record that writes only what they changed. Any case it cannot place is
//! read whole instead.

use super::{
    ChildKey, SessionCheckpoint, ThreadFold, WalkStep, checkpoint_place_after, leading_digest,
    reply_id,
};
use crate::cli::DebugLevel;
use crate::clock::Clock;
use crate::error::Result;
use crate::history::format::RecordLine;
use crate::history::format::splice::{Spliced, SubagentThread, subagent_turns, walk_spliced};
use crate::history::parser::{
    SubagentSearchPart, SubagentShare, adds_search_parts, fold_tail, folded_row,
    parse_errors_of_lines, resumed_thread_fold, session_time, subagent_share,
};
use crate::history::provider::transcript_reads::resume as resume_read;
use crate::history::provider::{
    LinkWindow, LinkedThread, ResumedWindow, Resumption, SessionRoot, SessionStorage, SessionStub,
    TranscriptRead,
};
use crate::history::sqlite_cache::{
    InPlaceRow, SessionRecord, SqliteCache, StoredFile, StoredResume, StubStamps, WindowUpdate,
};
use crate::history::{Conversation, MalformedLine, ParsePermit, TranscriptEntries};
use crate::log_entry::LogEntry;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// A resumed read's listed row and its record.
pub(crate) struct ResumedSession {
    pub(crate) row: Conversation,
    pub(crate) record: SessionRecord,
    /// The bytes the read read from the transcripts it opened.
    pub(crate) bytes_read: u64,
}

/// A resumed read of a changed session, or why the session is read whole.
pub(crate) type Resumed = std::result::Result<ResumedSession, WholeRead>;

/// Why a changed session is read whole instead of on from its stored copy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WholeRead {
    /// The stored copy holds no checkpoint.
    NoCheckpoint,
    /// The stored checkpoint does not decode, or a stored sub-agent turn
    /// after it names no transcript.
    CheckpointUnreadable,
    /// The stored copy is not the one the load listed: another load wrote
    /// the session since.
    StoredCopyReplaced,
    /// A sub-agent transcript's stored share does not decode.
    SubagentShareUnreadable,
    /// The session's own transcript continues an earlier one, or its read
    /// does not resume.
    OwnDoesNotResume,
    /// The session's own transcript was rewritten or its identity changed.
    OwnRewritten,
    /// A record of the session's own transcript after its stable cursor
    /// changes an entry before it.
    OwnChangesEarlierEntry,
    SubagentRemoved,
    /// A new sub-agent transcript is not one of the agent's transcripts.
    SubagentUnreadable,
    /// The session's files are unchanged, though its stamp is not.
    NoFileChanged,
    /// The session's leading entries besides its custom title changed, such
    /// as a title the agent generated.
    LeadingChanged,
    /// A changed sub-agent changes a turn before the checkpoint beyond its
    /// content, or removes one.
    TurnBeforeCheckpointChanged,
    /// A new entry sorts before the checkpoint.
    EntryBeforeCheckpoint,
    /// A reply continues one stored before the checkpoint.
    ReplyAcrossCheckpoint,
    /// A file changed during the read, or the row does not fold.
    ChangedDuringRead,
    /// The first working directory an entry of a session without a header
    /// names changed.
    CwdChanged,
}

impl WholeRead {
    pub(crate) fn reason(self) -> &'static str {
        match self {
            Self::NoCheckpoint => "the stored copy holds no checkpoint",
            Self::CheckpointUnreadable => "the stored checkpoint cannot be read",
            Self::StoredCopyReplaced => "another load rewrote the stored copy",
            Self::SubagentShareUnreadable => "a sub-agent transcript's stored share cannot be read",
            Self::OwnDoesNotResume => "the session's own transcript does not resume",
            Self::OwnRewritten => "the session's own transcript was rewritten",
            Self::OwnChangesEarlierEntry => {
                "a new record of the session's own transcript changes an earlier entry"
            }
            Self::SubagentRemoved => "a sub-agent transcript was removed",
            Self::SubagentUnreadable => "a new sub-agent transcript could not be read",
            Self::NoFileChanged => "no file changed",
            Self::LeadingChanged => "the session's title changed",
            Self::TurnBeforeCheckpointChanged => "a sub-agent changed a turn before the checkpoint",
            Self::EntryBeforeCheckpoint => "a new entry sorts before the checkpoint",
            Self::ReplyAcrossCheckpoint => "a reply continues across the checkpoint",
            Self::ChangedDuringRead => "a file changed during the read",
            Self::CwdChanged => "the session's working directory changed",
        }
    }
}

/// What a resumed read of one session reads with.
pub(crate) struct ResumeContext<'a> {
    pub(crate) storage: &'a dyn SessionStorage,
    pub(crate) cache: &'a SqliteCache,
    pub(crate) root: &'a SessionRoot,
    pub(crate) stub: &'a SessionStub,
    /// The session's stored row.
    pub(crate) stored: &'a Conversation,
    pub(crate) on_transcript_read: &'a (dyn Fn() + Sync),
    pub(crate) clock: &'a dyn Clock,
    pub(crate) debug_level: Option<DebugLevel>,
}

/// One sub-agent transcript of the session, as the resumed read takes it.
struct Thread<'a> {
    path: &'a Path,
    /// Its share of the row at its stable cursor, as stored or as read.
    fold: ThreadFold,
    stored_read: Option<TranscriptRead>,
    stored_fold: Vec<u8>,
    change: Change,
}

enum Change {
    Unchanged,
    Window(ResumedWindow),
    Whole(Box<SubagentShare>),
}

/// A sub-agent turn spliced after the checkpoint, with its place in the
/// splice order.
struct Child {
    entry: (RecordLine, LogEntry),
    key: ChildKey,
    /// The thread's place among the session's sub-agents.
    order: usize,
}

/// The session `context.stub` read on from its stored copy, `context.stored`,
/// or why it must be read whole: the stored copy holds no checkpoint, its own
/// transcript does not resume, a sub-agent transcript was removed, or the
/// read cannot place what changed after the checkpoint.
pub(crate) fn read_resumed(
    context: &ResumeContext<'_>,
    permit: ParsePermit<'_>,
) -> Result<Resumed> {
    let ResumeContext {
        storage,
        cache,
        root,
        stub,
        stored,
        on_transcript_read,
        ..
    } = *context;
    let Some(cached) = &stored.cached_session else {
        return Ok(Err(WholeRead::NoCheckpoint));
    };
    let resume = match cache.stored_resume(
        storage.cache(),
        &root.path,
        &stub.cache_key,
        cached.generation(),
    )? {
        Ok(resume) => resume,
        Err(whole) => return Ok(Err(whole)),
    };
    let stamps = StubStamps::current(stub);
    let reader = storage.transcript_reader();
    // A source without a stamp either time, such as a database's session,
    // is unchanged only when its reader says so.
    let is_changed = |file: &StoredFile| -> Result<bool> {
        let stamp = stamps.fingerprint_of(&file.path);
        if stamp.is_none() && file.fingerprint.is_none() {
            return Ok(!reader.is_unchanged_since(&file.path, &file.read)?);
        }
        Ok(stamp != file.fingerprint)
    };
    let Some(own) = resume
        .files
        .first()
        .filter(|file| file.kind == "locator" && file.path == stub.locator)
    else {
        return Ok(Err(WholeRead::OwnDoesNotResume));
    };
    let stored_subagents: HashMap<&Path, &StoredFile> = resume
        .files
        .iter()
        .filter(|file| file.kind == "subagent")
        .map(|file| (file.path.as_path(), file))
        .collect();
    let current: HashSet<&Path> = stub.subagents.iter().map(PathBuf::as_path).collect();
    if resume.files.iter().any(|file| file.kind == "continuation") || own.read.stable_line.is_none()
    {
        return Ok(Err(WholeRead::OwnDoesNotResume));
    }
    if stored_subagents.keys().any(|path| !current.contains(path)) {
        return Ok(Err(WholeRead::SubagentRemoved));
    }
    let own_window = if is_changed(own)? {
        on_transcript_read();
        match resume_read(reader, permit, &own.path, &own.read)? {
            Resumption::Window(window) => Some(window),
            Resumption::FullRead => return Ok(Err(WholeRead::OwnRewritten)),
            Resumption::ChangesEarlierEntry => return Ok(Err(WholeRead::OwnChangesEarlierEntry)),
        }
    } else {
        None
    };
    let mut threads = Vec::with_capacity(stub.subagents.len());
    for path in &stub.subagents {
        let thread = match stored_subagents.get(path.as_path()) {
            None => {
                on_transcript_read();
                let Some(projection) = reader.read_whole(permit, path)? else {
                    return Ok(Err(WholeRead::SubagentUnreadable));
                };
                let share = subagent_share(path, projection);
                Thread {
                    path,
                    fold: share.fold.clone(),
                    stored_read: None,
                    stored_fold: Vec::new(),
                    change: Change::Whole(Box::new(share)),
                }
            }
            Some(file) => {
                let Some(fold) = stored_share(file) else {
                    return Ok(Err(WholeRead::SubagentShareUnreadable));
                };
                let change = if is_changed(file)? {
                    on_transcript_read();
                    resumed_change(reader, permit, path, &file.read, &fold)?
                } else {
                    Change::Unchanged
                };
                Thread {
                    path,
                    fold,
                    stored_read: Some(file.read.clone()),
                    stored_fold: file.fold.clone(),
                    change,
                }
            }
        };
        threads.push(thread);
    }
    if own_window.is_none()
        && threads
            .iter()
            .all(|thread| matches!(thread.change, Change::Unchanged))
    {
        return Ok(Err(WholeRead::NoFileChanged));
    }
    let placed = match place(context, permit, &resume, own_window.as_ref(), &mut threads)? {
        Ok(placed) => placed,
        Err(whole) => return Ok(Err(whole)),
    };
    Ok(finish(context, resume, stamps, own_window, threads, placed))
}

/// The share of its session's row `file`, a stored sub-agent transcript,
/// holds at its stable cursor; `None` when it does not decode. A transcript
/// the last read left out, holding no record, stores no share and adds
/// nothing to the row.
fn stored_share(file: &StoredFile) -> Option<ThreadFold> {
    if file.fold.is_empty() {
        return Some(ThreadFold::default());
    }
    ThreadFold::decode(&file.fold)
}

/// The bytes a resumed read read: each window's, and each file read whole.
fn bytes_read(
    stamps: &StubStamps,
    own_window: Option<&ResumedWindow>,
    threads: &[Thread<'_>],
) -> u64 {
    let window_bytes = own_window.map_or(0, |window| window.bytes_read);
    threads
        .iter()
        .map(|thread| match &thread.change {
            Change::Unchanged => 0,
            Change::Window(window) => window.bytes_read,
            Change::Whole(_) => stamps
                .fingerprint_of(thread.path)
                .map_or(0, |fingerprint| fingerprint.file_size),
        })
        .sum::<u64>()
        + window_bytes
}

/// How the changed `window` of the sub-agent transcript at `path`, whose
/// share at its stable cursor is `fold`, is taken in: a window, unless its
/// title changed or its entries add search parts, which read it whole.
fn resumed_change(
    reader: &dyn crate::history::provider::TranscriptReader,
    permit: ParsePermit<'_>,
    path: &Path,
    stored: &TranscriptRead,
    fold: &ThreadFold,
) -> Result<Change> {
    if let Resumption::Window(window) = resume_read(reader, permit, path, stored)? {
        let title = leading_title(&reader.leading_entries(path, &window.read)?);
        if title == fold.title
            && !fold.has_parts_after_stable
            && !adds_search_parts(&window.entries)
        {
            return Ok(Change::Window(window));
        }
    }
    whole_change(reader, permit, path)
}

fn whole_change(
    reader: &dyn crate::history::provider::TranscriptReader,
    permit: ParsePermit<'_>,
    path: &Path,
) -> Result<Change> {
    Ok(match reader.read_whole(permit, path)? {
        Some(projection) => Change::Whole(Box::new(subagent_share(path, projection))),
        None => Change::Unchanged,
    })
}

/// The title the custom title among `entries` names.
fn leading_title(entries: &[(RecordLine, LogEntry)]) -> Option<String> {
    entries.iter().rev().find_map(|(_, entry)| match entry {
        LogEntry::CustomTitle { custom_title } => {
            let trimmed = custom_title.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_owned())
        }
        _ => None,
    })
}

/// What the changed transcripts put after the checkpoint, and in place
/// before it.
struct Placed {
    leading: Vec<(RecordLine, LogEntry)>,
    leading_title: Option<String>,
    tail: Vec<(RecordLine, LogEntry)>,
    in_place: Vec<InPlaceRow>,
    launch_calls: Vec<Option<String>>,
    ledger: Vec<u8>,
}

fn place(
    context: &ResumeContext<'_>,
    permit: ParsePermit<'_>,
    resume: &StoredResume,
    own_window: Option<&ResumedWindow>,
    threads: &mut [Thread<'_>],
) -> Result<std::result::Result<Placed, WholeRead>> {
    let ResumeContext {
        storage,
        cache,
        stub,
        on_transcript_read,
        ..
    } = *context;
    let reader = storage.transcript_reader();
    let linker = storage.thread_linker();
    let checkpoint = &resume.checkpoint;
    let own_read = own_window.map_or(&resume.files[0].read, |window| &window.read);
    let leading = reader.leading_entries(&stub.locator, own_read)?;
    if leading_digest(&leading) != checkpoint.leading_digest {
        return Ok(Err(WholeRead::LeadingChanged));
    }
    let title = leading_title(&leading);

    // Link the windows, and read whole each thread the links change outside
    // its window.
    let link = {
        let names = linked_threads(threads);
        let mut windows = Vec::new();
        if let Some(window) = own_window {
            windows.push(LinkWindow {
                transcript: None,
                start_line: window.start_line,
                entries: &window.entries,
            });
        }
        for thread in threads.iter() {
            match &thread.change {
                Change::Unchanged => {}
                Change::Window(window) => windows.push(LinkWindow {
                    transcript: Some(thread.path),
                    start_line: window.start_line,
                    entries: &window.entries,
                }),
                Change::Whole(share) => windows.push(LinkWindow {
                    transcript: Some(thread.path),
                    start_line: 0,
                    entries: &share.thread.entries,
                }),
            }
        }
        linker.update(&resume.ledger, &windows, &names)
    };
    for thread in threads.iter_mut() {
        let rereads = link.rereads.iter().any(|path| path == thread.path);
        if rereads && !matches!(thread.change, Change::Whole(_)) {
            if matches!(thread.change, Change::Unchanged) {
                on_transcript_read();
            }
            thread.change = whole_change(reader, permit, thread.path)?;
            if let Change::Whole(share) = &thread.change {
                thread.fold = share.fold.clone();
            }
        }
    }
    let folds: Vec<(PathBuf, ThreadFold)> = threads
        .iter()
        .map(|thread| (thread.path.to_path_buf(), thread.fold.clone()))
        .collect();
    let names: Vec<LinkedThread<'_>> = folds
        .iter()
        .map(|(path, fold)| LinkedThread {
            transcript: path,
            label: &fold.label,
            identity: &fold.identity,
            started: &fold.started,
        })
        .collect();
    let order: HashMap<PathBuf, usize> = folds
        .iter()
        .enumerate()
        .map(|(index, (path, _))| (path.clone(), index))
        .collect();
    let order_of = |path: &Path| order.get(path).copied().unwrap_or(usize::MAX);

    // The turns of each changed thread, and the rows its read changes in
    // place before the checkpoint: its stored turns from the line the read
    // derives turns again from.
    let mut new_children = Vec::new();
    let mut in_place = Vec::new();
    for (index, thread) in threads.iter_mut().enumerate() {
        let launch_call = link.launch_calls[index].clone();
        let (subagent, replaced_from) = match &mut thread.change {
            Change::Unchanged => continue,
            Change::Window(window) => (
                SubagentThread {
                    label: thread.fold.label.clone(),
                    transcript: std::sync::Arc::from(thread.path),
                    identity: thread.fold.identity.clone(),
                    launch_call,
                    started: thread.fold.last_timestamp.clone(),
                    entries: window.entries.clone(),
                },
                Some(window.start_line),
            ),
            Change::Whole(share) => {
                let mut subagent = SubagentThread {
                    label: share.thread.label.clone(),
                    transcript: share.thread.transcript.clone(),
                    identity: share.thread.identity.clone(),
                    launch_call: None,
                    started: share.thread.started.clone(),
                    entries: share.thread.entries.clone(),
                };
                linker.fold(&link.ledger, &mut subagent, &names);
                subagent.launch_call = launch_call;
                (subagent, thread.stored_read.is_some().then_some(0))
            }
        };
        let mut stored_turns = match replaced_from {
            Some(from_line) => cache
                .stored_turns(resume, thread.path)?
                .into_iter()
                .filter(|turn| turn.line >= from_line)
                .map(|turn| ((turn.line, 0), turn))
                .collect::<Vec<_>>(),
            None => Vec::new(),
        };
        number_by_line(&mut stored_turns);
        let mut stored_by_key: HashMap<(usize, usize), crate::history::sqlite_cache::StoredTurn> =
            stored_turns.into_iter().collect();
        let mut nth = HashMap::<usize, usize>::new();
        for turn in subagent_turns(vec![subagent]) {
            let line = turn.record().line;
            let index_in_line = {
                let count = nth.entry(line).or_default();
                *count += 1;
                *count - 1
            };
            let key = ChildKey {
                key: turn.timestamp.clone(),
                transcript: thread.path.to_path_buf(),
                line,
                nth: index_in_line,
            };
            let is_message =
                crate::agent::message_numbering::has_visible_block(turn.role, &turn.blocks);
            let reply_id = turn.reply_id.clone();
            let entry = turn.into_progress_entry();
            if let Some(stored) = stored_by_key.remove(&(line, index_in_line))
                && (stored.sequence as usize) < checkpoint.sequence
            {
                if stored.splice_key.as_deref() != Some(key.key.as_str())
                    || stored.is_message != is_message
                    || stored.reply_id != reply_id
                {
                    return Ok(Err(WholeRead::TurnBeforeCheckpointChanged));
                }
                in_place.push(InPlaceRow {
                    record: entry.0,
                    entry: entry.1,
                    id: stored.id,
                    message: stored.message,
                    digest: stored.digest,
                });
                continue;
            }
            new_children.push(Child {
                entry,
                order: order_of(thread.path),
                key,
            });
        }
        if stored_by_key
            .values()
            .any(|stored| (stored.sequence as usize) < checkpoint.sequence)
        {
            return Ok(Err(WholeRead::TurnBeforeCheckpointChanged));
        }
    }

    // The stored entries from the checkpoint on that stay, and the windows'.
    let own_start = own_window.map(|window| window.start_line);
    let starts: HashMap<&Path, Option<usize>> = threads
        .iter()
        .filter_map(|thread| match &thread.change {
            Change::Unchanged => None,
            Change::Window(window) => Some((thread.path, Some(window.start_line))),
            Change::Whole(_) => Some((thread.path, None)),
        })
        .collect();
    let mut parents = Vec::new();
    let mut children = Vec::new();
    let mut stored_nth = HashMap::<(PathBuf, usize), usize>::new();
    for stored in cache.stored_tail(resume, checkpoint.sequence)? {
        let transcript = stored.record.transcript.clone();
        if !stored.is_child {
            let replaced =
                transcript.is_none() && own_start.is_some_and(|start| stored.record.line >= start);
            if !replaced {
                parents.push((stored.record, stored.entry));
            }
            continue;
        }
        let Some(path) = transcript.as_deref() else {
            return Ok(Err(WholeRead::CheckpointUnreadable));
        };
        let replaced = match starts.get(path) {
            Some(Some(start)) => stored.record.line >= *start,
            Some(None) => true,
            None => false,
        };
        if replaced {
            continue;
        }
        let index_in_line = {
            let count = stored_nth
                .entry((path.to_path_buf(), stored.record.line))
                .or_default();
            *count += 1;
            *count - 1
        };
        let key = ChildKey {
            key: stored.splice_key.clone().unwrap_or_default(),
            transcript: path.to_path_buf(),
            line: stored.record.line,
            nth: index_in_line,
        };
        let mut entry = (stored.record, stored.entry);
        let launch_call = link.launch_calls.get(order_of(path)).cloned().flatten();
        relink(&mut entry.1, launch_call.as_deref());
        children.push(Child {
            entry,
            order: order_of(path),
            key,
        });
    }
    let bound = &checkpoint.splice;
    let new_parents = own_window.map_or(&[][..], |window| &window.entries[..]);
    if parents.is_empty()
        && bound.has_trailing_child
        && let Some(first) = new_parents.first()
    {
        let trailing = bound.last_child.as_ref().map(|child| child.key.as_str());
        if first.1.timestamp().is_none_or(|key| Some(key) <= trailing) {
            return Ok(Err(WholeRead::EntryBeforeCheckpoint));
        }
    }
    for child in &new_children {
        let before_max_parent = bound
            .max_parent_key
            .as_deref()
            .is_some_and(|max| child.key.key.as_str() < max);
        let before_last_child = bound
            .last_child
            .as_ref()
            .is_some_and(|last| !child.key.sorts_after(last, &order_of));
        if before_max_parent || before_last_child {
            return Ok(Err(WholeRead::EntryBeforeCheckpoint));
        }
    }
    parents.extend(new_parents.iter().cloned());
    children.extend(new_children);
    children.sort_by(|left, right| {
        (&left.key.key, left.order, left.key.line, left.key.nth).cmp(&(
            &right.key.key,
            right.order,
            right.key.line,
            right.key.nth,
        ))
    });
    let mut tail = Vec::with_capacity(parents.len() + children.len());
    walk_spliced(
        parents,
        |(_, entry)| entry.timestamp(),
        children,
        |child| child.key.key.as_str(),
        |step| {
            tail.push(match step {
                Spliced::Parent(entry) => entry,
                Spliced::Child(child) => child.entry,
            })
        },
    );
    let reply_ids: Vec<&str> = tail
        .iter()
        .filter_map(|(_, entry)| reply_id(entry))
        .collect();
    if cache.continues_stored_reply(resume, &reply_ids, checkpoint.sequence)? {
        return Ok(Err(WholeRead::ReplyAcrossCheckpoint));
    }
    Ok(Ok(Placed {
        leading,
        leading_title: title,
        tail,
        in_place,
        launch_calls: link.launch_calls,
        ledger: link.ledger,
    }))
}

/// Number the turns of `turns` that share a line, in order.
fn number_by_line<T>(turns: &mut [((usize, usize), T)]) {
    let mut nth = HashMap::<usize, usize>::new();
    for ((line, index), _) in turns.iter_mut() {
        let count = nth.entry(*line).or_default();
        *index = *count;
        *count += 1;
    }
}

/// The threads as the linker reads them.
fn linked_threads<'a>(threads: &'a [Thread<'_>]) -> Vec<LinkedThread<'a>> {
    threads
        .iter()
        .map(|thread| LinkedThread {
            transcript: thread.path,
            label: &thread.fold.label,
            identity: &thread.fold.identity,
            started: &thread.fold.started,
        })
        .collect()
}

/// Give `entry`, a spliced turn read back, its thread's launching call.
fn relink(entry: &mut LogEntry, launch_call: Option<&str>) {
    if let LogEntry::Progress { extra, .. } = entry
        && let Some(extra) = extra.as_object_mut()
    {
        match launch_call {
            Some(call) => {
                extra.insert("parentToolUseID".to_owned(), call.into());
            }
            None => {
                extra.remove("parentToolUseID");
            }
        }
    }
}

/// The resumed read's row and record.
fn finish(
    context: &ResumeContext<'_>,
    resume: StoredResume,
    stamps: StubStamps,
    own_window: Option<ResumedWindow>,
    threads: Vec<Thread<'_>>,
    placed: Placed,
) -> Resumed {
    let ResumeContext {
        storage,
        root,
        stub,
        stored,
        clock,
        debug_level,
        ..
    } = *context;
    let checkpoint = &resume.checkpoint;
    let own = &resume.files[0];
    let moved = placed.leading.len() as i64 - resume.leading_rows as i64;
    let bytes_read = bytes_read(&stamps, own_window.as_ref(), &threads);
    let subagents: HashSet<&Path> = stub.subagents.iter().map(PathBuf::as_path).collect();

    // Each file's read, share and token total once the read is taken in.
    let own_read = own_window
        .as_ref()
        .map_or_else(|| own.read.clone(), |window| window.read.clone());
    let mut reads = HashMap::new();
    reads.insert(own.path.clone(), (own_read.clone(), Vec::new()));
    let mut thread_tokens = 0;
    let mut has_transient_read_error = false;
    let mut is_missing_earlier_turns = false;
    let mut search_parts = Vec::new();
    for (index, thread) in threads.iter().enumerate() {
        let (read, fold) = match &thread.change {
            Change::Unchanged => (
                thread
                    .stored_read
                    .clone()
                    .unwrap_or_else(TranscriptRead::full_read),
                thread.fold.clone(),
            ),
            Change::Window(window) => (
                window.read.clone(),
                resumed_thread_fold(&thread.fold, &window.entries, window.read.stable_line),
            ),
            Change::Whole(share) => (share.read.clone(), share.fold.clone()),
        };
        let parts: Vec<String> = match &thread.change {
            Change::Whole(share) => share.search_parts.clone(),
            _ => resume
                .search_parts
                .iter()
                .filter(|(path, _)| path == thread.path)
                .map(|(_, text)| text.clone())
                .collect(),
        };
        search_parts.extend(parts.into_iter().map(|text| SubagentSearchPart {
            file_position: index + 1,
            text,
        }));
        thread_tokens += fold.total_tokens;
        has_transient_read_error |= fold.has_transient_read_error;
        is_missing_earlier_turns |= fold.is_missing_earlier_turns;
        let encoded = match &thread.change {
            Change::Unchanged => thread.stored_fold.clone(),
            _ => fold.encode(),
        };
        reads.insert(thread.path.to_path_buf(), (read, encoded));
    }

    // The fold from the checkpoint over the entries after it, kept at the
    // next checkpoint. Store none when the own transcript's stable line moved
    // back: it can then stand before the checkpoint.
    let steps = tail_steps(&placed.tail, &subagents);
    let place = checkpoint_place_after(&steps, own_read.stable_line, checkpoint.splice.clone())
        .filter(|_| own_read.stable_line >= own.read.stable_line);
    let start = checkpoint.fold.clone().with_leading_entries_moved(moved);
    let (fold, next_fold) = fold_tail(
        &start,
        placed.leading_title.as_deref(),
        &placed.tail,
        &subagents,
        place.as_ref().map(|(sequence, _)| *sequence),
    );
    let folded =
        folded_row(&fold, &stub.locator, debug_level).map_err(|_| WholeRead::ChangedDuringRead)?;
    if checkpoint.is_headerless && folded.cwd != stored.cwd {
        return Err(WholeRead::CwdChanged);
    }
    let next_checkpoint = place.zip(next_fold).map(|((sequence, splice), fold)| {
        let sequence = (checkpoint.sequence as i64 + moved) as usize + sequence;
        (
            SessionCheckpoint {
                sequence,
                fold,
                splice,
                header_time: checkpoint.header_time.clone(),
                is_headerless: checkpoint.is_headerless,
                leading_digest: leading_digest(&placed.leading),
            },
            placed.ledger.clone(),
        )
    });

    // The malformed lines: the stored ones before the own window, then the
    // window's.
    let own_start = own_window.as_ref().map(|window| window.start_line);
    let mut malformed_lines: Vec<MalformedLine> = resume
        .malformed_lines
        .iter()
        .filter(|line| own_start.is_none_or(|start| line.line_number < start))
        .cloned()
        .collect();
    if let Some(window) = &own_window {
        malformed_lines.extend(window.malformed_lines.iter().cloned());
    }

    let mut row = stored.clone();
    row.preview_first = folded.preview_first;
    row.preview_last = folded.preview_last;
    row.preview = row.preview_first.clone();
    row.summary = folded.summary;
    row.custom_title = folded.custom_title;
    row.model = folded.model;
    row.total_tokens = folded.total_tokens + thread_tokens;
    row.duration_minutes = folded.duration_minutes;
    row.message_count = folded.message_count;
    row.assistant_messages = folded.assistant_messages;
    let activity = if checkpoint.is_headerless {
        folded.last_activity
    } else {
        folded.latest_activity
    };
    row.timestamp = session_time(
        activity,
        &checkpoint.header_time,
        crate::history::parser::RowDating::new(stub.fingerprint.modified, clock),
    );
    row.parse_errors = parse_errors_of_lines(stored.source, &malformed_lines);
    row.has_transient_read_error = has_transient_read_error;
    row.is_missing_earlier_turns = is_missing_earlier_turns;
    row.subagents = stub.subagents.clone();
    row.message_times = fold.message_times(Some((
        &stored.message_times,
        checkpoint.fold.message_count(),
    )));

    let update = WindowUpdate {
        session: resume.session,
        generation: resume.generation,
        stored_leading_rows: resume.leading_rows,
        leading: placed.leading,
        checkpoint: resume.checkpoint.clone(),
        in_place: placed.in_place,
        launch_calls: stub
            .subagents
            .iter()
            .cloned()
            .zip(placed.launch_calls)
            .collect(),
    };
    let record = SessionRecord::resumed(
        storage.cache(),
        storage.source(),
        root,
        stub,
        stamps,
        &row,
        reads,
        TranscriptEntries {
            entries: placed.tail,
            malformed_lines,
        },
        search_parts,
        next_checkpoint,
        update,
    )
    .ok_or(WholeRead::ChangedDuringRead)?;
    Ok(ResumedSession {
        row,
        record,
        bytes_read,
    })
}

/// The steps of `tail`, entries in the view's order, where a spliced turn
/// comes from one of `subagents`.
fn tail_steps<'a>(
    tail: &'a [(RecordLine, LogEntry)],
    subagents: &HashSet<&Path>,
) -> Vec<WalkStep<'a>> {
    tail.iter()
        .map(|(record, entry)| {
            let is_child = record
                .transcript
                .as_deref()
                .is_some_and(|path| subagents.contains(path))
                && matches!(entry, LogEntry::Progress { .. });
            let key = if is_child {
                match entry {
                    LogEntry::Progress { extra, .. } => Some(
                        extra
                            .get("timestamp")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or_default(),
                    ),
                    _ => None,
                }
            } else {
                entry.timestamp()
            };
            WalkStep {
                record,
                key,
                is_child,
                entry: Some(entry),
                reply_id: reply_id(entry),
            }
        })
        .collect()
}
