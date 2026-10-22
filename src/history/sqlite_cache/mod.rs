//! The session cache database, `history.sqlite3` in the cache base: every
//! session's catalogue row and the entries `session_entries` returns for
//! it. Catalogue, viewer and agent readers share it.
//!
//! Each session's rows are written in short transactions outside the parse:
//! its catalogue row first, marked [`catalogue::WRITING`], then its entries
//! a slice at a time, then the row's status. A row still marked writing,
//! left by a run that stopped, is not current, and the next load writes the
//! session again.

mod admission;
mod base;
mod cached_session;
mod catalogue;
pub(crate) mod connection;
mod entries;
mod files;
mod free_space;
mod interactive;
mod next_step;
mod read_catalogue;
pub(crate) mod refresh;
mod schema;
mod search_parts;

pub(crate) use admission::admit_first_build;
pub use base::{CacheBase, ProcessIdentity};
pub use cached_session::CachedSession;
pub(crate) use cached_session::find_waiting as find_cached_session_waiting;
pub(crate) use catalogue::{SessionRecord, SessionStatus, subagents_as_listed};
pub(crate) use entries::StoredEntries;
pub use interactive::{DrawingThread, InteractiveCache, InteractiveRead};
pub(crate) use next_step::Fallback;
pub use next_step::NextStep;
pub(crate) use read_catalogue::CatalogueSession;
pub(crate) use search_parts::{SearchField, SearchPart};

use crate::cli::DebugLevel;
use crate::debug;
use crate::error::{AppError, Result};
#[cfg(test)]
use crate::history::TranscriptEntries;
use crate::history::fingerprint::CachedFingerprint;
use crate::history::provider::SessionCache;
use connection::Access;
use entries::path_text;
use rusqlite::{Connection, OptionalExtension, params};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::Receiver;
use std::time::Duration;

/// The database's file name in the cache base.
pub(crate) const DATABASE_FILE_NAME: &str = "history.sqlite3";

const PAGE_SIZE: u32 = 4096;

/// The busy timeout of the waiting reader and the writers: loads, refreshes,
/// agent commands and the TUI's background refresh thread. The TUI's drawing
/// thread opens neither; see [`interactive`].
const WAITING_BUSY_TIMEOUT: Duration = Duration::from_secs(30);

/// The busy timeout of cache reads on the TUI's drawing thread. Past it, the
/// read returns a miss with its next step: read the session again once the
/// refresh owning the cache completes, or request a session refresh.
const INTERACTIVE_BUSY_TIMEOUT: Duration = Duration::from_millis(150);

/// A reader's busy timeout.
#[derive(Clone, Copy, Debug)]
enum ReaderWait {
    /// At most [`WAITING_BUSY_TIMEOUT`]: loads, refreshes, agent commands and
    /// the TUI's background refresh thread.
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

/// The size the write-ahead log is truncated to after a checkpoint.
const JOURNAL_SIZE_LIMIT: u64 = 64 * 1024 * 1024;
const ADMISSION_COMPLETE: &str = "admission_complete";

fn database_path(base: &Path) -> PathBuf {
    base.join(DATABASE_FILE_NAME)
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
    connection: Connection,
    /// The base this connection's database lives in, as the rearview that
    /// opened it reaches it.
    base: CacheBase,
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
    /// Another schema version or page size: a writer rebuilds it.
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
        if !path.is_file() || base.is_refused() {
            return Ok(None);
        }
        let connection = connection::open(&path, Access::ReadOnly)?;
        connection.busy_timeout(wait.busy_timeout())?;
        let cache = Self {
            connection,
            base: base.clone(),
            wait,
        };
        if cache.stored_layout()? != StoredLayout::Current || !cache.is_admitted()? {
            return Ok(None);
        }
        Ok(Some(cache))
    }

    /// Opens `base`'s database for `owner`'s refresh, creating it and its
    /// tables when missing, and emptying it when its tables are of another
    /// schema version or page size. A failure revokes `owner`'s write
    /// permission.
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
        Ok(
            if self.meta(schema::SCHEMA_VERSION_KEY)? == Some(schema::SCHEMA_VERSION.to_string()) {
                StoredLayout::Current
            } else {
                StoredLayout::Outdated
            },
        )
    }

    fn meta(&self, key: &str) -> Result<Option<String>> {
        read_meta(&self.connection, key)
    }

    /// The cache base this connection's database lives in.
    fn base(&self) -> &CacheBase {
        &self.base
    }

    pub(crate) fn storage_is_current(&self, storage: SessionCache) -> Result<bool> {
        is_storage_current(&self.connection, storage)
    }

    /// The fingerprint of every current session `storage` found under
    /// `root`, by cache key.
    pub(crate) fn current_fingerprints(
        &self,
        storage: SessionCache,
        root: &Path,
    ) -> Result<HashMap<String, CachedFingerprint>> {
        let mut statement = self.connection.prepare_cached(
            "SELECT cache_key, file_size, mtime_secs, mtime_nsecs, id FROM sessions \
             WHERE storage = ?1 AND root = ?2 AND status != ?3 AND entries_error IS NULL \
             AND COALESCE(is_missing_earlier_turns, 0) = 0",
        )?;
        let candidates = statement
            .query_map(
                params![storage.directory, path_text(root), catalogue::WRITING],
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
            match files::validate(&self.connection, session) {
                Ok(()) => {
                    fingerprints.insert(key, stamp);
                }
                Err(AppError::SessionCacheSourceChanged(_)) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(fingerprints)
    }

    pub(crate) fn stored_keys(&self, storage: SessionCache, root: &Path) -> Result<Vec<String>> {
        Ok(self
            .connection
            .prepare_cached("SELECT cache_key FROM sessions WHERE storage = ?1 AND root = ?2")?
            .query_map(params![storage.directory, path_text(root)], |row| {
                row.get(0)
            })?
            .collect::<rusqlite::Result<_>>()?)
    }

    pub(crate) fn holds(
        &self,
        storage: SessionCache,
        root: &Path,
        stub: &crate::history::provider::SessionStub,
    ) -> Result<bool> {
        let Some(stamp) = stub.fingerprint.stamp() else {
            return Ok(false);
        };
        self.with_read_snapshot(|connection| {
            let session: Option<i64> = connection.query_row(
                "SELECT id FROM sessions WHERE storage = ?1 AND root = ?2 AND cache_key = ?3 \
                 AND file_size = ?4 AND mtime_secs = ?5 AND mtime_nsecs = ?6 AND status != 'writing' \
                 AND entries_error IS NULL AND COALESCE(is_missing_earlier_turns, 0) = 0",
                params![storage.directory, path_text(root), stub.cache_key, stamp.file_size as i64, stamp.mtime_secs as i64, stamp.mtime_nsecs],
                |row| row.get(0),
            ).optional()?;
            let Some(session) = session else { return Ok(false) };
            match files::validate(connection, session) {
                Ok(()) => Ok(true),
                Err(AppError::SessionCacheSourceChanged(_)) => Ok(false),
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
        root: &Path,
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
        root: &Path,
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
        root: &Path,
        cache_key: &str,
        messages: Option<std::ops::RangeInclusive<u64>>,
    ) -> Result<Option<TranscriptEntries>> {
        let Some(session) =
            entries::session_id_of(connection, storage.directory, &path_text(root), cache_key)?
        else {
            return Ok(None);
        };
        files::validate(connection, session)?;
        let entries = entries::read(connection, session, messages)?;
        Ok(Some(entries))
    }
}

impl CacheWriter {
    fn open(owner: &refresh::RefreshToken) -> Result<Self> {
        let mut writer = Self::connect(owner.clone())?;
        if writer.stored_layout()? == StoredLayout::Outdated {
            writer.rebuild()?;
            // Closing releases the maintenance connection's retained exclusive
            // lock. Reopen normally before returning to WAL and shared readers.
            drop(writer);
            writer = Self::connect(owner.clone())?;
        }
        writer.create_schema()?;
        Ok(writer)
    }

    fn connect(owner: refresh::RefreshToken) -> Result<Self> {
        interactive::refuse_the_drawing_thread("open a session cache writer");
        let base = owner.cache_base();
        std::fs::create_dir_all(base.path())?;
        let connection = connection::open(&database_path(base.path()), Access::Create)?;
        connection.busy_timeout(WAITING_BUSY_TIMEOUT)?;
        connection.pragma_update(None, "page_size", PAGE_SIZE)?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "synchronous", "NORMAL")?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        connection.pragma_update(None, "journal_size_limit", JOURNAL_SIZE_LIMIT as i64)?;
        Ok(Self {
            cache: SqliteCache {
                connection,
                base,
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
        // With exclusive locking in WAL mode, BEGIN EXCLUSIVE waits up to the
        // busy timeout for every other connection to close, and keeps the
        // lock across commits and VACUUM until this connection closes.
        self.connection
            .pragma_update(None, "locking_mode", "EXCLUSIVE")?;
        let batch = refresh::exclusive_transaction(&self.connection, turn)?;
        refresh::commit(batch, &self.owner)?;
        let mode: String =
            self.connection
                .query_row("PRAGMA journal_mode = DELETE", [], |row| row.get(0))?;
        if !mode.eq_ignore_ascii_case("delete") {
            return Err(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
                Some(format!(
                    "The session cache rebuild could not leave WAL mode: {mode}"
                )),
            )
            .into());
        }
        Ok(())
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
        batch.execute_batch("DROP TABLE IF EXISTS message_times")?;
        for table in schema::TABLES {
            batch.execute_batch(&format!("DROP TABLE IF EXISTS {table}"))?;
        }
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
        put_meta(
            &batch,
            schema::SCHEMA_VERSION_KEY,
            &schema::SCHEMA_VERSION.to_string(),
        )?;
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

    /// Moves every page of the write-ahead log into the database and
    /// truncates the log.
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

    /// Records what the first build's sample measured, by name.
    fn record_projection(&self, measures: &[(&str, String)]) -> Result<()> {
        for (name, value) in measures {
            self.set_meta(name, value)?;
        }
        Ok(())
    }

    /// Replaces the rows of `record`'s session with `record`.
    pub(crate) fn write(&mut self, record: &SessionRecord) -> Result<()> {
        if !self.is_admitted()? {
            return Err(AppError::SessionCacheNotReady {
                session: record.catalogue.cache_key.clone(),
                reason: "disk admission has not completed".to_owned(),
            });
        }
        self.write_checked(record, &|| Ok(()))
    }

    fn write_checked(
        &mut self,
        record: &SessionRecord,
        check_space: &dyn Fn() -> Result<()>,
    ) -> Result<()> {
        check_space()?;
        let session = self.write_catalogue(record)?;
        let owner = &self.owner;
        let entries_error = match &record.entries {
            Some(Ok(entries)) => {
                entries::write(
                    &mut self.cache.connection,
                    session,
                    entries,
                    check_space,
                    owner,
                )?;
                for (chunk_index, chunk) in record.search_parts.chunks(20_000).enumerate() {
                    check_space()?;
                    let batch = session_transaction(&mut self.cache.connection, session, owner)?;
                    {
                        let mut statement = batch.prepare_cached("INSERT INTO subagent_search_parts (session, file_position, position, sanitise_text, text) VALUES (?1, ?2, ?3, ?4, ?5)")?;
                        for (offset, part) in chunk.iter().enumerate() {
                            statement.execute(params![
                                session,
                                part.file_position as i64,
                                (chunk_index * 20_000 + offset) as i64,
                                part.sanitise_text,
                                part.text
                            ])?;
                        }
                    }
                    refresh::commit(batch, owner)?;
                }
                None
            }
            Some(Err(error)) => Some(error.as_str()),
            None => None,
        };
        check_space()?;
        self.set_final_status(session, record.catalogue.status, entries_error)
    }

    fn write_catalogue(&mut self, record: &SessionRecord) -> Result<i64> {
        let row = &record.catalogue;
        let batch = refresh::transaction(&self.connection, &self.owner)?;
        batch.execute(
            "DELETE FROM sessions WHERE storage = ?1 AND root = ?2 AND cache_key = ?3",
            params![row.storage, row.root, row.cache_key],
        )?;
        batch.execute(
            "INSERT INTO sessions (storage, source, root, cache_key, locator, canonical_path, \
             status, file_size, mtime_secs, mtime_nsecs, session_id, session_id_key, handle, \
             project_path, project_name, cwd, timestamp_ms, preview_first, preview_last, \
             summary, custom_title, model, total_tokens, duration_minutes, message_count, \
             assistant_messages, is_missing_earlier_turns) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, \
             ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, \
             ?25, ?26, ?27)",
            params![
                row.storage,
                row.source,
                row.root,
                row.cache_key,
                row.locator,
                row.canonical_path,
                catalogue::WRITING,
                row.fingerprint.file_size as i64,
                row.fingerprint.mtime_secs as i64,
                row.fingerprint.mtime_nsecs,
                row.session_id,
                row.session_id_key,
                row.handle,
                row.project_path,
                row.project_name,
                row.cwd,
                row.timestamp_ms,
                row.preview_first,
                row.preview_last,
                row.summary,
                row.custom_title,
                row.model,
                row.total_tokens,
                row.duration_minutes,
                row.message_count,
                row.assistant_messages,
                row.is_missing_earlier_turns,
            ],
        )?;
        let session = batch.last_insert_rowid();
        {
            let mut statement = batch.prepare_cached(
                "INSERT INTO session_files (session, position, kind, path, file_size, mtime_secs, \
                 mtime_nsecs) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )?;
            for (position, file) in row.files.iter().enumerate() {
                statement.execute(params![
                    session,
                    position as i64,
                    file.kind,
                    file.path,
                    file.fingerprint
                        .map(|fingerprint| fingerprint.file_size as i64),
                    file.fingerprint
                        .map(|fingerprint| fingerprint.mtime_secs as i64),
                    file.fingerprint.map(|fingerprint| fingerprint.mtime_nsecs),
                ])?;
            }
            let mut statement = batch.prepare_cached(
                "INSERT INTO parse_errors (session, position, line_number, line_content, \
                 error_message, context_before, context_after) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )?;
            for (position, error) in row.parse_errors.iter().enumerate() {
                statement.execute(params![
                    session,
                    position as i64,
                    error.line_number as i64,
                    error.line_content,
                    error.error_message,
                    serde_json::to_string(&error.context_before)?,
                    serde_json::to_string(&error.context_after)?,
                ])?;
            }
        }
        refresh::commit(batch, &self.owner)?;
        Ok(session)
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
        root: &Path,
        cache_keys: &[String],
    ) -> Result<()> {
        let batch = refresh::transaction(&self.connection, &self.owner)?;
        {
            let mut statement = batch.prepare_cached(
                "DELETE FROM sessions WHERE storage = ?1 AND root = ?2 AND cache_key = ?3",
            )?;
            let root = path_text(root);
            for cache_key in cache_keys {
                statement.execute(params![storage.directory, root, cache_key])?;
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
        let ownership = refresh::RefreshOwnership::acquire(base, || ())?;
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
    /// The sessions `storage` no longer finds at these cache keys under the
    /// root.
    Removed {
        storage: SessionCache,
        root: PathBuf,
        cache_keys: Vec<String>,
    },
}

/// Applies every write `writes` receives to `writer`, in order, until every
/// sender is gone or the owner's write permission is revoked. Other write
/// failures are reported under `--debug`, and the remaining records are
/// written.
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
        let outcome = match &write {
            CacheWrite::Session(record) => writer.write(record),
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
}

#[cfg(test)]
pub(crate) mod tests;
