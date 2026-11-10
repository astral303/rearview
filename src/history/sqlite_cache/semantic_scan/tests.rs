use super::*;
use crate::history::sqlite_cache::tests::{admitted_cache, every_record_copies, replace_copy};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};

/// `copy`'s semantic turns, read alone from the copy the cache holds.
fn own_turns(copy: &CachedSession) -> Vec<(usize, String)> {
    let mut turns = Vec::new();
    copy.clone()
        .ignoring_source_changes()
        .read_held_copy(|held| {
            held.stored_semantic_turns(|part| {
                turns.push((
                    part.ordinal.expect("a semantic turn has a message"),
                    part.text,
                ));
            })
        })
        .unwrap()
        .expect("the cache holds the copy");
    turns
}

/// The turns `scanned` read; panics for a copy it did not read.
fn read_turns(scanned: &ScannedTurns) -> &[(usize, String)] {
    match scanned {
        ScannedTurns::Read(turns) => turns,
        ScannedTurns::Replaced => panic!("the copy was replaced"),
        ScannedTurns::NoLongerCached => panic!("the copy is no longer in the session cache"),
        ScannedTurns::Failed(error) => panic!("the copy's read failed: {error}"),
    }
}

#[test]
fn scan_in_tasks_holds_each_sessions_turns_as_its_own_read_for_every_provider() {
    let base = tempfile::tempdir().unwrap();
    let copies = every_record_copies(base.path());
    let references = copies.iter().collect::<Vec<_>>();
    let own = copies.iter().map(own_turns).collect::<Vec<_>>();
    assert!(
        own.iter().any(|turns| turns.len() > 1),
        "a fixture holds several turns"
    );
    for entries_per_task in [ENTRIES_PER_TASK, 1] {
        let tasks = AtomicUsize::new(0);
        let scanned = scan_in_tasks_of(&references, entries_per_task, &|| {
            tasks.fetch_add(1, Ordering::Relaxed);
        })
        .unwrap();
        for ((copy, scanned), own) in copies.iter().zip(&scanned).zip(&own) {
            assert_eq!(
                read_turns(scanned),
                own.as_slice(),
                "{:?}, {entries_per_task} entries per task: the scan holds the session's turns \
                 in its order",
                copy.address().source
            );
        }
        if entries_per_task == 1 {
            assert!(
                tasks.load(Ordering::Relaxed) > copies.len(),
                "a task of one entry splits a session across tasks"
            );
        }
    }
}

#[test]
fn session_replaced_during_scan_is_reported_and_others_keep_their_turns() {
    let base = tempfile::tempdir().unwrap();
    let copies = every_record_copies(base.path());
    let references = copies.iter().collect::<Vec<_>>();
    let own = copies.iter().map(own_turns).collect::<Vec<_>>();
    let replaced = OnceLock::new();

    let scanned = scan_in_tasks_of(&references, ENTRIES_PER_TASK, &|| {
        replaced.get_or_init(|| replace_copy(&copies[1]));
    })
    .unwrap();

    assert!(
        matches!(scanned[1], ScannedTurns::Replaced),
        "the session replaced during the scan is reported"
    );
    for (position, (scanned, own)) in scanned.iter().zip(&own).enumerate() {
        if position != 1 {
            assert_eq!(
                read_turns(scanned),
                own.as_slice(),
                "copy {position} keeps its turns"
            );
        }
    }
}

#[test]
fn failed_read_of_one_sessions_entries_fails_that_session_alone() {
    let base = tempfile::tempdir().unwrap();
    let copies = every_record_copies(base.path());
    let references = copies.iter().collect::<Vec<_>>();
    let own = copies.iter().map(own_turns).collect::<Vec<_>>();
    let failing = own
        .iter()
        .position(|turns| !turns.is_empty())
        .expect("a fixture holds a turn");
    admitted_cache(base.path())
        .connection
        .execute(
            "UPDATE parts SET text = CAST(x'ff' AS TEXT) WHERE kind = 'text' \
             AND session = (SELECT id FROM sessions WHERE generation = ?1)",
            [copies[failing].generation()],
        )
        .unwrap();

    let scanned = scan_in_tasks_of(&references, ENTRIES_PER_TASK, &|| {}).unwrap();

    assert!(
        matches!(scanned[failing], ScannedTurns::Failed(_)),
        "the session whose text isn't UTF-8 fails"
    );
    for (position, (scanned, own)) in scanned.iter().zip(&own).enumerate() {
        if position != failing {
            assert_eq!(
                read_turns(scanned),
                own.as_slice(),
                "copy {position} keeps its turns"
            );
        }
    }
}

/// Each statement reads one session's entries through the index on
/// `(session, sequence)`, without scanning `entries`.
#[test]
fn semantic_statements_read_entries_through_the_session_sequence_index() {
    let connection = Connection::open_in_memory().unwrap();
    connection
        .execute_batch(super::super::schema::CREATE)
        .unwrap();
    for query in [entries::SEMANTIC_ENTRIES, SESSION_SEQUENCE_BOUNDS] {
        let mut statement = connection
            .prepare(&format!("EXPLAIN QUERY PLAN {query}"))
            .unwrap();
        let parameters = vec![1; statement.parameter_count()];
        let plan = statement
            .query_map(rusqlite::params_from_iter(parameters), |row| {
                row.get::<_, String>(3)
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
            .join("\n");
        assert!(
            plan.contains("INDEX sqlite_autoindex_entries_1 (session=?")
                && !plan.contains("SCAN e")
                && !plan.contains("SCAN entries"),
            "{query}: the statement reads entries through their session's index: {plan}"
        );
    }
}
