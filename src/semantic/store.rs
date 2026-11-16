//! The embedding of each passage, one row per passage, in a SQLite file of
//! its own in the semantic cache folder. A run reads the rows of the
//! passages it searches and writes the rows it embedded.

use crate::clock::Clock;
use crate::error::{AppError, CacheDeletion};
use crate::free_space::{DiskSpace, FREE_SPACE_FLOOR, SystemDiskSpace};
use crate::semantic::embed::is_valid_embedding;
use crate::semantic::types::{MODEL_DIMENSIONS, MODEL_NAME, SemanticCancellationToken};
use crate::sqlite::is_busy;
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

mod import;

/// The store's file name in the semantic cache folder.
pub(crate) const PASSAGE_STORE_FILE: &str = "embeddings-v2.sqlite3";
/// The layout of the store's tables and the way a text becomes an embedding.
/// A store of another version is emptied.
const STORE_VERSION: u32 = 1;
/// Five rows of a 384-value embedding fit a page of this size; the default
/// 4 KB page holds two.
const PAGE_SIZE: u32 = 8192;
/// Every write transaction but the import's is one batch of embeddings, one
/// stamp update or one deletion of unused passages.
const BUSY_TIMEOUT: Duration = Duration::from_secs(2);
/// A read checks for cancellation once in this many keys.
const KEYS_PER_CANCELLATION_CHECK: usize = 1_024;
/// A passage read again within this many days of its stamp keeps the stamp:
/// a session searched daily rewrites its stamps weekly.
const STAMP_REFRESH_DAYS: i64 = 7;
/// A passage read within this many days is kept: a session's embeddings
/// outlast a quarter without a search of it.
const UNUSED_DAYS_KEPT: i64 = 180;
/// A passage whose stamp is older than this many days is deleted. A read up
/// to [`STAMP_REFRESH_DAYS`] after the stamp leaves it as it is, and that
/// read still keeps the passage for [`UNUSED_DAYS_KEPT`] days.
const STAMP_DAYS_KEPT: i64 = UNUSED_DAYS_KEPT + STAMP_REFRESH_DAYS;
/// The unused passages are deleted by the first run that writes this many
/// days after the last deletion.
const DELETION_INTERVAL_DAYS: i64 = 7;
const SECONDS_PER_DAY: i64 = 24 * 60 * 60;

const SCHEMA: &str = "
    DROP TABLE IF EXISTS passage_uses;
    DROP TABLE IF EXISTS passages;
    DROP TABLE IF EXISTS meta;
    CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL) WITHOUT ROWID;
    CREATE TABLE passages (
        id INTEGER PRIMARY KEY,
        key BLOB NOT NULL UNIQUE,
        embedding BLOB NOT NULL
    );
    -- Apart from `passages`: a stamp update there would rewrite the page
    -- of every embedding it touches.
    CREATE TABLE passage_uses (
        id INTEGER PRIMARY KEY REFERENCES passages (id) ON DELETE CASCADE,
        used_day INTEGER NOT NULL
    );
";
const SELECT_ID: &str = "SELECT id FROM passages WHERE key = ?1";
const SELECT_PASSAGE: &str = "SELECT embedding, used_day
    FROM passages LEFT JOIN passage_uses USING (id) WHERE id = ?1";
/// Replaces the embedding of a key already stored. The stored embedding is
/// one of:
/// - equal to the new one
/// - written by another rearview after this run's read
/// - invalid
const UPSERT_PASSAGE: &str = "INSERT INTO passages (key, embedding) VALUES (?1, ?2)
    ON CONFLICT (key) DO UPDATE SET embedding = excluded.embedding RETURNING id";
/// Stamps the passage `?1` when it exists: another rearview's deletion may
/// have removed a passage this run read.
const UPSERT_STAMP: &str = "INSERT INTO passage_uses (id, used_day)
    SELECT id, ?2 FROM passages WHERE id = ?1
    ON CONFLICT (id) DO UPDATE SET used_day = excluded.used_day";
/// The day of the last deletion of unused passages, in `meta`.
const SELECT_DELETION_DAY: &str = "SELECT value FROM meta WHERE key = 'deletion_day'";
const UPSERT_DELETION_DAY: &str = "INSERT INTO meta (key, value) VALUES ('deletion_day', ?1)
    ON CONFLICT (key) DO UPDATE SET value = excluded.value";
/// Each deleted passage's stamp is deleted by the foreign key's cascade.
const DELETE_PASSAGES_USED_BEFORE: &str =
    "DELETE FROM passages WHERE id IN (SELECT id FROM passage_uses WHERE used_day < ?1)";

/// A passage's key in the store: the BLAKE3 hash of its text.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct PassageKey([u8; 32]);

impl PassageKey {
    pub fn of(text: &str) -> Self {
        Self(*blake3::hash(text.as_bytes()).as_bytes())
    }
}

/// The reason a run's embeddings were not stored.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SkipReason {
    /// The store's drive has less than [`FREE_SPACE_FLOOR`] free.
    LowSpace,
    /// Another rearview held the store's write lock past the busy timeout.
    Busy,
    /// The store did not open, or a statement failed.
    Error,
}

impl SkipReason {
    pub fn label(self) -> &'static str {
        match self {
            Self::LowSpace => "low-space",
            Self::Busy => "busy",
            Self::Error => "error",
        }
    }

    /// The reason as `--generate-semantic-cache` reports it.
    pub fn sentence(self) -> &'static str {
        match self {
            Self::LowSpace => "the drive is low on space",
            Self::Busy => "another rearview was writing the embeddings",
            Self::Error => "the embeddings file could not be written",
        }
    }
}

/// The passages one run read from the store and wrote to it.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct StoreUse {
    /// Distinct passages read with a valid embedding.
    pub read: usize,
    pub written: usize,
    /// Embedded passages whose write was skipped or failed. A later run
    /// embeds them again.
    pub skipped: usize,
    /// The reason of the last skipped write.
    pub skip_reason: Option<SkipReason>,
}

/// Receives the message of a store failure.
type FailureLog = Arc<dyn Fn(&str) + Send + Sync>;

/// The location of the stored embeddings. It holds the store's path, and a
/// refresh keeps a connection from its read to its end. Between refreshes
/// `--clear-semantic-cache` deletes the file: Windows does not delete a file
/// a running rearview has open.
pub struct PassageStore {
    /// `None` without a cache folder: every read misses and every write is
    /// skipped.
    path: Option<PathBuf>,
    dimensions: usize,
    /// [`BUSY_TIMEOUT`]. Tests of a busy store set it to zero: a write then
    /// fails at once.
    busy_timeout: Duration,
    /// [`import::BUSY_TIMEOUT`]. Tests of an import into a busy store set it
    /// to zero.
    import_busy_timeout: Duration,
    clock: Arc<dyn Clock>,
    disk: Arc<dyn DiskSpace>,
    /// Appends to the debug log. Tests record the messages: the debug log is
    /// a file in the user's home directory.
    failure_log: FailureLog,
}

impl PassageStore {
    /// The store in `semantic_folder`, the semantic cache folder.
    pub fn in_folder(semantic_folder: Option<&Path>, clock: Arc<dyn Clock>) -> Self {
        let log_clock = clock.clone();
        Self::at(
            semantic_folder.map(|folder| folder.join(PASSAGE_STORE_FILE)),
            MODEL_DIMENSIONS,
            clock,
            Arc::new(SystemDiskSpace),
            Arc::new(move |message| {
                let _ = crate::debug_log::log_debug(message, log_clock.as_ref());
            }),
        )
    }

    fn at(
        path: Option<PathBuf>,
        dimensions: usize,
        clock: Arc<dyn Clock>,
        disk: Arc<dyn DiskSpace>,
        failure_log: FailureLog,
    ) -> Self {
        Self {
            path,
            dimensions,
            busy_timeout: BUSY_TIMEOUT,
            import_busy_timeout: import::BUSY_TIMEOUT,
            clock,
            disk,
            failure_log,
        }
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Fail when a run could not store the embeddings it computes: the drive
    /// is low on space, or the store does not open.
    pub fn fail_unless_writable(&self) -> Result<(), AppError> {
        if let Some(free_bytes) = self.free_bytes_under_floor() {
            return Err(AppError::SessionCacheLowSpace {
                free_bytes,
                floor_bytes: FREE_SPACE_FLOOR,
                outcome: CacheDeletion::Pending,
            });
        }
        if self.open().connection.is_none() {
            let path = self.path.as_deref().map(Path::display);
            return Err(AppError::PassageStoreNotOpened(path.map_or_else(
                || "no cache directory".to_owned(),
                |path| path.to_string(),
            )));
        }
        Ok(())
    }

    /// Open the store for one run, creating it when it is missing and
    /// emptying it when it is of another version, model or dimension count,
    /// then import the embedding cache file of earlier releases when there
    /// is one. A store that does not open leaves the run without one.
    pub fn open(&self) -> OpenPassageStore<'_> {
        let mut open = OpenPassageStore {
            store: self,
            connection: None,
            was_created_again: false,
            today: self.clock.wall().timestamp().div_euclid(SECONDS_PER_DAY),
            stale_stamps: Vec::new(),
            has_written: false,
            has_logged_failure: false,
            usage: StoreUse::default(),
        };
        open.connect();
        open.import_old_file();
        open
    }

    /// The free bytes on the store's drive when they are fewer than
    /// [`FREE_SPACE_FLOOR`].
    fn free_bytes_under_floor(&self) -> Option<u64> {
        self.disk
            .available_bytes(self.path.as_deref()?)
            .filter(|&free| free < FREE_SPACE_FLOOR)
    }

    fn is_low_on_space(&self) -> bool {
        self.free_bytes_under_floor().is_some()
    }

    /// A connection to the store with its tables as [`SCHEMA`] creates
    /// them; `None` when the tables have to be created and the drive is low
    /// on space.
    fn open_connection(&self, path: &Path) -> rusqlite::Result<Option<Connection>> {
        let mut connection = Connection::open(path)?;
        connection.busy_timeout(self.busy_timeout)?;
        connection.pragma_update(None, "page_size", PAGE_SIZE)?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "synchronous", "NORMAL")?;
        connection.pragma_update(None, "foreign_keys", true)?;
        if !self.schema_matches(&connection)? {
            if self.is_low_on_space() {
                return Ok(None);
            }
            self.create_schema(&mut connection)?;
        }
        Ok(Some(connection))
    }

    fn expected_meta(&self) -> [(&'static str, String); 3] {
        [
            ("store_version", STORE_VERSION.to_string()),
            ("model", MODEL_NAME.to_string()),
            ("dimensions", self.dimensions.to_string()),
        ]
    }

    fn schema_matches(&self, connection: &Connection) -> rusqlite::Result<bool> {
        let has_meta: bool = connection.query_row(
            "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'meta')",
            [],
            |row| row.get(0),
        )?;
        if !has_meta {
            return Ok(false);
        }
        let stored = connection
            .prepare("SELECT key, value FROM meta")?
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<rusqlite::Result<HashMap<String, String>>>()?;
        Ok(self
            .expected_meta()
            .iter()
            .all(|(key, value)| stored.get(*key) == Some(value)))
    }

    fn create_schema(&self, connection: &mut Connection) -> rusqlite::Result<()> {
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        // Another rearview may have created the tables while this one waited
        // for the write lock.
        if self.schema_matches(&transaction)? {
            return Ok(());
        }
        transaction.execute_batch(SCHEMA)?;
        for (key, value) in self.expected_meta() {
            transaction.execute(
                "INSERT INTO meta (key, value) VALUES (?1, ?2)",
                (key, value),
            )?;
        }
        transaction.commit()
    }
}

/// The store, open for one run. Without a connection every read misses and
/// every write is skipped.
pub struct OpenPassageStore<'s> {
    store: &'s PassageStore,
    connection: Option<Connection>,
    /// True once this run deleted a damaged store and created it again; it
    /// does so once.
    was_created_again: bool,
    /// Days since the Unix epoch, UTC.
    today: i64,
    /// The ids read with a stamp older than [`STAMP_REFRESH_DAYS`].
    stale_stamps: Vec<i64>,
    /// True once a write transaction of this run committed.
    has_written: bool,
    /// True once this run wrote a failure to the debug log; it writes one.
    has_logged_failure: bool,
    usage: StoreUse,
}

/// The end of a read before its last key.
enum ReadStop {
    Cancelled,
    Failed(rusqlite::Error),
}

impl From<rusqlite::Error> for ReadStop {
    fn from(error: rusqlite::Error) -> Self {
        Self::Failed(error)
    }
}

impl OpenPassageStore<'_> {
    fn connect(&mut self) {
        let Some(path) = self.store.path.as_deref() else {
            return;
        };
        if let Some(folder) = path.parent() {
            if std::fs::create_dir_all(folder).is_err() {
                return;
            }
            // The whole-file cache of earlier releases left a temp file here
            // whenever a write stopped before its rename.
            crate::cache_file::sweep_stale_temp_files(folder, self.store.clock.as_ref());
        }
        match self.store.open_connection(path) {
            Ok(connection) => self.connection = connection,
            Err(error) => self.recover_from(&error),
        }
    }

    /// Write `error` to the debug log when it is this run's first failure.
    /// When it reports a damaged store, delete the store and create it
    /// again, once per run; a second damaged store, or one that cannot be
    /// deleted, leaves the run without a store.
    fn recover_from(&mut self, error: &rusqlite::Error) {
        self.log_first_failure(error);
        if !is_damaged(error) {
            return;
        }
        self.connection = None;
        let Some(path) = self.store.path.as_deref() else {
            return;
        };
        if self.was_created_again || delete_store_files(path).is_err() {
            return;
        }
        self.was_created_again = true;
        // The ids read from the deleted store name other rows in the new one.
        self.stale_stamps.clear();
        self.connection = self.store.open_connection(path).ok().flatten();
    }

    fn log_first_failure(&mut self, error: &rusqlite::Error) {
        if std::mem::replace(&mut self.has_logged_failure, true) {
            return;
        }
        let path = self.store.path.as_deref().unwrap_or(Path::new(""));
        (self.store.failure_log)(&format!("Passage store {} failed: {error}", path.display()));
    }

    /// The stored embedding of each of `keys` that has a valid one; `None`
    /// once `cancellation` is cancelled. A row whose embedding is not the
    /// store's dimension count of finite values is left out. After a failed
    /// read, the rows already read are returned.
    pub fn read<'k>(
        &mut self,
        keys: impl IntoIterator<Item = &'k PassageKey>,
        cancellation: &SemanticCancellationToken,
    ) -> Option<HashMap<PassageKey, Arc<[f32]>>> {
        let mut stored = HashMap::new();
        match self.read_into(keys, cancellation, &mut stored) {
            Ok(()) => {}
            Err(ReadStop::Cancelled) => return None,
            Err(ReadStop::Failed(error)) => self.recover_from(&error),
        }
        self.usage.read += stored.len();
        Some(stored)
    }

    fn read_into<'k>(
        &mut self,
        keys: impl IntoIterator<Item = &'k PassageKey>,
        cancellation: &SemanticCancellationToken,
        stored: &mut HashMap<PassageKey, Arc<[f32]>>,
    ) -> Result<(), ReadStop> {
        let Some(connection) = &self.connection else {
            return Ok(());
        };
        let stop_if_cancelled = |keys_read: usize| {
            if keys_read.is_multiple_of(KEYS_PER_CANCELLATION_CHECK) && cancellation.is_cancelled()
            {
                return Err(ReadStop::Cancelled);
            }
            Ok(())
        };
        let dimensions = self.store.dimensions;
        let transaction = connection.unchecked_transaction()?;
        let mut ids = Vec::new();
        let mut select_id = transaction.prepare(SELECT_ID)?;
        for (keys_read, key) in keys.into_iter().enumerate() {
            stop_if_cancelled(keys_read)?;
            let id = select_id
                .query_row([key.0.as_slice()], |row| row.get::<_, i64>(0))
                .optional()?;
            ids.extend(id.map(|id| (id, key)));
        }
        // Read the rows in id order: passages embedded together sit on the
        // same pages, and a read in key order fetches each page once per row.
        ids.sort_unstable_by_key(|(id, _)| *id);
        let mut select_passage = transaction.prepare(SELECT_PASSAGE)?;
        for (rows_read, (id, key)) in ids.into_iter().enumerate() {
            stop_if_cancelled(rows_read)?;
            let (embedding, used_day) = select_passage.query_row([id], |row| {
                let embedding = decoded_embedding(row.get_ref(0)?.as_blob()?, dimensions);
                Ok((embedding, row.get::<_, Option<i64>>(1)?))
            })?;
            let Some(embedding) = embedding else {
                continue;
            };
            if used_day.is_none_or(|day| self.today - day > STAMP_REFRESH_DAYS) {
                self.stale_stamps.push(id);
            }
            stored.insert(*key, embedding);
        }
        Ok(())
    }

    /// Store `passages` in one transaction, stamped today. A write that is
    /// skipped for low space, times out on a busy store or fails drops its
    /// rows and counts them in [`StoreUse::skipped`].
    pub fn write(&mut self, passages: &[(PassageKey, Arc<[f32]>)]) {
        if passages.is_empty() {
            return;
        }
        let today = self.today;
        let written = self.in_write_transaction(|transaction| {
            let mut upsert_passage = transaction.prepare(UPSERT_PASSAGE)?;
            let mut upsert_stamp = transaction.prepare(UPSERT_STAMP)?;
            for (key, embedding) in passages {
                let id: i64 = upsert_passage
                    .query_row((key.0.as_slice(), encoded_embedding(embedding)), |row| {
                        row.get(0)
                    })?;
                upsert_stamp.execute((id, today))?;
            }
            Ok(())
        });
        match written {
            Ok(()) => self.usage.written += passages.len(),
            Err(reason) => {
                self.usage.skipped += passages.len();
                self.usage.skip_reason = Some(reason);
            }
        }
    }

    fn in_write_transaction(
        &mut self,
        write: impl FnOnce(&Transaction<'_>) -> rusqlite::Result<()>,
    ) -> Result<(), SkipReason> {
        if self.store.is_low_on_space() {
            return Err(SkipReason::LowSpace);
        }
        let Some(connection) = self.connection.as_mut() else {
            return Err(SkipReason::Error);
        };
        let committed = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .and_then(|transaction| {
                write(&transaction)?;
                transaction.commit()
            });
        match committed {
            Ok(()) => {
                self.has_written = true;
                Ok(())
            }
            Err(error) if is_busy(&error) => Err(SkipReason::Busy),
            Err(error) => {
                self.recover_from(&error);
                Err(SkipReason::Error)
            }
        }
    }

    /// The rows in the store.
    pub fn passage_count(&self) -> usize {
        self.connection
            .as_ref()
            .and_then(|connection| {
                connection
                    .query_row("SELECT COUNT(*) FROM passages", [], |row| {
                        row.get::<_, i64>(0)
                    })
                    .ok()
            })
            .map_or(0, |rows| rows as usize)
    }

    /// End the run: stamp today the passages read with a stale stamp, delete
    /// the unused passages when their deletion is due, and return what the
    /// run read and wrote. A run that didn't write any embeddings and read
    /// fresh stamps alone doesn't write, and doesn't delete.
    pub fn finish(mut self) -> StoreUse {
        // The deletion reads the stamps: after a failed stamp update it
        // would delete passages this run read.
        let stamped = self.stamp_stale_passages_today();
        if stamped.is_ok() && self.has_written && self.is_deletion_due() {
            // Passages left undeleted are deleted by a later run that writes.
            let _ = self.delete_unused_passages();
        }
        self.usage
    }

    fn stamp_stale_passages_today(&mut self) -> Result<(), SkipReason> {
        let stale_stamps = std::mem::take(&mut self.stale_stamps);
        if stale_stamps.is_empty() {
            return Ok(());
        }
        let today = self.today;
        // A stamp left stale is updated by the next run that reads it.
        self.in_write_transaction(|transaction| {
            let mut upsert_stamp = transaction.prepare(UPSERT_STAMP)?;
            for id in &stale_stamps {
                upsert_stamp.execute((id, today))?;
            }
            Ok(())
        })
    }

    /// True when the store doesn't record a deletion of unused passages, or
    /// records one [`DELETION_INTERVAL_DAYS`] or more days ago.
    fn is_deletion_due(&self) -> bool {
        let Some(connection) = &self.connection else {
            return false;
        };
        let Ok(deletion_day) = connection
            .query_row(SELECT_DELETION_DAY, [], |row| row.get::<_, String>(0))
            .optional()
        else {
            return false;
        };
        deletion_day
            .and_then(|day| day.parse::<i64>().ok())
            .is_none_or(|day| self.today - day >= DELETION_INTERVAL_DAYS)
    }

    /// Delete the passages stamped more than [`STAMP_DAYS_KEPT`] days ago
    /// and record today as the day of the deletion. The file keeps its size:
    /// later rows reuse the deleted rows' pages.
    fn delete_unused_passages(&mut self) -> Result<(), SkipReason> {
        let today = self.today;
        self.in_write_transaction(|transaction| {
            transaction.execute(DELETE_PASSAGES_USED_BEFORE, [today - STAMP_DAYS_KEPT])?;
            transaction.execute(UPSERT_DELETION_DAY, [today.to_string()])?;
            Ok(())
        })
    }
}

fn is_damaged(error: &rusqlite::Error) -> bool {
    matches!(
        error.sqlite_error_code(),
        Some(rusqlite::ErrorCode::DatabaseCorrupt | rusqlite::ErrorCode::NotADatabase)
    )
}

/// Delete the store at `path` with its write-ahead log and shared-memory
/// files; a missing file counts as deleted.
fn delete_store_files(path: &Path) -> std::io::Result<()> {
    for suffix in ["", "-wal", "-shm"] {
        let mut file = path.as_os_str().to_owned();
        file.push(suffix);
        match std::fs::remove_file(&file) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => return Err(error),
            Ok(()) | Err(_) => {}
        }
    }
    Ok(())
}

/// `embedding` as the store keeps it: each value's four little-endian bytes.
fn encoded_embedding(embedding: &[f32]) -> Vec<u8> {
    embedding
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect()
}

/// The embedding `blob` encodes; `None` unless it is `dimensions` finite
/// values.
fn decoded_embedding(blob: &[u8], dimensions: usize) -> Option<Arc<[f32]>> {
    let (values, rest) = blob.as_chunks::<{ size_of::<f32>() }>();
    if values.len() != dimensions || !rest.is_empty() {
        return None;
    }
    let embedding = values
        .iter()
        .map(|bytes| f32::from_le_bytes(*bytes))
        .collect::<Arc<[f32]>>();
    is_valid_embedding(&embedding, dimensions).then_some(embedding)
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use crate::free_space::UnreportedFreeSpace;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// The length of every embedding in a [`temporary_store`].
    pub(crate) const FAKE_DIMENSIONS: usize = 2;

    /// The path of a store of its own, not yet created, in the running
    /// test's cache directory, which is removed when the test ends.
    pub(crate) fn temporary_store_path() -> PathBuf {
        static NEXT_STORE: AtomicUsize = AtomicUsize::new(0);
        crate::cache_file::test_support::test_cache_directory()
            .join(format!(
                "passage-store-{}",
                NEXT_STORE.fetch_add(1, Ordering::Relaxed)
            ))
            .join("embeddings.sqlite3")
    }

    /// A store of its own for embeddings of [`FAKE_DIMENSIONS`] values, on a
    /// drive that does not report its free space.
    pub(crate) fn temporary_store(clock: Arc<dyn Clock>) -> PassageStore {
        temporary_store_at(temporary_store_path(), clock)
    }

    /// The store at `path`, as [`temporary_store`] opens its own: a second
    /// one at the same path reads what the first wrote.
    pub(crate) fn temporary_store_at(path: PathBuf, clock: Arc<dyn Clock>) -> PassageStore {
        temporary_store_on(path, clock, Arc::new(UnreportedFreeSpace))
    }

    /// The store at `path` on the drive `disk`.
    pub(crate) fn temporary_store_on(
        path: PathBuf,
        clock: Arc<dyn Clock>,
        disk: Arc<dyn DiskSpace>,
    ) -> PassageStore {
        PassageStore::at(
            Some(path),
            FAKE_DIMENSIONS,
            clock,
            disk,
            discarded_failures(),
        )
    }

    /// A failure log that drops its messages.
    pub(crate) fn discarded_failures() -> FailureLog {
        Arc::new(|_| {})
    }

    /// The stored embeddings of `texts`, read without a cancellation.
    pub(crate) fn read_texts(
        open: &mut OpenPassageStore<'_>,
        texts: &[&str],
    ) -> HashMap<PassageKey, Arc<[f32]>> {
        let keys = texts
            .iter()
            .map(|text| PassageKey::of(text))
            .collect::<Vec<_>>();
        open.read(&keys, &SemanticCancellationToken::new())
            .expect("the read is not cancelled")
    }

    /// Store `embedding` for each of `texts`.
    pub(crate) fn store_passages(store: &PassageStore, texts: &[&str], embedding: [f32; 2]) {
        let passages = texts
            .iter()
            .map(|text| (PassageKey::of(text), Arc::from(embedding)))
            .collect::<Vec<_>>();
        let mut open = store.open();
        open.write(&passages);
        assert_eq!(
            open.finish().written,
            passages.len(),
            "the fixture's passages are stored"
        );
    }

    pub(crate) fn stored_embedding(store: &PassageStore, text: &str) -> Option<Arc<[f32]>> {
        read_texts(&mut store.open(), &[text]).remove(&PassageKey::of(text))
    }

    pub(crate) fn stored_passage_count(store: &PassageStore) -> usize {
        store.open().passage_count()
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;
    use crate::clock::TestClock;
    use crate::free_space::FixedFreeSpace;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const DAY: Duration = Duration::from_secs(SECONDS_PER_DAY as u64);

    fn store_at(path: &Path, dimensions: usize) -> PassageStore {
        PassageStore::at(
            Some(path.to_path_buf()),
            dimensions,
            Arc::new(TestClock::default()),
            Arc::new(FixedFreeSpace(10 * FREE_SPACE_FLOOR)),
            discarded_failures(),
        )
    }

    fn passage(text: &str, embedding: [f32; 2]) -> (PassageKey, Arc<[f32]>) {
        (PassageKey::of(text), Arc::from(embedding))
    }

    /// A connection of the test's own to `store`, as another rearview or a
    /// hand edit of the file has.
    fn raw_connection(store: &PassageStore) -> Connection {
        Connection::open(store.path().unwrap()).unwrap()
    }

    fn used_day(store: &PassageStore, text: &str) -> i64 {
        raw_connection(store)
            .query_row(
                "SELECT used_day FROM passages JOIN passage_uses USING (id) WHERE key = ?1",
                [PassageKey::of(text).0.as_slice()],
                |row| row.get(0),
            )
            .unwrap()
    }

    #[test]
    fn read_returns_written_rows_of_the_keys_asked_for() {
        let store = temporary_store(Arc::new(TestClock::default()));
        let mut open = store.open();
        open.write(&[
            passage("alpha", [1.0, 0.0]),
            passage("beta", [0.0, 1.0]),
            passage("gamma", [0.5, 0.5]),
        ]);
        let written = open.finish();
        let (alpha, beta) = (PassageKey::of("alpha"), PassageKey::of("beta"));

        let mut open = store.open();
        let read = read_texts(&mut open, &["alpha", "beta", "absent"]);
        let usage = open.finish();

        assert_eq!(
            read,
            HashMap::from([
                (alpha, Arc::from([1.0, 0.0])),
                (beta, Arc::from([0.0, 1.0]))
            ]),
            "the read returns the stored rows of the keys asked for, and no other"
        );
        assert_eq!(
            (written, usage),
            (
                StoreUse {
                    written: 3,
                    ..StoreUse::default()
                },
                StoreUse {
                    read: 2,
                    ..StoreUse::default()
                }
            ),
            "each run counts the passages it wrote and read"
        );
    }

    #[test]
    fn store_of_another_dimension_count_model_or_version_is_emptied() {
        // Each case: what differs, the recorded setting edited to another
        // value, and the dimension count the store is opened with again.
        let cases = [
            ("dimensions", None, FAKE_DIMENSIONS + 1),
            ("model", Some("model"), FAKE_DIMENSIONS),
            ("store_version", Some("store_version"), FAKE_DIMENSIONS),
        ];
        for (changed, edited_setting, reopened_dimensions) in cases {
            let path = temporary_store_path();
            let store = store_at(&path, FAKE_DIMENSIONS);
            store_passages(&store, &["stored"], [0.5, 0.5]);
            if let Some(setting) = edited_setting {
                raw_connection(&store)
                    .execute(
                        "UPDATE meta SET value = 'another' WHERE key = ?1",
                        [setting],
                    )
                    .unwrap();
            }

            let reopened = store_at(&path, reopened_dimensions);

            assert_eq!(
                stored_passage_count(&reopened),
                0,
                "a store whose {changed} differs is emptied"
            );
            store_passages(&reopened, &["stored afterwards"], [0.5, 0.5]);
            assert_eq!(
                stored_passage_count(&reopened),
                1,
                "the emptied store records the new {changed}: the next open keeps its row"
            );
        }
    }

    #[test]
    fn row_with_invalid_embedding_is_a_miss_and_is_replaced_by_the_next_write() {
        let invalid_blobs: [&[u8]; 3] = [
            &[],
            &[0; 4],
            // Two values, the first of them not a number.
            &[0, 0, 0xc0, 0x7f, 0, 0, 0, 0],
        ];
        for blob in invalid_blobs {
            let store = temporary_store(Arc::new(TestClock::default()));
            store_passages(&store, &["damaged", "valid"], [0.5, 0.5]);
            raw_connection(&store)
                .execute(
                    "UPDATE passages SET embedding = ?1 WHERE key = ?2",
                    (blob, PassageKey::of("damaged").0.as_slice()),
                )
                .unwrap();

            let before = stored_embedding(&store, "damaged");
            store_passages(&store, &["damaged"], [1.0, 0.0]);

            assert_eq!(before, None, "{blob:?} is not read as an embedding");
            assert_eq!(
                stored_embedding(&store, "damaged"),
                Some(Arc::from([1.0, 0.0])),
                "the passage's new embedding replaces {blob:?}"
            );
            assert_eq!(
                stored_embedding(&store, "valid"),
                Some(Arc::from([0.5, 0.5])),
                "the valid row stays"
            );
        }
    }

    /// The whole-file cache this store replaces kept at most 50,000 entries.
    #[test]
    fn store_keeps_more_than_50_000_passages() {
        const PASSAGES: usize = 50_001;
        let store = temporary_store(Arc::new(TestClock::default()));
        let passages = (0..PASSAGES)
            .map(|index| passage(&format!("passage {index}"), [index as f32, 1.0]))
            .collect::<Vec<_>>();

        let mut open = store.open();
        open.write(&passages);
        open.finish();

        assert_eq!(stored_passage_count(&store), PASSAGES);
        assert_eq!(
            stored_embedding(&store, "passage 0"),
            Some(Arc::from([0.0, 1.0])),
            "the first passage written is kept"
        );
    }

    #[test]
    fn two_writers_writing_in_turn_both_keep_their_passages() {
        let store = temporary_store(Arc::new(TestClock::default()));
        let (mut first, mut second) = (store.open(), store.open());

        first.write(&[passage("first one", [1.0, 0.0])]);
        second.write(&[passage("second one", [0.0, 1.0])]);
        first.write(&[
            passage("first two", [1.0, 0.0]),
            passage("shared", [0.5, 0.5]),
        ]);
        second.write(&[passage("shared", [0.5, 0.5])]);
        let usage = (first.finish(), second.finish());

        for text in ["first one", "first two", "second one", "shared"] {
            assert!(
                stored_embedding(&store, text).is_some(),
                "{text} is stored after both writers finish"
            );
        }
        assert_eq!(
            (usage.0.skipped, usage.1.skipped),
            (0, 0),
            "neither writer skips a write"
        );
    }

    #[test]
    fn write_to_busy_store_is_skipped_and_the_next_write_is_stored() {
        let store = PassageStore {
            busy_timeout: Duration::ZERO,
            ..temporary_store(Arc::new(TestClock::default()))
        };
        let mut open = store.open();
        let other_writer = raw_connection(&store);
        other_writer.execute_batch("BEGIN IMMEDIATE").unwrap();

        open.write(&[passage("while busy", [1.0, 0.0])]);
        other_writer.execute_batch("COMMIT").unwrap();
        open.write(&[passage("afterwards", [0.0, 1.0])]);
        let usage = open.finish();

        assert_eq!(
            usage,
            StoreUse {
                read: 0,
                written: 1,
                skipped: 1,
                skip_reason: Some(SkipReason::Busy),
            }
        );
        assert_eq!(
            (
                stored_embedding(&store, "while busy"),
                stored_embedding(&store, "afterwards").is_some()
            ),
            (None, true),
            "the skipped rows are dropped and the later write is stored"
        );
    }

    /// Another connection's `data_version` changes when any connection but
    /// itself commits a change.
    #[test]
    fn run_reading_fresh_stamps_doesnt_write() {
        let clock = Arc::new(TestClock::default());
        let store = temporary_store(clock.clone());
        store_passages(&store, &["stored"], [0.5, 0.5]);
        clock.advance(STAMP_REFRESH_DAYS as u32 * DAY);
        let observer = raw_connection(&store);
        let data_version = || -> i64 {
            observer
                .pragma_query_value(None, "data_version", |row| row.get(0))
                .unwrap()
        };
        let before = data_version();

        let mut open = store.open();
        let read = read_texts(&mut open, &["stored"]);
        open.finish();

        assert_eq!(read.len(), 1);
        assert_eq!(
            data_version(),
            before,
            "a read of a stamp {STAMP_REFRESH_DAYS} days old doesn't write"
        );
    }

    #[test]
    fn stale_stamp_of_a_passage_read_is_set_to_today() {
        let clock = Arc::new(TestClock::default());
        let store = temporary_store(clock.clone());
        store_passages(&store, &["read again", "not read"], [0.5, 0.5]);
        let written_on = used_day(&store, "read again");
        let days_later = STAMP_REFRESH_DAYS + 1;
        clock.advance(days_later as u32 * DAY);

        let mut open = store.open();
        read_texts(&mut open, &["read again"]);
        open.finish();

        assert_eq!(
            (used_day(&store, "read again"), used_day(&store, "not read")),
            (written_on + days_later, written_on),
            "the passage read is stamped today and the other keeps its stamp"
        );
    }

    #[test]
    fn stale_stamps_are_set_when_another_rearview_deleted_a_passage_read() {
        let clock = Arc::new(TestClock::default());
        let store = temporary_store(clock.clone());
        store_passages(&store, &["deleted meanwhile", "kept"], [0.5, 0.5]);
        let written_on = used_day(&store, "kept");
        let days_later = STAMP_REFRESH_DAYS + 1;
        clock.advance(days_later as u32 * DAY);

        let mut open = store.open();
        read_texts(&mut open, &["deleted meanwhile", "kept"]);
        let other_rearview = raw_connection(&store);
        other_rearview
            .pragma_update(None, "foreign_keys", true)
            .unwrap();
        other_rearview
            .execute(
                "DELETE FROM passages WHERE key = ?1",
                [PassageKey::of("deleted meanwhile").0.as_slice()],
            )
            .unwrap();
        open.finish();

        assert_eq!(
            used_day(&store, "kept"),
            written_on + days_later,
            "the passage still stored is stamped today"
        );
        let stamps: i64 = raw_connection(&store)
            .query_row("SELECT COUNT(*) FROM passage_uses", [], |row| row.get(0))
            .unwrap();
        assert_eq!(stamps, 1, "the deleted passage doesn't get a stamp");
    }

    #[test]
    fn cancelled_read_stops_within_the_keys_of_one_cancellation_check() {
        let store = temporary_store(Arc::new(TestClock::default()));
        let keys = (0..3 * KEYS_PER_CANCELLATION_CHECK)
            .map(|index| PassageKey::of(&format!("passage {index}")))
            .collect::<Vec<_>>();
        let cancellation = SemanticCancellationToken::new();
        let keys_taken = AtomicUsize::new(0);

        let mut open = store.open();
        let read = open.read(
            keys.iter().inspect(|_| {
                // Cancelled after the read's first check passed.
                if keys_taken.fetch_add(1, Ordering::Relaxed) == 1 {
                    cancellation.cancel();
                }
            }),
            &cancellation,
        );

        assert!(read.is_none(), "a cancelled read doesn't return any rows");
        assert_eq!(
            keys_taken.load(Ordering::Relaxed),
            KEYS_PER_CANCELLATION_CHECK + 1,
            "the read stops at its first check after the cancellation"
        );
    }

    /// Those of `texts` with a row in the store, found without a read
    /// through the store: that read stamps the row today.
    fn texts_with_a_row<'t>(store: &PassageStore, texts: &[&'t str]) -> Vec<&'t str> {
        let connection = raw_connection(store);
        let mut has_row = connection
            .prepare("SELECT EXISTS (SELECT 1 FROM passages WHERE key = ?1)")
            .unwrap();
        texts
            .iter()
            .copied()
            .filter(|text| {
                has_row
                    .query_row([PassageKey::of(text).0.as_slice()], |row| row.get(0))
                    .unwrap()
            })
            .collect()
    }

    #[test]
    fn run_that_writes_deletes_the_passages_stamped_over_187_days_ago() {
        let clock = Arc::new(TestClock::default());
        let store = temporary_store(clock.clone());
        store_passages(&store, &["unused", "read again"], [0.5, 0.5]);
        clock.advance(DAY);
        store_passages(&store, &["unused for a day less"], [0.5, 0.5]);
        clock.advance(STAMP_DAYS_KEPT as u32 * DAY);

        let mut open = store.open();
        read_texts(&mut open, &["read again"]);
        open.write(&[passage("embedded", [1.0, 0.0])]);
        open.finish();

        assert_eq!(
            texts_with_a_row(
                &store,
                &["unused", "read again", "unused for a day less", "embedded"]
            ),
            ["read again", "unused for a day less", "embedded"],
            "the passage stamped {} days ago alone is deleted",
            STAMP_DAYS_KEPT + 1
        );
        let stamps: i64 = raw_connection(&store)
            .query_row("SELECT COUNT(*) FROM passage_uses", [], |row| row.get(0))
            .unwrap();
        assert_eq!(stamps, 3, "the deleted passage's stamp is deleted with it");
    }

    /// The read on the last day that keeps the stamp is the passage's last
    /// use; the stamp is then [`STAMP_REFRESH_DAYS`] older than that use.
    #[test]
    fn passage_read_180_days_ago_with_an_older_stamp_is_kept() {
        let clock = Arc::new(TestClock::default());
        let store = temporary_store(clock.clone());
        store_passages(&store, &["read later"], [0.5, 0.5]);
        let stamped_on = used_day(&store, "read later");
        clock.advance(STAMP_REFRESH_DAYS as u32 * DAY);
        let mut open = store.open();
        read_texts(&mut open, &["read later"]);
        open.finish();
        clock.advance(UNUSED_DAYS_KEPT as u32 * DAY);

        store_passages(&store, &["embedded"], [1.0, 0.0]);

        assert_eq!(
            used_day(&store, "read later"),
            stamped_on,
            "the read {STAMP_REFRESH_DAYS} days after the stamp keeps it"
        );
        assert_eq!(
            texts_with_a_row(&store, &["read later"]),
            ["read later"],
            "the passage read {UNUSED_DAYS_KEPT} days ago is kept"
        );
    }

    #[test]
    fn run_that_doesnt_write_doesnt_delete() {
        let clock = Arc::new(TestClock::default());
        let store = temporary_store(clock.clone());
        store_passages(&store, &["unused"], [0.5, 0.5]);
        clock.advance((STAMP_DAYS_KEPT + 1) as u32 * DAY);

        let mut open = store.open();
        read_texts(&mut open, &["absent"]);
        open.finish();

        assert_eq!(
            texts_with_a_row(&store, &["unused"]),
            ["unused"],
            "a run without a write keeps the passage stamped over {STAMP_DAYS_KEPT} days ago"
        );
    }

    /// A drive low on space at one of its free-space checks, counted from
    /// zero, and with room at every other.
    struct DriveLowAtCheck {
        low_check: usize,
        checks: AtomicUsize,
    }

    impl DiskSpace for DriveLowAtCheck {
        fn available_bytes(&self, _path: &Path) -> Option<u64> {
            let check = self.checks.fetch_add(1, Ordering::SeqCst);
            Some(if check == self.low_check {
                FREE_SPACE_FLOOR - 1
            } else {
                10 * FREE_SPACE_FLOOR
            })
        }
    }

    /// The run's checks of the drive: the write's, the stamp update's, then
    /// the deletion's. Without the stamp update, the deletion finds the
    /// passage's old stamp.
    #[test]
    fn run_whose_stamp_update_fails_doesnt_delete_the_passages_it_read() {
        let clock = Arc::new(TestClock::default());
        let path = temporary_store_path();
        store_passages(
            &temporary_store_at(path.clone(), clock.clone()),
            &["read with an old stamp"],
            [0.5, 0.5],
        );
        clock.advance((STAMP_DAYS_KEPT + 1) as u32 * DAY);
        let store = PassageStore {
            disk: Arc::new(DriveLowAtCheck {
                low_check: 1,
                checks: AtomicUsize::new(0),
            }),
            ..temporary_store_at(path, clock)
        };

        let mut open = store.open();
        read_texts(&mut open, &["read with an old stamp"]);
        open.write(&[passage("embedded", [1.0, 0.0])]);
        let usage = open.finish();

        assert_eq!(usage.written, 1, "the run's write is stored");
        assert_eq!(
            texts_with_a_row(&store, &["read with an old stamp"]),
            ["read with an old stamp"],
            "the passage the run read is kept"
        );
    }

    #[test]
    fn unused_passages_are_deleted_at_most_once_in_7_days() {
        let clock = Arc::new(TestClock::default());
        let store = temporary_store(clock.clone());
        store_passages(&store, &["unused"], [0.5, 0.5]);
        // The last deletion before the stamp of "unused" passes its days
        // kept, which it does within the interval that follows.
        clock.advance((STAMP_DAYS_KEPT - DELETION_INTERVAL_DAYS + 2) as u32 * DAY);
        store_passages(&store, &["first"], [0.5, 0.5]);

        clock.advance((DELETION_INTERVAL_DAYS - 1) as u32 * DAY);
        store_passages(&store, &["second"], [0.5, 0.5]);
        let a_day_before_the_interval_ends = texts_with_a_row(&store, &["unused"]);
        clock.advance(DAY);
        store_passages(&store, &["third"], [0.5, 0.5]);

        assert_eq!(
            a_day_before_the_interval_ends,
            ["unused"],
            "a run {} days after the last deletion doesn't delete",
            DELETION_INTERVAL_DAYS - 1
        );
        assert_eq!(
            texts_with_a_row(&store, &["unused"]),
            Vec::<&str>::new(),
            "a run {DELETION_INTERVAL_DAYS} days after the last deletion deletes"
        );
    }

    #[test]
    fn damaged_store_is_replaced_and_stores_the_runs_passages() {
        let store = temporary_store(Arc::new(TestClock::default()));
        store_passages(&store, &["before the damage"], [0.5, 0.5]);
        std::fs::write(store.path().unwrap(), b"not a SQLite database, but as long").unwrap();

        let mut open = store.open();
        let read = read_texts(&mut open, &["before the damage"]);
        open.write(&[passage("after the damage", [1.0, 0.0])]);
        let usage = open.finish();

        assert!(read.is_empty(), "the replaced store is empty");
        assert_eq!(
            (usage.written, usage.skipped),
            (1, 0),
            "the run writes to the store created again"
        );
        assert_eq!(
            stored_embedding(&store, "after the damage"),
            Some(Arc::from([1.0, 0.0]))
        );
    }

    #[test]
    fn open_removes_leftover_temp_files_from_the_stores_folder() {
        use crate::cache_file::test_support::TempFileFixture;
        let clock = Arc::new(TestClock::default());
        let store = temporary_store(clock.clone());
        let fixture =
            TempFileFixture::in_directory(store.path().unwrap().parent().unwrap(), &clock);

        drop(store.open());

        fixture.assert_swept();
    }

    #[test]
    fn store_that_doesnt_open_misses_every_read_and_skips_every_write() {
        let path = temporary_store_path();
        // A folder where the store's file belongs: SQLite cannot open it.
        std::fs::create_dir_all(&path).unwrap();
        let store = store_at(&path, FAKE_DIMENSIONS);

        let mut open = store.open();
        let read = read_texts(&mut open, &["passage"]);
        open.write(&[passage("passage", [1.0, 0.0])]);
        let usage = open.finish();

        assert!(read.is_empty());
        assert_eq!(
            usage,
            StoreUse {
                read: 0,
                written: 0,
                skipped: 1,
                skip_reason: Some(SkipReason::Error),
            }
        );
    }

    #[test]
    fn run_on_store_that_doesnt_open_logs_the_sqlite_error_once() {
        let path = temporary_store_path();
        // A folder where the store's file belongs: SQLite cannot open it.
        std::fs::create_dir_all(&path).unwrap();
        let logged = Arc::new(Mutex::new(Vec::new()));
        let log = logged.clone();
        let store = PassageStore {
            failure_log: Arc::new(move |message| log.lock().unwrap().push(message.to_owned())),
            ..store_at(&path, FAKE_DIMENSIONS)
        };

        let mut open = store.open();
        open.write(&[passage("first", [1.0, 0.0])]);
        open.write(&[passage("second", [1.0, 0.0])]);
        open.finish();

        let logged = logged.lock().unwrap();
        assert_eq!(logged.len(), 1, "the run logs one failure: {logged:?}");
        assert!(
            logged[0].starts_with(&format!("Passage store {} failed: ", path.display())),
            "the message names the store before SQLite's error: {logged:?}"
        );
    }

    #[test]
    fn store_on_a_drive_low_on_space_or_that_doesnt_open_is_not_writable() {
        let low_on_space = PassageStore {
            disk: Arc::new(FixedFreeSpace(FREE_SPACE_FLOOR - 1)),
            ..store_at(&temporary_store_path(), FAKE_DIMENSIONS)
        };
        let unopenable_path = temporary_store_path();
        // A folder where the store's file belongs: SQLite cannot open it.
        std::fs::create_dir_all(&unopenable_path).unwrap();
        let unopenable = store_at(&unopenable_path, FAKE_DIMENSIONS);
        let writable = store_at(&temporary_store_path(), FAKE_DIMENSIONS);

        assert!(
            matches!(
                low_on_space.fail_unless_writable(),
                Err(AppError::SessionCacheLowSpace {
                    free_bytes,
                    floor_bytes: FREE_SPACE_FLOOR,
                    outcome: CacheDeletion::Pending,
                }) if free_bytes == FREE_SPACE_FLOOR - 1
            ),
            "a drive under the floor fails with the low-space error and its free bytes"
        );
        assert!(
            matches!(
                unopenable.fail_unless_writable(),
                Err(AppError::PassageStoreNotOpened(named))
                    if named == unopenable_path.display().to_string()
            ),
            "a store that doesn't open fails with an error naming its path"
        );
        assert!(
            writable.fail_unless_writable().is_ok(),
            "a store that opens on a drive with room passes"
        );
    }

    #[test]
    fn low_space_skips_the_write_and_keeps_the_store() {
        let path = temporary_store_path();
        store_passages(&store_at(&path, FAKE_DIMENSIONS), &["stored"], [0.5, 0.5]);
        let low_on_space = PassageStore {
            disk: Arc::new(FixedFreeSpace(FREE_SPACE_FLOOR - 1)),
            ..store_at(&path, FAKE_DIMENSIONS)
        };

        let mut open = low_on_space.open();
        let read = read_texts(&mut open, &["stored"]);
        open.write(&[passage("embedded", [1.0, 0.0])]);
        let usage = open.finish();

        assert_eq!(read.len(), 1, "the stored passage is read");
        assert_eq!(
            usage,
            StoreUse {
                read: 1,
                written: 0,
                skipped: 1,
                skip_reason: Some(SkipReason::LowSpace),
            }
        );
        assert_eq!(
            stored_passage_count(&store_at(&path, FAKE_DIMENSIONS)),
            1,
            "the store keeps its row and doesn't gain the skipped one"
        );
    }

    #[test]
    fn write_to_missing_store_on_a_drive_low_on_space_is_skipped_for_low_space() {
        let low_on_space = PassageStore {
            disk: Arc::new(FixedFreeSpace(FREE_SPACE_FLOOR - 1)),
            ..store_at(&temporary_store_path(), FAKE_DIMENSIONS)
        };

        let mut open = low_on_space.open();
        open.write(&[passage("embedded", [1.0, 0.0])]);
        let usage = open.finish();

        assert_eq!(
            (usage.skipped, usage.skip_reason),
            (1, Some(SkipReason::LowSpace))
        );
    }
}
