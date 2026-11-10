//! Lexical search's read of many sessions' search texts: `parts` is
//! scanned without joins, in tasks across threads, each task on a read
//! connection of its own: row ranges, or under an active window, the parts
//! of the window's messages through `parts_by_message`. A text's row holds
//! its session, its place and its message number.

use super::entries::{DERIVED, RESULT_TEXT};
use super::{CachedSession, SearchedFields, SqliteCache, entries};
use crate::error::{AppError, Result};
use crate::time_filter::TimeFilter;
use rayon::prelude::*;
use rusqlite::Connection;
use std::collections::{HashMap, HashSet};
use std::ops::RangeInclusive;

/// What a scan knows of the row of a text it keeps.
pub(crate) struct ScannedRow {
    /// The position of the text's session in the scanned copies.
    pub(crate) copy: usize,
    /// As [`super::search_texts::SearchText::ordinal`].
    pub(crate) ordinal: Option<usize>,
    /// The text's place in its session.
    pub(crate) order: ReadOrder,
}

/// A text's place in its session's order: its layout group, then its
/// entry's place in the session, then its part's place in the entry, or
/// among the parts outside every entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct ReadOrder {
    pub(super) group: LayoutGroup,
    pub(super) sequence: i64,
    pub(super) part: i64,
}

/// The caller's side of a scan, one per task.
pub(crate) trait TextVisitor: Send {
    /// True when the scan is to look up `text`'s row and keep it.
    fn examine(&mut self, text: &str) -> bool;
    /// `text`, the text last examined, belongs to a scanned copy, inside the
    /// fields and the window scanned.
    fn keep(&mut self, row: &ScannedRow, text: &str);
    /// The scanned copy at `copy` holds a text with more than whitespace
    /// inside the fields and the window scanned. Called at most once per
    /// copy and task, before the copy's first [`Self::keep`] in the task.
    fn holds_text(&mut self, _copy: usize) {}
}

/// The groups of `parts` holding search text, in the order a session's
/// joined search text lays them out.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum LayoutGroup {
    /// Blocks' parts.
    Block,
    /// Parts of kind `result-text`.
    ResultText,
    /// Parts of kind `derived`.
    Derived,
    /// Parts of kind `subagent-metadata`, outside every entry.
    SubagentMetadata,
}

impl LayoutGroup {
    /// The group of a part of `kind` holding search text. A part of any kind
    /// but a search-only one holds a block.
    pub(super) fn of_part(kind: &str) -> Self {
        match kind {
            DERIVED => Self::Derived,
            RESULT_TEXT => Self::ResultText,
            _ => Self::Block,
        }
    }
}

/// The columns a scan reads of each part holding search text.
const SCANNED_PART_COLUMNS: &str = "session, entry_sequence, message_ordinal, part, kind, text";

/// The parts holding search text in `fields` with ids from `?1` to `?2`, of
/// the sessions `only` keeps.
fn parts_in_rows(fields: SearchedFields, only: &SessionFilter) -> String {
    format!(
        "SELECT {SCANNED_PART_COLUMNS} FROM parts WHERE id BETWEEN ?1 AND ?2 \
         AND search_field IN ({}) AND text IS NOT NULL{}",
        fields.part_field_codes(),
        only.0,
    )
}

/// The parts holding search text in `fields` of the messages of the session
/// `?1` numbered from `?2` to `?3`, read through `parts_by_message`.
pub(super) fn parts_of_messages(fields: SearchedFields) -> String {
    format!(
        "SELECT {SCANNED_PART_COLUMNS} FROM parts WHERE session = ?1 \
         AND message_ordinal BETWEEN ?2 AND ?3 AND search_field IN ({}) AND text IS NOT NULL",
        fields.part_field_codes(),
    )
}

/// Every session's sub-agent metadata parts, outside every entry.
const SUBAGENT_METADATA_SCAN: &str = "SELECT session, part, text FROM parts WHERE entry IS NULL";

/// Row ranges a scan of each large table is split into, per thread: rows
/// differ in size by orders of magnitude, so many small ranges keep every
/// thread busy until the scan ends.
const RANGES_PER_THREAD: i64 = 16;
/// The fewest rows a range holds, so a small table is one range.
const MIN_ROWS_PER_RANGE: i64 = 4096;

/// A scan's visitors, one per task, and the copies it could not read.
pub(crate) struct ScannedTexts<V> {
    pub(crate) visitors: Vec<V>,
    /// The position of each copy the cache no longer holds, with the error
    /// reading it alone reports.
    pub(crate) missing: Vec<(usize, AppError)>,
    /// The position of each copy another write replaced before the scan
    /// ended, with the generation of the copy the cache holds now. The
    /// visitors may hold texts of either copy: callers drop them.
    pub(crate) replaced: Vec<(usize, i64)>,
}

/// Calls `visitor()`'s methods for each search text in `fields` of the
/// sessions `copies` read, within `window`: with `window` active, only the
/// texts of messages inside it, without the title, the summary or text
/// outside every message. Reads each copy as the cache holds it, whether or
/// not its transcripts changed since.
///
/// Each task reads in a snapshot of its own. Once every task ends, each
/// scanned session's generation is read again: a session another write
/// replaced during the scan is reported in `replaced`.
pub(crate) fn scan_search_texts<V: TextVisitor>(
    copies: &[&CachedSession],
    fields: SearchedFields,
    window: &TimeFilter,
    visitor: impl Fn() -> V + Sync,
) -> Result<ScannedTexts<V>> {
    let mut by_base: HashMap<&std::path::Path, Vec<usize>> = HashMap::new();
    for (position, copy) in copies.iter().enumerate() {
        by_base.entry(copy.base.path()).or_default().push(position);
    }
    let mut scanned = ScannedTexts {
        visitors: Vec::new(),
        missing: Vec::new(),
        replaced: Vec::new(),
    };
    for positions in by_base.into_values() {
        let first = copies[positions[0]];
        let Some(cache) = SqliteCache::open_waiting(&first.base)? else {
            scanned.missing.extend(
                positions
                    .iter()
                    .map(|&position| (position, not_in_cache(copies[position]))),
            );
            continue;
        };
        let (found, window, only) = cache.with_read_snapshot(|connection| {
            let found = ScannedSessions::of(connection, copies, positions.iter().copied())?;
            let window = StoredWindow::of(window)
                .map(|window| MessagesInWindow::of_sessions(connection, window, &found.current))
                .transpose()?;
            let only = SessionFilter::of(connection, &found.current)?;
            Ok((found, window, only))
        })?;
        let sessions = found.current;
        scanned.missing.extend(found.missing);
        scanned.replaced.extend(found.replaced);
        if sessions.is_empty() {
            continue;
        }
        let scan = Scan {
            sessions,
            only,
            fields,
            window,
        };
        let tasks = scan.tasks(&cache)?;
        drop(cache);
        scanned.visitors.extend(
            tasks
                .into_par_iter()
                .map_init(
                    || None,
                    |reader: &mut Option<SqliteCache>, task| {
                        let cache = open_once(reader, &first.base, first)?;
                        let mut visitor = visitor();
                        cache.with_read_snapshot(|connection| {
                            let mut range = TaskScan {
                                scan: &scan,
                                connection,
                                with_text: HashSet::new(),
                            };
                            range.scan(task, &mut visitor)
                        })?;
                        Ok(visitor)
                    },
                )
                .collect::<Result<Vec<_>>>()?,
        );
        let rechecked = match SqliteCache::open_waiting(&first.base)? {
            Some(cache) => cache.with_read_snapshot(|connection| {
                ScannedSessions::of(
                    connection,
                    copies,
                    scan.sessions.values().map(|&(position, _)| position),
                )
            })?,
            None => ScannedSessions {
                missing: scan
                    .sessions
                    .values()
                    .map(|&(position, _)| (position, not_in_cache(copies[position])))
                    .collect(),
                ..ScannedSessions::default()
            },
        };
        scanned.missing.extend(rechecked.missing);
        scanned.replaced.extend(rechecked.replaced);
    }
    Ok(scanned)
}

/// The scanned copies by their session's row id: each copy's position and
/// generation.
type SessionsById = HashMap<i64, (usize, i64)>;

/// One cache base's part of a scan.
struct Scan {
    sessions: SessionsById,
    only: SessionFilter,
    fields: SearchedFields,
    window: Option<MessagesInWindow>,
}

/// What the cache holds at the addresses of some scanned copies, by the
/// copies' positions.
#[derive(Default)]
struct ScannedSessions {
    /// The copies the cache holds, by their session's row id.
    current: SessionsById,
    /// As [`ScannedTexts::missing`].
    missing: Vec<(usize, AppError)>,
    /// As [`ScannedTexts::replaced`].
    replaced: Vec<(usize, i64)>,
}

impl ScannedSessions {
    fn of(
        connection: &Connection,
        copies: &[&CachedSession],
        positions: impl Iterator<Item = usize>,
    ) -> Result<Self> {
        let mut found = Self::default();
        for position in positions {
            let copy = copies[position];
            match stored_session(connection, copy) {
                Ok(stored) if stored.generation == copy.generation => {
                    found.current.insert(stored.id, (position, copy.generation));
                }
                Ok(stored) => found.replaced.push((position, stored.generation)),
                Err(error) => found.missing.push((position, error)),
            }
        }
        Ok(found)
    }
}

/// The scan's condition on a text's session, as SQL appended to a query's
/// `WHERE` clause: empty when the scan reads every listed session, else
/// the scanned sessions' ids, so the rows of the others are skipped before
/// their text is read.
struct SessionFilter(String);

impl SessionFilter {
    fn of(connection: &Connection, sessions: &SessionsById) -> Result<Self> {
        let listed: i64 = connection.query_row(
            "SELECT COUNT(*) FROM sessions WHERE status = 'listed'",
            [],
            |row| row.get(0),
        )?;
        if usize::try_from(listed).is_ok_and(|listed| sessions.len() >= listed) {
            return Ok(Self(String::new()));
        }
        let mut ids = sessions.keys().copied().collect::<Vec<_>>();
        ids.sort_unstable();
        let ids = ids
            .iter()
            .map(i64::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        // `+` keeps SQLite from reading the list through `parts_by_message`:
        // that index does not order a session's rows by id, so each row range
        // would read every part of every listed session.
        Ok(Self(format!(" AND +session IN ({ids})")))
    }
}

/// The session row at `copy`'s address, whichever copy it holds.
fn stored_session(connection: &Connection, copy: &CachedSession) -> Result<entries::StoredSession> {
    let address = copy.address();
    if !super::is_storage_current(connection, address.storage)? {
        return Err(not_in_cache(copy));
    }
    entries::listed_session_of(
        connection,
        address.storage.directory,
        address.roots.key(),
        &address.cache_key,
    )?
    .ok_or_else(|| not_in_cache(copy))
}

fn not_in_cache(copy: &CachedSession) -> AppError {
    AppError::SessionCacheNotReady {
        session: copy.address().cache_key.clone(),
        reason: "the session is not in the session cache".to_owned(),
    }
}

/// A time window in the milliseconds `messages` stores, both ends included.
#[derive(Clone, Copy)]
pub(super) struct StoredWindow {
    after: i64,
    before: i64,
}

impl StoredWindow {
    /// `window` in stored milliseconds; `None` when it is not active.
    pub(super) fn of(window: &TimeFilter) -> Option<Self> {
        window.is_active().then(|| Self {
            after: window
                .after
                .map_or(i64::MIN, |after| after.timestamp().saturating_mul(1000)),
            before: window.before.map_or(i64::MAX, |before| {
                before.timestamp().saturating_mul(1000).saturating_add(999)
            }),
        })
    }
}

/// Every message inside a window, of the scanned sessions, read through
/// `messages_by_time`.
const MESSAGES_IN_WINDOW: &str = "SELECT session, ordinal FROM messages \
     WHERE last_ms >= ?1 AND first_ms <= ?2 AND ordinal IS NOT NULL";
/// Every message of the session `?1` inside a window, read through the index
/// on `(session, ordinal)`.
const SESSION_MESSAGES_IN_WINDOW: &str = "SELECT session, ordinal FROM messages \
     WHERE session = ?1 AND ordinal IS NOT NULL AND last_ms >= ?2 AND first_ms <= ?3";

/// The numbers of the messages inside an active window, by session. A text
/// is inside the window when its part's message number is one of them.
pub(super) struct MessagesInWindow(HashMap<i64, Vec<i64>>);

impl MessagesInWindow {
    /// The messages inside `window` of each of `sessions`.
    fn of_sessions(
        connection: &Connection,
        window: StoredWindow,
        sessions: &SessionsById,
    ) -> Result<Self> {
        let mut statement = connection.prepare_cached(MESSAGES_IN_WINDOW)?;
        let rows = statement.query([window.after, window.before])?;
        Self::collect(rows, |session| sessions.contains_key(&session))
    }

    /// The messages inside `window` of the session `session`.
    pub(super) fn of_session(
        connection: &Connection,
        window: StoredWindow,
        session: i64,
    ) -> Result<Self> {
        let mut statement = connection.prepare_cached(SESSION_MESSAGES_IN_WINDOW)?;
        let rows = statement.query([session, window.after, window.before])?;
        Self::collect(rows, |_| true)
    }

    fn collect(mut rows: rusqlite::Rows<'_>, keeps: impl Fn(i64) -> bool) -> Result<Self> {
        let mut by_session: HashMap<i64, Vec<i64>> = HashMap::new();
        while let Some(row) = rows.next()? {
            let session = row.get(0)?;
            if keeps(session) {
                by_session.entry(session).or_default().push(row.get(1)?);
            }
        }
        for ordinals in by_session.values_mut() {
            ordinals.sort_unstable();
        }
        Ok(Self(by_session))
    }

    /// True when the message `ordinal` of `session` is inside the window;
    /// false for text outside every message.
    pub(super) fn holds(&self, session: i64, ordinal: Option<i64>) -> bool {
        ordinal.is_some_and(|ordinal| {
            self.0
                .get(&session)
                .is_some_and(|ordinals| ordinals.binary_search(&ordinal).is_ok())
        })
    }

    /// Each session's messages inside the window, as runs of consecutive
    /// numbers, each at most [`MESSAGES_PER_WINDOW_TASK`] long.
    fn runs(&self) -> impl Iterator<Item = ScanTask> + '_ {
        self.0.iter().flat_map(|(&session, ordinals)| {
            let mut runs: Vec<RangeInclusive<i64>> = Vec::new();
            for &ordinal in ordinals {
                match runs.last_mut() {
                    Some(run)
                        if *run.end() + 1 == ordinal
                            && run.end() - run.start() + 1 < MESSAGES_PER_WINDOW_TASK =>
                    {
                        *run = *run.start()..=ordinal;
                    }
                    _ => runs.push(ordinal..=ordinal),
                }
            }
            runs.into_iter()
                .map(move |ordinals| ScanTask::Messages { session, ordinals })
        })
    }
}

/// The most messages one task of a windowed scan reads, so a window keeping
/// many messages of one session spreads them across threads.
const MESSAGES_PER_WINDOW_TASK: i64 = 256;

/// One thread's share of a scan.
enum ScanTask {
    /// The parts with ids in the range.
    Parts(RangeInclusive<i64>),
    /// The parts of the messages of `session` numbered in `ordinals`: a
    /// windowed scan reads only the messages inside the window.
    Messages {
        session: i64,
        ordinals: RangeInclusive<i64>,
    },
    /// The sub-agent metadata parts, outside every entry.
    SubagentMetadata,
}

impl Scan {
    /// The scan's tasks. With a window, the parts of each run of messages
    /// inside it; without one, the parts in row ranges, then the sub-agent
    /// metadata when `fields` reads it.
    fn tasks(&self, cache: &SqliteCache) -> Result<Vec<ScanTask>> {
        if let Some(window) = &self.window {
            return Ok(window.runs().collect());
        }
        let mut tasks = KeySplit::of(&cache.connection, "parts", "id")?
            .ranges()
            .map(ScanTask::Parts)
            .collect::<Vec<_>>();
        if matches!(self.fields, SearchedFields::Agent) {
            tasks.push(ScanTask::SubagentMetadata);
        }
        Ok(tasks)
    }
}

fn open_once<'r>(
    reader: &'r mut Option<SqliteCache>,
    base: &super::CacheBase,
    copy: &CachedSession,
) -> Result<&'r SqliteCache> {
    if reader.is_none() {
        *reader = SqliteCache::open_waiting(base)?;
    }
    reader.as_ref().ok_or_else(|| not_in_cache(copy))
}

/// A table's key, from its least to its greatest value, split into ranges of
/// equal width across threads.
struct KeySplit {
    least: i64,
    greatest: i64,
    width: i64,
}

impl KeySplit {
    /// The split of `table`'s `key`, the first column of an index. An empty
    /// table has no range.
    fn of(connection: &Connection, table: &str, key: &str) -> Result<Self> {
        let (least, greatest): (Option<i64>, Option<i64>) =
            connection.query_row(&Self::bounds_query(table, key), [], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })?;
        let (least, greatest) = least.zip(greatest).unwrap_or((1, 0));
        let span = (greatest - least).saturating_add(1).max(0);
        let threads = i64::try_from(rayon::current_num_threads()).unwrap_or(1);
        let count = (span / MIN_ROWS_PER_RANGE)
            .clamp(1, threads * RANGES_PER_THREAD)
            .min(span.max(1));
        Ok(Self {
            least,
            greatest,
            width: ((span + count - 1) / count).max(1),
        })
    }

    /// The least and the greatest `key` of `table`, each in its own subquery:
    /// SQLite reads a lone MIN or MAX from one end of an index, but scans the
    /// whole table for the two together.
    fn bounds_query(table: &str, key: &str) -> String {
        format!("SELECT (SELECT MIN({key}) FROM {table}), (SELECT MAX({key}) FROM {table})")
    }

    fn ranges(&self) -> impl Iterator<Item = RangeInclusive<i64>> + use<> {
        let (least, greatest, width) = (self.least, self.greatest, self.width);
        (0..)
            .map(move |index| least + index * width)
            .take_while(move |start| *start <= greatest)
            .map(move |start| start..=start.saturating_add(width - 1).min(greatest))
    }
}

/// A search text's place, as its part's row stores it.
#[derive(Clone, Copy)]
pub(super) struct StoredPlace {
    /// The entry's `sequence`.
    pub(super) sequence: i64,
    /// The number of the message the entry contributes to; `None` for text
    /// outside every message.
    pub(super) ordinal: Option<i64>,
}

impl StoredPlace {
    /// The place in the columns `entry_sequence` and `message_ordinal` at
    /// `columns`.
    pub(super) fn at(row: &rusqlite::Row<'_>, columns: (usize, usize)) -> rusqlite::Result<Self> {
        Ok(Self {
            sequence: row.get(columns.0)?,
            ordinal: row.get(columns.1)?,
        })
    }

    /// The message number search reads: [`ScannedRow::ordinal`].
    pub(super) fn message(self) -> Option<usize> {
        self.ordinal
            .and_then(|ordinal| usize::try_from(ordinal).ok())
    }
}

/// One task's scan, on its own connection and read snapshot.
struct TaskScan<'s> {
    scan: &'s Scan,
    connection: &'s Connection,
    /// The scanned sessions found holding a text with more than whitespace.
    with_text: HashSet<i64>,
}

impl TaskScan<'_> {
    fn scan(&mut self, task: ScanTask, visitor: &mut impl TextVisitor) -> Result<()> {
        let fields = self.scan.fields;
        match task {
            ScanTask::Parts(rows) => self.scan_parts(
                &parts_in_rows(fields, &self.scan.only),
                &[*rows.start(), *rows.end()],
                visitor,
            ),
            ScanTask::Messages { session, ordinals } => self.scan_parts(
                &parts_of_messages(fields),
                &[session, *ordinals.start(), *ordinals.end()],
                visitor,
            ),
            ScanTask::SubagentMetadata => self.scan_subagent_metadata(visitor),
        }
    }

    /// Scans the parts holding search text, of every kind, that `sql`
    /// selects with `parameters`.
    fn scan_parts(
        &mut self,
        sql: &str,
        parameters: &[i64],
        visitor: &mut impl TextVisitor,
    ) -> Result<()> {
        let mut statement = self.connection.prepare_cached(sql)?;
        let mut rows = statement.query(rusqlite::params_from_iter(parameters))?;
        while let Some(row) = rows.next()? {
            let (session, text) = (row.get(0)?, text_at(row, 5)?);
            let examined = visitor.examine(text);
            let shows_text = self.shows_text(session, text);
            if !examined && !shows_text {
                continue;
            }
            let order = (LayoutGroup::of_part(text_at(row, 4)?), row.get(3)?);
            if let Some(scanned) = self.entry_row(session, StoredPlace::at(row, (1, 2))?, order) {
                self.visit(visitor, session, &scanned, text, (examined, shows_text));
            }
        }
        Ok(())
    }

    /// Scans the sub-agent metadata parts, outside every entry.
    fn scan_subagent_metadata(&mut self, visitor: &mut impl TextVisitor) -> Result<()> {
        let mut statement = self
            .connection
            .prepare_cached(&format!("{SUBAGENT_METADATA_SCAN}{}", self.scan.only.0))?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            let (session, text) = (row.get(0)?, text_at(row, 2)?);
            let examined = visitor.examine(text);
            let shows_text = self.shows_text(session, text);
            if !examined && !shows_text {
                continue;
            }
            if let Some(copy) = self.copy_of(session) {
                let scanned = ScannedRow {
                    copy,
                    ordinal: None,
                    order: ReadOrder {
                        group: LayoutGroup::SubagentMetadata,
                        sequence: 0,
                        part: row.get(1)?,
                    },
                };
                self.visit(visitor, session, &scanned, text, (examined, shows_text));
            }
        }
        Ok(())
    }

    /// True when `text` is the first text with more than whitespace this
    /// task finds of `session`.
    fn shows_text(&self, session: i64, text: &str) -> bool {
        !self.with_text.contains(&session) && !text.trim().is_empty()
    }

    /// Hands `text` of `session`, at `scanned`, to `visitor`: as a text it
    /// keeps when examined, and as the session's first text with more than
    /// whitespace when it shows text.
    fn visit(
        &mut self,
        visitor: &mut impl TextVisitor,
        session: i64,
        scanned: &ScannedRow,
        text: &str,
        (examined, shows_text): (bool, bool),
    ) {
        if shows_text {
            self.with_text.insert(session);
            visitor.holds_text(scanned.copy);
        }
        if examined {
            visitor.keep(scanned, text);
        }
    }

    /// The row of a text of `session` at `place`, in `group` at `part` in its
    /// entry; `None` when its session is not scanned, or the window leaves
    /// the text out.
    fn entry_row(
        &self,
        session: i64,
        place: StoredPlace,
        (group, part): (LayoutGroup, i64),
    ) -> Option<ScannedRow> {
        let copy = self.copy_of(session)?;
        if let Some(window) = &self.scan.window
            && !window.holds(session, place.ordinal)
        {
            return None;
        }
        Some(ScannedRow {
            copy,
            ordinal: place.message(),
            order: ReadOrder {
                group,
                sequence: place.sequence,
                part,
            },
        })
    }

    /// The position of the scanned copy of the session row `session`; `None`
    /// for a session not scanned. A copy replaced during the scan is found by
    /// the recheck after every task ends.
    fn copy_of(&self, session: i64) -> Option<usize> {
        self.scan
            .sessions
            .get(&session)
            .map(|&(position, _)| position)
    }
}

pub(super) fn text_at<'r>(row: &'r rusqlite::Row<'_>, index: usize) -> rusqlite::Result<&'r str> {
    row.get_ref(index)?.as_str().map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            index,
            rusqlite::types::Type::Text,
            Box::new(error),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::super::CatalogueSession;
    use super::super::tests::{admitted_cache, parsed_record, root_of, storage_of, stub_of};
    use super::*;
    use crate::history::provider::contract_tests::every_record_sessions;
    use chrono::TimeZone;
    use std::collections::BTreeSet;
    use std::ops::ControlFlow;

    /// A text with its place in its session and message number.
    type PlacedText = (ReadOrder, Option<usize>, String);

    /// Every kept text with the position of its copy, and the count of texts
    /// examined.
    #[derive(Default)]
    struct KeepEvery {
        kept: Vec<(usize, PlacedText)>,
        examined: usize,
    }

    impl TextVisitor for KeepEvery {
        fn examine(&mut self, _text: &str) -> bool {
            self.examined += 1;
            true
        }

        fn keep(&mut self, row: &ScannedRow, text: &str) {
            self.kept
                .push((row.copy, (row.order, row.ordinal, text.to_owned())));
        }
    }

    /// Each agent's every-record session cached in one database, as their
    /// copies. The first session gets a sub-agent metadata part, and each
    /// entry without a message a searched part, written directly: the
    /// fixtures don't hold either.
    fn every_record_copies(base: &std::path::Path) -> Vec<CachedSession> {
        let mut cache = admitted_cache(base);
        let copies = every_record_sessions()
            .iter()
            .map(|session| {
                let row = &session.row;
                let storage = storage_of(row.source);
                cache.admit_storage(storage).unwrap();
                let root = root_of(session);
                let stub = stub_of(session, 1);
                cache
                    .write(&parsed_record(row.source, &root, &stub))
                    .unwrap();
                let CatalogueSession::Listed(listed) = cache
                    .catalogue_session(storage, row.source, &root, &stub.cache_key, false)
                    .unwrap()
                    .unwrap()
                else {
                    panic!("listed fixture")
                };
                listed.cached_session.unwrap()
            })
            .collect::<Vec<_>>();
        cache
            .connection
            .execute(
                "INSERT INTO parts (session, part, kind, search_field, file_position, text) \
                 SELECT MIN(id), 0, 'subagent-metadata', 4, 1, 'child metadata' FROM sessions",
                [],
            )
            .unwrap();
        cache
            .connection
            .execute(
                "INSERT INTO parts (session, entry, part, kind, search_field, entry_sequence, \
                 text) SELECT session, id, 1000, 'text', 0, sequence, 'outside every message' \
                 FROM entries WHERE message IS NULL",
                [],
            )
            .unwrap();
        copies
    }

    /// Every text the scan keeps in `fields` within `window`, by copy, each
    /// copy's texts in their session's order.
    fn kept_rows(
        copies: &[&CachedSession],
        fields: SearchedFields,
        window: &TimeFilter,
    ) -> Vec<(usize, PlacedText)> {
        let scanned = scan_search_texts(copies, fields, window, KeepEvery::default).unwrap();
        assert!(scanned.missing.is_empty(), "the cache holds every copy");
        let mut kept = scanned
            .visitors
            .into_iter()
            .flat_map(|visitor| visitor.kept)
            .collect::<Vec<_>>();
        kept.sort();
        kept
    }

    /// Every text of `copy` in `fields` within `window`, read by range, in
    /// its session's order.
    fn read_by_range(
        copy: &CachedSession,
        fields: SearchedFields,
        window: &TimeFilter,
    ) -> Vec<PlacedText> {
        let mut read = Vec::new();
        let copy = copy.clone().ignoring_source_changes();
        copy.read_search_texts(fields, window, |text| {
            read.push((text.order, text.ordinal, text.text.to_owned()));
            ControlFlow::Continue(())
        })
        .unwrap()
        .expect("the cache holds the copy");
        read.sort();
        read
    }

    #[test]
    fn every_layout_group_contributes_texts() {
        let base = tempfile::tempdir().unwrap();
        let copies = every_record_copies(base.path());
        let copies = copies.iter().collect::<Vec<_>>();

        let kept = kept_rows(&copies, SearchedFields::Agent, &TimeFilter::default());
        let groups = kept
            .iter()
            .map(|(_, (order, ..))| order.group)
            .collect::<BTreeSet<_>>();
        assert_eq!(
            groups,
            BTreeSet::from([
                LayoutGroup::Block,
                LayoutGroup::ResultText,
                LayoutGroup::Derived,
                LayoutGroup::SubagentMetadata
            ]),
            "block parts, result texts, derived parts and sub-agent metadata parts each hold a text"
        );
        assert!(
            kept.iter()
                .any(|(_, (order, ordinal, _))| order.group == LayoutGroup::Block
                    && ordinal.is_none()),
            "a part outside every message holds a text"
        );
    }

    /// [`KeepEvery`] that replaces `copy` the first time any task examines a
    /// text, as a refresh writing while the scan reads does.
    struct ReplacesDuringScan<'a> {
        kept: KeepEvery,
        copy: &'a CachedSession,
        replaced: &'a std::sync::OnceLock<i64>,
    }

    impl TextVisitor for ReplacesDuringScan<'_> {
        fn examine(&mut self, text: &str) -> bool {
            self.replaced
                .get_or_init(|| super::super::tests::replace_copy(self.copy));
            self.kept.examine(text)
        }

        fn keep(&mut self, row: &ScannedRow, text: &str) {
            self.kept.keep(row, text);
        }
    }

    #[test]
    fn session_replaced_during_scan_is_reported_and_others_keep_their_texts() {
        let base = tempfile::tempdir().unwrap();
        let copies = every_record_copies(base.path());
        let references = copies.iter().collect::<Vec<_>>();
        let replaced = std::sync::OnceLock::new();
        let scanned = scan_search_texts(
            &references,
            SearchedFields::Agent,
            &TimeFilter::default(),
            || ReplacesDuringScan {
                kept: KeepEvery::default(),
                copy: &copies[1],
                replaced: &replaced,
            },
        )
        .unwrap();
        assert!(scanned.missing.is_empty(), "the cache holds every session");
        assert_eq!(
            scanned.replaced,
            vec![(1, *replaced.get().expect("a task examined a text"))],
            "the replaced session is reported with the generation the cache holds"
        );
        let kept = scanned
            .visitors
            .iter()
            .flat_map(|visitor| visitor.kept.kept.iter().map(|(copy, _)| *copy))
            .collect::<BTreeSet<_>>();
        assert!(
            (0..copies.len())
                .filter(|copy| *copy != 1)
                .all(|copy| kept.contains(&copy)),
            "every other session keeps its texts: {kept:?}"
        );
    }

    #[test]
    fn launch_read_holds_texts_of_session_replaced_since_its_row() {
        let base = tempfile::tempdir().unwrap();
        let copies = every_record_copies(base.path());
        let alone = |copy: &CachedSession| {
            crate::search::lexical::SessionSearchTexts::read(copy)
                .unwrap()
                .expect("the cache holds the copy")
                .joined()
                .clone()
        };
        let expected = alone(&copies[1]);
        super::super::tests::replace_copy(&copies[1]);
        let read = crate::search::CopiesToRead {
            copies: copies.clone(),
            are_every_row: true,
        }
        .read();
        assert!(
            read.iter().all(|(_, texts)| texts.is_some()),
            "every row holds texts"
        );
        assert_eq!(
            read[1].1.as_ref().unwrap().joined(),
            &expected,
            "the replaced row holds the texts of the copy the cache holds"
        );
    }

    #[test]
    fn scan_of_some_sessions_doesnt_examine_texts_of_others() {
        let base = tempfile::tempdir().unwrap();
        let copies = every_record_copies(base.path());
        for scanned in [vec![&copies[0]], vec![&copies[1], &copies[2]]] {
            let visitors = scan_search_texts(
                &scanned,
                SearchedFields::Agent,
                &TimeFilter::default(),
                KeepEvery::default,
            )
            .unwrap()
            .visitors;
            let examined = visitors
                .iter()
                .map(|visitor| visitor.examined)
                .sum::<usize>();
            let kept = visitors
                .iter()
                .map(|visitor| visitor.kept.len())
                .sum::<usize>();
            assert!(kept > 0, "the scanned sessions hold texts");
            assert_eq!(
                examined,
                kept,
                "the scan examines only the {} scanned sessions' texts",
                scanned.len()
            );
        }
    }

    #[test]
    fn every_row_read_in_one_scan_is_laid_out_as_each_session_read_alone_for_every_provider() {
        let base = tempfile::tempdir().unwrap();
        let copies = every_record_copies(base.path());
        let read = |are_every_row| {
            crate::search::CopiesToRead {
                copies: copies.clone(),
                are_every_row,
            }
            .read()
        };
        let (in_one_scan, alone) = (read(true), read(false));
        assert_eq!(in_one_scan.len(), alone.len());
        for ((key, scanned), (alone_key, alone)) in in_one_scan.into_iter().zip(alone) {
            assert_eq!(key, alone_key);
            assert_eq!(
                scanned.expect("the scan reads the copy").joined(),
                alone.expect("the range reads read the copy").joined(),
                "{key:?}: the scan lays out the session's texts as its own read does"
            );
        }
    }

    /// The split of a table holding rows of `ids`.
    fn split_of_rows(ids: &[i64]) -> KeySplit {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch("CREATE TABLE texts (id INTEGER PRIMARY KEY)")
            .unwrap();
        let mut insert = connection
            .prepare("INSERT INTO texts (id) VALUES (?1)")
            .unwrap();
        for id in ids {
            insert.execute([id]).unwrap();
        }
        KeySplit::of(&connection, "texts", "id").unwrap()
    }

    #[test]
    fn row_ranges_cover_each_row_once_with_gaps_in_ids_and_with_single_row_or_none() {
        let deleted_sessions =
            (1..=40_000).filter(|id| !(5_000..12_000).contains(id) && id % 7 != 0);
        for (case, ids) in [
            ("gaps", deleted_sessions.collect::<Vec<_>>()),
            ("single row", vec![42]),
            ("no row", Vec::new()),
        ] {
            let ranges = split_of_rows(&ids).ranges().collect::<Vec<_>>();
            if case == "gaps" {
                assert!(
                    ranges.len() > 1,
                    "{case}: the table splits into several ranges"
                );
            }
            for id in &ids {
                let covering = ranges.iter().filter(|range| range.contains(id)).count();
                assert_eq!(covering, 1, "{case}: one range covers row {id}: {ranges:?}");
            }
            if ids.is_empty() {
                assert!(ranges.is_empty(), "{case}: an empty table has no range");
            }
        }
    }

    #[test]
    fn split_bounds_read_each_scanned_tables_index_without_scanning_it() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(super::super::schema::CREATE)
            .unwrap();
        for (table, key) in [("parts", "id"), ("entries", "id"), ("messages", "id")] {
            let query = KeySplit::bounds_query(table, key);
            let plan = connection
                .prepare(&format!("EXPLAIN QUERY PLAN {query}"))
                .unwrap()
                .query_map([], |row| row.get::<_, String>(3))
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap()
                .join("\n");
            assert!(
                plan.contains(&format!("SEARCH {table}"))
                    && !plan.contains(&format!("SCAN {table}")),
                "{query}: the bounds come from the ends of an index, without a scan: {plan}"
            );
        }
    }

    /// A scan of some of the listed sessions reads each row range by id, not
    /// every part of each scanned session once per range.
    #[test]
    fn scan_of_some_sessions_reads_each_row_range_by_id() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(super::super::schema::CREATE)
            .unwrap();
        connection
            .execute_batch(
                "INSERT INTO roots (id, key) VALUES (1, 'root'); \
                 INSERT INTO sessions (id, storage, source, root, cache_key, status, file_size, \
                 mtime_secs, mtime_nsecs, generation) VALUES \
                 (1, 'storage', 'claude', 1, 'a', 'listed', 0, 0, 0, 1), \
                 (2, 'storage', 'claude', 1, 'b', 'listed', 0, 0, 0, 2)",
            )
            .unwrap();
        let only = SessionFilter::of(&connection, &HashMap::from([(1, (0, 1))])).unwrap();
        assert!(!only.0.is_empty(), "a scan of one of two sessions filters");
        let query = parts_in_rows(SearchedFields::Agent, &only);
        let plan = connection
            .prepare(&format!("EXPLAIN QUERY PLAN {query}"))
            .unwrap()
            .query_map([1, 2], |row| row.get::<_, String>(3))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
            .join("\n");
        assert!(
            plan.contains("SEARCH parts USING INTEGER PRIMARY KEY"),
            "{query}: the range is read by id: {plan}"
        );
    }

    /// A window's read costs the messages inside it, or one session's
    /// messages, not every message the cache holds.
    #[test]
    fn window_messages_are_read_through_an_index_without_scanning_messages() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(super::super::schema::CREATE)
            .unwrap();
        for (query, index) in [
            (MESSAGES_IN_WINDOW, "messages_by_time"),
            (SESSION_MESSAGES_IN_WINDOW, "sqlite_autoindex_messages_1"),
        ] {
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
                plan.contains(&format!("SEARCH messages USING INDEX {index} "))
                    && !plan.contains("SCAN messages"),
                "{query}: the window's messages are read through {index}: {plan}"
            );
        }
    }

    /// The scan of sub-agent metadata costs what those parts hold, not what
    /// every part holds.
    #[test]
    fn subagent_metadata_scan_searches_an_index_without_scanning_parts() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(super::super::schema::CREATE)
            .unwrap();
        let plan = connection
            .prepare(&format!("EXPLAIN QUERY PLAN {SUBAGENT_METADATA_SCAN}"))
            .unwrap()
            .query_map([], |row| row.get::<_, String>(3))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
            .join("\n");
        assert!(
            plan.contains("SEARCH parts USING") && !plan.contains("SCAN parts"),
            "the scan reads the parts outside every entry through an index: {plan}"
        );
    }

    #[test]
    fn session_read_by_range_returns_full_scans_texts_in_its_order_for_every_provider() {
        let base = tempfile::tempdir().unwrap();
        let copies = every_record_copies(base.path());
        let references = copies.iter().collect::<Vec<_>>();
        let since_1970 = TimeFilter {
            after: Some(crate::clock::Zone::System.timestamp_opt(0, 0).unwrap()),
            before: None,
        };
        for fields in [SearchedFields::List, SearchedFields::Agent] {
            for window in [TimeFilter::default(), since_1970] {
                let scanned = kept_rows(&references, fields, &window);
                for (position, copy) in copies.iter().enumerate() {
                    let from_scan = scanned
                        .iter()
                        .filter(|(copy, _)| *copy == position)
                        .map(|(_, text)| text.clone())
                        .collect::<Vec<_>>();
                    assert!(
                        !from_scan.is_empty(),
                        "{:?}, window {window:?}: the scan finds texts",
                        copy.address().source
                    );
                    assert_eq!(
                        read_by_range(copy, fields, &window),
                        from_scan,
                        "{:?}, window {window:?}: the range reads return the scan's texts and \
                         places",
                        copy.address().source
                    );
                }
            }
        }
    }
}
