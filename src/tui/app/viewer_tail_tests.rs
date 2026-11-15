//! After a refresh of the open session, the viewer renders the tail on
//! `viewer-render` and puts its rows after the rows it keeps. The rows, the
//! reader's place, the badge, the count highlight and the search equal those
//! a whole render shows.

use super::claude_home::{ClaudeHome, admitted_cache, app_reading, viewer_lacks_copy};
use super::test_waits::{VIEWER_READ_LANDS, finish_view_render, finish_view_search};
use super::view_state::TAIL_ROWS_SEARCHED_WHILE_DRAWING;
use super::viewer_refresh_tests::{
    arrived_below, cached_titled_session, click_call, focus_message, long_call_line, numbered,
    opened_outside_every_root, press_refresh, press_refresh_outside_every_root,
    refresh_and_receive_read, request_refresh, tool_run_lines, user_line, view, view_text,
    viewer_in, viewer_on, viewer_on_clock, write_lines, write_messages,
};
use super::viewer_render_tests::{Extents, Gate, gated_worker, recording_worker};
use super::viewer_search_tests::gated_worker as gated_search_worker;
use super::*;
use crate::cache_file::test_support::TemporaryCacheDirectory;
use crate::clock::TestClock;
use crate::history::ProcessIdentity;
use crate::history::provider::contract_tests::{
    ProviderFixture, kept_storage_under, load_under, registered_fixtures,
};
use crate::tui::viewer::{RowsToRender, SearchMatch, find_matches, render_parsed_conversation};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

const VIEWPORT: usize = 10;

fn press(app: &mut App, code: KeyCode) {
    app.handle_key(code, KeyModifiers::empty(), VIEWPORT);
}

/// `app`, its renders recorded.
fn recorded(mut app: App) -> (App, Extents) {
    let (worker, extents) = recording_worker();
    app.viewer_render = Some(worker);
    (app, extents)
}

/// `app`, each of its renders stopped at the returned gate.
fn gated(mut app: App) -> (App, Gate) {
    let (worker, gate) = gated_worker();
    app.viewer_render = Some(worker);
    (app, gate)
}

fn is_tail(rows_to_render: Option<RowsToRender>) -> bool {
    matches!(rows_to_render, Some(RowsToRender::Tail(_)))
}

/// The rows, messages and calls `app`'s viewer shows equal a whole render of
/// its conversation with the view's options, every field of every row.
fn assert_rows_are_whole_render(app: &App, case: &str) {
    let state = view(app);
    let whole =
        render_parsed_conversation(&state.parsed_conversation, &app.view_render_options(state));
    assert_eq!(
        format!("{:?}", state.rendered_lines),
        format!("{:?}", whole.lines),
        "{case}: the rows are a whole render's"
    );
    assert_eq!(
        format!(
            "{:?}",
            (
                &state.message_ranges,
                &state.call_ranges,
                &state.detail_calls
            )
        ),
        format!("{:?}", (&whole.messages, &whole.calls, &whole.detail_calls)),
        "{case}: the messages and calls are a whole render's"
    );
    assert_eq!(
        state.total_lines,
        whole.lines.len(),
        "{case}: the row count is a whole render's"
    );
}

/// What the user sees of a viewer: its rows and their ranges, the reader's
/// place, the count highlight and the badge, the expanded outputs and the
/// search. It records whether the count is highlighted, not until when:
/// while the cache is busy, a read sleeps on the test clock and moves its
/// time.
#[derive(Debug, PartialEq)]
struct Seen {
    rows: String,
    ranges: String,
    scroll_offset: usize,
    focus: Option<Focus>,
    message_nav_active: bool,
    is_count_highlighted: bool,
    arrived_below: Option<usize>,
    expanded: BTreeSet<ToolOutputId>,
    hovered: Option<ToolOutputId>,
    matches: Vec<SearchMatch>,
    current_match_index: usize,
}

impl Seen {
    fn of(app: &App) -> Self {
        let state = view(app);
        Self {
            rows: format!("{:?}", state.rendered_lines),
            ranges: format!(
                "{:?}",
                (
                    &state.message_ranges,
                    &state.call_ranges,
                    &state.detail_calls
                )
            ),
            scroll_offset: state.scroll_offset,
            focus: state.focus,
            message_nav_active: state.message_nav_active,
            is_count_highlighted: state.count_highlight.is_some(),
            arrived_below: arrived_below(app),
            expanded: state.expanded_tool_outputs.clone(),
            hovered: state.hovered_tool_output.clone(),
            matches: state.search_matches.clone(),
            current_match_index: state.current_match_index,
        }
    }
}

/// One session in two viewers, each with its own home and session cache: a
/// refresh renders the first viewer's tail, and the second viewer's whole
/// conversation.
struct Twins {
    // Declared before the caches and homes: an app drops its cache
    // connections before their folders are deleted.
    viewers: [(App, Extents); 2],
    _caches: [TemporaryCacheDirectory; 2],
    homes: [ClaudeHome; 2],
}

const TAIL_VIEWER: usize = 0;
const WHOLE_VIEWER: usize = 1;

impl Twins {
    /// The session `write` writes, open in both viewers, then `prepare` run
    /// in each and its renders and searches shown.
    fn open(write: impl Fn(&Path), prepare: impl Fn(&mut App)) -> Self {
        let homes = [ClaudeHome::new(), ClaudeHome::new()];
        let caches = [
            TemporaryCacheDirectory::new(),
            TemporaryCacheDirectory::new(),
        ];
        let viewers = std::array::from_fn(|index| {
            let home = &homes[index];
            write(&home.transcript("session"));
            let environment = home.environment_with_cache(
                caches[index].path(),
                ProcessIdentity::default(),
                Arc::new(TestClock::default()),
            );
            let (mut app, extents) = recorded(viewer_in(home, environment));
            prepare(&mut app);
            finish_view_render(&mut app, VIEWPORT);
            finish_view_search(&mut app);
            (app, extents)
        });
        Self {
            viewers,
            _caches: caches,
            homes,
        }
    }

    fn tail_viewer(&self) -> &App {
        &self.viewers[TAIL_VIEWER].0
    }

    /// Change the session with `write` and refresh both viewers, the second
    /// without the shown copy's entry hashes, so its reload renders whole.
    /// Returns the rows the first viewer's reload requested.
    fn refresh_after(&mut self, write: impl Fn(&Path)) -> Option<RowsToRender> {
        let mut extents = [None, None];
        for (index, home) in self.homes.iter().enumerate() {
            let (app, recorded) = &mut self.viewers[index];
            write(&home.transcript("session"));
            if index == WHOLE_VIEWER
                && let AppMode::View(state) = &mut app.app_mode
            {
                state.entry_hashes = None;
            }
            recorded.started();
            press_refresh(app);
            finish_view_search(app);
            extents[index] = recorded.last_started();
        }
        assert_eq!(
            extents[WHOLE_VIEWER],
            Some(RowsToRender::Whole),
            "the viewer without the shown copy's hashes renders whole"
        );
        extents[TAIL_VIEWER]
    }

    /// Both viewers show the same rows, place, marks and search.
    fn assert_alike(&self, case: &str) {
        assert_eq!(
            Seen::of(self.tail_viewer()),
            Seen::of(&self.viewers[WHOLE_VIEWER].0),
            "{case}: the viewer that renders the tail shows what a whole render shows"
        );
        assert_rows_are_whole_render(self.tail_viewer(), case);
    }
}

/// The text of the focused message's rows.
fn focused_message_text(app: &App) -> String {
    let state = view(app);
    let message = &state.message_ranges[state.focused_message().expect("a message is focused")];
    state.rendered_lines[message.rows()]
        .iter()
        .map(crate::tui::viewer::RenderedLine::text)
        .collect()
}

/// The id of the focused call's input.
fn focused_call_id(app: &App) -> Option<ToolOutputId> {
    let state = view(app);
    let call = state.call_ranges.get(state.focused_call()?)?;
    Some(call.input.id.clone())
}

#[test]
fn reader_at_bottom_sees_new_messages_badge_and_count_as_after_whole_render() {
    let mut twins = Twins::open(
        |path| write_messages(path, &numbered(0..30)),
        |app| press(app, KeyCode::End),
    );

    let rows_to_render = twins.refresh_after(|path| write_messages(path, &numbered(0..32)));

    assert!(
        is_tail(rows_to_render),
        "the refresh renders the tail: {rows_to_render:?}"
    );
    twins.assert_alike("a reader at the bottom");
    let app = twins.tail_viewer();
    let state = view(app);
    assert_eq!(
        (state.scroll_offset, arrived_below(app)),
        (state.total_lines - VIEWPORT, Some(2)),
        "the reader stays at the bottom, and `↓ 2 new` shows"
    );
}

#[test]
fn reader_scrolled_up_keeps_focused_message_as_after_whole_render() {
    let mut twins = Twins::open(
        |path| write_messages(path, &numbered(0..30)),
        |app| focus_message(app, "message 10"),
    );

    let rows_to_render = twins.refresh_after(|path| write_messages(path, &numbered(0..32)));

    assert!(
        is_tail(rows_to_render),
        "the refresh renders the tail: {rows_to_render:?}"
    );
    twins.assert_alike("a reader scrolled up");
    let app = twins.tail_viewer();
    assert!(
        focused_message_text(app).contains("message 10") && view(app).count_highlight.is_some(),
        "message 10 stays focused, and the count is highlighted"
    );
}

/// A message, a run of two calls, then `texts` as messages.
fn write_run_then(path: &Path, texts: &[String]) {
    let mut lines = vec![user_line("intro")];
    lines.extend(tool_run_lines());
    lines.extend(texts.iter().map(|text| user_line(text)));
    write_lines(path, lines);
}

/// Expand the run of [`write_run_then`] at its message stop and focus its
/// second call.
fn focus_runs_second_call(app: &mut App) {
    press(app, KeyCode::Char('J'));
    press(app, KeyCode::Right);
    finish_view_render(app, VIEWPORT);
    press(app, KeyCode::Char(']'));
    let focused = focused_call_id(app).expect("a call is focused");
    assert!(
        focused.0.ends_with(":id:toolu_2"),
        "the run's second call is focused: {focused:?}"
    );
}

#[test]
fn focused_call_of_expanded_run_before_tail_stays_focused_as_after_whole_render() {
    let mut twins = Twins::open(
        |path| write_run_then(path, &numbered(0..20)),
        focus_runs_second_call,
    );

    let rows_to_render = twins.refresh_after(|path| write_run_then(path, &numbered(0..22)));

    assert!(
        is_tail(rows_to_render),
        "the refresh renders the tail: {rows_to_render:?}"
    );
    twins.assert_alike("a call focused before the tail");
    let focused = focused_call_id(twins.tail_viewer());
    assert!(
        focused.is_some_and(|id| id.0.ends_with(":id:toolu_2")),
        "the second call stays focused"
    );
}

#[test]
fn focused_call_of_expanded_run_in_tail_stays_focused_as_after_whole_render() {
    let mut twins = Twins::open(|path| write_run_then(path, &[]), focus_runs_second_call);

    let rows_to_render = twins.refresh_after(|path| write_run_then(path, &numbered(0..2)));

    let Some(RowsToRender::Tail(start)) = rows_to_render else {
        panic!("the refresh renders the tail: {rows_to_render:?}");
    };
    assert_eq!(start.entry_index, 1, "the tail starts at the run");
    twins.assert_alike("a call focused in the tail");
    let focused = focused_call_id(twins.tail_viewer());
    assert!(
        focused.is_some_and(|id| id.0.ends_with(":id:toolu_2")),
        "the second call stays focused"
    );
}

#[test]
fn output_expanded_in_tail_stays_expanded_as_after_whole_render() {
    let write = |path: &Path, texts: &[String]| {
        let mut lines = vec![user_line("intro"), long_call_line("toolu_1", "first")];
        lines.extend(texts.iter().map(|text| user_line(text)));
        write_lines(path, lines);
    };
    let mut twins = Twins::open(
        |path| write(path, &[]),
        |app| {
            press(app, KeyCode::Char('t'));
            finish_view_render(app, VIEWPORT);
            click_call(app, "first");
        },
    );
    assert!(
        view_text(twins.tail_viewer()).contains("first 6"),
        "the call shows expanded"
    );

    let rows_to_render = twins.refresh_after(|path| write(path, &numbered(0..1)));

    let Some(RowsToRender::Tail(start)) = rows_to_render else {
        panic!("the refresh renders the tail: {rows_to_render:?}");
    };
    assert_eq!(start.entry_index, 1, "the tail starts at the call");
    twins.assert_alike("an output expanded in the tail");
    let text = view_text(twins.tail_viewer());
    assert!(
        text.contains("first 6") && text.contains("message 0"),
        "the call stays expanded above the new message: {text:?}"
    );
}

/// Search the viewer's rows for `query` (`/`, then `Enter`) and show its
/// matches.
fn search_for(app: &mut App, query: &str) {
    press(app, KeyCode::Char('/'));
    for c in query.chars() {
        press(app, KeyCode::Char(c));
    }
    press(app, KeyCode::Enter);
}

/// The query every message of [`numbered`] matches.
const QUERY: &str = "message";

/// The index of the first of the shown matches at or below the top row.
fn first_match_at_or_below_top_row(app: &App) -> usize {
    let state = view(app);
    state
        .search_matches
        .iter()
        .position(|found| found.first_row() >= state.scroll_offset)
        .expect("a match starts at or below the top row")
}

/// The shown matches are a search's over the shown rows, and no search is
/// pending.
fn assert_matches_are_searches_of_shown_rows(app: &App, case: &str) {
    let state = view(app);
    assert!(
        !app.is_view_search_pending(),
        "{case}: no search is pending"
    );
    assert_eq!(
        state.search_matches,
        find_matches(&state.rendered_lines, QUERY),
        "{case}: the matches are a search's over the new rows"
    );
}

#[test]
fn search_with_matches_in_kept_rows_and_tail_shows_them_as_after_whole_render() {
    let mut twins = Twins::open(
        |path| write_messages(path, &numbered(0..30)),
        |app| search_for(app, QUERY),
    );

    let rows_to_render = twins.refresh_after(|path| write_messages(path, &numbered(0..32)));

    assert!(
        is_tail(rows_to_render),
        "the refresh renders the tail: {rows_to_render:?}"
    );
    twins.assert_alike("a search");
    let app = twins.tail_viewer();
    assert_matches_are_searches_of_shown_rows(app, "a search");
    assert_eq!(
        view(app).search_matches.len(),
        32,
        "every message's match shows"
    );
}

#[test]
fn selected_match_above_tail_stays_selected_and_search_isnt_sent_to_viewer_search() {
    let home = ClaudeHome::new();
    let path = home.transcript("session");
    write_messages(&path, &numbered(0..30));
    let (mut app, extents) = recorded(viewer_on(&home));
    search_for(&mut app, QUERY);
    finish_view_search(&mut app);
    if let AppMode::View(state) = &mut app.app_mode {
        state.current_match_index = 3;
    }

    write_messages(&path, &numbered(0..32));
    press_refresh(&mut app);

    assert!(
        is_tail(extents.last_started()),
        "the refresh renders the tail"
    );
    assert_matches_are_searches_of_shown_rows(&app, "a match above the tail selected");
    assert_eq!(
        view(&app).current_match_index,
        3,
        "the match selected above the tail stays selected"
    );
}

/// The viewer of 30 messages with a search open and `select` run, after a
/// refresh appended two messages of more rows together than the drawing
/// thread searches. Returns the viewer once the search's matches arrive, and
/// whether the search was sent to `viewer-search` once the tail showed.
fn viewer_after_long_tail_with_search(select: impl Fn(&mut App)) -> (App, bool) {
    let home = ClaudeHome::new();
    let path = home.transcript("session");
    write_messages(&path, &numbered(0..30));
    let (mut app, extents) = recorded(viewer_on(&home));
    search_for(&mut app, QUERY);
    finish_view_search(&mut app);
    select(&mut app);
    let long_lines: String = (0..TAIL_ROWS_SEARCHED_WHILE_DRAWING / 2)
        .map(|line| format!("\nline {line}"))
        .collect();
    let mut texts = numbered(0..30);
    texts.extend(numbered(30..32).into_iter().map(|text| text + &long_lines));

    write_messages(&path, &texts);
    press_refresh(&mut app);
    let is_sent_to_viewer_search =
        app.is_view_search_pending() && view(&app).search_matches.is_empty();
    finish_view_search(&mut app);

    let Some(RowsToRender::Tail(start)) = extents.last_started() else {
        panic!("the refresh renders the tail");
    };
    let tail_rows = view(&app).total_lines - start.row;
    assert!(
        tail_rows > TAIL_ROWS_SEARCHED_WHILE_DRAWING,
        "the tail has more rows than the drawing thread searches: {tail_rows}"
    );
    (app, is_sent_to_viewer_search)
}

#[test]
fn long_tail_with_search_open_searches_on_viewer_search_and_selects_as_short_tail() {
    let (app, is_sent) = viewer_after_long_tail_with_search(|app| {
        if let AppMode::View(state) = &mut app.app_mode {
            state.current_match_index = 3;
        }
    });
    assert!(
        is_sent,
        "a match above the tail selected: the search is sent to viewer-search"
    );
    assert_matches_are_searches_of_shown_rows(&app, "a match above the tail selected");
    assert_eq!(
        view(&app).current_match_index,
        3,
        "the match selected above the tail stays selected"
    );

    let (app, is_sent) = viewer_after_long_tail_with_search(|app| {
        if let AppMode::View(state) = &mut app.app_mode {
            state.current_match_index = state.search_matches.len() - 1;
        }
    });
    assert!(
        is_sent,
        "a match in the tail selected: the search is sent to viewer-search"
    );
    assert_matches_are_searches_of_shown_rows(&app, "a match in the tail selected");
    assert_eq!(
        view(&app).current_match_index,
        first_match_at_or_below_top_row(&app),
        "the first match at or below the top row is selected"
    );
}

#[test]
fn first_match_at_or_below_top_row_is_selected_when_selected_match_is_in_tail() {
    let mut twins = Twins::open(
        |path| write_messages(path, &numbered(0..30)),
        |app| {
            search_for(app, QUERY);
            finish_view_search(app);
            press(app, KeyCode::End);
            if let AppMode::View(state) = &mut app.app_mode {
                state.current_match_index = state.search_matches.len() - 1;
            }
        },
    );

    let rows_to_render = twins.refresh_after(|path| write_messages(path, &numbered(0..32)));

    assert!(
        is_tail(rows_to_render),
        "the refresh renders the tail: {rows_to_render:?}"
    );
    twins.assert_alike("a match in the tail selected");
    let app = twins.tail_viewer();
    assert_eq!(
        view(app).current_match_index,
        first_match_at_or_below_top_row(app),
        "the first match at or below the top row is selected"
    );
}

#[test]
fn refresh_without_changed_entries_renders_last_message_again_as_after_whole_render() {
    let mut twins = Twins::open(
        |path| write_messages(path, &numbered(0..30)),
        |app| press(app, KeyCode::End),
    );

    let rows_to_render = twins.refresh_after(|path| {
        let mut transcript = std::fs::OpenOptions::new().append(true).open(path).unwrap();
        std::io::Write::write_all(&mut transcript, b"\n").unwrap();
    });

    assert!(
        is_tail(rows_to_render),
        "the refresh renders the tail: {rows_to_render:?}"
    );
    twins.assert_alike("a refresh without a changed entry");
    assert_eq!(
        view(twins.tail_viewer()).count_highlight,
        None,
        "the count isn't highlighted"
    );
}

#[test]
fn shorter_transcript_shows_rows_as_after_whole_render() {
    let mut twins = Twins::open(
        |path| write_messages(path, &numbered(0..30)),
        |app| press(app, KeyCode::End),
    );

    let rows_to_render = twins.refresh_after(|path| write_messages(path, &numbered(0..28)));

    assert!(
        is_tail(rows_to_render),
        "the refresh renders the tail: {rows_to_render:?}"
    );
    twins.assert_alike("a shorter transcript");
    assert!(
        !view_text(twins.tail_viewer()).contains("message 28"),
        "the removed messages are gone"
    );
}

#[test]
fn entry_changed_in_middle_shows_rows_as_after_whole_render() {
    let mut twins = Twins::open(
        |path| write_messages(path, &numbered(0..30)),
        |app| focus_message(app, "message 20"),
    );
    let mut edited = numbered(0..30);
    edited[15] = "message 15, edited".to_owned();

    let rows_to_render = twins.refresh_after(|path| write_messages(path, &edited));

    assert!(
        is_tail(rows_to_render),
        "the refresh renders the tail: {rows_to_render:?}"
    );
    twins.assert_alike("an entry changed in the middle");
    assert!(
        view_text(twins.tail_viewer()).contains("message 15, edited"),
        "the edited message shows"
    );
}

#[test]
fn file_opened_outside_every_root_renders_refresh_whole() {
    let home = ClaudeHome::new();
    let path = home.transcript("session");
    write_messages(&path, &numbered(0..3));
    let (mut app, extents) = recorded(opened_outside_every_root(&path));

    write_messages(&path, &numbered(0..5));
    press_refresh_outside_every_root(&mut app);

    assert_eq!(
        extents.started(),
        [RowsToRender::Whole],
        "the file read without entry hashes renders whole"
    );
    assert_rows_are_whole_render(&app, "a file outside every root");
    assert!(
        view_text(&app).contains("message 4"),
        "the new messages show"
    );
}

#[test]
fn cached_file_opened_directly_renders_first_read_whole_and_later_refresh_tail() {
    let home = ClaudeHome::new();
    let (transcript, environment) = cached_titled_session(&home);
    // The tail starts at this message: a tail from the first entry renders
    // whole.
    home.append(&transcript, 6, "second question");
    let file = DirectFile::canonicalise(transcript.clone(), environment.canonicaliser()).unwrap();
    let (mut app, extents) = recorded(App::new_single_file(
        file,
        ToolDisplayMode::Hidden,
        false,
        KeyBindings::default(),
        environment,
    ));
    app.serve_direct_file_read();
    super::test_waits::finish_pending_view_reload(&mut app, VIEWPORT);
    app.check_view_resize(80);
    finish_view_render(&mut app, VIEWPORT);
    let first_read = extents.started();

    home.append(&transcript, 7, "follow-up");
    press_refresh(&mut app);

    assert!(
        !first_read.is_empty() && first_read.iter().all(|rows| *rows == RowsToRender::Whole),
        "the first read and the resize render whole: {first_read:?}"
    );
    assert!(
        is_tail(extents.last_started()),
        "the later refresh renders the tail"
    );
    assert_rows_are_whole_render(&app, "a file opened directly");
    assert!(
        view_text(&app).contains("follow-up"),
        "the appended message shows"
    );
}

#[test]
fn copy_missing_at_open_renders_first_refresh_whole() {
    let home = ClaudeHome::new();
    let path = home.transcript("session");
    write_messages(&path, &numbered(0..3));
    let environment = home.environment();
    let rows = home.rows(&environment);
    rusqlite::Connection::open(
        environment
            .cache_base()
            .path()
            .join(crate::history::sqlite_cache::DATABASE_FILE_NAME),
    )
    .unwrap()
    .execute("DELETE FROM sessions", [])
    .unwrap();
    let mut app = app_reading(rows, environment);
    app.selected = Some(0);
    app.enter_view_mode(80);
    assert!(viewer_lacks_copy(&app), "the open finds the copy missing");
    let (mut app, extents) = recorded(app);

    write_messages(&path, &numbered(0..5));
    press_refresh(&mut app);

    assert_eq!(
        extents.last_started(),
        Some(RowsToRender::Whole),
        "the reload of a copy the open found missing renders whole"
    );
    assert_rows_are_whole_render(&app, "a copy missing at open");
    assert!(
        view_text(&app).contains("message 4"),
        "the refreshed copy's messages show"
    );
}

/// The timing column (`i`) shown and hidden again while the reload's read
/// runs: the render pending when the read arrives has the shown rows'
/// options.
#[test]
fn render_pending_when_reloads_read_arrives_renders_whole() {
    let home = ClaudeHome::new();
    let path = home.transcript("session");
    write_messages(&path, &numbered(0..30));
    let (mut app, gate) = gated(viewer_on(&home));
    write_messages(&path, &numbered(0..32));
    request_refresh(&mut app);
    press(&mut app, KeyCode::Char('i'));
    assert_eq!(gate.next_render(), (ToolDisplayMode::Hidden, true));
    press(&mut app, KeyCode::Char('i'));

    assert!(
        app.complete_pending_view_refresh(VIEWER_READ_LANDS),
        "the reload's read arrives"
    );
    gate.let_one_through();
    assert!(
        !gate.next_return().finished,
        "the newer requests stop the first display key's render"
    );
    assert_eq!(gate.next_render(), (ToolDisplayMode::Hidden, false));
    gate.let_one_through();
    finish_view_render(&mut app, VIEWPORT);

    assert_eq!(
        gate.extents.started(),
        [RowsToRender::Whole, RowsToRender::Whole],
        "the reload's read arriving while a render is pending renders whole"
    );
    assert_rows_are_whole_render(&app, "a render pending");
    assert!(
        view_text(&app).contains("message 31"),
        "the new messages show"
    );
}

/// A call expanded and collapsed again while the reload's read runs: its
/// message waits to render again when the read arrives, with the expanded
/// set of the shown rows.
#[test]
fn message_waiting_to_render_again_when_reloads_read_arrives_renders_whole() {
    let home = ClaudeHome::new();
    let path = home.transcript("session");
    let write = |texts: &[String]| {
        let mut lines = vec![user_line("intro"), long_call_line("toolu_1", "first")];
        lines.extend(texts.iter().map(|text| user_line(text)));
        write_lines(&path, lines);
    };
    write(&[]);
    let (mut app, extents) = recorded(viewer_on(&home));
    press(&mut app, KeyCode::Char('t'));
    finish_view_render(&mut app, VIEWPORT);
    write(&numbered(0..2));
    request_refresh(&mut app);
    click_call(&mut app, "first");
    click_call(&mut app, "first");
    extents.started();

    assert!(
        app.complete_pending_view_refresh(VIEWER_READ_LANDS),
        "the reload's read arrives"
    );
    finish_view_render(&mut app, VIEWPORT);

    assert_eq!(
        extents.started(),
        [RowsToRender::Whole],
        "the reload's read arriving while a message waits to render again renders whole"
    );
    assert_rows_are_whole_render(&app, "a message waiting to render again");
    assert!(
        view_text(&app).contains("message 1"),
        "the new messages show"
    );
}

#[test]
fn reload_after_date_moved_renders_whole() {
    let home = ClaudeHome::new();
    let path = home.transcript("session");
    write_messages(&path, &numbered(0..30));
    let clock = Arc::new(TestClock::default());
    let (mut app, extents) = recorded(viewer_on_clock(&home, clock.clone()));
    if let AppMode::View(state) = &mut app.app_mode {
        state.show_timing = true;
    }
    app.request_view_render();
    finish_view_render(&mut app, VIEWPORT);
    clock.sleep(Duration::from_secs(24 * 60 * 60));

    write_messages(&path, &numbered(0..32));
    press_refresh(&mut app);

    assert_eq!(
        extents.last_started(),
        Some(RowsToRender::Whole),
        "rows rendered for an earlier date render whole"
    );
    assert_rows_are_whole_render(&app, "the date moved");
}

/// The viewer of 30 messages, rendering through a gated worker, after a
/// refresh's read of 32 messages arrived: the tail's render waits at the
/// gate.
fn tail_render_at_gate(home: &ClaudeHome) -> (App, Gate) {
    let path = home.transcript("session");
    write_messages(&path, &numbered(0..30));
    let (mut app, gate) = gated(viewer_on(home));
    write_messages(&path, &numbered(0..32));
    refresh_and_receive_read(&mut app);
    gate.next_render();
    assert!(
        is_tail(gate.extents.last_started()),
        "the refresh renders the tail"
    );
    (app, gate)
}

/// The render at the gate, a tail's, returns stopped by a newer request,
/// whose whole render then reaches the gate, renders and shows.
fn stop_tail_and_show_whole_render(app: &mut App, gate: &Gate, when: &str) {
    gate.let_one_through();
    assert!(
        !gate.next_return().finished,
        "{when}: the newer request stops the tail's render"
    );
    gate.next_render();
    gate.let_one_through();
    finish_view_render(app, VIEWPORT);
    assert_eq!(
        gate.extents.started(),
        [RowsToRender::Whole],
        "{when}: a whole render replaces the tail's"
    );
    assert_rows_are_whole_render(app, when);
    assert!(
        view_text(app).contains("message 31"),
        "{when}: the new messages show"
    );
}

#[test]
fn display_key_during_tail_render_shows_whole_render_in_new_mode() {
    let home = ClaudeHome::new();
    let (mut app, gate) = tail_render_at_gate(&home);

    press(&mut app, KeyCode::Char('i'));

    stop_tail_and_show_whole_render(&mut app, &gate, "a display key");
    assert!(view(&app).show_timing, "the timing column shows");
}

#[test]
fn resize_during_tail_render_shows_whole_render_at_new_width() {
    let home = ClaudeHome::new();
    let (mut app, gate) = tail_render_at_gate(&home);

    app.check_view_resize(100);

    stop_tail_and_show_whole_render(&mut app, &gate, "a resize");
    assert_eq!(
        view(&app).frame_width,
        100,
        "the rows are rendered for the new width"
    );
}

#[test]
fn expand_during_tail_render_renders_in_place_once_tail_shows() {
    let home = ClaudeHome::new();
    let path = home.transcript("session");
    let write = |texts: &[String]| {
        let mut lines = vec![user_line("intro"), long_call_line("toolu_1", "first")];
        lines.extend(texts.iter().map(|text| user_line(text)));
        write_lines(&path, lines);
    };
    write(&[]);
    let (mut app, gate) = gated(viewer_on(&home));
    press(&mut app, KeyCode::Char('t'));
    gate.next_render();
    gate.let_one_through();
    finish_view_render(&mut app, VIEWPORT);
    assert!(
        gate.next_return().finished,
        "the truncated mode's render finishes"
    );
    write(&numbered(0..1));
    refresh_and_receive_read(&mut app);
    gate.next_render();

    click_call(&mut app, "first");
    gate.let_one_through();
    finish_view_render(&mut app, VIEWPORT);

    assert!(
        gate.next_return().finished && !gate.took_waiting_render(),
        "the tail's render finishes, and the expand doesn't request another"
    );
    assert!(
        matches!(
            gate.extents.started().as_slice(),
            [RowsToRender::Whole, RowsToRender::Tail(_)]
        ),
        "the reload renders the tail"
    );
    let text = view_text(&app);
    assert!(
        text.contains("message 0") && text.contains("first 6"),
        "the tail's rows show with the call expanded: {text:?}"
    );
    assert_rows_are_whole_render(&app, "an expand during the tail's render");
}

#[test]
fn refresh_finishing_during_tail_render_reads_once_tail_shows() {
    let home = ClaudeHome::new();
    let path = home.transcript("session");
    write_messages(&path, &numbered(0..30));
    let (mut app, gate) = gated(viewer_on(&home));
    write_messages(&path, &numbered(0..31));
    refresh_and_receive_read(&mut app);
    gate.next_render();
    write_messages(&path, &numbered(0..32));
    request_refresh(&mut app);

    let reads_during_tail = app.is_reading_open_session();
    gate.let_one_through();
    finish_view_render(&mut app, VIEWPORT);
    let first_rows = view_text(&app);
    let reads_once_tail_shows = app.is_reading_open_session();
    gate.let_one_through();
    super::test_waits::finish_pending_view_reload(&mut app, VIEWPORT);

    assert!(
        !reads_during_tail && reads_once_tail_shows,
        "the second refresh's read starts once the first tail's rows show"
    );
    assert!(
        first_rows.contains("message 30") && !first_rows.contains("message 31"),
        "the first tail's rows show: {first_rows:?}"
    );
    let extents = gate.extents.started();
    assert!(
        matches!(
            extents.as_slice(),
            [RowsToRender::Tail(_), RowsToRender::Tail(_)]
        ),
        "each reload renders its tail: {extents:?}"
    );
    assert_rows_are_whole_render(&app, "a second refresh");
    assert!(
        view_text(&app).contains("message 31"),
        "the second refresh's message shows"
    );
}

#[test]
fn leaving_viewer_during_tail_render_stops_it_and_next_viewer_renders() {
    let home = ClaudeHome::new();
    let (mut app, gate) = tail_render_at_gate(&home);

    press(&mut app, KeyCode::Char('q'));
    gate.let_one_through();
    assert!(
        !gate.next_return().finished,
        "leaving the viewer stops the tail's render"
    );
    app.enter_view_mode(80);
    press(&mut app, KeyCode::Char('t'));
    gate.next_render();
    gate.let_one_through();
    finish_view_render(&mut app, VIEWPORT);

    assert_eq!(
        view(&app).tool_display,
        ToolDisplayMode::Truncated,
        "the viewer opened again shows truncated mode"
    );
    assert_rows_are_whole_render(&app, "the viewer opened again");
}

#[test]
fn finished_tail_superseded_before_taken_isnt_shown_over_newer_render() {
    let home = ClaudeHome::new();
    let (mut app, gate) = tail_render_at_gate(&home);
    gate.let_one_through();
    assert!(gate.next_return().finished, "the tail's render finishes");

    press(&mut app, KeyCode::Char('i'));
    gate.let_one_through();
    finish_view_render(&mut app, VIEWPORT);

    assert_eq!(
        gate.next_render(),
        (ToolDisplayMode::Hidden, true),
        "the display key's render runs once the tail is dropped"
    );
    assert!(view(&app).show_timing, "the timing column shows");
    assert_rows_are_whole_render(&app, "a tail superseded before it was taken");
    assert!(
        view_text(&app).contains("message 31"),
        "the new messages show"
    );
}

#[test]
fn tail_arriving_while_search_shares_rows_shows_once_search_stops() {
    let home = ClaudeHome::new();
    let path = home.transcript("session");
    write_messages(&path, &numbered(0..30));
    let (mut app, gate) = gated(viewer_on(&home));
    let (search_worker, search_gate) = gated_search_worker(true);
    app.viewer_search = Some(search_worker);
    search_for(&mut app, QUERY);
    assert_eq!(search_gate.next_search(), QUERY);
    write_messages(&path, &numbered(0..32));
    refresh_and_receive_read(&mut app);
    gate.next_render();
    gate.let_one_through();
    assert!(gate.next_return().finished, "the tail's render finishes");

    app.receive_view_render(VIEWPORT, VIEWER_READ_LANDS);
    let waits_for_search = app.is_view_render_pending() && !view_text(&app).contains("message 31");
    search_gate.let_one_through();
    let stopped_search = search_gate.next_return();
    finish_view_render(&mut app, VIEWPORT);
    assert_eq!(search_gate.next_search(), QUERY, "the search is sent again");
    search_gate.let_one_through();
    finish_view_search(&mut app);

    assert!(
        waits_for_search,
        "the tail waits while the search shares the shown rows"
    );
    assert_eq!(
        stopped_search,
        (QUERY.to_owned(), false),
        "the tail stops the search sharing the rows"
    );
    assert_rows_are_whole_render(&app, "a search sharing the rows");
    assert_matches_are_searches_of_shown_rows(&app, "a search sharing the rows");
    assert_eq!(
        view(&app).search_matches.len(),
        32,
        "every message's match shows"
    );
}

#[test]
fn display_key_while_tail_waits_for_search_drops_tail_and_shows_whole_render() {
    let home = ClaudeHome::new();
    let path = home.transcript("session");
    write_messages(&path, &numbered(0..30));
    let (mut app, gate) = gated(viewer_on(&home));
    let (search_worker, search_gate) = gated_search_worker(true);
    app.viewer_search = Some(search_worker);
    search_for(&mut app, QUERY);
    assert_eq!(
        search_gate.next_search(),
        QUERY,
        "the search reaches its gate"
    );
    write_messages(&path, &numbered(0..32));
    refresh_and_receive_read(&mut app);
    gate.next_render();
    gate.let_one_through();
    assert!(gate.next_return().finished, "the tail's render finishes");
    app.receive_view_render(VIEWPORT, VIEWER_READ_LANDS);

    press(&mut app, KeyCode::Char('i'));
    app.receive_view_render(VIEWPORT, Duration::ZERO);
    let shows_tail = view_text(&app).contains("message 31");
    gate.next_render();
    let started = gate.extents.started();
    search_gate.let_one_through();
    let stopped_search = search_gate.next_return();
    gate.let_one_through();
    finish_view_render(&mut app, VIEWPORT);
    assert_eq!(search_gate.next_search(), QUERY, "the search is sent again");
    search_gate.let_one_through();
    finish_view_search(&mut app);

    assert!(!shows_tail, "the tail kept on the worker isn't shown");
    assert!(
        matches!(
            started.as_slice(),
            [RowsToRender::Tail(_), RowsToRender::Whole]
        ),
        "the display key's whole render starts once the kept tail is dropped: {started:?}"
    );
    assert_eq!(
        stopped_search,
        (QUERY.to_owned(), false),
        "the tail stops the search sharing the rows"
    );
    assert!(view(&app).show_timing, "the timing column shows");
    assert_rows_are_whole_render(&app, "a display key while the tail waits");
    assert!(
        view_text(&app).contains("message 31"),
        "the whole render's rows show the new messages"
    );
}

/// The display modes a refresh renders in: the tools mode, the timing
/// column and thinking.
fn every_mode() -> Vec<(ToolDisplayMode, bool, bool)> {
    let mut modes = Vec::new();
    for tool_display in [
        ToolDisplayMode::Hidden,
        ToolDisplayMode::Truncated,
        ToolDisplayMode::Full,
    ] {
        for show_timing in [false, true] {
            for show_thinking in [false, true] {
                modes.push((tool_display, show_timing, show_thinking));
            }
        }
    }
    modes
}

/// Show `app`'s rows in `modes` once they render.
fn show_in(
    app: &mut App,
    (tool_display, show_timing, show_thinking): (ToolDisplayMode, bool, bool),
) {
    if let AppMode::View(state) = &mut app.app_mode {
        state.tool_display = tool_display;
        state.show_timing = show_timing;
        state.show_thinking = show_thinking;
    }
    app.request_view_render();
    finish_view_render(app, VIEWPORT);
}

/// An agent's every-record session, written whole or cut to its first part,
/// as the agent wrote it while it worked.
enum Transcript {
    /// A transcript of lines; its first part is the first half of them.
    Lines {
        path: PathBuf,
        whole: String,
        first_part: String,
    },
    /// OpenCode's database; its first part leaves out the messages and
    /// parts created after the median part, and the whole puts them back
    /// from `original`, a copy in a temporary folder of its own.
    Database {
        path: PathBuf,
        original: PathBuf,
        _original_folder: tempfile::TempDir,
        first_part_until: i64,
    },
}

impl Transcript {
    /// The session discovery finds at `locator`, cut to its first part.
    fn cut(locator: &Path) -> Self {
        let transcript = if locator.is_file() {
            let whole = std::fs::read_to_string(locator).unwrap();
            let lines: Vec<&str> = whole.split_inclusive('\n').collect();
            let first_part = lines[..lines.len() / 2].concat();
            Self::Lines {
                path: locator.to_path_buf(),
                whole,
                first_part,
            }
        } else {
            let path = locator
                .ancestors()
                .find(|path| path.is_file())
                .expect("a session that isn't a file is a database's")
                .to_path_buf();
            let original_folder = tempfile::tempdir().unwrap();
            let original = original_folder.path().join("original.db");
            std::fs::copy(&path, &original).unwrap();
            let times: Vec<i64> = rusqlite::Connection::open(&path)
                .unwrap()
                .prepare("SELECT time_created FROM part ORDER BY time_created")
                .unwrap()
                .query_map([], |row| row.get(0))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap();
            Self::Database {
                path,
                original,
                _original_folder: original_folder,
                first_part_until: times[times.len() / 2],
            }
        };
        transcript.write_first_part();
        transcript
    }

    fn write_first_part(&self) {
        match self {
            Self::Lines {
                path, first_part, ..
            } => std::fs::write(path, first_part).unwrap(),
            Self::Database {
                path,
                first_part_until,
                ..
            } => {
                let connection = rusqlite::Connection::open(path).unwrap();
                for table in ["part", "message"] {
                    connection
                        .execute(
                            &format!("DELETE FROM {table} WHERE time_created > ?1"),
                            [first_part_until],
                        )
                        .unwrap();
                }
            }
        }
    }

    fn write_whole(&self) {
        match self {
            Self::Lines { path, whole, .. } => std::fs::write(path, whole).unwrap(),
            Self::Database { path, original, .. } => {
                let connection = rusqlite::Connection::open(path).unwrap();
                connection
                    .execute(
                        "ATTACH DATABASE ?1 AS original",
                        [original.to_string_lossy()],
                    )
                    .unwrap();
                for table in ["message", "part"] {
                    connection
                        .execute(
                            &format!(
                                "INSERT INTO {table} SELECT * FROM original.{table} \
                                 WHERE id NOT IN (SELECT id FROM {table})"
                            ),
                            [],
                        )
                        .unwrap();
                }
            }
        }
    }
}

/// `fixture`'s every-record session cut to its first part and open in the
/// viewer; then, in each display mode, the rest written and the session
/// refreshed, then cut again and refreshed. Each refresh shows the rows of
/// a whole render, and each refresh of the rest renders the tail.
fn assert_every_mode_renders_rest_as_whole_render(fixture: &'static dyn ProviderFixture) {
    let agent = fixture.provider().labels().name;
    let home = tempfile::tempdir().unwrap();
    fixture.write_every_record_session(home.path(), &fixture.every_record_session());
    let stubs = fixture.discover_under(home.path());
    let [stub] = stubs.as_slice() else {
        panic!("{agent}: discovery finds the session written: {stubs:?}");
    };
    let transcript = Transcript::cut(&stub.locator);
    let cache = admitted_cache();
    load_under(fixture, home.path(), cache.path());
    let rows = load_under(fixture, home.path(), cache.path());
    let environment = crate::history::environment::test_support::test_environment(
        vec![kept_storage_under(fixture, home.path())],
        Some(cache.path()),
    );
    let mut app = app_reading(rows, environment);
    app.selected = Some(0);
    app.enter_view_mode(80);
    let (mut app, extents) = recorded(app);

    for modes in every_mode() {
        let case = format!("{agent} {modes:?}");
        show_in(&mut app, modes);
        transcript.write_whole();
        extents.started();
        press_refresh(&mut app);
        assert!(
            is_tail(extents.last_started()),
            "{case}: the refresh of the rest renders the tail"
        );
        assert_rows_are_whole_render(&app, &format!("{case}, the rest written"));
        transcript.write_first_part();
        press_refresh(&mut app);
        assert_rows_are_whole_render(&app, &format!("{case}, cut again"));
    }
}

#[test]
fn every_agents_session_refreshed_with_its_rest_renders_tail_as_whole_render_in_every_mode() {
    // Each agent's session runs on a thread of its own: an `App` marks its
    // thread with a `DrawingThread`, and the drawing thread doesn't parse a
    // transcript.
    let running: Vec<_> = registered_fixtures()
        .into_iter()
        .map(|fixture| {
            std::thread::spawn(move || assert_every_mode_renders_rest_as_whole_render(fixture))
        })
        .collect();
    for agent in running {
        agent.join().unwrap();
    }
}
