//! Claude Code transcripts: one [`LogEntry`] record per line, with no session
//! header. A sub-agent's transcript is the same JSONL, kept in the session's
//! `subagents/` directory beside an `agent-<id>.meta.json` sidecar.

mod attached_file;
mod hooks;
mod ide_diagnostics;
mod queued_command;
pub(crate) mod rename;
pub(crate) mod subagent_launch;
pub(crate) mod subagent_report;
mod system_text;

use super::dropped::{DroppedRecords, SkippedKinds};
use super::lines::visit_parsed_lines;
use super::splice::{SubagentThread, progress_entries, splice_by_timestamp};
use super::{EarlierTurns, RecordLine, SessionFormat, SessionHeader, SessionProjection};
use crate::cli::DebugLevel;
use crate::debug;
use crate::error::Result;
use crate::history::{
    Conversation, MalformedLine, MalformedLineDetail, ParseError, Source, TranscriptEntries,
    parser, skill_text,
};
use crate::log_entry::{ContentBlock, LogEntry, Tool, UserContent, UserMessage};
use crate::parallel::ParallelThreshold;
use rayon::prelude::*;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::collections::VecDeque;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

/// The directory under `<project>/<session-id>/` holding the session's
/// sub-agent transcripts, nested ones included.
pub(crate) const SUBAGENTS_DIR: &str = "subagents";
/// `agent-<id>.jsonl`, the name of a sub-agent transcript.
pub(crate) const SUBAGENT_FILE_PREFIX: &str = "agent-";

pub(crate) static CLAUDE_TRANSCRIPT: ClaudeTranscript = ClaudeTranscript;

pub(crate) struct ClaudeTranscript;

impl SessionFormat for ClaudeTranscript {
    /// Claude states no session id, start time or cwd in a header: the id is
    /// the file name, the start time the first record's, and the cwd the
    /// first user record's. `None` unless the file holds a Claude record.
    fn parse_transcript_counting(
        &self,
        path: &Path,
        mut dropped: DroppedRecords,
    ) -> Result<Option<SessionProjection>> {
        let transcript = transcript_entries_with(path, ParallelThreshold::DEFAULT, &mut dropped)?;
        if !holds_a_claude_record(&transcript.entries) {
            return Ok(None);
        }
        let entries = transcript.entries;
        let header = SessionHeader {
            version: 0,
            id: session_id_of(path).unwrap_or_default().to_owned(),
            timestamp: entries
                .iter()
                .find_map(|(_, entry)| entry.timestamp())
                .unwrap_or_default()
                .to_owned(),
            cwd: entries
                .iter()
                .find_map(|(_, entry)| match entry {
                    LogEntry::User { cwd: Some(cwd), .. } => Some(PathBuf::from(cwd)),
                    _ => None,
                })
                .unwrap_or_default(),
            thread_label: None,
            subagent_identity: Default::default(),
        };
        Ok(Some(SessionProjection {
            source: Source::Claude,
            header,
            title: None,
            entries,
            leaf_id: None,
            malformed_lines: transcript
                .malformed_lines
                .iter()
                .map(|line| line.line_number)
                .collect(),
            dropped,
            earlier_turns: EarlierTurns::Read,
        }))
    }

    fn parse_conversation(
        &self,
        path: &Path,
        modified: Option<SystemTime>,
        debug_level: Option<DebugLevel>,
    ) -> Result<Option<Conversation>> {
        let reader = BufReader::new(File::open(path)?);
        conversation_from_reader(path, reader, modified, debug_level)
    }

    fn session_entries(
        &self,
        path: &Path,
        subagents: &[PathBuf],
    ) -> Result<Option<TranscriptEntries>> {
        let (session, transcripts) = if subagents.is_empty() {
            (transcript_entries(path), Vec::new())
        } else {
            rayon::join(
                || transcript_entries(path),
                || subagent_transcripts(subagents),
            )
        };
        let session = session?;
        if !holds_a_claude_record(&session.entries) {
            return Ok(None);
        }
        Ok(Some(with_subagents_spliced(session, transcripts)))
    }
}

/// The entries of each sub-agent transcript at `subagents` that reads, in
/// order, parsed on the rayon pool. One that cannot be read is left out.
fn subagent_transcripts(subagents: &[PathBuf]) -> Vec<(&PathBuf, Vec<(RecordLine, LogEntry)>)> {
    subagents
        .par_iter()
        .filter_map(|subagent| Some((subagent, transcript_entries(subagent).ok()?.entries)))
        .collect()
}

/// Every line of another agent's transcript reads as a [`LogEntry::Unknown`]
/// record or not at all.
fn holds_a_claude_record(entries: &[(RecordLine, LogEntry)]) -> bool {
    entries
        .iter()
        .any(|(_, entry)| !matches!(entry, LogEntry::Unknown))
}

/// The session id a Claude transcript's file name states: Claude names each
/// transcript `<session-id>.jsonl`, and writes no id into it.
pub(crate) fn session_id_of(path: &Path) -> Option<&str> {
    path.file_stem()?.to_str()
}

/// `session` with the sub-agent `transcripts` spliced in as `Progress`
/// entries, each under the label its sidecar names, without the records
/// repeating the `Agent` call that launched it, and with a delivered
/// `SubagentHandback` call folded into one record. The malformed lines are
/// the session's own; a sub-agent transcript's are not reported here, since
/// the view has no debug channel: the load reports it when the row is built.
fn with_subagents_spliced(
    session: TranscriptEntries,
    transcripts: Vec<(&PathBuf, Vec<(RecordLine, LogEntry)>)>,
) -> TranscriptEntries {
    let prompts = subagent_launch::agent_call_prompts(
        std::iter::once(&session.entries)
            .chain(transcripts.iter().map(|(_, entries)| entries))
            .flatten()
            .map(|(_, entry)| entry),
    );
    let threads = transcripts
        .into_iter()
        .map(|(subagent, entries)| {
            let sidecar = SubagentSidecar::read(subagent);
            let entries = subagent_report::fold_delivered_handbacks(
                subagent_report::without_handback_reminder(entries),
            );
            SubagentThread {
                label: subagent_label(subagent, &sidecar),
                transcript: Arc::from(subagent.as_path()),
                identity: Default::default(),
                started: entries
                    .iter()
                    .find_map(|(_, entry)| entry.timestamp())
                    .unwrap_or_default()
                    .to_owned(),
                entries: subagent_launch::without_launch_repeats(entries, &sidecar, &prompts),
            }
        })
        .collect();
    TranscriptEntries {
        entries: splice_by_timestamp(session.entries, progress_entries(threads)),
        malformed_lines: session.malformed_lines,
    }
}

/// One Claude transcript line, parsed by [`parse_record`].
enum ClaudeRecord {
    Entry(LogEntry),
    /// A `stop_hook_summary` record's errors, one `Hook` row each.
    Entries(Vec<LogEntry>),
    /// Placed by [`hooks::place_hook_attachments`] once the projection holds
    /// the whole transcript.
    HookAttachment(hooks::HookAttachment),
}

/// Parse one Claude transcript line into the entries the viewer and the row
/// both read.
fn parse_record(line: &str) -> serde_json::Result<ClaudeRecord> {
    let entry = serde_json::from_str::<LogEntry>(line)?;
    let record = match &entry {
        LogEntry::System { .. } => local_command_message(&entry)
            .or_else(|| system_text::system_text_row(&entry))
            .map(ClaudeRecord::Entry)
            .or_else(|| hooks::stop_hook_rows(&entry).map(ClaudeRecord::Entries)),
        LogEntry::Unknown => hooks::HookAttachment::parse(line)
            .map(ClaudeRecord::HookAttachment)
            .or_else(|| {
                queued_command::queued_message(line)
                    .or_else(|| attached_file::attached_file_message(line))
                    .or_else(|| ide_diagnostics::diagnostics_report(line))
                    .map(ClaudeRecord::Entry)
            }),
        _ => None,
    };
    Ok(record.unwrap_or(ClaudeRecord::Entry(entry)))
}

/// The row for the Claude transcript at `path`, read from `reader` and built
/// from the entries the viewer shows. Each malformed line keeps its text and
/// two lines either side of it for `--debug`.
pub(crate) fn conversation_from_reader(
    path: &Path,
    reader: impl BufRead,
    modified: Option<SystemTime>,
    debug_level: Option<DebugLevel>,
) -> Result<Option<Conversation>> {
    let transcript = transcript_entries_from(
        reader,
        &SubagentSidecars::of_transcript(path),
        ParallelThreshold::DEFAULT,
        &mut DroppedRecords::default(),
    )?;
    let parse_errors = transcript
        .malformed_lines
        .into_iter()
        .filter_map(|line| parse_error(path, line, debug_level))
        .collect();
    let conversation = parser::conversation_from_entries(
        Source::Claude,
        path.to_path_buf(),
        transcript.entries.into_iter().map(|(_, entry)| entry),
        parse_errors,
        modified,
        debug_level,
    );
    Ok(conversation.map(|conversation| Conversation {
        session_id: session_id_of(path).unwrap_or_default().to_owned(),
        ..conversation
    }))
}

/// `line` as the row's parse error, reported at warn level. `None` for a
/// line recorded without its text.
fn parse_error(
    path: &Path,
    line: MalformedLine,
    debug_level: Option<DebugLevel>,
) -> Option<ParseError> {
    let detail = line.detail?;
    debug::warn(
        debug_level,
        &format!(
            "Parse error in {} at line {}: {}",
            path.file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("unknown"),
            line.line_number,
            detail.error_message
        ),
    );
    Some(ParseError {
        line_number: line.line_number,
        line_content: detail.line_content,
        error_message: detail.error_message,
        context_before: detail.context_before,
        context_after: detail.context_after,
    })
}

/// `None` for any other record, and for a `local_command` record without
/// `content`.
fn local_command_message(entry: &LogEntry) -> Option<LogEntry> {
    let LogEntry::System { subtype, extra, .. } = entry else {
        return None;
    };
    if subtype != "local_command" {
        return None;
    }
    let field = |name: &str| extra.get(name).and_then(Value::as_str).map(str::to_owned);
    Some(user_record(UserContent::String(field("content")?), field))
}

/// A user message holding `content`. `field` reads its `timestamp`, `uuid` and
/// `cwd` from the record it replaces.
fn user_record(content: UserContent, field: impl Fn(&str) -> Option<String>) -> LogEntry {
    LogEntry::User {
        message: UserMessage {
            role: "user".to_owned(),
            content,
        },
        timestamp: field("timestamp"),
        uuid: field("uuid"),
        cwd: field("cwd"),
        parent_tool_use_id: None,
        source_tool_use_id: None,
        usage: None,
    }
}

/// Normalizes a Claude record for every reader: assigns each tool call's
/// canonical tool, replaces a background launch's receipt with one line, and
/// replaces a hand-back message with the report it carries, described from
/// `sidecars`.
pub(crate) fn normalize_entry(entry: &mut LogEntry, sidecars: &SubagentSidecars) {
    assign_canonical_tools(entry);
    subagent_launch::replace_background_launch_receipt(entry);
    subagent_report::replace_handback_message(entry, sidecars);
}

/// One Claude transcript, with no sub-agent transcript spliced in, each entry
/// normalized by [`normalize_entry`] and repeated skill text dropped. Skill
/// text is dropped before splicing because a spliced sub-agent turn does not
/// carry `sourceToolUseID`.
pub(crate) fn transcript_entries(path: &Path) -> Result<TranscriptEntries> {
    transcript_entries_with(
        path,
        ParallelThreshold::DEFAULT,
        &mut DroppedRecords::default(),
    )
}

/// [`transcript_entries`], its lines parsed on the rayon pool for a batch
/// `parallel` admits, with [`count_dropped_records`] counting into `dropped`.
fn transcript_entries_with(
    path: &Path,
    parallel: ParallelThreshold,
    dropped: &mut DroppedRecords,
) -> Result<TranscriptEntries> {
    let reader = BufReader::new(File::open(path)?);
    transcript_entries_from(
        reader,
        &SubagentSidecars::of_transcript(path),
        parallel,
        dropped,
    )
}

/// Lines a malformed line keeps on each side of it, for `--debug`.
const CONTEXT_LINES: usize = 2;

/// [`transcript_entries_with`] for the transcript `reader` holds, its
/// hand-backs described from `sidecars`.
fn transcript_entries_from(
    reader: impl BufRead,
    sidecars: &SubagentSidecars,
    parallel: ParallelThreshold,
    dropped: &mut DroppedRecords,
) -> Result<TranscriptEntries> {
    let mut records = Vec::new();
    let mut malformed_lines = Vec::new();
    let mut lines_before = VecDeque::with_capacity(CONTEXT_LINES + 1);
    visit_parsed_lines(
        reader,
        1,
        parallel,
        |line| {
            parse_record(line).map(|mut record| {
                if let ClaudeRecord::Entry(entry) = &mut record {
                    normalize_entry(entry, sidecars);
                }
                record
            })
        },
        |line_number, line, parsed| {
            add_context_after(&mut malformed_lines, &line);
            match parsed {
                Ok(record) => {
                    if dropped.is_counting()
                        && let ClaudeRecord::Entry(entry) = &record
                    {
                        count_dropped_records(&line, entry, dropped);
                    }
                    records.push((RecordLine::from(line_number), record));
                }
                Err(error) => malformed_lines.push(MalformedLine {
                    line_number,
                    detail: Some(MalformedLineDetail {
                        line_content: line.clone(),
                        error_message: error.to_string(),
                        context_before: lines_before.iter().cloned().collect(),
                        context_after: Vec::new(),
                    }),
                }),
            }
            lines_before.push_back(line);
            if lines_before.len() > CONTEXT_LINES {
                lines_before.pop_front();
            }
        },
    )?;
    Ok(TranscriptEntries {
        entries: skill_text::without_repeated_skill_text(hooks::place_hook_attachments(records)),
        malformed_lines,
    })
}

/// `line` as context after each of the last malformed lines still short of
/// [`CONTEXT_LINES`] after it.
fn add_context_after(malformed_lines: &mut [MalformedLine], line: &str) {
    for detail in malformed_lines
        .iter_mut()
        .rev()
        .take(CONTEXT_LINES)
        .filter_map(|malformed| malformed.detail.as_mut())
    {
        if detail.context_after.len() < CONTEXT_LINES {
            detail.context_after.push(line.to_owned());
        }
    }
}

/// Count a record with no [`LogEntry`] variant, a `system` record, and each
/// content block with no [`ContentBlock`] variant, under the Claude Code
/// version the record states. Attachments are counted by `attachment.type`
/// and `system` records by `subtype`, because Claude Code shows or hides each
/// record by that field.
fn count_dropped_records(line: &str, entry: &LogEntry, dropped: &mut DroppedRecords) {
    let Ok(raw) = serde_json::from_str::<Value>(line) else {
        return;
    };
    dropped.record_version(|| {
        raw.get("version")
            .and_then(Value::as_str)
            .map(str::to_owned)
    });
    match entry {
        LogEntry::System { subtype, .. } => SKIPPED_SYSTEM_SUBTYPES.count(Some(subtype), dropped),
        LogEntry::Unknown => match raw.get("type").and_then(Value::as_str) {
            Some("attachment") if queued_command::is_from_coordinator(&raw) => dropped.skipped(
                SKIPPED_ATTACHMENT_TYPES.field,
                Some(queued_command::ATTACHMENT_TYPE),
            ),
            Some("attachment") if hooks::is_stop_hook_error(&raw) => dropped.skipped(
                SKIPPED_ATTACHMENT_TYPES.field,
                Some(hooks::ERROR_ATTACHMENT_TYPE),
            ),
            Some("attachment") => SKIPPED_ATTACHMENT_TYPES.count(
                raw.pointer("/attachment/type").and_then(Value::as_str),
                dropped,
            ),
            record_type => SKIPPED_RECORD_TYPES.count(record_type, dropped),
        },
        _ => count_dropped_content_blocks(&raw, dropped),
    }
}

/// Count each block with no [`ContentBlock`] variant in a message's `content`
/// or a queued command's `prompt`.
fn count_dropped_content_blocks(raw: &Value, dropped: &mut DroppedRecords) {
    let Some(blocks) = raw
        .pointer("/message/content")
        .or_else(|| raw.pointer("/attachment/prompt"))
        .and_then(Value::as_array)
    else {
        return;
    };
    for block in blocks {
        if matches!(
            serde_json::from_value::<ContentBlock>(block.clone()),
            Ok(ContentBlock::Other)
        ) {
            SKIPPED_CONTENT_TYPES.count(block.get("type").and_then(Value::as_str), dropped);
        }
    }
}

/// Record types holding state Claude Code restores on resume or rewind, or
/// pointing to content shown elsewhere.
const SKIPPED_RECORD_TYPES: SkippedKinds = SkippedKinds {
    field: "type",
    values: &[
        "last-prompt",
        "mode",
        "cost-state",
        "atis-latch",
        "worktree-state",
        "relocated",
        "file-history-delta",
        "queue-operation",
        "fork-context-ref",
        "pr-link",
        "frame-link",
    ],
};

/// Attachment types with no conversation content: context for the model,
/// status lines, and records Claude Code does not show.
const SKIPPED_ATTACHMENT_TYPES: SkippedKinds = SkippedKinds {
    field: "attachment.type",
    values: &[
        "hook_success",
        "hook_additional_context",
        "command_permissions",
        "edited_text_file",
        "opened_file_in_ide",
        "plan_mode",
        "plan_mode_exit",
        "auto_mode",
        "auto_mode_exit",
        "ultrathink_effort",
        "deferred_tools_delta",
        "mcp_instructions_delta",
        "task_reminder",
        "date_change",
        "output_style",
        "skill_listing",
        "agent_listing_delta",
        "invoked_skills",
        "plan_file_reference",
        "task_status",
        "total_tokens_reminder",
        "batching_reminder_sent",
        "silent_turn_reminder",
        "bash_output_audience_note",
        "environment",
        "model",
        "instructions",
        "session_context",
        "date",
        "prompt_snapshot",
        "deferred_tools_record",
        "output_style_instructions",
        "credential_org",
        "thinking_drop",
        "read_truncation_notice",
        "skill_mention",
    ],
};

/// `system` subtypes that are status lines, or notices another record covers:
/// a failed request's final assistant message, a compaction's summary.
const SKIPPED_SYSTEM_SUBTYPES: SkippedKinds = SkippedKinds {
    field: "system.subtype",
    values: &[
        "turn_duration",
        "agents_killed",
        "api_error",
        "compact_boundary",
        // A summary listing no error; one listing errors reads as `Hook` rows.
        "stop_hook_summary",
        // A refusal with no fallback holds no text; the API error reply after
        // it shows the refusal.
        "model_refusal_no_fallback",
    ],
};

/// Content block types with no text to show: `fallback` marks where a reply
/// switched model.
const SKIPPED_CONTENT_TYPES: SkippedKinds = SkippedKinds {
    field: "content.type",
    values: &["fallback"],
};

/// The `agent-<id>.meta.json` sidecar beside a sub-agent transcript. An
/// absent or unreadable sidecar reads as the default: no type, no launching
/// call, not a fork.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SubagentSidecar {
    #[serde(default)]
    agent_type: Option<String>,
    /// The `description` of the `Agent` call that launched the sub-agent.
    #[serde(default)]
    description: Option<String>,
    /// The id of the parent's `Agent` call that launched the sub-agent.
    #[serde(default)]
    pub(crate) tool_use_id: Option<String>,
    #[serde(default)]
    pub(crate) is_fork: bool,
}

impl SubagentSidecar {
    pub(crate) fn read(transcript: &Path) -> Self {
        std::fs::read(transcript.with_extension("meta.json"))
            .ok()
            .and_then(|sidecar| serde_json::from_slice(&sidecar).ok())
            .unwrap_or_default()
    }
}

/// The sidecars of one session's sub-agents. A nested sub-agent's sidecar
/// sits in the same `subagents/` directory as the others.
pub(crate) struct SubagentSidecars {
    directory: PathBuf,
}

impl SubagentSidecars {
    /// The sidecars for the agents `transcript` names: `<session>/subagents/`
    /// for a session, the directory holding it for a sub-agent's transcript.
    pub(crate) fn of_transcript(transcript: &Path) -> Self {
        let directory = match transcript.parent() {
            Some(parent) if parent.file_name().is_some_and(|name| name == SUBAGENTS_DIR) => {
                parent.to_path_buf()
            }
            _ => transcript.with_extension("").join(SUBAGENTS_DIR),
        };
        Self { directory }
    }

    /// The launching call's description that agent `agent_id`'s sidecar
    /// records. `None` for an id that is not an agent id, so a message's
    /// text never names a path outside `subagents/`.
    pub(crate) fn description(&self, agent_id: &str) -> Option<String> {
        if agent_id.is_empty() || !agent_id.chars().all(|c| c.is_ascii_alphanumeric()) {
            return None;
        }
        let transcript = self
            .directory
            .join(format!("{SUBAGENT_FILE_PREFIX}{agent_id}.jsonl"));
        SubagentSidecar::read(&transcript)
            .description
            .filter(|description| !description.is_empty())
    }
}

/// The label a sub-agent transcript splices in under: the `agentType` its
/// sidecar records (`Explore`, `general-purpose`), or the agent id from the
/// file name when the sidecar names none.
pub(crate) fn subagent_label(transcript: &Path, sidecar: &SubagentSidecar) -> String {
    let agent_type = sidecar
        .agent_type
        .clone()
        .filter(|agent_type| !agent_type.is_empty());
    agent_type.unwrap_or_else(|| {
        transcript
            .file_stem()
            .and_then(|stem| stem.to_str())
            .map(|stem| stem.strip_prefix(SUBAGENT_FILE_PREFIX).unwrap_or(stem))
            .unwrap_or_default()
            .to_owned()
    })
}

/// Claude's tool names mapped onto the canonical [`Tool`] set.
///
/// Claude's records deserialize straight into [`LogEntry`] with every `tool`
/// at `Other`, so this runs on each entry after deserializing. Sub-agent
/// turns inside a `Progress` payload are assigned in place in the JSON, which
/// keeps the rest of the payload as written.
pub(crate) fn assign_canonical_tools(entry: &mut LogEntry) {
    match entry {
        LogEntry::Assistant { message, .. } => {
            for block in &mut message.content {
                if let ContentBlock::ToolUse {
                    name, tool, input, ..
                } = block
                {
                    *tool = canonical_tool(name);
                    canonicalize_input(*tool, input);
                }
            }
        }
        LogEntry::Progress { data, .. } => {
            for block in agent_progress_tool_use_blocks(data) {
                let Some(name) = block.get("name").and_then(Value::as_str) else {
                    continue;
                };
                let tool = canonical_tool(name);
                if let Some(input) = block.get_mut("input") {
                    canonicalize_input(tool, input);
                }
                block.insert("tool".to_owned(), json!(tool));
            }
        }
        _ => {}
    }
}

fn canonical_tool(name: &str) -> Tool {
    match name {
        "Bash" | "PowerShell" => Tool::Shell,
        "Read" => Tool::Read,
        "Edit" => Tool::Edit,
        "Write" => Tool::Write,
        "Grep" => Tool::Grep,
        "Glob" => Tool::Glob,
        // Claude Code no longer has `LS`; older sessions hold its calls.
        "LS" => Tool::DirectoryListing,
        "WebFetch" => Tool::WebFetch,
        "WebSearch" => Tool::WebSearch,
        "Skill" => Tool::Skill,
        "Task" | "Agent" => Tool::Agent,
        "SendMessage" => Tool::AgentMessage,
        "SubagentHandback" => Tool::AgentReport,
        "TaskOutput" => Tool::Wait,
        "TaskCreate" | "TaskUpdate" | "TodoWrite" => Tool::TaskList,
        _ => Tool::Other,
    }
}

/// Claude's inputs already use the canonical keys, except that `SendMessage`
/// addresses its `recipient` as `to`.
fn canonicalize_input(tool: Tool, input: &mut Value) {
    if tool == Tool::AgentMessage
        && let Some(object) = input.as_object_mut()
        && let Some(recipient) = object.remove("to")
    {
        object.insert("recipient".to_owned(), recipient);
    }
}

/// The `tool_use` blocks of an `agent_progress` payload, at the path
/// [`parse_agent_progress`](crate::log_entry::parse_agent_progress) reads them from.
fn agent_progress_tool_use_blocks(
    data: &mut Value,
) -> impl Iterator<Item = &mut Map<String, Value>> {
    data.pointer_mut("/message/message/content")
        .and_then(Value::as_array_mut)
        .into_iter()
        .flatten()
        .filter_map(Value::as_object_mut)
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("tool_use"))
}

#[cfg(test)]
pub(crate) mod test_support {
    pub(crate) const SLASH_COMMAND_AS_USER_RECORDS: [&str; 2] = [
        r#"{"type":"user","timestamp":"2026-10-01T10:00:00Z","cwd":"/repo","message":{"role":"user","content":"<command-name>/model</command-name>\n<command-message>model</command-message>\n<command-args></command-args>"}}"#,
        r#"{"type":"user","timestamp":"2026-10-01T10:00:01Z","cwd":"/repo","message":{"role":"user","content":"<local-command-stdout>Set model to Opus 5</local-command-stdout>"}}"#,
    ];

    pub(crate) const SLASH_COMMAND_AS_SYSTEM_RECORDS: [&str; 2] = [
        r#"{"type":"system","subtype":"local_command","level":"info","timestamp":"2026-10-01T10:00:00Z","cwd":"/repo","uuid":"a1","content":"<command-name>/model</command-name>\n<command-message>model</command-message>\n<command-args></command-args>"}"#,
        r#"{"type":"system","subtype":"local_command","level":"info","timestamp":"2026-10-01T10:00:01Z","cwd":"/repo","uuid":"a2","content":"<local-command-stdout>Set model to Opus 5</local-command-stdout>"}"#,
    ];

    pub(crate) const FAILED_SLASH_COMMAND_AS_SYSTEM_RECORDS: [&str; 2] = [
        r#"{"type":"system","subtype":"local_command","level":"info","timestamp":"2026-10-01T10:00:00Z","cwd":"/repo","uuid":"b1","content":"<command-name>/compact</command-name>\n<command-message>compact</command-message>\n<command-args></command-args>"}"#,
        r#"{"type":"system","subtype":"local_command","level":"info","timestamp":"2026-10-01T10:00:01Z","cwd":"/repo","uuid":"b2","content":"<local-command-stderr>Error during compaction: API Error: Connection lost mid-response.</local-command-stderr>"}"#,
    ];

    /// A prompt, then two prompts typed while Claude was working, as queued
    /// commands: one with an `origin`, one with a `commandMode` and no
    /// `origin`, holding an image.
    pub(crate) const QUEUED_PROMPTS_AS_ATTACHMENTS: [&str; 3] = [
        r#"{"type":"user","timestamp":"2026-10-01T10:00:00Z","cwd":"/repo","uuid":"q0","message":{"role":"user","content":"fix the build"}}"#,
        r#"{"type":"attachment","timestamp":"2026-10-01T10:00:05Z","cwd":"/repo","uuid":"q1","attachment":{"type":"queued_command","prompt":"and run the tests","commandMode":"prompt","origin":{"kind":"human"}}}"#,
        r#"{"type":"attachment","timestamp":"2026-10-01T10:00:09Z","cwd":"/repo","uuid":"q2","attachment":{"type":"queued_command","prompt":[{"type":"text","text":"this screen"},{"type":"image","source":{"type":"base64","media_type":"image/png","data":"iVBORw0KGgo="}}],"commandMode":"prompt"}}"#,
    ];

    pub(crate) const QUEUED_PROMPTS_AS_USER_RECORDS: [&str; 3] = [
        r#"{"type":"user","timestamp":"2026-10-01T10:00:00Z","cwd":"/repo","uuid":"q0","message":{"role":"user","content":"fix the build"}}"#,
        r#"{"type":"user","timestamp":"2026-10-01T10:00:05Z","cwd":"/repo","uuid":"q1","message":{"role":"user","content":"and run the tests"}}"#,
        r#"{"type":"user","timestamp":"2026-10-01T10:00:09Z","cwd":"/repo","uuid":"q2","message":{"role":"user","content":[{"type":"text","text":"this screen"},{"type":"image","source":{"type":"base64","media_type":"image/png","data":"iVBORw0KGgo="}}]}}"#,
    ];

    /// A prompt attaching a directory and a file with `@`.
    pub(crate) const ATTACHED_WITH_AT: [&str; 3] = [
        r#"{"type":"user","timestamp":"2026-10-01T10:00:00Z","cwd":"/repo","uuid":"f0","message":{"role":"user","content":"compare @plans and @src/main.rs"}}"#,
        r#"{"type":"attachment","timestamp":"2026-10-01T10:00:00Z","cwd":"/repo","uuid":"f1","attachment":{"type":"directory","path":"/repo/plans","content":"PLAN.md\nnotes.md","displayPath":"plans"}}"#,
        r#"{"type":"attachment","timestamp":"2026-10-01T10:00:00Z","cwd":"/repo","uuid":"f2","attachment":{"type":"file","filename":"/repo/src/main.rs","content":{"type":"text","file":{"filePath":"/repo/src/main.rs","content":"fn main() { attached_sentinel(); }","numLines":1,"startLine":1,"totalLines":1}},"displayPath":"src/main.rs"}}"#,
    ];

    /// A compaction's summary, then the files Claude Code attaches again: two
    /// files, and a reference to one too large to attach.
    pub(crate) const ATTACHED_AGAIN_AFTER_COMPACTION: [&str; 5] = [
        r#"{"type":"system","subtype":"compact_boundary","timestamp":"2026-10-01T11:00:00Z","uuid":"c0","content":"Conversation compacted"}"#,
        r#"{"type":"user","timestamp":"2026-10-01T11:00:00Z","cwd":"/repo","uuid":"c1","isCompactSummary":true,"message":{"role":"user","content":"This session is being continued from a previous conversation."}}"#,
        r#"{"type":"attachment","timestamp":"2026-10-01T11:00:00Z","cwd":"/repo","uuid":"c2","attachment":{"type":"file","filename":"/repo/src/a.rs","content":{"type":"text","file":{"filePath":"/repo/src/a.rs","content":"mod a;","numLines":1,"startLine":1,"totalLines":1}},"displayPath":"src/a.rs"}}"#,
        r#"{"type":"attachment","timestamp":"2026-10-01T11:00:00Z","cwd":"/repo","uuid":"c3","attachment":{"type":"file","filename":"/repo/src/b.rs","content":{"type":"text","file":{"filePath":"/repo/src/b.rs","content":"mod b;","numLines":1,"startLine":1,"totalLines":1}},"displayPath":"src/b.rs"}}"#,
        r#"{"type":"attachment","timestamp":"2026-10-01T11:00:00Z","cwd":"/repo","uuid":"c4","attachment":{"type":"compact_file_reference","filename":"/repo/src/big.rs","displayPath":"src/big.rs"}}"#,
    ];

    /// A compaction's summary, then file references and files attached again
    /// in turn, as Claude Code writes them.
    pub(crate) const INTERLEAVED_AFTER_COMPACTION: [&str; 6] = [
        r#"{"type":"user","timestamp":"2026-10-01T11:00:00Z","cwd":"/repo","uuid":"i0","isCompactSummary":true,"message":{"role":"user","content":"This session is being continued from a previous conversation."}}"#,
        r#"{"type":"attachment","timestamp":"2026-10-01T11:00:00Z","cwd":"/repo","uuid":"i1","attachment":{"type":"compact_file_reference","filename":"/repo/src/big.rs","displayPath":"src/big.rs"}}"#,
        r#"{"type":"attachment","timestamp":"2026-10-01T11:00:00Z","cwd":"/repo","uuid":"i2","attachment":{"type":"file","filename":"/repo/src/a.rs","content":{"type":"text","file":{"filePath":"/repo/src/a.rs","content":"mod a;","numLines":1,"startLine":1,"totalLines":1}},"displayPath":"src/a.rs"}}"#,
        r#"{"type":"attachment","timestamp":"2026-10-01T11:00:00Z","cwd":"/repo","uuid":"i3","attachment":{"type":"compact_file_reference","filename":"/repo/src/huge.rs","displayPath":"src/huge.rs"}}"#,
        r#"{"type":"attachment","timestamp":"2026-10-01T11:00:00Z","cwd":"/repo","uuid":"i4","attachment":{"type":"file","filename":"/repo/src/b.rs","content":{"type":"text","file":{"filePath":"/repo/src/b.rs","content":"mod b;","numLines":1,"startLine":1,"totalLines":1}},"displayPath":"src/b.rs"}}"#,
        r#"{"type":"attachment","timestamp":"2026-10-01T11:00:00Z","cwd":"/repo","uuid":"i5","attachment":{"type":"compact_file_reference","filename":"/repo/src/vast.rs","displayPath":"src/vast.rs"}}"#,
    ];

    /// Two shell calls, each with a `PreToolUse` hook's message or error
    /// between the call and its result.
    pub(crate) const HOOKED_SHELL_CALLS: [&str; 8] = [
        r#"{"type":"user","timestamp":"2026-10-01T12:00:00Z","cwd":"/repo","uuid":"k0","message":{"role":"user","content":"list and build"}}"#,
        r#"{"type":"assistant","timestamp":"2026-10-01T12:00:01Z","uuid":"k1","message":{"role":"assistant","id":"msg_k1","content":[{"type":"tool_use","id":"toolu_ls","name":"Bash","input":{"command":"ls"}}]}}"#,
        r#"{"type":"attachment","timestamp":"2026-10-01T12:00:02Z","cwd":"/repo","uuid":"k2","attachment":{"type":"hook_success","content":"","hookName":"PreToolUse:Bash","toolUseID":"toolu_ls","hookEvent":"PreToolUse"}}"#,
        r#"{"type":"attachment","timestamp":"2026-10-01T12:00:02Z","cwd":"/repo","uuid":"k3","attachment":{"type":"hook_system_message","content":"[bash-guard] SAFE: a read-only listing","hookName":"PreToolUse:Bash","toolUseID":"toolu_ls","hookEvent":"PreToolUse"}}"#,
        r#"{"type":"user","timestamp":"2026-10-01T12:00:03Z","cwd":"/repo","uuid":"k4","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_ls","content":"a.rs\nb.rs"}]}}"#,
        r#"{"type":"assistant","timestamp":"2026-10-01T12:00:04Z","uuid":"k5","message":{"role":"assistant","id":"msg_k5","content":[{"type":"tool_use","id":"toolu_build","name":"Bash","input":{"command":"cargo build"}}]}}"#,
        r#"{"type":"attachment","timestamp":"2026-10-01T12:00:05Z","cwd":"/repo","uuid":"k6","attachment":{"type":"hook_non_blocking_error","hookName":"PreToolUse:Bash","toolUseID":"toolu_build","hookEvent":"PreToolUse","stderr":"Failed with non-blocking status code: node: command not found","stdout":"","exitCode":127}}"#,
        r#"{"type":"user","timestamp":"2026-10-01T12:00:06Z","cwd":"/repo","uuid":"k7","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_build","content":"Finished dev profile"}]}}"#,
    ];

    pub(crate) const HOOK_MESSAGE_TEXT: &str =
        "PreToolUse:Bash hook: [bash-guard] SAFE: a read-only listing";
    pub(crate) const HOOK_ERROR_TEXT: &str =
        "PreToolUse:Bash hook error: Failed with non-blocking status code: node: command not found";

    /// A prompt and a `UserPromptSubmit` hook's message with a `toolUseID`
    /// naming no call.
    pub(crate) const HOOK_MESSAGE_WITHOUT_A_CALL: [&str; 2] = [
        r#"{"type":"user","timestamp":"2026-10-01T12:10:00Z","cwd":"/repo","uuid":"n0","message":{"role":"user","content":"fix the parser"}}"#,
        r#"{"type":"attachment","timestamp":"2026-10-01T12:10:00Z","cwd":"/repo","uuid":"n1","attachment":{"type":"hook_system_message","content":"Run the tests before you finish.","hookName":"UserPromptSubmit","toolUseID":"9f3c2a17-hook","hookEvent":"UserPromptSubmit"}}"#,
    ];

    pub(crate) const STOP_HOOK_ERROR: &str = r#"{"type":"attachment","timestamp":"2026-10-01T12:20:00Z","cwd":"/repo","uuid":"s1","attachment":{"type":"hook_non_blocking_error","hookName":"Stop","toolUseID":"5d1e-stop","hookEvent":"Stop","stderr":"Failed with non-blocking status code: notify: not found","stdout":"","exitCode":127}}"#;

    /// The `stop_hook_summary` record listing [`STOP_HOOK_ERROR`]'s error, in
    /// Claude Code 2.1.88's shape.
    pub(crate) const STOP_HOOK_SUMMARY: &str = r#"{"type":"system","subtype":"stop_hook_summary","hookCount":1,"hookInfos":[{"command":"notify done"}],"hookErrors":["Failed with non-blocking status code: notify: not found"],"preventedContinuation":false,"stopReason":"","hasOutput":true,"level":"suggestion","timestamp":"2026-10-01T12:20:01Z","uuid":"s2","toolUseID":"5d1e-stop"}"#;

    /// Five issues in two files: a body of seven lines, longer than a
    /// truncated body.
    pub(crate) const IDE_DIAGNOSTICS: &str = r#"{"type":"attachment","timestamp":"2026-10-01T13:00:00Z","cwd":"/repo","uuid":"d1","attachment":{"type":"diagnostics","isNew":true,"files":[{"uri":"file:///repo/src/main.rs","diagnostics":[{"message":"cannot find value `x` in this scope","severity":"Error","range":{"start":{"line":2,"character":4},"end":{"line":2,"character":5}},"source":"rustc","code":"E0425"},{"message":"unused import: `std::fs`","severity":"Warning","range":{"start":{"line":0,"character":4},"end":{"line":0,"character":11}},"source":"rustc","code":"unused_imports"},{"message":"unused variable: `y`","severity":"Warning","range":{"start":{"line":8,"character":8},"end":{"line":8,"character":9}},"source":"rustc"}]},{"uri":"file:///repo/src/lib.rs","diagnostics":[{"message":"missing documentation","severity":"Information","range":{"start":{"line":0,"character":0},"end":{"line":0,"character":1}}},{"message":"mismatched types","severity":"Error","range":{"start":{"line":11,"character":15},"end":{"line":11,"character":20}},"source":"rustc","code":308}]}]}}"#;

    pub(crate) const IDE_DIAGNOSTICS_SUMMARY: &str = "Found 5 new diagnostic issues in 2 files";

    /// A prompt, a session recap ending in Claude Code's hint, a refusal with
    /// a fallback model, and a refusal with no fallback.
    pub(crate) const RECAP_AND_REFUSALS: [&str; 4] = [
        r#"{"type":"user","timestamp":"2026-10-01T14:00:00Z","cwd":"/repo","uuid":"r0","message":{"role":"user","content":"fix the parser"}}"#,
        r#"{"type":"system","subtype":"away_summary","timestamp":"2026-10-01T14:30:00Z","uuid":"r1","content":"Goal: fix the parser. Next: run the tests. (disable recaps in /config)"}"#,
        r#"{"type":"system","subtype":"model_refusal_fallback","level":"warning","timestamp":"2026-10-01T14:31:00Z","uuid":"r2","content":"Safeguards flagged this message; switched model."}"#,
        r#"{"type":"system","subtype":"model_refusal_no_fallback","level":"warning","timestamp":"2026-10-01T14:32:00Z","uuid":"r3","content":""}"#,
    ];

    pub(crate) const RECAP_TEXT: &str = "Goal: fix the parser. Next: run the tests.";
    pub(crate) const REFUSAL_NOTICE_TEXT: &str = "Safeguards flagged this message; switched model.";
}

#[cfg(test)]
mod tests {
    use super::test_support::{
        ATTACHED_AGAIN_AFTER_COMPACTION, ATTACHED_WITH_AT, FAILED_SLASH_COMMAND_AS_SYSTEM_RECORDS,
        HOOK_ERROR_TEXT, HOOK_MESSAGE_TEXT, HOOK_MESSAGE_WITHOUT_A_CALL, HOOKED_SHELL_CALLS,
        IDE_DIAGNOSTICS, IDE_DIAGNOSTICS_SUMMARY, QUEUED_PROMPTS_AS_ATTACHMENTS,
        QUEUED_PROMPTS_AS_USER_RECORDS, SLASH_COMMAND_AS_SYSTEM_RECORDS,
        SLASH_COMMAND_AS_USER_RECORDS, STOP_HOOK_ERROR, STOP_HOOK_SUMMARY,
    };
    use super::*;
    use crate::history::format::dropped::DropOutcome;
    use crate::log_entry::{UserContent, parse_agent_progress};

    /// A session resumed under a newer Claude Code mixes versions; each
    /// record counts under its own.
    #[test]
    fn counting_names_unknown_record_and_block_types_under_each_records_version() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("session.jsonl");
        std::fs::write(
            &path,
            concat!(
                r#"{"type":"user","version":"2.1.300","message":{"role":"user","content":[{"type":"text","text":"hi"},{"type":"hologram"}]}}"#,
                "\n",
                r#"{"type":"teleport","version":"2.1.300"}"#,
                "\n",
                r#"{"type":"teleport","version":"2.1.301"}"#,
                "\n",
            ),
        )
        .unwrap();

        let projection = CLAUDE_TRANSCRIPT
            .parse_transcript_counting(&path, DroppedRecords::counting())
            .unwrap()
            .unwrap();

        assert_eq!(projection.dropped.first_version(), Some("2.1.300"));
        assert_eq!(
            projection.dropped.counts().collect::<Vec<_>>(),
            [
                (
                    Some("2.1.300"),
                    "content.type=hologram",
                    DropOutcome::Unrecognised,
                    1
                ),
                (
                    Some("2.1.300"),
                    "type=teleport",
                    DropOutcome::Unrecognised,
                    1
                ),
                (
                    Some("2.1.301"),
                    "type=teleport",
                    DropOutcome::Unrecognised,
                    1
                ),
            ]
        );
    }

    #[test]
    fn counting_names_each_attachment_type_and_system_subtype() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("session.jsonl");
        std::fs::write(
            &path,
            [
                r#"{"type":"user","version":"2.1.300","message":{"role":"user","content":"hi"}}"#,
                r#"{"type":"attachment","version":"2.1.300","attachment":{"type":"hologram","filename":"a.rs"}}"#,
                r#"{"type":"attachment","version":"2.1.300","attachment":{"type":"hologram","filename":"b.rs"}}"#,
                r#"{"type":"attachment","version":"2.1.300","attachment":{"type":"teleport","filename":"c.rs"}}"#,
                r#"{"type":"attachment","version":"2.1.300"}"#,
                r#"{"type":"system","version":"2.1.300","subtype":"hologram","content":"glow"}"#,
                r#"{"type":"system","version":"2.1.300","subtype":"teleport","content":"blink"}"#,
            ]
            .join("\n"),
        )
        .unwrap();

        let projection = CLAUDE_TRANSCRIPT
            .parse_transcript_counting(&path, DroppedRecords::counting())
            .unwrap()
            .unwrap();

        let unrecognised = |kind, count| (Some("2.1.300"), kind, DropOutcome::Unrecognised, count);
        assert_eq!(
            projection.dropped.counts().collect::<Vec<_>>(),
            [
                unrecognised("attachment.type=(missing)", 1),
                unrecognised("attachment.type=hologram", 2),
                unrecognised("attachment.type=teleport", 1),
                unrecognised("system.subtype=hologram", 1),
                unrecognised("system.subtype=teleport", 1),
            ]
        );
    }

    fn write_session(directory: &Path, name: &str, records: &[&str]) -> PathBuf {
        let path = directory.join(name);
        std::fs::write(&path, records.join("\n")).unwrap();
        path
    }

    #[test]
    fn a_session_of_slash_commands_from_system_records_is_listed_as_from_user_records() {
        let directory = tempfile::tempdir().unwrap();
        let row = |name, records: &[&str]| {
            let path = write_session(directory.path(), name, records);
            CLAUDE_TRANSCRIPT
                .parse_conversation(&path, None, None)
                .unwrap()
                .expect("a session holding a slash command is listed")
        };

        let from_user_records = row("user.jsonl", &SLASH_COMMAND_AS_USER_RECORDS);
        let from_system_records = row("system.jsonl", &SLASH_COMMAND_AS_SYSTEM_RECORDS);

        assert_eq!(from_system_records.preview, from_user_records.preview);
        assert_eq!(from_system_records.full_text, from_user_records.full_text);
        assert_eq!(
            from_system_records.message_count,
            from_user_records.message_count
        );
        assert_eq!(from_system_records.timestamp, from_user_records.timestamp);
        assert!(
            from_system_records
                .full_text
                .contains("Set model to Opus 5"),
            "{}",
            from_system_records.full_text
        );
    }

    #[test]
    fn a_failed_slash_commands_error_is_searched_and_doesnt_become_the_preview() {
        let directory = tempfile::tempdir().unwrap();
        let path = write_session(
            directory.path(),
            "session.jsonl",
            &FAILED_SLASH_COMMAND_AS_SYSTEM_RECORDS,
        );

        let row = CLAUDE_TRANSCRIPT
            .parse_conversation(&path, None, None)
            .unwrap()
            .expect("a session holding a slash command is listed");

        assert_eq!(row.preview, "/compact");
        assert_eq!(row.message_count, 1);
        assert!(
            row.full_text.contains("Error during compaction"),
            "{}",
            row.full_text
        );
    }

    #[test]
    fn a_slash_command_from_a_system_record_doesnt_count_as_dropped() {
        let directory = tempfile::tempdir().unwrap();
        let path = write_session(
            directory.path(),
            "session.jsonl",
            &SLASH_COMMAND_AS_SYSTEM_RECORDS,
        );

        let projection = CLAUDE_TRANSCRIPT
            .parse_transcript_counting(&path, DroppedRecords::counting())
            .unwrap()
            .unwrap();

        assert_eq!(projection.dropped.counts().count(), 0);
    }

    #[test]
    fn a_queued_prompt_reads_as_the_user_record_holding_its_text() {
        let directory = tempfile::tempdir().unwrap();
        let row = |name, records: &[&str]| {
            let path = write_session(directory.path(), name, records);
            CLAUDE_TRANSCRIPT
                .parse_conversation(&path, None, None)
                .unwrap()
                .expect("a session holding a prompt is listed")
        };

        let from_user_records = row("user.jsonl", &QUEUED_PROMPTS_AS_USER_RECORDS);
        let from_attachments = row("queued.jsonl", &QUEUED_PROMPTS_AS_ATTACHMENTS);

        assert_eq!(from_attachments.preview, from_user_records.preview);
        assert_eq!(from_attachments.full_text, from_user_records.full_text);
        assert_eq!(
            from_attachments.message_count,
            from_user_records.message_count
        );
        assert!(
            from_attachments.full_text.contains("and run the tests"),
            "{}",
            from_attachments.full_text
        );
    }

    fn queued_command_record(origin: &str, prompt: impl Into<Value>) -> String {
        let prompt: Value = prompt.into();
        json!({
            "type": "attachment",
            "version": "2.1.300",
            "timestamp": "2026-10-01T10:00:05Z",
            "attachment": {"type": "queued_command", "prompt": prompt, "origin": {"kind": origin}},
        })
        .to_string()
    }

    fn user_texts(entries: &[(RecordLine, LogEntry)]) -> Vec<&str> {
        entries
            .iter()
            .filter_map(|(_, entry)| match entry {
                LogEntry::User { message, .. } => message.content.whole_text(),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_queued_task_notification_and_handback_read_as_task_rows() {
        use crate::history::parse_task_report;
        use crate::history::task_notification::test_support::AGENT_REPORT;

        let handback = "<agent-message from=\"a4444444444444444\">\n[Subagent hand-back] The text below is the final report of a subagent this session delegated to. The report follows:\n  The CLI runs.\n</agent-message>";
        let directory = tempfile::tempdir().unwrap();
        let path = write_session(
            directory.path(),
            "session.jsonl",
            &[
                queued_command_record("task-notification", AGENT_REPORT).as_str(),
                queued_command_record("peer", handback).as_str(),
            ],
        );

        let entries = transcript_entries(&path).unwrap().entries;
        let summaries = user_texts(&entries)
            .into_iter()
            .filter_map(parse_task_report)
            .map(|report| report.summary)
            .collect::<Vec<_>>();

        assert_eq!(
            summaries,
            [
                parse_task_report(AGENT_REPORT).unwrap().summary,
                "Agent handed back its report".to_owned(),
            ]
        );
    }

    #[test]
    fn a_coordinators_queued_message_is_skipped_and_an_unknown_senders_is_unrecognised() {
        let directory = tempfile::tempdir().unwrap();
        let path = write_session(
            directory.path(),
            "session.jsonl",
            &[
                r#"{"type":"user","version":"2.1.300","message":{"role":"user","content":"review the PR"}}"#,
                queued_command_record("coordinator", "also check the CHANGELOG").as_str(),
                queued_command_record("broadcast", "a sender rearview does not know").as_str(),
                queued_command_record("human", "and the README").as_str(),
            ],
        );

        let projection = CLAUDE_TRANSCRIPT
            .parse_transcript_counting(&path, DroppedRecords::counting())
            .unwrap()
            .unwrap();

        assert_eq!(
            user_texts(&projection.entries),
            ["review the PR", "and the README"]
        );
        let counted = |outcome| {
            (
                Some("2.1.300"),
                "attachment.type=queued_command",
                outcome,
                1,
            )
        };
        assert_eq!(
            projection.dropped.counts().collect::<Vec<_>>(),
            [
                counted(DropOutcome::Skipped),
                counted(DropOutcome::Unrecognised)
            ]
        );
    }

    #[test]
    fn an_unknown_block_in_a_queued_prompt_counts_as_unrecognised() {
        let directory = tempfile::tempdir().unwrap();
        let prompt = json!([{"type": "text", "text": "this screen"}, {"type": "hologram"}]);
        let path = write_session(
            directory.path(),
            "session.jsonl",
            &[queued_command_record("human", prompt).as_str()],
        );

        let projection = CLAUDE_TRANSCRIPT
            .parse_transcript_counting(&path, DroppedRecords::counting())
            .unwrap()
            .unwrap();

        assert_eq!(
            projection.dropped.counts().collect::<Vec<_>>(),
            [(
                Some("2.1.300"),
                "content.type=hologram",
                DropOutcome::Unrecognised,
                1
            )]
        );
    }

    /// The name, tool, input and result of each call the user's entries hold.
    fn user_calls(entries: &[(RecordLine, LogEntry)]) -> Vec<(&str, Tool, &Value, Option<&Value>)> {
        entries
            .iter()
            .filter_map(|(_, entry)| match entry {
                LogEntry::User {
                    message:
                        UserMessage {
                            content: UserContent::Blocks(blocks),
                            ..
                        },
                    ..
                } => match blocks.as_slice() {
                    [
                        ContentBlock::ToolUse {
                            name, tool, input, ..
                        },
                        rest @ ..,
                    ] => {
                        let result = match rest {
                            [ContentBlock::ToolResult { content, .. }] => content.as_ref(),
                            _ => None,
                        };
                        Some((name.as_str(), *tool, input, result))
                    }
                    _ => None,
                },
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_directory_and_a_file_attached_with_at_read_as_a_listing_and_a_read() {
        let directory = tempfile::tempdir().unwrap();
        let path = write_session(directory.path(), "session.jsonl", &ATTACHED_WITH_AT);

        let entries = transcript_entries(&path).unwrap().entries;

        assert_eq!(
            user_calls(&entries),
            [
                (
                    "List dir",
                    Tool::DirectoryListing,
                    &json!({"path": "plans"}),
                    Some(&json!("PLAN.md\nnotes.md")),
                ),
                (
                    "Read",
                    Tool::Read,
                    &json!({"file_path": "src/main.rs"}),
                    Some(&json!("fn main() { attached_sentinel(); }")),
                ),
            ]
        );
    }

    #[test]
    fn an_attached_files_content_is_searched() {
        let directory = tempfile::tempdir().unwrap();
        let path = write_session(directory.path(), "session.jsonl", &ATTACHED_WITH_AT);

        let row = CLAUDE_TRANSCRIPT
            .parse_conversation(&path, None, None)
            .unwrap()
            .expect("a session holding a prompt is listed");

        assert_eq!(row.preview, "compare @plans and @src/main.rs");
        assert!(
            row.full_text.contains("attached_sentinel"),
            "{}",
            row.full_text
        );
    }

    #[test]
    fn part_of_an_attached_file_reads_with_its_line_range() {
        let directory = tempfile::tempdir().unwrap();
        let partial = r#"{"type":"attachment","uuid":"p1","attachment":{"type":"file","filename":"/repo/src/main.rs","content":{"type":"text","file":{"filePath":"/repo/src/main.rs","content":"line ten","numLines":5,"startLine":10,"totalLines":40}},"displayPath":"src/main.rs"}}"#;
        let path = write_session(directory.path(), "session.jsonl", &[partial]);

        let entries = transcript_entries(&path).unwrap().entries;

        let [(_, _, input, _)] = user_calls(&entries)[..] else {
            panic!("one read: {entries:?}");
        };
        assert_eq!(
            input,
            &json!({"file_path": "src/main.rs", "offset": 10, "limit": 5})
        );
    }

    #[test]
    fn a_file_reference_reads_as_a_reference_call_without_a_result() {
        let directory = tempfile::tempdir().unwrap();
        let path = write_session(
            directory.path(),
            "session.jsonl",
            &ATTACHED_AGAIN_AFTER_COMPACTION,
        );

        let entries = transcript_entries(&path).unwrap().entries;

        let references = user_calls(&entries)
            .into_iter()
            .filter(|(_, tool, _, _)| *tool == Tool::FileReference)
            .collect::<Vec<_>>();
        assert_eq!(
            references,
            [(
                "Reference",
                Tool::FileReference,
                &json!({"file_path": "src/big.rs"}),
                None
            )]
        );
    }

    #[test]
    fn a_file_attached_as_an_image_reads_without_a_result() {
        let directory = tempfile::tempdir().unwrap();
        let image = r#"{"type":"attachment","uuid":"m1","attachment":{"type":"file","filename":"/repo/shot.png","content":{"type":"image","file":{"base64":"iVBORw0KGgo=","type":"image/png","originalSize":8}},"displayPath":"shot.png"}}"#;
        let path = write_session(directory.path(), "session.jsonl", &[image]);

        let entries = transcript_entries(&path).unwrap().entries;

        assert_eq!(
            user_calls(&entries),
            [("Read", Tool::Read, &json!({"file_path": "shot.png"}), None)]
        );
    }

    #[test]
    fn attached_files_directories_and_file_references_dont_count_as_dropped() {
        let directory = tempfile::tempdir().unwrap();
        let records = [
            ATTACHED_WITH_AT.as_slice(),
            ATTACHED_AGAIN_AFTER_COMPACTION.as_slice(),
        ]
        .concat();
        let path = write_session(directory.path(), "session.jsonl", &records);

        let projection = CLAUDE_TRANSCRIPT
            .parse_transcript_counting(&path, DroppedRecords::counting())
            .unwrap()
            .unwrap();

        assert_eq!(
            projection.dropped.counts().collect::<Vec<_>>(),
            [(
                None,
                "system.subtype=compact_boundary",
                DropOutcome::Skipped,
                1
            )]
        );
    }

    /// Each result's call ID and content, in file order.
    fn results(entries: &[(RecordLine, LogEntry)]) -> Vec<(&str, Option<&Value>)> {
        entries
            .iter()
            .flat_map(|(_, entry)| match entry {
                LogEntry::User {
                    message:
                        UserMessage {
                            content: UserContent::Blocks(blocks),
                            ..
                        },
                    ..
                } => blocks.as_slice(),
                _ => &[],
            })
            .filter_map(|block| match block {
                ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    ..
                } => Some((tool_use_id.as_str(), content.as_ref())),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_hook_message_or_error_heads_the_result_of_its_call() {
        let directory = tempfile::tempdir().unwrap();
        let path = write_session(directory.path(), "session.jsonl", &HOOKED_SHELL_CALLS);

        let entries = transcript_entries(&path).unwrap().entries;

        assert_eq!(
            results(&entries),
            [
                (
                    "toolu_ls",
                    Some(&json!(format!("{HOOK_MESSAGE_TEXT}\na.rs\nb.rs")))
                ),
                (
                    "toolu_build",
                    Some(&json!(format!("{HOOK_ERROR_TEXT}\nFinished dev profile")))
                ),
            ]
        );
    }

    #[test]
    fn a_hook_message_after_its_calls_result_follows_the_result() {
        let directory = tempfile::tempdir().unwrap();
        let post_tool_use = r#"{"type":"attachment","uuid":"k8","attachment":{"type":"hook_system_message","content":"formatted b.rs","hookName":"PostToolUse:Bash","toolUseID":"toolu_ls","hookEvent":"PostToolUse"}}"#;
        let path = write_session(
            directory.path(),
            "session.jsonl",
            &[&HOOKED_SHELL_CALLS[..5], &[post_tool_use]].concat(),
        );

        let entries = transcript_entries(&path).unwrap().entries;

        assert_eq!(
            results(&entries),
            [(
                "toolu_ls",
                Some(&json!(format!(
                    "{HOOK_MESSAGE_TEXT}\na.rs\nb.rs\nPostToolUse:Bash hook: formatted b.rs"
                )))
            )]
        );
    }

    #[test]
    fn a_hook_message_for_a_call_without_a_result_is_its_result() {
        let directory = tempfile::tempdir().unwrap();
        let path = write_session(directory.path(), "session.jsonl", &HOOKED_SHELL_CALLS[..4]);

        let entries = transcript_entries(&path).unwrap().entries;

        assert_eq!(
            results(&entries),
            [("toolu_ls", Some(&json!(HOOK_MESSAGE_TEXT)))]
        );
    }

    #[test]
    fn a_hook_message_naming_no_call_reads_as_a_hook_row() {
        let directory = tempfile::tempdir().unwrap();
        let path = write_session(
            directory.path(),
            "session.jsonl",
            &HOOK_MESSAGE_WITHOUT_A_CALL,
        );

        let entries = transcript_entries(&path).unwrap().entries;

        assert_eq!(
            labelled_rows(&entries),
            [("Hook", "UserPromptSubmit: Run the tests before you finish.")]
        );
    }

    /// Each labelled row's label and text, in file order.
    fn labelled_rows(entries: &[(RecordLine, LogEntry)]) -> Vec<(&str, &str)> {
        entries
            .iter()
            .filter_map(|(_, entry)| match entry {
                LogEntry::PiMetadata { label, text, .. } => Some((label.as_str(), text.as_str())),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_stop_hook_summarys_error_reads_as_a_hook_row_and_its_attachment_doesnt() {
        let directory = tempfile::tempdir().unwrap();
        let path = write_session(
            directory.path(),
            "session.jsonl",
            &[STOP_HOOK_ERROR, STOP_HOOK_SUMMARY],
        );

        let entries = transcript_entries(&path).unwrap().entries;

        assert_eq!(
            labelled_rows(&entries),
            [(
                "Hook",
                "Stop error: Failed with non-blocking status code: notify: not found"
            )]
        );
    }

    #[test]
    fn hook_messages_and_errors_are_searched() {
        let directory = tempfile::tempdir().unwrap();
        let path = write_session(directory.path(), "session.jsonl", &HOOKED_SHELL_CALLS);

        let row = CLAUDE_TRANSCRIPT
            .parse_conversation(&path, None, None)
            .unwrap()
            .expect("a session holding a prompt is listed");

        for text in [HOOK_MESSAGE_TEXT, HOOK_ERROR_TEXT] {
            assert!(row.full_text.contains(text), "{}", row.full_text);
        }
    }

    #[test]
    fn a_stop_hooks_error_attachment_is_skipped_and_shown_records_arent() {
        let directory = tempfile::tempdir().unwrap();
        let records = [
            HOOKED_SHELL_CALLS.as_slice(),
            &[STOP_HOOK_ERROR, STOP_HOOK_SUMMARY, IDE_DIAGNOSTICS],
        ]
        .concat();
        let path = write_session(directory.path(), "session.jsonl", &records);

        let projection = CLAUDE_TRANSCRIPT
            .parse_transcript_counting(&path, DroppedRecords::counting())
            .unwrap()
            .unwrap();

        let skipped = |kind| (None, kind, DropOutcome::Skipped, 1);
        assert_eq!(
            projection.dropped.counts().collect::<Vec<_>>(),
            [
                skipped("attachment.type=hook_non_blocking_error"),
                skipped("attachment.type=hook_success"),
            ]
        );
    }

    #[test]
    fn ide_diagnostics_read_as_an_issues_report_listing_each_file_and_issue() {
        use crate::history::parse_task_report;
        use crate::history::task_notification::ReportKind;

        let directory = tempfile::tempdir().unwrap();
        let path = write_session(directory.path(), "session.jsonl", &[IDE_DIAGNOSTICS]);

        let entries = transcript_entries(&path).unwrap().entries;

        let [report] = user_texts(&entries)
            .into_iter()
            .filter_map(parse_task_report)
            .collect::<Vec<_>>()
            .try_into()
            .unwrap();
        assert_eq!(report.kind, ReportKind::Diagnostics);
        assert_eq!(report.summary, IDE_DIAGNOSTICS_SUMMARY);
        assert_eq!(
            report.body.as_deref(),
            Some(concat!(
                "src/main.rs:\n",
                "[Line 3:5] cannot find value `x` in this scope [E0425] (rustc)\n",
                "[Line 1:5] unused import: `std::fs` [unused_imports] (rustc)\n",
                "[Line 9:9] unused variable: `y` (rustc)\n",
                "src/lib.rs:\n",
                "[Line 1:1] missing documentation\n",
                "[Line 12:16] mismatched types [308] (rustc)",
            ))
        );
    }

    #[test]
    fn ide_diagnostics_are_searched() {
        let directory = tempfile::tempdir().unwrap();
        let path = write_session(directory.path(), "session.jsonl", &[IDE_DIAGNOSTICS]);

        let row = CLAUDE_TRANSCRIPT
            .parse_conversation(&path, None, None)
            .unwrap()
            .expect("a session holding diagnostics is listed");

        assert!(
            row.full_text.contains("mismatched types"),
            "{}",
            row.full_text
        );
    }

    #[test]
    fn the_issues_count_line_counts_every_listed_file_and_issue() {
        use crate::history::parse_task_report;

        let directory = tempfile::tempdir().unwrap();
        let diagnostics = r#"{"type":"attachment","uuid":"d2","attachment":{"type":"diagnostics","files":[{"uri":"file:///repo/a.rs","diagnostics":[{"message":"no position"}]},{"uri":"file:///repo/b.rs","diagnostics":[]}]}}"#;
        let path = write_session(directory.path(), "session.jsonl", &[diagnostics]);

        let entries = transcript_entries(&path).unwrap().entries;

        let summaries = user_texts(&entries)
            .into_iter()
            .filter_map(parse_task_report)
            .map(|report| report.summary)
            .collect::<Vec<_>>();
        assert_eq!(summaries, ["Found 1 new diagnostic issue in 2 files"]);
    }

    #[test]
    fn the_rows_message_count_and_ordinals_match_the_transcripts_around_hooks() {
        use crate::agent::transcript::{AgentMessagePart, AgentTranscript};

        const REPLY: &str = "The listing and the build both finished without an error.";
        let directory = tempfile::tempdir().unwrap();
        let reply = format!(
            r#"{{"type":"assistant","timestamp":"2026-10-01T12:00:07Z","uuid":"k8","message":{{"role":"assistant","id":"msg_k8","content":[{{"type":"text","text":"{REPLY}"}}]}}}}"#
        );
        let path = write_session(
            directory.path(),
            "session.jsonl",
            &[HOOKED_SHELL_CALLS.as_slice(), &[reply.as_str()]].concat(),
        );

        let row = CLAUDE_TRANSCRIPT
            .parse_conversation(&path, None, None)
            .unwrap()
            .expect("a session holding a prompt is listed");
        let transcript = AgentTranscript::load(&path).unwrap();

        assert_eq!(row.message_count, transcript.messages.len());
        let reply_ordinal = transcript
            .messages
            .iter()
            .find(|message| {
                message.parts.iter().any(
                    |part| matches!(part, AgentMessagePart::Text { text, .. } if text == REPLY),
                )
            })
            .expect("the transcript holds the reply")
            .ordinal;
        let turn = row
            .semantic_turns
            .iter()
            .position(|turn| turn.contains(REPLY))
            .expect("the row embeds the reply");
        assert_eq!(row.semantic_turn_ranges[turn].start, reply_ordinal);
    }

    #[test]
    fn a_malformed_line_keeps_its_text_and_the_lines_around_it_in_the_row() {
        let directory = tempfile::tempdir().unwrap();
        let lines = [
            r#"{"type":"user","timestamp":"2026-10-01T12:00:00Z","message":{"role":"user","content":"before"}}"#,
            "not json",
            r#"{"type":"assistant","timestamp":"2026-10-01T12:00:01Z","message":{"role":"assistant","content":[{"type":"text","text":"after"}]}}"#,
        ];
        let path = write_session(directory.path(), "session.jsonl", &lines);

        let row = CLAUDE_TRANSCRIPT
            .parse_conversation(&path, None, None)
            .unwrap()
            .expect("a session holding a prompt is listed");

        let [error] = row.parse_errors.as_slice() else {
            panic!("one parse error: {:?}", row.parse_errors);
        };
        assert_eq!(
            (error.line_number, error.line_content.as_str()),
            (2, "not json")
        );
        assert_eq!(error.context_before, [lines[0]]);
        assert_eq!(error.context_after, [lines[2]]);
    }

    #[test]
    fn a_skipped_kind_counts_as_skipped_and_a_shown_kind_isnt_counted() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("session.jsonl");
        std::fs::write(
            &path,
            [
                r#"{"type":"user","version":"2.1.300","message":{"role":"user","content":"hi"}}"#,
                r#"{"type":"last-prompt","version":"2.1.300"}"#,
                r#"{"type":"teleport","version":"2.1.300"}"#,
                r#"{"type":"attachment","version":"2.1.300","attachment":{"type":"skill_listing"}}"#,
                r#"{"type":"system","version":"2.1.300","subtype":"turn_duration","durationMs":1200}"#,
                r#"{"type":"system","version":"2.1.300","subtype":"model_refusal_no_fallback","content":""}"#,
                r#"{"type":"system","version":"2.1.300","subtype":"informational","content":"notice"}"#,
                r#"{"type":"system","version":"2.1.300","subtype":"model_refusal_fallback","content":"switched"}"#,
                r#"{"type":"system","version":"2.1.300","subtype":"away_summary","content":"recap"}"#,
                r#"{"type":"assistant","version":"2.1.300","message":{"role":"assistant","content":[{"type":"text","text":"hello"},{"type":"fallback"}]}}"#,
            ]
            .join("\n"),
        )
        .unwrap();

        let projection = CLAUDE_TRANSCRIPT
            .parse_transcript_counting(&path, DroppedRecords::counting())
            .unwrap()
            .unwrap();

        let counted = |kind, outcome| (Some("2.1.300"), kind, outcome, 1);
        assert_eq!(
            projection.dropped.counts().collect::<Vec<_>>(),
            [
                counted("attachment.type=skill_listing", DropOutcome::Skipped),
                counted("content.type=fallback", DropOutcome::Skipped),
                counted(
                    "system.subtype=model_refusal_no_fallback",
                    DropOutcome::Skipped
                ),
                counted("system.subtype=turn_duration", DropOutcome::Skipped),
                counted("type=last-prompt", DropOutcome::Skipped),
                counted("type=teleport", DropOutcome::Unrecognised),
            ]
        );
    }

    #[test]
    fn a_recap_and_a_refusal_warning_are_searched_and_counted_once() {
        use crate::agent::transcript::AgentTranscript;
        use test_support::{RECAP_AND_REFUSALS, RECAP_TEXT, REFUSAL_NOTICE_TEXT};
        let directory = tempfile::tempdir().unwrap();
        let path = write_session(directory.path(), "session.jsonl", &RECAP_AND_REFUSALS);

        let row = CLAUDE_TRANSCRIPT
            .parse_conversation(&path, None, None)
            .unwrap()
            .expect("a session holding a prompt is listed");

        assert_eq!(row.full_text.matches(RECAP_TEXT).count(), 1);
        assert!(
            !row.full_text.contains("disable recaps"),
            "{}",
            row.full_text
        );
        assert_eq!(row.full_text.matches(REFUSAL_NOTICE_TEXT).count(), 1);
        assert_eq!(row.message_count, 3, "the prompt, the recap and the notice");
        assert_eq!(
            row.message_count,
            AgentTranscript::load(&path).unwrap().messages.len()
        );
    }

    const SUBAGENT_FIXTURE: &str = "tests/fixtures/claude/-tmp-claude-subagent-fixture/7b2f3c1e-4a5d-4e6f-8a9b-0c1d2e3f4a5b.jsonl";

    fn session_entries(path: &Path, subagents: &[PathBuf]) -> Vec<(RecordLine, LogEntry)> {
        CLAUDE_TRANSCRIPT
            .session_entries(path, subagents)
            .unwrap()
            .expect("a Claude transcript")
            .entries
    }

    /// One word per entry: the parent's Agent calls and their results by
    /// tool-use id, a spliced sub-agent turn by its label and role.
    fn shape_of(entry: &LogEntry) -> String {
        match entry {
            LogEntry::User { message, .. } => match &message.content {
                UserContent::Blocks(blocks) => blocks
                    .iter()
                    .find_map(|block| match block {
                        ContentBlock::ToolResult { tool_use_id, .. } => {
                            Some(format!("result:{tool_use_id}"))
                        }
                        _ => None,
                    })
                    .unwrap_or_else(|| "user".to_owned()),
                UserContent::String(_) => "user".to_owned(),
            },
            LogEntry::Assistant { message, .. } => message
                .content
                .iter()
                .find_map(|block| match block {
                    ContentBlock::ToolUse { id, .. } => Some(format!("call:{id}")),
                    _ => None,
                })
                .unwrap_or_else(|| "assistant".to_owned()),
            LogEntry::Progress { data, .. } => {
                let progress = parse_agent_progress(data).expect("a spliced sub-agent turn");
                format!("{}:{}", progress.agent_id, progress.message.message_type)
            }
            other => panic!("unexpected entry {other:?}"),
        }
    }

    /// Each sub-agent's turns land between the Agent call that ran it and
    /// that call's result, under the `agentType` its sidecar names; the nested
    /// sub-agent's turns land among the turns of the sub-agent that ran it.
    /// Each sub-agent's opening message repeats its Agent call's prompt and is
    /// dropped.
    #[test]
    fn a_claude_sessions_sub_agent_turns_splice_in_under_their_agent_type() {
        let transcript = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(SUBAGENT_FIXTURE);
        let subagents = crate::history::provider::claude::subagent_transcripts(&transcript, None);
        assert_eq!(subagents.len(), 3);

        let shape = session_entries(&transcript, &subagents)
            .iter()
            .map(|(_, entry)| shape_of(entry))
            .collect::<Vec<_>>();

        assert_eq!(
            shape,
            [
                "user",
                "call:toolu_01FIXTUREAAAAAAAAAAAAAAA",
                "Explore:assistant",
                "Explore:user",
                "Explore:assistant",
                "result:toolu_01FIXTUREAAAAAAAAAAAAAAA",
                "call:toolu_01FIXTUREBBBBBBBBBBBBBBB",
                "general-purpose:assistant",
                "Explore:assistant",
                "general-purpose:user",
                "general-purpose:assistant",
                "result:toolu_01FIXTUREBBBBBBBBBBBBBBB",
                "user",
                "assistant",
            ]
        );
        assert_eq!(
            session_entries(&transcript, &[]).len(),
            7,
            "without the sub-agent transcripts the session's own entries stand alone"
        );
    }

    /// The launch session's entries as JSON: the spliced sub-agent turns, then
    /// the session's own records.
    fn launch_session_records() -> (Vec<String>, Vec<String>) {
        use subagent_launch::test_support::write_launch_session;
        let project = tempfile::tempdir().unwrap();
        let (transcript, subagents) = write_launch_session(project.path());
        let (spliced, own): (Vec<_>, Vec<_>) = session_entries(&transcript, &subagents)
            .into_iter()
            .map(|(_, entry)| entry)
            .partition(|entry| matches!(entry, LogEntry::Progress { .. }));
        let as_json = |entries: Vec<LogEntry>| -> Vec<String> {
            entries
                .iter()
                .map(|entry| serde_json::to_string(entry).unwrap())
                .collect()
        };
        (as_json(spliced), as_json(own))
    }

    #[test]
    fn a_forks_copy_of_its_agent_call_and_the_fork_instructions_are_dropped() {
        use subagent_launch::test_support::{FORK_BOILERPLATE, FORK_CALL_ID, FORK_TURN};
        let (spliced, _) = launch_session_records();

        assert!(
            spliced.iter().any(|turn| turn.contains(FORK_TURN)),
            "{spliced:#?}"
        );
        assert!(
            !spliced
                .iter()
                .any(|turn| turn.contains(FORK_CALL_ID) || turn.contains(FORK_BOILERPLATE)),
            "{spliced:#?}"
        );
    }

    #[test]
    fn a_sub_agents_opening_message_is_dropped_only_when_it_repeats_its_agent_calls_prompt() {
        use subagent_launch::test_support::{REPEATED_PROMPT, REWORDED_PROMPT};
        let (spliced, _) = launch_session_records();

        assert!(
            !spliced.iter().any(|turn| turn.contains(REPEATED_PROMPT)),
            "{spliced:#?}"
        );
        assert!(
            spliced.iter().any(|turn| turn.contains(REWORDED_PROMPT)),
            "{spliced:#?}"
        );
    }

    #[test]
    fn a_background_launchs_receipt_reads_running_in_the_background() {
        let (_, own) = launch_session_records();

        assert!(
            own.iter()
                .any(|record| record.contains(subagent_launch::BACKGROUND_LAUNCH_RESULT)),
            "{own:#?}"
        );
        assert!(
            !own.iter()
                .any(|record| record.contains("Async agent launched")),
            "{own:#?}"
        );
    }

    #[test]
    fn repeated_skill_text_is_dropped_from_the_session_and_its_sub_agent_transcripts() {
        use skill_text::test_support::{SKILL_CALL_LOAD, SUB_AGENT_SKILL_CALL_LOAD};

        let dir = tempfile::tempdir().unwrap();
        let session = dir.path().join("session.jsonl");
        std::fs::write(&session, SKILL_CALL_LOAD.join("\n")).unwrap();
        let subagent = dir.path().join("agent-a1111111111111111.jsonl");
        std::fs::write(&subagent, SUB_AGENT_SKILL_CALL_LOAD.join("\n")).unwrap();
        std::fs::write(
            subagent.with_extension("meta.json"),
            r#"{"agentType":"fork"}"#,
        )
        .unwrap();

        let shape = session_entries(&session, &[subagent])
            .iter()
            .map(|(_, entry)| shape_of(entry))
            .collect::<Vec<_>>();

        assert_eq!(
            shape,
            [
                "call:toolu_01SKILLMAINAAAAAAAAAAAAA",
                "result:toolu_01SKILLMAINAAAAAAAAAAAAA",
                "fork:assistant",
                "fork:user",
            ]
        );
    }

    /// Claude's format leaves every other agent's transcript unrecognized;
    /// the registry asks it first.
    #[test]
    fn a_claude_transcript_is_claudes_and_no_other_agents_transcript_is() {
        let directory = tempfile::tempdir().unwrap();
        let claude = directory.path().join("claude.jsonl");
        std::fs::write(
            &claude,
            r#"{"type":"user","cwd":"/tmp/project","timestamp":"2026-07-26T06:30:00.000Z","message":{"role":"user","content":"a question"}}"#,
        )
        .unwrap();
        let projection = CLAUDE_TRANSCRIPT
            .parse_transcript(&claude)
            .unwrap()
            .expect("a Claude transcript");
        assert_eq!(projection.header.id, "claude");
        assert_eq!(projection.header.cwd, PathBuf::from("/tmp/project"));
        assert_eq!(projection.header.timestamp, "2026-07-26T06:30:00.000Z");

        let fixtures = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
        for foreign in [
            "codex/rollout.jsonl",
            "kimi/wire.jsonl",
            "pi/v3-branched.jsonl",
            "omp/v3.jsonl",
        ] {
            let path = fixtures.join(foreign);
            assert!(
                CLAUDE_TRANSCRIPT.parse_transcript(&path).unwrap().is_none(),
                "{foreign}"
            );
            assert!(
                CLAUDE_TRANSCRIPT
                    .parse_conversation(&path, None, None)
                    .unwrap()
                    .is_none(),
                "{foreign}"
            );
        }
    }

    /// A file of lines that are not records is no agent's transcript.
    #[test]
    fn a_file_holding_no_claude_record_is_not_a_claude_transcript() {
        let directory = tempfile::tempdir().unwrap();
        for (name, contents) in [("malformed.jsonl", "{malformed"), ("empty.jsonl", "")] {
            let path = directory.path().join(name);
            std::fs::write(&path, contents).unwrap();

            assert!(CLAUDE_TRANSCRIPT.parse_transcript(&path).unwrap().is_none());
            assert!(
                CLAUDE_TRANSCRIPT
                    .session_entries(&path, &[])
                    .unwrap()
                    .is_none()
            );
            assert!(
                CLAUDE_TRANSCRIPT
                    .parse_conversation(&path, None, None)
                    .unwrap()
                    .is_none()
            );
        }
    }

    /// A file of summaries alone opens and deletes as Claude's but lists no
    /// row.
    #[test]
    fn a_claude_transcript_holding_no_conversation_doesnt_list_a_row() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("summary.jsonl");
        std::fs::write(&path, r#"{"type":"summary","summary":"Only metadata"}"#).unwrap();

        assert!(CLAUDE_TRANSCRIPT.parse_transcript(&path).unwrap().is_some());
        assert!(
            CLAUDE_TRANSCRIPT
                .parse_conversation(&path, None, None)
                .unwrap()
                .is_none()
        );
    }

    fn conversation_of(records: &[&str]) -> Conversation {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("session.jsonl");
        std::fs::write(&path, records.join("\n")).unwrap();
        CLAUDE_TRANSCRIPT
            .parse_conversation(&path, None, None)
            .unwrap()
            .expect("a conversation")
    }

    #[test]
    fn a_session_with_no_reply_counts_no_assistant_messages() {
        let conversation = conversation_of(&[
            r#"{"type":"user","message":{"role":"user","content":"<command-name>/status</command-name>"}}"#,
        ]);

        assert_eq!(conversation.assistant_messages, 0);
        assert_eq!(conversation.message_count, 1);
    }

    #[test]
    fn a_session_that_was_answered_counts_the_reply() {
        let conversation = conversation_of(&[
            r#"{"type":"user","message":{"role":"user","content":"hi"}}"#,
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"hello"}]}}"#,
        ]);

        assert_eq!(conversation.assistant_messages, 1);
    }

    fn assistant_entry_with_tool_uses(blocks: Vec<Value>) -> LogEntry {
        serde_json::from_value(json!({
            "type": "assistant",
            "message": {"role": "assistant", "content": blocks}
        }))
        .unwrap()
    }

    fn tool_use(name: &str, input: Value) -> Value {
        json!({"type": "tool_use", "id": "toolu_1", "name": name, "input": input})
    }

    fn assigned_tools(entry: &LogEntry) -> Vec<Tool> {
        let LogEntry::Assistant { message, .. } = entry else {
            panic!("expected an assistant entry");
        };
        message
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::ToolUse { tool, .. } => Some(*tool),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn every_claude_tool_name_lands_in_its_bucket() {
        let expected = [
            ("Bash", Tool::Shell),
            ("PowerShell", Tool::Shell),
            ("Read", Tool::Read),
            ("Edit", Tool::Edit),
            ("Write", Tool::Write),
            ("Grep", Tool::Grep),
            ("Glob", Tool::Glob),
            ("LS", Tool::DirectoryListing),
            ("WebFetch", Tool::WebFetch),
            ("WebSearch", Tool::WebSearch),
            ("Skill", Tool::Skill),
            ("Task", Tool::Agent),
            ("Agent", Tool::Agent),
            ("SendMessage", Tool::AgentMessage),
            ("SubagentHandback", Tool::AgentReport),
            ("TaskOutput", Tool::Wait),
            ("TaskCreate", Tool::TaskList),
            ("TaskUpdate", Tool::TaskList),
            ("TodoWrite", Tool::TaskList),
            ("ExitPlanMode", Tool::Other),
            ("EnterPlanMode", Tool::Other),
            ("ToolSearch", Tool::Other),
            ("AskUserQuestion", Tool::Other),
            ("TaskStop", Tool::Other),
            ("Artifact", Tool::Other),
            ("ReportFindings", Tool::Other),
            ("mcp__rustrover__ide_find_references", Tool::Other),
        ];
        let mut entry = assistant_entry_with_tool_uses(
            expected
                .iter()
                .map(|(name, _)| tool_use(name, json!({})))
                .collect(),
        );

        assign_canonical_tools(&mut entry);

        let tools: Vec<Tool> = expected.iter().map(|(_, tool)| *tool).collect();
        assert_eq!(assigned_tools(&entry), tools);
    }

    #[test]
    fn send_message_input_names_its_recipient() {
        let mut entry = assistant_entry_with_tool_uses(vec![tool_use(
            "SendMessage",
            json!({"to": "worker-1", "message": "status?", "summary": "ask"}),
        )]);

        assign_canonical_tools(&mut entry);

        let LogEntry::Assistant { message, .. } = &entry else {
            unreachable!()
        };
        let ContentBlock::ToolUse { input, .. } = &message.content[0] else {
            unreachable!()
        };
        assert_eq!(
            input,
            &json!({"recipient": "worker-1", "message": "status?", "summary": "ask"})
        );
    }

    #[test]
    fn agent_progress_tool_uses_are_assigned_in_the_payload() {
        let mut entry: LogEntry = serde_json::from_value(json!({
            "type": "progress",
            "data": {
                "type": "agent_progress",
                "agentId": "agent-1",
                "prompt": "look around",
                "message": {
                    "type": "assistant",
                    "message": {"role": "assistant", "content": [
                        {"type": "text", "text": "checking"},
                        tool_use("Grep", json!({"pattern": "fn main"})),
                        tool_use("SendMessage", json!({"to": "lead", "message": "done"})),
                    ]}
                }
            }
        }))
        .unwrap();

        assign_canonical_tools(&mut entry);

        let LogEntry::Progress { data, .. } = &entry else {
            unreachable!()
        };
        assert_eq!(data["prompt"], json!("look around"));
        let content = &data["message"]["message"]["content"];
        assert_eq!(content[0], json!({"type": "text", "text": "checking"}));
        assert_eq!(content[1]["tool"], json!("grep"));
        assert_eq!(content[2]["tool"], json!("agent_message"));
        assert_eq!(content[2]["input"]["recipient"], json!("lead"));
        let progress = parse_agent_progress(data).unwrap();
        let crate::log_entry::AgentContent::Blocks(blocks) = &progress.message.message.content;
        assert!(matches!(
            blocks[1],
            ContentBlock::ToolUse {
                tool: Tool::Grep,
                ..
            }
        ));
    }

    #[test]
    fn a_sub_agents_label_is_its_agent_type_or_its_id_without_a_sidecar() {
        let subagents = tempfile::tempdir().unwrap();
        let transcript = subagents.path().join("agent-a1111111111111111.jsonl");
        let sidecar = subagents.path().join("agent-a1111111111111111.meta.json");
        std::fs::write(&transcript, "{\"type\":\"user\"}\n").unwrap();
        let label = || subagent_label(&transcript, &SubagentSidecar::read(&transcript));

        assert_eq!(label(), "a1111111111111111");

        std::fs::write(&sidecar, r#"{"description":"no type","spawnDepth":1}"#).unwrap();
        assert_eq!(
            label(),
            "a1111111111111111",
            "a sidecar naming no agentType falls back to the id"
        );

        std::fs::write(&sidecar, r#"{"agentType":"Explore","spawnDepth":1}"#).unwrap();
        assert_eq!(label(), "Explore");
    }

    /// A transcript of `records` alternating user and assistant records, with
    /// a blank line every 37th line and a malformed one every 53rd.
    fn long_transcript(records: usize) -> String {
        let mut text = String::new();
        for index in 0..records {
            let record = if index % 2 == 0 {
                json!({
                    "type": "user",
                    "timestamp": "2026-08-01T10:00:00.000Z",
                    "message": {"role": "user", "content": format!("question-{index}-end")},
                })
            } else {
                json!({
                    "type": "assistant",
                    "timestamp": "2026-08-01T10:00:01.000Z",
                    "message": {
                        "id": format!("msg_{index}"),
                        "role": "assistant",
                        "content": [{"type": "text", "text": format!("answer-{index}-end")}],
                    },
                })
            };
            text.push_str(&record.to_string());
            text.push('\n');
            if index % 37 == 0 {
                text.push_str("  \n");
            }
            if index % 53 == 0 {
                text.push_str("{\"type\": \"user\", truncated\n");
            }
        }
        text
    }

    #[test]
    fn a_transcript_parses_the_same_on_the_rayon_pool_as_on_one_thread() {
        let directory = tempfile::tempdir().unwrap();
        let transcript = directory.path().join("long.jsonl");
        std::fs::write(&transcript, long_transcript(400)).unwrap();
        let parsed = |parallel| {
            let parsed =
                transcript_entries_with(&transcript, parallel, &mut DroppedRecords::default())
                    .unwrap();
            let malformed = parsed
                .malformed_lines
                .iter()
                .map(|line| {
                    let detail = line.detail.as_ref().unwrap();
                    (
                        line.line_number,
                        detail.line_content.clone(),
                        detail.error_message.clone(),
                    )
                })
                .collect::<Vec<_>>();
            (format!("{:?}", parsed.entries), malformed)
        };

        let (pool_entries, pool_malformed) = parsed(ParallelThreshold::ALWAYS);
        let (thread_entries, thread_malformed) = parsed(ParallelThreshold::NEVER);

        assert_eq!(pool_entries, thread_entries);
        assert_eq!(pool_malformed, thread_malformed);
        assert_eq!(pool_malformed.len(), 8);
        assert!(pool_entries.contains("answer-399-end"));
    }
}
