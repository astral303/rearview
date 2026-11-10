//! The drawing thread's session cache reads while another rearview refreshes
//! or rebuilds the cache.

use super::refresh_tests::refresh_found;
use super::*;
use crate::clock::TestClock;
use crate::history::provider::SessionRead;
use crate::history::provider::contract_tests::{load_under, registered_fixtures};
use crate::history::sqlite_cache::SqliteCache;
use crate::history::sqlite_cache::refresh::RefreshOwnership;
use crate::history::sqlite_cache::refresh::tests::{
    OtherProcessOwner, OwnerActivity, expire_last_commit,
};
use crate::history::workspace::tests::workspace_at_home;
use crate::history::{CacheFollowUp, SessionChanges};
use crate::history::{RefreshOwner, Source};
use crate::tui::export::ClipboardDestination;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

/// Longer than the drawing thread's busy timeout, far shorter than the 30
/// seconds the waiting reader spends behind a rebuild.
const WITHOUT_WAITING: Duration = Duration::from_secs(5);

/// A Claude session written under `home` and cached in `base`, as a warm
/// load lists it: its row reads its text from the database.
fn cached_session(home: &Path, base: &Path) -> Conversation {
    let fixture = registered_fixtures()[0];
    fixture.write_session(home, fixture.ids().session);
    SqliteCache::open(base).unwrap().mark_admitted().unwrap();
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

/// An app listing `rows`, reading the session cache under `base`.
fn app_reading(rows: Vec<Conversation>, base: &Path) -> App {
    let mut app = App::new(
        rows,
        ToolDisplayMode::Hidden,
        false,
        KeyBindings::default(),
        vec![],
        Arc::new(TestClock::default()),
    );
    app.set_cache_base_for_test(base);
    app.list_search_mode = ListSearchMode::Lexical;
    app
}

fn listed_unchanged(row: &Conversation, _: bool) -> Option<SessionRead> {
    Some(SessionRead::Listed(Box::new(row.clone())))
}

#[test]
fn a_conversation_copy_doesnt_wait_for_another_processs_rebuild() {
    let home = tempfile::tempdir().unwrap();
    let base = tempfile::tempdir().unwrap();
    let mut app = app_reading(vec![cached_session(home.path(), base.path())], base.path());
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
    let mut app = app_reading(vec![cached_session(home.path(), base.path())], base.path());
    app.set_session_reader_for_test(listed_unchanged);
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
    let mut app = app_reading(vec![cached_session(home.path(), base.path())], base.path());
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
    let mut app = app_reading(vec![row], base.path());

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
        InteractiveCache::default(),
        Arc::new(TestClock::default()),
    );
    app.set_cache_base_for_test(base.path());

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
    static IS_REFRESH_RUNNING: AtomicBool = AtomicBool::new(true);
    let home = tempfile::tempdir().unwrap();
    let fixture = registered_fixtures()[0];
    let path = fixture.write_session(home.path(), fixture.ids().session);
    let mut app = App::new_single_file(
        path.clone(),
        Source::Claude,
        ToolDisplayMode::Hidden,
        false,
        KeyBindings::default(),
        InteractiveCache::default(),
        Arc::new(TestClock::default()),
    );
    app.set_running_refresh_check_for_test(|| {
        IS_REFRESH_RUNNING
            .load(Ordering::Relaxed)
            .then_some(RefreshOwner::AnotherProcess)
    });
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

    IS_REFRESH_RUNNING.store(false, Ordering::Relaxed);
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
    fixture.write_session(home.path(), fixture.ids().session);
    let mut rows = load_under(fixture, home.path(), base.path());
    let held = RefreshOwnership::acquire(base.path(), || ()).unwrap();
    let mut app = app_reading(vec![rows.remove(0)], base.path());
    let reads = std::rc::Rc::new(std::cell::Cell::new(0));
    let counted = reads.clone();
    app.set_sessions_reader_for_test(Box::new(move |rows, _| {
        counted.set(counted.get() + 1);
        let follow_up =
            (counted.get() == 1).then_some(CacheFollowUp::AwaitRefresh(RefreshOwner::ThisProcess));
        rows.iter()
            .map(|row| crate::history::provider::InteractiveRead {
                value: Some(SessionRead::Listed(Box::new((*row).clone()))),
                follow_up,
            })
            .collect()
    }));
    let path = app.conversations()[0].path.clone();
    app.begin_refresh().unwrap();
    app.note_changed_during_refresh(&path);
    app.finish_refresh(refresh_found(SessionChanges::default()));
    assert!(app.is_awaiting_refresh());
    expire_last_commit(base.path());
    let other = OtherProcessOwner::hold(base.path());

    app.expire_awaited_refresh_check_for_test();
    app.check_awaited_refresh();
    assert!(
        app.is_waiting_for_another_rearview(),
        "the check sees the process that took the refresh over"
    );
    assert_eq!(reads.get(), 1);

    other.release();
    app.expire_awaited_refresh_check_for_test();
    app.check_awaited_refresh();
    assert_eq!(
        reads.get(),
        2,
        "the row was not read again once the other refresh completed"
    );
    assert!(!app.is_awaiting_refresh());
    drop(held);
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
    SqliteCache::open_waiting(base)
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

/// An app viewing `row`, a session `fixture` wrote under `home` and cached
/// in `base`, that reads listed sessions through `fixture`'s storage under
/// `home`: on the drawing thread as the TUI rereads a row, and on the
/// background refresh thread as it caches one.
fn app_viewing_written_session(
    row: Conversation,
    fixture: &'static dyn crate::history::provider::contract_tests::ProviderFixture,
    home: &Path,
    base: &Path,
) -> App {
    use crate::history::provider::contract_tests::{cache_under, reread_under};
    let mut app = app_reading(vec![row], base);
    let (home_path, base_path) = (home.to_path_buf(), base.to_path_buf());
    app.set_sessions_reader_for_test(Box::new(move |rows, _| {
        rows.iter()
            .map(|row| reread_under(fixture, &home_path, &base_path, &row.path))
            .collect()
    }));
    let (home_path, base_path) = (home.to_path_buf(), base.to_path_buf());
    app.set_session_cacher_for_test(std::sync::Arc::new(move |_, rows, _, _| {
        rows.iter()
            .map(|row| cache_under(fixture, &home_path, &base_path, &row.path))
            .collect()
    }));
    app.enter_view_mode(80);
    app
}

#[test]
fn ctrl_r_on_a_changed_session_reads_its_transcript_without_writing_the_cache() {
    let home = tempfile::tempdir().unwrap();
    let base = tempfile::tempdir().unwrap();
    let fixture = registered_fixtures()[0];
    let row = cached_session(home.path(), base.path());
    let cached_sizes = recorded_sizes(base.path());
    grow(&row.path);
    let mut app = app_viewing_written_session(row, fixture, home.path(), base.path());

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
    let mut app = app_viewing_written_session(row, fixture, home.path(), base.path());
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

    assert_eq!(app.cache.base(), None);
}

#[test]
#[should_panic(expected = "drawing thread")]
fn refresh_ownership_refuses_the_drawing_thread() {
    let base = tempfile::tempdir().unwrap();
    let _app = app_reading(Vec::new(), base.path());

    let _ = RefreshOwnership::acquire(base.path(), || ());
}

#[test]
#[should_panic(expected = "drawing thread")]
fn a_cache_writer_refuses_the_drawing_thread() {
    let base = tempfile::tempdir().unwrap();
    let ownership = RefreshOwnership::acquire(base.path(), || ()).unwrap();
    let _app = app_reading(Vec::new(), base.path());

    let _ = SqliteCache::open_for_refresh(ownership.token());
}

#[test]
#[should_panic(expected = "drawing thread")]
fn the_waiting_cache_reader_refuses_the_drawing_thread() {
    let base = tempfile::tempdir().unwrap();
    SqliteCache::open(base.path())
        .unwrap()
        .mark_admitted()
        .unwrap();
    let _app = app_reading(Vec::new(), base.path());

    let _ = SqliteCache::open_waiting(base.path());
}
