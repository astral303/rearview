//! After a refresh of the open session, the viewer renders the tail on
//! `viewer-render` and puts its rows after the rows it keeps. The rows, the
//! reader's place, the badge, the count highlight and the search equal those
//! a whole render shows.

use super::claude_home::{ClaudeHome, admitted_cache, app_reading, viewer_lacks_copy};
use super::test_waits::{
    VIEWER_READ_LANDS, finish_pending_view_reload, finish_view_render, finish_view_search,
    finish_waiting_tail,
};
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
use crate::history::format::RecordLine;
use crate::history::provider::contract_tests::{
    EarlierRecords, ProviderFixture, RecordTranscript, RewritableSession, append_lines,
    kept_storage_under, load_under, registered_fixtures,
};
use crate::log_entry::{ContentBlock, LogEntry, convert_agent_progress};
use crate::tui::viewer::{
    RowsToRender, SearchMatch, fields_differing, find_matches, render_parsed_conversation,
};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;

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

/// The address of the conversation `app`'s viewer shows. A reload that puts
/// a tail after the kept entries keeps the conversation where it is, and a
/// whole read replaces it with another.
fn conversation_address(app: &App) -> usize {
    Arc::as_ptr(&view(app).parsed_conversation) as usize
}

/// The conversation and the entry hashes `app`'s viewer keeps equal a whole
/// read's of the copy the cache has now.
fn assert_conversation_is_whole_reads(app: &App, case: &str) {
    let state = view(app);
    let whole = crate::history::environment::test_support::current_copy(
        crate::tui::viewer::parse_conversation_file_interactively(
            &app.interactive_cache(),
            state
                .cache_address
                .as_ref()
                .expect("the view has its session's address"),
            &state.publication,
        ),
    );
    assert_eq!(
        fields_differing(&state.parsed_conversation, &whole.conversation),
        Vec::<&str>::new(),
        "{case}: the view's conversation equals a whole read's in every field"
    );
    assert_eq!(
        state.entry_hashes.as_deref(),
        Some(&whole.entry_hashes),
        "{case}: the view keeps one hash per entry of the copy read"
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
            let shown = conversation_address(app);
            press_refresh(app);
            finish_view_search(app);
            extents[index] = recorded.last_started();
            if index == WHOLE_VIEWER {
                assert_ne!(
                    conversation_address(app),
                    shown,
                    "the viewer without the shown copy's hashes shows a whole read's conversation"
                );
            } else {
                assert_eq!(
                    conversation_address(app),
                    shown,
                    "the viewer with the shown copy's hashes puts the tail after the kept \
                     entries of the shown conversation"
                );
            }
            assert_conversation_is_whole_reads(app, "a refresh");
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
    assert!(
        !gate.next_return().finished,
        "the reload's tail, waiting for the conversation, stops the second display key's render"
    );
    finish_waiting_tail(&mut app);
    assert_eq!(
        gate.next_render(),
        (ToolDisplayMode::Hidden, false),
        "the reload's render starts once its tail is put after the kept entries"
    );
    gate.let_one_through();
    finish_view_render(&mut app, VIEWPORT);

    assert_eq!(
        gate.extents.started(),
        [RowsToRender::Whole; 3],
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

/// `fixture`'s every-record session cut in half and open in the viewer;
/// then, in each display mode, the rest written and the session refreshed,
/// then cut again and refreshed. Each refresh shows the rows of a whole
/// render, and each refresh of the rest renders the tail.
fn assert_every_mode_renders_rest_as_whole_render(fixture: &'static dyn ProviderFixture) {
    let session = FixtureSession::every_record(fixture);
    let agent = session.agent;
    let first_half = session.cut_in_half();
    session.write_cut(&first_half);
    let (_cache, app) = session.open();
    let (mut app, extents) = recorded(app);
    let shown_entries = view(&app).parsed_conversation.entry_count();
    assert!(
        0 < shown_entries && shown_entries < session.entries.len(),
        "{agent}: the session cut in half has some of its entries: {shown_entries}"
    );

    for modes in every_mode() {
        let case = format!("{agent} {modes:?}");
        show_in(&mut app, modes);
        session.write_whole();
        extents.started();
        let shown = conversation_address(&app);
        press_refresh(&mut app);
        assert!(
            is_tail(extents.last_started()),
            "{case}: the refresh of the rest renders the tail"
        );
        assert_eq!(
            conversation_address(&app),
            shown,
            "{case}: the reload puts the rest after the kept entries of the shown conversation"
        );
        assert_conversation_is_whole_reads(&app, &format!("{case}, the rest written"));
        assert_rows_are_whole_render(&app, &format!("{case}, the rest written"));
        session.write_cut(&first_half);
        press_refresh(&mut app);
        assert_conversation_is_whole_reads(&app, &format!("{case}, cut again"));
        assert_rows_are_whole_render(&app, &format!("{case}, cut again"));
    }
}

#[test]
fn every_agents_session_refreshed_with_its_rest_renders_tail_as_whole_render_in_every_mode() {
    assert_for_each_agent(
        registered_fixtures(),
        assert_every_mode_renders_rest_as_whole_render,
    );
}

/// An agent's session as the agent had written it at an earlier time.
struct Cut {
    earlier: EarlierRecords,
    /// The count of entries the session had then, for a cut that keeps the
    /// entries before one entry and removes the rest.
    entries_kept: Option<usize>,
}

impl Cut {
    /// A cut of the transcripts in `records_kept` to their counts of
    /// records, at a place that isn't between two of the session's entries.
    fn of_records(records_kept: BTreeMap<RecordTranscript, usize>) -> Self {
        Self {
            earlier: EarlierRecords {
                records_kept,
                running_call: None,
            },
            entries_kept: None,
        }
    }
}

/// The ids of the calls `entry` makes.
fn call_ids(entry: &LogEntry) -> Vec<&str> {
    entry
        .content_blocks()
        .iter()
        .filter_map(|block| match block {
            ContentBlock::ToolUse { id, .. } => Some(id.as_str()),
            _ => None,
        })
        .collect()
}

/// The ids of the calls `entry` has the results of.
fn result_ids(entry: &LogEntry) -> Vec<&str> {
    entry
        .content_blocks()
        .iter()
        .filter_map(|block| match block {
            ContentBlock::ToolResult { tool_use_id, .. } => Some(tool_use_id.as_str()),
            _ => None,
        })
        .collect()
}

/// An agent's session below a temporary home, where discovery finds it, with
/// the entries a whole read of it has. A test cuts these files, not the
/// fixture files other tests read at the same time.
struct FixtureSession {
    fixture: &'static dyn ProviderFixture,
    agent: &'static str,
    home: tempfile::TempDir,
    /// True for a session that isn't a file of its own: its agent keeps
    /// every session in one database.
    is_in_database: bool,
    has_subagents: bool,
    /// Each entry with its record, a sub-agent's turn as the viewer shows
    /// it.
    entries: Vec<(RecordLine, LogEntry)>,
    /// The session's files, written as a cut has them and whole again.
    stored: Box<dyn RewritableSession>,
}

impl FixtureSession {
    /// `fixture`'s every-record session.
    fn every_record(fixture: &'static dyn ProviderFixture) -> Self {
        let home = tempfile::tempdir().unwrap();
        fixture.write_every_record_session(home.path(), &fixture.every_record_session());
        Self::discovered(fixture, home)
    }

    /// The one session a test wrote below `home` with `fixture`.
    fn discovered(fixture: &'static dyn ProviderFixture, home: tempfile::TempDir) -> Self {
        let agent = fixture.provider().labels().name;
        let stubs = fixture.discover_under(home.path());
        let [stub] = stubs.as_slice() else {
            panic!("{agent}: discovery finds the session written: {stubs:?}");
        };
        let entries: Vec<(RecordLine, LogEntry)> = crate::history::normalized_session(
            crate::history::FOR_FIXTURES,
            fixture.provider().source(),
            &stub.locator,
            &stub.subagents,
        )
        .unwrap()
        .entries
        .into_iter()
        .map(|(record, entry)| (record, convert_agent_progress(entry)))
        .collect();
        Self {
            fixture,
            agent,
            home,
            is_in_database: !stub.locator.is_file(),
            has_subagents: !stub.subagents.is_empty(),
            entries,
            stored: fixture.rewritable_session(stub),
        }
    }

    fn record(&self, entry: usize) -> &RecordLine {
        &self.entries[entry].0
    }

    /// True when a cut can remove records of `transcript`: the session's
    /// own, or a sub-agent's.
    fn can_cut(&self, transcript: &RecordTranscript) -> bool {
        self.stored.can_cut(transcript)
    }

    /// The session before the agent wrote the record of its middle entry.
    fn cut_in_half(&self) -> Cut {
        self.cut_before(self.entries.len() / 2)
    }

    /// The first entry after the first for which `is_cut` is true.
    fn first_entry(&self, is_cut: impl Fn(usize) -> bool) -> Option<usize> {
        (1..self.entries.len()).find(|&entry| is_cut(entry))
    }

    /// The session before the agent wrote the record of the entry numbered
    /// `entry` and the records of the entries after it. A sub-agent whose
    /// first turn is one of those entries hasn't started: its transcript
    /// doesn't have a record. A record at line 0 isn't in a transcript.
    fn cut_before(&self, entry: usize) -> Cut {
        let (before, from) = self.entries.split_at(entry);
        let mut records_kept = BTreeMap::new();
        for (record, _) in from {
            if record.line > 0 && self.can_cut(&record.transcript) {
                let kept = records_kept
                    .entry(record.transcript.clone())
                    .or_insert(usize::MAX);
                *kept = (*kept).min(record.line - 1);
            }
        }
        for (transcript, kept) in &mut records_kept {
            let has_started = before
                .iter()
                .any(|(record, _)| record.transcript == *transcript);
            if transcript.is_some() && !has_started {
                *kept = 0;
            }
        }
        Cut {
            entries_kept: Some(entry),
            ..Cut::of_records(records_kept)
        }
    }

    /// The session cut before a call of its own that follows another call
    /// or a call's result.
    fn cut_inside_tool_run(&self) -> Option<Cut> {
        let is_tool_entry = |entry: usize| {
            let (record, entry) = &self.entries[entry];
            record.transcript.is_none()
                && !(call_ids(entry).is_empty() && result_ids(entry).is_empty())
        };
        self.first_entry(|entry| {
            is_tool_entry(entry - 1)
                && self.record(entry - 1) != self.record(entry)
                && self.record(entry).transcript.is_none()
                && !call_ids(&self.entries[entry].1).is_empty()
        })
        .map(|entry| self.cut_before(entry))
    }

    /// The session cut between a call of its own and the call's result:
    /// before the result's record or, when one record has both, inside that
    /// record.
    fn cut_between_call_and_result(&self) -> Option<Cut> {
        self.cut_before_result_in_later_record()
            .or_else(|| self.cut_inside_record())
    }

    /// The session cut before the result, in a record of its own, of a call
    /// the session made in an earlier record.
    fn cut_before_result_in_later_record(&self) -> Option<Cut> {
        self.first_entry(|entry| {
            let (record, result) = &self.entries[entry];
            let answered = result_ids(result);
            record.transcript.is_none()
                && self.entries[..entry].iter().any(|(call_record, call)| {
                    call_record.transcript.is_none()
                        && call_record != record
                        && call_ids(call).iter().any(|id| answered.contains(id))
                })
        })
        .map(|entry| self.cut_before(entry))
    }

    /// The session cut after the first turn of a sub-agent: before the
    /// sub-agent's next turn, or before the session's record that follows
    /// the sub-agent's one turn.
    fn cut_inside_subagents_turns(&self) -> Option<Cut> {
        self.first_entry(|entry| {
            let earlier = &self.record(entry - 1).transcript;
            earlier.is_some() && self.can_cut(earlier)
        })
        .map(|entry| self.cut_before(entry))
    }

    /// The session cut between a call and its result that one record has:
    /// the record of a running call, which the agent rewrites once the call
    /// finishes. The result's entry is then the second read from a record
    /// whose first entry the cut keeps. A transcript of lines doesn't have
    /// such a record: its agent appends whole records.
    fn cut_inside_record(&self) -> Option<Cut> {
        let result = self.first_entry(|entry| {
            self.record(entry - 1) == self.record(entry)
                && !call_ids(&self.entries[entry - 1].1).is_empty()
                && !result_ids(&self.entries[entry].1).is_empty()
                && self
                    .stored
                    .is_finished_call_written_running_first(self.record(entry))
        })?;
        let record = self.record(result);
        let after_record = (result..self.entries.len())
            .find(|&entry| self.record(entry) != record)
            .unwrap_or(self.entries.len());
        let mut cut = self.cut_before(after_record);
        cut.earlier.running_call = Some(record.clone());
        cut.entries_kept = Some(result);
        Some(cut)
    }

    /// A sub-agent's transcript before the record of the sub-agent's last
    /// turn, and every other transcript whole.
    fn cut_of_subagents_transcript_alone(&self) -> Option<Cut> {
        let transcript = self
            .entries
            .iter()
            .map(|(record, _)| &record.transcript)
            .find(|transcript| transcript.is_some() && self.can_cut(transcript))?;
        let last_turn = self
            .entries
            .iter()
            .rfind(|(record, _)| record.transcript == *transcript)
            .map(|(record, _)| record.line)?;
        Some(Cut::of_records(BTreeMap::from([(
            transcript.clone(),
            last_turn - 1,
        )])))
    }

    /// The label, identity and launching call of each sub-agent turn's
    /// thread, by the turn's record, as a read of the session's files finds
    /// them now.
    fn subagent_threads(&self) -> HashMap<RecordLine, serde_json::Value> {
        let stubs = self.fixture.discover_under(self.home.path());
        let [stub] = stubs.as_slice() else {
            return HashMap::new();
        };
        let Ok(session) = crate::history::normalized_session(
            crate::history::FOR_FIXTURES,
            self.fixture.provider().source(),
            &stub.locator,
            &stub.subagents,
        ) else {
            return HashMap::new();
        };
        session
            .entries
            .into_iter()
            .filter_map(|(record, entry)| match entry {
                LogEntry::Progress { data, extra } => Some((
                    record,
                    serde_json::json!([
                        data["agentId"],
                        data["identity"],
                        extra["parentToolUseID"]
                    ]),
                )),
                _ => None,
            })
            .collect()
    }

    /// The session's own transcript cut alone, before its last record that
    /// changes a sub-agent's thread row: without that record, a turn of the
    /// sub-agent has another label, identity or launching call than in the
    /// whole session. `None` when the session doesn't have such a record.
    ///
    /// Writes the session's files as each cut tried has them, and whole
    /// again before it returns.
    fn write_cuts_to_find_one_changing_thread_row(&self) -> Option<Cut> {
        let whole = self.subagent_threads();
        let mut own_lines: Vec<usize> = self
            .entries
            .iter()
            .filter(|(record, _)| record.transcript.is_none() && record.line > 1)
            .map(|(record, _)| record.line)
            .collect();
        own_lines.dedup();
        let mut changing_thread_row = None;
        for line in own_lines.into_iter().rev() {
            let cut = Cut::of_records(BTreeMap::from([(None, line - 1)]));
            self.write_cut(&cut);
            let is_thread_row_changed = self
                .subagent_threads()
                .iter()
                .any(|(record, thread)| whole.get(record).is_some_and(|whole| whole != thread));
            if is_thread_row_changed {
                changing_thread_row = Some(cut);
                break;
            }
        }
        self.write_whole();
        changing_thread_row
    }

    /// Write the session's files as `cut` has them.
    fn write_cut(&self, cut: &Cut) {
        self.stored.write_earlier(&cut.earlier);
    }

    /// Write the session's files whole.
    fn write_whole(&self) {
        self.stored.write_whole();
    }

    /// The session's viewer, reading it through a session cache of its own.
    fn open(&self) -> (TemporaryCacheDirectory, App) {
        let cache = admitted_cache();
        load_under(self.fixture, self.home.path(), cache.path());
        let rows = load_under(self.fixture, self.home.path(), cache.path());
        let environment = crate::history::environment::test_support::test_environment(
            vec![kept_storage_under(self.fixture, self.home.path())],
            Some(cache.path()),
        );
        let mut app = app_reading(rows, environment);
        app.selected = Some(0);
        app.enter_view_mode(80);
        (cache, app)
    }
}

/// Summary mode, without the timing column and thinking.
const SUMMARY_MODE: (ToolDisplayMode, bool, bool) = (ToolDisplayMode::Hidden, false, false);

/// Every call's whole output, with the timing column and thinking.
const FULL_OUTPUT_MODE: (ToolDisplayMode, bool, bool) = (ToolDisplayMode::Full, true, true);

/// A cut a test refreshes a session after, with its name and the display
/// modes its refreshes render in.
struct NamedCut {
    name: &'static str,
    cut: Cut,
    modes: &'static [(ToolDisplayMode, bool, bool)],
}

impl NamedCut {
    /// `cut`, refreshed with every call's whole output.
    fn in_full_output(name: &'static str, cut: Cut) -> Self {
        Self {
            name,
            cut,
            modes: &[FULL_OUTPUT_MODE],
        }
    }
}

/// The cuts of `session`: inside a tool run, between a call and its result,
/// inside a record when the agent rewrites a record, and, when the agent
/// writes sub-agent transcripts, inside a sub-agent's turns and of a
/// sub-agent's transcript alone. The cut inside a tool run also refreshes in
/// summary mode, which shows a tool run in one row.
fn cuts_of(session: &FixtureSession) -> Vec<NamedCut> {
    let agent = session.agent;
    let between_call_and_result = session
        .cut_between_call_and_result()
        .unwrap_or_else(|| panic!("{agent}: the session has a call and its result"));
    let inside_record = match between_call_and_result.earlier.running_call {
        Some(_) => None,
        None => session.cut_inside_record(),
    };
    assert_eq!(
        between_call_and_result.earlier.running_call.is_some() || inside_record.is_some(),
        session.is_in_database,
        "{agent}: a database has the part of a finished call, and a transcript of lines doesn't \
         have a record its agent rewrites"
    );
    let mut cuts = vec![
        NamedCut {
            name: "cut inside a tool run",
            cut: session.cut_inside_tool_run().unwrap_or_else(|| {
                panic!("{agent}: the session has a call after another call or a call's result")
            }),
            modes: &[SUMMARY_MODE, FULL_OUTPUT_MODE],
        },
        NamedCut::in_full_output("cut between a call and its result", between_call_and_result),
    ];
    cuts.extend(inside_record.map(|cut| NamedCut::in_full_output("cut inside a record", cut)));
    let inside_subagents_turns = session.cut_inside_subagents_turns();
    assert_eq!(
        inside_subagents_turns.is_some(),
        session.has_subagents,
        "{agent}: a session with sub-agent transcripts has a sub-agent's turn before its last \
         entry"
    );
    cuts.extend(
        inside_subagents_turns
            .map(|cut| NamedCut::in_full_output("cut inside a sub-agent's turns", cut)),
    );
    let of_subagents_transcript_alone = session.cut_of_subagents_transcript_alone();
    assert_eq!(
        of_subagents_transcript_alone.is_some(),
        session.has_subagents,
        "{agent}: a session with sub-agent transcripts has a sub-agent's turn"
    );
    cuts.extend(
        of_subagents_transcript_alone
            .map(|cut| NamedCut::in_full_output("sub-agent's transcript cut alone", cut)),
    );
    cuts
}

/// A session with the cuts a test refreshes it after.
type SessionWithCuts = (FixtureSession, Vec<NamedCut>);

/// `session` open in the viewer; then, for each of `cuts` in each of its
/// modes, the session written as the cut has it and refreshed, then written
/// whole and refreshed. The refresh of the whole session puts the tail
/// after the kept entries of the shown conversation, and each refresh shows
/// the conversation of a whole read and the rows of a whole render.
fn assert_refresh_after_each_cut_equals_whole_read_and_whole_render(
    (session, cuts): SessionWithCuts,
) {
    let (_cache, mut app) = session.open();

    for NamedCut { name, cut, modes } in &cuts {
        for &modes in *modes {
            let case = format!("{} {name} {modes:?}", session.agent);
            show_in(&mut app, modes);
            session.write_cut(cut);
            press_refresh(&mut app);
            if let Some(entries_kept) = cut.entries_kept {
                assert_eq!(
                    view(&app).parsed_conversation.entry_count(),
                    entries_kept,
                    "{case}: the cut session has the entries before the cut"
                );
            }
            assert_conversation_is_whole_reads(&app, &format!("{case}, the session cut"));
            assert_rows_are_whole_render(&app, &format!("{case}, the session cut"));
            let shown = conversation_address(&app);

            session.write_whole();
            press_refresh(&mut app);

            assert_eq!(
                conversation_address(&app),
                shown,
                "{case}: the reload puts the tail after the kept entries of the shown conversation"
            );
            assert_conversation_is_whole_reads(&app, &format!("{case}, the rest written"));
            assert_rows_are_whole_render(&app, &format!("{case}, the rest written"));
        }
    }
}

/// `assert_agent` run for each of `agents`, an agent's fixture or session.
/// Each agent's session runs on a thread of its own: an `App` marks its
/// thread with a `DrawingThread`, and the drawing thread doesn't parse a
/// transcript.
fn assert_for_each_agent<Agent: Send + 'static>(agents: Vec<Agent>, assert_agent: fn(Agent)) {
    let running: Vec<_> = agents
        .into_iter()
        .map(|agent| std::thread::spawn(move || assert_agent(agent)))
        .collect();
    let failed = running
        .into_iter()
        .filter_map(|agent| agent.join().err())
        .count();
    assert_eq!(failed, 0, "every agent's assertions pass");
}

#[test]
fn every_agents_session_written_in_part_shows_its_rest_as_whole_read_and_whole_render() {
    let sessions: Vec<SessionWithCuts> = registered_fixtures()
        .into_iter()
        .map(|fixture| {
            let session = FixtureSession::every_record(fixture);
            let cuts = cuts_of(&session);
            (session, cuts)
        })
        .collect();
    let with_subagents = sessions
        .iter()
        .filter(|(session, _)| session.has_subagents)
        .count();
    assert!(
        with_subagents > 1,
        "more than one agent writes sub-agent transcripts: {with_subagents}"
    );

    assert_for_each_agent(
        sessions,
        assert_refresh_after_each_cut_equals_whole_read_and_whole_render,
    );
}

/// One record of a Codex rollout: a response item with `payload`, written at
/// `second` of a minute after the fixture session's records.
fn codex_response_item(second: u32, payload: serde_json::Value) -> String {
    serde_json::json!({
        "timestamp": format!("2026-08-18T09:20:{second:02}.000Z"),
        "type": "response_item",
        "payload": payload,
    })
    .to_string()
}

/// A Codex session whose sub-agent wrote its turns before the session
/// recorded the result of the `spawn_agent` call that started it. The result
/// reports the sub-agent's thread, and the launching call in the thread row
/// of every turn of the sub-agent changes: no record of the sub-agent's
/// transcript changed.
#[test]
fn spawn_result_recorded_after_sub_agents_turns_changes_thread_row_and_equals_whole_read_and_whole_render()
 {
    let fixture = registered_fixtures()
        .into_iter()
        .find(|fixture| fixture.provider().source() == crate::history::Source::Codex)
        .expect("Codex has a fixture");
    let home = tempfile::tempdir().unwrap();
    let session_id = fixture.ids().session;
    let transcript = fixture.write_session(home.path(), session_id);
    fixture.write_subagent(home.path(), session_id);
    let spawn_result = serde_json::json!({"agent_id": fixture.subagent_id(session_id)});
    append_lines(
        &transcript,
        &[
            codex_response_item(
                0,
                serde_json::json!({
                    "type": "function_call",
                    "id": "fc_spawn",
                    "name": "spawn_agent",
                    "arguments": r#"{"agent_type":"explorer","message":"Read the notes."}"#,
                    "call_id": "call_spawn",
                }),
            ),
            codex_response_item(
                1,
                serde_json::json!({
                    "type": "function_call_output",
                    "id": "fco_spawn",
                    "call_id": "call_spawn",
                    "output": spawn_result.to_string(),
                }),
            ),
        ],
    );
    let session = FixtureSession::discovered(fixture, home);
    let before_spawn_result = session
        .write_cuts_to_find_one_changing_thread_row()
        .expect("the spawn result changes the launching call of the sub-agent's thread row");
    assert_eq!(
        before_spawn_result.earlier.records_kept.get(&None).copied(),
        session.entries.last().map(|(record, _)| record.line - 1),
        "the session's last record, the spawn result, changes the thread row"
    );

    assert_refresh_after_each_cut_equals_whole_read_and_whole_render((
        session,
        vec![NamedCut::in_full_output(
            "cut before the spawn result",
            before_spawn_result,
        )],
    ));
}

/// The viewer of 30 messages whose render for the timing column (`i`) waits
/// at the gate, after a refresh's read of 32 messages arrived: the reload's
/// tail waits for the render to free the shown conversation.
fn tail_waiting_for_render(home: &ClaudeHome) -> (App, Gate) {
    let path = home.transcript("session");
    write_messages(&path, &numbered(0..30));
    let (mut app, gate) = gated(viewer_on(home));
    press(&mut app, KeyCode::Char('i'));
    assert_eq!(gate.next_render(), (ToolDisplayMode::Hidden, true));
    write_messages(&path, &numbered(0..32));
    refresh_and_receive_read(&mut app);
    assert!(
        app.is_tail_waiting_for_conversation() && !view_text(&app).contains("message 31"),
        "the reload's tail waits while the display key's render shares the shown conversation"
    );
    (app, gate)
}

/// The render at the gate, let through, returns stopped.
fn let_stopped_render_return(gate: &Gate, render: &str) {
    gate.let_one_through();
    assert!(
        !gate.next_return().finished,
        "the waiting tail stops {render}: it shares the shown conversation"
    );
}

/// The waiting tail is put after the kept entries once the stopped renders
/// returned, and the reload's render, let through the gate, shows.
fn show_waiting_tail(app: &mut App, gate: &Gate) {
    finish_waiting_tail(app);
    gate.next_render();
    gate.let_one_through();
    finish_view_render(app, VIEWPORT);
}

#[test]
fn tail_arriving_during_render_is_put_after_kept_entries_once_render_stops() {
    let home = ClaudeHome::new();
    let (mut app, gate) = tail_waiting_for_render(&home);
    let shown = conversation_address(&app);

    let_stopped_render_return(&gate, "the display key's render");
    show_waiting_tail(&mut app, &gate);

    assert_eq!(
        conversation_address(&app),
        shown,
        "the tail is put after the kept entries of the shown conversation"
    );
    assert_conversation_is_whole_reads(&app, "a tail that waited for a render");
    assert_rows_are_whole_render(&app, "a tail that waited for a render");
    assert!(
        view(&app).show_timing && view_text(&app).contains("message 31"),
        "the new messages show with the timing column the display key asked for"
    );
}

#[test]
fn tail_arriving_after_render_finished_and_before_app_took_it_is_put_after_kept_entries() {
    let home = ClaudeHome::new();
    let path = home.transcript("session");
    write_messages(&path, &numbered(0..30));
    let (mut app, gate) = gated(viewer_on(&home));
    press(&mut app, KeyCode::Char('i'));
    assert_eq!(gate.next_render(), (ToolDisplayMode::Hidden, true));
    gate.let_one_through();
    assert!(
        gate.next_return().finished,
        "the display key's render finishes before a pass takes it"
    );
    let shown = conversation_address(&app);

    write_messages(&path, &numbered(0..32));
    refresh_and_receive_read(&mut app);
    let waits_for_finished_render = app.is_tail_waiting_for_conversation();
    let deadline = std::time::Instant::now() + VIEWER_READ_LANDS;
    while app.is_tail_waiting_for_conversation() {
        assert!(
            std::time::Instant::now() < deadline,
            "the tail is put after the kept entries within {VIEWER_READ_LANDS:?}"
        );
        app.receive_view_render(VIEWPORT, Duration::from_millis(1));
        app.complete_pending_view_refresh(Duration::ZERO);
    }
    gate.next_render();
    gate.let_one_through();
    finish_view_render(&mut app, VIEWPORT);

    assert!(
        waits_for_finished_render,
        "the tail waits while the finished render, not yet taken, shares the shown conversation"
    );
    let renders = gate.extents.started();
    assert!(
        matches!(
            renders.as_slice(),
            [RowsToRender::Whole, RowsToRender::Tail(_)]
        ),
        "the display key's whole render shows, then the reload renders the tail: {renders:?}"
    );
    assert_eq!(
        conversation_address(&app),
        shown,
        "the tail is put after the kept entries of the shown conversation"
    );
    assert_conversation_is_whole_reads(&app, "a tail that waited for a finished render");
    assert_rows_are_whole_render(&app, "a tail that waited for a finished render");
    assert!(
        view(&app).show_timing && view_text(&app).contains("message 31"),
        "the new messages show with the timing column the display key asked for"
    );
}

#[test]
fn display_key_while_tail_waits_shows_its_mode_once_tail_is_put_after_kept_entries() {
    let home = ClaudeHome::new();
    let (mut app, gate) = tail_waiting_for_render(&home);

    press(&mut app, KeyCode::Char('t'));
    let waits_for_second_render = !app.complete_pending_view_refresh(Duration::ZERO)
        && app.is_tail_waiting_for_conversation();
    let_stopped_render_return(&gate, "the first display key's render");
    assert_eq!(gate.next_render(), (ToolDisplayMode::Truncated, true));
    let_stopped_render_return(&gate, "the second display key's render");
    show_waiting_tail(&mut app, &gate);

    assert!(
        waits_for_second_render,
        "the tail waits while the second display key's render shares the conversation"
    );
    assert_eq!(
        (view(&app).tool_display, view(&app).show_timing),
        (ToolDisplayMode::Truncated, true),
        "the rows show in the mode of both display keys"
    );
    assert_conversation_is_whole_reads(&app, "a display key while the tail waits");
    assert_rows_are_whole_render(&app, "a display key while the tail waits");
    assert!(
        view_text(&app).contains("message 31"),
        "the new messages show"
    );
}

#[test]
fn resize_while_tail_waits_shows_new_width_once_tail_is_put_after_kept_entries() {
    let home = ClaudeHome::new();
    let (mut app, gate) = tail_waiting_for_render(&home);

    app.check_view_resize(100);
    let waits_for_resize_render = !app.complete_pending_view_refresh(Duration::ZERO)
        && app.is_tail_waiting_for_conversation();
    let_stopped_render_return(&gate, "the display key's render");
    gate.next_render();
    let_stopped_render_return(&gate, "the resize's render");
    show_waiting_tail(&mut app, &gate);

    assert!(
        waits_for_resize_render,
        "the tail waits while the resize's render shares the conversation"
    );
    assert_eq!(
        view(&app).frame_width,
        100,
        "the rows are rendered for the new width"
    );
    assert_conversation_is_whole_reads(&app, "a resize while the tail waits");
    assert_rows_are_whole_render(&app, "a resize while the tail waits");
    assert!(
        view_text(&app).contains("message 31"),
        "the new messages show"
    );
}

/// [`numbered`] messages, each longer than a frame of 80 columns and
/// shorter than one of 100.
fn numbered_wrapping_at_80_columns(numbers: std::ops::Range<usize>) -> Vec<String> {
    numbered(numbers)
        .into_iter()
        .map(|text| format!("{text} {}", "word ".repeat(15)))
        .collect()
}

#[test]
fn resize_while_reloads_read_is_pending_equals_whole_read_and_whole_render_at_new_width() {
    let home = ClaudeHome::new();
    let path = home.transcript("session");
    write_messages(&path, &numbered_wrapping_at_80_columns(0..30));
    let (mut app, gate) = gated(viewer_on(&home));
    let rows_at_80_columns = view(&app).total_lines;
    write_messages(&path, &numbered_wrapping_at_80_columns(0..32));
    request_refresh(&mut app);
    let is_read_pending = app.is_reading_open_session();

    app.check_view_resize(100);
    gate.next_render();
    assert!(
        app.complete_pending_view_refresh(VIEWER_READ_LANDS),
        "the reload's read arrives"
    );
    let_stopped_render_return(&gate, "the resize's render");
    show_waiting_tail(&mut app, &gate);

    assert!(
        is_read_pending,
        "the reload's read is pending when the frame is resized"
    );
    assert_eq!(
        view(&app).frame_width,
        100,
        "the rows are rendered for the new width"
    );
    assert!(
        view(&app).total_lines < rows_at_80_columns,
        "32 messages in 100 columns have fewer rows than 30 messages in 80 columns: {} and \
         {rows_at_80_columns}",
        view(&app).total_lines
    );
    assert_conversation_is_whole_reads(&app, "a resize while the reload's read is pending");
    assert_rows_are_whole_render(&app, "a resize while the reload's read is pending");
    assert!(
        view_text(&app).contains("message 31"),
        "the new messages show"
    );
}

#[test]
fn refresh_finishing_while_tail_waits_reads_once_tail_rows_show() {
    let home = ClaudeHome::new();
    let (mut app, gate) = tail_waiting_for_render(&home);

    write_messages(&home.transcript("session"), &numbered(0..33));
    request_refresh(&mut app);
    let reads_while_tail_waits = app.is_reading_open_session();
    let_stopped_render_return(&gate, "the display key's render");
    show_waiting_tail(&mut app, &gate);
    let first_rows = view_text(&app);
    let reads_once_tail_rows_show = app.is_reading_open_session();
    gate.let_one_through();
    finish_pending_view_reload(&mut app, VIEWPORT);

    assert!(
        !reads_while_tail_waits && reads_once_tail_rows_show,
        "the second refresh's read starts once the waiting tail's rows show"
    );
    assert!(
        first_rows.contains("message 31") && !first_rows.contains("message 32"),
        "the waiting tail's rows show first: {first_rows:?}"
    );
    assert_conversation_is_whole_reads(&app, "a second refresh while the tail waits");
    assert_rows_are_whole_render(&app, "a second refresh while the tail waits");
    assert!(
        view_text(&app).contains("message 32"),
        "the second refresh's message shows"
    );
}

#[test]
fn leaving_viewer_while_tail_waits_drops_tail_and_next_viewer_reads_refreshed_copy() {
    let home = ClaudeHome::new();
    let (mut app, gate) = tail_waiting_for_render(&home);

    press(&mut app, KeyCode::Char('q'));
    let is_tail_dropped =
        matches!(app.app_mode(), AppMode::List) && !app.is_tail_waiting_for_conversation();
    let_stopped_render_return(&gate, "the display key's render");
    app.enter_view_mode(80);

    assert!(is_tail_dropped, "leaving the viewer drops the waiting tail");
    assert_rows_are_whole_render(&app, "the viewer opened again");
    assert!(
        view_text(&app).contains("message 31"),
        "the viewer opened again shows the refreshed copy's messages"
    );
}

/// The view's hashes replaced, with equal ones, while the reload's read
/// runs. No path of the app does this: a reload alone replaces the hashes,
/// and one reload runs at a time.
#[test]
fn tail_compared_with_other_hashes_than_views_is_dropped_and_whole_copy_is_read() {
    let home = ClaudeHome::new();
    let path = home.transcript("session");
    write_messages(&path, &numbered(0..30));
    let mut app = viewer_on(&home);
    write_messages(&path, &numbered(0..32));
    request_refresh(&mut app);
    if let AppMode::View(state) = &mut app.app_mode {
        state.entry_hashes = state.entry_hashes.as_deref().cloned().map(Arc::new);
    }
    let shown = conversation_address(&app);

    let first_read_arrived = finish_pending_view_reload(&mut app, VIEWPORT);
    let reads_again = app.is_reading_open_session() && !view_text(&app).contains("message 31");
    let whole_read_arrived = finish_pending_view_reload(&mut app, VIEWPORT);

    assert!(
        first_read_arrived && reads_again,
        "the read compared with other hashes isn't shown, and the copy is read again"
    );
    assert!(whole_read_arrived, "the whole read arrives");
    assert_ne!(
        conversation_address(&app),
        shown,
        "the whole read's conversation replaces the shown one"
    );
    assert_conversation_is_whole_reads(&app, "a read compared with other hashes");
    assert_rows_are_whole_render(&app, "a read compared with other hashes");
    assert!(
        view_text(&app).contains("message 31"),
        "the new messages show"
    );
}

/// The view keeps the hashes of an earlier copy, one fewer than its
/// entries.
#[test]
fn view_with_other_count_of_hashes_than_entries_reads_whole_copy() {
    let home = ClaudeHome::new();
    let path = home.transcript("session");
    write_messages(&path, &numbered(0..3));
    let mut app = viewer_on(&home);
    let earlier_hashes = view(&app).entry_hashes.clone();
    write_messages(&path, &numbered(0..4));
    press_refresh(&mut app);
    if let AppMode::View(state) = &mut app.app_mode {
        state.entry_hashes = earlier_hashes;
    }
    let shown = conversation_address(&app);

    write_messages(&path, &numbered(0..5));
    press_refresh(&mut app);

    assert_ne!(
        conversation_address(&app),
        shown,
        "the view with one hash fewer than its entries shows a whole read's conversation"
    );
    assert_conversation_is_whole_reads(&app, "another count of hashes than entries");
    assert_rows_are_whole_render(&app, "another count of hashes than entries");
    assert!(
        view_text(&app).contains("message 4"),
        "the new message shows"
    );
}
