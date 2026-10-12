//! Every display output over each registered agent's every-record session,
//! read through the session row's agent. The test fails on a record type an
//! output drops. Each agent's session and markers are in its provider's
//! `ProviderFixture`.

use super::export::{ExportFormat, ExportOptions, format_entry_for_clipboard, generate_content};
use super::viewer::{
    RenderOptions, ToolDisplayMode, parse_conversation_file, render_parsed_conversation,
};
use crate::display::{DisplayFormat, DisplayOptions, printout, read_session_to_print};
use crate::history::Conversation;
use crate::history::provider::contract_tests::every_record_sessions;
use std::collections::BTreeSet;

const WITH_TOOLS_AND_THINKING: ExportOptions = ExportOptions {
    show_tools: true,
    show_thinking: true,
};

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
    let missing: Vec<String> = every_record_sessions()
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
