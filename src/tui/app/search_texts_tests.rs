use super::claude_home::{ClaudeHome, admitted_cache};
use crate::cache_file::test_support::TemporaryCacheDirectory;
use crate::clock::TestClock;
use crate::history::sqlite_cache::CachedSession;
use crate::history::{Conversation, Environment, ProcessIdentity};
use crate::search::lexical::SessionSearchTexts;
use crate::search::{CopiesToRead, ReadFailure, SearchTextsInMemory, SessionsWithoutTexts};
use chrono::Local;
use std::sync::Arc;

/// Reads the texts of each copy of `rows` that `texts` does not hold, as the
/// search worker's text reader reads them.
fn read_missing(texts: &mut SearchTextsInMemory, rows: &[Conversation]) -> usize {
    let copies = texts.keep_copies_of(rows).copies().to_vec();
    finish_reads(texts, &copies);
    copies.len()
}

/// The read of `cached`'s texts, as the search worker's text reader reads
/// them; fails the test when the read fails.
fn read(cached: &CachedSession) -> Result<Arc<SessionSearchTexts>, ReadFailure> {
    Ok(Arc::new(
        SessionSearchTexts::read(cached).expect("the cache holds the session"),
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
fn caching_environment(home: &ClaudeHome) -> (TemporaryCacheDirectory, Environment) {
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
    let reading = texts.keep_copies_of(&rewritten).copies().to_vec();
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
        matches!(&launch, CopiesToRead::EveryRow(copies) if copies.len() == 2),
        "the first read covers every row"
    );
    for (copy, (key, scanned)) in launch.copies().iter().zip(launch.read()) {
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
        matches!(&changed, CopiesToRead::EachSession { copies, .. } if copies.len() == 1),
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
    let copies = partly_read.keep_copies_of(&rows).copies().to_vec();
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

/// The texts `texts` holds for the session `name` after it reads each copy of
/// `rows` it doesn't hold, as the text reader reads them: a session holding
/// earlier texts reads only what an append on them changed.
fn reread(
    texts: &mut SearchTextsInMemory,
    rows: &[Conversation],
    name: &str,
) -> Arc<SessionSearchTexts> {
    let key = row(rows, name).cached_session.as_ref().unwrap().copy_key();
    let mut reread = None;
    for (copy, read) in texts.keep_copies_of(rows).read() {
        if copy == key {
            reread = Some(Arc::clone(
                read.as_ref().expect("the reread finds the session's copy"),
            ));
        }
        texts.finish_read(copy, read);
    }
    reread.expect("the session's changed copy is read again")
}

/// The texts of `name`'s copy among `rows`, read whole.
fn whole_read(rows: &[Conversation], name: &str) -> Arc<SessionSearchTexts> {
    read(row(rows, name).cached_session.as_ref().unwrap()).unwrap()
}

#[test]
fn reread_after_appended_messages_reads_their_texts_alone_and_holds_whole_reads_texts() {
    let home = ClaudeHome::new();
    let a = home.write(
        "a",
        5,
        &["first question", "second question", "third question"],
    );
    let (_cache, environment) = caching_environment(&home);
    let first = cached_rows(&home, &environment);
    let mut texts = SearchTextsInMemory::default();
    read_missing(&mut texts, &first);
    crate::history::sqlite_cache::tests::alter_stored_texts(
        row(&first, "a").cached_session.as_ref().unwrap(),
        "first question",
        "altered question",
    );

    home.append_reply(&a, 6, "an answer", 10);
    home.append(&a, 7, "fourth question");
    let appended = cached_rows(&home, &environment);
    let held = reread(&mut texts, &appended, "a");
    let whole = whole_read(&appended, "a");

    assert!(
        whole.joined().contains("altered question"),
        "a whole read shows the row altered before the append"
    );
    assert_eq!(
        &**held.joined(),
        whole
            .joined()
            .replace("altered question", "first question")
            .as_str(),
        "the reread holds the whole read's texts, the row before the append as first read"
    );
    assert!(
        held.joined().contains("fourth question") && held.joined().contains("an answer"),
        "the reread contains the appended message and reply: {}",
        held.joined()
    );
}

/// Adds a turn duration record, outside every message, sent `minute` past
/// noon, to the transcript at `path`.
fn append_turn_duration(path: &std::path::Path, minute: u32) {
    let mut transcript = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    let line = serde_json::json!({
        "type": "system",
        "subtype": "turn_duration",
        "durationMs": 1000,
        "level": "info",
        "timestamp": format!("2026-10-01T12:{minute:02}:58Z"),
    });
    std::io::Write::write_all(&mut transcript, format!("{line}\n").as_bytes()).unwrap();
}

fn copy_of<'r>(rows: &'r [Conversation], name: &str) -> &'r CachedSession {
    row(rows, name).cached_session.as_ref().unwrap()
}

#[test]
fn reread_after_appended_messages_keeps_held_texts_of_later_messages_and_outside_messages() {
    use crate::history::sqlite_cache::tests::{add_texts_outside_messages, alter_stored_texts};
    let home = ClaudeHome::new();
    let a = home.write(
        "a",
        5,
        &["first question", "second question", "third question"],
    );
    append_turn_duration(&a, 5);
    home.append(&a, 6, "fourth question");
    let (_cache, environment) = caching_environment(&home);
    let first = cached_rows(&home, &environment);
    add_texts_outside_messages(copy_of(&first, "a"), "noted outside");
    let mut texts = SearchTextsInMemory::default();
    read_missing(&mut texts, &first);
    for (from, to) in [
        ("second question", "altered second"),
        ("noted outside", "altered outside"),
    ] {
        alter_stored_texts(copy_of(&first, "a"), from, to);
    }

    home.append_reply(&a, 7, "an answer", 10);
    let appended = cached_rows(&home, &environment);
    let held = reread(&mut texts, &appended, "a");
    let whole = whole_read(&appended, "a");

    assert!(
        whole.joined().contains("altered second") && whole.joined().contains("altered outside"),
        "a whole read shows the rows altered before the append: {}",
        whole.joined()
    );
    assert_eq!(
        &**held.joined(),
        whole
            .joined()
            .replace("altered second", "second question")
            .replace("altered outside", "noted outside")
            .as_str(),
        "the reread keeps the held texts of a later message and outside every message"
    );
}

#[test]
fn reread_after_two_appends_in_a_row_holds_whole_reads_texts() {
    use crate::history::sqlite_cache::tests::alter_stored_texts;
    let home = ClaudeHome::new();
    let a = home.write("a", 5, &["first question", "second question"]);
    let (_cache, environment) = caching_environment(&home);
    let first = cached_rows(&home, &environment);
    let mut texts = SearchTextsInMemory::default();
    read_missing(&mut texts, &first);
    alter_stored_texts(copy_of(&first, "a"), "first question", "altered question");

    home.append(&a, 6, "third question");
    reread(&mut texts, &cached_rows(&home, &environment), "a");
    home.append(&a, 7, "fourth question");
    let appended = cached_rows(&home, &environment);
    let held = reread(&mut texts, &appended, "a");
    let whole = whole_read(&appended, "a");

    assert_eq!(
        &**held.joined(),
        whole
            .joined()
            .replace("altered question", "first question")
            .as_str(),
        "each reread reads its append alone and holds the whole read's texts"
    );
}

#[test]
fn reread_of_texts_older_than_the_appends_previous_copy_reads_the_session_whole() {
    use crate::history::sqlite_cache::tests::alter_stored_texts;
    let home = ClaudeHome::new();
    let a = home.write("a", 5, &["first question", "second question"]);
    let (_cache, environment) = caching_environment(&home);
    let first = cached_rows(&home, &environment);
    let mut texts = SearchTextsInMemory::default();
    read_missing(&mut texts, &first);
    alter_stored_texts(copy_of(&first, "a"), "first question", "altered question");

    home.append(&a, 6, "third question");
    cached_rows(&home, &environment);
    home.append(&a, 7, "fourth question");
    let appended = cached_rows(&home, &environment);
    let held = reread(&mut texts, &appended, "a");

    assert!(
        held.joined().contains("altered question"),
        "texts read before the append's previous copy are read again whole: {}",
        held.joined()
    );
    assert_eq!(held.joined(), whole_read(&appended, "a").joined());
}

#[test]
fn reread_after_append_without_message_texts_holds_whole_reads_texts() {
    let home = ClaudeHome::new();
    let a = home.write("a", 5, &["first question", "second question"]);
    append_turn_duration(&a, 5);
    let (_cache, environment) = caching_environment(&home);
    let first = cached_rows(&home, &environment);
    let mut texts = SearchTextsInMemory::default();
    read_missing(&mut texts, &first);
    let previous = copy_of(&first, "a").generation();

    append_turn_duration(&a, 6);
    let appended = cached_rows(&home, &environment);
    let append = copy_of(&appended, "a")
        .clone()
        .waiting()
        .read_copy(|copy| copy.append_on(previous))
        .unwrap();
    let held = reread(&mut texts, &appended, "a");

    assert_eq!(
        append.map(|append| append.first_message),
        Some(None),
        "the append didn't rewrite any message's texts"
    );
    assert_eq!(held.joined(), whole_read(&appended, "a").joined());
    assert_eq!(held.dialogue(), whole_read(&appended, "a").dialogue());
}

#[test]
fn reread_after_append_holds_whole_reads_texts_with_whitespace_normalised() {
    use crate::history::message_texts::PADDED_MESSAGE_SEPARATOR;
    use crate::history::sqlite_cache::tests::alter_stored_texts;
    let home = ClaudeHome::new();
    let a = home.write(
        "a",
        5,
        &["first   question\n\n", "\t second\u{a0} question"],
    );
    let (_cache, environment) = caching_environment(&home);
    let first = cached_rows(&home, &environment);
    let mut texts = SearchTextsInMemory::default();
    read_missing(&mut texts, &first);
    let previous = copy_of(&first, "a").generation();

    home.append(&a, 6, "third\u{3000}\u{3000}question\n");
    home.append(&a, 7, "becomes whitespace");
    let appended = cached_rows(&home, &environment);
    alter_stored_texts(
        copy_of(&appended, "a"),
        "becomes whitespace",
        " \n\t\u{2028} ",
    );
    let append = copy_of(&appended, "a")
        .clone()
        .waiting()
        .read_copy(|copy| copy.append_on(previous))
        .unwrap();
    let held = reread(&mut texts, &appended, "a");

    assert!(
        append.is_some(),
        "the reread reads the append's texts alone"
    );
    assert_eq!(
        &**held.joined(),
        ["first question", "second question", "third question"]
            .join(PADDED_MESSAGE_SEPARATOR)
            .as_str(),
        "the reread normalises whitespace in the kept and the appended texts and drops a text of whitespace alone"
    );
    assert_eq!(held.joined(), whole_read(&appended, "a").joined());
    assert_eq!(held.dialogue(), whole_read(&appended, "a").dialogue());
}

#[test]
fn reread_holds_whole_reads_texts_after_each_change_of_the_session() {
    let home = ClaudeHome::new();
    let parent = home.write("a", 5, &["delegate the survey", "check the quarry"]);
    let subagent = home.write_subagent(&parent, "a1", 5, &["survey the granite quarry"]);
    let (_cache, environment) = caching_environment(&home);
    let mut texts = SearchTextsInMemory::default();
    read_missing(&mut texts, &cached_rows(&home, &environment));
    let rename = |title: &str| {
        let crate::history::provider::Capability::Capable(renamer) =
            crate::history::Source::Claude.provider().rename()
        else {
            panic!("Claude renames a session");
        };
        renamer
            .rename_session(
                crate::history::FOR_FIXTURES,
                &parent,
                title,
                chrono::Utc::now(),
            )
            .unwrap();
    };
    let changes: [(&str, &dyn Fn()); 4] = [
        ("appended messages", &|| {
            home.append(&parent, 6, "report the findings")
        }),
        ("changed sub-agent transcript", &|| {
            home.append(&subagent, 6, "measure the basalt seam");
        }),
        ("rename", &|| rename("Quarry survey")),
        ("rewritten transcript", &|| {
            home.rewrite(&parent, 5, &["delegate the survey again", "check the mine"]);
        }),
    ];

    for (change, make) in changes {
        make();
        let rows = cached_rows(&home, &environment);
        let held = reread(&mut texts, &rows, "a");
        let whole = whole_read(&rows, "a");

        assert_eq!(
            held.joined(),
            whole.joined(),
            "{change}: the reread holds the texts a whole read gives"
        );
        assert_eq!(
            held.dialogue(),
            whole.dialogue(),
            "{change}: the reread's dialogue is the whole read's"
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
        .copies()
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
    assert_eq!(
        found(&rows, &texts, "third"),
        ["a"],
        "the read of the row's own copy holds its texts"
    );
}

#[test]
fn rows_whose_text_reader_is_gone_count_as_unreadable_not_loading() {
    use super::search_worker::{PendingWork, SearchCommand, SearchWorkerState};
    let home = ClaudeHome::new();
    home.write("a", 5, &["needle"]);
    home.write("b", 10, &["other"]);
    let (_cache, environment) = caching_environment(&home);
    let rows = cached_rows(&home, &environment);
    let (copies_to_read, reader) = std::sync::mpsc::channel();
    drop(reader);
    let (_generation, awaited) = super::request_generation::RequestGeneration::start();
    let (texts_reply, _) = std::sync::mpsc::channel();
    let mut worker = SearchWorkerState::new(copies_to_read, texts_reply, awaited);

    worker.apply(
        SearchCommand::UpdateData {
            searchable: Arc::new(crate::search::precompute_search_text(&rows)),
            conversations: Arc::new(rows.into_iter().map(Arc::new).collect()),
        }
        .into(),
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

#[test]
fn sessions_loading_count_down_with_each_batch_the_reader_replies() {
    use super::search_worker::{PendingWork, SearchCommand, SearchWorkerState, WorkerInput};
    let home = ClaudeHome::new();
    for (minute, name) in [(5, "a"), (10, "b"), (15, "c")] {
        home.write(name, minute, &["needle"]);
    }
    let (_cache, environment) = caching_environment(&home);
    let rows = cached_rows(&home, &environment);
    let one_at_a_time = CopiesToRead::EachSession {
        copies: SearchTextsInMemory::default()
            .keep_copies_of(&rows)
            .copies()
            .to_vec(),
        held: std::collections::HashMap::new(),
    };
    let batches = std::sync::Mutex::new(Vec::new());
    one_at_a_time.read_in_batches(&|batch| batches.lock().unwrap().push(batch));
    let (copies_to_read, _reader) = std::sync::mpsc::channel();
    let (_generation, awaited) = super::request_generation::RequestGeneration::start();
    let (texts_reply, _) = std::sync::mpsc::channel();
    let mut worker = SearchWorkerState::new(copies_to_read, texts_reply, awaited);
    worker.apply(
        SearchCommand::UpdateData {
            searchable: Arc::new(crate::search::precompute_search_text(&rows)),
            conversations: Arc::new(rows.into_iter().map(Arc::new).collect()),
        }
        .into(),
        &mut PendingWork::default(),
    );

    let mut loading = vec![worker.texts.sessions_without_texts().loading];
    for batch in batches.into_inner().unwrap() {
        worker.apply(WorkerInput::texts_read(batch), &mut PendingWork::default());
        loading.push(worker.texts.sessions_without_texts().loading);
    }

    assert_eq!(
        loading,
        [3, 2, 1, 0],
        "the read of one session at a time replies with each session as its read ends, and \
         each reply lowers the count of sessions loading"
    );
}

#[test]
fn search_worker_stops_once_app_drops_its_commands_while_a_read_runs() {
    use super::search_worker::{SearchCommand, spawn_search_worker};
    let home = ClaudeHome::new();
    home.write("a", 5, &["needle"]);
    let (_cache, environment) = caching_environment(&home);
    let rows = cached_rows(&home, &environment);
    let test_thread = std::thread::current().id();
    let gate =
        super::test_waits::ConnectionGate::closed(environment.cache_base().path(), move |thread| {
            thread.id() != test_thread
        });
    let worker = spawn_search_worker(Arc::new(TestClock::default()));
    worker
        .commands
        .send(SearchCommand::UpdateData {
            searchable: Arc::new(crate::search::precompute_search_text(&rows)),
            conversations: Arc::new(rows.into_iter().map(Arc::new).collect()),
        })
        .unwrap();
    let reading = worker.texts_held.recv_timeout(TEXTS_READ_LIMIT).unwrap();

    drop(worker.commands);
    let after_drop = worker.results.recv_timeout(TEXTS_READ_LIMIT);
    gate.open();

    assert_eq!(
        reading.sessions_without_texts.loading, 1,
        "the session's read is held at the gate"
    );
    assert_eq!(
        after_drop.err(),
        Some(std::sync::mpsc::RecvTimeoutError::Disconnected),
        "the worker stops while the read runs"
    );
}

/// The failure of each read in the reply of `reader` to a batch of two
/// sessions' copies; `None` for a read that returned texts.
fn failures_replied(
    reader: &std::sync::mpsc::Sender<super::search_worker::TextsToRead>,
) -> Vec<Option<String>> {
    let home = ClaudeHome::new();
    home.write("a", 5, &["needle"]);
    home.write("b", 10, &["other"]);
    let (_cache, environment) = caching_environment(&home);
    let rows = cached_rows(&home, &environment);
    let copies = SearchTextsInMemory::default().keep_copies_of(&rows);
    let (reply, replies) = std::sync::mpsc::channel();

    reader
        .send(super::search_worker::TextsToRead::new(copies, reply))
        .unwrap();
    replies
        .recv_timeout(TEXTS_READ_LIMIT)
        .expect("the reader replies")
        .into_texts_read()
        .expect("the reply contains the batch's reads")
        .into_iter()
        .map(|(_, read)| read.err().map(|failure| failure.to_string()))
        .collect()
}

#[test]
fn panicking_read_replies_with_every_copy_of_its_batch_failed() {
    use super::search_worker::{spawn_text_reader, text_reader_pool};
    let reader = spawn_text_reader(|_, _| panic!("the read broke"), text_reader_pool);

    let failures = failures_replied(&reader);

    assert_eq!(
        failures.len(),
        2,
        "the reply contains every copy of the batch"
    );
    for failure in failures {
        assert!(
            failure
                .as_deref()
                .is_some_and(|failure| failure.contains("panicked: the read broke")),
            "each copy failed with the panic: {failure:?}"
        );
    }
}

#[test]
fn failed_start_of_readers_threads_replies_with_every_copy_of_its_batch_failed() {
    use super::search_worker::spawn_text_reader;
    let reader = spawn_text_reader(crate::search::CopiesToRead::read_in_batches, || {
        rayon::ThreadPoolBuilder::new()
            .spawn_handler(|_| Err(std::io::Error::other("no thread left")))
            .build()
    });

    let failures = failures_replied(&reader);

    assert_eq!(
        failures.len(),
        2,
        "the reply contains every copy of the batch"
    );
    for failure in failures {
        assert!(
            failure.as_deref().is_some_and(|failure| {
                failure.contains("threads didn't start") && failure.contains("no thread left")
            }),
            "each copy failed with the build's error: {failure:?}"
        );
    }
}

#[test]
fn global_pool_job_runs_while_text_reader_holds_every_thread_of_its_pool() {
    use super::search_worker::{TextsToRead, spawn_text_reader, text_reader_pool};
    use std::sync::{Condvar, Mutex};
    static READ_STARTED: (Mutex<bool>, Condvar) = (Mutex::new(false), Condvar::new());
    static READ_RELEASED: (Mutex<bool>, Condvar) = (Mutex::new(false), Condvar::new());
    fn set(flag: &(Mutex<bool>, Condvar)) {
        *flag.0.lock().unwrap() = true;
        flag.1.notify_all();
    }
    fn wait_for(flag: &(Mutex<bool>, Condvar)) -> bool {
        let guard = flag.0.lock().unwrap();
        *flag
            .1
            .wait_timeout_while(guard, TEXTS_READ_LIMIT, |is_set| !*is_set)
            .unwrap()
            .0
    }
    let reader = spawn_text_reader(
        |_, reply| {
            rayon::broadcast(|_| {
                set(&READ_STARTED);
                wait_for(&READ_RELEASED);
            });
            reply(Vec::new());
        },
        text_reader_pool,
    );
    let (reply, replies) = std::sync::mpsc::channel();
    let copies = SearchTextsInMemory::default().keep_copies_of::<Conversation>(&[]);
    reader.send(TextsToRead::new(copies, reply)).unwrap();
    assert!(wait_for(&READ_STARTED), "the read starts");

    let (job_ran, job_done) = std::sync::mpsc::channel();
    rayon::spawn(move || job_ran.send(()).unwrap());
    let job = job_done.recv_timeout(TEXTS_READ_LIMIT);
    set(&READ_RELEASED);

    assert!(
        job.is_ok(),
        "a job queued on the global pool runs while the read holds its pool's threads"
    );
    replies
        .recv_timeout(TEXTS_READ_LIMIT)
        .expect("the reader replies once released");
}

/// The rows of a frame 100 columns wide and `height` rows high, as the TUI
/// draws it.
fn frame_rows(app: &super::App, height: u16) -> Vec<String> {
    let mut terminal =
        ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, height)).unwrap();
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
        .collect()
}

/// The search bar as the TUI draws it.
fn search_bar_text(app: &super::App) -> String {
    frame_rows(app, 10)
        .into_iter()
        .find(|row| row.contains("/2"))
        .expect("the search bar shows the result count")
}

/// Adds a model reply containing `blocks`, sent `minute` past noon, to the
/// transcript at `path`.
fn append_reply_blocks(path: &std::path::Path, minute: u32, blocks: &[serde_json::Value]) {
    let mut transcript = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    let line = serde_json::json!({
        "type": "assistant",
        "timestamp": format!("2026-10-01T12:{minute:02}:59Z"),
        "message": {"role": "assistant", "content": blocks},
    });
    std::io::Write::write_all(&mut transcript, format!("{line}\n").as_bytes()).unwrap();
}

#[test]
fn list_finds_sessions_matching_in_tool_call_thinking_and_subagent_text_and_shows_each_match() {
    use super::refresh_tests::listed_names;
    use crossterm::event::{KeyCode, KeyModifiers};
    let home = ClaudeHome::new();
    let tool_call = home.write("tool-call", 30, &["check the build"]);
    append_reply_blocks(
        &tool_call,
        30,
        &[
            serde_json::json!({"type": "tool_use", "id": "call", "name": "Bash",
            "input": {"command": "grep granite build.log"}}),
        ],
    );
    let thinking = home.write("thinking", 20, &["plan the release"]);
    append_reply_blocks(
        &thinking,
        20,
        &[
            serde_json::json!({"type": "thinking", "thinking": "weigh the granite option first",
            "signature": "sig"}),
        ],
    );
    let subagent_parent = home.write("sub-agent", 10, &["delegate the survey"]);
    home.write_subagent(&subagent_parent, "a1", 10, &["survey the granite quarry"]);
    home.write("dialogue-only", 5, &["water the plants"]);
    let (_cache, environment) = caching_environment(&home);
    let rows = cached_rows(&home, &environment);
    let mut app = super::claude_home::app_reading(rows, environment);
    receive_until(&mut app, |app| !app.is_reading_search_texts());

    for character in "granite".chars() {
        app.handle_key(KeyCode::Char(character), KeyModifiers::NONE, 20);
    }
    receive_until(&mut app, |app| !app.has_search_work_in_flight());

    let mut listed = listed_names(&app);
    listed.sort_unstable();
    assert_eq!(
        listed,
        ["sub-agent", "thinking", "tool-call"],
        "the list shows each session holding the word outside its dialogue"
    );
    let frame = frame_rows(&app, 20).join("\n");
    for context in [
        "grep granite build.log",
        "weigh the granite option first",
        "survey the granite quarry",
    ] {
        assert!(
            frame.contains(context),
            "a row's context line shows {context:?}:\n{frame}"
        );
    }
}

#[test]
fn session_found_in_subagents_thinking_shows_match_in_subagents_message_with_thinking_shown() {
    use super::refresh_tests::listed_names;
    use crossterm::event::{KeyCode, KeyModifiers};
    let home = ClaudeHome::new();
    let parent = home.write("sub-agent", 10, &["delegate the survey"]);
    let subagent = home.write_subagent(&parent, "a1", 10, &["survey the quarry"]);
    append_reply_blocks(
        &subagent,
        10,
        &[
            serde_json::json!({"type": "thinking", "thinking": "weigh the granite seam first",
                "signature": "sig"}),
            serde_json::json!({"type": "text", "text": "the seam is sound"}),
        ],
    );
    home.write("dialogue-only", 5, &["water the plants"]);
    let (_cache, environment) = caching_environment(&home);
    let rows = cached_rows(&home, &environment);
    let mut app = super::claude_home::app_reading(rows, environment);
    receive_until(&mut app, |app| !app.is_reading_search_texts());
    for character in "granite".chars() {
        app.handle_key(KeyCode::Char(character), KeyModifiers::NONE, 20);
    }
    receive_until(&mut app, |app| !app.has_search_work_in_flight());
    assert_eq!(
        listed_names(&app),
        ["sub-agent"],
        "the list finds the session by its sub-agent's thinking"
    );

    app.selected = Some(0);
    app.enter_view_mode(80);
    app.toggle_view_thinking();
    super::test_waits::finish_view_render(&mut app, 20);

    let super::AppMode::View(state) = app.app_mode() else {
        unreachable!("the viewer is open")
    };
    let matches = crate::tui::viewer::find_matches(&state.rendered_lines, "granite");
    assert_eq!(
        matches.len(),
        1,
        "the viewer's search finds the sub-agent's thinking once"
    );
    let row = matches[0].first_row();
    let message = state
        .message_ranges
        .iter()
        .find(|message| message.rows().contains(&row))
        .expect("the match is in a message");
    let message_text: String = state.rendered_lines[message.rows()]
        .iter()
        .map(crate::tui::viewer::RenderedLine::text)
        .collect();
    assert!(
        message_text.contains("the seam is sound"),
        "the match is in the sub-agent's message: {message_text:?}"
    );
}

/// Longer than the text reader takes to read a fixture session.
const TEXTS_READ_LIMIT: std::time::Duration = std::time::Duration::from_secs(10);

/// Receives the search worker's counts until `is_done`, failing after
/// [`TEXTS_READ_LIMIT`].
pub(super) fn receive_until(app: &mut super::App, is_done: impl Fn(&super::App) -> bool) {
    let deadline = std::time::Instant::now() + TEXTS_READ_LIMIT;
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
        "the search bar counts the session no longer in the session cache as unreadable: \
         {after_reads:?}"
    );
}

#[test]
fn session_read_again_counts_as_updating_until_its_read_ends() {
    let home = ClaudeHome::new();
    let a = home.write("a", 5, &["original wording"]);
    home.write("b", 10, &["unrelated"]);
    let (_cache, environment) = caching_environment(&home);
    let mut texts = SearchTextsInMemory::default();
    read_missing(&mut texts, &cached_rows(&home, &environment));
    home.append(&a, 6, "revised wording");

    let reading = texts
        .keep_copies_of(&cached_rows(&home, &environment))
        .copies()
        .to_vec();
    let while_reading = (texts.sessions_updating(), texts.sessions_without_texts());
    finish_reads(&mut texts, &reading);

    assert_eq!(
        while_reading,
        (1, SessionsWithoutTexts::default()),
        "the session read again counts as updating, not loading"
    );
    assert_eq!(
        texts.sessions_updating(),
        0,
        "the session stops updating once its read ends"
    );
}

#[test]
fn session_appended_after_generations_restart_holds_whole_reads_texts_and_stops_updating() {
    use crate::history::sqlite_cache::tests::{GenerationsRestart, restart_generations};
    for restart in [GenerationsRestart::SetBack, GenerationsRestart::Dropped] {
        let home = ClaudeHome::new();
        let a = home.write("a", 5, &["first question", "second question"]);
        home.write("b", 10, &["other"]);
        let (_cache, environment) = caching_environment(&home);
        let first = cached_rows(&home, &environment);
        let mut texts = SearchTextsInMemory::default();
        read_missing(&mut texts, &first);
        restart_generations(copy_of(&first, "a"), restart);

        home.append(&a, 6, "third question");
        let appended = cached_rows(&home, &environment);
        let held = reread(&mut texts, &appended, "a");

        assert_eq!(
            held.joined(),
            whole_read(&appended, "a").joined(),
            "{restart:?}: the reread holds a whole read's texts of the new copy"
        );
        assert_eq!(
            texts.sessions_updating(),
            0,
            "{restart:?}: the session stops updating once its read ends"
        );
        assert_eq!(
            found(&appended, &texts, "third"),
            ["a"],
            "{restart:?}: search reads the new copy's texts"
        );
    }
}

#[test]
fn every_row_changed_after_generations_restart_is_read_in_one_scan() {
    use crate::history::sqlite_cache::tests::{GenerationsRestart, restart_generations};
    for restart in [GenerationsRestart::SetBack, GenerationsRestart::Dropped] {
        let home = ClaudeHome::new();
        let a = home.write("a", 5, &["first question"]);
        let b = home.write("b", 10, &["other"]);
        let (_cache, environment) = caching_environment(&home);
        let first = cached_rows(&home, &environment);
        let mut texts = SearchTextsInMemory::default();
        read_missing(&mut texts, &first);
        restart_generations(copy_of(&first, "a"), restart);

        home.append(&a, 6, "second question");
        home.append(&b, 11, "another");
        let changed = cached_rows(&home, &environment);
        let reading = texts.keep_copies_of(&changed);
        let while_reading = texts.sessions_updating();
        for (copy, read) in reading.read() {
            texts.finish_read(copy, read);
        }

        assert!(
            matches!(reading, CopiesToRead::EveryRow(_)),
            "{restart:?}: the copies are read in one scan of every row"
        );
        assert_eq!(
            (while_reading, texts.sessions_updating()),
            (2, 0),
            "{restart:?}: both sessions update until their reads end"
        );
        for name in ["a", "b"] {
            assert_eq!(
                texts
                    .get(row(&changed, name))
                    .map(|held| held.joined().clone()),
                Some(whole_read(&changed, name).joined().clone()),
                "{restart:?}: {name} holds a whole read's texts of its new copy"
            );
        }
    }
}

#[test]
fn search_bar_shows_session_the_worker_reads_again_until_its_read_ends() {
    let home = ClaudeHome::new();
    let a = home.write("a", 5, &["needle"]);
    home.write("b", 10, &["other"]);
    let (_cache, environment) = caching_environment(&home);
    let rows = cached_rows(&home, &environment);
    let mut app = super::claude_home::app_reading(rows, environment);
    receive_until(&mut app, |app| !app.is_reading_search_texts());
    home.append(&a, 6, "revised wording");

    let gate =
        super::test_waits::ConnectionGate::closed(app.environment.cache_base().path(), |thread| {
            thread
                .name()
                .is_some_and(|name| name.starts_with("text-read"))
        });
    super::claude_home::serve_list_refresh(&mut app, crate::time_filter::TimeFilter::default());
    receive_until(&mut app, |app| app.texts_held.sessions_updating > 0);
    let while_reading = search_bar_text(&app);
    gate.open();
    receive_until(&mut app, |app| !app.is_reading_search_texts());

    assert!(
        while_reading.contains("1/2 · 1 session updating"),
        "the search bar shows the worker's count of the session read again: {while_reading:?}"
    );
    let after_read = search_bar_text(&app);
    assert!(
        !after_read.contains("updating"),
        "the count ends with the read: {after_read:?}"
    );
}

#[test]
fn search_bar_shows_sessions_updating_and_hides_them_behind_refreshing() {
    let home = ClaudeHome::new();
    home.write("a", 5, &["needle"]);
    home.write("b", 10, &["other"]);
    let (_cache, environment) = caching_environment(&home);
    let rows = cached_rows(&home, &environment);
    let mut app = super::claude_home::app_reading(rows, environment);
    receive_until(&mut app, |app| !app.is_reading_search_texts());
    // The counts are this test's, not the worker's.
    app.texts_held_rx = std::sync::mpsc::channel().1;
    app.texts_held.sessions_updating = 2;

    let updating = search_bar_text(&app);
    app.begin_refresh().expect("a refresh begins");
    let refreshing = frame_rows(&app, 10).join("\n");

    assert!(
        updating.contains("1/2 · 2 sessions updating"),
        "the search bar counts the sessions read again: {updating:?}"
    );
    assert!(
        refreshing.contains("refreshing…") && !refreshing.contains("updating"),
        "a refresh's label hides the count: {refreshing}"
    );
}

#[test]
fn texts_read_run_last_search_again_only_while_the_app_awaits_it() {
    use super::ListSearchMode;
    use super::search_worker::{PendingWork, SearchCommand, SearchWorkerState, WorkerInput};
    let (copies_to_read, _reader) = std::sync::mpsc::channel();
    let (mut generation, awaited) = super::request_generation::RequestGeneration::start();
    let (texts_reply, _) = std::sync::mpsc::channel();
    let mut worker = SearchWorkerState::new(copies_to_read, texts_reply, awaited);
    let mut searched = PendingWork::default();
    let searched_generation = generation.advance();
    worker.apply(
        SearchCommand::Search {
            query: "needle".to_owned(),
            generation: searched_generation,
            mode: ListSearchMode::Lexical,
        }
        .into(),
        &mut searched,
    );
    worker.search(searched.search.unwrap(), Local::now());
    let mut rerun_generation = || {
        let mut applied = PendingWork::default();
        worker.apply(WorkerInput::texts_read(Vec::new()), &mut applied);
        applied.search.map(|search| search.generation)
    };

    let while_awaited = rerun_generation();
    generation.advance();
    let once_cleared = rerun_generation();

    assert_eq!(
        while_awaited,
        Some(searched_generation),
        "texts read run the awaited search again"
    );
    assert_eq!(
        once_cleared, None,
        "texts read don't run a search the app no longer awaits"
    );
}

#[test]
fn texts_read_after_semantic_results_show_dont_rerun_their_lexical_fallback() {
    use super::ListSearchMode;
    use super::semantic_test_helpers::{
        connect_semantic_search_channels, send_semantic_complete_response,
    };
    let home = ClaudeHome::new();
    home.write("a", 5, &["needle"]);
    let (_cache, environment) = caching_environment(&home);
    let rows = cached_rows(&home, &environment);
    let test_thread = std::thread::current().id();
    let gate =
        super::test_waits::ConnectionGate::closed(environment.cache_base().path(), move |thread| {
            thread.id() != test_thread
        });
    let mut app = super::claude_home::app_reading(rows, environment);
    app.list_search_mode = ListSearchMode::Semantic;
    let (_request_tx, _request_rx, response_tx) = connect_semantic_search_channels(&mut app);
    app.query = "needle".to_owned();
    app.dispatch_search();
    let generation = app.search_generation();
    let fallback = app
        .search_rx
        .recv_timeout(TEXTS_READ_LIMIT)
        .expect("the search worker answers the lexical fallback over the texts held");
    send_semantic_complete_response(
        &response_tx,
        generation,
        vec![0],
        std::collections::HashMap::new(),
        super::SemanticProgress::Complete,
    );
    app.receive_search_results();

    gate.open();
    let mut reruns = Vec::new();
    let deadline = std::time::Instant::now() + TEXTS_READ_LIMIT;
    while app.texts_held.sessions_without_texts.loading > 0 || app.texts_held.updates_applied == 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "the session's texts are read"
        );
        while let Ok(held) = app.texts_held_rx.try_recv() {
            app.texts_held = held;
        }
        reruns.extend(app.search_rx.try_iter().map(|response| response.generation));
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    reruns.extend(app.search_rx.try_iter().map(|response| response.generation));

    assert_eq!(fallback.generation, generation);
    assert!(
        app.semantic_search.without_turns.is_some(),
        "the semantic results are on screen"
    );
    assert!(
        !reruns.contains(&generation),
        "texts read after the semantic results show don't rerun the fallback: {reruns:?}"
    );
}

#[test]
fn session_being_written_counts_as_loading_not_unreadable() {
    let home = ClaudeHome::new();
    home.write("a", 5, &["needle"]);
    home.write("b", 10, &["other"]);
    let (_cache, environment) = caching_environment(&home);
    let rows = cached_rows(&home, &environment);
    crate::history::sqlite_cache::tests::mark_writing(
        row(&rows, "a").cached_session.as_ref().unwrap(),
    );
    let mut texts = SearchTextsInMemory::default();

    let launch = texts.keep_copies_of(&rows);
    for (copy, read) in launch.read() {
        texts.finish_read(copy, read);
    }

    assert_eq!(
        texts.sessions_without_texts(),
        SessionsWithoutTexts {
            loading: 1,
            unreadable: 0
        },
        "the session being written counts as loading, not unreadable"
    );
    assert_eq!(found(&rows, &texts, "other"), ["b"]);
}

#[test]
fn failed_read_counts_as_unreadable_keeps_its_cause_and_is_read_again_when_rows_change() {
    let home = ClaudeHome::new();
    home.write("a", 5, &["needle"]);
    let (_cache, environment) = caching_environment(&home);
    let rows = cached_rows(&home, &environment);
    let mut texts = SearchTextsInMemory::default();
    let copies = texts.keep_copies_of(&rows).copies().to_vec();
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

/// A list loading a home's sessions, newest first, whose search reads no
/// session's texts until [`Self::read_texts`].
struct ListWithTextReadsHeld {
    app: super::App,
    gate: super::test_waits::ConnectionGate,
    _cache: TemporaryCacheDirectory,
}

impl ListWithTextReadsHeld {
    fn loading(home: &ClaudeHome) -> Self {
        let (cache, environment) = caching_environment(home);
        let rows = cached_rows(home, &environment);
        let gate =
            super::test_waits::ConnectionGate::closed(environment.cache_base().path(), |thread| {
                thread
                    .name()
                    .is_some_and(|name| name.starts_with("text-read"))
            });
        let mut app = super::App::new_loading_with_options(
            super::ToolDisplayMode::Hidden,
            false,
            crate::config::KeyBindings::default(),
            super::ListScope::default(),
            super::TuiSearchOptions::default(),
            environment,
        );
        app.append_conversations(rows);
        Self {
            app,
            gate,
            _cache: cache,
        }
    }

    fn press(&mut self, code: crossterm::event::KeyCode) {
        self.app
            .handle_key(code, crossterm::event::KeyModifiers::NONE, 10);
    }

    /// End the load and apply the first results after it, while no
    /// session's texts are read.
    fn finish_loading(&mut self) {
        self.app.finish_loading();
        receive_until(&mut self.app, |app| !app.has_search_work_in_flight());
    }

    /// Let the search read the sessions' texts, and apply results until
    /// they list `rows` rows.
    fn read_texts(&mut self, rows: usize) {
        self.gate.open();
        receive_until(&mut self.app, |app| app.filtered().len() == rows);
    }
}

impl Drop for ListWithTextReadsHeld {
    /// Opens the gate before the app drops: the app's text reader would
    /// otherwise wait at it while a failed test drops the app.
    fn drop(&mut self) {
        self.gate.open();
    }
}

#[test]
fn selection_hidden_by_first_results_after_load_isnt_selected_when_texts_read_show_it() {
    use super::refresh_tests::{listed_names, selected_name};
    use crossterm::event::KeyCode;
    let home = ClaudeHome::new();
    home.write_in("-tmp-needle", "a", 10, &["hello"]);
    home.write("b", 5, &["hello", "needle"]);
    let mut list = ListWithTextReadsHeld::loading(&home);
    list.press(KeyCode::Down);
    assert_eq!(selected_name(&list.app), Some("b"));
    "needle"
        .chars()
        .for_each(|character| list.press(KeyCode::Char(character)));

    list.finish_loading();
    assert_eq!(
        listed_names(&list.app),
        ["a"],
        "the first results match a by its project alone"
    );
    list.read_texts(2);

    assert_eq!(listed_names(&list.app), ["a", "b"]);
    assert_eq!(
        selected_name(&list.app),
        Some("a"),
        "the first row stays selected after the rerun"
    );
}

#[test]
fn selection_shown_by_first_results_after_load_stays_selected_when_texts_read_add_rows() {
    use super::refresh_tests::{listed_names, selected_name};
    use crossterm::event::KeyCode;
    let home = ClaudeHome::new();
    home.write("c", 20, &["hello", "needle"]);
    home.write_in("-tmp-needle", "a", 10, &["hello"]);
    home.write_in("-tmp-needle", "b", 5, &["hello"]);
    let mut list = ListWithTextReadsHeld::loading(&home);
    list.press(KeyCode::Down);
    list.press(KeyCode::Down);
    assert_eq!(selected_name(&list.app), Some("b"));
    "needle"
        .chars()
        .for_each(|character| list.press(KeyCode::Char(character)));

    list.finish_loading();
    assert_eq!(
        listed_names(&list.app),
        ["a", "b"],
        "the first results match a and b by their project alone"
    );
    assert_eq!(selected_name(&list.app), Some("b"));
    list.read_texts(3);

    assert_ne!(
        listed_names(&list.app).first(),
        Some(&"b"),
        "b is not the first row after the rerun"
    );
    assert_eq!(
        selected_name(&list.app),
        Some("b"),
        "the selection stays on b after the rerun"
    );
}
