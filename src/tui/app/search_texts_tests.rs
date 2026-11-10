use super::claude_home::{ClaudeHome, admitted_cache};
use crate::clock::TestClock;
use crate::history::sqlite_cache::CachedSession;
use crate::history::{Conversation, Environment, ProcessIdentity};
use crate::search::lexical::SessionSearchTexts;
use crate::search::{ReadFailure, SearchTextsInMemory, SessionsWithoutTexts};
use chrono::Local;
use std::sync::Arc;

/// Reads the texts of each copy of `rows` that `texts` does not hold, as the
/// search worker's text reader reads them.
fn read_missing(texts: &mut SearchTextsInMemory, rows: &[Conversation]) -> usize {
    let copies = texts.keep_copies_of(rows).copies;
    finish_reads(texts, &copies);
    copies.len()
}

/// The read of `cached`'s texts, as the search worker's text reader reads
/// them; fails the test when the read fails.
fn read(cached: &CachedSession) -> Result<Arc<SessionSearchTexts>, ReadFailure> {
    Ok(Arc::new(
        SessionSearchTexts::read(cached)
            .unwrap()
            .expect("the cache holds the session"),
    ))
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
        texts.finish_read(cached.copy_key(), read(cached));
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
    for (copy, (key, scanned)) in launch.copies.iter().zip(launch.read()) {
        assert_eq!(
            scanned.unwrap().joined(),
            read(copy).unwrap().joined(),
            "the read of every row lays out each session as its own read does"
        );
        texts.finish_read(key, read(copy));
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

/// The rows of `home`'s sessions after `texts` keeps their copies, and the
/// read of each copy it asks for, read before the sessions change again.
fn rows_and_reads(
    home: &ClaudeHome,
    environment: &Environment,
    texts: &mut SearchTextsInMemory,
) -> (Vec<Conversation>, Vec<crate::search::TextsRead>) {
    let rows = cached_rows(home, environment);
    let reads = texts
        .keep_copies_of(&rows)
        .copies
        .iter()
        .map(|cached| (cached.copy_key(), read(cached)))
        .collect();
    (rows, reads)
}

#[test]
fn session_rewritten_faster_than_its_reads_holds_newest_texts_read_until_its_own() {
    let home = ClaudeHome::new();
    let a = home.write("a", 5, &["first wording"]);
    let (_cache, environment) = caching_environment(&home);
    let mut texts = SearchTextsInMemory::default();
    let (_, first) = rows_and_reads(&home, &environment, &mut texts);
    home.append(&a, 6, "second wording");
    let (_, second) = rows_and_reads(&home, &environment, &mut texts);
    home.append(&a, 7, "third wording");
    let (rows, third) = rows_and_reads(&home, &environment, &mut texts);

    let [(copy, read)] = <[_; 1]>::try_from(second).ok().unwrap();
    texts.finish_read(copy, read);
    let after_second = (
        found(&rows, &texts, "second"),
        texts.sessions_without_texts(),
    );
    let [(copy, read)] = <[_; 1]>::try_from(first).ok().unwrap();
    texts.finish_read(copy, read);
    let after_first = found(&rows, &texts, "second");
    let [(copy, read)] = <[_; 1]>::try_from(third).ok().unwrap();
    texts.finish_read(copy, read);

    assert_eq!(
        after_second,
        (vec!["a".to_owned()], SessionsWithoutTexts::default()),
        "an earlier copy's texts are held while the row's copy is read"
    );
    assert_eq!(
        after_first,
        ["a"],
        "a read older than the texts held leaves them held"
    );
    assert_eq!(found(&rows, &texts, "third"), ["a"]);
}

#[test]
fn rows_whose_text_reader_is_gone_count_as_unreadable_not_loading() {
    use super::search_state::{PendingWork, SearchCommand, SearchWorkerState};
    let home = ClaudeHome::new();
    home.write("a", 5, &["needle"]);
    home.write("b", 10, &["other"]);
    let (_cache, environment) = caching_environment(&home);
    let rows = cached_rows(&home, &environment);
    let (copies_to_read, reader) = std::sync::mpsc::channel();
    drop(reader);
    let mut worker = SearchWorkerState::new(copies_to_read, Arc::default());
    let (texts_reply, _) = std::sync::mpsc::channel();

    worker.apply(
        SearchCommand::UpdateData {
            searchable: Arc::new(crate::search::precompute_search_text(&rows)),
            conversations: Arc::new(rows.into_iter().map(Arc::new).collect()),
            texts_reply,
        },
        &mut PendingWork::default(),
    );

    assert_eq!(
        worker.texts.sessions_without_texts(),
        SessionsWithoutTexts {
            loading: 0,
            unreadable: 2
        },
        "the rows the reader can no longer read count as unreadable"
    );
}

/// The search bar as the TUI draws it.
fn search_bar_text(app: &super::App) -> String {
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 10)).unwrap();
    terminal
        .draw(|frame| crate::tui::ui::render(frame, app))
        .unwrap();
    let buffer = terminal.backend().buffer();
    (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .find(|row| row.contains("/2"))
        .expect("the search bar shows the result count")
}

/// Longer than the text reader takes to read a fixture session.
const TEXTS_READ_LAND: std::time::Duration = std::time::Duration::from_secs(10);

/// Receives the search worker's counts until `is_done`, failing after
/// [`TEXTS_READ_LAND`].
fn receive_until(app: &mut super::App, is_done: impl Fn(&super::App) -> bool) {
    let deadline = std::time::Instant::now() + TEXTS_READ_LAND;
    loop {
        app.receive_search_results();
        if is_done(app) {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the search worker counts the sessions"
        );
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

#[test]
fn search_bar_shows_sessions_loading_until_their_reads_land_then_sessions_unreadable() {
    let home = ClaudeHome::new();
    home.write("a", 5, &["needle"]);
    let b = home.write("b", 10, &["other"]);
    let (_cache, environment) = caching_environment(&home);
    let rows = cached_rows(&home, &environment);
    std::fs::remove_file(&b).unwrap();
    home.rows(&environment);
    let test_thread = std::thread::current().id();
    let gate =
        super::test_waits::ConnectionGate::closed(environment.cache_base().path(), move |thread| {
            thread.id() != test_thread
        });
    let mut app = super::claude_home::app_reading(rows, environment);

    receive_until(&mut app, |app| {
        app.texts_held.updates_applied == app.search_data_sent
    });
    let while_reading = search_bar_text(&app);
    gate.open();
    receive_until(&mut app, |app| !app.is_reading_search_texts());

    assert!(
        while_reading.contains("1/2 · 2 sessions loading"),
        "the search bar counts both sessions loading while their reads run: {while_reading:?}"
    );
    let after_reads = search_bar_text(&app);
    assert!(
        after_reads.contains("1/2 · 1 session unreadable"),
        "the search bar counts the session that left the cache as unreadable: {after_reads:?}"
    );
}

#[test]
fn texts_read_run_last_search_again_only_while_the_app_awaits_it() {
    use super::ListSearchMode;
    use super::search_state::{PendingWork, SearchCommand, SearchWorkerState};
    use std::sync::atomic::{AtomicU64, Ordering};
    let (copies_to_read, _reader) = std::sync::mpsc::channel();
    let awaited = Arc::new(AtomicU64::new(1));
    let mut worker = SearchWorkerState::new(copies_to_read, Arc::clone(&awaited));
    let mut searched = PendingWork::default();
    worker.apply(
        SearchCommand::Search {
            query: "needle".to_owned(),
            generation: 1,
            mode: ListSearchMode::Lexical,
        },
        &mut searched,
    );
    worker.search(searched.search.unwrap(), Local::now());
    let mut rerun_generation = || {
        let mut applied = PendingWork::default();
        worker.apply(SearchCommand::TextsRead(Vec::new()), &mut applied);
        applied.search.map(|search| search.generation)
    };

    let while_awaited = rerun_generation();
    awaited.store(2, Ordering::Release);
    let once_cleared = rerun_generation();

    assert_eq!(
        while_awaited,
        Some(1),
        "texts read run the awaited search again"
    );
    assert_eq!(
        once_cleared, None,
        "texts read don't run a search the app no longer awaits"
    );
}

#[test]
fn failed_read_counts_as_unreadable_keeps_its_cause_and_is_read_again_when_rows_change() {
    let home = ClaudeHome::new();
    home.write("a", 5, &["needle"]);
    let (_cache, environment) = caching_environment(&home);
    let rows = cached_rows(&home, &environment);
    let mut texts = SearchTextsInMemory::default();
    let copies = texts.keep_copies_of(&rows).copies;
    let failure = ReadFailure::Error("disk I/O error".into());
    texts.finish_read(copies[0].copy_key(), Err(failure.clone()));

    assert_eq!(
        texts.sessions_without_texts(),
        SessionsWithoutTexts {
            loading: 0,
            unreadable: 1
        },
        "a failed read counts as unreadable, not loading"
    );
    assert_eq!(
        texts.read_failure(&rows[0]),
        Some(&failure),
        "the failed read keeps its cause"
    );
    assert_eq!(
        read_missing(&mut texts, &rows),
        1,
        "the next change of the rows reads the session again"
    );
    assert_eq!(found(&rows, &texts, "needle"), ["a"]);
}
