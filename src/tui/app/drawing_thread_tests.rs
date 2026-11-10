//! The drawing thread's session cache reads while another rearview refreshes
//! or rebuilds the cache.

use super::claude_home::{ClaudeHome, another_rearview, app_reading};
use super::refresh_tests::refresh_found;
use super::*;
use crate::clock::TestClock;
use crate::history::ProcessIdentity;
use crate::history::environment::test_support::real_time_environment;
use crate::history::provider::contract_tests::{
    kept_storage_under, load_under, registered_fixtures,
};
use crate::history::provider::walk::SystemLister;
use crate::history::sqlite_cache::SqliteCache;
use crate::history::sqlite_cache::connection::test_hooks;
use crate::history::sqlite_cache::refresh::RefreshOwnership;
use crate::history::sqlite_cache::refresh::tests::{
    OtherProcessOwner, OwnerActivity, exclusive_lock, expire_last_commit, rearview,
    writer_lock_held,
};
use crate::history::workspace::tests::workspace_at_home;
use crate::history::{Environment, RefreshOwner, SessionChanges};
use crate::tui::export::ClipboardDestination;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

/// Longer than the drawing thread's busy timeout, far shorter than the 30
/// seconds the waiting reader spends behind a rebuild.
const WITHOUT_WAITING: Duration = Duration::from_secs(5);

/// A session written under `home` and cached in `base`, as a warm load
/// lists it: its row reads its text from the database.
fn cached_session(home: &Path, base: &Path) -> Conversation {
    let fixture = registered_fixtures()[0];
    fixture.write_session(home, fixture.ids().session);
    SqliteCache::open(&rearview(base))
        .unwrap()
        .mark_admitted()
        .unwrap();
    load_under(fixture, home, base);
    let mut rows = load_under(fixture, home, base);
    assert_eq!(rows.len(), 1);
    let row = rows.remove(0);
    assert!(
        row.cached_session.is_some(),
        "the warm load reads the session from the cache"
    );
    row
}

/// Mark every session in `base`'s database as still being written, as a
/// refresh leaves a session between its catalogue row and its status.
fn mark_every_session_writing(base: &Path) {
    rusqlite::Connection::open(base.join(crate::history::sqlite_cache::DATABASE_FILE_NAME))
        .unwrap()
        .execute("UPDATE sessions SET status = 'writing'", [])
        .unwrap();
}

/// The first registered agent's storage pinned to `home`, with the session
/// cache under `base`, on the system clock: these tests run beside another
/// process that commits in real time.
fn reading_home(home: &Path, base: &Path) -> Environment {
    real_time_environment(
        vec![kept_storage_under(registered_fixtures()[0], home, None)],
        Some(base),
    )
}

#[test]
fn a_conversation_copy_doesnt_wait_for_another_processs_rebuild() {
    let home = tempfile::tempdir().unwrap();
    let base = tempfile::tempdir().unwrap();
    let mut app = app_reading(
        vec![cached_session(home.path(), base.path())],
        reading_home(home.path(), base.path()),
    );
    app.set_clipboard_writer_for_test(|_| Ok(ClipboardDestination::System));
    app.enter_view_mode(80);
    let rebuild = OtherProcessOwner::start(base.path(), OwnerActivity::InsideRebuild);

    let started = Instant::now();
    app.perform_export(1, true);

    assert!(
        started.elapsed() < WITHOUT_WAITING,
        "the copy doesn't wait for the rebuild"
    );
    assert_eq!(
        app.status_message().map(|(message, _)| message.as_str()),
        Some("Conversation copied to clipboard")
    );
    drop(rebuild);
}

#[test]
fn a_rename_with_an_active_query_doesnt_wait_for_another_processs_rebuild() {
    let home = tempfile::tempdir().unwrap();
    let base = tempfile::tempdir().unwrap();
    let mut app = app_reading(
        vec![cached_session(home.path(), base.path())],
        reading_home(home.path(), base.path()),
    );
    app.set_query_for_test("hello");
    let rebuild = OtherProcessOwner::start(base.path(), OwnerActivity::InsideRebuild);

    let started = Instant::now();
    app.show_renamed_session(0, "Renamed");

    assert!(
        started.elapsed() < WITHOUT_WAITING,
        "the rename doesn't wait for the rebuild"
    );
    drop(rebuild);
}

#[test]
fn a_workspace_toggle_with_an_active_query_doesnt_wait_for_another_processs_rebuild() {
    let home = tempfile::tempdir().unwrap();
    let base = tempfile::tempdir().unwrap();
    let mut app = app_reading(
        vec![cached_session(home.path(), base.path())],
        reading_home(home.path(), base.path()),
    );
    app.workspace = Some(workspace_at_home(home.path()));
    app.set_query_for_test("hello");
    let rebuild = OtherProcessOwner::start(base.path(), OwnerActivity::InsideRebuild);

    let started = Instant::now();
    app.toggle_workspace_filter();

    assert!(
        started.elapsed() < WITHOUT_WAITING,
        "the toggle doesn't wait for the rebuild"
    );
    drop(rebuild);
}

#[test]
fn opening_a_session_another_process_is_writing_waits_for_that_refresh() {
    let home = tempfile::tempdir().unwrap();
    let base = tempfile::tempdir().unwrap();
    let row = cached_session(home.path(), base.path());
    mark_every_session_writing(base.path());
    let other = OtherProcessOwner::hold(base.path());
    let mut app = app_reading(vec![row], reading_home(home.path(), base.path()));

    let started = Instant::now();
    app.enter_view_mode(80);

    assert!(started.elapsed() < WITHOUT_WAITING);
    assert!(matches!(app.app_mode(), AppMode::View(_)));
    assert!(
        app.is_waiting_for_another_rearview(),
        "the open session's row is queued for the completion check"
    );
    drop(other);
}

#[test]
fn a_file_opened_directly_while_another_process_writes_it_waits_for_that_refresh() {
    let home = tempfile::tempdir().unwrap();
    let base = tempfile::tempdir().unwrap();
    let row = cached_session(home.path(), base.path());
    mark_every_session_writing(base.path());
    let other = OtherProcessOwner::hold(base.path());
    let mut app = App::new_single_file(
        row.path.clone(),
        row.source,
        ToolDisplayMode::Hidden,
        false,
        KeyBindings::default(),
        reading_home(home.path(), base.path()),
    );

    let started = Instant::now();
    app.re_render_view(20);

    assert!(started.elapsed() < WITHOUT_WAITING);
    assert!(
        app.is_waiting_for_another_rearview(),
        "the file's row was not queued for the completion check"
    );
    drop(other);
}

#[test]
fn a_file_opened_directly_rereads_its_row_once_the_awaited_refresh_completes() {
    let home = tempfile::tempdir().unwrap();
    let base = tempfile::tempdir().unwrap();
    let fixture = registered_fixtures()[0];
    let path = fixture.write_session(home.path(), fixture.ids().session);
    let clock: Arc<dyn Clock> = Arc::new(TestClock::default());
    let other_refresh =
        RefreshOwnership::acquire(&another_rearview(base.path(), clock.clone()), || ()).unwrap();
    let mut app = App::new_single_file(
        path.clone(),
        fixture.provider().source(),
        ToolDisplayMode::Hidden,
        false,
        KeyBindings::default(),
        Environment::new(
            Vec::new(),
            Some(base.path().to_path_buf()),
            ProcessIdentity::default(),
            clock,
            Arc::new(SystemLister),
        ),
    );
    app.await_refresh([path], RefreshOwner::AnotherProcess);
    app.expire_awaited_refresh_check_for_test();

    app.check_awaited_refresh();
    assert!(
        app.idle_wait() > Duration::ZERO,
        "the loop waits for input after running the due check"
    );
    assert!(
        app.is_awaiting_refresh(),
        "the row waits while the other refresh runs"
    );

    drop(other_refresh);
    app.expire_awaited_refresh_check_for_test();
    app.check_awaited_refresh();

    assert!(!app.is_awaiting_refresh());
    assert_eq!(
        app.conversations().len(),
        1,
        "the file's row was lost when it was read again"
    );
}

#[test]
fn rows_awaiting_this_processs_refresh_wait_for_the_process_that_took_it_over() {
    let base = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let fixture = registered_fixtures()[0];
    let path = fixture.write_session(home.path(), fixture.ids().session);
    let mut rows = load_under(fixture, home.path(), base.path());
    let environment = reading_home(home.path(), base.path());
    let held = RefreshOwnership::acquire(environment.cache_base().unwrap(), || ()).unwrap();
    let mut app = app_reading(vec![rows.remove(0)], environment);
    app.begin_refresh().unwrap();
    app.note_changed_during_refresh(&path);
    app.finish_refresh(refresh_found(SessionChanges::default()));
    assert!(app.is_awaiting_refresh());
    assert!(!app.is_waiting_for_another_rearview());
    expire_last_commit(base.path());
    let other = OtherProcessOwner::hold(base.path());
    let size_before_the_takeover = listed_size(&app);
    let size_now = grow(&path);

    app.expire_awaited_refresh_check_for_test();
    app.check_awaited_refresh();
    assert!(
        app.is_waiting_for_another_rearview(),
        "the check sees the process that took the refresh over"
    );
    assert_eq!(listed_size(&app), size_before_the_takeover);

    other.release();
    app.expire_awaited_refresh_check_for_test();
    app.check_awaited_refresh();
    assert_eq!(
        listed_size(&app),
        Some(size_now),
        "the row was not read again once the other refresh completed"
    );
    assert!(!app.is_awaiting_refresh());
    drop(held);
}

/// The transcript size the first listed row was read at.
fn listed_size(app: &App) -> Option<u64> {
    app.conversations()[0]
        .fingerprint
        .map(|fingerprint| fingerprint.file_size)
}

/// The transcript sizes `base`'s database records, current or not.
fn recorded_sizes(base: &Path) -> Vec<i64> {
    rusqlite::Connection::open(base.join(crate::history::sqlite_cache::DATABASE_FILE_NAME))
        .unwrap()
        .prepare("SELECT file_size FROM sessions ORDER BY file_size")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

/// The transcript sizes `base`'s database holds as current for `fixture`'s
/// sessions under `home`.
fn current_sizes(
    fixture: &dyn crate::history::provider::contract_tests::ProviderFixture,
    home: &Path,
    base: &Path,
) -> Vec<u64> {
    SqliteCache::open_waiting(&rearview(base))
        .unwrap()
        .unwrap()
        .current_fingerprints(fixture.storage().cache(), &fixture.root_under(home).path)
        .unwrap()
        .into_values()
        .map(|fingerprint| fingerprint.file_size)
        .collect()
}

/// Add one line to the transcript at `path`; returns its new size.
fn grow(path: &Path) -> u64 {
    let mut transcript = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    std::io::Write::write_all(&mut transcript, b"\n").unwrap();
    drop(transcript);
    std::fs::metadata(path).unwrap().len()
}

/// An app viewing `row`, a session written under `home` and cached in
/// `base`.
fn app_viewing_written_session(row: Conversation, home: &Path, base: &Path) -> App {
    let mut app = app_reading(vec![row], reading_home(home, base));
    app.enter_view_mode(80);
    app
}

#[test]
fn ctrl_r_on_a_changed_session_reads_its_transcript_without_writing_the_cache() {
    let home = tempfile::tempdir().unwrap();
    let base = tempfile::tempdir().unwrap();
    let row = cached_session(home.path(), base.path());
    let cached_sizes = recorded_sizes(base.path());
    grow(&row.path);
    let mut app = app_viewing_written_session(row, home.path(), base.path());

    let started = Instant::now();
    app.refresh_open_session(20);

    assert!(started.elapsed() < WITHOUT_WAITING);
    assert!(
        matches!(app.app_mode(), AppMode::View(_)),
        "the refreshed session stays in the viewer: {:?}",
        app.status_message()
    );
    assert!(
        app.is_caching_queued(),
        "the session was not handed to the background refresh thread"
    );
    assert!(!app.is_waiting_for_another_rearview());
    drop(app);
    assert_eq!(
        recorded_sizes(base.path()),
        cached_sizes,
        "the drawing thread doesn't write the changed session to the database"
    );
}

#[test]
fn the_background_refresh_thread_caches_a_session_ctrl_r_read_and_updates_its_row() {
    let home = tempfile::tempdir().unwrap();
    let base = tempfile::tempdir().unwrap();
    let fixture = registered_fixtures()[0];
    let row = cached_session(home.path(), base.path());
    let path = row.path.clone();
    grow(&path);
    let mut app = app_viewing_written_session(row, home.path(), base.path());
    app.refresh_open_session(20);
    app.exit_view_mode();
    let size_now = grow(&path);

    let uncached = app
        .begin_caching_refresh()
        .expect("the session read outside the database is handed on");
    let outcome = std::thread::spawn(move || uncached.cache()).join().unwrap();
    app.finish_refresh(Ok(outcome));

    assert_eq!(
        app.conversations()[0]
            .fingerprint
            .map(|fingerprint| fingerprint.file_size),
        Some(size_now),
        "the row does not show the background refresh thread's read"
    );
    assert!(!app.is_refreshing());
    drop(app);
    assert_eq!(
        current_sizes(fixture, home.path(), base.path()),
        [size_now],
        "the background refresh thread did not cache the session"
    );
}

/// An app listing `home`'s sessions, read without a session cache, that
/// reads the session cache under `base` as this process reaches it, on
/// `clock`.
fn app_listing_home_over_cache(home: &ClaudeHome, base: &Path, clock: Arc<dyn Clock>) -> App {
    app_reading(
        home.rows(&home.environment()),
        home.environment_with_cache(base, ProcessIdentity::default(), clock),
    )
}

/// Eighth-pass findings (Claude F7, Codex F1): the drawing thread's check
/// for a refresh's completion opened a writable connection and took
/// SQLite's writer lock.
#[test]
#[ignore = "fixed in step 4"]
fn a_completion_check_opens_no_writable_connection() {
    let home = ClaudeHome::new();
    let base = tempfile::tempdir().unwrap();
    let a = home.write("a", 5, &["a"]);
    let clock: Arc<dyn Clock> = Arc::new(TestClock::default());
    let other_refresh =
        RefreshOwnership::acquire(&another_rearview(base.path(), clock.clone()), || ()).unwrap();
    let mut app = app_listing_home_over_cache(&home, base.path(), clock);
    app.await_refresh([a], RefreshOwner::AnotherProcess);
    let drawing_thread = std::thread::current().id();
    let writable_connections = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&writable_connections);
    let _hooks = test_hooks::on_every_connection(base.path(), move |connection| {
        if std::thread::current().id() == drawing_thread
            && !connection.is_readonly(rusqlite::MAIN_DB).unwrap()
        {
            counted.fetch_add(1, Ordering::SeqCst);
        }
    });

    app.expire_awaited_refresh_check_for_test();
    app.check_awaited_refresh();

    assert!(app.is_waiting_for_another_rearview());
    assert_eq!(writable_connections.load(Ordering::SeqCst), 0);
    drop(other_refresh);
}

/// Eighth-pass finding (Claude F7): a completion check beside another
/// connection's write transaction, while no refresh owned the cache,
/// reported another rearview's refresh.
#[test]
#[ignore = "fixed in step 4"]
fn a_completion_check_beside_a_write_without_an_owner_doesnt_report_another_rearview() {
    let home = ClaudeHome::new();
    let base = tempfile::tempdir().unwrap();
    let a = home.write("a", 5, &["a"]);
    let clock: Arc<dyn Clock> = Arc::new(TestClock::default());
    drop(RefreshOwnership::acquire(&another_rearview(base.path(), clock.clone()), || ()).unwrap());
    let write_without_an_owner = writer_lock_held(base.path());
    let mut app = app_listing_home_over_cache(&home, base.path(), clock);
    app.await_refresh([a], RefreshOwner::AnotherProcess);

    app.expire_awaited_refresh_check_for_test();
    app.check_awaited_refresh();

    assert!(!app.is_waiting_for_another_rearview());
    drop(write_without_an_owner);
}

/// Eighth-pass finding (Codex F3): after another rearview took over this
/// process's expired refresh and began a rebuild, its unreadable owner read
/// as this process's refresh, hiding the waiting label.
#[test]
#[ignore = "fixed in step 4"]
fn another_rearviews_rebuild_after_a_takeover_doesnt_read_as_this_processs_refresh() {
    let home = ClaudeHome::new();
    let base = tempfile::tempdir().unwrap();
    let a = home.write("a", 5, &["a"]);
    let clock: Arc<dyn Clock> = Arc::new(TestClock::default());
    let environment =
        home.environment_with_cache(base.path(), ProcessIdentity::default(), clock.clone());
    let paused_refresh =
        RefreshOwnership::acquire(environment.cache_base().unwrap(), || ()).unwrap();
    expire_last_commit(base.path());
    let takeover = RefreshOwnership::acquire(&another_rearview(base.path(), clock), || ()).unwrap();
    let rebuild = exclusive_lock(base.path());
    let mut app = app_reading(home.rows(&home.environment()), environment);
    app.await_refresh([a], RefreshOwner::ThisProcess);

    app.expire_awaited_refresh_check_for_test();
    app.check_awaited_refresh();

    assert!(app.is_waiting_for_another_rearview());
    drop(rebuild);
    drop(takeover);
    drop(paused_refresh);
}

#[test]
fn a_test_app_reads_no_session_cache() {
    let app = App::new(
        Vec::new(),
        ToolDisplayMode::Hidden,
        false,
        KeyBindings::default(),
        vec![],
        Arc::new(TestClock::default()),
    );

    assert!(app.environment.cache_base().is_none());
}

/// An app listing nothing, reading the session cache under `base`.
fn app_reading_cache(base: &Path) -> App {
    app_reading(Vec::new(), real_time_environment(Vec::new(), Some(base)))
}

#[test]
#[should_panic(expected = "drawing thread")]
fn refresh_ownership_refuses_the_drawing_thread() {
    let base = tempfile::tempdir().unwrap();
    let _app = app_reading_cache(base.path());

    let _ = RefreshOwnership::acquire(&rearview(base.path()), || ());
}

#[test]
#[should_panic(expected = "drawing thread")]
fn a_cache_writer_refuses_the_drawing_thread() {
    let base = tempfile::tempdir().unwrap();
    let ownership = RefreshOwnership::acquire(&rearview(base.path()), || ()).unwrap();
    let _app = app_reading_cache(base.path());

    let _ = SqliteCache::open_for_refresh(ownership.token());
}

#[test]
#[should_panic(expected = "drawing thread")]
fn the_waiting_cache_reader_refuses_the_drawing_thread() {
    let base = tempfile::tempdir().unwrap();
    SqliteCache::open(&rearview(base.path()))
        .unwrap()
        .mark_admitted()
        .unwrap();
    let _app = app_reading_cache(base.path());

    let _ = SqliteCache::open_waiting(&rearview(base.path()));
}
