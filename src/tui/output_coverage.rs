//! Every display output over one session per agent, each read through the
//! session row's agent. The test fails on a record type an output drops. Add
//! a new record type to an agent's fixture and to its markers.

use super::export::{ExportFormat, ExportOptions, format_entry_for_clipboard, generate_content};
use super::viewer::{
    RenderOptions, ToolDisplayMode, parse_conversation_file, render_parsed_conversation,
};
use crate::display::{DisplayFormat, DisplayOptions, printout, read_session_to_print};
use crate::history::{Conversation, Source};
use crate::search::test_fixtures::one_message_conversation;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

type Markers = &'static [(&'static str, &'static str)];

/// The marker text each record type carries in the Claude fixture.
const CLAUDE_MARKERS: Markers = &[
    ("user text", "USER_TEXT_SENTINEL"),
    ("assistant text", "ASSISTANT_TEXT_SENTINEL"),
    ("thinking", "THINKING_SENTINEL"),
    ("tool call", "TOOL_CALL_SENTINEL"),
    ("tool result", "TOOL_RESULT_SENTINEL"),
    ("task report", "TASK_REPORT_SENTINEL"),
    ("sub-agent transcript message", "SPLICED_SUBAGENT_SENTINEL"),
    ("inline sub-agent message", "INLINE_SUBAGENT_SENTINEL"),
];

const CODEX_MARKERS: Markers = &[
    ("user text", "active codex question"),
    ("assistant text", "codex answer searchable"),
    ("thinking", "reasoning summary visible"),
    ("tool call", "TOOL_INPUT_VISIBLE"),
    ("tool result", "tool output searchable"),
    ("sub-agent message", "child answer searchable"),
];

const PI_MARKERS: Markers = &[
    ("user text", "active root question"),
    ("assistant text", "root answer"),
    ("thinking", "private reasoning"),
    ("tool call", "README.md"),
    ("tool result", "tool output searchable"),
];

const OMP_MARKERS: Markers = &[
    ("user text", "OMP active question"),
    ("assistant text", "OMP active answer"),
    ("thinking", "OMP private reasoning"),
    ("sub-agent message", "OMP sub-agent answer searchable"),
];

const KIMI_MARKERS: Markers = &[
    ("user text", "active kimi question"),
    ("assistant text", "kimi answer searchable"),
    ("thinking", "THINKING_VISIBLE"),
    ("tool call", "TOOL_INPUT_VISIBLE"),
    ("tool result", "kimi tool output searchable"),
    ("sub-agent message", "kimi child answer searchable"),
];

const OPENCODE_MARKERS: Markers = &[
    ("user text", "find the fixture defect"),
    ("assistant text", "the defect hides in lib.rs"),
    ("thinking", "considering the fixture layout"),
    ("tool call", "/tmp/opencode-project/lib.rs"),
    ("tool result", "fn defect() {}"),
];

const WITH_TOOLS_AND_THINKING: ExportOptions = ExportOptions {
    show_tools: true,
    show_thinking: true,
};

fn fixture(relative: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(relative)
}

/// One agent's fixture session, as the session list holds it, and the marker
/// text of each record type it carries.
pub(crate) struct AgentSession {
    pub(crate) row: Conversation,
    markers: Markers,
    /// Keeps a fixture built on disk for the test alive.
    _storage: Option<tempfile::TempDir>,
}

impl AgentSession {
    fn new(source: Source, path: PathBuf, subagents: Vec<PathBuf>, markers: Markers) -> Self {
        Self {
            row: listed_row(source, &path, &subagents),
            markers,
            _storage: None,
        }
    }
}

pub(crate) fn agent_sessions() -> Vec<AgentSession> {
    let claude = fixture("claude/-tmp-claude-every-record-fixture")
        .join("5c0f1a2b-3d4e-4f60-8172-93a4b5c6d7e8.jsonl");
    let claude_subagents = crate::history::provider::claude::subagent_transcripts(&claude, None);
    assert_eq!(
        claude_subagents.len(),
        1,
        "the Claude fixture holds one sub-agent"
    );

    vec![
        AgentSession::new(Source::Claude, claude, claude_subagents, CLAUDE_MARKERS),
        AgentSession::new(
            Source::Codex,
            fixture("codex/rollout.jsonl"),
            vec![fixture("codex/subagent.jsonl")],
            CODEX_MARKERS,
        ),
        AgentSession::new(
            Source::Pi,
            fixture("pi/v3-branched.jsonl"),
            Vec::new(),
            PI_MARKERS,
        ),
        AgentSession::new(
            Source::Omp,
            fixture("omp/v3.jsonl"),
            vec![fixture("omp/subagent.jsonl")],
            OMP_MARKERS,
        ),
        AgentSession::new(
            Source::Kimi,
            fixture("kimi/wire.jsonl"),
            vec![fixture("kimi/subagent-wire.jsonl")],
            KIMI_MARKERS,
        ),
        opencode_session(),
    ]
}

/// OpenCode keeps sessions in a database, so its fixture is built for the
/// test.
fn opencode_session() -> AgentSession {
    use crate::history::format::opencode::{fixture, session_ref};

    let storage = tempfile::tempdir().unwrap();
    let database = storage.path().join("opencode.db");
    let connection = fixture::create_database(&database);
    fixture::standard_session(&connection, "ses_standard");
    drop(connection);
    AgentSession {
        _storage: Some(storage),
        ..AgentSession::new(
            Source::OpenCode,
            session_ref(&database, "ses_standard"),
            Vec::new(),
            OPENCODE_MARKERS,
        )
    }
}

fn viewer_text(row: &Conversation) -> String {
    let conversation = parse_conversation_file(row.source, &row.path, &row.subagents).unwrap();
    let options = RenderOptions {
        tool_display: ToolDisplayMode::Full,
        show_thinking: true,
        show_timing: false,
        content_width: 200,
        expanded_tool_outputs: BTreeSet::new(),
        can_expand: false,
    };
    render_parsed_conversation(&conversation, &options)
        .lines
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|(text, _)| text.as_str())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The session list's row for a fixture, with its agent and sub-agent
/// transcripts.
fn listed_row(source: Source, path: &Path, subagents: &[PathBuf]) -> Conversation {
    let mut row = one_message_conversation("fixture", chrono::Local::now(), None, None, None);
    row.source = source;
    row.path = path.to_owned();
    row.subagents = subagents.to_vec();
    row
}

/// Each entry's clipboard copy, in order.
fn clipboard_text(row: &Conversation) -> String {
    crate::history::display_log_entries(row.source, &row.path, &row.subagents)
        .unwrap()
        .entries
        .iter()
        .map(|entry| format_entry_for_clipboard(entry, WITH_TOOLS_AND_THINKING))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Every output's text for `row`, with tools and thinking shown.
fn outputs(row: &Conversation) -> Vec<(&'static str, String)> {
    let printout_options = DisplayOptions {
        no_tools: false,
        show_thinking: true,
        ..DisplayOptions::default()
    };
    let printed = |format| {
        let session = read_session_to_print(std::slice::from_ref(row), &row.path).unwrap();
        printout(session, &printout_options, format)
    };
    let export = |format| {
        generate_content(
            row.source,
            &row.path,
            &row.subagents,
            format,
            WITH_TOOLS_AND_THINKING,
        )
        .unwrap()
    };
    vec![
        ("viewer", viewer_text(row)),
        (
            "terminal printout",
            printed(DisplayFormat::Ledger { content_width: 200 }),
        ),
        (
            "terminal printout's plain form",
            printed(DisplayFormat::Plain),
        ),
        ("Plain export", export(ExportFormat::Plain)),
        ("Markdown export", export(ExportFormat::Markdown)),
        ("Ledger export", export(ExportFormat::Ledger)),
        ("clipboard copy", clipboard_text(row)),
    ]
}

#[test]
fn every_display_output_shows_each_agents_fixture_record_types() {
    let missing: Vec<String> = agent_sessions()
        .iter()
        .flat_map(|session| {
            let agent = session.row.source;
            outputs(&session.row)
                .into_iter()
                .flat_map(move |(output, text)| {
                    session
                        .markers
                        .iter()
                        .filter(move |(_, marker)| !text.contains(marker))
                        .map(move |(record, _)| {
                            format!("{agent:?} {output} is missing the {record}")
                        })
                })
                .collect::<Vec<_>>()
        })
        .collect();
    assert!(missing.is_empty(), "{}", missing.join("\n"));
}
