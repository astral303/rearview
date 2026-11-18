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
