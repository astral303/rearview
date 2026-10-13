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
use super::{
    EarlierTurns, RecordLine, SessionFormat, SessionHeader, SessionProjection, append_exit_code,
    block_texts, trim_blank_lines,
};
use crate::agent::sanitize::sanitize_agent_text;
use crate::agent::transcript::tool_result_text;
use crate::error::{AppError, Result};
use crate::history::provider::ReadError;
use crate::history::{Source, TaskReport};
use crate::log_entry::{
    AssistantMessage, ContentBlock, LogEntry, SubagentIdentity, TokenUsage, Tool, UserContent,
    UserMessage,
};
use crate::parallel::ParallelThreshold;
use serde_json::{Map, Value, json};
use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
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
        let Some(mut projection) = parse_thread(path, ParallelThreshold::DEFAULT, dropped)? else {
            return Ok(None);
        };
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
        Ok(Some(projection))
    }
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
) -> Result<Option<SessionProjection>> {
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
    last_model: Option<String>,
    own_history: OwnHistory,
    dropped: DroppedRecords,
    /// The oldest earlier rollout's start, when one was read.
    started: Option<String>,
    earlier_turns: EarlierTurns,
}

impl ThreadProjection {
    fn new(newest: &RolloutHeader, mut dropped: DroppedRecords) -> Self {
        // Record the newest rollout's version first: the dropped-records
        // report counts its malformed lines under the first version.
        dropped.record_version(|| newest.cli_version.clone());
        Self {
            entries: Vec::new(),
            malformed_lines: Vec::new(),
            last_model: None,
            own_history: match newest.own_history_start {
                Some(boundary) => OwnHistory::Pending { boundary },
                None => OwnHistory::Started,
            },
            dropped,
            started: None,
            earlier_turns: EarlierTurns::Read,
        }
    }

    /// A rollout that fails to read ends the chain, as a missing one does:
    /// drop the turns read so far, and read the newer rollouts.
    fn read_earlier_rollouts(&mut self, earlier: EarlierRollouts, parallel: ParallelThreshold) {
        self.earlier_turns = earlier.earlier_turns;
        for rollout in earlier.rollouts {
            if let Err(unread) = self.read_earlier(&rollout, parallel) {
                self.entries.clear();
                self.last_model = None;
                self.started = None;
                self.earlier_turns = unread;
                continue;
            }
            self.started.get_or_insert(rollout.header.timestamp);
        }
    }

    /// Read `rollout` up to its `end_byte_offset`. Fails with
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
            reader,
            first_record_line,
            parallel,
        )
        .map_err(unread)
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
            last_model,
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
                    // parent's history, the model it last named included.
                    entries.clear();
                    *last_model = None;
                    *own_history = OwnHistory::Started;
                }
                if let Some(entry) = normalize_line(object, last_model, dropped) {
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
    fn finish(self, newest: RolloutHeader) -> SessionProjection {
        SessionProjection {
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
        }
    }
}

/// An earlier rollout a thread continues: its header, and the byte just past
/// the last of its records the thread inherits.
struct EarlierRollout {
    path: PathBuf,
    header: RolloutHeader,
    end_byte_offset: u64,
}

/// The earlier rollouts a thread continues, oldest first.
struct EarlierRollouts {
    rollouts: Vec<EarlierRollout>,
    /// [`EarlierTurns::Read`] unless the oldest rollout read continues one
    /// that could not be read.
    earlier_turns: EarlierTurns,
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
    };
    let Some(mut base) = newest.history_base.clone() else {
        return earlier;
    };
    let rollouts = RolloutIndex::around(transcript);
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
        });
        match next {
            Some(next) => base = next,
            None => break,
        }
    }
    earlier.rollouts.reverse();
    earlier
}

/// Codex moves an archived thread's rollouts into this directory, beside the
/// `sessions` tree.
const ARCHIVED_SESSIONS_DIRECTORY: &str = "archived_sessions";

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
        if let Some(home) = sessions.parent() {
            index.add_rollouts_under(&home.join(ARCHIVED_SESSIONS_DIRECTORY));
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

fn normalize_line(
    object: &Map<String, Value>,
    last_model: &mut Option<String>,
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
        Some("turn_context") => model_change(payload, timestamp, last_model),
        Some("event_msg") => event_message(payload, dropped),
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

/// Reads a tool-result payload into a user entry. With no `call_id` the result
/// answers no call: it takes the item's own `id`, or `"unknown"` when the
/// payload carries neither, and keeps the tool name the payload gives.
fn tool_result(payload: &Map<String, Value>, timestamp: Option<String>) -> Option<LogEntry> {
    // Output is either one string or content blocks typed `input_text`, which
    // `tool_result_text` would skip, so the texts are gathered here.
    let text = match payload.get("output") {
        Some(Value::String(text)) => text.clone(),
        output => block_texts(output).join("\n"),
    };
    let text = tool_result_text(&Value::String(text)).unwrap_or_default();
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
/// the name its `spawn_agent` row shows.
fn final_answer_report(text: &str, author: Option<&str>) -> Option<TaskReport> {
    let mut lines = text.split_inclusive('\n');
    if lines.next()?.trim_end() != FINAL_ANSWER_TYPE {
        return None;
    }
    lines
        .by_ref()
        .find(|line| line.trim_end() == FINAL_ANSWER_PAYLOAD)?;
    let report: String = lines.collect();
    let task = author.and_then(|path| path.rsplit('/').find(|segment| !segment.is_empty()));
    Some(TaskReport::handed_back(task, &report))
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

/// An `event_msg` line: usage from `token_count`. An event in
/// [`RESTATED_EVENTS`] or [`LIFECYCLE_EVENTS`] counts as skipped, any other as
/// unrecognised.
fn event_message(payload: &Map<String, Value>, dropped: &mut DroppedRecords) -> Option<LogEntry> {
    let kind = payload.get("type").and_then(Value::as_str);
    match kind {
        Some("token_count") => token_usage(payload),
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

/// A thread's newest rollout seen so far, carrying the name fields that order
/// rollouts of one thread.
struct NewestRollout<'a> {
    timestamp: &'a str,
    rollout_id: &'a str,
    path: &'a Path,
}

impl NewestRollout<'_> {
    /// Name timestamps carry seconds only; the UUIDv7 rollout id breaks the
    /// tie, the same way Codex picks the file it resumes.
    fn supersedes(&self, other: &Self) -> bool {
        (self.timestamp, self.rollout_id) > (other.timestamp, other.rollout_id)
    }
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
            timestamp: name.timestamp,
            rollout_id: name.rollout_id,
            path,
        };
        match newest.entry(name.thread_id) {
            Entry::Vacant(slot) => {
                slot.insert(candidate);
            }
            Entry::Occupied(mut kept) if candidate.supersedes(kept.get()) => {
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
    use std::path::Path;

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
    use super::*;
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
        Ok(Some(thread.finish(header)))
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
            r#"{"timestamp":"2026-10-01T00:00:06.000Z","type":"event_msg","payload":{"type":"thread_goal_updated"}}"#,
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
                    "event_msg.type=thread_goal_updated",
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
            &mut None,
            &mut DroppedRecords::default(),
        )
    }

    #[test]
    fn a_sub_agents_final_answer_reads_as_its_report_under_its_task_name() {
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
    fn a_message_between_agents_that_is_not_a_final_answer_stays_a_call() {
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
    fn projects_the_dialogue_and_drops_model_context() {
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
    fn a_sub_agent_rollout_keeps_only_its_own_history() {
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
    fn a_migrated_sub_agent_rollout_keeps_its_whole_history() {
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
    fn a_rollout_parses_the_same_on_the_rayon_pool_as_on_one_thread() {
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
    fn a_header_naming_no_parent_records_a_session() {
        assert_eq!(kind_of("rollout.jsonl"), ThreadKind::Session);
    }

    /// The checked-in sub-agent fixtures predate the `source` field; the
    /// parent link alone makes them sub-agents.
    #[test]
    fn a_header_naming_a_parent_records_a_sub_agent() {
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
    fn a_guardian_review_header_is_skipped() {
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
    fn a_source_with_no_parent_recorded_is_a_session_unless_skipped() {
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
    fn a_file_that_is_not_a_rollout_has_no_thread_kind() {
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
    fn the_view_splices_sub_agent_threads_in_as_progress_entries() {
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

        let (_, last) = view.entries.last().unwrap();
        let LogEntry::Progress { data, .. } = last else {
            panic!("child entries carry later timestamps, so they splice at the end: {last:?}");
        };
        let progress = crate::log_entry::parse_agent_progress(data)
            .expect("a spliced entry is shaped as Claude's agent_progress");
        assert_eq!(progress.agent_id, SUBAGENT_THREAD);
    }

    #[test]
    fn a_migrated_sub_agent_thread_splices_into_the_view() {
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
    fn a_sub_agent_of_a_sub_agent_splices_into_the_root_view() {
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
    fn the_view_without_sub_agents_is_the_plain_parse() {
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

    fn write_rollout_text(home: &Path, stamp: &str, ids: &str, text: &str) -> PathBuf {
        let day = home.join("sessions").join(stamp[..10].replace('-', "/"));
        std::fs::create_dir_all(&day).unwrap();
        let path = day.join(format!("rollout-{stamp}-{ids}.jsonl"));
        std::fs::write(&path, text).unwrap();
        path
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
    fn an_edited_prompt_reads_the_earlier_rollout_up_to_the_edit_then_the_new_rollout() {
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
    fn a_thread_edited_twice_reads_each_rollout_up_to_where_the_next_continues_it() {
        let home = tempfile::tempdir().unwrap();
        let thread = edited_thread(home.path());

        assert_eq!(
            dialogue(&parsed(&thread.second_edit)),
            turns(&["alpha", "delta", "foxtrot"])
        );
    }

    #[test]
    fn an_edited_threads_row_counts_and_indexes_the_turns_it_inherits() {
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
    fn a_missing_or_compressed_earlier_rollout_ends_the_chain() {
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
    fn a_whole_chain_of_rollouts_doesnt_miss_earlier_turns() {
        let home = tempfile::tempdir().unwrap();
        let thread = edited_thread(home.path());

        assert_eq!(
            parsed(&thread.second_edit).earlier_turns,
            EarlierTurns::Read
        );
        assert_eq!(parsed(&thread.root).earlier_turns, EarlierTurns::Read);
    }

    #[test]
    fn a_rollout_not_found_after_a_transient_listing_error_is_transient() {
        let index = RolloutIndex {
            has_transient_error: true,
            ..RolloutIndex::default()
        };
        assert_eq!(index.find("absent").err(), Some(ReadError::Transient));

        let index = RolloutIndex::default();
        assert_eq!(index.find("absent").err(), Some(ReadError::Permanent));
    }

    #[test]
    fn an_earlier_rollout_cut_inside_its_header_ends_the_chain() {
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
    fn an_earlier_rollout_that_fails_to_reopen_ends_the_chain() {
        let home = tempfile::tempdir().unwrap();
        let thread = edited_thread(home.path());
        let header = |path: &Path| header_of(path).unwrap().unwrap();
        let earlier_rollout = |path: &Path, header_path: &Path| EarlierRollout {
            path: path.to_owned(),
            header: header(header_path),
            end_byte_offset: u64::MAX,
        };
        let earlier = EarlierRollouts {
            rollouts: vec![
                earlier_rollout(&thread.root, &thread.root),
                earlier_rollout(&home.path().join("deleted.jsonl"), &thread.root),
                earlier_rollout(&thread.first_edit, &thread.first_edit),
            ],
            earlier_turns: EarlierTurns::Read,
        };
        let mut thread_projection =
            ThreadProjection::new(&header(&thread.second_edit), DroppedRecords::default());

        thread_projection.read_earlier_rollouts(earlier, ParallelThreshold::DEFAULT);
        let projection = thread_projection.finish(header(&thread.second_edit));

        assert_eq!(dialogue(&projection), turns(&["delta", "echo"]));
        assert_eq!(projection.earlier_turns, EarlierTurns::Missing);
        assert_eq!(
            projection.header.timestamp, "2026-10-12T10:02:00.000Z",
            "the thread starts at the oldest rollout read"
        );
    }

    #[cfg(windows)]
    #[test]
    fn a_locked_earlier_rollout_ends_the_chain_with_a_transient_error_and_isnt_cached() {
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
    fn an_earlier_rollouts_malformed_line_doesnt_report_against_the_newest_rollout() {
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
    fn a_chain_of_rollouts_that_loops_back_stops_at_the_repeated_rollout() {
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
    fn a_paginated_fork_reads_the_turns_it_inherits_from_the_thread_it_forked() {
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
    fn an_earlier_rollout_codex_archived_is_read() {
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
    fn each_rollouts_dropped_records_count_under_the_version_that_wrote_it() {
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
    fn an_edit_left_unsent_still_holds_the_turns_before_it() {
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
    fn a_sub_agent_spawned_before_the_edit_sits_beside_the_turn_that_spawned_it() {
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

    #[test]
    fn a_file_that_is_not_a_rollout_is_not_recognized() {
        let pi =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/pi/v3-branched.jsonl");
        assert!(CODEX_ROLLOUT.parse_transcript(&pi).unwrap().is_none());

        let directory = tempfile::tempdir().unwrap();
        let claude = directory.path().join("claude.jsonl");
        std::fs::write(&claude, "{\"type\":\"user\"}\n").unwrap();
        assert!(CODEX_ROLLOUT.parse_transcript(&claude).unwrap().is_none());
    }

    #[test]
    fn a_thread_named_in_the_session_index_gets_its_newest_name() {
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
    fn a_standalone_result_keeps_the_tool_it_names() {
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
    fn a_paired_result_names_no_tool() {
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
    fn a_user_run_command_reads_as_a_call_and_its_result() {
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
    fn a_wrapper_that_fails_an_anchor_is_not_read_as_a_command() {
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
    fn a_wrapper_is_read_through_the_whitespace_around_it() {
        let padded = format!("\n{}\n", wrapper("ls", "0", "out"));

        let command = UserShellCommand::parse(&padded).expect("the anchors sit inside the padding");

        assert_eq!(command.command, "ls");
        assert_eq!(command.result_text(), "out");
    }

    #[test]
    fn a_command_that_printed_nothing_keeps_its_exit_line() {
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
