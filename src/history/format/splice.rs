//! Splicing sub-agent threads into the parent session's entry stream.
//!
//! Codex and Kimi record each sub-agent as a transcript of its own; the view
//! of a session merges those threads back in as `Progress` entries — the shape
//! Claude writes natively inside the parent file — ordered by timestamp.

use super::SessionProjection;
use crate::log_entry::{ContentBlock, LogEntry, UserContent};
use serde_json::json;

/// A sub-agent entry ready to splice: its timestamp decides where it lands in
/// the parent's stream, its line still names its place in the thread's own file.
pub(crate) struct SpliceEntry {
    timestamp: String,
    line: usize,
    entry: LogEntry,
}

/// Every thread's dialogue as `Progress` entries, sorted by timestamp. Each
/// thread arrives with the agent label its provider knows it by — a thread id,
/// an agent directory name.
///
/// Only the dialogue: metadata rows such as usage and model changes describe
/// the thread, not the session it folds into, and usage already folds at the
/// conversation level. Entries without a timestamp of their own inherit the
/// previous one, falling back to the thread's start.
pub(crate) fn progress_entries(threads: Vec<(String, SessionProjection)>) -> Vec<SpliceEntry> {
    let mut entries = Vec::new();
    for (agent_label, thread) in threads {
        let mut last_timestamp = thread.header.timestamp.clone();
        for (line, entry) in thread.entries {
            if let Some(timestamp) = entry_timestamp(&entry) {
                last_timestamp = timestamp.to_owned();
            }
            let Some(entry) = progress_entry(&agent_label, entry) else {
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
/// and keeps out of the session's own index.
fn progress_entry(agent_label: &str, entry: LogEntry) -> Option<LogEntry> {
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
    Some(LogEntry::Progress {
        data: json!({
            "type": "agent_progress",
            "agentId": agent_label,
            "message": {
                "type": role,
                "message": { "role": role, "content": blocks },
            },
        }),
        extra: json!({}),
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
        if let Some(parent_timestamp) = entry_timestamp(&entry) {
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

fn entry_timestamp(entry: &LogEntry) -> Option<&str> {
    match entry {
        LogEntry::User { timestamp, .. }
        | LogEntry::Assistant { timestamp, .. }
        | LogEntry::PiMetadata { timestamp, .. } => timestamp.as_deref(),
        _ => None,
    }
}
