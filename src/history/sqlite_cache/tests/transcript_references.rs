use super::super::refresh::{self, WriteBatch};
use super::super::{CacheWrite, SqliteCache, apply_writes};
use super::{AgentSession, every_record_sessions};
use crate::error::AppError;
use rusqlite::{Connection, OpenFlags, ffi};
use std::ffi::{CStr, c_int};
use std::path::Path;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};
use std::sync::mpsc;

/// The default VFS, but counting the statement journals SQLite writes to a
/// file in [`STATEMENT_JOURNAL_FILES`].
const COUNTING_VFS: &CStr = c"rearview-statement-journal-count";

static STATEMENT_JOURNAL_FILES: AtomicUsize = AtomicUsize::new(0);
static DEFAULT_VFS: AtomicPtr<ffi::sqlite3_vfs> = AtomicPtr::new(std::ptr::null_mut());

unsafe extern "C" fn open_counting_statement_journals(
    _counting: *mut ffi::sqlite3_vfs,
    name: ffi::sqlite3_filename,
    file: *mut ffi::sqlite3_file,
    flags: c_int,
    out_flags: *mut c_int,
) -> c_int {
    if flags & ffi::SQLITE_OPEN_SUBJOURNAL != 0 {
        STATEMENT_JOURNAL_FILES.fetch_add(1, Ordering::SeqCst);
    }
    let default = DEFAULT_VFS.load(Ordering::SeqCst);
    // SAFETY: `default` is SQLite's default VFS, which lives as long as the
    // process; the other arguments are SQLite's for this open, and the
    // counting VFS sizes `file` as the default VFS does.
    unsafe {
        let open = (*default).xOpen.expect("the default VFS opens files");
        open(default, name, file, flags, out_flags)
    }
}

/// Registers [`COUNTING_VFS`], once per process.
fn register_counting_vfs() {
    static REGISTERED: OnceLock<()> = OnceLock::new();
    REGISTERED.get_or_init(|| {
        // SAFETY: SQLite returns its default VFS, which lives as long as the
        // process. The counting VFS copies it, `pAppData` included, so each
        // method the copy keeps finds its own data; it is leaked, as a
        // registered VFS must outlive every connection using it.
        unsafe {
            let default = ffi::sqlite3_vfs_find(std::ptr::null());
            assert!(!default.is_null(), "SQLite has a default VFS");
            DEFAULT_VFS.store(default, Ordering::SeqCst);
            let counting = Box::leak(Box::new(ffi::sqlite3_vfs {
                pNext: std::ptr::null_mut(),
                zName: COUNTING_VFS.as_ptr(),
                xOpen: Some(open_counting_statement_journals),
                ..*default
            }));
            assert_eq!(ffi::sqlite3_vfs_register(counting, 0), ffi::SQLITE_OK);
        }
    });
}

/// A session of 2,000 entries read from one sub-agent transcript: removing
/// or moving its entries changes far more than 64 KiB of pages.
const SESSION_FROM_A_SUB_AGENT: &str = "
    INSERT INTO roots (id, key) VALUES (1, 'root');
    INSERT INTO sessions (id, storage, source, root, cache_key, status, file_size,
        mtime_secs, mtime_nsecs, generation)
        VALUES (1, 'claude', 'claude', 1, 'session', 'listed', 0, 0, 0, 1);
    INSERT INTO transcripts (id, session, root, path) VALUES (1, 1, 1, 'sub-agent.jsonl');
    INSERT INTO threads (session, number, transcript) VALUES (1, 1, 1);
    WITH RECURSIVE numbered (sequence) AS
        (SELECT 0 UNION ALL SELECT sequence + 1 FROM numbered WHERE sequence < 1999)
    INSERT INTO entries (session, contributes, position, sequence, record_transcript,
        record_line, kind, text, child, columns_digest, digest)
        SELECT 1, 0, sequence, sequence, 1, sequence, 'user', printf('%.200c', 'x'), 1, 0, 0
        FROM numbered;
";

/// The statement journals `statement` writes to a file, run in a
/// transaction rolled back after it.
fn statement_journal_files_of(connection: &Connection, statement: &str) -> usize {
    let before = STATEMENT_JOURNAL_FILES.load(Ordering::SeqCst);
    connection.execute_batch("BEGIN").unwrap();
    connection.execute(statement, []).unwrap();
    connection.execute_batch("ROLLBACK").unwrap();
    STATEMENT_JOURNAL_FILES.load(Ordering::SeqCst) - before
}

/// Counts the files SQLite moves a statement journal past 64 KiB to, with
/// temporary storage in files: SQLite doesn't report statement journals. In
/// WAL mode, as the cache runs, a statement journal holds every page its
/// statement changes. The update of every entry keeps a journal: the test
/// checks the count against it.
#[test]
fn session_removal_doesnt_keep_a_statement_journal() {
    register_counting_vfs();
    let directory = tempfile::tempdir().unwrap();
    let connection = Connection::open_with_flags_and_vfs(
        directory.path().join("cache.sqlite3"),
        OpenFlags::default(),
        COUNTING_VFS,
    )
    .unwrap();
    connection
        .pragma_update(None, "journal_mode", "WAL")
        .unwrap();
    connection
        .pragma_update(None, "foreign_keys", "ON")
        .unwrap();
    connection
        .pragma_update(None, "temp_store", "FILE")
        .unwrap();
    connection
        .execute_batch(super::super::schema::CREATE)
        .unwrap();
    connection.execute_batch(SESSION_FROM_A_SUB_AGENT).unwrap();

    let removal = statement_journal_files_of(&connection, "DELETE FROM sessions WHERE id = 1");
    let update = statement_journal_files_of(
        &connection,
        "UPDATE entries SET sequence = sequence + 2000 WHERE session = 1",
    );

    assert_eq!(
        (removal, update),
        (0, 1),
        "the session's removal doesn't keep a statement journal; the update of its entries keeps one"
    );
}

/// A transcript deleted while entries still name it passes its statement,
/// as the references are checked at the commit.
#[test]
fn deleted_transcript_still_referenced_fails_the_commit_with_its_cause() {
    let directory = tempfile::tempdir().unwrap();
    let cache = super::admitted_cache(directory.path());
    WriteBatch::run(&cache.connection, cache.token(), |batch| {
        Ok(batch.execute_batch(SESSION_FROM_A_SUB_AGENT)?)
    })
    .unwrap();

    let committed = WriteBatch::run(&cache.connection, cache.token(), |batch| {
        batch.execute("DELETE FROM transcripts WHERE id = 1", [])?;
        Ok(())
    });

    assert!(
        matches!(
            &committed,
            Err(AppError::SessionCache(error))
                if error.to_string().contains("FOREIGN KEY constraint failed")
        ),
        "the commit fails with SQLite's foreign-key error: {committed:?}"
    );
    if cfg!(debug_assertions) {
        assert!(
            committed.as_ref().is_err_and(|error| error
                .to_string()
                .contains("a row of entries references a missing row of transcripts")),
            "a debug build names the tables: {committed:?}"
        );
    }
    assert!(
        cache.can_publish(),
        "the failed commit keeps the owner's write permission"
    );
}

/// A session as the cache stores it.
#[derive(Debug, PartialEq)]
struct StoredSession {
    generation: i64,
    mtime_secs: i64,
    /// Each entry's sequence, kind and text.
    entries: Vec<(i64, String, Option<String>)>,
}

/// The session `session` writes, as the cache in `base` stores it; `None`
/// when it isn't in the cache.
fn stored_session(base: &Path, session: &AgentSession) -> Option<StoredSession> {
    use rusqlite::OptionalExtension;
    let connection = Connection::open(base.join(super::super::DATABASE_FILE_NAME)).unwrap();
    let (id, generation, mtime_secs) = connection
        .query_row(
            "SELECT id, generation, mtime_secs FROM sessions WHERE storage = ?1",
            [super::storage_of(session.row.source).directory],
            |row| Ok((row.get::<_, i64>(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()
        .unwrap()?;
    let entries = connection
        .prepare("SELECT sequence, kind, text FROM entries WHERE session = ?1 ORDER BY sequence")
        .unwrap()
        .query_map([id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    Some(StoredSession {
        generation,
        mtime_secs,
        entries,
    })
}

/// A session of one entry referencing its transcript, at ids the writes in
/// the test don't give.
const SESSION_OF_ONE_ENTRY: &str = "
    INSERT INTO roots (id, key) VALUES (900, 'other root');
    INSERT INTO sessions (id, storage, source, root, cache_key, status, file_size,
        mtime_secs, mtime_nsecs, generation)
        VALUES (900, 'other', 'claude', 900, 'other', 'listed', 0, 0, 0, 900);
    INSERT INTO transcripts (id, session, root, path) VALUES (900, 900, 900, 'other.jsonl');
    INSERT INTO entries (session, contributes, position, sequence, record_transcript,
        record_line, kind, text, child, columns_digest, digest)
        VALUES (900, 0, 0, 0, 900, 0, 'user', 'text', NULL, 0, 0);
";

/// A trigger deletes the transcript of [`SESSION_OF_ONE_ENTRY`] as the first
/// session's second write updates its row, leaving the entry referencing it.
#[test]
fn write_failing_its_references_at_commit_rolls_back_alone_and_later_writes_land() {
    let directory = tempfile::tempdir().unwrap();
    let base = super::rearview_at(directory.path());
    let ownership = refresh::RefreshOwnership::acquire_for_test(&base).unwrap();
    let writer = SqliteCache::open_for_refresh(ownership.token()).unwrap();
    writer.mark_admitted().unwrap();
    let sessions = every_record_sessions();
    let (failing, later) = (&sessions[0], &sessions[1]);
    let record_of = |session: &AgentSession, modified_secs| {
        super::parsed_record(
            session.row.source,
            &super::root_of(session),
            &super::stub_of(session, modified_secs),
        )
    };
    for session in [failing, later] {
        writer
            .admit_storage(super::storage_of(session.row.source))
            .unwrap();
    }
    writer.write(&record_of(failing, 1)).unwrap();
    let before = stored_session(directory.path(), failing);
    assert!(before.is_some(), "the first write lands");
    writer
        .connection
        .execute_batch(SESSION_OF_ONE_ENTRY)
        .unwrap();
    writer
        .connection
        .execute(
            &format!(
                "CREATE TRIGGER delete_referenced_transcript AFTER UPDATE ON sessions \
                 WHEN NEW.storage = '{}' AND NEW.mtime_secs = 2 \
                 BEGIN DELETE FROM transcripts WHERE id = 900; END",
                super::storage_of(failing.row.source).directory
            ),
            [],
        )
        .unwrap();
    let (send, receive) = mpsc::channel();
    let (write, failure) = CacheWrite::session(record_of(failing, 2));
    send.send(write).unwrap();
    send.send(CacheWrite::session(record_of(later, 1)).0)
        .unwrap();
    drop(send);

    apply_writes(writer, receive, None);

    let failure = failure.recv().unwrap();
    assert!(
        failure
            .as_ref()
            .is_err_and(|error| error.contains("FOREIGN KEY constraint failed")),
        "the write fails with its cause: {failure:?}"
    );
    assert_eq!(
        stored_session(directory.path(), failing),
        before,
        "the failed session keeps its previous rows and generation"
    );
    assert_eq!(
        stored_session(directory.path(), later).map(|stored| stored.mtime_secs),
        Some(1),
        "the later write lands"
    );
    assert!(
        ownership.token().can_publish(),
        "the owner keeps its write permission"
    );
}
