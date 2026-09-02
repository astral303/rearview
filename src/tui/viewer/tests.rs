use super::connectors::lane_color;
use super::markdown::render_markdown_to_lines;
use super::tools::{ToolCallRenderSpec, ToolOutputKind, make_tool_output_id, render_tool_call};
use super::*;
use crate::log_entry::Tool;

/// Helper to render markdown and extract just the content text (without styling)
fn render_to_text(input: &str, width: usize) -> String {
    let lines = render_markdown_to_lines(input, width);
    lines
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

#[test]
fn test_plain_text() {
    let result = render_to_text("Hello world", 80);
    assert_eq!(result.trim(), "Hello world");
}

#[test]
fn test_heading() {
    let result = render_to_text("# Heading 1", 80);
    assert!(result.contains("Heading 1"));
}

#[test]
fn test_heading_with_paragraph() {
    let result = render_to_text("# Heading\n\nSome text", 80);
    let lines: Vec<&str> = result.lines().collect();
    // Should have: heading, blank, text
    assert_eq!(lines.len(), 3, "Expected 3 lines, got:\n{}", result);
    assert!(lines[0].contains("Heading"));
    assert_eq!(lines[1], "");
    assert_eq!(lines[2], "Some text");
}

#[test]
fn test_paragraph_with_list() {
    let result = render_to_text("Some intro:\n\n- Item 1\n- Item 2", 80);
    let lines: Vec<&str> = result.lines().collect();
    // Should have: para, blank, item1, item2
    assert_eq!(lines.len(), 4, "Expected 4 lines, got:\n{}", result);
    assert_eq!(lines[0], "Some intro:");
    assert_eq!(lines[1], "");
    assert!(lines[2].contains("- Item 1"));
    assert!(lines[3].contains("- Item 2"));
}

#[test]
fn test_numbered_list_with_bold() {
    // This is the bug case: numbered list item starting with bold text
    let result = render_to_text("1. **Task 10:** description\n2. **Task 11:** more", 80);
    let lines: Vec<&str> = result.lines().collect();
    // Should have: item1, item2 (NO blank lines between number and content)
    assert_eq!(lines.len(), 2, "Expected 2 lines, got:\n{}", result);
    assert!(
        lines[0].starts_with("1. "),
        "Line should start with '1. ': {:?}",
        lines[0]
    );
    assert!(
        lines[0].contains("Task 10"),
        "Line should contain 'Task 10': {:?}",
        lines[0]
    );
    assert!(
        lines[1].starts_with("2. "),
        "Line should start with '2. ': {:?}",
        lines[1]
    );
    assert!(
        lines[1].contains("Task 11"),
        "Line should contain 'Task 11': {:?}",
        lines[1]
    );
}

#[test]
fn test_numbered_list_no_extra_blank_lines() {
    let input = "## Changes\n\n1. **First change:**\n   - details\n2. **Second change:**\n   - more details";
    let result = render_to_text(input, 80);
    let lines: Vec<&str> = result.lines().collect();

    // Verify no blank lines between "1." and "First change"
    let line1_idx = lines
        .iter()
        .position(|l| l.starts_with("1. "))
        .expect("Should find '1. '");
    assert!(
        lines[line1_idx].contains("First change"),
        "First item should be on same line as '1. '"
    );

    // Verify no blank lines between "2." and "Second change"
    let line2_idx = lines
        .iter()
        .position(|l| l.starts_with("2. "))
        .expect("Should find '2. '");
    assert!(
        lines[line2_idx].contains("Second change"),
        "Second item should be on same line as '2. '"
    );
}

#[test]
fn test_consecutive_list_items_no_blanks() {
    let result = render_to_text("- First\n- Second\n- Third", 80);
    let lines: Vec<&str> = result.lines().collect();
    // Should be exactly 3 lines, no blanks between items
    assert_eq!(
        lines.len(),
        3,
        "Expected 3 lines with no blanks, got:\n{}",
        result
    );
    assert!(lines[0].contains("- First"));
    assert!(lines[1].contains("- Second"));
    assert!(lines[2].contains("- Third"));
}

#[test]
fn test_nested_list() {
    let result = render_to_text("- Item 1\n  - Nested 1\n  - Nested 2\n- Item 2", 80);
    let lines: Vec<&str> = result.lines().collect();
    // Should have: item1, nested1, nested2, item2 (no extra blanks)
    assert_eq!(lines.len(), 4, "Expected 4 lines, got:\n{}", result);
    assert!(lines[0].contains("- Item 1"));
    assert!(lines[1].contains("- Nested 1"));
    assert!(lines[2].contains("- Nested 2"));
    assert!(lines[3].contains("- Item 2"));
}

#[test]
fn test_code_block() {
    let result = render_to_text("Text\n\n```rust\nlet x = 1;\n```\n\nMore text", 80);
    let lines: Vec<&str> = result.lines().collect();
    // TUI strips fence markers (signaled via color instead).
    assert!(!result.contains("```"));
    assert!(result.contains("let x = 1;"));

    // Check for proper spacing
    let text_idx = lines.iter().position(|l| l == &"Text").unwrap();
    let more_idx = lines.iter().position(|l| l == &"More text").unwrap();
    assert_eq!(lines[text_idx + 1], "", "Should have blank line after Text");
    assert_eq!(
        lines[more_idx - 1],
        "",
        "Should have blank line before More text"
    );
}

#[test]
fn test_block_quote() {
    let result = render_to_text("Text\n\n> Quote here", 80);
    let lines: Vec<&str> = result.lines().collect();
    // Block quote renders with quote prefix on one line, blank, then content
    // This is due to how the markdown parser handles block quotes
    assert_eq!(lines[0], "Text");
    assert_eq!(lines[1], ""); // blank before quote
    assert!(lines[2].starts_with("> "), "Should have quote prefix");
    // Content may be on same line or next line depending on parser
    let has_content =
        lines[2].contains("Quote here") || (lines.len() > 4 && lines[4].contains("Quote here"));
    assert!(has_content, "Should contain quote content");
}

#[test]
fn test_horizontal_rule() {
    let result = render_to_text("Before\n\n---\n\nAfter", 80);
    let lines: Vec<&str> = result.lines().collect();
    // Should have proper spacing around rule
    let before_idx = lines.iter().position(|l| l == &"Before").unwrap();
    let after_idx = lines.iter().position(|l| l == &"After").unwrap();
    // Rule should be on its own with blanks around it
    assert_eq!(
        lines[before_idx + 1],
        "",
        "Should have blank line after Before"
    );
    assert!(lines[before_idx + 2].contains("─"), "Should have rule");
    assert_eq!(
        lines[after_idx - 1],
        "",
        "Should have blank line before After"
    );
}

#[test]
fn test_multiple_paragraphs() {
    let result = render_to_text(
        "First paragraph.\n\nSecond paragraph.\n\nThird paragraph.",
        80,
    );
    let lines: Vec<&str> = result.lines().collect();
    // Should have: p1, blank, p2, blank, p3
    assert_eq!(lines.len(), 5, "Expected 5 lines, got:\n{}", result);
    assert_eq!(lines[0], "First paragraph.");
    assert_eq!(lines[1], "");
    assert_eq!(lines[2], "Second paragraph.");
    assert_eq!(lines[3], "");
    assert_eq!(lines[4], "Third paragraph.");
}

#[test]
fn test_list_with_multiline_items() {
    let input = "1. First item\n   with continuation\n2. Second item\n   also continued";
    let result = render_to_text(input, 80);
    let lines: Vec<&str> = result.lines().collect();

    // First item should start with "1. "
    assert!(lines[0].starts_with("1. "), "First line: {:?}", lines[0]);
    // Soft breaks join continuation to the same paragraph, so all first-item
    // text may appear on a single line at wide widths
    let first_item_text = lines.join(" ");
    assert!(
        first_item_text.contains("First item"),
        "Should contain first item text"
    );
    assert!(
        first_item_text.contains("with continuation"),
        "Should contain continuation"
    );

    // Second item should start with "2. "
    let line2_idx = lines
        .iter()
        .position(|l| l.starts_with("2. "))
        .expect("Should find '2. '");
    assert!(line2_idx >= 1, "Second item should appear after first");
}

#[test]
fn test_no_trailing_blank_lines() {
    let result = render_to_text("Text\n\n## Heading\n\nParagraph", 80);
    // Should not end with blank lines
    assert!(
        !result.ends_with("\n\n"),
        "Should not have trailing blank lines: {:?}",
        result
    );
}

#[test]
fn test_inline_code() {
    let result = render_to_text("Use `code` here", 80);
    assert!(result.contains("code"));
}

#[test]
fn test_bold_and_italic() {
    let result = render_to_text("**bold** and *italic* text", 80);
    // Just verify it renders without panicking and contains the text
    assert!(result.contains("bold"));
    assert!(result.contains("italic"));
}

#[test]
fn test_table_basic() {
    let input = "| A | B |\n|---|---|\n| 1 | 2 |";
    let result = render_to_text(input, 80);
    eprintln!("Table output:\n{}", result);
    assert!(result.contains('┌'), "Expected top-left corner");
    assert!(result.contains('│'), "Expected vertical border");
    assert!(result.contains('└'), "Expected bottom-left corner");
    assert!(result.contains(" A "), "Expected cell A");
    assert!(result.contains(" B "), "Expected cell B");
    assert!(result.contains(" 1 "), "Expected cell 1");
    assert!(result.contains(" 2 "), "Expected cell 2");
}

#[test]
fn test_table_column_widths() {
    let input = "| Short | Longer text |\n|---|---|\n| A | B |";
    let result = render_to_text(input, 80);
    eprintln!("Table output:\n{}", result);
    assert!(result.contains("Short"), "Expected Short");
    assert!(result.contains("Longer text"), "Expected Longer text");
    // Columns should be sized to fit longest content
    let lines: Vec<&str> = result.lines().collect();
    // All border lines should be same width
    let border_widths: Vec<usize> = lines
        .iter()
        .filter(|l| l.starts_with('┌') || l.starts_with('├') || l.starts_with('└'))
        .map(|l| l.chars().count())
        .collect();
    assert!(
        border_widths.windows(2).all(|w| w[0] == w[1]),
        "Border lines should be same width: {:?}",
        border_widths
    );
}

#[test]
fn test_table_multiple_rows() {
    let input = "| H1 | H2 | H3 |\n|----|----|----|\n| A | B | C |\n| D | E | F |";
    let result = render_to_text(input, 80);
    eprintln!("Table output:\n{}", result);
    assert!(result.contains('├'), "Expected row separators");
    assert!(result.contains('┼'), "Expected cross junctions");
}

fn line_text(line: &RenderedLine) -> String {
    line.spans.iter().map(|(text, _)| text.as_str()).collect()
}

fn rendered_text(conversation: &RenderedConversation) -> String {
    conversation
        .lines
        .iter()
        .map(line_text)
        .collect::<Vec<_>>()
        .join("\n")
}

fn test_render_options(tool_display: ToolDisplayMode) -> RenderOptions {
    RenderOptions {
        tool_display,
        show_thinking: false,
        show_timing: false,
        content_width: 80,
        expanded_tool_outputs: BTreeSet::new(),
        whole_task_reports: false,
    }
}

/// A Claude record as the viewer receives it: deserialized, then with the
/// canonical tool of each call assigned by the Claude provider.
fn claude_entry(json: &str) -> LogEntry {
    let mut entry = serde_json::from_str(json).unwrap();
    crate::history::provider::assign_canonical_tools(&mut entry);
    entry
}

fn tool_summary_entries() -> Vec<RenderableEntry> {
    vec![
        RenderableEntry {
            entry_index: 0,
            entry: claude_entry(
                r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_1","name":"Grep","input":{"pattern":"one"}}]}}"#,
            ),
        },
        RenderableEntry {
            entry_index: 1,
            entry: serde_json::from_str(
                r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"grep result"}]}}"#,
            )
            .unwrap(),
        },
        RenderableEntry {
            entry_index: 2,
            entry: claude_entry(
                r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_2","name":"Read","input":{"file_path":"src/main.rs"}},{"type":"tool_use","id":"toolu_3","name":"Bash","input":{"command":"cargo test"}}]}}"#,
            ),
        },
        RenderableEntry {
            entry_index: 3,
            entry: serde_json::from_str(
                r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_3","content":"bash result"}]}}"#,
            )
            .unwrap(),
        },
    ]
}

#[test]
fn hidden_tool_mode_renders_activity_summary() {
    let entry = RenderableEntry {
        entry_index: 0,
        entry: claude_entry(
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_1","name":"Grep","input":{"pattern":"one"}},{"type":"tool_use","id":"toolu_2","name":"Grep","input":{"pattern":"two"}},{"type":"tool_use","id":"toolu_3","name":"Read","input":{"file_path":"src/main.rs"}}]}}"#,
        ),
    };
    let rendered =
        render_parsed_conversation(&[entry], &test_render_options(ToolDisplayMode::Hidden));
    let text = rendered_text(&rendered);

    assert!(text.contains("Searched for 2 patterns"));
    assert!(text.contains("read 1 file"));
    assert!(!text.contains("Grep:"));
    assert!(!text.contains("Read:"));
}

#[test]
fn summary_names_what_claude_current_tools_did() {
    let entry = RenderableEntry {
        entry_index: 0,
        entry: claude_entry(
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_1","name":"PowerShell","input":{"command":"git status"}},{"type":"tool_use","id":"toolu_2","name":"TaskUpdate","input":{"taskId":"1","status":"completed"}},{"type":"tool_use","id":"toolu_3","name":"Agent","input":{"description":"Scout the tree","prompt":"List the modules."}}]}}"#,
        ),
    };
    let rendered =
        render_parsed_conversation(&[entry], &test_render_options(ToolDisplayMode::Hidden));
    let text = rendered_text(&rendered);

    assert!(
        text.contains("Ran 1 shell command, started 1 agent, updated the task list 1 time"),
        "{text}"
    );
}

#[test]
fn summary_counts_agent_messages_and_waits() {
    let entry = RenderableEntry {
        entry_index: 0,
        entry: claude_entry(
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_1","name":"SendMessage","input":{"to":"scout","message":"status?"}},{"type":"tool_use","id":"toolu_2","name":"TaskOutput","input":{"task_id":"t1"}},{"type":"tool_use","id":"toolu_3","name":"TaskOutput","input":{"task_id":"t2"}},{"type":"tool_use","id":"toolu_4","name":"ExitPlanMode","input":{"plan":"- step"}}]}}"#,
        ),
    };
    let rendered =
        render_parsed_conversation(&[entry], &test_render_options(ToolDisplayMode::Hidden));
    let text = rendered_text(&rendered);

    assert!(
        text.contains("Messaged 1 agent, waited 2 times, called 1 tool"),
        "{text}"
    );
}

fn codex_tool_run_entries() -> Vec<RenderableEntry> {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rollout.jsonl");
    std::fs::write(
        &path,
        concat!(
            r#"{"timestamp":"2026-08-01T10:00:00.000Z","type":"session_meta","payload":{"id":"019f0000-0000-7000-8000-00000000000c","timestamp":"2026-08-01T10:00:00.000Z","cwd":"/tmp/project"}}"#,
            "\n",
            r#"{"timestamp":"2026-08-01T10:00:01.000Z","type":"response_item","payload":{"type":"custom_tool_call","call_id":"call_1","name":"exec","input":"await tools.shell_command({\"command\":\"cargo test\"})"}}"#,
            "\n",
            r#"{"timestamp":"2026-08-01T10:00:02.000Z","type":"response_item","payload":{"type":"custom_tool_call_output","call_id":"call_1","output":"ok"}}"#,
            "\n",
            r#"{"timestamp":"2026-08-01T10:00:03.000Z","type":"response_item","payload":{"type":"custom_tool_call","call_id":"call_2","name":"apply_patch","input":"*** Begin Patch\n*** Update File: src/lib.rs\n@@\n-old\n+new\n*** End Patch"}}"#,
            "\n",
            r#"{"timestamp":"2026-08-01T10:00:04.000Z","type":"response_item","payload":{"type":"custom_tool_call_output","call_id":"call_2","output":"Done!"}}"#,
            "\n",
            r#"{"timestamp":"2026-08-01T10:00:05.000Z","type":"response_item","payload":{"type":"function_call","call_id":"call_3","name":"spawn_agent","arguments":"{\"task_name\":\"scout\",\"message\":\"List the modules.\"}"}}"#,
            "\n",
            r#"{"timestamp":"2026-08-01T10:00:06.000Z","type":"response_item","payload":{"type":"function_call_output","call_id":"call_3","output":"spawned"}}"#,
            "\n",
        ),
    )
    .unwrap();
    parse_conversation_file(crate::history::Source::Codex, &path, &[]).unwrap()
}

#[test]
fn summary_names_what_a_codex_run_did() {
    let entries = codex_tool_run_entries();
    let rendered =
        render_parsed_conversation(&entries, &test_render_options(ToolDisplayMode::Hidden));
    let text = rendered_text(&rendered);

    assert!(
        text.contains("Ran 1 shell command, edited 1 file, started 1 agent"),
        "{text}"
    );
    assert_eq!(text.matches("Codex").count(), 1);
}

#[test]
fn codex_tool_headers_print_the_codex_name() {
    let entries = codex_tool_run_entries();
    let rendered =
        render_parsed_conversation(&entries, &test_render_options(ToolDisplayMode::Truncated));
    let text = rendered_text(&rendered);

    for header in [
        "exec: cargo test",
        "apply_patch: src/lib.rs",
        "spawn_agent: scout",
    ] {
        assert!(text.contains(header), "missing {header:?} in:\n{text}");
    }
    assert_eq!(style_of_span(&rendered, "-old").fg, Some(th().diff_remove));
    assert_eq!(style_of_span(&rendered, "+new").fg, Some(th().diff_add));
}

fn kimi_tool_run_entries() -> Vec<RenderableEntry> {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("wire.jsonl");
    std::fs::write(
        &path,
        concat!(
            r#"{"type":"metadata","protocol_version":"1.5","created_at":1786010400000}"#,
            "\n",
            r#"{"type":"context.append_loop_event","event":{"type":"tool.call","uuid":"6f000000-0000-4000-8000-000000000001","toolCallId":"Bash_0","name":"Bash","args":{"command":"cargo test"}},"time":1786010401000}"#,
            "\n",
            r#"{"type":"context.append_loop_event","event":{"type":"tool.result","toolCallId":"Bash_0","result":{"output":"ok"}},"time":1786010402000}"#,
            "\n",
            r#"{"type":"context.append_loop_event","event":{"type":"tool.call","uuid":"6f000000-0000-4000-8000-000000000002","toolCallId":"Edit_0","name":"Edit","args":{"path":"src/lib.rs","old_string":"old","new_string":"new"}},"time":1786010403000}"#,
            "\n",
            r#"{"type":"context.append_loop_event","event":{"type":"tool.result","toolCallId":"Edit_0","result":{"output":"edited"}},"time":1786010404000}"#,
            "\n",
            r#"{"type":"context.append_loop_event","event":{"type":"tool.call","uuid":"6f000000-0000-4000-8000-000000000003","toolCallId":"Agent_0","name":"Agent","args":{"description":"scout","prompt":"List the modules."}},"time":1786010405000}"#,
            "\n",
            r#"{"type":"context.append_loop_event","event":{"type":"tool.result","toolCallId":"Agent_0","result":{"output":"done"}},"time":1786010406000}"#,
            "\n",
        ),
    )
    .unwrap();
    parse_conversation_file(crate::history::Source::Kimi, &path, &[]).unwrap()
}

#[test]
fn summary_names_what_a_kimi_run_did() {
    let entries = kimi_tool_run_entries();
    let rendered =
        render_parsed_conversation(&entries, &test_render_options(ToolDisplayMode::Hidden));
    let text = rendered_text(&rendered);

    assert!(
        text.contains("Ran 1 shell command, edited 1 file, started 1 agent"),
        "{text}"
    );
    assert_eq!(text.matches("Kimi").count(), 1);
}

#[test]
fn kimi_tool_headers_print_the_kimi_name() {
    let entries = kimi_tool_run_entries();
    let rendered =
        render_parsed_conversation(&entries, &test_render_options(ToolDisplayMode::Truncated));
    let text = rendered_text(&rendered);

    for header in ["Bash: cargo test", "Edit: src/lib.rs", "Agent: scout"] {
        assert!(text.contains(header), "missing {header:?} in:\n{text}");
    }
    assert_eq!(style_of_span(&rendered, "-old").fg, Some(th().diff_remove));
    assert_eq!(style_of_span(&rendered, "+new").fg, Some(th().diff_add));
}

fn pi_tool_run_entries() -> Vec<RenderableEntry> {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.jsonl");
    std::fs::write(
        &path,
        concat!(
            r#"{"type":"session","version":3,"id":"pi-tools","timestamp":"2026-08-01T10:00:00.000Z","cwd":"/tmp/project"}"#,
            "\n",
            r#"{"type":"message","id":"a1","parentId":null,"timestamp":"2026-08-01T10:00:01.000Z","message":{"role":"assistant","content":[{"type":"toolCall","id":"call_1","name":"bash","arguments":{"command":"cargo test"}}]}}"#,
            "\n",
            r#"{"type":"message","id":"t1","parentId":"a1","timestamp":"2026-08-01T10:00:02.000Z","message":{"role":"toolResult","toolCallId":"call_1","toolName":"bash","content":[{"type":"text","text":"ok"}]}}"#,
            "\n",
            r#"{"type":"message","id":"a2","parentId":"t1","timestamp":"2026-08-01T10:00:03.000Z","message":{"role":"assistant","content":[{"type":"toolCall","id":"call_2","name":"edit","arguments":{"path":"src/lib.rs","edits":[{"oldText":"old","newText":"new"}]}}]}}"#,
            "\n",
            r#"{"type":"message","id":"t2","parentId":"a2","timestamp":"2026-08-01T10:00:04.000Z","message":{"role":"toolResult","toolCallId":"call_2","toolName":"edit","content":[{"type":"text","text":"edited"}]}}"#,
            "\n",
            r#"{"type":"message","id":"a3","parentId":"t2","timestamp":"2026-08-01T10:00:05.000Z","message":{"role":"assistant","content":[{"type":"toolCall","id":"call_3","name":"read","arguments":{"path":"README.md"}}]}}"#,
            "\n",
            r#"{"type":"message","id":"t3","parentId":"a3","timestamp":"2026-08-01T10:00:06.000Z","message":{"role":"toolResult","toolCallId":"call_3","toolName":"read","content":[{"type":"text","text":"readme contents"}]}}"#,
            "\n",
        ),
    )
    .unwrap();
    parse_conversation_file(crate::history::Source::Pi, &path, &[]).unwrap()
}

#[test]
fn summary_names_what_a_pi_run_did() {
    let entries = pi_tool_run_entries();
    let rendered =
        render_parsed_conversation(&entries, &test_render_options(ToolDisplayMode::Hidden));
    let text = rendered_text(&rendered);

    assert!(
        text.contains("Read 1 file, ran 1 shell command, edited 1 file"),
        "{text}"
    );
    assert_eq!(text.matches("Pi").count(), 1);
}

#[test]
fn pi_tool_headers_print_the_pi_name() {
    let entries = pi_tool_run_entries();
    let rendered =
        render_parsed_conversation(&entries, &test_render_options(ToolDisplayMode::Truncated));
    let text = rendered_text(&rendered);

    for header in ["bash: cargo test", "edit: src/lib.rs", "read: README.md"] {
        assert!(text.contains(header), "missing {header:?} in:\n{text}");
    }
    assert_eq!(style_of_span(&rendered, "-old").fg, Some(th().diff_remove));
    assert_eq!(style_of_span(&rendered, "+new").fg, Some(th().diff_add));
}

fn omp_tool_run_entries() -> Vec<RenderableEntry> {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.jsonl");
    std::fs::write(
        &path,
        concat!(
            r#"{"type":"title","v":1,"title":"tools fixture","source":"user","updatedAt":"2026-08-01T10:00:00.000Z","pad":""}"#,
            "\n",
            r#"{"type":"session","version":3,"id":"omp-tools","timestamp":"2026-08-01T10:00:00.000Z","cwd":"/tmp/project"}"#,
            "\n",
            r#"{"type":"message","id":"a1","parentId":null,"timestamp":"2026-08-01T10:00:01.000Z","message":{"role":"assistant","content":[{"type":"toolCall","id":"call_1","name":"bash","arguments":{"command":"cargo test","i":"run the tests"}}]}}"#,
            "\n",
            r#"{"type":"message","id":"t1","parentId":"a1","timestamp":"2026-08-01T10:00:02.000Z","message":{"role":"toolResult","toolCallId":"call_1","toolName":"bash","content":[{"type":"text","text":"ok"}]}}"#,
            "\n",
            r#"{"type":"message","id":"a2","parentId":"t1","timestamp":"2026-08-01T10:00:03.000Z","message":{"role":"assistant","content":[{"type":"toolCall","id":"call_2","name":"edit","arguments":{"input":"[src/lib.rs#A1B2]\nINS.POST 3:\n+added\n[README.md#C3D4]\nDEL 1","i":"update regex"}}]}}"#,
            "\n",
            r#"{"type":"message","id":"t2","parentId":"a2","timestamp":"2026-08-01T10:00:04.000Z","message":{"role":"toolResult","toolCallId":"call_2","toolName":"edit","content":[{"type":"text","text":"edited"}]}}"#,
            "\n",
        ),
    )
    .unwrap();
    parse_conversation_file(crate::history::Source::Omp, &path, &[]).unwrap()
}

#[test]
fn summary_names_what_an_omp_run_did() {
    let entries = omp_tool_run_entries();
    let rendered =
        render_parsed_conversation(&entries, &test_render_options(ToolDisplayMode::Hidden));
    let text = rendered_text(&rendered);

    assert!(
        text.contains("Ran 1 shell command, edited 2 files"),
        "{text}"
    );
    assert_eq!(text.matches("OMP").count(), 1);
}

#[test]
fn an_omp_edit_shows_one_header_per_file_with_its_rows_coloured() {
    let entries = omp_tool_run_entries();
    let rendered =
        render_parsed_conversation(&entries, &test_render_options(ToolDisplayMode::Truncated));
    let text = rendered_text(&rendered);

    for header in ["bash: cargo test", "edit: src/lib.rs", "edit: README.md"] {
        assert!(text.contains(header), "missing {header:?} in:\n{text}");
    }
    assert!(text.contains("INS.POST 3:"), "{text}");
    assert!(text.contains("DEL 1"), "{text}");
    assert_eq!(style_of_span(&rendered, "+added").fg, Some(th().diff_add));
}

fn opencode_tool_run_entries() -> Vec<RenderableEntry> {
    use crate::history::format::opencode::{fixture, session_ref};

    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("opencode.db");
    let connection = fixture::create_database(&database);
    fixture::insert_session(
        &connection,
        &fixture::SessionSpec {
            id: "ses_tools",
            parent_id: None,
            directory: "/tmp/project",
            title: "fixture generated title",
            created_ms: 1755000100000i64,
            updated_ms: 1755000400000i64,
            archived_ms: None,
        },
    );
    fixture::insert_message(
        &connection,
        "msg_asst",
        "ses_tools",
        1755000200000i64,
        &serde_json::json!({ "role": "assistant", "time": { "created": 1755000200000i64 } }),
    );
    for (part, time_ms, tool, input) in [
        (
            "prt_0001",
            1755000210000i64,
            "bash",
            serde_json::json!({ "command": "cargo test" }),
        ),
        (
            "prt_0002",
            1755000220000i64,
            "edit",
            serde_json::json!({ "filePath": "src/lib.rs", "oldString": "old", "newString": "new" }),
        ),
        (
            "prt_0003",
            1755000230000i64,
            "task",
            serde_json::json!({ "description": "scout", "prompt": "List the modules.", "subagent_type": "explore" }),
        ),
    ] {
        fixture::insert_part(
            &connection,
            part,
            "msg_asst",
            "ses_tools",
            time_ms,
            &serde_json::json!({
                "type": "tool",
                "tool": tool,
                "callID": format!("call_{part}"),
                "state": { "status": "completed", "input": input, "output": "ok" },
            }),
        );
    }
    drop(connection);
    parse_conversation_file(
        crate::history::Source::OpenCode,
        &session_ref(&database, "ses_tools"),
        &[],
    )
    .unwrap()
}

#[test]
fn summary_names_what_an_opencode_run_did() {
    let entries = opencode_tool_run_entries();
    let rendered =
        render_parsed_conversation(&entries, &test_render_options(ToolDisplayMode::Hidden));
    let text = rendered_text(&rendered);

    assert!(
        text.contains("Ran 1 shell command, edited 1 file, started 1 agent"),
        "{text}"
    );
    assert_eq!(text.matches("OpenCode").count(), 1);
}

#[test]
fn opencode_tool_headers_print_the_opencode_name() {
    let entries = opencode_tool_run_entries();
    let rendered =
        render_parsed_conversation(&entries, &test_render_options(ToolDisplayMode::Truncated));
    let text = rendered_text(&rendered);

    for header in [
        "bash: cargo test",
        "edit: src/lib.rs",
        "task (explore): scout",
    ] {
        assert!(text.contains(header), "missing {header:?} in:\n{text}");
    }
    assert_eq!(style_of_span(&rendered, "-old").fg, Some(th().diff_remove));
    assert_eq!(style_of_span(&rendered, "+new").fg, Some(th().diff_add));
}

fn style_of_span<'a>(rendered: &'a RenderedConversation, text: &str) -> &'a LineStyle {
    rendered
        .lines
        .iter()
        .flat_map(|line| line.spans.iter())
        .find(|(span, _)| span == text)
        .map(|(_, style)| style)
        .unwrap_or_else(|| panic!("no span {text:?} in:\n{}", rendered_text(rendered)))
}

#[test]
fn diff_bodies_are_coloured_and_plain_bodies_are_not() {
    let entry = RenderableEntry {
        entry_index: 0,
        entry: claude_entry(
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_1","name":"Edit","input":{"file_path":"src/lib.rs","old_string":"old line","new_string":"new line"}},{"type":"tool_use","id":"toolu_2","name":"Agent","input":{"description":"Review","prompt":"- a markdown bullet\n+ not an addition"}}]}}"#,
        ),
    };
    let rendered =
        render_parsed_conversation(&[entry], &test_render_options(ToolDisplayMode::Full));

    assert_eq!(
        style_of_span(&rendered, "-old line").fg,
        Some(th().diff_remove)
    );
    assert_eq!(
        style_of_span(&rendered, "+new line").fg,
        Some(th().diff_add)
    );
    assert_eq!(style_of_span(&rendered, "- a markdown bullet").fg, None);
    assert_eq!(style_of_span(&rendered, "+ not an addition").fg, None);
}

/// A dimmed span draws in `text_muted` whatever its `fg`, so `.fg` alone
/// cannot show that a diff is colored on screen; `dimmed` has to be off too.
fn assert_signed_lines_colored_and_plain_rows_dimmed(rendered: &RenderedConversation) {
    let removed = style_of_span(rendered, "-old line");
    assert_eq!(removed.fg, Some(th().diff_remove));
    assert!(!removed.dimmed);
    let added = style_of_span(rendered, "+new line");
    assert_eq!(added.fg, Some(th().diff_add));
    assert!(!added.dimmed);

    // Inside a batch the header's tool word is split off in colour; the rest
    // of the header stays dimmed either way.
    let header = rendered
        .lines
        .iter()
        .flat_map(|line| line.spans.iter())
        .find(|(span, _)| span.ends_with("src/lib.rs"))
        .map(|(_, style)| style)
        .expect("the Edit header");
    assert!(header.dimmed);
    let plain = style_of_span(rendered, "- a markdown bullet");
    assert_eq!(plain.fg, None);
    assert!(plain.dimmed);
}

#[test]
fn a_diff_keeps_its_colors_inside_an_expanded_run_in_summary_mode() {
    let entry = RenderableEntry {
        entry_index: 0,
        entry: claude_entry(
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_1","name":"Edit","input":{"file_path":"src/lib.rs","old_string":"old line","new_string":"new line"}},{"type":"tool_use","id":"toolu_2","name":"Agent","input":{"description":"Review","prompt":"- a markdown bullet"}}]}}"#,
        ),
    };
    let mut options = test_render_options(ToolDisplayMode::Hidden);
    options
        .expanded_tool_outputs
        .insert(make_tool_summary_output_id(0, None));

    let rendered = render_parsed_conversation(&[entry], &options);

    assert_signed_lines_colored_and_plain_rows_dimmed(&rendered);
}

#[test]
fn a_subagent_diff_keeps_its_colors() {
    let entry = RenderableEntry {
        entry_index: 0,
        entry: claude_entry(
            r#"{"type":"assistant","parent_tool_use_id":"toolu_parent","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_1","name":"Edit","input":{"file_path":"src/lib.rs","old_string":"old line","new_string":"new line"}},{"type":"tool_use","id":"toolu_2","name":"Agent","input":{"description":"Review","prompt":"- a markdown bullet"}}]}}"#,
        ),
    };
    let mut options = test_render_options(ToolDisplayMode::Full);
    options.show_thinking = true;

    let rendered = render_parsed_conversation(&[entry], &options);

    assert_signed_lines_colored_and_plain_rows_dimmed(&rendered);
}

#[test]
fn hidden_tool_mode_coalesces_tool_only_entries_across_results() {
    let entries = tool_summary_entries();
    let rendered =
        render_parsed_conversation(&entries, &test_render_options(ToolDisplayMode::Hidden));
    let text = rendered_text(&rendered);

    assert!(text.contains("Searched for 1 pattern, read 1 file, ran 1 shell command"));
    assert_eq!(text.matches("Claude").count(), 1);
    assert!(!text.contains("Result"));
}

#[test]
fn tool_summary_uses_source_agent_label() {
    let entries = vec![
        RenderableEntry {
            entry_index: 0,
            entry: serde_json::from_str(
                r#"{"type":"assistant","agent":"Pi","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_1","name":"bash","input":{"command":"pwd"}}]}}"#,
            )
            .unwrap(),
        },
        RenderableEntry {
            entry_index: 1,
            entry: serde_json::from_str(
                r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"result"}]}}"#,
            )
            .unwrap(),
        },
    ];
    let summary_id = make_tool_summary_output_id(0, None);

    let collapsed =
        render_parsed_conversation(&entries, &test_render_options(ToolDisplayMode::Hidden));
    let collapsed_text = rendered_text(&collapsed);
    assert!(collapsed_text.contains("Pi"));
    assert!(!collapsed_text.contains("Claude"));

    let mut options = test_render_options(ToolDisplayMode::Hidden);
    options.expanded_tool_outputs.insert(summary_id);
    let expanded = render_parsed_conversation(&entries, &options);
    let expanded_text = rendered_text(&expanded);
    assert!(expanded_text.contains("Pi"));
    assert!(!expanded_text.contains("Claude"));
}

/// A command the user ran carries their label and completes the sentence it
/// opens, rather than borrowing the agent's `bash:` header.
#[test]
fn a_user_run_command_reads_as_you_ran_the_command() {
    let entries = vec![user_shell_entry(0, "call_1", "wc -l .gitignore")];

    let rendered =
        render_parsed_conversation(&entries, &test_render_options(ToolDisplayMode::Truncated));

    let text = rendered_text(&rendered);
    assert!(text.contains("You"), "{text}");
    assert!(text.contains("ran wc -l .gitignore"), "{text}");
    assert!(!text.contains("bash:"), "{text}");
    assert!(
        text.contains('↓'),
        "the result answers the call above it: {text}"
    );
}

/// Summary mode showed nothing for a user-run command, which owns no run of
/// its own until it opens one.
#[test]
fn a_user_run_command_collapses_to_a_run_of_its_own() {
    let entries = vec![user_shell_entry(0, "call_1", "wc -l .gitignore")];

    let collapsed =
        render_parsed_conversation(&entries, &test_render_options(ToolDisplayMode::Hidden));

    let text = rendered_text(&collapsed);
    assert!(text.contains("You"), "{text}");
    assert!(text.contains("Ran 1 shell command"), "{text}");
}

/// A run's stamp comes from the entry that opened it, so a user's own run
/// fills the timing column like the agent's.
#[test]
fn a_user_run_carries_the_stamp_of_the_command_that_opened_it() {
    const RAN_AT: &str = "2026-08-31T19:09:19.694Z";
    let mut entry = user_shell_entry(0, "call_1", "wc -l .gitignore");
    let LogEntry::User { timestamp, .. } = &mut entry.entry else {
        panic!("the helper builds a user entry");
    };
    *timestamp = Some(RAN_AT.to_owned());
    let mut options = test_render_options(ToolDisplayMode::Hidden);
    options.show_timing = true;

    let collapsed = render_parsed_conversation(&[entry], &options);

    let rendered = rendered_text(&collapsed);
    let summary_row = rendered
        .lines()
        .find(|line| line.contains("Ran 1 shell command"))
        .expect("the run collapses to a summary row");
    let stamp = format_timestamp(RAN_AT).expect("the stamp renders in the reader's own zone");
    assert!(
        summary_row.contains(&stamp),
        "the run's row has no stamp: {summary_row:?}"
    );
}

/// The user's own commands and the agent's calls collapse under their own
/// labels, however they interleave.
#[test]
fn a_user_run_command_does_not_join_the_agents_run() {
    let entries = vec![
        RenderableEntry {
            entry_index: 0,
            entry: serde_json::from_str(
                r#"{"type":"assistant","agent":"Pi","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_1","name":"bash","tool":"shell","input":{"command":"pwd"}}]}}"#,
            )
            .unwrap(),
        },
        RenderableEntry {
            entry_index: 1,
            entry: serde_json::from_str(
                r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"/tmp"}]}}"#,
            )
            .unwrap(),
        },
        user_shell_entry(2, "call_1", "wc -l .gitignore"),
    ];

    let collapsed =
        render_parsed_conversation(&entries, &test_render_options(ToolDisplayMode::Hidden));

    let text = rendered_text(&collapsed);
    assert_eq!(
        text.matches("Ran 1 shell command").count(),
        2,
        "one run each, not one run of two commands: {text}"
    );
    assert!(text.contains("Pi"), "{text}");
    assert!(text.contains("You"), "{text}");
}

/// A standalone result names its tool, so the row heads the result with it.
#[test]
fn a_received_tool_result_names_its_tool() {
    let entries = vec![received_result_entry(0, "send_message_to_thread")];

    let rendered =
        render_parsed_conversation(&entries, &test_render_options(ToolDisplayMode::Truncated));

    let text = rendered_text(&rendered);
    let rows = text
        .lines()
        .skip_while(|line| !line.contains("send_message_to_thread"))
        .take(2)
        .collect::<Vec<_>>();

    assert_eq!(
        rows.len(),
        2,
        "the tool heads the result on a row of its own: {text}"
    );
    assert!(rows[0].contains("Result"), "{text}");
    assert!(
        rows[1].contains("delegated task"),
        "the body keeps its own rows: {text}"
    );
}

/// Summary mode showed nothing for a standalone result: with no call above
/// it, it joined no run.
#[test]
fn a_received_tool_result_collapses_under_the_session_agent() {
    let entries = vec![
        RenderableEntry {
            entry_index: 0,
            entry: serde_json::from_str(
                r#"{"type":"assistant","agent":"Codex","message":{"role":"assistant","content":[{"type":"text","text":"working"}]}}"#,
            )
            .unwrap(),
        },
        received_result_entry(1, "send_message_to_thread"),
    ];

    let collapsed =
        render_parsed_conversation(&entries, &test_render_options(ToolDisplayMode::Hidden));

    let row = rendered_text(&collapsed);
    let summary_row = row
        .lines()
        .find(|line| line.contains("Received 1 tool result"))
        .expect("the result collapses to a run of its own");
    assert!(
        summary_row.contains("Codex"),
        "a run of received results carries the session's agent, not Claude: {summary_row:?}"
    );
}

/// A run mixing the agent's calls with a result it received names both.
#[test]
fn a_run_names_the_calls_it_made_and_the_results_it_received() {
    let entries = vec![
        RenderableEntry {
            entry_index: 0,
            entry: serde_json::from_str(
                r#"{"type":"assistant","agent":"Codex","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_1","name":"bash","tool":"shell","input":{"command":"pwd"}}]}}"#,
            )
            .unwrap(),
        },
        RenderableEntry {
            entry_index: 1,
            entry: serde_json::from_str(
                r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"/tmp"}]}}"#,
            )
            .unwrap(),
        },
        received_result_entry(2, "send_message_to_thread"),
    ];

    let collapsed =
        render_parsed_conversation(&entries, &test_render_options(ToolDisplayMode::Hidden));

    let text = rendered_text(&collapsed);
    assert!(
        text.contains("Ran 1 shell command, received 1 tool result"),
        "{text}"
    );
}

/// A user entry holding one standalone result, as the Codex reader builds it.
fn received_result_entry(entry_index: usize, name: &str) -> RenderableEntry {
    RenderableEntry {
        entry_index,
        entry: serde_json::from_str(&format!(
            r#"{{"type":"user","message":{{"role":"user","content":[
                {{"type":"tool_result","tool_use_id":"fco_1","standalone_tool_name":"{name}","content":"delegated task"}}
            ]}}}}"#
        ))
        .unwrap(),
    }
}

/// A user entry holding the command the user ran and what it printed, as the
/// Pi reader builds it.
fn user_shell_entry(entry_index: usize, call_id: &str, command: &str) -> RenderableEntry {
    RenderableEntry {
        entry_index,
        entry: serde_json::from_str(&format!(
            r#"{{"type":"user","message":{{"role":"user","content":[
                {{"type":"tool_use","id":"{call_id}","name":"bash","tool":"user_shell","input":{{"command":"{command}"}}}},
                {{"type":"tool_result","tool_use_id":"{call_id}","content":"4 .gitignore"}}
            ]}}}}"#
        ))
        .unwrap(),
    }
}

#[test]
fn hidden_pi_thinking_allows_clickable_grouped_tool_summary() {
    let entries = vec![
        RenderableEntry {
            entry_index: 0,
            entry: serde_json::from_str(
                r#"{"type":"assistant","agent":"Pi","message":{"role":"assistant","content":[{"type":"thinking","thinking":"inspect files","signature":""},{"type":"tool_use","id":"call_1","name":"bash","input":{"command":"pwd"}}]}}"#,
            )
            .unwrap(),
        },
        RenderableEntry {
            entry_index: 1,
            entry: serde_json::from_str(
                r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"call_1","content":"/tmp/project"}]}}"#,
            )
            .unwrap(),
        },
        RenderableEntry {
            entry_index: 2,
            entry: serde_json::from_str(
                r#"{"type":"assistant","agent":"Pi","message":{"role":"assistant","content":[{"type":"thinking","thinking":"inspect status","signature":""},{"type":"tool_use","id":"call_2","name":"bash","input":{"command":"git status"}}]}}"#,
            )
            .unwrap(),
        },
        RenderableEntry {
            entry_index: 3,
            entry: serde_json::from_str(
                r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"call_2","content":"clean"}]}}"#,
            )
            .unwrap(),
        },
    ];
    let summary_id = make_tool_summary_output_id(0, None);

    let collapsed =
        render_parsed_conversation(&entries, &test_render_options(ToolDisplayMode::Hidden));
    let collapsed_text = rendered_text(&collapsed);
    assert_eq!(collapsed_text.matches("Pi").count(), 1);
    assert!(
        collapsed
            .lines
            .iter()
            .any(|line| { line.clickable && line.tool_output_id.as_ref() == Some(&summary_id) })
    );

    let mut options = test_render_options(ToolDisplayMode::Hidden);
    options.expanded_tool_outputs.insert(summary_id);
    let expanded = render_parsed_conversation(&entries, &options);
    let expanded_text = rendered_text(&expanded);
    assert!(expanded_text.contains("pwd"));
    assert!(expanded_text.contains("git status"));
    assert!(expanded_text.contains("/tmp/project"));
    assert!(expanded_text.contains("clean"));
}

#[test]
fn expanded_tool_summary_renders_truncated_details() {
    let entries = tool_summary_entries();
    let mut options = test_render_options(ToolDisplayMode::Hidden);
    options
        .expanded_tool_outputs
        .insert(make_tool_summary_output_id(0, None));
    let rendered = render_parsed_conversation(&entries, &options);
    let text = rendered_text(&rendered);

    assert_eq!(
        text.matches("Searched for 1 pattern, read 1 file, ran 1 shell command (expanded):")
            .count(),
        1,
        "{text}"
    );
    assert!(text.contains("Grep: \"one\" in ."));
    assert!(text.contains("Read: src/main.rs"));
    assert!(text.contains("Bash: cargo test"));
    assert!(text.contains("Result"));
    assert!(text.contains("bash result"));
    assert!(rendered.lines.iter().any(|line| {
        line.clickable
            && line.tool_output_id.as_ref() == Some(&make_tool_summary_output_id(0, None))
    }));
}

#[test]
fn expanded_run_heading_is_the_only_row_with_the_run_id() {
    let entries = tool_summary_entries();
    let run_id = make_tool_summary_output_id(0, None);
    let mut options = test_render_options(ToolDisplayMode::Hidden);
    options.expanded_tool_outputs.insert(run_id.clone());
    let rendered = render_parsed_conversation(&entries, &options);

    let heading = &rendered.lines[0];
    assert!(
        line_text(heading).ends_with("(expanded):"),
        "{}",
        line_text(heading)
    );
    assert!(heading.clickable);
    assert_eq!(heading.tool_output_id.as_ref(), Some(&run_id));
    assert_eq!(lines_tagged_with(&rendered, &run_id), 1);

    // The first call follows the heading directly and keeps its own id.
    let first_call = &rendered.lines[1];
    assert!(line_text(first_call).contains("Grep: \"one\" in ."));
    assert_eq!(
        first_call.tool_output_id.as_ref(),
        Some(&make_tool_output_id(
            0,
            None,
            0,
            ToolOutputKind::ToolCall,
            Some("toolu_1")
        ))
    );

    assert_eq!(rendered.messages.len(), 1);
    assert_eq!(rendered.messages[0].entry_index, 0);
    assert_eq!(rendered.messages[0].start_line, 0);
}

#[test]
fn an_expanded_run_records_the_rows_of_each_call_and_its_result() {
    let entries = tool_summary_entries();
    let mut options = test_render_options(ToolDisplayMode::Hidden);
    options
        .expanded_tool_outputs
        .insert(make_tool_summary_output_id(0, None));
    let rendered = render_parsed_conversation(&entries, &options);

    let ids: Vec<_> = rendered
        .calls
        .iter()
        .map(|call| {
            (
                call.input.id.clone(),
                call.result.as_ref().map(|result| result.id.clone()),
            )
        })
        .collect();
    let call_id = |entry, block, raw| {
        make_tool_output_id(entry, None, block, ToolOutputKind::ToolCall, Some(raw))
    };
    let result_id = |entry, block, raw| {
        make_tool_output_id(entry, None, block, ToolOutputKind::ToolResult, Some(raw))
    };
    assert_eq!(
        ids,
        vec![
            (call_id(0, 0, "toolu_1"), Some(result_id(1, 0, "toolu_1"))),
            (call_id(2, 0, "toolu_2"), None),
            (call_id(2, 1, "toolu_3"), Some(result_id(3, 0, "toolu_3"))),
        ]
    );
    for call in &rendered.calls {
        assert_eq!(
            rendered.lines[call.input.start_line]
                .tool_output_id
                .as_ref(),
            Some(&call.input.id)
        );
    }
    for area in rendered
        .calls
        .iter()
        .flat_map(|call| std::iter::once(&call.input).chain(call.result.as_ref()))
    {
        assert!(area.start_line < area.end_line);
    }
    let bash_result = rendered.calls[2].result.as_ref().unwrap();
    let first_result_row = line_text(&rendered.lines[bash_result.start_line]);
    assert!(first_result_row.contains("Result"), "{first_result_row}");
    assert!(first_result_row.contains("bash result"));
}

#[test]
fn a_collapsed_run_and_the_detail_modes_record_no_call_ranges() {
    let entries = tool_summary_entries();
    for mode in [
        ToolDisplayMode::Hidden,
        ToolDisplayMode::Truncated,
        ToolDisplayMode::Full,
    ] {
        let rendered = render_parsed_conversation(&entries, &test_render_options(mode));
        assert!(rendered.calls.is_empty(), "{mode:?}");
    }
}

/// A Claude Code batch of interleaved calls as the file holds it: one block
/// per entry, both calls before either result.
fn interleaved_batch_entries() -> Vec<RenderableEntry> {
    vec![
        RenderableEntry {
            entry_index: 0,
            entry: claude_entry(
                r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_a","name":"Edit","input":{"file_path":"a.rs","old_string":"x","new_string":"y"}}]}}"#,
            ),
        },
        RenderableEntry {
            entry_index: 1,
            entry: claude_entry(
                r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_b","name":"Edit","input":{"file_path":"b.rs","old_string":"p","new_string":"q"}}]}}"#,
            ),
        },
        RenderableEntry {
            entry_index: 2,
            entry: serde_json::from_str(
                r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_a","content":"done a"}]}}"#,
            )
            .unwrap(),
        },
        RenderableEntry {
            entry_index: 3,
            entry: serde_json::from_str(
                r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_b","content":"done b"}]}}"#,
            )
            .unwrap(),
        },
    ]
}

fn render_expanded_run(entries: &[RenderableEntry], show_timing: bool) -> RenderedConversation {
    let mut options = test_render_options(ToolDisplayMode::Hidden);
    options.show_timing = show_timing;
    options
        .expanded_tool_outputs
        .insert(make_tool_summary_output_id(0, None));
    render_parsed_conversation(entries, &options)
}

/// The nine cells of the label column, `offset` cells in: 0 with timing
/// off, `TIMESTAMP_WIDTH` with it on.
fn label_column(line: &RenderedLine, offset: usize) -> String {
    cells(line, offset, NAME_WIDTH)
}

/// The three cells of the rule after a label column at `offset`.
fn rule(line: &RenderedLine, offset: usize) -> String {
    cells(line, offset + NAME_WIDTH, 3)
}

fn cells(line: &RenderedLine, from: usize, count: usize) -> String {
    line_text(line).chars().skip(from).take(count).collect()
}

/// A one-row `Read` call.
fn read_call_entry(entry_index: usize, tool_use_id: &str) -> RenderableEntry {
    RenderableEntry {
        entry_index,
        entry: claude_entry(&format!(
            r#"{{"type":"assistant","message":{{"role":"assistant","content":[{{"type":"tool_use","id":"{tool_use_id}","name":"Read","input":{{"file_path":"src/{tool_use_id}.rs"}}}}]}}}}"#
        )),
    }
}

fn lanes_of(rendered: &RenderedConversation) -> Vec<Option<usize>> {
    rendered.calls.iter().map(|call| call.lane).collect()
}

#[test]
fn interleaved_calls_take_lanes_in_call_order() {
    let batch = render_expanded_run(&interleaved_batch_entries(), false);
    assert_eq!(lanes_of(&batch), vec![Some(0), Some(1)]);
}

#[test]
fn a_call_never_answered_opens_no_lane() {
    // toolu_1 answered alone; toolu_2 never answered, so toolu_3 is alone too.
    let unanswered_between = render_expanded_run(&tool_summary_entries(), false);
    assert_eq!(lanes_of(&unanswered_between), vec![None, None, None]);

    let unanswered_first = render_expanded_run(
        &[
            read_call_entry(0, "toolu_a"),
            read_call_entry(1, "toolu_b"),
            read_call_entry(2, "toolu_c"),
            tool_result_entry(3, "toolu_b"),
            tool_result_entry(4, "toolu_c"),
        ],
        false,
    );
    assert_eq!(lanes_of(&unanswered_first), vec![None, Some(0), Some(1)]);
}

#[test]
fn a_connector_joins_each_call_of_a_batch_to_its_result() {
    let rendered = render_expanded_run(&interleaved_batch_entries(), false);
    let [a, b] = rendered.calls.as_slice() else {
        panic!("two calls expected, got {}", rendered.calls.len());
    };
    let (a_result, b_result) = (a.result.as_ref().unwrap(), b.result.as_ref().unwrap());
    let lines = &rendered.lines;

    let a_anchor = a.input.end_line - 1;
    assert_eq!(label_column(&lines[a_anchor], 0), "   ┌─────");
    assert_eq!(rule(&lines[a_anchor], 0), "─┘ ");
    assert!(
        lines[a_anchor]
            .spans
            .iter()
            .any(|(text, style)| text == "┌─────" && style.fg == lane_color(Some(0))),
        "{:?}",
        lines[a_anchor].spans
    );

    // A's lane crosses B's rows in cell 3, hidden where B's label sits.
    assert_eq!(label_column(&lines[b.input.start_line], 0), "   Claude");
    assert_eq!(label_column(&lines[b.input.start_line + 1], 0), "   │     ");
    let b_anchor = b.input.end_line - 1;
    assert_eq!(label_column(&lines[b_anchor], 0), "   │┌────");
    assert_eq!(rule(&lines[b_anchor], 0), "─┘ ");

    // The blank row above A's result carries A's tip beside B's lane.
    assert_eq!(
        label_column(&lines[a_result.start_line - 1], 0),
        "   ↓│    "
    );
    assert_eq!(label_column(&lines[a_result.start_line], 0), "   Result");
    assert_eq!(rule(&lines[a_result.start_line], 0), " ┐ ");

    assert_eq!(
        label_column(&lines[b_result.start_line - 1], 0),
        "    ↓    "
    );
    assert_eq!(rule(&lines[b_result.start_line], 0), " ┐ ");
}

#[test]
fn a_one_row_input_anchors_its_connector_on_the_rule_alone() {
    let rendered = render_expanded_run(&tool_summary_entries(), false);
    let grep = &rendered.calls[0];
    assert_eq!(grep.lane, None);
    assert_eq!(grep.input.end_line - grep.input.start_line, 1);
    let lines = &rendered.lines;

    assert_eq!(label_column(&lines[grep.input.start_line], 0), "   Claude");
    assert_eq!(rule(&lines[grep.input.start_line], 0), " ┤ ");
    let result = grep.result.as_ref().unwrap();
    assert_eq!(label_column(&lines[result.start_line - 1], 0), "   ↓     ");
    assert_eq!(rule(&lines[result.start_line], 0), " ┐ ");
}

#[test]
fn a_call_without_a_result_draws_no_connector() {
    let rendered = render_expanded_run(&tool_summary_entries(), false);
    let read = &rendered.calls[1];
    assert!(read.result.is_none());
    assert_eq!(rule(&rendered.lines[read.input.end_line - 1], 0), " │ ");

    let entering = rendered
        .lines
        .iter()
        .filter(|line| rule(line, 0) == " ┐ ")
        .count();
    assert_eq!(entering, 2, "one per call with a result");
}

#[test]
fn a_call_without_a_result_has_no_gap() {
    let rendered = render_expanded_run(&tool_summary_entries(), false);
    let read = &rendered.calls[1];
    assert!(read.result.is_none());
    assert!(read.input_to_result_gap().is_empty());
}

#[test]
fn connectors_sit_after_the_timing_column_when_timing_is_on() {
    let rendered = render_expanded_run(&interleaved_batch_entries(), true);
    let a = &rendered.calls[0];
    let lines = &rendered.lines;

    let a_anchor = a.input.end_line - 1;
    assert_eq!(label_column(&lines[a_anchor], TIMESTAMP_WIDTH), "   ┌─────");
    assert_eq!(rule(&lines[a_anchor], TIMESTAMP_WIDTH), "─┘ ");
    let tip = a.result.as_ref().unwrap().start_line - 1;
    assert_eq!(
        cells(&lines[tip], 0, TIMESTAMP_WIDTH + NAME_WIDTH),
        "          ↓│    "
    );
}

#[test]
fn a_result_is_labelled_result_without_an_arrow_in_every_mode() {
    let expanded = render_expanded_run(&interleaved_batch_entries(), false);
    assert!(!rendered_text(&expanded).contains('↳'));
    let result_row = expanded.calls[0].result.as_ref().unwrap().start_line;
    assert_eq!(label_column(&expanded.lines[result_row], 0), "   Result");

    for mode in [ToolDisplayMode::Truncated, ToolDisplayMode::Full] {
        let detail =
            render_parsed_conversation(&interleaved_batch_entries(), &test_render_options(mode));
        let text = rendered_text(&detail);
        assert!(text.contains("Result ┐ done a"), "{mode:?}:\n{text}");
        assert!(!text.contains('↳'), "{mode:?}:\n{text}");
    }
}

/// One call with a body, answered before the next call: no batch.
fn sequential_edit_entries() -> Vec<RenderableEntry> {
    vec![
        RenderableEntry {
            entry_index: 0,
            entry: claude_entry(
                r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_a","name":"Edit","input":{"file_path":"a.rs","old_string":"x","new_string":"y"}}]}}"#,
            ),
        },
        tool_result_entry(1, "toolu_a"),
    ]
}

/// `count` interleaved one-row `Read` calls, then their results in call
/// order.
fn batch_of(count: usize) -> Vec<RenderableEntry> {
    let calls = (0..count).map(|i| read_call_entry(i, &format!("toolu_{i}")));
    let results = (0..count).map(|i| tool_result_entry(count + i, &format!("toolu_{i}")));
    calls.chain(results).collect()
}

/// The style of the span covering `cell`, counted from the row's first cell.
fn style_at(line: &RenderedLine, cell: usize) -> &LineStyle {
    let mut next_cell = 0;
    line.spans
        .iter()
        .find_map(|(text, style)| {
            let start = next_cell;
            next_cell += text.chars().count();
            (start <= cell && cell < next_cell).then_some(style)
        })
        .unwrap_or_else(|| panic!("no span covers cell {cell} of {:?}", line_text(line)))
}

/// The style of the rule after a label column at `offset`, whatever glyph a
/// connector left in it.
fn rule_style(line: &RenderedLine, offset: usize) -> &LineStyle {
    style_at(line, offset + NAME_WIDTH)
}

#[test]
fn interleaved_calls_colour_their_rules_and_connectors_by_lane() {
    let rendered = render_expanded_run(&interleaved_batch_entries(), false);
    let palette = th().batch_call_colors;
    let lines = &rendered.lines;
    for (call, expected) in rendered.calls.iter().zip(palette) {
        for area in call.areas() {
            for line in &lines[area.start_line..area.end_line] {
                assert_eq!(
                    rule_style(line, 0).fg,
                    Some(expected),
                    "{}",
                    line_text(line)
                );
            }
        }
        let anchor = &lines[call.input.end_line - 1];
        assert!(
            anchor
                .spans
                .iter()
                .any(|(text, style)| text.starts_with('┌') && style.fg == Some(expected)),
            "{:?}",
            anchor.spans
        );
    }
}

#[test]
fn a_call_open_alone_draws_its_rule_and_connector_in_the_rule_grey() {
    let rendered = render_expanded_run(&sequential_edit_entries(), false);
    let call = &rendered.calls[0];
    assert_eq!(call.lane, None);
    let lines = &rendered.lines;
    let rule_grey = LineStyle::colored(th().border);

    // The run's dimmed rule would render lighter than the result's.
    assert_eq!(rule_style(&lines[call.input.start_line], 0), &rule_grey);
    let anchor = &lines[call.input.end_line - 1];
    assert_eq!(rule(anchor, 0), "─┘ ");
    assert_eq!(rule_style(anchor, 0), &rule_grey);
    let result = call.result.as_ref().unwrap();
    assert_eq!(rule(&lines[result.start_line], 0), " ┐ ");
    assert_eq!(rule_style(&lines[result.start_line], 0), &rule_grey);
}

#[test]
fn an_interleaved_call_colours_its_tool_word() {
    let rendered = render_expanded_run(&interleaved_batch_entries(), false);
    let header = &rendered.lines[rendered.calls[0].input.start_line];
    let (_, word) = header
        .spans
        .iter()
        .find(|(text, _)| text == "Edit:")
        .expect("the tool word in its own span");
    assert_eq!(word.fg, Some(th().batch_call_colors[0]));

    let alone = render_expanded_run(&sequential_edit_entries(), false);
    let header = &alone.lines[alone.calls[0].input.start_line];
    assert!(header.spans.iter().any(|(text, _)| text == "Edit: a.rs"));
}

#[test]
fn the_palette_repeats_past_its_end() {
    let palette = th().batch_call_colors;
    assert_eq!(lane_color(Some(palette.len())), Some(palette[0]));
}

#[test]
fn a_seventh_interleaved_call_keeps_its_colour_and_draws_no_connector() {
    let rendered = render_expanded_run(&batch_of(7), false);
    let seventh = &rendered.calls[6];
    assert_eq!(seventh.lane, Some(6));
    let lines = &rendered.lines;
    let palette = th().batch_call_colors;
    let expected = palette[6 % palette.len()];

    assert_eq!(
        rule_style(&lines[seventh.input.start_line], 0).fg,
        Some(expected)
    );
    let result = seventh.result.as_ref().unwrap();
    assert_eq!(rule(&lines[result.start_line], 0), " │ ");
    assert_eq!(rule_style(&lines[result.start_line], 0).fg, Some(expected));

    // The sixth call still fits under the label's last letter.
    let sixth_result = rendered.calls[5].result.as_ref().unwrap();
    assert_eq!(rule(&lines[sixth_result.start_line], 0), " ┐ ");
}

/// The first row whose text holds `text`.
fn row_containing(rendered: &RenderedConversation, text: &str) -> usize {
    rendered
        .lines
        .iter()
        .position(|line| line_text(line).contains(text))
        .unwrap_or_else(|| panic!("no row contains {text:?} in:\n{}", rendered_text(rendered)))
}

#[test]
fn the_detail_modes_join_each_call_to_its_result() {
    for mode in [ToolDisplayMode::Truncated, ToolDisplayMode::Full] {
        let rendered =
            render_parsed_conversation(&interleaved_batch_entries(), &test_render_options(mode));
        let lines = &rendered.lines;

        // Each input ends at its diff's last row.
        let a_anchor = row_containing(&rendered, "+y");
        assert_eq!(label_column(&lines[a_anchor], 0), "   ┌─────", "{mode:?}");
        assert_eq!(rule(&lines[a_anchor], 0), "─┘ ", "{mode:?}");
        let b_anchor = row_containing(&rendered, "+q");
        assert_eq!(label_column(&lines[b_anchor], 0), "   │┌────", "{mode:?}");

        let a_result = row_containing(&rendered, "done a");
        assert_eq!(
            label_column(&lines[a_result - 1], 0),
            "   ↓│    ",
            "{mode:?}"
        );
        assert_eq!(label_column(&lines[a_result], 0), "   Result", "{mode:?}");
        assert_eq!(rule(&lines[a_result], 0), " ┐ ", "{mode:?}");
        let b_result = row_containing(&rendered, "done b");
        assert_eq!(
            label_column(&lines[b_result - 1], 0),
            "    ↓    ",
            "{mode:?}"
        );
        assert_eq!(rule(&lines[b_result], 0), " ┐ ", "{mode:?}");
    }
}

#[test]
fn interleaved_calls_in_the_detail_modes_take_their_lane_colours() {
    for mode in [ToolDisplayMode::Truncated, ToolDisplayMode::Full] {
        let rendered =
            render_parsed_conversation(&interleaved_batch_entries(), &test_render_options(mode));
        let lines = &rendered.lines;
        let palette = th().batch_call_colors;

        for (call, expected) in [("+y", "done a"), ("+q", "done b")]
            .into_iter()
            .zip(palette)
        {
            let (anchor, result) = call;
            assert_eq!(
                rule_style(&lines[row_containing(&rendered, anchor)], 0).fg,
                Some(expected),
                "{mode:?}"
            );
            assert_eq!(
                rule_style(&lines[row_containing(&rendered, result)], 0).fg,
                Some(expected),
                "{mode:?}"
            );
        }

        let header = &lines[row_containing(&rendered, "Edit: b.rs")];
        let (_, word) = header
            .spans
            .iter()
            .find(|(text, _)| text == "Edit:")
            .expect("the tool word in its own span");
        assert_eq!(word.fg, Some(palette[1]), "{mode:?}");
    }
}

#[test]
fn a_call_alone_in_the_detail_modes_draws_its_connector_in_the_rule_grey() {
    let rendered = render_parsed_conversation(
        &tool_summary_entries(),
        &test_render_options(ToolDisplayMode::Truncated),
    );
    let lines = &rendered.lines;
    let rule_grey = LineStyle::colored(th().border);

    // Grep is answered before the next call; its one-row input anchors on
    // the rule.
    let grep = row_containing(&rendered, "Grep:");
    assert_eq!(rule(&lines[grep], 0), " ┤ ");
    assert_eq!(rule_style(&lines[grep], 0), &rule_grey);
    let grep_result = row_containing(&rendered, "grep result");
    assert_eq!(label_column(&lines[grep_result - 1], 0), "   ↓     ");
    assert_eq!(label_column(&lines[grep_result], 0), "   Result");
    assert_eq!(rule(&lines[grep_result], 0), " ┐ ");
    assert_eq!(rule_style(&lines[grep_result], 0), &rule_grey);
}

#[test]
fn a_call_never_answered_draws_no_connector_in_the_detail_modes() {
    let rendered = render_parsed_conversation(
        &tool_summary_entries(),
        &test_render_options(ToolDisplayMode::Truncated),
    );
    let lines = &rendered.lines;

    // Read is never answered, so Bash after it is alone.
    let read = row_containing(&rendered, "Read:");
    assert_eq!(rule(&lines[read], 0), " │ ");
    let bash = row_containing(&rendered, "Bash:");
    assert_eq!(rule(&lines[bash], 0), " ┤ ");
    assert_eq!(
        rule_style(&lines[bash], 0),
        &LineStyle::colored(th().border)
    );
}

/// A batch issued while calls of an earlier batch still await their
/// results, as a fork's calls interleave with the parent's: A, B, C; result
/// A; D (with a body), E; result B; result D; result C; result E.
fn later_batch_beside_open_calls() -> Vec<RenderableEntry> {
    vec![
        read_call_entry(0, "toolu_a"),
        read_call_entry(1, "toolu_b"),
        read_call_entry(2, "toolu_c"),
        tool_result_entry_holding(3, "toolu_a", "done a"),
        RenderableEntry {
            entry_index: 4,
            entry: claude_entry(
                r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_d","name":"Edit","input":{"file_path":"d.rs","old_string":"x","new_string":"dee"}}]}}"#,
            ),
        },
        read_call_entry(5, "toolu_e"),
        tool_result_entry_holding(6, "toolu_b", "done b"),
        tool_result_entry_holding(7, "toolu_d", "done d"),
        tool_result_entry_holding(8, "toolu_c", "done c"),
        tool_result_entry_holding(9, "toolu_e", "done e"),
    ]
}

/// An expanded run and truncated mode over the same entries, named for
/// assertion messages.
fn expanded_run_and_truncated_mode(
    entries: &[RenderableEntry],
) -> [(&'static str, RenderedConversation); 2] {
    [
        ("expanded run", render_expanded_run(entries, false)),
        (
            "truncated",
            render_parsed_conversation(entries, &test_render_options(ToolDisplayMode::Truncated)),
        ),
    ]
}

#[test]
fn a_later_batch_takes_the_lanes_right_of_the_open_calls() {
    let entries = later_batch_beside_open_calls();
    assert_eq!(
        lanes_of(&render_expanded_run(&entries, false)),
        vec![Some(0), Some(1), Some(2), Some(3), Some(4)]
    );

    for (mode, rendered) in expanded_run_and_truncated_mode(&entries) {
        let lines = &rendered.lines;
        let tip_above = |text| row_containing(&rendered, text) - 1;

        // B and C stay open past A's result, so D's `┌─────` starts right
        // of their lanes and E's lane sits right of D's.
        assert_eq!(
            label_column(&lines[tip_above("done a")], 0),
            "   ↓││   ",
            "{mode}"
        );
        let d_anchor = row_containing(&rendered, "+dee");
        assert_eq!(label_column(&lines[d_anchor], 0), "    ││┌──", "{mode}");
        assert_eq!(rule(&lines[d_anchor], 0), "─┘ ", "{mode}");
        assert_eq!(
            label_column(&lines[tip_above("done b")], 0),
            "    ↓│││ ",
            "{mode}"
        );
        assert_eq!(
            label_column(&lines[tip_above("done d")], 0),
            "     │↓│ ",
            "{mode}"
        );
        assert_eq!(
            label_column(&lines[tip_above("done c")], 0),
            "     ↓ │ ",
            "{mode}"
        );
        assert_eq!(
            label_column(&lines[tip_above("done e")], 0),
            "       ↓ ",
            "{mode}"
        );

        let palette = th().batch_call_colors;
        for (result, expected) in ["done a", "done b", "done c", "done d", "done e"]
            .into_iter()
            .zip(palette)
        {
            assert_eq!(
                rule_style(&lines[row_containing(&rendered, result)], 0).fg,
                Some(expected),
                "{mode}: {result}"
            );
        }
    }
}

#[test]
fn a_call_issued_beside_an_open_call_takes_the_next_lane_and_a_colour() {
    // B still awaits its result when C is issued, so C's lane sits right of
    // B's and C takes that lane's colour.
    let entries = vec![
        read_call_entry(0, "toolu_a"),
        read_call_entry(1, "toolu_b"),
        tool_result_entry_holding(2, "toolu_a", "done a"),
        read_call_entry(3, "toolu_c"),
        tool_result_entry_holding(4, "toolu_b", "done b"),
        tool_result_entry_holding(5, "toolu_c", "done c"),
    ];
    assert_eq!(
        lanes_of(&render_expanded_run(&entries, false)),
        vec![Some(0), Some(1), Some(2)]
    );

    for (mode, rendered) in expanded_run_and_truncated_mode(&entries) {
        let lines = &rendered.lines;
        let palette = th().batch_call_colors;

        let c = row_containing(&rendered, "Read: src/toolu_c.rs");
        assert_eq!(rule(&lines[c], 0), " ┤ ", "{mode}");
        assert_eq!(rule_style(&lines[c], 0).fg, Some(palette[2]), "{mode}");
        let b_result = row_containing(&rendered, "done b");
        assert_eq!(label_column(&lines[b_result - 1], 0), "    ↓│   ", "{mode}");
        let c_result = row_containing(&rendered, "done c");
        assert_eq!(label_column(&lines[c_result - 1], 0), "     ↓   ", "{mode}");
        assert_eq!(
            rule_style(&lines[c_result], 0).fg,
            Some(palette[2]),
            "{mode}"
        );
    }
}

/// Two one-row calls in one entry, answered in one entry: the shape of a
/// source that holds several blocks per entry.
fn two_calls_per_entry() -> Vec<RenderableEntry> {
    vec![
        RenderableEntry {
            entry_index: 0,
            entry: claude_entry(
                r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_a","name":"Read","input":{"file_path":"a.rs"}},{"type":"tool_use","id":"toolu_b","name":"Read","input":{"file_path":"b.rs"}}]}}"#,
            ),
        },
        RenderableEntry {
            entry_index: 1,
            entry: serde_json::from_str(
                r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_a","content":"done a"},{"type":"tool_result","tool_use_id":"toolu_b","content":"done b"}]}}"#,
            )
            .unwrap(),
        },
    ]
}

#[test]
fn consecutive_tool_blocks_of_one_entry_are_separated_by_a_blank_row() {
    let rendered = render_parsed_conversation(
        &two_calls_per_entry(),
        &test_render_options(ToolDisplayMode::Truncated),
    );
    let lines = &rendered.lines;
    let a = row_containing(&rendered, "Read: a.rs");
    let b = row_containing(&rendered, "Read: b.rs");
    let a_result = row_containing(&rendered, "done a");
    let b_result = row_containing(&rendered, "done b");
    assert_eq!(b, a + 2, "{}", rendered_text(&rendered));
    assert_eq!(b_result, a_result + 2, "{}", rendered_text(&rendered));

    // The blank rows carry the connectors: A's lane past B's row, and each
    // call's `↓` above its result.
    assert_eq!(rule(&lines[a], 0), " ┤ ");
    assert_eq!(label_column(&lines[a + 1], 0), "   │     ");
    assert_eq!(rule(&lines[b], 0), " ┤ ");
    assert_eq!(label_column(&lines[a_result - 1], 0), "   ↓│    ");
    assert_eq!(rule(&lines[a_result], 0), " ┐ ");
    assert_eq!(label_column(&lines[b_result - 1], 0), "    ↓    ");
    assert_eq!(rule(&lines[b_result], 0), " ┐ ");
}

#[test]
fn a_subagents_calls_draw_no_connector_in_the_detail_modes() {
    let entries = vec![
        RenderableEntry {
            entry_index: 0,
            entry: claude_entry(
                r#"{"type":"assistant","parent_tool_use_id":"toolu_parent","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_1","name":"Read","input":{"file_path":"a.rs"}}]}}"#,
            ),
        },
        RenderableEntry {
            entry_index: 1,
            entry: serde_json::from_str(
                r#"{"type":"user","parent_tool_use_id":"toolu_parent","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"done"}]}}"#,
            )
            .unwrap(),
        },
    ];
    let mut options = test_render_options(ToolDisplayMode::Truncated);
    options.show_thinking = true;
    let rendered = render_parsed_conversation(&entries, &options);

    let text = rendered_text(&rendered);
    assert!(text.contains("↳ Tool │ <Result>"), "{text}");
    for glyph in ['┤', '┘', '┐', '↓'] {
        assert!(!text.contains(glyph), "{text}");
    }
}

#[test]
fn detail_mode_connectors_sit_after_the_timing_column() {
    let entries = stamped_call(0, "toolu_1", Some(RUN_START), Some("2026-02-04T12:30:05Z"));
    let mut options = test_render_options(ToolDisplayMode::Truncated);
    options.show_timing = true;
    let rendered = render_parsed_conversation(&entries, &options);
    let lines = &rendered.lines;

    let call = row_containing(&rendered, "Bash: cargo test");
    assert_eq!(rule(&lines[call], TIMESTAMP_WIDTH), " ┤ ");
    let result = row_containing(&rendered, "Result");
    assert_eq!(
        cells(&lines[result - 1], 0, TIMESTAMP_WIDTH + NAME_WIDTH),
        "          ↓     "
    );
    assert_eq!(rule(&lines[result], TIMESTAMP_WIDTH), " ┐ ");
}

#[test]
fn expanded_run_heading_carries_the_timestamp_and_detail_rows_pad() {
    let entries = vec![
        RenderableEntry {
            entry_index: 0,
            entry: claude_entry(
                r#"{"type":"assistant","timestamp":"2026-02-04T12:34:56Z","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_1","name":"Bash","input":{"command":"cargo test"}}]}}"#,
            ),
        },
        tool_result_entry(1, "toolu_1"),
    ];
    let mut options = test_render_options(ToolDisplayMode::Hidden);
    options.show_timing = true;
    options
        .expanded_tool_outputs
        .insert(make_tool_summary_output_id(0, None));
    let rendered = render_parsed_conversation(&entries, &options);

    let stamp = &rendered.lines[0].spans[0].0;
    assert_eq!(stamp.len(), TIMESTAMP_WIDTH);
    assert!(stamp.contains(':'), "{stamp:?}");

    let detail_rows: Vec<_> = rendered
        .lines
        .iter()
        .skip(1)
        .filter(|line| !line.spans.is_empty())
        .collect();
    assert!(detail_rows.len() >= 2, "{}", rendered_text(&rendered));
    assert!(detail_rows.iter().all(|line| {
        let first = &line.spans[0].0;
        first.len() == TIMESTAMP_WIDTH && first.trim().is_empty()
    }));
}

/// The widest row in columns. The screen draws the gutter in front of every
/// row, so a row fits a frame when this plus `GUTTER_WIDTH` does.
fn widest_row(rendered: &RenderedConversation) -> usize {
    rendered
        .lines
        .iter()
        .map(|line| line_text(line).chars().count())
        .max()
        .unwrap_or(0)
}

#[test]
fn rows_fill_the_frame_exactly_with_the_timestamp_column_shown_and_hidden() {
    const FRAME_WIDTH: usize = 60;
    let long_result = "x".repeat(200);
    let entries = vec![
        RenderableEntry {
            entry_index: 0,
            entry: claude_entry(
                r#"{"type":"assistant","timestamp":"2026-02-04T12:34:56Z","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_1","name":"Bash","input":{"command":"cargo test"}}]}}"#,
            ),
        },
        RenderableEntry {
            entry_index: 1,
            entry: serde_json::from_str(&format!(
                r#"{{"type":"user","timestamp":"2026-02-04T12:35:00Z","message":{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"toolu_1","content":"{long_result}"}}]}}}}"#
            ))
            .unwrap(),
        },
    ];

    // Both entries carry a timestamp: a block without one renders without
    // the timing column and stops seven columns short of the frame.
    for show_timing in [false, true] {
        let options = RenderOptions {
            tool_display: ToolDisplayMode::Full,
            show_thinking: false,
            show_timing,
            content_width: content_width(FRAME_WIDTH, show_timing),
            expanded_tool_outputs: BTreeSet::new(),
            whole_task_reports: false,
        };
        let rendered = render_parsed_conversation(&entries, &options);

        assert_eq!(
            widest_row(&rendered) + GUTTER_WIDTH,
            FRAME_WIDTH,
            "timing {show_timing}:\n{}",
            rendered_text(&rendered)
        );
    }
}

/// A row's text after the rule, whichever ledger columns precede it; a
/// blank row between blocks has none.
fn row_content(line: &RenderedLine) -> String {
    if line.spans.is_empty() {
        return String::new();
    }
    let rule = line
        .spans
        .iter()
        .position(|(text, _)| text == " │ ")
        .unwrap_or_else(|| panic!("no rule in {:?}", line_text(line)));
    line.spans[rule + 1..]
        .iter()
        .map(|(text, _)| text.as_str())
        .collect()
}

/// The style of a row's text, the span after the ledger columns; `None` for
/// a blank row between blocks.
fn content_style(line: &RenderedLine) -> Option<&LineStyle> {
    line.spans.last().map(|(_, style)| style)
}

/// The rows of text after `indent`, with the indent removed; blank rows are
/// skipped.
fn rows_after_indent(rendered: &RenderedConversation, indent: usize) -> Vec<String> {
    rendered
        .lines
        .iter()
        .map(row_content)
        .filter(|row| !row.is_empty())
        .map(|row| row[indent..].to_string())
        .collect()
}

fn assert_rows_fit(rendered: &RenderedConversation, content_width: usize) {
    for line in &rendered.lines {
        assert!(
            row_content(line).chars().count() <= content_width,
            "wider than {content_width}: {:?}",
            line_text(line)
        );
    }
}

fn words(text: &str) -> Vec<&str> {
    text.split_whitespace().collect()
}

#[test]
fn a_long_diff_line_wraps_in_its_colour_with_its_text_one_column_in() {
    let old = "old ".repeat(50);
    let new = "new ".repeat(50);
    let entry = tool_use_entry(
        0,
        "toolu_1",
        "Edit",
        &format!(r#"{{"file_path":"src/lib.rs","old_string":"{old}","new_string":"{new}"}}"#),
    );
    let rendered =
        render_parsed_conversation(&[entry], &test_render_options(ToolDisplayMode::Full));
    assert_rows_fit(&rendered, 80);

    for (color, sign, word) in [(th().diff_remove, "-", "old"), (th().diff_add, "+", "new")] {
        let rows: Vec<(String, &LineStyle)> = rendered
            .lines
            .iter()
            .filter_map(|line| content_style(line).map(|style| (row_content(line), style)))
            .filter(|(_, style)| style.fg == Some(color))
            .collect();
        assert!(rows.len() >= 3, "{word}: {rows:?}");
        for (_, style) in &rows {
            assert!(!style.dimmed, "{rows:?}");
        }
        let (first, later) = rows.split_first().unwrap();
        assert!(first.0.starts_with(sign), "{rows:?}");
        for (row, _) in later {
            assert!(row.starts_with(&format!(" {word}")), "{rows:?}");
        }
        let text: String = rows.iter().map(|(row, _)| row.as_str()).collect();
        assert_eq!(words(text.strip_prefix(sign).unwrap()), vec![word; 50]);
    }
}

#[test]
fn a_header_wraps_under_its_value_column_and_shows_whole_when_truncated() {
    let path = format!("/{}", "x".repeat(149));
    let entry = tool_use_entry(
        0,
        "toolu_1",
        "Read",
        &format!(r#"{{"file_path":"{path}"}}"#),
    );
    let rendered =
        render_parsed_conversation(&[entry], &test_render_options(ToolDisplayMode::Truncated));
    assert_rows_fit(&rendered, 80);

    let call_id = make_tool_output_id(0, None, 0, ToolOutputKind::ToolCall, Some("toolu_1"));
    let header_rows: Vec<&RenderedLine> = rendered
        .lines
        .iter()
        .filter(|line| line.tool_output_id.as_ref() == Some(&call_id))
        .collect();
    for line in &header_rows {
        assert!(!line.clickable, "{}", line_text(line));
    }
    let rows: Vec<String> = header_rows.iter().map(|line| row_content(line)).collect();
    assert_eq!(rows.len(), 3, "{rows:?}");
    assert!(rows[0].starts_with("Read: /"), "{rows:?}");
    let value_column = "Read: ".len();
    for row in &rows[1..] {
        assert!(row.starts_with(&" ".repeat(value_column)), "{rows:?}");
        assert!(!row[value_column..].starts_with(' '), "{rows:?}");
    }
    let joined: String = rows.iter().map(|row| &row[value_column..]).collect();
    assert_eq!(joined, path);
    assert!(!rendered_text(&rendered).contains("more lines"));
}

#[test]
fn a_shell_command_continues_under_its_first_row() {
    let command = "cargo test --all --locked ".repeat(20);
    let entry = || {
        tool_use_entry(
            0,
            "toolu_1",
            "Bash",
            &format!(r#"{{"command":"{command}"}}"#),
        )
    };
    let value_column = "Bash: ".len();

    let truncated =
        render_parsed_conversation(&[entry()], &test_render_options(ToolDisplayMode::Truncated));
    assert_rows_fit(&truncated, 80);
    let rows: Vec<String> = truncated.lines.iter().map(row_content).collect();
    assert!(rows[0].starts_with("Bash: cargo test"), "{rows:?}");
    for row in &rows[1..=TRUNCATED_BODY_LINES] {
        assert!(row.starts_with("      "), "no blank row: {rows:?}");
        assert!(!row[value_column..].starts_with(' '), "{rows:?}");
    }
    let indicator = TRUNCATED_BODY_LINES + 1;
    assert!(rows[indicator].starts_with("      ("), "{rows:?}");
    assert!(rows[indicator].ends_with(" more lines...)"), "{rows:?}");
    assert!(truncated.lines[indicator].clickable);

    let full = render_parsed_conversation(&[entry()], &test_render_options(ToolDisplayMode::Full));
    let text = rows_after_indent(&full, value_column).join(" ");
    assert_eq!(words(&text), words(&command));
    assert!(!rendered_text(&full).contains("more lines"));
}

#[test]
fn a_long_summary_row_wraps_and_every_row_carries_the_runs_id() {
    let entries = tool_summary_entries();
    let run_id = make_tool_summary_output_id(0, None);
    let mut options = test_render_options(ToolDisplayMode::Hidden);
    options.content_width = 40;
    let rendered = render_parsed_conversation(&entries, &options);
    assert_rows_fit(&rendered, 40);

    let heading: Vec<&RenderedLine> = rendered
        .lines
        .iter()
        .filter(|line| line.tool_output_id.as_ref() == Some(&run_id))
        .collect();
    assert_eq!(heading.len(), 2, "{}", rendered_text(&rendered));
    assert!(heading.iter().all(|line| line.clickable));
    assert!(line_text(heading[1]).starts_with(&" ".repeat(NAME_WIDTH)));
    let text: String = heading.iter().map(|line| row_content(line) + " ").collect();
    assert_eq!(
        words(&text),
        words("Searched for 1 pattern, read 1 file, ran 1 shell command")
    );
}

#[test]
fn a_long_agent_prompt_wraps_uncoloured_after_a_blank_row() {
    let prompt = "word ".repeat(40);
    let entry = tool_use_entry(
        0,
        "toolu_1",
        "Agent",
        &format!(r#"{{"description":"Review","prompt":"{prompt}"}}"#),
    );
    let rendered =
        render_parsed_conversation(&[entry], &test_render_options(ToolDisplayMode::Full));
    assert_rows_fit(&rendered, 80);

    let rows: Vec<String> = rendered.lines.iter().map(row_content).collect();
    assert_eq!(rows[0], "Agent: Review");
    assert_eq!(rows[1], "");
    let body: Vec<&RenderedLine> = rendered.lines[2..]
        .iter()
        .filter(|line| !row_content(line).is_empty())
        .collect();
    assert!(body.len() >= 3, "{rows:?}");
    for line in &body {
        let style = content_style(line).unwrap();
        assert_eq!(style.fg, None);
        assert!(style.dimmed);
        assert!(!row_content(line).starts_with(' '), "flush left: {rows:?}");
    }
}

#[test]
fn truncation_counts_rows_and_a_click_reveals_the_rest() {
    // Two source lines: 200 characters wrap to three rows, 100 to two.
    let prompt = format!("{}\\n{}", "word ".repeat(40), "word ".repeat(20));
    let entry = || {
        tool_use_entry(
            0,
            "toolu_1",
            "Agent",
            &format!(r#"{{"description":"Review","prompt":"{prompt}"}}"#),
        )
    };
    let call_id = make_tool_output_id(0, None, 0, ToolOutputKind::ToolCall, Some("toolu_1"));
    let clickable_rows = |rendered: &RenderedConversation| -> Vec<String> {
        let clickable: Vec<&RenderedLine> = rendered
            .lines
            .iter()
            .filter(|line| line.clickable)
            .collect();
        for line in &clickable {
            assert_eq!(line.tool_output_id.as_ref(), Some(&call_id));
        }
        clickable.iter().map(|line| row_content(line)).collect()
    };

    let truncated =
        render_parsed_conversation(&[entry()], &test_render_options(ToolDisplayMode::Truncated));
    let rows = clickable_rows(&truncated);
    assert_eq!(rows.len(), TRUNCATED_BODY_LINES + 1, "{rows:?}");
    assert_eq!(rows[TRUNCATED_BODY_LINES], "(2 more lines...)");

    let mut options = test_render_options(ToolDisplayMode::Truncated);
    options.expanded_tool_outputs.insert(call_id.clone());
    let expanded = render_parsed_conversation(&[entry()], &options);
    let rows = clickable_rows(&expanded);
    assert_eq!(rows.len(), 5, "{rows:?}");
    assert!(!rendered_text(&expanded).contains("more lines"));
}

#[test]
fn a_wrapped_header_inside_a_batch_keeps_its_coloured_tool_word() {
    let path = format!("src/{}.rs", "x".repeat(100));
    let mut entries = interleaved_batch_entries();
    entries[0] = tool_use_entry(
        0,
        "toolu_a",
        "Edit",
        &format!(r#"{{"file_path":"{path}","old_string":"x","new_string":"y"}}"#),
    );
    let rendered = render_expanded_run(&entries, false);

    let start = rendered.calls[0].input.start_line;
    let (_, word) = rendered.lines[start]
        .spans
        .iter()
        .find(|(text, _)| text == "Edit:")
        .expect("the tool word in its own span");
    assert_eq!(word.fg, Some(th().batch_call_colors[0]));
    assert!(
        row_content(&rendered.lines[start + 1]).starts_with("      "),
        "{}",
        rendered_text(&rendered)
    );
}

#[test]
fn a_subagent_result_wraps_at_the_content_width() {
    let value = "x".repeat(200);
    let entry = RenderableEntry {
        entry_index: 0,
        entry: serde_json::from_str(&format!(
            r#"{{"type":"user","parent_tool_use_id":"toolu_parent","message":{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"toolu_1","content":{{"text":"{value}"}}}}]}}}}"#
        ))
        .unwrap(),
    };
    let mut options = test_render_options(ToolDisplayMode::Truncated);
    options.show_thinking = true;
    let rendered = render_parsed_conversation(&[entry], &options);
    assert_rows_fit(&rendered, 80);

    let text = rendered_text(&rendered);
    assert!(text.contains("<Result>"), "{text}");
    assert!(text.contains(" more lines...)"), "{text}");
    let result_id = make_tool_output_id(
        0,
        Some("toolu_parent"),
        0,
        ToolOutputKind::ToolResult,
        Some("toolu_1"),
    );
    assert_eq!(
        lines_tagged_with(&rendered, &result_id),
        TRUNCATED_RESULT_LINES + 1
    );
}

#[test]
fn a_content_width_of_zero_leaves_every_line_whole() {
    let new = "new ".repeat(50);
    let entry = tool_use_entry(
        0,
        "toolu_1",
        "Edit",
        &format!(r#"{{"file_path":"src/lib.rs","old_string":"old","new_string":"{new}"}}"#),
    );
    let mut options = test_render_options(ToolDisplayMode::Full);
    options.content_width = 0;
    let rendered = render_parsed_conversation(&[entry], &options);

    assert!(
        rendered
            .lines
            .iter()
            .any(|line| row_content(line) == format!("+{new}")),
        "{}",
        rendered_text(&rendered)
    );
}

/// A tool call and the result answering it, the pair summary mode folds into
/// one run. A `None` timestamp leaves that entry unstamped.
fn stamped_call(
    first_entry_index: usize,
    tool_use_id: &str,
    called_at: Option<&str>,
    answered_at: Option<&str>,
) -> Vec<RenderableEntry> {
    let stamp = |timestamp: Option<&str>| {
        timestamp.map_or(String::new(), |timestamp| {
            format!(r#""timestamp":"{timestamp}","#)
        })
    };
    let call = format!(
        r#"{{"type":"assistant",{}"message":{{"role":"assistant","content":[{{"type":"tool_use","id":"{tool_use_id}","name":"Bash","input":{{"command":"cargo test"}}}}]}}}}"#,
        stamp(called_at)
    );
    let result = format!(
        r#"{{"type":"user",{}"message":{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"{tool_use_id}","content":"ok"}}]}}}}"#,
        stamp(answered_at)
    );
    vec![
        RenderableEntry {
            entry_index: first_entry_index,
            entry: claude_entry(&call),
        },
        RenderableEntry {
            entry_index: first_entry_index + 1,
            entry: serde_json::from_str(&result).unwrap(),
        },
    ]
}

const RUN_START: &str = "2026-02-04T12:30:00Z";

fn render_run_with_timing(entries: &[RenderableEntry]) -> RenderedConversation {
    let mut options = test_render_options(ToolDisplayMode::Hidden);
    options.show_timing = true;
    render_parsed_conversation(entries, &options)
}

#[test]
fn a_runs_row_ends_with_its_duration_when_timing_is_on() {
    let entries = stamped_call(0, "toolu_1", Some(RUN_START), Some("2026-02-04T12:32:10Z"));
    let rendered = render_run_with_timing(&entries);

    let row = line_text(&rendered.lines[0]);
    assert!(row.ends_with("Ran 1 shell command · 2m"), "{row}");
}

#[test]
fn an_expanded_runs_heading_carries_the_duration_before_its_marker() {
    let entries = stamped_call(0, "toolu_1", Some(RUN_START), Some("2026-02-04T12:32:10Z"));
    let mut options = test_render_options(ToolDisplayMode::Hidden);
    options.show_timing = true;
    options
        .expanded_tool_outputs
        .insert(make_tool_summary_output_id(0, None));
    let rendered = render_parsed_conversation(&entries, &options);

    let heading = line_text(&rendered.lines[0]);
    assert!(
        heading.ends_with("Ran 1 shell command · 2m (expanded):"),
        "{heading}"
    );
    let rows_with_a_duration = rendered
        .lines
        .iter()
        .filter(|line| line_text(line).contains(" · "))
        .count();
    assert_eq!(rows_with_a_duration, 1, "{}", rendered_text(&rendered));
}

#[test]
fn a_run_shows_no_duration_when_timing_is_off() {
    let entries = stamped_call(0, "toolu_1", Some(RUN_START), Some("2026-02-04T12:32:10Z"));
    let rendered =
        render_parsed_conversation(&entries, &test_render_options(ToolDisplayMode::Hidden));

    let text = rendered_text(&rendered);
    assert!(text.contains("Ran 1 shell command"), "{text}");
    assert!(!text.contains(" · "), "{text}");
}

#[test]
fn a_short_run_reads_in_seconds_and_a_long_one_in_hours() {
    for (answered_at, expected) in [
        ("2026-02-04T12:30:40Z", "Ran 1 shell command · 40s"),
        ("2026-02-04T13:35:00Z", "Ran 1 shell command · 1h 5m"),
    ] {
        let entries = stamped_call(0, "toolu_1", Some(RUN_START), Some(answered_at));
        let rendered = render_run_with_timing(&entries);

        let row = line_text(&rendered.lines[0]);
        assert!(row.ends_with(expected), "{row}");
    }
}

#[test]
fn a_run_ends_at_the_last_absorbed_entry_that_is_stamped() {
    let mut entries = stamped_call(0, "toolu_1", Some(RUN_START), None);
    entries.extend(stamped_call(
        2,
        "toolu_2",
        Some("2026-02-04T12:31:00Z"),
        None,
    ));
    let rendered = render_run_with_timing(&entries);

    let row = line_text(&rendered.lines[0]);
    assert!(row.ends_with("Ran 2 shell commands · 1m"), "{row}");
}

#[test]
fn a_run_whose_first_entry_is_unstamped_shows_no_duration() {
    let entries = stamped_call(0, "toolu_1", None, Some("2026-02-04T12:32:10Z"));
    let rendered = render_run_with_timing(&entries);

    let text = rendered_text(&rendered);
    assert!(text.contains("Ran 1 shell command"), "{text}");
    assert!(!text.contains(" · "), "{text}");
}

#[test]
fn subagent_summary_label_parity() {
    // Subagent tool-only assistant followed by its tool-result. The
    // collapsed summary uses the nested ↳ label; the expanded detail
    // rows must use the same nested label, not the literal "Claude".
    let entries = vec![
        RenderableEntry {
            entry_index: 0,
            entry: claude_entry(
                r#"{"type":"assistant","parent_tool_use_id":"toolu_parent_abc","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_1","name":"Grep","input":{"pattern":"one"}}]}}"#,
            ),
        },
        RenderableEntry {
            entry_index: 1,
            entry: serde_json::from_str(
                r#"{"type":"user","parent_tool_use_id":"toolu_parent_abc","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"grep result"}]}}"#,
            )
            .unwrap(),
        },
    ];
    let expected_label = "↳parent_";
    let summary_id = make_tool_summary_output_id(0, Some("toolu_parent_abc"));

    // Collapsed: subagent label appears, no literal "Claude".
    let mut options = test_render_options(ToolDisplayMode::Hidden);
    options.show_thinking = true;
    let collapsed = render_parsed_conversation(&entries, &options);
    let collapsed_text = rendered_text(&collapsed);
    assert!(
        collapsed_text.contains(expected_label),
        "collapsed summary should use nested label: {}",
        collapsed_text
    );
    assert!(
        !collapsed_text.contains("Claude"),
        "collapsed subagent summary should not use literal Claude label: {}",
        collapsed_text
    );

    // Expanded: detail rows must use the same nested label, not "Claude".
    let mut options = test_render_options(ToolDisplayMode::Hidden);
    options.show_thinking = true;
    options.expanded_tool_outputs.insert(summary_id);
    let expanded = render_parsed_conversation(&entries, &options);
    let expanded_text = rendered_text(&expanded);
    assert!(
        expanded_text.contains("Grep: \"one\" in ."),
        "expanded detail row should render tool call: {}",
        expanded_text
    );
    assert!(
        expanded_text.contains(expected_label),
        "expanded detail row should use nested label: {}",
        expanded_text
    );
    assert!(
        !expanded_text.contains("Claude"),
        "expanded subagent detail rows must not use literal Claude label: {}",
        expanded_text
    );
}

#[test]
fn hidden_tool_mode_status_label_is_summary() {
    assert_eq!(ToolDisplayMode::Hidden.status_label(), "sum");
}

#[test]
fn tool_output_ids_use_stable_literal_format() {
    assert_eq!(
        make_tool_output_id(0, None, 0, ToolOutputKind::ToolCall, Some("toolu_1")).0,
        "entry:0:parent:top:block:0:kind:call:id:toolu_1"
    );
    assert_eq!(
        make_tool_output_id(
            1,
            Some("toolu_parent"),
            2,
            ToolOutputKind::ToolResult,
            Some("toolu_2"),
        )
        .0,
        "entry:1:parent:toolu_parent:block:2:kind:result:id:toolu_2"
    );
    assert_eq!(
        make_tool_summary_output_id(3, Some("toolu_parent")).0,
        "entry:3:parent:toolu_parent:kind:summary"
    );
}

#[test]
fn parse_conversation_file_preserves_entry_indices() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("conversation.jsonl");
    std::fs::write(
        &path,
        concat!(
            "\n",
            r#"{"type":"user","message":{"role":"user","content":"first"}}"#,
            "\n",
            "not json\n",
            r#"{"type":"file-history-snapshot","messageId":"m1","snapshot":{},"isSnapshotUpdate":false}"#,
            "\n",
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"second"}]}}"#,
            "\n",
        ),
    )
    .unwrap();

    let entries = parse_unattributed_conversation_file(&path).unwrap();

    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].entry_index, 0);
    assert_eq!(entries[1].entry_index, 2);
}

#[test]
fn show_thinking_controls_subagent_entries() {
    let entries = vec![
        RenderableEntry {
            entry_index: 0,
            entry: serde_json::from_str(
                r#"{"type":"assistant","parent_tool_use_id":"toolu_parent","message":{"role":"assistant","content":[{"type":"text","text":"subagent text"}]}}"#,
            )
            .unwrap(),
        },
        RenderableEntry {
            entry_index: 1,
            entry: serde_json::from_str(
                r#"{"type":"progress","data":{"type":"agent_progress","agentId":"agent-abcdef123456","message":{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"agent progress text"}]}}}}"#,
            )
            .unwrap(),
        },
    ];
    let hidden =
        render_parsed_conversation(&entries, &test_render_options(ToolDisplayMode::Hidden));
    assert!(!rendered_text(&hidden).contains("subagent text"));
    assert!(!rendered_text(&hidden).contains("agent progress text"));

    let mut options = test_render_options(ToolDisplayMode::Hidden);
    options.show_thinking = true;
    let shown = render_parsed_conversation(&entries, &options);
    let text = rendered_text(&shown);
    assert!(text.contains("subagent text"));
    assert!(text.contains("agent progress text"));
}

#[test]
fn tool_call_metadata_tracks_truncated_and_expanded_state() {
    let input = serde_json::json!({"command":"one\ntwo\nthree\nfour\nfive"});
    let output_id = make_tool_output_id(0, None, 0, ToolOutputKind::ToolCall, Some("toolu_1"));
    let mut lines = Vec::new();
    render_tool_call(
        &mut lines,
        &ToolCallRenderSpec {
            name: "Bash",
            tool: Tool::Shell,
            input: &input,
            label: "Claude",
            label_color: th().accent_dim,
            dimmed: false,
            tool_word_color: None,
            content_width: 80,
            timing: timing::TimingSlot::Disabled,
            tool_display: ToolDisplayMode::Truncated,
            tool_output_id: &output_id,
            expanded: false,
        },
    );
    assert!(
        lines
            .iter()
            .any(|line| line_text(line).contains("more lines"))
    );
    assert!(
        lines
            .iter()
            .any(|line| line.clickable && line.tool_output_id.as_ref() == Some(&output_id))
    );
    assert!(!lines.iter().any(|line| line_text(line).contains("five")));

    let mut expanded = Vec::new();
    render_tool_call(
        &mut expanded,
        &ToolCallRenderSpec {
            name: "Bash",
            tool: Tool::Shell,
            input: &input,
            label: "Claude",
            label_color: th().accent_dim,
            dimmed: false,
            tool_word_color: None,
            content_width: 80,
            timing: timing::TimingSlot::Disabled,
            tool_display: ToolDisplayMode::Truncated,
            tool_output_id: &output_id,
            expanded: true,
        },
    );
    assert!(
        !expanded
            .iter()
            .any(|line| line_text(line).contains("more lines"))
    );
    assert!(expanded.iter().any(|line| line_text(line).contains("five")));
}

#[test]
fn test_format_timestamp() {
    // UTC timestamp with Z suffix
    let ts = "2026-02-04T19:46:38.440Z";
    let result = format_timestamp(ts);
    assert!(result.is_some(), "Should parse UTC timestamp");
    let formatted = result.unwrap();
    // Should be HH:MM format (local time)
    assert_eq!(formatted.len(), 5, "Should be HH:MM format: {}", formatted);
    assert!(
        formatted.contains(':'),
        "Should contain colon: {}",
        formatted
    );

    // Timestamp with timezone offset
    let ts2 = "2026-02-04T14:46:38-05:00";
    let result2 = format_timestamp(ts2);
    assert!(result2.is_some(), "Should parse timestamp with offset");
}

// -----------------------------------------------------------------
// Render regression harness
//
// These tests pin the observable behavior of the rendering pipeline
// (text, span styles, clickability, tool output IDs, message ranges)
// so subsequent refactors of the viewer can detect drift.
// -----------------------------------------------------------------

fn line_style_at<'a>(line: &'a RenderedLine, text: &str) -> &'a LineStyle {
    &line
        .spans
        .iter()
        .find(|(t, _)| t == text)
        .unwrap_or_else(|| panic!("span {:?} not found in line {:?}", text, line_text(line)))
        .1
}

fn user_entry(entry_index: usize, text: &str, timestamp: Option<&str>) -> RenderableEntry {
    let ts_field = match timestamp {
        Some(t) => format!(r#","timestamp":"{}""#, t),
        None => String::new(),
    };
    let json = format!(
        r#"{{"type":"user","message":{{"role":"user","content":"{}"}}{}}}"#,
        text, ts_field
    );
    RenderableEntry {
        entry_index,
        entry: serde_json::from_str(&json).unwrap(),
    }
}

fn assistant_text_entry(
    entry_index: usize,
    text: &str,
    timestamp: Option<&str>,
) -> RenderableEntry {
    let ts_field = match timestamp {
        Some(t) => format!(r#","timestamp":"{}""#, t),
        None => String::new(),
    };
    let json = format!(
        r#"{{"type":"assistant","message":{{"role":"assistant","content":[{{"type":"text","text":"{}"}}]}}{}}}"#,
        text, ts_field
    );
    RenderableEntry {
        entry_index,
        entry: serde_json::from_str(&json).unwrap(),
    }
}

#[test]
fn message_ranges_track_user_and_assistant_entries() {
    let entries = vec![
        user_entry(0, "Hello", None),
        assistant_text_entry(1, "Hi there", None),
    ];
    let rendered =
        render_parsed_conversation(&entries, &test_render_options(ToolDisplayMode::Hidden));

    // Two messages, each with one content line and one trailing blank.
    assert_eq!(rendered.messages.len(), 2);

    let user = &rendered.messages[0];
    assert_eq!(user.entry_index, 0);
    assert_eq!(user.start_line, 0);
    assert_eq!(user.end_line, 1, "user range excludes trailing blank");

    let assistant = &rendered.messages[1];
    assert_eq!(assistant.entry_index, 1);
    assert_eq!(assistant.start_line, 2);
    assert_eq!(assistant.end_line, 3);

    // Lines: [user, blank, assistant, blank]
    assert_eq!(rendered.lines.len(), 4);
    assert!(rendered.lines[1].spans.is_empty());
    assert!(rendered.lines[3].spans.is_empty());
}

#[test]
fn message_ranges_skip_non_message_entries() {
    // A summary-only entry produces no rendered output and no MessageRange.
    let entries = vec![
        RenderableEntry {
            entry_index: 0,
            entry: serde_json::from_str(r#"{"type":"summary","summary":"ignored"}"#).unwrap(),
        },
        user_entry(1, "Hello", None),
    ];
    let rendered =
        render_parsed_conversation(&entries, &test_render_options(ToolDisplayMode::Hidden));

    assert_eq!(rendered.messages.len(), 1);
    assert_eq!(rendered.messages[0].entry_index, 1);
    assert_eq!(rendered.messages[0].start_line, 0);
}

#[test]
fn timing_enabled_renders_timestamp_prefix_span() {
    let entries = vec![user_entry(0, "Hello", Some("2026-02-04T12:34:56Z"))];
    let mut options = test_render_options(ToolDisplayMode::Hidden);
    options.show_timing = true;

    let rendered = render_parsed_conversation(&entries, &options);
    let first = &rendered.lines[0];
    let ts_span = &first.spans[0].0;

    assert_eq!(
        ts_span.len(),
        TIMESTAMP_WIDTH,
        "timestamp prefix span width: {:?}",
        ts_span
    );
    assert!(
        ts_span.starts_with(' ') && ts_span.ends_with(' ') && ts_span.contains(':'),
        "timestamp prefix should be ' HH:MM ', got {:?}",
        ts_span
    );
}

#[test]
fn timing_disabled_omits_timestamp_prefix_span() {
    let entries = vec![user_entry(0, "Hello", Some("2026-02-04T12:34:56Z"))];
    let rendered =
        render_parsed_conversation(&entries, &test_render_options(ToolDisplayMode::Hidden));
    let first = &rendered.lines[0];

    // First span is the right-aligned name column, not a timestamp.
    assert_eq!(first.spans[0].0.trim(), "You");
}

#[test]
fn invalid_timestamp_skips_timestamp_prefix() {
    // Even with show_timing=true, a non-RFC3339 timestamp produces no time prefix.
    let entries = vec![user_entry(0, "Hello", Some("not-a-timestamp"))];
    let mut options = test_render_options(ToolDisplayMode::Hidden);
    options.show_timing = true;

    let rendered = render_parsed_conversation(&entries, &options);
    let first = &rendered.lines[0];
    assert_eq!(
        first.spans[0].0.trim(),
        "You",
        "first span should be name column, not timestamp"
    );
}

#[test]
fn assistant_continuation_line_aligns_under_timestamp() {
    // Multi-line assistant text should pad continuation lines to the
    // timestamp width so the name column stays aligned.
    let entries = vec![assistant_text_entry(
        0,
        "line one\\n\\nline two",
        Some("2026-02-04T12:34:56Z"),
    )];
    let mut options = test_render_options(ToolDisplayMode::Hidden);
    options.show_timing = true;

    let rendered = render_parsed_conversation(&entries, &options);
    // Expect at least: header line + blank-paragraph + content line.
    let timestamp_span = &rendered.lines[0].spans[0].0;
    assert_eq!(timestamp_span.len(), TIMESTAMP_WIDTH);

    // Find a continuation line (one whose first span is whitespace of TIMESTAMP_WIDTH).
    let has_padded_continuation = rendered.lines.iter().skip(1).any(|line| {
        line.spans
            .first()
            .is_some_and(|(t, _)| t.len() == TIMESTAMP_WIDTH && t.trim().is_empty())
    });
    assert!(
        has_padded_continuation,
        "expected a continuation line padded to TIMESTAMP_WIDTH"
    );
}

#[test]
fn user_label_uses_text_primary_bold() {
    let entries = vec![user_entry(0, "Hello", None)];
    let rendered =
        render_parsed_conversation(&entries, &test_render_options(ToolDisplayMode::Hidden));
    let line = &rendered.lines[0];
    let name_text = format!("{:>width$}", "You", width = NAME_WIDTH);
    let style = line_style_at(line, &name_text);

    assert_eq!(style.fg, Some(th().text_primary));
    assert!(style.bold);
    assert!(!style.dimmed);
}

#[test]
fn assistant_label_uses_accent_bold() {
    let entries = vec![assistant_text_entry(0, "Hi", None)];
    let rendered =
        render_parsed_conversation(&entries, &test_render_options(ToolDisplayMode::Hidden));
    let line = &rendered.lines[0];
    let name_text = format!("{:>width$}", "Claude", width = NAME_WIDTH);
    let style = line_style_at(line, &name_text);

    assert_eq!(style.fg, Some(th().accent));
    assert!(style.bold);
}

#[test]
fn subagent_assistant_uses_nested_label_when_thinking_shown() {
    let entries = vec![RenderableEntry {
        entry_index: 0,
        entry: serde_json::from_str(
            r#"{"type":"assistant","parent_tool_use_id":"toolu_parent_abc","message":{"role":"assistant","content":[{"type":"text","text":"sub text"}]}}"#,
        )
        .unwrap(),
    }];
    let mut options = test_render_options(ToolDisplayMode::Hidden);
    options.show_thinking = true;

    let rendered = render_parsed_conversation(&entries, &options);
    let text = rendered_text(&rendered);
    assert!(text.contains("sub text"));
    assert!(
        text.contains('↳'),
        "subagent rows should use the nested-label arrow: {}",
        text
    );
}

#[test]
fn truncated_tool_call_header_carries_expected_tool_output_id() {
    let entries = vec![RenderableEntry {
        entry_index: 7,
        entry: claude_entry(
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_xyz","name":"Bash","input":{"command":"ls"}}]}}"#,
        ),
    }];
    let rendered =
        render_parsed_conversation(&entries, &test_render_options(ToolDisplayMode::Truncated));

    let expected = make_tool_output_id(7, None, 0, ToolOutputKind::ToolCall, Some("toolu_xyz"));
    assert_eq!(
        expected.0,
        "entry:7:parent:top:block:0:kind:call:id:toolu_xyz"
    );
    assert!(
        rendered
            .lines
            .iter()
            .any(|line| line.tool_output_id.as_ref() == Some(&expected)),
        "expected at least one rendered line tagged with the tool output id"
    );
}

#[test]
fn full_tool_mode_lines_are_not_clickable() {
    // The last line wraps, so the rows outnumber the source lines.
    let entries = vec![tool_use_entry(
        0,
        "toolu_1",
        "Bash",
        &format!(
            r#"{{"command":"one\ntwo\nthree\nfour\n{}five"}}"#,
            "word ".repeat(40)
        ),
    )];
    let rendered =
        render_parsed_conversation(&entries, &test_render_options(ToolDisplayMode::Full));

    assert!(
        rendered.lines.iter().all(|line| !line.clickable),
        "Full tool display mode should not produce clickable lines"
    );
    // Body should be fully visible — no truncation indicator.
    let text = rendered_text(&rendered);
    assert!(text.contains("five"));
    assert!(!text.contains("more lines"));
}

#[test]
fn tool_result_string_content_renders_as_text() {
    let entries = vec![
        RenderableEntry {
            entry_index: 0,
            entry: claude_entry(
                r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_1","name":"Bash","input":{"command":"echo hi"}}]}}"#,
            ),
        },
        RenderableEntry {
            entry_index: 1,
            entry: serde_json::from_str(
                r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"hello-world-output"}]}}"#,
            )
            .unwrap(),
        },
    ];
    let rendered =
        render_parsed_conversation(&entries, &test_render_options(ToolDisplayMode::Truncated));

    let text = rendered_text(&rendered);
    assert!(
        text.contains("hello-world-output"),
        "tool result string content should render verbatim: {}",
        text
    );
}

fn assistant_with_reordered_blocks_entry() -> RenderableEntry {
    // Source order is intentionally the reverse of the rendered
    // ordering contract (thinking → tool_use → text). The renderer
    // must reorder to text → tool/summary → thinking.
    RenderableEntry {
        entry_index: 0,
        entry: claude_entry(
            r#"{"type":"assistant","message":{"role":"assistant","content":[
                {"type":"thinking","thinking":"THINK_BLOCK","signature":"sig"},
                {"type":"tool_use","id":"toolu_1","name":"Bash","input":{"command":"echo hi"}},
                {"type":"text","text":"TEXT_BLOCK"}
            ]}}"#,
        ),
    }
}

#[test]
fn assistant_block_order_hidden_mode_text_then_summary_then_thinking() {
    let entries = vec![assistant_with_reordered_blocks_entry()];
    let mut options = test_render_options(ToolDisplayMode::Hidden);
    options.show_thinking = true;

    let rendered = render_parsed_conversation(&entries, &options);
    let text = rendered_text(&rendered);

    let pos_text = text.find("TEXT_BLOCK").expect("text block rendered");
    let pos_thinking = text.find("THINK_BLOCK").expect("thinking block rendered");
    // Hidden mode emits a tool-activity summary row instead of tool calls.
    let pos_summary = text
        .find("shell command")
        .or_else(|| text.find("ran 1"))
        .expect("tool summary row rendered");

    assert!(
        pos_text < pos_summary,
        "text must precede tool summary in Hidden mode: {}",
        text
    );
    assert!(
        pos_summary < pos_thinking,
        "thinking must follow tool summary in Hidden mode: {}",
        text
    );
}

#[test]
fn assistant_block_order_truncated_mode_text_then_tools_then_thinking() {
    let entries = vec![assistant_with_reordered_blocks_entry()];
    let mut options = test_render_options(ToolDisplayMode::Truncated);
    options.show_thinking = true;

    let rendered = render_parsed_conversation(&entries, &options);
    let text = rendered_text(&rendered);

    let pos_text = text.find("TEXT_BLOCK").expect("text block rendered");
    let pos_tool = text.find("Bash").expect("tool call rendered");
    let pos_thinking = text.find("THINK_BLOCK").expect("thinking block rendered");

    assert!(
        pos_text < pos_tool,
        "text must precede tool call in Truncated mode: {}",
        text
    );
    assert!(
        pos_tool < pos_thinking,
        "thinking must follow tool call in Truncated mode: {}",
        text
    );
}

#[test]
fn assistant_block_order_full_mode_text_then_tools_then_thinking() {
    let entries = vec![assistant_with_reordered_blocks_entry()];
    let mut options = test_render_options(ToolDisplayMode::Full);
    options.show_thinking = true;

    let rendered = render_parsed_conversation(&entries, &options);
    let text = rendered_text(&rendered);

    let pos_text = text.find("TEXT_BLOCK").expect("text block rendered");
    let pos_tool = text.find("Bash").expect("tool call rendered");
    let pos_thinking = text.find("THINK_BLOCK").expect("thinking block rendered");

    assert!(pos_text < pos_tool, "text must precede tool: {}", text);
    assert!(
        pos_tool < pos_thinking,
        "thinking must follow tool: {}",
        text
    );
}

#[test]
fn fixture_file_round_trip_renders_user_and_assistant() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fixture.jsonl");
    std::fs::write(
        &path,
        concat!(
            r#"{"type":"user","message":{"role":"user","content":"hello"}}"#,
            "\n",
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"world"}]}}"#,
            "\n",
        ),
    )
    .unwrap();

    let rendered =
        render_conversation(&path, &test_render_options(ToolDisplayMode::Hidden)).unwrap();
    let text = rendered_text(&rendered);
    assert!(text.contains("hello"));
    assert!(text.contains("world"));
    assert_eq!(rendered.messages.len(), 2);
}

// -----------------------------------------------------------------
// Postprocess pipeline unit tests
//
// These exercise the blank-line dedup and message-range remap pass
// directly, without going through the full render pipeline. They
// pin the exact behavior of the extracted helpers.
// -----------------------------------------------------------------

fn nonblank_line(text: &str) -> RenderedLine {
    RenderedLine::new(vec![(text.to_string(), LineStyle::default())])
}
fn blank_line() -> RenderedLine {
    RenderedLine::new(vec![])
}

#[test]
fn postprocess_collapses_runs_of_blanks_to_one() {
    let mut lines = vec![
        nonblank_line("a"),
        blank_line(),
        blank_line(),
        blank_line(),
        nonblank_line("b"),
    ];
    let mut messages = Vec::new();
    postprocess_blank_lines(&mut lines, &mut messages, std::iter::empty());

    assert_eq!(lines.len(), 3);
    assert!(!lines[0].spans.is_empty());
    assert!(lines[1].spans.is_empty());
    assert!(!lines[2].spans.is_empty());
}

#[test]
fn postprocess_remaps_range_spanning_removed_blank() {
    // Lines: 0=a, 1=blank, 2=blank (removed), 3=b
    // Range covers 0..3 (a + blank), should become 0..2 in compacted output.
    let mut lines = vec![
        nonblank_line("a"),
        blank_line(),
        blank_line(),
        nonblank_line("b"),
    ];
    let mut messages = vec![MessageRange {
        entry_index: 0,
        start_line: 0,
        end_line: 3,
    }];
    postprocess_blank_lines(&mut lines, &mut messages, std::iter::empty());

    assert_eq!(lines.len(), 3);
    assert_eq!(messages.len(), 1);
    // end_line walks back off the removed blank, which lands on the
    // surviving blank at original index 1 (new index 1), so end = 2.
    assert_eq!(messages[0].start_line, 0);
    assert_eq!(messages[0].end_line, 2);
}

#[test]
fn postprocess_clamps_range_ending_on_removed_blank() {
    // Range ends exactly on a removed blank.
    let mut lines = vec![
        nonblank_line("a"),
        blank_line(),
        blank_line(),
        nonblank_line("b"),
    ];
    let mut messages = vec![MessageRange {
        entry_index: 0,
        start_line: 0,
        end_line: 2,
    }];
    postprocess_blank_lines(&mut lines, &mut messages, std::iter::empty());

    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].start_line, 0);
    // last non-removed before end_line is 1 (kept blank); new index 1 + 1 = 2.
    // That is fine: it includes the kept blank.
    assert!(messages[0].end_line <= lines.len());
    assert!(messages[0].end_line > messages[0].start_line);
}

#[test]
fn postprocess_remaps_first_message_adjacent_to_removed_blank() {
    // Two messages back-to-back, with a doubled blank between them.
    let mut lines = vec![
        nonblank_line("first"),
        blank_line(),
        blank_line(),
        nonblank_line("second"),
        blank_line(),
    ];
    let mut messages = vec![
        MessageRange {
            entry_index: 0,
            start_line: 0,
            end_line: 1,
        },
        MessageRange {
            entry_index: 1,
            start_line: 3,
            end_line: 4,
        },
    ];
    postprocess_blank_lines(&mut lines, &mut messages, std::iter::empty());

    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0].start_line, 0);
    assert_eq!(messages[0].end_line, 1);
    // Second message should shift by 1 (one blank removed before it).
    assert_eq!(messages[1].start_line, 2);
    assert_eq!(messages[1].end_line, 3);
}

#[test]
fn postprocess_handles_trailing_blanks() {
    let mut lines = vec![nonblank_line("a"), blank_line(), blank_line(), blank_line()];
    let mut messages = vec![MessageRange {
        entry_index: 0,
        start_line: 0,
        end_line: 1,
    }];
    postprocess_blank_lines(&mut lines, &mut messages, std::iter::empty());

    // Two of the three trailing blanks collapse out.
    assert_eq!(lines.len(), 2);
    assert_eq!(messages[0].end_line, 1);
}

#[test]
fn postprocess_drops_empty_range_collapsed_to_zero() {
    // start_line == end_line after clamping → range removed.
    let mut lines = vec![nonblank_line("a"), blank_line()];
    let mut messages = vec![MessageRange {
        entry_index: 0,
        start_line: 1,
        end_line: 2,
    }];
    postprocess_blank_lines(&mut lines, &mut messages, std::iter::empty());

    // start_line was a kept blank at original index 1 → new index 1.
    // end_line maps to new_index[1] + 1 = 2, total_after = 2.
    // Range survives because 1 < 2.
    assert_eq!(messages.len(), 1);
    assert!(messages[0].start_line < messages[0].end_line);
}

#[test]
fn postprocess_remaps_call_areas_with_their_lines() {
    // Lines: 0=a, 1=blank, 2=blank (removed), 3=b
    let mut lines = vec![
        nonblank_line("a"),
        blank_line(),
        blank_line(),
        nonblank_line("b"),
    ];
    let id = make_tool_summary_output_id(0, None);
    let location = BlockLocation {
        entry_index: 0,
        block_index: 0,
    };
    let mut calls = [CallRange {
        input: CallArea {
            id: id.clone(),
            location,
            start_line: 0,
            end_line: 1,
        },
        result: Some(CallArea {
            id,
            location,
            start_line: 3,
            end_line: 4,
        }),
        lane: None,
    }];
    postprocess_blank_lines(&mut lines, &mut Vec::new(), calls.iter_mut());

    let call = &calls[0];
    assert_eq!((call.input.start_line, call.input.end_line), (0, 1));
    let result = call.result.as_ref().unwrap();
    assert_eq!((result.start_line, result.end_line), (2, 3));
}

#[test]
fn message_range_helper_excludes_trailing_blank() {
    let lines = vec![nonblank_line("hi"), blank_line()];
    let r = message_range_excluding_trailing_blank(&lines, 0, 2, 7).unwrap();
    assert_eq!(r.entry_index, 7);
    assert_eq!(r.start_line, 0);
    assert_eq!(r.end_line, 1);
}

#[test]
fn message_range_helper_returns_none_for_empty_slice() {
    let lines = vec![nonblank_line("hi")];
    assert!(message_range_excluding_trailing_blank(&lines, 1, 1, 0).is_none());
}

#[test]
fn message_range_helper_returns_none_when_only_trailing_blank() {
    let lines = vec![blank_line()];
    assert!(message_range_excluding_trailing_blank(&lines, 0, 1, 0).is_none());
}

// -----------------------------------------------------------------
// Pipeline tests for pending summary flush boundaries
// -----------------------------------------------------------------

#[test]
fn pending_summary_flushes_at_eof() {
    // A trailing tool-only assistant entry with no following user
    // result still flushes at EOF and produces a message range.
    let entries = vec![RenderableEntry {
        entry_index: 0,
        entry: claude_entry(
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_1","name":"Grep","input":{"pattern":"x"}}]}}"#,
        ),
    }];
    let rendered =
        render_parsed_conversation(&entries, &test_render_options(ToolDisplayMode::Hidden));

    let text = rendered_text(&rendered);
    assert!(text.contains("Searched for 1 pattern"));
    assert_eq!(rendered.messages.len(), 1);
    assert_eq!(rendered.messages[0].entry_index, 0);
}

#[test]
fn pending_summary_flushes_before_non_tool_message() {
    // Tool-only assistant followed by a user text message: the
    // pending summary must flush before the user message renders,
    // and both must produce distinct, non-overlapping ranges.
    let entries = vec![
        RenderableEntry {
            entry_index: 0,
            entry: claude_entry(
                r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_1","name":"Grep","input":{"pattern":"x"}}]}}"#,
            ),
        },
        user_entry(1, "follow up", None),
    ];
    let rendered =
        render_parsed_conversation(&entries, &test_render_options(ToolDisplayMode::Hidden));

    assert_eq!(rendered.messages.len(), 2);
    assert_eq!(rendered.messages[0].entry_index, 0);
    assert_eq!(rendered.messages[1].entry_index, 1);
    assert!(rendered.messages[0].end_line <= rendered.messages[1].start_line);
    let text = rendered_text(&rendered);
    assert!(text.contains("Searched for 1 pattern"));
    assert!(text.contains("follow up"));
}

fn tool_use_entry(
    entry_index: usize,
    tool_use_id: &str,
    name: &str,
    input: &str,
) -> RenderableEntry {
    let json = format!(
        r#"{{"type":"assistant","message":{{"role":"assistant","content":[{{"type":"tool_use","id":"{tool_use_id}","name":"{name}","input":{input}}}]}}}}"#
    );
    RenderableEntry {
        entry_index,
        entry: claude_entry(&json),
    }
}

fn tool_result_entry(entry_index: usize, tool_use_id: &str) -> RenderableEntry {
    tool_result_entry_holding(entry_index, tool_use_id, "ok")
}

/// A result whose `content` a test finds its rows by.
fn tool_result_entry_holding(
    entry_index: usize,
    tool_use_id: &str,
    content: &str,
) -> RenderableEntry {
    let json = format!(
        r#"{{"type":"user","message":{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"{tool_use_id}","content":"{content}"}}]}}}}"#
    );
    RenderableEntry {
        entry_index,
        entry: serde_json::from_str(&json).unwrap(),
    }
}

fn lines_tagged_with(rendered: &RenderedConversation, id: &ToolOutputId) -> usize {
    rendered
        .lines
        .iter()
        .filter(|line| line.tool_output_id.as_ref() == Some(id))
        .count()
}

#[test]
fn pending_summary_survives_entry_that_renders_nothing() {
    // Codex writes a usage entry after every tool result. It renders no
    // lines, so it must not end the run of tool calls around it.
    let entries = vec![
        tool_use_entry(0, "call_1", "exec", r#"{"command":"git status"}"#),
        tool_result_entry(1, "call_1"),
        RenderableEntry {
            entry_index: 2,
            entry: serde_json::from_str(
                r#"{"type":"pi-metadata","label":"Usage","text":"","searchable":false,"usage":{"input_tokens":12,"output_tokens":3}}"#,
            )
            .unwrap(),
        },
        tool_use_entry(3, "call_2", "exec", r#"{"command":"git diff"}"#),
        tool_result_entry(4, "call_2"),
    ];
    let rendered =
        render_parsed_conversation(&entries, &test_render_options(ToolDisplayMode::Hidden));

    let text = rendered_text(&rendered);
    assert!(text.contains("Called 2 tools"), "{text}");
    assert_eq!(
        lines_tagged_with(&rendered, &make_tool_summary_output_id(0, None)),
        1
    );
    assert_eq!(
        lines_tagged_with(&rendered, &make_tool_summary_output_id(3, None)),
        0
    );
    assert_eq!(rendered.messages.len(), 1);
    assert_eq!(rendered.messages[0].entry_index, 0);
}

#[test]
fn thinking_only_entry_splits_tool_run_only_when_thinking_is_shown() {
    let entries = vec![
        tool_use_entry(0, "toolu_1", "Grep", r#"{"pattern":"one"}"#),
        tool_result_entry(1, "toolu_1"),
        RenderableEntry {
            entry_index: 2,
            entry: serde_json::from_str(
                r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"thinking","thinking":"next step","signature":""}]}}"#,
            )
            .unwrap(),
        },
        tool_use_entry(3, "toolu_2", "Read", r#"{"file_path":"src/main.rs"}"#),
        tool_result_entry(4, "toolu_2"),
    ];

    let hidden =
        render_parsed_conversation(&entries, &test_render_options(ToolDisplayMode::Hidden));
    let hidden_text = rendered_text(&hidden);
    assert!(
        hidden_text.contains("Searched for 1 pattern, read 1 file"),
        "{hidden_text}"
    );
    assert!(!hidden_text.contains("next step"));
    assert_eq!(hidden.messages.len(), 1);

    let mut options = test_render_options(ToolDisplayMode::Hidden);
    options.show_thinking = true;
    let shown = render_parsed_conversation(&entries, &options);
    let shown_text = rendered_text(&shown);
    assert!(!shown_text.contains("Searched for 1 pattern, read 1 file"));
    assert!(
        shown_text.contains("Searched for 1 pattern"),
        "{shown_text}"
    );
    assert!(shown_text.contains("next step"), "{shown_text}");
    assert!(shown_text.contains("Read 1 file"), "{shown_text}");
    assert_eq!(
        lines_tagged_with(&shown, &make_tool_summary_output_id(0, None)),
        1
    );
    assert_eq!(
        lines_tagged_with(&shown, &make_tool_summary_output_id(3, None)),
        1
    );
    assert_eq!(shown.messages.len(), 3);
}

#[test]
fn consecutive_blank_lines_collapse_and_remap_ranges() {
    // Two adjacent user messages each emit a trailing blank; the dedup pass
    // collapses any double-blank that would arise from this sequence.
    let entries = vec![user_entry(0, "first", None), user_entry(1, "second", None)];
    let rendered =
        render_parsed_conversation(&entries, &test_render_options(ToolDisplayMode::Hidden));

    // No two consecutive empty lines should remain.
    for pair in rendered.lines.windows(2) {
        assert!(
            !(pair[0].spans.is_empty() && pair[1].spans.is_empty()),
            "consecutive empty lines should be collapsed"
        );
    }

    // Both user message ranges should still target valid, distinct lines.
    assert_eq!(rendered.messages.len(), 2);
    assert!(rendered.messages[0].end_line <= rendered.messages[1].start_line);
    assert!(rendered.messages[0].start_line < rendered.messages[0].end_line);
    assert!(rendered.messages[1].start_line < rendered.messages[1].end_line);
}

// -----------------------------------------------------------------
// Message renderer template ordering
//
// Pin the template-order contract: regardless of raw JSON block
// order, assistant messages render text first, then tool activity,
// then thinking. Skill-marker user messages render dimmed but
// remain top-level (not nested with the ↳ arrow).
// -----------------------------------------------------------------

fn first_index(text: &str, needle: &str) -> Option<usize> {
    text.find(needle)
}

#[test]
fn assistant_template_order_is_text_then_tool_then_thinking() {
    // Raw JSON order: thinking, tool_use, text. Template ordering
    // (text → tool → thinking) must override that.
    let entries = vec![RenderableEntry {
        entry_index: 0,
        entry: claude_entry(
            r#"{"type":"assistant","message":{"role":"assistant","content":[
                {"type":"thinking","thinking":"deep thought","signature":"sig"},
                {"type":"tool_use","id":"toolu_1","name":"Bash","input":{"command":"ls"}},
                {"type":"text","text":"text reply"}
            ]}}"#,
        ),
    }];
    let mut options = test_render_options(ToolDisplayMode::Truncated);
    options.show_thinking = true;
    let rendered = render_parsed_conversation(&entries, &options);
    let text = rendered_text(&rendered);

    let i_text = first_index(&text, "text reply").expect("text block rendered");
    let i_tool = first_index(&text, "Bash").expect("tool call rendered");
    let i_think = first_index(&text, "deep thought").expect("thinking rendered");

    assert!(i_text < i_tool, "text must precede tool call: {}", text);
    assert!(
        i_tool < i_think,
        "tool call must precede thinking: {}",
        text
    );
}

#[test]
fn skill_marker_user_message_renders_dimmed_but_top_level() {
    let entries = vec![RenderableEntry {
        entry_index: 0,
        entry: serde_json::from_str(
            r#"{"type":"user","message":{"role":"user","content":"Base directory for this skill: /tmp/x"}}"#,
        )
        .unwrap(),
    }];
    let rendered =
        render_parsed_conversation(&entries, &test_render_options(ToolDisplayMode::Hidden));
    let line = &rendered.lines[0];
    let name_text = format!("{:>width$}", "You", width = NAME_WIDTH);
    let style = line_style_at(line, &name_text);

    // Skill messages keep the "You" label (not ↳…), but render
    // dimmed without bold.
    assert!(style.dimmed);
    assert!(!style.bold);
}

#[test]
fn agent_progress_user_with_text_and_result_keeps_template_order() {
    // agent_progress user blocks aggregate text then render tool
    // results. Both must appear, in that order.
    let entries = vec![RenderableEntry {
        entry_index: 0,
        entry: serde_json::from_str(
            r#"{"type":"progress","data":{"type":"agent_progress","agentId":"agent-abc1234","message":{"type":"user","message":{"role":"user","content":[
                {"type":"text","text":"agent says hi"},
                {"type":"tool_result","tool_use_id":"toolu_1","content":"result body"}
            ]}}}}"#,
        )
        .unwrap(),
    }];
    let mut options = test_render_options(ToolDisplayMode::Truncated);
    options.show_thinking = true;
    let rendered = render_parsed_conversation(&entries, &options);
    let text = rendered_text(&rendered);

    let i_text = first_index(&text, "agent says hi").expect("text rendered");
    let i_result = first_index(&text, "result body").expect("result rendered");
    assert!(i_text < i_result, "text must precede result: {}", text);
}

#[test]
fn excluded_entry_kinds_produce_no_lines() {
    // Summary, system, custom-title, file-history, agent-name
    // entries are inert — they render nothing and do not produce
    // message ranges.
    let entries = vec![
        RenderableEntry {
            entry_index: 0,
            entry: serde_json::from_str(r#"{"type":"summary","summary":"x"}"#).unwrap(),
        },
        RenderableEntry {
            entry_index: 1,
            entry: serde_json::from_str(r#"{"type":"system","subtype":"info","message":"sys"}"#)
                .unwrap(),
        },
    ];
    let rendered =
        render_parsed_conversation(&entries, &test_render_options(ToolDisplayMode::Hidden));
    assert!(rendered.lines.is_empty());
    assert!(rendered.messages.is_empty());
}

mod task_reports {
    use super::*;
    use crate::history::task_notification::test_support::*;

    /// A user entry as Claude Code writes a task notification: string content
    /// holding the notification.
    fn task_report_entry(entry_index: usize, text: &str) -> RenderableEntry {
        let json = serde_json::json!({
            "type": "user",
            "timestamp": "2024-01-01T00:00:02Z",
            "message": {"role": "user", "content": text}
        });
        RenderableEntry {
            entry_index,
            entry: serde_json::from_value(json).unwrap(),
        }
    }

    fn report_body_id() -> ToolOutputId {
        make_tool_output_id(0, None, 0, ToolOutputKind::ToolResult, Some("task-report"))
    }

    /// A sub-agent that ran a command in the background receives the same
    /// notification; it renders as the sub-agent's dimmed text, whole, with
    /// nothing to toggle, not as a `Task` row.
    #[test]
    fn a_sub_agents_task_report_keeps_the_dimmed_path() {
        let json = serde_json::json!({
            "type": "user",
            "parent_tool_use_id": "toolu_parent",
            "message": {"role": "user", "content": AGENT_REPORT}
        });
        let entries = vec![RenderableEntry {
            entry_index: 0,
            entry: serde_json::from_value(json).unwrap(),
        }];
        let mut options = test_render_options(ToolDisplayMode::Truncated);
        options.show_thinking = true;

        let rendered = render_parsed_conversation(&entries, &options);

        let text = rendered_text(&rendered);
        let (label, label_style) = &rendered.lines[0].spans[0];
        assert_eq!(
            label.trim(),
            style::subagent_label("toolu_parent"),
            "{text}"
        );
        assert!(label_style.dimmed, "{text}");
        assert!(text.contains(AGENT_SUMMARY), "{text}");
        assert!(text.contains(AGENT_REPORT_LAST_LINE), "{text}");
        assert!(!text.contains("more lines"), "{text}");
        assert_no_wrapper_text(&text);
        assert!(rendered.lines.iter().all(|line| {
            line.tool_output_id.is_none()
                && line.spans.iter().skip(1).all(|(_, style)| style.dimmed)
        }));
    }

    const WRAPPER_FIELDS: [&str; 4] = ["<task-notification>", "task-id", "output-file", "<status>"];

    fn assert_no_wrapper_text(text: &str) {
        for field in WRAPPER_FIELDS {
            assert!(!text.contains(field), "{field} leaked into:\n{text}");
        }
    }

    #[test]
    fn a_background_command_renders_as_its_summary_alone() {
        let entries = vec![task_report_entry(0, BACKGROUND_COMMAND)];

        for mode in [
            ToolDisplayMode::Hidden,
            ToolDisplayMode::Truncated,
            ToolDisplayMode::Full,
        ] {
            let rendered = render_parsed_conversation(&entries, &test_render_options(mode));
            let text = rendered_text(&rendered);
            assert!(
                text.starts_with(&format!("     Task │ {BACKGROUND_COMMAND_SUMMARY}")),
                "{mode:?}:\n{text}"
            );
            assert!(!text.contains("more lines"), "{mode:?}:\n{text}");
            assert_no_wrapper_text(&text);
            assert_eq!(rendered.messages.len(), 1);
            assert!(
                !rendered.lines[0].clickable,
                "nothing to expand in {mode:?}"
            );
        }
    }

    #[test]
    fn an_agent_report_renders_its_summary_usage_and_a_truncated_body() {
        let entries = vec![task_report_entry(0, AGENT_REPORT)];

        for mode in [ToolDisplayMode::Hidden, ToolDisplayMode::Truncated] {
            let rendered = render_parsed_conversation(&entries, &test_render_options(mode));
            let text = rendered_text(&rendered);
            assert_eq!(
                line_text(&rendered.lines[0]),
                format!("     Task │ {AGENT_SUMMARY}")
            );
            assert_eq!(
                line_text(&rendered.lines[1]),
                format!("          │ {AGENT_USAGE_LINE}")
            );
            assert!(
                text.contains("Verified against source"),
                "{mode:?}:\n{text}"
            );
            assert!(!text.contains(AGENT_REPORT_LAST_LINE), "{mode:?}:\n{text}");
            assert!(text.contains("more lines...)"), "{mode:?}:\n{text}");
            assert_no_wrapper_text(&text);
            assert!(rendered.lines[0].clickable);
            assert_eq!(
                rendered.lines[0].tool_output_id.as_ref(),
                Some(&report_body_id())
            );
        }
    }

    #[test]
    fn an_agent_report_renders_whole_under_full_display_and_once_expanded() {
        let entries = vec![task_report_entry(0, AGENT_REPORT)];
        let mut expanded = test_render_options(ToolDisplayMode::Hidden);
        expanded.expanded_tool_outputs.insert(report_body_id());

        for (name, options) in [
            ("full", test_render_options(ToolDisplayMode::Full)),
            ("expanded", expanded),
        ] {
            let rendered = render_parsed_conversation(&entries, &options);
            let text = rendered_text(&rendered);
            assert!(text.contains(AGENT_USAGE_LINE), "{name}:\n{text}");
            assert!(text.contains(AGENT_REPORT_LAST_LINE), "{name}:\n{text}");
            assert!(!text.contains("more lines"), "{name}:\n{text}");
        }
        let full =
            render_parsed_conversation(&entries, &test_render_options(ToolDisplayMode::Full));
        assert!(
            full.lines.iter().all(|line| line.tool_output_id.is_none()),
            "full display has nothing to toggle"
        );
    }

    /// The rule beside every row of the report, the usage line and the
    /// `(N more lines...)` row included, is the rule beside a user's row;
    /// the label and the summary render undimmed.
    #[test]
    fn a_task_report_carries_a_plain_user_rows_rule_on_every_row() {
        let entries = vec![
            user_entry(0, "hello", None),
            task_report_entry(1, AGENT_REPORT),
        ];
        let rendered =
            render_parsed_conversation(&entries, &test_render_options(ToolDisplayMode::Truncated));
        let rule_of = |line: &RenderedLine| {
            line.spans
                .iter()
                .find(|(text, _)| text == " │ ")
                .map(|(_, style)| style.clone())
                .unwrap_or_else(|| panic!("no rule on {:?}", line_text(line)))
        };
        let user_rule = rule_of(&rendered.lines[rendered.messages[0].start_line]);
        assert_eq!(
            user_rule,
            LineStyle {
                fg: Some(th().border),
                ..Default::default()
            }
        );

        let block = &rendered.lines[rendered.messages[1].rows()];
        assert!(block.len() > 6, "{}", rendered_text(&rendered));
        for line in block {
            assert_eq!(rule_of(line), user_rule, "{:?}", line_text(line));
        }
        let (label, label_style) = &block[0].spans[0];
        assert_eq!(label.trim(), "Task");
        assert!(label_style.bold && !label_style.dimmed);
        let (summary, summary_style) = &block[0].spans[2];
        assert_eq!(summary, AGENT_SUMMARY);
        assert!(!summary_style.dimmed);
    }
}
