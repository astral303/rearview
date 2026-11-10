use super::claude_home::{ClaudeHome, admitted_cache};
use crate::clock::TestClock;
use crate::history::{Conversation, Environment, ProcessIdentity};
use crate::search::SearchTextsInMemory;
use crate::search::lexical::SessionSearchTexts;
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
    crate::search::try_search_with_texts(rows, &searchable, texts, query, Local::now())
        .unwrap()
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

#[test]
fn rewritten_session_replaces_its_texts_in_memory_and_deleted_session_drops_them() {
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
    assert_eq!(
        read_missing(&mut texts, &rewritten),
        1,
        "only the rewritten session is read again"
    );
    assert!(
        texts.get(row(&first, "a")).is_none(),
        "the old copy's texts are dropped"
    );
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
fn search_before_texts_are_read_returns_cache_paths_results() {
    let home = ClaudeHome::new();
    home.write("a", 5, &["shared needle", "only in a"]);
    home.write("b", 10, &["shared needle twice needle"]);
    let (_cache, environment) = caching_environment(&home);
    let rows = cached_rows(&home, &environment);
    let not_read = SearchTextsInMemory::default();
    let mut partly_read = SearchTextsInMemory::default();
    let copies = partly_read.keep_copies_of(&rows).copies;
    let first = &copies[0];
    partly_read.finish_read(
        first.copy_key(),
        SessionSearchTexts::read(first).unwrap().map(Arc::new),
    );
    let mut all_read = SearchTextsInMemory::default();
    read_missing(&mut all_read, &rows);

    for query in ["needle", "shared needle", "only", "\"twice needle\""] {
        let from_cache = found(&rows, &not_read, query);
        assert!(!from_cache.is_empty(), "{query:?} finds a session");
        assert_eq!(found(&rows, &partly_read, query), from_cache, "{query:?}");
        assert_eq!(found(&rows, &all_read, query), from_cache, "{query:?}");
    }
}
