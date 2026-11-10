use crate::agent::refs::MessageRange;
use crate::agent::time_span::TimeSpan;
use crate::history::message_texts::{MessageStarts, MessageTextBuilder};
use crate::history::{Conversation, MessageTextStarts};
use crate::search::normalize_for_search;
use crate::time_filter::TimeFilter;
use chrono::{DateTime, Local, TimeZone};
use std::path::PathBuf;

/// Claude Code sessions in a temporary directory, each cached in a session
/// cache of its own directory, with their rows as the cache lists them.
pub struct CachedClaudeSessions {
    home: tempfile::TempDir,
    cache: tempfile::TempDir,
    names: Vec<String>,
    pub rows: Vec<Conversation>,
}

/// Writes one Claude Code transcript per `(name, records)` pair, each record
/// one line, and caches each session as a load does.
pub fn cached_claude_sessions(
    transcripts: &[(&str, Vec<serde_json::Value>)],
) -> CachedClaudeSessions {
    use crate::history::Source;
    use crate::history::sqlite_cache::tests::rearview_at;
    use crate::history::sqlite_cache::{CatalogueSession, SqliteCache};

    let home = tempfile::tempdir().unwrap();
    let cache_directory = tempfile::tempdir().unwrap();
    let mut cache = SqliteCache::open(&rearview_at(cache_directory.path())).unwrap();
    cache.mark_admitted().unwrap();
    let storage = Source::Claude.provider().storage().cache();
    cache.admit_storage(storage).unwrap();
    let mut sessions = CachedClaudeSessions {
        home,
        cache: cache_directory,
        names: Vec::new(),
        rows: Vec::new(),
    };
    for (name, records) in transcripts {
        sessions.names.push((*name).to_owned());
        let writer = sessions.writer(sessions.names.len() - 1);
        writer.write_to(&mut cache, records);
        let CatalogueSession::Listed(cached) = cache
            .catalogue_session(storage, Source::Claude, &writer.root(), name, false)
            .unwrap()
            .unwrap()
        else {
            panic!("the session {name} is listed")
        };
        sessions.rows.push(*cached);
    }
    sessions
}

impl CachedClaudeSessions {
    /// The session cache's directory.
    pub fn cache_path(&self) -> &std::path::Path {
        self.cache.path()
    }

    /// The writer of the session of row `index`, as another rearview writes
    /// it. The row keeps naming the copy it read.
    pub fn writer(&self, index: usize) -> CachedClaudeSessionWriter {
        CachedClaudeSessionWriter {
            home: self.home.path().to_path_buf(),
            cache: self.cache.path().to_path_buf(),
            name: self.names[index].clone(),
        }
    }

    /// [`CachedClaudeSessionWriter::rewrite`] of row `index`.
    pub fn rewrite(&self, index: usize, records: &[serde_json::Value]) {
        self.writer(index).rewrite(records);
    }

    /// [`CachedClaudeSessionWriter::remove`] of row `index`.
    pub fn remove(&self, index: usize) {
        self.writer(index).remove();
    }

    /// Stores `bytes`, whether or not they are UTF-8, as the text of each
    /// text part of row `index`'s session, keeping its copy's generation.
    pub fn store_texts(&self, index: usize, bytes: &[u8]) {
        crate::history::sqlite_cache::tests::store_text_parts_of(
            self.cache.path(),
            &self.names[index],
            bytes,
        );
    }

    /// Row `index`'s session as the cache lists it now.
    pub fn listed_row(&self, index: usize) -> Conversation {
        use crate::history::Source;
        use crate::history::sqlite_cache::{CatalogueSession, SqliteCache};
        let writer = self.writer(index);
        let cache = SqliteCache::open(&crate::history::sqlite_cache::tests::rearview_at(
            self.cache.path(),
        ))
        .unwrap();
        let storage = Source::Claude.provider().storage().cache();
        let Some(CatalogueSession::Listed(row)) = cache
            .catalogue_session(storage, Source::Claude, &writer.root(), &writer.name, false)
            .unwrap()
        else {
            panic!("the cache lists the session {}", writer.name)
        };
        *row
    }
}

/// One session of [`CachedClaudeSessions`], written by another rearview.
pub struct CachedClaudeSessionWriter {
    home: PathBuf,
    cache: PathBuf,
    name: String,
}

impl CachedClaudeSessionWriter {
    /// Writes the session again from `records`, as a load writes a changed
    /// transcript: the cache then holds another copy of it.
    pub fn rewrite(&self, records: &[serde_json::Value]) {
        let mut cache = crate::history::sqlite_cache::SqliteCache::open(
            &crate::history::sqlite_cache::tests::rearview_at(&self.cache),
        )
        .unwrap();
        self.write_to(&mut cache, records);
    }

    /// Removes the session from the cache.
    pub fn remove(&self) {
        crate::history::sqlite_cache::tests::remove_sessions_where(
            &self.cache,
            "cache_key",
            &self.name,
        );
    }

    fn root(&self) -> crate::history::provider::SessionRoot {
        crate::history::provider::SessionRoot::new(&self.home)
    }

    /// Writes the transcript from `records`, and its record to `cache`.
    fn write_to(
        &self,
        cache: &mut crate::history::sqlite_cache::OwnedCache,
        records: &[serde_json::Value],
    ) {
        use crate::history::Source;
        use crate::history::provider::{Fingerprint, SessionStub};
        let path = self.home.join(format!("{}.jsonl", self.name));
        let lines = records
            .iter()
            .map(serde_json::Value::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&path, &lines).unwrap();
        let stub = SessionStub {
            locator: path,
            subagents: Vec::new(),
            cache_key: self.name.clone(),
            fingerprint: Fingerprint {
                size: lines.len() as u64,
                modified: Some(std::time::UNIX_EPOCH),
            },
        };
        let (record, _) = crate::history::sqlite_cache::tests::parsed_record_and_row(
            Source::Claude,
            &self.root(),
            &stub,
        );
        cache.write(&record).unwrap();
    }
}

/// A Claude Code user message with `text`, sent at `time`.
pub fn claude_user_record(text: &str, time: DateTime<Local>) -> serde_json::Value {
    serde_json::json!({
        "type": "user",
        "timestamp": time.to_rfc3339(),
        "message": {"role": "user", "content": text}
    })
}

/// Claude Code user messages with `texts`, an hour apart from noon on
/// 10 October.
pub fn october_messages(texts: &[&str]) -> Vec<serde_json::Value> {
    texts
        .iter()
        .zip(12..)
        .map(|(text, hour)| claude_user_record(text, october(10, hour)))
        .collect()
}

/// `day` October 2026 at `hour`, local time.
pub fn october(day: u32, hour: u32) -> DateTime<Local> {
    Local
        .with_ymd_and_hms(2026, 10, day, hour, 0, 0)
        .single()
        .expect("an unambiguous local time")
}

/// 10 October 2026 from 00:00 to 23:00.
pub fn window_on_october_10() -> TimeFilter {
    let at = |hour| october(10, hour).with_timezone(&crate::clock::Zone::System);
    TimeFilter {
        after: Some(at(0)),
        before: Some(at(23)),
    }
}

/// A session titled `title`, with one message per `(text, time)` pair. The
/// session's time is its last message's.
pub fn timed_messages_conversation(
    messages: &[(&str, DateTime<Local>)],
    title: Option<&str>,
) -> Conversation {
    let texts = messages.iter().map(|(text, _)| *text).collect::<Vec<_>>();
    let (full_text, full_text_starts) = text_with_starts(&texts, messages.len());
    let spans = messages
        .iter()
        .map(|(_, time)| Some(TimeSpan::from_seconds(time.timestamp(), time.timestamp())))
        .collect::<Vec<_>>();
    let timestamp = messages.last().map_or_else(Local::now, |(_, time)| *time);
    Conversation {
        search_text_lower: normalize_for_search(&full_text),
        full_text,
        message_count: messages.len(),
        message_times: crate::history::MessageTimes::new(&spans),
        message_text_starts: MessageTextStarts {
            full_text: full_text_starts,
            ..MessageTextStarts::default()
        },
        custom_title: title.map(str::to_string),
        ..one_message_conversation("", timestamp, None, None, None)
    }
}

/// `conversation` with `subagent_texts` as its `agent_search_text` and
/// `thinking_and_tool_call_texts` as its `thinking_and_tool_call_text`, one
/// per message in order.
pub fn with_agent_texts(
    mut conversation: Conversation,
    subagent_texts: &[&str],
    thinking_and_tool_call_texts: &[&str],
) -> Conversation {
    let message_count = conversation.message_count;
    let starts = &mut conversation.message_text_starts;
    (conversation.agent_search_text, starts.agent_search_text) =
        text_with_starts(subagent_texts, message_count);
    (
        conversation.thinking_and_tool_call_text,
        starts.thinking_and_tool_call_text,
    ) = text_with_starts(thinking_and_tool_call_texts, message_count);
    conversation
}

/// `texts` as the text of messages m1, m2, …, and where each message's text
/// starts.
fn text_with_starts(texts: &[&str], message_count: usize) -> (String, MessageStarts) {
    let mut builder = MessageTextBuilder::default();
    for (index, text) in texts.iter().enumerate() {
        builder.push(Some(index + 1), (*text).to_owned());
    }
    builder.finish(message_count, str::to_owned)
}

/// A session of one message, written at `timestamp`.
pub fn one_message_conversation(
    text: &str,
    timestamp: DateTime<Local>,
    summary: Option<&str>,
    title: Option<&str>,
    project: Option<&str>,
) -> Conversation {
    let mut builder = MessageTextBuilder::default();
    builder.push(Some(1), text.to_string());
    let (full_text, full_text_starts) = builder.finish(1, str::to_owned);

    Conversation {
        source: crate::history::Source::Claude,
        subagents: Vec::new(),
        session_id: String::new(),
        path: PathBuf::new(),
        index: 0,
        timestamp,
        preview: text.to_string(),
        preview_first: text.to_string(),
        preview_last: text.to_string(),
        full_text: full_text.clone(),
        agent_search_text: String::new(),
        thinking_and_tool_call_text: String::new(),
        semantic_turns: vec![text.to_string()],
        semantic_turn_ranges: vec![MessageRange::single(1)],
        search_text_lower: normalize_for_search(&full_text),
        project_name: project.map(str::to_string),
        project_path: None,
        cwd: None,
        message_count: 1,
        message_times: crate::history::MessageTimes::new(&[Some(TimeSpan::from_seconds(
            timestamp.timestamp(),
            timestamp.timestamp(),
        ))]),
        message_text_starts: MessageTextStarts {
            full_text: full_text_starts,
            ..MessageTextStarts::default()
        },
        assistant_messages: 1,
        parse_errors: vec![],
        summary: summary.map(str::to_string),
        custom_title: title.map(str::to_string),
        external_title: None,
        model: None,
        total_tokens: 0,
        duration_minutes: None,
        fingerprint: None,
        has_transient_read_error: false,
        is_missing_earlier_turns: false,
        cached_session: None,
        cache_address: None,
        publication: crate::history::sqlite_cache::Publication::Unpublished {
            discovered: Vec::new(),
        },
    }
}
