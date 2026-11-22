use super::{cached_row, every_record_rows, every_record_sessions};
use crate::agent::subagents::SubagentHandles;
use crate::agent::transcript::AgentTranscript;
use crate::history::Conversation;
use crate::history::Source;
use crate::history::provider::contract_tests::AgentSession;
use std::path::Path;

/// The counts of a transcript's sub-agent handles.
struct HandleCounts {
    every_handle: usize,
    /// The handles of launches whose sub-agents haven't written a message.
    without_messages: usize,
}

fn handles_of(session: &AgentSession) -> HandleCounts {
    let fixture = &session.row;
    let transcript =
        AgentTranscript::load_cached_for_test(fixture.source, &fixture.path, &fixture.subagents)
            .unwrap();
    let handles = SubagentHandles::of(&transcript);
    HandleCounts {
        every_handle: handles.count(),
        without_messages: handles.count() - handles.threads().count(),
    }
}

fn cached_subagent_count(row: &Conversation) -> usize {
    row.cached_session
        .as_ref()
        .unwrap()
        .read_copy(|held| held.subagent_count())
        .unwrap()
}

#[test]
fn every_agents_cached_subagent_count_is_its_transcripts_handle_count() {
    let base = tempfile::tempdir().unwrap();
    let mut sessions_with_subagents = 0;
    for (session, row) in every_record_sessions()
        .iter()
        .zip(every_record_rows(base.path()))
    {
        let handles = handles_of(session).every_handle;

        assert_eq!(
            cached_subagent_count(&row),
            handles,
            "{:?}: the cache counts as many sub-agents as the transcript has handles",
            row.source
        );
        sessions_with_subagents += usize::from(handles > 0);
    }
    assert!(
        sessions_with_subagents > 0,
        "an agent's every-record session has a sub-agent"
    );
}

/// The Claude every-record session, copied under `directory`, with one more
/// `Agent` call at its end: its sub-agent hasn't written a message.
fn claude_session_ending_in_launch_without_messages(directory: &Path) -> AgentSession {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/claude/-tmp-claude-every-record-fixture");
    let project = directory.join("-tmp-claude-every-record-fixture");
    let session = "5c0f1a2b-3d4e-4f60-8172-93a4b5c6d7e8";
    let subagents = project.join(session).join("subagents");
    std::fs::create_dir_all(&subagents).unwrap();
    let mut transcripts = Vec::new();
    for entry in std::fs::read_dir(fixture.join(session).join("subagents")).unwrap() {
        let source = entry.unwrap().path();
        let copy = subagents.join(source.file_name().unwrap());
        std::fs::copy(&source, &copy).unwrap();
        if copy
            .extension()
            .is_some_and(|extension| extension == "jsonl")
        {
            transcripts.push(copy);
        }
    }
    transcripts.sort();
    let transcript = project.join(format!("{session}.jsonl"));
    let text = std::fs::read_to_string(fixture.join(format!("{session}.jsonl"))).unwrap();
    let last: serde_json::Value = serde_json::from_str(text.lines().last().unwrap()).unwrap();
    let launch = serde_json::json!({
        "parentUuid": last["uuid"], "isSidechain": false, "type": "assistant",
        "uuid": "55555555-0000-4000-8000-0000000000ff", "sessionId": session,
        "timestamp": "2026-09-01T10:40:00.000Z", "cwd": "/tmp/claude-every-record-fixture",
        "message": {
            "id": "msg_fixture_unwritten_launch", "type": "message", "role": "assistant",
            "content": [{
                "type": "tool_use", "id": "toolu_01UNWRITTENLAUNCHAAAAAAAA", "name": "Agent",
                "input": {"subagent_type": "Explore", "description": "Not started", "prompt": "wait"}
            }]
        }
    });
    std::fs::write(&transcript, format!("{}\n{launch}\n", text.trim_end())).unwrap();
    AgentSession::new(Source::Claude, transcript, transcripts, Vec::new())
}

#[test]
fn cached_subagent_count_counts_launch_whose_subagent_hasnt_written() {
    let directory = tempfile::tempdir().unwrap();
    let session = claude_session_ending_in_launch_without_messages(directory.path());
    let handles = handles_of(&session);

    let row = cached_row(&directory.path().join("cache"), &session);

    assert_eq!(
        handles.without_messages, 1,
        "the last launch hasn't written"
    );
    assert_eq!(
        cached_subagent_count(&row),
        handles.every_handle,
        "the cache counts the launch whose sub-agent hasn't written"
    );
}
