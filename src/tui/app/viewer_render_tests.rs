//! Renders of the open session on `viewer-render`: keys act on the shown
//! rows while a render runs, a newer request stops the running render, and a
//! finished render waits for the app to take it.

use super::interaction_tests::{app_with_tool_conversation, view_state, write_named_conversation};
use super::test_waits::{VIEWER_READ_LANDS, finish_view_render};
use super::viewer_render::ViewerRenderWorker;
use super::*;
use crate::tui::viewer::render_parsed_conversation_while;
use std::sync::mpsc;

const VIEWPORT: usize = 17;

/// What a [`gated_worker`] saw of one render: the request's modes, and
/// whether the render finished.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Seen {
    pub(super) tool_display: ToolDisplayMode,
    pub(super) show_timing: bool,
    pub(super) finished: bool,
}

/// The gate a [`gated_worker`] stops each render at, before its first entry.
pub(super) struct Gate {
    started: mpsc::Receiver<(ToolDisplayMode, bool, Option<String>)>,
    release: mpsc::Sender<()>,
    returned: mpsc::Receiver<Seen>,
}

impl Gate {
    /// The modes of the next render to reach the gate.
    pub(super) fn next_render(&self) -> (ToolDisplayMode, bool) {
        let (tool_display, show_timing, _) = self.next_render_on_thread();
        (tool_display, show_timing)
    }

    /// The modes of the next render to reach the gate, and the name of the
    /// thread it runs on.
    fn next_render_on_thread(&self) -> (ToolDisplayMode, bool, Option<String>) {
        self.started
            .recv_timeout(VIEWER_READ_LANDS)
            .expect("a render reaches the gate")
    }

    pub(super) fn let_one_through(&self) {
        self.release.send(()).unwrap();
    }

    /// What the next render to return saw.
    pub(super) fn next_return(&self) -> Seen {
        self.returned
            .recv_timeout(VIEWER_READ_LANDS)
            .expect("a render returns")
    }
}

/// A worker that stops each render at the returned gate.
pub(super) fn gated_worker() -> (ViewerRenderWorker, Gate) {
    let (started_tx, started) = mpsc::channel();
    let (release, released) = mpsc::channel::<()>();
    let (returned_tx, returned) = mpsc::channel();
    let worker =
        ViewerRenderWorker::spawn_with(Box::new(move |conversation, options, keep_going| {
            let thread = std::thread::current().name().map(str::to_owned);
            let _ = started_tx.send((options.tool_display, options.show_timing, thread));
            let _ = released.recv();
            let rendered = render_parsed_conversation_while(conversation, options, keep_going);
            let _ = returned_tx.send(Seen {
                tool_display: options.tool_display,
                show_timing: options.show_timing,
                finished: rendered.is_some(),
            });
            rendered
        }));
    let gate = Gate {
        started,
        release,
        returned,
    };
    (worker, gate)
}

/// The viewer of one long message in summary mode, rendering through
/// `worker`.
fn viewer_through(worker: ViewerRenderWorker) -> (tempfile::TempDir, App) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.jsonl");
    write_named_conversation(&path, &"word ".repeat(400));
    let mut app = app_with_tool_conversation(path, ToolDisplayMode::Hidden);
    app.viewer_render = Some(worker);
    (dir, app)
}

/// A key pressed without waiting for the render it requests.
pub(super) fn press_now(app: &mut App, code: KeyCode) {
    app.handle_key(code, KeyModifiers::empty(), VIEWPORT);
}

fn seen(tool_display: ToolDisplayMode, finished: bool) -> Seen {
    Seen {
        tool_display,
        show_timing: false,
        finished,
    }
}

#[test]
fn keys_scroll_shown_rows_while_render_runs() {
    let (worker, gate) = gated_worker();
    let (_dir, mut app) = viewer_through(worker);
    let shown = Arc::clone(&view_state(&app).rendered_lines);

    press_now(&mut app, KeyCode::Char('t'));
    assert_eq!(gate.next_render(), (ToolDisplayMode::Truncated, false));
    press_now(&mut app, KeyCode::Down);

    let state = view_state(&app);
    assert!(
        Arc::ptr_eq(&state.rendered_lines, &shown),
        "the shown rows stay while the render runs"
    );
    assert_eq!(state.scroll_offset, 1, "the shown rows scroll");
    gate.let_one_through();
    finish_view_render(&mut app, VIEWPORT);
    assert!(
        !Arc::ptr_eq(&view_state(&app).rendered_lines, &shown),
        "the render's rows show once it arrives"
    );
}

#[test]
fn display_key_pressed_twice_stops_first_render_and_shows_second() {
    let (worker, gate) = gated_worker();
    let (_dir, mut app) = viewer_through(worker);

    press_now(&mut app, KeyCode::Char('t'));
    assert_eq!(gate.next_render(), (ToolDisplayMode::Truncated, false));
    press_now(&mut app, KeyCode::Char('t'));
    gate.let_one_through();

    assert_eq!(
        gate.next_return(),
        seen(ToolDisplayMode::Truncated, false),
        "the second press stops the first render at its first check"
    );
    assert_eq!(gate.next_render(), (ToolDisplayMode::Full, false));
    gate.let_one_through();
    finish_view_render(&mut app, VIEWPORT);
    assert_eq!(gate.next_return(), seen(ToolDisplayMode::Full, true));
    assert_eq!(view_state(&app).tool_display, ToolDisplayMode::Full);
}

#[test]
fn render_runs_on_viewer_render_thread() {
    let (worker, gate) = gated_worker();
    let (_dir, mut app) = viewer_through(worker);

    press_now(&mut app, KeyCode::Char('t'));

    assert_eq!(
        gate.next_render_on_thread().2.as_deref(),
        Some("viewer-render")
    );
    gate.let_one_through();
    finish_view_render(&mut app, VIEWPORT);
}

#[test]
fn leaving_viewer_during_render_stops_it() {
    let (worker, gate) = gated_worker();
    let (_dir, mut app) = viewer_through(worker);
    press_now(&mut app, KeyCode::Char('t'));
    gate.next_render();

    press_now(&mut app, KeyCode::Char('q'));
    gate.let_one_through();

    assert!(matches!(app.app_mode(), AppMode::List));
    assert_eq!(
        gate.next_return(),
        seen(ToolDisplayMode::Truncated, false),
        "the render stops at its first check"
    );
}

#[test]
fn click_on_message_doesnt_render_again_and_keeps_selected_match() {
    let (worker, _gate) = gated_worker();
    let (_dir, mut app) = viewer_through(worker);
    for code in "/word".chars().map(KeyCode::Char).chain([KeyCode::Enter]) {
        press_now(&mut app, code);
    }
    super::test_waits::finish_view_search(&mut app);
    press_now(&mut app, KeyCode::Char('n'));
    if let AppMode::View(state) = &mut app.app_mode {
        state.message_nav_active = false;
    }
    let selected = view_state(&app).current_match_index;
    let frame = Rect::new(0, 0, 80, 30);
    let row = crate::tui::ui::view_layout_rects(frame, &app, view_state(&app))
        .content
        .y;

    let changed = app.handle_view_click(row, frame);

    assert!(changed, "the click focuses the message");
    assert!(
        !app.is_view_render_pending(),
        "a click that only moves the focus doesn't render the rows again"
    );
    assert_eq!(
        (selected, view_state(&app).current_match_index),
        (1, 1),
        "the selected match stays"
    );
}

/// The pass that takes the finished render waiting to be taken, as the
/// event loop's next pass does. It waits for the `viewer-render` thread to
/// send it: the render returns at the gate just before it is sent.
fn take_finished_render(app: &mut App) {
    app.receive_view_render(VIEWPORT, VIEWER_READ_LANDS);
}

/// Let a display key's render finish, without the app taking it.
fn finish_render_without_taking_it(app: &mut App, gate: &Gate) {
    press_now(app, KeyCode::Char('t'));
    gate.next_render();
    gate.let_one_through();
    assert_eq!(gate.next_return(), seen(ToolDisplayMode::Truncated, true));
}

#[test]
fn next_render_starts_once_taken_renders_rows_are_released() {
    let (worker, gate) = gated_worker();
    let (_dir, mut app) = viewer_through(worker);
    finish_render_without_taking_it(&mut app, &gate);
    let taken = app
        .viewer_render
        .as_mut()
        .unwrap()
        .take_rendered(VIEWER_READ_LANDS)
        .expect("the finished render is taken");
    let rows = Arc::new(taken.rendered_conversation.lines);
    let released_rows = Arc::downgrade(&rows);

    press_now(&mut app, KeyCode::Char('t'));
    app.viewer_render.as_ref().unwrap().release(rows);
    let next = gate.next_render();

    assert_eq!(next, (ToolDisplayMode::Full, false));
    assert!(
        released_rows.upgrade().is_none(),
        "the next render starts once the taken render's rows are released and freed"
    );
    gate.let_one_through();
    finish_view_render(&mut app, VIEWPORT);
}

#[test]
fn rows_replaced_by_render_are_freed_before_next_render_starts() {
    let (worker, gate) = gated_worker();
    let (_dir, mut app) = viewer_through(worker);
    let shown = Arc::downgrade(&view_state(&app).rendered_lines);
    press_now(&mut app, KeyCode::Char('t'));
    gate.next_render();
    gate.let_one_through();
    finish_view_render(&mut app, VIEWPORT);

    press_now(&mut app, KeyCode::Char('t'));
    gate.next_render();

    assert!(
        shown.upgrade().is_none(),
        "the replaced rows are freed before the next render starts"
    );
    gate.let_one_through();
    finish_view_render(&mut app, VIEWPORT);
}

#[test]
fn display_key_in_viewer_opened_again_renders_after_finished_render_of_closed_viewer() {
    let (worker, gate) = gated_worker();
    let (_dir, mut app) = viewer_through(worker);
    finish_render_without_taking_it(&mut app, &gate);
    press_now(&mut app, KeyCode::Char('q'));
    take_finished_render(&mut app);
    app.enter_view_mode(80);

    press_now(&mut app, KeyCode::Char('t'));

    assert_eq!(
        gate.next_render(),
        (ToolDisplayMode::Full, false),
        "the closed viewer's render, taken on the pass after it closed, doesn't block the next"
    );
    gate.let_one_through();
    finish_view_render(&mut app, VIEWPORT);
    assert_eq!(view_state(&app).tool_display, ToolDisplayMode::Full);
    assert!(!app.is_view_render_pending());
}

#[test]
fn dropping_app_while_finished_render_waits_to_be_taken_returns() {
    let (worker, gate) = gated_worker();
    let (_dir, mut app) = viewer_through(worker);
    finish_render_without_taking_it(&mut app, &gate);

    drop(app);
}

#[test]
#[should_panic(expected = "the render panicked")]
fn render_thread_panic_resumes_where_renders_are_taken() {
    let worker = ViewerRenderWorker::spawn_with(Box::new(|_, _, _| panic!("the render panicked")));
    let (_dir, mut app) = viewer_through(worker);

    press_now(&mut app, KeyCode::Char('t'));

    finish_view_render(&mut app, VIEWPORT);
}
