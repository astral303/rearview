use crate::agent::message_numbering::{ThreadIdentity, ThreadNumbers};
use crate::history::format::RecordLine;
use crate::log_entry::{LogEntry, SubagentIdentity};
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Default, Hash, PartialEq, Eq)]
struct ThreadOriginKey {
    transcript: Option<i64>,
    label: Option<String>,
    nickname: Option<String>,
    role: Option<String>,
    agent_path: Option<String>,
    agent_id: Option<String>,
    launch_call: Option<String>,
}

#[derive(Default)]
pub(super) struct Numbering {
    transcripts: HashMap<Arc<Path>, i64>,
    origins: HashMap<ThreadOriginKey, i64>,
    positions: HashMap<i64, i64>,
    logical_threads: ThreadNumbers,
}

pub(super) struct Placement {
    pub(super) thread: i64,
    pub(super) logical_thread: i64,
    pub(super) position: i64,
    pub(super) transcript: Option<i64>,
}

impl Numbering {
    fn transcript(
        &mut self,
        connection: &Connection,
        session: i64,
        record: &RecordLine,
    ) -> rusqlite::Result<Option<i64>> {
        let Some(path) = &record.transcript else {
            return Ok(None);
        };
        if let Some(id) = self.transcripts.get(path) {
            return Ok(Some(*id));
        }
        connection.execute(
            "INSERT INTO transcripts (session, path) VALUES (?1, ?2)",
            params![session, super::path_text(path)],
        )?;
        let id = connection.last_insert_rowid();
        self.transcripts.insert(path.clone(), id);
        Ok(Some(id))
    }

    pub(super) fn place(
        &mut self,
        connection: &Connection,
        session: i64,
        record: &RecordLine,
        entry: &LogEntry,
    ) -> rusqlite::Result<Placement> {
        let transcript = self.transcript(connection, session, record)?;
        let key = match entry {
            LogEntry::User {
                parent_tool_use_id: Some(call),
                ..
            }
            | LogEntry::Assistant {
                parent_tool_use_id: Some(call),
                ..
            } => ThreadOriginKey {
                label: Some(call.clone()),
                launch_call: Some(call.clone()),
                ..ThreadOriginKey::default()
            },
            LogEntry::Progress { data, extra }
                if data.get("type").and_then(Value::as_str) == Some("agent_progress")
                    && data.get("agentId").and_then(Value::as_str).is_some() =>
            {
                let text =
                    |value: &Value, key| value.get(key).and_then(Value::as_str).map(str::to_owned);
                let identity = &data["identity"];
                ThreadOriginKey {
                    transcript,
                    label: text(data, "agentId"),
                    nickname: text(identity, "nickname"),
                    role: text(identity, "role"),
                    agent_path: text(identity, "agentPath"),
                    agent_id: text(identity, "agentId"),
                    launch_call: text(extra, "parentToolUseID"),
                }
            }
            _ => ThreadOriginKey::default(),
        };
        // Metadata changes get their own stored origin, preserving each
        // message's attribution without repeating the strings on entries.
        let record_transcript = transcript.filter(|value| Some(*value) != key.transcript);
        let number = self
            .logical_threads
            .number(ThreadIdentity::of(entry, record.transcript.as_deref()));
        let next_position = self.positions.entry(number).or_default();
        let position = *next_position;
        *next_position += 1;
        let thread = match self.origins.entry(key) {
            Entry::Occupied(entry) => *entry.get(),
            Entry::Vacant(entry) => {
                let key = entry.key();
                connection.execute(
                    "INSERT INTO threads (session, number, transcript, label, nickname, role, \
                 agent_path, agent_id, launch_call) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                    params![
                        session,
                        number,
                        key.transcript,
                        key.label,
                        key.nickname,
                        key.role,
                        key.agent_path,
                        key.agent_id,
                        key.launch_call
                    ],
                )?;
                *entry.insert(connection.last_insert_rowid())
            }
        };
        Ok(Placement {
            thread,
            logical_thread: number,
            position,
            transcript: record_transcript,
        })
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

pub(super) fn read(connection: &Connection, session: i64) -> rusqlite::Result<StoredThreads> {
    let transcripts: HashMap<i64, Arc<Path>> = connection
        .prepare("SELECT id, path FROM transcripts WHERE session = ?1")?
        .query_map([session], |row| {
            Ok((
                row.get(0)?,
                Arc::from(PathBuf::from(row.get::<_, String>(1)?)),
            ))
        })?
        .collect::<rusqlite::Result<_>>()?;
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
