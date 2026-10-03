//! Splicing sub-agent threads into the parent session's entry stream.
//!
//! Every agent that records a sub-agent as a transcript of its own has the
//! view of the session merge those threads back in as `Progress` entries,
//! the record Claude keeps for a sub-agent turn, ordered by timestamp.

use crate::log_entry::{ContentBlock, LogEntry, SubagentIdentity, UserContent};
use serde_json::json;

/// A sub-agent entry ready to splice: its timestamp decides where it lands in
/// the parent's stream, its line still names its place in the thread's own file.
pub(crate) struct SpliceEntry {
    timestamp: String,
    line: usize,
    entry: LogEntry,
}

/// One sub-agent thread to splice, with the agent label its provider knows
/// it by: a thread id, an agent directory name, a Claude agent type.
pub(crate) struct SubagentThread {
    pub(crate) label: String,
    /// Who the thread is, carried on every spliced turn for the viewer's
    /// labels; empty when the provider records nothing.
    pub(crate) identity: SubagentIdentity,
    /// When the thread started, for entries before its first timestamped
    /// one. Empty when the transcript records no start.
    pub(crate) started: String,
    pub(crate) entries: Vec<(usize, LogEntry)>,
}

/// Every thread's dialogue as `Progress` entries, sorted by timestamp.
///
/// Only the dialogue: metadata rows such as usage and model changes describe
/// the thread, not the session it folds into, and usage already folds at the
/// conversation level. Entries without a timestamp of their own inherit the
/// previous one, falling back to the thread's start.
pub(crate) fn progress_entries(threads: Vec<SubagentThread>) -> Vec<SpliceEntry> {
    let mut entries = Vec::new();
    for thread in threads {
        let mut last_timestamp = thread.started;
        for (line, entry) in thread.entries {
            if let Some(timestamp) = entry.timestamp() {
                last_timestamp = timestamp.to_owned();
            }
            let Some(entry) =
                progress_entry(&thread.label, &thread.identity, entry, &last_timestamp)
            else {
                continue;
            };
            entries.push(SpliceEntry {
                timestamp: last_timestamp.clone(),
                line,
                entry,
            });
        }
    }
    entries.sort_by(|left, right| left.timestamp.cmp(&right.timestamp));
    entries
}

/// The entry as Claude records a sub-agent turn: an `agent_progress` payload
/// whose `agentId` carries the agent label, which every consumer renders nested
/// and keeps out of the session's own index. The record carries the turn's
/// `timestamp` at the top level, where a Claude record carries its own, and
/// the thread's `identity` beside `agentId` when the provider records one.
fn progress_entry(
    agent_label: &str,
    identity: &SubagentIdentity,
    entry: LogEntry,
    timestamp: &str,
) -> Option<LogEntry> {
    let (role, blocks) = match entry {
        LogEntry::User { message, .. } => (
            "user",
            match message.content {
                UserContent::Blocks(blocks) => blocks,
                UserContent::String(text) => vec![ContentBlock::Text { text }],
            },
        ),
        LogEntry::Assistant { message, .. } => ("assistant", message.content),
        _ => return None,
    };
    let mut data = json!({
        "type": "agent_progress",
        "agentId": agent_label,
        "message": {
            "type": role,
            "message": { "role": role, "content": blocks },
        },
    });
    if !identity.is_empty() {
        data["identity"] = json!(identity);
    }
    Some(LogEntry::Progress {
        data,
        extra: if timestamp.is_empty() {
            json!({})
        } else {
            json!({ "timestamp": timestamp })
        },
    })
}

/// Merge `children` into `parent`, each child entry before the first parent
/// entry with a later timestamp. Ties keep the parent first: the dispatching
/// turn precedes the work it dispatched.
pub(crate) fn splice_by_timestamp(
    parent: Vec<(usize, LogEntry)>,
    children: Vec<SpliceEntry>,
) -> Vec<(usize, LogEntry)> {
    let mut spliced = Vec::with_capacity(parent.len() + children.len());
    let mut pending = children.into_iter().peekable();
    for (line, entry) in parent {
        if let Some(parent_timestamp) = entry.timestamp() {
            while let Some(child) =
                pending.next_if(|child| child.timestamp.as_str() < parent_timestamp)
            {
                spliced.push((child.line, child.entry));
            }
        }
        spliced.push((line, entry));
    }
    spliced.extend(pending.map(|child| (child.line, child.entry)));
    spliced
}
