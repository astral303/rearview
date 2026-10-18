//! Splicing sub-agent threads into the parent session's entry stream.
//!
//! Every agent that records a sub-agent as a transcript of its own has the
//! view of the session merge those threads back in as `Progress` entries,
//! the record Claude keeps for a sub-agent turn, ordered by timestamp. The
//! session list's row walks the same turns in the same order, so it numbers
//! messages as the view does.

use super::{RecordLine, SessionProjection};
use crate::agent::transcript::AgentMessageRole;
use crate::log_entry::{ContentBlock, LogEntry, SubagentIdentity, UserContent};
use serde_json::json;
use std::path::Path;
use std::sync::Arc;

/// One sub-agent thread to splice, with the agent label its provider knows
/// it by: a thread id, an agent directory name, a Claude agent type.
pub(crate) struct SubagentThread {
    pub(crate) label: String,
    /// The thread's transcript file, named in every spliced turn's
    /// `RecordLine`: labels repeat, as two sub-agents of one Claude agent type
    /// do.
    pub(crate) transcript: Arc<Path>,
    /// Who the thread is, carried on every spliced turn for the viewer's
    /// labels; empty when the provider records nothing.
    pub(crate) identity: SubagentIdentity,
    /// The id of the call that launched the thread, carried on every spliced
    /// turn; `None` when the provider does not link the two.
    pub(crate) launch_call: Option<String>,
    /// When the thread started, for entries before its first timestamped
    /// one. Empty when the transcript records no start.
    pub(crate) started: String,
    pub(crate) entries: Vec<(RecordLine, LogEntry)>,
}

impl SubagentThread {
    /// `projection`, the sub-agent transcript at `transcript`, under its
    /// header's thread label.
    pub(crate) fn of_projection(transcript: &Path, projection: SessionProjection) -> Self {
        let label = projection.header.thread_label().to_owned();
        Self {
            label,
            transcript: Arc::from(transcript),
            identity: projection.header.subagent_identity,
            launch_call: None,
            started: projection.header.timestamp,
            entries: projection.entries,
        }
    }
}

/// A sub-agent's user or assistant record: its splice timestamp, and its role,
/// blocks and reply id as a `Progress` entry carries them.
pub(crate) struct SubagentTurn {
    pub(crate) timestamp: String,
    pub(crate) role: AgentMessageRole,
    pub(crate) blocks: Vec<ContentBlock>,
    pub(crate) reply_id: Option<String>,
    record: RecordLine,
    thread: Arc<ThreadName>,
}

/// One thread's label, identity and launching call, carried on each of its
/// spliced turns.
struct ThreadName {
    label: String,
    identity: SubagentIdentity,
    launch_call: Option<String>,
}

/// Every thread's user and assistant records, sorted by timestamp.
///
/// Only the dialogue: metadata rows such as usage and model changes describe
/// the thread, not its session, and usage already folds in at the
/// conversation level. Entries without a timestamp of their own inherit the
/// previous one, falling back to the thread's start.
pub(crate) fn subagent_turns(threads: Vec<SubagentThread>) -> Vec<SubagentTurn> {
    let mut turns = Vec::new();
    for thread in threads {
        let name = Arc::new(ThreadName {
            label: thread.label,
            identity: thread.identity,
            launch_call: thread.launch_call,
        });
        let mut last_timestamp = thread.started;
        for (record, entry) in thread.entries {
            if let Some(timestamp) = entry.timestamp() {
                last_timestamp = timestamp.to_owned();
            }
            let (role, blocks, reply_id) = match entry {
                LogEntry::User { message, .. } => (
                    AgentMessageRole::User,
                    match message.content {
                        UserContent::Blocks(blocks) => blocks,
                        UserContent::String(text) => vec![ContentBlock::Text { text }],
                    },
                    None,
                ),
                LogEntry::Assistant { message, .. } => {
                    (AgentMessageRole::Assistant, message.content, message.id)
                }
                _ => continue,
            };
            turns.push(SubagentTurn {
                timestamp: last_timestamp.clone(),
                role,
                blocks,
                reply_id,
                record: RecordLine {
                    transcript: Some(thread.transcript.clone()),
                    line: record.line,
                },
                thread: name.clone(),
            });
        }
    }
    turns.sort_by(|left, right| left.timestamp.cmp(&right.timestamp));
    turns
}

impl SubagentTurn {
    /// The turn as Claude records a sub-agent turn: an `agent_progress`
    /// payload with the agent label in `agentId`. Every consumer renders it
    /// nested and keeps it out of the session's own index. The record carries
    /// the turn's `timestamp` and its thread's launching call in
    /// `parentToolUseID` at the top level, as a Claude record carries its
    /// own, and the thread's identity beside `agentId` when the provider
    /// records one.
    fn into_progress_entry(self) -> (RecordLine, LogEntry) {
        let role = self.role.progress_type();
        let mut data = json!({
            "type": "agent_progress",
            "agentId": self.thread.label,
            "message": {
                "type": role,
                "message": { "role": role, "content": self.blocks },
            },
        });
        if let Some(reply_id) = self.reply_id {
            data["message"]["message"]["id"] = json!(reply_id);
        }
        if !self.thread.identity.is_empty() {
            data["identity"] = json!(self.thread.identity);
        }
        let mut extra = json!({});
        if !self.timestamp.is_empty() {
            extra["timestamp"] = json!(self.timestamp);
        }
        if let Some(launch_call) = &self.thread.launch_call {
            extra["parentToolUseID"] = json!(launch_call);
        }
        let entry = LogEntry::Progress { data, extra };
        (self.record, entry)
    }
}

/// `parent` with every thread's turns merged in as `Progress` entries.
pub(crate) fn splice_threads(
    parent: Vec<(RecordLine, LogEntry)>,
    threads: Vec<SubagentThread>,
) -> Vec<(RecordLine, LogEntry)> {
    let turns = subagent_turns(threads);
    let mut spliced = Vec::with_capacity(parent.len() + turns.len());
    walk_spliced(
        parent,
        |(_, entry)| entry.timestamp(),
        turns,
        |turn| &turn.timestamp,
        |step| {
            spliced.push(match step {
                Spliced::Parent(entry) => entry,
                Spliced::Child(turn) => turn.into_progress_entry(),
            })
        },
    );
    spliced
}

/// One step of [`walk_spliced`]: an entry of the parent stream, or a child
/// merged into it.
pub(crate) enum Spliced<P, C> {
    Parent(P),
    Child(C),
}

/// Visit `parent` with `children` merged in; `children` must already be
/// sorted by timestamp. Each child comes before the first parent entry with
/// a later timestamp. Ties keep the parent first: a dispatching turn comes
/// before the sub-agent's work.
pub(crate) fn walk_spliced<P, C>(
    parent: impl IntoIterator<Item = P>,
    parent_timestamp: impl Fn(&P) -> Option<&str>,
    children: impl IntoIterator<Item = C>,
    child_timestamp: impl Fn(&C) -> &str,
    mut visit: impl FnMut(Spliced<P, C>),
) {
    let mut pending = children.into_iter().peekable();
    for entry in parent {
        if let Some(entry_timestamp) = parent_timestamp(&entry) {
            while let Some(child) =
                pending.next_if(|child| child_timestamp(child) < entry_timestamp)
            {
                visit(Spliced::Child(child));
            }
        }
        visit(Spliced::Parent(entry));
    }
    pending.for_each(|child| visit(Spliced::Child(child)));
}
