//! A failed data commit during the launch, a list `Ctrl+R` and a session
//! refresh, swept over every data commit of each, over two agents reading
//! two roots each, with the viewer closed and open. No parse begins after
//! the failure; every listed row is current in the cache or keeps a pending
//! refresh, and a session left unlisted keeps one that lists it; no row
//! waits for another rearview. Once commits succeed again, ticking lists
//! every session from a current copy.

use super::refresh_tests::serve_session_refresh;
use super::*;
use crate::clock::TestClock;
use crate::history::load_meter::LoadMeter;
use crate::history::provider::contract_tests::registered_fixtures;
use crate::history::provider::walk::SystemLister;
use crate::history::provider::{ParseWatch, SessionStorage, WatchedStorage, reread_sessions};
use crate::history::sqlite_cache::connection::test_hooks;
use crate::history::{
    AwaitedRefresh, Cancellation, LoaderMessage, NextStep, OperationScope, Patience,
    ProcessIdentity, RefreshOptions, RefreshOutcome, Source, refresh_sessions,
    stream_all_conversations,
};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// The agents whose sessions each case writes.
const SOURCES: [Source; 2] = [Source::Claude, Source::Codex];

/// The roots each agent reads.
const ROOTS: usize = 2;

/// The sessions each root holds.
const SESSIONS_PER_ROOT: usize = 2;

/// The data commits to the cache in one directory: commits of transactions
/// that changed a table other than the refresh state. One of them, the
/// `k`th from zero, rolls back; from then on `watch` counts the parses that
/// begin, and the data changes made after it count.
struct DataCommits {
    _hooks: test_hooks::Installed,
    commits: Arc<AtomicUsize>,
    writes_after_the_failure: Arc<AtomicUsize>,
}

impl DataCommits {
    /// Count the data commits to the cache in `base`, failing the one
    /// numbered `failing` when there is one and marking `watch` then.
    fn counted(base: &Path, failing: Option<usize>, watch: &ParseWatch) -> Self {
        let commits = Arc::new(AtomicUsize::new(0));
        let writes_after_the_failure = Arc::new(AtomicUsize::new(0));
        let has_failed = Arc::new(AtomicBool::new(false));
        let hooks = {
            let (commits, writes_after_the_failure) =
                (Arc::clone(&commits), Arc::clone(&writes_after_the_failure));
            let watch = watch.clone();
            test_hooks::on_every_connection(base, move |connection| {
                if connection.is_readonly(rusqlite::MAIN_DB).unwrap() {
                    return;
                }
                let changes_data = Arc::new(AtomicBool::new(false));
                let (changed, failed, after) = (
                    Arc::clone(&changes_data),
                    Arc::clone(&has_failed),
                    Arc::clone(&writes_after_the_failure),
                );
                connection
                    .update_hook(Some(
                        move |_: rusqlite::hooks::Action, _: &str, table: &str, _: i64| {
                            if table == "refresh_state" {
                                return;
                            }
                            changed.store(true, Ordering::SeqCst);
                            if failed.load(Ordering::SeqCst) {
                                after.fetch_add(1, Ordering::SeqCst);
                            }
                        },
                    ))
                    .unwrap();
                let rolled_back = Arc::clone(&changes_data);
                connection
                    .rollback_hook(Some(move || rolled_back.store(false, Ordering::SeqCst)))
                    .unwrap();
                let (commits, has_failed, watch) =
                    (Arc::clone(&commits), Arc::clone(&has_failed), watch.clone());
                connection
                    .commit_hook(Some(move || {
                        if !changes_data.swap(false, Ordering::SeqCst) {
                            return false;
                        }
                        let number = commits.fetch_add(1, Ordering::SeqCst);
                        let fails = Some(number) == failing;
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
            commits,
            writes_after_the_failure,
        }
    }

    fn count(&self) -> usize {
        self.commits.load(Ordering::SeqCst)
    }

    fn writes_after_the_failure(&self) -> usize {
        self.writes_after_the_failure.load(Ordering::SeqCst)
    }
}

/// Sessions of every agent in [`SOURCES`] under [`ROOTS`] roots each, read
/// through storages whose parses `watch` sees, with an admitted cache, on
/// test time.
struct Corpus {
    _homes: Vec<tempfile::TempDir>,
    cache: tempfile::TempDir,
    sessions: Vec<PathBuf>,
    watch: ParseWatch,
    clock: Arc<TestClock>,
    environment: Environment,
}

impl Corpus {
    fn new() -> Self {
        let homes: Vec<_> = (0..ROOTS).map(|_| tempfile::tempdir().unwrap()).collect();
        let cache = super::claude_home::admitted_cache();
        let watch = ParseWatch::default();
        let mut sessions = Vec::new();
        let mut storages: Vec<&'static dyn SessionStorage> = Vec::new();
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
            storages.push(Box::leak(Box::new(WatchedStorage {
                inner: fixture.storage(),
                roots: homes
                    .iter()
                    .map(|home| fixture.root_under(home.path()))
                    .collect(),
                watch: watch.clone(),
            })));
        }
        let clock = Arc::new(TestClock::default());
        let environment = Environment::new(
            storages,
            cache.path().to_path_buf(),
            ProcessIdentity::default(),
            clock.clone(),
            Arc::new(SystemLister),
        );
        Self {
            _homes: homes,
            cache,
            sessions,
            watch,
            clock,
            environment,
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

    /// The app after the launch, fed the launch's messages as the runtime
    /// feeds them.
    fn launched(&self) -> App {
        let mut app = App::new_loading_with_options(
            ToolDisplayMode::Hidden,
            false,
            KeyBindings::default(),
            ListScope::default(),
            TuiSearchOptions::default(),
            self.environment.clone(),
        );
        let environment = self.environment.clone();
        let (messages, received) = mpsc::channel();
        std::thread::spawn(move || {
            stream_all_conversations(
                &messages,
                &environment,
                RefreshOptions::default(),
                &LoadMeter::default(),
                &Cancellation::default(),
            );
        })
        .join()
        .unwrap();
        for message in received.try_iter() {
            match message {
                LoaderMessage::Fatal(error) => panic!("the launch failed: {error}"),
                LoaderMessage::Batch(rows) => app.append_conversations(rows),
                LoaderMessage::CacheWritesStopped(stale) => {
                    app.refresh_what_the_launch_could_not_cache(stale)
                }
                LoaderMessage::Done => app.finish_loading(),
                _ => {}
            }
        }
        app
    }

    /// True when the cache holds the session at `path`, `source`'s, as its
    /// transcripts are now, as the drawing thread reads it.
    fn is_current(&self, source: Source, path: &Path) -> bool {
        reread_sessions(&self.environment, source, &[path], false)
            .pop()
            .is_some_and(|read| read.next() == NextStep::Settled)
    }

    /// The file names of every session, sorted.
    fn session_names(&self) -> Vec<String> {
        sorted_names(self.sessions.iter().map(PathBuf::as_path))
    }
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
            .map(RefreshOutcome::Changes);
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
        app.act_on_next_step(path, NextStep::CacheInBackground);
    }
    serve_session_refresh(app);
}

/// Tick `app` as the runtime does once other owners stop: apply finished
/// refreshes, check awaited ones, and serve every requested session refresh,
/// until none is pending.
fn tick_until_no_refresh_is_pending(app: &mut App, clock: &TestClock) {
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
    /// Run this operation over a new corpus, counting its data commits and
    /// failing the one numbered `failing`. Returns the app after it, the
    /// commits and the corpus.
    fn run(self, failing: Option<usize>) -> (App, DataCommits, Corpus) {
        let corpus = Corpus::new();
        let counted = || DataCommits::counted(corpus.cache.path(), failing, &corpus.watch);
        let (app, commits) = match self {
            Self::ColdLaunch => {
                let commits = counted();
                (corpus.launched(), commits)
            }
            Self::WarmLaunch => {
                drop(corpus.launched());
                corpus.change_every_session();
                let commits = counted();
                (corpus.launched(), commits)
            }
            Self::ListRefresh { is_viewer_open } => {
                let mut app = corpus.launched();
                corpus.change_every_session();
                let commits = counted();
                while_the_viewer_is(is_viewer_open, &mut app, refresh_the_list);
                (app, commits)
            }
            Self::SessionRefresh { is_viewer_open } => {
                let mut app = corpus.launched();
                corpus.change_every_session();
                let commits = counted();
                while_the_viewer_is(is_viewer_open, &mut app, refresh_every_session);
                (app, commits)
            }
        };
        (app, commits, corpus)
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

/// Check the state `operation` left after failing data commit `failing`,
/// then tick until every session is listed from a current copy.
fn check_failed_operation(operation: Operation, failing: usize) {
    let (mut app, commits, corpus) = operation.run(Some(failing));
    let case = format!("{operation:?} failing data commit {failing}");

    assert_eq!(
        corpus.watch.parses_after_mark(),
        0,
        "{case}: a parse began after the failed commit"
    );
    assert_eq!(
        commits.writes_after_the_failure(),
        0,
        "{case}: the operation wrote the cache after the failed commit"
    );
    let listed: Vec<PathBuf> = app
        .conversations()
        .iter()
        .map(|row| row.path.clone())
        .collect();
    for row in app.conversations() {
        assert!(
            corpus.is_current(row.source, &row.path) || app.store.is_refresh_pending(&row.path),
            "{case}: {} is neither current nor pending a refresh",
            row.path.display()
        );
    }
    for session in &corpus.sessions {
        let is_listed = listed
            .iter()
            .any(|path| path.file_name() == session.file_name());
        assert!(
            is_listed
                || (app.store.is_refresh_pending(session)
                    && app.store.looked_up_source(session).is_some()),
            "{case}: {} is unlisted without a refresh that lists it",
            session.display()
        );
    }
    if let Some(awaiting) = &app.store.awaiting {
        assert_eq!(
            awaiting.awaited,
            AwaitedRefresh::ThisProcess,
            "{case}: rows wait for another rearview, and none runs"
        );
    }

    drop(commits);
    tick_until_no_refresh_is_pending(&mut app, &corpus.clock);
    assert_eq!(
        sorted_names(app.conversations().iter().map(|row| row.path.as_path())),
        corpus.session_names(),
        "{case}: a session stayed unlisted once commits succeeded"
    );
    for row in app.conversations() {
        assert!(
            corpus.is_current(row.source, &row.path),
            "{case}: {} stayed out of date once commits succeeded",
            row.path.display()
        );
    }
}

#[test]
fn every_failed_data_commit_stops_parsing_and_keeps_every_session_pending_until_it_is_current() {
    let parallelism = std::thread::available_parallelism().map_or(1, usize::from);
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
        let (_, clean, _) = operation.run(None);
        let data_commits = clean.count();
        assert!(data_commits > 0, "{operation:?} committed no data");

        let failing: Vec<usize> = (0..data_commits).collect();
        for cases in failing.chunks(parallelism) {
            std::thread::scope(|scope| {
                for &failing in cases {
                    scope.spawn(move || check_failed_operation(operation, failing));
                }
            });
        }
    }
}
