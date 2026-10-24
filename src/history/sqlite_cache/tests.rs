use super::*;
use crate::clock::{Clock, TestClock};
use crate::history::provider::contract_tests::{AgentSession, every_record_sessions};
use crate::history::provider::{Fingerprint, SessionRoot, SessionStub};
use crate::history::{DisplayEntries, Source, normalized_session};
use std::time::{Duration, UNIX_EPOCH};

mod bounded;
mod lifecycle;
mod message_schema;
mod sources;

const CACHE_KEY: &str = "every-record";

/// The cache in `directory` as one rearview reaches it, on test time.
pub(crate) fn rearview_at(directory: &Path) -> CacheBase {
    CacheBase::new(
        directory,
        ProcessIdentity::default(),
        std::sync::Arc::new(TestClock::default()),
    )
}

pub(super) fn admitted_cache(base: &Path) -> super::OwnedCache {
    let cache = SqliteCache::open(&rearview_at(base)).unwrap();
    cache.mark_admitted().unwrap();
    cache
}

fn source_record(source: Source, path: &Path) -> (SessionRecord, SessionRoot) {
    let session = AgentSession::new(source, path.to_path_buf(), Vec::new(), Vec::new());
    let root = root_of(&session);
    let record = SessionRecord::listed(
        crate::history::FOR_FIXTURES,
        storage_of(source),
        source,
        &root,
        &stub_of(&session, 1),
        &session.row,
    )
    .unwrap();
    (record, root)
}

/// `session`'s stub, as discovery would find its transcripts at `modified`.
pub(super) fn stub_of(session: &AgentSession, modified_secs: u64) -> SessionStub {
    let fingerprint = if session.row.source == Source::OpenCode {
        let root = root_of(session);
        let stamp = session
            .row
            .source
            .provider()
            .storage()
            .current_fingerprint(&root, &session.row.path)
            .unwrap()
            .unwrap();
        Fingerprint {
            size: stamp.file_size,
            modified: Some(UNIX_EPOCH + Duration::new(stamp.mtime_secs, stamp.mtime_nsecs)),
        }
    } else {
        Fingerprint {
            size: 1_000,
            modified: Some(UNIX_EPOCH + Duration::from_secs(modified_secs)),
        }
    };
    SessionStub {
        locator: session.row.path.clone(),
        subagents: session.row.subagents.clone(),
        cache_key: CACHE_KEY.to_owned(),
        fingerprint,
    }
}

pub(super) fn root_of(session: &AgentSession) -> SessionRoot {
    SessionRoot::new(session.row.path.parent().unwrap())
}

pub(super) fn storage_of(source: Source) -> SessionCache {
    source.provider().storage().cache()
}

#[test]
fn every_agents_cached_entries_preserve_agent_messages_and_viewer_output() {
    use crate::agent::transcript::AgentTranscript;
    use crate::tui::viewer::{self, RenderOptions, ToolDisplayMode};

    let base = tempfile::tempdir().unwrap();
    let mut cache = admitted_cache(base.path());
    for session in every_record_sessions() {
        let row = &session.row;
        let storage = storage_of(row.source);
        let root = root_of(&session);
        let record = SessionRecord::listed(
            crate::history::FOR_FIXTURES,
            storage,
            row.source,
            &root,
            &stub_of(&session, 1),
            row,
        )
        .unwrap();
        cache.write(&record).unwrap();
        let subagents = cache
            .connection
            .prepare(
                "SELECT f.path FROM session_files f JOIN sessions s ON s.id=f.session \
                 WHERE f.kind='subagent' AND s.storage=?1 AND s.root=?2 AND s.cache_key=?3 \
                 ORDER BY f.position",
            )
            .unwrap()
            .query_map(
                params![storage.directory, path_text(&root.path), CACHE_KEY],
                |row| row.get::<_, String>(0),
            )
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(
            subagents,
            row.subagents
                .iter()
                .map(|path| path_text(&files::key(path)))
                .collect::<Vec<_>>()
        );
        let expected = normalized_session(
            crate::history::FOR_FIXTURES,
            row.source,
            &row.path,
            &row.subagents,
        )
        .unwrap();

        let read = cache
            .entries(storage, &root.path, CACHE_KEY)
            .unwrap()
            .unwrap();

        let malformed = |entries: &TranscriptEntries| {
            entries
                .malformed_lines
                .iter()
                .map(|line| line.line_number)
                .collect::<Vec<_>>()
        };
        let malformed_details = |entries: &TranscriptEntries| {
            entries
                .malformed_lines
                .iter()
                .map(|line| {
                    format!(
                        "{} {:?}",
                        line.line_number,
                        line.detail.as_ref().map(|detail| (
                            &detail.line_content,
                            &detail.error_message,
                            &detail.context_before,
                            &detail.context_after
                        ))
                    )
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(malformed_details(&read), malformed_details(&expected));
        assert_eq!(
            canonical_message_times(
                AgentTranscript::from_entries(&row.path, read.entries.clone(), malformed(&read))
                    .unwrap()
            ),
            canonical_message_times(
                AgentTranscript::from_entries(
                    &row.path,
                    expected.entries.clone(),
                    malformed(&expected)
                )
                .unwrap()
            ),
            "{:?} agent messages",
            row.source,
        );
        assert!(
            expected
                .entries
                .iter()
                .any(|(record, _)| record.transcript.is_some())
                || row.subagents.is_empty(),
            "{:?}'s every-record session splices its sub-agents in",
            row.source
        );
        let expected = viewer::parsed_conversation(DisplayEntries::from(expected));
        let read = viewer::parsed_conversation(DisplayEntries::from(read));
        for tool_display in [
            ToolDisplayMode::Hidden,
            ToolDisplayMode::Truncated,
            ToolDisplayMode::Full,
        ] {
            for show_thinking in [false, true] {
                for show_timing in [false, true] {
                    let options = RenderOptions {
                        tool_display,
                        show_thinking,
                        show_timing,
                        content_width: 120,
                        expanded_tool_outputs: Default::default(),
                        can_expand: false,
                        today: TestClock::default().today(),
                        zone: TestClock::default().zone(),
                    };
                    let expected = viewer::render_parsed_conversation(&expected, &options);
                    let read = viewer::render_parsed_conversation(&read, &options);
                    assert_eq!(
                        format!("{:?}", read.lines),
                        format!("{:?}", expected.lines),
                        "{:?} viewer",
                        row.source
                    );
                    assert_eq!(
                        format!("{:?}", read.messages),
                        format!("{:?}", expected.messages)
                    );
                    assert_eq!(format!("{:?}", read.calls), format!("{:?}", expected.calls));
                }
            }
        }
    }
}

fn canonical_message_times(
    mut transcript: crate::agent::transcript::AgentTranscript,
) -> crate::agent::transcript::AgentTranscript {
    use crate::agent::transcript::AgentMessagePart;
    for message in &mut transcript.messages {
        for part in &mut message.parts {
            let (AgentMessagePart::Text { source, .. }
            | AgentMessagePart::ToolUse { source, .. }
            | AgentMessagePart::ToolResult { source, .. }
            | AgentMessagePart::Thinking { source, .. }) = part;
            source.timestamp = source
                .timestamp
                .as_deref()
                .and_then(|text| chrono::DateTime::parse_from_rfc3339(text).ok())
                .map(|time| {
                    time.with_timezone(&chrono::Utc)
                        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
                });
        }
    }
    transcript
}

pub(super) fn write_test_entries(
    cache: &mut CacheWriter,
    entries: TranscriptEntries,
) -> (SessionCache, SessionRoot) {
    let session = every_record_sessions().remove(0);
    let storage = storage_of(session.row.source);
    let root = root_of(&session);
    let mut record = SessionRecord::listed(
        crate::history::FOR_FIXTURES,
        storage,
        session.row.source,
        &root,
        &stub_of(&session, 1),
        &session.row,
    )
    .unwrap();
    record.entries = Some(Ok(entries));
    record.catalogue.files.clear();
    cache.mark_admitted().unwrap();
    cache.write(&record).unwrap();
    (storage, root)
}

#[test]
fn every_tool_result_is_stored_and_other_opaque_payloads_keep_only_their_positions() {
    use crate::history::format::RecordLine;
    use crate::log_entry::{ContentBlock, LogEntry, UserContent, UserMessage};
    use serde_json::{Value, json};

    let base = tempfile::tempdir().unwrap();
    let mut cache = admitted_cache(base.path());
    let source = base.path().join("source.jsonl");
    let unused = "opaque-payload".repeat(100_000);
    let blocks = vec![
        ContentBlock::Thinking {
            thinking: "retained thinking".to_owned(),
            signature: unused.clone(),
        },
        ContentBlock::Image {
            source: json!({"type": "base64", "data": unused}),
        },
        ContentBlock::ToolResult {
            tool_use_id: "mixed".to_owned(),
            standalone_tool_name: None,
            content: Some(json!([
                {"type": "image", "source": {"data": unused}},
                {"type": "text", "text": "retained result"},
                "retained bare string",
                {"type": "tool_reference", "tool_name": "unused tool"},
                {"type": "text", "text": "extra fields", "metadata": {"count": 1}},
            ])),
        },
        ContentBlock::Text {
            text: "question".to_owned(),
        },
    ];
    let original_record = RecordLine {
        transcript: Some(std::sync::Arc::from(source.as_path())),
        line: 1,
    };
    let expected_entries = vec![
        (
            original_record.clone(),
            LogEntry::User {
                message: UserMessage {
                    role: "user".to_owned(),
                    content: UserContent::Blocks(blocks),
                },
                timestamp: None,
                uuid: None,
                cwd: None,
                parent_tool_use_id: None,
                source_tool_use_id: None,
                usage: None,
            },
        ),
        (
            2.into(),
            LogEntry::FileHistorySnapshot {
                message_id: unused.clone(),
                snapshot: json!({"unused": unused}),
                is_snapshot_update: true,
            },
        ),
    ];
    let lines = expected_entries
        .iter()
        .map(|(_, entry)| serde_json::to_string(entry).unwrap())
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(&source, lines).unwrap();
    let (mut record, root) = source_record(Source::Claude, &source);
    record.entries = Some(Ok(TranscriptEntries {
        entries: expected_entries.clone(),
        malformed_lines: Vec::new(),
    }));
    let storage = storage_of(Source::Claude);
    cache.write(&record).unwrap();
    let read = cache
        .entries(storage, &root.path, CACHE_KEY)
        .unwrap()
        .unwrap();
    assert_eq!(read.entries[0].0, original_record);
    let LogEntry::User {
        message:
            UserMessage {
                content: UserContent::Blocks(blocks),
                ..
            },
        ..
    } = &read.entries[0].1
    else {
        panic!("a user message")
    };
    assert!(
        matches!(&blocks[0], ContentBlock::Thinking { thinking, signature } if thinking == "retained thinking" && signature.is_empty())
    );
    assert!(matches!(
        &blocks[1],
        ContentBlock::Image {
            source: Value::Null
        }
    ));
    assert_eq!(
        blocks.len(),
        4,
        "the original part indexes survive omitted images"
    );
    let ContentBlock::ToolResult {
        content: Some(content),
        ..
    } = &blocks[2]
    else {
        panic!("a result")
    };
    assert_eq!(content[1]["text"], "retained result");
    assert_eq!(content[2], "retained bare string");
    assert!(content[0].get("source").is_some());
    assert_eq!(content[4]["metadata"]["count"], 1);
    assert!(matches!(
        &read.entries[1].1,
        LogEntry::FileHistorySnapshot {
            snapshot: Value::Null,
            ..
        }
    ));
    use crate::agent::transcript::{AgentTranscript, agent_part_text};
    use crate::tui::viewer::{self, RenderOptions, ToolDisplayMode};
    let path = std::path::Path::new("session.jsonl");
    let expected_agent =
        AgentTranscript::from_entries(path, expected_entries.clone(), Vec::new()).unwrap();
    let cached_agent =
        AgentTranscript::from_entries(path, read.entries.clone(), Vec::new()).unwrap();
    let text_and_sources = |agent: &AgentTranscript| {
        agent
            .messages
            .iter()
            .flat_map(|message| {
                message
                    .parts
                    .iter()
                    .map(|part| (agent_part_text(part), part.source().clone()))
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(
        text_and_sources(&cached_agent),
        text_and_sources(&expected_agent)
    );
    let key =
        crate::agent::refs::AgentConversationKey::new("project", "source.jsonl", source.clone());
    let resolved = crate::agent::refs::ResolvedConversation {
        reference: key.conversation_ref(),
        key,
    };
    for (expected, cached) in expected_agent.messages.iter().zip(&cached_agent.messages) {
        let anchor = expected_agent.message_anchor(&resolved, expected);
        assert_eq!(cached_agent.message_anchor(&resolved, cached), anchor);
        assert_eq!(
            cached_agent.resolve_anchor(&resolved, &anchor).unwrap(),
            expected.ordinal
        );
    }
    let options = RenderOptions {
        tool_display: ToolDisplayMode::Full,
        show_thinking: true,
        show_timing: false,
        content_width: 120,
        expanded_tool_outputs: Default::default(),
        can_expand: false,
        today: TestClock::default().today(),
        zone: TestClock::default().zone(),
    };
    let expected_view = viewer::render_parsed_conversation(
        &viewer::parsed_conversation(DisplayEntries::from(expected_entries)),
        &options,
    );
    let cached_view = viewer::render_parsed_conversation(
        &viewer::parsed_conversation(DisplayEntries::from(read)),
        &options,
    );
    assert_eq!(
        format!("{:?}", cached_view.lines),
        format!("{:?}", expected_view.lines)
    );
    assert_eq!(
        format!("{:?}", cached_view.calls),
        format!("{:?}", expected_view.calls)
    );
    cache.checkpoint().unwrap();
    let size = std::fs::metadata(database_path(base.path())).unwrap().len();
    assert!(
        size < 3 * 1024 * 1024,
        "the thinking signature, image and snapshot payloads stay out of the database: {size} bytes"
    );
}

#[test]
fn subagent_metadata_is_shared_across_transactions_and_preserves_origins() {
    use crate::history::format::RecordLine;
    use crate::log_entry::LogEntry;
    use serde_json::json;
    use std::sync::Arc;

    let base = tempfile::tempdir().unwrap();
    let mut cache = admitted_cache(base.path());
    let progress = |nickname: &str, reply: &str| LogEntry::Progress {
        data: json!({"type": "agent_progress", "agentId": "shared label",
            "identity": {"nickname": nickname, "role": "reviewer", "agentPath": "/root/reviewer"},
            "message": {"type": "assistant", "message": {"role": "assistant", "id": reply, "content": [{"type": "text", "text": "answer"}]}}}),
        extra: json!({"timestamp": "2026-01-01T00:00:00Z", "parentToolUseID": "launch"}),
    };
    let first: Arc<std::path::Path> = Arc::from(std::path::Path::new("first.jsonl"));
    let second: Arc<std::path::Path> = Arc::from(std::path::Path::new("second.jsonl"));
    let mut entries: Vec<_> = (1..=20_001)
        .map(|line| {
            (
                RecordLine {
                    transcript: Some(first.clone()),
                    line,
                },
                progress("Scout", &format!("reply-{line}")),
            )
        })
        .collect();
    entries.push((
        RecordLine {
            transcript: Some(second.clone()),
            line: 1,
        },
        progress("Scout", "second-thread-reply"),
    ));
    entries.push((
        RecordLine {
            transcript: Some(first.clone()),
            line: 20_002,
        },
        progress("Renamed", "renamed-reply"),
    ));
    let (storage, root) = write_test_entries(
        &mut cache,
        TranscriptEntries {
            entries,
            malformed_lines: Vec::new(),
        },
    );
    let rows: (i64, i64, i64) = cache
        .connection
        .query_row(
            "SELECT (SELECT COUNT(*) FROM threads), (SELECT COUNT(*) FROM transcripts), \
         (SELECT COUNT(*) FROM entries)",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(rows, (3, 2, 20_003));
    let positions = cache
        .connection
        .prepare(
            "SELECT t.number, e.position FROM entries e JOIN messages m ON m.id=e.message \
             JOIN threads t ON t.id=m.thread \
         WHERE e.sequence >= 20000 ORDER BY e.sequence",
        )
        .unwrap()
        .query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(
        positions,
        [(1, 20_000), (2, 0), (1, 20_001)],
        "a metadata change keeps the logical thread and its next position"
    );
    let raw_wrappers: i64 = cache
        .connection
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('entries') WHERE name IN ('data', 'extra')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(raw_wrappers, 0);
    let read = cache
        .entries(storage, &root.path, CACHE_KEY)
        .unwrap()
        .unwrap();
    assert_eq!(read.entries[20_000].0.transcript.as_ref(), Some(&first));
    assert_eq!(read.entries[20_001].0.transcript.as_ref(), Some(&second));
    let LogEntry::Progress { data, extra } = &read.entries[20_002].1 else {
        panic!("a sub-agent turn")
    };
    assert_eq!(data["identity"]["nickname"], "Renamed");
    assert_eq!(data["message"]["message"]["id"], "renamed-reply");
    assert_eq!(extra["parentToolUseID"], "launch");
    assert_eq!(
        cache
            .connection
            .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap(),
        0
    );
}

#[test]
fn results_rendered_as_raw_json_are_stored_as_recorded() {
    use crate::log_entry::{ContentBlock, LogEntry, UserContent, UserMessage};
    use serde_json::json;

    let base = tempfile::tempdir().unwrap();
    let source = base.path().join("source.jsonl");
    let result_content =
        json!([{"type": "image", "source": {"type": "base64", "data": "encoded image"}}]);
    let lines = [
        json!({"type": "user", "message": {"role": "user", "content": "question"}}),
        json!({"type": "user", "message": {"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "image-result", "content": result_content},
            {"type": "tool_result", "tool_use_id": "object-result", "content": {"task_name": "/root/scout"}},
        ]}}),
    ].map(|line| line.to_string()).join("\n");
    std::fs::write(&source, &lines).unwrap();
    let session = every_record_sessions()
        .into_iter()
        .find(|session| session.row.source == Source::Claude)
        .unwrap();
    let storage = storage_of(Source::Claude);
    let root = root_of(&session);
    let mut record = SessionRecord::listed(
        crate::history::FOR_FIXTURES,
        storage,
        Source::Claude,
        &root,
        &stub_of(&session, 1),
        &session.row,
    )
    .unwrap();
    record.catalogue.locator = source.to_string_lossy().into_owned();
    let metadata = std::fs::metadata(&source).unwrap();
    record.catalogue.files = vec![catalogue::SessionFile {
        kind: "locator",
        path: record.catalogue.locator.clone(),
        fingerprint: Some(CachedFingerprint::of(
            metadata.len(),
            metadata.modified().unwrap(),
        )),
    }];
    record.entries = Some(Ok(normalized_session(
        crate::history::FOR_FIXTURES,
        Source::Claude,
        &source,
        &[],
    )
    .unwrap()));
    let mut cache = admitted_cache(&base.path().join("cache"));
    cache.write(&record).unwrap();
    let read = cache
        .entries(storage, &root.path, CACHE_KEY)
        .unwrap()
        .unwrap();
    let LogEntry::User {
        message:
            UserMessage {
                content: UserContent::Blocks(blocks),
                ..
            },
        ..
    } = &read.entries[1].1
    else {
        panic!("a user result entry")
    };
    assert!(
        matches!(&blocks[0], ContentBlock::ToolResult { content: Some(value), .. } if value == &result_content)
    );
    assert!(
        matches!(&blocks[1], ContentBlock::ToolResult { content: Some(value), .. } if value == &json!({"task_name": "/root/scout"}))
    );
    assert_eq!(
        cache
            .connection
            .query_row(
                "SELECT COUNT(*) FROM parts WHERE result_shape = 'json'",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
        2,
        "each of the two results that are neither text nor text items is stored as a JSON part"
    );

    std::fs::write(&source, lines.replace("image-result", "replaced-result")).unwrap();
    assert!(
        cache.entries(storage, &root.path, CACHE_KEY).is_err(),
        "a changed source doesn't silently supply a different result"
    );
}

#[test]
fn writing_a_session_again_replaces_its_rows() {
    let base = tempfile::tempdir().unwrap();
    let mut cache = admitted_cache(base.path());
    let session = every_record_sessions().remove(0);
    let row = &session.row;
    let storage = storage_of(row.source);
    let root = root_of(&session);
    let write = |cache: &mut CacheWriter, modified_secs| {
        let stub = stub_of(&session, modified_secs);
        cache
            .write(
                &SessionRecord::listed(
                    crate::history::FOR_FIXTURES,
                    storage,
                    row.source,
                    &root,
                    &stub,
                    row,
                )
                .unwrap(),
            )
            .unwrap();
    };
    write(&mut cache, 1);

    write(&mut cache, 2);

    let current = cache.current_fingerprints(storage, &root.path).unwrap();
    assert_eq!(
        current
            .get(CACHE_KEY)
            .map(|fingerprint| fingerprint.mtime_secs),
        Some(2)
    );
    let sessions: i64 = cache
        .connection
        .query_row("SELECT COUNT(*) FROM sessions", [], |row| row.get(0))
        .unwrap();
    assert_eq!(sessions, 1);
    let expected = normalized_session(
        crate::history::FOR_FIXTURES,
        row.source,
        &row.path,
        &row.subagents,
    )
    .unwrap();
    let read = cache
        .entries(storage, &root.path, CACHE_KEY)
        .unwrap()
        .unwrap();
    assert_eq!(read.entries.len(), expected.entries.len());
}

/// A `SessionCache::schema_version` bump invalidates that provider's
/// database rows.
#[test]
fn a_storage_written_under_another_schema_version_loses_its_sessions() {
    let base = tempfile::tempdir().unwrap();
    let mut cache = admitted_cache(base.path());
    let session = every_record_sessions().remove(0);
    let row = &session.row;
    let storage = storage_of(row.source);
    let root = root_of(&session);
    cache.admit_storage(storage).unwrap();
    cache
        .write(
            &SessionRecord::listed(
                crate::history::FOR_FIXTURES,
                storage,
                row.source,
                &root,
                &stub_of(&session, 1),
                row,
            )
            .unwrap(),
        )
        .unwrap();

    cache.admit_storage(storage).unwrap();
    let kept = cache.current_fingerprints(storage, &root.path).unwrap();
    cache
        .admit_storage(SessionCache {
            schema_version: storage.schema_version + 1,
            ..storage
        })
        .unwrap();

    assert_eq!(kept.len(), 1, "the same version keeps the sessions");
    assert!(
        cache
            .current_fingerprints(storage, &root.path)
            .unwrap()
            .is_empty()
    );
}

/// A run that stopped between a session's catalogue row and its last entry
/// leaves the row marked as writing, which is not current.
#[test]
fn a_session_still_being_written_is_not_current() {
    let base = tempfile::tempdir().unwrap();
    let mut cache = admitted_cache(base.path());
    let session = every_record_sessions().remove(0);
    let row = &session.row;
    let storage = storage_of(row.source);
    let root = root_of(&session);
    cache
        .write(
            &SessionRecord::listed(
                crate::history::FOR_FIXTURES,
                storage,
                row.source,
                &root,
                &stub_of(&session, 1),
                row,
            )
            .unwrap(),
        )
        .unwrap();

    cache
        .connection
        .execute("UPDATE sessions SET status = ?1", [catalogue::WRITING])
        .unwrap();

    assert!(
        cache
            .current_fingerprints(storage, &root.path)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn a_removed_session_leaves_no_rows() {
    let base = tempfile::tempdir().unwrap();
    let mut cache = admitted_cache(base.path());
    let session = every_record_sessions().remove(0);
    let row = &session.row;
    let storage = storage_of(row.source);
    let root = root_of(&session);
    cache
        .write(
            &SessionRecord::listed(
                crate::history::FOR_FIXTURES,
                storage,
                row.source,
                &root,
                &stub_of(&session, 1),
                row,
            )
            .unwrap(),
        )
        .unwrap();

    cache
        .remove(storage, &root.path, &[CACHE_KEY.to_owned()])
        .unwrap();

    for table in schema::TABLES.iter().filter(|table| **table != "meta") {
        let rows: i64 = cache
            .connection
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(rows, 0, "{table}");
    }
}

/// A database written under another version of these tables is emptied and
/// built again.
#[test]
fn a_database_of_another_schema_version_is_emptied() {
    let base = tempfile::tempdir().unwrap();
    let mut cache = admitted_cache(base.path());
    let session = every_record_sessions().remove(0);
    let row = &session.row;
    let storage = storage_of(row.source);
    let root = root_of(&session);
    cache
        .write(
            &SessionRecord::listed(
                crate::history::FOR_FIXTURES,
                storage,
                row.source,
                &root,
                &stub_of(&session, 1),
                row,
            )
            .unwrap(),
        )
        .unwrap();
    cache
        .set_meta(
            schema::SCHEMA_VERSION_KEY,
            &(schema::SCHEMA_VERSION - 1).to_string(),
        )
        .unwrap();
    cache.connection.execute_batch(
        "CREATE TABLE message_times (session INTEGER, ordinal INTEGER, first_second INTEGER, last_second INTEGER); \
         INSERT INTO message_times VALUES (1, 1, 0, 0);"
    ).unwrap();
    cache
        .connection
        .execute(
            "UPDATE parts SET text=?1 WHERE kind='text'",
            ["retained text".repeat(100_000)],
        )
        .unwrap();
    cache.checkpoint().unwrap();
    let previous_bytes = std::fs::metadata(database_path(base.path())).unwrap().len();
    drop(cache);

    let reopened = SqliteCache::open(&rearview_at(base.path())).unwrap();

    assert!(reopened.is_admitted().unwrap());
    assert_eq!(
        reopened
            .connection
            .query_row("SELECT COUNT(*) FROM sessions", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
    assert_eq!(
        reopened
            .connection
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE name='message_times'",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
        0
    );
    assert!(
        std::fs::metadata(database_path(base.path())).unwrap().len() < previous_bytes,
        "a rebuild releases the previous database's allocation"
    );
    assert_eq!(
        reopened
            .connection
            .pragma_query_value(None, "page_size", |row| row.get::<_, u32>(0))
            .unwrap(),
        PAGE_SIZE
    );
    assert_eq!(
        reopened
            .connection
            .pragma_query_value(None, "journal_mode", |row| row.get::<_, String>(0))
            .unwrap(),
        "wal"
    );
}

#[test]
fn a_one_kib_cache_is_rebuilt_at_the_selected_size() {
    let base = tempfile::tempdir().unwrap();
    let mut cache = SqliteCache::open(&rearview_at(base.path())).unwrap();
    write_test_entries(
        &mut cache,
        TranscriptEntries {
            entries: Vec::new(),
            malformed_lines: Vec::new(),
        },
    );
    cache
        .connection
        .pragma_update(None, "journal_mode", "DELETE")
        .unwrap();
    cache
        .connection
        .pragma_update(None, "page_size", 1024)
        .unwrap();
    cache.connection.execute_batch("VACUUM").unwrap();
    drop(cache);
    let reopened = SqliteCache::open(&rearview_at(base.path())).unwrap();
    assert!(reopened.is_admitted().unwrap());
    assert_eq!(
        reopened
            .connection
            .query_row("SELECT COUNT(*) FROM sessions", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
    assert_eq!(
        reopened
            .connection
            .pragma_query_value(None, "page_size", |row| row.get::<_, u32>(0))
            .unwrap(),
        PAGE_SIZE
    );
}
