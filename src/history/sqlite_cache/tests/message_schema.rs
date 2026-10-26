use super::*;
use crate::history::format::RecordLine;
use crate::log_entry::LogEntry;
use serde_json::{Value, json};
use std::sync::Arc;

pub(super) fn transcript(records: Vec<Value>) -> TranscriptEntries {
    TranscriptEntries {
        entries: records
            .into_iter()
            .enumerate()
            .map(|(index, value)| ((index + 1).into(), serde_json::from_value(value).unwrap()))
            .collect(),
        malformed_lines: Vec::new(),
    }
}

#[test]
fn message_links_its_records_and_keeps_its_millisecond_span() {
    let base = tempfile::tempdir().unwrap();
    let mut cache = SqliteCache::open(&super::rearview_at(base.path())).unwrap();
    let records = transcript(vec![
        json!({"type":"user","timestamp":"2026-01-01T10:00:00.125+02:00",
            "message":{"role":"user","content":"check the file"}}),
        json!({"type":"assistant","timestamp":"2026-01-01T08:00:01.250Z",
            "message":{"role":"assistant","id":"reply","model":"model",
                "usage":{"input_tokens":10,"output_tokens":1},
                "content":[{"type":"thinking","thinking":"check","signature":""},{"type":"text","text":"reading"}]}}),
        json!({"type":"assistant","timestamp":"2026-01-01T08:00:05.875Z",
            "message":{"role":"assistant","id":"reply","model":"model",
                "usage":{"input_tokens":10,"output_tokens":3},
                "content":[{"type":"tool_use","id":"call","name":"Read","input":{"path":"notes.txt"}}]}}),
        json!({"type":"user","timestamp":"2026-01-01T08:00:05.875Z",
            "message":{"role":"user","content":"another message at the same instant"}}),
        json!({"type":"pi-metadata","label":"Usage","text":"usage", "searchable":false,
            "usage":{"input_tokens":20,"output_tokens":4}}),
    ]);
    let (storage, root) = write_test_entries(&mut cache, records);
    let first = chrono::DateTime::parse_from_rfc3339("2026-01-01T08:00:01.250Z")
        .unwrap()
        .timestamp_millis();
    let last = first + 4625;
    let span = cache
        .connection
        .query_row(
            "SELECT first_ms,last_ms,usage_output FROM messages WHERE ordinal=2",
            [],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(span, (first, last, 3));
    let matching: Vec<i64> = cache
        .connection
        .prepare(
            "SELECT e.sequence FROM messages m JOIN entries e ON e.message=m.id \
         WHERE m.first_ms<=?1 AND m.last_ms>=?1 AND e.contributes ORDER BY e.sequence",
        )
        .unwrap()
        .query_map([first + 1000], |row| row.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(
        matching,
        [1, 2],
        "a between-record window selects exactly this message's entries"
    );
    let linked_parts: i64 = cache
        .connection
        .query_row(
            "SELECT COUNT(*) FROM parts p JOIN entries e ON e.id=p.entry \
         JOIN messages m ON m.id=e.message WHERE m.ordinal=2",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(linked_parts, 3);
    assert_eq!(
        cache
            .connection
            .query_row(
                "SELECT COUNT(*) FROM entries WHERE usage_input IS NOT NULL",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
        1,
        "only independent usage metadata remains on entries"
    );
    assert_eq!(
        cache
            .connection
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('entries') \
         WHERE name IN ('thread','role','message_id','model','agent','cwd','parent_tool_use_id')",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
        0,
        "shared fields are stored on messages"
    );
    let read = cache
        .entries(storage, &root.path, CACHE_KEY)
        .unwrap()
        .unwrap();
    assert_eq!(
        read.entries[0].1.timestamp(),
        Some("2026-01-01T08:00:00.125Z")
    );
    let LogEntry::PiMetadata {
        usage: Some(usage), ..
    } = &read.entries[4].1
    else {
        panic!("independent usage metadata is preserved")
    };
    assert_eq!(usage.input_tokens, 20);
    assert_eq!(usage.output_tokens, 4);
}

#[test]
fn empty_stream_records_update_usage_without_widening_visible_message() {
    let base = tempfile::tempdir().unwrap();
    let mut cache = SqliteCache::open(&super::rearview_at(base.path())).unwrap();
    write_test_entries(
        &mut cache,
        transcript(vec![
            json!({"type":"user","message":{"role":"user","content":"question"}}),
            json!({"type":"assistant","timestamp":"2026-01-01T00:00:00.000Z",
            "message":{"role":"assistant","id":"reply","content":[]}}),
            json!({"type":"assistant","timestamp":"2026-01-01T00:01:00.125Z",
            "message":{"role":"assistant","id":"reply","content":[{"type":"text","text":"answer"}]}}),
            json!({"type":"assistant","timestamp":"2026-01-01T00:02:00.000Z",
            "message":{"role":"assistant","id":"reply","content":[],
                "usage":{"input_tokens":10,"output_tokens":4}}}),
        ]),
    );
    let (spread, usage): (i64, i64) = cache
        .connection
        .query_row(
            "SELECT last_ms-first_ms,usage_output FROM messages WHERE ordinal=2",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!((spread, usage), (0, 4));
    let sequence: Vec<i64> = cache
        .connection
        .prepare(
            "SELECT e.sequence FROM entries e JOIN messages m ON m.id=e.message \
         WHERE m.ordinal=2 AND e.contributes ORDER BY e.sequence",
        )
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(sequence, [2]);
}

#[test]
fn copying_reply_id_between_threads_doesnt_join_their_time_spans() {
    let base = tempfile::tempdir().unwrap();
    let mut cache = SqliteCache::open(&super::rearview_at(base.path())).unwrap();
    let progress = |timestamp: &str| {
        serde_json::from_value(json!({
            "type":"progress","timestamp":timestamp,
            "data":{"type":"agent_progress","agentId":"shared label",
                "identity":{"nickname":"Scout"},
                "message":{"type":"assistant","message":{"role":"assistant","id":"copied-reply",
                    "content":[{"type":"text","text":"same recorded content"}]}}}
        }))
        .unwrap()
    };
    let mut records = Vec::new();
    for (path, day) in [("first.jsonl", 1), ("second.jsonl", 3)] {
        for (line, second) in [(1, 0), (2, 5)] {
            records.push((
                RecordLine {
                    transcript: Some(Arc::from(Path::new(path))),
                    line,
                },
                progress(&format!("2026-01-{day:02}T00:00:{second:02}.125Z")),
            ));
        }
    }
    write_test_entries(
        &mut cache,
        TranscriptEntries {
            entries: records,
            malformed_lines: Vec::new(),
        },
    );
    let spans: Vec<(i64, i64, i64)> = cache
        .connection
        .prepare(
            "SELECT m.ordinal,m.last_ms-m.first_ms,COUNT(e.id) FROM messages m \
         JOIN entries e ON e.message=m.id GROUP BY m.id ORDER BY m.ordinal",
        )
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(spans, [(1, 5000, 2), (2, 5000, 2)]);
}

#[test]
fn absent_or_unparseable_entry_time_doesnt_become_epoch_zero() {
    let base = tempfile::tempdir().unwrap();
    let mut cache = SqliteCache::open(&super::rearview_at(base.path())).unwrap();
    let (storage, root) = write_test_entries(
        &mut cache,
        transcript(vec![
            json!({"type":"user","message":{"role":"user","content":"no recorded time"}}),
            json!({"type":"assistant","timestamp":"invalid","message":{"role":"assistant",
            "content":[{"type":"text","text":"invalid recorded time"}]}}),
            json!({"type":"assistant","timestamp":"1970-01-01T00:00:00.000Z","message":{"role":"assistant",
            "content":[{"type":"text","text":"actual epoch zero"}]}}),
        ]),
    );
    let times: Vec<Option<i64>> = cache
        .connection
        .prepare("SELECT timestamp_ms FROM entries ORDER BY sequence")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(times, [None, None, Some(0)]);
    let read = cache
        .entries(storage, &root.path, CACHE_KEY)
        .unwrap()
        .unwrap();
    assert_eq!(read.entries[0].1.timestamp(), None);
    assert_eq!(read.entries[1].1.timestamp(), None);
    assert_eq!(
        read.entries[2].1.timestamp(),
        Some("1970-01-01T00:00:00.000Z")
    );
}
