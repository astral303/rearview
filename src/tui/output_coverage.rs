//! Every display output over one Claude session holding eight record types.
//! The test fails on a record type an output drops. Add a new record type to
//! the fixture and to `RECORD_MARKERS`.

use super::export::{ExportFormat, ExportOptions, extract_message_text, generate_content};
use super::viewer::{
    RenderOptions, ToolDisplayMode, parse_conversation_file, render_parsed_conversation,
};
use crate::display::{DisplayFormat, DisplayOptions, printout};
use crate::history::Source;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// The marker text each record type carries in the fixture.
const RECORD_MARKERS: [(&str, &str); 8] = [
    ("user text", "USER_TEXT_SENTINEL"),
    ("assistant text", "ASSISTANT_TEXT_SENTINEL"),
    ("thinking", "THINKING_SENTINEL"),
    ("tool call", "TOOL_CALL_SENTINEL"),
    ("tool result", "TOOL_RESULT_SENTINEL"),
    ("task report", "TASK_REPORT_SENTINEL"),
    ("sub-agent transcript message", "SPLICED_SUBAGENT_SENTINEL"),
    ("inline sub-agent message", "INLINE_SUBAGENT_SENTINEL"),
];

/// Sub-agent transcripts do not reach the terminal printout: it reads the
/// session file alone.
const PRINTOUT_GAPS: [&str; 1] = ["SPLICED_SUBAGENT_SENTINEL"];

const WITH_TOOLS_AND_THINKING: ExportOptions = ExportOptions {
    show_tools: true,
    show_thinking: true,
};

fn session() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/claude/-tmp-claude-every-record-fixture")
        .join("5c0f1a2b-3d4e-4f60-8172-93a4b5c6d7e8.jsonl")
}

fn viewer_text(path: &Path, subagents: &[PathBuf]) -> String {
    let conversation = parse_conversation_file(Source::Claude, path, subagents).unwrap();
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

/// Each entry's clipboard copy, in order.
fn clipboard_text(path: &Path, subagents: &[PathBuf]) -> String {
    (0..)
        .map_while(|entry_index| {
            extract_message_text(
                Source::Claude,
                path,
                subagents,
                entry_index,
                WITH_TOOLS_AND_THINKING,
            )
            .ok()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn every_display_output_shows_each_fixture_record_type() {
    let path = session();
    let subagents = crate::history::provider::claude::subagent_transcripts(&path, None);
    assert_eq!(subagents.len(), 1, "the fixture holds one sub-agent");
    let printout_options = DisplayOptions {
        no_tools: false,
        show_thinking: true,
        ..DisplayOptions::default()
    };
    let printed = |format| printout(&path, &printout_options, format).unwrap();
    let export = |format| {
        generate_content(
            Source::Claude,
            &path,
            &subagents,
            format,
            WITH_TOOLS_AND_THINKING,
        )
        .unwrap()
    };
    let outputs = [
        ("viewer", viewer_text(&path, &subagents), &[][..]),
        (
            "terminal printout",
            printed(DisplayFormat::Ledger { content_width: 200 }),
            &PRINTOUT_GAPS[..],
        ),
        (
            "terminal printout's plain form",
            printed(DisplayFormat::Plain),
            &PRINTOUT_GAPS[..],
        ),
        ("Plain export", export(ExportFormat::Plain), &[][..]),
        ("Markdown export", export(ExportFormat::Markdown), &[][..]),
        ("Ledger export", export(ExportFormat::Ledger), &[][..]),
        ("clipboard copy", clipboard_text(&path, &subagents), &[][..]),
    ];

    let missing: Vec<String> = outputs
        .iter()
        .flat_map(|(output, text, gaps)| {
            RECORD_MARKERS
                .iter()
                .filter(|(_, marker)| !gaps.contains(marker) && !text.contains(marker))
                .map(move |(record, _)| format!("{output} is missing the {record}"))
        })
        .collect();
    assert!(missing.is_empty(), "{}", missing.join("\n"));
}
