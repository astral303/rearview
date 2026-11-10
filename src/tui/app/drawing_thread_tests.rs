//! The drawing thread's session cache reads while another rearview refreshes
//! or rebuilds the cache.

use super::claude_home::{ClaudeHome, TemplateCache, another_rearview, app_reading};
use super::*;
use crate::clock::TestClock;
use crate::history::ProcessIdentity;
use crate::history::environment::test_support::{environment_on_clock, real_time_environment};
use crate::history::provider::contract_tests::{
    ProviderFixture, kept_storage_under, load_under, registered_fixtures,
};
use crate::history::provider::reread_sessions;
use crate::history::sqlite_cache::connection::test_hooks;
use crate::history::sqlite_cache::refresh::RefreshOwnership;
use crate::history::sqlite_cache::refresh::tests::{
    AnotherRearviewsOwner, OwnerActivity, exclusive_lock, expire_last_commit, rearview,
    writer_lock_held,
};
use crate::history::sqlite_cache::{
    CachedCopy, CachedRead, INTERACTIVE_BUSY_TIMEOUT, InteractiveCache, SqliteCache,
};
use crate::history::workspace::tests::workspace_at_home;
use crate::history::{AwaitedRefresh, Environment, NextStep, OtherRearview};
use crate::tui::export::ClipboardDestination;
use crate::tui::list_change::ListChange;
use std::path::Path;
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
        vec![kept_storage_under(registered_fixtures()[0], home)],
        Some(base),
    )
}

#[test]
fn conversation_copy_during_another_processs_rebuild_waits_for_session_without_blocking() {
    let home = tempfile::tempdir().unwrap();
    let base = tempfile::tempdir().unwrap();
    let mut app = app_reading(
        vec![cached_session(home.path(), base.path())],
        reading_home(home.path(), base.path()),
    );
    app.set_clipboard_writer_for_test(|_| Ok(ClipboardDestination::System));
    app.enter_view_mode(80);
    let rebuild = AnotherRearviewsOwner::start(base.path(), OwnerActivity::InsideRebuild);

    let started = Instant::now();
    app.perform_export(1, true);

    assert!(
        started.elapsed() < WITHOUT_WAITING,
        "the copy doesn't wait for the rebuild"
    );
    assert_eq!(app.status_message(), None, "the copy succeeds");
    assert!(app.is_open_session_updating());
    drop(rebuild);
}

#[test]
fn rename_with_active_query_doesnt_wait_for_another_processs_rebuild() {
    let home = tempfile::tempdir().unwrap();
    let base = tempfile::tempdir().unwrap();
    let mut app = app_reading(
        vec![cached_session(home.path(), base.path())],
        reading_home(home.path(), base.path()),
    );
    app.set_query_for_test("hello");
    let rebuild = AnotherRearviewsOwner::start(base.path(), OwnerActivity::InsideRebuild);

    let started = Instant::now();
    app.show_renamed_session(0, "Renamed");

    assert!(
        started.elapsed() < WITHOUT_WAITING,
        "the rename doesn't wait for the rebuild"
    );
    drop(rebuild);
}

#[test]
fn workspace_toggle_with_active_query_doesnt_wait_for_another_processs_rebuild() {
    let home = tempfile::tempdir().unwrap();
    let base = tempfile::tempdir().unwrap();
    let mut app = app_reading(
        vec![cached_session(home.path(), base.path())],
        reading_home(home.path(), base.path()),
    );
    app.workspace = Some(workspace_at_home(home.path()));
    app.set_query_for_test("hello");
    let rebuild = AnotherRearviewsOwner::start(base.path(), OwnerActivity::InsideRebuild);

    let started = Instant::now();
    app.toggle_workspace_filter();

    assert!(
        started.elapsed() < WITHOUT_WAITING,
        "the toggle doesn't wait for the rebuild"
    );
    drop(rebuild);
}

/// Room for a drawing-thread read's own work and the system timer's jitter
/// beside its busy wait. Shorter than the observation's 100 ms lock wait,
/// which sleeps about 180 ms on the system clock.
const READ_WORK_ALLOWANCE: Duration = Duration::from_millis(80);

/// The shortest of three runs of `run`, and the last run's result. One run
/// behind a lock can sleep tens of milliseconds longer than another under
/// load.
fn fastest_of_three<T>(mut run: impl FnMut() -> T) -> (Duration, T) {
    let mut fastest = Duration::MAX;
    let mut result = None;
    for _ in 0..3 {
        let started = Instant::now();
        result = Some(run());
        fastest = fastest.min(started.elapsed());
    }
    (fastest, result.unwrap())
}

/// Behind a rebuild's exclusive lock, each drawing-thread read waits for its
/// busy timeout once, then observes the refresh state without a second wait
/// and reports the rebuild. Timed on the system clock, the fastest of three
/// runs of each read against the fastest of three busy waits of the
/// interactive reader's open behind the same lock: SQLite sleeps in steps of
/// the system timer, so that wait ends past its busy timeout.
#[test]
fn drawing_thread_reads_behind_rebuild_wait_out_single_busy_timeout() {
    let home = tempfile::tempdir().unwrap();
    let base = tempfile::tempdir().unwrap();
    let row = cached_session(home.path(), base.path());
    let environment = reading_home(home.path(), base.path());
    let rebuilding =
        NextStep::AwaitRefresh(AwaitedRefresh::AnotherRearview(OtherRearview::Rebuilding));
    let rebuild = exclusive_lock(base.path());
    let (single_busy_wait, opened) =
        fastest_of_three(|| InteractiveCache::at(rearview(base.path())).open());
    assert!(
        matches!(&opened, Err(error) if crate::history::sqlite_cache::is_busy_error(error)),
        "the interactive reader doesn't open behind a rebuild"
    );

    let (read_waited, read) = fastest_of_three(|| {
        InteractiveCache::at(rearview(base.path())).read(|_| Ok(None::<CachedCopy<()>>))
    });
    let (reread_waited, reread) = fastest_of_three(|| {
        reread_sessions(&environment, row.source, &[row.path.as_path()], false)
    });
    drop(rebuild);

    assert!(
        matches!(read.into_read(), CachedRead::Missing { next } if next == rebuilding),
        "the interactive read behind a rebuild names the rebuild"
    );
    let reread_next: Vec<NextStep> = reread
        .into_iter()
        .map(|outcome| outcome.into_parts().1)
        .collect();
    assert_eq!(
        reread_next,
        [rebuilding],
        "the session reread behind a rebuild names the rebuild"
    );
    for (read, waited) in [
        ("the interactive read", read_waited),
        ("the session reread", reread_waited),
    ] {
        assert!(
            waited <= single_busy_wait + READ_WORK_ALLOWANCE,
            "{read} behind a rebuild waits at most one busy wait of {single_busy_wait:?} \
             plus its own work (fastest: {waited:?})"
        );
    }
}

/// The viewport the sweep's apps draw into, in rows.
const VIEWPORT: usize = 20;

/// One drawing-thread action of the sweep: its name in failure messages, and
/// what it does to the app, given the listed session's ID.
type AppAction = (&'static str, fn(&mut App, &str));

fn press(app: &mut App, code: KeyCode) -> Option<Action> {
    app.handle_key(code, KeyModifiers::NONE, VIEWPORT)
}

fn press_ctrl(app: &mut App, code: KeyCode) -> Option<Action> {
    app.handle_key(code, KeyModifiers::CONTROL, VIEWPORT)
}

/// The actions on an open session, its refresh finished and its transcript
/// grown since.
const VIEWER_ACTIONS: &[AppAction] = &[
    ("the viewer reload", |app, _| {
        app.complete_pending_view_refresh(VIEWPORT);
    }),
    ("a frame", |app, _| app.re_render_view(VIEWPORT)),
    ("a viewer Ctrl+R", |app, _| {
        press_ctrl(app, KeyCode::Char('r'));
    }),
    ("a copy", |app, _| app.perform_export(1, true)),
    ("a focused copy", |app, _| {
        press(app, KeyCode::Char(']'));
        press(app, KeyCode::Char('y'));
    }),
    ("the actions menu", |app, _| {
        press_ctrl(app, KeyCode::Char('x'));
        press(app, KeyCode::Esc);
    }),
    ("the completion check", check_the_awaited_refresh),
];

/// The actions in the list, after [`VIEWER_ACTIONS`].
const LIST_ACTIONS: &[AppAction] = &[
    ("closing the viewer", |app, _| app.exit_view_mode()),
    ("opening the session", |app, _| app.enter_view_mode(80)),
    ("closing it again", |app, _| app.exit_view_mode()),
    ("a rename", rename_the_selected_session),
    ("an ID lookup", |app, session_id| {
        app.set_query_for_test(session_id);
        app.update_filter();
    }),
    ("a search", |app, _| {
        app.set_query_for_test("hello");
        app.update_filter();
    }),
    ("a workspace toggle", |app, _| app.toggle_workspace_filter()),
    ("toggling the workspace back", |app, _| {
        app.toggle_workspace_filter();
    }),
    ("clearing the search", |app, _| {
        app.set_query_for_test("");
        app.update_filter();
    }),
    ("a delete", delete_the_selected_session),
    ("a list Ctrl+R", |app, _| {
        press_ctrl(app, KeyCode::Char('r'));
    }),
    ("the completion check", check_the_awaited_refresh),
];

fn check_the_awaited_refresh(app: &mut App, _session_id: &str) {
    app.expire_awaited_refresh_check_for_test();
    app.check_awaited_refresh();
}

/// The name box, its confirmation, and the renamed row once the worker
/// reports the rename done.
fn rename_the_selected_session(app: &mut App, _session_id: &str) {
    app.start_rename();
    app.handle_rename_key(KeyCode::Char('n'), KeyModifiers::NONE);
    app.handle_rename_key(KeyCode::Enter, KeyModifiers::NONE);
    assert!(
        matches!(app.begin_list_change(), Some(ListChange::Rename { .. })),
        "the rename confirmation requests a rename"
    );
    app.show_renamed_session(0, "Renamed");
}

/// The actions menu's Delete and its confirmation, handed on as the runtime
/// hands a confirmed delete to the worker.
fn delete_the_selected_session(app: &mut App, _session_id: &str) {
    press_ctrl(app, KeyCode::Char('x'));
    press(app, KeyCode::Char('d'));
    if let Some(Action::Delete(path)) = press(app, KeyCode::Char('y')) {
        app.request_delete(&path);
    }
    assert!(
        matches!(app.begin_list_change(), Some(ListChange::Delete { .. })),
        "the delete confirmation requests a delete"
    );
}

/// A provider's every-record session written under a home and cached once.
/// Each case reads a copy of the cache, on test time of its own.
struct CachedEveryRecordSession {
    fixture: &'static dyn ProviderFixture,
    home: tempfile::TempDir,
    template: TemplateCache,
    row: Conversation,
}

impl CachedEveryRecordSession {
    fn new(fixture: &'static dyn ProviderFixture) -> Self {
        let home = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        fixture.write_every_record_session(home.path(), &fixture.every_record_session());
        SqliteCache::open(&rearview(cache.path()))
            .unwrap()
            .mark_admitted()
            .unwrap();
        load_under(fixture, home.path(), cache.path());
        let mut rows = load_under(fixture, home.path(), cache.path());
        let agent = fixture.provider().labels().name;
        assert_eq!(rows.len(), 1, "{agent}: the home lists one session");
        let row = rows.remove(0);
        assert!(
            row.cached_session.is_some(),
            "{agent}: the warm load reads the session from the cache"
        );
        Self {
            fixture,
            home,
            template: TemplateCache::new(cache),
            row,
        }
    }

    /// The provider's name in failure messages.
    fn agent(&self) -> &'static str {
        self.fixture.provider().labels().name
    }

    /// A case's copy of the cache, and its environment reading it on test
    /// time of its own.
    fn case(&self) -> (tempfile::TempDir, Environment) {
        let cache = self.template.copy();
        let environment = environment_on_clock(
            vec![kept_storage_under(self.fixture, self.home.path())],
            Some(cache.path()),
            Arc::new(TestClock::default()),
        );
        (cache, environment)
    }

    /// An app over the session's row reading `environment`, the drawing
    /// thread's, with the workspace toggle naming the home.
    fn app_listing(&self, environment: Environment) -> App {
        let mut app = app_reading(vec![self.row.clone()], environment);
        app.set_clipboard_writer_for_test(|_| Ok(ClipboardDestination::System));
        app.workspace = Some(workspace_at_home(self.home.path()));
        app
    }
}

/// Grow the open session's transcript, refresh it through `Ctrl+R` as the
/// worker runs a session refresh, and grow it again: the viewer then holds a
/// finished refresh to reload, over a copy out of date again.
fn finish_a_refresh_of_the_open_session(app: &mut App, transcript: &Path) {
    grow(transcript);
    refresh_the_open_session(app);
    grow(transcript);
}

/// Refresh the open session through `Ctrl+R`, as the worker runs a session
/// refresh, leaving its read for the viewer to reload.
fn refresh_the_open_session(app: &mut App) {
    press_ctrl(app, KeyCode::Char('r'));
    super::refresh_tests::serve_session_refresh(app);
    assert!(
        app.pending_view_reload.is_some(),
        "the session refresh gives the viewer a reload"
    );
}

/// Another rearview's activity on the cache during a case; `None` while no
/// other rearview runs.
const OTHER_REARVIEWS: [Option<OwnerActivity>; 4] = [
    None,
    Some(OwnerActivity::Idle),
    Some(OwnerActivity::InsideBatch),
    Some(OwnerActivity::InsideRebuild),
];

/// Run each of `actions` on `app` while `activity` runs on the cache in
/// `cache`, and fail, naming `label` and the action, on a parse, a writable
/// connection or a wait on the app's clock past the drawing thread's busy
/// timeout. A parse fails through the panic of
/// [`crate::history::ParsePermit::admit`] on the drawing thread.
fn assert_no_action_parses_writes_or_waits(
    label: &str,
    app: &mut App,
    actions: &[AppAction],
    cache: &Path,
    activity: Option<OwnerActivity>,
) {
    let session_id = app
        .conversations()
        .first()
        .map(|row| row.session_id.clone())
        .unwrap_or_default();
    let clock = app.environment.clock().clone();
    let other = activity.map(|activity| AnotherRearviewsOwner::start(cache, activity));
    let drawing_thread = std::thread::current().id();
    let writable_connections = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&writable_connections);
    let _hooks = test_hooks::on_every_connection(cache, move |connection| {
        if std::thread::current().id() == drawing_thread
            && !connection.is_readonly(rusqlite::MAIN_DB).unwrap()
        {
            counted.fetch_add(1, Ordering::SeqCst);
        }
    });

    for (action, run) in actions {
        let started = clock.now();
        if let Err(panic) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run(app, &session_id);
        })) {
            let message = panic
                .downcast_ref::<String>()
                .map(String::as_str)
                .or_else(|| panic.downcast_ref::<&str>().copied())
                .unwrap_or("a panic without a message");
            panic!("{label}, {action}: {message}");
        }
        let waited = clock.elapsed_since(started);
        assert!(
            waited <= INTERACTIVE_BUSY_TIMEOUT,
            "{label}, {action}: the drawing thread waits at most its busy timeout: {waited:?}"
        );
        assert_eq!(
            writable_connections.load(Ordering::SeqCst),
            0,
            "{label}, {action}: the drawing thread doesn't open a writable connection"
        );
    }
    drop(other);
}

/// Beside no other rearview and another rearview's refresh, idle,
/// committing or rebuilding, no drawing-thread action parses a transcript,
/// opens a writable connection or waits past the drawing thread's busy
/// timeout: in the list and the viewer, and for a file opened directly.
/// Waits are measured on the app's injected clock; SQLite's own busy wait
/// sleeps on the system's. Each case runs on a thread of its own, its
/// drawing thread. The cases open the session and prepare its refresh one
/// at a time: each preparation grows the shared transcript.
#[test]
fn no_app_action_parses_writes_or_waits_beside_another_rearviews_activity() {
    let session = &CachedEveryRecordSession::new(registered_fixtures()[0]);
    let one_preparation_at_a_time = &std::sync::Mutex::new(());
    let prepared = |open: &dyn Fn() -> App| {
        let _turn = one_preparation_at_a_time
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut app = open();
        finish_a_refresh_of_the_open_session(&mut app, &session.row.path);
        app
    };
    std::thread::scope(|scope| {
        for activity in OTHER_REARVIEWS {
            scope.spawn(move || {
                let (cache, environment) = session.case();
                let mut app = prepared(&|| {
                    let mut app = session.app_listing(environment.clone());
                    app.enter_view_mode(80);
                    app
                });
                let label = format!("{}, list, {activity:?}", session.agent());
                let actions = [VIEWER_ACTIONS, LIST_ACTIONS].concat();
                assert_no_action_parses_writes_or_waits(
                    &label,
                    &mut app,
                    &actions,
                    cache.path(),
                    activity,
                );
            });
            scope.spawn(move || {
                let (cache, environment) = session.case();
                let mut app = prepared(&|| {
                    let mut app = App::new_single_file_read(
                        session.row.path.clone(),
                        ToolDisplayMode::Hidden,
                        false,
                        KeyBindings::default(),
                        environment.clone(),
                    );
                    app.set_clipboard_writer_for_test(|_| Ok(ClipboardDestination::System));
                    app
                });
                let label = format!("{}, a file opened directly, {activity:?}", session.agent());
                assert_no_action_parses_writes_or_waits(
                    &label,
                    &mut app,
                    VIEWER_ACTIONS,
                    cache.path(),
                    activity,
                );
            });
        }
    });
}

/// Behind another rearview's commit, the viewer's reload and opening a
/// session read each provider's every-record session without a parse, a
/// writable connection or a wait past the drawing thread's busy timeout.
/// Each provider runs on a thread of its own, its drawing thread.
#[test]
fn every_providers_session_opens_and_reloads_behind_another_rearviews_commit() {
    const READS: &[AppAction] = &[
        ("the viewer reload", |app, _| {
            app.complete_pending_view_refresh(VIEWPORT);
        }),
        ("closing the viewer", |app, _| app.exit_view_mode()),
        ("opening the session", |app, _| app.enter_view_mode(80)),
    ];
    std::thread::scope(|scope| {
        for fixture in registered_fixtures() {
            scope.spawn(move || {
                let session = CachedEveryRecordSession::new(fixture);
                let (cache, environment) = session.case();
                let mut app = session.app_listing(environment);
                app.enter_view_mode(80);
                refresh_the_open_session(&mut app);

                assert_no_action_parses_writes_or_waits(
                    session.agent(),
                    &mut app,
                    READS,
                    cache.path(),
                    Some(OwnerActivity::InsideBatch),
                );
            });
        }
    });
}

#[test]
fn opening_session_another_process_is_writing_waits_for_that_refresh() {
    let home = tempfile::tempdir().unwrap();
    let base = tempfile::tempdir().unwrap();
    let row = cached_session(home.path(), base.path());
    mark_every_session_writing(base.path());
    let other = AnotherRearviewsOwner::hold(base.path());
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
fn file_opened_directly_doesnt_read_on_drawing_thread_while_another_process_writes_it() {
    let home = tempfile::tempdir().unwrap();
    let base = tempfile::tempdir().unwrap();
    let row = cached_session(home.path(), base.path());
    mark_every_session_writing(base.path());
    let other = AnotherRearviewsOwner::hold(base.path());
    let mut app = App::new_single_file(
        row.path.clone(),
        ToolDisplayMode::Hidden,
        false,
        KeyBindings::default(),
        reading_home(home.path(), base.path()),
    );

    let started = Instant::now();
    app.re_render_view(20);

    assert!(started.elapsed() < WITHOUT_WAITING);
    assert!(
        app.is_open_session_updating(),
        "the file opened directly waits for the background worker's read"
    );
    drop(other);
}

#[test]
fn file_opened_directly_asks_for_session_refresh_once_awaited_refresh_completes() {
    let home = tempfile::tempdir().unwrap();
    let base = tempfile::tempdir().unwrap();
    let row = cached_session(home.path(), base.path());
    let path = row.path.clone();
    let mut app = App::new_single_file_read(
        path.clone(),
        ToolDisplayMode::Hidden,
        false,
        KeyBindings::default(),
        reading_home(home.path(), base.path()),
    );
    let other_refresh = AnotherRearviewsOwner::hold(base.path());
    app.record_next_step(
        &path,
        NextStep::AwaitRefresh(AwaitedRefresh::AnotherRearview(OtherRearview::Refreshing)),
    );
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
    assert!(
        app.begin_session_refresh().is_some(),
        "the file opened directly is handed to a session refresh"
    );
}

#[test]
fn rows_awaiting_this_processs_refresh_wait_for_process_that_took_it_over() {
    let base = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let fixture = registered_fixtures()[0];
    let path = fixture.write_session(home.path(), fixture.ids().session);
    let mut rows = load_under(fixture, home.path(), base.path());
    let environment = reading_home(home.path(), base.path());
    let held = RefreshOwnership::acquire_for_test(environment.cache_base()).unwrap();
    let mut app = app_reading(vec![rows.remove(0)], environment);
    grow(&path);
    app.record_next_step(&path, NextStep::AwaitRefresh(AwaitedRefresh::ThisProcess));
    assert!(!app.is_waiting_for_another_rearview());
    expire_last_commit(base.path());
    let other = AnotherRearviewsOwner::hold(base.path());

    app.expire_awaited_refresh_check_for_test();
    app.check_awaited_refresh();
    assert!(
        app.is_waiting_for_another_rearview(),
        "the check sees the process that took the refresh over"
    );
    assert!(!app.is_session_refresh_requested());

    other.release();
    app.expire_awaited_refresh_check_for_test();
    app.check_awaited_refresh();
    assert!(
        app.is_session_refresh_requested(),
        "the row refreshes once the other refresh completes"
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
fn ctrl_r_on_changed_session_hands_it_to_session_refresh_without_writing_cache() {
    let home = tempfile::tempdir().unwrap();
    let base = tempfile::tempdir().unwrap();
    let row = cached_session(home.path(), base.path());
    let cached_sizes = recorded_sizes(base.path());
    grow(&row.path);
    let mut app = app_viewing_written_session(row, home.path(), base.path());

    let started = Instant::now();
    app.handle_key(KeyCode::Char('r'), KeyModifiers::CONTROL, 20);

    assert!(started.elapsed() < WITHOUT_WAITING);
    assert!(
        matches!(app.app_mode(), AppMode::View(_)),
        "the refreshed session stays in the viewer: {:?}",
        app.status_message()
    );
    assert!(
        app.is_session_refresh_requested(),
        "the session is handed to a session refresh"
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
fn session_refresh_caches_session_ctrl_r_read_and_updates_its_row() {
    let home = tempfile::tempdir().unwrap();
    let base = tempfile::tempdir().unwrap();
    let fixture = registered_fixtures()[0];
    let row = cached_session(home.path(), base.path());
    let path = row.path.clone();
    grow(&path);
    let mut app = app_viewing_written_session(row, home.path(), base.path());
    app.handle_key(KeyCode::Char('r'), KeyModifiers::CONTROL, 20);
    app.exit_view_mode();
    let size_now = grow(&path);

    let sessions = app
        .begin_session_refresh()
        .expect("the session read outside the database is handed on");
    let cache_base = rearview(base.path());
    let outcome = std::thread::spawn(move || {
        let operation = crate::history::OperationScope::new(
            &cache_base,
            crate::history::Patience::UntilAcquired,
            Arc::default(),
        );
        let outcome = sessions.refresh(&operation);
        operation.finish().and(outcome)
    })
    .join()
    .unwrap();
    app.finish_refresh(outcome);

    assert_eq!(
        app.conversations()[0]
            .fingerprint
            .map(|fingerprint| fingerprint.file_size),
        Some(size_now),
        "the row shows the session refresh's read"
    );
    assert!(!app.is_refreshing());
    drop(app);
    assert_eq!(
        current_sizes(fixture, home.path(), base.path()),
        [size_now],
        "the session refresh caches the session"
    );
}

/// An app listing `home`'s sessions, loaded through the test's own session
/// cache, that reads the session cache under `base` as this process reaches
/// it, on `clock`.
fn app_listing_home_over_cache(home: &ClaudeHome, base: &Path, clock: Arc<dyn Clock>) -> App {
    app_reading(
        home.rows(&home.environment()),
        home.environment_with_cache(base, ProcessIdentity::default(), clock),
    )
}

/// The drawing thread's check for a refresh's completion reads the refresh
/// owner without taking SQLite's writer lock.
#[test]
fn completion_check_doesnt_open_writable_connection() {
    let home = ClaudeHome::new();
    let base = tempfile::tempdir().unwrap();
    let a = home.write("a", 5, &["a"]);
    let clock: Arc<dyn Clock> = Arc::new(TestClock::default());
    let other_refresh =
        RefreshOwnership::acquire_for_test(&another_rearview(base.path(), clock.clone())).unwrap();
    let mut app = app_listing_home_over_cache(&home, base.path(), clock);
    app.record_next_step(
        &a,
        NextStep::AwaitRefresh(AwaitedRefresh::AnotherRearview(OtherRearview::Refreshing)),
    );
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

/// Another connection's write transaction, while no refresh owns the cache,
/// is not another rearview's refresh.
#[test]
fn completion_check_beside_write_without_owner_doesnt_report_another_rearview() {
    let home = ClaudeHome::new();
    let base = tempfile::tempdir().unwrap();
    let a = home.write("a", 5, &["a"]);
    let clock: Arc<dyn Clock> = Arc::new(TestClock::default());
    drop(
        RefreshOwnership::acquire_for_test(&another_rearview(base.path(), clock.clone())).unwrap(),
    );
    let write_without_an_owner = writer_lock_held(base.path());
    let mut app = app_listing_home_over_cache(&home, base.path(), clock);
    app.record_next_step(
        &a,
        NextStep::AwaitRefresh(AwaitedRefresh::AnotherRearview(OtherRearview::Refreshing)),
    );

    app.expire_awaited_refresh_check_for_test();
    app.check_awaited_refresh();

    assert!(!app.is_waiting_for_another_rearview());
    drop(write_without_an_owner);
}

/// After another rearview takes over this process's expired refresh and
/// begins a rebuild, the refresh owner cannot be read. The completion check
/// reports another rearview, so the waiting label shows.
#[test]
fn another_rearviews_rebuild_after_takeover_doesnt_read_as_this_processs_refresh() {
    let home = ClaudeHome::new();
    let base = tempfile::tempdir().unwrap();
    let a = home.write("a", 5, &["a"]);
    let clock: Arc<dyn Clock> = Arc::new(TestClock::default());
    let environment =
        home.environment_with_cache(base.path(), ProcessIdentity::default(), clock.clone());
    let paused_refresh = RefreshOwnership::acquire_for_test(environment.cache_base()).unwrap();
    expire_last_commit(base.path());
    let takeover =
        RefreshOwnership::acquire_for_test(&another_rearview(base.path(), clock)).unwrap();
    let rebuild = exclusive_lock(base.path());
    let mut app = app_reading(home.rows(&home.environment()), environment);
    app.record_next_step(&a, NextStep::AwaitRefresh(AwaitedRefresh::ThisProcess));

    app.expire_awaited_refresh_check_for_test();
    app.check_awaited_refresh();

    assert!(app.is_waiting_for_another_rearview());
    drop(rebuild);
    drop(takeover);
    drop(paused_refresh);
}

/// An app listing nothing, reading the session cache under `base`.
fn app_reading_cache(base: &Path) -> App {
    app_reading(Vec::new(), real_time_environment(Vec::new(), Some(base)))
}

#[test]
#[should_panic(expected = "drawing thread")]
fn refresh_ownership_refuses_drawing_thread() {
    let base = tempfile::tempdir().unwrap();
    let _app = app_reading_cache(base.path());

    let _ = RefreshOwnership::acquire_for_test(&rearview(base.path()));
}

#[test]
#[should_panic(expected = "drawing thread")]
fn cache_writer_refuses_drawing_thread() {
    let base = tempfile::tempdir().unwrap();
    let ownership = RefreshOwnership::acquire_for_test(&rearview(base.path())).unwrap();
    let _app = app_reading_cache(base.path());

    let _ = SqliteCache::open_for_refresh(ownership.token());
}

#[test]
#[should_panic(expected = "drawing thread")]
fn waiting_cache_reader_refuses_drawing_thread() {
    let base = tempfile::tempdir().unwrap();
    SqliteCache::open(&rearview(base.path()))
        .unwrap()
        .mark_admitted()
        .unwrap();
    let _app = app_reading_cache(base.path());

    let _ = SqliteCache::open_waiting(&rearview(base.path()));
}
