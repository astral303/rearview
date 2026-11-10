//! A write of a session the cache already holds compares the new rows with
//! the stored ones and writes only the differences. Each test writes an
//! earlier version of a parse, then the parse, and compares the stored
//! session with a fresh write of the parse in another database.

use super::*;
use crate::history::provider::contract_tests::AgentSession;
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
    "search_texts",
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

struct Fixture {
    session: AgentSession,
    root: SessionRoot,
    stub: SessionStub,
}

impl Fixture {
    fn of(session: AgentSession) -> Self {
        let root = root_of(&session);
        let stub = stub_of(&session, 1);
        Self {
            session,
            root,
            stub,
        }
    }

    fn source(&self) -> Source {
        self.session.row.source
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
                "SELECT id FROM sessions WHERE storage = ?1 AND root = ?2 AND cache_key = ?3",
                params![
                    self.storage().directory,
                    path_text(&self.root.path),
                    CACHE_KEY
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
                &self.root.path,
                CACHE_KEY,
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
    record.entries.as_mut().unwrap().as_mut().unwrap()
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
            "SELECT storage, source, root, cache_key, locator, file_key, status, file_size, \
             mtime_secs, mtime_nsecs, session_id, session_id_key, handle, project_path, \
             project_name, cwd, timestamp_ms, preview_first, preview_last, summary, custom_title, \
             model, total_tokens, duration_minutes, message_count, assistant_messages, \
             is_missing_earlier_turns, entries_error FROM sessions WHERE id = ?1"
                .to_owned(),
        ),
        (
            "session_files",
            "SELECT position, kind, path, key, file_size, mtime_secs, mtime_nsecs \
             FROM session_files WHERE session = ?1 ORDER BY position"
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
            "subagent_search_parts",
            "SELECT position, file_position, text FROM subagent_search_parts \
             WHERE session = ?1 ORDER BY position"
                .to_owned(),
        ),
        (
            "transcripts",
            "SELECT path FROM transcripts WHERE session = ?1 ORDER BY path".to_owned(),
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
                 e.kind, e.timestamp_ms, e.content, e.search_field, e.text, e.text_codes, \
                 e.label, e.uuid, e.usage_input, e.usage_output, e.usage_cache_creation, \
                 e.usage_cache_read, e.level, e.duration_ms, e.parent_uuid, e.flag, e.digest \
                 FROM entries e LEFT JOIN transcripts tr ON tr.id = e.record_transcript \
                 WHERE e.session = ?1 ORDER BY e.sequence",
                first_entry("e.message")
            ),
        ),
        (
            "parts",
            "SELECT e.sequence, p.part, p.kind, p.search_field, p.text, p.text_codes, \
             p.call_id, p.name, p.tool, p.json, p.result_shape FROM parts p \
             JOIN entries e ON e.id = p.entry WHERE e.session = ?1 ORDER BY 1, 2"
                .to_owned(),
        ),
        (
            "result_items",
            "SELECT e.sequence, p.part, i.position, i.kind, i.searched, i.text, i.text_codes, \
             i.item_type FROM result_items i JOIN parts p ON p.id = i.part \
             JOIN entries e ON e.id = p.entry WHERE e.session = ?1 ORDER BY 1, 2, 3"
                .to_owned(),
        ),
        (
            "search_texts",
            "SELECT e.sequence, s.search_field, s.text FROM search_texts s \
             JOIN entries e ON e.id = s.entry WHERE s.session = ?1 ORDER BY e.sequence, s.id"
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
        .entries(fixture.storage(), &fixture.root.path, CACHE_KEY)
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
    copy.visit_search_texts(
        SearchedFields::Agent,
        &crate::time_filter::TimeFilter::default(),
        |text| {
            texts.push(format!("{:?} {:?} {}", text.field, text.ordinal, text.text));
            ControlFlow::Continue(())
        },
    )
    .unwrap()
    .unwrap();
    rows.push(format!("search texts {texts:?}"));
    let content = listed.load_content().unwrap();
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
/// file and its search parts.
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
        .search_parts
        .retain(|part| part.file_position != position);
    for part in &mut record.search_parts {
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
            writes_of(&writes, "parts", "INSERT") > 0,
            "{}: the write replaces the changed entry's parts: {writes:?}",
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
