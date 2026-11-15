//! The entry hashes the viewer's read returns: equal for the same content
//! however it was written, and different for an entry whose stored values
//! the read returns changed.

use super::*;
use crate::history::sqlite_cache::EntryHashes;
use crate::history::sqlite_cache::entries::THREAD_TEXT_COLUMNS;

/// The hashes of `fixture`'s copy in `cache`.
fn hashes_of(cache: &CacheWriter, fixture: &Fixture) -> EntryHashes {
    fixture
        .listed(cache)
        .cached_session
        .expect("the listed row has its copy")
        .read_copy(|held| held.entry_hashes())
        .unwrap()
}

#[test]
fn same_content_written_again_keeps_entry_hashes_for_every_agent() {
    for session in every_record_sessions() {
        let fixture = Fixture::of(session);
        let record = fixture.record();
        let fresh = FreshWrite::of(&fixture, &record);
        let mut earlier = fixture.record();
        assert!(
            without_last_own_entries(&mut earlier, 2) > 0,
            "{}: the session's own transcript has entries to leave out",
            fixture.label()
        );
        let base = tempfile::tempdir().unwrap();
        let cache = cache_with(base.path(), &fixture, &earlier);

        cache.write(&record).unwrap();
        let appended = hashes_of(&cache, &fixture);
        cache.write(&record).unwrap();
        let written_again = hashes_of(&cache, &fixture);

        let expected = hashes_of(&fresh.cache, &fixture);
        let entries = cache
            .entries(fixture.storage(), &fixture.root, &fixture.stub.cache_key)
            .unwrap()
            .unwrap();
        assert_eq!(
            expected.len(),
            entries.entries.len(),
            "{}: one hash per entry read",
            fixture.label()
        );
        assert_eq!(
            appended,
            expected,
            "{}: entries appended to an earlier copy hash as a fresh write's",
            fixture.label()
        );
        assert_eq!(
            written_again,
            expected,
            "{}: the same content written again hashes as a fresh write's",
            fixture.label()
        );
    }
}

/// A change to one stored value of a session, made in the database: the
/// statement making it, and the query of the entries whose read it changes.
struct StoredChange {
    name: String,
    /// A query of the row to change; `?1` is the session. A session without
    /// such a row returns none.
    target: &'static str,
    /// `?1` is the session, `?2` the target.
    change: String,
    /// The sequences of the entries whose read the change changes; `?1` is
    /// the session, `?2` the target.
    reached: &'static str,
}

/// The entry in the middle of the session.
const MIDDLE_ENTRY: &str = "SELECT id FROM entries WHERE session = ?1 \
     ORDER BY sequence LIMIT 1 OFFSET (SELECT COUNT(*) / 2 FROM entries WHERE session = ?1)";

/// The first message whose thread differs from the first message's thread.
const MESSAGE_OF_SECOND_THREAD: &str = "SELECT m.id FROM messages m WHERE m.session = ?1 \
     AND (SELECT COUNT(DISTINCT thread) FROM messages WHERE session = ?1) > 1 \
     AND m.thread <> (SELECT thread FROM messages WHERE session = ?1 ORDER BY id LIMIT 1) \
     ORDER BY m.id LIMIT 1";

const THREAD_WITH_MESSAGES: &str = "SELECT thread FROM messages WHERE session = ?1 \
     ORDER BY thread DESC LIMIT 1";

/// A thread with messages, in a session with a transcript row.
const THREAD_WITH_MESSAGES_AND_TRANSCRIPT: &str = "SELECT thread FROM messages WHERE session = ?1 \
     AND EXISTS (SELECT 1 FROM transcripts WHERE session = ?1) ORDER BY thread DESC LIMIT 1";

const ENTRIES_OF_THREAD: &str = "SELECT e.sequence FROM entries e \
     JOIN messages m ON m.id = e.message WHERE e.session = ?1 AND m.thread = ?2";

fn stored_changes() -> Vec<StoredChange> {
    let change = |name: &str, target, change: &str, reached| StoredChange {
        name: name.to_owned(),
        target,
        change: change.to_owned(),
        reached,
    };
    let mut changes = vec![
        change(
            "entries.digest",
            MIDDLE_ENTRY,
            "UPDATE entries SET digest = digest + 1 WHERE id = ?2 AND session = ?1",
            "SELECT sequence FROM entries WHERE id = ?2 AND session = ?1",
        ),
        change(
            "messages.digest",
            "SELECT message FROM entries WHERE session = ?1 AND message IS NOT NULL \
             ORDER BY sequence DESC LIMIT 1",
            "UPDATE messages SET digest = digest + 1 WHERE id = ?2 AND session = ?1",
            "SELECT sequence FROM entries WHERE message = ?2 AND session = ?1",
        ),
        change(
            "messages.thread",
            MESSAGE_OF_SECOND_THREAD,
            "UPDATE messages SET thread = \
             (SELECT thread FROM messages WHERE session = ?1 ORDER BY id LIMIT 1) \
             WHERE id = ?2",
            "SELECT sequence FROM entries WHERE message = ?2 AND session = ?1",
        ),
        change(
            "threads.transcript",
            THREAD_WITH_MESSAGES_AND_TRANSCRIPT,
            "UPDATE threads SET transcript = CASE WHEN transcript IS NULL \
             THEN (SELECT MIN(id) FROM transcripts WHERE session = ?1) ELSE NULL END \
             WHERE id = ?2 AND session = ?1",
            ENTRIES_OF_THREAD,
        ),
        change(
            "entries.record_transcript",
            "SELECT id FROM entries WHERE session = ?1 \
             AND EXISTS (SELECT 1 FROM transcripts WHERE session = ?1) \
             ORDER BY sequence LIMIT 1",
            "UPDATE entries SET record_transcript = CASE WHEN record_transcript IS NULL \
             THEN (SELECT MIN(id) FROM transcripts WHERE session = ?1) ELSE NULL END \
             WHERE id = ?2 AND session = ?1",
            "SELECT sequence FROM entries WHERE id = ?2 AND session = ?1",
        ),
        change(
            "transcripts.path",
            "SELECT t.id FROM transcripts t WHERE t.session = ?1 \
             AND (EXISTS (SELECT 1 FROM entries WHERE record_transcript = t.id) \
             OR EXISTS (SELECT 1 FROM threads WHERE transcript = t.id)) \
             ORDER BY t.id LIMIT 1",
            "UPDATE transcripts SET path = path || '-moved' WHERE id = ?2 AND session = ?1",
            "SELECT e.sequence FROM entries e LEFT JOIN messages m ON m.id = e.message \
             LEFT JOIN threads t ON t.id = m.thread \
             WHERE e.session = ?1 AND (e.record_transcript = ?2 OR t.transcript = ?2)",
        ),
    ];
    changes.extend(THREAD_TEXT_COLUMNS.map(|column| {
        change(
            &format!("threads.{column}"),
            THREAD_WITH_MESSAGES,
            &format!(
                "UPDATE threads SET {column} = COALESCE({column}, '') || '-changed' \
                 WHERE id = ?2 AND session = ?1"
            ),
            ENTRIES_OF_THREAD,
        )
    }));
    changes
}

#[test]
fn change_to_each_hashed_column_changes_hash_of_first_entry_it_reaches() {
    let fixtures: Vec<Fixture> = every_record_sessions()
        .into_iter()
        .map(Fixture::of)
        .collect();
    for change in stored_changes() {
        let changed_in = fixtures.iter().find_map(|fixture| {
            let fresh = FreshWrite::of(fixture, &fixture.record());
            let session = fixture.session_id(&fresh.cache);
            let target: i64 = fresh
                .cache
                .connection
                .query_row(change.target, [session], |row| row.get(0))
                .optional()
                .unwrap()?;
            let shown = hashes_of(&fresh.cache, fixture);
            let changed_rows = fresh
                .cache
                .connection
                .execute(&change.change, params![session, target])
                .unwrap();
            assert_eq!(
                changed_rows, 1,
                "{}: the change updates one row",
                change.name
            );
            let mut reached: Vec<usize> = fresh
                .cache
                .connection
                .prepare(change.reached)
                .unwrap()
                .query_map(params![session, target], |row| row.get::<_, i64>(0))
                .unwrap()
                .map(|sequence| sequence.unwrap() as usize)
                .collect();
            reached.sort_unstable();
            Some((
                fixture.label(),
                shown,
                hashes_of(&fresh.cache, fixture),
                reached,
            ))
        });
        let Some((label, shown, changed, reached)) = changed_in else {
            panic!("an every-record fixture has a row {} changes", change.name)
        };

        assert!(
            !reached.is_empty(),
            "{label}: {} reaches an entry",
            change.name
        );
        assert_eq!(
            shown.first_change_in(&changed),
            reached.first().copied(),
            "{label}: the first entry {} reaches is the first changed entry: {reached:?}",
            change.name
        );
    }
}
