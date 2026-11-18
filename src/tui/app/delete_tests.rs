//! Deletes from the list and the viewer: the dialogs act on the session they
//! were opened for, the selection follows its session, and a confirmed
//! delete shows while it runs.

use super::refresh_tests::{
    answer_search, app_listing, finish_deleting, listed_names, selected_name, session,
    take_over_search_results,
};
use super::*;
use crate::history::{RefreshOutcome, SessionChanges, UpdatedSession};
use crate::tui::list_change::{ListChange, ListChangeOutcome};

/// An app listing sessions `c`, `b` and `a`, newest first.
fn three_sessions() -> App {
    app_listing(vec![
        session("c", "c", 15, 1),
        session("b", "b", 10, 1),
        session("a", "a", 5, 1),
    ])
}

fn press(app: &mut App, code: KeyCode) -> Option<Action> {
    app.handle_key(code, KeyModifiers::NONE, 10)
}

fn press_ctrl(app: &mut App, code: KeyCode) -> Option<Action> {
    app.handle_key(code, KeyModifiers::CONTROL, 10)
}

fn path_of(app: &App, name: &str) -> PathBuf {
    app.conversations()
        .iter()
        .find(|row| row.session_id == name)
        .unwrap_or_else(|| panic!("{name} is listed"))
        .path
        .clone()
}

/// Search results for every row arrive while a dialog is open; with no
/// selection anchor, they select the first row.
fn move_selection_to_first_row_by_search_results(app: &mut App) {
    let results = take_over_search_results(app);
    let every_row = (0..app.conversations().len()).collect();
    answer_search(app, &results, every_row);
    assert_eq!(selected_name(app), Some("c"), "the selection moved");
}

#[test]
fn y_deletes_session_prompt_was_opened_for_after_selection_moved() {
    let mut app = three_sessions();
    press(&mut app, KeyCode::Down);
    press_ctrl(&mut app, KeyCode::Char('x'));
    press(&mut app, KeyCode::Char('d'));
    move_selection_to_first_row_by_search_results(&mut app);

    let action = press(&mut app, KeyCode::Char('y'));

    assert!(
        matches!(action, Some(Action::Delete(ref path)) if *path == path_of(&app, "b")),
        "y deletes the session the delete prompt was opened for"
    );
}

#[test]
fn menus_resume_and_fork_act_on_session_menu_was_opened_for_after_selection_moved() {
    for (key, is_expected) in [
        (
            'r',
            (|action| matches!(action, Action::Resume(_))) as fn(&Action) -> bool,
        ),
        ('f', |action| matches!(action, Action::ForkResume(_))),
    ] {
        let mut app = three_sessions();
        press(&mut app, KeyCode::Down);
        press_ctrl(&mut app, KeyCode::Char('x'));
        move_selection_to_first_row_by_search_results(&mut app);

        let action =
            press(&mut app, KeyCode::Char(key)).unwrap_or_else(|| panic!("{key} runs its action"));

        let acted_on = match &action {
            Action::Resume(path) | Action::ForkResume(path) => path.clone(),
            _ => PathBuf::new(),
        };
        assert!(is_expected(&action), "{key} runs its own action");
        assert_eq!(
            acted_on,
            path_of(&app, "b"),
            "{key} acts on the session the menu was opened for"
        );
    }
}

#[test]
fn menus_rename_renames_session_menu_was_opened_for_after_selection_moved() {
    let mut app = three_sessions();
    press(&mut app, KeyCode::Down);
    press_ctrl(&mut app, KeyCode::Char('x'));
    press(&mut app, KeyCode::Char('m'));
    move_selection_to_first_row_by_search_results(&mut app);

    press(&mut app, KeyCode::Char('n'));
    press(&mut app, KeyCode::Enter);

    let renamed = match app.begin_list_change() {
        Some(ListChange::Rename { path, .. }) => path,
        _ => panic!("Enter requests a rename"),
    };
    assert_eq!(
        renamed,
        path_of(&app, "b"),
        "the name box renames the session it was opened for"
    );
}

#[test]
fn viewers_menu_acts_on_open_session_after_selection_moved() {
    let mut app = three_sessions();
    press(&mut app, KeyCode::Down);
    app.enter_view_mode(80);
    move_selection_to_first_row_by_search_results(&mut app);

    press_ctrl(&mut app, KeyCode::Char('x'));
    let action = press(&mut app, KeyCode::Char('r'));

    assert!(
        matches!(action, Some(Action::Resume(ref path)) if *path == path_of(&app, "b")),
        "the viewer's menu resumes the open session"
    );
}

/// Keys that open a dialog on the selected session.
type OpenDialog = fn(&mut App);

#[test]
fn menu_prompt_and_name_box_close_when_their_session_leaves_list() {
    let dialogs: [(&str, OpenDialog); 3] = [
        ("the actions menu", |app| {
            press_ctrl(app, KeyCode::Char('x'));
        }),
        ("the delete prompt", |app| {
            press_ctrl(app, KeyCode::Char('x'));
            press(app, KeyCode::Char('d'));
        }),
        ("the name box", |app| {
            press_ctrl(app, KeyCode::Char('x'));
            press(app, KeyCode::Char('m'));
        }),
    ];
    for (dialog, open) in dialogs {
        let mut app = three_sessions();
        press(&mut app, KeyCode::Down);
        open(&mut app);
        assert_ne!(app.dialog_mode, DialogMode::None, "{dialog} opens");
        let b = path_of(&app, "b");

        finish_deleting(&mut app, &b);

        assert_eq!(listed_names(&app), ["c", "a"]);
        assert_eq!(
            app.dialog_mode,
            DialogMode::None,
            "{dialog} closes once its session leaves the list"
        );
    }
}

/// Apply a list refresh that found `changes` to `app`.
fn refresh_finding(app: &mut App, changes: SessionChanges) {
    app.begin_refresh().expect("the refresh begins");
    app.finish_refresh(Ok(RefreshOutcome::Changes(Box::new(changes))));
}

fn new_row(row: Conversation) -> UpdatedSession {
    UpdatedSession {
        row,
        replaces_listed_row: false,
        next: crate::history::NextStep::Settled,
    }
}

#[test]
fn selection_stays_on_its_session_when_a_delete_removes_a_row_above_it() {
    let mut app = three_sessions();
    press(&mut app, KeyCode::Down);
    let c = path_of(&app, "c");

    finish_deleting(&mut app, &c);

    assert_eq!(selected_name(&app), Some("b"));
}

#[test]
fn selection_stays_on_its_session_when_a_refresh_adds_or_removes_rows_above_it() {
    let mut app = three_sessions();
    press(&mut app, KeyCode::Down);
    let c = path_of(&app, "c");

    refresh_finding(
        &mut app,
        SessionChanges {
            updated: vec![new_row(session("d", "d", 20, 1))],
            ..Default::default()
        },
    );
    assert_eq!(listed_names(&app), ["d", "c", "b", "a"]);
    assert_eq!(selected_name(&app), Some("b"), "a row added above");

    refresh_finding(
        &mut app,
        SessionChanges {
            removed: vec![c],
            ..Default::default()
        },
    );
    assert_eq!(listed_names(&app), ["d", "b", "a"]);
    assert_eq!(selected_name(&app), Some("b"), "a row removed above");
}

#[test]
fn removed_selected_session_passes_selection_to_row_taking_its_place() {
    let mut app = three_sessions();
    press(&mut app, KeyCode::Down);
    let b = path_of(&app, "b");

    refresh_finding(
        &mut app,
        SessionChanges {
            removed: vec![b],
            ..Default::default()
        },
    );

    assert_eq!(listed_names(&app), ["c", "a"]);
    assert_eq!(selected_name(&app), Some("a"));
}

#[test]
fn removed_last_selected_session_passes_selection_to_new_last_row() {
    let mut app = three_sessions();
    press(&mut app, KeyCode::End);
    let a = path_of(&app, "a");

    refresh_finding(
        &mut app,
        SessionChanges {
            removed: vec![a],
            ..Default::default()
        },
    );

    assert_eq!(listed_names(&app), ["c", "b"]);
    assert_eq!(selected_name(&app), Some("b"));
}

/// Select the row of the session at `path` with `Home`, then `Down` until
/// the selection reaches it.
fn select_with_keys(app: &mut App, path: &std::path::Path) {
    press(app, KeyCode::Home);
    for _ in 0..app.filtered().len() {
        if app.get_selected_path().as_deref() == Some(path) {
            return;
        }
        press(app, KeyCode::Down);
    }
    panic!("the keys don't reach {}", path.display());
}

/// Confirm the delete of the session at `path` through the actions menu,
/// and hand it on as the runtime hands a confirmed delete to the worker.
fn confirm_delete_of(app: &mut App, path: &std::path::Path) {
    select_with_keys(app, path);
    press_ctrl(app, KeyCode::Char('x'));
    press(app, KeyCode::Char('d'));
    match press(app, KeyCode::Char('y')) {
        Some(Action::Delete(deleted)) => app.request_delete(&deleted),
        _ => panic!("y deletes {}", path.display()),
    }
}

/// Run each rename or delete `app` hands the worker, oldest first, as the
/// worker runs it; returns the paths in the order they ran.
fn serve_every_list_change(app: &mut App) -> Vec<PathBuf> {
    let mut served = Vec::new();
    while let Some(change) = app.begin_list_change() {
        served.push(match &change {
            ListChange::Rename { path, .. } | ListChange::Delete { path, .. } => path.clone(),
        });
        let outcome = make_as_worker(app, change);
        app.finish_list_change(outcome);
    }
    served
}

/// Make `change` in `app`'s environment on a thread of its own, as the
/// worker makes it.
fn make_as_worker(app: &App, change: ListChange) -> ListChangeOutcome {
    let environment = app.environment.clone();
    std::thread::spawn(move || {
        let operation = crate::history::environment::test_support::waiting_operation(&environment);
        let outcome = change.make(&operation, environment.clock().as_ref());
        operation.finish().fail_if_low_space().unwrap();
        outcome
    })
    .join()
    .unwrap()
}

/// The Codex thread ID numbered `number`, which the rollout's file name
/// carries.
fn codex_thread(number: usize) -> String {
    format!("019f0000-0000-7000-8000-0000000000{number:02}")
}

/// An app listing Codex sessions `0` to `5` written under `home`, loaded
/// through the test's own session cache. Codex deletes the sessions under
/// the tree holding the transcript, so a delete stays inside `home`.
fn app_listing_six_codex_sessions(home: &std::path::Path) -> (App, Vec<PathBuf>) {
    use crate::history::provider::contract_tests::{kept_storage_under, registered_fixtures};
    let fixture = registered_fixtures()
        .into_iter()
        .find(|fixture| fixture.storage().source() == crate::history::Source::Codex)
        .expect("Codex has a fixture");
    let transcripts = (0..6)
        .map(|number| fixture.write_session(home, &codex_thread(number)))
        .collect();
    let environment = crate::history::environment::test_support::test_environment(
        vec![kept_storage_under(fixture, home)],
        None,
    );
    let rows = crate::history::environment::test_support::load_rows_in(
        &environment,
        kept_storage_under(fixture, home),
    );
    (
        super::claude_home::app_reading(rows, environment),
        transcripts,
    )
}

/// The listed path of the session whose transcript is named as `transcript`.
fn listed_path_of(app: &App, transcript: &std::path::Path) -> PathBuf {
    app.conversations()
        .iter()
        .map(|row| row.path.clone())
        .find(|path| path.file_name() == transcript.file_name())
        .unwrap_or_else(|| panic!("{} is listed", transcript.display()))
}

#[test]
fn every_delete_confirmed_before_the_first_ends_is_carried_out_in_order() {
    let home = tempfile::tempdir().unwrap();
    let (mut app, transcripts) = app_listing_six_codex_sessions(home.path());
    let confirmed: Vec<PathBuf> = [4, 0, 2, 5, 1]
        .iter()
        .map(|&number| listed_path_of(&app, &transcripts[number]))
        .collect();

    for path in &confirmed {
        confirm_delete_of(&mut app, path);
    }
    let served = serve_every_list_change(&mut app);

    assert_eq!(served, confirmed, "each confirmed delete runs, in order");
    assert_eq!(
        app.conversations().len(),
        1,
        "the five deleted rows leave the list: {:?}",
        app.status_message()
    );
    for (number, transcript) in transcripts.iter().enumerate() {
        assert_eq!(
            transcript.exists(),
            number == 3,
            "session {number}'s transcript is deleted only when its delete was confirmed"
        );
    }
}

#[test]
fn menu_stays_open_when_another_session_leaves_list() {
    let mut app = three_sessions();
    press(&mut app, KeyCode::Down);
    press_ctrl(&mut app, KeyCode::Char('x'));
    let c = path_of(&app, "c");

    finish_deleting(&mut app, &c);

    assert!(
        matches!(&app.dialog_mode, DialogMode::ActionsMenu { session: Some(session), .. }
            if *session == path_of(&app, "b")),
        "the menu stays open on its own session"
    );
}

/// Confirm the delete of session `name` through the actions menu, and hand
/// it on as the runtime does; the worker doesn't take it.
fn confirm_delete_of_named(app: &mut App, name: &str) {
    let path = path_of(app, name);
    confirm_delete_of(app, &path);
}

/// Hand `app` a failed outcome of the delete of the session at `path`.
fn fail_deleting(app: &mut App, path: &std::path::Path) {
    app.finish_list_change(crate::tui::list_change::ListChangeOutcome::Deleted {
        path: path.to_path_buf(),
        result: Err(crate::error::AppError::DeleteRefused("refused".to_owned())),
        stages: crate::tui::list_change::DeleteStages::default(),
    });
}

#[test]
fn confirmed_delete_passes_selection_to_next_row_or_previous_when_last() {
    let mut app = three_sessions();
    confirm_delete_of_named(&mut app, "b");
    assert_eq!(selected_name(&app), Some("a"), "the next row");

    confirm_delete_of_named(&mut app, "a");
    assert_eq!(
        selected_name(&app),
        Some("c"),
        "the previous selectable row, with none after"
    );
}

#[test]
fn keys_and_click_pass_over_deleting_row() {
    let mut app = three_sessions();
    confirm_delete_of_named(&mut app, "b");

    for (key, expected) in [
        (KeyCode::Up, "c"),
        (KeyCode::Down, "a"),
        (KeyCode::PageUp, "c"),
        (KeyCode::PageDown, "a"),
        (KeyCode::Home, "c"),
        (KeyCode::End, "a"),
    ] {
        press(&mut app, key);
        assert_eq!(selected_name(&app), Some(expected), "{key:?}");
    }

    let frame = ratatui::layout::Rect::new(0, 0, 100, 30);
    let list_top = 3;
    let b_row = list_top + LIST_LINES_PER_ITEM as u16;
    assert!(
        !app.handle_list_click(b_row, frame),
        "a click on the deleting row doesn't land"
    );
    assert_eq!(selected_name(&app), Some("a"));
}

#[test]
fn search_results_list_deleting_row_without_selecting_it() {
    let mut app = three_sessions();
    confirm_delete_of_named(&mut app, "c");
    press(&mut app, KeyCode::End);
    let results = take_over_search_results(&mut app);

    answer_search(&mut app, &results, vec![0, 1, 2]);

    assert_eq!(listed_names(&app), ["c", "b", "a"]);
    assert_eq!(
        selected_name(&app),
        Some("b"),
        "the results select the first selectable row"
    );
}

#[test]
fn failed_delete_makes_row_selectable_again_and_reports_failure() {
    let mut app = three_sessions();
    confirm_delete_of_named(&mut app, "b");
    let b = path_of(&app, "b");

    fail_deleting(&mut app, &b);
    press(&mut app, KeyCode::Up);

    assert_eq!(selected_name(&app), Some("b"));
    assert!(!app.is_deleting(&b));
    assert_eq!(
        app.status_message().map(|(message, _)| message.as_str()),
        Some("Failed to delete: refused")
    );
    assert_eq!(app.deleting_status_text(), None);
}

#[test]
fn failed_delete_of_only_row_lets_keys_act_on_it_again() {
    let mut app = app_listing(vec![session("a", "a", 5, 1)]);
    confirm_delete_of_named(&mut app, "a");
    assert_eq!(selected_name(&app), None, "the deleting row isn't selected");
    let a = path_of(&app, "a");

    fail_deleting(&mut app, &a);
    press(&mut app, KeyCode::Down);
    press(&mut app, KeyCode::Enter);

    assert_eq!(selected_name(&app), Some("a"), "Down keeps a selected");
    assert_eq!(
        app.pending_open.as_ref().map(|open| &open.path),
        Some(&a),
        "Enter opens a"
    );
}

#[test]
fn row_arriving_while_every_listed_row_is_deleting_is_selected() {
    let mut app = App::new_loading_with_options(
        ToolDisplayMode::Hidden,
        false,
        KeyBindings::default(),
        ListScope::default(),
        TuiSearchOptions::default(),
        crate::history::environment::test_support::test_environment(Vec::new(), None),
    );
    app.append_conversations(vec![session("b", "b", 10, 1)]);
    // Delete is available during the launch while it waits for another
    // rearview.
    app.set_cache_wait(Some(crate::history::OtherRearview::Rebuilding));
    confirm_delete_of_named(&mut app, "b");

    app.append_conversations(vec![session("a", "a", 5, 1)]);

    assert_eq!(
        selected_name(&app),
        Some("a"),
        "the arriving row is selected, not the deleting one"
    );
}

#[test]
fn y_in_viewer_returns_to_list_showing_session_as_deleting() {
    let mut app = three_sessions();
    press(&mut app, KeyCode::Down);
    app.enter_view_mode(80);
    let b = path_of(&app, "b");

    press_ctrl(&mut app, KeyCode::Char('x'));
    press(&mut app, KeyCode::Char('d'));
    match press(&mut app, KeyCode::Char('y')) {
        Some(Action::Delete(path)) => app.request_delete(&path),
        _ => panic!("y deletes the open session"),
    }

    assert!(matches!(app.app_mode(), AppMode::List));
    assert!(app.is_deleting(&b));
    assert_eq!(selected_name(&app), Some("a"));
}

/// An app on `clock` listing sessions `c`, `b` and `a`, newest first.
fn three_sessions_on(clock: &Arc<crate::clock::TestClock>) -> App {
    super::claude_home::app_reading(
        vec![
            session("c", "c", 15, 1),
            session("b", "b", 10, 1),
            session("a", "a", 5, 1),
        ],
        crate::history::environment::test_support::environment_on(clock.clone()),
    )
}

#[test]
fn bar_shows_deleting_for_single_session_and_count_for_multiple() {
    let clock = Arc::new(crate::clock::TestClock::default());
    let mut app = three_sessions_on(&clock);
    assert_eq!(app.deleting_status_text(), None);

    confirm_delete_of_named(&mut app, "b");
    assert_eq!(app.deleting_status_text().as_deref(), Some("Deleting…"));
    clock.advance(std::time::Duration::from_millis(200));
    assert_eq!(app.deleting_status_text().as_deref(), Some("⠹ Deleting…"));

    confirm_delete_of_named(&mut app, "c");
    clock.advance(std::time::Duration::from_millis(1000));
    assert_eq!(
        app.deleting_status_text().as_deref(),
        Some("⠴ Deleting 2 sessions… 1s"),
        "the spinner and seconds count from the first delete"
    );

    for name in ["b", "c"] {
        let path = path_of(&app, name);
        finish_deleting(&mut app, &path);
    }
    assert_eq!(app.deleting_status_text(), None);
}

#[test]
fn delete_records_its_wait_in_the_queue_from_y() {
    let clock = Arc::new(crate::clock::TestClock::default());
    let mut app = three_sessions_on(&clock);
    confirm_delete_of_named(&mut app, "b");
    clock.advance(std::time::Duration::from_secs(2));

    let change = app.begin_list_change().expect("the delete is queued");
    let outcome = make_as_worker(&app, change);

    let ListChangeOutcome::Deleted { stages, .. } = outcome else {
        panic!("the delete ends with a delete's outcome");
    };
    assert_eq!(
        stages.queue,
        std::time::Duration::from_secs(2),
        "the queue stage counts from y until the worker begins the delete"
    );
}

/// Every character of the frame `app` draws `width` columns wide, row
/// after row.
fn drawn_rows(app: &App, width: u16) -> Vec<String> {
    let mut terminal =
        ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, 20)).unwrap();
    terminal
        .draw(|frame| crate::tui::ui::render(frame, app))
        .unwrap();
    let buffer = terminal.backend().buffer();
    (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect()
        })
        .collect()
}

#[test]
fn prompt_names_session_by_its_title_truncated_to_fit() {
    let mut row = session("b", "b", 10, 1);
    row.custom_title = Some("Plan the release of the migration tool".to_owned());
    let mut app = app_listing(vec![row]);
    press_ctrl(&mut app, KeyCode::Char('x'));
    press(&mut app, KeyCode::Char('d'));

    let wide = drawn_rows(&app, 80);
    let narrow = drawn_rows(&app, 40);

    assert!(
        wide.iter()
            .any(|row| row.contains(" Delete \"Plan the release of the migration tool\"? (y/n)")),
        "{wide:#?}"
    );
    assert!(
        narrow
            .iter()
            .any(|row| row.contains(" Delete \"Plan the release of …\"? (y/n)")),
        "{narrow:#?}"
    );
}

#[test]
fn deleting_row_is_drawn_in_disabled_grey() {
    let mut app = three_sessions();
    confirm_delete_of_named(&mut app, "b");
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 20)).unwrap();

    terminal
        .draw(|frame| crate::tui::ui::render(frame, &app))
        .unwrap();

    let buffer = terminal.backend().buffer();
    let disabled = crate::tui::theme::detect_theme().dim_label;
    let disabled = ratatui::style::Color::Rgb(disabled.0, disabled.1, disabled.2);
    // Inside the border, below the search bar, each row's header line and
    // preview line.
    let first_row_line = 3;
    let lines_of = |row: u16| {
        let top = first_row_line + row * LIST_LINES_PER_ITEM as u16;
        top..top + 2
    };
    let is_grey = |row: u16| {
        lines_of(row).all(|y| {
            (1..buffer.area.width - 1)
                .map(|x| &buffer[(x, y)])
                .filter(|cell| !cell.symbol().trim().is_empty())
                .all(|cell| cell.fg == disabled)
        })
    };
    assert!(is_grey(1), "b's row, deleting, is grey");
    assert!(!is_grey(2), "a's row keeps its colours");
}
