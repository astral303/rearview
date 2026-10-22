use super::*;
use crate::clock::{SystemClock, TestClock};
use crate::history::Source;
use crate::history::provider::{Fingerprint, SessionRoot, SessionStub};
use crate::history::sqlite_cache::{
    CacheWrite, CacheWriter, SessionRecord, SessionStatus, SqliteCache,
};
use std::io::Write;
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
#[derive(Clone, Copy)]
pub(crate) enum OwnerActivity {
    /// Holds ownership between batches.
    Idle,
    /// Commits one session, then holds the writer lock inside a second batch
    /// that deletes it.
    InsideBatch,
    /// Starts a rebuild and commits dropping `meta`, holding SQLite's
    /// exclusive lock.
    InsideRebuild,
}

impl OwnerActivity {
    const VARIABLE: &str = "REARVIEW_REFRESH_TEST_ACTIVITY";

    fn name(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::InsideBatch => "batch",
            Self::InsideRebuild => "rebuild",
        }
    }

    fn of_process() -> Self {
        match crate::history::provider::RootEnvironment::of_process()
            .var(Self::VARIABLE)
            .as_deref()
        {
            Some("batch") => Self::InsideBatch,
            Some("rebuild") => Self::InsideRebuild,
            _ => Self::Idle,
        }
    }
}

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
        let identity = connect(&self.base, POLL_EVERY)
            .unwrap()
            .query_row(
                "SELECT owner FROM refresh_state WHERE singleton = 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        RefreshToken::new(&rearview(&self.base), identity)
    }

    pub(crate) fn release(mut self) {
        self.child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(b"release\n")
            .unwrap();
        assert!(self.child.wait().unwrap().success());
    }
}

impl Drop for OtherProcessOwner {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
#[ignore = "subprocess helper for refresh ownership tests"]
fn owner_process() {
    let base = PathBuf::from(
        crate::history::provider::RootEnvironment::of_process()
            .var_os("REARVIEW_REFRESH_TEST_BASE")
            .expect("the parent test supplies the cache directory"),
    );
    let owner = RefreshOwnership::acquire(&rearview(&base), || ()).unwrap();
    let wait_for_release = || {
        std::fs::write(base.join("owner.ready"), "ready").unwrap();
        std::io::stdin().read_line(&mut String::new()).unwrap();
    };
    match OwnerActivity::of_process() {
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
    }
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
fn a_crashed_process_is_replaced_after_its_last_commit_expires() {
    let directory = tempfile::tempdir().unwrap();
    let base = rearview(directory.path());
    let child = OtherProcessOwner::hold(base.path());
    let previous = child.token().identity;
    drop(child);
    expire_last_commit(base.path());
    let replacement = RefreshOwnership::acquire(&base, || ()).unwrap();
    assert_ne!(replacement.token.identity, previous);
    replacement.token.write_batch(|| ()).unwrap();
}

#[test]
fn a_crash_releases_the_writer_lock_and_keeps_completed_batches() {
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
    let replacement = RefreshOwnership::acquire(&base, || ()).unwrap();
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
    )
    .unwrap()
}

#[test]
fn old_ownership_doesnt_return_after_release_and_reacquisition() {
    let directory = tempfile::tempdir().unwrap();
    let base = rearview(directory.path());
    let first = RefreshOwnership::acquire(&base, || ()).unwrap();
    let replaced = replace_owner(&base);
    connect(base.path(), POLL_EVERY)
        .unwrap()
        .execute(
            "UPDATE refresh_state SET owner = NULL WHERE owner = ?1",
            [&replaced.identity],
        )
        .unwrap();
    let current = RefreshOwnership::acquire(&base, || ()).unwrap();
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
fn a_write_batch_holds_the_writer_lock_and_a_replaced_owner_writes_nothing() {
    let directory = tempfile::tempdir().unwrap();
    let base = rearview(directory.path());
    let owner = RefreshOwnership::acquire(&base, || ()).unwrap();
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
    let owner = RefreshOwnership::acquire(&base, || ()).unwrap();
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
fn takeover_between_batches_prevents_more_entries_and_the_final_status() {
    let directory = tempfile::tempdir().unwrap();
    let base = rearview(directory.path());
    let owner = RefreshOwnership::acquire(&base, || ()).unwrap();
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
fn schema_and_page_rebuilds_preserve_the_active_ownership() {
    for page_size in [1024, 4096] {
        let directory = tempfile::tempdir().unwrap();
        let base = rearview(directory.path());
        let connection = Connection::open(super::super::database_path(base.path())).unwrap();
        connection
            .pragma_update(None, "page_size", page_size)
            .unwrap();
        connection.execute_batch("CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT); INSERT INTO meta VALUES ('schema_version', 'old')").unwrap();
        drop(connection);
        let owner = RefreshOwnership::acquire(&base, || ()).unwrap();
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
fn an_admitted_database_with_the_wrong_page_size_requires_refresh() {
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
    let owner = RefreshOwnership::acquire(&base, || ()).unwrap();
    let cache = SqliteCache::open_for_refresh(owner.token()).unwrap();
    assert_eq!(
        cache
            .connection
            .pragma_query_value(None, "page_size", |row| row.get::<_, u32>(0))
            .unwrap(),
        4096
    );
    assert!(!cache.is_admitted().unwrap());
}

#[test]
fn a_committed_batch_restarts_the_takeover_window() {
    let directory = tempfile::tempdir().unwrap();
    let base = rearview(directory.path());
    let owner = RefreshOwnership::acquire(&base, || ()).unwrap();
    let connection = connect(base.path(), POLL_EVERY).unwrap();
    connection
        .execute("UPDATE refresh_state SET last_commit_ms = 0", [])
        .unwrap();
    owner.token.write_batch(|| ()).unwrap();
    assert!(finds_a_recent_owner(&base, &mut None));
}

#[test]
fn a_long_batch_blocks_takeover_and_timestamps_its_end() {
    let directory = tempfile::tempdir().unwrap();
    let base = rearview(directory.path());
    let owner = RefreshOwnership::acquire(&base, || ()).unwrap();
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
            send.send(RefreshOwnership::acquire(&waiter, || ()))
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
    let owner = RefreshOwnership::acquire(&base, || ()).unwrap();
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
            send.send(RefreshOwnership::acquire(&waiter, || ()))
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
fn a_rebuild_blocked_at_begin_exclusive_preserves_the_existing_tables() {
    let directory = tempfile::tempdir().unwrap();
    let base = rearview(directory.path());
    let owner = RefreshOwnership::acquire(&base, || ()).unwrap();
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
fn a_previous_owner_cannot_start_rebuild_maintenance() {
    let directory = tempfile::tempdir().unwrap();
    let base = rearview(directory.path());
    let owner = RefreshOwnership::acquire(&base, || ()).unwrap();
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
fn a_crashed_rebuild_without_meta_still_corrects_the_page_size() {
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
    let owner = RefreshOwnership::acquire(&base, || ()).unwrap();
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
fn ownership_doesnt_advance_without_a_batch_commit() {
    let directory = tempfile::tempdir().unwrap();
    let base = rearview(directory.path());
    let owner = RefreshOwnership::acquire(&base, || ()).unwrap();
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
        running_refresh(&base).unwrap(),
        Some(RefreshOwner::ThisProcess)
    );
    assert!(matches!(
        RefreshOwnership::try_acquire_now(&base).unwrap(),
        Acquisition::HeldBy(RefreshOwner::ThisProcess)
    ));
    assert!(finds_a_recent_owner(&base, &mut None));
    assert_eq!(owner_of_busy_database(&base), RefreshOwner::ThisProcess);
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
    let owner = RefreshOwnership::acquire(&base, || ()).unwrap();
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
fn a_failed_read_revokes_queued_writes_even_after_the_failure_clears() {
    let directory = tempfile::tempdir().unwrap();
    let base = rearview(directory.path());
    let owner = RefreshOwnership::acquire(&base, || ()).unwrap();
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
fn a_rebuild_waits_at_begin_exclusive_for_an_open_reader_while_a_waiter_polls() {
    let directory = tempfile::tempdir().unwrap();
    let base = rearview(directory.path());
    let owner = RefreshOwnership::acquire(&base, || ()).unwrap();
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
                .send(RefreshOwnership::acquire(&waiter, || ()))
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
    let owner = RefreshOwnership::acquire(&base, || ()).unwrap();
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
fn a_new_commit_restarts_monotonic_observation_after_clock_rollback() {
    let directory = tempfile::tempdir().unwrap();
    let clock = Arc::new(TestClock::default());
    let base = rearview_on(directory.path(), Arc::clone(&clock));
    let owner = RefreshOwnership::acquire(&base, || ()).unwrap();
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
fn a_blocked_commit_revokes_writes_and_rolls_back_its_batch() {
    let directory = tempfile::tempdir().unwrap();
    let base = rearview(directory.path());
    let owner = RefreshOwnership::acquire(&base, || ()).unwrap();
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
fn failure_to_begin_a_write_batch_revokes_publication() {
    let directory = tempfile::tempdir().unwrap();
    let base = rearview(directory.path());
    let owner = RefreshOwnership::acquire(&base, || ()).unwrap();
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
fn a_wait_behind_a_held_writer_lock_ends_at_the_limit_without_ownership() {
    let directory = tempfile::tempdir().unwrap();
    let base = rearview(directory.path());
    let owner = RefreshOwnership::acquire(&base, || ()).unwrap();
    let connection = connect(base.path(), POLL_EVERY).unwrap();
    let batch = transaction(&connection, owner.token()).unwrap();
    let waiter = rearview_on(directory.path(), Arc::new(TestClock::default()));

    let waited = RefreshOwnership::acquire(&waiter, || ());

    assert!(matches!(waited, Err(AppError::SessionCache(error)) if is_busy(&error)));
    commit(batch, owner.token()).unwrap();
    owner.token.write_batch(|| ()).unwrap();
}

#[test]
fn an_owners_write_waits_for_its_own_batch_past_the_limit_without_revoking_it() {
    let directory = tempfile::tempdir().unwrap();
    let base = rearview(directory.path());
    let owner = RefreshOwnership::acquire(&base, || ()).unwrap();
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
fn an_uncontended_acquisition_doesnt_start_a_wait() {
    let directory = tempfile::tempdir().unwrap();
    let base = rearview(directory.path());
    let started = std::cell::Cell::new(false);
    let owner = RefreshOwnership::acquire(&base, || started.set(true)).unwrap();
    assert!(!started.get());
    drop(owner);
}

/// Wait until `count` reaches one, failing after five seconds.
fn wait_for_one(count: &std::sync::atomic::AtomicUsize) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while count.load(Ordering::Relaxed) == 0 {
        assert!(Instant::now() < deadline, "the waiter did not start a wait");
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn a_wait_for_another_processs_refresh_starts_once() {
    let directory = tempfile::tempdir().unwrap();
    let base = rearview(directory.path());
    let other = OtherProcessOwner::hold(base.path());
    let starts = std::sync::atomic::AtomicUsize::new(0);
    thread::scope(|scope| {
        let waiter = scope
            .spawn(|| RefreshOwnership::acquire(&base, || starts.fetch_add(1, Ordering::Relaxed)));
        wait_for_one(&starts);
        thread::sleep(POLL_EVERY * 2);
        other.release();
        waiter.join().unwrap().unwrap();
    });
    assert_eq!(starts.load(Ordering::Relaxed), 1);
}

#[test]
fn a_wait_for_this_processs_own_refresh_doesnt_start_a_wait() {
    let directory = tempfile::tempdir().unwrap();
    let base = rearview(directory.path());
    let held = RefreshOwnership::acquire(&base, || ()).unwrap();
    let starts = std::sync::atomic::AtomicUsize::new(0);
    thread::scope(|scope| {
        let waiter = scope
            .spawn(|| RefreshOwnership::acquire(&base, || starts.fetch_add(1, Ordering::Relaxed)));
        thread::sleep(POLL_EVERY * 3);
        drop(held);
        waiter.join().unwrap().unwrap();
    });
    assert_eq!(starts.load(Ordering::Relaxed), 0);
}

#[test]
fn a_running_refresh_names_its_process_without_waiting() {
    let directory = tempfile::tempdir().unwrap();
    let base = rearview(directory.path());
    assert_eq!(running_refresh(&base).unwrap(), None);
    let owner = RefreshOwnership::acquire(&base, || ()).unwrap();
    assert_eq!(
        running_refresh(&base).unwrap(),
        Some(RefreshOwner::ThisProcess)
    );
    assert!(matches!(
        RefreshOwnership::try_acquire_now(&base).unwrap(),
        Acquisition::HeldBy(RefreshOwner::ThisProcess)
    ));
    drop(owner);
    assert_eq!(running_refresh(&base).unwrap(), None);

    let other = OtherProcessOwner::hold(base.path());
    assert_eq!(
        running_refresh(&base).unwrap(),
        Some(RefreshOwner::AnotherProcess)
    );
    assert!(matches!(
        RefreshOwnership::try_acquire_now(&base).unwrap(),
        Acquisition::HeldBy(RefreshOwner::AnotherProcess)
    ));
    other.release();
    assert_eq!(running_refresh(&base).unwrap(), None);
    assert!(matches!(
        RefreshOwnership::try_acquire_now(&base).unwrap(),
        Acquisition::Acquired(_)
    ));
}

#[test]
fn another_processs_batch_holding_the_writer_lock_is_reported_as_its_refresh() {
    let directory = tempfile::tempdir().unwrap();
    let base = rearview(directory.path());
    let other = OtherProcessOwner::start(base.path(), OwnerActivity::InsideBatch);
    assert_eq!(
        running_refresh(&base).unwrap(),
        Some(RefreshOwner::AnotherProcess)
    );
    drop(other);
}

#[test]
fn a_takeover_from_this_process_is_another_processs_refresh_until_it_completes() {
    let directory = tempfile::tempdir().unwrap();
    let base = rearview(directory.path());
    let held = RefreshOwnership::acquire(&base, || ()).unwrap();
    expire_last_commit(base.path());
    let other = OtherProcessOwner::hold(base.path());

    assert_eq!(
        running_refresh(&base).unwrap(),
        Some(RefreshOwner::AnotherProcess)
    );
    assert!(matches!(
        RefreshOwnership::try_acquire_now(&base).unwrap(),
        Acquisition::HeldBy(RefreshOwner::AnotherProcess)
    ));

    other.release();
    assert_eq!(
        running_refresh(&base).unwrap(),
        None,
        "the replaced ownership doesn't hide the other refresh's completion"
    );
    drop(held);
}

#[test]
fn this_processs_own_ownership_ends_its_wait_for_another_process() {
    let directory = tempfile::tempdir().unwrap();
    let base = rearview(directory.path());
    drop(RefreshOwnership::acquire(&base, || ()).unwrap());
    let connection = connect(base.path(), POLL_EVERY).unwrap();
    connection
        .execute(
            "UPDATE refresh_state SET owner = 'another process', last_commit_ms = ?1",
            [now_ms(base.clock().as_ref())],
        )
        .unwrap();
    let meter = crate::history::load_meter::LoadMeter::default();
    thread::scope(|scope| {
        let load = scope.spawn(|| RefreshOwnership::acquire(&base, || meter.start_refresh_wait()));
        let deadline = Instant::now() + Duration::from_secs(5);
        while !meter.is_waiting_for_refresh() {
            assert!(Instant::now() < deadline, "the load did not start a wait");
            thread::sleep(Duration::from_millis(10));
        }

        connection
            .execute("UPDATE refresh_state SET owner = NULL", [])
            .unwrap();
        let Acquisition::Acquired(own) = RefreshOwnership::try_acquire_now(&base).unwrap() else {
            panic!("this thread acquires ownership before the load");
        };

        let deadline = Instant::now() + Duration::from_secs(2);
        while meter.is_waiting_for_refresh() {
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
    let other = OtherProcessOwner::start(base.path(), OwnerActivity::InsideRebuild);
    let started = Instant::now();

    let lookup = super::super::InteractiveCache::at(base.clone())
        .open()
        .and_then(|cache| {
            cache
                .map(|cache| cache.find_session(Source::Pi, Path::new("session.jsonl"), &[]))
                .transpose()
        });

    assert!(matches!(lookup, Err(error) if super::super::is_busy_error(&error)));
    assert_eq!(
        running_refresh(&base).unwrap(),
        Some(RefreshOwner::AnotherProcess)
    );
    assert_eq!(owner_of_busy_database(&base), RefreshOwner::AnotherProcess);
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the reads don't wait for the rebuild"
    );
    drop(other);
}
