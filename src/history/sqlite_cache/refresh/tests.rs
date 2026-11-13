use super::*;
use crate::clock::{SystemClock, TestClock};
use crate::history::Source;
use crate::history::provider::walk::SystemCanonicaliser;
use crate::history::provider::{Fingerprint, SessionRoot, SessionStub};
use crate::history::sqlite_cache::low_space::run_deletion;
use crate::history::sqlite_cache::observer::{Observation, observe};
use crate::history::sqlite_cache::{
    CacheWrite, CacheWriter, ProcessIdentity, SessionRecord, SessionStatus, SqliteCache, is_busy,
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
        SystemCanonicaliser::shared(),
    )
}

/// The cache in `directory` as one rearview reaches it, on `clock`.
fn rearview_on(directory: &Path, clock: Arc<TestClock>) -> CacheBase {
    CacheBase::new(
        directory,
        ProcessIdentity::default(),
        clock,
        SystemCanonicaliser::shared(),
    )
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
    /// the process's own variables: a test build's `RootEnvironment` doesn't
    /// set any.
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
    /// The longest the test waits for the other rearview to acquire
    /// ownership and start its activity before it fails: a watchdog for a
    /// hang.
    const ACQUIRES_WITHIN: Duration = Duration::from_secs(60);

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
            .recv_timeout(Self::ACQUIRES_WITHIN)
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
            let cache = SqliteCache::open_for_refresh(owner.token()).unwrap();
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

/// Fail each write of a session's catalogue row to the cache in `base`, as a
/// database that refuses the same statement each time does. The failed
/// statement rolls its batch back without revoking the owner's write
/// permission.
pub(crate) fn fail_session_row_writes(base: &Path) {
    connect(base, POLL_EVERY)
        .unwrap()
        .execute_batch(
            "CREATE TRIGGER fail_session_update AFTER UPDATE ON sessions \
             BEGIN SELECT RAISE(ABORT, 'injected session write failure'); END; \
             CREATE TRIGGER fail_session_insert AFTER INSERT ON sessions \
             BEGIN SELECT RAISE(ABORT, 'injected session write failure'); END;",
        )
        .unwrap();
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

/// Fail each acquisition of refresh ownership of the cache in `base`, as a
/// database that refuses the write to its refresh state does, until
/// [`allow_acquisitions`].
pub(crate) fn fail_acquisitions(base: &Path) {
    connect(base, POLL_EVERY)
        .unwrap()
        .execute_batch(
            "CREATE TRIGGER fail_acquisition AFTER UPDATE OF owner ON refresh_state \
             WHEN NEW.owner IS NOT NULL AND NEW.owner IS NOT OLD.owner \
             BEGIN SELECT RAISE(ABORT, 'injected acquisition failure'); END;",
        )
        .unwrap();
}

/// Undo [`fail_acquisitions`].
pub(crate) fn allow_acquisitions(base: &Path) {
    connect(base, POLL_EVERY)
        .unwrap()
        .execute_batch("DROP TRIGGER fail_acquisition")
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

/// The root of [`record`]'s session, shared by every test: an existing
/// empty directory, because the cache stores a session only below a root
/// that canonicalises.
fn record_root() -> SessionRoot {
    static ROOT: std::sync::OnceLock<tempfile::TempDir> = std::sync::OnceLock::new();
    SessionRoot::new(
        ROOT.get_or_init(|| tempfile::tempdir().expect("a temporary root"))
            .path(),
    )
}

fn record() -> SessionRecord {
    record_of("session")
}

/// The record of an empty Pi session stored at `cache_key`.
pub(crate) fn record_of(cache_key: &str) -> SessionRecord {
    record_with_reason(cache_key, "skipped transcript has no visible messages")
}

/// [`record_of`], with `reason` as the warning the cache stores for the
/// session.
pub(crate) fn record_with_reason(cache_key: &str, reason: &str) -> SessionRecord {
    let root = record_root();
    let stub = SessionStub {
        locator: root.path.join(format!("{cache_key}.jsonl")),
        subagents: Vec::new(),
        cache_key: cache_key.to_owned(),
        fingerprint: Fingerprint {
            size: 100,
            modified: Some(UNIX_EPOCH + Duration::from_secs(1)),
        },
    };
    SessionRecord::unlisted(
        &crate::history::sqlite_cache::RecordPlace {
            storage: Source::Pi.provider().storage().cache(),
            source: Source::Pi,
            roots: crate::history::sqlite_cache::SessionRoots::of_system(&root),
            directories: &crate::history::path::CanonicalDirectories::of_system(),
            linker: Source::Pi.provider().storage().thread_linker(),
            reader: Source::Pi.provider().storage().transcript_reader(),
        },
        &stub,
        SessionStatus::Empty,
        reason.to_owned(),
    )
    .unwrap()
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
fn write_batch_holds_writer_lock_and_replaced_owner_doesnt_write() {
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
    let old = SqliteCache::open_for_refresh(owner.token()).unwrap();
    old.mark_admitted().unwrap();
    let original = record();
    old.write(&original).unwrap();
    let replacement = replace_owner(&base);
    let current = SqliteCache::open_for_refresh(&replacement).unwrap();
    let mut newer = record();
    newer.catalogue.fingerprint.mtime_secs = 2;
    current.write(&newer).unwrap();
    let (send, receive) = mpsc::channel();
    send.send(CacheWrite::session(original).0).unwrap();
    send.send(CacheWrite::Removed {
        storage: Source::Pi.provider().storage().cache(),
        root: record_root(),
        cache_keys: vec!["session".to_owned()],
    })
    .unwrap();
    drop(send);
    super::super::apply_writes(old, receive, None);
    assert_eq!(
        current
            .current_fingerprints(Source::Pi.provider().storage().cache(), &record_root(), &[])
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
    let cache = SqliteCache::open_for_refresh(owner.token()).unwrap();
    cache.mark_admitted().unwrap();
    let record = record();
    let (session, _) = cache.write_catalogue(&record, None).unwrap();
    let replacement = replace_owner(&base);
    assert!(matches!(
        WriteBatch::run(&cache.connection, owner.token(), |batch| {
            super::super::require_writing(batch, session)
        }),
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
        let cache = SqliteCache::open_for_refresh(owner.token()).unwrap();
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

/// An operation's parse runs as one write batch of its ownership: another
/// rearview finds the writer lock held and doesn't take the cache over
/// until the parse ends, though the last commit is older than the takeover
/// window.
#[test]
fn owners_parse_blocks_takeover_until_it_ends() {
    use crate::history::sqlite_cache::{OperationScope, Patience};
    let directory = tempfile::tempdir().unwrap();
    let base = rearview(directory.path());
    let operation = OperationScope::new(&base, Patience::UntilAcquired, Arc::default());
    operation.wait_to_parse_as_owner(|_| ()).unwrap();
    let connection = connect(base.path(), POLL_EVERY).unwrap();
    let stale = || {
        connection
            .execute("UPDATE refresh_state SET last_commit_ms = 0", [])
            .unwrap()
    };
    stale();
    let another_rearview = rearview(directory.path());

    let during_parse = operation
        .wait_to_parse_as_owner(|_| RefreshOwnership::try_acquire_now(&another_rearview).unwrap())
        .unwrap();

    assert!(
        matches!(
            during_parse,
            Acquisition::HeldBy(RefreshOwner::AnotherProcess)
        ),
        "another rearview doesn't take over during the parse"
    );
    stale();
    assert!(
        matches!(
            RefreshOwnership::try_acquire_now(&another_rearview).unwrap(),
            Acquisition::Acquired(_)
        ),
        "once the parse ends, another rearview takes the stale ownership over"
    );
}

/// A parse batch that can't start fails with its own cause, not as lost
/// ownership.
#[test]
fn owners_parse_whose_batch_cant_start_fails_with_its_cause() {
    use crate::history::sqlite_cache::{OperationScope, Patience};
    let directory = tempfile::tempdir().unwrap();
    let base = rearview(directory.path());
    let operation = OperationScope::new(&base, Patience::UntilAcquired, Arc::default());
    operation.wait_to_parse_as_owner(|_| ()).unwrap();
    connect(base.path(), POLL_EVERY)
        .unwrap()
        .execute_batch("ALTER TABLE refresh_state RENAME TO moved_refresh_state")
        .unwrap();

    let parsed = operation.wait_to_parse_as_owner(|_| ());

    let error = parsed.expect_err("the parse fails").to_string();
    assert!(
        error.contains("refresh_state"),
        "the failure names its cause: {error}"
    );
}

/// A parse whose batch commit fails, here refused by a commit hook, fails
/// with the commit's own error, not as lost ownership, once the parse ran.
#[test]
fn owners_parse_whose_commit_fails_fails_with_commits_error() {
    use crate::history::sqlite_cache::{OperationScope, Patience};
    let directory = tempfile::tempdir().unwrap();
    let base = rearview(directory.path());
    let operation = OperationScope::new(&base, Patience::UntilAcquired, Arc::default());
    operation.wait_to_parse_as_owner(|_| ()).unwrap();
    let _refused_commits =
        connection::test_hooks::on_every_connection(directory.path(), |connection| {
            connection.commit_hook(Some(|| true)).unwrap();
        });
    let has_parsed = AtomicBool::new(false);

    let parsed = operation.wait_to_parse_as_owner(|_| has_parsed.store(true, Ordering::SeqCst));

    assert!(has_parsed.load(Ordering::SeqCst), "the parse runs");
    assert!(
        matches!(parsed, Err(AppError::SessionCache(_))),
        "the parse fails with the commit's error: {parsed:?}"
    );
}

/// The panic message of a parse of an owner's batch that runs `take_turn`
/// with the owner's token, on a thread of its own. Fail the test after ten
/// seconds instead of hanging it: a parse that takes the turn waits forever
/// behind its own batch.
#[cfg(debug_assertions)]
fn panic_of_parse_running(take_turn: fn(&RefreshToken)) -> String {
    let (ended, has_ended) = mpsc::channel();
    thread::spawn(move || {
        let directory = tempfile::tempdir().unwrap();
        let base = rearview(directory.path());
        let owner = RefreshOwnership::acquire_for_test(&base).unwrap();
        let parsed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            owner.token.parse_batch(|_| take_turn(&owner.token))
        }));
        let message = parsed.err().map(|panic| {
            panic
                .downcast_ref::<&str>()
                .map(|message| (*message).to_owned())
                .or_else(|| panic.downcast_ref::<String>().cloned())
                .unwrap_or_default()
        });
        let _ = ended.send(message);
    });
    has_ended
        .recv_timeout(Duration::from_secs(10))
        .expect("the parse ends instead of waiting for its owner's turn")
        .expect("the parse panics")
}

/// A parse that takes its owner's turn again, on its own thread or on a
/// thread of the pool it spreads over, would wait forever behind its own
/// batch; debug builds panic. The pool is the test's own: without the check,
/// its threads wait forever, and the suite's global pool stays free.
#[test]
#[cfg(debug_assertions)]
fn owners_parse_taking_its_owners_turn_panics() {
    use rayon::prelude::*;
    let on_its_thread = panic_of_parse_running(|token| {
        let _ = token.check_space();
    });
    let on_the_pool = panic_of_parse_running(|token| {
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(2)
            .build()
            .unwrap();
        pool.install(|| {
            [0, 1].par_iter().for_each(|_| {
                let _ = token.check_space();
            });
        });
    });

    for message in [on_its_thread, on_the_pool] {
        assert!(
            message.contains("took the owner's turn"),
            "the parse panics on taking the turn: {message}"
        );
    }
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
    let busy_waits = connection::test_hooks::BusyWaits::record_on_this_thread();
    assert!(matches!(cache.rebuild(), Err(AppError::SessionCache(error)) if is_busy(&error)));
    assert_eq!(
        busy_waits.take().iter().sum::<Duration>(),
        super::super::WAITING_BUSY_TIMEOUT,
        "the rebuild waits out its whole wait for the exclusive lock"
    );
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

/// A drive that reports five times the floor free until `is_full` is set,
/// then nothing.
#[derive(Default)]
struct DriveFilledLater {
    is_full: std::sync::atomic::AtomicBool,
}

impl super::super::DiskSpace for DriveFilledLater {
    fn available_bytes(&self, _path: &Path) -> Option<u64> {
        Some(if self.is_full.load(Ordering::SeqCst) {
            0
        } else {
            5 * FREE_SPACE_FLOOR
        })
    }
}

#[test]
fn rebuild_on_drive_low_on_space_fails_with_low_space_and_keeps_tables() {
    let directory = tempfile::tempdir().unwrap();
    let drive = Arc::new(DriveFilledLater::default());
    let base = rearview(directory.path()).with_disk_space(drive.clone());
    let owner = RefreshOwnership::acquire_for_test(&base).unwrap();
    let cache = SqliteCache::open_for_refresh(owner.token()).unwrap();
    cache.set_meta("sentinel", "keep").unwrap();
    drive.is_full.store(true, Ordering::SeqCst);

    let rebuilt = cache.rebuild();

    assert!(
        matches!(rebuilt, Err(AppError::SessionCacheLowSpace { .. })),
        "the rebuild finds the drive low on space: {rebuilt:?}"
    );
    assert_eq!(
        cache.meta("sentinel").unwrap().as_deref(),
        Some("keep"),
        "the rebuild keeps the tables"
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
    let cache = SqliteCache::open_for_refresh(owner.token()).unwrap();
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
    let cache = SqliteCache::open_for_refresh(owner.token()).unwrap();
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
    send.send(CacheWrite::session(changed).0).unwrap();
    send.send(CacheWrite::Removed {
        storage: Source::Pi.provider().storage().cache(),
        root: record_root(),
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
                &record_root(),
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

/// The files a deletion removes beside the database.
fn side_files(base: &Path) -> Vec<PathBuf> {
    super::super::DATABASE_SIDE_FILE_SUFFIXES
        .iter()
        .map(|suffix| base.join(format!("{}{suffix}", super::super::DATABASE_FILE_NAME)))
        .collect()
}

/// The names of the tables the database in `base` holds.
fn table_names(base: &Path) -> Vec<String> {
    connect(base, POLL_EVERY)
        .unwrap()
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

/// The commit time the database in `base` stores for its owner.
fn last_commit_ms(base: &Path) -> i64 {
    connect(base, POLL_EVERY)
        .unwrap()
        .query_row(
            "SELECT last_commit_ms FROM refresh_state WHERE singleton = 1",
            [],
            |row| row.get(0),
        )
        .unwrap()
}

/// Polls until `base`'s stored commit time is `expected`, failing after ten
/// seconds.
fn wait_for_commit_at(base: &Path, expected: i64) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while last_commit_ms(base) != expected {
        assert!(
            Instant::now() < deadline,
            "the owner commits at {expected} while the deletion waits"
        );
        thread::sleep(POLL_EVERY);
    }
}

/// Ownership of a cache in `base` with this release's tables, written
/// while the drive had room: the writer's opening checks free space.
fn owner_of_written_cache(base: &CacheBase) -> RefreshOwnership {
    drop(SqliteCache::open(&base.clone().with_free_space(10 * FREE_SPACE_FLOOR)).unwrap());
    RefreshOwnership::acquire_for_test(base).unwrap()
}

#[test]
fn low_space_deletion_keeps_database_another_reader_holds_open() {
    let directory = tempfile::tempdir().unwrap();
    let base = rearview(directory.path());
    let mut owner = owner_of_written_cache(&base);
    let reader = connect(base.path(), POLL_EVERY).unwrap();
    let read = |reader: &Connection| {
        reader.query_row("SELECT COUNT(*) FROM meta", [], |row| row.get::<_, i64>(0))
    };
    read(&reader).unwrap();

    let deletion = owner.delete_database(ExclusiveWait::of(POLL_EVERY));

    assert_eq!(
        deletion,
        CacheDeletion::Deferred,
        "the deletion waits out its wait for the reader, then defers"
    );
    assert!(read(&reader).is_ok(), "the reader still reads the database");
    drop(owner);
    assert_eq!(
        observe(&base).unwrap(),
        Observation::Idle,
        "the ownership of the kept database is released"
    );
}

#[test]
fn low_space_deletion_keeps_ownership_past_takeover_window_and_empties_database_once_reader_closes()
{
    let directory = tempfile::tempdir().unwrap();
    let clock = Arc::new(TestClock::default());
    let base = rearview_on(directory.path(), Arc::clone(&clock));
    let mut owner = owner_of_written_cache(&base);
    let identity = owner.token().identity.clone();
    let reader = connect(base.path(), POLL_EVERY).unwrap();
    reader
        .query_row("SELECT COUNT(*) FROM meta", [], |row| row.get::<_, i64>(0))
        .unwrap();
    let competing = rearview_on(directory.path(), Arc::clone(&clock));
    let mut observed = None;

    thread::scope(|scope| {
        let deletion = scope.spawn(move || {
            let deletion = owner.delete_database(ExclusiveWait {
                total: Duration::from_secs(30),
                attempt: POLL_EVERY,
            });
            drop(owner);
            deletion
        });
        assert!(
            !matches!(
                try_acquire(&competing, STALE_AFTER, &mut observed).unwrap(),
                Attempt::Acquired(_)
            ),
            "a competing owner doesn't acquire the fresh ownership"
        );
        clock.advance(STALE_AFTER);
        wait_for_commit_at(base.path(), now_ms(clock.as_ref()));
        assert!(
            !matches!(
                try_acquire(&competing, STALE_AFTER, &mut observed).unwrap(),
                Attempt::Acquired(_)
            ),
            "the deletion's commits keep a competing owner out past the takeover window"
        );
        drop(reader);
        assert_eq!(
            deletion.join().unwrap(),
            CacheDeletion::Deleted,
            "the deletion completes once the reader closes"
        );
    });

    assert_eq!(
        table_names(base.path()),
        ["refresh_state"],
        "the database keeps the refresh's state alone"
    );
    for file in side_files(base.path()) {
        assert!(!file.exists(), "{} is removed", file.display());
    }
    assert!(
        !base.process().holds(&identity),
        "this process no longer holds the deletion's ownership"
    );
    assert!(
        matches!(
            RefreshOwnership::try_acquire_now(&competing).unwrap(),
            Acquisition::Acquired(_)
        ),
        "the competing owner acquires the emptied cache at once"
    );
}

#[test]
fn low_space_deletion_that_fails_reports_the_database_without_verbatim_prefix_and_the_cause() {
    let directory = tempfile::tempdir().unwrap();
    let base = rearview(directory.path());
    let mut owner = owner_of_written_cache(&base);
    // A directory where the emptying writes its rollback journal: on every
    // platform, SQLite can't create the journal file there.
    let journal = side_files(base.path())
        .into_iter()
        .find(|file| file.to_string_lossy().ends_with("-journal"))
        .unwrap();
    std::fs::create_dir(&journal).unwrap();

    let deletion = owner.delete_database(ExclusiveWait::of(POLL_EVERY));

    let CacheDeletion::Failed { file, error } = &deletion else {
        panic!("the deletion fails: {deletion:?}");
    };
    assert_eq!(
        Path::new(file),
        without_verbatim_prefix(&super::super::database_path(
            &base.canonical_path().unwrap()
        )),
        "the failure names the database"
    );
    assert!(
        !file.starts_with(r"\\?\"),
        "the database is spelled without the verbatim prefix: {file}"
    );
    assert!(!error.is_empty(), "the failure carries its cause");
}

/// A low-space deletion over a write-ahead log that holds a load's writes
/// empties the database before it leaves WAL mode: leaving it copies the log
/// up to the emptied database's size, the file doesn't grow by any of the
/// load's pages, and VACUUM shrinks it to a few pages.
#[test]
fn low_space_deletion_over_full_log_leaves_wal_without_growing_database() {
    let directory = tempfile::tempdir().unwrap();
    let base = rearview(directory.path());
    let mut owner = owner_of_written_cache(&base);
    let database = super::super::database_path(base.path());
    leave_a_load_in_the_log(base.path());
    let size_before = file_size(&database);
    SIZES_AFTER_LEAVING_WAL.with_borrow_mut(Vec::clear);

    let deletion = owner.delete_database(ExclusiveWait::of(POLL_EVERY));

    assert_eq!(deletion, CacheDeletion::Deleted);
    let sizes = SIZES_AFTER_LEAVING_WAL.with_borrow(Clone::clone);
    assert!(
        !sizes.is_empty() && sizes.iter().all(|&size| size <= size_before),
        "leaving WAL mode copies none of the log's load into the database: {sizes:?} against \
         {size_before} bytes before"
    );
    let size_after = file_size(&database);
    assert!(
        size_after <= 16 * u64::from(super::super::PAGE_SIZE),
        "VACUUM shrinks the emptied database: {size_after} bytes"
    );
}

/// Writes of 4 MB to the database in `base`, left in its write-ahead log:
/// the connection making them closes without copying the log.
fn leave_a_load_in_the_log(base: &Path) {
    let writer = connect(base, POLL_EVERY).unwrap();
    writer
        .set_db_config(
            rusqlite::config::DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE,
            true,
        )
        .unwrap();
    writer
        .execute(
            "INSERT INTO meta (key, value) VALUES ('filler', ?1)",
            ["x".repeat(4_000_000)],
        )
        .unwrap();
}

fn file_size(path: &Path) -> u64 {
    std::fs::metadata(path).map_or(0, |metadata| metadata.len())
}

/// Once a write found the drive low on space, the checkpoint after the
/// writes, the cache writer's close, the ownership's release and a refresh
/// connection closing last leave the write-ahead log uncopied: copying it
/// would grow the database by the load's pages.
#[test]
fn checkpoints_after_drive_found_low_on_space_leave_log_uncopied() {
    let directory = tempfile::tempdir().unwrap();
    // The writer's opening checks free space once, for its schema batch,
    // and passes; the next check finds the drive low on space.
    let base = rearview(directory.path())
        .with_disk_space(Arc::new(super::super::DriveRunningOut::after(1, || {})));
    let owner = owner_of_written_cache(&base);
    let database = super::super::database_path(base.path());
    let writer = SqliteCache::open_for_refresh(owner.token()).unwrap();
    leave_a_load_in_the_log(base.path());
    let size_before = file_size(&database);
    owner.token().check_space().unwrap_err();

    writer.checkpoint_after_writes().unwrap();
    let after_checkpoint = file_size(&database);
    drop(writer);
    let after_writer = file_size(&database);
    drop(owner);
    let after_release = file_size(&database);
    drop(connect_writable(&base, POLL_EVERY).unwrap());
    let after_refresh_connection = file_size(&database);

    assert_eq!(
        (
            after_checkpoint,
            after_writer,
            after_release,
            after_refresh_connection
        ),
        (size_before, size_before, size_before, size_before),
        "the checkpoint, the writer, the release and a refresh connection leave the database at \
         its size"
    );
}

/// A low-space deletion whose emptying fails, here at its DROP, leaves the
/// database at its size: its connection closes without copying the
/// write-ahead log's load into the database.
#[test]
fn failed_low_space_deletion_leaves_database_at_its_size() {
    use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
    let directory = tempfile::tempdir().unwrap();
    let base = rearview(directory.path())
        .with_free_space(900_000_000)
        .with_low_space_wait(Duration::from_secs(5));
    let database = super::super::database_path(base.path());
    let owner = owner_of_written_cache(&base);
    leave_a_load_in_the_log(base.path());
    let size_before = file_size(&database);
    owner.token().check_space().unwrap_err();
    drop(owner);
    let _refused_drop =
        connection::test_hooks::on_every_connection(directory.path(), |connection| {
            connection
                .authorizer(Some(|context: AuthContext<'_>| {
                    if matches!(context.action, AuthAction::DropTable { .. }) {
                        Authorization::Deny
                    } else {
                        Authorization::Allow
                    }
                }))
                .unwrap();
        });

    let outcome = run_deletion(&base);

    assert!(
        matches!(outcome, Some(CacheDeletion::Failed { .. })),
        "the emptying fails: {outcome:?}"
    );
    assert_eq!(
        file_size(&database),
        size_before,
        "the failed deletion leaves the database at its size"
    );
}

/// Once a write found the drive low on space, a cache writer whose setup
/// fails after `journal_mode = WAL` opened the write-ahead log, here at
/// `foreign_keys`, leaves the database at its size: its connection closes
/// without copying the log into the database.
#[test]
fn writer_setup_failing_after_log_opens_leaves_database_at_its_size() {
    use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
    let directory = tempfile::tempdir().unwrap();
    let base = rearview(directory.path()).with_free_space(900_000_000);
    let owner = owner_of_written_cache(&base);
    let database = super::super::database_path(base.path());
    leave_a_load_in_the_log(base.path());
    let size_before = file_size(&database);
    owner.token().check_space().unwrap_err();
    let _refused_foreign_keys =
        connection::test_hooks::on_every_connection(directory.path(), |connection| {
            connection
                .authorizer(Some(|context: AuthContext<'_>| {
                    if matches!(
                        context.action,
                        AuthAction::Pragma {
                            pragma_name: "foreign_keys",
                            ..
                        }
                    ) {
                        Authorization::Deny
                    } else {
                        Authorization::Allow
                    }
                }))
                .unwrap();
        });

    let opened = SqliteCache::open_for_refresh(owner.token());

    assert!(opened.is_err(), "the writer's setup fails");
    assert_eq!(
        file_size(&database),
        size_before,
        "the writer's connection leaves the database at its size"
    );
}

/// A low-space deletion whose emptying outlasts the takeover window commits
/// its ownership again before its exclusive lock ends: a competing rearview
/// takes the emptied cache over only once the deletion's ownership ends.
#[test]
fn low_space_deletion_outlasting_takeover_window_keeps_ownership_until_it_ends() {
    use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
    let directory = tempfile::tempdir().unwrap();
    let clock = Arc::new(TestClock::default());
    let base = rearview_on(directory.path(), Arc::clone(&clock));
    let mut owner = owner_of_written_cache(&base);
    let competing = rearview_on(directory.path(), Arc::clone(&clock));
    let aging = Arc::clone(&clock);
    let _slow_emptying =
        connection::test_hooks::on_every_connection(directory.path(), move |connection| {
            let aging = Arc::clone(&aging);
            connection
                .authorizer(Some(move |context: AuthContext<'_>| {
                    if matches!(context.action, AuthAction::DropTable { .. }) {
                        aging.advance(STALE_AFTER);
                    }
                    Authorization::Allow
                }))
                .unwrap();
        });

    let deletion = owner.delete_database(ExclusiveWait::of(POLL_EVERY));

    assert_eq!(deletion, CacheDeletion::Deleted);
    assert!(
        !matches!(
            try_acquire(&competing, STALE_AFTER, &mut None).unwrap(),
            Attempt::Acquired(_)
        ),
        "a competing rearview doesn't take the cache over while the deletion's ownership holds"
    );
    drop(owner);
    assert!(
        matches!(
            RefreshOwnership::try_acquire_now(&competing).unwrap(),
            Acquisition::Acquired(_)
        ),
        "the competing rearview acquires the emptied cache once the ownership ends"
    );
}

/// A low-space deletion removes the files SQLite keeps beside the database
/// before its exclusive lock ends. A competing rearview takes the cache over
/// once the lock ends, past the takeover window: its write-ahead log stays,
/// and the deletion doesn't report a failure to remove it (Windows refuses
/// the removal of an open file; Unix would unlink it).
#[test]
fn low_space_deletion_leaves_log_competitor_writes_once_its_lock_ends() {
    let directory = tempfile::tempdir().unwrap();
    let clock = Arc::new(TestClock::default());
    let base = rearview_on(directory.path(), Arc::clone(&clock));
    let mut owner = owner_of_written_cache(&base);
    let competing = rearview_on(directory.path(), Arc::clone(&clock));
    let database = super::super::database_path(directory.path());
    let log = PathBuf::from(format!("{}-wal", database.display()));
    let competitor = std::rc::Rc::new(std::cell::RefCell::new(None));
    let held = std::rc::Rc::clone(&competitor);
    AFTER_DELETION_LOCK_ENDS.set(Some(Box::new(move || {
        clock.advance(STALE_AFTER);
        let Acquisition::Acquired(ownership) =
            RefreshOwnership::try_acquire_now(&competing).unwrap()
        else {
            panic!("the competing rearview takes the cache over once the lock ends");
        };
        let writer = rusqlite::Connection::open(&database).unwrap();
        writer
            .query_row("PRAGMA journal_mode = WAL", [], |_| Ok(()))
            .unwrap();
        writer
            .execute(
                "UPDATE refresh_state SET last_commit_ms = last_commit_ms",
                [],
            )
            .unwrap();
        *held.borrow_mut() = Some((ownership, writer));
    })));

    let deletion = owner.delete_database(ExclusiveWait::of(POLL_EVERY));

    assert_eq!(deletion, CacheDeletion::Deleted);
    assert!(
        competitor.borrow().is_some(),
        "the competing rearview wrote once the lock ended"
    );
    assert!(log.exists(), "the competing rearview's log stays");
}

/// A low-space deletion of one cache doesn't wait for this rearview's
/// connections to another cache, and doesn't refuse its new ones.
#[test]
fn low_space_deletion_of_one_cache_leaves_this_rearviews_other_cache_alone() {
    let process = ProcessIdentity::default();
    let directories = [tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap()];
    let [deleted, other] = directories.each_ref().map(|directory| {
        CacheBase::new(
            directory.path(),
            process.clone(),
            Arc::new(SystemClock::new()),
            SystemCanonicaliser::shared(),
        )
        .with_free_space(900_000_000)
        .with_low_space_wait(Duration::from_secs(30))
    });
    drop(owner_of_written_cache(&other));
    latch_low_space_as_first_write_does(&deleted);
    let held_open = SqliteCache::open_waiting(&other).unwrap().unwrap();

    let (outcome, opened_meanwhile) = thread::scope(|scope| {
        let running = scope.spawn(|| run_deletion(&deleted));
        let opened = SqliteCache::open_waiting(&other);
        (running.join().unwrap(), opened)
    });

    assert_eq!(
        outcome,
        Some(CacheDeletion::Deleted),
        "the deletion empties its cache while the other cache's connection stays open"
    );
    assert!(
        matches!(opened_meanwhile, Ok(Some(_))),
        "the other cache opens during the deletion"
    );
    drop(held_open);
}

/// A low-space deletion that finds the database file gone reports it
/// deleted, and doesn't create it again.
#[test]
fn low_space_deletion_of_missing_database_reports_it_deleted_without_creating_it() {
    let directory = tempfile::tempdir().unwrap();
    let base = rearview(directory.path())
        .with_free_space(900_000_000)
        .with_low_space_wait(Duration::from_secs(1));
    latch_low_space_as_first_write_does(&base);
    let database = super::super::database_path(&base.canonical_path().unwrap());
    std::fs::remove_file(&database).unwrap();
    for file in side_files(base.path()) {
        let _ = std::fs::remove_file(file);
    }

    assert_eq!(run_deletion(&base), Some(CacheDeletion::Deleted));
    assert!(
        !database.exists(),
        "the deletion doesn't create the database"
    );
}

/// A low-space deletion on a thread that holds one of this rearview's
/// connections to the cache fails its debug check: it would wait for that
/// connection to the end of its wait.
#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "the thread refusing connections doesn't hold any of them open")]
fn low_space_deletion_on_thread_holding_its_connection_fails_debug_check() {
    let directory = tempfile::tempdir().unwrap();
    let base = rearview(directory.path())
        .with_free_space(900_000_000)
        .with_low_space_wait(Duration::from_secs(1));
    latch_low_space_as_first_write_does(&base);
    let _held_open = SqliteCache::open_waiting(&base).unwrap().unwrap();

    run_deletion(&base);
}

/// A connection moved to another thread and used there is held there: the
/// thread it left passes the low-space deletion's debug check. Moved back
/// and used, it is held by that thread again, and the check fails there.
#[cfg(debug_assertions)]
#[test]
fn connection_moved_and_used_is_held_by_its_new_thread_in_debug_check() {
    let directory = tempfile::tempdir().unwrap();
    let base = rearview(directory.path());
    drop(owner_of_written_cache(&base));
    let cache = SqliteCache::open_waiting(&base).unwrap().unwrap();

    let cache = thread::scope(|scope| {
        let (used, is_used) = mpsc::channel();
        let (give_back, given_back) = mpsc::channel::<()>();
        let holder = scope.spawn(move || {
            cache.is_admitted().unwrap();
            used.send(()).unwrap();
            let _ = given_back.recv_timeout(Duration::from_secs(10));
            cache
        });
        is_used
            .recv_timeout(Duration::from_secs(10))
            .expect("the other thread uses the connection");
        let refused = base.refuse_connections(Duration::ZERO);
        assert!(
            !refused.all_closed,
            "the connection the other thread holds stays open"
        );
        drop(refused);
        give_back.send(()).unwrap();
        holder.join().unwrap()
    });
    cache.is_admitted().unwrap();

    let refusal = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        base.refuse_connections(Duration::ZERO)
    }));
    assert!(
        refusal.is_err(),
        "the thread using the connection again fails the debug check"
    );
}

/// Sets `base`'s low-space latch as a first write on a drive low on
/// space does: the write's ownership checks the space, fails, and is
/// released.
fn latch_low_space_as_first_write_does(base: &CacheBase) {
    let owner = owner_of_written_cache(base);
    owner.token().check_space().unwrap_err();
}

/// A free-space check on a drive low on space waits for its owner's turn
/// while a parse group holds it, and the TUI's quit revokes the write
/// permission meanwhile. The check fails without setting the low-space
/// latch, so the quit's read of the latch is final.
#[test]
fn space_check_waiting_for_turn_when_quit_revokes_permission_leaves_latch_clear() {
    let directory = tempfile::tempdir().unwrap();
    let base = rearview(directory.path()).with_free_space(900_000_000);
    let owner = RefreshOwnership::acquire_for_test(&base).unwrap();
    let token = owner.token();
    let parse_group = token.take_turn();

    let checked = thread::scope(|scope| {
        let check = scope.spawn(|| token.check_space());
        let deadline = Instant::now() + Duration::from_secs(10);
        while token.threads_waiting_for_turn() == 0 {
            assert!(Instant::now() < deadline, "the check waits for the turn");
            thread::yield_now();
        }
        token.revoke();
        drop(parse_group);
        check.join().unwrap()
    });

    assert!(
        matches!(checked, Err(AppError::SessionCacheRefreshLost)),
        "the check fails as revoked: {checked:?}"
    );
    assert!(!base.low_space().is_set(), "the latch stays clear");
}

/// A free-space check on a drive low on space outlasts the TUI exit's wait
/// for it and answers once the wait gave up. The check fails and leaves the
/// low-space latch clear, as rearview read it at exit.
#[test]
fn space_check_answering_after_the_exits_wait_leaves_latch_clear() {
    let directory = tempfile::tempdir().unwrap();
    let (checking, is_checking) = mpsc::channel::<()>();
    let (answer, answered) = mpsc::channel::<()>();
    let answered = Mutex::new(answered);
    let drive = super::super::DriveRunningOut::after(0, move || {
        checking.send(()).unwrap();
        let _ = answered
            .lock()
            .unwrap()
            .recv_timeout(Duration::from_secs(10));
    });
    let base = rearview(directory.path()).with_disk_space(Arc::new(drive));
    let owner = RefreshOwnership::acquire_for_test(&base).unwrap();
    let token = owner.token();

    let checked = thread::scope(|scope| {
        let check = scope.spawn(|| token.check_space());
        is_checking
            .recv_timeout(Duration::from_secs(10))
            .expect("the check reads the drive's free space");
        token.revoke();
        base.low_space().wait_for_checks(Duration::from_millis(50));
        let latch_at_exit = base.low_space().is_set();
        answer.send(()).unwrap();
        (latch_at_exit, check.join().unwrap())
    });

    assert!(!checked.0, "the latch is clear when the wait gives up");
    assert!(
        matches!(checked.1, Err(AppError::SessionCacheRefreshLost)),
        "the check fails as revoked: {:?}",
        checked.1
    );
    assert!(!base.low_space().is_set(), "the latch stays clear");
}

/// The TUI's shutdown while this rearview's own write batch holds SQLite's
/// writer lock tries the ownership's release once, without a busy wait: the
/// batch keeps the lock until it ends.
#[test]
fn shutdown_during_own_write_batch_releases_without_busy_wait() {
    let directory = tempfile::tempdir().unwrap();
    let base = rearview(directory.path());
    let owner = RefreshOwnership::acquire_for_test(&base).unwrap();
    let (in_batch, is_in_batch) = mpsc::channel();
    let (end_batch, batch_may_end) = mpsc::channel::<()>();

    let waits = thread::scope(|scope| {
        scope.spawn(move || {
            owner.token().write_batch(|| {
                in_batch.send(()).unwrap();
                let _ = batch_may_end.recv_timeout(Duration::from_secs(10));
            })
        });
        is_in_batch
            .recv_timeout(Duration::from_secs(10))
            .expect("the write batch begins");
        let busy_waits = connection::test_hooks::BusyWaits::record_on_this_thread();
        super::super::coordinator::Coordinator::of(&base)
            .shutdown(super::super::SHUTDOWN_RELEASE_WAIT);
        let waits = busy_waits.take();
        end_batch.send(()).unwrap();
        waits
    });

    assert_eq!(
        waits,
        [Duration::ZERO],
        "the shutdown tries the release once, without waiting for the writer lock"
    );
}

/// While its low-space deletion runs, this rearview's new connections to the
/// cache are refused. A reader opening each connection before closing the
/// one it holds closes it on the refusal, so the deletion empties the
/// database; the reader's next open finds the emptied cache.
#[test]
fn low_space_deletion_refuses_this_rearviews_new_connections_and_empties_database() {
    let directory = tempfile::tempdir().unwrap();
    let base = rearview(directory.path())
        .with_free_space(900_000_000)
        .with_low_space_wait(Duration::from_secs(5));
    latch_low_space_as_first_write_does(&base);
    let (reader_started, has_reader_started) = mpsc::channel();
    let reader_base = base.clone();

    let (outcome, (refused, opened_after_deletion)) = thread::scope(|scope| {
        let reader = scope.spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut held = SqliteCache::open_waiting(&reader_base).unwrap();
            reader_started.send(()).unwrap();
            let mut refused = false;
            while Instant::now() < deadline {
                match SqliteCache::open_waiting(&reader_base) {
                    Ok(Some(next)) => held = Some(next),
                    Ok(None) => return (refused, true),
                    Err(AppError::SessionCacheBeingDeleted) => {
                        refused = true;
                        held = None;
                        thread::sleep(Duration::from_millis(1));
                    }
                    Err(error) => panic!("the reader opens the cache: {error}"),
                }
            }
            drop(held);
            (refused, false)
        });
        has_reader_started
            .recv_timeout(Duration::from_secs(10))
            .expect("the reader opens the cache");
        let outcome = run_deletion(&base);
        (outcome, reader.join().unwrap())
    });

    assert_eq!(
        outcome,
        Some(CacheDeletion::Deleted),
        "the deletion empties the database once the reader's connection closes"
    );
    assert!(refused, "the reader's open is refused during the deletion");
    assert!(
        opened_after_deletion,
        "the reader's next open finds the emptied cache"
    );
    assert_eq!(table_names(base.path()), ["refresh_state"]);
}

/// Wait until `base`'s low-space deletion refuses this rearview's new
/// connections: from then on it waits for the open ones to close.
fn wait_for_refusal(base: &CacheBase) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while base.open_connection().is_ok() {
        assert!(
            Instant::now() < deadline,
            "the deletion refuses new connections"
        );
        thread::sleep(Duration::from_millis(1));
    }
}

/// While a low-space deletion waits for this rearview's reader, held open
/// past the takeover window, it doesn't hold an ownership that could
/// lapse: a competing rearview takes the cache over and writes, and once
/// both are done the deletion acquires the ownership and empties the
/// database.
#[test]
fn low_space_deletion_waiting_for_reader_past_takeover_window_empties_database_competitor_wrote() {
    let directory = tempfile::tempdir().unwrap();
    let clock = Arc::new(TestClock::default());
    let base = rearview_on(directory.path(), Arc::clone(&clock))
        .with_free_space(900_000_000)
        .with_low_space_wait(Duration::from_secs(10));
    latch_low_space_as_first_write_does(&base);
    let reader = SqliteCache::open_waiting(&base).unwrap().unwrap();
    let competing = rearview_on(directory.path(), Arc::clone(&clock));

    let outcome = thread::scope(|scope| {
        let running = scope.spawn(|| run_deletion(&base));
        wait_for_refusal(&base);
        clock.advance(STALE_AFTER);
        let competitor = RefreshOwnership::acquire_for_test(&competing).unwrap();
        connect(competing.path(), POLL_EVERY)
            .unwrap()
            .execute(
                "INSERT INTO meta (key, value) VALUES ('competitor', 'wrote')",
                [],
            )
            .unwrap();
        drop(competitor);
        drop(reader);
        running.join().unwrap()
    });

    assert_eq!(outcome, Some(CacheDeletion::Deleted));
    assert_eq!(table_names(base.path()), ["refresh_state"]);
    assert!(
        matches!(
            RefreshOwnership::try_acquire_now(&competing).unwrap(),
            Acquisition::Acquired(_)
        ),
        "the competing rearview acquires the emptied cache at once"
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

/// An owner of `base`'s cache, and a refresh connection to it whose
/// database holds one `sentinel` row of value 1.
fn owner_and_sentinel(base: &CacheBase) -> (RefreshOwnership, Connection) {
    let owner = RefreshOwnership::acquire_for_test(base).unwrap();
    let connection = connect(base.path(), Duration::ZERO).unwrap();
    connection
        .execute_batch("CREATE TABLE sentinel (value INTEGER); INSERT INTO sentinel VALUES (1)")
        .unwrap();
    (owner, connection)
}

fn sentinel(connection: &Connection) -> i64 {
    connection
        .query_row("SELECT value FROM sentinel", [], |row| row.get(0))
        .unwrap()
}

fn class_of<T>(ran: &Result<T>) -> Option<WriteErrorClass> {
    ran.as_ref().err().map(WriteErrorClass::of)
}

#[test]
fn batch_that_cant_begin_doesnt_run_body_and_revokes_write_permission() {
    let directory = tempfile::tempdir().unwrap();
    let (owner, connection) = owner_and_sentinel(&rearview(directory.path()));
    connection.execute_batch("BEGIN").unwrap();

    let ran = WriteBatch::run(&connection, owner.token(), |_| -> Result<()> {
        panic!("a batch that cannot begin doesn't run its body")
    });

    connection.execute_batch("ROLLBACK").unwrap();
    assert_eq!(
        class_of(&ran),
        Some(WriteErrorClass::Io),
        "SQLite refuses a BEGIN inside a transaction: {ran:?}"
    );
    assert!(
        !owner.token().can_publish(),
        "a batch that cannot begin revokes the owner's write permission"
    );
}

#[test]
fn batch_whose_ownership_check_fails_doesnt_run_body_and_revokes_write_permission() {
    let directory = tempfile::tempdir().unwrap();
    let (owner, connection) = owner_and_sentinel(&rearview(directory.path()));
    connection
        .execute_batch("ALTER TABLE refresh_state RENAME TO moved_state")
        .unwrap();

    let ran = WriteBatch::run(&connection, owner.token(), |_| -> Result<()> {
        panic!("a batch whose ownership check fails doesn't run its body")
    });

    assert_eq!(
        class_of(&ran),
        Some(WriteErrorClass::Io),
        "the ownership check fails reading a missing table: {ran:?}"
    );
    assert!(
        !owner.token().can_publish(),
        "a failed ownership check revokes the owner's write permission"
    );
}

#[test]
fn batch_whose_body_fails_rolls_back_and_revokes_write_permission_by_class() {
    let failures = [
        ("INSERT INTO missing VALUES (1)", WriteErrorClass::Io, false),
        (
            "INSERT INTO refresh_state (singleton, last_commit_ms) VALUES (1, 0)",
            WriteErrorClass::Other,
            true,
        ),
    ];
    for (statement, class, keeps_write_permission) in failures {
        let directory = tempfile::tempdir().unwrap();
        let (owner, connection) = owner_and_sentinel(&rearview(directory.path()));

        let ran = WriteBatch::run(&connection, owner.token(), |batch| {
            batch.execute("UPDATE sentinel SET value = 2", [])?;
            batch.execute(statement, [])?;
            Ok(())
        });

        assert_eq!(
            class_of(&ran),
            Some(class),
            "`{statement}` fails in the body: {ran:?}"
        );
        assert_eq!(sentinel(&connection), 1, "the body's batch rolls back");
        assert_eq!(
            owner.token().can_publish(),
            keeps_write_permission,
            "a failure of class {class:?} in the body {} the owner's write permission",
            if keeps_write_permission {
                "keeps"
            } else {
                "revokes"
            }
        );
    }
}

#[test]
fn batch_whose_commit_is_refused_rolls_back_and_revokes_write_permission() {
    let directory = tempfile::tempdir().unwrap();
    let (owner, mut connection) = owner_and_sentinel(&rearview(directory.path()));
    connection.commit_hook(Some(|| true)).unwrap();

    let ran = WriteBatch::run(&connection, owner.token(), |batch| {
        Ok(batch.execute("UPDATE sentinel SET value = 2", [])?)
    });

    assert_eq!(
        class_of(&ran),
        Some(WriteErrorClass::Io),
        "the commit hook refuses the commit: {ran:?}"
    );
    assert_eq!(sentinel(&connection), 1, "the refused batch rolls back");
    assert!(
        !owner.token().can_publish(),
        "a refused commit revokes the owner's write permission"
    );
}

#[test]
fn batch_whose_commit_time_isnt_recorded_rolls_back_and_revokes_write_permission() {
    let directory = tempfile::tempdir().unwrap();
    let (owner, connection) = owner_and_sentinel(&rearview(directory.path()));
    connection
        .execute_batch(
            "CREATE TRIGGER refuse_commit_time BEFORE UPDATE ON refresh_state \
             BEGIN SELECT RAISE(ABORT, 'refused'); END;",
        )
        .unwrap();

    let ran = WriteBatch::run(&connection, owner.token(), |batch| {
        Ok(batch.execute("UPDATE sentinel SET value = 2", [])?)
    });

    assert_eq!(
        class_of(&ran),
        Some(WriteErrorClass::Other),
        "a trigger refuses the commit time's update: {ran:?}"
    );
    assert_eq!(sentinel(&connection), 1, "the batch rolls back");
    assert!(
        !owner.token().can_publish(),
        "a commit time that can't be recorded revokes the owner's write permission"
    );
}

/// SQLite's full-disk error at the commit time's update is the low-space
/// error, its latch set before the write permission is revoked. The drive
/// reports more than the floor free; the error reports less.
#[test]
fn batch_whose_commit_time_finds_disk_full_fails_with_low_space() {
    let directory = tempfile::tempdir().unwrap();
    let base = rearview(directory.path()).with_free_space(5 * FREE_SPACE_FLOOR);
    let (owner, connection) = owner_and_sentinel(&base);
    connection
        .execute_batch(
            "CREATE TABLE filler (data BLOB); \
             CREATE TRIGGER fill_at_commit_time AFTER UPDATE ON refresh_state \
             BEGIN INSERT INTO filler VALUES (randomblob(200000)); END;",
        )
        .unwrap();

    let ran = WriteBatch::run(&connection, owner.token(), |batch| {
        let pages: i64 = batch.query_row("PRAGMA page_count", [], |row| row.get(0))?;
        batch.query_row(&format!("PRAGMA max_page_count = {pages}"), [], |_| Ok(()))?;
        Ok(())
    });

    let Err(AppError::SessionCacheLowSpace { free_bytes, .. }) = ran else {
        panic!("the full disk at the commit time's update is low space: {ran:?}");
    };
    assert!(
        free_bytes < FREE_SPACE_FLOOR,
        "the error reports less than the floor free: {free_bytes}"
    );
    assert!(base.low_space().is_set(), "the low-space latch is set");
    assert!(
        !owner.token().can_publish(),
        "the full disk revokes the owner's write permission"
    );
}

#[test]
fn batch_whose_commit_fails_foreign_key_rolls_back_and_keeps_write_permission() {
    let directory = tempfile::tempdir().unwrap();
    let (owner, connection) = owner_and_sentinel(&rearview(directory.path()));
    connection
        .execute_batch(
            "PRAGMA foreign_keys = ON;
             CREATE TABLE parents (id INTEGER PRIMARY KEY);
             CREATE TABLE children (parent INTEGER REFERENCES parents (id)
                 DEFERRABLE INITIALLY DEFERRED);",
        )
        .unwrap();

    let ran = WriteBatch::run(&connection, owner.token(), |batch| {
        batch.execute("UPDATE sentinel SET value = 2", [])?;
        Ok(batch.execute("INSERT INTO children (parent) VALUES (1)", [])?)
    });

    assert_eq!(
        class_of(&ran),
        Some(WriteErrorClass::Other),
        "the commit fails a deferred foreign key: {ran:?}"
    );
    assert_eq!(sentinel(&connection), 1, "the failed batch rolls back");
    assert!(
        owner.token().can_publish(),
        "a foreign-key failure at the commit keeps the owner's write permission"
    );
}

#[test]
fn batch_on_drive_low_on_space_doesnt_begin() {
    let directory = tempfile::tempdir().unwrap();
    let base = rearview(directory.path())
        .with_disk_space(Arc::new(super::super::DriveRunningOut::after(0, || {})));
    let owner = RefreshOwnership::acquire_for_test(&base).unwrap();
    let connection = connect(base.path(), Duration::ZERO).unwrap();

    let ran = WriteBatch::run(&connection, owner.token(), |_| -> Result<()> {
        panic!("a batch on a drive low on space doesn't run its body")
    });

    assert_eq!(
        class_of(&ran),
        Some(WriteErrorClass::LowSpace),
        "the free-space check fails first: {ran:?}"
    );
    assert!(
        !owner.token().can_publish(),
        "low space revokes the owner's write permission"
    );
}

#[test]
fn batch_whose_body_doesnt_find_anything_rolls_back_without_commit() {
    let directory = tempfile::tempdir().unwrap();
    let clock = Arc::new(TestClock::default());
    let (owner, connection) =
        owner_and_sentinel(&rearview_on(directory.path(), Arc::clone(&clock)));
    let last_commit = || -> i64 {
        connection
            .query_row("SELECT last_commit_ms FROM refresh_state", [], |row| {
                row.get(0)
            })
            .unwrap()
    };
    let committed_before = last_commit();
    clock.advance(Duration::from_secs(1));

    let ran = WriteBatch::run_or_roll_back(&connection, owner.token(), |batch| {
        batch.execute("UPDATE sentinel SET value = 2", [])?;
        Ok(None::<()>)
    });

    assert!(
        matches!(ran, Ok(None)),
        "the body doesn't find anything to write: {ran:?}"
    );
    assert_eq!(sentinel(&connection), 1, "the batch rolls back");
    assert_eq!(
        last_commit(),
        committed_before,
        "a batch whose body doesn't find anything doesn't commit"
    );
    assert!(
        owner.token().can_publish(),
        "the owner keeps its write permission"
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

/// A write batch of an owner the TUI's quit revoked fails at once while
/// another connection holds SQLite's writer lock.
#[test]
fn revoked_owners_write_batch_fails_without_waiting_for_held_writer_lock() {
    let directory = tempfile::tempdir().unwrap();
    let base = rearview(directory.path());
    let owner = RefreshOwnership::acquire_for_test(&base).unwrap();
    let holder = connect(base.path(), Duration::ZERO).unwrap();
    holder.execute_batch("BEGIN IMMEDIATE").unwrap();
    owner.token().revoke();
    let busy_waits = connection::test_hooks::BusyWaits::record_on_this_thread();
    let connection = connect(base.path(), POLL_EVERY).unwrap();

    let batch = transaction_within(&connection, owner.token(), POLL_EVERY * 2);

    assert!(
        matches!(batch, Err(AppError::SessionCacheRefreshLost)),
        "the batch fails as revoked: {:?}",
        batch.err()
    );
    assert_eq!(
        busy_waits.take(),
        Vec::<Duration>::new(),
        "the batch doesn't wait for the writer lock"
    );
    holder.execute_batch("ROLLBACK").unwrap();
}

#[test]
fn uncontended_acquisition_doesnt_publish_wait() {
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
fn wait_for_this_processs_own_refresh_doesnt_publish_wait() {
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

#[test]
fn exclusive_lock_released_within_open_wait_doesnt_publish_rebuild() {
    let directory = tempfile::tempdir().unwrap();
    let base = rearview(directory.path());
    SqliteCache::open(&base).unwrap().mark_admitted().unwrap();
    let rebuild = exclusive_lock(base.path());
    super::super::connection::test_hooks::at_the_first_busy_wait(move || drop(rebuild));
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
    let rebuild = exclusive_lock(base.path());
    super::super::connection::test_hooks::at_the_first_busy_wait(move || drop(rebuild));

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
    let address = super::super::SessionAddress::new(
        Source::Pi.provider().storage().cache(),
        Source::Pi,
        super::super::SessionRoots::of_system(&crate::history::provider::SessionRoot::new(
            directory.path(),
        )),
        "session.jsonl".to_owned(),
    );
    let started = Instant::now();

    let lookup = super::super::InteractiveCache::at(base.clone())
        .open()
        .and_then(|cache| {
            cache
                .map(|cache| super::super::found(cache.read_copy_at(&address, |_| Ok(()))))
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

/// The ending the ownership lifecycle sweep gives a refresh ownership whose
/// write permission a failed batch revoked.
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
                RevokedOwnershipEnd::Shutdown => {
                    Coordinator::of(&this).shutdown(super::super::SHUTDOWN_RELEASE_WAIT)
                }
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
