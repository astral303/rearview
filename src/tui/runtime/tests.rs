//! The TUI's event loop body against a test backend, fed the terminal events
//! a test scripts.

use super::*;
use crate::clock::{Clock, SystemClock, TestClock};
use crate::history::sqlite_cache::DATABASE_FILE_NAME;
use crate::history::sqlite_cache::connection::test_hooks::{self, CommitGate};
use crate::history::sqlite_cache::refresh::RefreshOwnership;
use crate::history::sqlite_cache::refresh::tests::{
    OtherProcessOwner, exclusive_lock, expire_last_commit, writer_lock_held,
};
use crate::history::{CacheBase, InteractiveCache, ProcessIdentity};
use crate::tui::SessionAction;
use crate::tui::app::AppMode;
use crate::tui::app::claude_home::{ClaudeHome, admitted_cache, another_rearview};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::backend::TestBackend;
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

/// The longest a test ticks while the load or a refresh runs.
const BACKGROUND_WORK_LIMIT: Duration = Duration::from_secs(10);

/// The TUI launched over `home`'s sessions with the test's own session cache:
/// the list, filled by the streaming load.
fn launched_over(home: &ClaudeHome) -> Runtime {
    launched_in(home.environment())
}

/// The TUI launched over the sessions `environment` reads.
fn launched_in(environment: Environment) -> Runtime {
    let app = App::new_loading_with_options(
        ToolDisplayMode::Hidden,
        false,
        KeyBindings::default(),
        ListScope::default(),
        TuiSearchOptions::default(),
        environment.clone(),
    );
    Runtime::listing(
        app,
        environment,
        Arc::new(LoadMeter::default()),
        RefreshOptions::default(),
    )
}

fn terminal() -> Terminal<TestBackend> {
    Terminal::new(TestBackend::new(100, 30)).unwrap()
}

/// Tick `runtime` without terminal input until `is_done` holds for its app.
fn tick_until(
    runtime: &mut Runtime,
    terminal: &mut Terminal<TestBackend>,
    is_done: impl Fn(&App) -> bool,
) {
    assert!(
        ticks_reach(runtime, terminal, BACKGROUND_WORK_LIMIT, is_done),
        "the background work finishes"
    );
}

/// True when ticking `runtime` without terminal input reaches a state
/// `is_reached` holds for within `limit`.
fn ticks_reach(
    runtime: &mut Runtime,
    terminal: &mut Terminal<TestBackend>,
    limit: Duration,
    is_reached: impl Fn(&App) -> bool,
) -> bool {
    let started = Instant::now();
    while !is_reached(runtime.app()) {
        if started.elapsed() > limit {
            return false;
        }
        runtime
            .tick(terminal, &mut |wait| {
                std::thread::sleep(wait.min(Duration::from_millis(10)));
                Ok(Vec::new())
            })
            .unwrap();
    }
    true
}

/// One tick whose poll returns `events`.
fn tick_with(
    runtime: &mut Runtime,
    terminal: &mut Terminal<TestBackend>,
    events: Vec<Event>,
) -> Tick {
    let mut events = Some(events);
    runtime
        .tick(terminal, &mut |_| Ok(events.take().unwrap_or_default()))
        .unwrap()
}

fn key(code: KeyCode, modifiers: KeyModifiers) -> Event {
    Event::Key(KeyEvent::new(code, modifiers))
}

fn listed_ids(app: &App) -> Vec<&str> {
    app.filtered()
        .iter()
        .map(|&index| app.conversations()[index].session_id.as_str())
        .collect()
}

/// Every character `terminal` last drew, row after row.
fn shown_text(terminal: &Terminal<TestBackend>) -> String {
    terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect()
}

/// True when ticking `runtime` without terminal input draws `text` within
/// `limit`.
fn ticks_show(
    runtime: &mut Runtime,
    terminal: &mut Terminal<TestBackend>,
    limit: Duration,
    text: &str,
) -> bool {
    let started = Instant::now();
    loop {
        runtime
            .tick(terminal, &mut |wait| {
                std::thread::sleep(wait.min(Duration::from_millis(10)));
                Ok(Vec::new())
            })
            .unwrap();
        if shown_text(terminal).contains(text) {
            return true;
        }
        if started.elapsed() > limit {
            return false;
        }
    }
}

#[test]
fn ticks_list_launch_load_and_esc_quits() {
    let home = ClaudeHome::new();
    home.write("b", 10, &["b"]);
    home.write("a", 5, &["a"]);
    let mut runtime = launched_over(&home);
    let mut terminal = terminal();

    tick_until(&mut runtime, &mut terminal, |app| !app.is_loading());
    assert_eq!(listed_ids(runtime.app()), ["b", "a"]);

    let tick = tick_with(
        &mut runtime,
        &mut terminal,
        vec![key(KeyCode::Esc, KeyModifiers::NONE)],
    );
    assert!(matches!(tick, Tick::Exit(Action::Quit)));
}

/// The number of sessions the database in `base` holds.
fn cached_session_count(base: &Path) -> i64 {
    rusqlite::Connection::open(base.join(DATABASE_FILE_NAME))
        .unwrap()
        .query_row("SELECT COUNT(*) FROM sessions", [], |row| row.get(0))
        .unwrap()
}

/// Refresh ownership of the cache `base` names, acquired on a thread of its
/// own, as a background thread holds it: the TUI's thread is the drawing
/// thread, which refuses ownership.
fn acquired_elsewhere(base: CacheBase) -> RefreshOwnership {
    std::thread::spawn(move || RefreshOwnership::acquire_for_test(&base).unwrap())
        .join()
        .unwrap()
}

/// Press `Ctrl+R`, then tick until the list refresh it requests has begun
/// and its outcome is applied.
fn refresh_the_list(runtime: &mut Runtime, terminal: &mut Terminal<TestBackend>) {
    tick_with(
        runtime,
        terminal,
        vec![key(KeyCode::Char('r'), KeyModifiers::CONTROL)],
    );
    tick_until(runtime, terminal, App::is_refreshing);
    tick_until(runtime, terminal, |app| !app.is_refreshing());
}

#[test]
fn ctrl_r_lists_session_written_after_launch() {
    let home = ClaudeHome::new();
    home.write("a", 5, &["a"]);
    let mut runtime = launched_over(&home);
    let mut terminal = terminal();
    tick_until(&mut runtime, &mut terminal, |app| !app.is_loading());
    home.write("b", 10, &["b"]);

    refresh_the_list(&mut runtime, &mut terminal);

    assert_eq!(listed_ids(runtime.app()), ["b", "a"]);
}

/// A launch whose cache write finds less than 1 GB free on the cache's drive
/// leaves the TUI with the free space, and the cache database is deleted.
#[test]
fn launch_whose_cache_write_finds_less_than_gigabyte_free_stops_with_free_space() {
    let home = ClaudeHome::new();
    home.write("a", 5, &["a"]);
    let cache = tempfile::tempdir().unwrap();
    let environment = home
        .environment_with_cache(
            cache.path(),
            ProcessIdentity::default(),
            Arc::new(SystemClock::new()),
        )
        .with_free_space(950_000_000);
    let mut runtime = launched_in(environment);
    let mut terminal = terminal();

    let started = Instant::now();
    let error = loop {
        let tick = runtime.tick(&mut terminal, &mut |wait| {
            std::thread::sleep(wait.min(Duration::from_millis(10)));
            Ok(Vec::new())
        });
        match tick {
            Err(error) => break error,
            Ok(_) => assert!(
                started.elapsed() < BACKGROUND_WORK_LIMIT,
                "the launch stops"
            ),
        }
    };

    assert_eq!(
        error.to_string(),
        "not enough disk space for the session cache (0.9 GB free, needs at least 1 GB). Free some space and run rearview again."
    );
    assert!(!cache.path().join(DATABASE_FILE_NAME).exists());
}

/// Behind another rearview's rebuild, the launch shows the rebuilding label
/// while it waits.
#[test]
fn launch_behind_another_rearviews_rebuild_shows_wait_at_once() {
    let home = ClaudeHome::new();
    home.write("a", 5, &["a"]);
    let cache = admitted_cache();
    let rebuild = exclusive_lock(cache.path());
    let mut runtime = launched_in(home.environment_with_cache(
        cache.path(),
        ProcessIdentity::default(),
        Arc::new(TestClock::default()),
    ));
    let mut terminal = terminal();

    let showed_the_wait = ticks_show(
        &mut runtime,
        &mut terminal,
        Duration::from_secs(2),
        "Another rearview is rebuilding the session cache…",
    );
    drop(rebuild);
    tick_until(&mut runtime, &mut terminal, |app| !app.is_loading());

    assert!(
        showed_the_wait,
        "the launch shows that another rearview rebuilds the cache while it waits"
    );
}

/// During another rearview's ordinary refresh, the launch lists the cache
/// as it is under the waiting label, then reads what that refresh left out
/// once it completes.
#[test]
fn launch_behind_another_rearviews_refresh_lists_cache_as_it_is_under_label() {
    let home = ClaudeHome::new();
    home.write("b", 10, &["b"]);
    let a = home.write("a", 5, &["a"]);
    let cache = admitted_cache();
    let clock: Arc<dyn Clock> = Arc::new(SystemClock::new());
    let environment =
        home.environment_with_cache(cache.path(), ProcessIdentity::default(), clock.clone());
    home.rows(&environment);
    home.append(&a, 6, "written after the cache");
    let other_refresh = acquired_elsewhere(another_rearview(cache.path(), clock));
    let mut runtime = launched_in(environment);
    let mut terminal = terminal();

    let listed_the_cache =
        ticks_reach(&mut runtime, &mut terminal, Duration::from_secs(5), |app| {
            listed_ids(app) == ["b", "a"]
        });
    assert!(
        listed_the_cache,
        "the launch lists the cached sessions while another rearview refreshes"
    );
    let shown = shown_text(&terminal);
    let cached_message_count = runtime.app().conversations()[1].message_count;
    drop(other_refresh);
    tick_until(&mut runtime, &mut terminal, |app| !app.is_loading());

    assert!(shown.contains("Another rearview is refreshing…"), "{shown}");
    assert_eq!(cached_message_count, 1);
    assert_eq!(runtime.app().conversations()[1].message_count, 2);
}

/// The refresh owner the session cache database in `base` stores; `None`
/// while no refresh owns the cache.
fn stored_owner(base: &Path) -> Option<String> {
    rusqlite::Connection::open(base.join(DATABASE_FILE_NAME))
        .unwrap()
        .query_row(
            "SELECT owner FROM refresh_state WHERE singleton = 1",
            [],
            |row| row.get(0),
        )
        .unwrap()
}

/// SQLite's exclusive lock on the session cache database in a base, as a
/// connection that closes last holds it through its checkpoint. It is taken
/// when the first connection that can write opens after a new refresh owner
/// acquired the cache, and released when the thread that opened that
/// connection opens another.
struct LockedAtNewOwnersFirstWriter {
    is_taken: Arc<AtomicBool>,
    _installed: test_hooks::Installed,
}

impl LockedAtNewOwnersFirstWriter {
    /// The lock for the next owner of the cache in `base`, after the owner
    /// stored now.
    fn on(base: &Path) -> Self {
        let directory = base.to_path_buf();
        let owner_before = stored_owner(base);
        let is_taken = Arc::new(AtomicBool::new(false));
        let taken = Arc::clone(&is_taken);
        let held: Mutex<Option<(std::thread::ThreadId, rusqlite::Connection)>> = Mutex::new(None);
        let installed = test_hooks::on_every_connection(base, move |connection| {
            let mut held = held.lock().unwrap();
            if let Some((thread, _)) = &*held {
                if *thread == std::thread::current().id() {
                    *held = None;
                }
                return;
            }
            if taken.load(Ordering::SeqCst)
                || connection.is_readonly(rusqlite::MAIN_DB).unwrap()
                || stored_owner(&directory).is_none_or(|owner| Some(owner) == owner_before)
            {
                return;
            }
            taken.store(true, Ordering::SeqCst);
            *held = Some((std::thread::current().id(), exclusive_lock(&directory)));
        });
        Self {
            is_taken,
            _installed: installed,
        }
    }

    fn is_taken(&self) -> bool {
        self.is_taken.load(Ordering::SeqCst)
    }
}

/// Behind another rearview's refresh, the launch lists the cached sessions,
/// then refreshes them once that refresh ends, when another connection
/// holds the database's exclusive lock as the launch opens its first write
/// batch.
#[test]
fn launch_behind_another_refresh_waits_out_locked_database_and_refreshes() {
    let home = ClaudeHome::new();
    home.write("b", 10, &["b"]);
    let a = home.write("a", 5, &["a"]);
    let cache = admitted_cache();
    let clock: Arc<dyn Clock> = Arc::new(SystemClock::new());
    let environment =
        home.environment_with_cache(cache.path(), ProcessIdentity::default(), clock.clone());
    home.rows(&environment);
    home.append(&a, 6, "written after the cache");
    let other_refresh = acquired_elsewhere(another_rearview(cache.path(), clock));
    let locked = LockedAtNewOwnersFirstWriter::on(cache.path());
    let mut runtime = launched_in(environment);
    let mut terminal = terminal();

    let listed_the_cache =
        ticks_reach(&mut runtime, &mut terminal, Duration::from_secs(5), |app| {
            listed_ids(app) == ["b", "a"]
        });
    drop(other_refresh);
    tick_until(&mut runtime, &mut terminal, |app| !app.is_loading());

    assert!(
        listed_the_cache,
        "the launch lists the cached sessions while another rearview refreshes"
    );
    assert!(
        locked.is_taken(),
        "the launch's refresh opens a connection that can write"
    );
    assert_eq!(
        runtime.app().conversations()[1].message_count,
        2,
        "the launch refreshes the changed session once the database is unlocked"
    );
}

/// Behind another rearview's refresh whose write outlasts the lock wait, the
/// launch keeps waiting, then caches the session it lists. It doesn't parse
/// the transcripts itself while the other rearview writes.
#[test]
fn launch_behind_another_rearviews_long_write_waits_for_it_instead_of_reading_transcripts() {
    let home = ClaudeHome::new();
    home.write("a", 5, &["a"]);
    let cache = admitted_cache();
    let clock: Arc<dyn Clock> = Arc::new(TestClock::default());
    let other_refresh =
        RefreshOwnership::acquire_for_test(&another_rearview(cache.path(), clock.clone())).unwrap();
    let write_batch = writer_lock_held(cache.path());
    let mut runtime =
        launched_in(home.environment_with_cache(cache.path(), ProcessIdentity::default(), clock));
    let mut terminal = terminal();

    let finished_during_the_write =
        ticks_reach(&mut runtime, &mut terminal, Duration::from_secs(1), |app| {
            !app.is_loading()
        });
    drop(write_batch);
    drop(other_refresh);
    tick_until(&mut runtime, &mut terminal, |app| !app.is_loading());

    assert!(
        !finished_during_the_write,
        "the launch doesn't read the transcripts itself while another rearview writes"
    );
    assert_eq!(
        cached_session_count(cache.path()),
        1,
        "the launch caches the session it lists"
    );
}

/// The message count of the copy of the session the database in `base`
/// holds.
fn cached_message_count(base: &Path) -> i64 {
    rusqlite::Connection::open(base.join(DATABASE_FILE_NAME))
        .unwrap()
        .query_row("SELECT message_count FROM sessions", [], |row| row.get(0))
        .unwrap()
}

/// A list `Ctrl+R` pressed during the launch is queued, and runs once the
/// launch finishes.
#[test]
fn ctrl_r_pressed_during_launch_runs_after_it() {
    let home = ClaudeHome::new();
    home.write("a", 5, &["a"]);
    let cache = admitted_cache();
    let rebuild = exclusive_lock(cache.path());
    let mut runtime = launched_in(home.environment_with_cache(
        cache.path(),
        ProcessIdentity::default(),
        Arc::new(TestClock::default()),
    ));
    let mut terminal = terminal();
    tick_with(
        &mut runtime,
        &mut terminal,
        vec![key(KeyCode::Char('r'), KeyModifiers::CONTROL)],
    );
    assert!(runtime.app().is_loading());

    drop(rebuild);
    tick_until(&mut runtime, &mut terminal, |app| !app.is_loading());

    assert!(
        runtime.app().is_refreshing(),
        "the Ctrl+R pressed during the launch starts a refresh"
    );
    tick_until(&mut runtime, &mut terminal, |app| !app.is_refreshing());
}

/// A list `Ctrl+R` waiting for another rearview's refresh when the TUI quits
/// doesn't take ownership once that refresh ends.
#[test]
fn list_refresh_waiting_at_quit_doesnt_refresh_once_other_rearview_finishes() {
    let home = ClaudeHome::new();
    let a = home.write("a", 5, &["a"]);
    let cache = admitted_cache();
    let clock: Arc<dyn Clock> = Arc::new(SystemClock::new());
    let mut runtime = launched_in(home.environment_with_cache(
        cache.path(),
        ProcessIdentity::default(),
        clock.clone(),
    ));
    let mut terminal = terminal();
    tick_until(&mut runtime, &mut terminal, |app| !app.is_loading());
    let other_refresh = acquired_elsewhere(another_rearview(cache.path(), clock.clone()));
    home.append(&a, 6, "written after the launch");
    tick_with(
        &mut runtime,
        &mut terminal,
        vec![key(KeyCode::Char('r'), KeyModifiers::CONTROL)],
    );
    assert!(ticks_show(
        &mut runtime,
        &mut terminal,
        BACKGROUND_WORK_LIMIT,
        "Another rearview is refreshing…"
    ));
    let tick = tick_with(
        &mut runtime,
        &mut terminal,
        vec![key(KeyCode::Esc, KeyModifiers::NONE)],
    );
    assert!(matches!(tick, Tick::Exit(Action::Quit)));

    drop(other_refresh);
    tick_until(&mut runtime, &mut terminal, |app| !app.is_refreshing());

    assert_eq!(
        cached_message_count(cache.path()),
        1,
        "the refresh doesn't run after the TUI quits"
    );
    assert_eq!(
        InteractiveCache::at(another_rearview(cache.path(), clock)).awaited_refresh(),
        None
    );
}

/// Quitting releases this process's refresh ownership in the database;
/// otherwise other rearviews wait up to 15 seconds to take it over.
#[test]
fn quitting_releases_refresh_ownership_this_process_holds() {
    let home = ClaudeHome::new();
    home.write("a", 5, &["a"]);
    let cache = admitted_cache();
    let clock: Arc<dyn Clock> = Arc::new(TestClock::default());
    let environment =
        home.environment_with_cache(cache.path(), ProcessIdentity::default(), clock.clone());
    let cache_base = environment.cache_base().clone();
    let mut runtime = launched_in(environment);
    let mut terminal = terminal();
    tick_until(&mut runtime, &mut terminal, |app| !app.is_loading());
    let held_by_a_background_thread = acquired_elsewhere(cache_base);

    let tick = tick_with(
        &mut runtime,
        &mut terminal,
        vec![key(KeyCode::Esc, KeyModifiers::NONE)],
    );

    assert!(matches!(tick, Tick::Exit(Action::Quit)));
    assert_eq!(
        InteractiveCache::at(another_rearview(cache.path(), clock)).awaited_refresh(),
        None,
        "another rearview doesn't find this process's refresh running"
    );
    assert!(!held_by_a_background_thread.token().can_publish());
}

/// Quitting while the launch parses a root stops the launch before its next
/// root, and leaves the cache to another rearview once the parse it held
/// ends.
#[test]
fn quitting_during_launch_stops_it_before_its_next_parse() {
    let (first_root, second_root) = (ClaudeHome::new(), ClaudeHome::new());
    first_root.write("0f000000-0000-4000-8000-000000000001", 5, &["first root"]);
    second_root.write("0f000000-0000-4000-8000-000000000002", 6, &["second root"]);
    second_root.write("0f000000-0000-4000-8000-000000000003", 7, &["second root"]);
    let cache = admitted_cache();
    let watch = crate::history::provider::ParseWatch::default();
    let storage: &'static dyn crate::history::provider::SessionStorage =
        Box::leak(Box::new(crate::history::provider::WatchedStorage {
            inner: crate::history::Source::Claude.provider().storage(),
            roots: [&first_root, &second_root]
                .map(|home| {
                    crate::history::provider::SessionRoot::new(home.project().parent().unwrap())
                })
                .to_vec(),
            watch: watch.clone(),
        }));
    let clock: Arc<dyn Clock> = Arc::new(SystemClock::new());
    let mut runtime = launched_in(Environment::new(
        vec![storage],
        cache.path().to_path_buf(),
        ProcessIdentity::default(),
        clock.clone(),
        Arc::new(crate::history::provider::walk::SystemLister),
    ));
    let mut terminal = terminal();
    watch.hold();
    let deadline = Instant::now() + BACKGROUND_WORK_LIMIT;
    while watch.parses_begun() == 0 {
        assert!(Instant::now() < deadline, "the launch begins parsing");
        tick_with(&mut runtime, &mut terminal, Vec::new());
    }

    let tick = tick_with(
        &mut runtime,
        &mut terminal,
        vec![key(KeyCode::Esc, KeyModifiers::NONE)],
    );
    watch.mark();
    watch.release();

    assert!(matches!(tick, Tick::Exit(Action::Quit)));
    let other = another_rearview(cache.path(), clock);
    let deadline = Instant::now() + BACKGROUND_WORK_LIMIT;
    while !matches!(
        RefreshOwnership::try_acquire_now(&other).unwrap(),
        crate::history::sqlite_cache::refresh::Acquisition::Acquired(_)
    ) {
        assert!(
            Instant::now() < deadline,
            "the launch releases the cache after the TUI quits"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        watch.parses_after_mark(),
        0,
        "the launch doesn't parse the next root after the TUI quits"
    );
}

/// While this rearview's own launch rebuilds an outdated cache, a session
/// found by ID waits for this rearview, not for another.
#[test]
fn id_lookup_during_this_rearviews_own_rebuild_doesnt_name_another_rearview() {
    const LATER: &str = "0f000000-0000-4000-8000-0000000000aa";
    let home = ClaudeHome::new();
    let a = home.write("0f000000-0000-4000-8000-000000000001", 5, &["first"]);
    let cache = admitted_cache();
    let mut terminal = terminal();
    let mut runtime = launched_over_cache(&home, cache.path(), &mut terminal);
    home.append(&a, 6, "written after the launch");
    home.write(LATER, 10, &["written later"]);
    rusqlite::Connection::open(cache.path().join(DATABASE_FILE_NAME))
        .unwrap()
        .execute(
            "UPDATE meta SET value = '0' WHERE key = 'schema_version'",
            [],
        )
        .unwrap();
    let rebuild = test_hooks::RebuildGate::closed_on(cache.path());
    open_the_selected_session(&mut runtime, &mut terminal);
    let deadline = Instant::now() + BACKGROUND_WORK_LIMIT;
    while !rebuild.is_rebuild_held() {
        assert!(
            Instant::now() < deadline,
            "the session refresh begins a rebuild"
        );
        tick_with(&mut runtime, &mut terminal, Vec::new());
    }
    tick_with(
        &mut runtime,
        &mut terminal,
        vec![key(KeyCode::Esc, KeyModifiers::NONE)],
    );

    tick_with(
        &mut runtime,
        &mut terminal,
        LATER
            .chars()
            .map(|character| key(KeyCode::Char(character), KeyModifiers::NONE))
            .collect(),
    );

    assert!(
        !ticks_show(
            &mut runtime,
            &mut terminal,
            Duration::from_secs(1),
            "Another rearview is rebuilding"
        ),
        "the viewer doesn't name this rearview's own rebuild as another rearview's"
    );
    drop(rebuild);
}

/// A list `Ctrl+R` pressed while a session refresh waits for another
/// rearview's refresh runs once that refresh ends, and lists the new session.
#[test]
fn ctrl_r_pressed_while_session_caches_in_background_runs_after_it() {
    let home = ClaudeHome::new();
    let a = home.write("a", 5, &["a"]);
    let cache = admitted_cache();
    let clock: Arc<dyn Clock> = Arc::new(SystemClock::new());
    let mut runtime = launched_in(home.environment_with_cache(
        cache.path(),
        ProcessIdentity::default(),
        clock.clone(),
    ));
    let mut terminal = terminal();
    tick_until(&mut runtime, &mut terminal, |app| !app.is_loading());
    home.append(&a, 6, "written after the launch");
    tick_with(
        &mut runtime,
        &mut terminal,
        vec![key(KeyCode::Enter, KeyModifiers::NONE)],
    );
    tick_until(&mut runtime, &mut terminal, |app| {
        matches!(app.app_mode(), AppMode::View(_))
    });
    assert!(runtime.app().is_session_refresh_requested());
    let other_refresh = acquired_elsewhere(another_rearview(cache.path(), clock));
    tick_with(
        &mut runtime,
        &mut terminal,
        vec![key(KeyCode::Esc, KeyModifiers::NONE)],
    );
    home.write("b", 10, &["b"]);
    tick_with(
        &mut runtime,
        &mut terminal,
        vec![key(KeyCode::Char('r'), KeyModifiers::CONTROL)],
    );

    drop(other_refresh);
    let listed_the_new_session =
        ticks_reach(&mut runtime, &mut terminal, Duration::from_secs(5), |app| {
            listed_ids(app).contains(&"b")
        });

    assert!(
        listed_the_new_session,
        "the Ctrl+R pressed while the session was being cached lists the new session"
    );
}

/// A list `Ctrl+R` whose cache writes fail keeps each row's cached copy with
/// a pending refresh.
#[test]
fn list_refresh_whose_cache_write_fails_keeps_its_rows_pending_refresh() {
    let home = ClaudeHome::new();
    let a = home.write("a", 5, &["a"]);
    let cache = admitted_cache();
    let mut runtime = launched_in(home.environment_with_cache(
        cache.path(),
        ProcessIdentity::default(),
        Arc::new(TestClock::default()),
    ));
    let mut terminal = terminal();
    tick_until(&mut runtime, &mut terminal, |app| !app.is_loading());
    home.append(&a, 6, "written after the launch");
    let commits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let _every_commit_after_the_first_fails =
        test_hooks::on_every_connection(cache.path(), move |connection| {
            let commits = Arc::clone(&commits);
            connection
                .commit_hook(Some(move || {
                    commits.fetch_add(1, std::sync::atomic::Ordering::SeqCst) > 0
                }))
                .unwrap();
        });

    refresh_the_list(&mut runtime, &mut terminal);

    let app = runtime.app();
    assert_eq!(
        app.conversations()[0].message_count,
        1,
        "the row is the cached copy"
    );
    assert!(
        app.is_session_refresh_requested() || app.is_awaiting_refresh(),
        "the row read from its older copy has a pending refresh"
    );
}

/// The TUI launched over `home`'s sessions with the session cache under
/// `cache`, on the system clock, once its launch finished.
fn launched_over_cache(
    home: &ClaudeHome,
    cache: &Path,
    terminal: &mut Terminal<TestBackend>,
) -> Runtime {
    let mut runtime = launched_in(home.environment_with_cache(
        cache,
        ProcessIdentity::default(),
        Arc::new(SystemClock::new()),
    ));
    tick_until(&mut runtime, terminal, |app| !app.is_loading());
    runtime
}

/// Refresh ownership of `cache` held by another rearview until dropped.
fn another_rearviews_refresh(cache: &Path) -> RefreshOwnership {
    acquired_elsewhere(another_rearview(cache, Arc::new(SystemClock::new())))
}

/// Open the selected session in the viewer.
fn open_the_selected_session(runtime: &mut Runtime, terminal: &mut Terminal<TestBackend>) {
    tick_with(
        runtime,
        terminal,
        vec![key(KeyCode::Enter, KeyModifiers::NONE)],
    );
    tick_until(runtime, terminal, |app| {
        matches!(app.app_mode(), AppMode::View(_))
    });
}

/// Delete every session `cache`'s database holds, as a cache that never
/// held them would read.
fn forget_every_cached_session(cache: &Path) {
    rusqlite::Connection::open(cache.join(DATABASE_FILE_NAME))
        .unwrap()
        .execute("DELETE FROM sessions", [])
        .unwrap();
}

/// What keeps a job of this rearview from finishing while a test looks at
/// the screen. Dropping it lets the job finish.
enum Hold {
    /// No other rearview runs; this rearview's own commit waits at a gate.
    ThisRearviewsCommit { _gate: CommitGate },
    /// Another rearview's refresh owns the cache.
    AnotherRearviewsRefresh { _ownership: RefreshOwnership },
}

impl Hold {
    fn this_rearviews_commit(cache: &Path) -> Self {
        Self::ThisRearviewsCommit {
            _gate: CommitGate::closed_on(cache),
        }
    }

    fn another_rearviews_refresh(cache: &Path) -> Self {
        Self::AnotherRearviewsRefresh {
            _ownership: another_rearviews_refresh(cache),
        }
    }

    /// True once this hold keeps `app`'s job from finishing: the job runs
    /// toward this rearview's gated commit, or publishes its wait for
    /// another rearview's refresh.
    fn is_holding(&self, app: &App) -> bool {
        match self {
            Self::ThisRearviewsCommit { .. } => app.is_refreshing(),
            Self::AnotherRearviewsRefresh { .. } => app.is_waiting_for_another_rearview(),
        }
    }
}

/// The label the viewer shows over a cached copy while `hold` keeps the
/// session's refresh from finishing.
fn updating_label(hold: &Hold) -> &'static str {
    match hold {
        Hold::ThisRearviewsCommit { .. } => "Updating…",
        Hold::AnotherRearviewsRefresh { .. } => "Another rearview is refreshing…",
    }
}

/// Open a session whose transcript changed since it was cached while `hold`
/// keeps its refresh from finishing: the viewer shows the cached copy under
/// `label`, then the newest messages once the refresh finishes.
fn assert_an_opened_changed_session_shows_its_copy_under(
    hold: impl FnOnce(&Path) -> Hold,
    label: impl Fn(&Hold) -> &'static str,
) {
    let home = ClaudeHome::new();
    let a = home.write("a", 5, &["first question"]);
    let cache = admitted_cache();
    let mut terminal = terminal();
    let mut runtime = launched_over_cache(&home, cache.path(), &mut terminal);
    home.append(&a, 6, "newest question");
    let hold = hold(cache.path());

    open_the_selected_session(&mut runtime, &mut terminal);

    assert!(
        ticks_show(
            &mut runtime,
            &mut terminal,
            Duration::from_secs(2),
            label(&hold)
        ),
        "the viewer shows {}",
        label(&hold)
    );
    let shown = shown_text(&terminal);
    assert!(
        shown.contains("first question"),
        "the viewer shows the cached copy: {shown}"
    );
    assert!(
        !shown.contains("newest question"),
        "the drawing thread doesn't read the transcript"
    );
    drop(hold);
    assert!(ticks_show(
        &mut runtime,
        &mut terminal,
        BACKGROUND_WORK_LIMIT,
        "newest question"
    ));
}

/// Opening a session whose transcript changed since it was cached shows the
/// cached copy under `Updating…`, then the newest messages once its refresh
/// finishes.
#[test]
fn opening_changed_session_shows_cached_copy_under_updating_until_its_refresh_lands() {
    assert_an_opened_changed_session_shows_its_copy_under(
        Hold::this_rearviews_commit,
        updating_label,
    );
}

/// Behind another rearview's refresh, the cached copy shows under that
/// rearview's label, and the newest messages once that refresh finishes.
#[test]
fn opening_changed_session_behind_another_rearviews_refresh_shows_copy_under_its_label() {
    assert_an_opened_changed_session_shows_its_copy_under(
        Hold::another_rearviews_refresh,
        updating_label,
    );
}

/// Open a session the cache lacks while `hold` keeps its refresh from
/// finishing: the viewer shows `label` and no message, then the session
/// once the refresh finishes.
fn assert_an_opened_uncached_session_shows(hold: impl FnOnce(&Path) -> Hold, label: &'static str) {
    let home = ClaudeHome::new();
    home.write("a", 5, &["first question"]);
    let cache = admitted_cache();
    let mut terminal = terminal();
    let mut runtime = launched_over_cache(&home, cache.path(), &mut terminal);
    forget_every_cached_session(cache.path());
    let hold = hold(cache.path());

    open_the_selected_session(&mut runtime, &mut terminal);

    assert!(
        ticks_show(&mut runtime, &mut terminal, Duration::from_secs(2), label),
        "the viewer shows {label}"
    );
    assert!(
        !shown_text(&terminal).contains("first question"),
        "the drawing thread doesn't read the transcript"
    );
    drop(hold);
    assert!(ticks_show(
        &mut runtime,
        &mut terminal,
        BACKGROUND_WORK_LIMIT,
        "first question"
    ));
}

/// A session waiting for another rearview that died during its refresh is
/// refreshed once that rearview's last commit is older than the takeover
/// window.
#[test]
fn opening_changed_session_after_another_rearview_died_mid_refresh_shows_its_newest_messages() {
    let home = ClaudeHome::new();
    let a = home.write("a", 5, &["first question"]);
    let cache = admitted_cache();
    let mut terminal = terminal();
    let mut runtime = launched_over_cache(&home, cache.path(), &mut terminal);
    home.append(&a, 6, "newest question");
    drop(OtherProcessOwner::hold(cache.path()));
    expire_last_commit(cache.path());

    open_the_selected_session(&mut runtime, &mut terminal);

    assert!(
        ticks_show(
            &mut runtime,
            &mut terminal,
            BACKGROUND_WORK_LIMIT,
            "newest question"
        ),
        "the viewer stops waiting for the rearview that died"
    );
}

/// Fail every commit that changes a session in the cache in `base` until
/// the result drops, as a failing disk or a rebuild that cannot start does.
fn fail_session_writes(base: &Path) -> test_hooks::Installed {
    test_hooks::on_every_connection(base, |connection| {
        if connection.is_readonly(rusqlite::MAIN_DB).unwrap() {
            return;
        }
        let changes_a_session = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let changed = Arc::clone(&changes_a_session);
        connection
            .update_hook(Some(
                move |_: rusqlite::hooks::Action, _: &str, table: &str, _: i64| {
                    if table != "refresh_state" {
                        changed.store(true, std::sync::atomic::Ordering::SeqCst);
                    }
                },
            ))
            .unwrap();
        connection
            .commit_hook(Some(move || {
                changes_a_session.swap(false, std::sync::atomic::Ordering::SeqCst)
            }))
            .unwrap();
    })
}

/// Typing text only the transcript holds, since the session was cached,
/// matches no row: the search worker reads the cached copy.
#[test]
fn search_for_text_newer_than_cached_copy_doesnt_match_session() {
    let home = ClaudeHome::new();
    let a = home.write("a", 5, &["first question"]);
    let cache = admitted_cache();
    let mut terminal = terminal();
    let mut runtime = launched_over_cache(&home, cache.path(), &mut terminal);
    home.append(&a, 6, "zebracorn");

    tick_with(
        &mut runtime,
        &mut terminal,
        "zebracorn"
            .chars()
            .map(|character| key(KeyCode::Char(character), KeyModifiers::NONE))
            .collect(),
    );
    ticks_reach(&mut runtime, &mut terminal, Duration::from_secs(2), |app| {
        !app.has_search_work_in_flight()
    });

    assert!(
        listed_ids(runtime.app()).is_empty(),
        "the search doesn't match text the cached copy lacks"
    );
}

/// A list refresh whose writes fail is this rearview's own: the list never
/// names another rearview.
#[test]
fn list_refresh_whose_writes_fail_doesnt_report_another_rearview() {
    let home = ClaudeHome::new();
    let a = home.write("a", 5, &["first question"]);
    let cache = admitted_cache();
    let mut terminal = terminal();
    let mut runtime = launched_over_cache(&home, cache.path(), &mut terminal);
    home.append(&a, 6, "newest question");
    let _failing = fail_session_writes(cache.path());

    tick_with(
        &mut runtime,
        &mut terminal,
        vec![key(KeyCode::Char('r'), KeyModifiers::CONTROL)],
    );

    assert!(
        !ticks_show(
            &mut runtime,
            &mut terminal,
            Duration::from_secs(3),
            "Another rearview is refreshing"
        ),
        "this rearview's failed refresh isn't reported as another rearview's"
    );
}

/// A viewer `Ctrl+R` whose writes fail keeps the session's refresh pending:
/// the copy stays under `Updating…`, and the newest messages show once
/// writes succeed again.
#[test]
fn viewer_refresh_whose_writes_fail_stays_pending_until_refresh_succeeds() {
    let home = ClaudeHome::new();
    let a = home.write("a", 5, &["first question"]);
    let cache = admitted_cache();
    let mut terminal = terminal();
    let mut runtime = launched_over_cache(&home, cache.path(), &mut terminal);
    open_the_selected_session(&mut runtime, &mut terminal);
    home.append(&a, 6, "newest question");
    let failing = fail_session_writes(cache.path());
    tick_with(
        &mut runtime,
        &mut terminal,
        vec![key(KeyCode::Char('r'), KeyModifiers::CONTROL)],
    );

    assert!(
        !ticks_reach(&mut runtime, &mut terminal, Duration::from_secs(3), |app| {
            !app.is_open_session_updating()
        }),
        "the session keeps a pending refresh after the failed refresh"
    );
    drop(failing);
    assert!(
        ticks_show(
            &mut runtime,
            &mut terminal,
            BACKGROUND_WORK_LIMIT,
            "newest question"
        ),
        "the newest messages show after writes succeed again"
    );
}

/// The text the export in
/// [`export_after_transcript_grew_waits_for_newest_messages`] put
/// on the clipboard.
static EXPORTED_AFTER_GROWTH: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());

/// An export asked for after the open session's transcript grew, with no
/// refresh pending, waits for the session's refresh: it holds the newest
/// messages.
#[test]
fn export_after_transcript_grew_waits_for_newest_messages() {
    let home = ClaudeHome::new();
    let a = home.write("a", 5, &["first question"]);
    let cache = admitted_cache();
    let mut terminal = terminal();
    let mut runtime = launched_over_cache(&home, cache.path(), &mut terminal);
    runtime.app_mut().set_clipboard_writer_for_test(|text| {
        *EXPORTED_AFTER_GROWTH.lock().unwrap() = text.to_owned();
        Ok(crate::tui::export::ClipboardDestination::System)
    });
    open_the_selected_session(&mut runtime, &mut terminal);
    home.append(&a, 6, "newest question");

    tick_with(
        &mut runtime,
        &mut terminal,
        vec![
            key(KeyCode::Char('y'), KeyModifiers::NONE),
            key(KeyCode::Char('2'), KeyModifiers::NONE),
        ],
    );
    tick_until(&mut runtime, &mut terminal, |_| {
        !EXPORTED_AFTER_GROWTH.lock().unwrap().is_empty()
    });

    assert!(
        EXPORTED_AFTER_GROWTH
            .lock()
            .unwrap()
            .contains("newest question"),
        "the export doesn't copy the stale copy"
    );
}

/// The TUI showing the file at `path` opened directly, in `environment`.
fn opened_directly(path: &Path, environment: Environment) -> Runtime {
    let app = App::new_single_file(
        path.to_path_buf(),
        ToolDisplayMode::Hidden,
        false,
        KeyBindings::default(),
        environment.clone(),
    );
    Runtime::single_file(app, environment)
}

/// A session its agent stores, opened directly while the cache lacks it,
/// shows `Loading…` until its session refresh caches it, then its messages;
/// `Ctrl+R` shows the newest messages.
#[test]
fn stored_session_opened_directly_shows_loading_then_its_messages_from_cache() {
    let home = ClaudeHome::new();
    let a = home.write("a", 5, &["first question"]);
    let cache = admitted_cache();
    let gate = CommitGate::closed_on(cache.path());
    let mut terminal = terminal();
    let mut runtime = opened_directly(
        &a,
        home.environment_with_cache(
            cache.path(),
            ProcessIdentity::default(),
            Arc::new(SystemClock::new()),
        ),
    );

    assert!(
        ticks_show(
            &mut runtime,
            &mut terminal,
            Duration::from_secs(2),
            "Loading…"
        ),
        "the file opened directly shows Loading…"
    );
    assert!(
        !shown_text(&terminal).contains("first question"),
        "the drawing thread doesn't read the transcript"
    );
    drop(gate);
    assert!(
        ticks_show(
            &mut runtime,
            &mut terminal,
            BACKGROUND_WORK_LIMIT,
            "first question"
        ),
        "the file opened directly shows its messages"
    );
    assert_eq!(
        cached_session_count(cache.path()),
        1,
        "the cache holds the session opened directly"
    );
    home.append(&a, 6, "newest question");
    tick_with(
        &mut runtime,
        &mut terminal,
        vec![key(KeyCode::Char('r'), KeyModifiers::CONTROL)],
    );
    assert!(
        ticks_show(
            &mut runtime,
            &mut terminal,
            BACKGROUND_WORK_LIMIT,
            "newest question"
        ),
        "Ctrl+R shows the newest message"
    );
}

/// A file outside every provider's roots, opened directly, shows its
/// messages once the background worker reads it, and stays out of the cache.
#[test]
fn file_outside_every_root_opened_directly_shows_its_messages_without_caching_them() {
    let home = ClaudeHome::new();
    let elsewhere = tempfile::tempdir().unwrap();
    let copy = elsewhere.path().join("copy.jsonl");
    std::fs::copy(home.write("a", 5, &["first question"]), &copy).unwrap();
    let cache = admitted_cache();
    let mut terminal = terminal();
    let mut runtime = opened_directly(
        &copy,
        home.environment_with_cache(
            cache.path(),
            ProcessIdentity::default(),
            Arc::new(SystemClock::new()),
        ),
    );

    assert!(
        ticks_show(
            &mut runtime,
            &mut terminal,
            BACKGROUND_WORK_LIMIT,
            "first question"
        ),
        "the file opened directly shows its messages"
    );
    assert_eq!(
        cached_session_count(cache.path()),
        0,
        "a file outside every root isn't cached"
    );
}

/// Opening a session the cache lacks shows `Loading…` until its refresh
/// finishes.
#[test]
fn opening_session_cache_lacks_shows_loading_until_its_refresh_lands() {
    assert_an_opened_uncached_session_shows(Hold::this_rearviews_commit, "Loading…");
}

/// Behind another rearview's refresh, a session the cache lacks shows that
/// rearview's label until that refresh finishes.
#[test]
fn opening_session_cache_lacks_behind_another_rearviews_refresh_shows_its_label() {
    assert_an_opened_uncached_session_shows(
        Hold::another_rearviews_refresh,
        "Another rearview is refreshing…",
    );
}

/// `Ctrl+R` in the viewer while `hold` keeps the session's refresh from
/// finishing: the copy on screen stays under the label, then the newest
/// messages show once the refresh finishes.
fn assert_ctrl_r_in_the_viewer_shows_the_newest_messages(hold: impl FnOnce(&Path) -> Hold) {
    let home = ClaudeHome::new();
    let a = home.write("a", 5, &["first question"]);
    let cache = admitted_cache();
    let mut terminal = terminal();
    let mut runtime = launched_over_cache(&home, cache.path(), &mut terminal);
    open_the_selected_session(&mut runtime, &mut terminal);
    home.append(&a, 6, "newest question");
    let hold = hold(cache.path());

    tick_with(
        &mut runtime,
        &mut terminal,
        vec![key(KeyCode::Char('r'), KeyModifiers::CONTROL)],
    );

    let label = updating_label(&hold);
    assert!(
        ticks_show(&mut runtime, &mut terminal, Duration::from_secs(2), label),
        "the viewer shows {label}"
    );
    assert!(
        !shown_text(&terminal).contains("newest question"),
        "the drawing thread doesn't read the transcript"
    );
    drop(hold);
    assert!(ticks_show(
        &mut runtime,
        &mut terminal,
        BACKGROUND_WORK_LIMIT,
        "newest question"
    ));
}

/// `Ctrl+R` in the viewer requests the session's refresh: the copy on screen
/// stays under `Updating…`, then the newest messages show once it finishes.
#[test]
fn ctrl_r_in_viewer_shows_newest_messages_once_sessions_refresh_lands() {
    assert_ctrl_r_in_the_viewer_shows_the_newest_messages(Hold::this_rearviews_commit);
}

/// Behind another rearview's refresh, the copy stays under that rearview's
/// label until that refresh finishes.
#[test]
fn ctrl_r_in_viewer_behind_another_rearviews_refresh_shows_its_label_then_newest_messages() {
    assert_ctrl_r_in_the_viewer_shows_the_newest_messages(Hold::another_rearviews_refresh);
}

/// A copy asked for while `hold` keeps the open session's refresh from
/// finishing waits for the refresh, so it holds the newest messages. `copied`
/// is the test's own clipboard.
fn assert_a_copy_while_updating_holds_the_newest_messages(
    hold: impl FnOnce(&Path) -> Hold,
    copied: &'static std::sync::Mutex<String>,
    writer: fn(&str) -> std::result::Result<crate::tui::export::ClipboardDestination, String>,
) {
    let home = ClaudeHome::new();
    let a = home.write("a", 5, &["first question"]);
    let cache = admitted_cache();
    let mut terminal = terminal();
    let mut runtime = launched_over_cache(&home, cache.path(), &mut terminal);
    runtime.app_mut().set_clipboard_writer_for_test(writer);
    home.append(&a, 6, "newest question");
    let hold = hold(cache.path());
    open_the_selected_session(&mut runtime, &mut terminal);

    tick_with(
        &mut runtime,
        &mut terminal,
        vec![
            key(KeyCode::Char('y'), KeyModifiers::NONE),
            key(KeyCode::Char('2'), KeyModifiers::NONE),
        ],
    );

    assert!(
        copied.lock().unwrap().is_empty(),
        "the copy waits for the session's refresh"
    );
    drop(hold);
    tick_until(&mut runtime, &mut terminal, |_| {
        !copied.lock().unwrap().is_empty()
    });
    assert!(
        copied.lock().unwrap().contains("newest question"),
        "the copy holds the newest message"
    );
}

/// The text the copy in [`copy_asked_for_while_session_updates_holds_its_newest_messages`]
/// put on the clipboard.
static COPIED_WHILE_UPDATING: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());

/// The text the copy in
/// [`copy_asked_for_behind_another_rearviews_refresh_holds_its_newest_messages`]
/// put on the clipboard.
static COPIED_BEHIND_ANOTHER_REARVIEW: std::sync::Mutex<String> =
    std::sync::Mutex::new(String::new());

/// A copy asked for while the open session updates waits for its refresh,
/// so it holds the newest messages.
#[test]
fn copy_asked_for_while_session_updates_holds_its_newest_messages() {
    assert_a_copy_while_updating_holds_the_newest_messages(
        Hold::this_rearviews_commit,
        &COPIED_WHILE_UPDATING,
        |text| {
            *COPIED_WHILE_UPDATING.lock().unwrap() = text.to_owned();
            Ok(crate::tui::export::ClipboardDestination::System)
        },
    );
}

/// Behind another rearview's refresh, the copy waits for that refresh too.
#[test]
fn copy_asked_for_behind_another_rearviews_refresh_holds_its_newest_messages() {
    assert_a_copy_while_updating_holds_the_newest_messages(
        Hold::another_rearviews_refresh,
        &COPIED_BEHIND_ANOTHER_REARVIEW,
        |text| {
            *COPIED_BEHIND_ANOTHER_REARVIEW.lock().unwrap() = text.to_owned();
            Ok(crate::tui::export::ClipboardDestination::System)
        },
    );
}

/// Type the ID of a session written after the launch while `hold` keeps its
/// refresh from finishing: the list shows `Loading…`, then lists the session
/// once the refresh finishes.
fn assert_an_id_lookup_lists_the_session_once_its_refresh_lands(hold: impl FnOnce(&Path) -> Hold) {
    const LATER: &str = "0f000000-0000-4000-8000-0000000000aa";
    let home = ClaudeHome::new();
    home.write("0f000000-0000-4000-8000-000000000001", 5, &["first"]);
    let cache = admitted_cache();
    let mut terminal = terminal();
    let mut runtime = launched_over_cache(&home, cache.path(), &mut terminal);
    home.write(LATER, 10, &["written after the launch"]);
    let hold = hold(cache.path());

    tick_with(
        &mut runtime,
        &mut terminal,
        LATER
            .chars()
            .map(|character| key(KeyCode::Char(character), KeyModifiers::NONE))
            .collect(),
    );

    assert!(ticks_show(
        &mut runtime,
        &mut terminal,
        Duration::from_secs(2),
        "Loading…"
    ));
    assert!(
        !listed_ids(runtime.app()).contains(&LATER),
        "the session isn't listed before its refresh finishes"
    );
    drop(hold);
    assert!(
        ticks_reach(&mut runtime, &mut terminal, BACKGROUND_WORK_LIMIT, |app| {
            listed_ids(app) == [LATER]
        }),
        "the session found by ID is listed"
    );
}

/// A session found by ID that the cache lacks shows `Loading…`, and is
/// listed once the list applies its refresh.
#[test]
fn id_lookup_of_session_cache_lacks_lists_it_once_its_refresh_lands() {
    assert_an_id_lookup_lists_the_session_once_its_refresh_lands(Hold::this_rearviews_commit);
}

/// Behind another rearview's refresh, the session found by ID is listed once
/// that refresh finishes.
#[test]
fn id_lookup_behind_another_rearviews_refresh_lists_session_once_it_finishes() {
    assert_an_id_lookup_lists_the_session_once_its_refresh_lands(Hold::another_rearviews_refresh);
}

/// The actions menu's row for `action`, as drawn.
fn menu_row<'a>(shown_rows: &'a [String], action: &str) -> &'a str {
    shown_rows
        .iter()
        .find(|row| row.contains(action))
        .unwrap_or_else(|| panic!("the menu shows a {action} row:\n{}", shown_rows.join("\n")))
}

/// Every row `terminal` last drew.
fn shown_rows(terminal: &Terminal<TestBackend>) -> Vec<String> {
    let buffer = terminal.backend().buffer();
    let width = usize::from(buffer.area.width);
    buffer
        .content()
        .chunks(width)
        .map(|row| row.iter().map(|cell| cell.symbol()).collect())
        .collect()
}

/// Press `Ctrl+X` and return the rows the actions menu draws.
fn actions_menu_rows(runtime: &mut Runtime, terminal: &mut Terminal<TestBackend>) -> Vec<String> {
    tick_with(
        runtime,
        terminal,
        vec![key(KeyCode::Char('x'), KeyModifiers::CONTROL)],
    );
    tick_with(runtime, terminal, Vec::new());
    shown_rows(terminal)
}

/// During the launch's own reads and writes, the actions menu greys Delete
/// and Rename with the reason, and the rename key reports it.
#[test]
fn rename_and_delete_are_unavailable_during_launch_with_reason() {
    let home = ClaudeHome::new();
    home.write("a", 5, &["a"]);
    let cache = admitted_cache();
    let gate = CommitGate::closed_on(cache.path());
    let mut runtime = launched_in(home.environment_with_cache(
        cache.path(),
        ProcessIdentity::default(),
        Arc::new(SystemClock::new()),
    ));
    let mut terminal = terminal();
    assert!(runtime.app().is_loading());

    let rows = actions_menu_rows(&mut runtime, &mut terminal);

    assert!(menu_row(&rows, "Delete…").contains("list loading"));
    assert!(menu_row(&rows, "Rename…").contains("list loading"));
    tick_with(
        &mut runtime,
        &mut terminal,
        vec![
            key(KeyCode::Esc, KeyModifiers::NONE),
            key(KeyCode::F(2), KeyModifiers::NONE),
        ],
    );
    assert!(ticks_show(
        &mut runtime,
        &mut terminal,
        Duration::from_secs(1),
        "Rename is unavailable while the list loads"
    ));
    drop(gate);
    tick_until(&mut runtime, &mut terminal, |app| !app.is_loading());
}

/// While the launch waits for another rearview's rebuild, the actions menu
/// doesn't grey Rename and Delete with the launch's reason. The rebuild's
/// lock keeps every row out of the list, so neither action has a session to
/// act on.
#[test]
fn rename_and_delete_dont_wait_while_launch_waits_for_another_rearviews_rebuild() {
    let home = ClaudeHome::new();
    home.write("a", 5, &["a"]);
    let cache = admitted_cache();
    let rebuild = exclusive_lock(cache.path());
    let mut runtime = launched_in(home.environment_with_cache(
        cache.path(),
        ProcessIdentity::default(),
        Arc::new(TestClock::default()),
    ));
    let mut terminal = terminal();
    assert!(ticks_show(
        &mut runtime,
        &mut terminal,
        Duration::from_secs(2),
        "Another rearview is rebuilding"
    ));

    let rows = actions_menu_rows(&mut runtime, &mut terminal);

    assert!(
        !menu_row(&rows, "Rename…").contains("list loading"),
        "Rename waits for another rearview's rebuild"
    );
    assert!(
        !menu_row(&rows, "Delete…").contains("list loading"),
        "Delete waits for another rearview's rebuild"
    );
    drop(rebuild);
    tick_until(&mut runtime, &mut terminal, |app| !app.is_loading());
}

/// While the launch waits for another rearview's refresh, Rename and Delete
/// of a session it lists from the cache stay available.
#[test]
fn rename_and_delete_stay_available_while_launch_waits_for_another_rearviews_refresh() {
    let home = ClaudeHome::new();
    let a = home.write("a", 5, &["a"]);
    let cache = admitted_cache();
    let environment = home.environment_with_cache(
        cache.path(),
        ProcessIdentity::default(),
        Arc::new(TestClock::default()),
    );
    home.rows(&environment);
    home.append(&a, 6, "written after the cache");
    let other_refresh = another_rearviews_refresh(cache.path());
    let mut runtime = launched_in(environment);
    let mut terminal = terminal();
    assert!(
        ticks_reach(&mut runtime, &mut terminal, Duration::from_secs(2), |app| {
            app.is_waiting_for_another_rearview() && listed_ids(app) == ["a"]
        }),
        "the launch lists the cached session while it waits for another rearview's refresh"
    );

    actions_menu_rows(&mut runtime, &mut terminal);

    assert!(
        runtime.app().is_action_available(SessionAction::Rename),
        "Rename waits for another rearview's refresh"
    );
    assert!(
        runtime.app().is_action_available(SessionAction::Delete),
        "Delete waits for another rearview's refresh"
    );
    drop(other_refresh);
    tick_until(&mut runtime, &mut terminal, |app| !app.is_loading());
}

/// Press `Ctrl+R` in the list while `hold` keeps the list refresh from
/// finishing, and return the rows of the actions menu drawn meanwhile.
fn actions_menu_during_a_list_refresh(hold: impl FnOnce(&Path) -> Hold) -> Vec<String> {
    let home = ClaudeHome::new();
    let a = home.write("a", 5, &["a"]);
    let cache = admitted_cache();
    let mut terminal = terminal();
    let mut runtime = launched_over_cache(&home, cache.path(), &mut terminal);
    home.append(&a, 6, "written after the launch");
    let hold = hold(cache.path());
    tick_with(
        &mut runtime,
        &mut terminal,
        vec![key(KeyCode::Char('r'), KeyModifiers::CONTROL)],
    );
    tick_until(&mut runtime, &mut terminal, |app| hold.is_holding(app));

    let rows = actions_menu_rows(&mut runtime, &mut terminal);
    drop(hold);
    rows
}

/// During a list `Ctrl+R`'s own reads and writes, the actions menu greys
/// Delete and Rename with the reason.
#[test]
fn rename_and_delete_are_unavailable_during_list_refresh_with_reason() {
    let rows = actions_menu_during_a_list_refresh(Hold::this_rearviews_commit);

    assert!(menu_row(&rows, "Delete…").contains("list refreshing"));
    assert!(menu_row(&rows, "Rename…").contains("list refreshing"));
}

/// While a list `Ctrl+R` waits for another rearview's refresh, Rename and
/// Delete stay available.
#[test]
fn rename_and_delete_stay_available_while_list_refresh_waits_for_another_rearview() {
    let rows = actions_menu_during_a_list_refresh(Hold::another_rearviews_refresh);

    assert!(
        !menu_row(&rows, "Delete…").contains("refreshing"),
        "Delete waits for another rearview's refresh"
    );
    assert!(
        !menu_row(&rows, "Rename…").contains("refreshing"),
        "Rename waits for another rearview's refresh"
    );
}

/// The actions menu of the open session while `hold` keeps that session's
/// refresh from finishing, then of the other listed session.
fn actions_menus_during_a_session_refresh(
    hold: impl FnOnce(&Path) -> Hold,
) -> (Vec<String>, Vec<String>) {
    let home = ClaudeHome::new();
    home.write("b", 5, &["b"]);
    let a = home.write("a", 10, &["a"]);
    let cache = admitted_cache();
    let mut terminal = terminal();
    let mut runtime = launched_over_cache(&home, cache.path(), &mut terminal);
    open_the_selected_session(&mut runtime, &mut terminal);
    home.append(&a, 11, "written after the launch");
    let hold = hold(cache.path());
    tick_with(
        &mut runtime,
        &mut terminal,
        vec![key(KeyCode::Char('r'), KeyModifiers::CONTROL)],
    );
    tick_until(&mut runtime, &mut terminal, |app| hold.is_holding(app));

    let refreshing = actions_menu_rows(&mut runtime, &mut terminal);
    tick_with(
        &mut runtime,
        &mut terminal,
        vec![
            key(KeyCode::Esc, KeyModifiers::NONE),
            key(KeyCode::Char('q'), KeyModifiers::NONE),
            key(KeyCode::Down, KeyModifiers::NONE),
        ],
    );
    let other = actions_menu_rows(&mut runtime, &mut terminal);
    drop(hold);
    (refreshing, other)
}

/// During a session's own refresh, only that session's rename and delete
/// wait.
#[test]
fn only_refreshing_sessions_rename_and_delete_wait_during_its_refresh() {
    let (refreshing, other) = actions_menus_during_a_session_refresh(Hold::this_rearviews_commit);

    assert!(menu_row(&refreshing, "Rename…").contains("refreshing"));
    assert!(menu_row(&refreshing, "Delete…").contains("refreshing"));
    assert!(
        !menu_row(&other, "Rename…").contains("refreshing"),
        "another session's rename waits for this session's refresh"
    );
}

/// While a session's refresh waits for another rearview's refresh, that
/// session's rename and delete stay available.
#[test]
fn sessions_rename_and_delete_stay_available_while_its_refresh_waits_for_another_rearview() {
    let (refreshing, _) = actions_menus_during_a_session_refresh(Hold::another_rearviews_refresh);

    assert!(
        !menu_row(&refreshing, "Rename…").contains("refreshing"),
        "Rename waits for another rearview's refresh"
    );
    assert!(
        !menu_row(&refreshing, "Delete…").contains("refreshing"),
        "Delete waits for another rearview's refresh"
    );
}

/// The actions menu of the open session once its refresh finished, its
/// outcome waiting for the viewer to close, while `meanwhile`, started on
/// the cache after that refresh, holds.
fn actions_menu_after_a_refresh_behind_the_viewer<T>(
    meanwhile: impl FnOnce(&Path) -> T,
) -> Vec<String> {
    let home = ClaudeHome::new();
    let a = home.write("a", 5, &["a"]);
    let cache = admitted_cache();
    let mut terminal = terminal();
    let mut runtime = launched_over_cache(&home, cache.path(), &mut terminal);
    open_the_selected_session(&mut runtime, &mut terminal);
    home.append(&a, 6, "written after the launch");
    tick_with(
        &mut runtime,
        &mut terminal,
        vec![key(KeyCode::Char('r'), KeyModifiers::CONTROL)],
    );
    assert!(ticks_show(
        &mut runtime,
        &mut terminal,
        BACKGROUND_WORK_LIMIT,
        "written after the launch"
    ));
    let _held = meanwhile(cache.path());

    actions_menu_rows(&mut runtime, &mut terminal)
}

/// A finished session refresh whose outcome waits for the viewer to close
/// leaves the session's rename and delete available.
#[test]
fn finished_refresh_waiting_for_viewer_to_close_leaves_rename_and_delete_available() {
    let rows = actions_menu_after_a_refresh_behind_the_viewer(|_| ());

    assert!(
        !menu_row(&rows, "Rename…").contains("refreshing"),
        "Rename waits for an outcome the viewer holds back"
    );
    assert!(
        !menu_row(&rows, "Delete…").contains("refreshing"),
        "Delete waits for an outcome the viewer holds back"
    );
}

/// A finished session refresh whose outcome waits for the viewer to close,
/// with another rearview refreshing, leaves the session's rename and delete
/// available.
#[test]
fn finished_refresh_behind_viewer_and_another_rearviews_refresh_leave_rename_and_delete_available()
{
    let rows = actions_menu_after_a_refresh_behind_the_viewer(another_rearviews_refresh);

    assert!(
        !menu_row(&rows, "Rename…").contains("refreshing"),
        "Rename waits for an outcome the viewer holds back"
    );
    assert!(
        !menu_row(&rows, "Delete…").contains("refreshing"),
        "Delete waits for an outcome the viewer holds back"
    );
}

/// Another rearview's refresh alone blocks neither rename nor delete.
#[test]
fn another_rearviews_refresh_doesnt_block_rename_or_delete() {
    let home = ClaudeHome::new();
    home.write("a", 5, &["a"]);
    let cache = admitted_cache();
    let mut terminal = terminal();
    let mut runtime = launched_over_cache(&home, cache.path(), &mut terminal);
    let other_refresh = another_rearviews_refresh(cache.path());

    actions_menu_rows(&mut runtime, &mut terminal);

    assert!(runtime.app().is_action_available(SessionAction::Rename));
    assert!(runtime.app().is_action_available(SessionAction::Delete));
    drop(other_refresh);
}
