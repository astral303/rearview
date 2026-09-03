use super::semantic_test_helpers::*;
use super::*;
use crate::config::KeyBinding;
use chrono::TimeZone;
use std::cell::RefCell;

/// The session ID `tests/fixtures/codex/rollout.jsonl` states in its header.
const CODEX_SESSION_ID: &str = "019f0000-0000-7000-8000-00000000000a";

fn test_conversation(path: PathBuf, custom_title: Option<String>) -> Conversation {
    let mut full_text = "hello body".to_string();
    if let Some(title) = &custom_title {
        full_text = format!("{} {}", title, full_text);
    }
    Conversation {
        source: crate::history::Source::Claude,
        subagents: Vec::new(),
        session_id: path
            .file_stem()
            .and_then(|name| name.to_str())
            .unwrap_or_default()
            .to_owned(),
        path,
        index: 0,
        timestamp: Local.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap(),
        preview: "hello body".to_string(),
        preview_first: "hello body".to_string(),
        preview_last: "hello body".to_string(),
        search_text_lower: search::normalize_for_search(&full_text),
        semantic_turns: vec!["hello body".to_string()],
        semantic_turn_ranges: vec![crate::agent::refs::MessageRange::single(1)],
        full_text,
        agent_search_text: String::new(),
        semantic_route_text: String::new(),
        project_name: Some("project".to_string()),
        project_path: None,
        cwd: None,
        message_count: 1,
        assistant_messages: 1,
        parse_errors: Vec::new(),
        summary: None,
        custom_title,
        model: None,
        total_tokens: 0,
        duration_minutes: None,
    }
}

fn app_with_conversation(path: PathBuf, custom_title: Option<String>) -> App {
    App::new(
        vec![test_conversation(path, custom_title)],
        ToolDisplayMode::Hidden,
        false,
        KeyBindings::default(),
        vec![],
    )
}

fn write_conversation(path: &std::path::Path, title: Option<&str>) {
    let mut lines = vec![r#"{"type":"user","timestamp":"2024-01-01T00:00:00Z","message":{"role":"user","content":"hello body"}}"#.to_string()];
    if let Some(title) = title {
        lines.push(format!(
            r#"{{"type":"custom-title","customTitle":"{}","sessionId":"abc123"}}"#,
            title
        ));
    }
    std::fs::write(path, lines.join("\n") + "\n").unwrap();
}

fn write_named_conversation(path: &std::path::Path, text: &str) {
    let line = serde_json::json!({
        "type": "user",
        "timestamp": "2024-01-01T00:00:00Z",
        "message": {"role": "user", "content": text}
    })
    .to_string();
    std::fs::write(path, format!("{line}\n")).unwrap();
}

fn write_tool_conversation(path: &std::path::Path) {
    let line = r#"{"type":"assistant","timestamp":"2024-01-01T00:00:00Z","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_1","name":"Bash","input":{"command":"one\ntwo\nthree\nfour\nfive"}}]}}"#;
    std::fs::write(path, format!("{line}\n")).unwrap();
}

/// One assistant message of two calls, each with an input long enough to be
/// truncated.
fn write_two_truncated_calls_conversation(path: &std::path::Path) {
    let line = r#"{"type":"assistant","timestamp":"2024-01-01T00:00:00Z","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_1","name":"Bash","input":{"command":"one\ntwo\nthree\nfour\nfive"}},{"type":"tool_use","id":"toolu_2","name":"Bash","input":{"command":"six\nseven\neight\nnine\nten"}}]}}"#;
    std::fs::write(path, format!("{line}\n")).unwrap();
}

/// One assistant message of a call whose input fits under the line limit.
fn write_short_tool_conversation(path: &std::path::Path) {
    let line = r#"{"type":"assistant","timestamp":"2024-01-01T00:00:00Z","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_1","name":"Bash","input":{"command":"ls"}}]}}"#;
    std::fs::write(path, format!("{line}\n")).unwrap();
}

/// A user message, a run of three calls, and a closing user message. The
/// first call's input and the third call's result are long enough to be
/// truncated; the second call has nothing to expand.
fn write_tool_run_conversation(path: &std::path::Path) {
    let lines = [
        r#"{"type":"user","timestamp":"2024-01-01T00:00:00Z","message":{"role":"user","content":"intro"}}"#,
        r#"{"type":"assistant","timestamp":"2024-01-01T00:00:01Z","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_1","name":"Bash","input":{"command":"one\ntwo\nthree\nfour\nfive"}}]}}"#,
        r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"ok"}]}}"#,
        r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_2","name":"Read","input":{"file_path":"src/lib.rs"}}]}}"#,
        r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_2","content":"short"}]}}"#,
        r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_3","name":"Bash","input":{"command":"ls"}}]}}"#,
        r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_3","content":"r1\nr2\nr3\nr4\nr5\nr6"}]}}"#,
        r#"{"type":"user","timestamp":"2024-01-01T00:00:02Z","message":{"role":"user","content":"outro"}}"#,
    ];
    std::fs::write(path, lines.join("\n") + "\n").unwrap();
}

/// Eight one-row messages, each on an even row with a blank row between, so a
/// five-row viewport holds three of them.
fn write_short_message_conversation(path: &std::path::Path) {
    let lines: Vec<String> = (0..8)
        .map(|index| {
            serde_json::json!({
                "type": "user",
                "timestamp": "2024-01-01T00:00:00Z",
                "message": {"role": "user", "content": format!("message {index}")}
            })
            .to_string()
        })
        .collect();
    std::fs::write(path, lines.join("\n") + "\n").unwrap();
}

fn app_with_short_messages(dir: &tempfile::TempDir) -> App {
    let path = dir.path().join("short.jsonl");
    write_short_message_conversation(&path);
    app_with_tool_conversation(path, ToolDisplayMode::Hidden)
}

fn app_with_focused_tool_run(dir: &tempfile::TempDir) -> App {
    let path = dir.path().join("run.jsonl");
    write_tool_run_conversation(&path);
    let mut app = app_with_tool_conversation(path, ToolDisplayMode::Hidden);
    press(&mut app, KeyCode::Char('J'));
    assert_eq!(focused_message(&app), Some(1));
    app
}

/// A user message, one call whose input and result are both long enough to
/// be truncated, and a closing user message.
fn write_doubly_truncated_call_conversation(path: &std::path::Path) {
    let lines = [
        r#"{"type":"user","timestamp":"2024-01-01T00:00:00Z","message":{"role":"user","content":"intro"}}"#,
        r#"{"type":"assistant","timestamp":"2024-01-01T00:00:01Z","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_1","name":"Bash","input":{"command":"one\ntwo\nthree\nfour\nfive"}}]}}"#,
        r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"r1\nr2\nr3\nr4\nr5\nr6"}]}}"#,
        r#"{"type":"user","timestamp":"2024-01-01T00:00:02Z","message":{"role":"user","content":"outro"}}"#,
    ];
    std::fs::write(path, lines.join("\n") + "\n").unwrap();
}

/// The run of `write_doubly_truncated_call_conversation` expanded, with its
/// one call focused and both of its bodies still truncated.
fn app_with_focused_doubly_truncated_call(dir: &tempfile::TempDir) -> App {
    let path = dir.path().join("run.jsonl");
    write_doubly_truncated_call_conversation(&path);
    let mut app = app_with_tool_conversation(path, ToolDisplayMode::Hidden);
    press(&mut app, KeyCode::Char('J'));
    press(&mut app, KeyCode::Right);
    assert_eq!(stop(&app), (Some(1), Some(0)));
    assert_eq!(expanded_tool_count(&app), 1);
    assert!(!view_text(&app).contains("five"));
    assert!(!view_text(&app).contains("r6"));
    app
}

fn focused_message(app: &App) -> Option<usize> {
    if let AppMode::View(state) = app.app_mode() {
        state.focused_message()
    } else {
        unreachable!()
    }
}

fn focused_call(app: &App) -> Option<usize> {
    if let AppMode::View(state) = app.app_mode() {
        state.focused_call()
    } else {
        unreachable!()
    }
}

/// The focus as `]` and `[` see it: which message, and which of its calls.
fn stop(app: &App) -> (Option<usize>, Option<usize>) {
    (focused_message(app), focused_call(app))
}

fn scroll_offset(app: &App) -> usize {
    if let AppMode::View(state) = app.app_mode() {
        state.scroll_offset
    } else {
        unreachable!()
    }
}

fn message_start(app: &App, message: usize) -> usize {
    if let AppMode::View(state) = app.app_mode() {
        state.message_ranges[message].start_line
    } else {
        unreachable!()
    }
}

fn call_start(app: &App, call: usize) -> usize {
    if let AppMode::View(state) = app.app_mode() {
        state.call_ranges[call].input.start_line
    } else {
        unreachable!()
    }
}

/// Scroll to `row` one press at a time, so the focus sync runs for every row.
/// A press that does not move the offset fails instead of looping forever.
fn scroll_to(app: &mut App, row: usize, viewport_height: usize) {
    loop {
        let before = scroll_offset(app);
        let code = match before.cmp(&row) {
            std::cmp::Ordering::Equal => return,
            std::cmp::Ordering::Less => KeyCode::Down,
            std::cmp::Ordering::Greater => KeyCode::Up,
        };
        app.handle_key(code, KeyModifiers::empty(), viewport_height);
        assert_ne!(scroll_offset(app), before, "cannot scroll to row {row}");
    }
}

fn app_with_tool_conversation(path: PathBuf, tool_display: ToolDisplayMode) -> App {
    let mut app = App::new(
        vec![test_conversation(path, None)],
        tool_display,
        false,
        KeyBindings::default(),
        vec![],
    );
    app.selected = Some(0);
    app.enter_view_mode(80);
    app
}

/// A Codex rollout, whose file name puts a timestamp before the session ID.
fn write_codex_rollout(directory: &std::path::Path) -> PathBuf {
    let path = directory.join(format!(
        "rollout-2026-02-04T12-30-00-{CODEX_SESSION_ID}.jsonl"
    ));
    std::fs::copy(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/codex/rollout.jsonl"),
        &path,
    )
    .unwrap();
    path
}

fn press(app: &mut App, code: KeyCode) {
    app.handle_key(code, KeyModifiers::empty(), 17);
}

thread_local! {
    static COPIED_TEXT: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}

fn record_copied_text(text: &str) -> Result<ClipboardDestination, String> {
    COPIED_TEXT.with(|copied| copied.borrow_mut().push(text.to_owned()));
    Ok(ClipboardDestination::System)
}

/// Run `action` and return what the app copied during it. Every test on this
/// thread shares the one buffer, so each read starts by clearing it.
fn copied_during(app: &mut App, action: impl FnOnce(&mut App)) -> Vec<String> {
    COPIED_TEXT.with(|copied| copied.borrow_mut().clear());
    action(app);
    COPIED_TEXT.with(|copied| std::mem::take(&mut *copied.borrow_mut()))
}

fn copied_by(app: &mut App, code: KeyCode) -> Vec<String> {
    copied_during(app, |app| press(app, code))
}

fn expanded_tool_count(app: &App) -> usize {
    if let AppMode::View(state) = app.app_mode() {
        state.expanded_tool_outputs.len()
    } else {
        unreachable!()
    }
}

fn tool_click_row(app: &App, frame: Rect) -> u16 {
    let rows = if let AppMode::View(state) = app.app_mode() {
        0..state.rendered_lines.len()
    } else {
        unreachable!()
    };
    click_row_in(app, frame, rows)
}

/// The screen row of the first clickable line among `rows`.
fn click_row_in(app: &App, frame: Rect, rows: std::ops::Range<usize>) -> u16 {
    if let AppMode::View(state) = app.app_mode() {
        let layout = ui::view_layout_rects(frame, app, state);
        let idx = rows
            .into_iter()
            .find(|&row| state.rendered_lines[row].clickable)
            .unwrap();
        layout.content.y + (idx - state.scroll_offset) as u16
    } else {
        unreachable!()
    }
}

fn view_text(app: &App) -> String {
    if let AppMode::View(state) = app.app_mode() {
        state
            .rendered_lines
            .iter()
            .flat_map(|line| line.spans.iter().map(|(text, _)| text.as_str()))
            .collect()
    } else {
        unreachable!()
    }
}

fn view_expanded_tool_id(app: &App) -> ToolOutputId {
    if let AppMode::View(state) = app.app_mode() {
        assert_eq!(state.expanded_tool_outputs.len(), 1);
        state.expanded_tool_outputs.iter().next().unwrap().clone()
    } else {
        unreachable!()
    }
}

fn assert_cached_tool_output_survives_file_removal(
    app: &mut App,
    expect_hovered_tool_output_retained: bool,
) {
    let frame = Rect::new(0, 0, 120, 20);
    let row = tool_click_row(app, frame);

    assert!(app.handle_view_click(row, frame, 17));
    let expanded_id = if expect_hovered_tool_output_retained {
        Some(view_expanded_tool_id(app))
    } else {
        None
    };
    assert!(view_text(app).contains("five"));

    assert!(app.handle_view_click(row, frame, 17));
    if let AppMode::View(state) = app.app_mode() {
        assert!(state.expanded_tool_outputs.is_empty());
        if expect_hovered_tool_output_retained {
            assert_eq!(state.hovered_tool_output, expanded_id);
        }
    }
    assert!(!view_text(app).contains("five"));
}

#[test]
fn semantic_ranked_selection_opens_selected_conversation_and_returns() {
    let dir = tempfile::tempdir().unwrap();
    let first = dir.path().join("first.jsonl");
    let second = dir.path().join("second.jsonl");
    write_named_conversation(&first, "first body");
    write_named_conversation(&second, "second body");
    let mut app = App::new_with_options(
        vec![
            test_conversation(first.clone(), None),
            test_conversation(second.clone(), None),
        ],
        ToolDisplayMode::Hidden,
        false,
        KeyBindings::default(),
        vec![],
        TuiSearchOptions {
            default_mode: ListSearchMode::Semantic,
        },
    );
    let (_request_tx, _request_rx, response_tx) = connect_semantic_search_channels(&mut app);
    app.list_search_mode = ListSearchMode::Semantic;
    app.search_generation = 7;
    app.semantic_search.pending_generation = Some(7);

    send_semantic_complete_response(
        &response_tx,
        7,
        vec![1, 0],
        HashMap::from([(1, test_semantic_metadata(1, "second semantic preview"))]),
        SemanticProgress::Complete,
    );

    assert!(app.receive_search_results());
    assert_eq!(app.filtered(), &[1, 0]);
    assert_eq!(app.selected(), Some(0));
    app.enter_view_mode(80);
    assert!(matches!(app.app_mode(), AppMode::View(_)));
    if let AppMode::View(state) = app.app_mode() {
        assert_eq!(state.conversation_path, second);
    }
    assert!(view_text(&app).contains("second body"));

    app.exit_view_mode();

    assert!(matches!(app.app_mode(), AppMode::List));
    assert_eq!(app.filtered(), &[1, 0]);
    assert_eq!(app.selected(), Some(0));
}

#[test]
fn semantic_list_click_uses_three_line_rows() {
    let dir = tempfile::tempdir().unwrap();
    let first = dir.path().join("first.jsonl");
    let second = dir.path().join("second.jsonl");
    write_named_conversation(&first, "first body");
    write_named_conversation(&second, "second body");
    let mut app = App::new_with_options(
        vec![
            test_conversation(first, None),
            test_conversation(second, None),
        ],
        ToolDisplayMode::Hidden,
        false,
        KeyBindings::default(),
        vec![],
        TuiSearchOptions {
            default_mode: ListSearchMode::Semantic,
        },
    );
    let (_request_tx, request_rx) = mpsc::channel();
    let (response_tx, response_rx) = mpsc::channel();
    app.semantic_search.worker_tx = Some(_request_tx);
    app.semantic_search.worker_rx = Some(response_rx);
    app.list_search_mode = ListSearchMode::Semantic;
    app.query = "needle".to_string();
    app.cursor_pos = app.query.chars().count();
    app.search_generation = 7;
    app.semantic_search.pending_generation = Some(7);
    drop(request_rx);
    send_semantic_complete_response(
        &response_tx,
        7,
        vec![0, 1],
        HashMap::from([
            (0, test_semantic_metadata(0, "first")),
            (1, test_semantic_metadata(1, "second")),
        ]),
        SemanticProgress::Complete,
    );
    app.receive_search_results();
    let frame = Rect::new(0, 0, 80, 20);

    assert!(app.handle_list_click(6, frame));

    assert_eq!(app.selected(), Some(1));
}

fn widest_row(app: &App) -> usize {
    let AppMode::View(state) = app.app_mode() else {
        unreachable!()
    };
    state
        .rendered_lines
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|(text, _)| text.chars().count())
                .sum()
        })
        .max()
        .unwrap()
}

fn row_count(app: &App) -> usize {
    let AppMode::View(state) = app.app_mode() else {
        unreachable!()
    };
    state.rendered_lines.len()
}

#[test]
fn i_rewraps_the_rows_beside_the_timestamp_column_and_back() {
    const FRAME_WIDTH: usize = 60;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("long.jsonl");
    write_named_conversation(&path, &"word ".repeat(60));
    let mut app = app_with_conversation(path, None);
    app.selected = Some(0);
    app.enter_view_mode(FRAME_WIDTH);
    let rows_without_timing = row_count(&app);
    assert!(rows_without_timing >= 5, "{rows_without_timing} rows");
    assert!(widest_row(&app) + crate::tui::viewer::GUTTER_WIDTH <= FRAME_WIDTH);

    press(&mut app, KeyCode::Char('i'));
    assert!(widest_row(&app) + crate::tui::viewer::GUTTER_WIDTH <= FRAME_WIDTH);
    assert!(row_count(&app) > rows_without_timing);

    press(&mut app, KeyCode::Char('i'));
    assert_eq!(row_count(&app), rows_without_timing);
}

#[test]
fn view_click_toggles_clickable_output() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tool.jsonl");
    write_tool_conversation(&path);
    let mut app = app_with_tool_conversation(path, ToolDisplayMode::Truncated);
    let frame = Rect::new(0, 0, 120, 20);
    let row = tool_click_row(&app, frame);

    assert!(app.handle_view_click(row, frame, 17));
    let expanded_id = view_expanded_tool_id(&app);
    assert!(view_text(&app).contains("five"));

    assert!(app.handle_view_click(row, frame, 17));
    if let AppMode::View(state) = app.app_mode() {
        assert!(state.expanded_tool_outputs.is_empty());
        assert_eq!(state.hovered_tool_output, Some(expanded_id));
    }
}

#[test]
fn view_click_uses_cached_entries_after_file_removed() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tool.jsonl");
    write_tool_conversation(&path);
    let mut app = app_with_tool_conversation(path.clone(), ToolDisplayMode::Truncated);
    std::fs::remove_file(&path).unwrap();
    assert_cached_tool_output_survives_file_removal(&mut app, true);
}

#[test]
fn single_file_view_click_uses_cached_entries_after_file_removed() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tool.jsonl");
    write_tool_conversation(&path);
    let mut app = App::new_single_file(
        path.clone(),
        ToolDisplayMode::Truncated,
        false,
        KeyBindings::default(),
    );
    app.check_view_resize(80, 17);
    std::fs::remove_file(&path).unwrap();
    assert_cached_tool_output_survives_file_removal(&mut app, false);
}

#[test]
fn view_hover_tracks_clickable_output() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tool.jsonl");
    write_tool_conversation(&path);
    let mut app = app_with_tool_conversation(path, ToolDisplayMode::Truncated);
    let frame = Rect::new(0, 0, 120, 20);
    let (row, id) = if let AppMode::View(state) = app.app_mode() {
        let layout = ui::view_layout_rects(frame, &app, state);
        let idx = state
            .rendered_lines
            .iter()
            .position(|line| line.clickable)
            .unwrap();
        let id = state.rendered_lines[idx].tool_output_id.clone().unwrap();
        (layout.content.y + (idx - state.scroll_offset) as u16, id)
    } else {
        unreachable!()
    };

    assert!(app.handle_view_mouse_move(row, frame));
    if let AppMode::View(state) = app.app_mode() {
        assert_eq!(state.hovered_tool_output, Some(id));
    }
    assert!(app.handle_view_mouse_move(0, frame));
    if let AppMode::View(state) = app.app_mode() {
        assert_eq!(state.hovered_tool_output, None);
    }
}

#[test]
fn enter_expands_and_collapses_the_focused_tool_run() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tool.jsonl");
    write_tool_conversation(&path);
    let mut app = app_with_tool_conversation(path, ToolDisplayMode::Hidden);
    press(&mut app, KeyCode::Char('J'));

    press(&mut app, KeyCode::Enter);
    assert_eq!(expanded_tool_count(&app), 1);
    assert!(view_text(&app).contains("(expanded):"));
    assert!(view_text(&app).contains("one"));

    press(&mut app, KeyCode::Enter);
    assert_eq!(expanded_tool_count(&app), 0);
    assert!(!view_text(&app).contains("(expanded):"));
}

#[test]
fn enter_without_message_navigation_does_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tool.jsonl");
    write_tool_conversation(&path);
    let mut app = app_with_tool_conversation(path, ToolDisplayMode::Hidden);

    press(&mut app, KeyCode::Enter);

    assert_eq!(expanded_tool_count(&app), 0);
}

#[test]
fn enter_on_a_message_without_a_tool_run_does_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("plain.jsonl");
    write_conversation(&path, None);
    let mut app = app_with_conversation(path, None);
    app.selected = Some(0);
    app.enter_view_mode(80);
    press(&mut app, KeyCode::Char('J'));

    press(&mut app, KeyCode::Enter);

    assert_eq!(expanded_tool_count(&app), 0);
}

#[test]
fn enter_in_truncated_mode_expands_and_collapses_the_messages_truncated_bodies() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tool.jsonl");
    write_two_truncated_calls_conversation(&path);
    let mut app = app_with_tool_conversation(path, ToolDisplayMode::Truncated);
    press(&mut app, KeyCode::Char('J'));

    press(&mut app, KeyCode::Enter);
    assert_eq!(expanded_tool_count(&app), 2);
    assert!(view_text(&app).contains("five"));
    assert!(view_text(&app).contains("ten"));

    press(&mut app, KeyCode::Enter);
    assert_eq!(expanded_tool_count(&app), 0);
    assert!(!view_text(&app).contains("five"));
    assert!(!view_text(&app).contains("ten"));
}

#[test]
fn enter_in_truncated_mode_expands_the_bodies_still_collapsed_before_collapsing_any() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tool.jsonl");
    write_two_truncated_calls_conversation(&path);
    let mut app = app_with_tool_conversation(path, ToolDisplayMode::Truncated);
    let frame = Rect::new(0, 0, 120, 20);
    assert!(app.handle_view_click(tool_click_row(&app, frame), frame, 17));
    assert_eq!(expanded_tool_count(&app), 1);

    press(&mut app, KeyCode::Enter);

    assert_eq!(expanded_tool_count(&app), 2);
    assert!(view_text(&app).contains("ten"));
}

#[test]
fn enter_while_typing_a_search_commits_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tool.jsonl");
    write_tool_conversation(&path);
    let mut app = app_with_tool_conversation(path, ToolDisplayMode::Hidden);
    press(&mut app, KeyCode::Char('/'));
    for c in "Ran".chars() {
        press(&mut app, KeyCode::Char(c));
    }

    press(&mut app, KeyCode::Enter);

    if let AppMode::View(state) = app.app_mode() {
        assert_eq!(state.search_mode, ViewSearchMode::Active);
    }
    assert_eq!(expanded_tool_count(&app), 0);
}

#[test]
fn right_arrow_expands_the_focused_run_and_focuses_its_first_call() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_with_focused_tool_run(&dir);

    press(&mut app, KeyCode::Right);

    assert_eq!(expanded_tool_count(&app), 1);
    assert!(view_text(&app).contains("(expanded):"));
    assert_eq!(focused_call(&app), Some(0));
}

#[test]
fn right_arrow_on_an_expanded_run_focuses_its_first_call() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_with_focused_tool_run(&dir);
    press(&mut app, KeyCode::Enter);
    assert_eq!(focused_call(&app), None);

    press(&mut app, KeyCode::Right);

    assert_eq!(expanded_tool_count(&app), 1);
    assert_eq!(focused_call(&app), Some(0));
}

#[test]
fn right_arrow_expands_a_calls_truncated_input_and_then_does_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_with_focused_tool_run(&dir);
    press(&mut app, KeyCode::Right);
    assert!(!view_text(&app).contains("five"));

    press(&mut app, KeyCode::Right);
    assert_eq!(expanded_tool_count(&app), 2);
    assert!(view_text(&app).contains("five"));
    assert_eq!(focused_call(&app), Some(0));

    press(&mut app, KeyCode::Right);
    assert_eq!(expanded_tool_count(&app), 2);
    assert_eq!(focused_call(&app), Some(0));
}

#[test]
fn right_arrow_on_a_call_with_nothing_to_expand_does_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_with_focused_tool_run(&dir);
    press(&mut app, KeyCode::Right);
    press(&mut app, KeyCode::Char('J'));
    assert_eq!(focused_call(&app), Some(1));

    press(&mut app, KeyCode::Right);

    assert_eq!(expanded_tool_count(&app), 1);
    assert_eq!(focused_call(&app), Some(1));
}

#[test]
fn left_arrow_collapses_the_expanded_area_then_leaves_the_call_then_collapses_the_run() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_with_focused_tool_run(&dir);
    press(&mut app, KeyCode::Right);
    press(&mut app, KeyCode::Right);
    assert_eq!(expanded_tool_count(&app), 2);

    press(&mut app, KeyCode::Left);
    assert_eq!(expanded_tool_count(&app), 1);
    assert!(!view_text(&app).contains("five"));
    assert_eq!(focused_call(&app), Some(0));

    press(&mut app, KeyCode::Left);
    assert_eq!(expanded_tool_count(&app), 1);
    assert_eq!(focused_call(&app), None);
    assert_eq!(focused_message(&app), Some(1));

    press(&mut app, KeyCode::Left);
    assert_eq!(expanded_tool_count(&app), 0);
    assert!(!view_text(&app).contains("(expanded):"));

    press(&mut app, KeyCode::Left);
    assert_eq!(expanded_tool_count(&app), 0);
    assert_eq!(focused_message(&app), Some(1));
}

#[test]
fn enter_on_a_call_whose_input_has_nothing_to_expand_toggles_its_result() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_with_focused_tool_run(&dir);
    press(&mut app, KeyCode::Right);
    press(&mut app, KeyCode::Char('J'));
    press(&mut app, KeyCode::Char('J'));
    assert_eq!(focused_call(&app), Some(2));
    assert!(!view_text(&app).contains("r6"));

    press(&mut app, KeyCode::Enter);
    assert_eq!(expanded_tool_count(&app), 2);
    assert!(view_text(&app).contains("r6"));

    press(&mut app, KeyCode::Enter);
    assert_eq!(expanded_tool_count(&app), 1);
    assert!(!view_text(&app).contains("r6"));
    assert_eq!(focused_call(&app), Some(2));
}

#[test]
fn enter_on_a_call_whose_result_has_nothing_to_expand_toggles_its_input() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_with_focused_tool_run(&dir);
    press(&mut app, KeyCode::Right);

    press(&mut app, KeyCode::Enter);

    assert_eq!(expanded_tool_count(&app), 2);
    assert!(view_text(&app).contains("five"));
}

#[test]
fn right_arrow_on_a_call_with_input_and_result_truncated_expands_both_together() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_with_focused_doubly_truncated_call(&dir);

    press(&mut app, KeyCode::Right);
    assert_eq!(expanded_tool_count(&app), 3);
    assert!(view_text(&app).contains("five"));
    assert!(view_text(&app).contains("r6"));
    assert_eq!(stop(&app), (Some(1), Some(0)));

    press(&mut app, KeyCode::Right);
    assert_eq!(expanded_tool_count(&app), 3);
    assert_eq!(stop(&app), (Some(1), Some(0)));
}

#[test]
fn left_arrow_on_a_call_with_input_and_result_expanded_collapses_both_then_leaves_the_call() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_with_focused_doubly_truncated_call(&dir);
    press(&mut app, KeyCode::Right);
    assert_eq!(expanded_tool_count(&app), 3);

    press(&mut app, KeyCode::Left);
    assert_eq!(expanded_tool_count(&app), 1);
    assert!(!view_text(&app).contains("five"));
    assert!(!view_text(&app).contains("r6"));
    assert_eq!(stop(&app), (Some(1), Some(0)));

    press(&mut app, KeyCode::Left);
    assert_eq!(expanded_tool_count(&app), 1);
    assert_eq!(stop(&app), (Some(1), None));
}

#[test]
fn enter_on_a_call_with_input_and_result_truncated_expands_and_collapses_both() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_with_focused_doubly_truncated_call(&dir);

    press(&mut app, KeyCode::Enter);
    assert_eq!(expanded_tool_count(&app), 3);
    assert!(view_text(&app).contains("five"));
    assert!(view_text(&app).contains("r6"));

    press(&mut app, KeyCode::Enter);
    assert_eq!(expanded_tool_count(&app), 1);
    assert!(!view_text(&app).contains("five"));
    assert!(!view_text(&app).contains("r6"));
    assert_eq!(stop(&app), (Some(1), Some(0)));
}

#[test]
fn enter_on_a_call_expands_the_body_still_collapsed_before_collapsing_any() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_with_focused_doubly_truncated_call(&dir);
    let frame = Rect::new(0, 0, 120, 20);
    let input_rows = if let AppMode::View(state) = app.app_mode() {
        let input = &state.call_ranges[0].input;
        input.start_line..input.end_line
    } else {
        unreachable!()
    };
    assert!(app.handle_view_click(click_row_in(&app, frame, input_rows), frame, 17));
    assert!(view_text(&app).contains("five"));
    assert!(!view_text(&app).contains("r6"));
    press(&mut app, KeyCode::Right);
    assert_eq!(stop(&app), (Some(1), Some(0)));

    press(&mut app, KeyCode::Enter);

    assert_eq!(expanded_tool_count(&app), 3);
    assert!(view_text(&app).contains("r6"));
}

#[test]
fn j_and_k_walk_a_runs_calls_and_step_out_at_its_ends() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_with_focused_tool_run(&dir);
    press(&mut app, KeyCode::Right);
    assert_eq!(focused_call(&app), Some(0));

    press(&mut app, KeyCode::Char('J'));
    assert_eq!(focused_call(&app), Some(1));
    press(&mut app, KeyCode::Char(']'));
    assert_eq!(focused_call(&app), Some(2));

    press(&mut app, KeyCode::Char('J'));
    assert_eq!(stop(&app), (Some(2), None));

    press(&mut app, KeyCode::Char('['));
    assert_eq!(stop(&app), (Some(1), Some(2)));
}

#[test]
fn j_from_the_message_above_lands_on_a_runs_first_call() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_with_focused_tool_run(&dir);
    press(&mut app, KeyCode::Right);
    press(&mut app, KeyCode::Char('K'));
    assert_eq!(stop(&app), (Some(0), None));

    press(&mut app, KeyCode::Char('J'));

    assert_eq!(stop(&app), (Some(1), Some(0)));
}

#[test]
fn k_at_a_runs_first_call_leaves_for_the_message_above() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_with_focused_tool_run(&dir);
    press(&mut app, KeyCode::Right);
    assert_eq!(stop(&app), (Some(1), Some(0)));

    press(&mut app, KeyCode::Char('K'));

    assert_eq!(stop(&app), (Some(0), None));
}

#[test]
fn left_then_j_leaves_the_run() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_with_focused_tool_run(&dir);
    press(&mut app, KeyCode::Right);
    press(&mut app, KeyCode::Left);
    assert_eq!(stop(&app), (Some(1), None));

    press(&mut app, KeyCode::Char('J'));

    assert_eq!(stop(&app), (Some(2), None));
    assert!(view_text(&app).contains("(expanded):"));
}

#[test]
fn every_step_forward_through_a_run_is_undone_by_one_step_back() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_with_focused_tool_run(&dir);
    press(&mut app, KeyCode::Right);
    press(&mut app, KeyCode::Char('K'));
    assert_eq!(stop(&app), (Some(0), None));

    let mut forward = vec![stop(&app)];
    for _ in 0..4 {
        press(&mut app, KeyCode::Char('J'));
        forward.push(stop(&app));
    }
    assert_eq!(forward.last(), Some(&(Some(2), None)), "{forward:?}");

    let mut backward = vec![stop(&app)];
    for _ in 0..4 {
        press(&mut app, KeyCode::Char('K'));
        backward.push(stop(&app));
    }
    backward.reverse();

    assert_eq!(forward, backward);
}

#[test]
fn arrows_without_message_navigation_do_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("run.jsonl");
    write_tool_run_conversation(&path);
    let mut app = app_with_tool_conversation(path, ToolDisplayMode::Hidden);

    press(&mut app, KeyCode::Right);
    press(&mut app, KeyCode::Left);

    assert_eq!(expanded_tool_count(&app), 0);
    assert_eq!(focused_call(&app), None);
}

#[test]
fn y_on_a_call_copies_its_header_full_input_and_result() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_with_focused_tool_run(&dir);
    app.set_clipboard_writer_for_test(record_copied_text);
    press(&mut app, KeyCode::Right);

    let copied = copied_by(&mut app, KeyCode::Char('y'));

    let [first] = copied.as_slice() else {
        panic!("y copied {copied:?}, not one text");
    };
    assert!(first.starts_with("Bash: one"), "{first}");
    assert!(first.contains("five"), "{first}");
    assert!(first.ends_with("\n\nok"), "{first}");
    assert_eq!(
        app.status_message.as_ref().map(|(text, _)| text.as_str()),
        Some("Call copied to clipboard")
    );

    press(&mut app, KeyCode::Char('J'));
    press(&mut app, KeyCode::Char('J'));

    assert_eq!(
        copied_by(&mut app, KeyCode::Char('y')),
        vec!["Bash: ls\n\nr1\nr2\nr3\nr4\nr5\nr6".to_string()]
    );
}

#[test]
fn y_with_no_call_focused_copies_the_message() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_with_focused_tool_run(&dir);
    app.set_clipboard_writer_for_test(record_copied_text);
    press(&mut app, KeyCode::Char('K'));

    let copied = copied_by(&mut app, KeyCode::Char('y'));

    assert_eq!(copied, vec!["intro".to_string()]);
    assert_eq!(
        app.status_message.as_ref().map(|(text, _)| text.as_str()),
        Some("Message copied to clipboard")
    );
}

#[test]
fn a_click_clears_call_focus() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_with_focused_tool_run(&dir);
    press(&mut app, KeyCode::Right);
    let frame = Rect::new(0, 0, 120, 20);
    let row = tool_click_row(&app, frame);

    assert!(app.handle_view_click(row, frame, 17));

    assert_eq!(focused_call(&app), None);
}

#[test]
fn leaving_summary_mode_clears_call_focus() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_with_focused_tool_run(&dir);
    press(&mut app, KeyCode::Right);

    press(&mut app, KeyCode::Char('t'));

    assert_eq!(focused_call(&app), None);
}

#[test]
fn a_scroll_that_moves_message_focus_clears_call_focus() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_with_focused_tool_run(&dir);
    press(&mut app, KeyCode::Right);

    app.handle_key(KeyCode::Char('G'), KeyModifiers::empty(), SHORT_VIEWPORT);

    assert_eq!(focused_message(&app), Some(2));
    assert_eq!(focused_call(&app), None);
}

/// A viewport too short to hold the three-call run, so scrolling has to move
/// the focus through it.
const SHORT_VIEWPORT: usize = 3;

#[test]
fn scrolling_through_an_expanded_run_moves_focus_call_by_call() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_with_focused_tool_run(&dir);
    press(&mut app, KeyCode::Right);
    assert_eq!(focused_call(&app), Some(0));

    let second = call_start(&app, 1);
    scroll_to(&mut app, second, SHORT_VIEWPORT);
    assert_eq!(focused_call(&app), Some(1));

    let third = call_start(&app, 2);
    scroll_to(&mut app, third, SHORT_VIEWPORT);
    assert_eq!(focused_call(&app), Some(2));

    let first = call_start(&app, 0);
    scroll_to(&mut app, first, SHORT_VIEWPORT);
    assert_eq!(focused_call(&app), Some(0));
}

/// Tall enough to hold two of the run's calls, so a scroll that leaves the
/// focused one on screen has an earlier call to fall back to.
const TWO_CALL_VIEWPORT: usize = 8;

#[test]
fn a_scroll_that_leaves_the_focused_call_on_screen_keeps_it_focused() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_with_focused_tool_run(&dir);
    press(&mut app, KeyCode::Right);
    let second = call_start(&app, 1);
    scroll_to(&mut app, second, TWO_CALL_VIEWPORT);
    app.handle_key(KeyCode::Char(']'), KeyModifiers::empty(), TWO_CALL_VIEWPORT);
    assert_eq!(stop(&app), (Some(1), Some(2)));

    app.handle_key(KeyCode::Down, KeyModifiers::empty(), TWO_CALL_VIEWPORT);

    assert_eq!(stop(&app), (Some(1), Some(2)));
}

#[test]
fn scrolling_a_runs_calls_off_the_bottom_keeps_the_run_focused() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_with_focused_tool_run(&dir);
    press(&mut app, KeyCode::Right);
    let first = call_start(&app, 0);
    scroll_to(&mut app, first, SHORT_VIEWPORT);
    assert_eq!(stop(&app), (Some(1), Some(0)));

    scroll_to(&mut app, 0, SHORT_VIEWPORT);

    assert_eq!(stop(&app), (Some(1), None));
}

/// Holds three of the short fixture's messages.
const PAGE_VIEWPORT: usize = 5;

#[test]
fn paging_down_past_the_focused_message_focuses_the_top_of_the_screen() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_with_short_messages(&dir);
    for _ in 0..2 {
        app.handle_key(KeyCode::Char(']'), KeyModifiers::empty(), PAGE_VIEWPORT);
    }
    assert_eq!(focused_message(&app), Some(2));

    for _ in 0..2 {
        app.handle_key(KeyCode::PageDown, KeyModifiers::empty(), PAGE_VIEWPORT);
    }

    assert_eq!(focused_message(&app), Some(5));
    assert_eq!(message_start(&app, 5), scroll_offset(&app));
}

#[test]
fn paging_up_past_the_focused_message_focuses_the_bottom_of_the_screen() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_with_short_messages(&dir);
    for _ in 0..6 {
        app.handle_key(KeyCode::Char(']'), KeyModifiers::empty(), PAGE_VIEWPORT);
    }
    assert_eq!(focused_message(&app), Some(6));

    for _ in 0..2 {
        app.handle_key(KeyCode::PageUp, KeyModifiers::empty(), PAGE_VIEWPORT);
    }

    assert_eq!(focused_message(&app), Some(2));
    let first_row_below_screen = scroll_offset(&app) + PAGE_VIEWPORT;
    assert!(message_start(&app, 3) >= first_row_below_screen);
}

#[test]
fn scrolling_back_into_a_run_focuses_the_call_on_screen_not_the_run() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_with_focused_tool_run(&dir);
    press(&mut app, KeyCode::Right);
    app.handle_key(KeyCode::Char('G'), KeyModifiers::empty(), SHORT_VIEWPORT);
    assert_eq!(focused_call(&app), None);

    let third = call_start(&app, 2);
    scroll_to(&mut app, third, SHORT_VIEWPORT);

    assert_eq!(focused_message(&app), Some(1));
    assert_eq!(focused_call(&app), Some(2));
}

#[test]
fn scrolling_over_a_collapsed_run_never_focuses_a_call() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_with_focused_tool_run(&dir);

    for _ in 0..12 {
        app.handle_key(KeyCode::Down, KeyModifiers::empty(), SHORT_VIEWPORT);
        assert_eq!(focused_call(&app), None);
    }
}

#[test]
fn cancel_rename_keeps_existing_title() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("abc123.jsonl");
    write_conversation(&path, Some("old"));
    let mut app = app_with_conversation(path, Some("old".to_string()));

    app.start_rename();
    assert!(matches!(app.dialog_mode, DialogMode::Rename { .. }));
    app.handle_rename_key(KeyCode::Esc, KeyModifiers::empty());

    assert_eq!(app.conversations[0].custom_title, Some("old".to_string()));
    assert_eq!(app.dialog_mode, DialogMode::None);
}

#[test]
fn configured_rename_key_starts_rename() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("abc123.jsonl");
    write_conversation(&path, None);
    let keys = KeyBindings {
        rename: KeyBinding {
            code: KeyCode::Char('t'),
            modifiers: KeyModifiers::CONTROL,
        },
        ..Default::default()
    };
    let mut app = App::new(
        vec![test_conversation(path, None)],
        ToolDisplayMode::Hidden,
        false,
        keys,
        vec![],
    );

    app.handle_key(KeyCode::Char('t'), KeyModifiers::CONTROL, 10);

    assert!(matches!(app.dialog_mode, DialogMode::Rename { .. }));
}

#[test]
fn bare_r_remains_search_input() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("abc123.jsonl");
    write_conversation(&path, None);
    let mut app = app_with_conversation(path, None);

    app.handle_key(KeyCode::Char('r'), KeyModifiers::empty(), 10);

    assert_eq!(app.query(), "r");
    assert_eq!(app.dialog_mode, DialogMode::None);
}

#[test]
fn submit_rename_reparses_and_updates_search_index() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("abc123.jsonl");
    write_conversation(&path, Some("old"));
    let mut app = app_with_conversation(path.clone(), Some("old".to_string()));

    app.start_rename();
    app.handle_rename_key(KeyCode::Char('u'), KeyModifiers::CONTROL);
    app.handle_rename_key(KeyCode::Char('n'), KeyModifiers::empty());
    app.handle_rename_key(KeyCode::Char('e'), KeyModifiers::empty());
    app.handle_rename_key(KeyCode::Char('w'), KeyModifiers::empty());
    app.handle_rename_key(KeyCode::Enter, KeyModifiers::empty());

    assert_eq!(app.conversations[0].custom_title, Some("new".to_string()));
    assert!(search::search(&app.conversations, &app.searchable, "new", Local::now()).contains(&0));
    assert!(search::search(&app.conversations, &app.searchable, "old", Local::now()).is_empty());
}

#[test]
fn submit_rename_preserves_selected_path() {
    let dir = tempfile::tempdir().unwrap();
    let first = dir.path().join("first.jsonl");
    let second = dir.path().join("second.jsonl");
    write_conversation(&first, None);
    write_conversation(&second, None);
    let mut app = App::new(
        vec![
            test_conversation(first, None),
            test_conversation(second.clone(), None),
        ],
        ToolDisplayMode::Hidden,
        false,
        KeyBindings::default(),
        vec![],
    );
    app.selected = Some(1);

    app.start_rename();
    app.handle_rename_key(KeyCode::Char('n'), KeyModifiers::empty());
    app.handle_rename_key(KeyCode::Enter, KeyModifiers::empty());

    assert_eq!(app.get_selected_path().as_deref(), Some(second.as_path()));
}

#[test]
fn submit_empty_rename_clears_searchable_title() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("abc123.jsonl");
    write_conversation(&path, Some("old"));
    let mut app = app_with_conversation(path.clone(), Some("old".to_string()));

    app.start_rename();
    app.handle_rename_key(KeyCode::Char('u'), KeyModifiers::CONTROL);
    app.handle_rename_key(KeyCode::Enter, KeyModifiers::empty());

    assert_eq!(app.conversations[0].custom_title, None);
    assert!(search::search(&app.conversations, &app.searchable, "old", Local::now()).is_empty());
}

fn app_with_codex_conversation(dir: &tempfile::TempDir) -> App {
    let path = write_codex_rollout(dir.path());
    let conversation = Conversation {
        source: crate::history::Source::Codex,
        session_id: CODEX_SESSION_ID.to_string(),
        ..test_conversation(path, None)
    };
    App::new(
        vec![conversation],
        ToolDisplayMode::Hidden,
        false,
        KeyBindings::default(),
        vec![],
    )
}

/// A Codex rollout is named for a timestamp and its thread id together, so a
/// pasted id matches no file name.
#[test]
fn a_pasted_session_id_selects_the_conversation_that_states_it() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_with_codex_conversation(&dir);

    app.set_query_for_test(CODEX_SESSION_ID);
    app.update_filter();

    assert_eq!(app.filtered(), [0]);
    assert!(app.is_session_id_query());
    assert_eq!(app.unresolved_session_id(), None);
}

#[test]
fn a_session_id_no_agent_stores_empties_the_list_and_names_the_id() {
    const ABSENT: &str = "019f0000-0000-7000-8000-0000000000ff";
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_with_codex_conversation(&dir);

    app.set_query_for_test(ABSENT);
    app.update_filter();

    assert!(app.filtered().is_empty());
    assert!(app.is_session_id_query());
    assert_eq!(app.unresolved_session_id(), Some(ABSENT));
}

/// One character short of an id, the query is text, so the list must not
/// mark it as a session id.
#[test]
fn an_id_one_character_short_is_a_text_search() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_with_codex_conversation(&dir);

    app.set_query_for_test(&CODEX_SESSION_ID[..CODEX_SESSION_ID.len() - 1]);
    app.update_filter();

    assert!(!app.is_session_id_query());
    assert_eq!(app.unresolved_session_id(), None);
}

/// The documented way to reach a transcript that quotes an id: the lookup must
/// let a quoted query through to ordinary search.
#[test]
fn a_quoted_session_id_searches_transcripts_instead() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_codex_rollout(dir.path());
    let mut conversation = Conversation {
        source: crate::history::Source::Codex,
        session_id: "019f0000-0000-7000-8000-0000000000fe".to_string(),
        ..test_conversation(path, None)
    };
    conversation.full_text = format!("resumed {CODEX_SESSION_ID} yesterday");
    conversation.search_text_lower = search::normalize_for_search(&conversation.full_text);
    let mut app = App::new(
        vec![conversation],
        ToolDisplayMode::Hidden,
        false,
        KeyBindings::default(),
        vec![],
    );

    app.set_query_for_test(&format!("\"{CODEX_SESSION_ID}\""));
    app.update_filter();

    assert_eq!(app.filtered(), [0]);
    assert!(!app.is_session_id_query());
}

#[test]
fn clearing_the_query_clears_the_unresolved_session_id() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_with_codex_conversation(&dir);
    app.set_query_for_test("019f0000-0000-7000-8000-0000000000ff");
    app.update_filter();
    assert!(app.unresolved_session_id().is_some());

    app.set_query_for_test("");
    app.update_filter();

    assert!(!app.is_session_id_query());
    assert_eq!(app.unresolved_session_id(), None);
    assert_eq!(app.filtered(), [0]);
}

#[test]
fn ctrl_l_lists_the_filters_the_load_ran_under() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_with_codex_conversation(&dir);
    app.set_active_filters(vec![crate::history::FilterTerm::new(
        "since",
        "2026-08-17 13:45",
    )]);

    app.handle_key(KeyCode::Char('l'), KeyModifiers::CONTROL, 17);
    assert_eq!(*app.dialog_mode(), DialogMode::ActiveFilters);

    press(&mut app, KeyCode::Esc);
    assert_eq!(*app.dialog_mode(), DialogMode::None);
}

/// An unfiltered load has nothing to list, and the key must not steal a
/// keystroke to say so.
#[test]
fn ctrl_l_does_nothing_when_no_filter_is_in_force() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_with_codex_conversation(&dir);

    app.handle_key(KeyCode::Char('l'), KeyModifiers::CONTROL, 17);

    assert_eq!(*app.dialog_mode(), DialogMode::None);
}

/// A load learns what it cannot read only while it runs, so the term joins
/// the list after the filters the load started under.
#[test]
fn a_term_the_load_adds_is_listed_by_ctrl_l() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_with_codex_conversation(&dir);
    let ignored =
        crate::history::FilterTerm::new("Codex", "3 ignored: compressed sessions unsupported");
    app.add_active_filter(ignored.clone());

    app.handle_key(KeyCode::Char('l'), KeyModifiers::CONTROL, 17);

    assert_eq!(*app.dialog_mode(), DialogMode::ActiveFilters);
    assert_eq!(app.active_filters(), [ignored]);
}

#[test]
fn copying_the_session_id_of_a_listed_conversation_yields_the_listed_id_not_the_file_name() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_codex_rollout(dir.path());
    let conversation = Conversation {
        source: crate::history::Source::Codex,
        session_id: CODEX_SESSION_ID.to_string(),
        ..test_conversation(path, None)
    };
    let mut app = App::new(
        vec![conversation],
        ToolDisplayMode::Hidden,
        false,
        KeyBindings::default(),
        vec![],
    );
    app.selected = Some(0);
    app.enter_view_mode(80);
    app.set_clipboard_writer_for_test(record_copied_text);

    let copied = copied_by(&mut app, KeyCode::Char('I'));

    assert_eq!(copied, vec![CODEX_SESSION_ID.to_string()]);
}

#[test]
fn copying_the_session_id_of_a_directly_opened_file_reads_its_header() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_codex_rollout(dir.path());
    let mut app =
        App::new_single_file(path, ToolDisplayMode::Hidden, false, KeyBindings::default());
    app.set_clipboard_writer_for_test(record_copied_text);

    let copied = copied_by(&mut app, KeyCode::Char('I'));

    assert_eq!(copied, vec![CODEX_SESSION_ID.to_string()]);
}

#[test]
fn a_file_that_holds_no_conversation_copies_no_session_id() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir
        .path()
        .join("019f0000-0000-7000-8000-00000000000b.jsonl");
    std::fs::write(&path, "").unwrap();
    let mut app =
        App::new_single_file(path, ToolDisplayMode::Hidden, false, KeyBindings::default());
    app.set_clipboard_writer_for_test(record_copied_text);

    let copied = copied_by(&mut app, KeyCode::Char('I'));

    assert!(copied.is_empty());
    assert_eq!(
        app.status_message.as_ref().map(|(text, _)| text.as_str()),
        Some("Unable to determine session ID")
    );
}

#[test]
fn copying_the_path_yields_the_path_not_the_session_id() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_codex_rollout(dir.path());
    let mut app = App::new_single_file(
        path.clone(),
        ToolDisplayMode::Hidden,
        false,
        KeyBindings::default(),
    );
    app.set_clipboard_writer_for_test(record_copied_text);

    let copied = copied_by(&mut app, KeyCode::Char('Y'));

    assert_eq!(copied, vec![path.display().to_string()]);
}

#[test]
fn exporting_to_the_clipboard_copies_the_generated_content() {
    const JSONL_EXPORT_OPTION: usize = 3;

    let dir = tempfile::tempdir().unwrap();
    let path = write_codex_rollout(dir.path());
    let mut app = App::new_single_file(
        path.clone(),
        ToolDisplayMode::Hidden,
        false,
        KeyBindings::default(),
    );
    app.set_clipboard_writer_for_test(record_copied_text);

    let copied = copied_during(&mut app, |app| {
        app.perform_export(JSONL_EXPORT_OPTION, true)
    });

    assert_eq!(copied, vec![std::fs::read_to_string(&path).unwrap()]);
    assert_eq!(
        app.status_message.as_ref().map(|(text, _)| text.as_str()),
        Some("Conversation copied to clipboard")
    );
}

/// A user message, then the report of a background agent, written as
/// Claude Code records it.
fn write_task_report_conversation(path: &std::path::Path) {
    use crate::history::task_notification::test_support::AGENT_REPORT;
    let lines = [
        r#"{"type":"user","timestamp":"2024-01-01T00:00:00Z","message":{"role":"user","content":"intro"}}"#.to_string(),
        serde_json::json!({
            "type": "user",
            "timestamp": "2024-01-01T00:00:02Z",
            "message": {"role": "user", "content": AGENT_REPORT}
        })
        .to_string(),
    ];
    std::fs::write(path, lines.join("\n") + "\n").unwrap();
}

#[test]
fn right_arrow_expands_the_focused_task_report_and_left_arrow_collapses_it() {
    use crate::history::task_notification::test_support::AGENT_REPORT_LAST_LINE;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("report.jsonl");
    write_task_report_conversation(&path);
    let mut app = app_with_tool_conversation(path, ToolDisplayMode::Truncated);
    press(&mut app, KeyCode::Char('J'));
    assert_eq!(focused_message(&app), Some(1));
    assert!(!view_text(&app).contains(AGENT_REPORT_LAST_LINE));

    press(&mut app, KeyCode::Right);
    assert_eq!(expanded_tool_count(&app), 1);
    assert!(view_text(&app).contains(AGENT_REPORT_LAST_LINE));
    assert_eq!(stop(&app), (Some(1), None));

    press(&mut app, KeyCode::Left);
    assert_eq!(expanded_tool_count(&app), 0);
    assert!(!view_text(&app).contains(AGENT_REPORT_LAST_LINE));
    assert_eq!(stop(&app), (Some(1), None));
}

#[test]
fn right_arrow_in_truncated_mode_expands_every_truncated_body_and_left_arrow_collapses_them() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tool.jsonl");
    write_two_truncated_calls_conversation(&path);
    let mut app = app_with_tool_conversation(path, ToolDisplayMode::Truncated);
    press(&mut app, KeyCode::Char('J'));
    assert_eq!(stop(&app), (Some(0), None));
    assert!(!view_text(&app).contains("five"));
    assert!(!view_text(&app).contains("ten"));

    press(&mut app, KeyCode::Right);
    assert_eq!(expanded_tool_count(&app), 2);
    assert!(view_text(&app).contains("five"));
    assert!(view_text(&app).contains("ten"));
    assert_eq!(stop(&app), (Some(0), None));

    press(&mut app, KeyCode::Left);
    assert_eq!(expanded_tool_count(&app), 0);
    assert!(!view_text(&app).contains("five"));
    assert!(!view_text(&app).contains("ten"));
    assert_eq!(stop(&app), (Some(0), None));
}

#[test]
fn right_arrow_in_truncated_mode_on_a_message_with_nothing_truncated_does_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tool.jsonl");
    write_short_tool_conversation(&path);
    let mut app = app_with_tool_conversation(path, ToolDisplayMode::Truncated);
    press(&mut app, KeyCode::Char('J'));
    let text_before = view_text(&app);

    press(&mut app, KeyCode::Right);

    assert_eq!(expanded_tool_count(&app), 0);
    assert_eq!(view_text(&app), text_before);
    assert_eq!(stop(&app), (Some(0), None));
}

#[test]
fn a_click_in_truncated_mode_toggles_the_clicked_body_alone() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tool.jsonl");
    write_two_truncated_calls_conversation(&path);
    let mut app = app_with_tool_conversation(path, ToolDisplayMode::Truncated);
    let frame = Rect::new(0, 0, 120, 20);
    let row = tool_click_row(&app, frame);

    assert!(app.handle_view_click(row, frame, 17));

    assert_eq!(expanded_tool_count(&app), 1);
    assert!(view_text(&app).contains("five"));
    assert!(!view_text(&app).contains("ten"));
}
