//! The TUI's event loop body against a test backend, fed the terminal events
//! a test scripts.

use super::*;
use crate::clock::{SystemClock, TestClock};
use crate::history::sqlite_cache::DATABASE_FILE_NAME;
use crate::history::sqlite_cache::connection::test_hooks;
use crate::history::sqlite_cache::refresh::RefreshOwnership;
use crate::history::sqlite_cache::refresh::tests::{exclusive_lock, writer_lock_held};
use crate::history::{CacheBase, InteractiveCache, ProcessIdentity};
use crate::tui::SessionAction;
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
fn ctrl_r_lists_a_session_written_after_the_launch() {
    let home = ClaudeHome::new();
    home.write("a", 5, &["a"]);
    let mut runtime = launched_over(&home);
    let mut terminal = terminal();
    tick_until(&mut runtime, &mut terminal, |app| !app.is_loading());
    home.write("b", 10, &["b"]);

    refresh_the_list(&mut runtime, &mut terminal);

    assert_eq!(listed_ids(runtime.app()), ["b", "a"]);
}

/// Rule 8: a launch whose cache write finds less than 1 GB free on the
/// cache's drive leaves the TUI with the free space, and the cache database
/// is deleted.
#[test]
fn a_launch_whose_cache_write_finds_less_than_a_gigabyte_free_stops_with_the_free_space() {
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

/// Eighth-pass finding (Claude F1): the launch waited behind another
/// rearview's rebuild without the waiting label.
#[test]
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

/// Rule 3: during another rearview's ordinary refresh, the launch lists the
/// cache as it is under the waiting label, then reads what that refresh left
/// out once it completes.
#[test]
fn a_launch_behind_another_rearviews_refresh_lists_the_cache_as_it_is_under_the_label() {
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
        "the launch listed nothing while another rearview refreshed"
    );
    let shown = shown_text(&terminal);
    let cached_message_count = runtime.app().conversations()[1].message_count;
    drop(other_refresh);
    tick_until(&mut runtime, &mut terminal, |app| !app.is_loading());

    assert!(shown.contains("Another rearview is refreshing…"), "{shown}");
    assert_eq!(cached_message_count, 1);
    assert_eq!(runtime.app().conversations()[1].message_count, 2);
}

/// Eighth-pass findings (Claude F2 and F3, Codex F2): a load behind another
/// rearview's refresh gave up after its lock wait and parsed the transcripts
/// itself, without caching them.
#[test]
fn a_launch_behind_another_rearviews_long_write_waits_for_it_instead_of_reading_transcripts() {
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

/// Rule 5: a list `Ctrl+R` pressed during the launch is queued, and runs
/// once the launch finishes.
#[test]
fn ctrl_r_pressed_during_the_launch_runs_after_it() {
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

/// Rule 5: a list `Ctrl+R` waiting for another rearview's refresh when the
/// TUI quits takes no ownership once that refresh ends.
#[test]
fn a_list_refresh_waiting_at_quit_doesnt_refresh_once_the_other_rearview_finishes() {
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

/// Eighth-pass finding (Claude F4): quitting left this process's refresh
/// ownership in the database, so other rearviews waited up to 15 seconds.
#[test]
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
        InteractiveCache::at(another_rearview(cache.path(), clock)).awaited_refresh(),
        None,
        "another rearview doesn't find this process's refresh running"
    );
    assert!(!held_by_a_background_thread.token().can_publish());
}

/// Eighth-pass findings (Claude F5, Codex D1): `Ctrl+R` pressed while the
/// background thread cached a session was dropped.
#[test]
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

    refresh_the_list(&mut runtime, &mut terminal);

    let app = runtime.app();
    assert_eq!(app.conversations()[0].message_count, 2);
    assert!(
        app.is_session_refresh_requested() || app.is_awaiting_refresh(),
        "the row read outside the database has no pending refresh"
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

/// Rule 4: opening a session whose transcript changed since it was cached
/// shows the cached copy under `Updating…`, then the newest messages once
/// its refresh lands.
#[test]
fn opening_a_changed_session_shows_the_cached_copy_until_its_refresh_lands() {
    let home = ClaudeHome::new();
    let a = home.write("a", 5, &["first question"]);
    let cache = admitted_cache();
    let mut terminal = terminal();
    let mut runtime = launched_over_cache(&home, cache.path(), &mut terminal);
    home.append(&a, 6, "newest question");
    let other_refresh = another_rearviews_refresh(cache.path());

    open_the_selected_session(&mut runtime, &mut terminal);

    assert!(ticks_show(
        &mut runtime,
        &mut terminal,
        Duration::from_secs(2),
        "Updating…"
    ));
    let shown = shown_text(&terminal);
    assert!(shown.contains("first question"), "{shown}");
    assert!(
        !shown.contains("newest question"),
        "the drawing thread read the transcript"
    );
    drop(other_refresh);
    assert!(ticks_show(
        &mut runtime,
        &mut terminal,
        BACKGROUND_WORK_LIMIT,
        "newest question"
    ));
}

/// Rule 4: opening a session the cache lacks shows `Loading…` until its
/// refresh lands.
#[test]
fn opening_a_session_the_cache_lacks_shows_loading_until_its_refresh_lands() {
    let home = ClaudeHome::new();
    home.write("a", 5, &["first question"]);
    let cache = admitted_cache();
    let mut terminal = terminal();
    let mut runtime = launched_over_cache(&home, cache.path(), &mut terminal);
    forget_every_cached_session(cache.path());
    let other_refresh = another_rearviews_refresh(cache.path());

    open_the_selected_session(&mut runtime, &mut terminal);

    assert!(ticks_show(
        &mut runtime,
        &mut terminal,
        Duration::from_secs(2),
        "Loading…"
    ));
    assert!(
        !shown_text(&terminal).contains("first question"),
        "the drawing thread read the transcript"
    );
    drop(other_refresh);
    assert!(ticks_show(
        &mut runtime,
        &mut terminal,
        BACKGROUND_WORK_LIMIT,
        "first question"
    ));
}

/// Rule 4: `Ctrl+R` in the viewer requests the session's refresh: the copy
/// on screen stays under `Updating…`, then the newest messages show once it
/// lands.
#[test]
fn ctrl_r_in_the_viewer_shows_the_newest_messages_once_the_sessions_refresh_lands() {
    let home = ClaudeHome::new();
    let a = home.write("a", 5, &["first question"]);
    let cache = admitted_cache();
    let mut terminal = terminal();
    let mut runtime = launched_over_cache(&home, cache.path(), &mut terminal);
    open_the_selected_session(&mut runtime, &mut terminal);
    home.append(&a, 6, "newest question");
    let other_refresh = another_rearviews_refresh(cache.path());

    tick_with(
        &mut runtime,
        &mut terminal,
        vec![key(KeyCode::Char('r'), KeyModifiers::CONTROL)],
    );

    assert!(ticks_show(
        &mut runtime,
        &mut terminal,
        Duration::from_secs(2),
        "Updating…"
    ));
    assert!(
        !shown_text(&terminal).contains("newest question"),
        "the drawing thread read the transcript"
    );
    drop(other_refresh);
    assert!(ticks_show(
        &mut runtime,
        &mut terminal,
        BACKGROUND_WORK_LIMIT,
        "newest question"
    ));
}

/// The text the copy in [`a_copy_asked_for_while_the_session_updates_holds_its_newest_messages`]
/// put on the clipboard.
static COPIED_WHILE_UPDATING: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());

/// Rule 4: a copy asked for while the open session updates waits for its
/// refresh, so it holds the newest messages.
#[test]
fn a_copy_asked_for_while_the_session_updates_holds_its_newest_messages() {
    let home = ClaudeHome::new();
    let a = home.write("a", 5, &["first question"]);
    let cache = admitted_cache();
    let mut terminal = terminal();
    let mut runtime = launched_over_cache(&home, cache.path(), &mut terminal);
    runtime.app_mut().set_clipboard_writer_for_test(|text| {
        *COPIED_WHILE_UPDATING.lock().unwrap() = text.to_owned();
        Ok(crate::tui::export::ClipboardDestination::System)
    });
    home.append(&a, 6, "newest question");
    let other_refresh = another_rearviews_refresh(cache.path());
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
        COPIED_WHILE_UPDATING.lock().unwrap().is_empty(),
        "the copy waits for the session's refresh"
    );
    drop(other_refresh);
    tick_until(&mut runtime, &mut terminal, |_| {
        !COPIED_WHILE_UPDATING.lock().unwrap().is_empty()
    });
    assert!(
        COPIED_WHILE_UPDATING
            .lock()
            .unwrap()
            .contains("newest question")
    );
}

/// Rule 4: a session found by ID that the cache lacks shows `Loading…`, and
/// is listed once its refresh lands.
#[test]
fn an_id_lookup_of_a_session_the_cache_lacks_lists_it_once_its_refresh_lands() {
    const LATER: &str = "0f000000-0000-4000-8000-0000000000aa";
    let home = ClaudeHome::new();
    home.write("0f000000-0000-4000-8000-000000000001", 5, &["first"]);
    let cache = admitted_cache();
    let mut terminal = terminal();
    let mut runtime = launched_over_cache(&home, cache.path(), &mut terminal);
    home.write(LATER, 10, &["written after the launch"]);
    let other_refresh = another_rearviews_refresh(cache.path());

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
    assert!(!listed_ids(runtime.app()).contains(&LATER));
    drop(other_refresh);
    assert!(
        ticks_reach(&mut runtime, &mut terminal, BACKGROUND_WORK_LIMIT, |app| {
            listed_ids(app) == [LATER]
        }),
        "the session found by ID is listed"
    );
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

/// Rule 6: during the launch, the actions menu greys Delete and Rename with
/// the reason, and the rename key reports it.
#[test]
fn rename_and_delete_are_unavailable_during_the_launch_with_the_reason() {
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
    drop(rebuild);
    tick_until(&mut runtime, &mut terminal, |app| !app.is_loading());
}

/// Rule 6: during a list `Ctrl+R`, the actions menu greys Delete and Rename
/// with the reason.
#[test]
fn rename_and_delete_are_unavailable_during_a_list_refresh_with_the_reason() {
    let home = ClaudeHome::new();
    let a = home.write("a", 5, &["a"]);
    let cache = admitted_cache();
    let mut terminal = terminal();
    let mut runtime = launched_over_cache(&home, cache.path(), &mut terminal);
    home.append(&a, 6, "written after the launch");
    let other_refresh = another_rearviews_refresh(cache.path());
    tick_with(
        &mut runtime,
        &mut terminal,
        vec![key(KeyCode::Char('r'), KeyModifiers::CONTROL)],
    );
    tick_until(&mut runtime, &mut terminal, App::is_refreshing);

    let rows = actions_menu_rows(&mut runtime, &mut terminal);

    assert!(menu_row(&rows, "Delete…").contains("list refreshing"));
    assert!(menu_row(&rows, "Rename…").contains("list refreshing"));
    drop(other_refresh);
}

/// Rule 6: during a session's refresh, only that session's rename and
/// delete wait.
#[test]
fn only_the_refreshing_sessions_rename_and_delete_wait_during_its_refresh() {
    let home = ClaudeHome::new();
    home.write("b", 5, &["b"]);
    let a = home.write("a", 10, &["a"]);
    let cache = admitted_cache();
    let mut terminal = terminal();
    let mut runtime = launched_over_cache(&home, cache.path(), &mut terminal);
    open_the_selected_session(&mut runtime, &mut terminal);
    home.append(&a, 11, "written after the launch");
    let other_refresh = another_rearviews_refresh(cache.path());
    tick_with(
        &mut runtime,
        &mut terminal,
        vec![key(KeyCode::Char('r'), KeyModifiers::CONTROL)],
    );
    tick_until(&mut runtime, &mut terminal, App::is_refreshing);

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

    assert!(menu_row(&refreshing, "Rename…").contains("refreshing"));
    assert!(menu_row(&refreshing, "Delete…").contains("refreshing"));
    assert!(!menu_row(&other, "Rename…").contains("refreshing"));
    assert!(runtime.app().is_action_available(SessionAction::Rename));
    drop(other_refresh);
}

/// Rule 6: another rearview's refresh alone blocks neither rename nor
/// delete.
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
