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
        let row = &session.row;
        let storage = storage_of(row.source);
        cache.admit_storage(storage).unwrap();
        let root = root_of(&session);
        let stub = stub_of(&session, 1);
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
        let super::super::CatalogueSession::Listed(cached) = cache
            .catalogue_session(storage, row.source, &root.path, &stub.cache_key, false)
            .unwrap()
            .unwrap()
        else {
            panic!("listed fixture")
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
                    .agent_range(&row.path, range)
                    .unwrap()
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
fn a_bounded_transcript_doesnt_load_unrelated_large_call_and_result_bodies() {
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
        .catalogue_session(storage, Source::Claude, &root.path, CACHE_KEY, false)
        .unwrap()
        .unwrap()
    else {
        panic!("listed fixture")
    };
    let transcript = cached
        .cached_session
        .as_ref()
        .unwrap()
        .agent_range(&source, MessageRange::single(1))
        .unwrap()
        .unwrap();
    assert_eq!(transcript.messages.len(), 3);
    assert!(
        format!("{transcript:?}").len() < 4000,
        "unrelated bodies are not retained in the context"
    );
    assert!(format!("{:?}", transcript.messages[0]).contains("small requested message"));
}

#[test]
fn sql_windows_keep_full_message_spans_and_the_final_millisecond() {
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
        .catalogue_session(storage, Source::Claude, &root.path, CACHE_KEY, false)
        .unwrap()
        .unwrap()
    else {
        panic!("listed fixture")
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
        .search_parts_within(&cached, &window, |part| {
            if part.field == super::super::SearchField::Full {
                selected.push((part.ordinal, part.text));
            }
        })
        .unwrap()
        .unwrap();
    assert_eq!(
        selected,
        [
            (Some(2), "opening fragment".to_owned()),
            (Some(2), "closing fragment".to_owned()),
            (Some(4), "last millisecond".to_owned())
        ]
    );
}
