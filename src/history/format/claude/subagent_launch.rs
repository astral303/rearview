//! Records Claude Code writes for the model when it launches a sub-agent: a
//! fork's copy of the parent's `Agent` call and the fork instructions
//! answering it, a sub-agent's opening message repeating the call's prompt,
//! and the receipt a background launch returns as the call's result.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::SubagentSidecar;
use crate::history::format::RecordLine;
use crate::history::format::splice::SubagentThread;
use crate::history::provider::{LinkWindow, LinkedThread};
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

/// The id and prompt of every `Agent` call among `entries`.
fn agent_call_prompts<'a>(
    entries: impl IntoIterator<Item = &'a (RecordLine, LogEntry)>,
) -> impl Iterator<Item = (&'a RecordLine, &'a str, &'a str)> {
    entries
        .into_iter()
        .filter_map(|(record, entry)| match entry {
            LogEntry::Assistant { message, .. } => Some((record, &message.content)),
            _ => None,
        })
        .flat_map(|(record, blocks)| blocks.iter().map(move |block| (record, block)))
        .filter_map(|(record, block)| match block {
            ContentBlock::ToolUse {
                id,
                tool: Tool::Agent,
                input,
                ..
            } => Some((record, id.as_str(), input.get("prompt")?.as_str()?)),
            _ => None,
        })
}

/// A fork's `entries` without its copy of the `Agent` call its sidecar
/// names and the result answering that copy, which holds the fork
/// instructions and the call's prompt. Any other sub-agent's entries stay.
pub(crate) fn without_fork_copies<Record>(
    mut entries: Vec<(Record, LogEntry)>,
    sidecar: &SubagentSidecar,
) -> Vec<(Record, LogEntry)> {
    if let (true, Some(call_id)) = (sidecar.is_fork, sidecar.tool_use_id.as_deref()) {
        entries.retain(|(_, entry)| !is_copy_of_call(entry, call_id));
    }
    entries
}

/// The index of the thread's first message among `entries`.
fn opening_index(entries: &[(RecordLine, LogEntry)]) -> Option<usize> {
    entries
        .iter()
        .position(|(_, entry)| matches!(entry, LogEntry::User { .. } | LogEntry::Assistant { .. }))
}

/// A hash of an `Agent` call's prompt: the ledger keeps it instead of the
/// prompt, to tell whether a sub-agent's opening message repeats it.
fn prompt_hash(prompt: &str) -> String {
    blake3::hash(prompt.as_bytes()).to_hex().to_string()
}

/// Where a ledger item was read: its transcript, `None` for the session's
/// own, and its line.
#[derive(Clone, Debug, Deserialize, Serialize)]
struct LedgerPlace {
    transcript: Option<PathBuf>,
    line: usize,
}

/// How a sub-agent thread was launched, as its sidecar records it.
#[derive(Clone, Debug, Deserialize, Serialize)]
struct ThreadLaunch {
    transcript: PathBuf,
    /// The parent's `Agent` call that launched the thread.
    call: Option<String>,
    is_fork: bool,
    /// The line of the thread's first message; `None` before one is read.
    opening: Option<usize>,
}

impl ThreadLaunch {
    /// The launch the sidecar beside the transcript at `transcript` records.
    fn read(transcript: &Path) -> Self {
        let sidecar = SubagentSidecar::read(transcript);
        Self {
            transcript: transcript.to_path_buf(),
            call: sidecar.tool_use_id,
            is_fork: sidecar.is_fork,
            opening: None,
        }
    }
}

/// What links a Claude session's sub-agent threads to the session: each
/// `Agent` call's prompt, by a hash, and each thread's launch. A
/// non-fork sub-agent's opening message repeating its launching call's
/// prompt, which may stand in another transcript, is dropped.
#[derive(Debug, Default, Deserialize, Serialize)]
pub(crate) struct LaunchLedger {
    /// Each `Agent` call: its place, its id and its prompt's hash.
    prompts: Vec<(LedgerPlace, String, String)>,
    launches: Vec<ThreadLaunch>,
}

impl LaunchLedger {
    pub(crate) fn decode(bytes: &[u8]) -> Self {
        serde_json::from_slice(bytes).unwrap_or_default()
    }

    pub(crate) fn encode(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("a launch ledger serializes")
    }

    /// The ledger of `session`, the session's own entries, and `threads`,
    /// each read whole; drops each thread's opening message that repeats
    /// its launching call's prompt, and gives it that call.
    pub(crate) fn link(session: &[(RecordLine, LogEntry)], threads: &mut [SubagentThread]) -> Self {
        let mut ledger = Self::default();
        ledger.add_prompts(None, session);
        for thread in threads.iter() {
            ledger.add_prompts(Some(&thread.transcript), &thread.entries);
        }
        for thread in threads.iter_mut() {
            let mut launch = ThreadLaunch::read(&thread.transcript);
            launch.opening =
                opening_index(&thread.entries).map(|index| thread.entries[index].0.line);
            thread.launch_call = launch.call.clone();
            ledger.drop_repeated_prompt(&mut thread.entries, &launch);
            ledger.launches.push(launch);
        }
        ledger
    }

    /// The ledger once `windows` replace what it holds of their transcripts,
    /// for a session whose threads are `threads`: each thread's launching
    /// call, in `threads` order, and the threads whose opening message the
    /// windows hold or whose launching call they add, which are read whole
    /// and given to [`Self::fold`].
    pub(crate) fn update(
        mut self,
        windows: &[LinkWindow<'_>],
        threads: &[LinkedThread<'_>],
    ) -> (Self, Vec<Option<String>>, Vec<PathBuf>) {
        let known: HashSet<String> = self.prompts.iter().map(|(_, id, _)| id.clone()).collect();
        for window in windows {
            self.prompts.retain(|(place, ..)| {
                place.transcript.as_deref() != window.transcript || place.line < window.start_line
            });
        }
        for window in windows {
            self.add_prompts(window.transcript, window.entries);
        }
        let added: HashSet<&str> = self
            .prompts
            .iter()
            .map(|(_, id, _)| id.as_str())
            .filter(|id| !known.contains(*id))
            .collect();
        let mut rereads = Vec::new();
        let mut launches = Vec::with_capacity(threads.len());
        for thread in threads {
            let mut launch = self
                .launches
                .iter()
                .find(|launch| launch.transcript == thread.transcript)
                .cloned()
                .unwrap_or_else(|| ThreadLaunch::read(thread.transcript));
            let window = windows
                .iter()
                .find(|window| window.transcript == Some(thread.transcript));
            let opening_of = |entries: &[(RecordLine, LogEntry)]| {
                opening_index(entries).map(|index| entries[index].0.line)
            };
            let mut reread = launch
                .call
                .as_deref()
                .is_some_and(|call| added.contains(call))
                && launch.opening.is_some();
            match window {
                // A whole read: the thread is folded as read.
                Some(window) if window.start_line == 0 => {
                    launch.opening = opening_of(window.entries);
                    reread = false;
                }
                Some(window) if launch.opening.is_none_or(|line| line >= window.start_line) => {
                    launch.opening = opening_of(window.entries);
                    let mut entries = window.entries.to_vec();
                    let before = entries.len();
                    self.drop_repeated_prompt(&mut entries, &launch);
                    reread |= entries.len() != before;
                }
                _ => {}
            }
            if reread {
                rereads.push(thread.transcript.to_path_buf());
            }
            launches.push(launch);
        }
        self.launches = launches;
        let launch_calls = self
            .launches
            .iter()
            .map(|launch| launch.call.clone())
            .collect();
        (self, launch_calls, rereads)
    }

    /// Drop `thread`'s opening message, read whole, when it repeats its
    /// launching call's prompt.
    pub(crate) fn fold(&self, thread: &mut SubagentThread) {
        let launch = self
            .launches
            .iter()
            .find(|launch| *launch.transcript == *thread.transcript)
            .cloned()
            .unwrap_or_else(|| ThreadLaunch::read(&thread.transcript));
        self.drop_repeated_prompt(&mut thread.entries, &launch);
    }

    /// Add the `Agent` calls among `entries`, read from `transcript`.
    fn add_prompts(&mut self, transcript: Option<&Path>, entries: &[(RecordLine, LogEntry)]) {
        for (record, id, prompt) in agent_call_prompts(entries) {
            let place = LedgerPlace {
                transcript: transcript.map(Path::to_path_buf),
                line: record.line,
            };
            self.prompts
                .push((place, id.to_owned(), prompt_hash(prompt)));
        }
    }

    /// Drop the opening message of `entries`, a non-fork thread launched as
    /// `launch`, when its text is exactly its launching call's prompt.
    fn drop_repeated_prompt(
        &self,
        entries: &mut Vec<(RecordLine, LogEntry)>,
        launch: &ThreadLaunch,
    ) {
        if launch.is_fork {
            return;
        }
        let Some(call) = launch.call.as_deref() else {
            return;
        };
        let Some((_, _, prompt)) = self.prompts.iter().find(|(_, id, _)| id == call) else {
            return;
        };
        if let Some(opening) = opening_index(entries)
            && matches!(
                &entries[opening].1,
                LogEntry::User { message, .. }
                    if message.content.whole_text().map(prompt_hash).as_ref() == Some(prompt)
            )
        {
            entries.remove(opening);
        }
    }
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
