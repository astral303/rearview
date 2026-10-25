//! The Codex rollout format: JSONL lines of `{timestamp, ordinal?, type, payload}`.
//!
//! A rollout records far more than the dialogue — model context snapshots,
//! security scores, rate-limit telemetry — so this reader is a projection by
//! design: `response_item` lines carry the conversation, `token_count` events
//! carry usage, `turn_context` names the model, and everything else is skipped
//! without being an error. Unknown line types are expected; Codex adds them
//! freely between versions.
//!
//! A thread can span several rollouts. Editing an earlier prompt, or forking a
//! paginated thread, starts a new rollout; its header's `history_base` names
//! the earlier rollout it continues. The reader reads the earlier rollouts
//! first.

mod tools;

use super::dropped::DroppedRecords;
use super::lines::visit_parsed_lines;
use super::splice::SubagentThread;
use super::{
    EarlierTurns, RecordLine, SessionFormat, SessionHeader, SessionProjection, append_exit_code,
    block_texts, trim_blank_lines,
};
use crate::agent::sanitize::sanitize_agent_text;
use crate::agent::transcript::tool_result_text;
use crate::error::{AppError, Result};
use crate::history::provider::ReadError;
use crate::history::provider::subagents::SubagentForest;
use crate::history::task_notification::ReportSender;
use crate::history::{Source, TaskReport, TranscriptEntries};
use crate::log_entry::{
    AssistantMessage, ContentBlock, DELIVERED_REPORT_KEY, LogEntry, NOTICE_LABEL, SubagentIdentity,
    TokenUsage, Tool, UserContent, UserMessage,
};
use crate::parallel::ParallelThreshold;
use chrono::{DateTime, FixedOffset};
use serde_json::{Map, Value, json};
use std::collections::hash_map::Entry;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::ffi::OsStr;
use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub struct CodexRolloutFormat;

pub static CODEX_ROLLOUT: CodexRolloutFormat = CodexRolloutFormat;

impl SessionFormat for CodexRolloutFormat {
    /// Reads the whole thread, however large. The biggest sessions are the
    /// most valuable to search, so nothing is truncated: the reader streams one
    /// line at a time, and what stays resident is the extracted text, not the
    /// raw file.
    fn parse_transcript_counting(
        &self,
        path: &Path,
        dropped: DroppedRecords,
    ) -> Result<Option<SessionProjection>> {
        Ok(read_thread(path, dropped)?.map(|thread| thread.projection))
    }

    /// Leaves out each discarded turn's sub-agents and the threads beneath
    /// them.
    fn session_entries(
        &self,
        path: &Path,
        subagents: &[PathBuf],
    ) -> Result<Option<TranscriptEntries>> {
        let (thread, mut threads) = rayon::join(
            || read_thread(path, DroppedRecords::default()),
            || super::subagent_threads(self, subagents),
        );
        let Some(thread) = thread? else {
            return Ok(None);
        };
        let discarded = thread.discarded_turn_subagents.transcripts_among(subagents);
        threads.retain(|subagent| !discarded.paths.contains(&*subagent.transcript));
        link_subagent_threads(&thread.projection.entries, &mut threads);
        let view = super::spliced(thread.projection, threads);
        Ok(Some(TranscriptEntries::of_projection(view)))
    }
}

/// Each sub-agent thread linked to the `spawn_agent` call that started it,
/// and with its copy of each report it handed back folded into a delivered
/// `final_answer` call: the report shows where the agent that launched it
/// received it. `session` holds the session's own entries; a nested
/// sub-agent's launch and reports are in its parent thread's.
pub(crate) fn link_subagent_threads(
    session: &[(RecordLine, LogEntry)],
    threads: &mut [SubagentThread],
) {
    let entries = || {
        session
            .iter()
            .chain(threads.iter().flat_map(|thread| &thread.entries))
            .map(|(_, entry)| entry)
    };
    let spawns = SpawnedAgents::of(entries());
    let reports = HandedBackReports::of(entries());
    let reports_by_thread = reports.by_thread(threads);
    for (thread, reports) in threads.iter_mut().zip(reports_by_thread) {
        thread.launch_call = spawns.launch_call(
            &thread.label,
            thread.identity.agent_path.as_deref(),
            instant_of(&thread.started),
        );
        fold_delivered_final_answers(&mut thread.entries, &reports);
    }
}

/// The `spawn_agent` calls of a session and its threads, by the thread each
/// result names: its agent path in `task_name`, or its thread id in
/// `agent_id`.
#[derive(Default)]
struct SpawnedAgents {
    /// Each call naming the key, in order.
    by_key: HashMap<String, Vec<SpawnCall>>,
}

/// A `spawn_agent` call: its id, and the time it was made.
struct SpawnCall {
    id: String,
    made: Option<DateTime<FixedOffset>>,
}

impl SpawnedAgents {
    fn of<'a>(entries: impl Iterator<Item = &'a LogEntry> + Clone) -> Self {
        let mut spawn_calls: HashMap<&str, Option<DateTime<FixedOffset>>> = HashMap::new();
        for entry in entries.clone() {
            if let LogEntry::Assistant {
                message, timestamp, ..
            } = entry
            {
                for block in &message.content {
                    if let ContentBlock::ToolUse { id, name, .. } = block
                        && name == SPAWN_AGENT
                    {
                        spawn_calls.insert(id, timestamp.as_deref().and_then(instant_of));
                    }
                }
            }
        }
        let mut spawned = Self::default();
        for entry in entries {
            let LogEntry::User {
                message:
                    UserMessage {
                        content: UserContent::Blocks(blocks),
                        ..
                    },
                ..
            } = entry
            else {
                continue;
            };
            for block in blocks {
                let ContentBlock::ToolResult {
                    tool_use_id,
                    content: Some(Value::String(output)),
                    ..
                } = block
                else {
                    continue;
                };
                let Some(made) = spawn_calls.get(tool_use_id.as_str()) else {
                    continue;
                };
                let Ok(result) = serde_json::from_str::<Value>(output) else {
                    continue;
                };
                for key in ["task_name", "agent_id"] {
                    if let Some(named) = result.get(key).and_then(Value::as_str) {
                        spawned
                            .by_key
                            .entry(named.to_owned())
                            .or_default()
                            .push(SpawnCall {
                                id: tool_use_id.clone(),
                                made: *made,
                            });
                    }
                }
            }
        }
        spawned
    }

    /// The call that started the thread `thread_id` at `agent_path`: of the
    /// calls naming it, the last made before the thread `started`, or else
    /// the first. A path names a new sub-agent each time it is spawned again.
    fn launch_call(
        &self,
        thread_id: &str,
        agent_path: Option<&str>,
        started: Option<DateTime<FixedOffset>>,
    ) -> Option<String> {
        let calls = self
            .by_key
            .get(thread_id)
            .or_else(|| self.by_key.get(agent_path?))?;
        calls
            .iter()
            .rev()
            .find(|call| {
                call.made
                    .zip(started)
                    .is_some_and(|(made, started)| made <= started)
            })
            .or_else(|| calls.first())
            .map(|call| call.id.clone())
    }
}

struct HandedBackReports(Vec<HandedBackReport>);

/// A report a sub-agent handed back: the agent path or agent id naming its
/// sender, its body, and when the agent that launched the sub-agent received
/// it.
struct HandedBackReport {
    sender: String,
    body: String,
    received: Option<DateTime<FixedOffset>>,
}

impl HandedBackReports {
    fn of<'a>(entries: impl Iterator<Item = &'a LogEntry>) -> Self {
        let mut reports = Vec::new();
        for entry in entries {
            let LogEntry::User {
                message, timestamp, ..
            } = entry
            else {
                continue;
            };
            let Some(report) = crate::history::user_task_report(&message.content) else {
                continue;
            };
            if let (Some(sender), Some(body)) = (report.sender.task_id, report.body) {
                reports.push(HandedBackReport {
                    sender,
                    body,
                    received: timestamp.as_deref().and_then(instant_of),
                });
            }
        }
        Self(reports)
    }

    /// The reports each of `threads` handed back, in the order of `threads`.
    fn by_thread(&self, threads: &[SubagentThread]) -> Vec<Vec<&HandedBackReport>> {
        let mut reports = vec![Vec::new(); threads.len()];
        for report in &self.0 {
            if let Some(thread) = report.sending_thread(threads) {
                reports[thread].push(report);
            }
        }
        reports
    }
}

impl HandedBackReport {
    /// The index of the thread that handed the report back: of the threads
    /// its sender names, the one started last before the report was
    /// received. A reused agent path names several. Without an RFC 3339
    /// time, the report answers its sender only when the sender names one
    /// thread.
    fn sending_thread(&self, threads: &[SubagentThread]) -> Option<usize> {
        let named = threads.iter().enumerate().filter(|(_, thread)| {
            let identity = &thread.identity;
            [&identity.agent_path, &identity.agent_id]
                .into_iter()
                .any(|name| name.as_deref() == Some(self.sender.as_str()))
        });
        match self.received {
            Some(received) => named
                .filter_map(|(index, thread)| Some((index, instant_of(&thread.started)?)))
                .filter(|&(_, started)| started <= received)
                .max_by_key(|&(_, started)| started)
                .map(|(index, _)| index),
            None => {
                let mut named = named.map(|(index, _)| index);
                let first = named.next();
                if named.next().is_none() { first } else { None }
            }
        }
    }

    /// True when `entry` is a reply holding the report's body alone, written
    /// at or before the report was received. A reply or a report without an
    /// RFC 3339 time counts as written before.
    fn is_delivered_by(&self, entry: &LogEntry) -> bool {
        let LogEntry::Assistant {
            message, timestamp, ..
        } = entry
        else {
            return false;
        };
        let written = timestamp.as_deref().and_then(instant_of);
        let is_written_before = match (written, self.received) {
            (Some(written), Some(received)) => written <= received,
            _ => true,
        };
        let texts = message
            .content
            .iter()
            .map(|block| match block {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Option<Vec<_>>>();
        is_written_before && texts.is_some_and(|texts| texts.join("\n").trim() == self.body.trim())
    }
}

/// Fold the reply delivering each of `reports` into a delivered
/// `final_answer` call, as the Claude reader folds a delivered
/// `SubagentHandback` call: the parent shows the report. Each report folds
/// one reply, the last [`HandedBackReport::is_delivered_by`] accepts; an
/// earlier copy of the body, or one written after the report, stays.
fn fold_delivered_final_answers(
    entries: &mut [(RecordLine, LogEntry)],
    reports: &[&HandedBackReport],
) {
    let delivering = reports
        .iter()
        .filter_map(|report| {
            entries
                .iter()
                .rposition(|(_, entry)| report.is_delivered_by(entry))
        })
        .collect::<BTreeSet<_>>();
    for index in delivering {
        let (record, entry) = &mut entries[index];
        if let LogEntry::Assistant { message, .. } = entry {
            message.content = vec![ContentBlock::ToolUse {
                id: format!("{FINAL_ANSWER_CALL}@{}", record.line),
                name: FINAL_ANSWER_CALL.to_owned(),
                tool: Tool::AgentReport,
                input: json!({ DELIVERED_REPORT_KEY: true }),
            }];
        }
    }
}

/// The name of the call a sub-agent's delivered final answer folds into.
const FINAL_ANSWER_CALL: &str = "final_answer";

/// The thread ending in the rollout at `path`, titled from the session index.
/// `None` when the file is not a rollout.
pub(crate) fn read_thread(path: &Path, dropped: DroppedRecords) -> Result<Option<ThreadRead>> {
    let Some(mut thread) = parse_thread(path, ParallelThreshold::DEFAULT, dropped)? else {
        return Ok(None);
    };
    let projection = &mut thread.projection;
    if let Some(title) = thread_title(path, &projection.header.id) {
        // Line 0: the title lives in the session index, not the rollout.
        projection.entries.insert(
            0,
            (
                RecordLine::from(0),
                LogEntry::CustomTitle {
                    custom_title: title.clone(),
                },
            ),
        );
        projection.title = Some(title);
    }
    Ok(Some(thread))
}

/// A thread as read: its projection, and each discarded turn's sub-agents.
pub(crate) struct ThreadRead {
    pub(crate) projection: SessionProjection,
    pub(crate) discarded_turn_subagents: DiscardedTurnSubagents,
}

/// The session-level facts of the first `session_meta` line.
///
/// Only the first one: a sub-agent rollout carries the parent's `session_meta`
/// again further down, as inherited model context. `id` is the thread id — the
/// `session_id` field holds the *parent's* id in a sub-agent file, so it never
/// identifies the file it appears in.
struct RolloutHeader {
    id: String,
    timestamp: String,
    cwd: String,
    kind: ThreadKind,
    own_history_start: Option<u64>,
    identity: SubagentIdentity,
    cli_version: Option<String>,
    /// The earlier rollout this one continues. Set when an edited prompt or a
    /// paginated fork started this rollout.
    history_base: Option<HistoryBase>,
}

#[derive(Clone, Debug)]
struct HistoryBase {
    /// The earlier rollout's id: the `_<rollout_id>` part of its file name, or
    /// the thread id for a thread's first rollout. Codex names the field
    /// `thread_id`.
    rollout_id: String,
    /// The byte just past the last record this rollout inherits from it.
    end_byte_offset: u64,
}

/// The thread a rollout's header records: a session, a sub-agent, or one
/// Codex ran for itself.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ThreadKind {
    /// A thread whose header names no parent.
    Session,
    /// A thread whose header names a parent: read through the session at
    /// the far end of its chain, or listed as a session when that parent has
    /// no rollout.
    Subagent { parent_thread_id: String },
    /// A Guardian review, compaction, memory consolidation or internal
    /// session: a thread Codex ran for itself and keeps off its own list.
    /// Not listed or read; deleted with the parent it names.
    Skipped { parent_thread_id: Option<String> },
}

impl ThreadKind {
    pub(crate) fn parent_thread_id(&self) -> Option<&str> {
        match self {
            Self::Session => None,
            Self::Subagent { parent_thread_id } => Some(parent_thread_id),
            Self::Skipped { parent_thread_id } => parent_thread_id.as_deref(),
        }
    }

    pub(crate) fn is_skipped(&self) -> bool {
        matches!(self, Self::Skipped { .. })
    }
}

fn rollout_header(value: &Value) -> Option<RolloutHeader> {
    let object = value.as_object()?;
    if object.get("type").and_then(Value::as_str) != Some("session_meta") {
        return None;
    }
    let payload = object.get("payload")?.as_object()?;
    let field = |name: &str| payload.get(name).and_then(Value::as_str).map(str::to_owned);
    Some(RolloutHeader {
        id: field("id")?,
        timestamp: field("timestamp")?,
        cwd: field("cwd")?,
        kind: thread_kind(payload),
        own_history_start: payload
            .get("subagent_history_start_ordinal")
            .and_then(Value::as_u64),
        identity: SubagentIdentity {
            nickname: field("agent_nickname"),
            role: field("agent_role"),
            agent_path: field("agent_path"),
            agent_id: None,
        },
        cli_version: field("cli_version"),
        history_base: payload.get("history_base").and_then(history_base),
    })
}

fn history_base(value: &Value) -> Option<HistoryBase> {
    Some(HistoryBase {
        rollout_id: value.get("thread_id")?.as_str()?.to_owned(),
        end_byte_offset: value.get("end_byte_offset")?.as_u64()?,
    })
}

fn thread_kind(payload: &Map<String, Value>) -> ThreadKind {
    let parent_thread_id = payload
        .get("parent_thread_id")
        .and_then(Value::as_str)
        .map(str::to_owned);
    thread_kind_of_source(payload.get("source"), parent_thread_id)
}

/// Classify a thread by its `source` — the `SessionSource` JSON a rollout
/// header carries and the `threads.source` column of Codex's state database
/// stores — and the parent recorded for it. `"cli"` and the other
/// interactive sources are strings, `{"subagent": …}` names a spawn variant,
/// `{"internal": …}` a session Codex ran for itself. Of the spawn variants
/// only `thread_spawn` is a thread the user's session started — a bare
/// `"thread_spawn"` in rollouts written before 0.150, an object carrying the
/// parent and depth since; `review`, `compact`, `memory_consolidation` and
/// `other` (`"guardian"` in the corpus) are Codex's own. With no `source`
/// (older rollouts) or a string one (`cli`), the parent alone decides, and a
/// `thread_spawn` thread with no parent recorded is a session.
pub(crate) fn thread_kind_of_source(
    source: Option<&Value>,
    parent_thread_id: Option<String>,
) -> ThreadKind {
    if let Some(Value::Object(source)) = source {
        if source.contains_key("internal") {
            return ThreadKind::Skipped { parent_thread_id };
        }
        if let Some(subagent) = source.get("subagent") {
            let started_by_a_thread = match subagent {
                Value::String(variant) => variant == "thread_spawn",
                Value::Object(variant) => variant.contains_key("thread_spawn"),
                _ => false,
            };
            if !started_by_a_thread {
                return ThreadKind::Skipped { parent_thread_id };
            }
        }
    }
    match parent_thread_id {
        Some(parent_thread_id) => ThreadKind::Subagent { parent_thread_id },
        None => ThreadKind::Session,
    }
}

/// The kind of thread a rollout records, read from its first line without
/// parsing the rest of the file. `None` for a file that does not open as a
/// rollout. The thread's id is in the file name, not read here.
pub(crate) fn thread_kind_of(path: &Path) -> Option<ThreadKind> {
    Some(header_of(path).ok()??.kind)
}

/// The header on the first line of the rollout at `path`, or `None` when that
/// line is not a rollout header.
fn header_of(path: &Path) -> Result<Option<RolloutHeader>> {
    let mut reader = BufReader::new(File::open(path)?);
    Ok(read_header(&mut reader)?.map(|(header, _)| header))
}

/// The header `reader` opens with, and the number of the line after it.
/// `None` when the first line holding more than whitespace is not a rollout
/// header.
fn read_header(reader: &mut impl BufRead) -> Result<Option<(RolloutHeader, usize)>> {
    let mut line = String::new();
    let mut line_number = 0;
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            return Ok(None);
        }
        line_number += 1;
        if line.trim().is_empty() {
            continue;
        }
        let header = serde_json::from_str::<Value>(&line)
            .ok()
            .and_then(|value| rollout_header(&value));
        return Ok(header.map(|header| (header, line_number + 1)));
    }
}

/// The thread ending in the rollout at `path`: its earlier rollouts, oldest
/// first, each up to where the next continues it, then `path`. `None` when the
/// file is not a rollout.
fn parse_thread(
    path: &Path,
    parallel: ParallelThreshold,
    dropped: DroppedRecords,
) -> Result<Option<ThreadRead>> {
    let mut reader = BufReader::new(File::open(path)?);
    let Some((header, first_record_line)) = read_header(&mut reader)? else {
        return Ok(None);
    };
    let mut thread = ThreadProjection::new(&header, dropped);
    thread.read_earlier_rollouts(earlier_rollouts(path, &header), parallel);
    thread.read_records(&header, None, reader, first_record_line, parallel)?;
    Ok(Some(thread.finish(header)))
}

struct ThreadProjection {
    entries: Vec<(RecordLine, LogEntry)>,
    /// The newest rollout's malformed lines, the ones `agent read` reports
    /// against the session's file.
    malformed_lines: Vec<usize>,
    last_shown: LastShown,
    own_history: OwnHistory,
    dropped: DroppedRecords,
    /// The oldest earlier rollout's start, when one was read.
    started: Option<String>,
    earlier_turns: EarlierTurns,
    discarded_turn_subagents: DiscardedTurnSubagents,
}

impl ThreadProjection {
    fn new(newest: &RolloutHeader, mut dropped: DroppedRecords) -> Self {
        // Record the newest rollout's version first: the dropped-records
        // report counts its malformed lines under the first version.
        dropped.record_version(|| newest.cli_version.clone());
        Self {
            entries: Vec::new(),
            malformed_lines: Vec::new(),
            last_shown: LastShown::default(),
            own_history: match newest.own_history_start {
                Some(boundary) => OwnHistory::Pending { boundary },
                None => OwnHistory::Started,
            },
            dropped,
            started: None,
            earlier_turns: EarlierTurns::Read,
            discarded_turn_subagents: DiscardedTurnSubagents::default(),
        }
    }

    /// A rollout that fails to read ends the chain, as a missing one does:
    /// drop the turns read so far, and read the newer rollouts.
    fn read_earlier_rollouts(&mut self, earlier: EarlierRollouts, parallel: ParallelThreshold) {
        self.earlier_turns = earlier.earlier_turns;
        for rollout in earlier.rollouts {
            if let Err(unread) = self.read_earlier(&rollout, parallel) {
                self.entries.clear();
                self.last_shown = LastShown::default();
                self.started = None;
                self.earlier_turns = unread;
                continue;
            }
            self.started.get_or_insert(rollout.header.timestamp);
        }
        let discarded = &mut self.discarded_turn_subagents;
        discarded.has_transient_error |= earlier.has_transient_error;
        for rollout in earlier.outside_chain {
            match File::open(&rollout.path) {
                Ok(file) => discarded.record_spawns_in(BufReader::new(file), rollout.replaced_at),
                Err(error) => discarded.record_read_error(error),
            }
        }
    }

    /// Read `rollout` up to its `end_byte_offset`. In the thread's own
    /// rollout, record the sub-agents started past that offset: an edited
    /// prompt discarded those turns. Fails with
    /// [`EarlierTurns::TransientError`] after a transient error, and
    /// [`EarlierTurns::Missing`] after a permanent one or when the offset cuts
    /// the header.
    fn read_earlier(
        &mut self,
        rollout: &EarlierRollout,
        parallel: ParallelThreshold,
    ) -> std::result::Result<(), EarlierTurns> {
        let unread = |error: AppError| EarlierTurns::from(ReadError::of(&error));
        let file = File::open(&rollout.path)
            .map_err(|error| unread(AppError::from(error)))?
            .take(rollout.end_byte_offset);
        let mut reader = BufReader::new(file);
        let Some((_, first_record_line)) = read_header(&mut reader).map_err(unread)? else {
            // `end_byte_offset` cuts the header.
            return Err(EarlierTurns::Missing);
        };
        let transcript = Arc::from(rollout.path.as_path());
        self.read_records(
            &rollout.header,
            Some(transcript),
            &mut reader,
            first_record_line,
            parallel,
        )
        .map_err(unread)?;
        if let Some(replaced_at) = rollout.replaced_at {
            let past_the_cut = BufReader::new(reader.into_inner().into_inner());
            self.discarded_turn_subagents
                .record_spawns_in(past_the_cut, replaced_at);
        }
        Ok(())
    }

    /// Read the records `reader` holds past `header`, counting dropped records
    /// under the version `header` names. `transcript` is the rollout they come
    /// from, or `None` for the thread's newest rollout.
    fn read_records(
        &mut self,
        header: &RolloutHeader,
        transcript: Option<Arc<Path>>,
        reader: impl BufRead,
        first_line_number: usize,
        parallel: ParallelThreshold,
    ) -> Result<()> {
        self.dropped.record_version(|| header.cli_version.clone());
        let Self {
            entries,
            malformed_lines,
            last_shown,
            own_history,
            dropped,
            ..
        } = self;
        visit_parsed_lines(
            reader,
            first_line_number,
            parallel,
            |line| serde_json::from_str::<Value>(line).ok(),
            |line, _, parsed| {
                let Some(object) = parsed.as_ref().and_then(Value::as_object) else {
                    // `malformed_lines` holds the newest rollout's lines;
                    // count an earlier rollout's under its version.
                    match transcript {
                        None => malformed_lines.push(line),
                        Some(_) => dropped.failed("line"),
                    }
                    return;
                };
                if let OwnHistory::Pending { boundary } = *own_history
                    && !ordinal_below(object, boundary)
                {
                    // The first own record: what was read so far is the
                    // parent's history, its last model and goal included.
                    entries.clear();
                    *last_shown = LastShown::default();
                    *own_history = OwnHistory::Started;
                }
                if let Some(entry) = normalize_line(object, last_shown, dropped) {
                    let record = RecordLine {
                        transcript: transcript.clone(),
                        line,
                    };
                    entries.push((record, entry));
                }
            },
        )?;
        Ok(())
    }

    /// The projection, with the facts of the thread's newest rollout and the
    /// time the thread started.
    fn finish(self, newest: RolloutHeader) -> ThreadRead {
        let projection = SessionProjection {
            source: Source::Codex,
            header: SessionHeader {
                version: 1,
                id: newest.id,
                timestamp: self.started.unwrap_or(newest.timestamp),
                cwd: PathBuf::from(newest.cwd),
                thread_label: None,
                subagent_identity: newest.identity,
            },
            title: None,
            entries: self.entries,
            leaf_id: None,
            malformed_lines: self.malformed_lines,
            dropped: self.dropped,
            earlier_turns: self.earlier_turns,
        };
        ThreadRead {
            projection,
            discarded_turn_subagents: self.discarded_turn_subagents,
        }
    }
}

/// Each `spawn_agent` result names the sub-agent by `agent_id`, or by its agent
/// path in `task_name`.
#[derive(Debug, Default)]
pub(crate) struct DiscardedTurnSubagents {
    thread_ids: HashSet<String>,
    /// Each agent path, with the start of the rollout that replaced the one
    /// naming it. A later sub-agent at that path is a new one.
    agent_paths: HashMap<String, DateTime<FixedOffset>>,
    /// True when a transient error kept discarded turns from being read.
    has_transient_error: bool,
}

/// The transcripts of discarded turns' sub-agents among a session's
/// sub-agents.
#[derive(Debug, Default)]
pub(crate) struct DiscardedTranscripts {
    pub(crate) paths: HashSet<PathBuf>,
    /// True when a transient error kept discarded turns or a transcript's
    /// header from being read: the row is read again on the next load.
    pub(crate) has_transient_error: bool,
}

impl DiscardedTurnSubagents {
    /// Record the sub-agent each `spawn_agent` result in `records` names. Stop
    /// at a read error and record a transient one; after a permanent one, the
    /// sub-agents of the turns left unread show.
    fn record_spawns_in(&mut self, records: impl BufRead, replaced_at: DateTime<FixedOffset>) {
        let mut spawn_calls = HashSet::new();
        for line in records.split(b'\n') {
            let line = match line {
                Ok(line) => line,
                Err(error) => return self.record_read_error(error),
            };
            let Ok(record) = serde_json::from_slice::<Value>(&line) else {
                continue;
            };
            let Some(payload) = record.get("payload").and_then(Value::as_object) else {
                continue;
            };
            let Some(call_id) = string_field(payload, "call_id") else {
                continue;
            };
            match payload.get("type").and_then(Value::as_str) {
                Some("function_call")
                    if string_field(payload, "name").as_deref() == Some(SPAWN_AGENT) =>
                {
                    spawn_calls.insert(call_id);
                }
                Some("function_call_output") if spawn_calls.contains(&call_id) => {
                    self.record_spawn_result(&output_text(payload), replaced_at);
                }
                _ => {}
            }
        }
    }

    fn record_read_error(&mut self, error: std::io::Error) {
        self.has_transient_error |= ReadError::of(&AppError::Io(error)) == ReadError::Transient;
    }

    fn record_spawn_result(&mut self, output: &str, replaced_at: DateTime<FixedOffset>) {
        let Ok(result) = serde_json::from_str::<Value>(output) else {
            // A failed spawn's result is plain text naming no thread.
            return;
        };
        let field = |name: &str| result.get(name).and_then(Value::as_str).map(str::to_owned);
        self.thread_ids.extend(field("agent_id"));
        if let Some(agent_path) = field("task_name") {
            let latest = self.agent_paths.entry(agent_path).or_insert(replaced_at);
            *latest = replaced_at.max(*latest);
        }
    }

    /// The transcripts among `subagents` of these sub-agents and of every
    /// thread beneath them. Reads each transcript's header when any sub-agent
    /// was recorded.
    pub(crate) fn transcripts_among(&self, subagents: &[PathBuf]) -> DiscardedTranscripts {
        let mut discarded = DiscardedTranscripts {
            paths: HashSet::new(),
            has_transient_error: self.has_transient_error,
        };
        if self.thread_ids.is_empty() && self.agent_paths.is_empty() {
            return discarded;
        }
        let mut threads: Vec<(&PathBuf, RolloutHeader)> = Vec::new();
        for path in subagents {
            match header_of(path) {
                Ok(Some(header)) => threads.push((path, header)),
                Ok(None) => {}
                Err(error) => {
                    discarded.has_transient_error |= ReadError::of(&error) == ReadError::Transient;
                }
            }
        }
        let started = threads
            .iter()
            .filter(|(_, header)| self.includes(header))
            .map(|(_, header)| header.id.clone());
        let shown = SubagentForest::new(threads.iter().map(|(_, header)| {
            let parent = header.kind.parent_thread_id().map(str::to_owned);
            (header.id.clone(), parent)
        }))
        .without(started);
        discarded.paths = threads
            .into_iter()
            .filter(|(_, header)| !shown.contains(&header.id))
            .map(|(path, _)| path.clone())
            .collect();
        discarded
    }

    fn includes(&self, header: &RolloutHeader) -> bool {
        let started = instant_of(&header.timestamp);
        self.thread_ids.contains(&header.id)
            || header
                .identity
                .agent_path
                .as_ref()
                .is_some_and(|agent_path| {
                    let replaced_at = self.agent_paths.get(agent_path);
                    replaced_at
                        .zip(started)
                        .is_some_and(|(replaced_at, started)| started < *replaced_at)
                })
    }
}

const SPAWN_AGENT: &str = "spawn_agent";

/// A header's or record's `timestamp` as an instant, to compare across
/// fractional precisions and offsets; `None` when it is not RFC 3339.
fn instant_of(timestamp: &str) -> Option<DateTime<FixedOffset>> {
    DateTime::parse_from_rfc3339(timestamp).ok()
}

/// An earlier rollout a thread continues: its header, and the byte just past
/// the last of its records the thread inherits.
struct EarlierRollout {
    path: PathBuf,
    header: RolloutHeader,
    end_byte_offset: u64,
    /// For the thread's own rollout, the start of the rollout that replaced
    /// it; its turns past `end_byte_offset` are discarded. `None` for another
    /// thread's rollout, or when that start cannot be read.
    replaced_at: Option<DateTime<FixedOffset>>,
}

/// The earlier rollouts a thread continues, oldest first, and the thread's
/// own rollouts an edit left outside that chain.
struct EarlierRollouts {
    rollouts: Vec<EarlierRollout>,
    /// [`EarlierTurns::Read`] unless the oldest rollout read continues one
    /// that could not be read.
    earlier_turns: EarlierTurns,
    /// Every turn in these rollouts is discarded.
    outside_chain: Vec<ReplacedRollout>,
    /// True when a transient error may have hidden one of the thread's own
    /// rollouts.
    has_transient_error: bool,
}

/// One of the thread's own earlier rollouts, and the start of the rollout
/// that replaced it.
#[derive(Clone)]
struct ReplacedRollout {
    path: PathBuf,
    replaced_at: DateTime<FixedOffset>,
}

impl From<ReadError> for EarlierTurns {
    fn from(error: ReadError) -> Self {
        match error {
            ReadError::Transient => Self::TransientError,
            ReadError::Permanent => Self::Missing,
        }
    }
}

/// The earlier rollouts `newest` continues, oldest first. A missing,
/// compressed or unreadable rollout ends the chain: the rollouts after it are
/// read, and the ones before it are not. A rollout already in the chain ends
/// it too.
fn earlier_rollouts(transcript: &Path, newest: &RolloutHeader) -> EarlierRollouts {
    let mut earlier = EarlierRollouts {
        rollouts: Vec::new(),
        earlier_turns: EarlierTurns::Read,
        outside_chain: Vec::new(),
        has_transient_error: false,
    };
    let Some(mut base) = newest.history_base.clone() else {
        return earlier;
    };
    let rollouts = RolloutIndex::around(transcript);
    let mut replaced = ReplacedRollouts::before(&rollouts, transcript, newest);
    let mut chained: HashSet<String> = RolloutFileName::parse_path(transcript)
        .map(|name| name.rollout_id.to_owned())
        .into_iter()
        .collect();
    while chained.insert(base.rollout_id.clone()) {
        let (path, header) = match rollouts.find(&base.rollout_id) {
            Ok(found) => found,
            Err(unread) => {
                earlier.earlier_turns = unread.into();
                break;
            }
        };
        let next = header.history_base.clone();
        earlier.rollouts.push(EarlierRollout {
            path,
            header,
            end_byte_offset: base.end_byte_offset,
            replaced_at: replaced
                .by_rollout_id
                .remove(&base.rollout_id)
                .map(|rollout| rollout.replaced_at),
        });
        match next {
            Some(next) => base = next,
            None => break,
        }
    }
    earlier.rollouts.reverse();
    earlier.outside_chain = replaced
        .by_rollout_id
        .into_iter()
        .filter(|(rollout_id, _)| !chained.contains(rollout_id))
        .map(|(_, rollout)| rollout)
        .collect();
    earlier.has_transient_error = replaced.has_transient_error;
    earlier
}

/// The thread's own rollouts older than its newest, each with the start of
/// the rollout that replaced it: the next in resume order.
struct ReplacedRollouts {
    by_rollout_id: HashMap<String, ReplacedRollout>,
    /// True when a transient error kept part of the trees or a header from
    /// being read.
    has_transient_error: bool,
}

impl ReplacedRollouts {
    fn before(index: &RolloutIndex, transcript: &Path, newest: &RolloutHeader) -> Self {
        let mut replaced = Self {
            by_rollout_id: HashMap::new(),
            has_transient_error: index.has_transient_error,
        };
        let Some(newest_name) = RolloutFileName::parse_path(transcript) else {
            return replaced;
        };
        let mut older: Vec<(RolloutFileName<'_>, &PathBuf)> = index
            .paths
            .values()
            .filter_map(|path| Some((RolloutFileName::parse_path(path)?, path)))
            .filter(|(name, _)| {
                name.thread_id == newest_name.thread_id
                    && name.resume_order() < newest_name.resume_order()
            })
            .collect();
        older.sort_by_key(|(name, _)| name.resume_order());
        let mut next_start = instant_of(&newest.timestamp);
        for (name, path) in older.into_iter().rev() {
            if let Some(replaced_at) = next_start {
                let rollout = ReplacedRollout {
                    path: path.clone(),
                    replaced_at,
                };
                let rollout_id = name.rollout_id.to_owned();
                replaced.by_rollout_id.insert(rollout_id, rollout);
            }
            next_start = match header_of(path) {
                Ok(header) => header.and_then(|header| instant_of(&header.timestamp)),
                Err(error) => {
                    replaced.has_transient_error |= ReadError::of(&error) == ReadError::Transient;
                    None
                }
            };
        }
        replaced
    }
}

/// Codex moves an archived thread's rollouts into this directory, beside the
/// `sessions` tree.
const ARCHIVED_SESSIONS_DIRECTORY: &str = "archived_sessions";

fn archived_tree_beside(sessions_tree: &Path) -> Option<PathBuf> {
    sessions_tree
        .parent()
        .map(|home| home.join(ARCHIVED_SESSIONS_DIRECTORY))
}

/// Every plain rollout in the archived tree beside `sessions_tree`; empty when
/// that tree does not exist. Fails when a directory or one of its entries
/// cannot be listed.
pub(crate) fn archived_rollouts_beside(sessions_tree: &Path) -> Result<Vec<PathBuf>> {
    let mut rollouts = Vec::new();
    let Some(archived) = archived_tree_beside(sessions_tree) else {
        return Ok(rollouts);
    };
    let entries = match std::fs::read_dir(&archived) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(rollouts),
        Err(error) => return Err(error.into()),
    };
    push_plain_rollouts(entries, &mut rollouts)?;
    rollouts.sort();
    Ok(rollouts)
}

fn push_plain_rollouts(entries: std::fs::ReadDir, rollouts: &mut Vec<PathBuf>) -> Result<()> {
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let kind = entry.file_type()?;
        if kind.is_dir() {
            push_plain_rollouts(std::fs::read_dir(&path)?, rollouts)?;
        } else if kind.is_file() && RolloutFileName::parse_path(&path).is_some() {
            rollouts.push(path);
        }
    }
    Ok(())
}

/// The threads beneath `threads`, nested ones included and `threads` left
/// out. Each `archived` rollout's header names its thread's parent. Fails
/// when a header cannot be read.
pub(crate) fn archived_threads_beneath(
    threads: &HashSet<String>,
    archived: &[PathBuf],
) -> Result<BTreeSet<String>> {
    let mut parent_of = HashMap::new();
    for rollout in archived {
        let Some(name) = RolloutFileName::parse_path(rollout) else {
            continue;
        };
        if threads.contains(name.thread_id) || parent_of.contains_key(name.thread_id) {
            continue;
        }
        let parent = header_of(rollout)?
            .and_then(|header| header.kind.parent_thread_id().map(str::to_owned));
        if let Some(parent) = parent {
            parent_of.insert(name.thread_id.to_owned(), parent);
        }
    }
    let forest = SubagentForest::new(
        parent_of
            .into_iter()
            .map(|(thread, parent)| (thread, Some(parent))),
    );
    Ok(threads
        .iter()
        .flat_map(|thread| forest.subagents_of(thread))
        .filter(|thread| !threads.contains(thread))
        .collect())
}

/// Every plain rollout in the sessions tree holding a transcript and in the
/// archived tree beside it, by rollout id; the sessions tree's copy wins.
#[derive(Default)]
struct RolloutIndex {
    paths: HashMap<String, PathBuf>,
    /// True when a transient error kept a directory or one of its entries
    /// from being listed, so a rollout absent from `paths` can still be on
    /// disk.
    has_transient_error: bool,
}

impl RolloutIndex {
    /// The index around `transcript`; empty for a transcript outside a
    /// sessions tree.
    fn around(transcript: &Path) -> Self {
        let mut index = Self::default();
        let Some(sessions) = sessions_tree_of(transcript) else {
            return index;
        };
        index.add_rollouts_under(sessions);
        if let Some(archived) = archived_tree_beside(sessions) {
            index.add_rollouts_under(&archived);
        }
        index
    }

    fn add_rollouts_under(&mut self, directory: &Path) {
        let entries = match std::fs::read_dir(directory) {
            Ok(entries) => entries,
            Err(error) => {
                self.record_listing_error(error);
                return;
            }
        };
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    self.record_listing_error(error);
                    continue;
                }
            };
            let path = entry.path();
            match entry.file_type() {
                Ok(kind) if kind.is_dir() => self.add_rollouts_under(&path),
                Ok(kind) if kind.is_file() => {
                    if let Some(name) = RolloutFileName::parse_path(&path) {
                        let rollout_id = name.rollout_id.to_owned();
                        self.paths.entry(rollout_id).or_insert(path);
                    }
                }
                Ok(_) => {}
                Err(error) => self.record_listing_error(error),
            }
        }
    }

    fn record_listing_error(&mut self, error: std::io::Error) {
        self.has_transient_error |= ReadError::of(&AppError::Io(error)) == ReadError::Transient;
    }

    /// The path and header of the rollout `rollout_id` names. A rollout
    /// absent from the index fails as transient when a transient error kept
    /// part of the tree from being listed.
    fn find(&self, rollout_id: &str) -> std::result::Result<(PathBuf, RolloutHeader), ReadError> {
        let Some(path) = self.paths.get(rollout_id) else {
            return Err(if self.has_transient_error {
                ReadError::Transient
            } else {
                ReadError::Permanent
            });
        };
        match header_of(path) {
            Ok(Some(header)) => Ok((path.clone(), header)),
            Ok(None) => Err(ReadError::Permanent),
            Err(error) => Err(ReadError::of(&error)),
        }
    }
}

/// The threads whose rollouts continue rollouts a delete would remove.
#[derive(Debug, Eq, PartialEq)]
pub(crate) enum ForksContinuing {
    None,
    Found(BTreeSet<String>),
    /// None found, but a transient error left part of the trees unread.
    Unchecked,
}

/// Find, in the sessions tree holding `transcript` and the archived tree
/// beside it, every thread outside `deleted_threads` with a rollout
/// continuing one in `deleted_rollouts`. Pass over a rollout with a permanent
/// error on its header: Codex cannot open it as a fork either.
pub(crate) fn forks_continuing(
    transcript: &Path,
    deleted_threads: &HashSet<String>,
    deleted_rollouts: &HashSet<String>,
) -> ForksContinuing {
    let index = RolloutIndex::around(transcript);
    let mut forks = BTreeSet::new();
    let mut has_transient_error = index.has_transient_error;
    for path in index.paths.values() {
        let Some(name) = RolloutFileName::parse_path(path) else {
            continue;
        };
        if deleted_threads.contains(name.thread_id) {
            continue;
        }
        match header_of(path) {
            Ok(Some(header)) => {
                let continues_a_deleted_rollout = header
                    .history_base
                    .is_some_and(|base| deleted_rollouts.contains(&base.rollout_id));
                if continues_a_deleted_rollout {
                    forks.insert(name.thread_id.to_owned());
                }
            }
            Ok(None) => {}
            Err(error) => {
                has_transient_error |= ReadError::of(&error) == ReadError::Transient;
            }
        }
    }
    if !forks.is_empty() {
        ForksContinuing::Found(forks)
    } else if has_transient_error {
        ForksContinuing::Unchecked
    } else {
        ForksContinuing::None
    }
}

/// Ownership of the lines below `subagent_history_start_ordinal`, decided by
/// whether a record at or past it follows.
///
/// A live sub-agent thread copies a bounded parent context into its rollout
/// and sets the boundary at its first own record: the lines below are the
/// parent's history, and indexing them would count the parent's text and
/// tokens twice once the thread folds into it. Codex's legacy-to-paginated
/// migration sets the same field past the last record of every sub-agent
/// rollout it rewrites: nothing follows, and the whole file is the thread's
/// own. A live thread that ended before its first own record has the migrated
/// shape and shows its copied context as its own.
#[derive(Clone, Copy)]
enum OwnHistory {
    /// Every line so far sits below the boundary. Its entries are kept until
    /// a record at or past the boundary shows them to be the parent's.
    Pending { boundary: u64 },
    /// Every line from here on is the thread's own.
    Started,
}

fn ordinal_below(object: &Map<String, Value>, boundary: u64) -> bool {
    object
        .get("ordinal")
        .and_then(Value::as_u64)
        .is_some_and(|ordinal| ordinal < boundary)
}

/// The last model and goal shown. A line that repeats them shows no row.
#[derive(Default)]
struct LastShown {
    model: Option<String>,
    goal: Option<ShownGoal>,
}

fn normalize_line(
    object: &Map<String, Value>,
    last_shown: &mut LastShown,
    dropped: &mut DroppedRecords,
) -> Option<LogEntry> {
    let timestamp = object
        .get("timestamp")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let payload = object.get("payload")?.as_object()?;
    let kind = object.get("type").and_then(Value::as_str);
    match kind {
        Some("response_item") => normalize_response_item(payload, timestamp, dropped),
        Some("compacted") => compaction_summary(payload, timestamp),
        Some("turn_context") => model_change(payload, timestamp, &mut last_shown.model),
        Some("event_msg") => event_message(payload, timestamp, &mut last_shown.goal, dropped),
        // A later `session_meta` is inherited context; `world_state`,
        // `security_risk_score` and `inter_agent_communication*` carry nothing
        // the browser renders or indexes.
        Some("session_meta" | "world_state" | "security_risk_score") => {
            dropped.skipped("type", kind);
            None
        }
        Some(name) if name.starts_with("inter_agent_communication") => {
            dropped.skipped("type", kind);
            None
        }
        _ => {
            dropped.unrecognised("type", kind);
            None
        }
    }
}

fn normalize_response_item(
    payload: &Map<String, Value>,
    timestamp: Option<String>,
    dropped: &mut DroppedRecords,
) -> Option<LogEntry> {
    let kind = payload.get("type").and_then(Value::as_str);
    match kind {
        Some("message") => normalize_message(payload, timestamp, dropped),
        Some("reasoning") => reasoning_summary(payload, timestamp),
        Some("custom_tool_call") => tool_call(payload, payload.get("input").cloned(), timestamp),
        Some("function_call") => {
            let arguments = payload.get("arguments").and_then(Value::as_str);
            let input = arguments.map(|raw| {
                serde_json::from_str(raw).unwrap_or_else(|_| json!({ "arguments": raw }))
            });
            tool_call(payload, input, timestamp)
        }
        Some("custom_tool_call_output" | "function_call_output") => tool_result(payload, timestamp),
        Some("agent_message") => inter_agent_message(payload, timestamp),
        Some("web_search_call") => web_search_call(payload, timestamp),
        _ => {
            dropped.unrecognised("response_item.type", kind);
            None
        }
    }
}

fn normalize_message(
    payload: &Map<String, Value>,
    timestamp: Option<String>,
    dropped: &mut DroppedRecords,
) -> Option<LogEntry> {
    let role = payload.get("role").and_then(Value::as_str);
    match role {
        Some("user") => {
            let texts = block_texts(payload.get("content"));
            // A shell command the user ran is one complete XML element, which
            // `is_injected_context` would otherwise read as context Codex
            // injected.
            if let [text] = texts.as_slice()
                && let Some(command) = UserShellCommand::parse(text)
            {
                return Some(user_shell_command_entry(payload, &command, timestamp));
            }
            let typed_by_the_user = texts
                .into_iter()
                .filter(|text| !is_injected_context(text))
                .collect::<Vec<_>>();
            if typed_by_the_user.is_empty() {
                return None;
            }
            Some(LogEntry::User {
                message: UserMessage {
                    role: "user".to_owned(),
                    content: UserContent::Blocks(
                        typed_by_the_user
                            .into_iter()
                            .map(|text| ContentBlock::Text { text })
                            .collect(),
                    ),
                },
                timestamp,
                uuid: None,
                cwd: None,
                parent_tool_use_id: None,
                source_tool_use_id: None,
                usage: None,
            })
        }
        Some("assistant") => {
            let texts = block_texts(payload.get("content"));
            if texts.is_empty() {
                return None;
            }
            Some(assistant_entry(
                payload,
                texts
                    .into_iter()
                    .map(|text| ContentBlock::Text { text })
                    .collect(),
                timestamp,
            ))
        }
        // Developer messages are prompt plumbing — skills, permissions, task
        // wiring — not conversation.
        Some("developer") => {
            dropped.skipped("message.role", role);
            None
        }
        _ => {
            dropped.unrecognised("message.role", role);
            None
        }
    }
}

/// A command the user ran through Codex, as its `<user_shell_command>` wrapper
/// records it. `Duration` is in the wrapper too, with no row to sit on.
struct UserShellCommand<'a> {
    command: &'a str,
    exit_code: i64,
    output: &'a str,
}

/// The literals Codex builds the wrapper from, in the order they appear.
const WRAPPER_OPEN: &str = "<user_shell_command>\n<command>\n";
const COMMAND_END: &str = "\n</command>\n<result>\n";
const EXIT_CODE_LABEL: &str = "Exit code: ";
const OUTPUT_LABEL: &str = "\nOutput:\n";
const WRAPPER_CLOSE: &str = "\n</result>\n</user_shell_command>";

impl<'a> UserShellCommand<'a> {
    /// Codex interpolates the command and the output into the wrapper raw, so
    /// a command that prints `</result>` leaves text no XML parser accepts.
    /// Anchoring on the wrapper's own literals reads each half whole: the
    /// closing anchor matches the last closing tag, the split the first
    /// opening one.
    ///
    /// A wrapper that fails an anchor falls through to `is_injected_context`,
    /// which drops it, so a format Codex changes costs a missing row rather
    /// than a wrong one.
    fn parse(text: &'a str) -> Option<Self> {
        let inner = text
            .trim()
            .strip_prefix(WRAPPER_OPEN)?
            .strip_suffix(WRAPPER_CLOSE)?;
        let (command, result) = inner.split_once(COMMAND_END)?;
        let (exit_line, _) = result.split_once('\n')?;
        Some(Self {
            command,
            exit_code: exit_line
                .strip_prefix(EXIT_CODE_LABEL)?
                .trim()
                .parse()
                .ok()?,
            // A command that printed nothing carries no `Output:` section.
            output: result.split_once(OUTPUT_LABEL).map_or("", |(_, out)| out),
        })
    }

    /// Builds the result body: what the command printed, with the exit line
    /// below it when the command failed, and the shell's terminal styling and
    /// `\r\n` endings stripped.
    fn result_text(&self) -> String {
        let mut text = trim_blank_lines(&sanitize_agent_text(self.output)).to_owned();
        if self.exit_code != 0 {
            append_exit_code(&mut text, self.exit_code);
        }
        tool_result_text(&Value::String(text)).unwrap_or_default()
    }
}

/// Builds one command's call-and-result pair, as the Pi reader builds it for
/// its own record.
fn user_shell_command_entry(
    payload: &Map<String, Value>,
    command: &UserShellCommand<'_>,
    timestamp: Option<String>,
) -> LogEntry {
    let call_id = string_field(payload, "id").unwrap_or_else(|| "unknown".to_owned());
    LogEntry::User {
        message: UserMessage {
            role: "user".to_owned(),
            content: UserContent::Blocks(vec![
                ContentBlock::ToolUse {
                    id: call_id.clone(),
                    name: "shell".to_owned(),
                    tool: Tool::UserShell,
                    input: json!({ "command": sanitize_agent_text(command.command) }),
                },
                ContentBlock::ToolResult {
                    tool_use_id: call_id,
                    content: Some(json!(command.result_text())),
                    standalone_tool_name: None,
                },
            ]),
        },
        timestamp,
        uuid: None,
        cwd: None,
        parent_tool_use_id: None,
        source_tool_use_id: None,
        usage: None,
    }
}

/// Reasoning arrives encrypted with an optional plain-text summary; only the
/// summary is readable, and locally it is almost always absent.
fn reasoning_summary(payload: &Map<String, Value>, timestamp: Option<String>) -> Option<LogEntry> {
    let blocks = block_texts(payload.get("summary"))
        .into_iter()
        .map(|thinking| ContentBlock::Thinking {
            thinking,
            signature: String::new(),
        })
        .collect::<Vec<_>>();
    if blocks.is_empty() {
        return None;
    }
    Some(assistant_entry(payload, blocks, timestamp))
}

fn tool_call(
    payload: &Map<String, Value>,
    input: Option<Value>,
    timestamp: Option<String>,
) -> Option<LogEntry> {
    let call_id = string_field(payload, "call_id").unwrap_or_else(|| "unknown".to_owned());
    let name = string_field(payload, "name").unwrap_or_else(|| "unknown".to_owned());
    let blocks = tools::tool_use_blocks(&call_id, &name, &input.unwrap_or_else(|| json!({})));
    Some(assistant_entry(payload, blocks, timestamp))
}

/// The model's web search through Codex's built-in tool. The call has no
/// result: Codex saves none.
fn web_search_call(payload: &Map<String, Value>, timestamp: Option<String>) -> Option<LogEntry> {
    let id = string_field(payload, "id").unwrap_or_else(|| "web_search".to_owned());
    let action = payload.get("action").and_then(Value::as_object);
    let block = tools::web_search_call_block(&id, action);
    Some(assistant_entry(payload, vec![block], timestamp))
}

/// Reads a tool-result payload into a user entry. With no `call_id` the result
/// answers no call: it takes the item's own `id`, or `"unknown"` when the
/// payload carries neither, and keeps the tool name the payload gives.
fn tool_result(payload: &Map<String, Value>, timestamp: Option<String>) -> Option<LogEntry> {
    let text = tool_result_text(&Value::String(output_text(payload))).unwrap_or_default();
    // The id and the name are decided together: with no `call_id` the result
    // answers no call, so it takes the item's own id and the tool it names.
    let (tool_use_id, standalone_tool_name) = match string_field(payload, "call_id") {
        Some(call_id) => (call_id, None),
        None => (
            string_field(payload, "id").unwrap_or_else(|| "unknown".to_owned()),
            string_field(payload, "name"),
        ),
    };
    Some(LogEntry::User {
        message: UserMessage {
            role: "user".to_owned(),
            content: UserContent::Blocks(vec![ContentBlock::ToolResult {
                tool_use_id,
                content: Some(json!(text)),
                standalone_tool_name,
            }]),
        },
        timestamp,
        uuid: None,
        cwd: None,
        parent_tool_use_id: None,
        source_tool_use_id: None,
        usage: None,
    })
}

/// A tool result's output: one string, or the text of content blocks typed
/// `input_text`, which `tool_result_text` would skip.
fn output_text(payload: &Map<String, Value>) -> String {
    match payload.get("output") {
        Some(Value::String(text)) => text.clone(),
        output => block_texts(output).join("\n"),
    }
}

/// A message between agents of a multi-agent run. A sub-agent's final answer
/// reads as a task report: the report the sub-agent hands back. Any other
/// message is modelled as a tool call, the same treatment Claude's Task
/// dispatches get: visible in the viewer, absent from the search index, while
/// the sub-agent's own transcript carries the searchable content.
fn inter_agent_message(
    payload: &Map<String, Value>,
    timestamp: Option<String>,
) -> Option<LogEntry> {
    let text = block_texts(payload.get("content")).join("\n");
    if let Some(report) = final_answer_report(&text, string_field(payload, "author").as_deref()) {
        return Some(LogEntry::User {
            message: UserMessage {
                role: "user".to_owned(),
                content: UserContent::String(report.notification_text()),
            },
            timestamp,
            uuid: None,
            cwd: None,
            parent_tool_use_id: None,
            source_tool_use_id: None,
            usage: None,
        });
    }
    let block = ContentBlock::ToolUse {
        id: string_field(payload, "id").unwrap_or_else(|| "inter_agent".to_owned()),
        name: "inter_agent_message".to_owned(),
        tool: Tool::Other,
        input: json!({
            "author": string_field(payload, "author"),
            "recipient": string_field(payload, "recipient"),
            "message": text,
        }),
    };
    Some(assistant_entry(payload, vec![block], timestamp))
}

/// The first line of a sub-agent's final answer; header lines follow, then
/// `Payload:` on a line of its own and the report.
const FINAL_ANSWER_TYPE: &str = "Message Type: FINAL_ANSWER";
const FINAL_ANSWER_PAYLOAD: &str = "Payload:";

/// The report a final answer carries, named by the task of the sub-agent that
/// wrote it: the last segment of its agent path (`/root/scout` is `scout`),
/// the name its `spawn_agent` row shows. The agent path is its sender.
fn final_answer_report(text: &str, author: Option<&str>) -> Option<TaskReport> {
    let mut lines = text.split_inclusive('\n');
    if lines.next()?.trim_end() != FINAL_ANSWER_TYPE {
        return None;
    }
    lines
        .by_ref()
        .find(|line| line.trim_end() == FINAL_ANSWER_PAYLOAD)?;
    let report: String = lines.collect();
    let task = author.and_then(task_name);
    Some(
        TaskReport::handed_back(task, &report).sent_by(ReportSender {
            task_id: author.map(str::to_owned),
            launch_call: None,
        }),
    )
}

/// The task name an agent path ends with: `scout` for `/root/scout`.
fn task_name(agent_path: &str) -> Option<&str> {
    agent_path.rsplit('/').find(|segment| !segment.is_empty())
}

fn assistant_entry(
    payload: &Map<String, Value>,
    content: Vec<ContentBlock>,
    timestamp: Option<String>,
) -> LogEntry {
    LogEntry::Assistant {
        agent: Some("Codex".to_owned()),
        message: AssistantMessage {
            role: "assistant".to_owned(),
            content,
            model: None,
            usage: None,
            id: string_field(payload, "id"),
        },
        timestamp,
        uuid: None,
        parent_tool_use_id: None,
    }
}

fn compaction_summary(payload: &Map<String, Value>, timestamp: Option<String>) -> Option<LogEntry> {
    // `replacement_history` is deliberately unread: it restates earlier lines.
    let message = payload.get("message").and_then(Value::as_str)?;
    if message.is_empty() {
        return None;
    }
    Some(LogEntry::PiMetadata {
        label: "Compaction".to_owned(),
        text: message.to_owned(),
        timestamp,
        searchable: false,
        usage: None,
    })
}

fn model_change(
    payload: &Map<String, Value>,
    timestamp: Option<String>,
    last_model: &mut Option<String>,
) -> Option<LogEntry> {
    let model = payload.get("model").and_then(Value::as_str)?;
    if last_model.as_deref() == Some(model) {
        return None;
    }
    *last_model = Some(model.to_owned());
    Some(LogEntry::PiMetadata {
        label: "Model".to_owned(),
        text: model.to_owned(),
        timestamp,
        searchable: false,
        usage: None,
    })
}

/// Events that restate a line the rollout already holds as a `response_item`:
/// a message, reasoning, or a tool call's result.
const RESTATED_EVENTS: &[&str] = &[
    "agent_message",
    "user_message",
    "agent_reasoning",
    "agent_reasoning_raw_content",
    "exec_command_end",
    "patch_apply_end",
    "mcp_tool_call_end",
    "web_search_end",
    "item_completed",
];

/// Events that mark a turn's or a thread's lifecycle and carry nothing the
/// browser renders. A compaction's summary is the `compacted` line, and a
/// sub-agent's activity is in its own rollout.
const LIFECYCLE_EVENTS: &[&str] = &[
    "task_started",
    "task_complete",
    "thread_settings_applied",
    "thread_rolled_back",
    "context_compacted",
    "sub_agent_activity",
];

/// An `event_msg` line: usage from `token_count`, a `turn_aborted` notice, or a
/// `thread_goal_updated` row. An event in [`RESTATED_EVENTS`] or
/// [`LIFECYCLE_EVENTS`] counts as skipped, any other as unrecognised.
fn event_message(
    payload: &Map<String, Value>,
    timestamp: Option<String>,
    last_goal: &mut Option<ShownGoal>,
    dropped: &mut DroppedRecords,
) -> Option<LogEntry> {
    let kind = payload.get("type").and_then(Value::as_str);
    match kind {
        Some("token_count") => token_usage(payload),
        Some("turn_aborted") => turn_aborted(payload, timestamp, dropped),
        Some("thread_goal_updated") => goal_update(payload, timestamp, last_goal),
        Some(name) if RESTATED_EVENTS.contains(&name) || LIFECYCLE_EVENTS.contains(&name) => {
            dropped.skipped("event_msg.type", kind);
            None
        }
        _ => {
            dropped.unrecognised("event_msg.type", kind);
            None
        }
    }
}

/// Codex's notice for an interrupted turn or a goal-budget stop, in its words.
/// Codex shows none for a replaced turn or an ended review.
fn turn_aborted(
    payload: &Map<String, Value>,
    timestamp: Option<String>,
    dropped: &mut DroppedRecords,
) -> Option<LogEntry> {
    let reason = payload.get("reason").and_then(Value::as_str);
    let notice = match reason {
        Some("interrupted") => "Conversation interrupted",
        Some("budget_limited") => "Goal budget reached",
        Some("replaced" | "review_ended") => return None,
        _ => {
            dropped.unrecognised("turn_aborted.reason", reason);
            return None;
        }
    };
    Some(shown_metadata(NOTICE_LABEL, notice.to_owned(), timestamp))
}

/// A goal as its `Goal` row showed it.
struct ShownGoal {
    created_at: Option<i64>,
    objective: String,
    status: String,
}

/// A `Goal` row when the goal or its status changes: the objective for a new
/// goal, the status for an existing one. Codex also records an update as the
/// goal's token and time usage grows; a usage change alone shows no row.
fn goal_update(
    payload: &Map<String, Value>,
    timestamp: Option<String>,
    last_goal: &mut Option<ShownGoal>,
) -> Option<LogEntry> {
    let goal = payload.get("goal")?.as_object()?;
    let update = ShownGoal {
        created_at: goal.get("createdAt").and_then(Value::as_i64),
        objective: string_field(goal, "objective")?,
        status: string_field(goal, "status").unwrap_or_default(),
    };
    let text = match last_goal.as_ref() {
        Some(last)
            if last.created_at == update.created_at && last.objective == update.objective =>
        {
            if last.status == update.status {
                return None;
            }
            goal_status_words(&update.status).to_owned()
        }
        _ => update.objective.clone(),
    };
    *last_goal = Some(update);
    Some(shown_metadata("Goal", text, timestamp))
}

/// A goal status in Codex's words.
fn goal_status_words(status: &str) -> &str {
    match status {
        "blocked" => "stalled",
        "usageLimited" => "usage limited",
        "budgetLimited" => "limited by budget",
        status => status,
    }
}

/// A labelled row with `searchable` set: the viewer shows it and search finds
/// it.
fn shown_metadata(label: &str, text: String, timestamp: Option<String>) -> LogEntry {
    LogEntry::PiMetadata {
        label: label.to_owned(),
        text,
        timestamp,
        searchable: true,
        usage: None,
    }
}

/// Usage from a `token_count` event, as an invisible metadata entry.
///
/// The per-event `last_token_usage` values are summed because the running
/// `total_token_usage` cannot be trusted: it resets at compaction and inherits
/// the parent's context in a sub-agent thread — both observed in the corpus.
fn token_usage(payload: &Map<String, Value>) -> Option<LogEntry> {
    if payload.get("type").and_then(Value::as_str) != Some("token_count") {
        return None;
    }
    let last = payload.get("info")?.get("last_token_usage")?.as_object()?;
    let count = |field: &str| last.get(field).and_then(Value::as_u64).unwrap_or(0);
    let (input, cached) = (count("input_tokens"), count("cached_input_tokens"));
    let usage = TokenUsage {
        // Codex's input count includes cache reads; splitting them keeps the
        // total a plain sum of the four fields, as for every other source.
        input_tokens: input.saturating_sub(cached),
        output_tokens: count("output_tokens"),
        cache_creation_input_tokens: count("cache_write_input_tokens"),
        cache_read_input_tokens: cached,
    };
    let total = usage.input_tokens
        + usage.output_tokens
        + usage.cache_creation_input_tokens
        + usage.cache_read_input_tokens;
    if total == 0 {
        return None;
    }
    Some(LogEntry::PiMetadata {
        label: "Usage".to_owned(),
        text: String::new(),
        timestamp: None,
        searchable: false,
        usage: Some(usage),
    })
}

fn string_field(payload: &Map<String, Value>, name: &str) -> Option<String> {
    payload.get(name).and_then(Value::as_str).map(str::to_owned)
}

/// Whether a user-role content block is context Codex injected rather than
/// something the user typed.
///
/// Codex wraps every detectable injected fragment in a marker pair — a
/// `<tag>…</tag>` wrapper, a bare placeholder tag, or the AGENTS.md pair — so
/// the test is structural rather than a list of tag names that would go stale.
/// The known cost: a prompt that is one complete XML element and nothing else
/// is misread as context, as is a `<user_shell_command>` record of a command
/// the user ran through the agent.
fn is_injected_context(text: &str) -> bool {
    let text = text.trim();
    if text.starts_with("# AGENTS.md instructions") && text.ends_with("</INSTRUCTIONS>") {
        return true;
    }
    let Some(rest) = text.strip_prefix('<') else {
        return false;
    };
    let Some(end) = rest.find('>') else {
        return false;
    };
    let name = &rest[..end];
    if name.is_empty() || name.starts_with('/') {
        return false;
    }
    // A lone tag with nothing after it is a placeholder such as
    // `<no retained transcript delta entries>`.
    if rest[end + 1..].trim().is_empty() {
        return true;
    }
    text.ends_with(&format!("</{name}>"))
}

/// A rollout filename, split into the fields the name encodes:
/// `rollout-<YYYY-MM-DDThh-mm-ss>-<thread_id>.jsonl`, with `_<rollout_id>`
/// spliced in before the extension when an edited prompt continued the thread
/// in a new rollout. Mirrors Codex's own parser.
///
/// The timestamp digits and the UUIDv7 rollout id both order lexically, so
/// "which file is newest" is a plain string comparison.
pub(crate) struct RolloutFileName<'a> {
    pub(crate) thread_id: &'a str,
    pub(crate) timestamp: &'a str,
    pub(crate) rollout_id: &'a str,
}

impl<'a> RolloutFileName<'a> {
    pub(crate) fn parse(name: &'a str) -> Option<Self> {
        let core = name.strip_prefix("rollout-")?.strip_suffix(".jsonl")?;
        let timestamp = core.get(..19)?;
        if !is_rollout_timestamp(timestamp) || core.get(19..20)? != "-" {
            return None;
        }
        let ids = core.get(20..)?;
        let (thread_id, rollout_id) = ids.split_once('_').unwrap_or((ids, ids));
        (is_uuid(thread_id) && is_uuid(rollout_id)).then_some(Self {
            thread_id,
            timestamp,
            rollout_id,
        })
    }

    pub(crate) fn parse_path(path: &'a Path) -> Option<Self> {
        Self::parse(path.file_name()?.to_str()?)
    }

    /// Orders a thread's rollouts oldest first. Name timestamps carry seconds
    /// only; the UUIDv7 rollout id breaks the tie, the same way Codex picks the
    /// file it resumes.
    fn resume_order(&self) -> (&'a str, &'a str) {
        (self.timestamp, self.rollout_id)
    }
}

/// `YYYY-MM-DDThh-mm-ss`, the second-resolution stamp rollout names carry.
fn is_rollout_timestamp(text: &str) -> bool {
    let bytes = text.as_bytes();
    bytes.len() == 19
        && bytes.iter().enumerate().all(|(index, byte)| match index {
            4 | 7 | 13 | 16 => *byte == b'-',
            10 => *byte == b'T',
            _ => byte.is_ascii_digit(),
        })
}

fn is_uuid(text: &str) -> bool {
    let bytes = text.as_bytes();
    bytes.len() == 36
        && bytes.iter().enumerate().all(|(index, byte)| match index {
            8 | 13 | 18 | 23 => *byte == b'-',
            _ => byte.is_ascii_hexdigit(),
        })
}

/// A thread's newest rollout seen so far, with its
/// [`RolloutFileName::resume_order`].
struct NewestRollout<'a> {
    order: (&'a str, &'a str),
    path: &'a Path,
}

/// One file per thread: the newest, chosen the way Codex resolves a resume.
/// Earlier rollouts from an edited prompt are skipped; the reader reads them
/// through the newest one's `history_base`.
pub(crate) fn newest_rollouts_per_thread(files: &[PathBuf]) -> Vec<PathBuf> {
    let mut newest: HashMap<&str, NewestRollout<'_>> = HashMap::new();
    for path in files {
        let Some(name) = RolloutFileName::parse_path(path) else {
            continue;
        };
        let candidate = NewestRollout {
            order: name.resume_order(),
            path,
        };
        match newest.entry(name.thread_id) {
            Entry::Vacant(slot) => {
                slot.insert(candidate);
            }
            Entry::Occupied(mut kept) if candidate.order > kept.get().order => {
                kept.insert(candidate);
            }
            Entry::Occupied(_) => {}
        }
    }
    let mut files = newest
        .into_values()
        .map(|rollout| rollout.path.to_path_buf())
        .collect::<Vec<_>>();
    files.sort();
    files
}

/// Rollouts sit exactly this many directory levels below the `sessions` tree,
/// in a directory per day: `YYYY/MM/DD/rollout-….jsonl`.
pub(crate) const SESSIONS_TREE_DEPTH: usize = 3;

/// The `sessions` tree holding `path`, or `None` when the file sits outside one.
pub(crate) fn sessions_tree_of(path: &Path) -> Option<&Path> {
    path.ancestors()
        .find(|ancestor| ancestor.file_name() == Some(OsStr::new("sessions")))
}

/// `session_index.jsonl` sits beside the `sessions` tree in the Codex home.
pub(crate) fn index_beside_sessions_tree(sessions_tree: &Path) -> Option<PathBuf> {
    sessions_tree
        .parent()
        .map(|home| home.join("session_index.jsonl"))
}

/// The session index of the sessions tree holding `transcript`, or `None` when
/// the file sits outside one.
pub(crate) fn session_index_path(transcript: &Path) -> Option<PathBuf> {
    sessions_tree_of(transcript).and_then(index_beside_sessions_tree)
}

/// The thread's user-visible name. Index records are append-only and the last
/// record for an id wins, so renames never rewrite the rollout itself.
fn thread_title(transcript: &Path, thread_id: &str) -> Option<String> {
    let index = session_index_path(transcript)?;
    index_titles(&index).remove(thread_id)
}

/// Every thread name the session index records, cleaned of line breaks, the
/// newest record per id winning. A missing or unreadable index is an empty map.
pub(crate) fn index_titles(index: &Path) -> HashMap<String, String> {
    let Ok(contents) = std::fs::read_to_string(index) else {
        return HashMap::new();
    };
    let mut titles = HashMap::new();
    for line in contents.lines() {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let Some(id) = value.get("id").and_then(Value::as_str) else {
            continue;
        };
        let Some(name) = value.get("thread_name").and_then(Value::as_str) else {
            continue;
        };
        let name = name.replace(['\r', '\n'], " ").trim().to_owned();
        if name.is_empty() {
            // The newest record has no usable name, so the thread has none.
            titles.remove(id);
        } else {
            titles.insert(id.to_owned(), name);
        }
    }
    titles
}

/// Rollouts written by hand for the tests here and in the provider: the
/// checked-in sub-agent fixture carries fixed ids, and a test tree needs ids
/// of its own.
#[cfg(test)]
pub(crate) mod test_support {
    use serde_json::{Value, json};
    use std::path::{Path, PathBuf};

    pub(crate) const KEPT_SUBAGENT: &str = "019f0000-0000-7000-8000-0000000000b1";
    pub(crate) const DISCARDED_SUBAGENT: &str = "019f0000-0000-7000-8000-0000000000b2";
    pub(crate) const DISCARDED_BY_ID_SUBAGENT: &str = "019f0000-0000-7000-8000-0000000000b3";
    pub(crate) const NESTED_SUBAGENT: &str = "019f0000-0000-7000-8000-0000000000b4";
    pub(crate) const OUTSIDE_CHAIN_SUBAGENT: &str = "019f0000-0000-7000-8000-0000000000b5";
    const FIRST_EDIT: &str = "019f0000-0000-7000-8000-0000000000a1";
    const SECOND_EDIT: &str = "019f0000-0000-7000-8000-0000000000a2";
    const TIMESTAMP: &str = "2026-10-12T10:00:00.000Z";

    /// A thread edited twice, and the sub-agents its turns started. The turn
    /// before the first edit started `kept`. The first edit's discarded turn
    /// started `discarded`, named by agent path, and `discarded_by_id`, named
    /// by `agent_id`; `discarded` started `nested`. The second edit continues
    /// the root rollout too: the first edit's rollout, with the
    /// `outside_chain` sub-agent its turn started, is left outside the chain.
    /// Each sub-agent answers `<its thread ID> report`.
    pub(crate) struct EditedThreadWithSubagents {
        pub(crate) root: PathBuf,
        /// The root rollout's text up to the first edit.
        pub(crate) kept_text: String,
        pub(crate) first_edit: PathBuf,
        /// The rollout the second edit started.
        pub(crate) newest: PathBuf,
        pub(crate) kept: PathBuf,
        pub(crate) discarded: PathBuf,
        pub(crate) discarded_by_id: PathBuf,
        pub(crate) nested: PathBuf,
        pub(crate) outside_chain: PathBuf,
    }

    impl EditedThreadWithSubagents {
        pub(crate) fn subagents(&self) -> Vec<PathBuf> {
            vec![
                self.kept.clone(),
                self.discarded.clone(),
                self.discarded_by_id.clone(),
                self.nested.clone(),
                self.outside_chain.clone(),
            ]
        }
    }

    pub(crate) fn write_edited_thread_with_subagents(
        home: &Path,
        thread_id: &str,
    ) -> EditedThreadWithSubagents {
        let kept_text = header_line(thread_id, json!({}))
            + &message_line("user", "alpha")
            + &spawn_lines("call_kept", json!({"task_name": "/root/kept"}));
        let discarded_text = message_line("user", "bravo")
            + &spawn_lines("call_discarded", json!({"task_name": "/root/discarded"}))
            + &spawn_lines("call_by_id", json!({"agent_id": DISCARDED_BY_ID_SUBAGENT}));
        let root = write_rollout_text(
            home,
            "2026-10-12T10-00-00",
            thread_id,
            &(kept_text.clone() + &discarded_text),
        );
        let continuing_root = json!({"thread_id": thread_id, "end_byte_offset": kept_text.len()});
        let first_edit = write_rollout_text(
            home,
            "2026-10-12T10-02-00",
            &format!("{thread_id}_{FIRST_EDIT}"),
            &(header_line(
                thread_id,
                json!({"timestamp": "2026-10-12T10:02:00.000Z", "history_base": continuing_root}),
            ) + &message_line("user", "delta")
                + &spawn_lines("call_outside", json!({"task_name": "/root/outside"}))),
        );
        let newest = write_rollout_text(
            home,
            "2026-10-12T10-04-00",
            &format!("{thread_id}_{SECOND_EDIT}"),
            &(header_line(
                thread_id,
                json!({"timestamp": "2026-10-12T10:04:00.000Z", "history_base": continuing_root}),
            ) + &message_line("user", "echo")),
        );
        EditedThreadWithSubagents {
            root,
            kept_text,
            first_edit,
            newest,
            kept: write_subagent_answer(
                home,
                KEPT_SUBAGENT,
                thread_id,
                Some("/root/kept"),
                "2026-10-12T10:00:30.000Z",
            ),
            discarded: write_subagent_answer(
                home,
                DISCARDED_SUBAGENT,
                thread_id,
                Some("/root/discarded"),
                "2026-10-12T10:01:30.000Z",
            ),
            discarded_by_id: write_subagent_answer(
                home,
                DISCARDED_BY_ID_SUBAGENT,
                thread_id,
                None,
                "2026-10-12T10:01:31.000Z",
            ),
            nested: write_subagent_answer(
                home,
                NESTED_SUBAGENT,
                DISCARDED_SUBAGENT,
                Some("/root/discarded/nested"),
                "2026-10-12T10:01:40.000Z",
            ),
            outside_chain: write_subagent_answer(
                home,
                OUTSIDE_CHAIN_SUBAGENT,
                thread_id,
                Some("/root/outside"),
                "2026-10-12T10:02:30.000Z",
            ),
        }
    }

    /// Writes a rollout of the sub-agent `thread_id`, run under
    /// `parent_thread_id` at `agent_path` from `started`, that answers
    /// `<thread_id> report`.
    pub(crate) fn write_subagent_answer(
        home: &Path,
        thread_id: &str,
        parent_thread_id: &str,
        agent_path: Option<&str>,
        started: &str,
    ) -> PathBuf {
        let fields = json!({
            "timestamp": started,
            "parent_thread_id": parent_thread_id,
            "agent_path": agent_path,
        });
        write_rollout_text(
            home,
            "2026-10-12T10-01-00",
            thread_id,
            &(header_line(thread_id, fields)
                + &message_line("assistant", &format!("{thread_id} report"))),
        )
    }

    /// Writes `text` where Codex files a rollout: the day of `stamp`, then
    /// `rollout-<stamp>-<ids>.jsonl`.
    pub(crate) fn write_rollout_text(home: &Path, stamp: &str, ids: &str, text: &str) -> PathBuf {
        let day = home.join("sessions").join(stamp[..10].replace('-', "/"));
        std::fs::create_dir_all(&day).unwrap();
        let path = day.join(format!("rollout-{stamp}-{ids}.jsonl"));
        std::fs::write(&path, text).unwrap();
        path
    }

    /// A header line for `thread_id`, with `fields` added to its payload.
    fn header_line(thread_id: &str, fields: Value) -> String {
        let mut payload = json!({"id": thread_id, "timestamp": TIMESTAMP, "cwd": "/tmp/project"});
        payload
            .as_object_mut()
            .unwrap()
            .extend(fields.as_object().unwrap().clone());
        record_line(json!({"type": "session_meta", "payload": payload}))
    }

    fn message_line(role: &str, text: &str) -> String {
        let kind = if role == "user" {
            "input_text"
        } else {
            "output_text"
        };
        record_line(json!({
            "type": "response_item",
            "payload": {"type": "message", "role": role, "content": [{"type": kind, "text": text}]},
        }))
    }

    /// A `spawn_agent` call and its `result`.
    pub(crate) fn spawn_lines(call_id: &str, result: Value) -> String {
        let arguments = json!({"task_name": "task", "message": "go"}).to_string();
        record_line(json!({
            "type": "response_item",
            "payload": {"type": "function_call", "name": "spawn_agent", "call_id": call_id, "arguments": arguments},
        })) + &record_line(json!({
            "type": "response_item",
            "payload": {"type": "function_call_output", "call_id": call_id, "output": result.to_string()},
        }))
    }

    fn record_line(mut record: Value) -> String {
        record["timestamp"] = json!(TIMESTAMP);
        format!("{record}\n")
    }

    /// Writes a rollout of `thread_id` at `path` whose header names
    /// `parent_thread_id` as the thread it ran under, followed by one
    /// assistant message, `sub-agent answer visible`.
    pub(crate) fn write_subagent_rollout(path: &Path, thread_id: &str, parent_thread_id: &str) {
        write_rollout_with_source(path, thread_id, parent_thread_id, None);
    }

    /// [`write_subagent_rollout`] with the header's `source` set to a
    /// Guardian review of `parent_thread_id`, as Codex 0.150 writes it.
    pub(crate) fn write_guardian_rollout(path: &Path, thread_id: &str, parent_thread_id: &str) {
        write_rollout_with_source(
            path,
            thread_id,
            parent_thread_id,
            Some(r#"{"subagent":{"other":"guardian"}}"#),
        );
    }

    fn write_rollout_with_source(
        path: &Path,
        thread_id: &str,
        parent_thread_id: &str,
        source: Option<&str>,
    ) {
        let source = source
            .map(|source| format!(",\"source\":{source}"))
            .unwrap_or_default();
        std::fs::write(
            path,
            format!(
                concat!(
                    "{{\"timestamp\":\"2026-08-02T10:00:05.000Z\",\"type\":\"session_meta\",",
                    "\"payload\":{{\"id\":\"{id}\",\"timestamp\":\"2026-08-02T10:00:05.000Z\",",
                    "\"cwd\":\"/tmp/project\",\"parent_thread_id\":\"{parent}\"{source}}}}}\n",
                    "{{\"timestamp\":\"2026-08-02T10:00:06.000Z\",\"type\":\"response_item\",",
                    "\"payload\":{{\"type\":\"message\",\"role\":\"assistant\",",
                    "\"content\":[{{\"type\":\"output_text\",\"text\":\"sub-agent answer visible\"}}]}}}}\n",
                ),
                id = thread_id,
                parent = parent_thread_id,
                source = source,
            ),
        )
        .unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::write_rollout_text;
    use super::*;
    use crate::history::format::at_own_lines;
    use crate::history::format::dropped::DropOutcome;
    use crate::history::parser::process_conversation_file;

    /// A rollout from `reader` alone, its lines past the header parsed on the
    /// rayon pool for a batch `parallel` admits. `None` when the first line is
    /// not a rollout header.
    fn parse_reader(
        mut reader: impl BufRead,
        parallel: ParallelThreshold,
        dropped: DroppedRecords,
    ) -> Result<Option<SessionProjection>> {
        let Some((header, first_record_line)) = read_header(&mut reader)? else {
            return Ok(None);
        };
        let mut thread = ThreadProjection::new(&header, dropped);
        thread.read_records(&header, None, reader, first_record_line, parallel)?;
        Ok(Some(thread.finish(header).projection))
    }

    #[test]
    fn counting_names_skipped_and_unrecognised_line_payload_and_event_types() {
        let rollout = [
            r#"{"timestamp":"2026-10-01T00:00:00.000Z","type":"session_meta","payload":{"id":"019a0000-0000-7000-8000-000000000001","timestamp":"2026-10-01T00:00:00.000Z","cwd":"/tmp","cli_version":"0.160.0"}}"#,
            r#"{"timestamp":"2026-10-01T00:00:01.000Z","type":"teleport","payload":{}}"#,
            r#"{"timestamp":"2026-10-01T00:00:02.000Z","type":"response_item","payload":{"type":"hologram"}}"#,
            r#"{"timestamp":"2026-10-01T00:00:03.000Z","type":"world_state","payload":{}}"#,
            r#"{"timestamp":"2026-10-01T00:00:04.000Z","type":"event_msg","payload":{"type":"agent_message"}}"#,
            r#"{"timestamp":"2026-10-01T00:00:05.000Z","type":"event_msg","payload":{"type":"task_started"}}"#,
            r#"{"timestamp":"2026-10-01T00:00:06.000Z","type":"event_msg","payload":{"type":"thread_name_updated"}}"#,
        ]
        .join("\n");

        let projection = parse_reader(
            std::io::Cursor::new(rollout.as_bytes()),
            ParallelThreshold::NEVER,
            DroppedRecords::counting(),
        )
        .unwrap()
        .expect("a rollout");

        let version = Some("0.160.0");
        assert_eq!(
            projection.dropped.counts().collect::<Vec<_>>(),
            [
                (
                    version,
                    "event_msg.type=agent_message",
                    DropOutcome::Skipped,
                    1
                ),
                (
                    version,
                    "event_msg.type=task_started",
                    DropOutcome::Skipped,
                    1
                ),
                (
                    version,
                    "event_msg.type=thread_name_updated",
                    DropOutcome::Unrecognised,
                    1
                ),
                (
                    version,
                    "response_item.type=hologram",
                    DropOutcome::Unrecognised,
                    1
                ),
                (version, "type=teleport", DropOutcome::Unrecognised, 1),
                (version, "type=world_state", DropOutcome::Skipped, 1),
            ]
        );
    }

    const TURN_RECORDS_THREAD: &str = "019a0000-0000-7000-8000-000000000001";

    /// A rollout of `records`, each a line type and its payload, one second
    /// apart after the header.
    fn rollout_text(records: &[(&str, Value)]) -> String {
        let header = json!({
            "timestamp": "2026-10-01T00:00:00.000Z",
            "type": "session_meta",
            "payload": {"id": TURN_RECORDS_THREAD, "timestamp": "2026-10-01T00:00:00.000Z", "cwd": "/tmp", "cli_version": "0.162.1"},
        });
        let lines = records.iter().enumerate().map(|(index, (kind, payload))| {
            json!({
                "timestamp": format!("2026-10-01T00:00:{:02}.000Z", index + 1),
                "type": kind,
                "payload": payload,
            })
        });
        std::iter::once(header)
            .chain(lines)
            .map(|record| format!("{record}\n"))
            .collect()
    }

    fn projected(records: &[(&str, Value)]) -> SessionProjection {
        parse_reader(
            std::io::Cursor::new(rollout_text(records).into_bytes()),
            ParallelThreshold::NEVER,
            DroppedRecords::counting(),
        )
        .unwrap()
        .expect("a rollout")
    }

    /// The label and text of each `searchable` metadata row.
    fn shown_rows(projection: &SessionProjection) -> Vec<(&str, &str)> {
        projection
            .entries
            .iter()
            .filter_map(|(_, entry)| match entry {
                LogEntry::PiMetadata {
                    label,
                    text,
                    searchable: true,
                    ..
                } => Some((label.as_str(), text.as_str())),
                _ => None,
            })
            .collect()
    }

    fn turn_aborted_record(reason: &str) -> (&'static str, Value) {
        let payload = json!({"type": "turn_aborted", "turn_id": "turn", "reason": reason});
        ("event_msg", payload)
    }

    #[test]
    fn interrupted_turn_or_budget_stop_shows_notice_and_other_reasons_dont() {
        let projection = projected(&[
            turn_aborted_record("interrupted"),
            turn_aborted_record("replaced"),
            turn_aborted_record("review_ended"),
            turn_aborted_record("budget_limited"),
            turn_aborted_record("teleported"),
        ]);

        assert_eq!(
            shown_rows(&projection),
            [
                ("Notice", "Conversation interrupted"),
                ("Notice", "Goal budget reached"),
            ]
        );
        assert_eq!(
            projection.dropped.counts().collect::<Vec<_>>(),
            [(
                Some("0.162.1"),
                "turn_aborted.reason=teleported",
                DropOutcome::Unrecognised,
                1
            )]
        );
    }

    fn goal_update_record(
        created_at: i64,
        status: &str,
        tokens_used: i64,
    ) -> (&'static str, Value) {
        let goal = json!({
            "threadId": TURN_RECORDS_THREAD,
            "objective": format!("objective {created_at}"),
            "status": status,
            "tokensUsed": tokens_used,
            "timeUsedSeconds": tokens_used / 100,
            "createdAt": created_at,
            "updatedAt": created_at + tokens_used,
        });
        let payload =
            json!({"type": "thread_goal_updated", "threadId": TURN_RECORDS_THREAD, "goal": goal});
        ("event_msg", payload)
    }

    #[test]
    fn goal_shows_row_when_it_is_set_or_its_status_changes() {
        let projection = projected(&[
            goal_update_record(1, "active", 0),
            goal_update_record(1, "active", 500),
            goal_update_record(1, "paused", 900),
            goal_update_record(2, "active", 0),
            goal_update_record(2, "blocked", 300),
        ]);

        assert_eq!(
            shown_rows(&projection),
            [
                ("Goal", "objective 1"),
                ("Goal", "paused"),
                ("Goal", "objective 2"),
                ("Goal", "stalled"),
            ]
        );
    }

    #[test]
    fn each_web_search_action_reads_as_web_search_call() {
        let web_search = |id: &str, action: Value| {
            let payload = json!({"type": "web_search_call", "id": id, "status": "completed", "action": action});
            ("response_item", payload)
        };
        let projection = projected(&[
            web_search(
                "ws_1",
                json!({"type": "search", "query": "rust regex", "queries": ["rust regex"]}),
            ),
            web_search(
                "ws_2",
                json!({"type": "search", "queries": ["serde", "toml"]}),
            ),
            web_search(
                "ws_3",
                json!({"type": "open_page", "url": "https://example.com/docs"}),
            ),
            web_search(
                "ws_4",
                json!({"type": "find_in_page", "url": "https://example.com/docs", "pattern": "needle"}),
            ),
            web_search("ws_5", Value::Null),
        ]);

        let calls: Vec<_> = projection
            .entries
            .iter()
            .flat_map(|(_, entry)| match entry {
                LogEntry::Assistant { message, .. } => message.content.clone(),
                _ => Vec::new(),
            })
            .map(|block| match block {
                ContentBlock::ToolUse {
                    id,
                    name,
                    tool,
                    input,
                } => (id, name, tool, input),
                other => panic!("a web search reads as a call, not {other:?}"),
            })
            .collect();
        let call = |id: &str, tool: Tool, input: Value| {
            (id.to_owned(), "web_search".to_owned(), tool, input)
        };
        assert_eq!(
            calls,
            [
                call("ws_1", Tool::WebSearch, json!({"query": "rust regex"})),
                call("ws_2", Tool::WebSearch, json!({"query": "serde, toml"})),
                call(
                    "ws_3",
                    Tool::WebFetch,
                    json!({"url": "https://example.com/docs"})
                ),
                call(
                    "ws_4",
                    Tool::WebSearch,
                    json!({"query": "'needle' in https://example.com/docs"})
                ),
                call("ws_5", Tool::WebSearch, json!({"query": ""})),
            ]
        );
    }

    fn fixture(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/codex")
            .join(name)
    }

    fn entry_json(projection: &SessionProjection) -> String {
        projection
            .entries
            .iter()
            .map(|(_, entry)| serde_json::to_string(entry).unwrap())
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// A sub-agent's final answer, trimmed from a real rollout; the report is
    /// a neutral value of the same shape.
    const FINAL_ANSWER: &str = r#"{"timestamp":"2026-08-01T02:01:16.169Z","type":"response_item","payload":{"type":"agent_message","id":"amsg_019fbb0d-0000-7000-8000-000000000001","author":"/root/state_ledger","recipient":"/root","content":[{"type":"input_text","text":"Message Type: FINAL_ANSWER\nTask name: /root\nSender: /root/state_ledger\nPayload:\nSearch ledger, read-only.\n\n| Search kind | Count |\n|---|---|\n| Direct | 5 |"}]}}"#;

    fn normalized(line: &str) -> Option<LogEntry> {
        let record: Value = serde_json::from_str(line).unwrap();
        normalize_line(
            record.as_object().unwrap(),
            &mut LastShown::default(),
            &mut DroppedRecords::default(),
        )
    }

    #[test]
    fn sub_agents_final_answer_reads_as_its_report_under_its_task_name() {
        let Some(LogEntry::User { message, .. }) = normalized(FINAL_ANSWER) else {
            panic!("a final answer reads as a user entry");
        };

        let report = crate::history::user_task_report(&message.content).expect("a task report");
        assert_eq!(
            report.summary,
            "Agent \"state_ledger\" handed back its report"
        );
        assert_eq!(
            report.body.as_deref(),
            Some("Search ledger, read-only.\n\n| Search kind | Count |\n|---|---|\n| Direct | 5 |")
        );
    }

    #[test]
    fn sub_agents_report_names_its_agent_path_as_its_sender() {
        let Some(LogEntry::User { message, .. }) = normalized(FINAL_ANSWER) else {
            panic!("a final answer reads as a user entry");
        };

        let report = crate::history::user_task_report(&message.content).expect("a task report");
        assert_eq!(report.sender.task_id.as_deref(), Some("/root/state_ledger"));
    }

    fn assistant_with(content: Vec<ContentBlock>, timestamp: &str) -> LogEntry {
        LogEntry::Assistant {
            agent: Some("Codex".to_owned()),
            message: AssistantMessage {
                role: "assistant".to_owned(),
                content,
                model: None,
                usage: None,
                id: None,
            },
            timestamp: Some(timestamp.to_owned()),
            uuid: None,
            parent_tool_use_id: None,
        }
    }

    fn user_with(content: UserContent) -> LogEntry {
        LogEntry::User {
            message: UserMessage {
                role: "user".to_owned(),
                content,
            },
            timestamp: None,
            uuid: None,
            cwd: None,
            parent_tool_use_id: None,
            source_tool_use_id: None,
            usage: None,
        }
    }

    fn text(text: &str) -> ContentBlock {
        ContentBlock::Text {
            text: text.to_owned(),
        }
    }

    /// The session spawns `/root/scout` and receives its report; the
    /// sub-agent's thread holds a reply and the report it handed back.
    #[test]
    fn sub_agent_thread_links_to_its_spawn_and_folds_report_it_handed_back() {
        let report = TaskReport::handed_back(Some("scout"), "The notes list the steps.").sent_by(
            ReportSender {
                task_id: Some("/root/scout".to_owned()),
                launch_call: None,
            },
        );
        let session = at_own_lines(vec![
            (
                1,
                assistant_with(
                    vec![ContentBlock::ToolUse {
                        id: "call_spawn".to_owned(),
                        name: SPAWN_AGENT.to_owned(),
                        tool: Tool::Agent,
                        input: json!({"description": "scout"}),
                    }],
                    "2026-10-14T15:41:25Z",
                ),
            ),
            (
                2,
                user_with(UserContent::Blocks(vec![ContentBlock::ToolResult {
                    tool_use_id: "call_spawn".to_owned(),
                    content: Some(json!(r#"{"task_name":"/root/scout"}"#)),
                    standalone_tool_name: None,
                }])),
            ),
            (
                3,
                user_with(UserContent::String(report.notification_text())),
            ),
        ]);
        let reply = "Reading the notes.";
        let mut threads = vec![SubagentThread {
            label: SUBAGENT_THREAD.to_owned(),
            transcript: Arc::from(Path::new("scout.jsonl")),
            identity: SubagentIdentity {
                agent_path: Some("/root/scout".to_owned()),
                ..SubagentIdentity::default()
            },
            launch_call: None,
            started: "2026-10-14T15:41:26Z".to_owned(),
            entries: at_own_lines(vec![
                (1, assistant_with(vec![text(reply)], "2026-10-14T15:41:30Z")),
                (
                    2,
                    assistant_with(
                        vec![text("The notes list the steps.\n")],
                        "2026-10-14T15:41:40Z",
                    ),
                ),
            ]),
        }];

        link_subagent_threads(&session, &mut threads);

        let thread = &threads[0];
        assert_eq!(thread.launch_call.as_deref(), Some("call_spawn"));
        let contents = thread
            .entries
            .iter()
            .map(|(_, entry)| match entry {
                LogEntry::Assistant { message, .. } => message.content.clone(),
                other => panic!("{other:?}"),
            })
            .collect::<Vec<_>>();
        assert!(
            matches!(&contents[0][..], [ContentBlock::Text { text }] if text == reply),
            "{:?}",
            contents[0]
        );
        assert!(
            matches!(
                &contents[1][..],
                [ContentBlock::ToolUse { name, tool: Tool::AgentReport, input, .. }]
                    if name == FINAL_ANSWER_CALL && input == &json!({ DELIVERED_REPORT_KEY: true })
            ),
            "{:?}",
            contents[1]
        );
    }

    /// The `spawn_agent` call `id`, made at `made`, at `line`, and its result
    /// naming `/root/scout` on the next line.
    fn scout_spawn(line: usize, id: &str, made: &str) -> [(usize, LogEntry); 2] {
        [
            (
                line,
                assistant_with(
                    vec![ContentBlock::ToolUse {
                        id: id.to_owned(),
                        name: SPAWN_AGENT.to_owned(),
                        tool: Tool::Agent,
                        input: json!({"description": "scout"}),
                    }],
                    made,
                ),
            ),
            (
                line + 1,
                user_with(UserContent::Blocks(vec![ContentBlock::ToolResult {
                    tool_use_id: id.to_owned(),
                    content: Some(json!(r#"{"task_name":"/root/scout"}"#)),
                    standalone_tool_name: None,
                }])),
            ),
        ]
    }

    /// The scout's report "Done", as the session receives it at `received`.
    fn done_report_received(received: &str) -> LogEntry {
        let report = TaskReport::handed_back(Some("scout"), "Done").sent_by(ReportSender {
            task_id: Some("/root/scout".to_owned()),
            launch_call: None,
        });
        let mut entry = user_with(UserContent::String(report.notification_text()));
        if let LogEntry::User { timestamp, .. } = &mut entry {
            *timestamp = Some(received.to_owned());
        }
        entry
    }

    /// A `/root/scout` run labelled `label`, started at `started`, replying
    /// "Done" at each of `replied`.
    fn scout_run(label: &str, started: &str, replied: &[&str]) -> SubagentThread {
        let replies = replied
            .iter()
            .enumerate()
            .map(|(index, written)| (index + 1, assistant_with(vec![text("Done")], written)))
            .collect();
        SubagentThread {
            label: label.to_owned(),
            transcript: Arc::from(Path::new(&format!("{label}.jsonl"))),
            identity: SubagentIdentity {
                agent_path: Some("/root/scout".to_owned()),
                ..SubagentIdentity::default()
            },
            launch_call: None,
            started: started.to_owned(),
            entries: at_own_lines(replies),
        }
    }

    /// For each of `thread`'s replies, true when it folded into a delivered
    /// `final_answer` call.
    fn folded_replies(thread: &SubagentThread) -> Vec<bool> {
        thread
            .entries
            .iter()
            .map(|(_, entry)| {
                matches!(entry, LogEntry::Assistant { message, .. }
                    if matches!(&message.content[..], [ContentBlock::ToolUse { name, .. }] if name == FINAL_ANSWER_CALL))
            })
            .collect()
    }

    /// The times of a second `/root/scout` run: its spawn, its start, its
    /// reply "Done", and the session's receipt of the report "Done".
    struct SecondScoutRun<'a> {
        spawned: &'a str,
        started: &'a str,
        replied: &'a str,
        report_received: &'a str,
    }

    /// Each run's launching call, and whether its reply folded, for a first
    /// run spawned at 15:00:00, started at 15:00:01 and replying at
    /// 15:00:50, and the `second` run.
    fn link_two_scout_runs(second: SecondScoutRun<'_>) -> (Vec<Option<String>>, Vec<bool>) {
        let mut session = scout_spawn(1, "call_first", "2026-10-14T15:00:00Z").to_vec();
        session.extend(scout_spawn(3, "call_second", second.spawned));
        session.push((5, done_report_received(second.report_received)));
        let mut threads = vec![
            scout_run("first", "2026-10-14T15:00:01Z", &["2026-10-14T15:00:50Z"]),
            scout_run("second", second.started, &[second.replied]),
        ];

        link_subagent_threads(&at_own_lines(session), &mut threads);

        let launch_calls = threads
            .iter()
            .map(|thread| thread.launch_call.clone())
            .collect();
        let folded = threads.iter().flat_map(folded_replies).collect();
        (launch_calls, folded)
    }

    fn first_and_second_launch_calls() -> Vec<Option<String>> {
        vec![
            Some("call_first".to_owned()),
            Some("call_second".to_owned()),
        ]
    }

    /// The session receives a report from the first run alone; each run
    /// replies "Done".
    #[test]
    fn later_run_of_task_path_doesnt_fold_reply_matching_earlier_runs_report() {
        let (launch_calls, folded) = link_two_scout_runs(SecondScoutRun {
            spawned: "2026-10-14T15:02:00Z",
            started: "2026-10-14T15:02:01Z",
            replied: "2026-10-14T15:02:30Z",
            report_received: "2026-10-14T15:01:00Z",
        });

        assert_eq!(launch_calls, first_and_second_launch_calls());
        assert_eq!(folded, [true, false]);
    }

    /// The second spawn is made at a coarser precision than the second run's
    /// start, then at the run's start instant written at +01:00.
    #[test]
    fn runs_launch_is_last_spawn_made_before_it_started_whatever_precision_or_offset() {
        for (spawned, started) in [
            ("2026-10-14T15:04:00Z", "2026-10-14T15:04:00.250Z"),
            ("2026-10-14T16:04:01+01:00", "2026-10-14T15:04:01Z"),
        ] {
            let (launch_calls, _) = link_two_scout_runs(SecondScoutRun {
                spawned,
                started,
                replied: "2026-10-14T15:04:30Z",
                report_received: "2026-10-14T15:01:00Z",
            });

            assert_eq!(
                launch_calls,
                first_and_second_launch_calls(),
                "{spawned}, {started}"
            );
        }
    }

    /// The second run starts half a second after the report arrives, written
    /// at a finer precision: the report is the first run's. Then the second
    /// run starts at 15:04:30 UTC, written at -01:00, after the first run and
    /// before the report: the report is the second run's.
    #[test]
    fn report_answers_run_started_last_before_it_whatever_precision_or_offset() {
        let (_, after_the_report) = link_two_scout_runs(SecondScoutRun {
            spawned: "2026-10-14T15:04:00Z",
            started: "2026-10-14T15:05:00.500Z",
            replied: "2026-10-14T15:05:00.700Z",
            report_received: "2026-10-14T15:05:00Z",
        });
        let (_, before_the_report) = link_two_scout_runs(SecondScoutRun {
            spawned: "2026-10-14T15:04:00Z",
            started: "2026-10-14T14:04:30-01:00",
            replied: "2026-10-14T15:04:50Z",
            report_received: "2026-10-14T15:05:00Z",
        });

        assert_eq!(after_the_report, [true, false]);
        assert_eq!(before_the_report, [false, true]);
    }

    /// For each reply "Done" of a `/root/scout` run, written at one of
    /// `replied`, true when it folds into a delivered `final_answer` call.
    /// The session receives the scout's report "Done" at 15:01:00.
    fn folded_done_replies(replied: &[&str]) -> Vec<bool> {
        let mut session = scout_spawn(1, "call_spawn", "2026-10-14T15:00:00Z").to_vec();
        session.push((3, done_report_received("2026-10-14T15:01:00Z")));
        let mut threads = vec![scout_run("scout", "2026-10-14T15:00:01Z", replied)];

        link_subagent_threads(&at_own_lines(session), &mut threads);

        folded_replies(&threads[0])
    }

    #[test]
    fn report_folds_only_last_matching_reply_written_before_it() {
        let folded = folded_done_replies(&["2026-10-14T15:00:10Z", "2026-10-14T15:00:50Z"]);

        assert_eq!(folded, [false, true]);
    }

    #[test]
    fn reply_matching_report_written_after_it_doesnt_fold() {
        let folded = folded_done_replies(&["2026-10-14T15:00:50Z", "2026-10-14T15:01:30Z"]);

        assert_eq!(folded, [true, false]);
    }

    /// A reply written half a second after the report, at a finer
    /// precision, then a reply written at the report's instant at +01:00.
    #[test]
    fn report_folds_last_reply_written_before_it_whatever_precision_or_offset() {
        let finer_precision =
            folded_done_replies(&["2026-10-14T15:00:30Z", "2026-10-14T15:01:00.500Z"]);
        let other_offset =
            folded_done_replies(&["2026-10-14T15:00:30Z", "2026-10-14T16:01:00+01:00"]);

        assert_eq!(finer_precision, [true, false]);
        assert_eq!(other_offset, [false, true]);
    }

    #[test]
    fn message_between_agents_that_is_not_final_answer_stays_call() {
        let message = FINAL_ANSWER.replace("Message Type: FINAL_ANSWER", "Message Type: MESSAGE");

        let Some(LogEntry::Assistant { message, .. }) = normalized(&message) else {
            panic!("any other message reads as an assistant call");
        };
        assert!(
            matches!(&message.content[..], [ContentBlock::ToolUse { name, .. }] if name == "inter_agent_message"),
            "{:?}",
            message.content
        );
    }

    #[test]
    fn projects_dialogue_and_drops_model_context() {
        let projection = CODEX_ROLLOUT
            .parse_transcript(&fixture("rollout.jsonl"))
            .unwrap()
            .unwrap();

        assert_eq!(projection.source, Source::Codex);
        assert_eq!(projection.header.id, "019f0000-0000-7000-8000-00000000000a");
        assert_eq!(projection.header.cwd, PathBuf::from("/tmp/project"));
        assert_eq!(projection.title, None);
        assert_eq!(projection.malformed_lines, vec![17]);

        let json = entry_json(&projection);
        for kept in [
            "active codex question",
            "codex answer searchable",
            "reasoning summary visible",
            "tool output searchable",
            "function output searchable",
            "TOOL_INPUT_VISIBLE",
            "INTER_AGENT_TASK_TEXT",
            "compaction summary text",
            "gpt-5.2-test",
        ] {
            assert!(json.contains(kept), "missing {kept:?}");
        }
        for dropped in [
            "BASE_INSTRUCTIONS_SENTINEL",
            "ENV_CONTEXT_SENTINEL",
            "AGENTS_MD_SENTINEL",
            "no retained transcript",
            "DEVELOPER_SENTINEL",
            "ENCRYPTED_SENTINEL",
            "EVENT_DUPLICATE_SENTINEL",
            "REPLACEMENT_SENTINEL",
            "WORLD_STATE_SENTINEL",
        ] {
            assert!(!json.contains(dropped), "leaked {dropped:?}");
        }
    }

    #[test]
    fn extracts_codex_conversation_metadata() {
        let conversation = process_conversation_file(fixture("rollout.jsonl"), None, None)
            .unwrap()
            .unwrap();

        assert_eq!(conversation.source, Source::Codex);
        assert_eq!(
            conversation.session_id,
            "019f0000-0000-7000-8000-00000000000a"
        );
        assert_eq!(conversation.model.as_deref(), Some("gpt-5.2-test"));
        // Two token_count events: (1100-1000)+1000+7+40 and 200+0+0+30.
        assert_eq!(conversation.total_tokens, 1377);
        assert_eq!(conversation.duration_minutes, Some(2));
        assert!(conversation.preview.starts_with("active codex question"));
        for searchable in ["active codex question", "tool output searchable"] {
            assert!(
                conversation.full_text.contains(searchable),
                "missing {searchable:?} from {:?}",
                conversation.full_text
            );
        }
        for hidden in [
            "compaction summary text",
            "INTER_AGENT_TASK_TEXT",
            "reasoning summary visible",
            "ENV_CONTEXT_SENTINEL",
        ] {
            assert!(
                !conversation.full_text.contains(hidden),
                "{hidden:?} does not belong in the search index"
            );
        }
    }

    /// The inherited prefix is the parent's history; indexing it would count
    /// the parent's text and tokens twice after the thread folds into it.
    #[test]
    fn sub_agent_rollout_keeps_only_its_own_history() {
        let projection = CODEX_ROLLOUT
            .parse_transcript(&fixture("subagent.jsonl"))
            .unwrap()
            .unwrap();

        assert_eq!(projection.header.id, "019f0000-0000-7000-8000-00000000000b");
        let json = entry_json(&projection);
        assert!(json.contains("child task question"));
        assert!(json.contains("child answer searchable"));
        assert!(!json.contains("INHERITED_PARENT_SENTINEL"));
        assert!(!json.contains("INHERITED_ANSWER_SENTINEL"));
        assert!(!json.contains("DEVELOPER_TASK_SENTINEL"));
        assert_eq!(
            json.matches("gpt-5.2-test").count(),
            1,
            "the copied context named the same model; the thread's own first turn_context is still a model change"
        );
    }

    /// Codex's legacy-to-paginated migration sets the boundary past the last
    /// record of every sub-agent rollout it rewrites; nothing follows it, so
    /// the whole file is the thread's own history, usage included.
    #[test]
    fn migrated_sub_agent_rollout_keeps_its_whole_history() {
        let projection = CODEX_ROLLOUT
            .parse_transcript(&fixture("subagent-migrated.jsonl"))
            .unwrap()
            .unwrap();

        assert_eq!(projection.header.id, MIGRATED_SUBAGENT_THREAD);
        let json = entry_json(&projection);
        assert!(json.contains("migrated task question"));
        assert!(json.contains("migrated answer searchable"));
        assert!(json.contains("\"input_tokens\":300"));
        assert!(!json.contains("PAGINATED_EVENT_SENTINEL"));
    }

    /// A sub-agent rollout of `records` messages after its header, whose own
    /// history starts at ordinal `boundary`, with a blank line every 37th line
    /// and a malformed one every 53rd.
    fn long_subagent_rollout(records: u64, boundary: u64) -> String {
        let header = json!({
            "timestamp": "2026-08-02T10:00:00.000Z",
            "ordinal": 0,
            "type": "session_meta",
            "payload": {
                "id": SUBAGENT_THREAD,
                "parent_thread_id": PARENT_THREAD,
                "timestamp": "2026-08-02T10:00:00.000Z",
                "cwd": "/tmp/project",
                "subagent_history_start_ordinal": boundary,
            },
        });
        let mut text = format!("{header}\n");
        for ordinal in 1..=records {
            let (role, kind) = if ordinal % 2 == 0 {
                ("assistant", "output_text")
            } else {
                ("user", "input_text")
            };
            let record = json!({
                "timestamp": "2026-08-02T10:00:01.000Z",
                "ordinal": ordinal,
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": role,
                    "content": [{"type": kind, "text": format!("record-{ordinal}-end")}],
                },
            });
            text.push_str(&format!("{record}\n"));
            if ordinal % 37 == 0 {
                text.push('\n');
            }
            if ordinal % 53 == 0 {
                text.push_str("{\"type\": truncated\n");
            }
        }
        text
    }

    #[test]
    fn rollout_parses_same_on_rayon_pool_as_on_one_thread() {
        let rollout = long_subagent_rollout(400, 150);
        let parsed = |parallel| {
            let projection = parse_reader(
                std::io::Cursor::new(rollout.as_bytes()),
                parallel,
                DroppedRecords::counting(),
            )
            .unwrap()
            .expect("a rollout");
            format!("{projection:?}")
        };

        let on_the_pool = parsed(ParallelThreshold::ALWAYS);

        assert_eq!(on_the_pool, parsed(ParallelThreshold::NEVER));
        assert!(on_the_pool.contains("record-400-end"));
        assert!(
            !on_the_pool.contains("record-149-end"),
            "the records below the boundary are the parent's"
        );
    }

    const PARENT_THREAD: &str = "019f0000-0000-7000-8000-00000000000a";
    const SUBAGENT_THREAD: &str = "019f0000-0000-7000-8000-00000000000b";
    const MIGRATED_SUBAGENT_THREAD: &str = "019f0000-0000-7000-8000-00000000000e";

    fn kind_of(name: &str) -> ThreadKind {
        thread_kind_of(&fixture(name)).unwrap()
    }

    #[test]
    fn header_naming_no_parent_records_session() {
        assert_eq!(kind_of("rollout.jsonl"), ThreadKind::Session);
    }

    /// The checked-in sub-agent fixtures predate the `source` field; the
    /// parent link alone makes them sub-agents.
    #[test]
    fn header_naming_parent_records_sub_agent() {
        for name in ["subagent.jsonl", "subagent-migrated.jsonl"] {
            assert_eq!(
                kind_of(name),
                ThreadKind::Subagent {
                    parent_thread_id: PARENT_THREAD.to_owned()
                },
                "{name}"
            );
        }
    }

    /// A Guardian review names the thread it reviewed as its parent, and its
    /// transcript restates that thread's conversation. Reading it as a
    /// sub-agent would count the parent's text and tokens twice.
    #[test]
    fn guardian_review_header_is_skipped() {
        assert_eq!(
            kind_of("guardian.jsonl"),
            ThreadKind::Skipped {
                parent_thread_id: Some(PARENT_THREAD.to_owned())
            }
        );
    }

    #[test]
    fn every_source_codex_runs_for_itself_is_skipped() {
        let kind = |source: Value| {
            let mut payload = json!({
                "id": SUBAGENT_THREAD,
                "timestamp": "2026-08-02T10:00:00.000Z",
                "cwd": "/tmp/project",
                "parent_thread_id": PARENT_THREAD,
            });
            payload["source"] = source;
            thread_kind(payload.as_object().unwrap())
        };

        for skipped in [
            json!({"subagent": {"other": "guardian"}}),
            json!({"subagent": "review"}),
            json!({"subagent": "compact"}),
            json!({"subagent": "memory_consolidation"}),
            json!({"internal": "guardian"}),
        ] {
            assert_eq!(
                kind(skipped.clone()),
                ThreadKind::Skipped {
                    parent_thread_id: Some(PARENT_THREAD.to_owned())
                },
                "{skipped}"
            );
        }
        for subagent_source in [
            json!({"subagent": {"thread_spawn": {"parent_thread_id": PARENT_THREAD, "depth": 1}}}),
            json!({"subagent": "thread_spawn"}),
        ] {
            assert_eq!(
                kind(subagent_source.clone()),
                ThreadKind::Subagent {
                    parent_thread_id: PARENT_THREAD.to_owned()
                },
                "{subagent_source}"
            );
        }
        assert_eq!(
            kind(json!("cli")),
            ThreadKind::Subagent {
                parent_thread_id: PARENT_THREAD.to_owned()
            },
            "an interactive source with a parent link is still a sub-agent"
        );
    }

    /// The state database records a sub-agent's parent as an edge, not in
    /// its `source`; a `thread_spawn` row no edge names is a session.
    #[test]
    fn source_with_no_parent_recorded_is_session_unless_skipped() {
        let source = json!({"subagent": {"thread_spawn": {"parent_thread_id": PARENT_THREAD}}});
        assert_eq!(
            thread_kind_of_source(Some(&source), None),
            ThreadKind::Session
        );
        assert_eq!(
            thread_kind_of_source(Some(&json!({"subagent": {"other": "guardian"}})), None),
            ThreadKind::Skipped {
                parent_thread_id: None
            }
        );
        assert_eq!(thread_kind_of_source(None, None), ThreadKind::Session);
    }

    #[test]
    fn file_that_is_not_rollout_has_no_thread_kind() {
        let pi =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/pi/v3-branched.jsonl");
        assert!(thread_kind_of(&pi).is_none());
        assert!(thread_kind_of(Path::new("/absent/rollout.jsonl")).is_none());
    }

    /// The parent rollout and the sub-agent fixture, copied into a tree.
    fn family_tree(home: &Path) -> (PathBuf, PathBuf) {
        let day = home.join("sessions/2026/08/01");
        std::fs::create_dir_all(&day).unwrap();
        let parent = day.join(format!("rollout-2026-08-01T10-00-00-{PARENT_THREAD}.jsonl"));
        std::fs::copy(fixture("rollout.jsonl"), &parent).unwrap();
        let subagent = day.join(format!(
            "rollout-2026-08-02T10-00-00-{SUBAGENT_THREAD}.jsonl"
        ));
        std::fs::copy(fixture("subagent.jsonl"), &subagent).unwrap();
        (parent, subagent)
    }

    #[test]
    fn view_splices_sub_agent_threads_in_as_progress_entries() {
        let home = tempfile::tempdir().unwrap();
        let (parent, subagent) = family_tree(home.path());

        let indexed = CODEX_ROLLOUT.parse_transcript(&parent).unwrap().unwrap();
        assert!(
            !entry_json(&indexed).contains("child answer searchable"),
            "the plain parse must not splice; the row merges whole threads instead"
        );

        let view = super::super::view_projection(&CODEX_ROLLOUT, &parent, &[subagent])
            .unwrap()
            .unwrap();
        let json = entry_json(&view);
        assert!(json.contains("child task question"));
        assert!(json.contains("child answer searchable"));
        assert!(
            !json.contains("INHERITED_PARENT_SENTINEL"),
            "a thread's inherited prefix is the parent's own history"
        );

        let position_of = |text: &str| {
            view.entries
                .iter()
                .position(|(_, entry)| serde_json::to_string(entry).unwrap().contains(text))
                .unwrap()
        };
        let (_, spliced) = &view.entries[position_of("child answer searchable")];
        let LogEntry::Progress { data, .. } = spliced else {
            panic!("a child entry splices in as a progress entry: {spliced:?}");
        };
        let progress = crate::log_entry::parse_agent_progress(data)
            .expect("a spliced entry is shaped as Claude's agent_progress");
        assert_eq!(progress.agent_id, SUBAGENT_THREAD);
        assert!(
            position_of("child answer searchable") < position_of("codex answer searchable"),
            "the child's turns, timed before the parent's answer, splice in before it"
        );
    }

    #[test]
    fn migrated_sub_agent_thread_splices_into_view() {
        let home = tempfile::tempdir().unwrap();
        let (parent, _) = family_tree(home.path());
        let migrated = home.path().join("sessions/2026/08/01").join(format!(
            "rollout-2026-08-02T11-00-00-{MIGRATED_SUBAGENT_THREAD}.jsonl"
        ));
        std::fs::copy(fixture("subagent-migrated.jsonl"), &migrated).unwrap();

        let view = super::super::view_projection(&CODEX_ROLLOUT, &parent, &[migrated])
            .unwrap()
            .unwrap();

        assert!(entry_json(&view).contains("migrated answer searchable"));
    }

    /// The row names nested threads with the rest, flattened; the view
    /// splices every one it is handed.
    #[test]
    fn sub_agent_of_sub_agent_splices_into_root_view() {
        let home = tempfile::tempdir().unwrap();
        let (parent, subagent) = family_tree(home.path());
        let nested = "019f0000-0000-7000-8000-00000000000d";
        let nested_rollout = home
            .path()
            .join("sessions/2026/08/01")
            .join(format!("rollout-2026-08-02T10-00-05-{nested}.jsonl"));
        test_support::write_subagent_rollout(&nested_rollout, nested, SUBAGENT_THREAD);

        let view =
            super::super::view_projection(&CODEX_ROLLOUT, &parent, &[subagent, nested_rollout])
                .unwrap()
                .unwrap();

        assert!(entry_json(&view).contains("sub-agent answer visible"));
        assert!(entry_json(&view).contains("child answer searchable"));
    }

    #[test]
    fn view_without_sub_agents_is_plain_parse() {
        let view = super::super::view_projection(&CODEX_ROLLOUT, &fixture("rollout.jsonl"), &[])
            .unwrap()
            .unwrap();

        assert!(entry_json(&view).contains("active codex question"));
        assert!(!entry_json(&view).contains("agent_progress"));
    }

    const FIRST_EDIT: &str = "019f0000-0000-7000-8000-0000000000a1";
    const SECOND_EDIT: &str = "019f0000-0000-7000-8000-0000000000a2";
    const FORK: &str = "019f0000-0000-7000-8000-00000000000f";

    fn rollout_line(value: Value) -> String {
        format!("{value}\n")
    }

    fn header_line(
        thread_id: &str,
        timestamp: &str,
        cli_version: &str,
        history_base: Option<Value>,
    ) -> String {
        let mut payload = json!({
            "id": thread_id,
            "timestamp": timestamp,
            "cwd": "/tmp/project",
            "cli_version": cli_version,
        });
        if let Some(history_base) = history_base {
            payload["history_base"] = history_base;
        }
        rollout_line(json!({"timestamp": timestamp, "type": "session_meta", "payload": payload}))
    }

    /// A prompt and its answer; the answer reads `answer to <prompt>`.
    fn turn(prompt: &str, timestamp: &str) -> String {
        let message = |role: &str, kind: &str, text: String| {
            rollout_line(json!({
                "timestamp": timestamp,
                "type": "response_item",
                "payload": {"type": "message", "role": role, "content": [{"type": kind, "text": text}]},
            }))
        };
        message("user", "input_text", prompt.to_owned())
            + &message("assistant", "output_text", format!("answer to {prompt}"))
    }

    /// The `history_base` for inheriting `inherited`, the opening text of the
    /// rollout `rollout_id`.
    fn continuing(rollout_id: &str, inherited: &str) -> Value {
        json!({
            "thread_id": rollout_id,
            "end_ordinal_exclusive": 1,
            "end_byte_offset": inherited.len(),
        })
    }

    /// One thread in three rollouts: alpha and bravo; then delta and echo,
    /// after bravo's prompt was edited; then foxtrot, after echo's was.
    struct EditedThread {
        root: PathBuf,
        first_edit: PathBuf,
        second_edit: PathBuf,
        second_edit_text: String,
    }

    fn edited_thread(home: &Path) -> EditedThread {
        let root_kept = header_line(PARENT_THREAD, "2026-10-12T10:00:00.000Z", "0.160.0", None)
            + &turn("alpha", "2026-10-12T10:00:01.000Z");
        let root_text = root_kept.clone() + &turn("bravo", "2026-10-12T10:01:00.000Z");
        let root = write_rollout_text(home, "2026-10-12T10-00-00", PARENT_THREAD, &root_text);

        let first_kept = header_line(
            PARENT_THREAD,
            "2026-10-12T10:02:00.000Z",
            "0.162.1",
            Some(continuing(PARENT_THREAD, &root_kept)),
        ) + &turn("delta", "2026-10-12T10:02:01.000Z");
        let first_text = first_kept.clone() + &turn("echo", "2026-10-12T10:03:00.000Z");
        let first_edit = write_rollout_text(
            home,
            "2026-10-12T10-02-00",
            &format!("{PARENT_THREAD}_{FIRST_EDIT}"),
            &first_text,
        );

        let second_edit_text = header_line(
            PARENT_THREAD,
            "2026-10-12T10:04:00.000Z",
            "0.162.1",
            Some(continuing(FIRST_EDIT, &first_kept)),
        ) + &turn("foxtrot", "2026-10-12T10:04:01.000Z");
        let second_edit = write_rollout_text(
            home,
            "2026-10-12T10-04-00",
            &format!("{PARENT_THREAD}_{SECOND_EDIT}"),
            &second_edit_text,
        );
        EditedThread {
            root,
            first_edit,
            second_edit,
            second_edit_text,
        }
    }

    fn parsed(path: &Path) -> SessionProjection {
        CODEX_ROLLOUT.parse_transcript(path).unwrap().unwrap()
    }

    fn dialogue(projection: &SessionProjection) -> Vec<String> {
        projection
            .entries
            .iter()
            .flat_map(|(_, entry)| match entry {
                LogEntry::User { message, .. } => match &message.content {
                    UserContent::Blocks(blocks) => blocks.clone(),
                    UserContent::String(text) => vec![ContentBlock::Text { text: text.clone() }],
                },
                LogEntry::Assistant { message, .. } => message.content.clone(),
                _ => Vec::new(),
            })
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(text),
                _ => None,
            })
            .collect()
    }

    fn turns(prompts: &[&str]) -> Vec<String> {
        prompts
            .iter()
            .flat_map(|prompt| [(*prompt).to_owned(), format!("answer to {prompt}")])
            .collect()
    }

    #[test]
    fn edited_prompt_reads_earlier_rollout_up_to_edit_then_new_rollout() {
        let home = tempfile::tempdir().unwrap();
        let thread = edited_thread(home.path());

        let projection = parsed(&thread.first_edit);

        assert_eq!(dialogue(&projection), turns(&["alpha", "delta", "echo"]));
        let (alpha, _) = &projection.entries[0];
        assert_eq!(
            alpha.transcript.as_deref(),
            Some(thread.root.as_path()),
            "an inherited record names the rollout it was read from"
        );
        let (delta, _) = &projection.entries[2];
        assert_eq!(delta.transcript, None);
    }

    #[test]
    fn thread_edited_twice_reads_each_rollout_up_to_where_next_continues_it() {
        let home = tempfile::tempdir().unwrap();
        let thread = edited_thread(home.path());

        assert_eq!(
            dialogue(&parsed(&thread.second_edit)),
            turns(&["alpha", "delta", "foxtrot"])
        );
    }

    #[test]
    fn edited_threads_row_counts_and_indexes_turns_it_inherits() {
        let home = tempfile::tempdir().unwrap();
        let thread = edited_thread(home.path());

        let conversation = process_conversation_file(thread.second_edit, None, None)
            .unwrap()
            .unwrap();

        assert_eq!(conversation.message_count, 6);
        for kept in ["alpha", "delta", "foxtrot"] {
            assert!(conversation.full_text.contains(kept), "missing {kept:?}");
        }
        for replaced in ["bravo", "echo"] {
            assert!(
                !conversation.full_text.contains(replaced),
                "indexed {replaced:?}"
            );
        }
    }

    #[test]
    fn missing_or_compressed_earlier_rollout_ends_chain() {
        let home = tempfile::tempdir().unwrap();
        let thread = edited_thread(home.path());
        let compressed = thread.first_edit.with_extension("jsonl.zst");
        std::fs::rename(&thread.first_edit, &compressed).unwrap();

        let projection = parsed(&thread.second_edit);
        assert_eq!(dialogue(&projection), turns(&["foxtrot"]));
        assert_eq!(projection.earlier_turns, EarlierTurns::Missing);

        std::fs::remove_file(compressed).unwrap();
        let projection = parsed(&thread.second_edit);
        assert_eq!(dialogue(&projection), turns(&["foxtrot"]));
        assert_eq!(projection.earlier_turns, EarlierTurns::Missing);
        let row = process_conversation_file(thread.second_edit, None, None)
            .unwrap()
            .unwrap();
        assert!(row.is_missing_earlier_turns, "the row the list counts");
        assert!(!row.has_transient_read_error, "the row is cached");
    }

    #[test]
    fn whole_chain_of_rollouts_doesnt_miss_earlier_turns() {
        let home = tempfile::tempdir().unwrap();
        let thread = edited_thread(home.path());

        assert_eq!(
            parsed(&thread.second_edit).earlier_turns,
            EarlierTurns::Read
        );
        assert_eq!(parsed(&thread.root).earlier_turns, EarlierTurns::Read);
    }

    #[test]
    fn rollout_not_found_after_transient_listing_error_is_transient() {
        let index = RolloutIndex {
            has_transient_error: true,
            ..RolloutIndex::default()
        };
        assert_eq!(index.find("absent").err(), Some(ReadError::Transient));

        let index = RolloutIndex::default();
        assert_eq!(index.find("absent").err(), Some(ReadError::Permanent));
    }

    #[test]
    fn earlier_rollout_cut_inside_its_header_ends_chain() {
        let home = tempfile::tempdir().unwrap();
        let thread = edited_thread(home.path());
        let cut_header = json!({"thread_id": FIRST_EDIT, "end_byte_offset": 5});
        write_rollout_text(
            home.path(),
            "2026-10-12T10-04-00",
            &format!("{PARENT_THREAD}_{SECOND_EDIT}"),
            &(header_line(
                PARENT_THREAD,
                "2026-10-12T10:04:00.000Z",
                "0.162.1",
                Some(cut_header),
            ) + &turn("foxtrot", "2026-10-12T10:04:01.000Z")),
        );

        let projection = parsed(&thread.second_edit);

        assert_eq!(dialogue(&projection), turns(&["foxtrot"]));
        assert_eq!(projection.earlier_turns, EarlierTurns::Missing);
    }

    #[test]
    fn earlier_rollout_that_fails_to_reopen_ends_chain() {
        let home = tempfile::tempdir().unwrap();
        let thread = edited_thread(home.path());
        let header = |path: &Path| header_of(path).unwrap().unwrap();
        let earlier_rollout = |path: &Path, header_path: &Path| EarlierRollout {
            path: path.to_owned(),
            header: header(header_path),
            end_byte_offset: u64::MAX,
            replaced_at: None,
        };
        let earlier = EarlierRollouts {
            rollouts: vec![
                earlier_rollout(&thread.root, &thread.root),
                earlier_rollout(&home.path().join("deleted.jsonl"), &thread.root),
                earlier_rollout(&thread.first_edit, &thread.first_edit),
            ],
            earlier_turns: EarlierTurns::Read,
            outside_chain: Vec::new(),
            has_transient_error: false,
        };
        let mut thread_projection =
            ThreadProjection::new(&header(&thread.second_edit), DroppedRecords::default());

        thread_projection.read_earlier_rollouts(earlier, ParallelThreshold::DEFAULT);
        let projection = thread_projection
            .finish(header(&thread.second_edit))
            .projection;

        assert_eq!(dialogue(&projection), turns(&["delta", "echo"]));
        assert_eq!(projection.earlier_turns, EarlierTurns::Missing);
        assert_eq!(
            projection.header.timestamp, "2026-10-12T10:02:00.000Z",
            "the thread starts at the oldest rollout read"
        );
    }

    #[cfg(windows)]
    #[test]
    fn locked_earlier_rollout_ends_chain_with_transient_error_and_isnt_cached() {
        use std::os::windows::fs::OpenOptionsExt;

        let home = tempfile::tempdir().unwrap();
        let thread = edited_thread(home.path());
        let _lock = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&thread.first_edit)
            .unwrap();

        let projection = parsed(&thread.second_edit);
        assert_eq!(dialogue(&projection), turns(&["foxtrot"]));
        assert_eq!(projection.earlier_turns, EarlierTurns::TransientError);
        let row = process_conversation_file(thread.second_edit, None, None)
            .unwrap()
            .unwrap();
        assert!(row.is_missing_earlier_turns, "the row the list counts");
        assert!(row.has_transient_read_error, "the row is read again");
    }

    #[test]
    fn earlier_rollouts_malformed_line_doesnt_report_against_newest_rollout() {
        let home = tempfile::tempdir().unwrap();
        let root_kept =
            header_line(PARENT_THREAD, "2026-10-12T10:00:00.000Z", "0.160.0", None) + "{not json\n";
        write_rollout_text(
            home.path(),
            "2026-10-12T10-00-00",
            PARENT_THREAD,
            &root_kept,
        );
        let newest = write_rollout_text(
            home.path(),
            "2026-10-12T10-01-00",
            &format!("{PARENT_THREAD}_{FIRST_EDIT}"),
            &(header_line(
                PARENT_THREAD,
                "2026-10-12T10:01:00.000Z",
                "0.162.1",
                Some(continuing(PARENT_THREAD, &root_kept)),
            ) + &turn("delta", "2026-10-12T10:01:01.000Z")
                + "{not json either\n"),
        );

        let projection = CODEX_ROLLOUT
            .parse_transcript_counting(&newest, DroppedRecords::counting())
            .unwrap()
            .unwrap();

        assert_eq!(
            projection.malformed_lines,
            [4],
            "the newest rollout's own malformed line, at its line in that file"
        );
        assert_eq!(
            projection.dropped.counts().collect::<Vec<_>>(),
            [(Some("0.160.0"), "line", DropOutcome::Failed, 1)]
        );
        assert_eq!(projection.dropped.first_version(), Some("0.162.1"));
    }

    #[test]
    fn chain_of_rollouts_that_loops_back_stops_at_repeated_rollout() {
        let home = tempfile::tempdir().unwrap();
        let looped_back = header_line(
            PARENT_THREAD,
            "2026-10-12T10:00:00.000Z",
            "0.162.1",
            Some(json!({"thread_id": FIRST_EDIT, "end_byte_offset": 1})),
        ) + &turn("second", "2026-10-12T10:00:01.000Z");
        write_rollout_text(
            home.path(),
            "2026-10-12T10-00-00",
            &format!("{PARENT_THREAD}_{SECOND_EDIT}"),
            &looped_back,
        );
        let newest = write_rollout_text(
            home.path(),
            "2026-10-12T10-01-00",
            &format!("{PARENT_THREAD}_{FIRST_EDIT}"),
            &(header_line(
                PARENT_THREAD,
                "2026-10-12T10:01:00.000Z",
                "0.162.1",
                Some(continuing(SECOND_EDIT, &looped_back)),
            ) + &turn("first", "2026-10-12T10:01:01.000Z")),
        );

        assert_eq!(dialogue(&parsed(&newest)), turns(&["second", "first"]));
    }

    #[test]
    fn paginated_fork_reads_turns_it_inherits_from_thread_it_forked() {
        let home = tempfile::tempdir().unwrap();
        let thread = edited_thread(home.path());
        let fork = write_rollout_text(
            home.path(),
            "2026-10-12T10-05-00",
            FORK,
            &(header_line(
                FORK,
                "2026-10-12T10:05:00.000Z",
                "0.162.1",
                Some(continuing(SECOND_EDIT, &thread.second_edit_text)),
            ) + &turn("golf", "2026-10-12T10:05:01.000Z")),
        );

        let projection = parsed(&fork);

        assert_eq!(
            dialogue(&projection),
            turns(&["alpha", "delta", "foxtrot", "golf"])
        );
        assert_eq!(projection.header.id, FORK);
        assert_eq!(
            projection.header.timestamp, "2026-10-12T10:00:00.000Z",
            "the thread started with the oldest rollout it continues"
        );
    }

    #[test]
    fn earlier_rollout_codex_archived_is_read() {
        let home = tempfile::tempdir().unwrap();
        let thread = edited_thread(home.path());
        let archived = home.path().join(ARCHIVED_SESSIONS_DIRECTORY);
        std::fs::create_dir_all(&archived).unwrap();
        std::fs::rename(
            &thread.root,
            archived.join(thread.root.file_name().unwrap()),
        )
        .unwrap();

        assert_eq!(
            dialogue(&parsed(&thread.first_edit)),
            turns(&["alpha", "delta", "echo"])
        );
    }

    #[test]
    fn each_rollouts_dropped_records_count_under_version_that_wrote_it() {
        let home = tempfile::tempdir().unwrap();
        let teleport = rollout_line(
            json!({"timestamp": "2026-10-12T10:00:02.000Z", "type": "teleport", "payload": {}}),
        );
        let root_kept =
            header_line(PARENT_THREAD, "2026-10-12T10:00:00.000Z", "0.160.0", None) + &teleport;
        write_rollout_text(
            home.path(),
            "2026-10-12T10-00-00",
            PARENT_THREAD,
            &root_kept,
        );
        let newest = write_rollout_text(
            home.path(),
            "2026-10-12T10-01-00",
            &format!("{PARENT_THREAD}_{FIRST_EDIT}"),
            &(header_line(
                PARENT_THREAD,
                "2026-10-12T10:01:00.000Z",
                "0.162.1",
                Some(continuing(PARENT_THREAD, &root_kept)),
            ) + &teleport),
        );

        let projection = CODEX_ROLLOUT
            .parse_transcript_counting(&newest, DroppedRecords::counting())
            .unwrap()
            .unwrap();

        assert_eq!(
            projection.dropped.counts().collect::<Vec<_>>(),
            [
                (
                    Some("0.160.0"),
                    "type=teleport",
                    DropOutcome::Unrecognised,
                    1
                ),
                (
                    Some("0.162.1"),
                    "type=teleport",
                    DropOutcome::Unrecognised,
                    1
                ),
            ]
        );
    }

    /// An edit left unsent starts a rollout that holds no prompt of its own.
    #[test]
    fn edit_left_unsent_still_holds_turns_before_it() {
        let home = tempfile::tempdir().unwrap();
        let root_text = header_line(PARENT_THREAD, "2026-10-12T10:00:00.000Z", "0.162.1", None)
            + &turn("alpha", "2026-10-12T10:00:01.000Z");
        write_rollout_text(
            home.path(),
            "2026-10-12T10-00-00",
            PARENT_THREAD,
            &root_text,
        );
        let unsent = write_rollout_text(
            home.path(),
            "2026-10-12T10-01-00",
            &format!("{PARENT_THREAD}_{FIRST_EDIT}"),
            &header_line(
                PARENT_THREAD,
                "2026-10-12T10:01:00.000Z",
                "0.162.1",
                Some(continuing(PARENT_THREAD, &root_text)),
            ),
        );

        let conversation = process_conversation_file(unsent, None, None)
            .unwrap()
            .expect("the thread holds the conversation it inherits");

        assert_eq!(conversation.message_count, 2);
    }

    #[test]
    fn sub_agent_spawned_before_edit_sits_beside_turn_that_spawned_it() {
        let home = tempfile::tempdir().unwrap();
        let thread = edited_thread(home.path());
        let subagent = write_rollout_text(
            home.path(),
            "2026-10-12T10-00-30",
            SUBAGENT_THREAD,
            &(rollout_line(json!({
                "timestamp": "2026-10-12T10:00:30.000Z",
                "type": "session_meta",
                "payload": {
                    "id": SUBAGENT_THREAD,
                    "timestamp": "2026-10-12T10:00:30.000Z",
                    "cwd": "/tmp/project",
                    "parent_thread_id": PARENT_THREAD,
                },
            })) + &rollout_line(json!({
                "timestamp": "2026-10-12T10:00:31.000Z",
                "type": "response_item",
                "payload": {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "sub-agent report"}]},
            }))),
        );

        let view = super::super::view_projection(&CODEX_ROLLOUT, &thread.first_edit, &[subagent])
            .unwrap()
            .unwrap();

        let index_of = |text: &str| {
            view.entries
                .iter()
                .position(|(_, entry)| serde_json::to_string(entry).unwrap().contains(text))
                .unwrap()
        };
        let alpha_answer = index_of("answer to alpha");
        let spliced = index_of("sub-agent report");
        let delta = index_of("answer to delta");
        assert!(
            alpha_answer < spliced && spliced < delta,
            "alpha's answer at {alpha_answer}, the sub-agent at {spliced}, delta at {delta}"
        );
    }

    fn view_text(path: &Path, subagents: &[PathBuf]) -> String {
        let view = CODEX_ROLLOUT
            .session_entries(path, subagents)
            .unwrap()
            .unwrap();
        view.entries
            .iter()
            .map(|(_, entry)| serde_json::to_string(entry).unwrap())
            .collect()
    }

    fn report(thread_id: &str) -> String {
        format!("{thread_id} report")
    }

    #[test]
    fn view_leaves_out_discarded_turns_sub_agents_and_threads_beneath_them() {
        let home = tempfile::tempdir().unwrap();
        let thread = test_support::write_edited_thread_with_subagents(home.path(), PARENT_THREAD);

        let view = view_text(&thread.newest, &thread.subagents());

        assert!(
            view.contains(&report(test_support::KEPT_SUBAGENT)),
            "started before the first edit"
        );
        for discarded in [
            test_support::DISCARDED_SUBAGENT,
            test_support::DISCARDED_BY_ID_SUBAGENT,
            test_support::NESTED_SUBAGENT,
            test_support::OUTSIDE_CHAIN_SUBAGENT,
        ] {
            assert!(!view.contains(&report(discarded)), "showed {discarded}");
        }
    }

    #[test]
    fn sub_agent_started_after_edit_shows_at_discarded_turns_agent_path() {
        let home = tempfile::tempdir().unwrap();
        let thread = test_support::write_edited_thread_with_subagents(home.path(), PARENT_THREAD);
        let reused = "019f0000-0000-7000-8000-0000000000b6";
        let reused_rollout = test_support::write_subagent_answer(
            home.path(),
            reused,
            PARENT_THREAD,
            Some("/root/discarded"),
            "2026-10-12T10:05:00.000Z",
        );

        assert!(view_text(&thread.newest, &[reused_rollout]).contains(&report(reused)));
    }

    #[test]
    fn forks_sub_agent_shows_when_forked_thread_used_its_agent_path_past_fork() {
        let home = tempfile::tempdir().unwrap();
        let thread = test_support::write_edited_thread_with_subagents(home.path(), PARENT_THREAD);
        let fork = write_rollout_text(
            home.path(),
            "2026-10-12T10-01-45",
            FORK,
            &(header_line(
                FORK,
                "2026-10-12T10:01:45.000Z",
                "0.162.1",
                Some(continuing(PARENT_THREAD, &thread.kept_text)),
            ) + &turn("golf", "2026-10-12T10:01:46.000Z")),
        );
        let fork_subagent = "019f0000-0000-7000-8000-0000000000c1";
        let fork_subagent_rollout = test_support::write_subagent_answer(
            home.path(),
            fork_subagent,
            FORK,
            Some("/root/discarded"),
            "2026-10-12T10:01:50.000Z",
        );

        let view = view_text(&fork, &[fork_subagent_rollout]);

        assert!(view.contains(&report(fork_subagent)));
    }

    /// Fails every read with its error kind.
    struct FailingRead(std::io::ErrorKind);

    impl Read for FailingRead {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            Err(self.0.into())
        }
    }

    #[test]
    fn transient_error_reading_discarded_turns_is_recorded_and_permanent_one_isnt() {
        let spawn = test_support::spawn_lines(
            "call_by_id",
            json!({"agent_id": test_support::DISCARDED_BY_ID_SUBAGENT}),
        );
        let replaced_at = instant_of("2026-10-12T10:02:00.000Z").unwrap();
        for (kind, is_transient) in [
            (std::io::ErrorKind::WouldBlock, true),
            (std::io::ErrorKind::InvalidData, false),
        ] {
            let records =
                BufReader::new(std::io::Cursor::new(spawn.clone()).chain(FailingRead(kind)));
            let mut discarded = DiscardedTurnSubagents::default();

            discarded.record_spawns_in(records, replaced_at);

            assert_eq!(discarded.has_transient_error, is_transient, "{kind:?}");
            assert!(
                discarded
                    .thread_ids
                    .contains(test_support::DISCARDED_BY_ID_SUBAGENT),
                "the spawn read before the {kind:?} error"
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn locked_sub_agent_header_marks_discarded_transcripts_transient() {
        use std::os::windows::fs::OpenOptionsExt;

        let home = tempfile::tempdir().unwrap();
        let thread = test_support::write_edited_thread_with_subagents(home.path(), PARENT_THREAD);
        let read = read_thread(&thread.newest, DroppedRecords::default())
            .unwrap()
            .unwrap();
        let _lock = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&thread.kept)
            .unwrap();

        let discarded = read
            .discarded_turn_subagents
            .transcripts_among(&thread.subagents());

        assert!(discarded.has_transient_error);
    }

    #[test]
    fn file_that_is_not_rollout_is_not_recognized() {
        let pi =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/pi/v3-branched.jsonl");
        assert!(CODEX_ROLLOUT.parse_transcript(&pi).unwrap().is_none());

        let directory = tempfile::tempdir().unwrap();
        let claude = directory.path().join("claude.jsonl");
        std::fs::write(&claude, "{\"type\":\"user\"}\n").unwrap();
        assert!(CODEX_ROLLOUT.parse_transcript(&claude).unwrap().is_none());
    }

    #[test]
    fn thread_named_in_session_index_gets_its_newest_name() {
        let home = tempfile::tempdir().unwrap();
        let day = home.path().join("sessions/2026/08/01");
        std::fs::create_dir_all(&day).unwrap();
        let transcript =
            day.join("rollout-2026-08-01T10-00-00-019f0000-0000-7000-8000-00000000000a.jsonl");
        std::fs::copy(fixture("rollout.jsonl"), &transcript).unwrap();
        std::fs::write(
            home.path().join("session_index.jsonl"),
            concat!(
                "{\"id\":\"019f0000-0000-7000-8000-00000000000a\",\"thread_name\":\"old name\",\"updated_at\":\"2026-08-01T11:00:00Z\"}\n",
                "{\"id\":\"019f0000-0000-7000-8000-00000000000f\",\"thread_name\":\"someone else\",\"updated_at\":\"2026-08-01T11:30:00Z\"}\n",
                "{\"id\":\"019f0000-0000-7000-8000-00000000000a\",\"thread_name\":\"newest\\nname\",\"updated_at\":\"2026-08-01T12:00:00Z\"}\n",
            ),
        )
        .unwrap();

        let projection = CODEX_ROLLOUT
            .parse_transcript(&transcript)
            .unwrap()
            .unwrap();

        assert_eq!(projection.title.as_deref(), Some("newest name"));
        assert!(matches!(
            &projection.entries[0].1,
            LogEntry::CustomTitle { custom_title } if custom_title == "newest name"
        ));
    }

    #[test]
    fn standalone_result_keeps_tool_it_names() {
        let projection = CODEX_ROLLOUT
            .parse_transcript(&fixture("rollout.jsonl"))
            .unwrap()
            .unwrap();

        let named = tool_results(&projection)
            .into_iter()
            .filter(|result| result.standalone_tool_name.is_some())
            .collect::<Vec<_>>();

        let [received] = named.as_slice() else {
            panic!("expected the one standalone result the fixture records: {named:?}");
        };
        assert_eq!(
            received.standalone_tool_name.as_deref(),
            Some("send_message_to_thread")
        );
        assert_eq!(
            received.tool_use_id, "fco_standalone",
            "with no call to answer, the result takes the item's own id"
        );
        assert_eq!(received.text.as_deref(), Some("delegated task searchable"));
    }

    /// One tool result of a projection, as the reader built it.
    #[derive(Debug)]
    struct ProjectedResult {
        tool_use_id: String,
        standalone_tool_name: Option<String>,
        text: Option<String>,
    }

    /// Every tool result the projection holds, in order.
    fn tool_results(projection: &SessionProjection) -> Vec<ProjectedResult> {
        projection
            .entries
            .iter()
            .filter_map(|(_, entry)| match entry {
                LogEntry::User { message, .. } => match &message.content {
                    UserContent::Blocks(blocks) => Some(blocks),
                    UserContent::String(_) => None,
                },
                _ => None,
            })
            .flatten()
            .filter_map(|block| match block {
                ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    standalone_tool_name,
                } => Some(ProjectedResult {
                    tool_use_id: tool_use_id.clone(),
                    standalone_tool_name: standalone_tool_name.clone(),
                    text: content.as_ref().and_then(Value::as_str).map(str::to_owned),
                }),
                _ => None,
            })
            .collect()
    }

    /// A result that answers a call carries no name of its own: the call above
    /// it names the tool.
    #[test]
    fn paired_result_names_no_tool() {
        let projection = CODEX_ROLLOUT
            .parse_transcript(&fixture("rollout.jsonl"))
            .unwrap()
            .unwrap();

        let paired = tool_results(&projection)
            .into_iter()
            .find(|result| result.tool_use_id == "call_1")
            .expect("the fixture holds a result answering call_1");

        assert_eq!(paired.standalone_tool_name, None, "{paired:?}");
    }

    /// Codex wraps a command the user ran in one XML element, which
    /// `is_injected_context` would otherwise drop as context Codex injected.
    #[test]
    fn user_run_command_reads_as_call_and_its_result() {
        let projection = CODEX_ROLLOUT
            .parse_transcript(&fixture("rollout.jsonl"))
            .unwrap()
            .unwrap();

        let commands = projection
            .entries
            .iter()
            .filter_map(ProjectedCommand::of)
            .collect::<Vec<_>>();

        let [succeeded, failed, printed_closing_tag] = commands.as_slice() else {
            panic!("expected the three commands the fixture records: {commands:?}");
        };

        assert_eq!(
            succeeded.call_id, "msg_shell_ok",
            "the call uses the message id"
        );
        assert_eq!(
            succeeded.result_id, succeeded.call_id,
            "the result answers that call"
        );
        assert_eq!(succeeded.command, "wc -l README.md");
        assert_eq!(succeeded.result, "shell output searchable");
        assert_eq!(
            succeeded.timestamp.as_deref(),
            Some("2026-08-01T10:02:05.000Z"),
            "the entry opens a run, which is stamped from the record"
        );

        assert_eq!(failed.command, "wc -l missing");
        assert_eq!(
            failed.result, "wc: no such file\nExit code: 1",
            "a failed command names its exit code, with no terminal styling"
        );

        assert_eq!(
            printed_closing_tag.command, r#"echo x; echo "</result>""#,
            "a command that prints the wrapper's closing tag is read whole"
        );
        assert_eq!(printed_closing_tag.result, "x\n</result>");
    }

    /// One command the user ran, as the projection records it.
    #[derive(Debug)]
    struct ProjectedCommand {
        call_id: String,
        result_id: String,
        command: String,
        result: String,
        timestamp: Option<String>,
    }

    impl ProjectedCommand {
        fn of((_, entry): &(RecordLine, LogEntry)) -> Option<Self> {
            let LogEntry::User {
                message, timestamp, ..
            } = entry
            else {
                return None;
            };
            let UserContent::Blocks(blocks) = &message.content else {
                return None;
            };
            let [
                ContentBlock::ToolUse {
                    id, tool, input, ..
                },
                ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    ..
                },
            ] = blocks.as_slice()
            else {
                return None;
            };
            (*tool == Tool::UserShell).then(|| Self {
                call_id: id.clone(),
                result_id: tool_use_id.clone(),
                command: input["command"]
                    .as_str()
                    .expect("the call carries its command")
                    .to_owned(),
                result: content
                    .as_ref()
                    .and_then(Value::as_str)
                    .expect("the result carries the output")
                    .to_owned(),
                timestamp: timestamp.clone(),
            })
        }
    }

    /// Fills in the wrapper Codex writes for one command.
    fn wrapper(command: &str, exit_code: &str, output: &str) -> String {
        format!(
            "<user_shell_command>\n<command>\n{command}\n</command>\n<result>\n\
             Exit code: {exit_code}\nDuration: 0.1 seconds\nOutput:\n{output}\n\
             </result>\n</user_shell_command>"
        )
    }

    /// A wrapper the anchors do not fit is left to `is_injected_context`, which
    /// drops it — a missing row rather than a wrong one.
    #[test]
    fn wrapper_that_fails_anchor_is_not_read_as_command() {
        for unreadable in [
            "plain question".to_owned(),
            "<user_instructions>be brief</user_instructions>".to_owned(),
            wrapper("ls", "", "out"),
            wrapper("ls", "not a number", "out"),
            wrapper("ls", "0", "out").replace("\n</result>\n</user_shell_command>", ""),
        ] {
            assert!(
                UserShellCommand::parse(&unreadable).is_none(),
                "{unreadable:?}"
            );
        }
    }

    #[test]
    fn wrapper_is_read_through_whitespace_around_it() {
        let padded = format!("\n{}\n", wrapper("ls", "0", "out"));

        let command = UserShellCommand::parse(&padded).expect("the anchors sit inside the padding");

        assert_eq!(command.command, "ls");
        assert_eq!(command.result_text(), "out");
    }

    #[test]
    fn command_that_printed_nothing_keeps_its_exit_line() {
        let silent = wrapper("false", "1", "").replace("\nOutput:\n\n", "\n");

        let command = UserShellCommand::parse(&silent).expect("the wrapper still fits the anchors");

        assert_eq!(
            command.result_text(),
            "Exit code: 1",
            "with no output the exit line opens the result rather than following a blank row"
        );
    }

    #[test]
    fn injected_context_is_recognized_structurally() {
        for injected in [
            "<environment_context>\n<cwd>/tmp</cwd>\n</environment_context>",
            "<user_instructions>be brief</user_instructions>",
            "<no retained transcript delta entries>",
            "# AGENTS.md instructions for /tmp\n\n<INSTRUCTIONS>\nrules\n</INSTRUCTIONS>",
        ] {
            assert!(is_injected_context(injected), "{injected}");
        }
        for typed_by_a_user in [
            "why does <b>bold</b> not render?",
            "look at <this",
            "</closing> only",
            "plain question",
        ] {
            assert!(!is_injected_context(typed_by_a_user), "{typed_by_a_user}");
        }
    }
}
