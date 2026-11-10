use super::claude_home::{ClaudeHome, admitted_cache};
use crate::clock::TestClock;
use crate::history::sqlite_cache::CachedSession;
use crate::history::{Conversation, Environment, ProcessIdentity};
use crate::search::lexical::SessionSearchTexts;
use crate::search::{SearchTextsInMemory, SessionsWithoutTexts};
use chrono::Local;
use std::sync::Arc;

/// Reads the texts of each copy of `rows` that `texts` does not hold, as the
/// search worker's text reader reads them.
fn read_missing(texts: &mut SearchTextsInMemory, rows: &[Conversation]) -> usize {
    let copies = texts.keep_copies_of(rows).copies;
    for cached in &copies {
        let read = SessionSearchTexts::read(cached).unwrap().map(Arc::new);
        texts.finish_read(cached.copy_key(), read);
    }
    copies.len()
}

fn found(rows: &[Conversation], texts: &SearchTextsInMemory, query: &str) -> Vec<String> {
    let searchable = crate::search::precompute_search_text(rows);
    crate::search::search_with_texts(rows, &searchable, texts, query, Local::now())
        .into_iter()
        .map(|index| rows[index].session_id.clone())
        .collect()
}

fn row<'a>(rows: &'a [Conversation], name: &str) -> &'a Conversation {
    rows.iter().find(|row| row.session_id == name).unwrap()
}

/// `home`'s environment over an admitted session cache: a load caches what
/// it reads.
fn caching_environment(home: &ClaudeHome) -> (tempfile::TempDir, Environment) {
    let cache = admitted_cache();
    let environment = home.environment_with_cache(
        cache.path(),
        ProcessIdentity::default(),
        Arc::new(TestClock::default()),
    );
    (cache, environment)
}

/// `home`'s rows as a load lists them from the session cache, after a first
/// load caches each session new or changed since.
fn cached_rows(home: &ClaudeHome, environment: &Environment) -> Vec<Conversation> {
    home.rows(environment);
    let rows = home.rows(environment);
    assert!(rows.iter().all(|row| row.cached_session.is_some()));
    rows
}

/// Holds the read of each of `copies`.
fn finish_reads(texts: &mut SearchTextsInMemory, copies: &[CachedSession]) {
    for cached in copies {
        let read = SessionSearchTexts::read(cached).unwrap().map(Arc::new);
        texts.finish_read(cached.copy_key(), read);
    }
}

#[test]
fn rewritten_session_keeps_its_texts_until_new_copys_read_and_deleted_session_drops_them() {
    let home = ClaudeHome::new();
    let a = home.write("a", 5, &["original wording"]);
    let b = home.write("b", 10, &["unrelated"]);
    let (_cache, environment) = caching_environment(&home);
    let first = cached_rows(&home, &environment);
    let mut texts = SearchTextsInMemory::default();
    assert_eq!(read_missing(&mut texts, &first), 2);
    assert_eq!(found(&first, &texts, "original"), ["a"]);

    home.append(&a, 6, "revised wording");
    let rewritten = cached_rows(&home, &environment);
    let reading = texts.keep_copies_of(&rewritten).copies;
    assert_eq!(reading.len(), 1, "only the rewritten session is read again");
    assert_eq!(
        (
            found(&rewritten, &texts, "original"),
            found(&rewritten, &texts, "revised")
        ),
        (vec!["a".to_owned()], Vec::new()),
        "until its new copy is read, the rewritten session is searched in its previous texts"
    );
    assert_eq!(
        texts.sessions_without_texts(),
        SessionsWithoutTexts::default(),
        "a session searched in its previous texts doesn't count as loading"
    );
    finish_reads(&mut texts, &reading);
    assert_eq!(found(&rewritten, &texts, "revised"), ["a"]);

    std::fs::remove_file(&b).unwrap();
    let remaining = cached_rows(&home, &environment);
    assert_eq!(read_missing(&mut texts, &remaining), 0);
    assert!(
        texts.get(row(&rewritten, "b")).is_none(),
        "the deleted session's texts are dropped"
    );
}

#[test]
fn first_read_covers_every_row_and_read_after_change_covers_changed_row() {
    let home = ClaudeHome::new();
    let a = home.write("a", 5, &["original wording"]);
    home.write("b", 10, &["unrelated"]);
    let (_cache, environment) = caching_environment(&home);
    let first = cached_rows(&home, &environment);
    let mut texts = SearchTextsInMemory::default();

    let launch = texts.keep_copies_of(&first);
    assert!(
        launch.are_every_row && launch.copies.len() == 2,
        "the first read covers every row"
    );
    for (copy, read) in launch.copies.iter().zip(launch.read()) {
        assert_eq!(
            read.1.map(|texts| texts.joined().clone()),
            SessionSearchTexts::read(copy)
                .unwrap()
                .map(|texts| texts.joined().clone()),
            "the read of every row lays out each session as its own read does"
        );
        texts.finish_read(
            read.0,
            SessionSearchTexts::read(copy).unwrap().map(Arc::new),
        );
    }

    home.append(&a, 6, "revised wording");
    let changed = texts.keep_copies_of(&cached_rows(&home, &environment));
    assert!(
        !changed.are_every_row && changed.copies.len() == 1,
        "the read after one session changed covers that session alone"
    );
}

#[test]
fn search_reads_held_texts_alone_and_counts_sessions_still_loading() {
    let home = ClaudeHome::new();
    home.write("a", 5, &["shared needle", "only in a"]);
    home.write("b", 10, &["shared needle twice needle"]);
    let (_cache, environment) = caching_environment(&home);
    let rows = cached_rows(&home, &environment);
    let mut partly_read = SearchTextsInMemory::default();
    let copies = partly_read.keep_copies_of(&rows).copies;
    finish_reads(&mut partly_read, &copies[..1]);
    let held = &rows[0].session_id;
    let mut all_read = SearchTextsInMemory::default();
    read_missing(&mut all_read, &rows);

    assert_eq!(
        partly_read.sessions_without_texts(),
        SessionsWithoutTexts {
            loading: 1,
            unreadable: 0
        },
        "the session whose texts are being read counts as loading"
    );
    for query in ["needle", "shared needle", "only", "\"twice needle\""] {
        let every_text = found(&rows, &all_read, query);
        assert!(!every_text.is_empty(), "{query:?} finds a session");
        assert_eq!(
            found(&rows, &partly_read, query),
            every_text
                .into_iter()
                .filter(|found| found == held)
                .collect::<Vec<_>>(),
            "{query:?}: the search finds the session whose texts are held alone"
        );
    }
}

#[test]
fn failed_read_counts_as_unreadable_and_is_read_again_when_rows_change() {
    let home = ClaudeHome::new();
    home.write("a", 5, &["needle"]);
    let (_cache, environment) = caching_environment(&home);
    let rows = cached_rows(&home, &environment);
    let mut texts = SearchTextsInMemory::default();
    let copies = texts.keep_copies_of(&rows).copies;
    texts.finish_read(copies[0].copy_key(), None);

    assert_eq!(
        texts.sessions_without_texts(),
        SessionsWithoutTexts {
            loading: 0,
            unreadable: 1
        },
        "a failed read counts as unreadable, not loading"
    );
    assert_eq!(
        read_missing(&mut texts, &rows),
        1,
        "the next change of the rows reads the session again"
    );
    assert_eq!(found(&rows, &texts, "needle"), ["a"]);
}
