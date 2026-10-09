//! `Ctrl+R` in the viewer: the open session read again from its file.

use super::*;
use crate::history::provider::{ReadError, SessionRead};
use crate::history::{Source, UpdatedSession};

const VIEWPORT: usize = 10;

fn user_line(text: &str) -> String {
    user_line_at(text, 0)
}

fn user_line_at(text: &str, second: usize) -> String {
    serde_json::json!({
        "type": "user",
        "timestamp": format!("2024-01-01T00:00:{second:02}Z"),
        "message": {"role": "user", "content": text}
    })
    .to_string()
}

fn numbered(range: std::ops::Range<usize>) -> Vec<String> {
    range.map(|index| format!("message {index}")).collect()
}

fn write_messages(path: &Path, texts: &[String]) {
    let lines: Vec<String> = texts.iter().map(|text| user_line(text)).collect();
    std::fs::write(path, lines.join("\n") + "\n").unwrap();
}

/// `count` messages, message N sent at second N.
fn write_timed_messages(path: &Path, count: usize) {
    let lines: Vec<String> = (0..count)
        .map(|index| user_line_at(&format!("message {index}"), index))
        .collect();
    std::fs::write(path, lines.join("\n") + "\n").unwrap();
}

/// A sub-agent transcript of `turns` turns, all sent at `second`: the splice
/// places them after the parent's message sent then.
fn write_subagent_turns(path: &Path, turns: usize, second: usize) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let lines: Vec<String> = (0..turns)
        .map(|index| user_line_at(&format!("sub-agent turn {index}"), second))
        .collect();
    std::fs::write(path, lines.join("\n") + "\n").unwrap();
}

/// A user message, a run of three calls, and a closing user message.
fn tool_run_lines() -> Vec<String> {
    [
        r#"{"type":"assistant","timestamp":"2024-01-01T00:00:01Z","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_1","name":"Bash","input":{"command":"one\ntwo\nthree\nfour\nfive\nsix"}}]}}"#,
        r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"ok"}]}}"#,
        r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_2","name":"Read","input":{"file_path":"src/lib.rs"}}]}}"#,
        r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_2","content":"short"}]}}"#,
    ]
    .map(str::to_owned)
    .to_vec()
}

/// The session's row as the list reads it from its file alone, with the
/// sub-agent transcripts the row already lists: these files live in temporary
/// directories that no agent's discovery lists.
fn read_from_its_file(row: &Conversation, _show_last: bool) -> Option<SessionRead> {
    let mut read = crate::history::parser::process_conversation_file(row.path.clone(), None, None)
        .ok()
        .flatten()?;
    read.index = row.index;
    read.subagents = row.subagents.clone();
    Some(SessionRead::Listed(Box::new(read)))
}

fn row_from_file(path: &Path) -> Conversation {
    crate::history::parser::process_conversation_file(path.to_path_buf(), None, None)
        .unwrap()
        .unwrap()
}

/// The list holding the session at `path`, open in the viewer.
fn viewer_on(path: &Path) -> App {
    viewer_on_with_subagents(path, Vec::new())
}

/// The list holding the session at `path`, whose row lists the sub-agent
/// transcripts `subagents`, open in the viewer.
fn viewer_on_with_subagents(path: &Path, subagents: Vec<PathBuf>) -> App {
    let row = Conversation {
        subagents,
        ..row_from_file(path)
    };
    let mut app = App::new(
        vec![row],
        ToolDisplayMode::Hidden,
        false,
        KeyBindings::default(),
        vec![],
    );
    app.set_session_reader_for_test(read_from_its_file);
    app.selected = Some(0);
    app.enter_view_mode(80);
    app
}

/// `Ctrl+R`, then the next frame's read.
fn press_refresh(app: &mut App) {
    app.handle_key(KeyCode::Char('r'), KeyModifiers::CONTROL, VIEWPORT);
    assert!(app.complete_pending_view_refresh(VIEWPORT));
}

fn view(app: &App) -> &ViewState {
    match app.app_mode() {
        AppMode::View(state) => state,
        AppMode::List => panic!("the viewer is closed"),
    }
}

fn view_text(app: &App) -> String {
    view(app)
        .rendered_lines
        .iter()
        .flat_map(|line| line.spans.iter().map(|(text, _)| text.as_str()))
        .collect()
}

fn top_row_text(app: &App) -> String {
    let state = view(app);
    state.rendered_lines[state.scroll_offset]
        .spans
        .iter()
        .map(|(text, _)| text.as_str())
        .collect()
}

fn status_text(app: &App) -> Option<&str> {
    app.status_message().map(|(message, _)| message.as_str())
}

#[test]
fn a_refresh_reads_on_the_next_frame_and_a_second_press_adds_no_read() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.jsonl");
    write_messages(&path, &numbered(0..3));
    let mut app = viewer_on(&path);
    write_messages(&path, &numbered(0..5));

    app.handle_key(KeyCode::Char('r'), KeyModifiers::CONTROL, VIEWPORT);
    app.handle_key(KeyCode::Char('r'), KeyModifiers::CONTROL, VIEWPORT);

    assert!(app.is_refreshing_open_session());
    assert!(!view_text(&app).contains("message 4"));
    assert!(app.complete_pending_view_refresh(VIEWPORT));
    assert!(view_text(&app).contains("message 4"));
    assert!(!app.complete_pending_view_refresh(VIEWPORT));
}

#[test]
fn a_refresh_asked_for_before_leaving_the_viewer_doesnt_run_on_the_next_session() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.jsonl");
    write_messages(&path, &numbered(0..3));
    let mut app = viewer_on(&path);

    app.handle_key(KeyCode::Char('r'), KeyModifiers::CONTROL, VIEWPORT);
    app.handle_key(KeyCode::Esc, KeyModifiers::empty(), VIEWPORT);
    assert!(!app.is_refreshing_open_session());
    assert!(app.request_open());
    assert!(app.complete_pending_open(80));

    assert!(!app.is_refreshing_open_session());
    assert!(!app.complete_pending_view_refresh(VIEWPORT));
}

#[test]
fn leaving_the_viewer_with_q_drops_a_pending_refresh() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.jsonl");
    write_messages(&path, &numbered(0..3));
    let mut app = viewer_on(&path);

    app.handle_key(KeyCode::Char('r'), KeyModifiers::CONTROL, VIEWPORT);
    app.handle_key(KeyCode::Char('q'), KeyModifiers::empty(), VIEWPORT);

    assert!(matches!(app.app_mode(), AppMode::List));
    assert!(!app.is_refreshing_open_session());
    assert!(
        !app.complete_pending_view_refresh(VIEWPORT),
        "no read ran, so the frame has nothing new to draw"
    );
}

/// The messages the last refresh added below a reader at the bottom, as the
/// new-messages badge shows them.
fn arrived_below(app: &App) -> Option<usize> {
    view(app).count_highlight?.arrived_below
}

#[test]
fn at_the_bottom_a_refresh_shows_the_new_messages_and_counts_them_for_the_badge() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.jsonl");
    write_messages(&path, &numbered(0..30));
    let mut app = viewer_on(&path);
    app.handle_key(KeyCode::End, KeyModifiers::empty(), VIEWPORT);

    write_messages(&path, &numbered(0..32));
    press_refresh(&mut app);

    assert!(view_text(&app).contains("message 31"));
    let state = view(&app);
    assert_eq!(state.scroll_offset, state.total_lines - VIEWPORT);
    assert_eq!(arrived_below(&app), Some(2));
    assert_eq!(status_text(&app), None);
}

#[test]
fn at_the_bottom_a_refresh_that_finds_nothing_new_reports_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.jsonl");
    write_messages(&path, &numbered(0..30));
    let mut app = viewer_on(&path);
    app.handle_key(KeyCode::End, KeyModifiers::empty(), VIEWPORT);

    press_refresh(&mut app);

    assert_eq!(view(&app).count_highlight, None);
    assert_eq!(status_text(&app), None);
}

#[test]
fn scrolled_up_a_refresh_with_new_messages_highlights_only_the_count() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.jsonl");
    write_messages(&path, &numbered(0..30));
    let mut app = viewer_on(&path);
    scroll_to_message(&mut app, "message 10");

    write_messages(&path, &numbered(0..32));
    press_refresh(&mut app);

    assert!(view_text(&app).contains("message 31"));
    assert!(view(&app).count_highlight.is_some());
    assert_eq!(arrived_below(&app), None);
    assert_eq!(status_text(&app), None);
}

/// Scroll so the message holding `text` is the top row; returns that
/// message's index.
fn scroll_to_message(app: &mut App, text: &str) -> usize {
    let AppMode::View(state) = &mut app.app_mode else {
        panic!("the viewer is closed");
    };
    let message = state
        .message_ranges
        .iter()
        .position(|range| {
            state.rendered_lines[range.rows()].iter().any(|line| {
                let row: String = line.spans.iter().map(|(text, _)| text.as_str()).collect();
                row.contains(text)
            })
        })
        .unwrap();
    state.scroll_offset = state.message_ranges[message].start_line;
    message
}

thread_local! {
    static COPIED: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
}

fn record_copy(text: &str) -> Result<ClipboardDestination, String> {
    COPIED.with(|copied| copied.borrow_mut().push(text.to_owned()));
    Ok(ClipboardDestination::System)
}

#[test]
fn message_copy_copies_the_message_on_screen_after_the_file_grew() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.jsonl");
    let subagent = dir.path().join("session/subagents/agent-x.jsonl");
    write_timed_messages(&path, 30);
    write_subagent_turns(&subagent, 1, 5);
    let mut app = viewer_on_with_subagents(&path, vec![subagent.clone()]);
    app.set_clipboard_writer_for_test(record_copy);
    let message_index = scroll_to_message(&mut app, "message 10");
    if let AppMode::View(state) = &mut app.app_mode {
        state.message_nav_active = true;
        state.focus = Some(Focus {
            message_index,
            call_index: None,
        });
    }

    // Not read again: the copy must not copy whatever now sits at its place.
    write_subagent_turns(&subagent, 3, 5);
    COPIED.with(|copied| copied.borrow_mut().clear());
    app.handle_key(KeyCode::Char('y'), KeyModifiers::empty(), VIEWPORT);

    let copied = COPIED.with(|copied| copied.borrow().clone());
    assert_eq!(copied.len(), 1, "{copied:?}");
    assert!(copied[0].contains("message 10"), "{copied:?}");
}

#[test]
fn scrolled_up_a_refresh_keeps_the_same_message_at_the_top() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.jsonl");
    let subagent = dir.path().join("session/subagents/agent-x.jsonl");
    write_timed_messages(&path, 30);
    write_subagent_turns(&subagent, 1, 5);
    let mut app = viewer_on_with_subagents(&path, vec![subagent.clone()]);
    scroll_to_message(&mut app, "message 10");
    assert!(top_row_text(&app).contains("message 10"));

    // The sub-agent's new turns splice in after message 5, above the reader.
    write_subagent_turns(&subagent, 3, 5);
    press_refresh(&mut app);

    assert!(top_row_text(&app).contains("message 10"));
}

#[test]
fn an_expanded_run_stays_expanded_when_sub_agent_turns_splice_in_above_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.jsonl");
    let subagent = dir.path().join("session/subagents/agent-x.jsonl");
    let mut lines = vec![user_line("intro")];
    lines.extend(tool_run_lines());
    lines.push(user_line("outro"));
    std::fs::write(&path, lines.join("\n") + "\n").unwrap();
    write_subagent_turns(&subagent, 1, 0);
    let mut app = viewer_on_with_subagents(&path, vec![subagent.clone()]);
    app.handle_key(KeyCode::Char('J'), KeyModifiers::empty(), VIEWPORT);
    app.handle_key(KeyCode::Enter, KeyModifiers::empty(), VIEWPORT);
    let expanded = view(&app).expanded_tool_outputs.clone();
    assert!(!expanded.is_empty());
    assert!(view_text(&app).contains("src/lib.rs"));

    write_subagent_turns(&subagent, 3, 0);
    press_refresh(&mut app);

    assert_eq!(view(&app).expanded_tool_outputs.len(), expanded.len());
    assert_ne!(view(&app).expanded_tool_outputs, expanded);
    assert!(view_text(&app).contains("src/lib.rs"));
}

#[test]
fn a_refresh_updates_the_sessions_row_in_the_list() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.jsonl");
    write_messages(&path, &numbered(0..3));
    let mut app = viewer_on(&path);
    let before = app.conversations()[0].message_count;

    write_messages(&path, &numbered(0..5));
    press_refresh(&mut app);

    assert_eq!(app.conversations()[0].message_count, before + 2);
}

#[test]
fn a_deleted_session_returns_to_the_list() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.jsonl");
    write_messages(&path, &numbered(0..3));
    let mut app = viewer_on(&path);

    std::fs::remove_file(&path).unwrap();
    press_refresh(&mut app);

    assert!(matches!(app.app_mode(), AppMode::List));
    assert!(app.conversations().is_empty());
    assert_eq!(status_text(&app), Some("Session not found"));
}

#[test]
fn a_failed_read_keeps_the_view() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.jsonl");
    write_messages(&path, &numbered(0..3));
    let mut app = viewer_on(&path);
    app.set_session_reader_for_test(|_, _| Some(SessionRead::Failed(ReadError::Permanent)));

    press_refresh(&mut app);

    assert!(view_text(&app).contains("message 2"));
    assert_eq!(
        status_text(&app),
        Some("Refresh failed: the session could not be read")
    );
}

fn count_highlight_remaining(app: &App) -> std::time::Duration {
    app.count_highlight_remaining()
        .unwrap_or(std::time::Duration::ZERO)
}

#[test]
fn a_refresh_that_adds_messages_highlights_the_count_for_three_seconds() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.jsonl");
    write_messages(&path, &numbered(0..30));
    let mut app = viewer_on(&path);
    scroll_to_message(&mut app, "message 10");

    write_messages(&path, &numbered(0..32));
    press_refresh(&mut app);

    let remaining = count_highlight_remaining(&app);
    assert!(
        remaining > std::time::Duration::from_millis(2500)
            && remaining <= std::time::Duration::from_secs(3),
        "{remaining:?}"
    );
}

#[test]
fn a_refresh_that_adds_nothing_doesnt_highlight_the_count() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.jsonl");
    write_messages(&path, &numbered(0..30));
    let mut app = viewer_on(&path);

    press_refresh(&mut app);

    assert_eq!(app.count_highlight_remaining(), None);
}

/// Two seconds pass: one second of the current highlight is left.
fn two_seconds_pass(app: &mut App) {
    if let AppMode::View(state) = &mut app.app_mode
        && let Some(highlight) = &mut state.count_highlight
    {
        highlight.until -= std::time::Duration::from_secs(2);
    }
    assert!(count_highlight_remaining(app) < std::time::Duration::from_millis(1500));
}

#[test]
fn a_second_count_change_restarts_the_highlight_from_that_refresh() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.jsonl");
    write_messages(&path, &numbered(0..30));
    let mut app = viewer_on(&path);
    scroll_to_message(&mut app, "message 10");
    write_messages(&path, &numbered(0..31));
    press_refresh(&mut app);

    two_seconds_pass(&mut app);
    write_messages(&path, &numbered(0..32));
    press_refresh(&mut app);

    assert!(
        count_highlight_remaining(&app) > std::time::Duration::from_millis(2500),
        "the second change restarts the three seconds"
    );
}

#[test]
fn a_second_refresh_at_the_bottom_shows_its_own_count_of_new_messages() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.jsonl");
    write_messages(&path, &numbered(0..30));
    let mut app = viewer_on(&path);
    app.handle_key(KeyCode::End, KeyModifiers::empty(), VIEWPORT);
    write_messages(&path, &numbered(0..32));
    press_refresh(&mut app);
    assert_eq!(arrived_below(&app), Some(2));

    two_seconds_pass(&mut app);
    write_messages(&path, &numbered(0..33));
    press_refresh(&mut app);

    assert_eq!(arrived_below(&app), Some(1), "not a running total");
    assert!(count_highlight_remaining(&app) > std::time::Duration::from_millis(2500));
}

#[test]
fn a_failed_display_read_leaves_the_row_as_it_was() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.jsonl");
    write_messages(&path, &numbered(0..3));
    let mut app = viewer_on(&path);
    let before = app.conversations()[0].message_count;
    app.set_session_reader_for_test(|row, _| {
        Some(SessionRead::Listed(Box::new(Conversation {
            message_count: row.message_count + 2,
            ..row.clone()
        })))
    });

    // The row's read succeeds; the viewer's own read of the file fails.
    std::fs::remove_file(&path).unwrap();
    press_refresh(&mut app);

    assert_eq!(app.conversations()[0].message_count, before);
    assert!(
        status_text(&app).is_some_and(|text| text.starts_with("Refresh failed: ")),
        "{:?}",
        status_text(&app)
    );
}

#[test]
fn a_list_refresh_applied_later_doesnt_undo_a_viewer_refresh() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.jsonl");
    write_messages(&path, &numbered(0..3));
    let mut app = viewer_on(&path);
    let read_by_the_list_refresh = app.conversations()[0].clone();
    app.begin_refresh().unwrap();

    write_messages(&path, &numbered(0..5));
    press_refresh(&mut app);
    app.handle_key(KeyCode::Esc, KeyModifiers::empty(), VIEWPORT);
    app.finish_refresh(Ok(SessionChanges {
        updated: vec![UpdatedSession {
            row: read_by_the_list_refresh,
            replaces_listed_row: true,
        }],
        ..Default::default()
    }));

    assert_eq!(
        app.conversations()[0].message_count,
        row_from_file(&path).message_count
    );
}

#[test]
fn a_refresh_reads_a_directly_opened_file_again() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.jsonl");
    write_messages(&path, &numbered(0..3));
    let mut app = App::new_single_file(
        path.clone(),
        Source::Claude,
        ToolDisplayMode::Hidden,
        false,
        KeyBindings::default(),
    );
    app.check_view_resize(80, VIEWPORT);

    write_messages(&path, &numbered(0..5));
    press_refresh(&mut app);

    assert!(view_text(&app).contains("message 4"));
    assert_eq!(app.conversations()[0].message_count, 5);
}

/// The event loop for a directly opened file waits `idle_wait`, the same rule
/// as the list's loop.
#[test]
fn a_directly_opened_file_waits_only_until_its_count_highlight_ends() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.jsonl");
    write_messages(&path, &numbered(0..3));
    let mut app = App::new_single_file(
        path.clone(),
        Source::Claude,
        ToolDisplayMode::Hidden,
        false,
        KeyBindings::default(),
    );
    app.check_view_resize(80, VIEWPORT);
    assert_eq!(app.idle_wait(), std::time::Duration::from_secs(3600));

    write_messages(&path, &numbered(0..5));
    press_refresh(&mut app);

    assert!(app.idle_wait() <= std::time::Duration::from_secs(3));
}
