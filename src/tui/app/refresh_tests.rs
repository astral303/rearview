use super::claude_home::{ClaudeHome, admitted_cache, another_rearview, app_reading};
use super::refresh_state::AppliedChanges;
use super::*;
use crate::clock::TestClock;
use crate::error::AppError;
use crate::history::environment::test_support::test_environment;
use crate::history::fingerprint::CachedFingerprint;
use crate::history::provider::SessionTitle;
use crate::history::sqlite_cache::refresh::RefreshOwnership;
use crate::history::{
    AwaitedRefresh, FoundSession, NextStep, OtherRearview, ProcessIdentity, RefreshOutcome,
    SessionChanges, Source, UpdatedSession,
};
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

/// An app listing `rows`, newest first, reading no agent's storage.
fn app_listing(rows: Vec<Conversation>) -> App {
    app_reading(rows, test_environment(Vec::new(), None))
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
        next: NextStep::Settled,
    }
}

fn changed_session(row: Conversation) -> UpdatedSession {
    UpdatedSession {
        row,
        replaces_listed_row: true,
        next: NextStep::Settled,
    }
}

fn status_text(app: &App) -> Option<&str> {
    app.status_message().map(|(message, _)| message.as_str())
}

/// The outcome of a `Ctrl+R` refresh that found `changes`.
pub(super) fn refresh_found(changes: SessionChanges) -> crate::error::Result<RefreshOutcome> {
    Ok(RefreshOutcome::Changes(changes))
}

#[test]
fn new_session_is_listed_by_its_time() {
    let mut app = app_listing(vec![session("b", "b", 10, 1), session("a", "a", 5, 1)]);
    app.begin_refresh().unwrap();

    app.finish_refresh(refresh_found(SessionChanges {
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
    app.settle_search_for_test();
    assert_eq!(listed_names(&app), ["a"]);

    app.apply_session_changes(SessionChanges {
        updated: vec![
            new_session(session("c", "another needle", 20, 1)),
            new_session(session("d", "nothing here", 30, 1)),
        ],
        ..Default::default()
    });
    app.settle_search_for_test();

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
    app.settle_search_for_test();
    assert_eq!(listed_names(&app), ["a"]);

    app.set_query_for_test("original");
    app.update_filter_for_test();
    app.settle_search_for_test();
    assert!(listed_names(&app).is_empty());
}

#[test]
fn a_second_refresh_doesnt_begin_while_one_runs() {
    let mut app = app_listing(vec![session("a", "a", 5, 1)]);

    assert!(app.begin_refresh().is_some());
    assert!(app.begin_refresh().is_none());

    app.finish_refresh(refresh_found(SessionChanges::default()));
    assert!(app.begin_refresh().is_some());
}

/// Rule 5: a list `Ctrl+R` pressed while another job runs waits, one at
/// most, and begins once that job's outcome is applied.
#[test]
fn ctrl_r_pressed_twice_while_a_refresh_runs_requests_one_list_refresh() {
    let mut app = app_listing(vec![session("a", "a", 5, 1)]);
    app.begin_refresh().unwrap();

    app.request_list_refresh();
    app.request_list_refresh();

    assert!(app.begin_requested_refresh().is_none());
    app.finish_refresh(refresh_found(SessionChanges::default()));
    assert!(app.begin_requested_refresh().is_some());
    app.finish_refresh(refresh_found(SessionChanges::default()));
    assert!(
        app.begin_requested_refresh().is_none(),
        "the second press doesn't request a second refresh"
    );
}

#[test]
fn a_refresh_doesnt_begin_while_the_list_loads() {
    let mut app = App::new_loading_with_options(
        ToolDisplayMode::Hidden,
        false,
        KeyBindings::default(),
        ListScope::default(),
        TuiSearchOptions::default(),
        test_environment(Vec::new(), None),
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
    app.begin_refresh().unwrap();
    app.handle_key(KeyCode::Down, KeyModifiers::NONE, 10);
    app.remove_selected_from_list();

    app.finish_refresh(refresh_found(SessionChanges {
        updated: vec![changed_session(session("a", "grew", 15, 2))],
        removed: vec![doomed.path],
        ..Default::default()
    }));

    assert_eq!(listed_names(&app), ["b"]);
    assert_eq!(status_text(&app), None, "nothing the refresh found applied");
}

#[test]
fn renamed_session_keeps_lists_last_message_preview() {
    let home = ClaudeHome::new();
    home.write("a", 5, &["first", "last"]);
    let mut app = home.app();
    app.set_show_last(true);

    app.show_renamed_session(0, "Renamed");
    serve_session_refresh(&mut app);

    assert_shows_the_last_message_preview(&app.conversations()[0]);
}

/// `row`, read from a session of two messages, previews them last message
/// first.
fn assert_shows_the_last_message_preview(row: &Conversation) {
    assert_ne!(row.preview_last, row.preview_first);
    assert_eq!(row.preview, row.preview_last);
}

/// Apply a refresh to `app` that found the changed transcript of its first
/// row unreadable; returns the sessions it skipped.
fn find_first_row_unreadable(app: &mut App) -> SkippedSessions {
    let skipped = SkippedSessions {
        unreadable: HashMap::from([(
            app.conversations()[0].path.clone(),
            FoundSession {
                source: Source::Claude,
                fingerprint: Some(fingerprint(2)),
                has_transient_read_error: false,
            },
        )]),
        ..Default::default()
    };
    app.begin_refresh().unwrap();
    app.finish_refresh(refresh_found(SessionChanges {
        skipped: skipped.clone(),
        ..Default::default()
    }));
    skipped
}

#[test]
fn refresh_hands_listed_unreadable_session_to_next_one() {
    let mut app = app_listing(vec![session("a", "a", 5, 1)]);
    let skipped = find_first_row_unreadable(&mut app);

    let known = app.begin_refresh().unwrap();

    assert_eq!(known.skipped, skipped);
}

#[test]
fn rename_reads_unreadable_session() {
    let home = ClaudeHome::new();
    home.write("a", 5, &["first", "last"]);
    let mut app = home.app();
    find_first_row_unreadable(&mut app);
    app.set_show_last(true);

    app.show_renamed_session(0, "Renamed");
    serve_session_refresh(&mut app);

    assert_shows_the_last_message_preview(&app.conversations()[0]);
}

fn renaming(app: &mut App) {
    app.dialog_mode = DialogMode::Rename {
        input: "Renamed".to_owned(),
        cursor: 7,
    };
}

#[test]
fn rename_whose_read_fails_shows_saved_title() {
    let home = ClaudeHome::new();
    home.write("a", 5, &["a"]);
    let mut app = home.app();
    home.refuse_project();
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
#[ignore = "fixed in fix step 2"]
fn rename_whose_session_is_gone_reports_it_and_keeps_dialog() {
    let home = ClaudeHome::new();
    let path = home.write("a", 5, &["a"]);
    let mut app = home.app();
    std::fs::remove_file(path).unwrap();
    renaming(&mut app);

    app.show_renamed_session(0, "Renamed");

    assert!(matches!(app.dialog_mode, DialogMode::Rename { .. }));
    assert_eq!(
        status_text(&app),
        Some("Failed to rename: conversation became empty")
    );
}

/// Run the session refresh `app` begins as the background worker runs it,
/// on a thread of its own, and hand `app` its outcome.
pub(super) fn serve_session_refresh(app: &mut App) {
    let refresh = app
        .begin_session_refresh()
        .expect("a session refresh begins");
    let environment = app.environment.clone();
    let outcome = std::thread::spawn(move || {
        let operation = crate::history::OperationScope::new(
            environment.cache_base(),
            crate::history::Patience::UntilAcquired,
            Arc::default(),
        );
        let outcome = refresh.refresh(&operation);
        operation.finish().and(outcome)
    })
    .join()
    .unwrap();
    app.finish_refresh(outcome);
}

#[test]
fn a_session_refresh_discovers_its_sessions_once() {
    let home = ClaudeHome::new();
    let b = home.write("b", 10, &["b"]);
    let a = home.write("a", 5, &["a"]);
    let mut app = home.app();
    app.act_on_next_step(&a, NextStep::CacheInBackground);
    app.act_on_next_step(&b, NextStep::CacheInBackground);
    let listings_before = home.times_project_listed();

    serve_session_refresh(&mut app);

    assert_eq!(
        home.times_project_listed() - listings_before,
        1,
        "the sessions share one discovery"
    );
}

/// The listed row of the session `name`.
fn row_named<'a>(app: &'a App, name: &str) -> &'a Conversation {
    app.conversations()
        .iter()
        .find(|row| row.session_id == name)
        .unwrap_or_else(|| panic!("{name} is listed"))
}

#[test]
fn a_row_waiting_for_another_refresh_is_refreshed_once_it_completes() {
    let home = ClaudeHome::new();
    let cache = admitted_cache();
    let clock: Arc<dyn crate::clock::Clock> = Arc::new(TestClock::default());
    home.write("b", 10, &["b"]);
    let a = home.write("a", 5, &["a"]);
    let rows = home.rows(&home.environment());
    let other_refresh =
        RefreshOwnership::acquire_for_test(&another_rearview(cache.path(), clock.clone())).unwrap();
    let environment = home.environment_with_cache(cache.path(), ProcessIdentity::default(), clock);
    let mut app = app_reading(rows, environment);
    app.await_refresh(
        [a.clone()],
        AwaitedRefresh::AnotherRearview(OtherRearview::Refreshing),
    );
    home.append(&a, 6, "written during the other refresh");

    app.expire_awaited_refresh_check_for_test();
    app.check_awaited_refresh();
    assert!(
        app.is_waiting_for_another_rearview(),
        "the row waits while the other refresh runs"
    );
    assert!(!app.is_session_refresh_requested());

    drop(other_refresh);
    app.expire_awaited_refresh_check_for_test();
    app.check_awaited_refresh();
    assert!(!app.is_waiting_for_another_rearview());
    serve_session_refresh(&mut app);

    assert_eq!(row_named(&app, "a").message_count, 2);
    assert_eq!(listed_names(&app), ["b", "a"]);
}

#[test]
fn a_session_refresh_removes_rows_now_gone_or_empty_and_keeps_failed_reads() {
    let home = ClaudeHome::new();
    let cache = admitted_cache();
    let clock: Arc<dyn crate::clock::Clock> = Arc::new(TestClock::default());
    let gone = home.write("gone", 40, &["gone"]);
    let empty = home.write("empty", 30, &["empty"]);
    let failed = home.write_in("-tmp-unreadable", "failed", 20, &["failed"]);
    let listed = home.write("listed", 10, &["listed"]);
    let rows = home.rows(&home.environment());
    let environment = home.environment_with_cache(cache.path(), ProcessIdentity::default(), clock);
    let mut app = app_reading(rows, environment);
    for path in [&gone, &empty, &failed, &listed] {
        app.act_on_next_step(path, NextStep::CacheInBackground);
    }

    std::fs::remove_file(&gone).unwrap();
    std::fs::write(&empty, "").unwrap();
    home.refuse(failed.parent().unwrap());
    serve_session_refresh(&mut app);

    assert_eq!(listed_names(&app), ["failed", "listed"]);
}

#[test]
fn only_sessions_opened_out_of_date_wait_for_the_refresh() {
    let home = ClaudeHome::new();
    let cache = admitted_cache();
    let clock: Arc<dyn crate::clock::Clock> = Arc::new(TestClock::default());
    let b = home.write("b", 10, &["b"]);
    let a = home.write("a", 5, &["a"]);
    let environment =
        home.environment_with_cache(cache.path(), ProcessIdentity::default(), clock.clone());
    let rows = home.rows(&environment);
    home.append(&a, 6, "written after the cache");
    let other_refresh =
        RefreshOwnership::acquire_for_test(&another_rearview(cache.path(), clock)).unwrap();
    let mut app = app_reading(rows, environment);

    for _ in 0..2 {
        app.enter_view_mode(80);
        app.exit_view_mode();
        app.handle_key(KeyCode::Down, KeyModifiers::NONE, 10);
    }

    assert!(app.store.is_refresh_pending(&a));
    assert!(
        !app.store.is_refresh_pending(&b),
        "the cache served b, so it doesn't wait for the refresh"
    );
    drop(other_refresh);
}

#[test]
fn an_open_requested_before_a_completion_check_opens_the_requested_session() {
    let home = ClaudeHome::new();
    home.write("newer", 20, &["newer"]);
    let requested = home.write("older", 10, &["older"]);
    let mut app = home.app();
    home.append(&requested, 30, "written last");
    app.await_refresh(
        [requested.clone()],
        AwaitedRefresh::AnotherRearview(OtherRearview::Refreshing),
    );
    app.handle_key(KeyCode::Down, KeyModifiers::NONE, 10);
    assert!(app.request_open());

    app.expire_awaited_refresh_check_for_test();
    app.check_awaited_refresh();
    assert!(app.complete_pending_open(80));

    let AppMode::View(state) = app.app_mode() else {
        panic!("a session opens: {:?}", status_text(&app));
    };
    assert_eq!(state.conversation_path, requested);
}

#[test]
fn a_session_refresh_waits_while_a_delete_confirmation_is_open() {
    let home = ClaudeHome::new();
    home.write("b", 10, &["b"]);
    let a = home.write("a", 5, &["a"]);
    let mut app = home.app();
    std::fs::remove_file(&a).unwrap();
    app.act_on_next_step(&a, NextStep::CacheInBackground);
    app.handle_key(KeyCode::Down, KeyModifiers::NONE, 10);
    app.dialog_mode = DialogMode::ConfirmDelete;

    assert!(app.begin_session_refresh().is_none());
    assert_eq!(listed_names(&app), ["b", "a"]);
    assert_eq!(
        selected_name(&app),
        Some("a"),
        "the confirmation applies to the session it asked about"
    );

    app.dialog_mode = DialogMode::None;
    serve_session_refresh(&mut app);

    assert_eq!(listed_names(&app), ["b"]);
}

#[test]
fn rows_waiting_for_this_processs_own_refresh_dont_report_another_rearview() {
    let home = ClaudeHome::new();
    let cache = tempfile::tempdir().unwrap();
    let a = home.write("a", 5, &["a"]);
    let rows = home.rows(&home.environment());
    let environment = home.environment_with_cache(
        cache.path(),
        ProcessIdentity::default(),
        Arc::new(TestClock::default()),
    );
    let own_refresh = RefreshOwnership::acquire_for_test(environment.cache_base()).unwrap();
    let mut app = app_reading(rows, environment);
    app.await_refresh([a], AwaitedRefresh::ThisProcess);

    assert!(app.is_awaiting_refresh());
    assert!(!app.is_waiting_for_another_rearview());
    app.expire_awaited_refresh_check_for_test();
    app.check_awaited_refresh();
    assert!(
        app.is_awaiting_refresh(),
        "the row waits while this process's refresh runs"
    );
    drop(own_refresh);
}

#[test]
fn session_that_cannot_be_read_again_keeps_its_row() {
    let home = ClaudeHome::new();
    home.write("b", 10, &["b"]);
    let a = home.write("a", 5, &["a"]);
    let mut app = home.app();
    home.refuse_project();
    app.act_on_next_step(&a, NextStep::CacheInBackground);

    serve_session_refresh(&mut app);

    assert_eq!(listed_names(&app), ["b", "a"]);
    assert_eq!(status_text(&app), None, "nothing is counted as removed");
}

#[test]
fn session_its_agent_no_longer_finds_is_removed_when_read_again() {
    let home = ClaudeHome::new();
    home.write("b", 10, &["b"]);
    let a = home.write("a", 5, &["a"]);
    let mut app = home.app();
    std::fs::remove_file(&a).unwrap();
    app.act_on_next_step(&a, NextStep::CacheInBackground);

    serve_session_refresh(&mut app);

    assert_eq!(listed_names(&app), ["b"]);
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

    app.finish_refresh(refresh_found(SessionChanges {
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

    app.finish_refresh(refresh_found(SessionChanges {
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
