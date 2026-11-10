//! `Ctrl+R` in the viewer: the open session's refresh, and the viewer reading
//! the refreshed copy from the cache.

use super::claude_home::{ClaudeHome, app_reading, serve_list_refresh};
use super::commit_failure_tests::launched_app;
use super::refresh_tests::{refresh_found, serve_session_refresh};
use super::test_waits::{ConnectionGate, VIEWER_READ_LANDS};
use super::*;
use crate::clock::TestClock;
use crate::history::environment::test_support::test_environment;
use crate::history::provider::contract_tests::{EXTERNAL_TITLE_RENAMES, Naming, name_session};
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
    assert!(app.complete_pending_view_refresh(VIEWPORT, VIEWER_READ_LANDS));
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
    assert!(app.complete_pending_view_refresh(VIEWPORT, VIEWER_READ_LANDS));
    assert!(view_text(&app).contains("message 4"));
    assert!(!app.complete_pending_view_refresh(VIEWPORT, VIEWER_READ_LANDS));
}

#[test]
fn reload_keeps_old_copy_under_updating_until_its_read_lands() {
    let home = ClaudeHome::new();
    let path = home.transcript("session");
    write_messages(&path, &numbered(0..30));
    let mut app = viewer_on(&home);
    write_messages(&path, &numbered(0..31));
    let gate = ConnectionGate::closed(app.environment.cache_base().path(), |thread| {
        thread.name() == Some("viewer-read")
    });

    request_refresh(&mut app);
    let waits_for_the_read = !app.complete_pending_view_refresh(VIEWPORT, Duration::ZERO);
    let old_copy_on_screen = !view_text(&app).contains("message 30");
    let status = app.open_session_status();
    app.handle_key(KeyCode::Char('j'), KeyModifiers::empty(), VIEWPORT);
    let scrolled = view(&app).scroll_offset;
    gate.open();
    let landed = app.complete_pending_view_refresh(VIEWPORT, VIEWER_READ_LANDS);

    assert!(
        waits_for_the_read && old_copy_on_screen,
        "the viewer keeps the old copy while the reload's read runs"
    );
    assert_eq!(
        status,
        Some(OpenSessionStatus::Updating),
        "the viewer shows the session as updating until the read lands"
    );
    assert_eq!(scrolled, 1, "the old copy scrolls while the read runs");
    assert!(
        landed && view_text(&app).contains("message 30"),
        "the reload shows once its read lands"
    );
    assert_eq!(app.open_session_status(), None);
}

#[test]
fn refresh_finishing_during_gated_read_queues_one_read_after_it() {
    let home = ClaudeHome::new();
    let path = home.transcript("session");
    write_messages(&path, &numbered(0..30));
    let mut app = viewer_on(&home);
    let gate = ConnectionGate::closed(app.environment.cache_base().path(), |thread| {
        thread.name() == Some("viewer-read")
    });

    write_messages(&path, &numbered(0..31));
    request_refresh(&mut app);
    write_messages(&path, &numbered(0..32));
    request_refresh(&mut app);
    let waits_for_the_first_read = !app.complete_pending_view_refresh(VIEWPORT, Duration::ZERO);
    gate.open();
    let first_landed = app.complete_pending_view_refresh(VIEWPORT, VIEWER_READ_LANDS);
    let reads_again = app.is_reading_open_session();
    let status_between = app.open_session_status();
    let second_landed = app.complete_pending_view_refresh(VIEWPORT, VIEWER_READ_LANDS);

    assert!(
        waits_for_the_first_read,
        "the second refresh doesn't replace the read running"
    );
    assert!(first_landed, "the first read lands and shows");
    assert!(
        reads_again,
        "the newer refresh's read starts once the first lands"
    );
    assert_eq!(
        status_between,
        Some(OpenSessionStatus::Updating),
        "the viewer shows the session as updating until the newer read lands"
    );
    assert!(
        second_landed && view_text(&app).contains("message 31"),
        "the newer refresh's read shows the newest messages"
    );
    assert!(
        !app.complete_pending_view_refresh(VIEWPORT, VIEWER_READ_LANDS),
        "no further read follows"
    );
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
    assert!(crate::tui::app::test_waits::finish_pending_open(
        &mut app, 80
    ));
    let next_session = view(&app).conversation_path.clone();
    serve_session_refresh(&mut app);

    assert_ne!(next_session, opened_first, "the next session opens");
    assert_eq!(
        view(&app).conversation_path,
        next_session,
        "the earlier session's refresh keeps the open session"
    );
    assert!(
        !app.complete_pending_view_refresh(VIEWPORT, VIEWER_READ_LANDS),
        "the earlier session's refresh doesn't reload the next session's view"
    );
}

#[test]
fn leaving_viewer_with_q_drops_its_pending_reload() {
    let home = ClaudeHome::new();
    let path = home.transcript("session");
    write_messages(&path, &numbered(0..3));
    let mut app = viewer_on(&home);

    app.handle_key(KeyCode::Char('r'), KeyModifiers::CONTROL, VIEWPORT);
    app.handle_key(KeyCode::Char('q'), KeyModifiers::empty(), VIEWPORT);
    serve_session_refresh(&mut app);

    assert!(matches!(app.app_mode(), AppMode::List));
    assert!(
        !app.complete_pending_view_refresh(VIEWPORT, VIEWER_READ_LANDS),
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

/// The export menu's plain-text option.
const PLAIN_EXPORT_OPTION: usize = 1;

#[test]
fn deferred_export_still_out_of_date_after_its_refresh_reports_failed_read() {
    let home = ClaudeHome::new();
    let path = home.transcript("session");
    write_messages(&path, &numbered(0..3));
    let mut app = viewer_on(&home);
    app.set_clipboard_writer_for_test(record_copy);
    COPIED.with(|copied| copied.borrow_mut().clear());
    write_messages(&path, &numbered(0..5));
    home.refuse_project();

    app.perform_export(PLAIN_EXPORT_OPTION, true);
    serve_session_refresh(&mut app);

    let copied = COPIED.with(|copied| copied.borrow().clone());
    assert!(
        copied.is_empty(),
        "the export doesn't copy the out-of-date copy: {copied:?}"
    );
    assert_eq!(
        status_text(&app),
        Some("Refresh failed: the session could not be read")
    );
}

/// The export menu's JSONL option.
const JSONL_EXPORT_OPTION: usize = 3;

#[test]
fn jsonl_export_of_out_of_date_session_copies_transcript_at_once() {
    let home = ClaudeHome::new();
    let path = home.transcript("session");
    write_messages(&path, &numbered(0..3));
    let mut app = viewer_on(&home);
    app.set_clipboard_writer_for_test(record_copy);
    COPIED.with(|copied| copied.borrow_mut().clear());
    write_messages(&path, &numbered(0..5));
    app.record_next_step(&path, crate::history::NextStep::CacheInBackground);

    app.perform_export(JSONL_EXPORT_OPTION, true);

    let copied = COPIED.with(|copied| copied.borrow().clone());
    assert_eq!(
        copied,
        [std::fs::read_to_string(&path).unwrap()],
        "the JSONL export copies the transcript before the session's refresh"
    );
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
    home.refuse(home.project().parent().unwrap());

    request_refresh(&mut app);

    assert!(view_text(&app).contains("message 2"));
    assert_eq!(
        status_text(&app),
        Some("Refresh failed: the session could not be read")
    );
}

/// A launch caches the session, then its directory stops listing and its
/// transcript grows: no refresh can bring the copy up to date.
#[test]
fn changed_session_under_unreadable_directory_reports_failed_refresh_once() {
    let home = ClaudeHome::new();
    let cache = super::claude_home::admitted_cache();
    let hidden = home.write_in("-tmp-unreadable", "hidden", 20, &["the hidden question"]);
    let environment = home.environment_with_cache(
        cache.path(),
        crate::history::ProcessIdentity::default(),
        Arc::new(TestClock::default()),
    );
    let mut app = launched_app(&environment, crate::history::RefreshOptions::default());
    home.refuse(hidden.parent().unwrap());
    home.append(&hidden, 30, "the newest question");
    app.selected = Some(0);
    app.enter_view_mode(80);

    let mut refreshes = 0;
    for _ in 0..4 {
        if app.is_session_refresh_requested() {
            serve_session_refresh(&mut app);
            refreshes += 1;
        }
        app.complete_pending_view_refresh(VIEWPORT, VIEWER_READ_LANDS);
    }

    assert_eq!(
        refreshes, 1,
        "the viewer doesn't request another refresh after one returns the older copy"
    );
    assert!(
        view_text(&app).contains("the hidden question"),
        "the viewer keeps the cached copy"
    );
    assert_eq!(
        status_text(&app),
        Some("Refresh failed: the session could not be read")
    );
}

/// A launch caches both sessions, then the directory of one stops listing.
#[test]
fn cached_session_under_unreadable_directory_stays_listed_and_opens_in_viewer() {
    let home = ClaudeHome::new();
    let cache = super::claude_home::admitted_cache();
    let hidden = home.write_in("-tmp-unreadable", "hidden", 20, &["the hidden question"]);
    home.write("listed", 10, &["the listed question"]);
    let environment = home.environment_with_cache(
        cache.path(),
        crate::history::ProcessIdentity::default(),
        Arc::new(TestClock::default()),
    );
    let options = crate::history::RefreshOptions::default();
    drop(launched_app(&environment, options));
    home.refuse(hidden.parent().unwrap());
    let names = |app: &App| -> Vec<String> {
        app.filtered()
            .iter()
            .map(|&index| app.conversations()[index].session_id.clone())
            .collect()
    };

    let mut app = launched_app(&environment, options);
    assert_eq!(
        names(&app),
        ["hidden", "listed"],
        "the launch lists the cached session under the unreadable directory"
    );
    serve_list_refresh(&mut app, TimeFilter::default());
    assert_eq!(
        names(&app),
        ["hidden", "listed"],
        "a list refresh keeps the cached session under the unreadable directory"
    );
    app.selected = Some(0);
    app.enter_view_mode(80);
    assert!(
        view_text(&app).contains("the hidden question"),
        "the viewer shows the cached copy"
    );
    request_refresh(&mut app);

    assert!(
        view_text(&app).contains("the hidden question"),
        "the viewer's refresh keeps the cached copy"
    );
    assert_eq!(
        status_text(&app),
        None,
        "the viewer's refresh reads the cached copy without reporting a failure"
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

#[test]
fn list_refresh_finishing_in_viewer_requests_open_sessions_refresh_at_once() {
    let home = ClaudeHome::new();
    let path = home.transcript("session");
    write_messages(&path, &numbered(0..3));
    let mut app = viewer_on(&home);
    let read_from_an_older_copy = app.conversations()[0].clone();
    app.begin_refresh().unwrap();

    app.finish_refresh(refresh_found(SessionChanges {
        updated: vec![UpdatedSession {
            row: read_from_an_older_copy,
            replaces_listed_row: true,
            next: crate::history::NextStep::CacheInBackground,
        }],
        ..Default::default()
    }));

    assert!(
        app.is_open_session_updating(),
        "the open session's refresh doesn't wait for the viewer to close"
    );
}

#[test]
fn rename_in_viewer_acts_on_row_its_finished_refresh_read() {
    let home = ClaudeHome::new();
    let path = home.transcript("session");
    write_messages(&path, &numbered(0..3));
    let mut app = viewer_on(&home);
    write_messages(&path, &numbered(0..5));
    press_refresh(&mut app);

    app.handle_key(KeyCode::F(2), KeyModifiers::NONE, VIEWPORT);
    assert!(
        matches!(app.dialog_mode, DialogMode::Rename { .. }),
        "the rename key opens the name box behind a finished refresh"
    );
    for typed in "Renamed".chars() {
        app.handle_key(KeyCode::Char(typed), KeyModifiers::NONE, VIEWPORT);
    }
    app.handle_key(KeyCode::Enter, KeyModifiers::NONE, VIEWPORT);
    super::refresh_tests::serve_list_change(&mut app);
    app.handle_key(KeyCode::Esc, KeyModifiers::NONE, VIEWPORT);
    app.apply_finished_refresh();

    assert_eq!(
        app.conversations()[0].custom_title.as_deref(),
        Some("Renamed"),
        "the refresh read before the rename keeps the renamed row"
    );
}

#[test]
fn copy_out_of_date_again_on_next_frame_requests_another_refresh() {
    let home = ClaudeHome::new();
    let path = home.transcript("session");
    write_messages(&path, &numbered(0..3));
    let mut app = viewer_on(&home);
    write_messages(&path, &numbered(0..5));
    request_refresh(&mut app);
    write_messages(&path, &numbered(0..7));

    assert!(app.complete_pending_view_refresh(VIEWPORT, VIEWER_READ_LANDS));

    assert!(
        app.is_open_session_updating(),
        "the next frame requests a refresh after reading an out-of-date copy"
    );
}

/// `Ctrl+R` on a file opened directly outside every provider's session
/// roots, the background worker's read of it, then the next frame showing
/// that read.
fn press_refresh_outside_every_root(app: &mut App) {
    app.handle_key(KeyCode::Char('r'), KeyModifiers::CONTROL, VIEWPORT);
    app.serve_direct_file_read();
    app.complete_pending_view_refresh(VIEWPORT, VIEWER_READ_LANDS);
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

    let row = &app.conversations()[0];
    assert_eq!(row.subagents, vec![subagent]);
    assert_eq!(
        view(&app).publication,
        row.publication,
        "the view reads the refreshed row's copy"
    );
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

/// Session `a` of `home`, titled `Named session`, cached by a load of its
/// root through the returned environment. Returns its transcript.
fn cached_titled_session(home: &ClaudeHome) -> (PathBuf, Environment) {
    let transcript = home.write("a", 5, &["a question"]);
    let title = r#"{"type":"custom-title","customTitle":"Named session","sessionId":"a"}"#;
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&transcript)
        .unwrap();
    std::io::Write::write_all(&mut file, format!("{title}\n").as_bytes()).unwrap();
    let environment = home.environment();
    home.rows(&environment);
    (transcript, environment)
}

/// The spellings of `transcript` a user may type: discovery's, one through a
/// `..` component, and its canonical path when that differs from discovery's
/// (on Windows the verbatim `\\?\` form).
#[allow(clippy::disallowed_methods)]
fn typed_spellings(transcript: &Path) -> Vec<PathBuf> {
    let project = transcript.parent().unwrap();
    let mut spellings = vec![
        transcript.to_path_buf(),
        project
            .join("..")
            .join(project.file_name().unwrap())
            .join(transcript.file_name().unwrap()),
    ];
    let canonical = std::fs::canonicalize(transcript).unwrap();
    if canonical != transcript {
        spellings.push(canonical);
    }
    spellings
}

/// The file at `typed` opened directly in `environment`, its read shown.
fn opened_directly_as(typed: &Path, environment: Environment) -> App {
    let file = DirectFile::canonicalise(typed.to_path_buf(), environment.canonicaliser()).unwrap();
    let mut app = App::new_single_file(
        file,
        ToolDisplayMode::Hidden,
        false,
        KeyBindings::default(),
        environment,
    );
    app.serve_direct_file_read();
    app.complete_pending_view_refresh(VIEWPORT, VIEWER_READ_LANDS);
    app.check_view_resize(80, VIEWPORT);
    app
}

/// The viewer header's first line, as the frame draws it.
fn header_line(app: &App) -> String {
    header_rows(app, 1)
}

/// The frame's first `rows` rows, as it draws them, one line each.
fn header_rows(app: &App, rows: u16) -> String {
    let mut terminal =
        ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, VIEWPORT as u16)).unwrap();
    terminal
        .draw(|frame| crate::tui::ui::render(frame, app))
        .unwrap();
    let buffer = terminal.backend().buffer();
    (0..rows)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
                .trim_end()
                .to_owned()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn cached_session_opened_directly_shows_its_header_whatever_spelling() {
    let home = ClaudeHome::new();
    let (transcript, environment) = cached_titled_session(&home);
    let opened_from_list = viewer_on(&home);
    let expected = header_line(&opened_from_list);
    assert!(
        expected.contains("Named session"),
        "the header of the session opened from the list shows its title: {expected:?}"
    );

    for typed in typed_spellings(&transcript) {
        let app = opened_directly_as(&typed, environment.clone());

        assert_eq!(
            header_line(&app),
            expected,
            "the header shows the session's title, project and dates, opened as {typed:?}"
        );
    }
}

#[test]
fn refresh_of_cached_session_opened_directly_updates_viewer_whatever_spelling() {
    let home = ClaudeHome::new();
    let (transcript, environment) = cached_titled_session(&home);

    for (index, typed) in typed_spellings(&transcript).iter().enumerate() {
        let mut app = opened_directly_as(typed, environment.clone());
        let follow_up = format!("follow-up {index}");
        home.append(&transcript, 6 + index as u32, &follow_up);
        let messages = 2 + index;

        press_refresh(&mut app);

        assert!(
            view_text(&app).contains(&follow_up),
            "the viewer shows the refreshed message, opened as {typed:?}"
        );
        let counts: Vec<usize> = app
            .conversations()
            .iter()
            .map(|row| row.message_count)
            .collect();
        assert_eq!(
            counts,
            vec![messages],
            "the list's one row is the refreshed session, opened as {typed:?}"
        );
        assert!(
            header_line(&app).contains(&format!("{messages} messages")),
            "the header shows the refreshed message count, opened as {typed:?}"
        );
    }
}

/// The app viewing session `a` of `home`, whose unpublished row expects a
/// sub-agent that the cached copy lacks: `a`'s parent transcript is
/// unchanged since the copy was written, and the write that adds the
/// sub-agent has not landed.
fn viewer_over_unwritten_subagent(home: &ClaudeHome) -> App {
    let environment = home.environment();
    let transcript = home.write("a", 5, &["first question"]);
    let mut rows = home.rows(&environment);
    let subagent = home.write_subagent(&transcript, "worker", 6, &["sub-agent turn"]);
    rows[0].subagents = vec![subagent.clone()];
    rows[0].publication = crate::history::sqlite_cache::Publication::Unpublished {
        discovered: vec![subagent],
    };
    let mut app = app_reading(rows, environment);
    app.selected = Some(0);
    app.enter_view_mode(80);
    app
}

#[test]
fn viewer_over_copy_lacking_the_rows_sub_agent_shows_updating() {
    let home = ClaudeHome::new();

    let app = viewer_over_unwritten_subagent(&home);

    assert!(
        view_text(&app).contains("first question"),
        "the viewer shows the copy from before the sub-agent"
    );
    assert_eq!(
        app.open_session_status(),
        Some(OpenSessionStatus::Updating),
        "the copy lacks the row's sub-agent, so the viewer shows it as out of date"
    );
}

/// Another rearview publishes a copy of session `a` with a sub-agent its
/// listed row lacks: the viewer and the export read that copy as current.
#[test]
fn viewer_and_export_over_copy_newer_than_the_row_read_it_as_current() {
    let home = ClaudeHome::new();
    let environment = home.environment();
    let transcript = home.write("a", 5, &["first question"]);
    let rows = home.rows(&environment);
    home.write_subagent(&transcript, "worker", 6, &["sub-agent turn"]);
    home.append(&transcript, 7, "a follow-up");
    home.rows(&home.environment());
    let mut app = app_reading(rows, environment);
    app.selected = Some(0);
    app.enter_view_mode(80);

    assert!(
        view_text(&app).contains("a follow-up"),
        "the viewer shows the newer copy"
    );
    assert_eq!(
        app.open_session_status(),
        None,
        "the viewer doesn't mark the newer copy as out of date"
    );
    assert_eq!(app.conversations()[0].message_count, 1);
    assert!(
        header_line(&app).contains("3 messages"),
        "the header counts the newer copy's follow-up and sub-agent turn: {}",
        header_line(&app)
    );
    assert!(
        app.current_export_entries().is_some(),
        "the export reads the newer copy as current"
    );
}

/// Another rearview publishes a copy of session `a` with a message its
/// listed row lacks and the row's sub-agents: the viewer's header shows the
/// copy's catalogue row.
#[test]
fn viewer_over_newer_copy_with_the_rows_sub_agents_shows_the_copys_header() {
    let home = ClaudeHome::new();
    let environment = home.environment();
    let transcript = home.write("a", 5, &["first question"]);
    let rows = home.rows(&environment);
    home.append(&transcript, 6, "a follow-up");
    home.rows(&home.environment());
    let mut app = app_reading(rows, environment);
    app.selected = Some(0);
    app.enter_view_mode(80);

    assert!(
        view_text(&app).contains("a follow-up"),
        "the viewer shows the newer copy"
    );
    assert_eq!(app.conversations()[0].message_count, 1);
    assert!(
        header_line(&app).contains("2 messages"),
        "the header counts the newer copy's follow-up: {}",
        header_line(&app)
    );
}

/// A Codex thread renamed in `session_index.jsonl` after its load: the list
/// refresh retitles its row, and the viewer's header shows the new title
/// over the copy's catalogue row, which holds the parse's.
#[test]
fn viewer_over_codex_thread_renamed_in_its_index_shows_the_new_name() {
    let fixture = crate::history::provider::contract_tests::registered_fixtures()
        .into_iter()
        .find(|fixture| fixture.provider().source() == Source::Codex)
        .unwrap();
    let home = tempfile::tempdir().unwrap();
    fixture.write_session(home.path(), fixture.ids().session);
    let storage =
        crate::history::provider::contract_tests::kept_storage_under(fixture, home.path());
    let cache = super::claude_home::admitted_cache();
    let environment = test_environment(vec![storage], Some(cache.path()));
    let rows = rows_of(&environment, storage);
    let session_id = rows[0].session_id.clone();
    let mut app = app_reading(rows, environment);
    std::fs::write(
        home.path().join("session_index.jsonl"),
        serde_json::json!({
            "id": session_id,
            "thread_name": "Renamed thread",
            "updated_at": "2026-08-19T10:00:00Z",
        })
        .to_string()
            + "\n",
    )
    .unwrap();

    serve_list_refresh(&mut app, crate::time_filter::TimeFilter::default());
    app.selected = Some(0);
    app.enter_view_mode(80);

    assert!(
        header_line(&app).contains("Renamed thread"),
        "the header shows the name in the index: {}",
        header_line(&app)
    );
}

#[test]
fn every_reader_finds_session_by_name_renamed_outside_its_transcript() {
    // Each agent's list runs on a thread of its own: a list marks its
    // thread as the drawing thread, and the drawing thread doesn't parse a
    // transcript.
    let wrong = EXTERNAL_TITLE_RENAMES
        .into_iter()
        .flat_map(|rename| [(rename, Launch::OverCache), (rename, Launch::OnEmptyCache)])
        .flat_map(|(rename, launch)| {
            std::thread::spawn(move || renamed_session_misses(rename, launch))
                .join()
                .unwrap()
        })
        .collect::<Vec<_>>();
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}

/// The rows a list launches with.
#[derive(Clone, Copy, Debug)]
enum Launch {
    /// Read from the copy an earlier load wrote.
    OverCache,
    /// Parsed by the list's own first load.
    OnEmptyCache,
}

/// The misses [`every_reader_finds_session_by_name_renamed_outside_its_transcript`]
/// reports for one rename after `launch`. The second check after a
/// transcript append catches a resumed read that brings back the old name.
fn renamed_session_misses(
    (source, first, second): (Source, Naming, Naming),
    launch: Launch,
) -> Vec<String> {
    use crate::history::sqlite_cache::admitted_reads;
    let fixture = crate::history::provider::contract_tests::registered_fixtures()
        .into_iter()
        .find(|fixture| fixture.provider().source() == source)
        .unwrap();
    let case = format!("{source:?} {first:?} then {second:?}, {launch:?}");
    let home = tempfile::tempdir().unwrap();
    let locator = fixture.write_session(home.path(), fixture.ids().session);
    name_session(source, &locator, "Zebrafish plans", first);
    let storage =
        crate::history::provider::contract_tests::kept_storage_under(fixture, home.path());
    let cache = super::claude_home::admitted_cache();
    let environment = test_environment(vec![storage], Some(cache.path()));
    let mut rows = rows_of(&environment, storage);
    if let Launch::OverCache = launch {
        rows = rows_of(&environment, storage);
    }
    let body_word = rows[0]
        .preview_first
        .split(|ch: char| !ch.is_alphabetic())
        .find(|word| word.len() > 3)
        .expect("the session's text holds a word")
        .to_lowercase();
    name_session(source, &locator, "Yakety release", second);
    let mut app = app_reading(rows, environment);
    let reads_before = admitted_reads::count(&locator);

    serve_list_refresh(&mut app, crate::time_filter::TimeFilter::default());

    let mut wrong = Vec::new();
    if admitted_reads::count(&locator) != reads_before {
        wrong.push(format!("{case}: the refresh doesn't read the transcript"));
    }
    assert_eq!(
        app.conversations[0].cached_session.is_some(),
        matches!(launch, Launch::OverCache),
        "{case}: the list's row reads the cache, or holds its parse's texts in memory"
    );
    wrong.extend(
        reader_misses(&app, &body_word)
            .into_iter()
            .map(|miss| format!("{case}: {miss}")),
    );

    append_copy_of_last_line(&locator);
    serve_list_refresh(&mut app, crate::time_filter::TimeFilter::default());
    wrong.extend(
        reader_misses(&app, &body_word)
            .into_iter()
            .map(|miss| format!("{case}, after the transcript gains a line: {miss}")),
    );
    wrong
}

/// The misses of every reader of `app`'s list, read off the drawing thread
/// as the list's search worker reads the cache. The session's text holds
/// `body_word`.
fn reader_misses(app: &App, body_word: &str) -> Vec<String> {
    let (conversations, searchable) = (&app.conversations, &app.searchable);
    std::thread::scope(|scope| {
        scope
            .spawn(|| {
                let mut misses = list_search_misses(conversations, searchable, body_word);
                misses.extend(other_reader_misses(conversations));
                misses
            })
            .join()
            .unwrap()
    })
}

fn append_copy_of_last_line(locator: &Path) {
    let text = std::fs::read_to_string(locator).unwrap();
    let last = text.lines().last().expect("the transcript holds a line");
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(locator)
        .unwrap();
    let separator = if text.ends_with('\n') { "" } else { "\n" };
    std::io::Write::write_all(&mut file, format!("{separator}{last}\n").as_bytes()).unwrap();
}

/// The misses of the list's search of `conversations`, the refreshed list,
/// for its session renamed from "Zebrafish plans" to "Yakety release". The
/// session's text holds `body_word`.
fn list_search_misses(
    conversations: &[Conversation],
    searchable: &[crate::search::SearchableConversation],
    body_word: &str,
) -> Vec<String> {
    let mut wrong = Vec::new();
    let in_memory = crate::search::SearchTextsInMemory::read(conversations);
    let without_texts = crate::search::SearchTextsInMemory::default();
    let name_queries = [("yakety".to_owned(), 1), ("zebrafish".to_owned(), 0)];
    let body_query = (format!("yakety {body_word}"), 1);
    // A row without held texts is searched on its title, summary and
    // project name: whether its body word matches depends on its summary.
    for (texts, read, queries) in [
        (
            &in_memory,
            "with the texts held in memory",
            [name_queries.as_slice(), std::slice::from_ref(&body_query)].concat(),
        ),
        (&without_texts, "without held texts", name_queries.to_vec()),
    ] {
        for (query, expected) in queries {
            let found = crate::search::try_debug_search(
                conversations,
                searchable,
                texts,
                &query,
                chrono::Local::now(),
                |_| true,
            )
            .unwrap()
            .results
            .len();
            if found != expected {
                wrong.push(format!(
                    "{query:?} finds {expected} session(s) read {read}, not {found}"
                ));
            }
        }
    }
    wrong
}

/// The misses of the readers other than the list's search, for the session
/// of `conversations` renamed from "Zebrafish plans" to "Yakety release":
/// the list's evidence and context lines, `agent search`'s ranking and exact
/// filter, the agent's semantic route and the semantic search's exact
/// preview.
fn other_reader_misses(conversations: &[Conversation]) -> Vec<String> {
    use crate::search::lexical::{RetrievalTerms, precompute_agent_search_text, try_agent_search};
    use crate::search::preview::{
        HighlightQuery, build_context_segments_from_ranges, build_literal_context_segments,
    };
    use crate::search::query::ParsedQuery;
    let mut wrong = Vec::new();
    let row = &conversations[0];
    let in_memory = crate::search::SearchTextsInMemory::read(conversations);
    for (texts, read) in [
        (in_memory.get(row), "with the texts held in memory"),
        (None, "without held texts"),
    ] {
        let evidence = crate::search::evidence::build_lexical_evidence(
            row,
            texts,
            &ParsedQuery::parse("yakety"),
        );
        let line = evidence.as_ref().and_then(|evidence| {
            build_context_segments_from_ranges(&evidence.texts(row), &evidence.context_ranges, 80)
        });
        if !line.as_deref().is_some_and(|line| line.contains("Yakety")) {
            wrong.push(format!(
                "the evidence read {read} shows the new name: {line:?}"
            ));
        }
    }
    let line =
        build_literal_context_segments(row, &row.preview, &HighlightQuery::parse("\"yakety\""), 80);
    if !line.as_deref().is_some_and(|line| line.contains("Yakety")) {
        wrong.push(format!(
            "the literal context line shows the new name: {line:?}"
        ));
    }
    let unwindowed = crate::time_filter::TimeFilter::default();
    let agent_texts = precompute_agent_search_text(conversations, &unwindowed);
    for (query, expected) in [("yakety", 1), ("zebrafish", 0)] {
        let found = try_agent_search(
            conversations,
            &agent_texts,
            query,
            RetrievalTerms::WordsAndLiterals,
            chrono::Local::now(),
            |_| true,
            crate::search::ScanScope::AllWorkspaces,
        )
        .unwrap()
        .ranked
        .len();
        if found != expected {
            wrong.push(format!(
                "agent search {query:?} finds {expected} session(s), not {found}"
            ));
        }
    }
    let every_row = (0..conversations.len()).collect::<Vec<_>>();
    for (phrase, expected) in [("yakety release", 1), ("zebrafish", 0)] {
        let found = crate::agent::service::lexically_rank_scoped(
            conversations,
            phrase,
            crate::agent::service::ScopedSessions {
                indices: &every_row,
                scope: crate::agent::search::AgentSearchScope::Global,
            },
            crate::search::mode::SearchMode::Exact,
            &unwindowed,
            &crate::clock::TestClock::default(),
        )
        .unwrap()
        .ranked
        .len();
        if found != expected {
            wrong.push(format!(
                "exact agent search {phrase:?} finds {expected} session(s), not {found}"
            ));
        }
    }
    let content = conversations[0].load_held_content().unwrap().unwrap().row;
    let route = content.semantic_route_text().to_lowercase();
    if !route.contains("yakety") || route.contains("zebrafish") {
        wrong.push(format!(
            "the semantic route text holds the new name, not the old: {route:?}"
        ));
    }
    let quoted = ParsedQuery::parse("\"yakety\"");
    let evidence =
        crate::search::build_session_list_evidence(conversations, &in_memory, &[0], &quoted);
    let preview =
        super::exact_literal::exact_literal_results(conversations, &evidence, &[0], &quoted)
            .remove(&0)
            .map(|metadata| metadata.explanation.evidence_preview);
    if !preview
        .as_deref()
        .is_some_and(|preview| preview.contains("Yakety"))
    {
        wrong.push(format!(
            "the semantic search's exact preview shows the new name: {preview:?}"
        ));
    }
    wrong
}

/// Every session `storage` stores, as a load through `environment` lists
/// them.
fn rows_of(
    environment: &crate::history::Environment,
    storage: &'static dyn crate::history::provider::SessionStorage,
) -> Vec<Conversation> {
    let operation = crate::history::environment::test_support::waiting_operation(environment);
    let rows = crate::history::provider::load_sessions(
        environment,
        storage,
        crate::history::provider::LoadOptions::default(),
        &operation,
        &mut |_, _| {},
    )
    .unwrap()
    .conversations;
    operation.finish().unwrap();
    rows
}

/// `Ctrl+R` over session `a`, whose transcript gained a summary too long
/// for the header's first line: the layout gives the header the line the
/// summary is drawn on.
#[test]
fn viewer_refresh_that_adds_a_long_summary_lays_out_its_header_line() {
    let home = ClaudeHome::new();
    let transcript = home.write("a", 5, &["first question"]);
    let mut app = viewer_on(&home);
    let summary = "a summary long enough to need the header's second line ".repeat(3);
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&transcript)
        .unwrap();
    std::io::Write::write_all(
        &mut file,
        format!(
            "{}\n",
            serde_json::json!({"type": "summary", "summary": summary, "leafUuid": "leaf"})
        )
        .as_bytes(),
    )
    .unwrap();
    drop(file);

    press_refresh(&mut app);

    let area = ratatui::layout::Rect::new(0, 0, 100, VIEWPORT as u16);
    let layout = crate::tui::ui::view_layout_rects(area, &app, view(&app));
    assert_eq!(
        layout.header.height, 3,
        "the header's height with the summary on its own line"
    );
    assert!(
        header_rows(&app, 3).contains("a summary long enough"),
        "the header draws the summary: {}",
        header_rows(&app, 3)
    );
}

/// `Ctrl+R` over session `a`, whose transcript was rewritten, so the
/// refresh reads it whole, and whose write fails: the viewer shows the
/// older copy, and its header counts that copy's messages, not the parse's.
#[test]
fn viewer_refresh_whose_whole_write_fails_shows_older_copys_header() {
    let home = ClaudeHome::new();
    home.write("a", 5, &["first question"]);
    let mut app = viewer_on(&home);
    home.write("a", 5, &["rewritten question", "another question"]);
    let _failing = crate::history::sqlite_cache::connection::test_hooks::fail_the_first_data_commit(
        app.environment.cache_base().path(),
        || {},
    );

    press_refresh(&mut app);

    assert!(
        view_text(&app).contains("first question"),
        "the viewer shows the older copy: {}",
        view_text(&app)
    );
    assert!(
        header_line(&app).contains("1 message"),
        "the header counts the older copy's messages: {}",
        header_line(&app)
    );
}

/// `Ctrl+R` in the viewer over session `a`, whose transcript gained a
/// message: the header counts the refreshed row's messages, though the
/// list's row stays as it was until the viewer closes.
#[test]
fn viewer_refresh_shows_refreshed_rows_header() {
    let home = ClaudeHome::new();
    let environment = home.environment();
    let transcript = home.write("a", 5, &["first question"]);
    let rows = home.rows(&environment);
    let mut app = app_reading(rows, environment);
    app.selected = Some(0);
    app.enter_view_mode(80);
    home.append(&transcript, 6, "a follow-up");

    press_refresh(&mut app);

    assert!(view_text(&app).contains("a follow-up"));
    assert_eq!(app.conversations()[0].message_count, 1);
    assert!(
        header_line(&app).contains("2 messages"),
        "the header counts the refreshed row's messages: {}",
        header_line(&app)
    );
}

#[test]
fn export_over_copy_lacking_the_rows_sub_agent_is_not_current() {
    let home = ClaudeHome::new();
    let mut app = viewer_over_unwritten_subagent(&home);

    assert!(
        app.current_export_entries().is_none(),
        "the export doesn't present the older copy as current"
    );
}

/// The TUI load of three sessions and `Ctrl+R` in the viewer canonicalise
/// the cache directory and the root, not a path per session. The test keeps
/// every call the environment's canonicaliser received.
#[test]
fn load_and_ctrl_r_canonicalise_only_the_cache_directory_and_the_root() {
    use crate::history::provider::walk::test_support::CountingCanonicaliser;
    let home = ClaudeHome::new();
    let canonicaliser = Arc::new(CountingCanonicaliser::default());
    let environment = home
        .environment_on(Arc::new(TestClock::default()))
        .with_canonicaliser(canonicaliser.clone());
    let cache_directory = environment.cache_base().path().to_path_buf();
    let sessions: Vec<_> = ["a", "b", "c"]
        .iter()
        .enumerate()
        .map(|(index, name)| home.write(name, 5 + index as u32, &["question"]))
        .collect();
    let loaded = home.rows(&environment);
    let canonicalised_by_load = canonicaliser.canonicalised();
    let mut app = app_reading(loaded, environment);
    app.selected = Some(0);
    app.enter_view_mode(80);
    home.append(&sessions[2], 20, "follow-up");

    press_refresh(&mut app);

    let canonicalised_by_refresh =
        canonicaliser.canonicalised()[canonicalised_by_load.len()..].to_vec();
    let projects = home
        .storage()
        .roots_in(&crate::history::provider::RootEnvironment::of_process())
        .unwrap()[0]
        .path
        .clone();
    assert_eq!(
        canonicalised_by_load,
        [cache_directory, projects.clone()],
        "the load of three sessions canonicalises the cache directory and the root once each"
    );
    assert_eq!(
        canonicalised_by_refresh,
        [projects],
        "Ctrl+R canonicalises the root once"
    );
}

/// The file opened directly (`rearview <file>`) by its canonical path
/// canonicalises the root once, for choosing the storage and reading the
/// session, then the cache directory once. It doesn't canonicalise the file
/// or its directory.
#[test]
fn direct_file_by_canonical_path_canonicalises_only_the_root_and_the_cache_directory() {
    use crate::history::environment::test_support::waiting_operation;
    use crate::history::provider::walk::Canonicaliser;
    use crate::history::provider::walk::test_support::CountingCanonicaliser;
    let home = ClaudeHome::new();
    let transcript = home.write("a", 5, &["question"]);
    let canonicaliser = Arc::new(CountingCanonicaliser::default());
    let environment = home.environment().with_canonicaliser(canonicaliser.clone());
    let canonical = canonicaliser.canonicalise(&transcript).unwrap();
    let before = canonicaliser.canonicalised().len();
    let cache_directory = environment.cache_base().path().to_path_buf();
    let operation = waiting_operation(&environment);

    let read =
        crate::history::provider::open_direct_file(&environment, &canonical, false, &operation);

    operation.finish().unwrap();
    assert!(matches!(
        read,
        Ok(crate::history::provider::DirectFileRead::Session { .. })
    ));
    let projects = home
        .storage()
        .roots_in(&crate::history::provider::RootEnvironment::of_process())
        .unwrap()[0]
        .path
        .clone();
    assert_eq!(
        canonicaliser.canonicalised()[before..],
        [projects, cache_directory],
        "the root and the cache directory canonicalise once each"
    );
}
