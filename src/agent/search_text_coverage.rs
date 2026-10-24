//! Exact `agent search` picks sessions by their cached text. The cached text
//! holds every segment an opened session's search reads, so exact search
//! never skips a session it would match. Checked over each registered agent's
//! every-record session, and over text, thinking and tool results longer than
//! a search segment. The cached text also holds each searched and
//! agent-searched marker of the every-record sessions. It does not hold any
//! of their hidden markers.

use crate::agent::retrieval::segment_texts;
use crate::agent::transcript::{AgentTranscript, MAX_AGENT_SEGMENT_CHARS};
use crate::history::environment::test_support::test_cache_base;
use crate::history::parser::normalize_whitespace;
use crate::history::provider::contract_tests::{MarkerSearch, every_record_sessions, list_row};
use crate::history::{Conversation, Source};
use crate::search::literal::{Literal, build_agent_literal_corpus};
use crate::time_filter::TimeFilter;
use serde_json::json;
use std::path::{Path, PathBuf};

/// Each segment an opened session's search reads that the cached text of the
/// session's row lacks. The cached text collapses runs of whitespace, so each
/// segment is compared with its whitespace collapsed.
fn uncached_segments(source: Source, path: &Path, subagents: &[PathBuf]) -> Vec<String> {
    let (cached, segments) = cached_and_searched_text(source, path, subagents);
    segments
        .into_iter()
        .map(|text| normalize_whitespace(&text))
        .filter(|text| !cached.contains(text.as_str()))
        .collect()
}

/// The session's cached text, and the segments an opened session's search
/// reads.
fn cached_and_searched_text(
    source: Source,
    path: &Path,
    subagents: &[PathBuf],
) -> (String, Vec<String>) {
    let cached = cached_text(&list_row(source, path, subagents));
    let transcript =
        AgentTranscript::load_owned(&test_cache_base(), source, path, subagents).unwrap();
    (cached, segment_texts(&transcript))
}

fn cached_text(row: &Conversation) -> String {
    build_agent_literal_corpus(std::slice::from_ref(row), &TimeFilter::default())
        .remove(0)
        .text
}

fn write_session(directory: &Path, file_name: &str, records: &[serde_json::Value]) -> PathBuf {
    let path = directory.join(file_name);
    std::fs::write(
        &path,
        records
            .iter()
            .map(serde_json::Value::to_string)
            .collect::<Vec<_>>()
            .join("\n"),
    )
    .unwrap();
    path
}

#[test]
fn every_agents_cached_text_holds_each_segment_opened_session_searches() {
    let gaps: Vec<String> = every_record_sessions()
        .iter()
        .flat_map(|session| {
            let row = &session.row;
            uncached_segments(row.source, &row.path, &row.subagents)
                .into_iter()
                .map(move |text| format!("{:?}: {text}", row.source))
        })
        .collect();

    assert!(gaps.is_empty(), "{}", gaps.join("\n"));
}

#[test]
fn every_agents_cached_text_holds_each_searched_and_agent_searched_marker() {
    let gaps: Vec<String> = every_record_sessions()
        .iter()
        .flat_map(|session| {
            let cached = cached_text(&session.row);
            session
                .markers
                .iter()
                .filter(|marker| marker.search != MarkerSearch::Unchecked)
                .filter(|marker| !Literal::new(marker.text.to_owned()).matches(&cached))
                .map(|marker| format!("{:?}: {}", session.row.source, marker.record))
                .collect::<Vec<_>>()
        })
        .collect();

    assert!(
        gaps.is_empty(),
        "searched and agent-searched markers missing from the cached text:\n{}",
        gaps.join("\n")
    );
}

#[test]
fn every_agents_cached_text_doesnt_hold_hidden_marker() {
    let gaps: Vec<String> = every_record_sessions()
        .iter()
        .flat_map(|session| {
            let cached = cached_text(&session.row);
            session
                .hidden
                .iter()
                .filter(|(_, text)| Literal::new((*text).to_owned()).matches(&cached))
                .map(|(record, _)| format!("{:?}: {record}", session.row.source))
                .collect::<Vec<_>>()
        })
        .collect();

    assert!(
        gaps.is_empty(),
        "hidden markers in the cached text:\n{}",
        gaps.join("\n")
    );
}

#[test]
fn cached_text_holds_text_and_thinking_longer_than_search_segment() {
    // Each part pads with its own letter, so one part's segment cannot match
    // inside another part's cached text.
    let long = |padding: &str, marker: &str| {
        format!("{} {marker}", padding.repeat(MAX_AGENT_SEGMENT_CHARS))
    };
    let session = "9d1e2f3a-4b5c-4d6e-8f70-81a2b3c4d5e6";
    let assistant = |id: &str, block: serde_json::Value| {
        json!({
            "type": "assistant",
            "timestamp": "2026-09-01T10:00:01.000Z",
            "sessionId": session,
            "message": {"id": id, "role": "assistant", "content": [block]}
        })
    };
    let records = [
        json!({
            "type": "user",
            "timestamp": "2026-09-01T10:00:00.000Z",
            "sessionId": session,
            "cwd": "/tmp/long-parts",
            "message": {"role": "user", "content": "Summarize the logs"}
        }),
        assistant(
            "msg_text",
            json!({"type": "text", "text": long("a", "TEXT_TAIL_SENTINEL")}),
        ),
        assistant(
            "msg_thinking",
            json!({"type": "thinking", "thinking": long("b", "THINKING_TAIL_SENTINEL"), "signature": "sig"}),
        ),
        json!({
            "type": "progress",
            "timestamp": "2026-09-01T10:00:02.000Z",
            "sessionId": session,
            "parentToolUseID": "toolu_task",
            "data": {
                "type": "agent_progress",
                "agentId": "a7777777",
                "prompt": "Read the logs",
                "message": {"type": "assistant", "message": {"role": "assistant", "content": [
                    {"type": "text", "text": long("c", "PROGRESS_TAIL_SENTINEL")}
                ]}}
            }
        }),
    ];
    let directory = tempfile::tempdir().unwrap();
    let path = write_session(directory.path(), &format!("{session}.jsonl"), &records);

    let gaps = uncached_segments(Source::Claude, &path, &[]);

    assert!(
        gaps.is_empty(),
        "segments missing from the cached text, by their last 40 characters: {:?}",
        gaps.iter()
            .map(|text| text
                .chars()
                .skip(text.chars().count().saturating_sub(40))
                .collect::<String>())
            .collect::<Vec<_>>()
    );
}

/// A tool result longer than a search segment, with a phrase in its middle
/// split by a tab and a line break.
fn long_tool_output() -> String {
    format!(
        "{} retry\tbackoff\n  window {}",
        "d ".repeat(MAX_AGENT_SEGMENT_CHARS),
        "e ".repeat(MAX_AGENT_SEGMENT_CHARS)
    )
}

fn claude_session_with_a_long_tool_result(directory: &Path) -> PathBuf {
    let session = "4e5f6a7b-8c9d-4e0f-9a1b-2c3d4e5f6a7b";
    let record = |kind: &str, second: u32, message: serde_json::Value| {
        json!({
            "type": kind,
            "timestamp": format!("2026-09-01T10:00:0{second}.000Z"),
            "sessionId": session,
            "cwd": "/tmp/long-tool-result",
            "message": message,
        })
    };
    write_session(
        directory,
        &format!("{session}.jsonl"),
        &[
            record(
                "user",
                0,
                json!({"role": "user", "content": "Tail the log"}),
            ),
            record(
                "assistant",
                1,
                json!({"id": "msg_call", "role": "assistant", "content": [
                    {"type": "tool_use", "id": "toolu_log", "name": "Bash", "input": {"command": "cat app.log"}}
                ]}),
            ),
            record(
                "user",
                2,
                json!({"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_log", "content": long_tool_output()}
                ]}),
            ),
        ],
    )
}

fn codex_session_with_a_long_tool_result(directory: &Path) -> PathBuf {
    let item = |second: u32, payload: serde_json::Value| {
        json!({
            "timestamp": format!("2026-09-01T10:00:0{second}.000Z"),
            "type": "response_item",
            "payload": payload,
        })
    };
    let id = "019f0000-0000-7000-8000-0000000000b1";
    write_session(
        directory,
        &format!("rollout-2026-09-01T10-00-00-{id}.jsonl"),
        &[
            json!({
                "timestamp": "2026-09-01T10:00:00.000Z",
                "type": "session_meta",
                "payload": {"session_id": id, "id": id, "timestamp": "2026-09-01T10:00:00.000Z", "cwd": "/tmp/long-tool-result", "originator": "codex-tui", "cli_version": "0.144.1", "source": "cli"}
            }),
            item(
                1,
                json!({"type": "message", "id": "msg_u", "role": "user", "content": [{"type": "input_text", "text": "Tail the log"}]}),
            ),
            item(
                2,
                json!({"type": "function_call", "id": "fc_log", "name": "exec_command", "arguments": "{\"cmd\":\"cat app.log\"}", "call_id": "call_log"}),
            ),
            item(
                3,
                json!({"type": "function_call_output", "id": "fco_log", "call_id": "call_log", "output": long_tool_output()}),
            ),
        ],
    )
}

#[test]
fn cached_text_holds_whole_long_tool_result_and_matches_it_as_search_does() {
    let directory = tempfile::tempdir().unwrap();
    let sessions = [
        (
            Source::Claude,
            claude_session_with_a_long_tool_result(directory.path()),
        ),
        (
            Source::Codex,
            codex_session_with_a_long_tool_result(directory.path()),
        ),
    ];
    let phrase = Literal::new("retry backoff window".to_owned());

    for (source, path) in sessions {
        assert_eq!(uncached_segments(source, &path, &[]), Vec::<String>::new());
        let (cached, segments) = cached_and_searched_text(source, &path, &[]);
        assert!(phrase.matches(&cached), "{source:?}: the cached text");
        assert!(
            segments.iter().any(|segment| phrase.matches(segment)),
            "{source:?}: an opened session's search"
        );
    }
}
