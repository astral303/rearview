//! Every connection rearview opens to a session cache database opens here.
//! In test builds, [`test_hooks`] installs a test's SQLite hooks on each of
//! them.

use rusqlite::{Connection, OpenFlags};
use std::path::Path;

/// A connection's permitted access to the database file.
#[derive(Clone, Copy, Debug)]
pub(super) enum Access {
    ReadOnly,
    /// Read and write a database file that exists.
    ReadWrite,
    /// Read and write, creating the database file when it is missing.
    Create,
}

/// A connection to the database at `path`.
pub(super) fn open(path: &Path, access: Access) -> rusqlite::Result<Connection> {
    let connection = match access {
        Access::ReadOnly => Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?,
        Access::ReadWrite => Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE)?,
        Access::Create => Connection::open(path)?,
    };
    #[cfg(test)]
    test_hooks::install(&connection, path);
    Ok(connection)
}

/// SQLite hooks a test installs on every connection [`open`] makes to one
/// database: `commit_hook` to fail a chosen commit, a progress handler to
/// interrupt a statement. Keyed by the database's path, so tests running in
/// parallel on their own temporary databases do not see each other's hooks.
#[cfg(test)]
pub(crate) mod test_hooks {
    use rusqlite::Connection;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex, PoisonError};

    type Install = Arc<dyn Fn(&Connection) + Send + Sync>;

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
        install: impl Fn(&Connection) + Send + Sync + 'static,
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

    pub(super) fn install(connection: &Connection, path: &Path) {
        let key = database_key(path);
        let installers: Vec<Install> = INSTALLERS
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .filter(|(path, _, _)| *path == key)
            .map(|(_, _, install)| Arc::clone(install))
            .collect();
        for install in installers {
            install(connection);
        }
    }

    /// `path` with its directory canonical: the database file may not exist
    /// yet, and one base is spelled several ways.
    fn database_key(path: &Path) -> PathBuf {
        let directory = path.parent().unwrap_or(path);
        let directory =
            std::fs::canonicalize(directory).unwrap_or_else(|_| directory.to_path_buf());
        match path.file_name() {
            Some(name) => directory.join(name),
            None => directory,
        }
    }
}
