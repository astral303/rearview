//! Every connection rearview opens to a session cache database opens here.
//! In test builds, [`test_hooks`] installs a test's SQLite hooks on each of
//! them.

use super::base::{CacheBase, OpenConnection};
use rusqlite::{Connection, FileControl, OpenFlags};
use std::path::Path;
use std::time::Duration;

/// A connection's permitted access to the database file.
#[derive(Clone, Copy, Debug)]
pub(super) enum Access {
    ReadOnly,
    /// Read and write a database file that exists.
    ReadWrite,
    /// Read and write, creating the database file when it is missing.
    Create,
}

/// The size the write-ahead log is truncated to after a checkpoint.
const JOURNAL_SIZE_LIMIT: u64 = 64 * 1024 * 1024;

/// A connection to the database at `path`, configured as [`configure`]
/// describes. For tests only: rearview opens its connections with
/// [`open_cache`].
#[cfg(test)]
pub(super) fn open(
    path: &Path,
    access: Access,
    busy_timeout: Duration,
) -> rusqlite::Result<Connection> {
    let mut connection = open_file(path, access)?;
    configure(&mut connection, access, busy_timeout)?;
    Ok(connection)
}

/// A connection to `base`'s database, configured as [`configure`] describes,
/// as a [`CacheConnection`] from before its first statement: a setup
/// statement that fails after the write-ahead log opened closes it under
/// `base`'s low-space close setting too.
pub(super) fn open_cache(
    base: &CacheBase,
    access: Access,
    busy_timeout: Duration,
) -> rusqlite::Result<CacheConnection> {
    let mut connection = CacheConnection {
        connection: open_file(&super::database_path(base.path()), access)?,
        base: base.clone(),
        counted: None,
    };
    configure(&mut connection.connection, access, busy_timeout)?;
    Ok(connection)
}

/// [`open_cache`], counted among this rearview's open connections to `base`
/// from before it opens until it closes (see [`CacheBase::open_connection`]).
pub(super) fn open_counted(
    base: &CacheBase,
    access: Access,
    busy_timeout: Duration,
) -> crate::error::Result<CacheConnection> {
    let counted = base.open_connection()?;
    let mut connection = open_cache(base, access, busy_timeout)?;
    connection.counted = Some(counted);
    Ok(connection)
}

/// A connection to a cache base's database. Once a write of the base found
/// its drive short of space, it closes without copying the write-ahead log
/// into the database: the copy needs the space the drive lacks.
pub(super) struct CacheConnection {
    connection: Connection,
    base: CacheBase,
    /// Declared after `connection`, so the count drops once it closes: a
    /// low-space deletion waits for it. `None` for a connection
    /// [`open_cache`] opens alone.
    counted: Option<OpenConnection>,
}

impl CacheConnection {
    /// The cache base this connection's database lives in, as the rearview
    /// that opened it reaches it.
    pub(super) fn base(&self) -> &CacheBase {
        &self.base
    }
}

impl std::ops::Deref for CacheConnection {
    type Target = Connection;

    fn deref(&self) -> &Connection {
        #[cfg(debug_assertions)]
        if let Some(counted) = &self.counted {
            counted.held_on_this_thread();
        }
        &self.connection
    }
}

impl std::ops::DerefMut for CacheConnection {
    fn deref_mut(&mut self) -> &mut Connection {
        #[cfg(debug_assertions)]
        if let Some(counted) = &self.counted {
            counted.held_on_this_thread();
        }
        &mut self.connection
    }
}

impl Drop for CacheConnection {
    /// Once the base's low-space latch is set, turn off the checkpoint
    /// SQLite runs when the last connection closes.
    fn drop(&mut self) {
        if !self.base.low_space().is_set() {
            return;
        }
        // SQLite fails the call only for an option it does not know.
        let kept = self.connection.set_db_config(
            rusqlite::config::DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE,
            true,
        );
        debug_assert!(kept.is_ok(), "SQLite turns the checkpoint on close off");
    }
}

fn open_file(path: &Path, access: Access) -> rusqlite::Result<Connection> {
    let connection = match access {
        // Without SQLite's lock on each call: rusqlite's `Connection` is not
        // `Sync`, which keeps two threads from using one connection at once.
        Access::ReadOnly => Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?,
        Access::ReadWrite => Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE)?,
        Access::Create => Connection::open(path)?,
    };
    // Install before any statement runs: a test's hook may lock the database
    // before this connection first reads it, as another rearview's rebuild
    // starting then does.
    #[cfg(test)]
    let connection = test_hooks::install(connection, path);
    Ok(connection)
}

/// Gives `connection` a persistent write-ahead log: the next connection
/// reuses the `-wal` and `-shm` files instead of creating them again. Each
/// statement waits at most `busy_timeout` for a lock, the statements this
/// function runs included.
fn configure(
    connection: &mut Connection,
    access: Access,
    busy_timeout: Duration,
) -> rusqlite::Result<()> {
    connection.busy_timeout(busy_timeout)?;
    #[cfg(test)]
    test_hooks::record_busy_waits(connection, busy_timeout)?;
    set_persistent_wal(connection, true)?;
    if !matches!(access, Access::ReadOnly) {
        // Without a journal size limit, a writable connection that closes
        // last leaves the persistent write-ahead log at its full size.
        connection.pragma_update(None, "journal_size_limit", JOURNAL_SIZE_LIMIT as i64)?;
        // An automatic checkpoint runs inside the commit that crosses its
        // threshold and syncs both files, stalling a load's write. The
        // writer checkpoints once when its writes end instead
        // (`CacheWriter::checkpoint_after_writes`).
        connection.pragma_update(None, "wal_autocheckpoint", 0)?;
    }
    Ok(())
}

/// Turn off SQLite's memory statistics: while they are on, every allocation
/// and free in any connection takes one process-wide mutex. SQLite accepts
/// this only before it initialises, when the process opens its first
/// connection.
pub(crate) fn turn_off_memory_statistics() -> rusqlite::Result<()> {
    // SAFETY: `SQLITE_CONFIG_MEMSTATUS` takes one int argument. SQLite
    // refuses the call with `SQLITE_MISUSE` once it is initialised.
    let code = unsafe {
        rusqlite::ffi::sqlite3_config(
            rusqlite::ffi::SQLITE_CONFIG_MEMSTATUS,
            std::ffi::c_int::from(false),
        )
    };
    if code == rusqlite::ffi::SQLITE_OK {
        Ok(())
    } else {
        Err(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(code),
            Some("turning off SQLite's memory statistics".to_owned()),
        ))
    }
}

/// Set SQLite's `SQLITE_FCNTL_PERSIST_WAL` on `connection`'s main database.
/// While it is set, `connection` keeps the `-wal` and `-shm` files when it
/// closes last or leaves WAL mode; while it is unset, it deletes them.
pub(super) fn set_persistent_wal(
    connection: &Connection,
    is_persistent: bool,
) -> rusqlite::Result<()> {
    let mut value = std::ffi::c_int::from(is_persistent);
    connection.file_control(Some(rusqlite::MAIN_DB), FileControl::PersistWal(&mut value))
}

/// SQLite hooks a test installs on every connection [`open`] makes to one
/// database: `commit_hook` to fail a chosen commit, a progress handler to
/// interrupt a statement. Keyed by the database's path, so tests running in
/// parallel on their own temporary databases do not see each other's hooks.
#[cfg(test)]
pub(crate) mod test_hooks {
    use rusqlite::Connection;
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex, PoisonError};
    use std::time::Duration;

    type Install = Arc<dyn Fn(&mut Connection) + Send + Sync>;

    static INSTALLERS: Mutex<Vec<(PathBuf, u64, Install)>> = Mutex::new(Vec::new());
    static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    /// Removes its installer when dropped.
    pub(crate) struct Installed {
        id: u64,
    }

    impl Drop for Installed {
        fn drop(&mut self) {
            INSTALLERS
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .retain(|(_, id, _)| *id != self.id);
        }
    }

    /// Run `install` on every connection opened to the session cache database
    /// in `base` until the result drops.
    pub(crate) fn on_every_connection(
        base: &Path,
        install: impl Fn(&mut Connection) + Send + Sync + 'static,
    ) -> Installed {
        let id = NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        INSTALLERS
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((
                database_key(&base.join(super::super::DATABASE_FILE_NAME)),
                id,
                Arc::new(install),
            ));
        Installed { id }
    }

    /// Fail the first commit to the session cache database in `base` that
    /// changes session data, calling `on_failure` then, until the result
    /// drops.
    pub(crate) fn fail_the_first_data_commit(
        base: &Path,
        on_failure: impl Fn() + Send + Sync + 'static,
    ) -> Installed {
        at_the_first_data_commit(base, FirstDataCommit::Fails, on_failure)
    }

    /// Call `action` as the first commit to the session cache database in
    /// `base` that changes session data lands, until the result drops.
    pub(crate) fn run_at_the_first_data_commit(
        base: &Path,
        action: impl Fn() + Send + Sync + 'static,
    ) -> Installed {
        at_the_first_data_commit(base, FirstDataCommit::Lands, action)
    }

    /// Call `action` once, as the read-only connection `opens` picks opens to
    /// the session cache database in `base`, before it runs a statement. The
    /// hook stays installed until the returned `ReaderAction` drops.
    pub(crate) fn run_as_a_reader_opens(
        base: &Path,
        opens: ReaderOpening,
        action: impl FnOnce() + Send + 'static,
    ) -> ReaderAction {
        let arming_thread = std::thread::current().id();
        let readers_on_arming_thread = std::sync::atomic::AtomicUsize::new(0);
        let ran = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let has_run = Arc::clone(&ran);
        let action = Mutex::new(Some(Box::new(action) as Box<dyn FnOnce() + Send>));
        let installed = on_every_connection(base, move |connection| {
            if !connection.is_readonly(rusqlite::MAIN_DB).unwrap() {
                return;
            }
            let on_arming_thread = std::thread::current().id() == arming_thread;
            let named = match opens {
                ReaderOpening::FirstOnAnotherThread => !on_arming_thread,
                ReaderOpening::OnThisThread(nth) => {
                    on_arming_thread
                        && readers_on_arming_thread
                            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                            + 1
                            == nth
                }
            };
            if !named {
                return;
            }
            let action = action.lock().unwrap_or_else(PoisonError::into_inner).take();
            if let Some(action) = action {
                action();
                ran.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        });
        ReaderAction {
            _installed: installed,
            has_run,
        }
    }

    /// An action [`run_as_a_reader_opens`] installed.
    pub(crate) struct ReaderAction {
        _installed: Installed,
        has_run: Arc<std::sync::atomic::AtomicBool>,
    }

    impl ReaderAction {
        pub(crate) fn has_run(&self) -> bool {
            self.has_run.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    /// The reader [`run_as_a_reader_opens`] runs its action at.
    #[derive(Clone, Copy)]
    pub(crate) enum ReaderOpening {
        /// The first reader opened on a thread other than the one that armed
        /// it: in a scan of the cache, the reader of its first task.
        FirstOnAnotherThread,
        /// The reader the arming thread opens `n`-th, counting from 1.
        OnThisThread(usize),
    }

    /// The outcome of the first commit that changes session data.
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum FirstDataCommit {
        Fails,
        Lands,
    }

    /// Call `action` at the first commit to the session cache database in
    /// `base` that changes session data, until the result drops. That commit
    /// fails or lands as `commit` specifies.
    fn at_the_first_data_commit(
        base: &Path,
        commit: FirstDataCommit,
        action: impl Fn() + Send + Sync + 'static,
    ) -> Installed {
        use std::sync::atomic::{AtomicBool, Ordering};
        let has_seen_first_data_commit = Arc::new(AtomicBool::new(false));
        let action: Arc<dyn Fn() + Send + Sync> = Arc::new(action);
        on_every_connection(base, move |connection| {
            if connection.is_readonly(rusqlite::MAIN_DB).unwrap() {
                return;
            }
            let changes_data = Arc::new(AtomicBool::new(false));
            let changed = Arc::clone(&changes_data);
            connection
                .update_hook(Some(
                    move |_: rusqlite::hooks::Action, _: &str, table: &str, _: i64| {
                        if table != "refresh_state" {
                            changed.store(true, Ordering::SeqCst);
                        }
                    },
                ))
                .unwrap();
            let rolled_back = Arc::clone(&changes_data);
            connection
                .rollback_hook(Some(move || rolled_back.store(false, Ordering::SeqCst)))
                .unwrap();
            let (has_seen, action) = (Arc::clone(&has_seen_first_data_commit), Arc::clone(&action));
            connection
                .commit_hook(Some(move || {
                    let is_first = changes_data.swap(false, Ordering::SeqCst)
                        && !has_seen.swap(true, Ordering::SeqCst);
                    if is_first {
                        action();
                    }
                    is_first && commit == FirstDataCommit::Fails
                }))
                .unwrap();
        })
    }

    /// Holds every commit to the session cache database in a base until
    /// opened or dropped, as a long write batch holds a refresh: the writer
    /// waits inside its commit.
    pub(crate) struct CommitGate {
        is_open: Arc<(Mutex<bool>, std::sync::Condvar)>,
        _installed: Installed,
    }

    impl CommitGate {
        /// A gate holding the commits to the database in `base`.
        pub(crate) fn closed_on(base: &Path) -> Self {
            let is_open = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
            let gate = Arc::clone(&is_open);
            let installed = on_every_connection(base, move |connection| {
                // A reader, such as the drawing thread's, never waits here.
                if connection.is_readonly(rusqlite::MAIN_DB).unwrap() {
                    return;
                }
                let gate = Arc::clone(&gate);
                connection
                    .commit_hook(Some(move || {
                        let (is_open, opened) = &*gate;
                        let mut is_open = is_open.lock().unwrap_or_else(PoisonError::into_inner);
                        while !*is_open {
                            is_open = opened.wait(is_open).unwrap_or_else(PoisonError::into_inner);
                        }
                        false
                    }))
                    .unwrap();
            });
            Self {
                is_open,
                _installed: installed,
            }
        }

        /// Let every held and later commit through.
        pub(crate) fn open(&self) {
            let (is_open, opened) = &*self.is_open;
            *is_open.lock().unwrap_or_else(PoisonError::into_inner) = true;
            opened.notify_all();
        }
    }

    impl Drop for CommitGate {
        fn drop(&mut self) {
            self.open();
        }
    }

    /// Holds a rebuild of the session cache database in a base inside the
    /// commit of its transaction that drops the tables, holding SQLite's
    /// exclusive lock, until dropped.
    pub(crate) struct RebuildGate {
        is_open: Arc<(Mutex<bool>, std::sync::Condvar)>,
        is_rebuild_held: Arc<std::sync::atomic::AtomicBool>,
        _installed: Installed,
    }

    impl RebuildGate {
        /// A gate holding the next rebuild of the database in `base`.
        pub(crate) fn closed_on(base: &Path) -> Self {
            use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
            use std::sync::atomic::{AtomicBool, Ordering};
            let is_open = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
            let is_rebuild_held = Arc::new(AtomicBool::new(false));
            let (gate, held) = (Arc::clone(&is_open), Arc::clone(&is_rebuild_held));
            let installed = on_every_connection(base, move |connection| {
                let drops_tables = Arc::new(AtomicBool::new(false));
                let dropping = Arc::clone(&drops_tables);
                connection
                    .authorizer(Some(move |context: AuthContext<'_>| {
                        if matches!(context.action, AuthAction::DropTable { .. }) {
                            dropping.store(true, Ordering::SeqCst);
                        }
                        Authorization::Allow
                    }))
                    .unwrap();
                let (gate, held) = (Arc::clone(&gate), Arc::clone(&held));
                connection
                    .commit_hook(Some(move || {
                        if !drops_tables.swap(false, Ordering::SeqCst) {
                            return false;
                        }
                        held.store(true, Ordering::SeqCst);
                        let (is_open, opened) = &*gate;
                        let mut is_open = is_open.lock().unwrap_or_else(PoisonError::into_inner);
                        while !*is_open {
                            is_open = opened.wait(is_open).unwrap_or_else(PoisonError::into_inner);
                        }
                        false
                    }))
                    .unwrap();
            });
            Self {
                is_open,
                is_rebuild_held,
                _installed: installed,
            }
        }

        /// True once a rebuild waits at the gate.
        pub(crate) fn is_rebuild_held(&self) -> bool {
            self.is_rebuild_held
                .load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    impl Drop for RebuildGate {
        fn drop(&mut self) {
            let (is_open, opened) = &*self.is_open;
            *is_open.lock().unwrap_or_else(PoisonError::into_inner) = true;
            opened.notify_all();
        }
    }

    std::thread_local! {
        /// The busy waits of the [`BusyWaits`] recording on this thread.
        static RECORDED_BUSY_WAITS: std::cell::RefCell<Option<Arc<Mutex<Vec<Duration>>>>> =
            const { std::cell::RefCell::new(None) };
    }

    /// Records a busy wait for each lock met by a connection [`super::open`]
    /// opens on this thread, until dropped. The connection gives up at once
    /// instead of sleeping, and the wait it would have slept, its busy
    /// timeout, is recorded: a test counts waits without timing them. A
    /// connection without a busy timeout records a zero wait.
    pub(crate) struct BusyWaits {
        waits: Arc<Mutex<Vec<Duration>>>,
        _not_send: std::marker::PhantomData<*const ()>,
    }

    impl BusyWaits {
        pub(crate) fn record_on_this_thread() -> Self {
            let waits = Arc::new(Mutex::new(Vec::new()));
            RECORDED_BUSY_WAITS.with(|recorded| *recorded.borrow_mut() = Some(Arc::clone(&waits)));
            Self {
                waits,
                _not_send: std::marker::PhantomData,
            }
        }

        /// The waits recorded since the last call, oldest first.
        pub(crate) fn take(&self) -> Vec<Duration> {
            std::mem::take(&mut *self.waits.lock().unwrap_or_else(PoisonError::into_inner))
        }
    }

    impl Drop for BusyWaits {
        fn drop(&mut self) {
            RECORDED_BUSY_WAITS.with(|recorded| *recorded.borrow_mut() = None);
        }
    }

    std::thread_local! {
        /// The action [`at_the_first_busy_wait`] armed on this thread.
        static AT_FIRST_BUSY_WAIT: std::cell::RefCell<Option<Box<dyn FnOnce() + Send>>> =
            const { std::cell::RefCell::new(None) };
    }

    /// Run `action` when the next connection [`super::open`] opens on this
    /// thread first meets a lock, inside that connection's busy wait. The
    /// connection then retries at once, and waits out the rest of its busy
    /// timeout as SQLite's handler does; a connection without a busy
    /// timeout gives up after the action. A test releases a lock inside the
    /// wait without timing it.
    pub(crate) fn at_the_first_busy_wait(action: impl FnOnce() + Send + 'static) {
        AT_FIRST_BUSY_WAIT.with(|armed| *armed.borrow_mut() = Some(Box::new(action)));
    }

    /// Replace `connection`'s busy wait of `busy_timeout` with the one a
    /// test arranged on this thread: a recorded wait while a [`BusyWaits`]
    /// records, or the wait [`at_the_first_busy_wait`] armed.
    pub(super) fn record_busy_waits(
        connection: &mut Connection,
        busy_timeout: Duration,
    ) -> rusqlite::Result<()> {
        if let Some(action) = AT_FIRST_BUSY_WAIT.with(|armed| armed.borrow_mut().take()) {
            return wait_after_first_busy(connection, busy_timeout, action);
        }
        let Some(waits) = RECORDED_BUSY_WAITS.with(|recorded| recorded.borrow().clone()) else {
            return Ok(());
        };
        connection.busy_handler(Some(move |attempt: i32| {
            if attempt == 0 {
                waits
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push(busy_timeout);
            }
            false
        }))
    }

    /// The busy wait [`at_the_first_busy_wait`] arms on `connection`.
    fn wait_after_first_busy(
        connection: &mut Connection,
        busy_timeout: Duration,
        action: Box<dyn FnOnce() + Send>,
    ) -> rusqlite::Result<()> {
        let mut action = Some(action);
        let mut first_busy = None;
        connection.busy_handler(Some(move |_attempt: i32| {
            let started = *first_busy.get_or_insert_with(std::time::Instant::now);
            if let Some(action) = action.take() {
                action();
                return !busy_timeout.is_zero();
            }
            std::thread::sleep(Duration::from_millis(1));
            started.elapsed() < busy_timeout
        }))
    }

    pub(super) fn install(mut connection: Connection, path: &Path) -> Connection {
        let key = database_key(path);
        let installers: Vec<Install> = INSTALLERS
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .filter(|(path, _, _)| *path == key)
            .map(|(_, _, install)| Arc::clone(install))
            .collect();
        for install in installers {
            install(&mut connection);
        }
        connection
    }

    /// Each [`database_key`] by the path it was asked for.
    static DATABASE_KEYS: Mutex<BTreeMap<PathBuf, PathBuf>> = Mutex::new(BTreeMap::new());

    /// `path` with its directory canonical: the database file may not exist
    /// yet, and one base is spelled several ways. Canonicalises each path
    /// once, as every connection a test build opens asks for its key; a
    /// directory that cannot be canonicalised yet is tried again next time.
    fn database_key(path: &Path) -> PathBuf {
        let known = DATABASE_KEYS
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(path)
            .cloned();
        if let Some(key) = known {
            return key;
        }
        let directory = path.parent().unwrap_or(path);
        // Test builds only, once per database path: no environment reaches
        // a connection's installers.
        #[allow(clippy::disallowed_methods)]
        let Ok(directory) = std::fs::canonicalize(directory) else {
            return path.to_path_buf();
        };
        let key = match path.file_name() {
            Some(name) => directory.join(name),
            None => directory,
        };
        DATABASE_KEYS
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(path.to_path_buf(), key.clone());
        key
    }
}
