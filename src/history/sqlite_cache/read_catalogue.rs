//! Catalogue metadata without loading message bodies.

use super::files::RootKey;
use super::roots::{Places, StoredPath};
use super::{CachedSession, Publication, SessionAddress, SessionRoots, SqliteCache};
use crate::agent::diagnostic::AgentWarning;
use crate::agent::time_span::TimeSpan;
use crate::error::{AppError, Result};
use crate::history::fingerprint::CachedFingerprint;
use crate::history::provider::{RecordedSession, SessionCache, SessionRoot};
use crate::history::{Conversation, MessageTimes, ParseError, Source};
use chrono::{Local, TimeZone};
use rusqlite::{Connection, Row};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// A catalogue row. An empty or unreadable row carries the warning agent
/// commands print for its session, as the owner's load recorded it; `None`
/// for a row recorded without one.
#[derive(Clone)]
pub(crate) enum CatalogueSession {
    Listed(Box<Conversation>),
    Empty(Option<AgentWarning>),
    Unreadable(Option<AgentWarning>),
}

/// A listed catalogue row's transcripts and what its session recorded of
/// itself.
pub(crate) struct ListedSession {
    pub(crate) cache_key: String,
    pub(crate) locator: PathBuf,
    pub(crate) subagents: Vec<PathBuf>,
    pub(crate) recorded: RecordedSession,
}

impl SqliteCache {
    pub(crate) fn catalogue(
        &self,
        storage: SessionCache,
        source: Source,
        root: &SessionRoot,
        show_last: bool,
    ) -> Result<HashMap<String, CatalogueSession>> {
        self.read_catalogue(storage, source, root, show_last, None)
    }

    /// What each session under `root` recorded of itself, by cache key: its
    /// id and working directory when it is listed, `None` when it holds no
    /// conversation or could not be read. Read without the rows' text.
    pub(crate) fn recorded_sessions(
        &self,
        storage: SessionCache,
        root: &SessionRoot,
    ) -> Result<HashMap<String, Option<RecordedSession>>> {
        let roots = self.session_roots(root);
        self.with_read_snapshot(|connection| {
            let mut statement = connection.prepare(&format!(
                "SELECT cache_key, status, session_id, cwd FROM sessions \
                 WHERE storage = ?1 AND root = (SELECT id FROM roots WHERE key = ?2) \
                 AND status != 'writing' AND {}",
                super::catalogue::ENTRIES_NOT_FAILED
            ))?;
            let mut rows = statement.query(rusqlite::params![
                storage.directory,
                roots.key().map(RootKey::as_str)
            ])?;
            let mut sessions = HashMap::new();
            while let Some(row) = rows.next()? {
                let status: String = row.get(1)?;
                let recorded = match status.as_str() {
                    "listed" => Some(RecordedSession {
                        session_id: row.get(2)?,
                        project_dir: row.get::<_, Option<String>>(3)?.map(PathBuf::from),
                    }),
                    "empty" | "unreadable" => None,
                    _ => continue,
                };
                sessions.insert(row.get(0)?, recorded);
            }
            Ok(sessions)
        })
    }

    /// The listed sessions under `root` whose transcript lies under one of
    /// `directories`, in path order, with their paths in `root`'s spelling.
    /// Read without the rows' text; the sub-agents are read for those
    /// sessions alone.
    pub(crate) fn listed_sessions_under(
        &self,
        storage: SessionCache,
        root: &SessionRoot,
        directories: &[&Path],
    ) -> Result<Vec<ListedSession>> {
        let roots = self.session_roots(root);
        self.with_read_snapshot(|connection| {
            let places = Places::stored(connection, &roots)?;
            let mut statement = connection.prepare(&format!(
                "SELECT s.id, s.cache_key, s.session_id, s.cwd, f.root, f.path FROM sessions s \
                 JOIN session_files f ON f.session = s.id AND f.kind = 'locator' \
                 WHERE s.storage = ?1 AND s.root = ?2 AND s.status = 'listed' AND {}",
                super::catalogue::ENTRIES_NOT_FAILED
            ))?;
            let mut rows =
                statement.query(rusqlite::params![storage.directory, places.listing_root()])?;
            let mut sessions = Vec::new();
            while let Some(row) = rows.next()? {
                let locator = match places.path(StoredPath {
                    root: row.get(4)?,
                    path: row.get(5)?,
                }) {
                    Ok(locator) => locator,
                    Err(error) if error.is_changed_source() => continue,
                    Err(error) => return Err(error),
                };
                if !directories
                    .iter()
                    .any(|directory| locator.starts_with(directory))
                {
                    continue;
                }
                let Some(files) = transcripts_of(connection, &places, row.get(0)?)? else {
                    continue;
                };
                sessions.push(ListedSession {
                    cache_key: row.get(1)?,
                    locator: files.locator,
                    subagents: files.subagents,
                    recorded: RecordedSession {
                        session_id: row.get(2)?,
                        project_dir: row.get::<_, Option<String>>(3)?.map(PathBuf::from),
                    },
                });
            }
            sessions.sort_by(|left, right| left.locator.cmp(&right.locator));
            Ok(sessions)
        })
    }

    pub(crate) fn catalogue_session(
        &self,
        storage: SessionCache,
        source: Source,
        root: &SessionRoot,
        key: &str,
        show_last: bool,
    ) -> Result<Option<CatalogueSession>> {
        Ok(self
            .read_catalogue(storage, source, root, show_last, Some(key))?
            .remove(key))
    }

    /// The listed catalogue row of the copy at `address`; `None` when the
    /// cache does not list it.
    pub(crate) fn catalogue_row_at(
        &self,
        address: &SessionAddress,
    ) -> Result<Option<Conversation>> {
        let session = self
            .read_catalogue_through(
                address.storage,
                address.source,
                address.roots.clone(),
                false,
                Some(&address.cache_key),
            )?
            .remove(&address.cache_key);
        Ok(match session {
            Some(CatalogueSession::Listed(row)) => Some(*row),
            _ => None,
        })
    }

    fn read_catalogue(
        &self,
        storage: SessionCache,
        source: Source,
        root: &SessionRoot,
        show_last: bool,
        key: Option<&str>,
    ) -> Result<HashMap<String, CatalogueSession>> {
        self.read_catalogue_through(storage, source, self.session_roots(root), show_last, key)
    }

    fn read_catalogue_through(
        &self,
        storage: SessionCache,
        source: Source,
        roots: SessionRoots,
        show_last: bool,
        key: Option<&str>,
    ) -> Result<HashMap<String, CatalogueSession>> {
        self.with_read_snapshot(|connection| {
            let places = Places::stored(connection, &roots)?;
            let sql = format!(
                "SELECT id, cache_key, status, file_size, mtime_secs, mtime_nsecs, \
                 session_id, project_path, project_name, cwd, timestamp_ms, preview_first, \
                 preview_last, summary, custom_title, model, total_tokens, duration_minutes, \
                 message_count, assistant_messages, is_missing_earlier_turns, entries_error, \
                 generation FROM sessions WHERE storage = ?1 AND root = ?2 \
                 AND status != 'writing' AND {} {}",
                super::catalogue::ENTRIES_NOT_FAILED,
                if key.is_some() {
                    "AND cache_key = ?3"
                } else {
                    ""
                }
            );
            let mut parameters = vec![
                rusqlite::types::Value::from(storage.directory.to_owned()),
                places.listing_root().into(),
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
                    "listed" => {
                        let Some(files) = transcripts_of(connection, &places, row.get(0)?)? else {
                            continue;
                        };
                        let cached_session = CachedSession {
                            base: self.base().clone(),
                            address: SessionAddress::new(
                                storage,
                                source,
                                roots.clone(),
                                row.get(1)?,
                            ),
                            generation: row.get(22)?,
                            wait: self.wait,
                            checks_source: true,
                        };
                        CatalogueSession::Listed(Box::new(conversation(
                            connection,
                            row,
                            files,
                            source,
                            show_last,
                            cached_session,
                        )?))
                    }
                    "empty" => CatalogueSession::Empty(recorded_warning(row)?),
                    "unreadable" => CatalogueSession::Unreadable(recorded_warning(row)?),
                    _ => continue,
                };
                sessions.insert(row.get(1)?, session);
            }
            Ok(sessions)
        })
    }
}

/// The warning an empty or unreadable catalogue `row` records.
fn recorded_warning(row: &Row<'_>) -> Result<Option<AgentWarning>> {
    let recorded: Option<String> = row.get(21)?;
    Ok(recorded.and_then(|recorded| AgentWarning::from_recorded(&recorded)))
}

/// The listed catalogue `row`, whose transcripts are `files`.
fn conversation(
    connection: &Connection,
    row: &Row<'_>,
    files: Transcripts,
    source: Source,
    show_last: bool,
    cached_session: CachedSession,
) -> Result<Conversation> {
    let session: i64 = row.get(0)?;
    let timestamp_ms: i64 = row.get(10)?;
    let timestamp = Local
        .timestamp_millis_opt(timestamp_ms)
        .single()
        .ok_or_else(|| AppError::SessionCacheNotReady {
            session: session.to_string(),
            reason: "invalid catalogue timestamp".to_owned(),
        })?;
    let preview_first: String = row.get(11)?;
    let preview_last: String = row.get(12)?;
    let message_count = row.get::<_, i64>(18)? as usize;
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
        cache_address: Some(cached_session.address().clone()),
        publication: Publication::Published {
            generation: cached_session.generation(),
        },
        cached_session: Some(cached_session),
        source,
        session_id: row.get(6)?,
        subagents: files.subagents,
        path: files.locator,
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
        semantic_turns: Vec::new(),
        semantic_turn_ranges: Vec::new(),
        search_text_lower: String::new(),
        project_path: row.get::<_, Option<String>>(7)?.map(PathBuf::from),
        project_name: row.get(8)?,
        cwd: row.get::<_, Option<String>>(9)?.map(PathBuf::from),
        message_count,
        message_times: MessageTimes::new(&spans),
        message_text_starts: Default::default(),
        assistant_messages: row.get::<_, i64>(19)? as usize,
        parse_errors,
        summary: row.get(13)?,
        custom_title: row.get(14)?,
        model: row.get(15)?,
        total_tokens: row.get::<_, i64>(16)? as u64,
        duration_minutes: row.get::<_, Option<i64>>(17)?.map(|minutes| minutes as u64),
        fingerprint: Some(CachedFingerprint {
            file_size: row.get::<_, i64>(3)? as u64,
            mtime_secs: row.get::<_, i64>(4)? as u64,
            mtime_nsecs: row.get(5)?,
        }),
        has_transient_read_error: false,
        is_missing_earlier_turns: row.get(20)?,
    })
}

/// A listed row's own transcript and its sub-agent transcripts, in order.
struct Transcripts {
    locator: PathBuf,
    subagents: Vec<PathBuf>,
}

/// The transcripts the row `session` lists, at their paths in `places`;
/// `None` for a row with a file below a root `places` lack. The load reads
/// such a session again.
fn transcripts_of(
    connection: &Connection,
    places: &Places,
    session: i64,
) -> Result<Option<Transcripts>> {
    let mut statement = connection.prepare_cached(
        "SELECT kind, root, path FROM session_files WHERE session = ?1 \
         AND kind IN ('locator', 'subagent') ORDER BY position",
    )?;
    let mut rows = statement.query([session])?;
    let mut locator = None;
    let mut subagents = Vec::new();
    while let Some(row) = rows.next()? {
        let path = match places.path(StoredPath {
            root: row.get(1)?,
            path: row.get(2)?,
        }) {
            Ok(path) => path,
            Err(error) if error.is_changed_source() => return Ok(None),
            Err(error) => return Err(error),
        };
        if row.get::<_, String>(0)? == "locator" {
            locator = Some(path);
        } else {
            subagents.push(path);
        }
    }
    Ok(locator.map(|locator| Transcripts { locator, subagents }))
}

fn json_error(error: serde_json::Error) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(error))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::provider::contract_tests::every_record_sessions;
    use crate::history::sqlite_cache::tests::{
        admitted_cache, parsed_record, parsed_record_and_row, root_of, storage_of, stub_of,
    };

    #[test]
    fn rows_content_is_its_copy_as_it_is_and_replaced_copy_is_miss() {
        let home = tempfile::tempdir().unwrap();
        let base = tempfile::tempdir().unwrap();
        let path = home.path().join("session.jsonl");
        let record = |text| {
            serde_json::json!({"type":"user", "timestamp":"2026-01-01T00:00:00.123Z", "message":{"role":"user", "content":text}}).to_string()
        };
        std::fs::write(&path, record("original text")).unwrap();
        let source = Source::Claude;
        let storage = source.provider().storage().cache();
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
        let write = parsed_record(source, &root, &stub);
        cache.write(&write).unwrap();
        let CatalogueSession::Listed(cached) = cache
            .catalogue_session(storage, source, &root, &stub.cache_key, false)
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
            cached.load_content().is_err(),
            "the content of a replaced copy isn't read from its transcript"
        );
        let CatalogueSession::Listed(current) = cache
            .catalogue_session(storage, source, &root, &stub.cache_key, false)
            .unwrap()
            .unwrap()
        else {
            panic!("listed fixture")
        };
        std::fs::write(&path, record("fresh replacement text")).unwrap();
        let content = current.load_content().unwrap();
        assert!(
            content.full_text.contains("original text")
                && !content.full_text.contains("fresh replacement text"),
            "a changed transcript's copy is read without parsing the transcript"
        );
        std::fs::remove_file(&path).unwrap();
        let rows = [*current];
        let searchable = crate::search::precompute_search_text(&rows);
        assert!(
            crate::search::try_search(&rows, &searchable, "original", Local::now()).is_ok(),
            "a row whose transcript is gone is searched in its copy"
        );
    }

    #[test]
    fn catalogue_metadata_matches_each_providers_row_without_reading_bodies() {
        let base = tempfile::tempdir().unwrap();
        let mut cache = admitted_cache(base.path());
        for session in every_record_sessions() {
            let root = root_of(&session);
            let stub = stub_of(&session, 1);
            let (record, original) = parsed_record_and_row(session.row.source, &root, &stub);
            let original = &original;
            let storage = storage_of(original.source);
            cache.admit_storage(storage).unwrap();
            cache.write(&record).unwrap();
            let mut catalogue = cache
                .catalogue(storage, original.source, &root, false)
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
            ] {
                assert_eq!(actual, expected, "{} content", original.source.label());
            }
            let agent_route = |row: &Conversation| {
                crate::agent::service::agent_route_semantic_conversation(row)
                    .map(|route| route.semantic_turns)
            };
            assert_eq!(
                agent_route(&content),
                agent_route(original),
                "the agent's semantic route reads the same text from the {} content as from its parse",
                original.source.label()
            );
            assert_eq!(content.semantic_turns, original.semantic_turns);
            assert_eq!(content.semantic_turn_ranges, original.semantic_turn_ranges);
            assert_eq!(content.message_text_starts, original.message_text_starts);
        }
    }
}
