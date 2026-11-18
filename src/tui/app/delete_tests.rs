//! Deletes from the list and the viewer: the dialogs act on the session they
//! were opened for, the selection follows its session, and a confirmed
//! delete shows while it runs.

use super::refresh_tests::{
    answer_search, app_listing, finish_deleting, listed_names, selected_name, session,
    take_over_search_results,
};
use super::*;
use crate::history::{RefreshOutcome, SessionChanges, UpdatedSession};
use crate::tui::list_change::ListChange;

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

#[test]
fn menu_prompt_and_name_box_close_when_their_session_leaves_list() {
    let dialogs: [(&str, fn(&mut App)); 3] = [
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

/// Select the row of the session at `path`, as the arrow keys select it.
fn select(app: &mut App, path: &std::path::Path) {
    let row = app
        .filtered()
        .iter()
        .position(|&index| app.conversations()[index].path == path)
        .unwrap_or_else(|| panic!("{} is shown", path.display()));
    app.selected = Some(row);
}

/// Confirm the delete of the session at `path` through the actions menu,
/// and hand it on as the runtime hands a confirmed delete to the worker.
fn confirm_delete_of(app: &mut App, path: &std::path::Path) {
    select(app, path);
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
        let environment = app.environment.clone();
        let outcome = std::thread::spawn(move || {
            let operation =
                crate::history::environment::test_support::waiting_operation(&environment);
            let outcome = change.make(&operation);
            operation.finish().fail_if_low_space().unwrap();
            outcome
        })
        .join()
        .unwrap();
        app.finish_list_change(outcome);
    }
    served
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
