//! The viewer's `/` search on its own thread: keys typed during a search
//! show at once, a superseded search stops, and a paste runs one search.

use super::interaction_tests::{
    app_with_tool_conversation, press, view_state, write_named_conversation,
};
use super::test_waits::{VIEWER_READ_LANDS, finish_view_search};
use super::view_state::VIEW_SEARCH_PAUSE;
use super::viewer_search::ViewerSearchWorker;
use super::*;
use crate::tui::viewer::find_matches_while;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use std::sync::{Mutex, mpsc};

/// Two matches of `cache rebuild`, and of `cache`.
const TWO_MATCHES: &str = "cache rebuild, then cache rebuild again";

/// One match of `cache rebuild`, below the first screen.
fn match_below_first_screen() -> String {
    format!("{}cache rebuild", "word ".repeat(400))
}

/// The queries a worker searched, in order, each with whether its search
/// finished.
type Searched = Arc<Mutex<Vec<(String, bool)>>>;

fn searched(searched: &Searched) -> Vec<(String, bool)> {
    searched.lock().unwrap().clone()
}

fn finished(query: &str) -> (String, bool) {
    (query.to_owned(), true)
}

fn stopped(query: &str) -> (String, bool) {
    (query.to_owned(), false)
}

/// The viewer of one message, `text`, searching through `worker`.
fn viewer_of(text: &str, worker: ViewerSearchWorker) -> (tempfile::TempDir, App) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.jsonl");
    write_named_conversation(&path, text);
    let mut app = app_with_tool_conversation(path, ToolDisplayMode::Hidden);
    app.viewer_search = Some(worker);
    (dir, app)
}

/// A worker running the viewer's search, recording each search.
fn recording_worker() -> (ViewerSearchWorker, Searched) {
    let searched = Searched::default();
    let record = Arc::clone(&searched);
    let worker = ViewerSearchWorker::spawn_with(Box::new(move |rows, query, keep_going| {
        let found = find_matches_while(rows, query, keep_going);
        record
            .lock()
            .unwrap()
            .push((query.to_owned(), found.is_some()));
        found
    }));
    (worker, searched)
}

/// The gate a [`gated_worker`] stops each search at, before its first row.
struct Gate {
    started: mpsc::Receiver<String>,
    release: mpsc::Sender<()>,
    returned: mpsc::Receiver<(String, bool)>,
}

impl Gate {
    /// The query of the next search to reach the gate.
    fn next_search(&self) -> String {
        self.started
            .recv_timeout(VIEWER_READ_LANDS)
            .expect("a search reaches the gate")
    }

    fn let_one_through(&self) {
        self.release.send(()).unwrap();
    }

    /// The query of the next search to return, and whether it finished.
    fn next_return(&self) -> (String, bool) {
        self.returned
            .recv_timeout(VIEWER_READ_LANDS)
            .expect("a search returns")
    }
}

/// A worker that stops each search at the returned gate. With
/// `honours_stop` false, a superseded search searches to its end and sends
/// its matches anyway.
fn gated_worker(honours_stop: bool) -> (ViewerSearchWorker, Gate) {
    let (started_tx, started) = mpsc::channel();
    let (release, released) = mpsc::channel::<()>();
    let (returned_tx, returned) = mpsc::channel();
    let worker = ViewerSearchWorker::spawn_with(Box::new(move |rows, query, keep_going| {
        let _ = started_tx.send(query.to_owned());
        let _ = released.recv();
        let found = if honours_stop {
            find_matches_while(rows, query, keep_going)
        } else {
            find_matches_while(rows, query, &|| true)
        };
        let _ = returned_tx.send((query.to_owned(), found.is_some()));
        found
    }));
    let gate = Gate {
        started,
        release,
        returned,
    };
    (worker, gate)
}

fn type_into(app: &mut App, text: &str) {
    for c in text.chars() {
        press(app, KeyCode::Char(c));
    }
}

fn press_ctrl(app: &mut App, c: char) {
    app.handle_key(KeyCode::Char(c), KeyModifiers::CONTROL, 17);
}

/// The pause after typing passes, and the event loop sends the search.
fn pass_typing_pause(app: &mut App) {
    app.clock().sleep(VIEW_SEARCH_PAUSE);
    app.send_due_view_search();
}

fn is_paused(app: &App) -> bool {
    matches!(
        view_state(app).pending_search,
        Some(PendingViewSearch::Paused { .. })
    )
}

fn is_running(app: &App) -> bool {
    matches!(
        view_state(app).pending_search,
        Some(PendingViewSearch::Running { .. })
    )
}

/// The rows of the frame the app draws, without trailing spaces.
fn frame_rows(app: &App) -> Vec<String> {
    let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
    terminal
        .draw(|frame| crate::tui::ui::render(frame, app))
        .unwrap();
    let buffer = terminal.backend().buffer();
    (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
                .trim_end()
                .to_owned()
        })
        .collect()
}

/// The search line in the frame the app draws: the row starting with `  /`.
fn search_line(app: &App) -> String {
    frame_rows(app)
        .into_iter()
        .find(|row| row.starts_with("  /"))
        .expect("the frame shows the search line")
}

/// Let the stopped search return, and check that it stopped at its first
/// check; its matches, if any arrived, aren't shown.
fn assert_search_stopped(app: &mut App, gate: &Gate) {
    gate.let_one_through();
    assert_eq!(
        gate.next_return(),
        stopped("cache"),
        "the running search stops at its first check"
    );
    app.receive_view_search_results(Duration::ZERO);
}

#[test]
fn keys_typed_during_search_show_at_once_and_stop_that_search() {
    let (worker, gate) = gated_worker(true);
    let (_dir, mut app) = viewer_of(TWO_MATCHES, worker);
    press(&mut app, KeyCode::Char('/'));
    type_into(&mut app, "cache");
    pass_typing_pause(&mut app);
    assert_eq!(gate.next_search(), "cache");

    type_into(&mut app, " rebuild");

    assert_eq!(
        view_state(&app).search_query,
        "cache rebuild",
        "the keys typed while the search runs edit the query"
    );
    assert_eq!(
        search_line(&app),
        "  /cache rebuild",
        "the search line shows the edited query"
    );
    gate.let_one_through();
    assert_eq!(
        gate.next_return(),
        stopped("cache"),
        "the edit stops the running search at its first check"
    );
    pass_typing_pause(&mut app);
    assert_eq!(gate.next_search(), "cache rebuild");
    gate.let_one_through();
    finish_view_search(&mut app);
    assert_eq!(gate.next_return(), finished("cache rebuild"));
    assert_eq!(view_state(&app).search_matches.len(), 2);
}

#[test]
fn paste_runs_single_search_once_typing_pauses() {
    let (worker, searches) = recording_worker();
    let (_dir, mut app) = viewer_of(TWO_MATCHES, worker);
    press(&mut app, KeyCode::Char('/'));
    let just_short_of_pause = VIEW_SEARCH_PAUSE - Duration::from_millis(1);

    type_into(&mut app, "cache");
    app.clock().sleep(just_short_of_pause);
    type_into(&mut app, " rebuild");
    app.clock().sleep(just_short_of_pause);
    app.send_due_view_search();

    assert!(
        is_paused(&app),
        "each key restarts the pause, so the search is still paused"
    );
    finish_view_search(&mut app);
    assert_eq!(searched(&searches), [finished("cache rebuild")]);
    assert_eq!(view_state(&app).search_matches.len(), 2);
}

#[test]
fn clearing_query_clears_matches_without_search() {
    for clear in ['u', 'w'] {
        let (worker, searches) = recording_worker();
        let (_dir, mut app) = viewer_of(TWO_MATCHES, worker);
        press(&mut app, KeyCode::Char('/'));
        type_into(&mut app, "cache");
        finish_view_search(&mut app);

        press_ctrl(&mut app, clear);

        let state = view_state(&app);
        assert!(
            state.search_matches.is_empty(),
            "Ctrl+{clear} clears the matches"
        );
        assert_eq!(
            state.pending_search, None,
            "Ctrl+{clear} doesn't schedule a search"
        );
        assert_eq!(
            searched(&searches),
            [finished("cache")],
            "Ctrl+{clear} doesn't send a search"
        );
    }
}

#[test]
fn enter_during_typing_pause_searches_at_once_and_stays_on_matches() {
    let (worker, _searches) = recording_worker();
    let (_dir, mut app) = viewer_of(TWO_MATCHES, worker);
    press(&mut app, KeyCode::Char('/'));
    type_into(&mut app, "cache");

    press(&mut app, KeyCode::Enter);

    assert!(is_running(&app), "the search is sent without the pause");
    assert_eq!(view_state(&app).search_mode, ViewSearchMode::Active);
    finish_view_search(&mut app);
    let state = view_state(&app);
    assert_eq!(state.search_mode, ViewSearchMode::Active);
    assert_eq!(state.search_matches.len(), 2);
}

#[test]
fn enter_closes_search_once_its_search_doesnt_find_any_match() {
    let (worker, _searches) = recording_worker();
    let (_dir, mut app) = viewer_of(TWO_MATCHES, worker);
    press(&mut app, KeyCode::Char('/'));
    type_into(&mut app, "zzqqxj");
    press(&mut app, KeyCode::Enter);

    finish_view_search(&mut app);

    assert_eq!(view_state(&app).search_mode, ViewSearchMode::Off);
}

#[test]
fn status_bar_shows_searching_after_enter_while_search_runs() {
    let (worker, gate) = gated_worker(true);
    let (_dir, mut app) = viewer_of(TWO_MATCHES, worker);
    press(&mut app, KeyCode::Char('/'));
    type_into(&mut app, "cache");
    press(&mut app, KeyCode::Enter);
    assert_eq!(gate.next_search(), "cache");

    app.clock().sleep(Duration::from_millis(100));

    assert!(
        frame_rows(&app)
            .iter()
            .any(|row| row.contains("│  next  Nprev  ⠙ searching  Esc")),
        "the status bar shows the running search in place of the count: {:#?}",
        frame_rows(&app)
    );
    gate.let_one_through();
}

#[test]
fn next_match_key_before_matches_arrive_doesnt_move() {
    let (worker, gate) = gated_worker(true);
    let (_dir, mut app) = viewer_of(TWO_MATCHES, worker);
    press(&mut app, KeyCode::Char('/'));
    type_into(&mut app, "cache");
    press(&mut app, KeyCode::Enter);
    assert_eq!(gate.next_search(), "cache");

    press(&mut app, KeyCode::Char('n'));
    gate.let_one_through();
    finish_view_search(&mut app);

    let state = view_state(&app);
    assert_eq!(state.search_matches.len(), 2);
    assert_eq!(
        state.current_match_index, 0,
        "`n` pressed before the matches arrived isn't kept for them"
    );
}

#[test]
fn view_moved_while_search_runs_stays_where_it_was_moved() {
    let (worker, gate) = gated_worker(true);
    let (_dir, mut app) = viewer_of(&match_below_first_screen(), worker);
    press(&mut app, KeyCode::Char('/'));
    type_into(&mut app, "cache rebuild");
    press(&mut app, KeyCode::Enter);
    assert_eq!(gate.next_search(), "cache rebuild");

    for _ in 0..3 {
        press(&mut app, KeyCode::Down);
    }
    let moved_to = view_state(&app).scroll_offset;
    gate.let_one_through();
    finish_view_search(&mut app);

    let state = view_state(&app);
    assert_eq!(state.search_matches.len(), 1);
    assert_eq!(
        (moved_to, state.scroll_offset),
        (3, 3),
        "the view stays where the user scrolled while the search ran"
    );
}

#[test]
fn older_search_matches_arent_shown_and_newest_scroll_view_to_first_match() {
    let (worker, gate) = gated_worker(false);
    let (_dir, mut app) = viewer_of(&match_below_first_screen(), worker);
    press(&mut app, KeyCode::Char('/'));
    type_into(&mut app, "cache");
    pass_typing_pause(&mut app);
    assert_eq!(gate.next_search(), "cache");
    type_into(&mut app, " rebuild");
    pass_typing_pause(&mut app);
    assert!(
        gate.started.try_recv().is_err(),
        "the newer search starts only once the running search returns"
    );

    gate.let_one_through();
    assert_eq!(gate.next_return(), finished("cache"));
    assert_eq!(gate.next_search(), "cache rebuild");
    app.receive_view_search_results(VIEWER_READ_LANDS);

    let state = view_state(&app);
    assert!(
        state.search_matches.is_empty() && state.scroll_offset == 0,
        "the older search's matches aren't shown"
    );
    assert!(is_running(&app), "the newest search still runs");
    gate.let_one_through();
    finish_view_search(&mut app);
    let state = view_state(&app);
    assert_eq!(state.search_matches.len(), 1);
    assert_eq!(state.scroll_offset, state.search_matches[0].first_row());
    assert!(
        state.scroll_offset > 0,
        "the view scrolls to the match below the first screen"
    );
}

#[test]
fn rerender_while_matches_show_searches_again_without_pause() {
    let (worker, searches) = recording_worker();
    let (_dir, mut app) = viewer_of(TWO_MATCHES, worker);
    press(&mut app, KeyCode::Char('/'));
    type_into(&mut app, "cache");
    press(&mut app, KeyCode::Enter);
    finish_view_search(&mut app);

    app.check_view_resize(60, 17);

    assert!(is_running(&app), "the search is sent without the pause");
    assert!(
        view_state(&app).search_matches.is_empty(),
        "the old rows' matches are cleared"
    );
    finish_view_search(&mut app);
    assert_eq!(searched(&searches), [finished("cache"), finished("cache")]);
    assert_eq!(view_state(&app).search_matches.len(), 2);
}

/// Three matches of `cache rebuild`, each a screen or more below the one
/// before, then four screens of other words.
fn matches_screens_apart() -> String {
    let screen = "word ".repeat(400);
    let screens_after = screen.repeat(4);
    format!("cache rebuild {screen}cache rebuild {screen}cache rebuild {screens_after}")
}

/// The viewer of [`matches_screens_apart`] showing its matches.
fn viewer_showing_matches() -> (tempfile::TempDir, App) {
    let (worker, _searches) = recording_worker();
    let (dir, mut app) = viewer_of(&matches_screens_apart(), worker);
    press(&mut app, KeyCode::Char('/'));
    type_into(&mut app, "cache rebuild");
    press(&mut app, KeyCode::Enter);
    finish_view_search(&mut app);
    (dir, app)
}

/// The view rendered again at another width, and the search of its new rows
/// run to its end. Returns the top row before the matches arrived.
fn rendered_again_at_new_width(app: &mut App) -> usize {
    app.check_view_resize(60, 17);
    let top_row = view_state(app).scroll_offset;
    finish_view_search(app);
    top_row
}

#[test]
fn search_of_rows_rendered_again_selects_first_match_at_or_below_top_row() {
    let (_dir, mut app) = viewer_showing_matches();
    let first_match_row = view_state(&app).search_matches[0].first_row();
    while view_state(&app).scroll_offset <= first_match_row {
        press(&mut app, KeyCode::Down);
    }

    let top_row = rendered_again_at_new_width(&mut app);

    let state = view_state(&app);
    let rows: Vec<usize> = state
        .search_matches
        .iter()
        .map(|found| found.first_row())
        .collect();
    assert_eq!(rows.len(), 3);
    assert!(
        rows[0] < top_row && rows[1] >= top_row,
        "the first match starts above the top row, the second at or below it: {rows:?} from {top_row}"
    );
    assert_eq!(
        state.current_match_index, 1,
        "the selected match is the first at or below the top row"
    );
    assert_eq!(
        state.scroll_offset, top_row,
        "the view doesn't scroll when the matches arrive"
    );
}

#[test]
fn search_of_rows_rendered_again_selects_last_match_when_every_match_is_above_top_row() {
    let (_dir, mut app) = viewer_showing_matches();
    press(&mut app, KeyCode::End);

    let top_row = rendered_again_at_new_width(&mut app);

    let state = view_state(&app);
    assert!(
        state
            .search_matches
            .iter()
            .all(|found| found.first_row() < top_row),
        "every match starts above the top row {top_row}"
    );
    assert_eq!(
        state.current_match_index,
        state.search_matches.len() - 1,
        "the selected match is the last"
    );
    assert_eq!(state.scroll_offset, top_row);
}

#[test]
fn search_line_shows_searching_once_search_runs_past_spinner_delay() {
    let (worker, gate) = gated_worker(true);
    let (_dir, mut app) = viewer_of(TWO_MATCHES, worker);
    press(&mut app, KeyCode::Char('/'));
    type_into(&mut app, "cache");
    assert_eq!(
        search_line(&app),
        "  /cache",
        "the search line shows the query alone during the pause"
    );
    pass_typing_pause(&mut app);
    assert_eq!(gate.next_search(), "cache");
    assert_eq!(
        search_line(&app),
        "  /cache",
        "the search line shows the query alone before the search has run 100 ms"
    );

    app.clock().sleep(Duration::from_millis(100));
    assert_eq!(search_line(&app), "  /cache ⠙ searching");
    app.clock().sleep(Duration::from_secs(1));
    assert_eq!(search_line(&app), "  /cache ⠸ searching 1s");

    gate.let_one_through();
    finish_view_search(&mut app);
    assert_eq!(search_line(&app), "  /cache (1/2)");
}

#[test]
fn closing_search_during_search_stops_it() {
    for after_enter in [false, true] {
        let (worker, gate) = gated_worker(true);
        let (_dir, mut app) = viewer_of(TWO_MATCHES, worker);
        press(&mut app, KeyCode::Char('/'));
        type_into(&mut app, "cache");
        if after_enter {
            press(&mut app, KeyCode::Enter);
        } else {
            pass_typing_pause(&mut app);
        }
        assert_eq!(gate.next_search(), "cache");

        press(&mut app, KeyCode::Esc);

        assert_search_stopped(&mut app, &gate);
        let state = view_state(&app);
        assert_eq!(
            (&state.search_mode, state.search_matches.len()),
            (&ViewSearchMode::Off, 0),
            "the search is off without matches (after Enter: {after_enter})"
        );
        assert_eq!(state.pending_search, None);
    }
}

#[test]
fn opening_search_again_during_search_stops_it_without_count() {
    let (worker, gate) = gated_worker(true);
    let (_dir, mut app) = viewer_of(TWO_MATCHES, worker);
    press(&mut app, KeyCode::Char('/'));
    type_into(&mut app, "cache");
    press(&mut app, KeyCode::Enter);
    assert_eq!(gate.next_search(), "cache");

    press(&mut app, KeyCode::Char('/'));

    assert_search_stopped(&mut app, &gate);
    assert_eq!(
        search_line(&app),
        "  /",
        "the search line shows an empty query without a count"
    );
}

#[test]
fn leaving_view_during_search_stops_it() {
    let (worker, gate) = gated_worker(true);
    let (_dir, mut app) = viewer_of(TWO_MATCHES, worker);
    press(&mut app, KeyCode::Char('/'));
    type_into(&mut app, "cache");
    press(&mut app, KeyCode::Enter);
    assert_eq!(gate.next_search(), "cache");

    press(&mut app, KeyCode::Char('q'));

    assert!(matches!(app.app_mode(), AppMode::List));
    assert_search_stopped(&mut app, &gate);
}

#[test]
fn dropping_app_during_search_stops_it() {
    let (worker, gate) = gated_worker(true);
    let (_dir, mut app) = viewer_of(TWO_MATCHES, worker);
    press(&mut app, KeyCode::Char('/'));
    type_into(&mut app, "cache");
    press(&mut app, KeyCode::Enter);
    assert_eq!(gate.next_search(), "cache");

    drop(app);

    gate.let_one_through();
    assert_eq!(
        gate.next_return(),
        stopped("cache"),
        "the running search stops at its first check"
    );
}

#[test]
#[should_panic(expected = "the search panicked")]
fn search_thread_panic_resumes_where_matches_are_received() {
    let worker = ViewerSearchWorker::spawn_with(Box::new(|_, _, _| panic!("the search panicked")));
    let (_dir, mut app) = viewer_of(TWO_MATCHES, worker);
    press(&mut app, KeyCode::Char('/'));
    type_into(&mut app, "cache");
    press(&mut app, KeyCode::Enter);

    finish_view_search(&mut app);
}
