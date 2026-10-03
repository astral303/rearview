//! Records Claude Code writes for the model when it launches a sub-agent: a
//! fork's copy of the parent's `Agent` call and the fork instructions
//! answering it, a sub-agent's opening message repeating the call's prompt,
//! and the receipt a background launch returns as the call's result.

use std::collections::HashMap;

use serde_json::Value;

use crate::history::provider::claude::SubagentSidecar;
use crate::log_entry::{ContentBlock, LogEntry, Tool, UserContent};

/// The opening sentence of a background launch's receipt. Instructions to the
/// model, the agent ID and the output file follow it.
const BACKGROUND_LAUNCH_RECEIPT: &str = "Async agent launched successfully.";

/// The result a background launch shows in place of that receipt.
pub(crate) const BACKGROUND_LAUNCH_RESULT: &str = "Running in the background";

/// Replaces a background launch's receipt with [`BACKGROUND_LAUNCH_RESULT`].
pub(crate) fn replace_background_launch_receipt(entry: &mut LogEntry) {
    let LogEntry::User { message, .. } = entry else {
        return;
    };
    let UserContent::Blocks(blocks) = &mut message.content else {
        return;
    };
    for block in blocks {
        if let ContentBlock::ToolResult {
            content: Some(content),
            ..
        } = block
            && opening_text(content).is_some_and(|text| text.starts_with(BACKGROUND_LAUNCH_RECEIPT))
        {
            *content = Value::String(BACKGROUND_LAUNCH_RESULT.to_owned());
        }
    }
}

/// A tool result's text when it is a string, or its first text block.
fn opening_text(content: &Value) -> Option<&str> {
    match content {
        Value::String(text) => Some(text),
        Value::Array(blocks) => blocks.iter().find_map(|block| block.get("text")?.as_str()),
        _ => None,
    }
}

/// The prompt of every `Agent` call among `entries`, by call id.
pub(crate) fn agent_call_prompts<'a>(
    entries: impl IntoIterator<Item = &'a LogEntry>,
) -> HashMap<String, String> {
    entries
        .into_iter()
        .filter_map(|entry| match entry {
            LogEntry::Assistant { message, .. } => Some(&message.content),
            _ => None,
        })
        .flatten()
        .filter_map(|block| match block {
            ContentBlock::ToolUse {
                id,
                tool: Tool::Agent,
                input,
                ..
            } => Some((id.clone(), input.get("prompt")?.as_str()?.to_owned())),
            _ => None,
        })
        .collect()
}

/// A sub-agent's `entries` without the records repeating the `Agent` call
/// its sidecar names. A fork's transcript opens with a copy of that call and
/// a result holding the fork instructions and the call's prompt; both are
/// dropped. A non-fork sub-agent's opening message is dropped when its text
/// is exactly that prompt.
pub(crate) fn without_launch_repeats(
    mut entries: Vec<(usize, LogEntry)>,
    sidecar: &SubagentSidecar,
    prompts: &HashMap<String, String>,
) -> Vec<(usize, LogEntry)> {
    let Some(call_id) = sidecar.tool_use_id.as_deref() else {
        return entries;
    };
    if sidecar.is_fork {
        entries.retain(|(_, entry)| !is_copy_of_call(entry, call_id));
        return entries;
    }
    let Some(prompt) = prompts.get(call_id) else {
        return entries;
    };
    let opening = entries
        .iter()
        .position(|(_, entry)| matches!(entry, LogEntry::User { .. } | LogEntry::Assistant { .. }));
    if let Some(opening) = opening
        && matches!(
            &entries[opening].1,
            LogEntry::User { message, .. } if whole_text(&message.content) == Some(prompt.as_str())
        )
    {
        entries.remove(opening);
    }
    entries
}

/// True for a fork's copy of the call `call_id` and for the result answering
/// that copy.
fn is_copy_of_call(entry: &LogEntry, call_id: &str) -> bool {
    match entry {
        LogEntry::Assistant { message, .. } => message.content.iter().any(
            |block| matches!(block, ContentBlock::ToolUse { id, .. } if id == call_id),
        ),
        LogEntry::User { message, .. } => match &message.content {
            UserContent::Blocks(blocks) => blocks.iter().any(|block| {
                matches!(block, ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == call_id)
            }),
            UserContent::String(_) => false,
        },
        _ => false,
    }
}

/// A user message's text when the message is nothing but text.
fn whole_text(content: &UserContent) -> Option<&str> {
    match content {
        UserContent::String(text) => Some(text),
        UserContent::Blocks(blocks) => match blocks.as_slice() {
            [ContentBlock::Text { text }] => Some(text),
            _ => None,
        },
    }
}

/// Records trimmed from real transcripts, for the tests of the reader, the
/// summary-mode viewer and the printout. Paths, IDs and prompts are neutral
/// values of the same shape. The session launches a fork in the background
/// and two sub-agents in the foreground: one opening with its `Agent` call's
/// prompt, one opening with a reworded prompt.
#[cfg(test)]
pub(crate) mod test_support {
    use std::path::{Path, PathBuf};

    pub(crate) const FORK_CALL_ID: &str = "toolu_01LAUNCHFORKAAAAAAAAAAA";
    pub(crate) const FORK_BOILERPLATE: &str = "<fork-boilerplate>";
    pub(crate) const FORK_TURN: &str = "The fork reads the plan.";
    pub(crate) const REPEATED_PROMPT: &str = "Check how the CLI is invoked";
    pub(crate) const REWORDED_PROMPT: &str = "Check the other file, please";

    /// The session: the user's message, the fork's `Agent` call and its
    /// background receipt, then two foreground `Agent` calls and their
    /// results.
    const SESSION: [&str; 7] = [
        r#"{"type":"user","isSidechain":false,"timestamp":"2026-10-03T05:34:00.000Z","message":{"role":"user","content":"Start the work"}}"#,
        r#"{"type":"assistant","isSidechain":false,"timestamp":"2026-10-03T05:34:01.000Z","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_01LAUNCHFORKAAAAAAAAAAA","name":"Agent","input":{"description":"Read the plan","subagent_type":"fork","prompt":"Read the plan","run_in_background":true}}]}}"#,
        r#"{"type":"user","isSidechain":false,"timestamp":"2026-10-03T05:34:02.000Z","message":{"role":"user","content":[{"tool_use_id":"toolu_01LAUNCHFORKAAAAAAAAAAA","type":"tool_result","content":[{"type":"text","text":"Async agent launched successfully. (This tool result is internal metadata — never quote or paste any part of it, including the agentId below, into a user-facing reply.)\nagentId: a1111111111111111 (internal ID - do not mention to user. Use SendMessage with to: 'a1111111111111111', summary: '<5-10 word recap>' to continue this agent.)\nThe agent is working in the background. You will be notified automatically when it completes.\noutput_file: C:\\Users\\example\\AppData\\Local\\Temp\\claude\\tasks\\a1111111111111111.output"}]}]},"toolUseResult":{"isAsync":true,"status":"async_launched","agentId":"a1111111111111111","description":"Read the plan","prompt":"Read the plan"}}"#,
        r#"{"type":"assistant","isSidechain":false,"timestamp":"2026-10-03T05:34:03.000Z","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_01LAUNCHREPEATBBBBBBBBB","name":"Agent","input":{"description":"Check the CLI","subagent_type":"general-purpose","prompt":"Check how the CLI is invoked"}}]}}"#,
        r#"{"type":"user","isSidechain":false,"timestamp":"2026-10-03T05:34:09.000Z","message":{"role":"user","content":[{"tool_use_id":"toolu_01LAUNCHREPEATBBBBBBBBB","type":"tool_result","content":[{"type":"text","text":"The CLI runs as a subprocess."}]}]}}"#,
        r#"{"type":"assistant","isSidechain":false,"timestamp":"2026-10-03T05:34:10.000Z","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_01LAUNCHREWORDCCCCCCCCC","name":"Agent","input":{"description":"Check the other file","subagent_type":"Explore","prompt":"Check the other file"}}]}}"#,
        r#"{"type":"user","isSidechain":false,"timestamp":"2026-10-03T05:34:15.000Z","message":{"role":"user","content":[{"tool_use_id":"toolu_01LAUNCHREWORDCCCCCCCCC","type":"tool_result","content":[{"type":"text","text":"The other file is unused."}]}]}}"#,
    ];

    /// The fork's transcript: the reference to the parent's history, the
    /// copy of the parent's `Agent` call, the result answering that copy
    /// with the fork instructions, and the fork's first turn.
    const FORK: [&str; 4] = [
        r#"{"type":"fork-context-ref","agentId":"a1111111111111111","parentSessionId":"5c1d2e3f-4a5b-4c6d-8e7f-8091a2b3c4d5","parentLastUuid":"00000000-0000-4000-8000-000000000001","contextLength":2}"#,
        r#"{"type":"assistant","isSidechain":true,"agentId":"a1111111111111111","timestamp":"2026-10-03T05:34:01.000Z","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_01LAUNCHFORKAAAAAAAAAAA","name":"Agent","input":{"description":"Read the plan","subagent_type":"fork","prompt":"Read the plan","run_in_background":true}}]}}"#,
        r#"{"type":"user","isSidechain":true,"agentId":"a1111111111111111","timestamp":"2026-10-03T05:34:02.300Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_01LAUNCHFORKAAAAAAAAAAA","content":[{"type":"text","text":"Fork started — processing in background"}]},{"type":"text","text":"<fork-boilerplate>\nYou are a worker fork. Execute ONE directive, then stop.\n</fork-boilerplate>\n\nYour directive: Read the plan"}]}}"#,
        r#"{"type":"assistant","isSidechain":true,"agentId":"a1111111111111111","timestamp":"2026-10-03T05:34:04.000Z","message":{"role":"assistant","content":[{"type":"text","text":"The fork reads the plan."}]}}"#,
    ];
    const FORK_SIDECAR: &str = r#"{"agentType":"fork","isFork":true,"description":"Read the plan","toolUseId":"toolu_01LAUNCHFORKAAAAAAAAAAA","spawnDepth":1,"requestShape":"background"}"#;

    /// A sub-agent opening with its `Agent` call's prompt.
    const REPEATED: [&str; 2] = [
        r#"{"type":"user","isSidechain":true,"agentId":"b2222222222222222","timestamp":"2026-10-03T05:34:04.000Z","message":{"role":"user","content":"Check how the CLI is invoked"}}"#,
        r#"{"type":"assistant","isSidechain":true,"agentId":"b2222222222222222","timestamp":"2026-10-03T05:34:05.000Z","message":{"role":"assistant","content":[{"type":"text","text":"Reading main.rs."}]}}"#,
    ];
    const REPEATED_SIDECAR: &str = r#"{"agentType":"general-purpose","description":"Check the CLI","toolUseId":"toolu_01LAUNCHREPEATBBBBBBBBB","spawnDepth":1}"#;

    /// The same shape, opening with a prompt reworded from its call's.
    const REWORDED: [&str; 2] = [
        r#"{"type":"user","isSidechain":true,"agentId":"c3333333333333333","timestamp":"2026-10-03T05:34:11.000Z","message":{"role":"user","content":"Check the other file, please"}}"#,
        r#"{"type":"assistant","isSidechain":true,"agentId":"c3333333333333333","timestamp":"2026-10-03T05:34:12.000Z","message":{"role":"assistant","content":[{"type":"text","text":"Reading lib.rs."}]}}"#,
    ];
    const REWORDED_SIDECAR: &str = r#"{"agentType":"Explore","description":"Check the other file","toolUseId":"toolu_01LAUNCHREWORDCCCCCCCCC","spawnDepth":1}"#;

    /// Writes the session under `project` and returns its transcript and its
    /// sub-agent transcripts.
    pub(crate) fn write_launch_session(project: &Path) -> (PathBuf, Vec<PathBuf>) {
        assert!(FORK[2].contains(FORK_BOILERPLATE) && FORK[3].contains(FORK_TURN));
        assert!(REPEATED[0].contains(REPEATED_PROMPT) && REWORDED[0].contains(REWORDED_PROMPT));
        let transcript = project.join("5c1d2e3f-4a5b-4c6d-8e7f-8091a2b3c4d5.jsonl");
        let subagents = transcript.with_extension("").join("subagents");
        std::fs::create_dir_all(&subagents).unwrap();
        write_lines(&transcript, &SESSION);
        let threads = [
            ("agent-a1111111111111111", &FORK[..], FORK_SIDECAR),
            ("agent-b2222222222222222", &REPEATED[..], REPEATED_SIDECAR),
            ("agent-c3333333333333333", &REWORDED[..], REWORDED_SIDECAR),
        ];
        let paths = threads
            .into_iter()
            .map(|(name, records, sidecar)| {
                let path = subagents.join(name).with_extension("jsonl");
                write_lines(&path, records);
                std::fs::write(path.with_extension("meta.json"), sidecar).unwrap();
                path
            })
            .collect();
        (transcript, paths)
    }

    fn write_lines(path: &Path, records: &[&str]) {
        std::fs::write(path, records.join("\n") + "\n").unwrap();
    }
}
