//! A failed commit during the launch, a list `Ctrl+R` and a session
//! refresh, swept over every data commit of each: every readable session
//! stays listed, the operation writes nothing after the failure, and every
//! row is current in the cache or keeps a pending refresh.

use super::claude_home::{ClaudeHome, admitted_cache};
use super::refresh_tests::serve_session_refresh;
use super::*;
use crate::clock::TestClock;
use crate::history::load_meter::LoadMeter;
use crate::history::provider::reread_sessions;
use crate::history::sqlite_cache::connection::test_hooks;
use crate::history::{
    LoaderMessage, NextStep, OperationScope, Patience, ProcessIdentity, RefreshOptions,
    RefreshOutcome, Source, refresh_sessions, stream_all_conversations,
};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// The sessions each case writes, oldest first.
const SESSIONS: [&str; 3] = ["c", "b", "a"];

/// The data commits to the cache in one directory: commits of transactions
/// that changed a table other than the refresh state. One of them, the
/// `k`th from zero, rolls back, and the data changes made after it count.
struct DataCommits {
    _hooks: test_hooks::Installed,
    commits: Arc<AtomicUsize>,
    writes_after_the_failure: Arc<AtomicUsize>,
}

impl DataCommits {
    /// Count the data commits to the cache in `base`, failing the one
    /// numbered `failing` when there is one.
    fn counted(base: &Path, failing: Option<usize>) -> Self {
        let commits = Arc::new(AtomicUsize::new(0));
        let writes_after_the_failure = Arc::new(AtomicUsize::new(0));
        let has_failed = Arc::new(AtomicBool::new(false));
        let hooks = {
            let (commits, writes_after_the_failure) =
                (Arc::clone(&commits), Arc::clone(&writes_after_the_failure));
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
                let (commits, has_failed) = (Arc::clone(&commits), Arc::clone(&has_failed));
                connection
                    .commit_hook(Some(move || {
                        if !changes_data.swap(false, Ordering::SeqCst) {
                            return false;
                        }
                        let number = commits.fetch_add(1, Ordering::SeqCst);
                        let fails = Some(number) == failing;
                        if fails {
                            has_failed.store(true, Ordering::SeqCst);
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

/// `SESSIONS` written under a new home, and an admitted cache.
fn sessions_and_cache() -> (ClaudeHome, tempfile::TempDir) {
    let home = ClaudeHome::new();
    for (minute, name) in SESSIONS.iter().enumerate() {
        home.write(name, minute as u32, &[name]);
    }
    (home, admitted_cache())
}

/// Add a message to each of `SESSIONS` under `home`.
fn change_every_session(home: &ClaudeHome) {
    for name in SESSIONS {
        home.append(&home.transcript(name), 30, "written after the launch");
    }
}

/// The app after the launch over `home`'s sessions with the cache in
/// `cache`, fed the launch's messages as the runtime feeds them.
fn launched(home: &ClaudeHome, cache: &Path) -> App {
    let environment = home.environment_with_cache(
        cache,
        ProcessIdentity::default(),
        Arc::new(TestClock::default()),
    );
    let mut app = App::new_loading_with_options(
        ToolDisplayMode::Hidden,
        false,
        KeyBindings::default(),
        ListScope::default(),
        TuiSearchOptions::default(),
        environment.clone(),
    );
    let (messages, received) = mpsc::channel();
    std::thread::spawn(move || {
        stream_all_conversations(
            &messages,
            &environment,
            RefreshOptions::default(),
            &LoadMeter::default(),
        );
    })
    .join()
    .unwrap();
    for message in received.try_iter() {
        match message {
            LoaderMessage::Fatal(error) => panic!("the launch failed: {error}"),
            LoaderMessage::Batch(rows) => app.append_conversations(rows),
            LoaderMessage::CacheWritesStopped => app.refresh_rows_the_launch_parsed(),
            LoaderMessage::Done => app.finish_loading(),
            _ => {}
        }
    }
    app
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

/// One operation the sweep fails a commit of.
#[derive(Clone, Copy, Debug)]
enum Operation {
    Launch,
    ListRefresh,
    SessionRefresh,
}

impl Operation {
    /// Run this operation over new sessions, counting its data commits and
    /// failing the one numbered `failing`. Returns the app after it and the
    /// commits.
    fn run(self, failing: Option<usize>) -> (App, DataCommits, ClaudeHome, tempfile::TempDir) {
        let (home, cache) = sessions_and_cache();
        let (app, commits) = match self {
            Self::Launch => {
                let commits = DataCommits::counted(cache.path(), failing);
                (launched(&home, cache.path()), commits)
            }
            Self::ListRefresh => {
                let mut app = launched(&home, cache.path());
                change_every_session(&home);
                let commits = DataCommits::counted(cache.path(), failing);
                refresh_the_list(&mut app);
                (app, commits)
            }
            Self::SessionRefresh => {
                let mut app = launched(&home, cache.path());
                change_every_session(&home);
                let commits = DataCommits::counted(cache.path(), failing);
                refresh_every_session(&mut app);
                (app, commits)
            }
        };
        (app, commits, home, cache)
    }
}

/// True when the cache holds the session at `path` as its transcripts are
/// now, as the drawing thread reads it.
fn is_current(app: &App, path: &Path) -> bool {
    reread_sessions(&app.environment, Source::Claude, &[path], false)
        .pop()
        .is_some_and(|read| read.next() == NextStep::Settled)
}

#[test]
fn every_failed_data_commit_keeps_the_sessions_listed_and_their_refreshes_pending() {
    for operation in [
        Operation::Launch,
        Operation::ListRefresh,
        Operation::SessionRefresh,
    ] {
        let (_, clean, _home, _cache) = operation.run(None);
        let data_commits = clean.count();
        assert!(data_commits > 0, "{operation:?} committed no data");

        for failing in 0..data_commits {
            let (app, commits, _home, _cache) = operation.run(Some(failing));
            let case = format!("{operation:?} failing data commit {failing}");

            let mut listed: Vec<&str> = app
                .conversations()
                .iter()
                .map(|row| row.session_id.as_str())
                .collect();
            listed.sort_unstable();
            assert_eq!(listed, ["a", "b", "c"], "{case}");
            assert_eq!(commits.writes_after_the_failure(), 0, "{case}");
            for row in app.conversations() {
                assert!(
                    is_current(&app, &row.path) || app.store.is_refresh_pending(&row.path),
                    "{case}: {} is neither cached nor pending a refresh",
                    row.session_id
                );
            }
        }
    }
}
