use crate::agent::diagnostic::{AgentError, AgentErrorKind};
use crate::agent::message_numbering::{MessageNumbering, Numbered, is_visible_block, visible_text};
use crate::agent::refs::ResolvedConversation;
use crate::agent::sanitize::sanitize_agent_text;
use crate::agent::visibility::ContentVisibility;
use crate::error::{AppError, Result};
use crate::history::format::RecordLine;
use crate::history::task_notification::{ReportKind, ReportSender};
use crate::history::{
    TaskReport, blocks_task_report, extract_skill_preview, parse_task_report, user_task_report,
};
use crate::log_entry::{
    AgentContent, AgentMessage as ProgressMessage, AgentProgressData, AssistantMessage,
    ContentBlock, LogEntry, SubagentIdentity, Tool, UserContent, UserMessage,
    agent_progress_identity, labeled_metadata_text, parse_agent_progress, progress_launch_call,
    progress_timestamp,
};
use crate::tool_format::{ToolCallHeader, launch_prompt, tool_call_header};
use crate::tui::viewer::local_command_output;
use serde_json::Value;
use std::borrow::Cow;
use std::collections::HashMap;
#[cfg(test)]
use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Clone, Debug, PartialEq)]
pub struct AgentTranscript {
    pub path: PathBuf,
    pub messages: Vec<AgentMessage>,
    pub malformed_lines: Vec<usize>,
    pub summary: Option<String>,
    pub custom_title: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AgentMessage {
    pub ordinal: usize,
    pub role: AgentMessageRole,
    /// True for a user message carrying a sub-agent's or a background task's
    /// report.
    pub is_report: bool,
    /// The agent or call a report names as its sender; empty for any other
    /// message.
    pub report_sender: ReportSender,
    /// True for a report carrying the reported text; false for a bare
    /// finish notice, such as `Agent "scout" finished`, and any other
    /// message.
    pub report_has_text: bool,
    pub assistant_message_id: Option<String>,
    pub parent_tool_use_id: Option<String>,
    /// The sub-agent thread the message belongs to; `None` for the session's
    /// own messages.
    pub subagent: Option<Arc<SubagentOrigin>>,
    pub parts: Vec<AgentMessagePart>,
}

/// The sub-agent thread a message belongs to, as the transcript records it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubagentOrigin {
    /// The label the provider knows the thread by: its messages'
    /// `parent_tool_use_id`. Two threads can share one, as two sub-agents
    /// of one Claude agent type do.
    pub label: String,
    /// The thread's own transcript; `None` for a thread recorded in the
    /// session's transcript.
    pub transcript: Option<Arc<Path>>,
    /// The id of the call that launched the thread, when the transcript
    /// links the two.
    pub launch_call: Option<String>,
    pub identity: SubagentIdentity,
}

impl SubagentOrigin {
    /// The label and transcript identifying the thread.
    pub fn key(&self) -> (&str, Option<&Path>) {
        (&self.label, self.transcript.as_deref())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AgentMessageRole {
    User,
    Assistant,
}

impl AgentMessageRole {
    /// `User` for a sub-agent progress record of `type` `user`, `Assistant`
    /// for `assistant`; `None` for any other `type`.
    pub(crate) fn of_progress(message_type: &str) -> Option<Self> {
        match message_type {
            "user" => Some(Self::User),
            "assistant" => Some(Self::Assistant),
            _ => None,
        }
    }

    /// The progress record `type` for this role; [`Self::of_progress`] reads
    /// it back.
    pub(crate) fn progress_type(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Assistant => "assistant",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum AgentMessagePart {
    Text {
        text: String,
        source: AgentPartSource,
    },
    ToolUse {
        id: String,
        name: String,
        tool: Tool,
        input: serde_json::Value,
        source: AgentPartSource,
    },
    ToolResult {
        tool_use_id: String,
        content: Option<serde_json::Value>,
        source: AgentPartSource,
    },
    Thinking {
        thinking: String,
        source: AgentPartSource,
    },
}

impl AgentMessagePart {
    pub fn source(&self) -> &AgentPartSource {
        match self {
            Self::Text { source, .. }
            | Self::ToolUse { source, .. }
            | Self::ToolResult { source, .. }
            | Self::Thinking { source, .. } => source,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentPartSource {
    pub role: AgentMessageRole,
    pub timestamp: Option<String>,
    pub jsonl_line: usize,
    pub part_index: usize,
    pub assistant_message_id: Option<String>,
    pub parent_tool_use_id: Option<String>,
    pub tool_name: Option<String>,
}

impl AgentTranscript {
    /// Load a bare file, read by the format of the agent its projection
    /// names. Production reads arrive with a resolved key and read the
    /// session cache; only fixtures come in bare.
    #[cfg(test)]
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let transcript =
            crate::history::format::sniffed_session_entries(crate::history::FOR_FIXTURES, path)
                .map_err(|error| transcript_error(path, error))?
                .ok_or_else(|| {
                    unrecognized_transcript(
                        path,
                        "not a transcript of any supported agent".to_owned(),
                    )
                })?;
        Self::from_transcript_entries(path, transcript)
    }

    /// The transcript at `path`, attributed to `source`, with the sub-agent
    /// transcripts at `subagents` spliced in, parsed for a session the cache
    /// holds no copy of: one that holds no conversation the provider lists,
    /// or one whose read failed. Only that provider's format reads it, so a
    /// locator never meets a foreign format.
    pub fn parse_uncached(
        permit: crate::history::ParsePermit<'_>,
        source: crate::history::Source,
        path: impl AsRef<Path>,
        subagents: &[PathBuf],
    ) -> Result<Self> {
        let path = path.as_ref();
        let provider = source.provider();
        let transcript = provider
            .format()
            .session_entries(permit, path, subagents)
            .map_err(|error| transcript_error(path, error))?
            .ok_or_else(|| {
                unrecognized_transcript(path, declined_detail(provider.labels().display, path))
            })?;
        Self::from_transcript_entries(path, transcript)
    }

    /// `source`'s session at `path` with `subagents`, written into this
    /// test's own session cache by a load as the refresh owner, then loaded
    /// as [`Self::load_cached_as_is`] loads it.
    #[cfg(test)]
    pub(crate) fn load_cached_for_test(
        source: crate::history::Source,
        path: impl AsRef<Path>,
        subagents: &[PathBuf],
    ) -> Result<Self> {
        let path = path.as_ref();
        crate::history::environment::test_support::cache_files(source, path, subagents);
        Self::load_cached_as_is(
            &crate::history::environment::test_support::test_cache_base(),
            source,
            path,
            subagents,
        )
    }

    /// The copy of the transcript at `path`, attributed to `source` with its
    /// row's `subagents`, as the cache under `base` holds it, whether or not
    /// the transcript changed since. Parses nothing: fails when the copy is
    /// missing from the cache.
    pub fn load_cached_as_is(
        base: &crate::history::CacheBase,
        source: crate::history::Source,
        path: impl AsRef<Path>,
        subagents: &[PathBuf],
    ) -> Result<Self> {
        let path = path.as_ref();
        let stored =
            crate::history::sqlite_cache::find_cached_session_as_is(base, source, path, subagents)?
                .map(|cached| cached.numbered_entries(None))
                .transpose()?
                .flatten()
                .ok_or_else(|| {
                    AgentError::io(
                        Some(&path.to_string_lossy()),
                        "this transcript is not in the session cache",
                    )
                })?;
        Self::from_stored(path, stored)
    }

    fn from_transcript_entries(
        path: &Path,
        transcript: crate::history::TranscriptEntries,
    ) -> Result<Self> {
        let malformed_lines = transcript
            .malformed_lines
            .iter()
            .map(|line| line.line_number)
            .collect();
        Self::from_entries(path, transcript.entries, malformed_lines)
    }

    /// A transcript of raw [`LogEntry`] records, for tests of what a
    /// transcript holds.
    #[cfg(test)]
    pub(crate) fn from_reader(path: PathBuf, reader: impl BufRead) -> Result<Self> {
        let mut entries = Vec::new();
        let mut malformed_lines = Vec::new();
        for (line_index, line) in reader.lines().enumerate() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<LogEntry>(&line) {
                Ok(entry) => entries.push((RecordLine::from(line_index + 1), entry)),
                Err(_) => malformed_lines.push(line_index + 1),
            }
        }
        Self::from_entries(&path, entries, malformed_lines)
    }

    /// Each entry with the line of the file it came from, for its parts to
    /// keep: an entry spliced in from a sub-agent transcript keeps that
    /// transcript's line. `malformed_lines` are the source file's, since the
    /// entries hold none.
    pub(crate) fn from_entries(
        path: &Path,
        entries: Vec<(RecordLine, LogEntry)>,
        malformed_lines: Vec<usize>,
    ) -> Result<Self> {
        Self::from_entries_with_numbers(path, entries, malformed_lines, None)
    }

    pub(crate) fn from_stored(
        path: &Path,
        stored: crate::history::sqlite_cache::StoredEntries,
    ) -> Result<Self> {
        let malformed = stored
            .transcript
            .malformed_lines
            .iter()
            .map(|line| line.line_number)
            .collect();
        Self::from_entries_with_numbers(
            path,
            stored.transcript.entries,
            malformed,
            Some(stored.numbers),
        )
    }

    fn from_entries_with_numbers(
        path: &Path,
        entries: Vec<(RecordLine, LogEntry)>,
        malformed_lines: Vec<usize>,
        stored_numbers: Option<Vec<Option<Numbered>>>,
    ) -> Result<Self> {
        let mut messages: Vec<AgentMessage> = Vec::new();
        let mut summary = None;
        let mut custom_title = None;
        let mut numbering = MessageNumbering::default();
        let mut origins = SubagentOrigins::default();
        for (index, (record, entry)) in entries.into_iter().enumerate() {
            let jsonl_line = record.line;
            let thread = numbering.thread(crate::agent::message_numbering::ThreadIdentity::of(
                &entry,
                record.transcript.as_deref(),
            ));
            let numbered = stored_numbers.as_ref().map_or_else(
                || numbering.number_in_thread(&entry, thread),
                |numbers| numbers[index],
            );
            match entry {
                LogEntry::User {
                    message,
                    timestamp,
                    parent_tool_use_id,
                    ..
                } => {
                    let Some(numbered) = numbered else {
                        continue;
                    };
                    let mut agent_message = user_message_to_agent(
                        message,
                        timestamp,
                        jsonl_line,
                        parent_tool_use_id,
                        numbered.ordinal,
                    );
                    agent_message.subagent = origins.of_sidechain(&agent_message);
                    messages.push(agent_message);
                }
                LogEntry::Assistant {
                    message,
                    timestamp,
                    parent_tool_use_id,
                    ..
                } => {
                    let Some(numbered) = numbered else {
                        continue;
                    };
                    let mut agent_message = assistant_message_to_agent(
                        message,
                        timestamp,
                        jsonl_line,
                        parent_tool_use_id,
                        numbered.ordinal,
                    );
                    agent_message.subagent = origins.of_sidechain(&agent_message);
                    push_numbered(&mut messages, numbered, agent_message);
                }
                LogEntry::PiMetadata {
                    label,
                    text,
                    timestamp,
                    ..
                } => {
                    let Some(numbered) = numbered else {
                        continue;
                    };
                    let rendered = labeled_metadata_text(&label, &text);
                    messages.push(AgentMessage {
                        ordinal: numbered.ordinal,
                        role: AgentMessageRole::User,
                        is_report: false,
                        report_sender: ReportSender::default(),
                        report_has_text: false,
                        assistant_message_id: None,
                        parent_tool_use_id: None,
                        subagent: None,
                        parts: vec![AgentMessagePart::Text {
                            text: rendered,
                            source: AgentPartSource {
                                role: AgentMessageRole::User,
                                timestamp,
                                jsonl_line,
                                part_index: 0,
                                assistant_message_id: None,
                                parent_tool_use_id: None,
                                tool_name: None,
                            },
                        }],
                    });
                }
                LogEntry::Progress { data, extra } => {
                    if let Some(progress) = parse_agent_progress(&data) {
                        let AgentContent::Blocks(blocks) = &progress.message.message.content;
                        let numbered = stored_numbers.as_ref().map_or_else(
                            || {
                                let role =
                                    AgentMessageRole::of_progress(&progress.message.message_type)?;
                                numbering.number_progress_in_thread(
                                    role,
                                    blocks,
                                    progress.message.message.id.as_deref(),
                                    thread,
                                )
                            },
                            |numbers| numbers[index],
                        );
                        if let Some((numbered, mut agent_message)) = progress_message_to_agent(
                            progress,
                            progress_timestamp(&extra),
                            jsonl_line,
                            numbered,
                        ) {
                            agent_message.subagent = Some(origins.of_progress(
                                &agent_message,
                                &data,
                                &extra,
                                record.transcript,
                            ));
                            push_numbered(&mut messages, numbered, agent_message);
                        }
                    }
                }
                LogEntry::Summary { summary: value } => {
                    if summary.is_none() && !value.trim().is_empty() {
                        summary = Some(value);
                    }
                }
                LogEntry::AiTitle { ai_title } => {
                    if !ai_title.trim().is_empty() {
                        summary = Some(ai_title);
                    }
                }
                LogEntry::CustomTitle {
                    custom_title: value,
                } => {
                    custom_title = (!value.trim().is_empty()).then_some(value);
                }
                LogEntry::FileHistorySnapshot { .. }
                | LogEntry::System { .. }
                | LogEntry::AgentName { .. }
                | LogEntry::PermissionMode { .. }
                | LogEntry::Unknown => {}
            }
        }

        Ok(Self {
            path: path.to_path_buf(),
            messages,
            malformed_lines,
            summary,
            custom_title,
        })
    }

    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }

    pub fn malformed_warning_detail(&self) -> Option<String> {
        (!self.malformed_lines.is_empty()).then(|| {
            format!(
                "skipped {} malformed JSONL record(s) at lines {}",
                self.malformed_lines.len(),
                line_number_list(&self.malformed_lines)
            )
        })
    }

    pub fn message_anchor(
        &self,
        resolved: &ResolvedConversation,
        message: &AgentMessage,
    ) -> String {
        message_anchor(resolved, message)
    }

    pub fn resolve_anchor(&self, resolved: &ResolvedConversation, anchor: &str) -> Result<usize> {
        validate_anchor(anchor)?;
        let matches = self
            .messages
            .iter()
            .filter(|message| self.message_anchor(resolved, message) == anchor)
            .map(|message| message.ordinal)
            .collect::<Vec<_>>();
        match matches.as_slice() {
            [ordinal] => Ok(*ordinal),
            [] => Err(AgentError::new(
                AgentErrorKind::NotFound,
                Some(anchor),
                format!("message anchor {anchor} was not found in the conversation"),
            )
            .into()),
            _ => Err(AgentError::new(
                AgentErrorKind::AmbiguousRef,
                Some(anchor),
                format!("message anchor {anchor} matches {} messages", matches.len()),
            )
            .into()),
        }
    }
}

const ANCHOR_PREFIX: &str = "ma_";
const ANCHOR_DIGEST_HEX_DIGITS: usize = 16;

fn message_anchor(resolved: &ResolvedConversation, message: &AgentMessage) -> String {
    let mut identity = String::new();
    identity.push_str(&resolved.reference.full_ref());
    identity.push('|');
    identity.push_str(match message.role {
        AgentMessageRole::User => "user",
        AgentMessageRole::Assistant => "assistant",
    });
    for part in &message.parts {
        identity.push('|');
        match part {
            AgentMessagePart::Text { text, .. } => {
                identity.push_str("text:");
                identity.push_str(&normalized_anchor_text(text));
            }
            AgentMessagePart::ToolUse { name, input, .. } => {
                identity.push_str("tool:");
                identity.push_str(name);
                identity.push(':');
                identity.push_str(&normalized_anchor_text(&input.to_string()));
            }
            AgentMessagePart::ToolResult { content, .. } => {
                identity.push_str("result:");
                if let Some(content) = content {
                    identity.push_str(&normalized_anchor_text(&content.to_string()));
                }
            }
            AgentMessagePart::Thinking { thinking, .. } => {
                identity.push_str("thinking:");
                identity.push_str(&normalized_anchor_text(thinking));
            }
        }
    }
    let digest = blake3::hash(identity.as_bytes()).to_hex();
    format!("{ANCHOR_PREFIX}{}", &digest[..ANCHOR_DIGEST_HEX_DIGITS])
}

fn normalized_anchor_text(text: &str) -> String {
    sanitize_agent_text(text)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn validate_anchor(anchor: &str) -> Result<()> {
    let valid = anchor.strip_prefix(ANCHOR_PREFIX).is_some_and(|digest| {
        digest.len() == ANCHOR_DIGEST_HEX_DIGITS && digest.chars().all(|c| c.is_ascii_hexdigit())
    });
    if valid {
        Ok(())
    } else {
        Err(AgentError::invalid_ref(
            anchor,
            format!(
                "message anchor must use {ANCHOR_PREFIX} followed by {ANCHOR_DIGEST_HEX_DIGITS} hexadecimal characters"
            ),
        )
        .into())
    }
}

fn unrecognized_transcript(path: &Path, detail: String) -> AppError {
    AgentError::malformed_transcript(Some(&path.to_string_lossy()), detail).into()
}

/// The detail for a file `agent`'s format does not read as its own, such as
/// a transcript of malformed lines alone: those lines, when the file is text
/// with any line that is not JSON.
fn declined_detail(agent: &str, path: &Path) -> String {
    let malformed = lines_that_are_not_json(path);
    if malformed.is_empty() {
        format!("not a {agent} transcript")
    } else {
        format!(
            "not a {agent} transcript; malformed lines: {}",
            line_number_list(&malformed)
        )
    }
}

/// The numbers of the non-blank lines of the text file at `path` that do not
/// parse as JSON; none for a file that cannot be read as text.
fn lines_that_are_not_json(path: &Path) -> Vec<usize> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    text.lines()
        .enumerate()
        .filter(|(_, line)| {
            !line.trim().is_empty() && serde_json::from_str::<serde_json::Value>(line).is_err()
        })
        .map(|(index, _)| index + 1)
        .collect()
}

fn line_number_list(lines: &[usize]) -> String {
    const SHOWN: usize = 8;
    let mut output = lines
        .iter()
        .take(SHOWN)
        .map(usize::to_string)
        .collect::<Vec<_>>()
        .join(",");
    if lines.len() > SHOWN {
        output.push_str(",...");
    }
    output
}

fn user_message_to_agent(
    message: UserMessage,
    timestamp: Option<String>,
    jsonl_line: usize,
    parent_tool_use_id: Option<String>,
    ordinal: usize,
) -> AgentMessage {
    let MessageReport {
        is_report,
        sender: report_sender,
        has_text: report_has_text,
    } = MessageReport::of(user_task_report(&message.content));
    let parts = match message.content {
        UserContent::String(text) => visible_text(AgentMessageRole::User, text)
            .map(|text| AgentMessagePart::Text {
                text,
                source: source(
                    AgentMessageRole::User,
                    timestamp,
                    jsonl_line,
                    0,
                    None,
                    parent_tool_use_id.clone(),
                    None,
                ),
            })
            .into_iter()
            .collect(),
        UserContent::Blocks(blocks) => blocks_to_parts(
            AgentMessageRole::User,
            blocks,
            timestamp,
            jsonl_line,
            None,
            parent_tool_use_id.clone(),
        ),
    };
    AgentMessage {
        ordinal,
        role: AgentMessageRole::User,
        is_report,
        report_sender,
        report_has_text,
        assistant_message_id: None,
        parent_tool_use_id,
        subagent: None,
        parts,
    }
}

/// The facts a message's report records: whether it is a sub-agent's or a
/// background task's report, who sent it, and whether it carries the
/// reported text.
#[derive(Default)]
pub(crate) struct MessageReport {
    pub(crate) is_report: bool,
    pub(crate) sender: ReportSender,
    pub(crate) has_text: bool,
}

impl MessageReport {
    pub(crate) fn of_blocks(role: AgentMessageRole, blocks: &[ContentBlock]) -> Self {
        match role {
            AgentMessageRole::User => Self::of(blocks_task_report(blocks)),
            AgentMessageRole::Assistant => Self::default(),
        }
    }

    /// `report`'s facts; none for IDE diagnostics, also written as a report,
    /// and for no report.
    fn of(report: Option<TaskReport>) -> Self {
        match report {
            Some(report) if report.kind == ReportKind::Task => Self {
                is_report: true,
                has_text: report.body.is_some(),
                sender: report.sender,
            },
            _ => Self::default(),
        }
    }
}

/// The sub-agent thread of each message, one shared record per thread.
#[derive(Default)]
struct SubagentOrigins {
    by_key: HashMap<(String, Option<Arc<Path>>), Arc<SubagentOrigin>>,
}

impl SubagentOrigins {
    /// The thread of a Claude sidechain record: its `parent_tool_use_id`
    /// names the call that launched the sub-agent. `None` for the session's
    /// own messages.
    fn of_sidechain(&mut self, message: &AgentMessage) -> Option<Arc<SubagentOrigin>> {
        let launch_call = message.parent_tool_use_id.clone()?;
        Some(self.shared(SubagentOrigin {
            label: launch_call.clone(),
            transcript: None,
            launch_call: Some(launch_call),
            identity: SubagentIdentity::default(),
        }))
    }

    /// The thread of a sub-agent turn a `Progress` record carries, read from
    /// `transcript`, the file of the record, or the session's own.
    fn of_progress(
        &mut self,
        message: &AgentMessage,
        data: &Value,
        extra: &Value,
        transcript: Option<Arc<Path>>,
    ) -> Arc<SubagentOrigin> {
        self.shared(SubagentOrigin {
            label: message.parent_tool_use_id.clone().unwrap_or_default(),
            transcript,
            launch_call: progress_launch_call(extra),
            identity: agent_progress_identity(data)
                .map(|(_, identity)| identity)
                .unwrap_or_default(),
        })
    }

    /// The record already held for `origin`'s thread, or `origin`.
    fn shared(&mut self, origin: SubagentOrigin) -> Arc<SubagentOrigin> {
        let key = (origin.label.clone(), origin.transcript.clone());
        self.by_key
            .entry(key)
            .or_insert_with(|| Arc::new(origin))
            .clone()
    }
}

fn assistant_message_to_agent(
    message: AssistantMessage,
    timestamp: Option<String>,
    jsonl_line: usize,
    parent_tool_use_id: Option<String>,
    ordinal: usize,
) -> AgentMessage {
    let assistant_message_id = message.id;
    let parts = blocks_to_parts(
        AgentMessageRole::Assistant,
        message.content,
        timestamp,
        jsonl_line,
        assistant_message_id.clone(),
        parent_tool_use_id.clone(),
    );
    AgentMessage {
        ordinal,
        role: AgentMessageRole::Assistant,
        is_report: false,
        report_sender: ReportSender::default(),
        report_has_text: false,
        assistant_message_id,
        parent_tool_use_id,
        subagent: None,
        parts,
    }
}

/// Push `message`, or append its parts to the earlier message when
/// `numbered` continues it: records sharing a reply id are one reply, as
/// Claude Code writes each content block of a reply as a record of its own.
fn push_numbered(messages: &mut Vec<AgentMessage>, numbered: Numbered, message: AgentMessage) {
    if numbered.starts_message {
        messages.push(message);
    } else {
        let index = messages
            .binary_search_by_key(&numbered.ordinal, |message| message.ordinal)
            .expect("a continuing reply has a first entry");
        messages[index].parts.extend(message.parts);
    }
}

fn progress_message_to_agent(
    progress: AgentProgressData,
    timestamp: Option<String>,
    jsonl_line: usize,
    numbered: Option<Numbered>,
) -> Option<(Numbered, AgentMessage)> {
    let role = AgentMessageRole::of_progress(&progress.message.message_type)?;
    let ProgressMessage { message, .. } = progress.message;
    let AgentContent::Blocks(blocks) = message.content;
    let reply_id = message.id;
    let numbered = numbered?;
    let report = MessageReport::of_blocks(role, &blocks);
    let parent_tool_use_id = Some(progress.agent_id);
    let parts = blocks_to_parts(
        role,
        blocks,
        timestamp,
        jsonl_line,
        reply_id.clone(),
        parent_tool_use_id.clone(),
    );
    Some((
        numbered,
        AgentMessage {
            ordinal: numbered.ordinal,
            role,
            is_report: report.is_report,
            report_sender: report.sender,
            report_has_text: report.has_text,
            assistant_message_id: reply_id,
            parent_tool_use_id,
            subagent: None,
            parts,
        },
    ))
}

fn blocks_to_parts(
    role: AgentMessageRole,
    blocks: Vec<ContentBlock>,
    timestamp: Option<String>,
    jsonl_line: usize,
    assistant_message_id: Option<String>,
    parent_tool_use_id: Option<String>,
) -> Vec<AgentMessagePart> {
    blocks
        .into_iter()
        .enumerate()
        .filter(|(_, block)| is_visible_block(role, block))
        .filter_map(|(part_index, block)| match block {
            ContentBlock::Text { text } => {
                visible_text(role, text).map(|text| AgentMessagePart::Text {
                    text,
                    source: source(
                        role,
                        timestamp.clone(),
                        jsonl_line,
                        part_index,
                        assistant_message_id.clone(),
                        parent_tool_use_id.clone(),
                        None,
                    ),
                })
            }
            ContentBlock::ToolUse {
                id,
                name,
                tool,
                input,
            } => Some(AgentMessagePart::ToolUse {
                id,
                name: name.clone(),
                tool,
                input,
                source: source(
                    role,
                    timestamp.clone(),
                    jsonl_line,
                    part_index,
                    assistant_message_id.clone(),
                    parent_tool_use_id.clone(),
                    Some(name),
                ),
            }),
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                ..
            } => Some(AgentMessagePart::ToolResult {
                tool_use_id,
                content,
                source: source(
                    role,
                    timestamp.clone(),
                    jsonl_line,
                    part_index,
                    assistant_message_id.clone(),
                    parent_tool_use_id.clone(),
                    None,
                ),
            }),
            ContentBlock::Thinking { thinking, .. } => Some(AgentMessagePart::Thinking {
                thinking,
                source: source(
                    role,
                    timestamp.clone(),
                    jsonl_line,
                    part_index,
                    assistant_message_id.clone(),
                    parent_tool_use_id.clone(),
                    None,
                ),
            }),
            ContentBlock::Image { .. } | ContentBlock::Other => None,
        })
        .collect()
}

/// A part's whole text as search reads it, terminal codes stripped. A tool
/// call reads as its [`tool_call_text`].
pub(crate) fn agent_part_text(part: &AgentMessagePart) -> Option<String> {
    let text = match part {
        AgentMessagePart::Text { text, .. } => sanitize_agent_text(text),
        AgentMessagePart::ToolUse {
            name, tool, input, ..
        } => tool_call_text(name, *tool, input),
        AgentMessagePart::ToolResult { content, .. } => {
            content.as_ref().and_then(tool_result_search_text)?
        }
        AgentMessagePart::Thinking { thinking, .. } => sanitize_agent_text(thinking),
    };
    (!text.trim().is_empty()).then_some(text)
}

/// [`agent_part_text`] within [`MAX_AGENT_SEGMENT_CHARS`]: a tool result
/// keeps its first and last characters, any other part its first.
pub(crate) fn bounded_agent_part_text(part: &AgentMessagePart) -> Option<String> {
    let text = match part {
        AgentMessagePart::ToolResult { content, .. } => {
            sanitize_agent_text(&content.as_ref().and_then(bounded_tool_result_text)?)
        }
        _ => agent_part_text(part)?,
    };
    non_empty_text(&truncate_chars(&text, MAX_AGENT_SEGMENT_CHARS))
}

pub(crate) const MAX_AGENT_SEGMENT_CHARS: usize = 16 * 1024;

/// The search text of each block `agent search` reads, one entry per block.
pub(crate) fn agent_search_parts_from_blocks(
    role: AgentMessageRole,
    blocks: &[ContentBlock],
) -> Vec<String> {
    let visibility = ContentVisibility::SEARCH;
    blocks
        .iter()
        .filter(|block| match block {
            ContentBlock::Text { .. } => true,
            ContentBlock::ToolUse { .. } => visibility.tools,
            ContentBlock::ToolResult { .. } => visibility.tool_results,
            ContentBlock::Thinking { .. } => visibility.thinking,
            ContentBlock::Image { .. } | ContentBlock::Other => false,
        })
        .filter_map(|block| agent_search_text_from_block(role, block))
        .collect()
}

/// Each thinking block's whole text and each call's [`tool_call_text`], one
/// entry per block.
pub(crate) fn thinking_and_tool_call_parts(blocks: &[ContentBlock]) -> Vec<String> {
    blocks
        .iter()
        .filter_map(thinking_or_tool_call_text)
        .collect()
}

/// The text of a user block as the agent sees it: a skill invocation as its
/// command line, a task report as its summary and body, a slash command's
/// output or error as the viewer shows it, anything else as written.
pub(crate) fn agent_user_text(text: &str) -> Cow<'_, str> {
    if let Some(report) = parse_task_report(text) {
        return Cow::Owned(report.search_text());
    }
    if let Some(output) = local_command_output(text.trim()) {
        return Cow::Owned(output);
    }
    extract_skill_preview(text).map_or(Cow::Borrowed(text), Cow::Owned)
}

/// `block`'s whole search text with terminal codes stripped.
fn agent_search_text_from_block(role: AgentMessageRole, block: &ContentBlock) -> Option<String> {
    match block {
        ContentBlock::Text { text } => {
            let text = match role {
                AgentMessageRole::User => agent_user_text(text),
                AgentMessageRole::Assistant => Cow::Borrowed(text.as_str()),
            };
            non_empty_text(&sanitize_agent_text(&text))
        }
        ContentBlock::ToolResult { content, .. } => {
            content.as_ref().and_then(tool_result_search_text)
        }
        ContentBlock::ToolUse { .. } | ContentBlock::Thinking { .. } => {
            thinking_or_tool_call_text(block)
        }
        ContentBlock::Image { .. } | ContentBlock::Other => None,
    }
}

fn thinking_or_tool_call_text(block: &ContentBlock) -> Option<String> {
    let sanitized = match block {
        ContentBlock::ToolUse {
            name, tool, input, ..
        } => tool_call_text(name, *tool, input),
        ContentBlock::Thinking { thinking, .. } => sanitize_agent_text(thinking),
        _ => return None,
    };
    non_empty_text(&sanitized)
}

fn non_empty_text(text: &str) -> Option<String> {
    (!text.trim().is_empty()).then(|| text.to_string())
}

/// A tool call as search reads it, terminal codes stripped: the header the
/// viewer shows, with every line of a shell command, then the whole
/// [`launch_prompt`]. When the header's value is empty, as for a call the
/// viewer shows as raw input, the header is the call's name and input keys.
pub(crate) fn tool_call_text(name: &str, tool: Tool, input: &Value) -> String {
    let header = tool_call_header(name, tool, input);
    let text = if header.value.trim().is_empty() {
        name_and_input_keys(name, input, MAX_AGENT_SEGMENT_CHARS)
    } else {
        format!("{}{}", header.prefix, header.value)
    };
    with_launch_prompt(sanitize_agent_text(&text), tool, input)
}

/// The lines of a shell command that `agent read` shows.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CommandLines {
    /// The first non-empty line, then a count of the others.
    FirstLine,
    All,
}

/// A tool call as `agent read` shows it: [`tool_call_text`] with the header
/// on one line, truncated to a search segment. With [`CommandLines::All`], a
/// shell command shows every line, untruncated.
pub(crate) fn tool_call_read_text(
    name: &str,
    tool: Tool,
    input: &Value,
    lines: CommandLines,
) -> String {
    let header = tool_call_header(name, tool, input);
    let shown =
        if lines == CommandLines::All && header.value_is_command && !header.value.trim().is_empty()
        {
            sanitize_agent_text(&whole_command(&header))
        } else {
            truncate_chars(
                &sanitize_agent_text(&tool_call_line(&header, name, input)),
                MAX_AGENT_SEGMENT_CHARS,
            )
        };
    with_launch_prompt(shown, tool, input)
}

const COMMAND_CONTINUATION_INDENT: &str = "  ";

/// The header with every line of its command, each later line indented so the
/// command's last line stands apart from the part after it.
fn whole_command(header: &ToolCallHeader) -> String {
    let mut lines = header.value.lines();
    let mut text = format!("{}{}", header.prefix, lines.next().unwrap_or_default());
    for line in lines {
        text.push('\n');
        if !line.is_empty() {
            text.push_str(COMMAND_CONTINUATION_INDENT);
            text.push_str(line);
        }
    }
    text
}

fn with_launch_prompt(header: String, tool: Tool, input: &Value) -> String {
    match launch_prompt(tool, input) {
        Some(prompt) => format!("{header}\n{}", sanitize_agent_text(prompt)),
        None => header,
    }
}

/// The call's header on one line. A shell command shows its first non-empty
/// line and counts the others: `Bash: cd src (+4 lines)`.
fn tool_call_line(header: &ToolCallHeader, name: &str, input: &Value) -> String {
    if header.value.trim().is_empty() {
        return name_and_input_keys(name, input, MAX_AGENT_SEGMENT_CHARS);
    }
    if !header.value_is_command {
        return format!(
            "{}{}",
            header.prefix,
            header.value.lines().collect::<Vec<_>>().join(" ")
        );
    }
    let shown = header
        .value
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or_default();
    match header.value.lines().count() - 1 {
        0 => format!("{}{shown}", header.prefix),
        1 => format!("{}{shown} (+1 line)", header.prefix),
        hidden => format!("{}{shown} (+{hidden} lines)", header.prefix),
    }
}

/// `tool NAME input_keys=KEY,KEY`, within `max_chars`.
fn name_and_input_keys(name: &str, input: &Value, max_chars: usize) -> String {
    let mut acc = BoundedHeadTail::new(max_chars);
    acc.push_str("tool ");
    acc.push_str(name);
    if let Value::Object(map) = input {
        let prefix_len = acc.len_chars();
        acc.push_str(" input_keys=");
        let mut wrote_key = false;
        for key in map.keys() {
            if acc.head_is_full() && wrote_key {
                break;
            }
            if wrote_key {
                acc.push_str(",");
            }
            acc.push_str(key);
            wrote_key = true;
        }
        if !wrote_key {
            acc.truncate_to(prefix_len);
        }
    }
    acc.finish()
}

pub(crate) fn bounded_tool_result_text(content: &Value) -> Option<String> {
    bounded_tool_result_text_with_limit(content, MAX_AGENT_SEGMENT_CHARS)
}

/// The text of a tool result's string or text items, bounded to the head and
/// tail of `max_chars`.
pub(crate) fn bounded_tool_result_text_with_limit(
    content: &Value,
    max_chars: usize,
) -> Option<String> {
    let mut acc = BoundedHeadTail::new(max_chars);
    for text in tool_result_texts(content)? {
        acc.push_separator('\n');
        acc.push_str(text);
    }
    non_empty_text(&acc.finish())
}

/// A tool result's texts joined by line breaks, whole. `None` when they are
/// blank.
pub(crate) fn tool_result_text(content: &Value) -> Option<String> {
    unless_blank(tool_result_texts(content)?.join("\n"))
}

/// [`tool_result_text`] as the cached text and an opened session's search
/// read it: terminal codes are stripped from each text before they are
/// joined. The formats keep a result's codes, so the viewer can show them.
pub(crate) fn tool_result_search_text(content: &Value) -> Option<String> {
    let texts: Vec<String> = tool_result_texts(content)?
        .into_iter()
        .map(sanitize_agent_text)
        .collect();
    unless_blank(texts.join("\n"))
}

fn unless_blank(text: String) -> Option<String> {
    (!text.trim().is_empty()).then_some(text)
}

/// The texts a tool result holds: the string itself, or each string and
/// text block of an array. `None` for any other shape.
fn tool_result_texts(content: &Value) -> Option<Vec<&str>> {
    match content {
        Value::String(text) => Some(vec![text.as_str()]),
        Value::Array(items) => Some(
            items
                .iter()
                .filter_map(|item| match item {
                    Value::String(text) => Some(text.as_str()),
                    Value::Object(map) => {
                        let ty = map.get("type").and_then(|value| value.as_str());
                        if ty.is_none() || ty == Some("text") {
                            map.get("text").and_then(|value| value.as_str())
                        } else {
                            None
                        }
                    }
                    _ => None,
                })
                .collect(),
        ),
        _ => None,
    }
}

pub(crate) fn truncate_chars(text: &str, max_chars: usize) -> String {
    text.chars().take(max_chars).collect()
}

#[derive(Debug)]
pub(crate) struct BoundedHeadTail {
    max_chars: usize,
    head_chars: usize,
    tail_chars: usize,
    head: String,
    tail: std::collections::VecDeque<char>,
    seen_chars: usize,
    head_seen_chars: usize,
}

impl BoundedHeadTail {
    pub(crate) fn new(max_chars: usize) -> Self {
        let head_chars = max_chars.saturating_sub(max_chars / 4);
        let tail_chars = max_chars.saturating_sub(head_chars);
        Self {
            max_chars,
            head_chars,
            tail_chars,
            head: String::new(),
            tail: std::collections::VecDeque::new(),
            seen_chars: 0,
            head_seen_chars: 0,
        }
    }

    pub(crate) fn push_str(&mut self, text: &str) {
        for ch in text.chars() {
            self.push_char(ch);
        }
    }

    pub(crate) fn push_separator(&mut self, separator: char) {
        if self.seen_chars > 0 {
            self.push_char(separator);
        }
    }

    fn push_char(&mut self, ch: char) {
        if self.max_chars == 0 {
            self.seen_chars += 1;
            return;
        }
        if self.head_seen_chars < self.head_chars {
            self.head.push(ch);
            self.head_seen_chars += 1;
        } else if self.tail_chars > 0 {
            if self.tail.len() == self.tail_chars {
                self.tail.pop_front();
            }
            self.tail.push_back(ch);
        }
        self.seen_chars += 1;
    }

    fn finish(self) -> String {
        if self.seen_chars <= self.max_chars {
            let mut output = self.head;
            output.extend(self.tail);
            return output;
        }
        if self.max_chars == 0 {
            return String::new();
        }
        let head_len = self.head.chars().count();
        let mut tail = self.tail;
        let mut include_separator = !tail.is_empty();
        while head_len + usize::from(include_separator) + tail.len() > self.max_chars {
            if include_separator && head_len + tail.len() <= self.max_chars {
                include_separator = false;
                break;
            }
            if tail.pop_front().is_none() {
                include_separator = false;
                break;
            }
        }
        if tail.is_empty() {
            include_separator = false;
        }
        let mut output = self.head;
        if include_separator {
            output.push(' ');
        }
        output.extend(tail);
        output
    }

    fn len_chars(&self) -> usize {
        self.seen_chars
    }

    fn head_is_full(&self) -> bool {
        self.head_seen_chars >= self.head_chars
    }

    fn truncate_to(&mut self, len: usize) {
        if len >= self.seen_chars {
            return;
        }
        let current = self.clone_string();
        self.head.clear();
        self.tail.clear();
        self.seen_chars = 0;
        self.head_seen_chars = 0;
        self.push_str(&current.chars().take(len).collect::<String>());
    }

    fn clone_string(&self) -> String {
        let mut output = self.head.clone();
        output.extend(self.tail.iter().copied());
        output
    }
}

fn source(
    role: AgentMessageRole,
    timestamp: Option<String>,
    jsonl_line: usize,
    part_index: usize,
    assistant_message_id: Option<String>,
    parent_tool_use_id: Option<String>,
    tool_name: Option<String>,
) -> AgentPartSource {
    AgentPartSource {
        role,
        timestamp,
        jsonl_line,
        part_index,
        assistant_message_id,
        parent_tool_use_id,
        tool_name,
    }
}

/// A failure to read the transcript at `path`: I/O as I/O, anything else as
/// a malformed transcript.
fn transcript_error(path: &Path, error: AppError) -> AppError {
    let reference = path.to_string_lossy();
    match error {
        AppError::Io(error) => AgentError::io(
            Some(&reference),
            format!("failed to read transcript: {error}"),
        )
        .into(),
        error => AgentError::malformed_transcript(Some(&reference), error.to_string()).into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::test_support::{assistant_jsonl_line as assistant, user_jsonl_line as user};
    use std::io::Cursor;

    fn parse(content: &str) -> AgentTranscript {
        AgentTranscript::from_reader(PathBuf::from("test.jsonl"), Cursor::new(content))
            .expect("transcript should parse")
    }

    fn resolved() -> ResolvedConversation {
        let key = crate::agent::refs::AgentConversationKey::new(
            "project-a",
            "test.jsonl",
            PathBuf::from("test.jsonl"),
        );
        ResolvedConversation {
            reference: key.conversation_ref(),
            key,
        }
    }

    #[test]
    fn report_carries_its_text_and_finish_notice_doesnt() {
        let notification = |fields: &str| {
            user(&format!(
                "<task-notification>\n<task-id>a1</task-id>\n<status>completed</status>\n{fields}</task-notification>"
            ))
        };
        let transcript = parse(
            &[
                notification(
                    "<summary>Agent \"scout\" handed back its report</summary>\n<result>The tree holds src.</result>\n",
                ),
                notification("<summary>Agent \"scout\" finished</summary>\n"),
            ]
            .join("\n"),
        );

        let reports = transcript
            .messages
            .iter()
            .map(|message| (message.is_report, message.report_has_text))
            .collect::<Vec<_>>();

        assert_eq!(reports, [(true, true), (true, false)]);
    }

    #[test]
    fn message_anchor_survives_unrelated_prepended_messages() {
        let target = user("durable target");
        let original = parse(&target);
        let prepended = parse(&[user("unrelated"), target].join("\n"));
        let resolved = resolved();

        assert_eq!(
            original.message_anchor(&resolved, &original.messages[0]),
            prepended.message_anchor(&resolved, &prepended.messages[1])
        );
    }

    #[test]
    fn duplicate_content_anchor_is_ambiguous() {
        let transcript = parse(&[user("duplicate"), user("duplicate")].join("\n"));
        let resolved = resolved();
        let anchor = transcript.message_anchor(&resolved, &transcript.messages[0]);

        let error = transcript.resolve_anchor(&resolved, &anchor).unwrap_err();

        assert!(matches!(
            error,
            crate::error::AppError::Agent(AgentError {
                kind: AgentErrorKind::AmbiguousRef,
                ..
            })
        ));
    }

    #[test]
    fn canonical_ordinals_ignore_metadata_warmup_clear_and_progress() {
        let content = [
            r#"{"type":"summary","summary":"summary"}"#.to_string(),
            user("Warmup"),
            assistant("Ready"),
            user("Caveat: The messages below were generated by the user while running local commands."),
            user("<command-name>/clear</command-name>"),
            user("<local-command-stdout></local-command-stdout>"),
            r#"{"type":"progress","data":{"type":"agent_progress","agentId":"a1"}}"#.to_string(),
            user("real question"),
            assistant("real answer"),
            user("<command-message>consult</command-message><command-name>/consult</command-name><command-args>topic</command-args>"),
        ]
        .join("\n");

        let transcript = parse(&content);
        assert_eq!(transcript.messages.len(), 3);
        assert_eq!(transcript.messages[0].ordinal, 1);
        assert_eq!(transcript.messages[1].ordinal, 2);
        assert_eq!(transcript.messages[2].ordinal, 3);
        assert!(matches!(
            &transcript.messages[2].parts[0],
            AgentMessagePart::Text { text, .. } if text == "/consult topic"
        ));
    }

    #[test]
    fn malformed_lines_are_skipped_without_consuming_ordinals() {
        let content = [
            user("first"),
            "{malformed".to_string(),
            assistant("second"),
            "not json".to_string(),
            user("third"),
        ]
        .join("\n");

        let transcript = parse(&content);

        assert_eq!(transcript.malformed_lines, vec![2, 4]);
        assert_eq!(
            transcript
                .messages
                .iter()
                .map(|message| message.ordinal)
                .collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        assert_eq!(
            transcript.malformed_warning_detail().as_deref(),
            Some("skipped 2 malformed JSONL record(s) at lines 2,4")
        );
    }

    /// The error names the agent the key attributed the file to and the
    /// lines it could not read.
    #[test]
    fn wholly_malformed_transcript_is_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("test.jsonl");
        std::fs::write(&path, "{malformed\nnot json").unwrap();

        let error = AgentTranscript::parse_uncached(
            crate::history::FOR_FIXTURES,
            crate::history::Source::Claude,
            &path,
            &[],
        )
        .unwrap_err();

        let crate::error::AppError::Agent(AgentError { kind, detail, .. }) = error else {
            panic!("an agent error: {error:?}");
        };
        assert_eq!(
            kind,
            crate::agent::diagnostic::AgentErrorKind::MalformedTranscript
        );
        assert_eq!(detail, "not a Claude transcript; malformed lines: 1,2");
    }

    #[test]
    fn agent_progress_entries_use_subagent_visibility() {
        let content = [
            user("question"),
            r#"{"type":"progress","data":{"type":"agent_progress","agentId":"agent-abcdef","message":{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"subagent hidden text"}]}}}}"#.to_string(),
            assistant("answer"),
        ]
        .join("\n");

        let transcript = parse(&content);

        assert_eq!(transcript.messages.len(), 3);
        assert_eq!(transcript.messages[1].ordinal, 2);
        assert_eq!(
            transcript.messages[1].parent_tool_use_id.as_deref(),
            Some("agent-abcdef")
        );
        assert!(matches!(
            &transcript.messages[1].parts[0],
            AgentMessagePart::Text { text, .. } if text == "subagent hidden text"
        ));
        assert_eq!(transcript.messages[2].ordinal, 3);
    }

    /// With sub-agents, the agent CLI reads a session through the Claude
    /// reader.
    #[test]
    fn sub_agents_skill_text_doesnt_show_as_message() {
        use crate::history::skill_text::test_support::SUB_AGENT_SKILL_CALL_LOAD;

        let dir = tempfile::tempdir().unwrap();
        let session = dir.path().join("session.jsonl");
        std::fs::write(&session, user("Review the change")).unwrap();
        let subagent = dir.path().join("agent-a1111111111111111.jsonl");
        let reply = r#"{"type":"assistant","timestamp":"2026-10-03T12:00:07.000Z","message":{"role":"assistant","content":[{"type":"text","text":"The change reads well."}]}}"#;
        std::fs::write(
            &subagent,
            [SUB_AGENT_SKILL_CALL_LOAD.as_slice(), &[reply]]
                .concat()
                .join("\n"),
        )
        .unwrap();

        let transcript = AgentTranscript::parse_uncached(
            crate::history::FOR_FIXTURES,
            crate::history::Source::Claude,
            &session,
            &[subagent],
        )
        .unwrap();

        let texts = transcript
            .messages
            .iter()
            .flat_map(|message| &message.parts)
            .filter_map(|part| match part {
                AgentMessagePart::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(texts, ["Review the change", "The change reads well."]);
        assert_eq!(
            transcript
                .messages
                .iter()
                .map(|message| message.ordinal)
                .collect::<Vec<_>>(),
            [1, 2, 3, 4],
            "the session's question, the sub-agent's `Skill` call, its result and its reply"
        );
    }

    /// One record of the reply `msg_1`, holding `content` alone, as Claude
    /// Code writes each part of a reply.
    fn reply_record(content: serde_json::Value) -> String {
        serde_json::json!({
            "type": "assistant",
            "timestamp": "2024-01-01T00:00:01Z",
            "message": {"id": "msg_1", "role": "assistant", "content": [content]}
        })
        .to_string()
    }

    fn tool_result_record(tool_use_id: &str) -> String {
        serde_json::json!({
            "type": "user",
            "timestamp": "2024-01-01T00:00:02Z",
            "message": {
                "role": "user",
                "content": [{"type": "tool_result", "tool_use_id": tool_use_id, "content": "ok"}]
            }
        })
        .to_string()
    }

    /// Each part of `message` with the line of its record: thinking and text
    /// by their text, a tool call by its tool's name, a result by its call id.
    fn part_lines(message: &AgentMessage) -> Vec<(&str, usize)> {
        message
            .parts
            .iter()
            .map(|part| match part {
                AgentMessagePart::Thinking { thinking, source } => {
                    (thinking.as_str(), source.jsonl_line)
                }
                AgentMessagePart::Text { text, source } => (text.as_str(), source.jsonl_line),
                AgentMessagePart::ToolUse { name, source, .. } => {
                    (name.as_str(), source.jsonl_line)
                }
                AgentMessagePart::ToolResult {
                    tool_use_id,
                    source,
                    ..
                } => (tool_use_id.as_str(), source.jsonl_line),
            })
            .collect()
    }

    fn ordinals(transcript: &AgentTranscript) -> Vec<usize> {
        transcript
            .messages
            .iter()
            .map(|message| message.ordinal)
            .collect()
    }

    #[test]
    fn records_sharing_assistant_id_join_one_message_in_record_order() {
        let content = [
            user("question"),
            reply_record(serde_json::json!({"type": "thinking", "thinking": "plan", "signature": "sig"})),
            reply_record(serde_json::json!({"type": "text", "text": "I'll check the cache first"})),
            reply_record(
                serde_json::json!({"type": "tool_use", "id": "toolu_1", "name": "Bash", "input": {"command": "ls"}}),
            ),
            user("next"),
        ]
        .join("\n");

        let transcript = parse(&content);

        assert_eq!(ordinals(&transcript), [1, 2, 3]);
        let reply = &transcript.messages[1];
        assert_eq!(
            part_lines(reply),
            [("plan", 2), ("I'll check the cache first", 3), ("Bash", 4)]
        );
    }

    #[test]
    fn later_tool_call_joins_its_reply_ahead_of_earlier_calls_result() {
        let content = [
            user("question"),
            reply_record(
                serde_json::json!({"type": "tool_use", "id": "toolu_a", "name": "Read", "input": {"file_path": "a.md"}}),
            ),
            tool_result_record("toolu_a"),
            reply_record(
                serde_json::json!({"type": "tool_use", "id": "toolu_b", "name": "Grep", "input": {"pattern": "b"}}),
            ),
            tool_result_record("toolu_b"),
        ]
        .join("\n");

        let transcript = parse(&content);

        assert_eq!(ordinals(&transcript), [1, 2, 3, 4]);
        assert_eq!(
            part_lines(&transcript.messages[1]),
            [("Read", 2), ("Grep", 4)]
        );
        assert_eq!(part_lines(&transcript.messages[2]), [("toolu_a", 3)]);
        assert_eq!(part_lines(&transcript.messages[3]), [("toolu_b", 5)]);
    }

    #[test]
    fn agent_search_text_ignores_non_text_tool_result_json() {
        let blocks = vec![ContentBlock::ToolResult {
            tool_use_id: "toolu_1".to_string(),
            standalone_tool_name: None,
            content: Some(serde_json::json!({"secret":"object_needle"})),
        }];

        let text = agent_search_parts_from_blocks(AgentMessageRole::User, &blocks).join(" ");

        assert!(text.is_empty());
    }

    #[test]
    fn agent_search_text_caps_long_name_and_input_keys_line() {
        let mut input = serde_json::Map::new();
        for index in 0..MAX_AGENT_SEGMENT_CHARS {
            input.insert(format!("long_key_{index}"), Value::Bool(true));
        }
        let blocks = vec![ContentBlock::ToolUse {
            id: "toolu_1".to_string(),
            name: "Bash".to_string(),
            tool: crate::log_entry::Tool::Shell,
            input: Value::Object(input),
        }];

        let text = agent_search_parts_from_blocks(AgentMessageRole::Assistant, &blocks).join(" ");

        assert!(text.chars().count() <= MAX_AGENT_SEGMENT_CHARS);
        assert!(text.starts_with("tool Bash input_keys="));
    }

    fn shell_input(command: &str) -> Value {
        serde_json::json!({"command": command, "description": "Run the checks"})
    }

    #[test]
    fn agent_read_shows_tool_calls_header_on_one_line() {
        let grep = serde_json::json!({"pattern": "first\nsecond", "path": "src"});

        for lines in [CommandLines::FirstLine, CommandLines::All] {
            assert_eq!(
                tool_call_read_text("Bash", Tool::Shell, &shell_input("cargo test"), lines),
                "Bash: cargo test"
            );
            assert_eq!(
                tool_call_read_text("Grep", Tool::Grep, &grep, lines),
                r#"Grep: "first second" in src"#
            );
        }
    }

    #[test]
    fn read_of_several_messages_shows_commands_first_non_empty_line_and_counts_others() {
        let read = |command| {
            tool_call_read_text(
                "Bash",
                Tool::Shell,
                &shell_input(command),
                CommandLines::FirstLine,
            )
        };

        assert_eq!(read("cd src\ncargo test"), "Bash: cd src (+1 line)");
        assert_eq!(
            read("cd src\ncargo test\ncargo clippy"),
            "Bash: cd src (+2 lines)"
        );
        assert_eq!(read("\ncargo test"), "Bash: cargo test (+1 line)");
    }

    #[test]
    fn one_message_read_shows_command_whole_and_untruncated() {
        let read = |command| tool_call_read_text("Bash", Tool::Shell, &command, CommandLines::All);
        let long = "x".repeat(MAX_AGENT_SEGMENT_CHARS * 2);

        assert_eq!(
            read(shell_input("cd src\r\ncargo test\n\ncargo clippy")),
            "Bash: cd src\n  cargo test\n\n  cargo clippy"
        );
        assert_eq!(read(shell_input(&long)), format!("Bash: {long}"));
    }

    #[test]
    fn search_reads_every_line_of_command() {
        assert_eq!(
            tool_call_text("Bash", Tool::Shell, &shell_input("cd src\ncargo test")),
            "Bash: cd src\ncargo test"
        );
    }

    #[test]
    fn call_viewer_shows_as_raw_input_reads_as_its_name_and_input_keys() {
        let input = serde_json::json!({"todos": [{"content": "ship it"}]});

        for text in [
            tool_call_read_text("TodoWrite", Tool::Other, &input, CommandLines::FirstLine),
            tool_call_read_text("TodoWrite", Tool::Other, &input, CommandLines::All),
            tool_call_text("TodoWrite", Tool::Other, &input),
        ] {
            assert_eq!(text, "tool TodoWrite input_keys=todos");
        }
    }

    #[test]
    fn read_of_several_messages_truncates_command_longer_than_search_segment_and_search_reads_it_whole()
     {
        let command = shell_input(&"x".repeat(MAX_AGENT_SEGMENT_CHARS * 2));

        let read = tool_call_read_text("Bash", Tool::Shell, &command, CommandLines::FirstLine);
        let searched = tool_call_text("Bash", Tool::Shell, &command);

        assert_eq!(read.chars().count(), MAX_AGENT_SEGMENT_CHARS);
        assert!(read.starts_with("Bash: xxx"), "{}", &read[..20]);
        assert_eq!(
            searched.chars().count(),
            "Bash: ".len() + MAX_AGENT_SEGMENT_CHARS * 2
        );
    }

    #[test]
    fn agent_search_text_keeps_long_tool_result_whole() {
        let long = format!(
            "HEAD{}MIDDLE{}TAIL",
            "x".repeat(MAX_AGENT_SEGMENT_CHARS),
            "y".repeat(MAX_AGENT_SEGMENT_CHARS)
        );
        let blocks = vec![ContentBlock::ToolResult {
            tool_use_id: "toolu_1".to_string(),
            standalone_tool_name: None,
            content: Some(Value::String(long.clone())),
        }];

        let text = agent_search_parts_from_blocks(AgentMessageRole::User, &blocks).join(" ");

        assert_eq!(text, long);
    }

    #[test]
    fn agent_search_text_joins_every_text_item_of_tool_result_array() {
        let blocks = vec![ContentBlock::ToolResult {
            tool_use_id: "toolu_1".to_string(),
            standalone_tool_name: None,
            content: Some(Value::Array(vec![
                Value::String("HEAD".to_string()),
                Value::String("x".repeat(MAX_AGENT_SEGMENT_CHARS * 2)),
                serde_json::json!({"type":"image","source":"IMAGE_DATA"}),
                serde_json::json!({"type":"text","text":"TAIL"}),
            ])),
        }];

        let text = agent_search_parts_from_blocks(AgentMessageRole::User, &blocks).join(" ");

        assert_eq!(
            text,
            format!("HEAD\n{}\nTAIL", "x".repeat(MAX_AGENT_SEGMENT_CHARS * 2))
        );
    }

    #[test]
    fn bounded_tool_result_limit_preserves_head_and_tail() {
        let content = Value::String(format!("HEAD{}TAIL", "x".repeat(200)));

        let text = bounded_tool_result_text_with_limit(&content, 32).unwrap();

        assert_eq!(text.chars().count(), 32);
        assert!(text.starts_with("HEAD"));
        assert!(text.ends_with("TAIL"));
    }

    #[test]
    fn name_and_input_keys_stops_before_late_keys() {
        let mut input = serde_json::Map::new();
        for index in 0..MAX_AGENT_SEGMENT_CHARS {
            input.insert(format!("key_{index:05}"), Value::Bool(true));
        }

        let text = name_and_input_keys("Bash", &Value::Object(input), 128);

        assert!(text.chars().count() <= 128);
        assert!(text.starts_with("tool Bash input_keys=key_00000"));
        assert!(!text.contains("key_10000"));
    }

    #[test]
    fn bounded_head_tail_preserves_exact_limit_text() {
        let mut acc = BoundedHeadTail::new(4);
        acc.push_str("abcd");

        assert_eq!(acc.finish(), "abcd");
    }

    #[test]
    fn bounded_head_tail_handles_zero_limit() {
        let mut acc = BoundedHeadTail::new(0);
        acc.push_str("abcd");

        assert!(acc.finish().is_empty());
    }

    #[test]
    fn bounded_head_tail_respects_small_limits() {
        for max in 0..=8 {
            let mut acc = BoundedHeadTail::new(max);
            acc.push_str("αβγδεζηθ");

            let output = acc.finish();

            assert!(
                output.chars().count() <= max,
                "max {max} produced {output:?}"
            );
            assert!(
                !output.ends_with(' '),
                "max {max} produced dangling separator in {output:?}"
            );
            if max >= 4 {
                assert!(
                    output.ends_with('θ'),
                    "max {max} lost tail evidence in {output:?}"
                );
            }
        }
    }

    #[test]
    fn preserves_part_level_metadata_for_mixed_messages() {
        let content = [
            user("question"),
            serde_json::json!({
                "type": "assistant",
                "timestamp": "2024-01-01T00:00:01Z",
                "parent_tool_use_id": "toolu_parent",
                "message": {
                    "id": "msg_2",
                    "role": "assistant",
                    "content": [
                        {"type": "thinking", "thinking": "plan", "signature": "sig"},
                        {"type": "text", "text": "answer"},
                        {"type": "tool_use", "id": "toolu_1", "name": "Bash", "input": {"command": "ls"}}
                    ]
                }
            })
            .to_string(),
            serde_json::json!({
                "type": "user",
                "timestamp": "2024-01-01T00:00:02Z",
                "parent_tool_use_id": "toolu_parent",
                "message": {
                    "role": "user",
                    "content": [
                        {"type": "text", "text": "tool response"},
                        {"type": "tool_result", "tool_use_id": "toolu_1", "content": "ok"}
                    ]
                }
            })
            .to_string(),
        ]
        .join("\n");

        let transcript = parse(&content);
        let assistant = &transcript.messages[1];
        assert_eq!(assistant.role, AgentMessageRole::Assistant);
        assert!(
            assistant
                .parts
                .iter()
                .all(|part| part.source().timestamp.as_deref() == Some("2024-01-01T00:00:01Z"))
        );
        assert_eq!(
            assistant.parent_tool_use_id.as_deref(),
            Some("toolu_parent")
        );
        assert!(matches!(
            &assistant.parts[0],
            AgentMessagePart::Thinking { thinking, source }
                if thinking == "plan"
                    && source.part_index == 0
                    && source.assistant_message_id.as_deref() == Some("msg_2")
        ));
        assert!(matches!(
            &assistant.parts[2],
            AgentMessagePart::ToolUse { id, name, source, .. }
                if id == "toolu_1"
                    && name == "Bash"
                    && source.tool_name.as_deref() == Some("Bash")
                    && source.parent_tool_use_id.as_deref() == Some("toolu_parent")
        ));

        let user = &transcript.messages[2];
        assert!(matches!(
            &user.parts[1],
            AgentMessagePart::ToolResult { tool_use_id, content, source }
                if tool_use_id == "toolu_1"
                    && content.as_ref().and_then(|v| v.as_str()) == Some("ok")
                    && source.role == AgentMessageRole::User
                    && source.jsonl_line == 3
        ));
    }

    /// A task report reaches the agent CLI and MCP as its summary and its
    /// report, as a skill invocation reaches them as its command line.
    #[test]
    fn task_report_reads_as_its_summary_and_report() {
        use crate::history::task_notification::test_support::*;
        let content = [user("Verify the claims"), user(AGENT_REPORT)].join("\n");

        let transcript = parse(&content);

        assert_eq!(transcript.messages.len(), 2);
        let AgentMessagePart::Text { text, .. } = &transcript.messages[1].parts[0] else {
            panic!("{:?}", transcript.messages[1].parts);
        };
        assert!(
            text.starts_with(&format!("{AGENT_SUMMARY}\n\nVerified against source")),
            "{text}"
        );
        assert!(text.ends_with(AGENT_REPORT_LAST_LINE));
        assert!(!text.contains("task-id"), "{text}");

        let search_text = agent_search_parts_from_blocks(
            AgentMessageRole::User,
            &[ContentBlock::Text {
                text: AGENT_REPORT.to_owned(),
            }],
        )
        .join(" ");
        assert!(search_text.starts_with(AGENT_SUMMARY), "{search_text}");
        assert!(!search_text.contains("output-file"), "{search_text}");
    }

    #[test]
    fn task_report_is_report_and_ide_diagnostics_and_prompt_are_not() {
        use crate::history::task_notification::test_support::*;
        use crate::history::task_notification::{ReportKind, ReportSender, TaskReport};
        let diagnostics = TaskReport {
            kind: ReportKind::Diagnostics,
            summary: "Found 1 new diagnostic issue in 1 file".to_owned(),
            usage: None,
            body: None,
            sender: ReportSender::default(),
        }
        .notification_text();
        let content = [
            user("Verify the claims"),
            user(AGENT_REPORT),
            user(BACKGROUND_COMMAND),
            user(&diagnostics),
        ]
        .join("\n");

        let transcript = parse(&content);

        let reports = transcript
            .messages
            .iter()
            .map(|message| message.is_report)
            .collect::<Vec<_>>();
        assert_eq!(reports, [false, true, true, false]);
    }

    /// Two Claude sub-agents of one agent type share a label; each spliced
    /// turn names its transcript and its launching call.
    #[test]
    fn two_sub_agents_sharing_label_are_two_threads_each_naming_its_launch() {
        let turn = |transcript: &str, text: &str, launch_call: &str| {
            let record = RecordLine {
                transcript: Some(std::sync::Arc::from(Path::new(transcript))),
                line: 1,
            };
            let entry = LogEntry::Progress {
                data: serde_json::json!({
                    "type": "agent_progress",
                    "agentId": "general-purpose",
                    "identity": {"agentId": transcript},
                    "message": {
                        "type": "assistant",
                        "message": {"role": "assistant", "content": [{"type": "text", "text": text}]}
                    }
                }),
                extra: serde_json::json!({
                    "timestamp": "2026-10-14T15:41:30Z",
                    "parentToolUseID": launch_call
                }),
            };
            (record, entry)
        };
        let entries = vec![
            turn("agent-a1.jsonl", "first sub-agent", "toolu_a"),
            turn("agent-b2.jsonl", "second sub-agent", "toolu_b"),
        ];

        let transcript =
            AgentTranscript::from_entries(Path::new("session.jsonl"), entries, Vec::new()).unwrap();

        let origins = transcript
            .messages
            .iter()
            .map(|message| message.subagent.as_deref().unwrap())
            .collect::<Vec<_>>();
        assert_ne!(origins[0].key(), origins[1].key());
        assert_eq!(origins[0].launch_call.as_deref(), Some("toolu_a"));
        assert_eq!(origins[1].launch_call.as_deref(), Some("toolu_b"));
        assert_eq!(
            origins[1].identity.agent_id.as_deref(),
            Some("agent-b2.jsonl")
        );
    }

    /// A sub-agent's transcript spliced into its parent's reads as progress
    /// records.
    #[test]
    fn task_report_in_sub_agents_transcript_is_report() {
        use crate::history::task_notification::test_support::AGENT_REPORT;
        let progress = |text: &str| {
            serde_json::json!({
                "type": "progress",
                "data": {
                    "type": "agent_progress",
                    "agentId": "agent-abcdef",
                    "message": {
                        "type": "user",
                        "message": {"role": "user", "content": [{"type": "text", "text": text}]}
                    }
                }
            })
            .to_string()
        };
        let content = [
            user("question"),
            progress("Check the release notes"),
            progress(AGENT_REPORT),
        ]
        .join("\n");

        let transcript = parse(&content);

        let reports = transcript
            .messages
            .iter()
            .map(|message| message.is_report)
            .collect::<Vec<_>>();
        assert_eq!(reports, [false, false, true]);
    }
}
