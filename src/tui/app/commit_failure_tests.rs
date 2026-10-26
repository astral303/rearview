//! A failed data commit during the launch, a list `Ctrl+R` and a session
//! refresh, swept over every data commit of the first and the last session
//! each writes, over two agents reading two roots each, with the viewer
//! closed and open. No parse begins after the failure; every listed row is
//! current in the cache or keeps a pending refresh, and a session left
//! unlisted keeps one that lists it; no row waits for another rearview.
//! Once commits succeed again, ticking lists every session from a current
//! copy.

use super::claude_home::{ClaudeHome, TemplateCache, admitted_cache, another_rearview};
use super::refresh_tests::serve_session_refresh;
use super::*;
use crate::clock::{Clock, SystemClock, TestClock};
use crate::history::load_meter::LoadMeter;
use crate::history::provider::contract_tests::registered_fixtures;
use crate::history::provider::walk::SystemLister;
use crate::history::provider::{
    ParseWatch, SessionRoot, SessionStorage, WatchedStorage, reread_sessions,
};
use crate::history::sqlite_cache::connection::test_hooks;
use crate::history::sqlite_cache::refresh::RefreshOwnership;
use crate::history::sqlite_cache::refresh::tests::{allow_owner_commits, fail_owner_commits};
use crate::history::{
    AwaitedRefresh, Cancellation, LoaderMessage, NextStep, OperationScope, Patience,
    ProcessIdentity, RefreshOptions, RefreshOutcome, Source, refresh_sessions,
    stream_all_conversations,
};
use rusqlite::hooks::Action;
use std::collections::HashSet;
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// The agents whose sessions each case writes.
const SOURCES: [Source; 2] = [Source::Claude, Source::Codex];

/// The roots each agent reads.
const ROOTS: usize = 2;

/// The sessions each root holds.
const SESSIONS_PER_ROOT: usize = 2;

/// The kinds of data commit a session's write makes, named from the rows
/// each transaction changed.
#[derive(Clone, Debug, PartialEq)]
enum CommitKind {
    /// Inserts the session's row in the catalogue.
    Catalogue,
    /// Inserts a chunk of the session's entries.
    Entries,
    /// Updates the session's row only: its message counts or its final status.
    SessionUpdate,
    /// Any other change, listed.
    Unnamed(String),
}

impl CommitKind {
    fn of(changes: &[(Action, String)]) -> Self {
        let inserts = |table: &str| {
            changes
                .iter()
                .any(|(action, changed)| *action == Action::SQLITE_INSERT && changed == table)
        };
        if inserts("sessions") {
            Self::Catalogue
        } else if inserts("entries") {
            Self::Entries
        } else if changes
            .iter()
            .all(|(action, table)| *action == Action::SQLITE_UPDATE && table == "sessions")
        {
            Self::SessionUpdate
        } else {
            Self::Unnamed(format!("{changes:?}"))
        }
    }
}

/// The data commits to the cache in one directory: commits of transactions
/// that changed a table other than the refresh state. One of them, the
/// `k`th from zero, rolls back; from then on `watch` counts the parses that
/// begin, and the data changes made after it count.
struct DataCommits {
    _hooks: test_hooks::Installed,
    kinds: Arc<Mutex<Vec<CommitKind>>>,
    writes_after_the_failure: Arc<AtomicUsize>,
}

impl DataCommits {
    /// Record the kind of each data commit to the cache in `base`, failing
    /// the one numbered `failing` when there is one and marking `watch` then.
    fn counted(base: &Path, failing: Option<usize>, watch: &ParseWatch) -> Self {
        let kinds = Arc::new(Mutex::new(Vec::new()));
        let writes_after_the_failure = Arc::new(AtomicUsize::new(0));
        let has_failed = Arc::new(AtomicBool::new(false));
        let hooks = {
            let (kinds, writes_after_the_failure) =
                (Arc::clone(&kinds), Arc::clone(&writes_after_the_failure));
            let watch = watch.clone();
            test_hooks::on_every_connection(base, move |connection| {
                if connection.is_readonly(rusqlite::MAIN_DB).unwrap() {
                    return;
                }
                let changes: Arc<Mutex<Vec<(Action, String)>>> = Arc::default();
                let (changed, failed, after) = (
                    Arc::clone(&changes),
                    Arc::clone(&has_failed),
                    Arc::clone(&writes_after_the_failure),
                );
                connection
                    .update_hook(Some(move |action: Action, _: &str, table: &str, _: i64| {
                        if table == "refresh_state" {
                            return;
                        }
                        let mut changed = changed.lock().unwrap();
                        let change = (action, table.to_owned());
                        if !changed.contains(&change) {
                            changed.push(change);
                        }
                        if failed.load(Ordering::SeqCst) {
                            after.fetch_add(1, Ordering::SeqCst);
                        }
                    }))
                    .unwrap();
                let rolled_back = Arc::clone(&changes);
                connection
                    .rollback_hook(Some(move || rolled_back.lock().unwrap().clear()))
                    .unwrap();
                let (kinds, has_failed, watch) =
                    (Arc::clone(&kinds), Arc::clone(&has_failed), watch.clone());
                connection
                    .commit_hook(Some(move || {
                        let changes = std::mem::take(&mut *changes.lock().unwrap());
                        if changes.is_empty() {
                            return false;
                        }
                        let mut kinds = kinds.lock().unwrap();
                        let fails = Some(kinds.len()) == failing;
                        kinds.push(CommitKind::of(&changes));
                        if fails {
                            has_failed.store(true, Ordering::SeqCst);
                            watch.mark();
                        }
                        fails
                    }))
                    .unwrap();
            })
        };
        Self {
            _hooks: hooks,
            kinds,
            writes_after_the_failure,
        }
    }

    /// The kind of each data commit so far, in commit order.
    fn kinds(&self) -> Vec<CommitKind> {
        self.kinds.lock().unwrap().clone()
    }

    fn writes_after_the_failure(&self) -> usize {
        self.writes_after_the_failure.load(Ordering::SeqCst)
    }
}

/// The data commits the sweep fails, from the kinds of a clean run's data
/// commits: every commit of the first session written, in the first parse
/// group, and of the last, in a later group. The last session's last commit
/// is the operation's last data commit. Panics when a commit is of no named
/// kind or a kind goes unswept: a missed commit can leave a session out of
/// date without a pending refresh.
fn commits_to_fail(operation: Operation, kinds: &[CommitKind]) -> Vec<usize> {
    for (number, kind) in kinds.iter().enumerate() {
        assert!(
            !matches!(kind, CommitKind::Unnamed(_)),
            "{operation:?}: data commit {number} is of a named kind: {kind:?}"
        );
    }
    let session_starts: Vec<usize> = (0..kinds.len())
        .filter(|&number| kinds[number] == CommitKind::Catalogue)
        .collect();
    assert_eq!(
        session_starts.first(),
        Some(&0),
        "{operation:?}: the first data commit begins a session's write"
    );
    assert!(
        session_starts.len() > SESSIONS_PER_ROOT,
        "{operation:?}: the last session written is in a later parse group"
    );
    let first_session = 0..session_starts[1];
    let last_session = session_starts[session_starts.len() - 1]..kinds.len();
    let failing: Vec<usize> = first_session.chain(last_session).collect();
    for kind in kinds {
        assert!(
            failing.iter().any(|&number| kinds[number] == *kind),
            "{operation:?}: a failing commit is of kind {kind:?}"
        );
    }
    failing
}

/// Sessions of every agent in [`SOURCES`] under [`ROOTS`] roots each.
struct Corpus {
    _homes: Vec<tempfile::TempDir>,
    sessions: Vec<PathBuf>,
    /// Each agent's storage, with the roots it reads.
    storages: Vec<(&'static dyn SessionStorage, Vec<SessionRoot>)>,
}

impl Corpus {
    fn new() -> Self {
        let homes: Vec<_> = (0..ROOTS).map(|_| tempfile::tempdir().unwrap()).collect();
        let mut sessions = Vec::new();
        let mut storages = Vec::new();
        for source in SOURCES {
            let fixture = registered_fixtures()
                .into_iter()
                .find(|fixture| fixture.provider().source() == source)
                .unwrap();
            for (root, home) in homes.iter().enumerate() {
                for index in 0..SESSIONS_PER_ROOT {
                    let id = session_id(source, root * SESSIONS_PER_ROOT + index);
                    sessions.push(fixture.write_session(home.path(), &id));
                }
            }
            let roots = homes
                .iter()
                .map(|home| fixture.root_under(home.path()))
                .collect();
            storages.push((fixture.storage(), roots));
        }
        Self {
            _homes: homes,
            sessions,
            storages,
        }
    }

    /// Add an empty line to every session's transcript.
    fn change_every_session(&self) {
        for session in &self.sessions {
            let mut transcript = std::fs::OpenOptions::new()
                .append(true)
                .open(session)
                .unwrap();
            std::io::Write::write_all(&mut transcript, b"\n").unwrap();
        }
    }

    /// A new rearview reading these sessions through storages whose parses
    /// `watch` sees, with the session cache in `cache`, on `clock`.
    fn environment(&self, cache: &Path, watch: &ParseWatch, clock: Arc<TestClock>) -> Environment {
        let storages = self
            .storages
            .iter()
            .map(|(storage, roots)| -> &'static dyn SessionStorage {
                Box::leak(Box::new(WatchedStorage {
                    inner: *storage,
                    roots: roots.clone(),
                    watch: watch.clone(),
                }))
            })
            .collect();
        Environment::new(
            storages,
            cache.to_path_buf(),
            ProcessIdentity::default(),
            clock,
            Arc::new(SystemLister),
        )
    }

    /// The file names of every session, sorted.
    fn session_names(&self) -> Vec<String> {
        sorted_names(self.sessions.iter().map(PathBuf::as_path))
    }
}

/// A corpus and the session cache an operation starts from, set up once
/// for every case of the operations that share it.
struct Setup {
    corpus: Corpus,
    cache: TemplateCache,
    /// The launch's messages, for an app to start from, when the setup
    /// launched before the transcripts changed.
    launch: Vec<LoaderMessage>,
}

impl Setup {
    /// A cache holding none of the sessions.
    fn cold() -> Self {
        Self {
            corpus: Corpus::new(),
            cache: TemplateCache::new(admitted_cache()),
            launch: Vec::new(),
        }
    }

    /// A cache holding every session, each changed since the launch that
    /// cached it.
    fn launched_then_changed() -> Self {
        let corpus = Corpus::new();
        let cache = admitted_cache();
        let environment = corpus.environment(
            cache.path(),
            &ParseWatch::default(),
            Arc::new(TestClock::default()),
        );
        let launch = launch_messages(&environment, RefreshOptions::default());
        corpus.change_every_session();
        Self {
            corpus,
            cache: TemplateCache::new(cache),
            launch,
        }
    }
}

/// One case: a setup's corpus over a copy of its cache, read through
/// storages whose parses `watch` sees, on test time.
struct Case<'setup> {
    corpus: &'setup Corpus,
    cache: tempfile::TempDir,
    watch: ParseWatch,
    clock: Arc<TestClock>,
    environment: Environment,
}

impl<'setup> Case<'setup> {
    fn of(setup: &'setup Setup) -> Self {
        let cache = setup.cache.copy();
        let watch = ParseWatch::default();
        let clock = Arc::new(TestClock::default());
        let environment = setup
            .corpus
            .environment(cache.path(), &watch, clock.clone());
        Self {
            corpus: &setup.corpus,
            cache,
            watch,
            clock,
            environment,
        }
    }

    /// The app after the launch.
    fn launched(&self) -> App {
        launched_app(&self.environment, RefreshOptions::default())
    }

    /// The paths of `rows` whose sessions the cache holds as their
    /// transcripts are now, as the drawing thread reads them. Rereads each
    /// agent's rows together, so its roots are discovered once.
    fn current_paths(&self, rows: &[Conversation]) -> HashSet<PathBuf> {
        let mut current = HashSet::new();
        for source in SOURCES {
            let paths: Vec<&Path> = rows
                .iter()
                .filter(|row| row.source == source)
                .map(|row| row.path.as_path())
                .collect();
            let outcomes = reread_sessions(&self.environment, source, &paths, false);
            current.extend(
                paths
                    .into_iter()
                    .zip(outcomes)
                    .filter_map(|(path, outcome)| {
                        (outcome.into_parts().1 == NextStep::Settled).then(|| path.to_path_buf())
                    }),
            );
        }
        current
    }
}

/// The app after a launch through `environment` with `options`, fed the
/// launch's messages as the runtime feeds them.
pub(super) fn launched_app(environment: &Environment, options: RefreshOptions) -> App {
    let window = options.time;
    app_fed(environment, window, &launch_messages(environment, options))
}

/// The messages a launch through `environment` with `options` sends.
fn launch_messages(environment: &Environment, options: RefreshOptions) -> Vec<LoaderMessage> {
    let environment = environment.clone();
    let (messages, received) = mpsc::channel();
    std::thread::spawn(move || {
        stream_all_conversations(
            &messages,
            &environment,
            options,
            &LoadMeter::default(),
            &Cancellation::default(),
        );
    })
    .join()
    .unwrap();
    received.try_iter().collect()
}

/// An app through `environment` listing under `window`, fed a launch's
/// `messages` as the runtime feeds them.
fn app_fed(environment: &Environment, window: TimeFilter, messages: &[LoaderMessage]) -> App {
    let mut app = App::new_loading_with_options(
        ToolDisplayMode::Hidden,
        false,
        KeyBindings::default(),
        ListScope::default(),
        TuiSearchOptions::default(),
        environment.clone(),
    );
    app.set_list_window(window);
    for message in messages {
        match message {
            LoaderMessage::Fatal(error) => panic!("the launch doesn't fail: {error}"),
            LoaderMessage::Batch(rows) => app.append_conversations(rows.clone()),
            LoaderMessage::CacheWritesStopped(stale) => {
                app.refresh_what_the_launch_could_not_cache(stale.clone())
            }
            LoaderMessage::Done => app.finish_loading(),
            _ => {}
        }
    }
    app
}

/// The `index`th session ID of `source`'s sessions, in its agent's shape.
fn session_id(source: Source, index: usize) -> String {
    match source {
        Source::Codex => format!("019f0000-0000-7000-8000-{:012}", index + 1),
        _ => format!("0f000000-0000-4000-8000-{:012}", index + 1),
    }
}

fn sorted_names<'a>(paths: impl Iterator<Item = &'a Path>) -> Vec<String> {
    let mut names: Vec<String> = paths
        .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    names.sort_unstable();
    names
}

/// A list `Ctrl+R` of `app`, run as the background worker runs it.
fn refresh_the_list(app: &mut App) {
    let known = app.begin_refresh().expect("a list refresh begins");
    let environment = app.environment.clone();
    let outcome = std::thread::spawn(move || {
        let operation = OperationScope::new(
            environment.cache_base(),
            Patience::UntilAcquired,
            Arc::default(),
        );
        let outcome = refresh_sessions(&environment, &known, RefreshOptions::default(), &operation)
            .map(|changes| RefreshOutcome::Changes(Box::new(changes)));
        operation.finish().and(outcome)
    })
    .join()
    .unwrap();
    app.finish_refresh(outcome);
}

/// A session refresh of every listed row of `app`.
fn refresh_every_session(app: &mut App) {
    let paths: Vec<PathBuf> = app
        .conversations()
        .iter()
        .map(|row| row.path.clone())
        .collect();
    for path in &paths {
        app.record_next_step(path, NextStep::CacheInBackground);
    }
    serve_session_refresh(app);
}

/// Tick `app` as the runtime does once other owners stop: apply finished
/// refreshes, check awaited ones, and serve every requested session refresh,
/// until none is pending.
pub(super) fn tick_until_no_refresh_is_pending(app: &mut App, clock: &TestClock) {
    for _ in 0..10 {
        app.apply_finished_refresh();
        clock.advance(Duration::from_secs(5));
        app.check_awaited_refresh();
        if app.store.is_session_refresh_requested() {
            serve_session_refresh(app);
        } else if app.store.awaiting.is_none() {
            return;
        }
    }
    panic!("refreshes complete within ten ticks");
}

/// Behind another rearview's refresh, the launch lists the cache as it is,
/// then refreshes under its own ownership once that refresh ends. When those
/// writes stop, the launch requests a session refresh of the changed session
/// it read from its old copy; once commits succeed again, that refresh lists
/// the newest messages.
#[test]
fn launch_behind_another_refresh_whose_writes_stop_requests_refresh_of_stale_session() {
    let home = ClaudeHome::new();
    home.write("unchanged", 10, &["unchanged"]);
    let changed = home.write("changed", 5, &["cached"]);
    let cache = admitted_cache();
    let clock: Arc<dyn Clock> = Arc::new(SystemClock::new());
    let environment =
        home.environment_with_cache(cache.path(), ProcessIdentity::default(), clock.clone());
    home.rows(&environment);
    home.append(&changed, 6, "written after the cache");
    let other_refresh =
        RefreshOwnership::acquire_for_test(&another_rearview(cache.path(), clock)).unwrap();
    fail_owner_commits(cache.path());

    let (sender, received) = mpsc::channel();
    let launch = {
        let environment = environment.clone();
        std::thread::spawn(move || {
            stream_all_conversations(
                &sender,
                &environment,
                RefreshOptions::default(),
                &LoadMeter::default(),
                &Cancellation::default(),
            );
        })
    };
    let mut messages = Vec::new();
    while !messages
        .iter()
        .any(|message| matches!(message, LoaderMessage::Batch(_)))
    {
        messages.push(received.recv().unwrap());
    }
    drop(other_refresh);
    launch.join().unwrap();
    messages.extend(received.try_iter());
    assert!(
        messages
            .iter()
            .any(|message| matches!(message, LoaderMessage::CacheWritesStopped(_))),
        "the launch's writes stop"
    );
    let mut app = app_applying(&environment, messages);

    assert!(
        app.store.is_refresh_pending(&changed),
        "the changed session's refresh is pending after the launch's writes stop"
    );
    allow_owner_commits(cache.path());
    app.expire_awaited_refresh_check_for_test();
    app.check_awaited_refresh();
    serve_session_refresh(&mut app);
    let changed_row = app
        .conversations()
        .iter()
        .find(|row| row.path == changed)
        .expect("the changed session is listed");
    assert_eq!(
        changed_row.message_count, 2,
        "the session refresh lists the changed session's newest message"
    );
}

/// An app through `environment` fed a launch's `messages` as the runtime
/// applies them.
fn app_applying(environment: &Environment, messages: Vec<LoaderMessage>) -> App {
    let mut app = App::new_loading_with_options(
        ToolDisplayMode::Hidden,
        false,
        KeyBindings::default(),
        ListScope::default(),
        TuiSearchOptions::default(),
        environment.clone(),
    );
    for message in messages {
        match message {
            LoaderMessage::Fatal(error) => panic!("the launch doesn't fail: {error}"),
            LoaderMessage::Batch(rows) => app.append_conversations(rows),
            LoaderMessage::Changes(changes) => app.add_launch_changes(changes),
            LoaderMessage::CacheWritesStopped(stale) => {
                app.refresh_what_the_launch_could_not_cache(stale)
            }
            LoaderMessage::Done => app.finish_loading(),
            _ => {}
        }
    }
    app
}

/// One operation the sweep fails a commit of.
#[derive(Clone, Copy, Debug)]
enum Operation {
    /// The launch over a cache holding none of the sessions.
    ColdLaunch,
    /// The launch over a cache holding every session, each changed since.
    WarmLaunch,
    ListRefresh {
        is_viewer_open: bool,
    },
    SessionRefresh {
        is_viewer_open: bool,
    },
}

impl Operation {
    /// Run this operation in `case`, counting its data commits and failing
    /// the one numbered `failing`. A list or session refresh starts from an
    /// app fed the launch of `setup`. Returns the app after it and the
    /// commits.
    fn run(self, setup: &Setup, case: &Case, failing: Option<usize>) -> (App, DataCommits) {
        let counted = || DataCommits::counted(case.cache.path(), failing, &case.watch);
        let launched_before_the_change =
            || app_fed(&case.environment, TimeFilter::default(), &setup.launch);
        match self {
            Self::ColdLaunch | Self::WarmLaunch => {
                let commits = counted();
                (case.launched(), commits)
            }
            Self::ListRefresh { is_viewer_open } => {
                let mut app = launched_before_the_change();
                let commits = counted();
                while_the_viewer_is(is_viewer_open, &mut app, refresh_the_list);
                (app, commits)
            }
            Self::SessionRefresh { is_viewer_open } => {
                let mut app = launched_before_the_change();
                let commits = counted();
                while_the_viewer_is(is_viewer_open, &mut app, refresh_every_session);
                (app, commits)
            }
        }
    }

    /// The setup of `cold` or `warm` this operation starts from.
    fn setup<'a>(self, cold: &'a Setup, warm: &'a Setup) -> &'a Setup {
        match self {
            Self::ColdLaunch => cold,
            _ => warm,
        }
    }
}

/// Run `operation` on `app` with the viewer open when `is_open`, then close
/// it.
fn while_the_viewer_is(is_open: bool, app: &mut App, operation: fn(&mut App)) {
    if is_open {
        app.enter_view_mode(80);
    }
    operation(app);
    if is_open {
        app.exit_view_mode();
        app.apply_finished_refresh();
    }
}

/// Check the state `operation`, started from `setup`, left after failing
/// data commit `failing`, of `kind` in the clean run, then tick until every
/// session is listed from a current copy.
fn check_failed_operation(setup: &Setup, operation: Operation, failing: usize, kind: &CommitKind) {
    let case = Case::of(setup);
    let (mut app, commits) = operation.run(setup, &case, Some(failing));
    let label = format!("{operation:?} failing data commit {failing} ({kind:?})");

    assert_eq!(
        commits.kinds().get(failing),
        Some(kind),
        "{label}: the failed commit is of the clean run's kind"
    );
    assert_eq!(
        case.watch.parses_after_mark(),
        0,
        "{label}: parsing stops at the failed commit"
    );
    assert_eq!(
        commits.writes_after_the_failure(),
        0,
        "{label}: the operation doesn't write the cache after the failed commit"
    );
    let listed: Vec<PathBuf> = app
        .conversations()
        .iter()
        .map(|row| row.path.clone())
        .collect();
    let current = case.current_paths(app.conversations());
    for row in app.conversations() {
        assert!(
            current.contains(&row.path) || app.store.is_refresh_pending(&row.path),
            "{label}: {} is current or pending a refresh",
            row.path.display()
        );
    }
    for session in &case.corpus.sessions {
        let is_listed = listed
            .iter()
            .any(|path| path.file_name() == session.file_name());
        assert!(
            is_listed
                || (app.store.is_refresh_pending(session)
                    && app.store.unlisted_source(session).is_some()),
            "{label}: {} is listed or has a pending refresh that lists it",
            session.display()
        );
    }
    if let Some(awaiting) = &app.store.awaiting {
        assert_eq!(
            awaiting.awaited,
            AwaitedRefresh::ThisProcess,
            "{label}: rows wait for this rearview's refresh"
        );
    }

    drop(commits);
    tick_until_no_refresh_is_pending(&mut app, &case.clock);
    assert_eq!(
        sorted_names(app.conversations().iter().map(|row| row.path.as_path())),
        case.corpus.session_names(),
        "{label}: every session is listed once commits succeed"
    );
    let current = case.current_paths(app.conversations());
    for row in app.conversations() {
        assert!(
            current.contains(&row.path),
            "{label}: {} is current once commits succeed",
            row.path.display()
        );
    }
}

#[test]
fn failed_data_commit_of_each_kind_stops_parsing_and_keeps_every_session_pending_until_it_is_current()
 {
    let parallelism = std::thread::available_parallelism().map_or(1, usize::from);
    let (cold, warm) = (Setup::cold(), Setup::launched_then_changed());
    for operation in [
        Operation::ColdLaunch,
        Operation::WarmLaunch,
        Operation::ListRefresh {
            is_viewer_open: false,
        },
        Operation::ListRefresh {
            is_viewer_open: true,
        },
        Operation::SessionRefresh {
            is_viewer_open: false,
        },
        Operation::SessionRefresh {
            is_viewer_open: true,
        },
    ] {
        let setup = operation.setup(&cold, &warm);
        let (_, clean) = operation.run(setup, &Case::of(setup), None);
        let kinds = clean.kinds();
        assert!(!kinds.is_empty(), "{operation:?} commits data");

        let failing = commits_to_fail(operation, &kinds);
        for cases in failing.chunks(parallelism) {
            std::thread::scope(|scope| {
                for &failing in cases {
                    let kind = &kinds[failing];
                    scope.spawn(move || check_failed_operation(setup, operation, failing, kind));
                }
            });
        }
    }
}
