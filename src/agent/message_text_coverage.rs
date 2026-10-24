//! Each message's slice of the cached search texts holds the markers
//! `agent read` shows in that message, and no other message's markers.
//! Checked over each registered agent's every-record session, read with its
//! sub-agent transcripts, and over a Codex sub-agent's handed-back report.

use crate::agent::refs::MessageRange;
use crate::agent::transcript::{AgentTranscript, agent_part_text};
use crate::history::environment::test_support::test_cache_base;
use crate::history::provider::contract_tests::{MarkerSearch, every_record_sessions, list_row};
use crate::history::{Conversation, Source};
use crate::search::literal::Literal;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

fn messages_holding_in_cache(row: &Conversation, literal: &Literal) -> BTreeSet<usize> {
    (1..=row.message_count)
        .filter(|ordinal| {
            row.texts_of(&[MessageRange::single(*ordinal)])
                .into_array()
                .iter()
                .any(|text| literal.matches(text))
        })
        .collect()
}

fn messages_holding_in_agent_read(
    transcript: &AgentTranscript,
    literal: &Literal,
) -> BTreeSet<usize> {
    transcript
        .messages
        .iter()
        .filter(|message| {
            message
                .parts
                .iter()
                .filter_map(agent_part_text)
                .any(|text| literal.matches(&text))
        })
        .map(|message| message.ordinal)
        .collect()
}

#[test]
fn each_markers_cached_text_is_in_message_agent_read_shows_it_in() {
    let misplaced: Vec<String> = every_record_sessions()
        .iter()
        .flat_map(|session| {
            let row = &session.row;
            let transcript = AgentTranscript::load_owned(
                &test_cache_base(),
                row.source,
                &row.path,
                &row.subagents,
            )
            .unwrap();
            session
                .markers
                .iter()
                .filter(|marker| marker.search != MarkerSearch::Unchecked)
                .filter_map(|marker| {
                    let literal = Literal::new(marker.text.to_owned());
                    let cached = messages_holding_in_cache(row, &literal);
                    let shown = messages_holding_in_agent_read(&transcript, &literal);
                    (cached != shown).then(|| {
                        format!(
                            "{:?} {}: cached in {cached:?}, agent read shows it in {shown:?}",
                            row.source, marker.record
                        )
                    })
                })
                .collect::<Vec<_>>()
        })
        .collect();

    assert!(misplaced.is_empty(), "{}", misplaced.join("\n"));
}

const PARENT_THREAD: &str = "019f0000-0000-7000-8000-0000000000c1";
const SCOUT_THREAD: &str = "019f0000-0000-7000-8000-0000000000c2";
const SCOUT_REPORT: &str = "SCOUT_REPORT_SENTINEL: the notes list the steps.";

/// A Codex rollout of `lines` at `path`, each `(second, type, payload)`.
fn write_rollout(path: &Path, lines: &[(u32, &str, Value)]) {
    let records = lines
        .iter()
        .map(|(second, kind, payload)| {
            json!({
                "timestamp": format!("2026-08-01T10:00:{second:02}.000Z"),
                "type": kind,
                "payload": payload,
            })
            .to_string()
        })
        .collect::<Vec<_>>();
    std::fs::write(path, records.join("\n")).unwrap();
}

/// The session spawns `/root/scout` and receives its final answer; the
/// scout's rollout holds the reply it handed back.
fn codex_session_with_a_handed_back_report(directory: &Path) -> (PathBuf, PathBuf) {
    let parent = directory.join(format!("rollout-2026-08-01T10-00-00-{PARENT_THREAD}.jsonl"));
    let scout = directory.join(format!("rollout-2026-08-01T10-00-04-{SCOUT_THREAD}.jsonl"));
    write_rollout(
        &parent,
        &[
            (
                0,
                "session_meta",
                json!({"id": PARENT_THREAD, "timestamp": "2026-08-01T10:00:00.000Z", "cwd": "/tmp/project", "originator": "codex-tui", "cli_version": "0.162.1", "source": "cli"}),
            ),
            (
                1,
                "response_item",
                json!({"type": "message", "id": "msg_u", "role": "user", "content": [{"type": "input_text", "text": "List the release steps"}]}),
            ),
            (
                2,
                "response_item",
                json!({"type": "function_call", "id": "fc_spawn", "name": "spawn_agent", "arguments": "{\"task_name\":\"scout\",\"message\":\"List the release steps.\"}", "call_id": "call_spawn"}),
            ),
            (
                3,
                "response_item",
                json!({"type": "function_call_output", "id": "fco_spawn", "call_id": "call_spawn", "output": "{\"task_name\":\"/root/scout\"}"}),
            ),
            (
                20,
                "response_item",
                json!({"type": "agent_message", "id": "amsg_report", "author": "/root/scout", "recipient": "/root", "content": [{"type": "input_text", "text": format!("Message Type: FINAL_ANSWER\nTask name: /root\nSender: /root/scout\nPayload:\n{SCOUT_REPORT}")}]}),
            ),
        ],
    );
    write_rollout(
        &scout,
        &[
            (
                4,
                "session_meta",
                json!({"id": SCOUT_THREAD, "parent_thread_id": PARENT_THREAD, "timestamp": "2026-08-01T10:00:04.000Z", "cwd": "/tmp/project", "source": {"subagent": {"thread_spawn": {"parent_thread_id": PARENT_THREAD, "depth": 1, "agent_path": "/root/scout"}}}, "agent_path": "/root/scout"}),
            ),
            (
                10,
                "response_item",
                json!({"type": "message", "id": "msg_reply", "role": "assistant", "content": [{"type": "output_text", "text": SCOUT_REPORT}], "phase": "final_answer"}),
            ),
        ],
    );
    (parent, scout)
}

/// `agent read` shows the report in the session's message alone: the reader
/// folds the scout's copy into a delivered `final_answer` call.
#[test]
fn codex_sub_agents_handed_back_report_is_cached_only_where_agent_read_shows_it() {
    let directory = tempfile::tempdir().unwrap();
    let (parent, scout) = codex_session_with_a_handed_back_report(directory.path());
    let subagents = [scout];
    let row = list_row(Source::Codex, &parent, &subagents);
    let transcript =
        AgentTranscript::load_owned(&test_cache_base(), Source::Codex, &parent, &subagents)
            .unwrap();
    let report = Literal::new(SCOUT_REPORT.to_owned());

    let shown = messages_holding_in_agent_read(&transcript, &report);

    assert_eq!(shown.len(), 1, "{transcript:#?}");
    assert_eq!(messages_holding_in_cache(&row, &report), shown);
}
