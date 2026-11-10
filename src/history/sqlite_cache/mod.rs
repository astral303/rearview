//! The session cache database, `history.sqlite3` in the cache base: every
//! session's catalogue row and the entries `session_entries` returns for
//! it. Catalogue, viewer and agent readers share it.
//!
//! A session the cache holds as a complete listed copy is written in one
//! transaction: each new row is compared with its stored version, and only
//! the rows that differ are written (see `entries::diff`). Any other session
//! is written in short transactions outside the parse: its catalogue row
//! first, marked [`catalogue::WRITING`], then its entries a slice at a time,
//! then the row's status. A row still marked writing, left by a run that
//! stopped, is not current, and the next load writes the session again.

mod base;
mod cached_session;
mod catalogue;
pub(crate) mod connection;
mod coordinator;
mod copies_scan;
mod entries;
mod files;
mod free_space;
mod interactive;
pub(crate) mod low_space;
mod next_step;
mod observer;
mod permit;
mod positional;
mod read_catalogue;
pub(crate) mod refresh;
mod roots;
mod schema;
mod scope;
mod search_parts;
mod search_texts;
mod semantic_routes;
mod semantic_scan;
mod text_scan;
mod window;

pub use base::{CacheBase, ProcessIdentity};
#[cfg(test)]
pub(crate) use base::{DiskSpace, DriveRunningOut};
pub(crate) use cached_session::Currency;
pub(crate) use cached_session::HeldCopy;
pub(crate) use cached_session::find_as_is_without_waiting as find_cached_session_as_is_without_waiting;
pub(crate) use cached_session::find_copy_without_waiting as find_cached_copy_without_waiting;
pub(crate) use cached_session::find_waiting as find_cached_session_waiting;
pub use cached_session::{CachedSession, Publication, SessionAddress};
pub(crate) use cached_session::{CopyKey, SessionKey};
#[cfg(test)]
pub(crate) use catalogue::RecordEntries;
pub(crate) use catalogue::{ListedRecord, RecordPlace, SessionRecord, SessionStatus, StubStamps};
pub use coordinator::WaitStatus;
pub(crate) use coordinator::{Coordinator, SHUTDOWN_RELEASE_WAIT};
pub(crate) use copies_scan::CopyOutcome;
pub(crate) use entries::StoredEntries;
pub use interactive::{
    AdoptedCopy, CachedRead, CurrentCopy, DrawingThread, InteractiveCache, InteractiveRead,
};
pub(crate) use interactive::{CachedCopy, refuse_the_drawing_thread};
pub use next_step::NextStep;
pub use observer::{AwaitedRefresh, Observation, OtherRearview};
pub use permit::{Admitted, ParsePermit};
#[cfg(test)]
pub(crate) use permit::{FOR_FIXTURES, admitted_reads};
pub(crate) use read_catalogue::CatalogueSession;
pub(crate) use refresh::FREE_SPACE_FLOOR;
pub use scope::{Cancellation, OperationScope, Patience, StaleCause};
pub(crate) use scope::{LiveOwner, OperationAccess, read_when_unlocked};
pub(crate) use search_parts::{SearchField, SearchPart, SemanticTurns};
pub(crate) use search_texts::SearchedFields;
pub(crate) use semantic_routes::{
    RouteKey, RoutesStored, SemanticRoute, store_routes as store_semantic_routes,
};
pub(crate) use semantic_scan::{ScannedTurns, scan_semantic_turns};
pub(crate) use text_scan::{ReadOrder, ScanScope, ScannedRow, TextVisitor, scan_search_texts};
pub(crate) use window::{InPlaceRow, StoredFile, StoredResume, StoredTurn, WindowUpdate};

use crate::cli::DebugLevel;
use crate::debug;
use crate::error::{AppError, Result};
use crate::history::TranscriptEntries;
use crate::history::fingerprint::CachedFingerprint;
use crate::history::format::RecordLine;
use crate::history::provider::{SessionCache, SessionRoot};
use crate::log_entry::LogEntry;
use connection::Access;
use files::RootKey;
pub(crate) use roots::SessionRoots;
use roots::{Places, WrittenPlaces};
use rusqlite::{Connection, OptionalExtension, params};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::Receiver;
use std::time::Duration;

/// The database's file name in the cache base.
pub(crate) const DATABASE_FILE_NAME: &str = "history.sqlite3";

const PAGE_SIZE: u32 = 4096;

/// The busy timeout of the waiting reader and the writers: loads, refreshes,
/// agent commands and the TUI's background worker. The TUI's drawing
/// thread opens neither; see [`interactive`].
const WAITING_BUSY_TIMEOUT: Duration = Duration::from_secs(30);

/// The busy timeout of cache reads on the TUI's drawing thread. Past it, the
/// read returns a miss with its next step: read the session again once the
/// refresh owning the cache completes, or request a session refresh.
pub(crate) const INTERACTIVE_BUSY_TIMEOUT: Duration = Duration::from_millis(150);

/// A reader's busy timeout.
#[derive(Clone, Copy, Debug)]
enum ReaderWait {
    /// At most [`WAITING_BUSY_TIMEOUT`]: loads, refreshes, agent commands and
    /// the TUI's background worker.
    Waiting(Duration),
    /// [`INTERACTIVE_BUSY_TIMEOUT`]: the TUI's drawing thread.
    Interactive,
}

impl ReaderWait {
    /// `base`'s reader with this wait.
    fn open(self, base: &CacheBase) -> Result<Option<SqliteCache>> {
        match self {
            Self::Waiting(timeout) => SqliteCache::open_waiting_at_most(base, timeout),
            Self::Interactive => SqliteCache::open_with(base, self),
        }
    }

    fn busy_timeout(self) -> Duration {
        match self {
            Self::Waiting(timeout) => timeout,
            Self::Interactive => INTERACTIVE_BUSY_TIMEOUT,
        }
    }
}

const ADMISSION_COMPLETE: &str = "admission_complete";

fn database_path(base: &Path) -> PathBuf {
    base.join(DATABASE_FILE_NAME)
}

/// The files SQLite keeps beside the database: its write-ahead log,
/// shared-memory file and rollback journal, named by these suffixes.
const DATABASE_SIDE_FILE_SUFFIXES: [&str; 3] = ["-wal", "-shm", "-journal"];

/// True when the database under `base` holds neither a `sessions` table
/// nor a file beside it, as a low-space deletion leaves it.
#[cfg(test)]
pub(crate) fn is_emptied(base: &Path) -> bool {
    let database = database_path(base);
    let side_file = DATABASE_SIDE_FILE_SUFFIXES.iter().any(|suffix| {
        let mut path = database.clone().into_os_string();
        path.push(suffix);
        PathBuf::from(path).exists()
    });
    let sessions_table: bool = Connection::open(&database)
        .unwrap()
        .query_row(
            "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE name = 'sessions')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    !side_file && !sessions_table
}

/// Drop every table this release or an earlier one created, but
/// `refresh_state`.
fn drop_session_tables(connection: &Connection) -> rusqlite::Result<()> {
    for table in schema::RETIRED_TABLES.iter().chain(schema::TABLES) {
        connection.execute_batch(&format!("DROP TABLE IF EXISTS {table}"))?;
    }
    Ok(())
}

/// True for SQLite's refusal while another connection holds a lock the
/// statement needs.
pub(crate) fn is_busy(error: &rusqlite::Error) -> bool {
    matches!(error, rusqlite::Error::SqliteFailure(error, _)
        if matches!(error.code, rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked))
}

/// [`is_busy`] for an error as this crate reports it.
pub(crate) fn is_busy_error(error: &AppError) -> bool {
    matches!(error, AppError::SessionCache(error) if is_busy(error))
}

/// A reader connection to a cache base's database. It never creates,
/// rebuilds or invalidates a cache; [`CacheWriter`] holds every write.
pub(crate) struct SqliteCache {
    connection: connection::CacheConnection,
    /// The wait of this connection, and of the reads of each session it
    /// finds.
    wait: ReaderWait,
}

/// A connection that writes a cache base's database for a refresh owner.
/// Every write checks the owner's write permission. Reads go through its
/// [`SqliteCache`].
pub(crate) struct CacheWriter {
    cache: SqliteCache,
    owner: refresh::RefreshToken,
}

impl std::ops::Deref for CacheWriter {
    type Target = SqliteCache;

    fn deref(&self) -> &SqliteCache {
        &self.cache
    }
}

impl std::ops::DerefMut for CacheWriter {
    fn deref_mut(&mut self) -> &mut SqliteCache {
        &mut self.cache
    }
}

/// The stored layout's compatibility with this release's tables.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StoredLayout {
    Current,
    /// No tables yet.
    Missing,
    /// Another schema version, semantic turn version or page size, older or
    /// newer: a writer empties and rebuilds it.
    Outdated,
}

impl SqliteCache {
    /// `base`'s database when it holds an admitted cache of this release's
    /// layout; `None` otherwise. Each statement waits up to 30 seconds for
    /// another connection's lock, such as a rebuild's; the TUI's drawing
    /// thread reads through [`InteractiveCache`] instead.
    pub(crate) fn open_waiting(base: &CacheBase) -> Result<Option<Self>> {
        Self::open_waiting_at_most(base, WAITING_BUSY_TIMEOUT)
    }

    /// [`Self::open_waiting`], each statement waiting at most `timeout`.
    pub(crate) fn open_waiting_at_most(
        base: &CacheBase,
        timeout: Duration,
    ) -> Result<Option<Self>> {
        interactive::refuse_the_drawing_thread("open the waiting session cache reader");
        Self::open_with(base, ReaderWait::Waiting(timeout.min(WAITING_BUSY_TIMEOUT)))
    }

    fn open_with(base: &CacheBase, wait: ReaderWait) -> Result<Option<Self>> {
        let path = database_path(base.path());
        if !path.is_file() {
            return Ok(None);
        }
        let connection = connection::open_counted(base, Access::ReadOnly, wait.busy_timeout())?;
        let cache = Self { connection, wait };
        if cache.stored_layout()? != StoredLayout::Current || !cache.is_admitted()? {
            return Ok(None);
        }
        Ok(Some(cache))
    }

    /// Opens `base`'s database for `owner`'s refresh, creating it and its
    /// tables when missing, and emptying it when its tables are of another
    /// schema version, semantic turn version or page size. A failure revokes
    /// `owner`'s write permission.
    pub(crate) fn open_for_refresh(owner: &refresh::RefreshToken) -> Result<CacheWriter> {
        CacheWriter::open(owner).inspect_err(|_| owner.revoke())
    }

    fn stored_layout(&self) -> Result<StoredLayout> {
        let page_size: u32 = self
            .connection
            .pragma_query_value(None, "page_size", |row| row.get(0))?;
        if page_size != PAGE_SIZE {
            return Ok(StoredLayout::Outdated);
        }
        let has_meta: bool = self.connection.query_row(
            "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'meta')",
            [],
            |row| row.get(0),
        )?;
        if !has_meta {
            return Ok(StoredLayout::Missing);
        }
        for (key, version) in schema::layout_versions() {
            if self.meta(key)? != Some(version) {
                return Ok(StoredLayout::Outdated);
            }
        }
        Ok(StoredLayout::Current)
    }

    fn meta(&self, key: &str) -> Result<Option<String>> {
        read_meta(&self.connection, key)
    }

    /// The cache base this connection's database lives in.
    fn base(&self) -> &CacheBase {
        self.connection.base()
    }

    /// `root` and the directories beside it, with the keys this database
    /// stores them under.
    pub(crate) fn session_roots(&self, root: &SessionRoot) -> SessionRoots {
        SessionRoots::of(root, self.base().canonicaliser().as_ref())
    }

    pub(crate) fn storage_is_current(&self, storage: SessionCache) -> Result<bool> {
        is_storage_current(&self.connection, storage)
    }

    /// The fingerprint of every current session `storage` found under
    /// `root`, by cache key. A session in `discovered` with the fingerprint
    /// the cache recorded is not stat again for the files discovery stamped:
    /// its transcript and sub-agent transcripts.
    pub(crate) fn current_fingerprints(
        &self,
        storage: SessionCache,
        root: &SessionRoot,
        discovered: &[crate::history::provider::SessionStub],
    ) -> Result<HashMap<String, CachedFingerprint>> {
        let discovered: HashMap<&str, &crate::history::provider::SessionStub> = discovered
            .iter()
            .map(|stub| (stub.cache_key.as_str(), stub))
            .collect();
        let places = Places::stored(&self.connection, &self.session_roots(root))?;
        let mut statement = self.connection.prepare_cached(&format!(
            "SELECT cache_key, file_size, mtime_secs, mtime_nsecs, id FROM sessions \
             WHERE storage = ?1 AND root = ?2 AND status != ?3 AND {}",
            catalogue::ENTRIES_NOT_FAILED
        ))?;
        let candidates = statement
            .query_map(
                params![storage.directory, places.listing_root(), catalogue::WRITING],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        (
                            CachedFingerprint {
                                file_size: row.get::<_, i64>(1)? as u64,
                                mtime_secs: row.get::<_, i64>(2)? as u64,
                                mtime_nsecs: row.get::<_, i64>(3)? as u32,
                            },
                            row.get::<_, i64>(4)?,
                        ),
                    ))
                },
            )?
            .collect::<rusqlite::Result<HashMap<_, _>>>()?;
        let mut fingerprints = HashMap::new();
        for (key, (stamp, session)) in candidates {
            let stamped: HashSet<&Path> = discovered
                .get(key.as_str())
                .filter(|stub| stub.fingerprint.stamp() == Some(stamp))
                .map(|stub| {
                    std::iter::once(stub.locator.as_path())
                        .chain(stub.subagents.iter().map(PathBuf::as_path))
                        .collect()
                })
                .unwrap_or_default();
            match files::validate_unstamped(&self.connection, &places, session, &stamped) {
                Ok(()) => {
                    fingerprints.insert(key, stamp);
                }
                Err(error) if error.is_changed_source() => {}
                Err(error) => return Err(error),
            }
        }
        Ok(fingerprints)
    }

    pub(crate) fn stored_keys(
        &self,
        storage: SessionCache,
        root: &SessionRoot,
    ) -> Result<Vec<String>> {
        Ok(self
            .connection
            .prepare_cached(
                "SELECT cache_key FROM sessions WHERE storage = ?1 \
                 AND root = (SELECT id FROM roots WHERE key = ?2)",
            )?
            .query_map(
                params![
                    storage.directory,
                    self.session_roots(root).key().map(RootKey::as_str)
                ],
                |row| row.get(0),
            )?
            .collect::<rusqlite::Result<_>>()?)
    }

    pub(crate) fn holds(
        &self,
        storage: SessionCache,
        root: &SessionRoot,
        stub: &crate::history::provider::SessionStub,
    ) -> Result<bool> {
        let Some(stamp) = stub.fingerprint.stamp() else {
            return Ok(false);
        };
        let roots = self.session_roots(root);
        self.with_read_snapshot(|connection| {
            let places = Places::stored(connection, &roots)?;
            let session: Option<i64> = connection.query_row(
                &format!(
                    "SELECT id FROM sessions WHERE storage = ?1 AND root = ?2 AND cache_key = ?3 \
                     AND file_size = ?4 AND mtime_secs = ?5 AND mtime_nsecs = ?6 \
                     AND status != 'writing' AND {}",
                    catalogue::ENTRIES_NOT_FAILED
                ),
                params![storage.directory, places.listing_root(), stub.cache_key, stamp.file_size as i64, stamp.mtime_secs as i64, stamp.mtime_nsecs],
                |row| row.get(0),
            ).optional()?;
            let Some(session) = session else { return Ok(false) };
            match files::validate(connection, &places, session) {
                Ok(()) => Ok(true),
                Err(error) if error.is_changed_source() => Ok(false),
                Err(error) => Err(error),
            }
        })
    }

    pub(crate) fn is_admitted(&self) -> Result<bool> {
        Ok(self.meta(ADMISSION_COMPLETE)?.as_deref() == Some("1"))
    }

    /// The entries written for the session `storage` found at `cache_key`
    /// under `root`; `None` when the database holds no such session.
    #[cfg(test)]
    pub(crate) fn entries(
        &self,
        storage: SessionCache,
        root: &SessionRoot,
        cache_key: &str,
    ) -> Result<Option<TranscriptEntries>> {
        self.with_read_snapshot(|connection| {
            self.read_entries(connection, storage, root, cache_key, None)
        })
    }

    #[cfg(test)]
    pub(crate) fn message_entries(
        &self,
        storage: SessionCache,
        root: &SessionRoot,
        cache_key: &str,
        messages: std::ops::RangeInclusive<u64>,
    ) -> Result<Option<TranscriptEntries>> {
        self.with_read_snapshot(|connection| {
            self.read_entries(connection, storage, root, cache_key, Some(messages))
        })
    }

    fn with_read_snapshot<T>(&self, read: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let transaction = self.connection.unchecked_transaction()?;
        let result = read(&transaction)?;
        transaction.commit()?;
        Ok(result)
    }

    #[cfg(test)]
    fn read_entries(
        &self,
        connection: &Connection,
        storage: SessionCache,
        root: &SessionRoot,
        cache_key: &str,
        messages: Option<std::ops::RangeInclusive<u64>>,
    ) -> Result<Option<TranscriptEntries>> {
        let roots = self.session_roots(root);
        let Some(session) =
            entries::listed_session_of(connection, storage.directory, roots.key(), cache_key)?
        else {
            return Ok(None);
        };
        let places = Places::stored(connection, &roots)?;
        files::validate(connection, &places, session.id)?;
        let entries = entries::read(connection, session.id, &places, messages)?;
        Ok(Some(entries))
    }
}

/// The rows a resumed read's [`window::WindowUpdate`] writes, built before
/// the write's transaction.
struct WindowRows<'a> {
    leading: entries::SessionRows<'a>,
    tail: entries::SessionRows<'a>,
    in_place: entries::SessionRows<'a>,
    /// Each stored row the in-place rows replace: its id, message and digest.
    stored_in_place: Vec<(i64, Option<i64>, i64)>,
}

impl<'a> WindowRows<'a> {
    /// The rows of `update`, whose entries from the checkpoint on are
    /// `entries` and whose rows changed in place hold `in_place_entries`.
    fn of(
        update: &'a window::WindowUpdate,
        entries: &'a TranscriptEntries,
        in_place_entries: &'a [(RecordLine, LogEntry)],
        subagents: &HashSet<&Path>,
    ) -> Self {
        let fold = &update.checkpoint.fold;
        let start = entries::RowsStart {
            numbering: fold.numbering().clone(),
            positions: fold.positions(),
            sequence: update.checkpoint.sequence,
        };
        Self {
            leading: entries::SessionRows::from(
                &update.leading,
                subagents,
                &entries::RowsStart::default(),
            ),
            tail: entries::SessionRows::from(&entries.entries, subagents, &start),
            in_place: entries::SessionRows::from(
                in_place_entries,
                subagents,
                &entries::RowsStart::default(),
            ),
            stored_in_place: update
                .in_place
                .iter()
                .map(|row| (row.id, row.message, row.digest))
                .collect(),
        }
    }
}

impl CacheWriter {
    /// The writer of `owner`'s database, in this release's layout and
    /// admitted. A database an earlier release left unadmitted, its first
    /// build's sample stopped part way, loses that sample's sessions.
    fn open(owner: &refresh::RefreshToken) -> Result<Self> {
        let mut writer = Self::connect(owner.clone())?;
        if writer.stored_layout()? == StoredLayout::Outdated {
            let base = owner.cache_base();
            let _rebuilding = base.process().mark_rebuild(base.path());
            writer.rebuild()?;
            // Closing releases the maintenance connection's retained exclusive
            // lock. Reopen normally before returning to WAL and shared readers.
            drop(writer);
            writer = Self::connect(owner.clone())?;
        }
        writer.create_schema()?;
        if !writer.is_admitted()? {
            writer.remove_every_session()?;
            writer.mark_admitted()?;
        }
        Ok(writer)
    }

    fn connect(owner: refresh::RefreshToken) -> Result<Self> {
        interactive::refuse_the_drawing_thread("open a session cache writer");
        let base = owner.cache_base();
        std::fs::create_dir_all(base.path())?;
        let connection = connection::open_counted(&base, Access::Create, WAITING_BUSY_TIMEOUT)?;
        connection.pragma_update(None, "page_size", PAGE_SIZE)?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "synchronous", "NORMAL")?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        // A write that compares a session with its stored rows prepares about
        // forty statements; keep each prepared across the sessions of a load.
        connection.set_prepared_statement_cache_capacity(64);
        Ok(Self {
            cache: SqliteCache {
                connection,
                wait: ReaderWait::Waiting(WAITING_BUSY_TIMEOUT),
            },
            owner,
        })
    }

    /// True while this connection's owner keeps its write permission.
    fn can_publish(&self) -> bool {
        self.owner.can_publish()
    }

    /// Take SQLite's exclusive lock for the rebuild, inside `turn`, and
    /// leave WAL mode.
    fn begin_rebuild(&self, turn: &refresh::Turn<'_>) -> Result<()> {
        refresh::take_exclusive_lock(
            &self.connection,
            &self.owner,
            turn,
            refresh::ExclusiveWait::of(WAITING_BUSY_TIMEOUT),
            |batch| refresh::commit(batch, &self.owner),
        )?;
        refresh::leave_wal(&self.connection)
    }

    /// Empty the database into this release's layout, holding the owner's
    /// turn throughout, so the writer lock's 30-second wait limit measures
    /// only other connections here too.
    fn rebuild(&self) -> Result<()> {
        let turn = self.owner.take_turn();
        self.begin_rebuild(&turn)?;
        // Do not discard the existing cache until exclusion and the journal
        // transition have succeeded. Interrupted rebuilds remain retryable.
        let batch = refresh::transaction_in_turn(&self.connection, &self.owner, &turn)?;
        drop_session_tables(&batch)?;
        refresh::commit(batch, &self.owner)?;
        self.connection
            .pragma_update(None, "page_size", PAGE_SIZE)?;
        self.connection.execute_batch("VACUUM")?;
        self.create_schema_in_turn(&turn)
    }

    fn create_schema(&self) -> Result<()> {
        self.create_schema_in_turn(&self.owner.take_turn())
    }

    fn create_schema_in_turn(&self, turn: &refresh::Turn<'_>) -> Result<()> {
        let batch = refresh::transaction_in_turn(&self.connection, &self.owner, turn)?;
        batch.execute_batch(schema::CREATE)?;
        for (key, version) in schema::layout_versions() {
            put_meta(&batch, key, &version)?;
        }
        refresh::commit(batch, &self.owner)?;
        Ok(())
    }

    fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        let batch = refresh::transaction(&self.connection, &self.owner)?;
        put_meta(&batch, key, value)?;
        refresh::commit(batch, &self.owner)?;
        Ok(())
    }

    /// Deletes `storage`'s sessions when they were written under another
    /// `SessionCache::schema_version` of the storage, so the load writes
    /// them again, and records the version they are written under now.
    pub(crate) fn admit_storage(&self, storage: SessionCache) -> Result<()> {
        if self.storage_is_current(storage)? {
            return Ok(());
        }
        let batch = refresh::transaction(&self.connection, &self.owner)?;
        batch.execute(
            "DELETE FROM sessions WHERE storage = ?1",
            [storage.directory],
        )?;
        put_meta(
            &batch,
            &storage_schema_key(storage),
            &storage.schema_version.to_string(),
        )?;
        refresh::commit(batch, &self.owner)?;
        Ok(())
    }

    pub(crate) fn mark_admitted(&self) -> Result<()> {
        self.set_meta(ADMISSION_COMPLETE, "1")
    }

    fn remove_every_session(&self) -> Result<()> {
        let batch = refresh::transaction(&self.connection, &self.owner)?;
        batch.execute(
            "DELETE FROM sessions WHERE NOT EXISTS \
             (SELECT 1 FROM meta WHERE key = ?1 AND value = '1')",
            [ADMISSION_COMPLETE],
        )?;
        refresh::commit(batch, &self.owner)?;
        Ok(())
    }

    /// Copies the write-ahead log's pages into the database, up to the first
    /// page a reader still needs, without waiting for readers or writers.
    /// Call it once a batch of writes ends. Connections that write do not
    /// checkpoint automatically, and a long-running TUI's log grows until a
    /// checkpoint runs. Skipped once a write found the drive short of space:
    /// the copy would grow the database the low-space deletion empties.
    pub(crate) fn checkpoint_after_writes(&self) -> Result<()> {
        if self.owner.is_low_space() {
            return Ok(());
        }
        self.connection
            .query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |_| Ok(()))?;
        Ok(())
    }

    /// Moves every page of the write-ahead log into the database and
    /// truncates the log.
    #[cfg(test)]
    fn checkpoint(&self) -> Result<()> {
        let busy: bool =
            self.connection
                .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| row.get(0))?;
        if busy {
            return Err(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
                Some("The session cache checkpoint is blocked by another connection".to_owned()),
            )
            .into());
        }
        Ok(())
    }

    /// Replaces the rows of `record`'s session with `record`, and returns the
    /// generation the write gave the session's row. Each step of the write
    /// checks the free space on the cache's drive, and fails once it is
    /// below the floor (see [`refresh::RefreshToken::check_space`]).
    pub(crate) fn write(&mut self, record: &SessionRecord) -> Result<i64> {
        if !self.is_admitted()? {
            return Err(AppError::SessionCacheNotReady {
                session: record.catalogue.cache_key.clone(),
                reason: "the database is not admitted".to_owned(),
            });
        }
        let owner = self.owner.clone();
        self.write_checked(record, &|| owner.check_space())
    }

    fn write_checked(
        &mut self,
        record: &SessionRecord,
        check_space: &dyn Fn() -> Result<()>,
    ) -> Result<i64> {
        check_space()?;
        let subagents = record
            .catalogue
            .files
            .iter()
            .filter(|file| file.kind == "subagent")
            .map(|file| file.path.as_path())
            .collect();
        let written = record
            .entries
            .as_ref()
            .and_then(catalogue::RecordEntries::written);
        if let (Some(update), Some(entries)) = (&record.update, written) {
            return self.update_window(record, update, entries, &subagents);
        }
        let rows = written.map(|entries| entries::SessionRows::of(entries, &subagents));
        // The free space is checked above, outside the transaction: the check
        // takes the owner's turn, which the transaction holds.
        if let (Some(rows), Some(entries)) = (&rows, written)
            && let Some(generation) = self.update_stored(record, rows, entries)?
        {
            return Ok(generation);
        }
        self.insert_session(record, rows.as_ref(), check_space)
    }

    /// Writes `record`, a resumed read's `update` of its stored session
    /// whose entries from the checkpoint on are `entries`, in one
    /// transaction. Fails when the stored copy is not the one the read
    /// resumed from.
    fn update_window(
        &mut self,
        record: &SessionRecord,
        update: &window::WindowUpdate,
        entries: &TranscriptEntries,
        subagents: &HashSet<&Path>,
    ) -> Result<i64> {
        let in_place_entries = update.in_place_entries();
        let WindowRows {
            leading,
            tail,
            in_place,
            stored_in_place,
        } = WindowRows::of(update, entries, &in_place_entries, subagents);
        let batch = refresh::transaction(&self.cache.connection, &self.owner)?;
        let keys = &catalogue_keys(&batch, &record.catalogue)?;
        let places = &keys.places;
        let stored: Option<i64> = batch
            .query_row(
                "SELECT generation FROM sessions WHERE id = ?1 AND status = 'listed' \
                 AND entries_error IS NULL",
                [update.session],
                |row| row.get(0),
            )
            .optional()?;
        if stored != Some(update.generation) {
            return Err(AppError::SessionCacheSuperseded(update.session));
        }
        let session = update.session;
        let generation = write_catalogue_rows(
            &batch,
            session,
            record,
            keys,
            record.catalogue.message_count,
        )?;
        for (path, launch_call) in &update.launch_calls {
            let place = places.place_written(path)?;
            batch
                .prepare_cached(
                    "UPDATE threads SET launch_call = ?4 WHERE session = ?1 AND transcript = \
                     (SELECT id FROM transcripts WHERE session = ?1 AND root = ?2 AND path = ?3) \
                     AND launch_call IS NOT ?4",
                )?
                .execute(params![session, place.root, place.path, launch_call])?;
        }
        entries::replace_leading(&batch, session, places, &leading)?;
        entries::update_tail(&batch, session, places, &tail)?;
        entries::rewrite_in_place(&batch, session, places, &in_place, &stored_in_place)?;
        write_malformed_lines(&batch, session, entries)?;
        refresh::commit(batch, &self.owner)?;
        Ok(generation)
    }

    /// Writes `record`, a listed session with `entries`, whose rows are
    /// `rows`, over its stored rows in one transaction, writing only the
    /// rows that differ, and returns the generation it gave the session's
    /// row. `None`, without writing, when the cache holds no complete listed
    /// copy of the session to compare with.
    fn update_stored(
        &mut self,
        record: &SessionRecord,
        rows: &entries::SessionRows,
        entries: &TranscriptEntries,
    ) -> Result<Option<i64>> {
        let row = &record.catalogue;
        let batch = refresh::transaction(&self.cache.connection, &self.owner)?;
        let keys = &catalogue_keys(&batch, row)?;
        let session: Option<i64> = batch
            .query_row(
                "SELECT id FROM sessions WHERE storage = ?1 AND root = ?2 AND cache_key = ?3 \
                 AND status = 'listed' AND entries_error IS NULL",
                params![row.storage, keys.root, row.cache_key],
                |row| row.get(0),
            )
            .optional()?;
        let Some(session) = session else {
            return Ok(None);
        };
        let generation =
            write_catalogue_rows(&batch, session, record, keys, Some(rows.message_count()))?;
        entries::update(&batch, session, &keys.places, rows)?;
        write_malformed_lines(&batch, session, entries)?;
        refresh::commit(batch, &self.owner)?;
        Ok(Some(generation))
    }

    /// Replaces the rows of `record`'s session with `record`, whose entry
    /// rows are `rows`: its catalogue row first, marked writing, then its
    /// entries a slice at a time, then the row's status.
    fn insert_session(
        &mut self,
        record: &SessionRecord,
        rows: Option<&entries::SessionRows>,
        check_space: &dyn Fn() -> Result<()>,
    ) -> Result<i64> {
        let (session, keys) = self.write_catalogue(record, rows)?;
        let generation: i64 = self.connection.query_row(
            "SELECT generation FROM sessions WHERE id = ?1",
            [session],
            |row| row.get(0),
        )?;
        let owner = &self.owner;
        let entries_error = match &record.entries {
            Some(catalogue::RecordEntries::Written(entries)) => {
                let rows = rows.expect("listed entries have rows");
                entries::insert(
                    &mut self.cache.connection,
                    session,
                    &keys.places,
                    rows,
                    check_space,
                    owner,
                )?;
                check_space()?;
                let batch = session_transaction(&mut self.cache.connection, session, owner)?;
                write_malformed_lines(&batch, session, entries)?;
                write_subagent_metadata(&batch, session, record, &keys, &[])?;
                refresh::commit(batch, owner)?;
                None
            }
            Some(catalogue::RecordEntries::Unwritten { reason, .. }) => Some(reason.as_str()),
            None => record.unlisted_reason.as_deref(),
        };
        check_space()?;
        self.set_final_status(session, record.catalogue.status, entries_error)?;
        Ok(generation)
    }

    /// Inserts `record`'s catalogue row, marked writing; returns its id and
    /// where its files are stored.
    fn write_catalogue(
        &mut self,
        record: &SessionRecord,
        rows: Option<&entries::SessionRows>,
    ) -> Result<(i64, catalogue::CatalogueKeys)> {
        let row = &record.catalogue;
        let batch = refresh::transaction(&self.cache.connection, &self.owner)?;
        let keys = catalogue_keys(&batch, row)?;
        batch.execute(
            "DELETE FROM sessions WHERE storage = ?1 AND root = ?2 AND cache_key = ?3",
            params![row.storage, keys.root, row.cache_key],
        )?;
        let generation = next_generation(&batch)?;
        let values = row.values(
            catalogue::WRITING,
            rows.map(entries::SessionRows::message_count)
                .or(row.message_count),
            None,
            generation,
        );
        let mut statement = batch.prepare_cached(&catalogue::insert_sql())?;
        statement.raw_bind_parameter(1, row.storage)?;
        statement.raw_bind_parameter(2, keys.root)?;
        statement.raw_bind_parameter(3, &row.cache_key)?;
        for (offset, value) in values.iter().enumerate() {
            statement.raw_bind_parameter(offset + 4, value)?;
        }
        statement.raw_execute()?;
        drop(statement);
        let session = batch.last_insert_rowid();
        positional::SESSION_FILES.write(
            &batch,
            session,
            &row.file_rows(&keys),
            Vec::new(),
            catalogue::file_key_of,
        )?;
        write_transcript_reads(&batch, session, row, &keys)?;
        write_checkpoint(&batch, session, record, &keys.places)?;
        write_parse_errors(&batch, session, row)?;
        refresh::commit(batch, &self.owner)?;
        Ok((session, keys))
    }

    /// Replace the [`catalogue::WRITING`] status of `session`'s row with
    /// `status`, once its entries are written.
    fn set_final_status(
        &self,
        session: i64,
        status: SessionStatus,
        entries_error: Option<&str>,
    ) -> Result<()> {
        let batch = refresh::transaction(&self.connection, &self.owner)?;
        let changed = batch.execute(
            "UPDATE sessions SET status = ?1, entries_error = ?2 WHERE id = ?3 AND status = ?4",
            params![status.as_str(), entries_error, session, catalogue::WRITING],
        )?;
        if changed != 1 {
            return Err(AppError::SessionCacheSuperseded(session));
        }
        refresh::commit(batch, &self.owner)?;
        Ok(())
    }

    /// Deletes the rows of each session `storage` found at one of `cache_keys`
    /// under `root`.
    pub(crate) fn remove(
        &mut self,
        storage: SessionCache,
        root: &SessionRoot,
        cache_keys: &[String],
    ) -> Result<()> {
        let roots = self.session_roots(root);
        let batch = refresh::transaction(&self.connection, &self.owner)?;
        {
            let mut statement = batch.prepare_cached(
                "DELETE FROM sessions WHERE storage = ?1 AND cache_key = ?3 \
                 AND root = (SELECT id FROM roots WHERE key = ?2)",
            )?;
            for cache_key in cache_keys {
                statement.execute(params![
                    storage.directory,
                    roots.key().map(RootKey::as_str),
                    cache_key
                ])?;
            }
        }
        refresh::commit(batch, &self.owner)?;
        Ok(())
    }
}

/// A writable cache and the refresh ownership its writes check, released
/// after the cache closes. Tests write through it as a refresh owner does.
#[cfg(test)]
pub(crate) struct OwnedCache {
    writer: CacheWriter,
    ownership: refresh::RefreshOwnership,
}

#[cfg(test)]
impl OwnedCache {
    pub(crate) fn token(&self) -> &refresh::RefreshToken {
        self.ownership.token()
    }
}

#[cfg(test)]
impl std::ops::Deref for OwnedCache {
    type Target = CacheWriter;

    fn deref(&self) -> &CacheWriter {
        &self.writer
    }
}

#[cfg(test)]
impl std::ops::DerefMut for OwnedCache {
    fn deref_mut(&mut self) -> &mut CacheWriter {
        &mut self.writer
    }
}

#[cfg(test)]
impl SqliteCache {
    /// Opens `base`'s database as [`Self::open_for_refresh`] does, acquiring
    /// refresh ownership of `base` for as long as the result lives.
    pub(crate) fn open(base: &CacheBase) -> Result<OwnedCache> {
        let ownership = refresh::RefreshOwnership::acquire_for_test(base)?;
        let writer = Self::open_for_refresh(ownership.token())?;
        Ok(OwnedCache { writer, ownership })
    }
}

fn read_meta(connection: &Connection, key: &str) -> Result<Option<String>> {
    Ok(connection
        .query_row("SELECT value FROM meta WHERE key = ?1", [key], |row| {
            row.get(0)
        })
        .optional()?)
}

fn put_meta(connection: &Connection, key: &str, value: &str) -> Result<()> {
    connection.execute(
        "INSERT INTO meta (key, value) VALUES (?1, ?2) ON CONFLICT (key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )?;
    Ok(())
}

fn storage_schema_key(storage: SessionCache) -> String {
    format!("storage_schema:{}", storage.directory)
}

/// True when `storage`'s sessions were written under the
/// `SessionCache::schema_version` this release gives the storage.
fn is_storage_current(connection: &Connection, storage: SessionCache) -> Result<bool> {
    Ok(
        read_meta(connection, &storage_schema_key(storage))?.as_deref()
            == Some(storage.schema_version.to_string().as_str()),
    )
}

/// A generation no earlier write in this database gave a session row.
fn next_generation(connection: &Connection) -> Result<i64> {
    Ok(connection.query_row(
        "INSERT INTO meta (key, value) VALUES ('last_generation', '1') \
         ON CONFLICT (key) DO UPDATE SET value = CAST(value AS INTEGER) + 1 \
         RETURNING CAST(value AS INTEGER)",
        [],
        |row| row.get(0),
    )?)
}

/// The keys `row` is stored under: the rows of the roots its files are
/// placed below, inserted when missing, and the stored path of each of its
/// files.
fn catalogue_keys(
    connection: &Connection,
    row: &catalogue::CatalogueRow,
) -> Result<catalogue::CatalogueKeys> {
    let placed: Vec<&roots::FilePlace> = row.files.iter().map(|file| &file.place).collect();
    let session = row.files.first().map_or(Path::new(""), |own| &own.path);
    let (places, files) = WrittenPlaces::of(connection, &row.roots, session, &placed)?;
    Ok(catalogue::CatalogueKeys {
        root: places.written_listing_root(),
        files,
        places,
    })
}

fn write_transcript_reads(
    connection: &Connection,
    session: i64,
    row: &catalogue::CatalogueRow,
    keys: &catalogue::CatalogueKeys,
) -> Result<()> {
    let table = positional::TRANSCRIPT_READS;
    let stored = table.stored(connection, session)?;
    table.write(
        connection,
        session,
        &row.read_rows(keys),
        stored,
        catalogue::read_key_of,
    )
}

/// Updates the stored session row `session` to `record`'s, with
/// `message_count`, and writes its files, their reads, its checkpoint, its
/// parse errors and its sub-agent metadata parts. Returns the generation it
/// gave the row.
fn write_catalogue_rows(
    connection: &Connection,
    session: i64,
    record: &SessionRecord,
    keys: &catalogue::CatalogueKeys,
    message_count: Option<i64>,
) -> Result<i64> {
    let row = &record.catalogue;
    let generation = next_generation(connection)?;
    let values = row.values(row.status.as_str(), message_count, None, generation);
    let mut statement = connection.prepare_cached(&catalogue::update_sql())?;
    for (offset, value) in values.iter().enumerate() {
        statement.raw_bind_parameter(offset + 1, value)?;
    }
    statement.raw_bind_parameter(values.len() + 1, session)?;
    statement.raw_execute()?;
    drop(statement);
    let stored_files = positional::SESSION_FILES.stored(connection, session)?;
    write_subagent_metadata(connection, session, record, keys, &stored_files)?;
    positional::SESSION_FILES.write(
        connection,
        session,
        &row.file_rows(keys),
        stored_files,
        catalogue::file_key_of,
    )?;
    write_transcript_reads(connection, session, row, keys)?;
    write_checkpoint(connection, session, record, &keys.places)?;
    write_parse_errors(connection, session, row)?;
    Ok(generation)
}

/// Stores where a resumed read of `record`'s session starts, its transcripts
/// named below the roots of `places`, or removes the stored checkpoint of a
/// session whose next change is read whole.
fn write_checkpoint(
    connection: &Connection,
    session: i64,
    record: &SessionRecord,
    places: &Places,
) -> Result<()> {
    let stored = record
        .checkpoint
        .as_ref()
        .and_then(|checkpoint| checkpoint.stored_in(places));
    match stored {
        Some((checkpoint, ledger)) => {
            connection
                .prepare_cached(
                    "INSERT INTO session_checkpoints (session, sequence, checkpoint, ledger) \
                     VALUES (?1, ?2, ?3, ?4) ON CONFLICT (session) DO UPDATE SET \
                     sequence = excluded.sequence, checkpoint = excluded.checkpoint, \
                     ledger = excluded.ledger WHERE sequence != excluded.sequence \
                     OR checkpoint != excluded.checkpoint OR ledger != excluded.ledger",
                )?
                .execute(params![
                    session,
                    checkpoint.sequence as i64,
                    checkpoint.encode(),
                    ledger
                ])?;
        }
        None => {
            connection
                .prepare_cached("DELETE FROM session_checkpoints WHERE session = ?1")?
                .execute([session])?;
        }
    }
    Ok(())
}

fn write_parse_errors(
    connection: &Connection,
    session: i64,
    row: &catalogue::CatalogueRow,
) -> Result<()> {
    let table = positional::PARSE_ERRORS;
    let stored = table.stored(connection, session)?;
    table.write(
        connection,
        session,
        &row.parse_error_rows(),
        stored,
        |values| table.integer(values, "line_number"),
    )
}

fn write_malformed_lines(
    connection: &Connection,
    session: i64,
    entries: &TranscriptEntries,
) -> Result<()> {
    let table = positional::MALFORMED_LINES;
    let stored = table.stored(connection, session)?;
    table.write(
        connection,
        session,
        &entries::malformed_line_rows(&entries.malformed_lines),
        stored,
        |values| table.integer(values, "line_number"),
    )
}

/// Writes the sub-agent metadata parts of `record`. `keys` places its files;
/// `stored_files` are its session's stored files before this write.
fn write_subagent_metadata(
    connection: &Connection,
    session: i64,
    record: &SessionRecord,
    keys: &catalogue::CatalogueKeys,
    stored_files: &[(i64, Vec<rusqlite::types::Value>)],
) -> Result<()> {
    let stored_places: HashMap<i64, roots::StoredPath> = stored_files
        .iter()
        .map(|(position, values)| Ok((*position, catalogue::file_place_of(values)?)))
        .collect::<Result<_>>()?;
    let table = positional::SUBAGENT_METADATA_PARTS;
    let stored = table.stored(connection, session)?;
    table.write(
        connection,
        session,
        &catalogue::CatalogueRow::subagent_metadata_rows(keys, &record.subagent_metadata),
        stored,
        |values| {
            Ok(stored_places
                .get(&table.integer(values, "file_position")?)
                .cloned())
        },
    )
}

fn session_transaction<'a>(
    connection: &'a mut Connection,
    session: i64,
    owner: &'a refresh::RefreshToken,
) -> Result<refresh::WriteBatch<'a>> {
    let batch = refresh::transaction(connection, owner)?;
    let is_writing: bool = batch.query_row(
        "SELECT EXISTS (SELECT 1 FROM sessions WHERE id = ?1 AND status = ?2)",
        params![session, catalogue::WRITING],
        |row| row.get(0),
    )?;
    if !is_writing {
        return Err(AppError::SessionCacheSuperseded(session));
    }
    Ok(batch)
}

/// A change the writer applies to a cache base's database.
pub(crate) enum CacheWrite {
    Session(Box<SessionRecord>),
    /// A resumed read's record, whose writer reports the generation it gave
    /// the session's row, or why the write failed.
    Resumed {
        record: Box<SessionRecord>,
        reply: std::sync::mpsc::Sender<std::result::Result<i64, String>>,
    },
    /// The sessions `storage` no longer finds at these cache keys under the
    /// root.
    Removed {
        storage: SessionCache,
        root: SessionRoot,
        cache_keys: Vec<String>,
    },
}

/// Applies every write `writes` receives to `writer`, in order, until every
/// sender is gone or the owner's write permission is revoked, then
/// checkpoints. Other write failures are reported under `--debug`, and the
/// remaining records are written.
pub(crate) fn apply_writes(
    mut writer: CacheWriter,
    writes: Receiver<CacheWrite>,
    debug_level: Option<DebugLevel>,
) {
    let mut reported = HashSet::new();
    for write in writes {
        if !writer.can_publish() {
            break;
        }
        let mut write_logged = |record: &SessionRecord| {
            let started = std::time::Instant::now();
            let changes_before = writer.connection.total_changes();
            let outcome = writer.write(record);
            // SQLite counts the rows each statement changed, not those a
            // cascade deleted.
            debug::debug(
                debug_level,
                &format!(
                    "Wrote session {} to the session cache database: {} rows changed in {} ms",
                    record.catalogue.cache_key,
                    writer.connection.total_changes() - changes_before,
                    started.elapsed().as_millis()
                ),
            );
            outcome
        };
        let outcome = match &write {
            CacheWrite::Session(record) => write_logged(record).map(|_| ()),
            CacheWrite::Resumed { record, reply } => {
                let outcome = write_logged(record);
                let _ = reply.send(outcome.as_ref().copied().map_err(ToString::to_string));
                outcome.map(|_| ())
            }
            CacheWrite::Removed {
                storage,
                root,
                cache_keys,
            } => writer.remove(*storage, root, cache_keys),
        };
        if let Err(error) = outcome {
            let message = error.to_string();
            if reported.insert(message.clone()) {
                debug::warn(
                    debug_level,
                    &format!("Failed to write the session cache database: {message}"),
                );
            }
        }
    }
    if let Err(error) = writer.checkpoint_after_writes() {
        debug::warn(
            debug_level,
            &format!("Failed to checkpoint the session cache database: {error}"),
        );
    }
}

#[cfg(test)]
pub(crate) mod tests;
