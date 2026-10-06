//! Claude Code transcripts: one [`LogEntry`] record per line, with no session
//! header. A sub-agent's transcript is the same JSONL, kept in the session's
//! `subagents/` directory beside an `agent-<id>.meta.json` sidecar.

pub(crate) mod rename;
pub(crate) mod subagent_launch;
pub(crate) mod subagent_report;

use super::splice::{SubagentThread, progress_entries, splice_by_timestamp};
use super::{SessionFormat, SessionHeader, SessionProjection};
use crate::cli::DebugLevel;
use crate::error::Result;
use crate::history::{
    Conversation, MalformedLine, MalformedLineDetail, Source, TranscriptEntries, parser, skill_text,
};
use crate::log_entry::{ContentBlock, LogEntry, Tool};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
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
    fn parse_transcript(&self, path: &Path) -> Result<Option<SessionProjection>> {
        let transcript = transcript_entries(path)?;
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
        }))
    }

    /// Read line by line rather than from the projection, so each malformed
    /// line keeps its text and the lines around it for `--debug`.
    fn parse_conversation(
        &self,
        path: &Path,
        modified: Option<SystemTime>,
        debug_level: Option<DebugLevel>,
    ) -> Result<Option<Conversation>> {
        let reader = BufReader::new(File::open(path)?);
        let conversation = parser::process_conversation_reader(
            Source::Claude,
            path.to_path_buf(),
            reader,
            modified,
            debug_level,
        )?;
        Ok(conversation.map(|conversation| Conversation {
            session_id: session_id_of(path).unwrap_or_default().to_owned(),
            ..conversation
        }))
    }

    fn session_entries(
        &self,
        path: &Path,
        subagents: &[PathBuf],
    ) -> Result<Option<TranscriptEntries>> {
        let session = transcript_entries(path)?;
        if !holds_a_claude_record(&session.entries) {
            return Ok(None);
        }
        Ok(Some(with_subagents_spliced(session, subagents)))
    }
}

/// Every line of another agent's transcript reads as a [`LogEntry::Unknown`]
/// record or not at all.
fn holds_a_claude_record(entries: &[(usize, LogEntry)]) -> bool {
    entries
        .iter()
        .any(|(_, entry)| !matches!(entry, LogEntry::Unknown))
}

/// The session id a Claude transcript's file name states: Claude names each
/// transcript `<session-id>.jsonl`, and writes no id into it.
pub(crate) fn session_id_of(path: &Path) -> Option<&str> {
    path.file_stem()?.to_str()
}

/// `session` with the sub-agent transcripts at `subagents` spliced in as
/// `Progress` entries, each under the label its sidecar names, without the
/// records repeating the `Agent` call that launched it, and with a delivered
/// `SubagentHandback` call folded into one record. The malformed lines are
/// the session's own; a sub-agent transcript's are not reported here, and one
/// that cannot be read is left out, since the view has no debug channel: the
/// load reports it when the row is built.
fn with_subagents_spliced(session: TranscriptEntries, subagents: &[PathBuf]) -> TranscriptEntries {
    let transcripts: Vec<(&PathBuf, Vec<(usize, LogEntry)>)> = subagents
        .iter()
        .filter_map(|subagent| Some((subagent, transcript_entries(subagent).ok()?.entries)))
        .collect();
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
    let reader = BufReader::new(File::open(path)?);
    let sidecars = SubagentSidecars::of_transcript(path);
    let mut entries = Vec::new();
    let mut malformed_lines = Vec::new();
    for (line_index, line) in reader.lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str(&line) {
            Ok(mut entry) => {
                normalize_entry(&mut entry, &sidecars);
                entries.push((line_index + 1, entry));
            }
            Err(error) => malformed_lines.push(MalformedLine {
                line_number: line_index + 1,
                detail: Some(MalformedLineDetail {
                    line_content: line,
                    error_message: error.to_string(),
                }),
            }),
        }
    }
    Ok(TranscriptEntries {
        entries: skill_text::without_repeated_skill_text(entries),
        malformed_lines,
    })
}

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
mod tests {
    use super::*;
    use crate::log_entry::{UserContent, parse_agent_progress};

    const SUBAGENT_FIXTURE: &str = "tests/fixtures/claude/-tmp-claude-subagent-fixture/7b2f3c1e-4a5d-4e6f-8a9b-0c1d2e3f4a5b.jsonl";

    fn session_entries(path: &Path, subagents: &[PathBuf]) -> Vec<(usize, LogEntry)> {
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
}
