use super::*;
use crate::clock::{SystemClock, TestClock};
use crate::history::Source;
use crate::history::provider::{Fingerprint, SessionRoot, SessionStub};
use crate::history::sqlite_cache::observer::{Observation, observe};
use crate::history::sqlite_cache::{
    CacheWrite, CacheWriter, ProcessIdentity, SessionRecord, SessionStatus, SqliteCache,
};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Instant, UNIX_EPOCH};

/// The cache in `directory` as one rearview reaches it, on the system clock:
/// these tests interleave with real threads and processes.
pub(crate) fn rearview(directory: &Path) -> CacheBase {
    CacheBase::new(
        directory,
        ProcessIdentity::default(),
        Arc::new(SystemClock::new()),
    )
}

/// The cache in `directory` as one rearview reaches it, on `clock`.
fn rearview_on(directory: &Path, clock: Arc<TestClock>) -> CacheBase {
    CacheBase::new(directory, ProcessIdentity::default(), clock)
}

/// The helper process's activity once it owns the refresh, until the test
/// releases it or kills it.
#[derive(Clone, Copy, Debug)]
pub(crate) enum OwnerActivity {
    /// Holds ownership between batches.
    Idle,
    /// Commits one session, then holds the writer lock inside a second batch
    /// that deletes it.
    InsideBatch,
    /// Starts a rebuild and commits dropping `meta`, holding SQLite's
    /// exclusive lock.
    InsideRebuild,
    /// Holds ownership after a write batch whose commit failed revoked its
    /// write permission.
    Revoked,
}

impl OwnerActivity {
    const VARIABLE: &str = "REARVIEW_REFRESH_TEST_ACTIVITY";

    fn name(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::InsideBatch => "batch",
            Self::InsideRebuild => "rebuild",
            Self::Revoked => "revoked",
        }
    }

    /// The activity the parent test set for this helper process. Read from
    /// the process's own variables: a test build's `RootEnvironment` sets none.
    #[allow(clippy::disallowed_methods)]
    fn of_process() -> Self {
        match std::env::var(Self::VARIABLE).ok().as_deref() {
            Some("batch") => Self::InsideBatch,
            Some("rebuild") => Self::InsideRebuild,
            Some("revoked") => Self::Revoked,
            _ => Self::Idle,
        }
    }
}

/// Another rearview's refresh ownership of the cache in a base, held in this
/// process on a thread of its own in one [`OwnerActivity`]. Dropping it ends
/// the activity cleanly and releases the ownership; a test of an owner that
/// dies uses [`OtherProcessOwner`] instead.
pub(crate) struct AnotherRearviewsOwner {
    base: PathBuf,
    release: Option<mpsc::Sender<()>>,
    thread: Option<thread::JoinHandle<()>>,
}

impl AnotherRearviewsOwner {
    pub(crate) fn hold(base: &Path) -> Self {
        Self::start(base, OwnerActivity::Idle)
    }

    pub(crate) fn start(base: &Path, activity: OwnerActivity) -> Self {
        let (ready, is_ready) = mpsc::channel();
        let (release, released) = mpsc::channel::<()>();
        let directory = base.to_path_buf();
        let thread = thread::spawn(move || {
            run_owner(&directory, activity, || {
                let _ = ready.send(());
                let _ = released.recv();
            });
        });
        is_ready
            .recv_timeout(Duration::from_secs(10))
            .expect("another rearview acquires ownership");
        Self {
            base: base.to_path_buf(),
            release: Some(release),
            thread: Some(thread),
        }
    }

    pub(crate) fn token(&self) -> RefreshToken {
        stored_owner_token(&self.base)
    }

    /// End the activity and release the ownership, failing on the owner's
    /// panic.
    pub(crate) fn release(mut self) {
        self.release.take();
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap();
        }
    }
}

impl Drop for AnotherRearviewsOwner {
    fn drop(&mut self) {
        self.release.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The stored owner's token as a rearview of its own reaches `base`.
fn stored_owner_token(base: &Path) -> RefreshToken {
    let identity = connect(base, POLL_EVERY)
        .unwrap()
        .query_row(
            "SELECT owner FROM refresh_state WHERE singleton = 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    RefreshToken::new(&rearview(base), identity)
}

/// A refresh owner in another process, which a test can kill.
pub(crate) struct OtherProcessOwner {
    child: Child,
    base: PathBuf,
}

impl OtherProcessOwner {
    pub(crate) fn hold(base: &Path) -> Self {
        Self::start(base, OwnerActivity::Idle)
    }

    pub(crate) fn start(base: &Path, activity: OwnerActivity) -> Self {
        let child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "history::sqlite_cache::refresh::tests::owner_process",
                "--ignored",
                "--nocapture",
            ])
            .env("REARVIEW_REFRESH_TEST_BASE", base)
            .env(OwnerActivity::VARIABLE, activity.name())
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        let mut owner = Self {
            child,
            base: base.to_path_buf(),
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        while !base.join("owner.ready").exists() {
            assert!(
                owner.child.try_wait().unwrap().is_none(),
                "the refresh helper runs until it acquires ownership"
            );
            assert!(
                Instant::now() < deadline,
                "the refresh helper acquires ownership"
            );
            thread::sleep(Duration::from_millis(10));
        }
        owner
    }

    pub(crate) fn token(&self) -> RefreshToken {
        stored_owner_token(&self.base)
    }
}

/// Kills the owner, as a crash or a closed terminal does.
impl Drop for OtherProcessOwner {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
#[ignore = "subprocess helper for refresh ownership tests"]
#[allow(clippy::disallowed_methods)]
fn owner_process() {
    let base = PathBuf::from(
        std::env::var_os("REARVIEW_REFRESH_TEST_BASE")
            .expect("the parent test supplies the cache directory"),
    );
    run_owner(&base, OwnerActivity::of_process(), || {
        std::fs::write(base.join("owner.ready"), "ready").unwrap();
        std::io::stdin().read_line(&mut String::new()).unwrap();
    });
}

/// Acquire refresh ownership of the cache in `base` as a rearview of its
/// own, enter `activity`, and hold it until `wait_for_release` returns.
fn run_owner(base: &Path, activity: OwnerActivity, wait_for_release: impl FnOnce()) {
    let owner = RefreshOwnership::acquire_for_test(&rearview(base)).unwrap();
    match activity {
        OwnerActivity::Idle => wait_for_release(),
        OwnerActivity::InsideBatch => {
            let mut cache = SqliteCache::open_for_refresh(owner.token()).unwrap();
            cache.mark_admitted().unwrap();
            cache.write(&record()).unwrap();
            let batch = transaction(&cache.connection, owner.token()).unwrap();
            batch.execute("DELETE FROM sessions", []).unwrap();
            wait_for_release();
            commit(batch, owner.token()).unwrap();
        }
        OwnerActivity::InsideRebuild => {
            let cache = CacheWriter::connect(owner.token().clone()).unwrap();
            let turn = owner.token().take_turn();
            cache.begin_rebuild(&turn).unwrap();
            let batch = transaction_in_turn(&cache.connection, owner.token(), &turn).unwrap();
            batch.execute_batch("DROP TABLE meta").unwrap();
            commit(batch, owner.token()).unwrap();
            wait_for_release();
        }
        OwnerActivity::Revoked => {
            fail_owner_commits(base);
            assert!(owner.token().write_batch(|| ()).is_err());
            allow_owner_commits(base);
            wait_for_release();
        }
    }
}

/// Fail every later commit of the refresh owner of the cache in `base`, as a
/// database that cannot record a commit fails it, until
/// [`allow_owner_commits`].
pub(crate) fn fail_owner_commits(base: &Path) {
    connect(base, POLL_EVERY)
        .unwrap()
        .execute_batch(
            "CREATE TRIGGER fail_owner_commit BEFORE UPDATE OF last_commit_ms ON refresh_state \
             WHEN NEW.owner = OLD.owner \
             BEGIN SELECT RAISE(ABORT, 'injected commit failure'); END;",
        )
        .unwrap();
}

/// Undo [`fail_owner_commits`].
pub(crate) fn allow_owner_commits(base: &Path) {
    connect(base, POLL_EVERY)
        .unwrap()
        .execute_batch("DROP TRIGGER fail_owner_commit")
        .unwrap();
}

/// A connection holding SQLite's exclusive lock on the session cache
/// database in `base`, as a rebuild holds it: no other connection reads the
/// database until it drops.
pub(crate) fn exclusive_lock(base: &Path) -> rusqlite::Connection {
    let connection = rusqlite::Connection::open(super::super::database_path(base)).unwrap();
    connection
        .pragma_update(None, "locking_mode", "EXCLUSIVE")
        .unwrap();
    connection
        .execute_batch("BEGIN EXCLUSIVE; COMMIT;")
        .unwrap();
    connection
}

/// A connection inside a write transaction on the session cache database in
/// `base`, as a write batch holds SQLite's writer lock.
pub(crate) fn writer_lock_held(base: &Path) -> rusqlite::Connection {
    let connection = rusqlite::Connection::open(super::super::database_path(base)).unwrap();
    connection.execute_batch("BEGIN IMMEDIATE").unwrap();
    connection
}

/// Make the refresh owner's last commit older than the takeover window.
pub(crate) fn expire_last_commit(base: &Path) {
    connect(base, POLL_EVERY)
        .unwrap()
        .execute("UPDATE refresh_state SET last_commit_ms = 0", [])
        .unwrap();
}

#[test]
fn crashed_process_is_replaced_after_its_last_commit_expires() {
    let directory = tempfile::tempdir().unwrap();
    let base = rearview(directory.path());
    let child = OtherProcessOwner::hold(base.path());
    let previous = child.token().identity;
    drop(child);
    expire_last_commit(base.path());
    let replacement = RefreshOwnership::acquire_for_test(&base).unwrap();
    assert_ne!(replacement.token.identity, previous);
    replacement.token.write_batch(|| ()).unwrap();
}

#[test]
fn crash_releases_writer_lock_and_keeps_completed_batches() {
    let directory = tempfile::tempdir().unwrap();
    let base = rearview(directory.path());
    let child = OtherProcessOwner::start(base.path(), OwnerActivity::InsideBatch);
    drop(child);
    let connection = connect(base.path(), POLL_EVERY).unwrap();
    let completed: i64 = connection
        .query_row("SELECT COUNT(*) FROM sessions", [], |row| row.get(0))
        .unwrap();
    assert_eq!(completed, 1, "the earlier committed session is preserved");
    connection
        .execute("UPDATE refresh_state SET last_commit_ms = 0", [])
        .unwrap();
    let replacement = RefreshOwnership::acquire_for_test(&base).unwrap();
    replacement.token.write_batch(|| ()).unwrap();
}

/// Take refresh ownership of `base` at once, as a waiter does from a stale
/// owner.
pub(crate) fn replace_owner(base: &CacheBase) -> RefreshToken {
    match try_acquire(base, Duration::ZERO, &mut None).unwrap() {
        Attempt::Acquired(token) => token,
        Attempt::Owned(_) | Attempt::WriterBusy(_) => panic!("ownership is replaced"),
    }
}

/// True when one attempt to acquire ownership of `base` finds a recent
/// owner.
fn finds_a_recent_owner(base: &CacheBase, observed: &mut Option<ObservedRefresh>) -> bool {
    matches!(
        try_acquire(base, STALE_AFTER, observed).unwrap(),
        Attempt::Owned(_)
    )
}

fn record() -> SessionRecord {
    let root = SessionRoot::new("virtual");
    let stub = SessionStub {
        locator: root.path.join("session.jsonl"),
        subagents: Vec::new(),
        cache_key: "session".to_owned(),
        fingerprint: Fingerprint {
            size: 100,
            modified: Some(UNIX_EPOCH + Duration::from_secs(1)),
        },
    };
    SessionRecord::unlisted(
        Source::Pi.provider().storage().cache(),
        Source::Pi,
        &root,
        &stub,
        SessionStatus::Empty,
        "skipped transcript has no visible messages".to_owned(),
    )
    .unwrap()
}

#[test]
fn old_ownership_doesnt_return_after_release_and_reacquisition() {
    let directory = tempfile::tempdir().unwrap();
    let base = rearview(directory.path());
    let first = RefreshOwnership::acquire_for_test(&base).unwrap();
    let replaced = replace_owner(&base);
    connect(base.path(), POLL_EVERY)
        .unwrap()
        .execute(
            "UPDATE refresh_state SET owner = NULL WHERE owner = ?1",
            [&replaced.identity],
        )
        .unwrap();
    let current = RefreshOwnership::acquire_for_test(&base).unwrap();
    assert_ne!(first.token.identity, current.token.identity);
    assert!(matches!(
        first
            .token
            .write_batch(|| panic!("the old owner doesn't publish")),
        Err(AppError::SessionCacheRefreshLost)
    ));
    drop(first);
    current.token.write_batch(|| ()).unwrap();
}

#[test]
fn write_batch_holds_writer_lock_and_replaced_owner_writes_nothing() {
    let directory = tempfile::tempdir().unwrap();
    let base = rearview(directory.path());
    let owner = RefreshOwnership::acquire_for_test(&base).unwrap();
    let written = base.path().join("written.txt");
    owner.token.write_batch(||{
        let mut contender = connect(base.path(), Duration::ZERO).unwrap();
        assert!(matches!(
            contender.transaction_with_behavior(TransactionBehavior::Immediate),
            Err(rusqlite::Error::SqliteFailure(error, _)) if error.code == rusqlite::ErrorCode::DatabaseBusy
        ));
        std::fs::write(&written, "current owner").unwrap();
    }).unwrap();
    let replacement = replace_owner(&base);
    assert!(matches!(
        owner
            .token
            .write_batch(|| std::fs::write(&written, "old owner").unwrap()),
        Err(AppError::SessionCacheRefreshLost)
    ));
    assert_eq!(std::fs::read_to_string(written).unwrap(), "current owner");
    replacement.write_batch(|| ()).unwrap();
}

#[test]
fn queued_writes_and_deletions_keep_their_original_owner_identity() {
    let directory = tempfile::tempdir().unwrap();
    let base = rearview(directory.path());
    let owner = RefreshOwnership::acquire_for_test(&base).unwrap();
    let mut old = SqliteCache::open_for_refresh(owner.token()).unwrap();
    old.mark_admitted().unwrap();
    let original = record();
    old.write(&original).unwrap();
    let replacement = replace_owner(&base);
    let mut current = SqliteCache::open_for_refresh(&replacement).unwrap();
    let mut newer = record();
    newer.catalogue.fingerprint.mtime_secs = 2;
    current.write(&newer).unwrap();
    let (send, receive) = mpsc::channel();
    send.send(CacheWrite::Session(Box::new(original))).unwrap();
    send.send(CacheWrite::Removed {
        storage: Source::Pi.provider().storage().cache(),
        root: PathBuf::from("virtual"),
        cache_keys: vec!["session".to_owned()],
    })
    .unwrap();
    drop(send);
    super::super::apply_writes(old, receive, None);
    assert_eq!(
        current
            .current_fingerprints(
                Source::Pi.provider().storage().cache(),
                Path::new("virtual")
            )
            .unwrap()["session"]
            .mtime_secs,
        2
    );
}

#[test]
fn takeover_between_batches_prevents_more_entries_and_final_status() {
    let directory = tempfile::tempdir().unwrap();
    let base = rearview(directory.path());
    let owner = RefreshOwnership::acquire_for_test(&base).unwrap();
    let mut cache = SqliteCache::open_for_refresh(owner.token()).unwrap();
    cache.mark_admitted().unwrap();
    let record = record();
    let session = cache.write_catalogue(&record).unwrap();
    let replacement = replace_owner(&base);
    assert!(matches!(
        super::super::session_transaction(&mut cache.connection, session, owner.token()),
        Err(AppError::SessionCacheRefreshLost)
    ));
    assert!(matches!(
        cache.set_final_status(session, SessionStatus::Empty, None),
        Err(AppError::SessionCacheRefreshLost)
    ));
    replacement.write_batch(|| ()).unwrap();
}

#[test]
fn schema_and_page_rebuilds_preserve_active_ownership() {
    for page_size in [1024, 4096] {
        let directory = tempfile::tempdir().unwrap();
        let base = rearview(directory.path());
        let connection = Connection::open(super::super::database_path(base.path())).unwrap();
        connection
            .pragma_update(None, "page_size", page_size)
            .unwrap();
        connection.execute_batch("CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT); INSERT INTO meta VALUES ('schema_version', 'old')").unwrap();
        drop(connection);
        let owner = RefreshOwnership::acquire_for_test(&base).unwrap();
        let mut cache = SqliteCache::open_for_refresh(owner.token()).unwrap();
        cache.mark_admitted().unwrap();
        cache.write(&record()).unwrap();
        owner.token.write_batch(|| ()).unwrap();
        assert_eq!(
            cache
                .connection
                .pragma_query_value(None, "page_size", |row| row.get::<_, u32>(0))
                .unwrap(),
            4096
        );
    }
}

#[test]
fn admitted_database_with_wrong_page_size_requires_refresh() {
    let directory = tempfile::tempdir().unwrap();
    let base = rearview(directory.path());
    let cache = SqliteCache::open(&base).unwrap();
    cache.mark_admitted().unwrap();
    cache
        .connection
        .pragma_update(None, "journal_mode", "DELETE")
        .unwrap();
    cache
        .connection
        .pragma_update(None, "page_size", 1024)
        .unwrap();
    cache.connection.execute_batch("VACUUM").unwrap();
    drop(cache);
    assert!(SqliteCache::open_waiting(&base).unwrap().is_none());
    let owner = RefreshOwnership::acquire_for_test(&base).unwrap();
    let cache = SqliteCache::open_for_refresh(owner.token()).unwrap();
    assert_eq!(
        cache
            .connection
            .pragma_query_value(None, "page_size", |row| row.get::<_, u32>(0))
            .unwrap(),
        4096
    );
    assert!(
        cache.is_admitted().unwrap(),
        "the rebuilt database waits for an admission"
    );
}

#[test]
fn committed_batch_restarts_takeover_window() {
    let directory = tempfile::tempdir().unwrap();
    let base = rearview(directory.path());
    let owner = RefreshOwnership::acquire_for_test(&base).unwrap();
    let connection = connect(base.path(), POLL_EVERY).unwrap();
    connection
        .execute("UPDATE refresh_state SET last_commit_ms = 0", [])
        .unwrap();
    owner.token.write_batch(|| ()).unwrap();
    assert!(finds_a_recent_owner(&base, &mut None));
}

#[test]
fn long_batch_blocks_takeover_and_timestamps_its_end() {
    let directory = tempfile::tempdir().unwrap();
    let base = rearview(directory.path());
    let owner = RefreshOwnership::acquire_for_test(&base).unwrap();
    let connection = connect(base.path(), POLL_EVERY).unwrap();
    // Simulate the previously committed state becoming stale during this batch.
    connection
        .execute("UPDATE refresh_state SET last_commit_ms = 0", [])
        .unwrap();
    let token = owner.token().clone();
    let transaction = transaction(&connection, &token).unwrap();
    std::thread::scope(|scope| {
        let (send, receive) = mpsc::channel();
        let waiter = base.clone();
        scope.spawn(move || {
            send.send(RefreshOwnership::acquire_for_test(&waiter))
                .unwrap()
        });
        assert!(
            receive.recv_timeout(POLL_EVERY * 4).is_err(),
            "a waiter doesn't acquire ownership during an active batch"
        );
        let finished_after = now_ms(base.clock().as_ref());
        commit(transaction, &token).unwrap();
        let last_commit: i64 = connection
            .query_row("SELECT last_commit_ms FROM refresh_state", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert!(
            last_commit >= finished_after,
            "the timestamp follows the end of the batch"
        );
        assert!(
            receive.recv_timeout(POLL_EVERY * 4).is_err(),
            "the waiter uses the timestamp from after the active batch commits"
        );
        drop(owner);
        receive
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap();
    });
}

#[test]
fn rebuild_exclusion_survives_commits_and_waiting_connections() {
    let directory = tempfile::tempdir().unwrap();
    let base = rearview(directory.path());
    let owner = RefreshOwnership::acquire_for_test(&base).unwrap();
    let cache = SqliteCache::open_for_refresh(owner.token()).unwrap();
    cache.begin_rebuild(&owner.token().take_turn()).unwrap();
    cache
        .connection
        .execute("UPDATE refresh_state SET last_commit_ms = 0", [])
        .unwrap();
    std::thread::scope(|scope| {
        let (send, receive) = mpsc::channel();
        let waiter = base.clone();
        scope.spawn(move || {
            send.send(RefreshOwnership::acquire_for_test(&waiter))
                .unwrap()
        });
        assert!(
            receive.recv_timeout(POLL_EVERY * 4).is_err(),
            "a waiter doesn't enter between maintenance transactions"
        );
        let finished_after = now_ms(base.clock().as_ref());
        cache.rebuild().unwrap();
        let last_commit: i64 = cache
            .connection
            .query_row("SELECT last_commit_ms FROM refresh_state", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert!(last_commit >= finished_after);
        assert!(
            receive.recv_timeout(POLL_EVERY * 2).is_err(),
            "maintenance keeps exclusion until closing"
        );
        drop(cache);
        assert!(
            receive.recv_timeout(POLL_EVERY * 2).is_err(),
            "the waiter uses the rebuild's final timestamp"
        );
        drop(owner);
        let replacement = receive
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap();
        SqliteCache::open_for_refresh(replacement.token()).unwrap();
    });
}

#[test]
fn rebuild_blocked_at_begin_exclusive_preserves_existing_tables() {
    let directory = tempfile::tempdir().unwrap();
    let base = rearview(directory.path());
    let owner = RefreshOwnership::acquire_for_test(&base).unwrap();
    let cache = SqliteCache::open_for_refresh(owner.token()).unwrap();
    cache.set_meta("sentinel", "keep").unwrap();
    let reader = connect(base.path(), POLL_EVERY).unwrap();
    reader
        .query_row("SELECT value FROM meta WHERE key = 'sentinel'", [], |row| {
            row.get::<_, String>(0)
        })
        .unwrap();
    cache.connection.busy_timeout(Duration::ZERO).unwrap();
    assert!(matches!(cache.rebuild(), Err(AppError::SessionCache(error)) if is_busy(&error)));
    drop(cache);
    assert_eq!(
        reader
            .query_row("SELECT value FROM meta WHERE key = 'sentinel'", [], |row| {
                row.get::<_, String>(0)
            })
            .unwrap(),
        "keep"
    );
}

#[test]
fn previous_owner_cannot_start_rebuild_maintenance() {
    let directory = tempfile::tempdir().unwrap();
    let base = rearview(directory.path());
    let owner = RefreshOwnership::acquire_for_test(&base).unwrap();
    let cache = SqliteCache::open_for_refresh(owner.token()).unwrap();
    cache.set_meta("sentinel", "keep").unwrap();
    let replacement = replace_owner(&base);
    assert!(matches!(
        cache.rebuild(),
        Err(AppError::SessionCacheRefreshLost)
    ));
    drop(cache);
    let cache = SqliteCache::open_for_refresh(&replacement).unwrap();
    assert_eq!(cache.meta("sentinel").unwrap().as_deref(), Some("keep"));
}

#[test]
fn crashed_rebuild_without_meta_still_corrects_page_size() {
    let directory = tempfile::tempdir().unwrap();
    let base = rearview(directory.path());
    let connection = Connection::open(super::super::database_path(base.path())).unwrap();
    connection.pragma_update(None, "page_size", 1024).unwrap();
    connection.execute_batch("CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT); INSERT INTO meta VALUES ('schema_version', 'old')").unwrap();
    drop(connection);
    let child = OtherProcessOwner::start(base.path(), OwnerActivity::InsideRebuild);
    drop(child);
    let connection = connect(base.path(), POLL_EVERY).unwrap();
    assert_eq!(
        connection
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE name = 'meta'",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
        0
    );
    connection
        .execute("UPDATE refresh_state SET last_commit_ms = 0", [])
        .unwrap();
    drop(connection);
    let owner = RefreshOwnership::acquire_for_test(&base).unwrap();
    let cache = SqliteCache::open_for_refresh(owner.token()).unwrap();
    assert_eq!(
        cache
            .connection
            .pragma_query_value(None, "page_size", |row| row.get::<_, u32>(0))
            .unwrap(),
        4096
    );
    assert_eq!(
        cache
            .meta(super::super::schema::SCHEMA_VERSION_KEY)
            .unwrap(),
        Some(super::super::schema::SCHEMA_VERSION.to_string())
    );
}

#[test]
fn ownership_doesnt_advance_without_batch_commit() {
    let directory = tempfile::tempdir().unwrap();
    let base = rearview(directory.path());
    let owner = RefreshOwnership::acquire_for_test(&base).unwrap();
    let connection = connect(base.path(), POLL_EVERY).unwrap();
    let last_commit = || {
        connection
            .query_row("SELECT last_commit_ms FROM refresh_state", [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap()
    };
    let before = now_ms(base.clock().as_ref()) - 1_000;
    connection
        .execute("UPDATE refresh_state SET last_commit_ms = ?1", [before])
        .unwrap();
    assert_eq!(
        observe(&base).unwrap(),
        Observation::Running {
            owner: RefreshOwner::ThisProcess,
            last_commit_ms: before,
            commit_age: crate::history::sqlite_cache::observer::CommitAge::Recent,
        }
    );
    assert!(matches!(
        RefreshOwnership::try_acquire_now(&base).unwrap(),
        Acquisition::HeldBy(RefreshOwner::ThisProcess)
    ));
    assert!(finds_a_recent_owner(&base, &mut None));
    assert_eq!(last_commit(), before);
    drop(owner);
    let state: (Option<String>, i64) = connection
        .query_row(
            "SELECT owner, last_commit_ms FROM refresh_state",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert!(state.0.is_none());
    assert!(state.1 >= before);
}

#[test]
fn sqlite_content_batches_also_record_their_commit_time() {
    let directory = tempfile::tempdir().unwrap();
    let base = rearview(directory.path());
    let owner = RefreshOwnership::acquire_for_test(&base).unwrap();
    let mut cache = SqliteCache::open_for_refresh(owner.token()).unwrap();
    cache.mark_admitted().unwrap();
    cache
        .connection
        .execute("UPDATE refresh_state SET last_commit_ms = 0", [])
        .unwrap();
    cache.write(&record()).unwrap();
    let last_commit: i64 = cache
        .connection
        .query_row("SELECT last_commit_ms FROM refresh_state", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert!(last_commit > 0);
    assert!(finds_a_recent_owner(&base, &mut None));
}

#[test]
fn failed_read_revokes_queued_writes_even_after_failure_clears() {
    let directory = tempfile::tempdir().unwrap();
    let base = rearview(directory.path());
    let owner = RefreshOwnership::acquire_for_test(&base).unwrap();
    let mut cache = SqliteCache::open_for_refresh(owner.token()).unwrap();
    cache.mark_admitted().unwrap();
    let original = record();
    cache.write(&original).unwrap();
    let connection = connect(base.path(), POLL_EVERY).unwrap();
    connection.execute_batch("CREATE TRIGGER fail_commit BEFORE UPDATE OF last_commit_ms ON refresh_state WHEN NEW.owner = OLD.owner BEGIN SELECT RAISE(ABORT, 'injected timestamp failure'); END;").unwrap();
    let (value, coordinated) = owner.token().read(|| 42);
    assert_eq!(value, 42);
    assert!(coordinated.is_err());
    connection
        .execute_batch("DROP TRIGGER fail_commit")
        .unwrap();
    let (send, receive) = mpsc::channel();
    let mut changed = record();
    changed.catalogue.fingerprint.mtime_secs = 2;
    send.send(CacheWrite::Session(Box::new(changed))).unwrap();
    send.send(CacheWrite::Removed {
        storage: Source::Pi.provider().storage().cache(),
        root: PathBuf::from(&original.catalogue.root),
        cache_keys: vec![original.catalogue.cache_key.clone()],
    })
    .unwrap();
    drop(send);
    super::super::apply_writes(cache, receive, None);
    let cache = SqliteCache::open_waiting(&base).unwrap().unwrap();
    assert!(
        cache
            .holds(
                Source::Pi.provider().storage().cache(),
                Path::new(&original.catalogue.root),
                &SessionStub {
                    locator: PathBuf::new(),
                    subagents: Vec::new(),
                    cache_key: original.catalogue.cache_key,
                    fingerprint: Fingerprint {
                        size: 100,
                        modified: Some(UNIX_EPOCH + Duration::from_secs(1))
                    },
                }
            )
            .unwrap()
    );
    assert!(
        owner
            .token()
            .write_batch(|| panic!("the revoked owner doesn't publish"))
            .is_err()
    );
}

#[test]
fn rebuild_waits_at_begin_exclusive_for_open_reader_while_waiter_polls() {
    let directory = tempfile::tempdir().unwrap();
    let base = rearview(directory.path());
    let owner = RefreshOwnership::acquire_for_test(&base).unwrap();
    let cache = SqliteCache::open_for_refresh(owner.token()).unwrap();
    let reader = connect(base.path(), POLL_EVERY).unwrap();
    reader
        .query_row("SELECT COUNT(*) FROM meta", [], |row| row.get::<_, i64>(0))
        .unwrap();
    thread::scope(|scope| {
        let (send, receive) = mpsc::channel();
        let rebuild = scope.spawn(move || {
            let result = cache.begin_rebuild(&cache.owner.take_turn());
            send.send(result).unwrap();
            cache
        });
        assert!(
            matches!(
                receive.recv_timeout(POLL_EVERY * 2),
                Err(mpsc::RecvTimeoutError::Timeout)
            ),
            "the rebuild waits for the blocking reader to close"
        );
        let waiter = base.clone();
        let (acquired, waiting) = mpsc::channel();
        scope.spawn(move || {
            acquired
                .send(RefreshOwnership::acquire_for_test(&waiter))
                .unwrap()
        });
        drop(reader);
        receive
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap();
        assert!(
            matches!(
                waiting.recv_timeout(POLL_EVERY * 2),
                Err(mpsc::RecvTimeoutError::Timeout)
            ),
            "the waiter uses the rebuild's final commit"
        );
        drop(rebuild.join().unwrap());
        drop(owner);
        waiting
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap();
    });
}

#[test]
fn clock_rollback_doesnt_extend_unchanged_ownership_indefinitely() {
    let directory = tempfile::tempdir().unwrap();
    let clock = Arc::new(TestClock::default());
    let base = rearview_on(directory.path(), Arc::clone(&clock));
    let owner = RefreshOwnership::acquire_for_test(&base).unwrap();
    let connection = connect(base.path(), POLL_EVERY).unwrap();
    let future = now_ms(clock.as_ref()) + 3_600_000;
    connection
        .execute("UPDATE refresh_state SET last_commit_ms = ?1", [future])
        .unwrap();
    let mut observed = None;
    assert!(finds_a_recent_owner(&base, &mut observed));
    clock.advance(STALE_AFTER);
    let Attempt::Acquired(replacement) = try_acquire(&base, STALE_AFTER, &mut observed).unwrap()
    else {
        panic!("the unchanged ownership expires");
    };
    assert_ne!(replacement.identity, owner.token.identity);
    assert!(owner.token.write_batch(|| ()).is_err());
    replacement.write_batch(|| ()).unwrap();
}

#[test]
fn new_commit_restarts_monotonic_observation_after_clock_rollback() {
    let directory = tempfile::tempdir().unwrap();
    let clock = Arc::new(TestClock::default());
    let base = rearview_on(directory.path(), Arc::clone(&clock));
    let owner = RefreshOwnership::acquire_for_test(&base).unwrap();
    let connection = connect(base.path(), POLL_EVERY).unwrap();
    let future = now_ms(clock.as_ref()) + 3_600_000;
    connection
        .execute("UPDATE refresh_state SET last_commit_ms = ?1", [future])
        .unwrap();
    let observed_since = clock.now();
    clock.advance(STALE_AFTER);
    let mut observed = Some(ObservedRefresh {
        owner: owner.token.identity.clone(),
        last_commit_ms: future - 1,
        since: observed_since,
    });
    assert!(finds_a_recent_owner(&base, &mut observed));
    assert!(clock.elapsed_since(observed.unwrap().since) < STALE_AFTER);
}

#[test]
fn blocked_commit_revokes_writes_and_rolls_back_its_batch() {
    let directory = tempfile::tempdir().unwrap();
    let base = rearview(directory.path());
    let owner = RefreshOwnership::acquire_for_test(&base).unwrap();
    let connection = connect(base.path(), Duration::ZERO).unwrap();
    connection
        .execute_batch("CREATE TABLE sentinel (value INTEGER); INSERT INTO sentinel VALUES (1)")
        .unwrap();
    let reader = connect(base.path(), Duration::ZERO).unwrap();
    let read = reader.unchecked_transaction().unwrap();
    read.query_row("SELECT value FROM sentinel", [], |row| row.get::<_, i64>(0))
        .unwrap();
    let write = transaction(&connection, owner.token()).unwrap();
    write.execute("UPDATE sentinel SET value = 2", []).unwrap();
    assert!(commit(write, owner.token()).is_err());
    assert!(!owner.token.can_publish());
    drop(read);
    assert_eq!(
        connection
            .query_row("SELECT value FROM sentinel", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert!(
        owner
            .token
            .write_batch(|| panic!("a failed commit loses write permission"))
            .is_err()
    );
}

#[test]
fn failure_to_begin_write_batch_revokes_publication() {
    let directory = tempfile::tempdir().unwrap();
    let base = rearview(directory.path());
    let owner = RefreshOwnership::acquire_for_test(&base).unwrap();
    let connection = connect(base.path(), Duration::ZERO).unwrap();
    connection.execute_batch("BEGIN").unwrap();
    assert!(transaction(&connection, owner.token()).is_err());
    connection.execute_batch("ROLLBACK").unwrap();
    assert!(!owner.token.can_publish());
    assert!(
        owner
            .token
            .write_batch(|| panic!("a failed BEGIN loses write permission"))
            .is_err()
    );
}

#[test]
fn wait_within_limit_behind_held_writer_lock_ends_without_ownership() {
    let directory = tempfile::tempdir().unwrap();
    let base = rearview(directory.path());
    let owner = RefreshOwnership::acquire_for_test(&base).unwrap();
    let connection = connect(base.path(), POLL_EVERY).unwrap();
    let batch = transaction(&connection, owner.token()).unwrap();
    let waiter = rearview_on(directory.path(), Arc::new(TestClock::default()));
    let status = WaitStatus::default();

    let waited = RefreshOwnership::acquire_within(
        &waiter,
        Duration::from_secs(1),
        &status,
        &Cancellation::default(),
    )
    .unwrap();

    assert!(waited.is_none(), "a held refresh stays with its owner");
    assert_eq!(status.current(), Some(OtherRearview::Refreshing));
    commit(batch, owner.token()).unwrap();
    owner.token.write_batch(|| ()).unwrap();
}

#[test]
fn owners_write_waits_for_its_own_batch_past_limit_without_revoking_it() {
    let directory = tempfile::tempdir().unwrap();
    let base = rearview(directory.path());
    let owner = RefreshOwnership::acquire_for_test(&base).unwrap();
    let token = owner.token();
    let lock_wait_limit = POLL_EVERY * 2;
    thread::scope(|scope| {
        let (started, batch_started) = mpsc::channel();
        let batch = scope.spawn(move || {
            token.write_batch(|| {
                started.send(()).unwrap();
                thread::sleep(lock_wait_limit * 3);
            })
        });
        batch_started.recv().unwrap();
        let connection = connect(base.path(), POLL_EVERY).unwrap();
        let write = transaction_within(&connection, token, lock_wait_limit)
            .expect("the write waits behind its own owner's batch");
        commit(write, token).unwrap();
        batch.join().unwrap().unwrap();
    });
    assert!(token.can_publish(), "the owner keeps its own refresh");
}

#[test]
fn uncontended_acquisition_publishes_no_wait() {
    let directory = tempfile::tempdir().unwrap();
    let base = rearview(directory.path());
    let status = WaitStatus::default();
    let owner = RefreshOwnership::acquire(&base, &status, &Cancellation::default()).unwrap();
    assert_eq!(status.current(), None);
    drop(owner);
}

/// Wait until `status` publishes a wait, failing after five seconds.
fn wait_for_a_published_wait(status: &WaitStatus) -> OtherRearview {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(wait) = status.current() {
            return wait;
        }
        assert!(Instant::now() < deadline, "the waiter publishes its wait");
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn wait_for_another_processs_refresh_publishes_it() {
    let directory = tempfile::tempdir().unwrap();
    let base = rearview(directory.path());
    let other = AnotherRearviewsOwner::hold(base.path());
    let status = WaitStatus::default();
    thread::scope(|scope| {
        let waiter =
            scope.spawn(|| RefreshOwnership::acquire(&base, &status, &Cancellation::default()));
        assert_eq!(
            wait_for_a_published_wait(&status),
            OtherRearview::Refreshing
        );
        other.release();
        waiter.join().unwrap().unwrap();
    });
}

#[test]
fn wait_for_this_processs_own_refresh_publishes_no_wait() {
    let directory = tempfile::tempdir().unwrap();
    let base = rearview(directory.path());
    let held = RefreshOwnership::acquire_for_test(&base).unwrap();
    let status = WaitStatus::default();
    thread::scope(|scope| {
        let waiter =
            scope.spawn(|| RefreshOwnership::acquire(&base, &status, &Cancellation::default()));
        let deadline = Instant::now() + POLL_EVERY * 3;
        while Instant::now() < deadline {
            assert_eq!(status.current(), None);
            thread::sleep(Duration::from_millis(10));
        }
        drop(held);
        waiter.join().unwrap().unwrap();
    });
}

/// A rebuild's exclusive lock on the session cache database in `base`,
/// released `after` the first connection rearview opens to it starts
/// opening.
fn exclusive_lock_released_after_first_open(
    base: &Path,
    after: Duration,
) -> super::super::connection::test_hooks::Installed {
    let lock = Mutex::new(Some(exclusive_lock(base)));
    super::super::connection::test_hooks::on_every_connection(base, move |_| {
        if let Some(held) = lock.lock().unwrap().take() {
            thread::spawn(move || {
                thread::sleep(after);
                drop(held);
            });
        }
    })
}

#[test]
fn exclusive_lock_released_within_open_wait_doesnt_publish_rebuild() {
    let directory = tempfile::tempdir().unwrap();
    let base = rearview(directory.path());
    SqliteCache::open(&base).unwrap().mark_admitted().unwrap();
    let _released =
        exclusive_lock_released_after_first_open(base.path(), REBUILD_LOCK_THRESHOLD / 4);
    let status = WaitStatus::default();

    let owner = RefreshOwnership::acquire(&base, &status, &Cancellation::default()).unwrap();

    assert_eq!(
        status.current(),
        None,
        "a lock released within the waiter's open wait doesn't publish a wait"
    );
    drop(owner);
}

#[test]
fn exclusive_lock_held_past_open_wait_publishes_rebuild_after_waiting() {
    let directory = tempfile::tempdir().unwrap();
    let base = rearview(directory.path());
    SqliteCache::open(&base).unwrap().mark_admitted().unwrap();
    let rebuild = exclusive_lock(base.path());
    let status = WaitStatus::default();
    thread::scope(|scope| {
        let started = Instant::now();
        let waiter =
            scope.spawn(|| RefreshOwnership::acquire(&base, &status, &Cancellation::default()));

        assert_eq!(
            wait_for_a_published_wait(&status),
            OtherRearview::Rebuilding
        );
        assert!(
            started.elapsed() >= REBUILD_LOCK_THRESHOLD / 2,
            "the waiter waits for the lock before publishing the rebuild"
        );
        drop(rebuild);
        waiter.join().unwrap().unwrap();
    });
}

#[test]
fn observation_doesnt_report_rebuild_for_lock_released_within_open_wait() {
    let directory = tempfile::tempdir().unwrap();
    let base = rearview(directory.path());
    SqliteCache::open(&base).unwrap().mark_admitted().unwrap();
    let _released =
        exclusive_lock_released_after_first_open(base.path(), REBUILD_LOCK_THRESHOLD / 4);

    assert_eq!(
        observe(&base).unwrap(),
        Observation::Idle,
        "a lock released within the observation's open wait is observed as idle"
    );
}

#[test]
fn observation_reports_rebuild_for_lock_held_past_open_wait_after_waiting() {
    let directory = tempfile::tempdir().unwrap();
    let base = rearview(directory.path());
    SqliteCache::open(&base).unwrap().mark_admitted().unwrap();
    let rebuild = exclusive_lock(base.path());
    let started = Instant::now();

    let observation = observe(&base).unwrap();

    assert_eq!(observation, Observation::Unknown);
    assert!(
        started.elapsed() >= REBUILD_LOCK_THRESHOLD / 2,
        "the observation waits for the lock before reporting"
    );
    drop(rebuild);
}

#[test]
fn observation_names_refreshs_process_without_waiting() {
    let directory = tempfile::tempdir().unwrap();
    let base = rearview(directory.path());
    assert_eq!(observe(&base).unwrap(), Observation::Idle);
    let owner = RefreshOwnership::acquire_for_test(&base).unwrap();
    assert!(matches!(
        observe(&base).unwrap(),
        Observation::Running {
            owner: RefreshOwner::ThisProcess,
            ..
        }
    ));
    assert!(matches!(
        RefreshOwnership::try_acquire_now(&base).unwrap(),
        Acquisition::HeldBy(RefreshOwner::ThisProcess)
    ));
    drop(owner);
    assert_eq!(observe(&base).unwrap(), Observation::Idle);

    let other = AnotherRearviewsOwner::hold(base.path());
    assert!(matches!(
        observe(&base).unwrap(),
        Observation::Running {
            owner: RefreshOwner::AnotherProcess,
            ..
        }
    ));
    assert!(matches!(
        RefreshOwnership::try_acquire_now(&base).unwrap(),
        Acquisition::HeldBy(RefreshOwner::AnotherProcess)
    ));
    other.release();
    assert_eq!(observe(&base).unwrap(), Observation::Idle);
    assert!(matches!(
        RefreshOwnership::try_acquire_now(&base).unwrap(),
        Acquisition::Acquired(_)
    ));
}

#[test]
fn another_processs_batch_holding_writer_lock_is_observed_as_its_refresh() {
    let directory = tempfile::tempdir().unwrap();
    let base = rearview(directory.path());
    let other = AnotherRearviewsOwner::start(base.path(), OwnerActivity::InsideBatch);
    assert!(matches!(
        observe(&base).unwrap(),
        Observation::Running {
            owner: RefreshOwner::AnotherProcess,
            ..
        }
    ));
    drop(other);
}

#[test]
fn takeover_from_this_process_is_another_processs_refresh_until_it_completes() {
    let directory = tempfile::tempdir().unwrap();
    let base = rearview(directory.path());
    let held = RefreshOwnership::acquire_for_test(&base).unwrap();
    expire_last_commit(base.path());
    let other = AnotherRearviewsOwner::hold(base.path());

    assert!(matches!(
        observe(&base).unwrap(),
        Observation::Running {
            owner: RefreshOwner::AnotherProcess,
            ..
        }
    ));
    assert!(matches!(
        RefreshOwnership::try_acquire_now(&base).unwrap(),
        Acquisition::HeldBy(RefreshOwner::AnotherProcess)
    ));

    other.release();
    assert_eq!(
        observe(&base).unwrap(),
        Observation::Idle,
        "the replaced ownership doesn't hide the other refresh's completion"
    );
    drop(held);
}

#[test]
fn this_processs_own_ownership_ends_its_wait_for_another_process() {
    let directory = tempfile::tempdir().unwrap();
    let base = rearview(directory.path());
    drop(RefreshOwnership::acquire_for_test(&base).unwrap());
    let connection = connect(base.path(), POLL_EVERY).unwrap();
    connection
        .execute(
            "UPDATE refresh_state SET owner = 'another process', last_commit_ms = ?1",
            [now_ms(base.clock().as_ref())],
        )
        .unwrap();
    let status = WaitStatus::default();
    thread::scope(|scope| {
        let load =
            scope.spawn(|| RefreshOwnership::acquire(&base, &status, &Cancellation::default()));
        wait_for_a_published_wait(&status);

        connection
            .execute("UPDATE refresh_state SET owner = NULL", [])
            .unwrap();
        let Acquisition::Acquired(own) = RefreshOwnership::try_acquire_now(&base).unwrap() else {
            panic!("this thread acquires ownership before the load");
        };

        let deadline = Instant::now() + Duration::from_secs(2);
        while status.current().is_some() {
            assert!(
                Instant::now() < deadline,
                "the load stops waiting for another process during this process's refresh"
            );
            thread::sleep(Duration::from_millis(10));
        }
        drop(own);
        load.join().unwrap().unwrap();
    });
}

#[test]
fn reads_without_waiting_report_another_processs_rebuild_as_busy_at_once() {
    let directory = tempfile::tempdir().unwrap();
    let base = rearview(directory.path());
    SqliteCache::open(&base).unwrap().mark_admitted().unwrap();
    let other = AnotherRearviewsOwner::start(base.path(), OwnerActivity::InsideRebuild);
    let started = Instant::now();

    let lookup = super::super::InteractiveCache::at(base.clone())
        .open()
        .and_then(|cache| {
            cache
                .map(|cache| cache.find_session(Source::Pi, Path::new("session.jsonl"), &[]))
                .transpose()
        });

    assert!(matches!(lookup, Err(error) if super::super::is_busy_error(&error)));
    assert_eq!(observe(&base).unwrap(), Observation::Unknown);
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the reads don't wait for the rebuild"
    );
    drop(other);
}

/// How the ownership lifecycle sweep ends a refresh ownership whose write
/// permission a failed batch revoked.
#[derive(Clone, Copy, Debug)]
enum RevokedOwnershipEnd {
    /// Its operation finishes, dropping the ownership.
    Finish,
    /// This rearview shuts the cache down, as the TUI does at exit.
    Shutdown,
    /// Another rearview takes over once the last commit is older than the
    /// takeover window, and then the operation finishes.
    Takeover,
}

/// The refresh owning the cache, as `base` observes it; `None` while none
/// does.
fn observed_owner(base: &CacheBase) -> Option<RefreshOwner> {
    match observe(base).unwrap() {
        Observation::Running { owner, .. } => Some(owner),
        Observation::Idle | Observation::Unknown | Observation::RebuildingHere => None,
    }
}

/// A refresh ownership revoked at each batch lasts until its operation
/// ends: this rearview keeps seeing its own refresh, another sees it
/// running, and each end leaves the cache to another rearview at once,
/// without clearing a replacement owner.
#[test]
fn revoked_ownership_lasts_until_its_operation_ends_and_no_longer() {
    use crate::history::sqlite_cache::{AwaitedRefresh, Coordinator, NextStep};
    for committed in 0..3 {
        for end in [
            RevokedOwnershipEnd::Finish,
            RevokedOwnershipEnd::Shutdown,
            RevokedOwnershipEnd::Takeover,
        ] {
            let case = format!("revoked after {committed} committed batches, ended by {end:?}");
            let directory = tempfile::tempdir().unwrap();
            let this = rearview(directory.path());
            let other = rearview(directory.path());
            let ownership = RefreshOwnership::acquire_for_test(&this).unwrap();
            for _ in 0..committed {
                ownership.token().write_batch(|| ()).unwrap();
            }
            fail_owner_commits(directory.path());
            assert!(
                ownership.token().write_batch(|| ()).is_err(),
                "{case}: the batch whose commit fails doesn't commit"
            );
            allow_owner_commits(directory.path());

            assert_eq!(
                observed_owner(&this),
                Some(RefreshOwner::ThisProcess),
                "{case}: this rearview sees its revoked refresh as its own"
            );
            assert_eq!(
                observed_owner(&other),
                Some(RefreshOwner::AnotherProcess),
                "{case}: another rearview finds the revoked refresh running"
            );
            assert_eq!(
                NextStep::after_read_without_writing(&this),
                NextStep::AwaitRefresh(AwaitedRefresh::ThisProcess),
                "{case}: a read without writing waits for this rearview's refresh"
            );

            match end {
                RevokedOwnershipEnd::Finish => drop(ownership),
                RevokedOwnershipEnd::Shutdown => Coordinator::of(&this).shutdown(),
                RevokedOwnershipEnd::Takeover => {
                    expire_last_commit(directory.path());
                    let replacement = RefreshOwnership::acquire_for_test(&other).unwrap();
                    assert_eq!(
                        observed_owner(&this),
                        Some(RefreshOwner::AnotherProcess),
                        "{case}: this rearview sees the replacement as another rearview's"
                    );
                    drop(ownership);
                    assert_eq!(
                        observed_owner(&other),
                        Some(RefreshOwner::ThisProcess),
                        "{case}: the revoked ownership's end keeps its replacement"
                    );
                    drop(replacement);
                    continue;
                }
            }
            assert!(
                matches!(
                    RefreshOwnership::try_acquire_now(&other).unwrap(),
                    Acquisition::Acquired(_)
                ),
                "{case}: another rearview acquires ownership at once"
            );
        }
    }
}

/// A rearview killed while its revoked ownership lasted keeps readers
/// waiting only until its last commit is older than the takeover window.
#[test]
fn killed_revoked_owner_stops_holding_readers_once_its_last_commit_expires() {
    let directory = tempfile::tempdir().unwrap();
    let this = rearview(directory.path());
    let killed = OtherProcessOwner::start(directory.path(), OwnerActivity::Revoked);
    assert_eq!(
        observe(&this).unwrap().awaited(),
        Some(super::super::AwaitedRefresh::AnotherRearview(
            OtherRearview::Refreshing
        ))
    );

    drop(killed);
    expire_last_commit(directory.path());

    assert_eq!(
        observe(&this).unwrap().awaited(),
        None,
        "a reader waits for an owner whose last commit is past the takeover window"
    );
    assert!(matches!(
        RefreshOwnership::try_acquire_now(&this).unwrap(),
        Acquisition::Acquired(_)
    ));
}
