//! The TUI's event loop body against a test backend, fed the terminal events
//! a test scripts.

use super::*;
use crate::clock::{SystemClock, TestClock};
use crate::history::sqlite_cache::DATABASE_FILE_NAME;
use crate::history::sqlite_cache::connection::test_hooks;
use crate::history::sqlite_cache::refresh::RefreshOwnership;
use crate::history::sqlite_cache::refresh::tests::{exclusive_lock, writer_lock_held};
use crate::history::{CacheBase, InteractiveCache, ProcessIdentity};
use crate::time_filter::TimeFilter;
use crate::tui::app::AppMode;
use crate::tui::app::claude_home::{ClaudeHome, admitted_cache, another_rearview};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::backend::TestBackend;
use std::path::Path;
use std::time::Instant;

/// The longest a test ticks while the load or a refresh runs.
const BACKGROUND_WORK_LIMIT: Duration = Duration::from_secs(10);

/// The TUI launched over `home`'s sessions without a session cache: the
/// list, filled by the streaming load.
fn launched_over(home: &ClaudeHome) -> Runtime {
    launched_in(home.environment())
}

/// The TUI launched over the sessions `environment` reads.
fn launched_in(environment: Environment) -> Runtime {
    let load_meter = Arc::new(LoadMeter::default());
    let loader = crate::history::load_all_conversations_streaming(
        environment.clone(),
        false,
        None,
        TimeFilter::default(),
        Arc::clone(&load_meter),
    );
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
        loader,
        load_meter,
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

#[test]
fn ticks_list_the_launch_load_and_esc_quits() {
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
    std::thread::spawn(move || RefreshOwnership::acquire(&base, || ()).unwrap())
        .join()
        .unwrap()
}

#[test]
fn ctrl_r_lists_a_session_written_after_the_launch() {
    let home = ClaudeHome::new();
    home.write("a", 5, &["a"]);
    let mut runtime = launched_over(&home);
    let mut terminal = terminal();
    tick_until(&mut runtime, &mut terminal, |app| !app.is_loading());
    home.write("b", 10, &["b"]);

    tick_with(
        &mut runtime,
        &mut terminal,
        vec![key(KeyCode::Char('r'), KeyModifiers::CONTROL)],
    );
    tick_until(&mut runtime, &mut terminal, |app| !app.is_refreshing());

    assert_eq!(listed_ids(runtime.app()), ["b", "a"]);
}

/// Eighth-pass finding (Claude F1): the launch waited behind another
/// rearview's rebuild without the waiting label.
#[test]
#[ignore = "fixed in step 4"]
fn a_launch_behind_another_rearviews_rebuild_shows_the_wait_at_once() {
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

    let showed_the_wait = ticks_reach(
        &mut runtime,
        &mut terminal,
        Duration::from_secs(2),
        App::is_waiting_for_another_rearview,
    );
    drop(rebuild);
    tick_until(&mut runtime, &mut terminal, |app| !app.is_loading());

    assert!(
        showed_the_wait,
        "the launch waited without showing that another rearview holds the cache"
    );
}

/// Eighth-pass findings (Claude F2 and F3, Codex F2): a load behind another
/// rearview's refresh gave up after its lock wait and parsed the transcripts
/// itself, without caching them.
#[test]
#[ignore = "fixed in step 4"]
fn a_launch_behind_another_rearviews_long_write_waits_for_it_instead_of_reading_transcripts() {
    let home = ClaudeHome::new();
    home.write("a", 5, &["a"]);
    let cache = admitted_cache();
    let clock: Arc<dyn Clock> = Arc::new(TestClock::default());
    let other_refresh =
        RefreshOwnership::acquire(&another_rearview(cache.path(), clock.clone()), || ()).unwrap();
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

/// Eighth-pass finding (Claude F4): quitting left this process's refresh
/// ownership in the database, so other rearviews waited up to 15 seconds.
#[test]
#[ignore = "fixed in step 5"]
fn quitting_releases_the_refresh_ownership_this_process_holds() {
    let home = ClaudeHome::new();
    home.write("a", 5, &["a"]);
    let cache = admitted_cache();
    let clock: Arc<dyn Clock> = Arc::new(TestClock::default());
    let environment =
        home.environment_with_cache(cache.path(), ProcessIdentity::default(), clock.clone());
    let cache_base = environment.cache_base().unwrap().clone();
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
        InteractiveCache::at(another_rearview(cache.path(), clock)).running_refresh(),
        None,
        "another rearview doesn't find this process's refresh running"
    );
    assert!(!held_by_a_background_thread.token().can_publish());
}

/// Eighth-pass findings (Claude F5, Codex D1): `Ctrl+R` pressed while the
/// background thread cached a session was dropped.
#[test]
#[ignore = "fixed in step 5"]
fn ctrl_r_pressed_while_a_session_caches_in_the_background_runs_after_it() {
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
    assert!(runtime.app().is_caching_queued());
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

/// Eighth-pass finding (Claude F6): a list `Ctrl+R` that could not write the
/// cache showed rows read from their transcripts and never read them again.
#[test]
fn a_list_refresh_whose_cache_write_fails_keeps_its_rows_pending_a_refresh() {
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

    tick_with(
        &mut runtime,
        &mut terminal,
        vec![key(KeyCode::Char('r'), KeyModifiers::CONTROL)],
    );
    tick_until(&mut runtime, &mut terminal, |app| !app.is_refreshing());

    let app = runtime.app();
    assert_eq!(app.conversations()[0].message_count, 2);
    assert!(
        app.is_caching_queued() || app.is_awaiting_refresh(),
        "the row read outside the database has no pending refresh"
    );
}
