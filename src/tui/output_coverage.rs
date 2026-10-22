//! Every display output over each registered agent's every-record session,
//! read through the session row's agent. The tests fail on a record kind an
//! output drops, on a hidden record kind an output shows, on a call whose run
//! shows another summary-mode row than its fixture names, and on a call an
//! output shows as raw JSON. Each agent's session and markers are in its
//! provider's `ProviderFixture`.

use super::export::{ExportFormat, ExportOptions, export_content, format_entry_for_clipboard};
use super::viewer::{
    RenderOptions, ToolDisplayMode, parse_conversation_file_interactively,
    render_parsed_conversation,
};
use crate::clock::{Clock, TestClock};
use crate::display::{DisplayFormat, DisplayOptions, printout, read_session_to_print};
use crate::history::provider::contract_tests::{AgentSession, every_record_sessions};
use crate::history::{Conversation, DisplayEntries, InteractiveCache};
use std::collections::BTreeSet;

const WITH_TOOLS_AND_THINKING: ExportOptions = ExportOptions {
    show_tools: true,
    show_thinking: true,
};

/// `row`'s entries as the TUI's exports and clipboard copy read them, without
/// a session cache.
fn entries_of(row: &Conversation) -> DisplayEntries {
    crate::history::display_log_entries_interactively(
        &InteractiveCache::default(),
        row.source,
        &row.path,
        &row.subagents,
    )
    .unwrap()
    .value
    .expect("a read without a session cache parses the transcripts")
}

fn viewer_text(row: &Conversation, tool_display: ToolDisplayMode) -> String {
    let conversation = parse_conversation_file_interactively(
        &InteractiveCache::default(),
        row.source,
        &row.path,
        &row.subagents,
    )
    .unwrap()
    .value
    .expect("a read without a session cache parses the transcripts");
    let options = RenderOptions {
        tool_display,
        show_thinking: true,
        show_timing: false,
        content_width: 200,
        expanded_tool_outputs: BTreeSet::new(),
        can_expand: false,
        today: TestClock::default().today(),
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
    entries_of(row)
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
        let session = read_session_to_print(None, std::slice::from_ref(row), &row.path).unwrap();
        printout(session, &printout_options, format)
    };
    let export = |format| {
        export_content(&row.path, format, WITH_TOOLS_AND_THINKING, || {
            Ok(entries_of(row))
        })
        .unwrap()
    };
    vec![
        ("viewer", viewer_text(row, ToolDisplayMode::Full)),
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

/// Each output's dropped record kinds and shown hidden record kinds.
fn display_gaps(session: &AgentSession) -> Vec<String> {
    let agent = session.row.source;
    let mut gaps = Vec::new();
    for (output, text) in outputs(&session.row) {
        for marker in &session.markers {
            if !text.contains(marker.text) {
                gaps.push(format!(
                    "{agent:?} {output} is missing the {}",
                    marker.record
                ));
            }
        }
        for (record, marker) in session.hidden {
            if text.contains(marker) {
                gaps.push(format!("{agent:?} {output} shows the hidden {record}"));
            }
        }
    }
    gaps
}

#[test]
fn every_display_output_shows_each_agents_record_kinds_and_no_hidden_one() {
    let gaps: Vec<String> = every_record_sessions()
        .iter()
        .flat_map(display_gaps)
        .collect();

    assert!(gaps.is_empty(), "{}", gaps.join("\n"));
}

/// Each call marker whose run shows another summary-mode row than the
/// fixture names, and each output line that shows a call marker beside a JSON
/// key.
fn call_gaps(session: &AgentSession) -> Vec<String> {
    let agent = session.row.source;
    let calls: Vec<_> = session
        .markers
        .iter()
        .filter_map(|marker| Some((marker, marker.summary_row?)))
        .collect();
    let summary = viewer_text(&session.row, ToolDisplayMode::Hidden);
    let mut gaps: Vec<String> = calls
        .iter()
        .filter(|(_, row)| {
            !summary
                .lines()
                .any(|line| line.trim_end().ends_with(&format!("│ {row}")))
        })
        .map(|(marker, row)| {
            format!(
                "{agent:?} summary mode has no `{row}` row for the {}",
                marker.record
            )
        })
        .collect();
    for (output, text) in outputs(&session.row) {
        for (marker, _) in &calls {
            if text
                .lines()
                .any(|line| line.contains(marker.text) && line.contains("\":"))
            {
                gaps.push(format!(
                    "{agent:?} {output} shows the {} as raw JSON",
                    marker.record
                ));
            }
        }
    }
    if !gaps.is_empty() {
        gaps.push(format!("{agent:?} summary mode:\n{summary}"));
    }
    gaps
}

#[test]
fn each_agents_calls_show_their_summary_row_and_no_output_shows_them_as_raw_json() {
    let gaps: Vec<String> = every_record_sessions().iter().flat_map(call_gaps).collect();

    assert!(gaps.is_empty(), "{}", gaps.join("\n"));
}
