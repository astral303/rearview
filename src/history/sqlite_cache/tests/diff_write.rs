//! A write of a session the cache already holds compares the new rows with
//! the stored ones and writes only the differences. Each test writes an
//! earlier version of a parse, then the parse, and compares the stored
//! session with a fresh write of the parse in another database.

pub(super) mod resumed;

use super::*;
use crate::history::provider::contract_tests::AgentSession;
use crate::history::provider::transcript_reads::resume;
use crate::history::provider::{Resumption, TranscriptRead};
use crate::log_entry::{ContentBlock, LogEntry, UserContent};
use std::collections::{BTreeMap, BTreeSet};
use std::ops::ControlFlow;

/// The tables of a session's entry rows.
const ENTRY_TABLES: &[&str] = &[
    "entries",
    "messages",
    "threads",
    "transcripts",
    "parts",
    "result_items",
];

/// Counts each row inserted, updated or deleted in [`ENTRY_TABLES`] through
/// `connection`, cascades included, until [`take_row_writes`].
fn watch_row_writes(connection: &Connection) {
    connection
        .execute_batch("CREATE TEMP TABLE row_writes (row_table TEXT, operation TEXT)")
        .unwrap();
    for table in ENTRY_TABLES {
        for operation in ["INSERT", "UPDATE", "DELETE"] {
            connection
                .execute_batch(&format!(
                    "CREATE TEMP TRIGGER {table}_{operation} AFTER {operation} ON main.{table} \
                     BEGIN INSERT INTO row_writes VALUES ('{table}', '{operation}'); END"
                ))
                .unwrap();
        }
    }
}

/// The rows written since the last call, by table and operation.
fn take_row_writes(connection: &Connection) -> BTreeMap<(String, String), i64> {
    let writes = connection
        .prepare("SELECT row_table, operation, COUNT(*) FROM row_writes GROUP BY 1, 2")
        .unwrap()
        .query_map([], |row| Ok(((row.get(0)?, row.get(1)?), row.get(2)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    connection.execute("DELETE FROM row_writes", []).unwrap();
    writes
}

fn writes_of(writes: &BTreeMap<(String, String), i64>, table: &str, operation: &str) -> i64 {
    writes
        .get(&(table.to_owned(), operation.to_owned()))
        .copied()
        .unwrap_or_default()
}

pub(super) struct Fixture {
    source: Source,
    root: SessionRoot,
    stub: SessionStub,
    /// Keeps a fixture session built on disk alive.
    _session: Option<AgentSession>,
}

impl Fixture {
    pub(super) fn of(session: AgentSession) -> Self {
        Self {
            source: session.row.source,
            root: root_of(&session),
            stub: stub_of(&session, 1),
            _session: Some(session),
        }
    }

    /// The one session of the Codex home `home`, under the root Codex's
    /// storage gives the home, as its discovery finds it.
    fn of_codex_home(home: &Path) -> Self {
        let storage = Source::Codex.provider().storage();
        let home_variable = |name: &str| (name == "CODEX_HOME").then(|| home.into());
        let environment = crate::history::provider::RootEnvironment::new(
            &home_variable,
            Some(home.to_path_buf()),
            None,
        );
        let root = storage.roots_in(&environment).unwrap().remove(0);
        let mut stubs = storage
            .discover(&root, &crate::history::provider::walk::SystemLister)
            .unwrap()
            .stubs;
        assert_eq!(stubs.len(), 1, "the Codex home holds one session");
        Self {
            source: Source::Codex,
            root,
            stub: stubs.remove(0),
            _session: None,
        }
    }

    fn source(&self) -> Source {
        self.source
    }

    fn label(&self) -> &'static str {
        self.source().label()
    }

    fn storage(&self) -> SessionCache {
        storage_of(self.source())
    }

    fn record(&self) -> SessionRecord {
        parsed_record(self.source(), &self.root, &self.stub)
    }

    fn session_id(&self, cache: &CacheWriter) -> i64 {
        cache
            .connection
            .query_row(
                "SELECT id FROM sessions WHERE storage = ?1 AND cache_key = ?3 \
                 AND root = (SELECT id FROM roots WHERE key = ?2)",
                params![
                    self.storage().directory,
                    cache
                        .session_roots(&self.root)
                        .key()
                        .map(|key| key.as_str()),
                    self.stub.cache_key
                ],
                |row| row.get(0),
            )
            .unwrap()
    }

    fn listed(&self, cache: &CacheWriter) -> Conversation {
        let CatalogueSession::Listed(listed) = cache
            .catalogue_session(
                self.storage(),
                self.source(),
                &self.root,
                &self.stub.cache_key,
                false,
            )
            .unwrap()
            .unwrap()
        else {
            panic!("{}: listed fixture", self.label())
        };
        *listed
    }
}

fn entries_of(record: &mut SessionRecord) -> &mut TranscriptEntries {
    let Some(RecordEntries::Written(entries)) = &mut record.entries else {
        panic!("the record writes its entries")
    };
    entries
}

/// Each stored row of `session` in `table` from `query`, whose `?1` is the
/// session's id.
fn table_rows(cache: &CacheWriter, session: i64, table: &str, query: &str) -> Vec<String> {
    let mut statement = cache.connection.prepare(query).unwrap();
    let columns = statement.column_count();
    statement
        .query_map([session], |row| {
            let values = (0..columns)
                .map(|index| row.get::<_, rusqlite::types::Value>(index))
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(format!("{table} {values:?}"))
        })
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

/// `fixture`'s session as `cache` stores it, without surrogate ids: each
/// row of each table, then what its readers return.
fn stored_session(cache: &CacheWriter, fixture: &Fixture) -> Vec<String> {
    let session = fixture.session_id(cache);
    let thread = "(SELECT t.number || ' ' || COALESCE(tr.path, '') || ' ' || COALESCE(t.label, '') \
        || ' ' || COALESCE(t.nickname, '') || ' ' || COALESCE(t.launch_call, '') FROM threads t \
        LEFT JOIN transcripts tr ON tr.id = t.transcript WHERE t.id = m.thread)";
    let first_entry = |message: &str| {
        format!("(SELECT MIN(x.sequence) FROM entries x WHERE x.message = {message})")
    };
    let mut rows = Vec::new();
    for (table, query) in [
        (
            "sessions",
            "SELECT storage, source, (SELECT key FROM roots WHERE id = root), cache_key, \
             status, file_size, \
             mtime_secs, mtime_nsecs, session_id, session_id_key, handle, project_path, \
             project_name, cwd, timestamp_ms, preview_first, preview_last, summary, custom_title, \
             model, total_tokens, duration_minutes, message_count, assistant_messages, \
             is_missing_earlier_turns, entries_error FROM sessions WHERE id = ?1"
                .to_owned(),
        ),
        (
            "session_files",
            "SELECT position, kind, (SELECT key FROM roots WHERE id = root), path, key, \
             file_size, mtime_secs, mtime_nsecs \
             FROM session_files WHERE session = ?1 ORDER BY position"
                .to_owned(),
        ),
        (
            "transcript_reads",
            "SELECT position, (SELECT key FROM roots WHERE id = root), path, cursor, \
             stable_cursor, identity, rewrite_check, parser_state \
             FROM transcript_reads WHERE session = ?1 ORDER BY position"
                .to_owned(),
        ),
        (
            "parse_errors",
            "SELECT * FROM parse_errors WHERE session = ?1 ORDER BY position".to_owned(),
        ),
        (
            "malformed_lines",
            "SELECT * FROM malformed_lines WHERE session = ?1 ORDER BY position".to_owned(),
        ),
        (
            "parts outside entries",
            "SELECT part, kind, search_field, entry_sequence, message_ordinal, search_position, \
             file_position, text, text_codes, call_id, name, tool, json, result_shape FROM parts \
             WHERE session = ?1 AND entry IS NULL ORDER BY part"
                .to_owned(),
        ),
        (
            "transcripts",
            "SELECT (SELECT key FROM roots WHERE id = root) AS root_key, path FROM transcripts \
             WHERE session = ?1 ORDER BY root_key, path"
                .to_owned(),
        ),
        (
            "threads",
            "SELECT t.number, tr.path, t.label, t.nickname, t.role, t.agent_path, t.agent_id, \
             t.launch_call FROM threads t LEFT JOIN transcripts tr ON tr.id = t.transcript \
             WHERE t.session = ?1 ORDER BY 1, 2, 3, 4, 5, 6, 7, 8"
                .to_owned(),
        ),
        (
            "messages",
            format!(
                "SELECT {}, {thread}, m.ordinal, m.role, m.reply_id, m.first_ms, m.last_ms, \
                 m.model, m.agent, m.cwd, m.parent_tool_use_id, m.source_tool_use_id, \
                 m.report_has_text, m.report_task_id, m.report_launch_call, m.usage_input, \
                 m.usage_output, m.usage_cache_creation, m.usage_cache_read, m.digest \
                 FROM messages m WHERE m.session = ?1 ORDER BY 1",
                first_entry("m.id")
            ),
        ),
        (
            "entries",
            format!(
                "SELECT e.sequence, e.position, e.contributes, {}, tr.path, e.record_line, \
                 e.kind, e.timestamp_ms, e.content, e.text, e.text_codes, \
                 e.label, e.uuid, e.usage_input, e.usage_output, e.usage_cache_creation, \
                 e.usage_cache_read, e.level, e.duration_ms, e.parent_uuid, e.flag, e.digest \
                 FROM entries e LEFT JOIN transcripts tr ON tr.id = e.record_transcript \
                 WHERE e.session = ?1 ORDER BY e.sequence",
                first_entry("e.message")
            ),
        ),
        (
            "parts",
            "SELECT e.sequence, p.part, p.kind, p.search_field, p.entry_sequence, \
             p.message_ordinal, p.search_position, p.text, p.text_codes, p.call_id, p.name, \
             p.tool, p.json, p.result_shape FROM parts p \
             JOIN entries e ON e.id = p.entry WHERE e.session = ?1 ORDER BY 1, 2"
                .to_owned(),
        ),
        (
            "result_items",
            "SELECT e.sequence, p.part, i.position, i.kind, i.text, i.text_codes, \
             i.item_type FROM result_items i JOIN parts p ON p.id = i.part \
             JOIN entries e ON e.id = p.entry WHERE e.session = ?1 ORDER BY 1, 2, 3"
                .to_owned(),
        ),
    ] {
        rows.extend(table_rows(cache, session, table, &query));
    }
    let (count, first, last): (i64, Option<i64>, Option<i64>) = cache
        .connection
        .query_row(
            "SELECT COUNT(*), MIN(sequence), MAX(sequence) FROM entries WHERE session = ?1",
            [session],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert!(
        count == 0 || (first == Some(0) && last == Some(count - 1)),
        "{}: the entries' sequences run from 0 without gaps: {count} entries, {first:?} to {last:?}",
        fixture.label()
    );
    let entries = cache
        .entries(fixture.storage(), &fixture.root, &fixture.stub.cache_key)
        .unwrap()
        .unwrap();
    rows.push(format!("read entries {:?}", entries.entries));
    for line in &entries.malformed_lines {
        rows.push(format!(
            "read malformed line {} {:?}",
            line.line_number,
            line.detail.as_ref().map(|detail| (
                &detail.line_content,
                &detail.error_message,
                &detail.context_before,
                &detail.context_after
            ))
        ));
    }
    let listed = fixture.listed(cache);
    let copy = listed.cached_session.clone().unwrap();
    let mut texts = Vec::new();
    copy.read_held_copy(|held| {
        held.visit_search_texts(
            SearchedFields::Agent,
            &crate::time_filter::TimeFilter::default(),
            |text| {
                let placed = format!("{:?} {:?} {}", text.order, text.ordinal, text.text);
                texts.push((text.order, placed));
                ControlFlow::Continue(())
            },
        )
    })
    .unwrap()
    .unwrap();
    texts.sort();
    let texts = texts.into_iter().map(|(_, text)| text).collect::<Vec<_>>();
    rows.push(format!("search texts {texts:?}"));
    let content = listed.load_held_content().unwrap().unwrap().row;
    rows.push(format!(
        "content {:?} {:?} {:?} {:?} {:?} {:?} {:?}",
        content.full_text,
        content.agent_search_text,
        content.thinking_and_tool_call_text,
        content.semantic_turns,
        content.message_text_starts,
        content.message_times,
        content.message_count
    ));
    rows
}

/// Asserts that `cache` stores `fixture`'s session as a fresh write of
/// `record` in another database does.
fn assert_matches_fresh_write(cache: &CacheWriter, fixture: &Fixture, record: &SessionRecord) {
    let base = tempfile::tempdir().unwrap();
    let mut fresh = admitted_cache(base.path());
    fresh.admit_storage(fixture.storage()).unwrap();
    fresh.write(record).unwrap();
    let expected = stored_session(&fresh, fixture);
    let actual = stored_session(cache, fixture);
    for (actual, expected) in actual.iter().zip(&expected) {
        assert_eq!(
            actual,
            expected,
            "{}: the session after the write matches a fresh write",
            fixture.label()
        );
    }
    assert_eq!(
        actual.len(),
        expected.len(),
        "{}: the session after the write holds the rows a fresh write holds",
        fixture.label()
    );
}

/// A cache holding `before`, `fixture`'s session as an earlier write left
/// it, its writes watched from then on.
fn cache_holding(base: &Path, fixture: &Fixture, before: &SessionRecord) -> OwnedCache {
    let mut cache = admitted_cache(base);
    cache.admit_storage(fixture.storage()).unwrap();
    cache.write(before).unwrap();
    watch_row_writes(&cache.connection);
    cache
}

fn ids(cache: &CacheWriter, table: &str) -> BTreeSet<i64> {
    cache
        .connection
        .prepare(&format!("SELECT id FROM {table}"))
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

/// The sub-agent transcript of `record` whose entries the parse splices in
/// before a later entry of the session's own, with the index of its last
/// such entry.
fn subagent_spliced_mid_session(record: &mut SessionRecord) -> Option<(PathBuf, usize)> {
    let subagents: Vec<PathBuf> = record
        .catalogue
        .files
        .iter()
        .filter(|file| file.kind == "subagent")
        .map(|file| file.path.clone())
        .collect();
    let entries = &entries_of(record).entries;
    let last_own = entries
        .iter()
        .rposition(|(line, _)| line.transcript.is_none())?;
    subagents.into_iter().find_map(|subagent| {
        let last = entries[..last_own]
            .iter()
            .rposition(|(line, _)| line.transcript.as_deref() == Some(subagent.as_path()))?;
        Some((subagent, last))
    })
}

/// `record` without the sub-agent transcript at `path`: its entries, its
/// file and its metadata parts.
fn without_subagent(record: &mut SessionRecord, path: &Path) {
    entries_of(record)
        .entries
        .retain(|(line, _)| line.transcript.as_deref() != Some(path));
    let position = record
        .catalogue
        .files
        .iter()
        .position(|file| file.path == path)
        .unwrap();
    record.catalogue.files.remove(position);
    record
        .subagent_metadata
        .retain(|part| part.file_position != position);
    for part in &mut record.subagent_metadata {
        if part.file_position > position {
            part.file_position -= 1;
        }
    }
}

/// Removes up to `count` of the last entries of the session's own
/// transcript from `record`; returns how many it removed.
fn without_last_own_entries(record: &mut SessionRecord, count: usize) -> usize {
    let entries = &mut entries_of(record).entries;
    let mut removed = 0;
    while removed < count {
        let Some(last) = entries
            .iter()
            .rposition(|(line, _)| line.transcript.is_none())
        else {
            break;
        };
        entries.remove(last);
        removed += 1;
    }
    removed
}

/// The `transcript_reads` rows of `session`, in position order, each with
/// its path joined with `root`, the root every file of the session sits
/// under.
fn stored_reads(cache: &CacheWriter, session: i64, root: &Path) -> Vec<(PathBuf, TranscriptRead)> {
    cache
        .connection
        .prepare(
            "SELECT path, cursor, stable_cursor, identity, rewrite_check, parser_state, \
             stable_line FROM transcript_reads WHERE session = ?1 ORDER BY position",
        )
        .unwrap()
        .query_map([session], |row| {
            Ok((
                root.join(row.get::<_, String>(0)?),
                TranscriptRead {
                    cursor: row.get(1)?,
                    stable_cursor: row.get(2)?,
                    identity: row.get(3)?,
                    rewrite_check: row.get(4)?,
                    parser_state: row.get(5)?,
                    stable_line: row.get::<_, Option<i64>>(6)?.map(|line| line as usize),
                },
            ))
        })
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

/// The `session_files` paths of `session`, in position order, each joined
/// with `root`, the root every file of the session sits under.
fn stored_file_paths(cache: &CacheWriter, session: i64, root: &Path) -> Vec<PathBuf> {
    cache
        .connection
        .prepare("SELECT path FROM session_files WHERE session = ?1 ORDER BY position")
        .unwrap()
        .query_map([session], |row| Ok(root.join(row.get::<_, String>(0)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

#[test]
fn write_stores_read_of_every_session_file_that_resumes_through_its_agents_reader() {
    for session in every_record_sessions() {
        let fixture = Fixture::of(session);
        let mut before = fixture.record();
        for file in &mut before.catalogue.files {
            file.read = TranscriptRead::full_read();
        }
        let base = tempfile::tempdir().unwrap();
        let mut cache = cache_holding(base.path(), &fixture, &before);

        let record = fixture.record();
        cache.write(&record).unwrap();

        let session = fixture.session_id(&cache);
        let reads = stored_reads(&cache, session, &fixture.root.path);
        assert_eq!(
            reads
                .iter()
                .map(|(path, _)| path.clone())
                .collect::<Vec<_>>(),
            stored_file_paths(&cache, session, &fixture.root.path),
            "{}: each session file has its read, at its position",
            fixture.label()
        );
        let reader = fixture.source().provider().storage().transcript_reader();
        for (path, read) in &reads {
            let resumption = resume(reader, crate::history::FOR_FIXTURES, path, read).unwrap();
            assert!(
                matches!(resumption, Resumption::Window(_)),
                "{}: {} resumes through its agent's reader: {resumption:?}",
                fixture.label(),
                path.display()
            );
        }
        assert_matches_fresh_write(&cache, &fixture, &record);
    }
}

/// A Codex session whose rollouts before an edit Codex archived, its own and
/// its sub-agent's: a write of its parse over a copy missing its last entry
/// matches a fresh write, with files under the sessions tree and under the
/// archived tree beside it.
#[test]
fn later_write_of_session_with_files_under_two_roots_matches_fresh_write() {
    let home = tempfile::tempdir().unwrap();
    crate::history::format::codex::test_support::write_session_with_archived_bases(
        home.path(),
        "019f0000-0000-7000-8000-00000000000a",
        "019f0000-0000-7000-8000-00000000000b",
    );
    let fixture = Fixture::of_codex_home(home.path());
    let mut earlier = fixture.record();
    entries_of(&mut earlier).entries.pop();
    let base = tempfile::tempdir().unwrap();
    let mut cache = cache_holding(base.path(), &fixture, &earlier);

    let record = fixture.record();
    cache.write(&record).unwrap();

    let roots: i64 = cache
        .connection
        .query_row(
            "SELECT COUNT(DISTINCT root) FROM session_files WHERE session = ?1",
            [fixture.session_id(&cache)],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(roots, 2, "the session's files sit under both trees");
    assert_matches_fresh_write(&cache, &fixture, &record);
}

#[test]
fn parse_equal_to_stored_session_writes_no_entry_rows_for_every_agent() {
    for session in every_record_sessions() {
        let fixture = Fixture::of(session);
        let base = tempfile::tempdir().unwrap();
        let mut cache = cache_holding(base.path(), &fixture, &fixture.record());
        let session = fixture.session_id(&cache);

        let record = fixture.record();
        cache.write(&record).unwrap();

        assert_eq!(
            take_row_writes(&cache.connection),
            BTreeMap::new(),
            "{}: a parse equal to the stored session writes no entry row",
            fixture.label()
        );
        assert_eq!(
            fixture.session_id(&cache),
            session,
            "{}: the session row keeps its id",
            fixture.label()
        );
        assert_matches_fresh_write(&cache, &fixture, &record);
    }
}

/// A title the agent keeps outside the transcript is a field of the session
/// row: a rename, a second rename and the agent's files restored don't write
/// any entry row. A fixture without a title has one added, then removed; a
/// fixture with one has it changed.
#[test]
fn title_outside_transcript_renamed_doesnt_write_entry_rows_for_every_agent() {
    use super::search_texts::{rename, writable};
    use crate::history::provider::contract_tests::copy_tree;
    let mut renamed_outside = Vec::new();
    for session in every_record_sessions() {
        let (home, copy) = writable(session);
        let home = home.expect("the copy has a home");
        let saved = tempfile::tempdir().unwrap();
        copy_tree(home.path(), saved.path());
        let source = copy.row.source;
        let path = copy.row.path.clone();
        let mut fixture = Fixture::of(copy);
        let mut original = fixture.record();
        let base = tempfile::tempdir().unwrap();
        let mut cache = cache_holding(base.path(), &fixture, &original);
        let session_id = fixture.session_id(&cache);
        let original_title = fixture.listed(&cache).custom_title;

        let restamp = |fixture: &mut Fixture| {
            if source == Source::OpenCode {
                // The stub carries the database's stamp, and a rename moves it.
                fixture.stub.fingerprint = opencode_fingerprint(&fixture.root, &path);
            }
        };
        rename(source, &path, "Renamed by hand");
        restamp(&mut fixture);
        let transcript_records = |record: &mut SessionRecord| {
            entries_of(record)
                .entries
                .iter()
                .filter(|(line, entry)| !crate::history::parser::is_leading_title(line, entry))
                .count()
        };
        if transcript_records(&mut fixture.record()) != transcript_records(&mut original) {
            // The agent records a rename in the transcript.
            continue;
        }
        renamed_outside.push(source);
        let restore = || {
            std::fs::remove_dir_all(home.path()).unwrap();
            copy_tree(saved.path(), home.path());
        };
        /// A case, the title the session row names after it, and the change
        /// to the agent's files that makes it.
        type Step<'a> = (&'a str, Option<String>, &'a dyn Fn());
        let steps: [Step<'_>; 3] = [
            ("the rename", Some("Renamed by hand".to_owned()), &|| {}),
            ("a second rename", Some("Renamed again".to_owned()), &|| {
                rename(source, &path, "Renamed again");
            }),
            ("the files restored", original_title, &restore),
        ];
        for (case, expected, change) in steps {
            change();
            restamp(&mut fixture);
            let record = fixture.record();
            cache.write(&record).unwrap();
            assert_eq!(
                take_row_writes(&cache.connection),
                BTreeMap::new(),
                "{}: {case} doesn't write any entry row",
                fixture.label()
            );
            assert_eq!(
                fixture.listed(&cache).custom_title,
                expected,
                "{}: {case} names the session row's title",
                fixture.label()
            );
            assert_matches_fresh_write(&cache, &fixture, &record);
        }
        assert_eq!(
            fixture.session_id(&cache),
            session_id,
            "{}: the session row keeps its id",
            fixture.label()
        );
    }
    assert_eq!(
        renamed_outside,
        [Source::Codex, Source::OpenCode, Source::Kimi, Source::Grok],
        "the agents keeping a renamed title outside the transcript: Claude, Pi and OMP append \
         the rename to it"
    );
}

#[test]
fn entries_appended_to_own_transcript_are_inserted_for_every_agent() {
    for session in every_record_sessions() {
        let fixture = Fixture::of(session);
        let mut before = fixture.record();
        let appended = without_last_own_entries(&mut before, 2);
        assert!(
            appended > 0,
            "{}: the fixture has entries of its own",
            fixture.label()
        );
        let base = tempfile::tempdir().unwrap();
        let mut cache = cache_holding(base.path(), &fixture, &before);
        let stored = ids(&cache, "entries");

        let record = fixture.record();
        cache.write(&record).unwrap();

        let writes = take_row_writes(&cache.connection);
        assert_eq!(
            writes_of(&writes, "entries", "INSERT"),
            appended as i64,
            "{}: the write inserts the appended entries: {writes:?}",
            fixture.label()
        );
        assert_eq!(
            writes_of(&writes, "entries", "DELETE"),
            0,
            "{}: the write deletes no entry: {writes:?}",
            fixture.label()
        );
        assert!(
            stored.is_subset(&ids(&cache, "entries")),
            "{}: the earlier entries keep their ids",
            fixture.label()
        );
        assert_matches_fresh_write(&cache, &fixture, &record);
    }
}

/// The parts whose session is not their entry's.
fn parts_outside_their_entrys_session(cache: &CacheWriter) -> i64 {
    cache
        .connection
        .query_row(
            "SELECT COUNT(*) FROM parts p JOIN entries e ON e.id = p.entry \
             WHERE p.session != e.session",
            [],
            |row| row.get(0),
        )
        .unwrap()
}

#[test]
fn appended_entries_parts_carry_their_session_for_every_agent() {
    for session in every_record_sessions() {
        let fixture = Fixture::of(session);
        let mut before = fixture.record();
        without_last_own_entries(&mut before, 2);
        let base = tempfile::tempdir().unwrap();
        let mut cache = admitted_cache(base.path());
        cache.admit_storage(fixture.storage()).unwrap();
        let other = SessionStub {
            cache_key: "other".to_owned(),
            ..fixture.stub.clone()
        };
        cache
            .write(&parsed_record(fixture.source(), &fixture.root, &other))
            .unwrap();
        cache.write(&before).unwrap();
        watch_row_writes(&cache.connection);

        cache.write(&fixture.record()).unwrap();

        let writes = take_row_writes(&cache.connection);
        assert!(
            writes_of(&writes, "parts", "INSERT") > 0,
            "{}: the write inserts the appended entries' parts: {writes:?}",
            fixture.label()
        );
        assert_eq!(
            parts_outside_their_entrys_session(&cache),
            0,
            "{}: each part carries its entry's session, of the two sessions held",
            fixture.label()
        );
    }
}

#[test]
fn removed_session_deletes_its_parts_and_result_items() {
    for session in every_record_sessions() {
        let fixture = Fixture::of(session);
        let base = tempfile::tempdir().unwrap();
        let mut cache = cache_holding(base.path(), &fixture, &fixture.record());

        cache
            .remove(fixture.storage(), &fixture.root, &[CACHE_KEY.to_owned()])
            .unwrap();

        let writes = take_row_writes(&cache.connection);
        assert!(
            writes_of(&writes, "parts", "DELETE") > 0,
            "{}: the session held parts: {writes:?}",
            fixture.label()
        );
        for table in ["parts", "result_items"] {
            let left: i64 = cache
                .connection
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(
                left,
                0,
                "{}: the removed session leaves no {table} row",
                fixture.label()
            );
        }
    }
}

#[test]
fn subagent_turn_spliced_mid_session_moves_later_entries_in_place() {
    let mut tested = Vec::new();
    for session in every_record_sessions() {
        let fixture = Fixture::of(session);
        let mut before = fixture.record();
        let Some((_, last)) = subagent_spliced_mid_session(&mut before) else {
            continue;
        };
        let entries = &mut entries_of(&mut before).entries;
        let line = entries[last].0.clone();
        let first = entries
            .iter()
            .position(|(record, _)| *record == line)
            .unwrap();
        let count = entries.len();
        entries.retain(|(record, _)| *record != line);
        let spliced = count - entries.len();
        let later = entries.len() - first;
        let base = tempfile::tempdir().unwrap();
        let mut cache = cache_holding(base.path(), &fixture, &before);
        let stored = ids(&cache, "entries");
        let stored_messages = ids(&cache, "messages");

        let record = fixture.record();
        cache.write(&record).unwrap();

        let writes = take_row_writes(&cache.connection);
        assert_eq!(
            writes_of(&writes, "entries", "INSERT"),
            spliced as i64,
            "{}: the write inserts the spliced turn's entries: {writes:?}",
            fixture.label()
        );
        assert_eq!(
            writes_of(&writes, "entries", "UPDATE"),
            later as i64,
            "{}: each later entry moves in place in one update: {writes:?}",
            fixture.label()
        );
        assert!(
            stored.is_subset(&ids(&cache, "entries")),
            "{}: the later entries keep their ids",
            fixture.label()
        );
        assert!(
            stored_messages.is_subset(&ids(&cache, "messages")),
            "{}: the later messages keep their ids",
            fixture.label()
        );
        assert_matches_fresh_write(&cache, &fixture, &record);
        tested.push(fixture.source());
    }
    for source in [Source::Codex, Source::Claude] {
        assert!(
            tested.contains(&source),
            "{} splices a sub-agent turn mid-session",
            source.label()
        );
    }
}

#[test]
fn new_subagent_thread_is_inserted_and_removed_one_deleted() {
    let mut tested = Vec::new();
    for session in every_record_sessions() {
        let fixture = Fixture::of(session);
        let mut without = fixture.record();
        let Some((subagent, _)) = subagent_spliced_mid_session(&mut without) else {
            continue;
        };
        without_subagent(&mut without, &subagent);
        let record = fixture.record();

        let base = tempfile::tempdir().unwrap();
        let mut cache = cache_holding(base.path(), &fixture, &without);
        let stored = ids(&cache, "entries");
        cache.write(&record).unwrap();
        let writes = take_row_writes(&cache.connection);
        assert!(
            writes_of(&writes, "threads", "INSERT") > 0
                && writes_of(&writes, "transcripts", "INSERT") > 0,
            "{}: the write inserts the new sub-agent's thread and transcript: {writes:?}",
            fixture.label()
        );
        assert!(
            stored.is_subset(&ids(&cache, "entries")),
            "{}: the other entries keep their ids",
            fixture.label()
        );
        assert_matches_fresh_write(&cache, &fixture, &record);

        let base = tempfile::tempdir().unwrap();
        let mut cache = cache_holding(base.path(), &fixture, &record);
        cache.write(&without).unwrap();
        let writes = take_row_writes(&cache.connection);
        assert!(
            writes_of(&writes, "threads", "DELETE") > 0
                && writes_of(&writes, "transcripts", "DELETE") > 0
                && writes_of(&writes, "entries", "INSERT") == 0,
            "{}: the write deletes the removed sub-agent's rows and inserts none: {writes:?}",
            fixture.label()
        );
        assert_matches_fresh_write(&cache, &fixture, &without);
        tested.push(fixture.source());
    }
    for source in [Source::Codex, Source::Claude] {
        assert!(
            tested.contains(&source),
            "{} has a sub-agent",
            source.label()
        );
    }
}

#[test]
fn entries_that_change_order_move_in_place_for_every_agent() {
    for session in every_record_sessions() {
        let fixture = Fixture::of(session);
        let mut before = fixture.record();
        let entries = &mut entries_of(&mut before).entries;
        let own: Vec<usize> = (0..entries.len())
            .filter(|&index| entries[index].0.transcript.is_none())
            .collect();
        let Some(pair) = own
            .windows(2)
            .rev()
            .find(|pair| entries[pair[0]].0.line != entries[pair[1]].0.line)
        else {
            continue;
        };
        entries.swap(pair[0], pair[1]);
        let base = tempfile::tempdir().unwrap();
        let mut cache = cache_holding(base.path(), &fixture, &before);
        let stored = ids(&cache, "entries");

        let record = fixture.record();
        cache.write(&record).unwrap();

        assert_eq!(
            ids(&cache, "entries"),
            stored,
            "{}: the entries that changed order keep their ids",
            fixture.label()
        );
        assert_matches_fresh_write(&cache, &fixture, &record);
    }
}

#[test]
fn removed_entries_are_deleted_for_every_agent() {
    for session in every_record_sessions() {
        let fixture = Fixture::of(session);
        let mut after = fixture.record();
        let removed = without_last_own_entries(&mut after, 2);
        let base = tempfile::tempdir().unwrap();
        let mut cache = cache_holding(base.path(), &fixture, &fixture.record());

        cache.write(&after).unwrap();

        let writes = take_row_writes(&cache.connection);
        assert_eq!(
            (
                writes_of(&writes, "entries", "DELETE"),
                writes_of(&writes, "entries", "INSERT")
            ),
            (removed as i64, 0),
            "{}: the write deletes the removed entries and inserts none: {writes:?}",
            fixture.label()
        );
        assert_matches_fresh_write(&cache, &fixture, &after);
    }
}

fn blocks_of(entry: &mut LogEntry) -> Option<&mut Vec<ContentBlock>> {
    match entry {
        LogEntry::User {
            message:
                crate::log_entry::UserMessage {
                    content: UserContent::Blocks(blocks),
                    ..
                },
            ..
        } => Some(blocks),
        LogEntry::Assistant { message, .. } => Some(&mut message.content),
        _ => None,
    }
}

/// Replaces the content of the last result in `record`, or of its last text
/// when it holds no result, with a stand-in; returns the entry's index.
fn with_stand_in(record: &mut SessionRecord) -> Option<usize> {
    let entries = &mut entries_of(record).entries;
    let is_result = |block: &ContentBlock| matches!(block, ContentBlock::ToolResult { .. });
    let is_text = |block: &ContentBlock| matches!(block, ContentBlock::Text { .. });
    let index = entries
        .iter_mut()
        .rposition(|(_, entry)| blocks_of(entry).is_some_and(|blocks| blocks.iter().any(is_result)))
        .or_else(|| {
            entries.iter_mut().rposition(|(_, entry)| {
                blocks_of(entry).is_some_and(|blocks| blocks.iter().any(is_text))
            })
        })?;
    for block in blocks_of(&mut entries[index].1)? {
        match block {
            ContentBlock::ToolResult { content, .. } => {
                *content = Some(serde_json::json!("STAND_IN result"));
            }
            ContentBlock::Text { text } => *text = "STAND_IN text".to_owned(),
            _ => {}
        }
    }
    Some(index)
}

#[test]
fn changed_entry_is_updated_in_place_for_every_agent() {
    let mut tested = Vec::new();
    for session in every_record_sessions() {
        let fixture = Fixture::of(session);
        let mut before = fixture.record();
        let Some(changed) = with_stand_in(&mut before) else {
            continue;
        };
        let base = tempfile::tempdir().unwrap();
        let mut cache = cache_holding(base.path(), &fixture, &before);
        let stored = ids(&cache, "entries");
        let changed_id: i64 = cache
            .connection
            .query_row(
                "SELECT id FROM entries WHERE sequence = ?1",
                [changed as i64],
                |row| row.get(0),
            )
            .unwrap();

        let record = fixture.record();
        cache.write(&record).unwrap();

        let writes = take_row_writes(&cache.connection);
        assert_eq!(
            (
                writes_of(&writes, "entries", "UPDATE"),
                writes_of(&writes, "entries", "INSERT"),
                writes_of(&writes, "entries", "DELETE")
            ),
            (1, 0, 0),
            "{}: the write updates the changed entry alone: {writes:?}",
            fixture.label()
        );
        assert!(
            writes_of(&writes, "parts", "UPDATE") > 0,
            "{}: the write updates the changed entry's changed parts: {writes:?}",
            fixture.label()
        );
        assert_eq!(
            ids(&cache, "entries"),
            stored,
            "{}: the changed entry keeps its id {changed_id}",
            fixture.label()
        );
        assert_matches_fresh_write(&cache, &fixture, &record);
        tested.push(fixture.source());
    }
    for source in [Source::Codex, Source::Claude] {
        assert!(
            tested.contains(&source),
            "{} has a changed entry",
            source.label()
        );
    }
}

/// Replaces the first text block of the last entry of `record` holding a
/// text block and another block with a stand-in; returns the entry's index.
fn with_one_text_stand_in(record: &mut SessionRecord) -> Option<usize> {
    let entries = &mut entries_of(record).entries;
    let index = entries.iter_mut().rposition(|(_, entry)| {
        blocks_of(entry).is_some_and(|blocks| {
            blocks.len() > 1
                && blocks
                    .iter()
                    .any(|block| matches!(block, ContentBlock::Text { .. }))
        })
    })?;
    let blocks = blocks_of(&mut entries[index].1)?;
    let Some(ContentBlock::Text { text }) = blocks
        .iter_mut()
        .find(|block| matches!(block, ContentBlock::Text { .. }))
    else {
        unreachable!("the entry holds a text block")
    };
    *text = "STAND_IN text".to_owned();
    Some(index)
}

/// The id and digest of each part of the entry at `sequence`.
fn parts_of_entry_at(cache: &CacheWriter, sequence: usize) -> Vec<(i64, i64)> {
    cache
        .connection
        .prepare(
            "SELECT p.id, p.digest FROM parts p JOIN entries e ON e.id = p.entry \
             WHERE e.sequence = ?1 ORDER BY p.part",
        )
        .unwrap()
        .query_map([sequence as i64], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

#[test]
fn entry_with_one_changed_part_updates_that_part_alone_for_every_agent() {
    let mut tested = Vec::new();
    for session in every_record_sessions() {
        let fixture = Fixture::of(session);
        let mut before = fixture.record();
        let Some(changed) = with_one_text_stand_in(&mut before) else {
            continue;
        };
        let base = tempfile::tempdir().unwrap();
        let mut cache = cache_holding(base.path(), &fixture, &before);
        let parts_before = parts_of_entry_at(&cache, changed);

        let record = fixture.record();
        cache.write(&record).unwrap();

        let writes = take_row_writes(&cache.connection);
        let parts_after = parts_of_entry_at(&cache, changed);
        let kept = parts_before
            .iter()
            .zip(&parts_after)
            .filter(|(before, after)| before == after)
            .count();
        assert_eq!(
            (
                writes_of(&writes, "entries", "UPDATE"),
                writes_of(&writes, "parts", "UPDATE"),
                writes_of(&writes, "parts", "INSERT") + writes_of(&writes, "parts", "DELETE"),
                writes_of(&writes, "result_items", "INSERT")
                    + writes_of(&writes, "result_items", "DELETE"),
            ),
            (1, 1, 0, 0),
            "{}: the write updates the entry's digest and its changed part alone: {writes:?}",
            fixture.label()
        );
        assert_eq!(
            (
                parts_after.len(),
                kept,
                parts_before.iter().map(|(id, _)| id).collect::<Vec<_>>()
            ),
            (
                parts_before.len(),
                parts_before.len() - 1,
                parts_after.iter().map(|(id, _)| id).collect::<Vec<_>>()
            ),
            "{}: every part keeps its id, and every unchanged part its row",
            fixture.label()
        );
        assert_matches_fresh_write(&cache, &fixture, &record);
        tested.push(fixture.source());
    }
    assert!(
        tested.contains(&Source::OpenCode),
        "OpenCode has an entry with a text and another block: {tested:?}"
    );
}

/// Removes the last block of the last entry of `record` holding a call and
/// another block; returns the entry's index.
fn without_last_block_of_entry_with_call(record: &mut SessionRecord) -> Option<usize> {
    let entries = &mut entries_of(record).entries;
    let is_call = |block: &ContentBlock| matches!(block, ContentBlock::ToolUse { .. });
    let index = entries.iter_mut().rposition(|(_, entry)| {
        blocks_of(entry).is_some_and(|blocks| blocks.len() > 1 && blocks.iter().any(is_call))
    })?;
    blocks_of(&mut entries[index].1)?.pop();
    Some(index)
}

#[test]
fn block_appended_to_entry_with_call_inserts_its_parts_alone_for_every_agent() {
    let mut tested = Vec::new();
    for session in every_record_sessions() {
        let fixture = Fixture::of(session);
        let mut before = fixture.record();
        let Some(changed) = without_last_block_of_entry_with_call(&mut before) else {
            continue;
        };
        let base = tempfile::tempdir().unwrap();
        let mut cache = cache_holding(base.path(), &fixture, &before);
        let parts_before = parts_of_entry_at(&cache, changed);

        let record = fixture.record();
        cache.write(&record).unwrap();

        let writes = take_row_writes(&cache.connection);
        let parts_after = parts_of_entry_at(&cache, changed);
        let added = parts_after.len() - parts_before.len();
        assert_eq!(
            (
                writes_of(&writes, "entries", "UPDATE"),
                writes_of(&writes, "parts", "INSERT"),
                writes_of(&writes, "parts", "UPDATE") + writes_of(&writes, "parts", "DELETE"),
                writes_of(&writes, "result_items", "DELETE"),
            ),
            (1, added as i64, 0, 0),
            "{}: the write updates the entry's digest and inserts the new block's parts \
             alone: {writes:?}",
            fixture.label()
        );
        let kept: BTreeSet<_> = parts_after.iter().collect();
        assert!(
            added > 0 && parts_before.iter().all(|part| kept.contains(part)),
            "{}: every earlier part keeps its id and its row: {parts_before:?} then \
             {parts_after:?}",
            fixture.label()
        );
        assert_matches_fresh_write(&cache, &fixture, &record);
        tested.push(fixture.source());
    }
    for source in [
        Source::Claude,
        Source::Codex,
        Source::Kimi,
        Source::Pi,
        Source::Omp,
    ] {
        assert!(
            tested.contains(&source),
            "{} has an entry holding a call and another block: {tested:?}",
            source.label()
        );
    }
}

/// Empties the first text block of the last entry of `record` holding two
/// texts that aren't empty, so search reads one text fewer before the
/// second; returns the entry's index.
fn with_first_of_two_texts_emptied(record: &mut SessionRecord) -> Option<usize> {
    let entries = &mut entries_of(record).entries;
    let is_text = |block: &ContentBlock| matches!(block, ContentBlock::Text { text } if !text.trim().is_empty());
    let index = entries.iter_mut().rposition(|(_, entry)| {
        blocks_of(entry)
            .is_some_and(|blocks| blocks.iter().filter(|block| is_text(block)).count() > 1)
    })?;
    let first = blocks_of(&mut entries[index].1)?
        .iter_mut()
        .find(|block| is_text(block))?;
    *first = ContentBlock::Text {
        text: String::new(),
    };
    Some(index)
}

#[test]
fn text_filled_in_before_another_moves_the_later_texts_search_position_alone_for_every_agent() {
    let mut tested = Vec::new();
    for session in every_record_sessions() {
        let fixture = Fixture::of(session);
        let mut before = fixture.record();
        let Some(changed) = with_first_of_two_texts_emptied(&mut before) else {
            continue;
        };
        let base = tempfile::tempdir().unwrap();
        let mut cache = cache_holding(base.path(), &fixture, &before);
        let parts_before = parts_of_entry_at(&cache, changed);

        let record = fixture.record();
        cache.write(&record).unwrap();

        let writes = take_row_writes(&cache.connection);
        let parts_after = parts_of_entry_at(&cache, changed);
        let kept = parts_before
            .iter()
            .filter(|part| parts_after.contains(part))
            .count();
        assert_eq!(
            (
                writes_of(&writes, "parts", "INSERT") + writes_of(&writes, "parts", "DELETE"),
                parts_after.len(),
                kept,
            ),
            (0, parts_before.len(), parts_before.len() - 1),
            "{}: the filled-in text's part alone changes its row: {writes:?}",
            fixture.label()
        );
        assert!(
            writes_of(&writes, "parts", "UPDATE") > 1,
            "{}: the write moves the later text's search position: {writes:?}",
            fixture.label()
        );
        assert_matches_fresh_write(&cache, &fixture, &record);
        tested.push(fixture.source());
    }
    assert!(
        tested.contains(&Source::OpenCode),
        "OpenCode has an entry holding two texts: {tested:?}"
    );
}

/// Removes the text blocks of the first entry of `record` holding one;
/// returns the entry's index.
fn without_first_text(record: &mut SessionRecord) -> Option<usize> {
    let entries = &mut entries_of(record).entries;
    let is_text = |block: &ContentBlock| matches!(block, ContentBlock::Text { .. });
    let index = entries
        .iter_mut()
        .position(|(_, entry)| blocks_of(entry).is_some_and(|blocks| blocks.iter().any(is_text)))?;
    blocks_of(&mut entries[index].1)?.retain(|block| !is_text(block));
    Some(index)
}

/// Each stored message's ordinal, by its id.
fn message_ordinals(cache: &CacheWriter) -> BTreeMap<i64, Option<i64>> {
    cache
        .connection
        .prepare("SELECT id, ordinal FROM messages")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

#[test]
fn message_renumbered_in_place_moves_its_texts_search_place_for_every_agent() {
    let mut tested = Vec::new();
    for session in every_record_sessions() {
        let fixture = Fixture::of(session);
        let mut before = fixture.record();
        if without_first_text(&mut before).is_none() {
            continue;
        }
        let base = tempfile::tempdir().unwrap();
        let mut cache = cache_holding(base.path(), &fixture, &before);
        let stored = message_ordinals(&cache);

        let record = fixture.record();
        cache.write(&record).unwrap();

        let renumbered = message_ordinals(&cache)
            .iter()
            .any(|(id, ordinal)| stored.get(id).is_some_and(|stored| stored != ordinal));
        if !renumbered {
            continue;
        }
        assert_matches_fresh_write(&cache, &fixture, &record);
        tested.push(fixture.source());
    }
    assert!(
        !tested.is_empty(),
        "a message keeps its row while its ordinal changes for some agent"
    );
}

#[test]
fn reader_holding_earlier_generation_sees_it_change() {
    let session = every_record_sessions()
        .into_iter()
        .find(|session| session.row.source == Source::Claude)
        .unwrap();
    let fixture = Fixture::of(session);
    let mut before = fixture.record();
    without_last_own_entries(&mut before, 1);
    let base = tempfile::tempdir().unwrap();
    let mut cache = cache_holding(base.path(), &fixture, &before);
    let earlier = fixture.listed(&cache).cached_session.unwrap();
    let session = fixture.session_id(&cache);

    cache.write(&fixture.record()).unwrap();

    assert_eq!(
        fixture.session_id(&cache),
        session,
        "the session row keeps its id"
    );
    assert!(
        matches!(
            earlier.numbered_entries(None),
            Err(AppError::SessionCacheNotReady { .. })
        ),
        "a reader of the earlier generation is refused"
    );
    let current = fixture.listed(&cache).cached_session.unwrap();
    assert_ne!(
        current.copy_key(),
        earlier.copy_key(),
        "the copy after the write has another key"
    );
    assert!(current.numbered_entries(None).unwrap().is_some());
}
