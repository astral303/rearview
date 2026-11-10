//! Every connection rearview opens to a session cache database opens here.
//! In test builds, [`test_hooks`] installs a test's SQLite hooks on each of
//! them.

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

/// A connection to the database at `path`, with a persistent write-ahead
/// log: the next connection reuses the `-wal` and `-shm` files instead of
/// creating them again. Each statement waits at most `busy_timeout` for a
/// lock, the statements this function runs included.
pub(super) fn open(
    path: &Path,
    access: Access,
    busy_timeout: Duration,
) -> rusqlite::Result<Connection> {
    let connection = match access {
        Access::ReadOnly => Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?,
        Access::ReadWrite => Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE)?,
        Access::Create => Connection::open(path)?,
    };
    // Install before any statement runs: a test's hook may lock the database
    // before this connection first reads it, as another rearview's rebuild
    // starting then does.
    #[cfg(test)]
    let connection = test_hooks::install(connection, path);
    connection.busy_timeout(busy_timeout)?;
    set_persistent_wal(&connection, true)?;
    if !matches!(access, Access::ReadOnly) {
        // Without a journal size limit, a writable connection that closes
        // last leaves the persistent write-ahead log at its full size.
        connection.pragma_update(None, "journal_size_limit", JOURNAL_SIZE_LIMIT as i64)?;
    }
    Ok(connection)
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
