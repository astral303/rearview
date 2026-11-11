use super::*;
use crate::agent::protocol::{
    ProtocolOptions, ReadRequest, format_outline_with_warnings, format_read,
};
use crate::agent::refs::MessageRange;
use crate::agent::test_support::resolved_conversation;
use crate::agent::transcript::AgentTranscript;
use crate::time_filter::TimeFilter;

#[test]
fn bounded_reads_and_outlines_keep_global_links_handles_and_times_for_every_provider() {
    let base = tempfile::tempdir().unwrap();
    let mut cache = admitted_cache(base.path());
    for session in every_record_sessions() {
        let root = root_of(&session);
        let stub = stub_of(&session, 1);
        let (record, row) = parsed_record_and_row(session.row.source, &root, &stub);
        let row = &row;
        let storage = storage_of(row.source);
        cache.admit_storage(storage).unwrap();
        cache.write(&record).unwrap();
        let super::super::CatalogueSession::Listed(cached) = cache
            .catalogue_session(storage, row.source, &root, &stub.cache_key, false)
            .unwrap()
            .unwrap()
        else {
            panic!("the fixture session is listed")
        };
        let expected =
            AgentTranscript::load_cached_for_test(row.source, &row.path, &row.subagents).unwrap();
        let resolved = resolved_conversation(row);
        for start in 1..=expected.messages.len() {
            for end in [start, (start + 2).min(expected.messages.len())] {
                let range = MessageRange { start, end };
                let actual = cached
                    .cached_session
                    .as_ref()
                    .unwrap()
                    .read_copy(|held| held.agent_range(&row.path, range))
                    .unwrap();
                assert_eq!(actual.messages.len(), expected.messages.len());
                for full in [false, true] {
                    for budget in [None, Some(600)] {
                        let options = ProtocolOptions {
                            budget,
                            tools: full,
                            tool_results: full,
                            thinking: full,
                            subagents: full,
                            window: TimeFilter::default(),
                        };
                        let request = |transcript| ReadRequest {
                            resolved: &resolved,
                            transcript,
                            range: Some(range),
                            subagent: None,
                            call: None,
                        };
                        assert_eq!(
                            format_read(&[request(&actual)], None, None, options).unwrap(),
                            format_read(&[request(&expected)], None, None, options).unwrap(),
                            "{} read m{start}..m{end}, full={full}, budget={budget:?}",
                            row.source.label()
                        );
                        let written_at = crate::agent::test_support::written_at();
                        assert_eq!(
                            format_outline_with_warnings(
                                &request(&actual),
                                options,
                                &[],
                                written_at
                            )
                            .unwrap(),
                            format_outline_with_warnings(
                                &request(&expected),
                                options,
                                &[],
                                written_at
                            )
                            .unwrap(),
                            "{} outline m{start}..m{end}, full={full}, budget={budget:?}",
                            row.source.label()
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn bounded_transcript_doesnt_load_unrelated_large_call_and_result_bodies() {
    let base = tempfile::tempdir().unwrap();
    let source = base.path().join("source.jsonl");
    let large = "large body ".repeat(400_000);
    let records = [
        serde_json::json!({"type":"user", "message":{"role":"user", "content":"small requested message"}}),
        serde_json::json!({"type":"assistant", "message":{"role":"assistant", "content":[{"type":"tool_use", "id":"write", "name":"Write", "input":{"content":large}}]}}),
        serde_json::json!({"type":"user", "message":{"role":"user", "content":[{"type":"tool_result", "tool_use_id":"write", "content":large}]}}),
    ].map(|record| record.to_string()).join("\n");
    std::fs::write(&source, records).unwrap();
    let (record, root) = source_record(Source::Claude, &source);
    let mut cache = admitted_cache(&base.path().join("cache"));
    let storage = storage_of(Source::Claude);
    cache.admit_storage(storage).unwrap();
    cache.write(&record).unwrap();
    let super::super::CatalogueSession::Listed(cached) = cache
        .catalogue_session(storage, Source::Claude, &root, CACHE_KEY, false)
        .unwrap()
        .unwrap()
    else {
        panic!("the fixture session is listed")
    };
    let transcript = cached
        .cached_session
        .as_ref()
        .unwrap()
        .read_copy(|held| held.agent_range(&source, MessageRange::single(1)))
        .unwrap();
    assert_eq!(transcript.messages.len(), 3);
    assert!(
        format!("{transcript:?}").len() < 4000,
        "unrelated bodies are not retained in the context"
    );
    assert!(format!("{:?}", transcript.messages[0]).contains("small requested message"));
}

/// A bounded read reads each call's sub-agent link fields from the stored
/// context, without reading the call's input: with every stored input
/// replaced by text that is not JSON, the read is unchanged. The calls are
/// Claude Code's: a foreground and a background `Agent` launch, and a
/// `SendMessage` to a sub-agent.
#[test]
fn bounded_read_doesnt_read_unselected_call_inputs() {
    let base = tempfile::tempdir().unwrap();
    let source = base.path().join("source.jsonl");
    let records = [
        serde_json::json!({"type":"user", "message":{"role":"user", "content":"small requested message"}}),
        serde_json::json!({"type":"assistant", "message":{"role":"assistant", "content":[
            {"type":"tool_use", "id":"scout", "name":"Agent", "input":{"description":"Scout", "subagent_type":"explorer", "prompt":"Look around"}},
            {"type":"tool_use", "id":"watcher", "name":"Agent", "input":{"description":"Watch the build", "subagent_type":"general-purpose", "prompt":"Watch the build", "run_in_background":true}},
            {"type":"tool_use", "id":"nudge", "name":"SendMessage", "input":{"recipient":"worker-1", "message":"Start with src.", "summary":"nudge"}},
        ]}}),
        serde_json::json!({"type":"user", "message":{"role":"user", "content":[
            {"type":"tool_result", "tool_use_id":"scout", "content":"Done"},
            {"type":"tool_result", "tool_use_id":"watcher", "content":"Started"},
            {"type":"tool_result", "tool_use_id":"nudge", "content":"Sent"},
        ]}}),
    ].map(|record| record.to_string()).join("\n");
    std::fs::write(&source, records).unwrap();
    let (record, root) = source_record(Source::Claude, &source);
    let mut cache = admitted_cache(&base.path().join("cache"));
    let storage = storage_of(Source::Claude);
    cache.admit_storage(storage).unwrap();
    cache.write(&record).unwrap();
    let super::super::CatalogueSession::Listed(cached) = cache
        .catalogue_session(storage, Source::Claude, &root, CACHE_KEY, false)
        .unwrap()
        .unwrap()
    else {
        panic!("the fixture session is listed")
    };
    let read = || {
        cached
            .cached_session
            .as_ref()
            .unwrap()
            .read_copy(|held| held.agent_range(&source, MessageRange::single(1)))
            .unwrap()
    };
    let before = read();
    cache
        .connection
        .execute("UPDATE parts SET json = 'not JSON' WHERE kind = 'call'", [])
        .unwrap();

    let after = read();

    assert_eq!(
        format!("{after:?}"),
        format!("{before:?}"),
        "the read gives the call its link fields without its input"
    );
    let calls = format!("{:?}", after.messages[1]);
    for (field, value) in [
        ("subagent type", "explorer"),
        ("background launch", "run_in_background"),
        ("message recipient", "worker-1"),
    ] {
        assert!(
            calls.contains(value),
            "the calls' context holds the {field}: {calls}"
        );
    }
}

#[test]
fn call_context_is_stored_for_agent_calls_alone_without_absent_fields() {
    let base = tempfile::tempdir().unwrap();
    let source = base.path().join("source.jsonl");
    let records = [
        serde_json::json!({"type":"user", "message":{"role":"user", "content":"run it"}}),
        serde_json::json!({"type":"assistant", "message":{"role":"assistant", "content":[
            {"type":"tool_use", "id":"list", "name":"Bash", "input":{"command":"ls", "description":"List files", "run_in_background":false}},
            {"type":"tool_use", "id":"scout", "name":"Agent", "input":{"description":"Scout", "prompt":"Look around"}},
        ]}}),
    ].map(|record| record.to_string()).join("\n");
    std::fs::write(&source, records).unwrap();
    let (record, _) = source_record(Source::Claude, &source);
    let mut cache = admitted_cache(&base.path().join("cache"));
    cache.admit_storage(storage_of(Source::Claude)).unwrap();
    cache.write(&record).unwrap();

    let contexts: Vec<(String, Option<String>)> = cache
        .connection
        .prepare("SELECT call_id, call_context FROM parts WHERE kind = 'call' ORDER BY call_id")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();

    assert_eq!(
        contexts,
        [
            ("list".to_owned(), None),
            (
                "scout".to_owned(),
                Some(r#"{"description":"Scout"}"#.to_owned())
            ),
        ]
    );
}

/// The pages SQLite reads into a cache of eight pages to run a bounded
/// read's query of the calls and results, over a Claude session whose
/// `Agent` call has a prompt of `body_bytes` and whose result as many bytes
/// of text.
fn calls_and_results_page_reads(body_bytes: usize) -> i32 {
    let base = tempfile::tempdir().unwrap();
    let source = base.path().join("source.jsonl");
    let body = "b".repeat(body_bytes);
    let records = [
        serde_json::json!({"type":"user", "message":{"role":"user", "content":"small requested message"}}),
        serde_json::json!({"type":"assistant", "message":{"role":"assistant", "content":[{"type":"tool_use", "id":"scout", "name":"Agent", "input":{"description":"Scout", "subagent_type":"explorer", "prompt":body}}]}}),
        serde_json::json!({"type":"user", "message":{"role":"user", "content":[{"type":"tool_result", "tool_use_id":"scout", "content":body}]}}),
    ].map(|record| record.to_string()).join("\n");
    std::fs::write(&source, records).unwrap();
    let (record, _) = source_record(Source::Claude, &source);
    let cache_base = base.path().join("cache");
    let mut cache = admitted_cache(&cache_base);
    cache.admit_storage(storage_of(Source::Claude)).unwrap();
    cache.write(&record).unwrap();
    let session: i64 = cache
        .connection
        .query_row("SELECT id FROM sessions", [], |row| row.get(0))
        .unwrap();
    let reader = Connection::open_with_flags(
        cache_base.join(super::super::DATABASE_FILE_NAME),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    reader.pragma_update(None, "cache_size", 8).unwrap();
    let mut statement = reader
        .prepare(super::super::entries::CALLS_AND_RESULTS)
        .unwrap();
    let mut rows = statement.query([session]).unwrap();
    while let Some(row) = rows.next().unwrap() {
        for column in 0..6 {
            let _: rusqlite::types::Value = row.get(column).unwrap();
        }
    }
    let (mut misses, mut highest) = (0, 0);
    // SAFETY: the handle is `reader`'s, open for the call, and both
    // pointers are to live ints.
    let code = unsafe {
        rusqlite::ffi::sqlite3_db_status(
            reader.handle(),
            rusqlite::ffi::SQLITE_DBSTATUS_CACHE_MISS,
            &mut misses,
            &mut highest,
            0,
        )
    };
    assert_eq!(code, rusqlite::ffi::SQLITE_OK);
    misses
}

#[test]
fn bounded_read_of_calls_doesnt_read_the_overflow_pages_of_large_inputs_and_results() {
    let small = calls_and_results_page_reads(16);
    let large = calls_and_results_page_reads(1 << 20);

    assert!(
        large <= small + 4,
        "the read doesn't reach the overflow pages of a 1 MiB prompt or result, within 4 \
         pages: {small} pages for 16 bytes, {large} for 1 MiB"
    );
}

#[test]
fn sql_windows_keep_full_message_spans_and_final_millisecond() {
    let base = tempfile::tempdir().unwrap();
    let source = base.path().join("source.jsonl");
    let records = [
        serde_json::json!({"type":"user", "timestamp":"2026-01-01T00:00:00.000Z", "message":{"role":"user", "content":"question"}}),
        serde_json::json!({"type":"assistant", "timestamp":"2026-01-01T00:00:00.100Z", "message":{"role":"assistant", "id":"reply", "content":[{"type":"text", "text":"opening fragment"}]}}),
        serde_json::json!({"type":"user", "timestamp":"2026-01-01T00:00:00.500Z", "message":{"role":"user", "content":"outside window"}}),
        serde_json::json!({"type":"assistant", "timestamp":"2026-01-01T00:00:01.999Z", "message":{"role":"assistant", "id":"reply", "content":[{"type":"text", "text":"closing fragment"}]}}),
        serde_json::json!({"type":"user", "timestamp":"2026-01-01T00:00:01.999Z", "message":{"role":"user", "content":"last millisecond"}}),
    ].map(|record| record.to_string()).join("\n");
    std::fs::write(&source, records).unwrap();
    let (record, root) = source_record(Source::Claude, &source);
    let mut cache = admitted_cache(&base.path().join("cache"));
    let storage = storage_of(Source::Claude);
    cache.admit_storage(storage).unwrap();
    cache.write(&record).unwrap();
    let super::super::CatalogueSession::Listed(cached) = cache
        .catalogue_session(storage, Source::Claude, &root, CACHE_KEY, false)
        .unwrap()
        .unwrap()
    else {
        panic!("the fixture session is listed")
    };
    let second = chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:01Z")
        .unwrap()
        .with_timezone(&crate::clock::Zone::System);
    let window = TimeFilter {
        after: Some(second),
        before: Some(second),
    };
    let mut selected = Vec::new();
    cached
        .cached_session
        .as_ref()
        .unwrap()
        .read_copy(|held| {
            held.visit_search_texts(SearchedFields::List, &window, |text| {
                selected.push((text.order, text.ordinal, text.text.to_owned()));
                std::ops::ControlFlow::Continue(())
            })
        })
        .unwrap();
    selected.sort();
    assert_eq!(
        selected
            .into_iter()
            .map(|(_, ordinal, text)| (ordinal, text))
            .collect::<Vec<_>>(),
        [
            (Some(2), "opening fragment".to_owned()),
            (Some(2), "closing fragment".to_owned()),
            (Some(4), "last millisecond".to_owned())
        ]
    );
}
