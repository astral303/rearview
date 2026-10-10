//! The cached text `agent search` picks sessions by holds every segment an
//! opened session's search reads, so exact search never skips a session it
//! would match. Checked over each agent's fixture session, and over text and
//! thinking longer than a search segment.

use crate::agent::retrieval::segment_texts;
use crate::agent::transcript::{AgentTranscript, MAX_AGENT_SEGMENT_CHARS};
use crate::history::Source;
use crate::history::parser::normalize_whitespace;
use crate::history::provider::{Fingerprint, SessionRoot, SessionStub};
use crate::search::literal::build_agent_literal_corpus;
use crate::tui::output_coverage::agent_sessions;
use serde_json::json;
use std::path::{Path, PathBuf};

/// Each segment an opened session's search reads that the cached text of the
/// session's row lacks. The cached text collapses runs of whitespace, so each
/// segment is compared with its whitespace collapsed.
fn uncached_segments(source: Source, path: &Path, subagents: &[PathBuf]) -> Vec<String> {
    let stub = SessionStub {
        locator: path.to_owned(),
        subagents: subagents.to_vec(),
        cache_key: String::new(),
        fingerprint: Fingerprint {
            size: 0,
            modified: None,
        },
    };
    let root = SessionRoot::new(path.parent().unwrap());
    let row = source
        .provider()
        .storage()
        .parse_session(&stub, &root, None, &|| {})
        .unwrap()
        .unwrap();
    let cached = build_agent_literal_corpus(&[row]).remove(0).text;
    let transcript = AgentTranscript::load_owned(source, path, subagents).unwrap();
    segment_texts(&transcript)
        .into_iter()
        .map(|text| normalize_whitespace(&text))
        .filter(|text| !cached.contains(text.as_str()))
        .collect()
}

#[test]
fn every_agents_cached_text_holds_each_segment_an_opened_session_searches() {
    let gaps: Vec<String> = agent_sessions()
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
fn the_cached_text_holds_text_and_thinking_longer_than_a_search_segment() {
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
    let path = directory.path().join(format!("{session}.jsonl"));
    std::fs::write(
        &path,
        records
            .iter()
            .map(serde_json::Value::to_string)
            .collect::<Vec<_>>()
            .join("\n"),
    )
    .unwrap();

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
