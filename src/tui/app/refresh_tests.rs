use super::refresh_state::AppliedChanges;
use super::*;
use crate::clock::TestClock;
use crate::error::AppError;
use crate::history::cache::CachedFingerprint;
use crate::history::provider::{ReadError, SessionRead, SessionTitle};
use crate::history::{FoundSession, Source, UpdatedSession};
use crate::search::test_fixtures::one_message_conversation;
use chrono::TimeZone;

fn fingerprint(size: u64) -> CachedFingerprint {
    CachedFingerprint {
        file_size: size,
        mtime_secs: size,
        mtime_nsecs: 0,
    }
}

/// A session named `name`, active at `minute` past noon, read from
/// transcripts of `size` bytes.
fn session(name: &str, text: &str, minute: u32, size: u64) -> Conversation {
    let mut row = one_message_conversation(
        text,
        chrono::Local
            .with_ymd_and_hms(2026, 10, 1, 12, minute, 0)
            .unwrap(),
        None,
        None,
        Some("project"),
    );
    row.path = PathBuf::from(format!("/sessions/{name}.jsonl"));
    row.session_id = name.to_owned();
    row.fingerprint = Some(fingerprint(size));
    row
}

/// An app listing `rows`, newest first.
fn app_listing(rows: Vec<Conversation>) -> App {
    App::new(
        rows,
        ToolDisplayMode::Hidden,
        false,
        KeyBindings::default(),
        vec![],
        Arc::new(TestClock::default()),
    )
}

fn listed_names(app: &App) -> Vec<&str> {
    app.filtered()
        .iter()
        .map(|&index| app.conversations()[index].session_id.as_str())
        .collect()
}

fn selected_name(app: &App) -> Option<&str> {
    app.get_selected_conversation_index()
        .map(|index| app.conversations()[index].session_id.as_str())
}

fn new_session(row: Conversation) -> UpdatedSession {
    UpdatedSession {
        row,
        replaces_listed_row: false,
    }
}

fn changed_session(row: Conversation) -> UpdatedSession {
    UpdatedSession {
        row,
        replaces_listed_row: true,
    }
}

/// Session `a` read again after the user renamed it to `Renamed` in its
/// agent's title store: the transcript, and so its fingerprint, unchanged.
fn a_renamed_in_its_title_store(_: &Conversation, _: bool) -> Option<SessionRead> {
    let mut read = session("a", "a", 5, 1);
    read.custom_title = Some("Renamed".to_owned());
    Some(SessionRead::Listed(Box::new(read)))
}

/// Session `a` read again after the user renamed it to `Renamed` while the
/// agent wrote more messages.
fn a_renamed_with_new_messages(_: &Conversation, _: bool) -> Option<SessionRead> {
    let mut read = session("a", "written meanwhile", 20, 9);
    read.custom_title = Some("Renamed".to_owned());
    Some(SessionRead::Listed(Box::new(read)))
}

/// Session `a` read again, its preview naming the preview the list asked
/// for.
fn a_previewed_as_asked(_: &Conversation, show_last: bool) -> Option<SessionRead> {
    let mut read = session("a", "a", 5, 1);
    read.preview = if show_last { "last" } else { "first" }.to_owned();
    Some(SessionRead::Listed(Box::new(read)))
}

fn never_read(row: &Conversation, _: bool) -> Option<SessionRead> {
    panic!("{} was read again", row.session_id)
}

fn unreadable(_: &Conversation, _: bool) -> Option<SessionRead> {
    Some(SessionRead::Failed(ReadError::Permanent))
}

fn gone(_: &Conversation, _: bool) -> Option<SessionRead> {
    None
}

fn status_text(app: &App) -> Option<&str> {
    app.status_message().map(|(message, _)| message.as_str())
}

#[test]
fn new_session_is_listed_by_its_time() {
    let mut app = app_listing(vec![session("b", "b", 10, 1), session("a", "a", 5, 1)]);
    app.begin_refresh().unwrap();

    app.finish_refresh(Ok(SessionChanges {
        updated: vec![
            new_session(session("c", "c", 20, 1)),
            new_session(session("between", "x", 7, 1)),
        ],
        ..Default::default()
    }));

    assert_eq!(listed_names(&app), ["c", "b", "between", "a"]);
    assert_eq!(status_text(&app), Some("2 new"));
    assert!(!app.is_refreshing());
}

#[test]
fn changed_session_replaces_its_row() {
    let mut app = app_listing(vec![
        session("b", "b", 10, 1),
        session("a", "first words", 5, 1),
    ]);

    let applied = app.apply_session_changes(SessionChanges {
        updated: vec![changed_session(session("a", "second words", 20, 2))],
        ..Default::default()
    });

    assert_eq!(applied.changed, 1);
    assert_eq!(listed_names(&app), ["a", "b"]);
    assert_eq!(app.conversations().len(), 2);
    assert_eq!(app.conversations()[0].preview, "second words");
    assert_eq!(app.conversations()[0].fingerprint, Some(fingerprint(2)));
}

#[test]
fn session_gone_from_disk_is_removed() {
    let gone = session("a", "a", 5, 1);
    let mut app = app_listing(vec![session("b", "b", 10, 1), gone.clone()]);

    let applied = app.apply_session_changes(SessionChanges {
        removed: vec![gone.path],
        ..Default::default()
    });

    assert_eq!(applied.removed, 1);
    assert_eq!(listed_names(&app), ["b"]);
}

#[test]
fn selection_stays_on_its_session() {
    let mut app = app_listing(vec![session("b", "b", 10, 1), session("a", "a", 5, 1)]);
    app.handle_key(KeyCode::Down, KeyModifiers::NONE, 10);
    assert_eq!(selected_name(&app), Some("a"));

    app.apply_session_changes(SessionChanges {
        updated: vec![new_session(session("c", "c", 20, 1))],
        ..Default::default()
    });

    assert_eq!(selected_name(&app), Some("a"));
}

#[test]
fn selection_moves_to_first_row_when_its_session_is_removed() {
    let selected = session("b", "b", 10, 1);
    let mut app = app_listing(vec![session("c", "c", 20, 1), selected.clone()]);
    app.handle_key(KeyCode::Down, KeyModifiers::NONE, 10);
    assert_eq!(selected_name(&app), Some("b"));

    app.apply_session_changes(SessionChanges {
        removed: vec![selected.path],
        ..Default::default()
    });

    assert_eq!(selected_name(&app), Some("c"));
}

#[test]
fn refresh_reruns_query() {
    let mut app = app_listing(vec![
        session("b", "unrelated", 10, 1),
        session("a", "needle", 5, 1),
    ]);
    app.set_query_for_test("needle");
    app.update_filter_for_test();
    assert_eq!(listed_names(&app), ["a"]);

    app.apply_session_changes(SessionChanges {
        updated: vec![
            new_session(session("c", "another needle", 20, 1)),
            new_session(session("d", "nothing here", 30, 1)),
        ],
        ..Default::default()
    });

    assert_eq!(listed_names(&app), ["c", "a"]);
}

#[test]
fn search_finds_changed_session_by_its_new_text() {
    let mut app = app_listing(vec![
        session("b", "b", 10, 1),
        session("a", "original wording", 5, 1),
    ]);
    app.apply_session_changes(SessionChanges {
        updated: vec![changed_session(session("a", "revised wording", 5, 2))],
        ..Default::default()
    });

    app.set_query_for_test("revised");
    app.update_filter_for_test();
    assert_eq!(listed_names(&app), ["a"]);

    app.set_query_for_test("original");
    app.update_filter_for_test();
    assert!(listed_names(&app).is_empty());
}

#[test]
fn refresh_requested_while_one_runs_is_ignored() {
    let mut app = app_listing(vec![session("a", "a", 5, 1)]);

    assert!(app.begin_refresh().is_some());
    assert!(app.begin_refresh().is_none());

    app.finish_refresh(Ok(SessionChanges::default()));
    assert!(app.begin_refresh().is_some());
}

#[test]
fn refresh_requested_while_list_loads_is_ignored() {
    let mut app = App::new_loading_with_options(
        ToolDisplayMode::Hidden,
        false,
        KeyBindings::default(),
        ListScope::default(),
        TuiSearchOptions::default(),
        Arc::new(TestClock::default()),
    );

    assert!(app.begin_refresh().is_none());
    assert!(!app.is_refreshing());
}

#[test]
fn known_sessions_are_listed_rows_and_skipped_sessions() {
    let mut app = app_listing(vec![session("a", "a", 5, 7)]);
    let found = |size| FoundSession {
        source: Source::Claude,
        fingerprint: Some(fingerprint(size)),
        has_transient_read_error: false,
    };
    let skipped = SkippedSessions {
        unlisted: HashMap::from([(PathBuf::from("/sessions/empty.jsonl"), found(3))]),
        unreadable: HashMap::from([(PathBuf::from("/sessions/broken.jsonl"), found(4))]),
    };
    app.add_skipped_sessions(skipped.clone());

    let known = app.begin_refresh().unwrap();

    assert_eq!(
        known.listed,
        HashMap::from([(PathBuf::from("/sessions/a.jsonl"), found(7))])
    );
    assert_eq!(known.skipped, skipped);
}

#[test]
fn session_deleted_during_refresh_stays_deleted() {
    let doomed = session("a", "a", 5, 1);
    let mut app = app_listing(vec![session("b", "b", 10, 1), doomed.clone()]);
    app.set_session_reader_for_test(never_read);
    app.begin_refresh().unwrap();
    app.handle_key(KeyCode::Down, KeyModifiers::NONE, 10);
    app.remove_selected_from_list();

    app.finish_refresh(Ok(SessionChanges {
        updated: vec![changed_session(session("a", "grew", 15, 2))],
        removed: vec![doomed.path],
        ..Default::default()
    }));

    assert_eq!(listed_names(&app), ["b"]);
    assert_eq!(status_text(&app), None, "nothing the refresh found applied");
}

#[test]
fn rename_in_title_store_during_refresh_keeps_its_new_title() {
    let mut app = app_listing(vec![session("a", "a", 5, 1)]);
    app.set_session_reader_for_test(a_renamed_in_its_title_store);
    app.begin_refresh().unwrap();
    app.show_renamed_session(0, "Renamed");

    // The refresh read the title store before the rename.
    app.finish_refresh(Ok(SessionChanges {
        external_titles: vec![(
            Source::Claude,
            HashMap::from([("a".to_owned(), SessionTitle::Custom("Before".to_owned()))]),
        )],
        ..Default::default()
    }));

    assert_eq!(
        app.conversations()[0].custom_title.as_deref(),
        Some("Renamed")
    );
    assert_eq!(
        status_text(&app),
        Some("Session renamed"),
        "the refresh doesn't count the user's own rename"
    );
}

#[test]
fn rename_during_refresh_also_shows_messages_written_meanwhile() {
    let mut app = app_listing(vec![session("a", "a", 5, 1)]);
    app.set_session_reader_for_test(a_renamed_in_its_title_store);
    app.begin_refresh().unwrap();
    app.show_renamed_session(0, "Renamed");
    app.set_session_reader_for_test(a_renamed_with_new_messages);

    // The refresh read the new messages before the rename, so its row has
    // the old title.
    app.finish_refresh(Ok(SessionChanges {
        updated: vec![changed_session(session("a", "written meanwhile", 20, 9))],
        ..Default::default()
    }));

    let row = &app.conversations()[0];
    assert_eq!(row.custom_title.as_deref(), Some("Renamed"));
    assert_eq!(row.preview, "written meanwhile");
    assert_eq!(row.fingerprint, Some(fingerprint(9)));
    assert_eq!(status_text(&app), Some("1 changed"));
}

#[test]
fn renamed_session_keeps_lists_last_message_preview() {
    let mut app = app_listing(vec![session("a", "a", 5, 1)]);
    app.set_show_last(true);
    app.set_session_reader_for_test(a_previewed_as_asked);

    app.show_renamed_session(0, "Renamed");

    assert_eq!(app.conversations()[0].preview, "last");
}

/// An app listing session `a`, after a refresh that found its changed
/// transcript unreadable.
fn app_listing_a_as_unreadable() -> (App, SkippedSessions) {
    let mut app = app_listing(vec![session("a", "a", 5, 1)]);
    let skipped = SkippedSessions {
        unreadable: HashMap::from([(
            PathBuf::from("/sessions/a.jsonl"),
            FoundSession {
                source: Source::Claude,
                fingerprint: Some(fingerprint(2)),
                has_transient_read_error: false,
            },
        )]),
        ..Default::default()
    };
    app.begin_refresh().unwrap();
    app.finish_refresh(Ok(SessionChanges {
        skipped: skipped.clone(),
        ..Default::default()
    }));
    (app, skipped)
}

#[test]
fn refresh_hands_listed_unreadable_session_to_next_one() {
    let (mut app, skipped) = app_listing_a_as_unreadable();

    let known = app.begin_refresh().unwrap();

    assert_eq!(known.skipped, skipped);
}

#[test]
fn rename_reads_unreadable_session() {
    let (mut app, _) = app_listing_a_as_unreadable();
    app.set_show_last(true);
    app.set_session_reader_for_test(a_previewed_as_asked);

    app.show_renamed_session(0, "Renamed");

    assert_eq!(app.conversations()[0].preview, "last");
}

fn renaming(app: &mut App) {
    app.dialog_mode = DialogMode::Rename {
        input: "Renamed".to_owned(),
        cursor: 7,
    };
}

#[test]
fn rename_whose_read_fails_shows_saved_title() {
    let mut app = app_listing(vec![session("a", "a", 5, 1)]);
    app.set_session_reader_for_test(unreadable);
    renaming(&mut app);

    app.show_renamed_session(0, "Renamed");

    assert_eq!(app.dialog_mode, DialogMode::None);
    assert_eq!(
        app.conversations()[0].custom_title.as_deref(),
        Some("Renamed")
    );
    assert_eq!(status_text(&app), Some("Session renamed"));
}

#[test]
fn rename_whose_session_is_gone_reports_it_and_keeps_dialog() {
    let mut app = app_listing(vec![session("a", "a", 5, 1)]);
    app.set_session_reader_for_test(gone);
    renaming(&mut app);

    app.show_renamed_session(0, "Renamed");

    assert!(matches!(app.dialog_mode, DialogMode::Rename { .. }));
    assert_eq!(
        status_text(&app),
        Some("Failed to rename: conversation became empty")
    );
}

#[test]
fn session_read_again_when_refresh_applies_keeps_lists_last_message_preview() {
    let mut app = app_listing(vec![session("b", "b", 10, 1), session("a", "a", 5, 1)]);
    app.set_show_last(true);
    app.set_session_reader_for_test(a_previewed_as_asked);
    app.begin_refresh().unwrap();
    app.note_changed_during_refresh(Path::new("/sessions/a.jsonl"));

    app.finish_refresh(Ok(SessionChanges::default()));

    let a = app
        .conversations()
        .iter()
        .find(|row| row.session_id == "a")
        .unwrap();
    assert_eq!(a.preview, "last");
}

#[test]
fn sessions_changed_during_refresh_are_read_again_in_one_batch() {
    let mut app = app_listing(vec![session("b", "b", 10, 1), session("a", "a", 5, 1)]);
    let batches = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let seen = batches.clone();
    app.set_sessions_reader_for_test(Box::new(move |rows, _| {
        seen.borrow_mut().push(rows.len());
        rows.iter()
            .map(|_| Some(SessionRead::Failed(ReadError::Permanent)))
            .collect()
    }));
    app.begin_refresh().unwrap();
    app.note_changed_during_refresh(Path::new("/sessions/a.jsonl"));
    app.note_changed_during_refresh(Path::new("/sessions/b.jsonl"));

    app.finish_refresh(Ok(SessionChanges::default()));

    assert_eq!(*batches.borrow(), [2]);
}

#[test]
fn session_that_cannot_be_read_again_keeps_its_row() {
    let mut app = app_listing(vec![session("b", "b", 10, 1), session("a", "a", 5, 1)]);
    app.set_session_reader_for_test(unreadable);
    app.begin_refresh().unwrap();
    app.note_changed_during_refresh(Path::new("/sessions/a.jsonl"));

    app.finish_refresh(Ok(SessionChanges::default()));

    assert_eq!(listed_names(&app), ["b", "a"]);
    assert_eq!(status_text(&app), None, "nothing is counted as removed");
}

#[test]
fn session_its_agent_no_longer_finds_is_removed_when_read_again() {
    let mut app = app_listing(vec![session("b", "b", 10, 1), session("a", "a", 5, 1)]);
    app.set_session_reader_for_test(gone);
    app.begin_refresh().unwrap();
    app.note_changed_during_refresh(Path::new("/sessions/a.jsonl"));

    app.finish_refresh(Ok(SessionChanges::default()));

    assert_eq!(listed_names(&app), ["b"]);
    assert_eq!(status_text(&app), Some("1 removed"));
}

#[test]
fn failed_refresh_keeps_list_and_reports_why() {
    let mut app = app_listing(vec![session("b", "b", 10, 1), session("a", "a", 5, 1)]);
    app.begin_refresh().unwrap();

    app.finish_refresh(Err(AppError::ConfigError("no home directory".to_owned())));

    assert_eq!(listed_names(&app), ["b", "a"]);
    assert_eq!(
        status_text(&app),
        Some("Refresh failed: Configuration error: no home directory")
    );
    assert!(!app.is_refreshing());
}

#[test]
fn refresh_thread_that_stops_without_outcome_reports_it_once() {
    let mut app = app_listing(vec![session("a", "a", 5, 1)]);
    app.begin_refresh().unwrap();

    app.finish_refresh(Err(AppError::RefreshStopped));

    assert_eq!(
        status_text(&app),
        Some("Refresh failed: it stopped before finishing")
    );
}

#[test]
fn refresh_that_finishes_in_viewer_applies_back_in_list() {
    let mut app = app_listing(vec![session("a", "a", 5, 1)]);
    app.begin_refresh().unwrap();
    app.app_mode = AppMode::View(ViewState::initial(
        PathBuf::from("/sessions/a.jsonl"),
        Source::Claude,
        Some("a".to_owned()),
        Vec::new(),
        ToolDisplayMode::Hidden,
        false,
    ));

    app.finish_refresh(Ok(SessionChanges {
        updated: vec![new_session(session("c", "c", 20, 1))],
        ..Default::default()
    }));
    assert_eq!(app.conversations().len(), 1);
    assert!(app.is_refreshing());

    app.app_mode = AppMode::List;
    app.apply_finished_refresh();

    assert_eq!(listed_names(&app), ["c", "a"]);
    assert!(!app.is_refreshing());
}

#[test]
fn refresh_replaces_ignored_terms_and_keeps_launch_filters() {
    let mut app = app_listing(vec![session("a", "a", 5, 1)]);
    let since = FilterTerm::new("since", "2026-10-01 00:00");
    app.set_active_filters(vec![since.clone()]);
    app.add_active_filter(FilterTerm::new("Codex", "3 ignored: compressed sessions"));

    let refreshed = FilterTerm::new("Codex", "4 ignored: compressed sessions");
    app.apply_session_changes(SessionChanges {
        ignored: vec![refreshed.clone()],
        ..Default::default()
    });

    assert_eq!(
        app.active_filters().collect::<Vec<_>>(),
        [&since, &refreshed]
    );
}

#[test]
fn ctrl_l_counts_loaded_sessions_missing_earlier_turns_as_rows_change() {
    let mut deleted = session("d", "d", 6, 4);
    deleted.is_missing_earlier_turns = true;
    let mut app = app_listing(vec![deleted, session("a", "a", 5, 1)]);
    assert_eq!(
        app.active_filters().collect::<Vec<_>>(),
        [&FilterTerm::new(
            "Claude",
            "1 session missing earlier turns"
        )]
    );

    app.remove_selected_from_list();
    assert!(!app.has_active_filters(), "a list delete recounts");

    let mut missing = session("b", "b", 5, 2);
    missing.is_missing_earlier_turns = true;
    let mut also_missing = session("c", "c", 5, 3);
    also_missing.is_missing_earlier_turns = true;
    app.apply_session_changes(SessionChanges {
        updated: vec![new_session(missing), new_session(also_missing)],
        ..Default::default()
    });
    assert_eq!(
        app.active_filters().collect::<Vec<_>>(),
        [&FilterTerm::new(
            "Claude",
            "2 sessions missing earlier turns"
        )]
    );

    app.apply_session_changes(SessionChanges {
        removed: vec![PathBuf::from("/sessions/c.jsonl")],
        ..Default::default()
    });
    assert_eq!(
        app.active_filters().collect::<Vec<_>>(),
        [&FilterTerm::new(
            "Claude",
            "1 session missing earlier turns"
        )]
    );
}

#[test]
fn title_stored_beside_transcript_retitles_its_row() {
    let mut app = app_listing(vec![session("a", "a", 5, 1)]);

    let applied = app.apply_session_changes(SessionChanges {
        external_titles: vec![(
            Source::Claude,
            HashMap::from([("a".to_owned(), SessionTitle::Custom("Retitled".to_owned()))]),
        )],
        ..Default::default()
    });

    assert_eq!(applied.changed, 1);
    assert_eq!(
        app.conversations()[0].custom_title.as_deref(),
        Some("Retitled")
    );
}

#[test]
fn refresh_that_finds_nothing_changed_reports_nothing() {
    let listed = session("a", "a", 5, 1);
    let mut app = app_listing(vec![listed.clone()]);
    app.begin_refresh().unwrap();

    app.finish_refresh(Ok(SessionChanges {
        external_titles: vec![(
            Source::Claude,
            HashMap::from([("other".to_owned(), SessionTitle::Custom("x".to_owned()))]),
        )],
        ..Default::default()
    }));

    assert_eq!(status_text(&app), None);
    assert_eq!(listed_names(&app), ["a"]);
}

#[test]
fn summary_names_each_kind_of_change_it_counted() {
    let summary = |new, changed, removed| {
        AppliedChanges {
            new,
            changed,
            removed,
        }
        .summary()
    };

    assert_eq!(
        summary(3, 1, 1).as_deref(),
        Some("3 new · 1 changed · 1 removed")
    );
    assert_eq!(summary(0, 2, 0).as_deref(), Some("2 changed"));
    assert_eq!(summary(0, 0, 0), None);
}

#[test]
fn ctrl_r_in_list_asks_for_refresh() {
    let mut app = app_listing(vec![session("a", "a", 5, 1)]);

    let action = app.handle_key(KeyCode::Char('r'), KeyModifiers::CONTROL, 10);

    assert!(matches!(action, Some(Action::Refresh)));
    assert_eq!(app.query(), "", "the key does not type into the query");
}
