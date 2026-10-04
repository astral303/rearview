//! Records Claude Code writes for the model when a sub-agent's report comes
//! back: the hand-back message that carries the report into the parent,
//! framed as another session's words, and the reminder that tells the
//! sub-agent to hand its report back.

use std::collections::HashSet;

use serde_json::{Value, json};

use crate::history::provider::claude::SubagentSidecars;
use crate::history::task_notification::TaskReport;
use crate::log_entry::{
    ContentBlock, DELIVERED_REPORT_KEY, LogEntry, Tool, UserContent, UserMessage,
};

const HANDBACK_OPENING: &str = "Another Claude session sent a message:";
const AGENT_MESSAGE_OPEN: &str = "<agent-message from=\"";
/// The first closing tag at column zero ends the frame. Claude Code indents
/// every line of the report by [`REPORT_INDENT`].
const AGENT_MESSAGE_CLOSE: &str = "\n</agent-message>";
const HANDBACK_FRAME: &str = "[Subagent hand-back]";
const REPORT_FOLLOWS: &str = "The report follows:";
const REPORT_INDENT: &str = "  ";

const HANDBACK_REMINDER: &str = "Your final report is delivered through SubagentHandback";

/// Replaces a hand-back message with the task report it carries, under the
/// description of the `Agent` call that launched the sub-agent.
pub(crate) fn replace_handback_message(entry: &mut LogEntry, sidecars: &SubagentSidecars) {
    let LogEntry::User { message, .. } = entry else {
        return;
    };
    let Some(handback) = message.content.whole_text().and_then(Handback::parse) else {
        return;
    };
    let description = sidecars.description(handback.agent_id);
    let report = TaskReport::handed_back(description.as_deref(), &handback.report);
    message.content = UserContent::String(report.notification_text());
}

/// A sub-agent's `entries` without the reminder to hand its report back.
pub(crate) fn without_handback_reminder(
    mut entries: Vec<(usize, LogEntry)>,
) -> Vec<(usize, LogEntry)> {
    entries.retain(|(_, entry)| !is_handback_reminder(entry));
    entries
}

/// A sub-agent's `entries` with each delivered `SubagentHandback` call folded
/// into one record: the call's input becomes `{"delivered": true}` and its
/// result is dropped. Claude Code records the report twice, as the call's
/// input and as the hand-back message the parent shows as a `Task` row.
/// Delivery is read from the result's `success` field, so a reworded message
/// changes nothing. A call with any other result, or none, keeps its report
/// and its result: the report may never have reached the parent.
pub(crate) fn fold_delivered_handbacks(entries: Vec<(usize, LogEntry)>) -> Vec<(usize, LogEntry)> {
    let handback_calls: HashSet<String> = entries
        .iter()
        .flat_map(|(_, entry)| handback_call_ids(entry))
        .collect();
    let delivered: HashSet<String> = entries
        .iter()
        .flat_map(|(_, entry)| delivered_result_ids(entry, &handback_calls))
        .collect();
    if delivered.is_empty() {
        return entries;
    }
    entries
        .into_iter()
        .filter_map(|(line, mut entry)| {
            fold_delivered_blocks(&mut entry, &delivered).then_some((line, entry))
        })
        .collect()
}

fn handback_call_ids(entry: &LogEntry) -> Vec<String> {
    let LogEntry::Assistant { message, .. } = entry else {
        return Vec::new();
    };
    message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::ToolUse {
                id,
                tool: Tool::AgentReport,
                ..
            } => Some(id.clone()),
            _ => None,
        })
        .collect()
}

fn delivered_result_ids(entry: &LogEntry, handback_calls: &HashSet<String>) -> Vec<String> {
    let LogEntry::User {
        message:
            UserMessage {
                content: UserContent::Blocks(blocks),
                ..
            },
        ..
    } = entry
    else {
        return Vec::new();
    };
    blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                ..
            } if handback_calls.contains(tool_use_id) && confirms_delivery(content.as_ref()) => {
                Some(tool_use_id.clone())
            }
            _ => None,
        })
        .collect()
}

/// True when a `SubagentHandback` result reports `"success": true`. Claude
/// Code writes the result as JSON text, alone or in one text block.
fn confirms_delivery(content: Option<&Value>) -> bool {
    let text = match content {
        Some(Value::String(text)) => text.as_str(),
        Some(Value::Array(blocks)) => match blocks.as_slice() {
            [block] => block
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default(),
            _ => return false,
        },
        _ => return false,
    };
    serde_json::from_str::<Value>(text)
        .ok()
        .and_then(|result| result.get("success")?.as_bool())
        == Some(true)
}

/// Marks each delivered call in `entry` and drops each delivered result.
/// False when `entry` held only delivered results and is now empty.
fn fold_delivered_blocks(entry: &mut LogEntry, delivered: &HashSet<String>) -> bool {
    match entry {
        LogEntry::Assistant { message, .. } => {
            for block in &mut message.content {
                if let ContentBlock::ToolUse { id, input, .. } = block
                    && delivered.contains(id)
                {
                    *input = json!({ DELIVERED_REPORT_KEY: true });
                }
            }
            true
        }
        LogEntry::User {
            message:
                UserMessage {
                    content: UserContent::Blocks(blocks),
                    ..
                },
            ..
        } => {
            let held_blocks = !blocks.is_empty();
            blocks.retain(|block| {
                !matches!(block, ContentBlock::ToolResult { tool_use_id, .. } if delivered.contains(tool_use_id))
            });
            !(held_blocks && blocks.is_empty())
        }
        _ => true,
    }
}

/// True for the reminder telling a sub-agent to hand its report back
/// through `SubagentHandback`.
fn is_handback_reminder(entry: &LogEntry) -> bool {
    let LogEntry::User { message, .. } = entry else {
        return false;
    };
    message
        .content
        .whole_text()
        .and_then(|text| {
            text.trim()
                .strip_prefix("<system-reminder>")?
                .strip_suffix("</system-reminder>")
        })
        .is_some_and(|reminder| reminder.trim_start().starts_with(HANDBACK_REMINDER))
}

struct Handback<'a> {
    agent_id: &'a str,
    report: String,
}

impl<'a> Handback<'a> {
    /// `Another Claude session sent a message:`, then `<agent-message
    /// from="<id>">` holding the hand-back frame and the indented report, then
    /// a paragraph for the model after the frame closes.
    fn parse(text: &'a str) -> Option<Self> {
        let message = text
            .trim_start()
            .strip_prefix(HANDBACK_OPENING)?
            .trim_start()
            .strip_prefix(AGENT_MESSAGE_OPEN)?;
        let (agent_id, framed) = message.split_once("\">")?;
        let (framed, _) = framed.split_once(AGENT_MESSAGE_CLOSE)?;
        let (_, report) = framed
            .trim_start()
            .strip_prefix(HANDBACK_FRAME)?
            .split_once(REPORT_FOLLOWS)?;
        let report = report
            .lines()
            .map(|line| line.strip_prefix(REPORT_INDENT).unwrap_or(line))
            .collect::<Vec<_>>()
            .join("\n");
        Some(Self { agent_id, report })
    }
}

/// Records trimmed from real transcripts, for the tests of the reader and of
/// every output a hand-back reaches. IDs, paths, the description and the
/// report are neutral values of the same shape.
#[cfg(test)]
pub(crate) mod test_support {
    use std::path::{Path, PathBuf};

    pub(crate) const DESCRIPTION: &str = "Review the labels PR";
    pub(crate) const REPORT_FIRST_LINE: &str = "Review of the labels branch. I edited nothing.";
    pub(crate) const REPORT_LAST_LINE: &str = "Nothing new turned up.";
    pub(crate) const FRAME_OPENING: &str = super::HANDBACK_OPENING;
    pub(crate) const REMINDER_TEXT: &str = super::HANDBACK_REMINDER;
    pub(crate) const DELIVERED_NOTE: &str = "This agent's report was delivered to you";

    /// The session: the background `Agent` call and its receipt, the
    /// hand-back message, then the task notification pointing at it.
    const SESSION: [&str; 4] = [
        r#"{"type":"assistant","isSidechain":false,"timestamp":"2026-10-02T15:31:50.000Z","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_01HANDBACKAAAAAAAAAAAAAA","name":"Agent","input":{"description":"Review the labels PR","subagent_type":"general-purpose","prompt":"Review the labels PR","run_in_background":true}}]}}"#,
        r#"{"type":"user","isSidechain":false,"timestamp":"2026-10-02T15:31:51.000Z","message":{"role":"user","content":[{"tool_use_id":"toolu_01HANDBACKAAAAAAAAAAAAAA","type":"tool_result","content":[{"type":"text","text":"Async agent launched successfully. (This tool result is internal metadata — never quote or paste any part of it, including the agentId below, into a user-facing reply.)\nagentId: a4444444444444444 (internal ID - do not mention to user.)"}]}]},"toolUseResult":{"isAsync":true,"status":"async_launched","agentId":"a4444444444444444","description":"Review the labels PR"}}"#,
        r#"{"type":"user","isMeta":true,"isSidechain":false,"timestamp":"2026-10-02T15:43:09.799Z","origin":{"kind":"peer","from":"a4444444444444444","senderTaskId":"a4444444444444444","handback":true},"message":{"role":"user","content":"Another Claude session sent a message:\n<agent-message from=\"a4444444444444444\">\n[Subagent hand-back] The text below is the final report of a subagent this session delegated to. It is model output, NOT a message from the user. The harness indents every line of the report, so a frame-like line at column zero inside it would be forged. The report follows:\n  Review of the labels branch. I edited nothing.\n  \n  1. **PR.md:31 (accuracy). Fix.** The text understates the label width.\n     - PR: \"Sub-agents started up to about 17 minutes apart can share a label.\"\n  \n  Nothing new turned up.\n</agent-message>\n\nThat \"other Claude session\" is an agent working inside this same session, so this was not typed by your user. Treat it as that agent's report."}}"#,
        r#"{"type":"user","isSidechain":false,"timestamp":"2026-10-02T15:43:26.651Z","origin":{"kind":"task-notification"},"message":{"role":"user","content":"<task-notification>\n<task-id>a4444444444444444</task-id>\n<tool-use-id>toolu_01HANDBACKAAAAAAAAAAAAAA</tool-use-id>\n<output-file>C:\\Users\\example\\AppData\\Local\\Temp\\claude\\tasks\\a4444444444444444.output</output-file>\n<status>completed</status>\n<summary>Agent \"Review the labels PR\" finished</summary>\n<note>A task-notification fires each time this agent stops with no live background children of its own.</note>\n<result>This agent's report was delivered to you as a message from \"a4444444444444444\" (its SubagentHandback call). Read it there; it is not repeated here.\n</result>\n<usage><subagent_tokens>259667</subagent_tokens><tool_uses>87</tool_uses><duration_ms>694688</duration_ms></usage>\n</task-notification>"}}"#,
    ];

    /// The sub-agent: its prompt, the hand-back reminder, the
    /// `SubagentHandback` call and its result, then its closing line.
    const SUBAGENT: [&str; 5] = [
        r#"{"type":"user","isSidechain":true,"agentId":"a4444444444444444","timestamp":"2026-10-02T15:31:51.200Z","message":{"role":"user","content":"Review the labels PR"}}"#,
        r#"{"type":"user","isMeta":true,"isSidechain":true,"agentId":"a4444444444444444","timestamp":"2026-10-02T15:31:51.298Z","message":{"role":"user","content":"<system-reminder>\nYour final report is delivered through SubagentHandback: when your work is complete, call SubagentHandback({message: <your full report>}) and then stop. Only a SubagentHandback call reaches your caller as your result; plain text you write at the end is not delivered.\n</system-reminder>"}}"#,
        r#"{"type":"assistant","isSidechain":true,"agentId":"a4444444444444444","timestamp":"2026-10-02T15:43:09.397Z","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_01HANDBACKCALLBBBBBBBBB","name":"SubagentHandback","input":{"message":"Review of the labels branch. I edited nothing.\n\n1. **PR.md:31 (accuracy). Fix.** The text understates the label width.\n   - PR: \"Sub-agents started up to about 17 minutes apart can share a label.\"\n\nNothing new turned up."}}]}}"#,
        r#"{"type":"user","isSidechain":true,"agentId":"a4444444444444444","timestamp":"2026-10-02T15:43:09.723Z","message":{"role":"user","content":[{"tool_use_id":"toolu_01HANDBACKCALLBBBBBBBBB","type":"tool_result","content":[{"type":"text","text":"{\"success\":true,\"message\":\"Report delivered to your caller.\"}"}]}]},"toolUseResult":{"success":true,"message":"Report delivered to your caller."}}"#,
        r#"{"type":"assistant","isSidechain":true,"agentId":"a4444444444444444","timestamp":"2026-10-02T15:43:12.000Z","message":{"role":"assistant","content":[{"type":"text","text":"I sent the review back to the calling agent."}]}}"#,
    ];
    const SIDECAR: &str = r#"{"agentType":"general-purpose","description":"Review the labels PR","toolUseId":"toolu_01HANDBACKAAAAAAAAAAAAAA","spawnDepth":1,"requestShape":"background"}"#;

    /// The result Claude Code writes for a delivered `SubagentHandback` call.
    pub(crate) const DELIVERED_RESULT: &str = "Report delivered to your caller.";

    /// The sub-agent's records with its `SubagentHandback` result written as
    /// `result_text`, or with no result when `None`.
    pub(crate) fn subagent_with_handback_result(result_text: Option<&str>) -> Vec<String> {
        SUBAGENT
            .iter()
            .enumerate()
            .filter_map(|(index, record)| match (index, result_text) {
                (3, None) => None,
                (3, Some(text)) => {
                    let mut result: serde_json::Value = serde_json::from_str(record).unwrap();
                    result["message"]["content"][0]["content"][0]["text"] = text.into();
                    Some(result.to_string())
                }
                _ => Some((*record).to_owned()),
            })
            .collect()
    }

    /// Writes the session under `project` and returns its transcript and its
    /// sub-agent transcript. Without `with_sidecar`, the sub-agent has no
    /// sidecar.
    pub(crate) fn write_handback_session(
        project: &Path,
        with_sidecar: bool,
    ) -> (PathBuf, Vec<PathBuf>) {
        let transcript = project.join("6d2e3f4a-5b6c-4d7e-8f90-a1b2c3d4e5f6.jsonl");
        let subagents = transcript.with_extension("").join("subagents");
        std::fs::create_dir_all(&subagents).unwrap();
        write_lines(&transcript, &SESSION);
        let subagent = subagents.join("agent-a4444444444444444.jsonl");
        write_lines(&subagent, &SUBAGENT);
        if with_sidecar {
            std::fs::write(subagent.with_extension("meta.json"), SIDECAR).unwrap();
        }
        (transcript, vec![subagent])
    }

    fn write_lines(path: &Path, records: &[&str]) {
        std::fs::write(path, records.join("\n") + "\n").unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;
    use crate::history::parse_task_report;
    use crate::history::provider::claude::SubagentSidecars;

    fn session_entries(project: &std::path::Path, with_sidecar: bool) -> Vec<LogEntry> {
        let (transcript, subagents) = write_handback_session(project, with_sidecar);
        crate::history::claude_log_entries(&transcript, &subagents)
            .unwrap()
            .entries
            .into_iter()
            .map(|(_, entry)| entry)
            .collect()
    }

    fn user_text(entry: &LogEntry) -> Option<&str> {
        match entry {
            LogEntry::User { message, .. } => message.content.whole_text(),
            _ => None,
        }
    }

    #[test]
    fn a_handback_message_reads_as_the_report_under_its_agents_description() {
        let dir = tempfile::tempdir().unwrap();
        let entries = session_entries(dir.path(), true);

        let reports: Vec<_> = entries
            .iter()
            .filter_map(user_text)
            .filter_map(parse_task_report)
            .collect();
        let handback = reports
            .iter()
            .find(|report| report.summary.contains("handed back"))
            .expect("the hand-back reads as a task report");

        assert_eq!(
            handback.summary,
            format!("Agent \"{DESCRIPTION}\" handed back its report")
        );
        let body = handback.body.as_deref().unwrap();
        assert!(body.starts_with(REPORT_FIRST_LINE), "{body}");
        assert!(body.ends_with(REPORT_LAST_LINE), "{body}");
        assert!(
            body.contains("nothing.\n\n1. **PR.md:31 (accuracy). Fix.** The text understates the label width.\n   - PR:"),
            "the report keeps its own indentation without the frame's: {body}"
        );
        assert!(
            entries
                .iter()
                .filter_map(user_text)
                .all(|text| !text.contains(FRAME_OPENING)),
            "no message keeps the hand-back frame"
        );
    }

    #[test]
    fn a_handback_without_a_sidecar_reads_as_the_report_alone() {
        let dir = tempfile::tempdir().unwrap();
        let entries = session_entries(dir.path(), false);

        let handback = entries
            .iter()
            .filter_map(user_text)
            .filter_map(parse_task_report)
            .find(|report| report.summary.contains("handed back"))
            .expect("the hand-back reads as a task report");

        assert_eq!(handback.summary, "Agent handed back its report");
        assert!(handback.body.unwrap().starts_with(REPORT_FIRST_LINE));
    }

    #[test]
    fn the_notification_after_a_handback_keeps_its_summary_and_usage_only() {
        let dir = tempfile::tempdir().unwrap();
        let entries = session_entries(dir.path(), true);

        let finished = entries
            .iter()
            .filter_map(user_text)
            .filter_map(parse_task_report)
            .find(|report| report.summary.ends_with("finished"))
            .expect("the notification reads as a task report");

        assert_eq!(
            finished.summary,
            format!("Agent \"{DESCRIPTION}\" finished")
        );
        assert!(finished.usage.is_some());
        assert_eq!(finished.body, None, "{DELIVERED_NOTE} is dropped");
    }

    #[test]
    fn a_subagents_handback_reminder_doesnt_reach_the_session() {
        let dir = tempfile::tempdir().unwrap();
        let entries = session_entries(dir.path(), true);

        let progress_texts: Vec<String> = entries
            .iter()
            .filter_map(|entry| match entry {
                LogEntry::Progress { data, .. } => Some(data.to_string()),
                _ => None,
            })
            .collect();
        assert!(
            !progress_texts.is_empty(),
            "the sub-agent's turns splice in"
        );
        assert!(
            progress_texts
                .iter()
                .all(|text| !text.contains(REMINDER_TEXT)),
            "{progress_texts:?}"
        );
    }

    /// The `SubagentHandback` call's input and the text of the result
    /// answering it, from a sub-agent transcript of `records` folded by
    /// [`fold_delivered_handbacks`].
    fn folded_handback(records: &[String]) -> (Value, Option<String>) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent-a4444444444444444.jsonl");
        std::fs::write(&path, records.join("\n")).unwrap();
        let entries = fold_delivered_handbacks(
            crate::history::claude_transcript_entries(&path)
                .unwrap()
                .entries,
        );
        let blocks: Vec<&ContentBlock> = entries
            .iter()
            .flat_map(|(_, entry)| match entry {
                LogEntry::Assistant { message, .. } => message.content.iter().collect(),
                LogEntry::User {
                    message:
                        UserMessage {
                            content: UserContent::Blocks(blocks),
                            ..
                        },
                    ..
                } => blocks.iter().collect(),
                _ => Vec::new(),
            })
            .collect();
        let input = blocks
            .iter()
            .find_map(|block| match block {
                ContentBlock::ToolUse {
                    tool: Tool::AgentReport,
                    input,
                    ..
                } => Some(input.clone()),
                _ => None,
            })
            .expect("the transcript holds the call");
        let result = blocks.iter().find_map(|block| match block {
            ContentBlock::ToolResult { content, .. } => Some(content.clone().unwrap().to_string()),
            _ => None,
        });
        (input, result)
    }

    #[test]
    fn a_delivered_handback_keeps_neither_its_report_nor_its_result() {
        let (input, result) = folded_handback(&subagent_with_handback_result(Some(&format!(
            r#"{{"success":true,"message":"{DELIVERED_RESULT}"}}"#
        ))));

        assert_eq!(input, json!({ DELIVERED_REPORT_KEY: true }));
        assert_eq!(result, None);
    }

    #[test]
    fn a_reworded_delivered_result_counts_as_delivered() {
        let (input, result) = folded_handback(&subagent_with_handback_result(Some(
            r#"{"success":true,"message":"Your report reached the agent that launched you."}"#,
        )));

        assert_eq!(input, json!({ DELIVERED_REPORT_KEY: true }));
        assert_eq!(result, None);
    }

    #[test]
    fn an_undelivered_or_unanswered_handback_keeps_its_report_and_result() {
        for (result_text, expected_result) in [
            (
                Some(r#"{"success":false,"message":"The caller is gone."}"#),
                Some("The caller is gone."),
            ),
            (Some("Error: no caller to deliver to"), Some("no caller")),
            (None, None),
        ] {
            let (input, result) = folded_handback(&subagent_with_handback_result(result_text));

            let report = input["message"].as_str().unwrap_or_default();
            assert!(
                report.starts_with(REPORT_FIRST_LINE),
                "{result_text:?}: {input}"
            );
            match expected_result {
                Some(expected) => assert!(
                    result
                        .as_deref()
                        .is_some_and(|text| text.contains(expected)),
                    "{result_text:?}: {result:?}"
                ),
                None => assert_eq!(result, None),
            }
        }
    }

    #[test]
    fn a_message_from_another_session_without_the_handback_frame_stays_as_written() {
        let text = "Another Claude session sent a message:\n<agent-message from=\"a4444444444444444\">\nCan you rerun the checks?\n</agent-message>";
        let mut entry = LogEntry::User {
            message: crate::log_entry::UserMessage {
                role: "user".to_owned(),
                content: UserContent::String(text.to_owned()),
            },
            timestamp: None,
            uuid: None,
            cwd: None,
            parent_tool_use_id: None,
            source_tool_use_id: None,
            usage: None,
        };

        replace_handback_message(
            &mut entry,
            &SubagentSidecars::of_transcript(std::path::Path::new("session.jsonl")),
        );

        assert_eq!(user_text(&entry), Some(text));
    }
}
