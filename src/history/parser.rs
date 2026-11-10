//! Building a conversation's row — preview text, message counts, search text
//! and working directory — from its transcript's [`LogEntry`] records.

use super::fingerprint::FileStamp;
use super::format::splice::{
    Spliced, SubagentThread, SubagentTurn, splice_turns, subagent_turns, walk_spliced,
};
use super::format::{RecordLine, SessionFormat, SessionProjection};
use super::message_texts::{MessageTextBuilder, MessageTextStarts, push_separated};
use super::provider::{ReadError, SessionStub, TranscriptRead};
use super::resumed::{SessionCheckpoint, ThreadFold, checkpoint_place, leading_digest, walk_steps};
use super::row_fold::{FoldCheckpoint, FoldedRow, RowFold, Unlisted};
use super::{Conversation, ParseError, ParsePermit, Source, TranscriptEntries, parse_task_report};
use crate::agent::message_numbering::{ThreadIdentity, has_visible_block};
use crate::agent::refs::MessageRange;
use crate::agent::sanitize::{sanitize_agent_text, without_terminal_codes};
use crate::agent::transcript::{
    AgentMessageRole, agent_search_parts_from_blocks, thinking_and_tool_call_parts,
};
use crate::cli::DebugLevel;
use crate::clock::Clock;
use crate::debug;
use crate::error::{AppError, Result};
use crate::log_entry::{
    AgentContent, ContentBlock, LogEntry, UserContent, extract_text_from_assistant,
    extract_text_from_blocks, extract_text_from_user, labeled_metadata_text, parse_agent_progress,
    progress_timestamp, search_texts_of_blocks, search_texts_of_user,
};
use crate::search::normalize_for_search;
use crate::semantic::filter::{SemanticTurnRole, filter_turn};
use chrono::{DateTime, Local};
use rayon::prelude::*;
use std::collections::{BTreeSet, HashMap};

use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// The reason a session's parse yields no row for the list. The session
/// cache records it, so agent commands can warn about the session without
/// parsing it again.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UnlistedReason {
    /// The file is not a transcript in the agent's format.
    ForeignTranscript,
    /// The transcript holds no message the list numbers.
    NoVisibleMessages,
    /// The transcript holds messages, but no text the list previews, or only
    /// `/clear` commands.
    NoConversationMetadata,
}

/// A session's parse: the row the list shows with the transcripts the parse
/// read, or the reason it shows none.
pub type ParsedSession = std::result::Result<ListedSession, UnlistedReason>;

/// A row builder's result: the row the list shows, or the reason it shows
/// none.
pub(crate) type ParsedRow = std::result::Result<Conversation, UnlistedReason>;

/// A listed session's parse.
pub struct ListedSession {
    pub row: Conversation,
    pub(crate) transcripts: SessionTranscripts,
}

/// What a listed session's parse read of its transcripts. The session
/// cache's record is built from it, so no transcript is read twice.
pub(crate) struct SessionTranscripts {
    /// The session's own entries and malformed lines.
    own: TranscriptEntries,
    /// Each earlier transcript the session's own continues, stamped before
    /// it was read.
    own_earlier_transcripts: Vec<FileStamp>,
    /// What the read of the session's own transcript leaves for the next.
    own_read: TranscriptRead,
    subagents: SubagentTranscripts,
    /// Where a resumed read of the session starts; `None` when its own
    /// transcript does not resume.
    pub(crate) checkpoint: Option<SessionCheckpoint>,
    /// The agent's ledger of what links the session's threads.
    pub(crate) ledger: Vec<u8>,
}

impl SessionTranscripts {
    pub(crate) fn new(
        own: TranscriptEntries,
        own_earlier_transcripts: Vec<FileStamp>,
        own_read: TranscriptRead,
        subagents: SubagentTranscripts,
    ) -> Self {
        Self {
            own,
            own_earlier_transcripts,
            own_read,
            subagents,
            checkpoint: None,
            ledger: Vec::new(),
        }
    }

    /// The sub-agent transcript at `path`'s share of the row at its stable
    /// cursor, encoded; `None` when the parse did not read it.
    pub(crate) fn subagent_fold(&self, path: &Path) -> Option<&[u8]> {
        self.subagents.folds.get(path).map(Vec::as_slice)
    }

    /// What the read of the session's own transcript leaves for the next.
    pub(crate) fn own_read(&self) -> &TranscriptRead {
        &self.own_read
    }

    /// What the read of the sub-agent transcript at `path` leaves for the
    /// next; `None` when the parse did not read it.
    pub(crate) fn subagent_read(&self, path: &Path) -> Option<&TranscriptRead> {
        self.subagents.reads.get(path)
    }

    /// Every transcript the session's entries and its sub-agents' records
    /// came from, beside the session's own: each sub-agent's, and each
    /// earlier transcript the session or a sub-agent continues.
    pub(crate) fn transcripts_read(&self) -> BTreeSet<&Path> {
        self.own
            .entries
            .iter()
            .filter_map(|(record, _)| record.transcript.as_deref())
            .chain(
                self.subagents
                    .turns
                    .iter()
                    .filter_map(SubagentTurn::transcript),
            )
            .chain(self.subagents.sources.iter().map(PathBuf::as_path))
            .collect()
    }

    /// The stamp taken before the read of `path`, an earlier transcript the
    /// session or one of its sub-agents continues.
    pub(crate) fn stamp_before_read(&self, path: &Path) -> Option<&FileStamp> {
        self.own_earlier_transcripts
            .iter()
            .chain(&self.subagents.earlier_transcripts)
            .find(|stamp| stamp.path == path)
    }

    /// The entries the viewer reads, each sub-agent turn spliced into the
    /// session's own, and the sub-agents' search parts.
    pub(crate) fn into_entries_and_search_parts(
        self,
    ) -> (TranscriptEntries, Vec<SubagentSearchPart>) {
        let entries = TranscriptEntries {
            entries: splice_turns(self.own.entries, self.subagents.turns),
            malformed_lines: self.own.malformed_lines,
        };
        (entries, self.subagents.search_parts)
    }
}

/// A sub-agent transcript read whole, as a resumed read takes it in.
pub(crate) struct SubagentShare {
    pub(crate) fold: ThreadFold,
    pub(crate) search_parts: Vec<String>,
    pub(crate) thread: SubagentThread,
    pub(crate) read: TranscriptRead,
}

/// `projection`, the sub-agent transcript at `path` read whole, as a
/// resumed read takes it in.
pub(crate) fn subagent_share(path: &Path, mut projection: SessionProjection) -> SubagentShare {
    let (_, fold) = SubagentContribution::of_projection(&projection);
    let search_parts =
        SubagentContribution::search_parts(projection.entries.iter().map(|(_, entry)| entry));
    let read = std::mem::replace(&mut projection.transcript_read, TranscriptRead::full_read());
    SubagentShare {
        fold,
        search_parts,
        thread: SubagentThread::of_projection(path, projection),
        read,
    }
}

/// `fold`, a sub-agent transcript's share of its row at its stable cursor,
/// once `entries`, read from that cursor on, follow it; the new share stands
/// at `stable_line`.
pub(crate) fn resumed_thread_fold(
    fold: &ThreadFold,
    entries: &[(RecordLine, LogEntry)],
    stable_line: Option<usize>,
) -> ThreadFold {
    let stable_index = stable_line.map(|line| {
        entries
            .iter()
            .position(|(record, _)| record.line >= line)
            .unwrap_or(entries.len())
    });
    let mut builder = ConversationBuilder {
        keeps_only_text_outside_turns: true,
        fold: RowFold::resumed(&fold.fold),
        checkpoint_at: stable_index,
        ..ConversationBuilder::default()
    };
    builder.push_spliced(entries.iter().map(|(_, entry)| entry), &[]);
    let checkpoint = builder.checkpoint.take();
    let before_stable = &entries[..stable_index.unwrap_or(0)];
    ThreadFold {
        last_timestamp: before_stable
            .iter()
            .rev()
            .find_map(|(_, entry)| entry.timestamp())
            .map_or_else(|| fold.last_timestamp.clone(), str::to_owned),
        fold: checkpoint.unwrap_or_default(),
        has_parts_after_stable: stable_index.is_some_and(|index| {
            !SubagentContribution::search_parts(entries[index..].iter().map(|(_, entry)| entry))
                .is_empty()
        }),
        total_tokens: builder.fold.total_tokens(),
        ..fold.clone()
    }
}

/// True when `entries` add to their sub-agent transcript's search parts.
pub(crate) fn adds_search_parts(entries: &[(RecordLine, LogEntry)]) -> bool {
    !SubagentContribution::search_parts(entries.iter().map(|(_, entry)| entry)).is_empty()
}

/// The session row's fold resumed from `checkpoint`, its leading entries
/// naming `leading_title`, over `tail`, the session's entries from the
/// checkpoint on in the view's order, where a spliced turn comes from one of
/// `subagents`; with the fold as it stood before the entry at
/// `checkpoint_at` of `tail`.
pub(crate) fn fold_tail(
    checkpoint: &FoldCheckpoint,
    leading_title: Option<&str>,
    tail: &[(RecordLine, LogEntry)],
    subagents: &std::collections::HashSet<&Path>,
    checkpoint_at: Option<usize>,
) -> (RowFold, Option<FoldCheckpoint>) {
    let mut fold = RowFold::resumed(checkpoint);
    fold.replace_leading_title(leading_title);
    let mut builder = ConversationBuilder {
        fold,
        checkpoint_at,
        ..ConversationBuilder::default()
    };
    for (record, entry) in tail {
        builder.keep_checkpoint();
        match spliced_mark(record, entry, subagents) {
            Some(mark) => builder.push_mark(&mark),
            None => builder.push_record(Some(record), entry),
        }
        builder.steps += 1;
    }
    builder.keep_checkpoint();
    let checkpoint = builder.checkpoint.take();
    (builder.fold, checkpoint)
}

/// `entry`, read at `record`, as the mark of the spliced turn it is, when
/// it came from one of `subagents`. The mark carries no texts.
fn spliced_mark(
    record: &RecordLine,
    entry: &LogEntry,
    subagents: &std::collections::HashSet<&Path>,
) -> Option<SubagentMark> {
    let transcript = record.transcript.as_deref()?;
    if !subagents.contains(transcript) {
        return None;
    }
    let LogEntry::Progress { data, extra } = entry else {
        return None;
    };
    let progress = parse_agent_progress(data)?;
    let role = AgentMessageRole::of_progress(&progress.message.message_type)?;
    let AgentContent::Blocks(blocks) = progress.message.message.content;
    Some(SubagentMark {
        thread: ThreadIdentity::of(entry, Some(transcript)),
        record: record.clone(),
        timestamp: progress_timestamp(extra).unwrap_or_default(),
        is_message: has_visible_block(role, &blocks),
        reply_id: progress.message.message.id,
        agent_search_parts: Vec::new(),
        thinking_and_tool_call_parts: Vec::new(),
    })
}

/// One search part of a sub-agent transcript: text outside its spliced
/// turns.
pub(crate) struct SubagentSearchPart {
    /// The transcript's position among the row's sub-agents, from 1.
    pub(crate) file_position: usize,
    pub(crate) text: String,
}

/// Process a single conversation file, letting the registry decide which format
/// owns it.
#[cfg(test)]
pub fn process_conversation_file(
    path: PathBuf,
    modified: Option<SystemTime>,
    debug_level: Option<DebugLevel>,
) -> Result<Option<Conversation>> {
    super::format::sniffed_conversation(
        &path,
        RowDating::new(modified, &crate::clock::TestClock::default()),
        debug_level,
    )
}

/// Process a session as `format` rather than asking the registry, its
/// sub-agent transcripts merged in. `on_transcript_read` is called once for
/// each transcript read: the session's own, then each sub-agent's. A row
/// without any time is dated `clock`'s current time.
///
/// A caller that knows which root a transcript came from knows more than the file
/// does: a Pi-family transcript with no OMP title slot belongs to whichever agent
/// owns the directory holding it.
pub fn process_session_file(
    permit: ParsePermit<'_>,
    stub: &SessionStub,
    format: &dyn SessionFormat,
    debug_level: Option<DebugLevel>,
    on_transcript_read: &(dyn Fn() + Sync),
    clock: &dyn Clock,
) -> Result<ParsedSession> {
    let session = format.parse_transcript(permit, &stub.locator)?;
    process_projected_session(
        permit,
        stub,
        session,
        format,
        debug_level,
        on_transcript_read,
        clock,
    )
}

/// Build the row from an already projected session. Parse each sub-agent
/// transcript once as `subagent_format`: number its turns where the view
/// splices them in, and add its text, tokens and read errors. Skip an
/// unrecognized sub-agent transcript. `on_transcript_read` is called for the
/// session's own transcript, already read by the caller, and then once for
/// each sub-agent transcript.
pub fn process_projected_session(
    permit: ParsePermit<'_>,
    stub: &SessionStub,
    session: Option<SessionProjection>,
    subagent_format: &dyn SessionFormat,
    debug_level: Option<DebugLevel>,
    on_transcript_read: &(dyn Fn() + Sync),
    clock: &dyn Clock,
) -> Result<ParsedSession> {
    on_transcript_read();
    let Some(session) = session else {
        return Ok(Err(UnlistedReason::ForeignTranscript));
    };
    let subagents =
        SubagentReads::of_projections(permit, stub, subagent_format, on_transcript_read);
    Ok(row_with_subagents(
        stub,
        session,
        subagents,
        Vec::new(),
        debug_level,
        clock,
    ))
}

/// [`process_projected_session`] for a format whose sub-agent threads
/// depend on each other and on the session: `link` rewrites the threads,
/// given the session's entries, before their turns are numbered, and
/// returns its ledger of what links them. Every sub-agent transcript is read
/// and held at once.
#[allow(clippy::too_many_arguments)]
pub fn process_linked_session(
    permit: ParsePermit<'_>,
    stub: &SessionStub,
    session: Option<SessionProjection>,
    subagent_format: &dyn SessionFormat,
    link: impl FnOnce(&[(RecordLine, LogEntry)], &mut [SubagentThread]) -> Vec<u8>,
    debug_level: Option<DebugLevel>,
    on_transcript_read: &(dyn Fn() + Sync),
    clock: &dyn Clock,
) -> Result<ParsedSession> {
    on_transcript_read();
    let Some(session) = session else {
        return Ok(Err(UnlistedReason::ForeignTranscript));
    };
    let (subagents, ledger) = SubagentReads::of_linked_projections(
        permit,
        stub,
        subagent_format,
        on_transcript_read,
        |threads| link(&session.entries, threads),
    );
    Ok(row_with_subagents(
        stub,
        session,
        subagents,
        ledger,
        debug_level,
        clock,
    ))
}

fn row_with_subagents(
    stub: &SessionStub,
    mut session: SessionProjection,
    subagents: SubagentReads,
    ledger: Vec<u8>,
    debug_level: Option<DebugLevel>,
    clock: &dyn Clock,
) -> ParsedSession {
    let steps = walk_steps(&session.entries, &subagents.marks, |mark| {
        (
            &mark.record,
            mark.timestamp.as_str(),
            mark.reply_id.as_deref(),
        )
    });
    let place = checkpoint_place(&steps, session.transcript_read.stable_line);
    drop(steps);
    let (mut row, fold) = spliced_conversation_with_checkpoint(
        stub.locator.clone(),
        &session,
        &subagents.marks,
        place.as_ref().map(|(sequence, _)| *sequence),
        RowDating::new(stub.fingerprint.modified, clock),
        debug_level,
    )?;
    let checkpoint = place
        .zip(fold)
        .map(|((sequence, splice), fold)| SessionCheckpoint {
            sequence,
            fold,
            splice,
            header_time: session.header.timestamp.clone(),
            is_headerless: false,
            leading_digest: leading_digest(&session.entries),
        });
    let subagents = subagents.merge_into(&mut row, &stub.subagents, debug_level);
    let own_earlier_transcripts = std::mem::take(&mut session.earlier_transcripts);
    let own_read = std::mem::replace(&mut session.transcript_read, TranscriptRead::full_read());
    let mut transcripts = SessionTranscripts::new(
        TranscriptEntries::of_projection(session),
        own_earlier_transcripts,
        own_read,
        subagents,
    );
    transcripts.checkpoint = checkpoint;
    transcripts.ledger = ledger;
    Ok(ListedSession { row, transcripts })
}

/// The parse of a session whose format states no header, as
/// [`headerless_session`] builds it.
pub(crate) struct HeaderlessSession<'a> {
    pub(crate) source: Source,
    pub(crate) stub: &'a SessionStub,
    /// The id the format's own rule names the session by.
    pub(crate) session_id: String,
    pub(crate) own: TranscriptEntries,
    /// What the read of the session's own transcript leaves for the next.
    pub(crate) own_read: TranscriptRead,
    pub(crate) subagents: SubagentReads,
    /// The format's ledger of what links the session's threads.
    pub(crate) ledger: Vec<u8>,
    pub(crate) parse_errors: Vec<ParseError>,
    pub(crate) dating: RowDating<'a>,
    pub(crate) debug_level: Option<DebugLevel>,
}

/// The listed session `session` reads as: its row dated by its last
/// timestamped message, else by `session.dating`, since Claude Code appends
/// untimestamped bookkeeping records when it reopens a session; with the
/// checkpoint a resumed read starts from.
pub(crate) fn headerless_session(session: HeaderlessSession<'_>) -> ParsedSession {
    let HeaderlessSession {
        source,
        stub,
        session_id,
        own,
        own_read,
        subagents,
        ledger,
        parse_errors,
        dating,
        debug_level,
    } = session;
    let steps = walk_steps(&own.entries, &subagents.marks, |mark| {
        (
            &mark.record,
            mark.timestamp.as_str(),
            mark.reply_id.as_deref(),
        )
    });
    let place = checkpoint_place(&steps, own_read.stable_line);
    drop(steps);
    let mut builder = ConversationBuilder {
        parse_errors,
        checkpoint_at: place.as_ref().map(|(sequence, _)| *sequence),
        ..ConversationBuilder::default()
    };
    builder.push_spliced_records(&own.entries, &subagents.marks);
    let fold = builder.checkpoint.take();
    let timestamp = builder
        .fold
        .last_activity()
        .map(|last| last.with_timezone(&Local))
        .unwrap_or_else(|| dating.fallback_time());
    let mut row = builder.finish(source, stub.locator.clone(), timestamp, debug_level)?;
    row.session_id = session_id;
    let checkpoint = place
        .zip(fold)
        .map(|((sequence, splice), fold)| SessionCheckpoint {
            sequence,
            fold,
            splice,
            header_time: String::new(),
            is_headerless: true,
            leading_digest: leading_digest(&own.entries),
        });
    let subagents = subagents.merge_into(&mut row, &stub.subagents, debug_level);
    let mut transcripts = SessionTranscripts::new(own, Vec::new(), own_read, subagents);
    transcripts.checkpoint = checkpoint;
    transcripts.ledger = ledger;
    Ok(ListedSession { row, transcripts })
}

/// Read each of `stub`'s sub-agent transcripts with `read`, announcing each
/// to `on_transcript_read`, in parallel a pool's width at a time: the batch
/// bounds how many parsed transcripts wait in memory. The reads keep the
/// stub's order, so the merged text is the same on every load.
fn read_subagent_transcripts<T: Send>(
    stub: &SessionStub,
    on_transcript_read: &(dyn Fn() + Sync),
    read: impl Fn(&Path) -> T + Sync,
) -> Vec<T> {
    let mut reads = Vec::with_capacity(stub.subagents.len());
    for batch in stub.subagents.chunks(rayon::current_num_threads()) {
        reads.par_extend(batch.par_iter().map(|subagent| {
            let read = read(subagent);
            on_transcript_read();
            read
        }));
    }
    reads
}

/// A sub-agent turn as the session's row numbers it: where the view splices
/// it in, whether `agent read` numbers it as a message, its reply id, and the
/// parts the parent writes at its message in `agent_search_text` and
/// `thinking_and_tool_call_text`.
pub(crate) struct SubagentMark {
    thread: ThreadIdentity,
    record: RecordLine,
    timestamp: String,
    is_message: bool,
    reply_id: Option<String>,
    agent_search_parts: Vec<String>,
    thinking_and_tool_call_parts: Vec<String>,
}

/// `turns` as the row numbers them, in their order.
fn subagent_marks(turns: &[SubagentTurn]) -> Vec<SubagentMark> {
    turns
        .iter()
        .map(|turn| {
            let search_parts = match turn.role {
                AgentMessageRole::User => user_blocks_text(&turn.blocks).search_parts,
                AgentMessageRole::Assistant => search_texts_of_blocks(&turn.blocks),
            };
            SubagentMark {
                thread: turn.thread_identity(),
                record: turn.record().clone(),
                is_message: has_visible_block(turn.role, &turn.blocks),
                timestamp: turn.timestamp.clone(),
                reply_id: turn.reply_id.clone(),
                agent_search_parts: search_parts
                    .iter()
                    .map(|part| sanitize_agent_text(part))
                    .collect(),
                thinking_and_tool_call_parts: thinking_and_tool_call_parts(&turn.blocks),
            }
        })
        .collect()
}

/// A session's sub-agent transcripts, each read once. Each transcript's
/// [`SubagentContribution`] joins the session's row, and its turns are
/// numbered among the session's messages.
#[derive(Default)]
pub(crate) struct SubagentReads {
    /// One contribution per recognized transcript, with or without user text,
    /// in the stub's order.
    contributions: Vec<SubagentContribution>,
    /// The numbering of each turn in `transcripts`, in their order.
    marks: Vec<SubagentMark>,
    transcripts: SubagentTranscripts,
    unreadable: Vec<(PathBuf, AppError)>,
}

/// What a session's sub-agent transcripts hold for its record in the
/// session cache.
#[derive(Default)]
pub(crate) struct SubagentTranscripts {
    /// Every sub-agent turn, in the view's splice order.
    turns: Vec<SubagentTurn>,
    search_parts: Vec<SubagentSearchPart>,
    sources: BTreeSet<PathBuf>,
    earlier_transcripts: Vec<FileStamp>,
    /// What the read of each sub-agent transcript leaves for the next, by
    /// its path.
    reads: HashMap<PathBuf, TranscriptRead>,
    /// Each sub-agent transcript's encoded [`ThreadFold`], by its path.
    folds: HashMap<PathBuf, Vec<u8>>,
}

impl SubagentTranscripts {
    /// Add `read`, the sub-agent transcript at `path`, at `file_position`
    /// among the row's sub-agents, counted from 1.
    pub(crate) fn add(&mut self, file_position: usize, path: &Path, read: SubagentRecordParts) {
        self.search_parts.extend(
            read.search_parts
                .into_iter()
                .map(|text| SubagentSearchPart {
                    file_position,
                    text,
                }),
        );
        self.sources.extend(read.sources);
        self.earlier_transcripts.extend(read.earlier_transcripts);
        self.reads.insert(path.to_path_buf(), read.transcript_read);
        self.folds.insert(path.to_path_buf(), read.thread_fold);
    }
}

/// One sub-agent transcript's part of its session's record, beside its
/// turns.
pub(crate) struct SubagentRecordParts {
    search_parts: Vec<String>,
    /// The transcripts the records came from, beside the sub-agent's own.
    sources: BTreeSet<PathBuf>,
    earlier_transcripts: Vec<FileStamp>,
    transcript_read: TranscriptRead,
    /// The transcript's [`ThreadFold`], encoded; empty when not folded.
    thread_fold: Vec<u8>,
}

impl SubagentRecordParts {
    /// The parts of the sub-agent transcript holding `entries`, its earlier
    /// transcripts stamped as `earlier_transcripts`, read through the full
    /// read.
    pub(crate) fn of_entries(
        entries: &[(RecordLine, LogEntry)],
        earlier_transcripts: Vec<FileStamp>,
    ) -> Self {
        Self {
            search_parts: SubagentContribution::search_parts(
                entries.iter().map(|(_, entry)| entry),
            ),
            sources: entries
                .iter()
                .filter_map(|(record, _)| record.transcript.as_deref().map(Path::to_path_buf))
                .collect(),
            earlier_transcripts,
            transcript_read: TranscriptRead::full_read(),
            thread_fold: Vec::new(),
        }
    }

    /// [`Self::of_projection`] for `projection`, a sub-agent transcript read
    /// whole, with its share of the row at its stable cursor.
    pub(crate) fn of_read(projection: &mut SessionProjection) -> Self {
        let (_, fold) = SubagentContribution::of_projection(projection);
        Self::of_projection(projection, fold)
    }

    /// [`Self::of_entries`] for `projection`, whose share of the row at its
    /// stable cursor is `fold`; takes its earlier transcripts and what its
    /// read leaves for the next.
    fn of_projection(projection: &mut SessionProjection, fold: ThreadFold) -> Self {
        let earlier_transcripts = std::mem::take(&mut projection.earlier_transcripts);
        Self {
            transcript_read: std::mem::replace(
                &mut projection.transcript_read,
                TranscriptRead::full_read(),
            ),
            thread_fold: fold.encode(),
            ..Self::of_entries(&projection.entries, earlier_transcripts)
        }
    }
}

impl SubagentReads {
    /// Build each thread's contribution and turns from the entries the view
    /// splices in, so the row counts the text and tokens the view shows.
    /// `transcripts` holds each thread's record parts.
    pub(crate) fn of_threads(
        threads: Vec<SubagentThread>,
        mut transcripts: SubagentTranscripts,
        unreadable: Vec<(PathBuf, AppError)>,
    ) -> Self {
        let contributions = threads
            .iter()
            .map(|thread| {
                SubagentContribution::of_entries(thread.entries.iter().map(|(_, entry)| entry))
            })
            .collect();
        transcripts.turns = subagent_turns(threads);
        Self {
            contributions,
            marks: subagent_marks(&transcripts.turns),
            transcripts,
            unreadable,
        }
    }

    /// Read each of `stub`'s sub-agent transcripts once as `format`, for its
    /// contribution, its turns and its record parts.
    fn of_projections(
        permit: ParsePermit<'_>,
        stub: &SessionStub,
        format: &dyn SessionFormat,
        on_transcript_read: &(dyn Fn() + Sync),
    ) -> Self {
        let transcripts = read_subagent_transcripts(stub, on_transcript_read, |subagent| {
            let projection = format.parse_transcript(permit, subagent)?;
            Ok::<_, AppError>(projection.map(|mut projection| {
                let (contribution, fold) = SubagentContribution::of_projection(&projection);
                let record = SubagentRecordParts::of_projection(&mut projection, fold);
                let thread = SubagentThread::of_projection(subagent, projection);
                (contribution, record, subagent_turns(vec![thread]))
            }))
        });
        let mut reads = Self::default();
        for (index, (subagent, transcript)) in stub.subagents.iter().zip(transcripts).enumerate() {
            match transcript {
                Ok(Some((contribution, record, turns))) => {
                    reads.contributions.push(contribution);
                    reads.transcripts.add(index + 1, subagent, record);
                    reads.transcripts.turns.extend(turns);
                }
                Ok(None) => {}
                Err(error) => reads.unreadable.push((subagent.clone(), error)),
            }
        }
        // Stable, so turns at one timestamp keep their threads' order, as the
        // view sorts the turns of every thread at once.
        reads
            .transcripts
            .turns
            .sort_by(|left, right| left.timestamp.cmp(&right.timestamp));
        reads.marks = subagent_marks(&reads.transcripts.turns);
        reads
    }

    /// Read every one of `stub`'s sub-agent transcripts as `format` and hold
    /// the threads at once, for `link` to rewrite before their turns are
    /// numbered. Each contribution and record part comes from its transcript
    /// as read.
    fn of_linked_projections(
        permit: ParsePermit<'_>,
        stub: &SessionStub,
        format: &dyn SessionFormat,
        on_transcript_read: &(dyn Fn() + Sync),
        link: impl FnOnce(&mut [SubagentThread]) -> Vec<u8>,
    ) -> (Self, Vec<u8>) {
        let transcripts = read_subagent_transcripts(stub, on_transcript_read, |subagent| {
            let projection = format.parse_transcript(permit, subagent)?;
            Ok::<_, AppError>(projection.map(|mut projection| {
                let (contribution, fold) = SubagentContribution::of_projection(&projection);
                let record = SubagentRecordParts::of_projection(&mut projection, fold);
                (
                    contribution,
                    record,
                    SubagentThread::of_projection(subagent, projection),
                )
            }))
        });
        let mut contributions = Vec::new();
        let mut records = SubagentTranscripts::default();
        let mut threads = Vec::new();
        let mut unreadable = Vec::new();
        for (index, (subagent, transcript)) in stub.subagents.iter().zip(transcripts).enumerate() {
            match transcript {
                Ok(Some((contribution, record, thread))) => {
                    contributions.push(contribution);
                    records.add(index + 1, subagent, record);
                    threads.push(thread);
                }
                Ok(None) => {}
                Err(error) => unreadable.push((subagent.clone(), error)),
            }
        }
        let ledger = link(&mut threads);
        records.turns = subagent_turns(threads);
        let reads = Self {
            contributions,
            marks: subagent_marks(&records.turns),
            transcripts: records,
            unreadable,
        };
        (reads, ledger)
    }

    /// Add each contribution to `session`, and report each unreadable
    /// transcript. `session` is the row built with these marks. Returns the
    /// transcripts' part of the session's record.
    ///
    /// An unreadable transcript is left out and reported at warn level, as an
    /// unreadable session is, rather than failing the session: its own
    /// transcript still reads, and failing it would delist it until the
    /// transcript changed on disk. A transient error reading the transcript,
    /// or reported by its contribution, also sets `has_transient_read_error`,
    /// so the session is read again. The row names every transcript in
    /// `subagents`, an unreadable one included, as discovery found them.
    pub(crate) fn merge_into(
        self,
        session: &mut Conversation,
        subagents: &[PathBuf],
        debug_level: Option<DebugLevel>,
    ) -> SubagentTranscripts {
        for contribution in self.contributions {
            contribution.add_to(session);
        }
        for (subagent, error) in self.unreadable {
            super::format::report_unreadable_subagent(
                debug_level,
                session.source,
                &session.session_id,
                &subagent,
                &error,
            );
            if ReadError::of(&error) == ReadError::Transient {
                session.has_transient_read_error = true;
            }
        }
        session.subagents = subagents.to_vec();
        self.transcripts
    }
}

/// One sub-agent transcript's tokens, read errors and text outside its
/// turns, for its parent session's row. Every transcript contributes, with or
/// without user text; the preview requirement decides only whether a session
/// is listed. Each turn's text comes from its [`SubagentMark`].
#[derive(Default)]
pub(crate) struct SubagentContribution {
    /// The transcript's title, summary, metadata rows and nested sub-agents'
    /// messages: text the view does not show among the parent's messages.
    pub(crate) text_outside_turns: String,
    pub(crate) total_tokens: u64,
    pub(crate) has_transient_read_error: bool,
    pub(crate) is_missing_earlier_turns: bool,
}

impl SubagentContribution {
    /// Raw text pieces omitted by the dialogue splice: titles, searchable
    /// metadata and nested progress. Keep boundaries and normalise on read.
    pub(crate) fn search_parts<'a>(entries: impl IntoIterator<Item = &'a LogEntry>) -> Vec<String> {
        let mut builder = ConversationBuilder {
            keeps_only_text_outside_turns: true,
            ..Default::default()
        };
        builder.push_spliced(entries, &[]);
        let leading = [builder.fold.custom_title(), builder.fold.summary()]
            .into_iter()
            .flatten()
            .cloned()
            .collect::<Vec<_>>();
        leading
            .into_iter()
            .chain(builder.full_text.into_parts())
            .chain(builder.agent_search_text.into_parts())
            .collect()
    }

    /// Build the contribution from a transcript's `entries`, in file order.
    fn of_entries<'a>(entries: impl IntoIterator<Item = &'a LogEntry>) -> Self {
        Self::of_entries_with_checkpoint(entries, None).0
    }

    /// [`Self::of_entries`], with the fold as it stood before the entry at
    /// `checkpoint_at`.
    fn of_entries_with_checkpoint<'a>(
        entries: impl IntoIterator<Item = &'a LogEntry>,
        checkpoint_at: Option<usize>,
    ) -> (Self, Option<FoldCheckpoint>) {
        let mut builder = ConversationBuilder {
            keeps_only_text_outside_turns: true,
            checkpoint_at,
            ..ConversationBuilder::default()
        };
        builder.push_spliced(entries, &[]);
        let checkpoint = builder.checkpoint.take();
        (builder.subagent_contribution(), checkpoint)
    }

    /// The contribution of `projection`, a sub-agent transcript, and its
    /// share of the row at its stable cursor.
    fn of_projection(projection: &SessionProjection) -> (Self, ThreadFold) {
        let entries = &projection.entries;
        let stable_index = projection.transcript_read.stable_line.map(|line| {
            entries
                .iter()
                .position(|(record, _)| record.line >= line)
                .unwrap_or(entries.len())
        });
        let (contribution, checkpoint) =
            Self::of_entries_with_checkpoint(entries.iter().map(|(_, entry)| entry), stable_index);
        let before_stable = &entries[..stable_index.unwrap_or(0)];
        let header = &projection.header;
        let fold = ThreadFold {
            label: header.thread_label().to_owned(),
            identity: header.subagent_identity.clone(),
            started: header.timestamp.clone(),
            last_timestamp: before_stable
                .iter()
                .rev()
                .find_map(|(_, entry)| entry.timestamp())
                .unwrap_or(&header.timestamp)
                .to_owned(),
            fold: checkpoint.unwrap_or_default(),
            has_parts_after_stable: stable_index.is_some_and(|index| {
                !Self::search_parts(entries[index..].iter().map(|(_, entry)| entry)).is_empty()
            }),
            title: projection.title.clone(),
            total_tokens: contribution.total_tokens,
            is_missing_earlier_turns: projection.earlier_turns.is_missing(),
            has_transient_read_error: projection.earlier_turns.is_transient_error(),
        };
        let contribution = Self {
            has_transient_read_error: fold.has_transient_read_error,
            is_missing_earlier_turns: fold.is_missing_earlier_turns,
            ..contribution
        };
        (contribution, fold)
    }

    /// Add the text to the end of `session`'s `agent_search_text`, outside
    /// the session list's search and outside every message, and add the
    /// tokens and read errors. Do not add a message: the session's numbering
    /// already counts each spliced turn. The caller rebuilds the semantic
    /// routing text once, after the last contribution.
    fn add_to(self, session: &mut Conversation) {
        push_separated(&mut session.agent_search_text, &self.text_outside_turns);
        session.total_tokens += self.total_tokens;
        session.has_transient_read_error |= self.has_transient_read_error;
        session.is_missing_earlier_turns |= self.is_missing_earlier_turns;
    }
}

/// The time a row is dated when its transcript does not show one: the
/// transcript's modification time, else the clock's current time.
#[derive(Clone, Copy)]
pub(crate) struct RowDating<'a> {
    modified: Option<SystemTime>,
    clock: &'a dyn Clock,
}

impl<'a> RowDating<'a> {
    pub(crate) fn new(modified: Option<SystemTime>, clock: &'a dyn Clock) -> Self {
        Self { modified, clock }
    }

    fn fallback_time(&self) -> DateTime<Local> {
        self.modified
            .map(DateTime::<Local>::from)
            .unwrap_or_else(|| self.clock.wall().with_timezone(&Local))
    }
}

/// Build a conversation from an already normalized transcript.
///
/// The entries are handed to the builder directly. Serializing them back to JSON
/// so a reader could parse them again would hold two more full copies of the
/// file's content in memory.
pub(crate) fn conversation_from_projection(
    path: PathBuf,
    projection: &SessionProjection,
    dating: RowDating<'_>,
    debug_level: Option<DebugLevel>,
) -> ParsedRow {
    spliced_conversation_from_projection(path, projection, &[], dating, debug_level)
}

/// [`conversation_from_projection`], each sub-agent turn in `marks` numbered
/// where the view splices it in.
fn spliced_conversation_from_projection(
    path: PathBuf,
    projection: &SessionProjection,
    marks: &[SubagentMark],
    dating: RowDating<'_>,
    debug_level: Option<DebugLevel>,
) -> ParsedRow {
    spliced_conversation_with_checkpoint(path, projection, marks, None, dating, debug_level)
        .map(|(conversation, _)| conversation)
}

/// [`spliced_conversation_from_projection`], with the row's fold as it
/// stood before the entry at `checkpoint_at` in the view's order.
fn spliced_conversation_with_checkpoint(
    path: PathBuf,
    projection: &SessionProjection,
    marks: &[SubagentMark],
    checkpoint_at: Option<usize>,
    dating: RowDating<'_>,
    debug_level: Option<DebugLevel>,
) -> std::result::Result<(Conversation, Option<FoldCheckpoint>), UnlistedReason> {
    let mut builder = ConversationBuilder {
        parse_errors: parse_errors_of(projection.source, &projection.malformed_lines),
        checkpoint_at,
        ..ConversationBuilder::default()
    };
    builder.push_spliced_records(&projection.entries, marks);
    let checkpoint = builder.checkpoint.take();
    let timestamp = session_time(
        builder.fold.latest_activity(),
        &projection.header.timestamp,
        dating,
    );

    let mut conversation = builder.finish(projection.source, path, timestamp, debug_level)?;
    conversation.is_missing_earlier_turns = projection.earlier_turns.is_missing();
    conversation.has_transient_read_error = projection.earlier_turns.is_transient_error();
    conversation.session_id = projection.header.id.clone();
    conversation.cwd = Some(projection.header.cwd.clone());
    conversation.project_path = Some(projection.header.cwd.clone());
    conversation.project_name = Some(super::format_short_name_from_path(&projection.header.cwd));
    Ok((conversation, checkpoint))
}

/// The parse errors of a `source` transcript's `malformed_lines`.
pub(crate) fn parse_errors_of(source: Source, malformed_lines: &[usize]) -> Vec<ParseError> {
    malformed_lines
        .iter()
        .map(|line_number| ParseError {
            line_number: *line_number,
            line_content: String::new(),
            error_message: format!("malformed {} JSONL record", source.label()),
            context_before: Vec::new(),
            context_after: Vec::new(),
        })
        .collect()
}

/// The parse errors of a `source` transcript's `malformed_lines`: each line
/// recorded with its text keeps it, its error and its context.
pub(crate) fn parse_errors_of_lines(
    source: Source,
    malformed_lines: &[super::MalformedLine],
) -> Vec<ParseError> {
    malformed_lines
        .iter()
        .map(|line| match &line.detail {
            Some(detail) => ParseError {
                line_number: line.line_number,
                line_content: detail.line_content.clone(),
                error_message: detail.error_message.clone(),
                context_before: detail.context_before.clone(),
                context_after: detail.context_after.clone(),
            },
            None => parse_errors_of(source, &[line.line_number]).remove(0),
        })
        .collect()
}

/// The time a session with a header is dated: its latest activity, else its
/// header's time, else `dating`'s.
pub(crate) fn session_time(
    latest_activity: Option<DateTime<chrono::FixedOffset>>,
    header_time: &str,
    dating: RowDating<'_>,
) -> DateTime<Local> {
    latest_activity
        .map(|timestamp| timestamp.with_timezone(&Local))
        .or_else(|| {
            DateTime::parse_from_rfc3339(header_time)
                .ok()
                .map(|timestamp| timestamp.with_timezone(&Local))
        })
        .unwrap_or_else(|| dating.fallback_time())
}

/// Accumulates one conversation as its log entries stream past in file order.
///
/// Separating accumulation from line reading lets a caller that already holds
/// parsed entries — every provider whose transcript is normalized before it
/// reaches here — feed them straight in, rather than re-serializing them to JSON
/// only to parse that JSON back.
#[derive(Default)]
struct ConversationBuilder {
    /// Set for a sub-agent transcript: the parent session reads each turn's
    /// text from the turn's [`SubagentMark`].
    keeps_only_text_outside_turns: bool,
    /// The row's facts besides its texts.
    fold: RowFold,
    full_text: MessageTextBuilder,
    agent_search_text: MessageTextBuilder,
    thinking_and_tool_call_text: MessageTextBuilder,
    semantic_turns: Vec<String>,
    semantic_turn_ranges: Vec<MessageRange>,
    parse_errors: Vec<ParseError>,
    assistant_id_semantic_indices: HashMap<usize, usize>,
    /// The step of the view's walk before which the fold is kept as
    /// `checkpoint`.
    checkpoint_at: Option<usize>,
    checkpoint: Option<FoldCheckpoint>,
    steps: usize,
}

/// The text one user turn contributes to the list preview, the search index
/// and the semantic turn.
struct UserTurnText {
    preview: String,
    /// The search text of each part, one entry per part.
    search_parts: Vec<String>,
    semantic: String,
    /// A slash command as `agent read` shows it, `/name args`.
    skill_preview: Option<String>,
}

pub(crate) fn search_parts_of_entry(entry: &LogEntry, spliced: bool) -> [Vec<String>; 3] {
    let mut full = Vec::new();
    let mut agent = Vec::new();
    let mut thinking = Vec::new();
    match entry {
        LogEntry::User { message, .. } => {
            full = user_turn_text(&message.content).search_parts;
            if let UserContent::Blocks(blocks) = &message.content {
                thinking = thinking_and_tool_call_parts(blocks);
            }
        }
        LogEntry::Assistant { message, .. } => {
            full = search_texts_of_blocks(&message.content);
            thinking = thinking_and_tool_call_parts(&message.content);
        }
        LogEntry::PiMetadata {
            label,
            text,
            searchable: true,
            ..
        } => full.push(labeled_metadata_text(label, text)),
        LogEntry::Progress { data, .. } => {
            if let Some(progress) = parse_agent_progress(data)
                && let Some(role) = AgentMessageRole::of_progress(&progress.message.message_type)
            {
                let AgentContent::Blocks(blocks) = progress.message.message.content;
                if spliced {
                    thinking = thinking_and_tool_call_parts(&blocks);
                    let parts = match role {
                        AgentMessageRole::User => {
                            user_turn_text(&UserContent::Blocks(blocks)).search_parts
                        }
                        AgentMessageRole::Assistant => search_texts_of_blocks(&blocks),
                    };
                    agent = parts
                        .into_iter()
                        .map(|part| sanitize_agent_text(&part))
                        .collect();
                } else {
                    agent = agent_search_parts_from_blocks(role, &blocks);
                }
            }
        }
        _ => {}
    }
    [full, agent, thinking]
}

pub(crate) fn semantic_text_of_entry(entry: &LogEntry) -> Option<String> {
    match entry {
        LogEntry::User { message, .. } => filter_turn(
            SemanticTurnRole::User,
            &user_turn_text(&message.content).semantic,
        ),
        LogEntry::Assistant { message, .. } => filter_turn(
            SemanticTurnRole::Assistant,
            &extract_text_from_assistant(message),
        ),
        LogEntry::PiMetadata { text, .. } => filter_turn(SemanticTurnRole::User, text),
        _ => None,
    }
}

/// A task report previews as its summary and is searched and embedded as its
/// summary and body, never as the notification's machine state; any other
/// message previews and embeds as its text and is searched with its tool
/// results. A slash command is searched as `agent read` shows it, so a phrase
/// spanning the name and its arguments matches in both.
fn user_turn_text(content: &UserContent) -> UserTurnText {
    user_turn_text_of(extract_text_from_user(content), || {
        search_texts_of_user(content)
    })
}

/// [`user_turn_text`] for a sub-agent turn's `blocks`.
fn user_blocks_text(blocks: &[ContentBlock]) -> UserTurnText {
    user_turn_text_of(extract_text_from_blocks(blocks), || {
        search_texts_of_blocks(blocks)
    })
}

/// [`user_turn_text`] for a turn whose text is `text`; `search_texts` gives
/// the search text of each of its parts.
fn user_turn_text_of(text: String, search_texts: impl FnOnce() -> Vec<String>) -> UserTurnText {
    let (preview, search_parts, semantic) = match parse_task_report(&text) {
        Some(report) => {
            let search = report.search_text();
            (report.summary, vec![search.clone()], search)
        }
        None => (text.clone(), search_texts(), text),
    };
    let skill_preview = extract_skill_preview(&preview);
    UserTurnText {
        search_parts: skill_preview
            .clone()
            .map_or(search_parts, |skill_preview| vec![skill_preview]),
        preview,
        semantic,
        skill_preview,
    }
}

impl ConversationBuilder {
    /// Push `entries`, the session's own records in file order, numbering
    /// each sub-agent turn in `marks` where the view splices it in. Each
    /// turn's text comes from its [`SubagentMark`].
    fn push_spliced<'a>(
        &mut self,
        entries: impl IntoIterator<Item = &'a LogEntry>,
        marks: &[SubagentMark],
    ) {
        self.push_steps(entries.into_iter().map(|entry| (None, entry)), marks);
    }

    /// [`Self::push_spliced`] for entries with their records: the session's
    /// own entries at line 0 are its leading entries, and each entry's
    /// thread is its record's.
    fn push_spliced_records<'a>(
        &mut self,
        entries: impl IntoIterator<Item = &'a (RecordLine, LogEntry)>,
        marks: &[SubagentMark],
    ) {
        self.push_steps(
            entries
                .into_iter()
                .map(|(record, entry)| (Some(record), entry)),
            marks,
        );
    }

    fn push_steps<'a>(
        &mut self,
        entries: impl IntoIterator<Item = (Option<&'a RecordLine>, &'a LogEntry)>,
        marks: &[SubagentMark],
    ) {
        walk_spliced(
            entries,
            |(_, entry)| entry.timestamp(),
            marks,
            |mark| &mark.timestamp,
            |step| {
                self.keep_checkpoint();
                match step {
                    Spliced::Parent((record, entry)) => self.push_record(record, entry),
                    Spliced::Child(mark) => self.push_mark(mark),
                }
                self.steps += 1;
            },
        );
        self.keep_checkpoint();
    }

    fn keep_checkpoint(&mut self) {
        if self.checkpoint_at == Some(self.steps) && self.checkpoint.is_none() {
            self.checkpoint = Some(self.fold.checkpoint());
        }
    }

    fn push_mark(&mut self, mark: &SubagentMark) {
        let ordinal = self.fold.place_child(
            mark.thread.clone(),
            mark.is_message,
            mark.reply_id.as_deref(),
            &mark.timestamp,
        );
        self.agent_search_text
            .push_parts(ordinal, mark.agent_search_parts.clone());
        self.thinking_and_tool_call_text
            .push_parts(ordinal, mark.thinking_and_tool_call_parts.clone());
    }

    /// Push `entry`, read at `record` when the caller knows it.
    fn push_record(&mut self, record: Option<&RecordLine>, entry: &LogEntry) {
        let transcript = record.and_then(|record| record.transcript.as_deref());
        let is_leading = record.is_some_and(|record| record.line == 0 && transcript.is_none());
        let folded = self
            .fold
            .place_parent(entry, ThreadIdentity::of(entry, transcript));
        let thread = folded.thread;
        let numbered = folded.numbered;
        let ordinal = numbered.map(|numbered| numbered.ordinal);
        match entry {
            LogEntry::User {
                message,
                cwd,
                usage,
                ..
            } => {
                if let Some(usage) = usage {
                    self.fold.add_anonymous_usage(usage);
                }
                if let Some(cwd) = cwd {
                    self.fold.add_cwd(cwd);
                }

                if let UserContent::Blocks(blocks) = &message.content {
                    self.push_turn_thinking_and_tool_calls(ordinal, blocks);
                }

                let UserTurnText {
                    preview: preview_text,
                    search_parts,
                    semantic: semantic_input,
                    skill_preview,
                } = user_turn_text(&message.content);

                if preview_text.is_empty() && search_parts.iter().all(String::is_empty) {
                    return;
                }

                if !preview_text.is_empty() {
                    self.fold.add_user_message(&preview_text);
                }

                self.push_turn_search_parts(ordinal, search_parts);
                let effective_preview = if let Some(skill_preview) = skill_preview {
                    skill_preview
                } else if !preview_text.is_empty() && is_clear_metadata_message(&preview_text) {
                    return;
                } else {
                    preview_text
                };

                let Some(numbered) = numbered else {
                    return;
                };
                if effective_preview.is_empty() {
                    return;
                }
                if let Some(turn) = filter_turn(SemanticTurnRole::User, &semantic_input) {
                    self.semantic_turns.push(turn);
                    self.semantic_turn_ranges
                        .push(MessageRange::single(numbered.ordinal));
                }
                self.fold.add_user_preview(effective_preview);
            }
            LogEntry::Assistant { message, .. } => {
                if let Some(model) = &message.model {
                    self.fold.add_first_model(model);
                }
                // Streaming can write one reply as several records: the last
                // usage of a reply id counts once.
                if let Some(usage) = &message.usage {
                    self.fold
                        .add_reply_usage(thread, message.id.as_deref(), usage);
                }

                let preview_text = extract_text_from_assistant(message);
                self.push_turn_search_parts(ordinal, search_texts_of_blocks(&message.content));
                self.push_turn_thinking_and_tool_calls(ordinal, &message.content);

                let Some(numbered) = numbered else {
                    return;
                };
                if numbered.starts_message {
                    self.fold.add_assistant_message();
                }
                if preview_text.is_empty() {
                    return;
                }
                let message_range = MessageRange::single(numbered.ordinal);
                match message.id.as_deref() {
                    Some(_) => {
                        self.append_reply_semantic_turn(
                            numbered.ordinal,
                            &preview_text,
                            message_range,
                        );
                        self.fold
                            .add_reply_preview(numbered.ordinal, preview_text, true);
                    }
                    None => {
                        if let Some(turn) = filter_turn(SemanticTurnRole::Assistant, &preview_text)
                        {
                            self.semantic_turns.push(turn);
                            self.semantic_turn_ranges.push(message_range);
                        }
                        self.fold
                            .add_reply_preview(numbered.ordinal, preview_text, false);
                    }
                }
            }
            LogEntry::Summary { summary } => self.fold.add_summary(summary),
            LogEntry::AiTitle { ai_title } => self.fold.set_summary(ai_title),
            LogEntry::CustomTitle { custom_title } => self.fold.set_title(custom_title, is_leading),
            LogEntry::PiMetadata {
                label,
                text,
                searchable,
                usage,
                ..
            } => {
                if let Some(usage) = usage {
                    self.fold.add_anonymous_usage(usage);
                }
                if label == "Model" && !text.is_empty() {
                    self.fold.set_model(text);
                }
                if *searchable {
                    let text = labeled_metadata_text(label, text);
                    self.fold.note_search_text(std::slice::from_ref(&text));
                    self.full_text.push(ordinal, text);
                }
                if let Some(numbered) = numbered
                    && let Some(turn) = filter_turn(SemanticTurnRole::User, text)
                {
                    self.semantic_turns.push(turn);
                    self.semantic_turn_ranges
                        .push(MessageRange::single(numbered.ordinal));
                }
            }
            LogEntry::Progress { data, extra } => {
                if let Some(progress) = parse_agent_progress(data)
                    && let Some(role) =
                        AgentMessageRole::of_progress(&progress.message.message_type)
                {
                    let message = progress.message.message;
                    let AgentContent::Blocks(blocks) = message.content;
                    let numbered = self.fold.number_progress(
                        role,
                        &blocks,
                        message.id.as_deref(),
                        thread,
                        progress_timestamp(extra).as_deref(),
                    );
                    self.agent_search_text.push_parts(
                        numbered.map(|numbered| numbered.ordinal),
                        agent_search_parts_from_blocks(role, &blocks),
                    );
                }
            }
            LogEntry::AgentName { .. } => {}
            LogEntry::System { .. } => {}
            _ => {}
        }
    }

    /// Skipped for a sub-agent transcript: the parent writes each turn's
    /// parts from its [`SubagentMark`].
    fn push_turn_search_parts(&mut self, ordinal: Option<usize>, parts: Vec<String>) {
        if !self.keeps_only_text_outside_turns {
            self.fold.note_search_text(&parts);
            self.full_text.push_parts(ordinal, parts);
        }
    }

    /// Skipped for a sub-agent transcript: the parent writes each turn's
    /// parts from its [`SubagentMark`].
    fn push_turn_thinking_and_tool_calls(
        &mut self,
        ordinal: Option<usize>,
        blocks: &[ContentBlock],
    ) {
        if !self.keeps_only_text_outside_turns {
            self.thinking_and_tool_call_text
                .push_parts(ordinal, thinking_and_tool_call_parts(blocks));
        }
    }

    /// Append one record's filtered `text` to the semantic turn of the reply
    /// at `ordinal`.
    fn append_reply_semantic_turn(&mut self, ordinal: usize, text: &str, range: MessageRange) {
        let Some(turn) = filter_turn(SemanticTurnRole::Assistant, text) else {
            return;
        };
        match self.assistant_id_semantic_indices.get(&ordinal).copied() {
            Some(index) => {
                let reply = &mut self.semantic_turns[index];
                reply.push(' ');
                reply.push_str(&turn);
            }
            None => {
                self.assistant_id_semantic_indices
                    .insert(ordinal, self.semantic_turns.len());
                self.semantic_turns.push(turn);
                self.semantic_turn_ranges.push(range);
            }
        }
    }

    /// The accumulated conversation, or the reason it holds nothing worth
    /// listing: a `/clear`-only session, or one with no previewable text.
    fn finish(
        self,
        source: Source,
        path: PathBuf,
        timestamp: DateTime<Local>,
        debug_level: Option<DebugLevel>,
    ) -> ParsedRow {
        let folded = folded_row(&self.fold, &path, debug_level)?;
        let message_count = folded.message_count;
        let custom_title = folded.custom_title.clone();
        let summary = folded.summary.clone();
        let (full_text, full_text_starts) = self.full_text.finish(
            title_and_summary(&custom_title, &summary),
            message_count,
            normalize_full_text,
        );
        let (agent_search_text, agent_search_text_starts) =
            self.agent_search_text
                .finish([], message_count, normalize_whitespace);
        let (thinking_and_tool_call_text, thinking_and_tool_call_text_starts) = self
            .thinking_and_tool_call_text
            .finish([], message_count, normalize_whitespace);

        // Pre-normalize search text to avoid re-normalizing on every startup
        let search_text_lower = normalize_for_search(&full_text);

        let (semantic_turns, semantic_turn_ranges): (Vec<_>, Vec<_>) = self
            .semantic_turns
            .into_iter()
            .zip(self.semantic_turn_ranges)
            .filter(|(turn, _)| !turn.is_empty())
            .unzip();

        let message_times = self.fold.message_times(None);

        Ok(Conversation {
            cached_session: None,
            source,
            subagents: Vec::new(),
            // Set by the caller, from the format's own rule for naming
            // sessions.
            session_id: String::new(),
            path,
            index: 0,
            timestamp,
            preview: folded.preview_first.clone(),
            preview_first: folded.preview_first,
            preview_last: folded.preview_last,
            full_text,
            agent_search_text,
            thinking_and_tool_call_text,
            semantic_turns,
            semantic_turn_ranges,
            search_text_lower,
            project_name: None,
            project_path: None,
            cwd: folded.cwd,
            message_count,
            message_times,
            message_text_starts: MessageTextStarts {
                full_text: full_text_starts,
                agent_search_text: agent_search_text_starts,
                thinking_and_tool_call_text: thinking_and_tool_call_text_starts,
            },
            assistant_messages: folded.assistant_messages,
            parse_errors: self.parse_errors,
            summary,
            custom_title,
            model: folded.model,
            total_tokens: folded.total_tokens,
            duration_minutes: folded.duration_minutes,
            fingerprint: None,
            has_transient_read_error: false,
            is_missing_earlier_turns: false,
        })
    }

    /// The accumulated sub-agent transcript as a [`SubagentContribution`],
    /// with or without a preview.
    fn subagent_contribution(self) -> SubagentContribution {
        let total_tokens = self.fold.total_tokens();
        let mut text_outside_turns = self.full_text.into_text(
            title_and_summary(
                &self.fold.custom_title().cloned(),
                &self.fold.summary().cloned(),
            ),
            normalize_full_text,
        );
        push_separated(
            &mut text_outside_turns,
            &self.agent_search_text.into_text([], normalize_whitespace),
        );
        SubagentContribution {
            text_outside_turns,
            total_tokens,
            ..SubagentContribution::default()
        }
    }
}

/// The row facts of `fold`, the fold of the session at `path`, or the
/// reason it holds nothing worth listing: a `/clear`-only session, or one
/// without previewable text.
pub(crate) fn folded_row(
    fold: &RowFold,
    path: &Path,
    debug_level: Option<DebugLevel>,
) -> std::result::Result<FoldedRow, UnlistedReason> {
    let filename = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("unknown");
    let unlisted = if fold.has_no_messages() {
        UnlistedReason::NoVisibleMessages
    } else {
        UnlistedReason::NoConversationMetadata
    };
    fold.finish().map_err(|reason| {
        let message = match reason {
            Unlisted::ClearOnly => format!("Filtered {filename}: clear-only conversation"),
            Unlisted::Empty {
                has_search_text,
                previews,
            } => format!(
                "Filtered {filename}: empty conversation (has_search_text={has_search_text}, preview_parts={previews})"
            ),
        };
        debug::debug(debug_level, &message);
        unlisted
    })
}

/// The text `full_text` opens with, outside every message.
fn title_and_summary<'a>(
    custom_title: &'a Option<String>,
    summary: &'a Option<String>,
) -> impl Iterator<Item = &'a str> {
    [custom_title.as_deref(), summary.as_deref()]
        .into_iter()
        .flatten()
}

/// The row for a transcript's `entries` in file order, for a format with no
/// session header, as Claude's, each sub-agent turn in `marks` numbered where
/// the view splices it in. `parse_errors` are the transcript's malformed
/// lines.
///
/// Date the row by its last timestamped message, else by `dating`. Claude
/// Code appends untimestamped bookkeeping records when it reopens a session;
/// they move the file's modification time past the last message.
#[cfg(test)]
pub(crate) fn conversation_from_entries<'a>(
    source: Source,
    path: PathBuf,
    entries: impl IntoIterator<Item = &'a LogEntry>,
    marks: &[SubagentMark],
    parse_errors: Vec<ParseError>,
    dating: RowDating<'_>,
    debug_level: Option<DebugLevel>,
) -> ParsedRow {
    let mut builder = ConversationBuilder {
        parse_errors,
        ..ConversationBuilder::default()
    };
    builder.push_spliced(entries, marks);
    let timestamp = builder
        .fold
        .last_activity()
        .map(|last| last.with_timezone(&Local))
        .unwrap_or_else(|| dating.fallback_time());
    builder.finish(source, path, timestamp, debug_level)
}

/// Detects metadata emitted by the /clear command wrapper messages and
/// other system-injected boilerplate that should not appear in previews.
pub(crate) fn is_clear_metadata_message(message: &str) -> bool {
    let trimmed = message.trim();

    trimmed.is_empty()
        || trimmed.starts_with(
            "Caveat: The messages below were generated by the user while running local commands.",
        )
        || trimmed.contains("<local-command-caveat>")
        || trimmed.contains("<command-name>/clear</command-name>")
        || trimmed.contains("<command-message>clear</command-message>")
        || (trimmed.contains("<command-name>") && !trimmed.contains("<command-name>/"))
        || trimmed.contains("<local-command-stdout>")
        || trimmed.contains("<local-command-stderr>")
        || super::skill_text::skill_directory(trimmed).is_some()
}

/// Claude Code opens a slash command's text with one of these tags. A prompt
/// that quotes a tag after other words is not a slash command.
const COMMAND_TAG_OPENERS: [&str; 2] = ["<command-name>", "<command-message>"];

/// Extract a clean preview from a skill invocation message (e.g. "/consult how to do X?").
/// Returns None if the message is not a skill invocation or is a /clear command.
pub(crate) fn extract_skill_preview(message: &str) -> Option<String> {
    let trimmed = message.trim();
    if !COMMAND_TAG_OPENERS
        .iter()
        .any(|opener| trimmed.starts_with(opener))
    {
        return None;
    }

    let start = trimmed.find("<command-name>")?;
    let end = trimmed.find("</command-name>")?;
    let content_start = start + "<command-name>".len();
    if content_start >= end {
        return None;
    }

    let command_name = &trimmed[content_start..end];
    if !command_name.starts_with('/') || command_name == "/clear" {
        return None;
    }

    // Extract command args if present
    if let Some(args_start) = trimmed.find("<command-args>")
        && let Some(args_end) = trimmed.find("</command-args>")
    {
        let args_content_start = args_start + "<command-args>".len();
        if args_content_start < args_end {
            let args = trimmed[args_content_start..args_end].trim();
            if !args.is_empty() {
                return Some(format!("{} {}", command_name, args));
            }
        }
    }

    Some(command_name.to_string())
}

/// True when `user_messages` hold only a `/clear` command's caveat, command
/// and output, as the row's fold reads them.
#[cfg(test)]
pub(crate) fn is_clear_only_conversation(user_messages: &[String]) -> bool {
    super::row_fold::is_clear_only(user_messages)
}

/// Normalize whitespace in a string
pub(crate) fn normalize_whitespace(s: &str) -> String {
    let mut normalized = String::with_capacity(s.len());
    for word in s.split_whitespace() {
        if !normalized.is_empty() {
            normalized.push(' ');
        }
        normalized.push_str(word);
    }
    normalized
}

/// `text` as `full_text` keeps it: terminal codes and control characters
/// removed, whitespace collapsed.
pub(crate) fn normalize_full_text(text: &str) -> String {
    normalize_whitespace(&without_terminal_codes(text))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::TestClock;
    use crate::history::format::claude;
    use std::io::Cursor;

    /// Helper to create a user message JSON line
    fn user_msg(text: &str, cwd: Option<&str>) -> String {
        let cwd_json = match cwd {
            Some(c) => format!(r#""cwd": "{}","#, c),
            None => String::new(),
        };
        format!(
            r#"{{"type": "user", "timestamp": "2024-01-01T00:00:00Z", {}  "message": {{"role": "user", "content": "{}"}}}}"#,
            cwd_json, text
        )
    }

    /// Helper to create an assistant message JSON line
    fn assistant_msg(text: &str) -> String {
        format!(
            r#"{{"type": "assistant", "timestamp": "2024-01-01T00:00:00Z", "message": {{"role": "assistant", "content": [{{"type": "text", "text": "{}"}}]}}}}"#,
            text
        )
    }

    /// Helper to create an assistant message with model and usage
    fn assistant_msg_with_usage(
        text: &str,
        model: &str,
        input: u64,
        output: u64,
        cache_creation: u64,
        cache_read: u64,
    ) -> String {
        format!(
            r#"{{"type": "assistant", "timestamp": "2024-01-01T00:00:00Z", "message": {{"role": "assistant", "model": "{}", "usage": {{"input_tokens": {}, "output_tokens": {}, "cache_creation_input_tokens": {}, "cache_read_input_tokens": {}}}, "content": [{{"type": "text", "text": "{}"}}]}}}}"#,
            model, input, output, cache_creation, cache_read, text
        )
    }

    /// Helper to parse JSONL content
    fn parse_jsonl(content: &str) -> Result<Option<Conversation>> {
        claude::conversation_from_reader(
            Path::new("test.jsonl"),
            Cursor::new(content),
            RowDating::new(None, &TestClock::default()),
            None,
        )
    }

    // === Warmup message filtering ===

    #[test]
    fn filters_warmup_messages_from_preview() {
        let content = [
            user_msg("Warmup", None),
            assistant_msg("Ready"),
            user_msg("Hello world", None),
            assistant_msg("Hi there"),
        ]
        .join("\n");

        let conv = parse_jsonl(&content).unwrap().unwrap();

        // Preview should NOT include the warmup exchange
        assert!(!conv.preview.contains("Warmup"));
        assert!(!conv.preview.contains("Ready"));
        assert!(conv.preview.contains("Hello world"));
        assert!(conv.preview.contains("Hi there"));

        // But full_text SHOULD include warmup content for searching
        assert!(conv.full_text.contains("Warmup"));
        assert!(conv.full_text.contains("Ready"));
    }

    #[test]
    fn warmup_only_conversation_excluded_from_preview_but_preserved() {
        // A conversation with only warmup should still be valid if it has content
        let content = [
            user_msg("Warmup", None),
            assistant_msg("Ready"),
            user_msg("Actual question", None),
        ]
        .join("\n");

        let conv = parse_jsonl(&content).unwrap().unwrap();
        assert!(!conv.preview.contains("Warmup"));
        assert!(conv.preview.contains("Actual question"));
    }

    // === Clear command filtering ===

    #[test]
    fn filters_clear_only_conversations() {
        let content = [
            user_msg(
                "Caveat: The messages below were generated by the user while running local commands.",
                None,
            ),
            user_msg("<command-name>/clear</command-name>", None),
            user_msg("<local-command-stdout></local-command-stdout>", None),
        ]
        .join("\n");

        let result = parse_jsonl(&content).unwrap();
        assert!(
            result.is_none(),
            "Clear-only conversation should be filtered"
        );
    }

    #[test]
    fn preserves_clear_command_in_mixed_conversation() {
        let content = [
            user_msg("Hello", None),
            assistant_msg("Hi"),
            user_msg(
                "Caveat: The messages below were generated by the user while running local commands.",
                None,
            ),
            user_msg("<command-name>/clear</command-name>", None),
            user_msg("Another question", None),
        ]
        .join("\n");

        let conv = parse_jsonl(&content).unwrap().unwrap();
        // The conversation should be preserved since it has real content
        assert!(conv.preview.contains("Hello"));
        assert!(conv.preview.contains("Another question"));
    }

    // === CWD extraction ===

    #[test]
    fn extracts_cwd_from_first_user_message() {
        let content = [
            user_msg("Hello", Some("/home/user/project")),
            assistant_msg("Hi"),
            user_msg("More", Some("/other/path")),
        ]
        .join("\n");

        let conv = parse_jsonl(&content).unwrap().unwrap();
        assert_eq!(
            conv.cwd,
            Some(PathBuf::from("/home/user/project")),
            "Should extract cwd from first user message"
        );
    }

    #[test]
    fn handles_missing_cwd() {
        let content = [user_msg("Hello", None), assistant_msg("Hi")].join("\n");

        let conv = parse_jsonl(&content).unwrap().unwrap();
        assert!(conv.cwd.is_none());
    }

    // === Empty conversation handling ===

    #[test]
    fn handles_empty_conversation() {
        let content = "";
        let result = parse_jsonl(content).unwrap();
        assert!(result.is_none(), "Empty conversation should return None");
    }

    #[test]
    fn handles_only_whitespace() {
        let content = "\n\n   \n\n";
        let result = parse_jsonl(content).unwrap();
        assert!(result.is_none());
    }

    // === Session timestamp ===

    fn parse_jsonl_modified_at(content: &str, modified: DateTime<Local>) -> Conversation {
        claude::conversation_from_reader(
            Path::new("test.jsonl"),
            Cursor::new(content),
            RowDating::new(Some(SystemTime::from(modified)), &TestClock::default()),
            None,
        )
        .unwrap()
        .unwrap()
    }

    fn local_time(rfc3339: &str) -> DateTime<Local> {
        DateTime::parse_from_rfc3339(rfc3339)
            .unwrap()
            .with_timezone(&Local)
    }

    #[test]
    fn session_is_dated_by_its_last_message_not_files_modification_time() {
        let content = [
            user_msg("Hello", None),
            r#"{"type": "assistant", "timestamp": "2026-09-02T19:55:19Z", "message": {"role": "assistant", "content": [{"type": "text", "text": "Done"}]}}"#.to_owned(),
            r#"{"type": "last-prompt", "lastPrompt": "Hello"}"#.to_owned(),
            r#"{"type": "cost-state"}"#.to_owned(),
        ]
        .join("\n");

        let conv = parse_jsonl_modified_at(&content, local_time("2026-09-06T22:49:44Z"));

        assert_eq!(conv.timestamp, local_time("2026-09-02T19:55:19Z"));
    }

    #[test]
    fn system_record_after_last_message_does_not_date_session() {
        let content = [
            user_msg("Hello", None),
            r#"{"type": "assistant", "timestamp": "2026-09-02T19:55:19Z", "message": {"role": "assistant", "content": [{"type": "text", "text": "Done"}]}}"#.to_owned(),
            r#"{"type": "system", "subtype": "turn_duration", "timestamp": "2026-09-02T19:58:22Z"}"#.to_owned(),
        ]
        .join("\n");

        let conv = parse_jsonl_modified_at(&content, local_time("2026-09-06T22:49:44Z"));

        assert_eq!(conv.timestamp, local_time("2026-09-02T19:55:19Z"));
    }

    #[test]
    fn session_without_timestamped_message_is_dated_by_files_modification_time() {
        let content = [
            r#"{"type": "user", "message": {"role": "user", "content": "Hello"}}"#,
            r#"{"type": "assistant", "message": {"role": "assistant", "content": [{"type": "text", "text": "Done"}]}}"#,
        ]
        .join("\n");
        let modified = local_time("2026-09-06T22:49:44Z");

        let conv = parse_jsonl_modified_at(&content, modified);

        assert_eq!(conv.timestamp, modified);
    }

    #[test]
    fn session_without_timestamped_message_or_modification_time_is_dated_now() {
        let content = [
            r#"{"type": "user", "message": {"role": "user", "content": "Hello"}}"#,
            r#"{"type": "assistant", "message": {"role": "assistant", "content": [{"type": "text", "text": "Done"}]}}"#,
        ]
        .join("\n");
        let clock = TestClock::default();

        let conv = claude::conversation_from_reader(
            Path::new("test.jsonl"),
            Cursor::new(content),
            RowDating::new(None, &clock),
            None,
        )
        .unwrap()
        .unwrap();

        assert_eq!(conv.timestamp, clock.wall());
    }

    // === Message counting ===

    #[test]
    fn counts_messages_correctly() {
        let content = [
            user_msg("First", None),
            assistant_msg("Response 1"),
            user_msg("Second", None),
            assistant_msg("Response 2"),
        ]
        .join("\n");

        let conv = parse_jsonl(&content).unwrap().unwrap();
        assert_eq!(conv.message_count, 4, "Should count 4 messages");
    }

    #[test]
    fn excludes_warmup_from_message_count() {
        let content = [
            user_msg("Warmup", None),
            assistant_msg("Ready"),
            user_msg("Real question", None),
            assistant_msg("Real answer"),
        ]
        .join("\n");

        let conv = parse_jsonl(&content).unwrap().unwrap();
        // Warmup and Ready should not be counted
        assert_eq!(
            conv.message_count, 2,
            "Should count 2 messages (excluding warmup)"
        );
    }

    // === Sub-agent transcripts ===

    #[test]
    fn subagent_transcript_with_transient_read_error_marks_its_session_for_another_read() {
        let content = [user_msg("Question", None), assistant_msg("Answer")].join("\n");
        let mut session = parse_jsonl(&content).unwrap().unwrap();
        let contribution = SubagentContribution {
            has_transient_read_error: true,
            ..SubagentContribution::default()
        };

        contribution.add_to(&mut session);

        assert!(session.has_transient_read_error);
    }

    #[test]
    fn subagent_transcript_missing_earlier_turns_marks_its_session_as_missing_them() {
        let content = [user_msg("Question", None), assistant_msg("Answer")].join("\n");
        let mut session = parse_jsonl(&content).unwrap().unwrap();
        let contribution = SubagentContribution {
            is_missing_earlier_turns: true,
            ..SubagentContribution::default()
        };

        contribution.add_to(&mut session);

        assert!(session.is_missing_earlier_turns);
    }

    // === Parse error handling ===

    #[test]
    fn captures_parse_errors_with_context() {
        let content = [
            user_msg("Line 1", None),
            "invalid json here".to_string(),
            user_msg("Line 3", None),
        ]
        .join("\n");

        let conv = parse_jsonl(&content).unwrap().unwrap();
        assert_eq!(conv.parse_errors.len(), 1);

        let error = &conv.parse_errors[0];
        assert_eq!(error.line_number, 2);
        assert!(error.line_content.contains("invalid json"));
        assert!(!error.error_message.is_empty());
        // Context before should have line 1
        assert_eq!(error.context_before.len(), 1);
        // Context after should have line 3
        assert_eq!(error.context_after.len(), 1);
    }

    #[test]
    fn parse_error_line_numbers_survive_skipped_metadata_message() {
        let content = [
            user_msg("<local-command-stdout>ok</local-command-stdout>", None),
            user_msg("Line 2", None),
            "invalid json here".to_string(),
            assistant_msg("Response"),
        ]
        .join("\n");

        let conv = parse_jsonl(&content).unwrap().unwrap();
        assert_eq!(conv.parse_errors.len(), 1);
        assert_eq!(
            conv.parse_errors[0].line_number, 3,
            "a message filtered out of the preview still occupies its line"
        );
    }

    #[test]
    fn parse_error_context_after_capped_at_two_lines() {
        let content = [
            user_msg("Before 1", None),
            user_msg("Before 2", None),
            "invalid json".to_string(),
            user_msg("After 1", None),
            user_msg("After 2", None),
            user_msg("After 3", None),
            assistant_msg("Response"),
        ]
        .join("\n");

        let conv = parse_jsonl(&content).unwrap().unwrap();
        assert_eq!(conv.parse_errors.len(), 1);

        let error = &conv.parse_errors[0];
        assert_eq!(error.line_number, 3);
        assert_eq!(
            error.context_before.len(),
            2,
            "context_before should have at most 2 lines"
        );
        assert_eq!(
            error.context_after.len(),
            2,
            "context_after should have at most 2 lines"
        );
    }

    // === Preview order ===

    #[test]
    fn both_preview_variants_computed() {
        let content = [
            user_msg("First", None),
            assistant_msg("Response 1"),
            user_msg("Second", None),
            assistant_msg("Response 2"),
            user_msg("Third", None),
            assistant_msg("Response 3"),
        ]
        .join("\n");

        let conv = parse_jsonl(&content).unwrap().unwrap();

        // preview_first should start with "First"
        assert!(
            conv.preview_first.starts_with("First"),
            "preview_first should start with First: {}",
            conv.preview_first
        );

        // preview_last should start with the last message (Response 3)
        assert!(
            conv.preview_last.starts_with("Response 3"),
            "preview_last should start with Response 3: {}",
            conv.preview_last
        );
    }

    // === Helper function tests ===

    #[test]
    fn is_clear_metadata_message_detects_patterns() {
        assert!(is_clear_metadata_message(""));
        assert!(is_clear_metadata_message("   "));
        assert!(is_clear_metadata_message(
            "Caveat: The messages below were generated by the user while running local commands."
        ));
        assert!(is_clear_metadata_message(
            "<local-command-caveat>something</local-command-caveat>"
        ));
        assert!(is_clear_metadata_message(
            "<command-name>/clear</command-name>"
        ));
        assert!(is_clear_metadata_message(
            "<command-message>clear</command-message>"
        ));
        assert!(is_clear_metadata_message(
            "<local-command-stdout>output</local-command-stdout>"
        ));
        // <command-args> alone should NOT match - it appears in all skill invocations
        assert!(!is_clear_metadata_message(
            "<command-args>foo</command-args>"
        ));

        assert!(is_clear_metadata_message(
            "Base directory for this skill: /Users/raine/.claude/skills/consult\n\nConsult an external LLM."
        ));

        // Should NOT match normal messages
        assert!(!is_clear_metadata_message("Hello world"));
        assert!(!is_clear_metadata_message("What is the meaning of life?"));

        // Skill invocation with command-name should NOT be filtered as clear metadata
        assert!(!is_clear_metadata_message(
            "<command-message>consult</command-message>\n<command-name>/consult</command-name>\n<command-args>how to do X?</command-args>"
        ));
    }

    #[test]
    fn extract_skill_preview_extracts_command_with_args() {
        assert_eq!(
            extract_skill_preview(
                "<command-message>consult</command-message>\n<command-name>/consult</command-name>\n<command-args>how to do X?</command-args>"
            ),
            Some("/consult how to do X?".to_string())
        );
    }

    #[test]
    fn extract_skill_preview_extracts_command_without_args() {
        assert_eq!(
            extract_skill_preview("<command-name>/help</command-name>"),
            Some("/help".to_string())
        );
    }

    #[test]
    fn extract_skill_preview_skips_clear() {
        assert_eq!(
            extract_skill_preview(
                "<command-name>/clear</command-name>\n<command-message>clear</command-message>\n<command-args></command-args>"
            ),
            None
        );
    }

    #[test]
    fn extract_skill_preview_returns_none_for_normal_text() {
        assert_eq!(extract_skill_preview("Hello world"), None);
    }

    #[test]
    fn prompt_quoting_command_tag_after_other_words_isnt_slash_command() {
        assert_eq!(
            extract_skill_preview(
                "why does <command-name>/model</command-name> show twice in the list?"
            ),
            None
        );
    }

    #[test]
    fn skill_invocation_conversation_not_filtered() {
        // A conversation that starts with /clear then has a skill invocation
        // should NOT be filtered out
        let content = [
            user_msg(
                "Caveat: The messages below were generated by the user while running local commands.",
                None,
            ),
            user_msg(
                "<command-name>/clear</command-name> <command-message>clear</command-message> <command-args></command-args>",
                None,
            ),
            user_msg("<local-command-stdout></local-command-stdout>", None),
            user_msg(
                "<command-message>consult</command-message> <command-name>/consult</command-name> <command-args>how to implement sidebar?</command-args>",
                None,
            ),
            assistant_msg("Here's how to implement it..."),
        ]
        .join("\n");

        let conv = parse_jsonl(&content).unwrap();
        assert!(
            conv.is_some(),
            "Conversation with skill invocation should not be filtered"
        );
        let conv = conv.unwrap();
        assert!(
            conv.preview.contains("/consult"),
            "Preview should contain the skill command: {}",
            conv.preview
        );
    }

    #[test]
    fn normalize_whitespace_collapses_runs() {
        assert_eq!(normalize_whitespace("hello  world"), "hello world");
        assert_eq!(normalize_whitespace("  hello   world  "), "hello world");
        assert_eq!(normalize_whitespace("a\n\n\nb"), "a b");
        assert_eq!(
            normalize_whitespace("\t\thello\t\tworld\t\t"),
            "hello world"
        );
        assert_eq!(normalize_whitespace(""), "");
    }

    #[test]
    fn is_clear_only_conversation_requires_all_three_markers() {
        // Empty is not clear-only
        assert!(!is_clear_only_conversation(&[]));

        // Just caveat is not enough
        assert!(!is_clear_only_conversation(&[
            "Caveat: The messages below were generated by the user while running local commands."
                .to_string()
        ]));

        // Caveat + command but no stdout
        assert!(!is_clear_only_conversation(&[
            "Caveat: The messages below were generated by the user while running local commands."
                .to_string(),
            "<command-name>/clear</command-name>".to_string(),
        ]));

        // All three = clear-only
        assert!(is_clear_only_conversation(&[
            "Caveat: The messages below were generated by the user while running local commands."
                .to_string(),
            "<command-name>/clear</command-name>".to_string(),
            "<local-command-stdout></local-command-stdout>".to_string(),
        ]));

        // Any substantive message disqualifies
        assert!(!is_clear_only_conversation(&[
            "Caveat: The messages below were generated by the user while running local commands."
                .to_string(),
            "<command-name>/clear</command-name>".to_string(),
            "<local-command-stdout></local-command-stdout>".to_string(),
            "Hello world".to_string(),
        ]));
    }

    // === Summary extraction ===

    #[test]
    fn extracts_summary_from_jsonl() {
        let content = [
            r#"{"type": "summary", "summary": "Test conversation summary", "leafUuid": "abc123"}"#
                .to_string(),
            user_msg("Hello", None),
            assistant_msg("Hi there"),
        ]
        .join("\n");

        let conv = parse_jsonl(&content).unwrap().unwrap();
        assert_eq!(
            conv.summary,
            Some("Test conversation summary".to_string()),
            "Should extract summary from summary entry"
        );
    }

    #[test]
    fn summary_included_in_full_text() {
        let content = [
            r#"{"type": "summary", "summary": "Important topic discussion", "leafUuid": "abc123"}"#
                .to_string(),
            user_msg("Hello", None),
            assistant_msg("Hi there"),
        ]
        .join("\n");

        let conv = parse_jsonl(&content).unwrap().unwrap();
        assert!(
            conv.full_text.contains("Important topic discussion"),
            "Summary should be included in full_text for searching"
        );
    }

    #[test]
    fn handles_conversation_without_summary() {
        let content = [user_msg("Hello", None), assistant_msg("Hi there")].join("\n");

        let conv = parse_jsonl(&content).unwrap().unwrap();
        assert!(conv.summary.is_none(), "Should have no summary");
    }

    #[test]
    fn takes_first_summary_if_multiple() {
        let content = [
            r#"{"type": "summary", "summary": "First summary", "leafUuid": "abc"}"#.to_string(),
            user_msg("Hello", None),
            r#"{"type": "summary", "summary": "Second summary", "leafUuid": "def"}"#.to_string(),
            assistant_msg("Hi there"),
        ]
        .join("\n");

        let conv = parse_jsonl(&content).unwrap().unwrap();
        assert_eq!(
            conv.summary,
            Some("First summary".to_string()),
            "Should keep first summary encountered"
        );
    }

    #[test]
    fn extracts_ai_title_as_summary() {
        let content = [
            r#"{"type":"ai-title","aiTitle":"Plan activities","sessionId":"abc"}"#.to_string(),
            user_msg("Hello", None),
            assistant_msg("Hi there"),
        ]
        .join("\n");

        let conv = parse_jsonl(&content).unwrap().unwrap();
        assert_eq!(conv.summary, Some("Plan activities".to_string()));
    }

    #[test]
    fn takes_last_ai_title_if_multiple() {
        let content = [
            r#"{"type":"ai-title","aiTitle":"First title","sessionId":"abc"}"#.to_string(),
            user_msg("Hello", None),
            assistant_msg("Hi there"),
            r#"{"type":"ai-title","aiTitle":"Final title","sessionId":"abc"}"#.to_string(),
        ]
        .join("\n");

        let conv = parse_jsonl(&content).unwrap().unwrap();
        assert_eq!(conv.summary, Some("Final title".to_string()));
    }

    #[test]
    fn ignores_empty_ai_title() {
        let content = [
            r#"{"type":"ai-title","aiTitle":"First title","sessionId":"abc"}"#.to_string(),
            user_msg("Hello", None),
            r#"{"type":"ai-title","aiTitle":"","sessionId":"abc"}"#.to_string(),
            assistant_msg("Hi there"),
        ]
        .join("\n");

        let conv = parse_jsonl(&content).unwrap().unwrap();
        assert_eq!(conv.summary, Some("First title".to_string()));
    }

    #[test]
    fn ai_title_included_in_full_text() {
        let content = [
            r#"{"type":"ai-title","aiTitle":"ai-title-search-sentinel","sessionId":"abc"}"#
                .to_string(),
            user_msg("Hello", None),
            assistant_msg("Hi there"),
        ]
        .join("\n");

        let conv = parse_jsonl(&content).unwrap().unwrap();
        assert!(conv.full_text.contains("ai-title-search-sentinel"));
    }

    // === Model and token extraction ===

    #[test]
    fn extracts_model_from_assistant_message() {
        let content = [
            user_msg("Hello", None),
            assistant_msg_with_usage("Hi there", "claude-opus-4-5-20251101", 100, 50, 0, 0),
        ]
        .join("\n");

        let conv = parse_jsonl(&content).unwrap().unwrap();
        assert_eq!(
            conv.model,
            Some("claude-opus-4-5-20251101".to_string()),
            "Should extract model from assistant message"
        );
    }

    #[test]
    fn accumulates_tokens_across_messages() {
        let content = [
            user_msg("Hello", None),
            assistant_msg_with_usage("Hi", "claude-opus-4-5-20251101", 100, 50, 10, 5),
            user_msg("How are you?", None),
            assistant_msg_with_usage("Good!", "claude-opus-4-5-20251101", 200, 100, 20, 10),
        ]
        .join("\n");

        let conv = parse_jsonl(&content).unwrap().unwrap();
        // Total = (100+50+10+5) + (200+100+20+10) = 495 (all token types)
        assert_eq!(
            conv.total_tokens, 495,
            "Should accumulate all token types from all assistant messages"
        );
    }

    #[test]
    fn takes_first_model_if_multiple() {
        let content = [
            user_msg("Hello", None),
            assistant_msg_with_usage("Hi", "claude-opus-4-5-20251101", 100, 50, 0, 0),
            user_msg("Follow up", None),
            assistant_msg_with_usage("Response", "claude-sonnet-4-20250514", 200, 100, 0, 0),
        ]
        .join("\n");

        let conv = parse_jsonl(&content).unwrap().unwrap();
        assert_eq!(
            conv.model,
            Some("claude-opus-4-5-20251101".to_string()),
            "Should keep first model encountered"
        );
    }

    #[test]
    fn handles_missing_model_and_usage() {
        let content = [user_msg("Hello", None), assistant_msg("Hi there")].join("\n");

        let conv = parse_jsonl(&content).unwrap().unwrap();
        assert!(conv.model.is_none(), "Should have no model");
        assert_eq!(conv.total_tokens, 0, "Should have zero tokens");
    }

    // === Custom title extraction ===

    #[test]
    fn extracts_custom_title_from_jsonl() {
        let content = [
            r#"{"type": "custom-title", "customTitle": "my session", "sessionId": "abc123"}"#
                .to_string(),
            user_msg("Hello", None),
            assistant_msg("Hi there"),
        ]
        .join("\n");

        let conv = parse_jsonl(&content).unwrap().unwrap();
        assert_eq!(
            conv.custom_title,
            Some("my session".to_string()),
            "Should extract custom title"
        );
    }

    #[test]
    fn takes_last_custom_title_if_multiple() {
        let content = [
            r#"{"type": "custom-title", "customTitle": "first name", "sessionId": "abc"}"#
                .to_string(),
            user_msg("Hello", None),
            assistant_msg("Hi there"),
            r#"{"type": "custom-title", "customTitle": "renamed", "sessionId": "abc"}"#.to_string(),
        ]
        .join("\n");

        let conv = parse_jsonl(&content).unwrap().unwrap();
        assert_eq!(
            conv.custom_title,
            Some("renamed".to_string()),
            "Should keep last custom title (user renamed)"
        );
    }

    #[test]
    fn custom_title_included_in_full_text() {
        let content = [
            r#"{"type": "custom-title", "customTitle": "unique-session-name", "sessionId": "abc"}"#
                .to_string(),
            user_msg("Hello", None),
            assistant_msg("Hi there"),
        ]
        .join("\n");

        let conv = parse_jsonl(&content).unwrap().unwrap();
        assert!(
            conv.full_text.contains("unique-session-name"),
            "Custom title should be included in full_text for searching"
        );
    }

    #[test]
    fn ignores_empty_custom_title() {
        let content = [
            r#"{"type": "custom-title", "customTitle": "", "sessionId": "abc"}"#.to_string(),
            user_msg("Hello", None),
            assistant_msg("Hi there"),
        ]
        .join("\n");

        let conv = parse_jsonl(&content).unwrap().unwrap();
        assert!(
            conv.custom_title.is_none(),
            "Empty custom title should be treated as None"
        );
    }

    #[test]
    fn empty_custom_title_clears_previous() {
        let content = [
            r#"{"type": "custom-title", "customTitle": "initial name", "sessionId": "abc"}"#
                .to_string(),
            user_msg("Hello", None),
            assistant_msg("Hi there"),
            r#"{"type": "custom-title", "customTitle": "", "sessionId": "abc"}"#.to_string(),
        ]
        .join("\n");

        let conv = parse_jsonl(&content).unwrap().unwrap();
        assert!(
            conv.custom_title.is_none(),
            "Empty custom title should clear previous title"
        );
    }

    #[test]
    fn handles_conversation_without_custom_title() {
        let content = [user_msg("Hello", None), assistant_msg("Hi there")].join("\n");

        let conv = parse_jsonl(&content).unwrap().unwrap();
        assert!(conv.custom_title.is_none(), "Should have no custom title");
    }

    #[test]
    fn custom_title_takes_precedence_over_ai_title() {
        let content = [
            r#"{"type":"ai-title","aiTitle":"auto title","sessionId":"abc"}"#.to_string(),
            user_msg("Hello", None),
            assistant_msg("Hi there"),
            r#"{"type":"custom-title","customTitle":"manual title","sessionId":"abc"}"#.to_string(),
        ]
        .join("\n");

        let conv = parse_jsonl(&content).unwrap().unwrap();
        assert_eq!(conv.custom_title, Some("manual title".to_string()));
        assert_eq!(conv.summary, Some("auto title".to_string()));
    }

    #[test]
    fn parses_agent_name_metadata() {
        let content = [
            user_msg("Hello", None),
            assistant_msg("Hi there"),
            r#"{"type":"custom-title","customTitle":"renamed","sessionId":"abc"}"#.to_string(),
            r#"{"type":"agent-name","agentName":"renamed","sessionId":"abc"}"#.to_string(),
        ]
        .join("\n");

        let conv = parse_jsonl(&content).unwrap().unwrap();
        assert_eq!(conv.custom_title, Some("renamed".to_string()));
        assert!(conv.parse_errors.is_empty());
    }

    // === ToolResult search indexing ===

    /// Helper to create a user message with tool result (string content)
    fn user_msg_with_tool_result(text: &str, tool_output: &str) -> String {
        format!(
            r#"{{"type": "user", "timestamp": "2024-01-01T00:00:00Z", "message": {{"role": "user", "content": [{{"type": "text", "text": "{}"}}, {{"type": "tool_result", "tool_use_id": "toolu_123", "content": "{}"}}]}}}}"#,
            text, tool_output
        )
    }

    /// Helper to create a user message with tool result (array-of-blocks content)
    fn user_msg_with_tool_result_blocks(text: &str, tool_output: &str) -> String {
        format!(
            r#"{{"type": "user", "timestamp": "2024-01-01T00:00:00Z", "message": {{"role": "user", "content": [{{"type": "text", "text": "{}"}}, {{"type": "tool_result", "tool_use_id": "toolu_123", "content": [{{"type": "text", "text": "{}"}}]}}]}}}}"#,
            text, tool_output
        )
    }

    #[test]
    fn tool_result_string_included_in_full_text() {
        let content = [
            user_msg_with_tool_result("run this", "command output here"),
            assistant_msg("Done"),
        ]
        .join("\n");

        let conv = parse_jsonl(&content).unwrap().unwrap();
        assert!(
            conv.full_text.contains("command output here"),
            "Tool result string should be in full_text for search: {}",
            conv.full_text
        );
    }

    #[test]
    fn tool_result_array_included_in_full_text() {
        let content = [
            user_msg_with_tool_result_blocks("check file", "file contents xyz"),
            assistant_msg("Got it"),
        ]
        .join("\n");

        let conv = parse_jsonl(&content).unwrap().unwrap();
        assert!(
            conv.full_text.contains("file contents xyz"),
            "Tool result array blocks should be in full_text: {}",
            conv.full_text
        );
    }

    #[test]
    fn tool_result_not_in_preview() {
        let content = [
            user_msg_with_tool_result(
                "run this",
                "verbose tool output should not appear in preview",
            ),
            assistant_msg("Done"),
        ]
        .join("\n");

        let conv = parse_jsonl(&content).unwrap().unwrap();
        assert!(
            !conv.preview.contains("verbose tool output"),
            "Tool result should NOT be in preview: {}",
            conv.preview
        );
        assert!(
            conv.preview.contains("run this"),
            "Text blocks should still be in preview: {}",
            conv.preview
        );
    }

    #[test]
    fn tool_result_not_in_semantic_turns() {
        let content = [
            user_msg_with_tool_result(
                "run this",
                "verbose tool output should not be embedded semantically",
            ),
            assistant_msg("Done"),
        ]
        .join("\n");

        let conv = parse_jsonl(&content).unwrap().unwrap();
        assert!(
            conv.full_text.contains("verbose tool output"),
            "Tool result should remain in lexical full_text: {}",
            conv.full_text
        );
        assert!(
            conv.search_text_lower.contains("verbose tool output"),
            "Tool result should remain in lexical search_text_lower: {}",
            conv.search_text_lower
        );
        assert!(
            !conv
                .semantic_turns
                .join(" ")
                .contains("verbose tool output"),
            "Tool result should not be in semantic_turns: {:?}",
            conv.semantic_turns
        );
        assert_eq!(conv.semantic_turns, vec!["run this", "Done"]);
    }

    #[test]
    fn long_normal_dialogue_middle_text_remains_searchable() {
        let middle = "middlenormaldialogueuniqueneedle";
        let long_text = format!(
            "HEAD {} {middle} {} TAIL",
            "a".repeat(20_000),
            "b".repeat(20_000)
        );
        let content = [
            user_msg("question", None),
            serde_json::json!({
                "type": "assistant",
                "timestamp": "2024-01-01T00:00:00Z",
                "message": {
                    "role": "assistant",
                    "content": [{"type": "text", "text": long_text}]
                }
            })
            .to_string(),
        ]
        .join("\n");

        let conv = parse_jsonl(&content).unwrap().unwrap();

        assert!(conv.full_text.contains(middle));
        assert!(conv.search_text_lower.contains(middle));
    }

    #[test]
    fn semantic_turns_exclude_parser_metadata_while_lexical_text_keeps_search_payloads() {
        let content = [
            r#"{"type":"summary","summary":"summary lexical sentinel","leafUuid":"abc"}"#
                .to_string(),
            r#"{"type":"custom-title","customTitle":"title lexical sentinel","sessionId":"abc"}"#
                .to_string(),
            user_msg(
                "visible user semantic sentinel",
                Some("/cwd/private-sentinel"),
            ),
            serde_json::json!({
                "type": "assistant",
                "timestamp": "2024-01-01T00:00:00Z",
                "message": {
                    "role": "assistant",
                    "content": [
                        {"type": "text", "text": "visible mixed assistant semantic sentinel"},
                        {
                            "type": "tool_use",
                            "id": "toolu_1",
                            "name": "Bash",
                            "input": {"command": "tool call private sentinel"}
                        }
                    ]
                }
            })
            .to_string(),
            user_msg_with_tool_result(
                "after tool visible semantic",
                "tool result lexical sentinel",
            ),
            user_msg("<command-name>ls</command-name>", None),
            user_msg(
                "<local-command-stdout>local stdout lexical sentinel</local-command-stdout>",
                None,
            ),
            assistant_msg("visible assistant semantic sentinel"),
        ]
        .join("\n");

        let conv = parse_jsonl(&content).unwrap().unwrap();
        let semantic = conv.semantic_turns.join(" ");

        assert_eq!(
            conv.semantic_turns,
            vec![
                "visible user semantic sentinel",
                "visible mixed assistant semantic sentinel",
                "after tool visible semantic",
                "visible assistant semantic sentinel"
            ]
        );
        for excluded in [
            "summary lexical sentinel",
            "title lexical sentinel",
            "tool call private sentinel",
            "tool result lexical sentinel",
            "local stdout lexical sentinel",
            "/cwd/private-sentinel",
        ] {
            assert!(
                !semantic.contains(excluded),
                "{excluded} leaked into {semantic:?}"
            );
        }
        for included in [
            "summary lexical sentinel",
            "title lexical sentinel",
            "tool result lexical sentinel",
            "local stdout lexical sentinel",
        ] {
            assert!(
                conv.full_text.contains(included),
                "{included} missing from {}",
                conv.full_text
            );
            assert!(
                conv.search_text_lower.contains(included),
                "{included} missing from {}",
                conv.search_text_lower
            );
        }
        assert!(!conv.full_text.contains("/cwd/private-sentinel"));
        assert_eq!(conv.project_name, None);
        assert_eq!(conv.project_path, None);
    }

    #[test]
    fn local_command_stdout_remains_lexical_only() {
        let content = [
            user_msg("Real question", None),
            assistant_msg("Real answer"),
            user_msg(
                "Caveat: The messages below were generated by the user while running local commands.",
                None,
            ),
            user_msg("<command-name>ls</command-name>", None),
            user_msg(
                "<local-command-stdout>local stdout payload</local-command-stdout>",
                None,
            ),
            user_msg("Follow up", None),
            assistant_msg("Follow up answer"),
        ]
        .join("\n");

        let conv = parse_jsonl(&content).unwrap().unwrap();
        assert!(conv.full_text.contains("local stdout payload"));
        assert!(conv.search_text_lower.contains("local stdout payload"));
        assert_eq!(
            conv.semantic_turns,
            vec![
                "Real question",
                "Real answer",
                "Follow up",
                "Follow up answer"
            ]
        );
    }

    #[test]
    fn command_wrappers_are_not_embedded_semantically() {
        let content = [
            user_msg("Find conversations about semantic search", None),
            assistant_msg("Relevant answer"),
            user_msg(
                "<command-message>goal</command-message> <command-name>/goal</command-name> <command-args>improve semantic input</command-args>",
                None,
            ),
            user_msg("After command real question", None),
            assistant_msg("After command real answer"),
        ]
        .join("\n");

        let conv = parse_jsonl(&content).unwrap().unwrap();
        let semantic = conv.semantic_turns.join(" ");

        assert!(conv.full_text.contains("improve semantic input"));
        assert!(!semantic.contains("<command-message>"));
        assert!(!semantic.contains("/goal"));
        assert_eq!(
            conv.semantic_turns,
            vec![
                "Find conversations about semantic search",
                "Relevant answer",
                "improve semantic input",
                "After command real question",
                "After command real answer"
            ]
        );
    }

    #[test]
    fn workflow_status_narration_remains_semantic_text() {
        let content = [
            user_msg("Implement semantic cache", None),
            assistant_msg("I’ll run cargo test and just check before committing."),
            assistant_msg("Validation passed and I committed the phase."),
            assistant_msg("Semantic cache stores visible dialogue embeddings."),
        ]
        .join("\n");

        let conv = parse_jsonl(&content).unwrap().unwrap();

        assert!(conv.full_text.contains("cargo test"));
        assert!(conv.full_text.contains("Validation passed"));
        assert_eq!(
            conv.semantic_turns,
            vec![
                "Implement semantic cache",
                "I’ll run cargo test and just check before committing.",
                "Validation passed and I committed the phase.",
                "Semantic cache stores visible dialogue embeddings."
            ]
        );
    }

    #[test]
    fn markdown_code_fences_remain_lexical_only() {
        let content = [
            user_msg("Explain semantic cache", None),
            serde_json::json!({
                "type": "assistant",
                "timestamp": "2024-01-01T00:00:00Z",
                "message": {
                    "role": "assistant",
                    "content": [{
                        "type": "text",
                        "text": "Semantic cache stores dialogue.\n```rust\nlet secret = tool_output();\n```\nUse generated embeddings."
                    }]
                }
            })
            .to_string(),
        ]
        .join("\n");

        let conv = parse_jsonl(&content).unwrap().unwrap();
        let semantic = conv.semantic_turns.join(" ");

        assert!(conv.full_text.contains("let secret"));
        assert!(semantic.contains("Semantic cache stores dialogue."));
        assert!(semantic.contains("Use generated embeddings."));
        assert!(!semantic.contains("let secret"));
        assert!(!semantic.contains("```"));
    }

    #[test]
    fn inline_markdown_code_fences_remain_lexical_only() {
        let content = [
            user_msg("Design the preview pane", None),
            serde_json::json!({
                "type": "assistant",
                "timestamp": "2024-01-01T00:00:00Z",
                "message": {
                    "role": "assistant",
                    "content": [{
                        "type": "text",
                        "text": "The idea is a responsive detail pane. ```text ╭─ Search ─╮ │ pasted UI mockup │ ╰──────────╯ ``` It should help users recognize conversations."
                    }]
                }
            })
            .to_string(),
        ]
        .join("\n");

        let conv = parse_jsonl(&content).unwrap().unwrap();
        let semantic = conv.semantic_turns.join(" ");

        assert!(conv.full_text.contains("pasted UI mockup"));
        assert!(semantic.contains("responsive detail pane"));
        assert!(semantic.contains("recognize conversations"));
        assert!(!semantic.contains("pasted UI mockup"));
        assert!(!semantic.contains("╭─ Search"));
        assert!(!semantic.contains("```"));
    }

    #[test]
    fn assistant_id_registers_tool_only_chunk_before_text_chunk() {
        let content = [
            user_msg("question", None),
            serde_json::json!({
                "type": "assistant",
                "timestamp": "2024-01-01T00:00:00Z",
                "message": {"id": "msg_1", "role": "assistant", "content": [{"type": "tool_use", "id": "toolu_1", "name": "Bash", "input": {"command": "pwd"}}]}
            })
            .to_string(),
            serde_json::json!({
                "type": "assistant",
                "timestamp": "2024-01-01T00:00:01Z",
                "message": {"id": "msg_1", "role": "assistant", "content": [{"type": "text", "text": "final"}]}
            })
            .to_string(),
            user_msg("next", None),
        ]
        .join("\n");

        let conv = parse_jsonl(&content).unwrap().unwrap();

        assert_eq!(conv.semantic_turns, vec!["question", "final", "next"]);
        assert_eq!(
            conv.semantic_turn_ranges,
            vec![
                MessageRange::single(1),
                MessageRange::single(2),
                MessageRange::single(3),
            ]
        );
    }

    /// One assistant record of the reply `id`, holding `block`.
    fn reply_record(id: &str, block: serde_json::Value) -> String {
        serde_json::json!({
            "type": "assistant",
            "timestamp": "2024-01-01T00:00:00Z",
            "message": {"id": id, "role": "assistant", "content": [block]}
        })
        .to_string()
    }

    fn text_block(text: &str) -> serde_json::Value {
        serde_json::json!({"type": "text", "text": text})
    }

    #[test]
    fn reply_split_across_records_keeps_all_its_text_in_its_turn_and_preview() {
        let content = [
            user_msg("question", None),
            reply_record("msg_1", text_block("Checking the cache.")),
            reply_record(
                "msg_1",
                serde_json::json!({"type": "tool_use", "id": "toolu_1", "name": "Bash", "input": {"command": "pwd"}}),
            ),
            reply_record("msg_1", text_block("Then the index.")),
            user_msg("next", None),
        ]
        .join("\n");

        let conv = parse_jsonl(&content).unwrap().unwrap();

        assert_eq!(
            conv.semantic_turns,
            vec!["question", "Checking the cache. Then the index.", "next"]
        );
        assert_eq!(
            conv.semantic_turn_ranges,
            vec![
                MessageRange::single(1),
                MessageRange::single(2),
                MessageRange::single(3),
            ]
        );
        assert_eq!(
            conv.preview_first,
            "question ... Checking the cache. Then the index. ... next"
        );
    }

    #[test]
    fn interleaved_reply_records_keep_each_replys_message_range() {
        let content = [
            user_msg("question", None),
            reply_record("msg_1", text_block("first")),
            reply_record("msg_2", text_block("second")),
            reply_record(
                "msg_1",
                text_block("<system-reminder>hidden</system-reminder>"),
            ),
            reply_record("msg_2", text_block("more")),
        ]
        .join("\n");

        let conv = parse_jsonl(&content).unwrap().unwrap();

        assert_eq!(
            conv.semantic_turns,
            vec!["question", "first", "second more"]
        );
        assert_eq!(
            conv.semantic_turn_ranges,
            vec![
                MessageRange::single(1),
                MessageRange::single(2),
                MessageRange::single(3),
            ]
        );
    }

    #[test]
    fn split_replys_records_sit_together_in_search_text() {
        let content = [
            user_msg("question", None),
            reply_record("msg_1", text_block("first half")),
            reply_record("msg_2", text_block("other reply")),
            reply_record("msg_1", text_block("second half")),
        ]
        .join("\n");

        let conv = parse_jsonl(&content).unwrap().unwrap();
        let starts = &conv.message_text_starts.full_text;

        assert_eq!(
            conv.full_text,
            "question \u{1e} first half \u{1e} second half \u{1e} other reply"
        );
        assert_eq!(
            starts.text_of(&conv.full_text, &[MessageRange::single(2)]),
            " \u{1e} first half \u{1e} second half"
        );
        assert_eq!(
            starts.text_of(&conv.full_text, &[MessageRange::single(3)]),
            " \u{1e} other reply"
        );
    }

    #[test]
    fn session_list_doesnt_match_quoted_text_across_two_parts_of_message() {
        let two_blocks = serde_json::json!({
            "type": "assistant",
            "timestamp": "2024-01-01T00:00:00Z",
            "message": {
                "id": "msg_1",
                "role": "assistant",
                "content": [text_block("make"), text_block("clean builds")],
            }
        })
        .to_string();
        let split_reply = [
            user_msg("question", None),
            reply_record("msg_1", text_block("first half")),
            reply_record("msg_2", text_block("other reply")),
            reply_record("msg_1", text_block("second half")),
        ];
        let sessions = [
            [user_msg("question", None), two_blocks].join("\n"),
            split_reply.join("\n"),
        ]
        .map(|content| parse_jsonl(&content).unwrap().unwrap());
        let searchable = crate::search::precompute_search_text(&sessions);
        let list_search =
            |query| crate::search::search(&sessions, &searchable, query, Local::now());

        assert!(list_search("\"make clean\"").is_empty());
        assert!(list_search("\"half second\"").is_empty());
        assert_eq!(list_search("\"clean builds\""), [0]);
        assert_eq!(list_search("\"second half\""), [1]);
    }

    #[test]
    fn quoted_text_across_two_messages_doesnt_match_search_text() {
        let content = [user_msg("restart the", None), assistant_msg("service now")].join("\n");

        let conv = parse_jsonl(&content).unwrap().unwrap();
        let literal = |text: &str| crate::search::literal::Literal::new(text.to_owned());

        assert!(!literal("the service").matches(&conv.full_text));
        assert!(literal("service now").matches(&conv.full_text));
    }

    #[test]
    fn title_opens_search_text_before_every_message() {
        let content = [
            r#"{"type":"custom-title","customTitle":"Named session"}"#.to_owned(),
            user_msg("question", None),
            assistant_msg("answer"),
        ]
        .join("\n");

        let conv = parse_jsonl(&content).unwrap().unwrap();
        let starts = &conv.message_text_starts.full_text;

        assert_eq!(
            conv.full_text,
            "Named session \u{1e} question \u{1e} answer"
        );
        assert_eq!(
            starts.text_of(
                &conv.full_text,
                &[MessageRange {
                    start: 1,
                    end: conv.message_count
                }]
            ),
            " \u{1e} question \u{1e} answer"
        );
    }

    #[test]
    fn unclosed_code_fence_in_one_record_doesnt_drop_replys_later_records() {
        let content = [
            user_msg("question", None),
            reply_record("msg_1", text_block("Plan:\n```rust\nlet x = 1;")),
            reply_record("msg_1", text_block("Then rebuild the index.")),
        ]
        .join("\n");

        let conv = parse_jsonl(&content).unwrap().unwrap();

        assert_eq!(
            conv.semantic_turns,
            vec!["question", "Plan: Then rebuild the index."]
        );
    }

    /// A reply holding the thinking block `thinking`, a `WebFetch` call and
    /// the text `answer`.
    fn reply_with_thinking_and_a_tool_call(thinking: &str) -> String {
        [
            user_msg("question", None),
            serde_json::json!({
                "type": "assistant",
                "timestamp": "2024-01-01T00:00:00Z",
                "message": {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": thinking, "signature": "sig"},
                    {"type": "tool_use", "id": "toolu_1", "name": "WebFetch", "input": {"url": "https://example.com"}},
                    text_block("answer"),
                ]}
            })
            .to_string(),
        ]
        .join("\n")
    }

    #[test]
    fn thinking_and_tool_names_stay_out_of_semantic_route_text() {
        let content = reply_with_thinking_and_a_tool_call("weighing the stale index");

        let route_text = parse_jsonl(&content)
            .unwrap()
            .unwrap()
            .semantic_route_text();

        assert!(!route_text.contains("weighing"), "{route_text}");
        assert!(!route_text.contains("WebFetch"), "{route_text}");
    }

    #[test]
    fn thinking_block_longer_than_search_segment_is_cached_whole() {
        let thinking = format!(
            "weighing {} the stale index",
            "x".repeat(crate::agent::transcript::MAX_AGENT_SEGMENT_CHARS)
        );
        let content = reply_with_thinking_and_a_tool_call(&thinking);

        let conv = parse_jsonl(&content).unwrap().unwrap();

        assert!(
            conv.thinking_and_tool_call_text.contains("the stale index"),
            "the end of a long thinking block is missing from the cached text"
        );
    }

    #[test]
    fn thinking_and_tool_calls_reach_agent_text_but_not_full_text() {
        let content = reply_with_thinking_and_a_tool_call("weighing the stale index");

        let conv = parse_jsonl(&content).unwrap().unwrap();

        assert!(
            conv.thinking_and_tool_call_text
                .contains("weighing the stale index"),
            "{}",
            conv.thinking_and_tool_call_text
        );
        assert!(
            conv.thinking_and_tool_call_text
                .contains("WebFetch: https://example.com"),
            "{}",
            conv.thinking_and_tool_call_text
        );
        assert!(!conv.full_text.contains("weighing"), "{}", conv.full_text);
        assert!(!conv.full_text.contains("WebFetch"), "{}", conv.full_text);
    }

    #[test]
    fn phrase_split_by_terminal_code_is_found_in_full_text() {
        let content = [
            user_msg_with_tool_result("run it", "error\\u001b[0m: disk full"),
            assistant_msg("Done"),
        ]
        .join("\n");

        let conv = parse_jsonl(&content).unwrap().unwrap();

        assert!(
            conv.full_text.contains("error: disk full"),
            "{}",
            conv.full_text
        );
    }

    #[test]
    fn searchable_metadata_entry_is_searched_with_its_label() {
        let content = [
            user_msg("question", None),
            serde_json::json!({
                "type": "pi-metadata",
                "label": "extension notice",
                "text": "watch the build",
                "searchable": true
            })
            .to_string(),
        ]
        .join("\n");

        let conv = parse_jsonl(&content).unwrap().unwrap();

        assert!(
            conv.full_text
                .contains("[extension notice] watch the build"),
            "{}",
            conv.full_text
        );
    }

    #[test]
    fn semantic_turn_ranges_track_visible_dialogue_only() {
        let content = [
            user_msg("first visible", None),
            user_msg_with_tool_result("second visible", "tool output only literal"),
            assistant_msg("third visible"),
        ]
        .join("\n");

        let conv = parse_jsonl(&content).unwrap().unwrap();

        assert_eq!(
            conv.semantic_turns,
            vec!["first visible", "second visible", "third visible"]
        );
        assert_eq!(
            conv.semantic_turn_ranges,
            vec![
                MessageRange::single(1),
                MessageRange::single(2),
                MessageRange::single(3),
            ]
        );
        assert!(!conv.semantic_turns.join(" ").contains("tool output"));
    }

    #[test]
    fn semantic_turn_ranges_include_agent_progress_ordinals() {
        let content = [
            user_msg("first visible", None),
            r#"{"type":"progress","data":{"type":"agent_progress","agentId":"agent-abcdef","message":{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"subagent hidden text"}]}}}}"#.to_string(),
            assistant_msg("final assistant text"),
        ]
        .join("\n");

        let conv = parse_jsonl(&content).unwrap().unwrap();

        assert_eq!(
            conv.semantic_turns,
            vec!["first visible", "final assistant text"]
        );
        assert_eq!(
            conv.semantic_turn_ranges,
            vec![MessageRange::single(1), MessageRange::single(3)]
        );
        assert!(conv.agent_search_text.contains("subagent hidden text"));
        assert!(!conv.full_text.contains("subagent hidden text"));
    }

    #[test]
    fn semantic_turn_ranges_skip_assistant_image_only_ordinals() {
        let content = [
            user_msg("first visible", None),
            serde_json::json!({
                "type": "assistant",
                "timestamp": "2024-01-01T00:00:00Z",
                "message": {"role": "assistant", "content": [{"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "abc"}}]}
            })
            .to_string(),
            assistant_msg("final assistant text"),
        ]
        .join("\n");

        let conv = parse_jsonl(&content).unwrap().unwrap();

        assert_eq!(
            conv.semantic_turns,
            vec!["first visible", "final assistant text"]
        );
        assert_eq!(
            conv.semantic_turn_ranges,
            vec![MessageRange::single(1), MessageRange::single(2)]
        );
    }

    #[test]
    fn semantic_turn_ranges_use_canonical_agent_ordinals() {
        let content = [
            user_msg("first visible", None),
            serde_json::json!({
                "type": "assistant",
                "timestamp": "2024-01-01T00:00:00Z",
                "message": {
                    "role": "assistant",
                    "content": [{"type": "tool_use", "id": "toolu_1", "name": "Bash", "input": {"command": "pwd"}}]
                }
            })
            .to_string(),
            serde_json::json!({
                "type": "user",
                "timestamp": "2024-01-01T00:00:00Z",
                "message": {
                    "role": "user",
                    "content": [{"type": "tool_result", "tool_use_id": "toolu_1", "content": "tool output only"}]
                }
            })
            .to_string(),
            assistant_msg("final assistant text"),
        ]
        .join("\n");

        let conv = parse_jsonl(&content).unwrap().unwrap();

        assert_eq!(
            conv.semantic_turns,
            vec!["first visible", "final assistant text"]
        );
        assert_eq!(
            conv.semantic_turn_ranges,
            vec![MessageRange::single(1), MessageRange::single(4)]
        );
    }

    #[test]
    fn clear_conversation_still_filtered_with_tool_results() {
        let content = [
            user_msg(
                "Caveat: The messages below were generated by the user while running local commands.",
                None,
            ),
            user_msg("<command-name>/clear</command-name>", None),
            user_msg("<local-command-stdout></local-command-stdout>", None),
        ]
        .join("\n");

        let result = parse_jsonl(&content).unwrap();
        assert!(
            result.is_none(),
            "Clear-only conversation should still be filtered"
        );
    }

    #[test]
    fn task_report_previews_as_its_summary_and_indexes_its_body() {
        use super::super::task_notification::test_support::*;
        let notification = serde_json::json!({
            "type": "user",
            "timestamp": "2024-01-01T00:00:02Z",
            "message": {"role": "user", "content": AGENT_REPORT}
        })
        .to_string();
        let content = [
            user_msg("Verify the claims", None),
            assistant_msg("Four passes are running."),
            notification,
        ]
        .join("\n");

        let conv = parse_jsonl(&content).unwrap().unwrap();

        assert!(
            conv.preview_last.starts_with(AGENT_SUMMARY),
            "{}",
            conv.preview_last
        );
        assert!(!conv.preview_last.contains(AGENT_REPORT_LAST_LINE));
        assert!(conv.full_text.contains(AGENT_REPORT_LAST_LINE));
        let last_turn = conv.semantic_turns.last().unwrap();
        assert!(last_turn.contains("Verified against source"), "{last_turn}");
        for wrapper_field in ["task-notification", "task-id", "output-file", "completed"] {
            assert!(
                !conv.full_text.contains(wrapper_field),
                "{wrapper_field} in {}",
                conv.full_text
            );
            assert!(!last_turn.contains(wrapper_field), "{last_turn}");
        }
        assert_eq!(conv.message_count, 3);
    }

    /// Sub-agent transcripts parse in batches of the pool's width. With more
    /// of them than one batch holds, each transcript's text joins the row in
    /// the stub's order, and the routing text covers it.
    #[test]
    fn more_sub_agents_than_one_batch_merge_in_stubs_order() {
        let directory = tempfile::tempdir().unwrap();
        let write = |name: &str, text: &str| {
            let path = directory.path().join(name);
            std::fs::write(
                &path,
                [user_msg(text, None), assistant_msg("ok")].join("\n"),
            )
            .unwrap();
            path
        };
        let locator = write("session.jsonl", "parent request");
        let threads = rayon::current_num_threads() * 2 + 1;
        let subagents: Vec<PathBuf> = (0..threads)
            .map(|index| {
                write(
                    &format!("agent-{index:03}.jsonl"),
                    &format!("thread{index:03}"),
                )
            })
            .collect();
        let stub = SessionStub {
            locator,
            subagents: subagents.clone(),
            cache_key: "session.jsonl".to_owned(),
            fingerprint: crate::history::provider::Fingerprint {
                size: 0,
                modified: None,
            },
        };

        let format = &claude::CLAUDE_TRANSCRIPT;
        let projection = format
            .parse_transcript(crate::history::FOR_FIXTURES, &stub.locator)
            .unwrap();

        let session = process_projected_session(
            crate::history::FOR_FIXTURES,
            &stub,
            projection,
            format,
            None,
            &|| {},
            &TestClock::default(),
        )
        .unwrap()
        .unwrap()
        .row;

        let positions: Vec<usize> = (0..threads)
            .map(|index| {
                session
                    .agent_search_text
                    .find(&format!("thread{index:03}"))
                    .unwrap_or_else(|| panic!("thread{index:03} missing"))
            })
            .collect();
        assert!(positions.windows(2).all(|pair| pair[0] < pair[1]));
        assert_eq!(session.subagents, subagents);
        let route_text = session.semantic_route_text();
        assert!(route_text.contains("thread"), "{route_text}");
    }

    #[test]
    fn sub_agents_thinking_and_tool_calls_reach_sessions_agent_text() {
        let directory = tempfile::tempdir().unwrap();
        let locator = directory.path().join("session.jsonl");
        std::fs::write(
            &locator,
            [user_msg("parent request", None), assistant_msg("ok")].join("\n"),
        )
        .unwrap();
        let subagent = directory.path().join("agent-1.jsonl");
        std::fs::write(
            &subagent,
            [
                user_msg("sub-agent task", None),
                serde_json::json!({
                    "type": "assistant",
                    "timestamp": "2024-01-01T00:00:00Z",
                    "message": {"role": "assistant", "content": [
                        {"type": "thinking", "thinking": "sub-agent deliberation", "signature": "sig"},
                        {"type": "tool_use", "id": "toolu_1", "name": "Grep", "input": {"pattern": "x"}},
                    ]}
                })
                .to_string(),
            ]
            .join("\n"),
        )
        .unwrap();
        let stub = SessionStub {
            locator,
            subagents: vec![subagent],
            cache_key: "session.jsonl".to_owned(),
            fingerprint: crate::history::provider::Fingerprint {
                size: 0,
                modified: None,
            },
        };
        let session = claude::session_row(
            crate::history::FOR_FIXTURES,
            &stub,
            None,
            &|| {},
            &TestClock::default(),
        )
        .unwrap()
        .unwrap()
        .row;

        assert!(
            session
                .thinking_and_tool_call_text
                .contains("sub-agent deliberation"),
            "{}",
            session.thinking_and_tool_call_text
        );
        assert!(
            session
                .thinking_and_tool_call_text
                .contains(r#"Grep: "x" in ."#),
            "{}",
            session.thinking_and_tool_call_text
        );
    }
}
