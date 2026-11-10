//! What a resumed read of a changed session starts from, for every agent:
//! where in the view's order the stored session stays as it is, the session
//! row's fold there, and each sub-agent transcript's share of the row at its
//! stable cursor. Nothing here knows a format; each agent's reader and
//! thread linker supply what does.

mod read;
#[cfg(test)]
mod tests;

pub(crate) use read::{ResumeContext, ResumedSession, WholeRead, read_resumed};

use crate::agent::message_numbering::has_visible_block;
use crate::agent::transcript::AgentMessageRole;
use crate::history::format::RecordLine;
use crate::history::format::splice::{Spliced, walk_spliced};
use crate::history::row_fold::FoldCheckpoint;
use crate::log_entry::{LogEntry, SubagentIdentity};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Where a resumed read of a session starts its session row and numbering:
/// before the entry at `sequence` in the view's order. Every entry before
/// it stays where it is stored.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct SessionCheckpoint {
    pub(crate) sequence: usize,
    /// The fold of the entries before `sequence`.
    pub(crate) fold: FoldCheckpoint,
    pub(crate) splice: SpliceBound,
    /// The time the session's header names, for a session without
    /// activity.
    pub(crate) header_time: String,
    /// True for a session whose format states no header: its row is dated
    /// by its last activity, and its working directory is the first one an
    /// entry names.
    pub(crate) is_headerless: bool,
    /// The [`leading_digest`] of the session's leading entries. A resumed
    /// read whose leading entries have another reads the session whole.
    #[serde(default)]
    pub(crate) leading_digest: String,
}

/// A hash of the leading entries `entries` open with, the session's own at
/// line 0, besides custom titles, which a resumed read replaces; empty when
/// there are none. The fold before a checkpoint holds what the others set,
/// such as a summary from a title the agent generated.
pub(crate) fn leading_digest(entries: &[(RecordLine, LogEntry)]) -> String {
    let others: Vec<String> = entries
        .iter()
        .take_while(|(record, _)| record.line == 0 && record.transcript.is_none())
        .filter(|(_, entry)| !matches!(entry, LogEntry::CustomTitle { .. }))
        .map(|(_, entry)| serde_json::to_string(entry).expect("an entry serializes"))
        .collect();
    if others.is_empty() {
        return String::new();
    }
    blake3::hash(others.join("\n").as_bytes())
        .to_hex()
        .to_string()
}

impl SessionCheckpoint {
    pub(crate) fn encode(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("a session checkpoint serializes")
    }

    pub(crate) fn decode(bytes: &[u8]) -> Option<Self> {
        serde_json::from_slice(bytes).ok()
    }
}

/// What the entries before a checkpoint bound in the view's order: a new
/// entry must sort after all of them to be placed after the checkpoint.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub(crate) struct SpliceBound {
    /// The greatest timestamp of a session entry before the checkpoint. A
    /// sub-agent turn with an earlier one splices before that entry.
    pub(crate) max_parent_key: Option<String>,
    /// The last sub-agent turn before the checkpoint.
    pub(crate) last_child: Option<ChildKey>,
    /// True when that turn comes after the last session entry before the
    /// checkpoint: a new session entry then needs a later timestamp than the
    /// turn's.
    pub(crate) has_trailing_child: bool,
}

/// A sub-agent turn's place in the splice order: its timestamp, then its
/// thread's place among the session's sub-agents, then its place in its
/// thread.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct ChildKey {
    pub(crate) key: String,
    pub(crate) transcript: PathBuf,
    pub(crate) line: usize,
    /// The turn's index among its thread's turns from `line`.
    pub(crate) nth: usize,
}

impl ChildKey {
    /// True when this turn splices after `other`, with `order` giving each
    /// transcript's place among the session's sub-agents.
    pub(crate) fn sorts_after(&self, other: &ChildKey, order: &dyn Fn(&Path) -> usize) -> bool {
        let rank = |key: &ChildKey| (key.key.clone(), order(&key.transcript), key.line, key.nth);
        rank(self) > rank(other)
    }
}

/// One step of the view's walk, as the checkpoint reads it.
pub(crate) struct WalkStep<'a> {
    pub(crate) record: &'a RecordLine,
    /// The timestamp the splice orders the step by: a session entry's own,
    /// a sub-agent turn's.
    pub(crate) key: Option<&'a str>,
    pub(crate) is_child: bool,
    pub(crate) entry: Option<&'a LogEntry>,
    /// The id of the reply the step's record belongs to, which numbers its
    /// message.
    pub(crate) reply_id: Option<&'a str>,
}

/// The id of the reply `entry` belongs to, which numbers its message: an
/// assistant record's, or a spliced sub-agent turn's.
pub(crate) fn reply_id(entry: &LogEntry) -> Option<&str> {
    match entry {
        LogEntry::Assistant { message, .. } => message.id.as_deref(),
        LogEntry::Progress { data, .. } => data
            .pointer("/message/message/id")
            .and_then(serde_json::Value::as_str),
        _ => None,
    }
}

/// The sequence a checkpoint stands at, and what the entries before it
/// bound, for a view walked as `steps`: after the last session entry before
/// `own_stable_line`, the line the next read of the session's own transcript
/// derives entries again from, before the session's open reply, and before
/// every reply of any thread with records on both sides. `None` when the
/// session's own transcript does not resume.
///
/// Sub-agent transcripts' stable lines do not bound the checkpoint: each
/// stands at the sub-agent's last prompt, near its start, so it would hold
/// every checkpoint before the session's first sub-agent turn. A resumed
/// read rewrites a changed sub-agent's turns before the checkpoint in place.
pub(crate) fn checkpoint_place(
    steps: &[WalkStep<'_>],
    own_stable_line: Option<usize>,
) -> Option<(usize, SpliceBound)> {
    checkpoint_place_after(steps, own_stable_line, SpliceBound::default())
}

/// [`checkpoint_place`] for `steps` that follow entries bounded as `bound`;
/// the sequence is counted from the first of `steps`.
pub(crate) fn checkpoint_place_after(
    steps: &[WalkStep<'_>],
    own_stable_line: Option<usize>,
    bound: SpliceBound,
) -> Option<(usize, SpliceBound)> {
    let own_stable_line = own_stable_line?;
    let first_unstable = steps
        .iter()
        .position(|step| !step.is_child && step.record.line >= own_stable_line)
        .unwrap_or(steps.len());
    let sequence = after_last_parent(&steps[..first_unstable]);
    let sequence = before_open_reply(&steps[..sequence]);
    let sequence = before_split_replies(steps, sequence);
    let mut bound = bound;
    let mut nth = std::collections::HashMap::<(&Path, usize), usize>::new();
    for step in &steps[..sequence] {
        if step.is_child {
            let transcript = step.record.transcript.as_deref().unwrap_or(Path::new(""));
            let index = nth.entry((transcript, step.record.line)).or_default();
            bound.last_child = Some(ChildKey {
                key: step.key.unwrap_or_default().to_owned(),
                transcript: transcript.to_path_buf(),
                line: step.record.line,
                nth: *index,
            });
            *index += 1;
            bound.has_trailing_child = true;
            continue;
        }
        bound.has_trailing_child = false;
        if let Some(key) = step.key
            && bound.max_parent_key.as_deref().is_none_or(|max| key > max)
        {
            bound.max_parent_key = Some(key.to_owned());
        }
    }
    Some((sequence, bound))
}

/// The place just after the last session entry of `steps`, or 0.
fn after_last_parent(steps: &[WalkStep<'_>]) -> usize {
    steps
        .iter()
        .rposition(|step| !step.is_child)
        .map_or(0, |last_parent| last_parent + 1)
}

/// The place, after a session entry and at most `steps.len()`, before the
/// first record of the last reply in the session's own thread among
/// `steps`: a later record can still continue that reply, and a resumed read
/// must number its records with the ones before. An earlier reply counts as
/// complete; a last reply without an id cannot be continued.
fn before_open_reply(steps: &[WalkStep<'_>]) -> usize {
    fn own_reply<'a>(step: &WalkStep<'a>) -> Option<&'a crate::log_entry::AssistantMessage> {
        match step.entry {
            Some(LogEntry::Assistant {
                message,
                parent_tool_use_id: None,
                ..
            }) if !step.is_child => Some(message),
            _ => None,
        }
    }
    let last_reply = steps
        .iter()
        .rev()
        .filter_map(own_reply)
        .find(|message| has_visible_block(AgentMessageRole::Assistant, &message.content));
    let Some(open) = last_reply.and_then(|message| message.id.as_deref()) else {
        return steps.len();
    };
    let first = steps
        .iter()
        .position(|step| own_reply(step).is_some_and(|message| message.id.as_deref() == Some(open)))
        .unwrap_or(steps.len());
    after_last_parent(&steps[..first])
}

/// The place, after a session entry and at most `place`, before the first
/// record of each reply among `steps` with a record before it and one from
/// it on. A resumed read derives the records from the checkpoint on again,
/// and must number them with the ones before. Moving back can split another
/// reply, so the move repeats.
fn before_split_replies(steps: &[WalkStep<'_>], mut place: usize) -> usize {
    loop {
        let later: std::collections::HashSet<&str> = steps[place..]
            .iter()
            .filter_map(|step| step.reply_id)
            .collect();
        let Some(first) = steps[..place]
            .iter()
            .position(|step| step.reply_id.is_some_and(|id| later.contains(id)))
        else {
            return place;
        };
        place = after_last_parent(&steps[..first]);
    }
}

/// The steps of the view of `entries`, the session's own, with `turns`, its
/// sub-agent turns in splice order, each given as its record, timestamp and
/// reply id.
pub(crate) fn walk_steps<'a, T>(
    entries: &'a [(RecordLine, LogEntry)],
    turns: &'a [T],
    turn_place: impl Fn(&'a T) -> (&'a RecordLine, &'a str, Option<&'a str>),
) -> Vec<WalkStep<'a>> {
    let mut steps = Vec::with_capacity(entries.len() + turns.len());
    walk_spliced(
        entries,
        |(_, entry)| entry.timestamp(),
        turns,
        |turn| turn_place(turn).1,
        |step| {
            steps.push(match step {
                Spliced::Parent((record, entry)) => WalkStep {
                    record,
                    key: entry.timestamp(),
                    is_child: false,
                    entry: Some(entry),
                    reply_id: reply_id(entry),
                },
                Spliced::Child(turn) => {
                    let (record, key, reply_id) = turn_place(turn);
                    WalkStep {
                        record,
                        key: Some(key),
                        is_child: true,
                        entry: None,
                        reply_id,
                    }
                }
            })
        },
    );
    steps
}

/// A sub-agent transcript's share of its session's row at the transcript's
/// stable cursor, and who its thread is.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub(crate) struct ThreadFold {
    pub(crate) label: String,
    pub(crate) identity: SubagentIdentity,
    pub(crate) started: String,
    /// The timestamp the thread's next turn inherits when it has none: the
    /// last one before the stable cursor, else `started`.
    pub(crate) last_timestamp: String,
    /// The fold of the transcript's entries before the stable cursor, as a
    /// sub-agent transcript's contribution folds them.
    pub(crate) fold: FoldCheckpoint,
    /// True when the transcript's entries from the stable cursor on add to
    /// its search parts.
    pub(crate) has_parts_after_stable: bool,
    /// The title the transcript's leading entries name.
    pub(crate) title: Option<String>,
    /// The tokens of every entry the transcript holds.
    pub(crate) total_tokens: u64,
    pub(crate) is_missing_earlier_turns: bool,
    pub(crate) has_transient_read_error: bool,
}

impl ThreadFold {
    pub(crate) fn encode(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("a thread fold serializes")
    }

    pub(crate) fn decode(bytes: &[u8]) -> Option<Self> {
        serde_json::from_slice(bytes).ok()
    }
}
