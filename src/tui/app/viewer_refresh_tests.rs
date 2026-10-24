//! `Ctrl+R` in the viewer: the open session's refresh, and the viewer reading
//! the refreshed copy from the cache.

use super::claude_home::{ClaudeHome, app_reading};
use super::refresh_tests::{refresh_found, serve_session_refresh};
use super::*;
use crate::clock::TestClock;
use crate::history::environment::test_support::test_environment;
use crate::history::{SessionChanges, Source, UpdatedSession};
use std::path::Path;

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
    write_lines(path, texts.iter().map(|text| user_line(text)).collect());
}

/// `count` messages, message N sent at second N.
fn write_timed_messages(path: &Path, count: usize) {
    write_lines(
        path,
        (0..count)
            .map(|index| user_line_at(&format!("message {index}"), index))
            .collect(),
    );
}

fn write_lines(path: &Path, lines: Vec<String>) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
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

/// The row of the transcript at `path`, parsed off the drawing thread, as
/// the background worker parses it.
fn row_from_file(path: &Path) -> Conversation {
    let path = path.to_path_buf();
    std::thread::spawn(move || {
        crate::history::parser::process_conversation_file(path, None, None)
            .unwrap()
            .unwrap()
    })
    .join()
    .unwrap()
}

/// The list holding `home`'s sessions, the first open in the viewer.
fn viewer_on(home: &ClaudeHome) -> App {
    viewer_on_clock(home, Arc::new(TestClock::default()))
}

/// [`viewer_on`], on a `clock` the test moves.
fn viewer_on_clock(home: &ClaudeHome, clock: Arc<TestClock>) -> App {
    let environment = home.environment_on(clock);
    let mut app = app_reading(home.rows(&environment), environment);
    app.selected = Some(0);
    app.enter_view_mode(80);
    app
}

/// `Ctrl+R`, the session refresh it asks for, then the next frame's read of
/// the refreshed copy.
fn press_refresh(app: &mut App) {
    request_refresh(app);
    assert!(app.complete_pending_view_refresh(VIEWPORT));
}

/// `Ctrl+R`, and the session refresh it asks for, run as the background
/// worker runs it.
fn request_refresh(app: &mut App) {
    app.handle_key(KeyCode::Char('r'), KeyModifiers::CONTROL, VIEWPORT);
    serve_session_refresh(app);
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
fn refresh_reads_on_next_frame_and_second_press_adds_no_read() {
    let home = ClaudeHome::new();
    let path = home.transcript("session");
    write_messages(&path, &numbered(0..3));
    let mut app = viewer_on(&home);
    write_messages(&path, &numbered(0..5));

    app.handle_key(KeyCode::Char('r'), KeyModifiers::CONTROL, VIEWPORT);
    app.handle_key(KeyCode::Char('r'), KeyModifiers::CONTROL, VIEWPORT);

    assert!(app.is_open_session_updating());
    assert!(!view_text(&app).contains("message 4"));
    serve_session_refresh(&mut app);
    assert!(
        !app.is_session_refresh_requested(),
        "the second press doesn't ask for a second refresh"
    );
    assert!(app.complete_pending_view_refresh(VIEWPORT));
    assert!(view_text(&app).contains("message 4"));
    assert!(!app.complete_pending_view_refresh(VIEWPORT));
}

#[test]
fn refresh_asked_for_before_leaving_viewer_doesnt_run_on_next_session() {
    let home = ClaudeHome::new();
    let first = home.transcript("first");
    write_timed_messages(&first, 3);
    let second = home.transcript("second");
    write_messages(&second, &numbered(10..12));
    let mut app = viewer_on(&home);
    let opened_first = view(&app).conversation_path.clone();

    app.handle_key(KeyCode::Char('r'), KeyModifiers::CONTROL, VIEWPORT);
    app.handle_key(KeyCode::Esc, KeyModifiers::empty(), VIEWPORT);
    app.selected = app
        .filtered()
        .iter()
        .position(|&index| app.conversations()[index].path != opened_first);
    assert!(app.request_open());
    assert!(app.complete_pending_open(80));
    let next_session = view(&app).conversation_path.clone();
    serve_session_refresh(&mut app);

    assert_ne!(next_session, opened_first, "the next session opens");
    assert_eq!(
        view(&app).conversation_path,
        next_session,
        "the earlier session's refresh keeps the open session"
    );
    assert!(
        !app.complete_pending_view_refresh(VIEWPORT),
        "the earlier session's refresh doesn't reload the next session's view"
    );
}

#[test]
fn leaving_the_viewer_with_q_drops_its_pending_reload() {
    let home = ClaudeHome::new();
    let path = home.transcript("session");
    write_messages(&path, &numbered(0..3));
    let mut app = viewer_on(&home);

    app.handle_key(KeyCode::Char('r'), KeyModifiers::CONTROL, VIEWPORT);
    app.handle_key(KeyCode::Char('q'), KeyModifiers::empty(), VIEWPORT);
    serve_session_refresh(&mut app);

    assert!(matches!(app.app_mode(), AppMode::List));
    assert!(
        !app.complete_pending_view_refresh(VIEWPORT),
        "a closed viewer doesn't draw a reload"
    );
}

/// The messages the last refresh added below a reader at the bottom, as the
/// new-messages badge shows them.
fn arrived_below(app: &App) -> Option<usize> {
    view(app).count_highlight?.arrived_below
}

#[test]
fn at_bottom_refresh_shows_new_messages_and_counts_them_for_badge() {
    let home = ClaudeHome::new();
    let path = home.transcript("session");
    write_messages(&path, &numbered(0..30));
    let mut app = viewer_on(&home);
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
fn at_bottom_refresh_that_finds_nothing_new_reports_nothing() {
    let home = ClaudeHome::new();
    let path = home.transcript("session");
    write_messages(&path, &numbered(0..30));
    let mut app = viewer_on(&home);
    app.handle_key(KeyCode::End, KeyModifiers::empty(), VIEWPORT);

    press_refresh(&mut app);

    assert_eq!(view(&app).count_highlight, None);
    assert_eq!(status_text(&app), None);
}

#[test]
fn scrolled_up_refresh_with_new_messages_highlights_only_count() {
    let home = ClaudeHome::new();
    let path = home.transcript("session");
    write_messages(&path, &numbered(0..30));
    let mut app = viewer_on(&home);
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
fn message_copy_copies_message_on_screen_after_file_grew() {
    let home = ClaudeHome::new();
    let path = home.transcript("session");
    let subagent = home.project().join("session/subagents/agent-x.jsonl");
    write_timed_messages(&path, 30);
    write_subagent_turns(&subagent, 1, 5);
    let mut app = viewer_on(&home);
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
fn scrolled_up_refresh_keeps_same_message_at_top() {
    let home = ClaudeHome::new();
    let path = home.transcript("session");
    let subagent = home.project().join("session/subagents/agent-x.jsonl");
    write_timed_messages(&path, 30);
    write_subagent_turns(&subagent, 1, 5);
    let mut app = viewer_on(&home);
    scroll_to_message(&mut app, "message 10");
    assert!(top_row_text(&app).contains("message 10"));

    // The sub-agent's new turns splice in after message 5, above the reader.
    write_subagent_turns(&subagent, 3, 5);
    press_refresh(&mut app);

    assert!(top_row_text(&app).contains("message 10"));
}

#[test]
fn expanded_run_stays_expanded_when_sub_agent_turns_splice_in_above_it() {
    let home = ClaudeHome::new();
    let path = home.transcript("session");
    let subagent = home.project().join("session/subagents/agent-x.jsonl");
    let mut lines = vec![user_line("intro")];
    lines.extend(tool_run_lines());
    lines.push(user_line("outro"));
    write_lines(&path, lines);
    write_subagent_turns(&subagent, 1, 0);
    let mut app = viewer_on(&home);
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
fn refresh_updates_sessions_row_in_list() {
    let home = ClaudeHome::new();
    let path = home.transcript("session");
    write_messages(&path, &numbered(0..3));
    let mut app = viewer_on(&home);
    let before = app.conversations()[0].message_count;

    write_messages(&path, &numbered(0..5));
    press_refresh(&mut app);
    app.handle_key(KeyCode::Esc, KeyModifiers::empty(), VIEWPORT);
    app.apply_finished_refresh();

    assert_eq!(app.conversations()[0].message_count, before + 2);
}

#[test]
fn deleted_session_returns_to_list() {
    let home = ClaudeHome::new();
    let path = home.transcript("session");
    write_messages(&path, &numbered(0..3));
    let mut app = viewer_on(&home);

    std::fs::remove_file(&path).unwrap();
    request_refresh(&mut app);

    assert!(matches!(app.app_mode(), AppMode::List));
    assert!(app.conversations().is_empty());
    assert_eq!(status_text(&app), Some("Session not found"));
}

#[test]
fn failed_read_keeps_view() {
    let home = ClaudeHome::new();
    let path = home.transcript("session");
    write_messages(&path, &numbered(0..3));
    let mut app = viewer_on(&home);
    home.refuse_project();

    request_refresh(&mut app);

    assert!(view_text(&app).contains("message 2"));
    assert_eq!(
        status_text(&app),
        Some("Refresh failed: the session could not be read")
    );
}

#[test]
fn refresh_that_adds_messages_highlights_count_for_three_seconds() {
    let home = ClaudeHome::new();
    let path = home.transcript("session");
    write_messages(&path, &numbered(0..30));
    let clock = Arc::new(TestClock::default());
    let mut app = viewer_on_clock(&home, clock.clone());
    scroll_to_message(&mut app, "message 10");

    write_messages(&path, &numbered(0..32));
    press_refresh(&mut app);

    assert_eq!(
        app.count_highlight_remaining(),
        Some(Duration::from_secs(3))
    );
    clock.advance(Duration::from_millis(3001));
    assert_eq!(app.count_highlight_remaining(), None);
}

#[test]
fn refresh_that_adds_nothing_doesnt_highlight_count() {
    let home = ClaudeHome::new();
    let path = home.transcript("session");
    write_messages(&path, &numbered(0..30));
    let mut app = viewer_on(&home);

    press_refresh(&mut app);

    assert_eq!(app.count_highlight_remaining(), None);
}

/// Two seconds pass on `clock`: one second of the current highlight is left.
fn two_seconds_pass(clock: &TestClock, app: &App) {
    clock.advance(Duration::from_secs(2));
    assert_eq!(
        app.count_highlight_remaining(),
        Some(Duration::from_secs(1))
    );
}

#[test]
fn second_count_change_restarts_highlight_from_that_refresh() {
    let home = ClaudeHome::new();
    let path = home.transcript("session");
    write_messages(&path, &numbered(0..30));
    let clock = Arc::new(TestClock::default());
    let mut app = viewer_on_clock(&home, clock.clone());
    scroll_to_message(&mut app, "message 10");
    write_messages(&path, &numbered(0..31));
    press_refresh(&mut app);

    two_seconds_pass(&clock, &app);
    write_messages(&path, &numbered(0..32));
    press_refresh(&mut app);

    assert_eq!(
        app.count_highlight_remaining(),
        Some(Duration::from_secs(3)),
        "the second change restarts the three seconds"
    );
}

#[test]
fn second_refresh_at_bottom_shows_its_own_count_of_new_messages() {
    let home = ClaudeHome::new();
    let path = home.transcript("session");
    write_messages(&path, &numbered(0..30));
    let clock = Arc::new(TestClock::default());
    let mut app = viewer_on_clock(&home, clock.clone());
    app.handle_key(KeyCode::End, KeyModifiers::empty(), VIEWPORT);
    write_messages(&path, &numbered(0..32));
    press_refresh(&mut app);
    assert_eq!(arrived_below(&app), Some(2));

    two_seconds_pass(&clock, &app);
    write_messages(&path, &numbered(0..33));
    press_refresh(&mut app);

    assert_eq!(arrived_below(&app), Some(1), "not a running total");
    assert_eq!(
        app.count_highlight_remaining(),
        Some(Duration::from_secs(3))
    );
}

#[test]
fn list_refresh_applied_later_doesnt_undo_viewer_refresh() {
    let home = ClaudeHome::new();
    let path = home.transcript("session");
    write_messages(&path, &numbered(0..3));
    let mut app = viewer_on(&home);
    let read_by_the_list_refresh = app.conversations()[0].clone();
    app.begin_refresh().unwrap();

    write_messages(&path, &numbered(0..5));
    app.handle_key(KeyCode::Char('r'), KeyModifiers::CONTROL, VIEWPORT);
    app.handle_key(KeyCode::Esc, KeyModifiers::empty(), VIEWPORT);
    app.finish_refresh(refresh_found(SessionChanges {
        updated: vec![UpdatedSession {
            row: read_by_the_list_refresh,
            replaces_listed_row: true,
            next: crate::history::NextStep::Settled,
        }],
        ..Default::default()
    }));
    serve_session_refresh(&mut app);

    assert_eq!(
        app.conversations()[0].message_count,
        row_from_file(&path).message_count
    );
}

/// `Ctrl+R` on a file opened directly outside every provider's session
/// roots, the background worker's read of it, then the next frame showing
/// that read.
fn press_refresh_outside_every_root(app: &mut App) {
    app.handle_key(KeyCode::Char('r'), KeyModifiers::CONTROL, VIEWPORT);
    app.serve_direct_file_read();
    app.complete_pending_view_refresh(VIEWPORT);
}

/// The file at `path` opened directly in an environment reading no
/// provider's sessions, so it lies outside every session root.
fn opened_outside_every_root(path: &Path) -> App {
    let mut app = App::new_single_file_read(
        path.to_path_buf(),
        ToolDisplayMode::Hidden,
        false,
        KeyBindings::default(),
        test_environment(Vec::new(), None),
    );
    app.check_view_resize(80, VIEWPORT);
    app
}

#[test]
fn refresh_reads_directly_opened_file_again() {
    let home = ClaudeHome::new();
    let path = home.transcript("session");
    write_messages(&path, &numbered(0..3));
    let mut app = opened_outside_every_root(&path);

    write_messages(&path, &numbered(0..5));
    press_refresh_outside_every_root(&mut app);

    assert!(view_text(&app).contains("message 4"));
    assert_eq!(app.conversations()[0].message_count, 5);
}

/// A session holding only its header lists no row; a refresh once it holds
/// its conversation shows its sub-agents.
#[test]
fn directly_opened_file_holding_only_its_header_shows_its_sub_agents_after_refresh() {
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/omp");
    let session = std::fs::read_to_string(fixtures.join("v3.jsonl")).unwrap();
    let title_slot_and_header: Vec<&str> = session.lines().take(2).collect();
    let directory = tempfile::tempdir().unwrap();
    let project = directory.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    let path = project.join("session.jsonl");
    std::fs::write(&path, title_slot_and_header.join("\n") + "\n").unwrap();
    let subagent = project.join("session").join("worker.jsonl");
    std::fs::create_dir_all(subagent.parent().unwrap()).unwrap();
    std::fs::copy(fixtures.join("subagent.jsonl"), &subagent).unwrap();
    let omp: &'static dyn crate::history::provider::SessionStorage =
        Box::leak(Box::new(crate::history::provider::RootedStorage {
            inner: Source::Omp.provider().storage(),
            root: crate::history::provider::SessionRoot::new(directory.path()).in_agent_tree(),
        }));
    let mut app = App::new_single_file_read(
        path.clone(),
        ToolDisplayMode::Hidden,
        false,
        KeyBindings::default(),
        test_environment(vec![omp], None),
    );
    app.check_view_resize(80, VIEWPORT);

    std::fs::write(&path, session).unwrap();
    press_refresh(&mut app);

    assert_eq!(view(&app).subagents, vec![subagent]);
}

/// The event loop for a directly opened file waits `idle_wait`, the same rule
/// as the list's loop.
#[test]
fn directly_opened_file_waits_only_until_its_count_highlight_ends() {
    let home = ClaudeHome::new();
    let path = home.transcript("session");
    write_messages(&path, &numbered(0..3));
    let mut app = opened_outside_every_root(&path);
    assert_eq!(app.idle_wait(), Duration::from_secs(3600));

    write_messages(&path, &numbered(0..5));
    press_refresh_outside_every_root(&mut app);

    assert_eq!(app.idle_wait(), Duration::from_secs(3));
}
