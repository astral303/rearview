use super::*;
use crate::agent::refs::MessageRange;
use crate::history::sqlite_cache::CopyMiss;
use crate::history::sqlite_cache::tests::{
    admitted_cache, every_record_copies, mark_writing, replace_copy,
};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};

/// `copy`'s stored semantic turns, read alone from the copy the cache holds.
fn own_stored_turns(copy: &CachedSession) -> Vec<(usize, String)> {
    let mut turns = Vec::new();
    copy.read_copy(|held| {
        held.stored_semantic_turns(|part| {
            turns.push((
                part.ordinal.expect("a semantic turn has a message"),
                part.text,
            ));
        })
    })
    .expect("the cache holds the copy");
    turns
}

/// `copy`'s semantic turns, read alone from the copy the cache holds.
fn own_turns(copy: &CachedSession) -> SemanticTurns {
    let mut turns = SemanticTurns::default();
    for (ordinal, text) in own_stored_turns(copy) {
        turns.push(ordinal, text);
    }
    turns
}

/// The turns `scanned` read; panics for a copy it did not read.
fn read_turns<A>(scanned: &ScannedTurns<A>) -> &SemanticTurns {
    match scanned {
        Ok(ScannedCopy::Scanned(turns)) => turns,
        Ok(ScannedCopy::ReadAlone(_)) => panic!("the copy was replaced"),
        Err(miss) => panic!("the scan missed the copy: {miss:?}"),
    }
}

/// [`scan_in_tasks_of`], keeping the generation of a copy read alone.
fn scan_in_tasks_with_generations(
    copies: &[&CachedSession],
    entries_per_task: i64,
    before_task: &(dyn Fn() + Sync),
) -> Vec<ScannedTurns<i64>> {
    scan_in_tasks_of(copies, entries_per_task, before_task, |_, held| {
        Ok(held.generation())
    })
    .unwrap()
}

#[test]
fn scan_in_tasks_holds_each_sessions_turns_as_its_own_read_for_every_provider() {
    let base = tempfile::tempdir().unwrap();
    let copies = every_record_copies(base.path());
    let references = copies.iter().collect::<Vec<_>>();
    let own = copies.iter().map(own_turns).collect::<Vec<_>>();
    assert!(
        copies.iter().any(|copy| own_stored_turns(copy).len() > 1),
        "a fixture holds several turns"
    );
    for entries_per_task in [ENTRIES_PER_TASK, 1] {
        let tasks = AtomicUsize::new(0);
        let scanned = scan_in_tasks_with_generations(&references, entries_per_task, &|| {
            tasks.fetch_add(1, Ordering::Relaxed);
        });
        for ((copy, scanned), own) in copies.iter().zip(&scanned).zip(&own) {
            assert_eq!(
                read_turns(scanned),
                own,
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
fn failed_copy_drops_its_waiting_turns_and_doesnt_add_later_reads() {
    let mut copy = CopyTurns::default();
    copy.add(3..=4, vec![(2, "a later task's turn".to_owned())]);
    assert_eq!(copy.waiting.len(), 1, "a later task's turns wait");

    copy.drop_turns();
    copy.add(0..=2, vec![(1, "the first task's turn".to_owned())]);

    assert!(
        copy.waiting.is_empty() && copy.turns.is_empty(),
        "the failed copy doesn't hold any turns"
    );
}

#[test]
fn session_replaced_during_scan_is_read_alone_and_others_keep_their_turns() {
    let base = tempfile::tempdir().unwrap();
    let copies = every_record_copies(base.path());
    let references = copies.iter().collect::<Vec<_>>();
    let own = copies.iter().map(own_turns).collect::<Vec<_>>();
    let replaced = OnceLock::new();

    let scanned = scan_in_tasks_with_generations(&references, ENTRIES_PER_TASK, &|| {
        replaced.get_or_init(|| replace_copy(&copies[1]));
    });

    let generation = *replaced.get().expect("a task read turns");
    assert!(
        matches!(scanned[1], Ok(ScannedCopy::ReadAlone(read)) if read == generation),
        "the session replaced during the scan is read alone from the copy the cache holds: \
         {:?}",
        scanned[1]
    );
    for (position, (scanned, own)) in scanned.iter().zip(&own).enumerate() {
        if position != 1 {
            assert_eq!(read_turns(scanned), own, "copy {position} keeps its turns");
        }
    }
}

#[test]
fn failed_read_of_one_sessions_turns_fails_that_session_alone() {
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
            "UPDATE semantic_turns SET text = CAST(x'ff' AS TEXT) \
             WHERE session = (SELECT id FROM sessions WHERE generation = ?1)",
            [copies[failing].generation()],
        )
        .unwrap();

    let scanned = scan_in_tasks_with_generations(&references, ENTRIES_PER_TASK, &|| {});

    assert!(
        matches!(scanned[failing], Err(CopyMiss::ReadError(_))),
        "the read of the session with a turn that isn't UTF-8 fails"
    );
    for (position, (scanned, own)) in scanned.iter().zip(&own).enumerate() {
        if position != failing {
            assert_eq!(read_turns(scanned), own, "copy {position} keeps its turns");
        }
    }
}

#[test]
fn session_written_before_or_during_scan_is_not_ready_alone_and_others_keep_their_turns() {
    let base = tempfile::tempdir().unwrap();
    let copies = every_record_copies(base.path());
    let references = copies.iter().collect::<Vec<_>>();
    let own = copies.iter().map(own_turns).collect::<Vec<_>>();
    let (before, during) = (0, 2);
    mark_writing(&copies[before]);
    let marked = OnceLock::new();

    let scanned = scan_in_tasks_with_generations(&references, ENTRIES_PER_TASK, &|| {
        marked.get_or_init(|| mark_writing(&copies[during]));
    });

    for (position, (scanned, own)) in scanned.iter().zip(&own).enumerate() {
        if position == before || position == during {
            assert!(
                matches!(scanned, Err(CopyMiss::NotReady)),
                "copy {position}, written while the scan reads, is not ready: {scanned:?}"
            );
        } else {
            assert_eq!(read_turns(scanned), own, "copy {position} keeps its turns");
        }
    }
}

/// The scan's two tasks of one session run on one thread, and another
/// rearview rewrites the session between their reads: the first task reads
/// the old copy and the second the new one. The assembled row holds the new
/// copy alone: its turns, their message ranges, and its message times.
#[test]
fn session_rewritten_between_two_task_reads_assembles_the_new_copy_alone() {
    use crate::history::Conversation;
    use crate::search::test_fixtures::{
        cached_claude_sessions, claude_user_record, october, october_messages,
    };
    let sessions = cached_claude_sessions(&[(
        "rewritten",
        october_messages(&["first question", "second question"]),
    )]);
    let row = &sessions.rows[0];
    let copy = row.cached_session.as_ref().expect("the row has a copy");
    let rewrite = ["third question", "fourth question", "fifth question"]
        .iter()
        .zip(9..)
        .map(|(text, hour)| claude_user_record(text, october(11, hour)))
        .collect::<Vec<_>>();
    let task_reads = AtomicUsize::new(0);
    let one_thread = rayon::ThreadPoolBuilder::new()
        .num_threads(1)
        .build()
        .unwrap();

    let rows = [(row, copy)];
    let scanned = one_thread.install(|| {
        scan_in_tasks_of(
            &[copy],
            1,
            &|| {
                if task_reads.fetch_add(1, Ordering::SeqCst) == 1 {
                    sessions.rewrite(0, &rewrite);
                }
            },
            Conversation::semantic_turns_read_alone(&rows),
        )
        .unwrap()
    });
    let held = Conversation::with_scanned_turns_of(&rows, scanned)
        .remove(0)
        .expect("the session is in the session cache");

    assert_eq!(
        task_reads.into_inner(),
        2,
        "the old copy's two turns are read in two tasks"
    );
    let rewritten = sessions.listed_row(0);
    assert_ne!(
        rewritten.message_times, row.message_times,
        "the rewrite changes the message times"
    );
    assert_eq!(
        (
            &held.row.semantic_turns,
            &held.row.semantic_turn_ranges,
            held.row.message_count,
            &held.row.message_times,
            held.generation,
        ),
        (
            &vec![
                "third question".to_owned(),
                "fourth question".to_owned(),
                "fifth question".to_owned(),
            ],
            &(1..=3).map(MessageRange::single).collect::<Vec<_>>(),
            3,
            &rewritten.message_times,
            rewritten.cached_session.as_ref().unwrap().generation(),
        ),
        "the row holds the rewritten copy's turns, message ranges, count and times"
    );
}

/// Lays out `stored`, each an ordinal and a semantic turn, as
/// [`SemanticTurns`] does.
fn laid_out(stored: &[(usize, &str)]) -> (Vec<String>, Vec<MessageRange>) {
    let mut turns = SemanticTurns::default();
    for &(ordinal, text) in stored {
        turns.push(ordinal, text.to_owned());
    }
    turns.into_parts()
}

#[test]
fn stored_turns_of_one_message_join_into_its_turn_wherever_they_come() {
    let ranges = |ordinals: &[usize]| {
        ordinals
            .iter()
            .map(|&ordinal| MessageRange::single(ordinal))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        laid_out(&[(1, "a"), (1, "b"), (2, "c")]),
        (vec!["a b".to_owned(), "c".to_owned()], ranges(&[1, 2])),
        "a message's adjacent turns join"
    );
    assert_eq!(
        laid_out(&[(1, "a"), (2, "b"), (1, "c"), (3, "d"), (2, "e")]),
        (
            vec!["a c".to_owned(), "b e".to_owned(), "d".to_owned()],
            ranges(&[1, 2, 3])
        ),
        "a message's turns join after another message's, in its first turn's place"
    );
}

#[test]
fn copy_turns_lay_out_tasks_in_sequence_order_whatever_order_they_end_in() {
    let mut copy = CopyTurns::default();

    copy.add(2..=3, vec![(2, "c".to_owned())]);
    copy.add(4..=5, vec![(3, "d".to_owned())]);
    copy.add(0..=1, vec![(1, "a".to_owned()), (2, "b".to_owned())]);

    let expected = {
        let mut turns = SemanticTurns::default();
        for (ordinal, text) in [(1, "a"), (2, "b"), (2, "c"), (3, "d")] {
            turns.push(ordinal, text.to_owned());
        }
        turns
    };
    assert_eq!(
        copy.turns, expected,
        "the turns follow the tasks' sequences, the turn read in the first task joining the \
         next task's"
    );
    assert!(copy.waiting.is_empty(), "no task's turns wait");
}

/// Each statement reads one session's turns by their key, without joins and
/// without scanning the table.
#[test]
fn semantic_statements_read_turns_by_their_session_and_sequence() {
    let connection = Connection::open_in_memory().unwrap();
    connection
        .execute_batch(super::super::schema::CREATE)
        .unwrap();
    for query in [entries::SEMANTIC_TURNS, SESSION_SEQUENCE_BOUNDS] {
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
        let searches = plan
            .lines()
            .filter(|step| step.contains("SEARCH"))
            .collect::<Vec<_>>();
        assert!(
            !searches.is_empty()
                && searches
                    .iter()
                    .all(|step| step.contains("semantic_turns USING PRIMARY KEY (session=?"))
                && !plan.contains("SCAN semantic_turns"),
            "{query}: the statement reads turns by their key alone: {plan}"
        );
    }
}
