//! Catalogue metadata without loading message bodies.

use super::{CachedSession, SqliteCache, path_text};
use crate::agent::time_span::TimeSpan;
use crate::error::Result;
use crate::history::cache::CachedFingerprint;
use crate::history::provider::SessionCache;
use crate::history::{Conversation, MessageTimes, ParseError, Source};
use chrono::{Local, TimeZone};
use rusqlite::{Connection, Row};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Clone)]
pub(crate) enum CatalogueSession {
    Listed(Box<Conversation>),
    Empty,
    Unreadable,
}

impl SqliteCache {
    pub(crate) fn catalogue(
        &self,
        storage: SessionCache,
        source: Source,
        root: &Path,
        show_last: bool,
    ) -> Result<HashMap<String, CatalogueSession>> {
        self.read_catalogue(storage, source, root, show_last, None)
    }

    pub(crate) fn catalogue_session(
        &self,
        storage: SessionCache,
        source: Source,
        root: &Path,
        key: &str,
        show_last: bool,
    ) -> Result<Option<CatalogueSession>> {
        Ok(self
            .read_catalogue(storage, source, root, show_last, Some(key))?
            .remove(key))
    }

    fn read_catalogue(
        &self,
        storage: SessionCache,
        source: Source,
        root: &Path,
        show_last: bool,
        key: Option<&str>,
    ) -> Result<HashMap<String, CatalogueSession>> {
        self.with_read_snapshot(|connection| {
            let sql = format!(
                "SELECT id, cache_key, status, locator, file_size, mtime_secs, mtime_nsecs, \
                 session_id, project_path, project_name, cwd, timestamp_ms, preview_first, \
                 preview_last, summary, custom_title, model, total_tokens, duration_minutes, \
                 message_count, assistant_messages, is_missing_earlier_turns \
                 FROM sessions WHERE storage = ?1 AND root = ?2 AND status != 'writing' \
                 AND entries_error IS NULL {}",
                if key.is_some() {
                    "AND cache_key = ?3"
                } else {
                    ""
                }
            );
            let mut parameters = vec![
                rusqlite::types::Value::from(storage.directory.to_owned()),
                path_text(root).into(),
            ];
            if let Some(key) = key {
                parameters.push(key.to_owned().into());
            }
            let mut statement = connection.prepare(&sql)?;
            let mut rows = statement.query(rusqlite::params_from_iter(parameters))?;
            let mut sessions = HashMap::new();
            while let Some(row) = rows.next()? {
                let status: String = row.get(2)?;
                let session = match status.as_str() {
                    "listed" => CatalogueSession::Listed(Box::new(conversation(
                        connection,
                        row,
                        source,
                        show_last,
                        CachedSession {
                            base: self.base().clone(),
                            storage,
                            root: root.to_path_buf(),
                            cache_key: row.get(1)?,
                            generation: row.get(0)?,
                            source,
                            wait: self.wait,
                        },
                    )?)),
                    "empty" => CatalogueSession::Empty,
                    "unreadable" => CatalogueSession::Unreadable,
                    _ => continue,
                };
                sessions.insert(row.get(1)?, session);
            }
            Ok(sessions)
        })
    }
}

fn conversation(
    connection: &Connection,
    row: &Row<'_>,
    source: Source,
    show_last: bool,
    cached_session: CachedSession,
) -> Result<Conversation> {
    let session: i64 = row.get(0)?;
    let timestamp_ms: i64 = row.get(11)?;
    let timestamp = Local
        .timestamp_millis_opt(timestamp_ms)
        .single()
        .ok_or_else(|| crate::error::AppError::SessionCacheNotReady {
            session: session.to_string(),
            reason: "invalid catalogue timestamp".to_owned(),
        })?;
    let preview_first: String = row.get(12)?;
    let preview_last: String = row.get(13)?;
    let subagents = connection.prepare_cached(
        "SELECT path FROM session_files WHERE session = ?1 AND kind = 'subagent' ORDER BY position",
    )?.query_map([session], |row| row.get::<_, String>(0).map(PathBuf::from))?
        .collect::<rusqlite::Result<_>>()?;
    let message_count = row.get::<_, i64>(19)? as usize;
    let mut spans = vec![None; message_count];
    let mut statement = connection.prepare_cached(
        "SELECT ordinal, first_ms, last_ms FROM messages WHERE session = ?1 AND ordinal IS NOT NULL ORDER BY ordinal",
    )?;
    let mut times = statement.query([session])?;
    while let Some(time) = times.next()? {
        let ordinal = time.get::<_, i64>(0)? as usize;
        let first: Option<i64> = time.get(1)?;
        let last: Option<i64> = time.get(2)?;
        if let Some(slot) = ordinal
            .checked_sub(1)
            .and_then(|index| spans.get_mut(index))
        {
            *slot = first.zip(last).map(|(first, last)| {
                TimeSpan::from_seconds(first.div_euclid(1000), last.div_euclid(1000))
            });
        }
    }
    let parse_errors = connection.prepare_cached(
        "SELECT line_number, line_content, error_message, context_before, context_after FROM parse_errors WHERE session = ?1 ORDER BY position",
    )?.query_map([session], |row| Ok(ParseError {
        line_number: row.get::<_, i64>(0)? as usize, line_content: row.get(1)?, error_message: row.get(2)?,
        context_before: serde_json::from_str(&row.get::<_, String>(3)?).map_err(json_error)?,
        context_after: serde_json::from_str(&row.get::<_, String>(4)?).map_err(json_error)?,
    }))?.collect::<rusqlite::Result<_>>()?;
    Ok(Conversation {
        cached_session: Some(cached_session),
        source,
        session_id: row.get(7)?,
        subagents,
        path: row.get::<_, String>(3)?.into(),
        index: 0,
        timestamp,
        preview: if show_last {
            preview_last.clone()
        } else {
            preview_first.clone()
        },
        preview_first,
        preview_last,
        full_text: String::new(),
        agent_search_text: String::new(),
        thinking_and_tool_call_text: String::new(),
        semantic_route_text: String::new(),
        semantic_turns: Vec::new(),
        semantic_turn_ranges: Vec::new(),
        search_text_lower: String::new(),
        project_path: row.get::<_, Option<String>>(8)?.map(PathBuf::from),
        project_name: row.get(9)?,
        cwd: row.get::<_, Option<String>>(10)?.map(PathBuf::from),
        message_count,
        message_times: MessageTimes::new(&spans),
        message_text_starts: Default::default(),
        assistant_messages: row.get::<_, i64>(20)? as usize,
        parse_errors,
        summary: row.get(14)?,
        custom_title: row.get(15)?,
        model: row.get(16)?,
        total_tokens: row.get::<_, i64>(17)? as u64,
        duration_minutes: row.get::<_, Option<i64>>(18)?.map(|minutes| minutes as u64),
        fingerprint: Some(CachedFingerprint {
            file_size: row.get::<_, i64>(4)? as u64,
            mtime_secs: row.get::<_, i64>(5)? as u64,
            mtime_nsecs: row.get(6)?,
        }),
        has_transient_read_error: false,
        is_missing_earlier_turns: row.get(21)?,
    })
}

fn json_error(error: serde_json::Error) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(error))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::provider::contract_tests::every_record_sessions;
    use crate::history::sqlite_cache::{
        SessionRecord,
        tests::{admitted_cache, root_of, storage_of, stub_of},
    };

    #[test]
    fn changed_sources_reparse_and_missing_sources_fail_search() {
        let home = tempfile::tempdir().unwrap();
        let base = tempfile::tempdir().unwrap();
        let path = home.path().join("session.jsonl");
        let record = |text| {
            serde_json::json!({"type":"user", "timestamp":"2026-01-01T00:00:00.123Z", "message":{"role":"user", "content":text}}).to_string()
        };
        std::fs::write(&path, record("original text")).unwrap();
        let source = Source::Claude;
        let storage = source.provider().storage().cache();
        let original = source
            .provider()
            .format()
            .parse_conversation(
                &path,
                crate::history::parser::RowDating::new(None, &crate::clock::TestClock::default()),
                None,
            )
            .unwrap()
            .unwrap();
        let root = crate::history::provider::SessionRoot::new(home.path());
        let stub = crate::history::provider::SessionStub {
            locator: path.clone(),
            subagents: Vec::new(),
            cache_key: "session".to_owned(),
            fingerprint: crate::history::provider::Fingerprint {
                size: 1000,
                modified: Some(std::time::UNIX_EPOCH),
            },
        };
        let mut cache = admitted_cache(base.path());
        cache.admit_storage(storage).unwrap();
        let write = SessionRecord::listed(storage, source, &root, &stub, &original).unwrap();
        cache.write(&write).unwrap();
        let CatalogueSession::Listed(cached) = cache
            .catalogue_session(storage, source, &root.path, &stub.cache_key, false)
            .unwrap()
            .unwrap()
        else {
            panic!("listed fixture")
        };
        cache.write(&write).unwrap();
        assert!(
            cached
                .cached_session
                .as_ref()
                .unwrap()
                .numbered_entries(None)
                .is_err(),
            "replaced generations are not silently reused"
        );
        assert!(
            cached
                .load_content()
                .unwrap()
                .full_text
                .contains("original text")
        );
        std::fs::write(&path, record("fresh replacement text")).unwrap();
        assert!(
            cached
                .load_content()
                .unwrap()
                .full_text
                .contains("fresh replacement text")
        );
        std::fs::remove_file(&path).unwrap();
        let rows = [*cached];
        let searchable = crate::search::precompute_search_text(&rows);
        assert!(crate::search::try_search(&rows, &searchable, "original", Local::now()).is_err());
    }

    #[test]
    fn catalogue_metadata_matches_each_providers_row_without_reading_bodies() {
        let base = tempfile::tempdir().unwrap();
        let mut cache = admitted_cache(base.path());
        for session in every_record_sessions() {
            let original = &session.row;
            let storage = storage_of(original.source);
            let root = root_of(&session);
            let stub = stub_of(&session, 1);
            cache.admit_storage(storage).unwrap();
            cache
                .write(
                    &SessionRecord::listed(storage, original.source, &root, &stub, original)
                        .unwrap(),
                )
                .unwrap();
            let mut catalogue = cache
                .catalogue(storage, original.source, &root.path, false)
                .unwrap();
            let CatalogueSession::Listed(cached) = catalogue.remove(&stub.cache_key).unwrap()
            else {
                panic!("listed session")
            };
            assert_eq!(cached.session_id, original.session_id);
            assert_eq!(cached.timestamp, original.timestamp);
            assert_eq!(cached.preview_first, original.preview_first);
            assert_eq!(cached.preview_last, original.preview_last);
            assert_eq!(cached.message_count, original.message_count);
            assert_eq!(cached.message_times, original.message_times);
            assert_eq!(cached.assistant_messages, original.assistant_messages);
            assert_eq!(cached.total_tokens, original.total_tokens);
            assert_eq!(cached.summary, original.summary);
            assert_eq!(cached.custom_title, original.custom_title);
            assert_eq!(cached.subagents.len(), original.subagents.len());
            assert!(cached.full_text.is_empty());
            assert!(cached.semantic_turns.is_empty());
            let mut parts = 0;
            assert_eq!(
                cached
                    .cached_session
                    .as_ref()
                    .unwrap()
                    .search_parts(&cached, |_| parts += 1)
                    .unwrap(),
                Some(())
            );
            assert!(parts > 0);
            let content = cached.load_content().unwrap();
            for (actual, expected) in [
                (&content.full_text, &original.full_text),
                (&content.agent_search_text, &original.agent_search_text),
                (
                    &content.thinking_and_tool_call_text,
                    &original.thinking_and_tool_call_text,
                ),
                (&content.semantic_route_text, &original.semantic_route_text),
            ] {
                assert_eq!(actual, expected, "{} content", original.source.label());
            }
            assert_eq!(content.semantic_turns, original.semantic_turns);
            assert_eq!(content.semantic_turn_ranges, original.semantic_turn_ranges);
            assert_eq!(content.message_text_starts, original.message_text_starts);
        }
    }
}
