use crate::agent::message_numbering::{NumberingCheckpoint, ThreadIdentity, ThreadNumbers};
use crate::history::format::RecordLine;
use crate::history::sqlite_cache::roots::{Places, StoredPath};
use crate::log_entry::{LogEntry, SubagentIdentity};
use rusqlite::Connection;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::path::Path;
use std::sync::Arc;

/// A `threads` row's columns besides its number: the transcript its turns
/// came from and the agent label they carry. A session holds one row for
/// each.
#[derive(Clone, Debug, Default, Hash, PartialEq, Eq)]
pub(super) struct ThreadOrigin {
    pub(super) transcript: Option<Arc<Path>>,
    pub(super) label: Option<String>,
    pub(super) nickname: Option<String>,
    pub(super) role: Option<String>,
    pub(super) agent_path: Option<String>,
    pub(super) agent_id: Option<String>,
    pub(super) launch_call: Option<String>,
}

pub(super) struct ThreadRow {
    pub(super) origin: ThreadOrigin,
    /// The index of the origin's transcript in [`Numbering::transcripts`].
    pub(super) transcript: Option<usize>,
    pub(super) number: i64,
}

/// The transcripts and threads of a session's entries, numbered in the
/// order the entries first name them.
#[derive(Default)]
pub(super) struct Numbering {
    pub(super) transcripts: Vec<Arc<Path>>,
    transcript_indexes: HashMap<Arc<Path>, usize>,
    pub(super) threads: Vec<ThreadRow>,
    origins: HashMap<ThreadOrigin, usize>,
    positions: HashMap<i64, i64>,
    logical_threads: ThreadNumbers,
}

/// An entry's place: indexes into [`Numbering`]'s transcripts and threads.
pub(super) struct Placement {
    pub(super) thread: usize,
    pub(super) logical_thread: i64,
    pub(super) position: i64,
    /// The entry's transcript when its thread's origin names another one.
    pub(super) transcript: Option<usize>,
    /// The entry's transcript; `None` for the session's own.
    pub(super) record_transcript: Option<usize>,
}

impl Numbering {
    /// The numbering after the entries `numbering` and `positions` were
    /// checkpointed past, before it holds any transcript or thread row.
    pub(super) fn resumed(numbering: &NumberingCheckpoint, positions: &HashMap<i64, i64>) -> Self {
        Self {
            positions: positions.clone(),
            logical_threads: ThreadNumbers::resumed(&numbering.threads),
            ..Self::default()
        }
    }

    fn transcript(&mut self, path: &Arc<Path>) -> usize {
        match self.transcript_indexes.entry(path.clone()) {
            Entry::Occupied(entry) => *entry.get(),
            Entry::Vacant(entry) => {
                self.transcripts.push(path.clone());
                *entry.insert(self.transcripts.len() - 1)
            }
        }
    }

    pub(super) fn place(&mut self, record: &RecordLine, entry: &LogEntry) -> Placement {
        let transcript = record.transcript.as_ref().map(|path| self.transcript(path));
        let key = match entry {
            LogEntry::User {
                parent_tool_use_id: Some(call),
                ..
            }
            | LogEntry::Assistant {
                parent_tool_use_id: Some(call),
                ..
            } => ThreadOrigin {
                label: Some(call.clone()),
                launch_call: Some(call.clone()),
                ..ThreadOrigin::default()
            },
            LogEntry::Progress { data, extra }
                if data.get("type").and_then(Value::as_str) == Some("agent_progress")
                    && data.get("agentId").and_then(Value::as_str).is_some() =>
            {
                let text =
                    |value: &Value, key| value.get(key).and_then(Value::as_str).map(str::to_owned);
                let identity = &data["identity"];
                ThreadOrigin {
                    transcript: record.transcript.clone(),
                    label: text(data, "agentId"),
                    nickname: text(identity, "nickname"),
                    role: text(identity, "role"),
                    agent_path: text(identity, "agentPath"),
                    agent_id: text(identity, "agentId"),
                    launch_call: text(extra, "parentToolUseID"),
                }
            }
            _ => ThreadOrigin::default(),
        };
        // Each change of a thread's metadata stores its own `threads` row:
        // each message keeps its attribution, and entries don't repeat the
        // strings.
        let record_transcript = transcript.filter(|_| record.transcript != key.transcript);
        let number = self
            .logical_threads
            .number(ThreadIdentity::of(entry, record.transcript.as_deref()));
        let next_position = self.positions.entry(number).or_default();
        let position = *next_position;
        *next_position += 1;
        let thread = match self.origins.entry(key) {
            Entry::Occupied(entry) => *entry.get(),
            Entry::Vacant(entry) => {
                let origin = entry.key().clone();
                self.threads.push(ThreadRow {
                    transcript: transcript.filter(|_| origin.transcript.is_some()),
                    origin,
                    number,
                });
                *entry.insert(self.threads.len() - 1)
            }
        };
        Placement {
            thread,
            logical_thread: number,
            position,
            transcript: record_transcript,
            record_transcript: transcript,
        }
    }
}

pub(super) struct Thread {
    pub(super) transcript: Option<Arc<Path>>,
    label: Option<String>,
    identity: SubagentIdentity,
    launch_call: Option<String>,
}

impl Thread {
    pub(super) fn origin(&self) -> Option<Arc<crate::agent::transcript::SubagentOrigin>> {
        Some(Arc::new(crate::agent::transcript::SubagentOrigin {
            label: self.label.clone()?,
            transcript: self.transcript.clone(),
            launch_call: self.launch_call.clone(),
            identity: self.identity.clone(),
        }))
    }

    pub(super) fn progress(
        &self,
        role: Option<String>,
        reply_id: Option<String>,
        timestamp: Option<String>,
        blocks: Vec<crate::log_entry::ContentBlock>,
    ) -> LogEntry {
        let Some(label) = &self.label else {
            return LogEntry::Progress {
                data: Value::Null,
                extra: json!({}),
            };
        };
        let mut message = json!({"role": role, "content": blocks});
        if let Some(id) = reply_id {
            message["id"] = json!(id);
        }
        let mut data = json!({
            "type": "agent_progress", "agentId": label,
            "message": {"type": role, "message": message},
        });
        if !self.identity.is_empty() {
            data["identity"] = serde_json::to_value(&self.identity).expect("identity serializes");
        }
        let mut extra = json!({});
        if let Some(timestamp) = timestamp {
            extra["timestamp"] = json!(timestamp);
        }
        if let Some(call) = &self.launch_call {
            extra["parentToolUseID"] = json!(call);
        }
        LogEntry::Progress { data, extra }
    }
}

pub(super) struct StoredThreads {
    pub(super) threads: HashMap<i64, Thread>,
    pub(super) transcripts: HashMap<i64, Arc<Path>>,
}

/// The threads of `session`, with the transcripts they came from at their
/// paths in `places`.
pub(super) fn read(
    connection: &Connection,
    session: i64,
    places: &Places,
) -> crate::error::Result<StoredThreads> {
    let mut transcripts: HashMap<i64, Arc<Path>> = HashMap::new();
    let mut statement =
        connection.prepare_cached("SELECT id, root, path FROM transcripts WHERE session = ?1")?;
    let mut rows = statement.query([session])?;
    while let Some(row) = rows.next()? {
        let path = places.path(StoredPath {
            root: row.get(1)?,
            path: row.get(2)?,
        })?;
        transcripts.insert(row.get(0)?, Arc::from(path));
    }
    let threads = connection
        .prepare(
            "SELECT id, transcript, label, nickname, role, agent_path, agent_id, launch_call \
         FROM threads WHERE session = ?1",
        )?
        .query_map([session], |row| {
            Ok((
                row.get(0)?,
                Thread {
                    transcript: row
                        .get::<_, Option<i64>>(1)?
                        .map(|id| transcripts[&id].clone()),
                    label: row.get(2)?,
                    identity: SubagentIdentity {
                        nickname: row.get(3)?,
                        role: row.get(4)?,
                        agent_path: row.get(5)?,
                        agent_id: row.get(6)?,
                    },
                    launch_call: row.get(7)?,
                },
            ))
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok(StoredThreads {
        threads,
        transcripts,
    })
}
