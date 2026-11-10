//! Lexical search's read of many sessions' search texts: `parts` is
//! scanned without joins, in tasks across threads, each task on a read
//! connection of its own: row ranges, or for one project's sessions or under
//! an active window, runs of each session's messages through
//! `parts_by_message`. A text's row holds its session, its place and its
//! message number.

use super::{CachedSession, SearchedFields, SqliteCache};
use crate::error::{AppError, Result};
use crate::time_filter::TimeFilter;
use rayon::prelude::*;
use rusqlite::Connection;
use std::collections::{HashMap, HashSet};
use std::ops::RangeInclusive;

/// A kept text's row, as the scan read it.
pub(crate) struct ScannedRow {
    /// The position of the text's session in the scanned copies.
    pub(crate) copy: usize,
    /// As [`super::search_texts::SearchText::ordinal`].
    pub(crate) ordinal: Option<usize>,
    /// The text's place in its session.
    pub(crate) order: ReadOrder,
}

/// A text's place in its session's order: its layout group, then its
/// entry's place in the session, then its position among the entry's search
/// texts, or its part's place among the parts outside every entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct ReadOrder {
    pub(super) group: LayoutGroup,
    pub(super) sequence: i64,
    pub(super) position: i64,
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
    /// An entry's parts.
    Entry,
    /// Parts of kind `subagent-metadata`, outside every entry.
    SubagentMetadata,
}

/// The columns a scan reads of each part holding search text.
const SCANNED_PART_COLUMNS: &str =
    "session, entry_sequence, message_ordinal, search_position, text";

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

/// The parts holding search text in `fields` of the session `?1` outside
/// every message, read through `parts_by_message`.
fn parts_outside_messages(fields: SearchedFields) -> String {
    format!(
        "SELECT {SCANNED_PART_COLUMNS} FROM parts WHERE session = ?1 \
         AND message_ordinal IS NULL AND search_field IN ({}) AND text IS NOT NULL",
        fields.part_field_codes(),
    )
}

/// Every session's sub-agent metadata parts, outside every entry.
const SUBAGENT_METADATA_SCAN: &str = "SELECT session, part, text FROM parts WHERE entry IS NULL";
/// The session `?1`'s sub-agent metadata parts, outside every entry.
const SESSION_SUBAGENT_METADATA_SCAN: &str =
    "SELECT session, part, text FROM parts WHERE session = ?1 AND entry IS NULL";

/// The sessions a scan reads, as the command selected them; the selection
/// decides how the scan reads `parts`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ScanScope {
    /// One project's sessions: each session's parts are read through
    /// `parts_by_message`, which costs what the project holds.
    Project,
    /// Sessions of every workspace, whether or not some are left out: `parts`
    /// is read in row ranges. Each part read through `parts_by_message` also
    /// looks up its row, so a scan of most of the cache reads faster in rows'
    /// order.
    AllWorkspaces,
}

/// Row ranges a scan of each large table is split into, per thread: rows
/// differ in size by orders of magnitude, so many small ranges keep every
/// thread busy until the scan ends.
const RANGES_PER_THREAD: i64 = 16;
/// The fewest rows a range holds, so a small table is one range.
const MIN_ROWS_PER_RANGE: i64 = 4096;

/// A scan's visitors, one per task, and the copies it could not read.
pub(crate) struct ScannedTexts<V> {
    pub(crate) visitors: Vec<V>,
    /// The position of each copy whose session the cache no longer lists.
    pub(crate) missing: Vec<usize>,
    /// The position of each copy another write replaced before the scan
    /// ended, with the generation of the copy the cache holds now. The
    /// visitors may hold texts of either copy: callers drop them.
    pub(crate) replaced: Vec<(usize, i64)>,
}

/// Calls `visitor()`'s methods for each search text in `fields` of the
/// sessions `copies` read, within `window`: with `window` active, only the
/// texts of messages inside it, without the title, the summary or text
/// outside every message. `scope` is the selection `copies` came from.
/// Reads each copy as the cache holds it, whether or not its transcripts
/// changed since.
///
/// Each task reads in a snapshot of its own. Once every task ends, each
/// scanned session's generation is read again: a session another write
/// replaced during the scan is reported in `replaced`.
pub(crate) fn scan_search_texts<V: TextVisitor>(
    copies: &[&CachedSession],
    fields: SearchedFields,
    window: &TimeFilter,
    scope: ScanScope,
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
            scanned.missing.extend(positions);
            continue;
        };
        let (found, scan, tasks) = cache.with_read_snapshot(|connection| {
            let mut found = ScannedSessions::of(connection, copies, positions.iter().copied())?;
            let sessions = std::mem::take(&mut found.current);
            let window = StoredWindow::of(window)
                .map(|window| MessagesInWindow::of_sessions(connection, window, &sessions, scope))
                .transpose()?;
            let scan = Scan {
                only: SessionFilter::of(connection, &sessions)?,
                sessions,
                fields,
                window,
            };
            let tasks = scan.tasks(connection, scope)?;
            Ok((found, scan, tasks))
        })?;
        scanned.missing.extend(found.missing);
        scanned.replaced.extend(found.replaced);
        if scan.sessions.is_empty() {
            continue;
        }
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
        let rechecked = cache.with_read_snapshot(|connection| {
            ScannedSessions::of(
                connection,
                copies,
                scan.sessions.values().map(|&(position, _)| position),
            )
        })?;
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

/// The copies the cache holds at the addresses of some scanned copies, and
/// the scanned copies missing or replaced, by the copies' positions.
#[derive(Default)]
struct ScannedSessions {
    /// The copies the cache holds, by their session's row id.
    current: SessionsById,
    /// As [`ScannedTexts::missing`].
    missing: Vec<usize>,
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
            match copy.listed_in(connection)? {
                Some(stored) if stored.generation == copy.generation => {
                    found.current.insert(stored.id, (position, copy.generation));
                }
                Some(stored) => found.replaced.push((position, stored.generation)),
                None => found.missing.push(position),
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

/// Every session's messages inside a window, read through
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
    /// The messages inside `window` of each of `sessions`, which the
    /// selection `scope` made: one project's sessions are read one session at
    /// a time, any other selection through the time of every session's
    /// messages.
    fn of_sessions(
        connection: &Connection,
        window: StoredWindow,
        sessions: &SessionsById,
        scope: ScanScope,
    ) -> Result<Self> {
        match scope {
            ScanScope::Project => {
                let mut by_session = HashMap::new();
                for &session in sessions.keys() {
                    by_session.extend(Self::of_session(connection, window, session)?.0);
                }
                Ok(Self(by_session))
            }
            ScanScope::AllWorkspaces => {
                let mut statement = connection.prepare_cached(MESSAGES_IN_WINDOW)?;
                let rows = statement.query([window.after, window.before])?;
                Self::collect(rows, |session| sessions.contains_key(&session))
            }
        }
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

    /// Each session's messages inside the window, in runs.
    fn runs(&self) -> impl Iterator<Item = ScanTask> + '_ {
        self.0
            .iter()
            .flat_map(|(&session, ordinals)| message_runs(session, ordinals.iter().copied()))
    }

    /// The numbers of `session`'s messages inside the window, as runs of
    /// consecutive numbers.
    pub(super) fn runs_of(&self, session: i64) -> Vec<RangeInclusive<i64>> {
        consecutive_runs(
            self.0.get(&session).into_iter().flatten().copied(),
            i64::MAX,
        )
    }
}

/// The messages of `session` numbered `ordinals`, in ascending order, as
/// tasks reading runs of consecutive numbers, each at most
/// [`MESSAGES_PER_TASK`] long.
fn message_runs(session: i64, ordinals: impl IntoIterator<Item = i64>) -> Vec<ScanTask> {
    consecutive_runs(ordinals, MESSAGES_PER_TASK)
        .into_iter()
        .map(|ordinals| ScanTask::Messages { session, ordinals })
        .collect()
}

/// `ordinals`, in ascending order, as runs of consecutive numbers, each at
/// most `longest` long.
fn consecutive_runs(
    ordinals: impl IntoIterator<Item = i64>,
    longest: i64,
) -> Vec<RangeInclusive<i64>> {
    let mut runs: Vec<RangeInclusive<i64>> = Vec::new();
    for ordinal in ordinals {
        match runs.last_mut() {
            Some(run) if *run.end() + 1 == ordinal && run.end() - run.start() + 1 < longest => {
                *run = *run.start()..=ordinal;
            }
            _ => runs.push(ordinal..=ordinal),
        }
    }
    runs
}

/// The most messages one task reads, so a session with many messages spreads
/// them across threads.
const MESSAGES_PER_TASK: i64 = 256;

/// A share of a scan one thread reads in one snapshot.
#[derive(Debug, PartialEq, Eq)]
enum ScanTask {
    /// The parts with ids in the range, of the scanned sessions.
    Parts(RangeInclusive<i64>),
    /// The parts of the messages of `session` numbered in `ordinals`.
    Messages {
        session: i64,
        ordinals: RangeInclusive<i64>,
    },
    /// The scanned sessions' sub-agent metadata parts, outside every entry.
    SubagentMetadata,
    /// The parts holding search text of `session` outside every message.
    OutsideMessages { session: i64 },
    /// The sub-agent metadata parts of `session`.
    SessionSubagentMetadata { session: i64 },
}

impl Scan {
    /// The scan's tasks for the selection `scope`, read on `connection`. With
    /// a window, the parts of each run of messages inside it. Without one,
    /// for one project's sessions, each session's messages in runs, its text
    /// outside every message and, when `fields` reads it, its sub-agent
    /// metadata; for any other selection, the parts in row ranges, then the
    /// sub-agent metadata when `fields` reads it.
    fn tasks(&self, connection: &Connection, scope: ScanScope) -> Result<Vec<ScanTask>> {
        if let Some(window) = &self.window {
            return Ok(window.runs().collect());
        }
        let reads_subagent_metadata = matches!(self.fields, SearchedFields::Agent);
        let mut tasks = Vec::new();
        match scope {
            ScanScope::Project => {
                let mut message_count = connection
                    .prepare_cached("SELECT message_count FROM sessions WHERE id = ?1")?;
                for &session in self.sessions.keys() {
                    let count: i64 = message_count.query_row([session], |row| row.get(0))?;
                    tasks.extend(message_runs(session, 1..=count));
                    tasks.push(ScanTask::OutsideMessages { session });
                    if reads_subagent_metadata {
                        tasks.push(ScanTask::SessionSubagentMetadata { session });
                    }
                }
            }
            ScanScope::AllWorkspaces => {
                tasks.extend(
                    KeySplit::of(connection, "parts", "id")?
                        .ranges()
                        .map(ScanTask::Parts),
                );
                if reads_subagent_metadata {
                    tasks.push(ScanTask::SubagentMetadata);
                }
            }
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

/// An entry's search text's place, as its part's row stores it.
#[derive(Clone, Copy)]
pub(super) struct StoredPlace {
    /// The entry's `sequence`.
    pub(super) sequence: i64,
    /// The number of the message the entry contributes to; `None` for text
    /// outside every message.
    pub(super) ordinal: Option<i64>,
    /// The text's `search_position` in its entry.
    position: i64,
}

impl StoredPlace {
    /// The place in the columns `entry_sequence`, `message_ordinal` and
    /// `search_position` at `columns`.
    pub(super) fn at(
        row: &rusqlite::Row<'_>,
        columns: (usize, usize, usize),
    ) -> rusqlite::Result<Self> {
        Ok(Self {
            sequence: row.get(columns.0)?,
            ordinal: row.get(columns.1)?,
            position: row.get(columns.2)?,
        })
    }

    /// The message number search reads: [`ScannedRow::ordinal`].
    pub(super) fn message(self) -> Option<usize> {
        self.ordinal
            .and_then(|ordinal| usize::try_from(ordinal).ok())
    }

    /// The text's place in its session's order.
    pub(super) fn order(self) -> ReadOrder {
        ReadOrder {
            group: LayoutGroup::Entry,
            sequence: self.sequence,
            position: self.position,
        }
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
            ScanTask::SubagentMetadata => self.scan_subagent_metadata(
                &format!("{SUBAGENT_METADATA_SCAN}{}", self.scan.only.0),
                &[],
                visitor,
            ),
            ScanTask::OutsideMessages { session } => {
                self.scan_parts(&parts_outside_messages(fields), &[session], visitor)
            }
            ScanTask::SessionSubagentMetadata { session } => {
                self.scan_subagent_metadata(SESSION_SUBAGENT_METADATA_SCAN, &[session], visitor)
            }
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
            let (session, text) = (row.get(0)?, text_at(row, 4)?);
            let examined = visitor.examine(text);
            if !examined && !self.is_first_text(session, text) {
                continue;
            }
            if let Some(scanned) = self.entry_row(session, StoredPlace::at(row, (1, 2, 3))?) {
                self.record_first_text(visitor, session, &scanned, text);
                if examined {
                    visitor.keep(&scanned, text);
                }
            }
        }
        Ok(())
    }

    /// Scans the sub-agent metadata parts, outside every entry.
    fn scan_subagent_metadata(
        &mut self,
        sql: &str,
        parameters: &[i64],
        visitor: &mut impl TextVisitor,
    ) -> Result<()> {
        let mut statement = self.connection.prepare_cached(sql)?;
        let mut rows = statement.query(rusqlite::params_from_iter(parameters))?;
        while let Some(row) = rows.next()? {
            let (session, text) = (row.get(0)?, text_at(row, 2)?);
            let examined = visitor.examine(text);
            if !examined && !self.is_first_text(session, text) {
                continue;
            }
            if let Some(copy) = self.copy_of(session) {
                let scanned = ScannedRow {
                    copy,
                    ordinal: None,
                    order: ReadOrder {
                        group: LayoutGroup::SubagentMetadata,
                        sequence: 0,
                        position: row.get(1)?,
                    },
                };
                self.record_first_text(visitor, session, &scanned, text);
                if examined {
                    visitor.keep(&scanned, text);
                }
            }
        }
        Ok(())
    }

    /// True when `text` has more than whitespace and is the first such text
    /// this task finds of `session`.
    fn is_first_text(&self, session: i64, text: &str) -> bool {
        !self.with_text.contains(&session) && !text.trim().is_empty()
    }

    /// Calls [`TextVisitor::holds_text`] for `scanned`'s copy when `text`, of
    /// `session`, is the first text with more than whitespace this task
    /// finds.
    fn record_first_text(
        &mut self,
        visitor: &mut impl TextVisitor,
        session: i64,
        scanned: &ScannedRow,
        text: &str,
    ) {
        if self.is_first_text(session, text) {
            self.with_text.insert(session);
            visitor.holds_text(scanned.copy);
        }
    }

    /// The row of an entry's text of `session` at `place`; `None` when its
    /// session is not scanned. A windowed scan reads only the messages inside
    /// its window.
    fn entry_row(&self, session: i64, place: StoredPlace) -> Option<ScannedRow> {
        let copy = self.copy_of(session)?;
        Some(ScannedRow {
            copy,
            ordinal: place.message(),
            order: place.order(),
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
                 search_position, text) SELECT session, id, 1000, 'text', 0, sequence, 1000, \
                 'outside every message' FROM entries WHERE message IS NULL",
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
        scope: ScanScope,
    ) -> Vec<(usize, PlacedText)> {
        let scanned = scan_search_texts(copies, fields, window, scope, KeepEvery::default).unwrap();
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
        copy.read_held_copy(|held| {
            held.visit_search_texts(fields, window, |text| {
                read.push((text.order, text.ordinal, text.text.to_owned()));
                ControlFlow::Continue(())
            })
        })
        .unwrap()
        .expect("the cache holds the copy");
        read.sort();
        read
    }

    #[test]
    fn every_layout_group_and_kind_of_part_holding_search_text_contributes_texts() {
        let base = tempfile::tempdir().unwrap();
        let copies = every_record_copies(base.path());
        let references = copies.iter().collect::<Vec<_>>();

        let kept = kept_rows(
            &references,
            SearchedFields::Agent,
            &TimeFilter::default(),
            ScanScope::AllWorkspaces,
        );
        let groups = kept
            .iter()
            .map(|(_, (order, ..))| order.group)
            .collect::<BTreeSet<_>>();
        assert_eq!(
            groups,
            BTreeSet::from([LayoutGroup::Entry, LayoutGroup::SubagentMetadata]),
            "entries' parts and sub-agent metadata parts each hold a text"
        );
        assert!(
            kept.iter()
                .any(|(_, (order, ordinal, _))| order.group == LayoutGroup::Entry
                    && ordinal.is_none()),
            "a part outside every message holds a text"
        );
        let cache = SqliteCache::open_waiting(&copies[0].base).unwrap().unwrap();
        let kinds = cache
            .connection
            .prepare(
                "SELECT DISTINCT kind FROM parts WHERE search_field IS NOT NULL AND text <> ''",
            )
            .unwrap()
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<rusqlite::Result<BTreeSet<_>>>()
            .unwrap();
        assert!(
            ["text", "result-text", "derived", "subagent-metadata"]
                .iter()
                .all(|kind| kinds.contains(*kind)),
            "block parts, result texts, derived parts and sub-agent metadata parts each hold a \
             text: {kinds:?}"
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
            ScanScope::AllWorkspaces,
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
    fn launch_read_and_read_per_copy_hold_texts_of_session_replaced_since_its_row() {
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
        for (are_every_row, read) in [(true, "the launch read"), (false, "a read per copy")] {
            let texts = crate::search::CopiesToRead {
                copies: copies.clone(),
                are_every_row,
            }
            .read();
            assert!(
                texts.iter().all(|(_, texts)| texts.is_some()),
                "{read}: every row holds texts"
            );
            assert_eq!(
                texts[1].1.as_ref().unwrap().joined(),
                &expected,
                "{read}: the replaced row holds the texts of the copy the cache holds"
            );
        }
    }

    #[test]
    fn scan_of_some_sessions_doesnt_examine_texts_of_others() {
        let base = tempfile::tempdir().unwrap();
        let copies = every_record_copies(base.path());
        let selections = [vec![&copies[0]], vec![&copies[1], &copies[2]]];
        for (scanned, scope) in selections.iter().flat_map(|scanned| {
            [ScanScope::Project, ScanScope::AllWorkspaces].map(|scope| (scanned, scope))
        }) {
            let visitors = scan_search_texts(
                scanned,
                SearchedFields::Agent,
                &TimeFilter::default(),
                scope,
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
                "{scope:?}: the scan examines only the {} scanned sessions' texts",
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

    /// The scan of sub-agent metadata, and a project scope's read of a
    /// session's text outside every message, cost what those parts hold, not
    /// what every part holds.
    #[test]
    fn subagent_metadata_and_outside_messages_scans_search_an_index_without_scanning_parts() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(super::super::schema::CREATE)
            .unwrap();
        for query in [
            SUBAGENT_METADATA_SCAN.to_owned(),
            SESSION_SUBAGENT_METADATA_SCAN.to_owned(),
            parts_outside_messages(SearchedFields::Agent),
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
                plan.contains("SEARCH parts USING") && !plan.contains("SCAN parts"),
                "{query}: the scan reads its parts through an index: {plan}"
            );
        }
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
                for scope in [ScanScope::Project, ScanScope::AllWorkspaces] {
                    let scanned = kept_rows(&references, fields, &window, scope);
                    for (position, copy) in copies.iter().enumerate() {
                        let from_scan = scanned
                            .iter()
                            .filter(|(copy, _)| *copy == position)
                            .map(|(_, text)| text.clone())
                            .collect::<Vec<_>>();
                        let source = copy.address().source;
                        assert!(
                            !from_scan.is_empty(),
                            "{source:?}, window {window:?}, {scope:?}: the scan finds texts"
                        );
                        assert_eq!(
                            read_by_range(copy, fields, &window),
                            from_scan,
                            "{source:?}, window {window:?}, {scope:?}: the range reads return \
                             the scan's texts and places"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn message_runs_split_at_gaps_in_numbers_and_at_longest_run() {
        assert_eq!(
            consecutive_runs([1, 2, 3, 5, 6, 9], 2),
            [1..=2, 3..=3, 5..=6, 9..=9],
            "a run ends at a gap and at two messages"
        );
    }

    #[test]
    fn project_scope_reads_its_sessions_by_message_and_every_but_one_reads_row_ranges() {
        let base = tempfile::tempdir().unwrap();
        let copies = every_record_copies(base.path());
        let every_but_one = copies[1..].iter().collect::<Vec<_>>();
        let cache = SqliteCache::open_waiting(&copies[0].base).unwrap().unwrap();
        let tasks = |scope| {
            cache
                .with_read_snapshot(|connection| {
                    let sessions =
                        ScannedSessions::of(connection, &every_but_one, 0..every_but_one.len())?
                            .current;
                    Scan {
                        only: SessionFilter::of(connection, &sessions)?,
                        sessions,
                        fields: SearchedFields::Agent,
                        window: None,
                    }
                    .tasks(connection, scope)
                })
                .unwrap()
        };
        let reads_rows =
            |task: &ScanTask| matches!(task, ScanTask::Parts(_) | ScanTask::SubagentMetadata);
        let project = tasks(ScanScope::Project);
        assert!(
            project
                .iter()
                .any(|task| matches!(task, ScanTask::Messages { .. }))
                && !project.iter().any(reads_rows),
            "a project scope reads each session's messages, without row ranges: {project:?}"
        );
        let all_workspaces = tasks(ScanScope::AllWorkspaces);
        assert!(
            !all_workspaces.is_empty() && all_workspaces.iter().all(reads_rows),
            "every session but one is read in row ranges: {all_workspaces:?}"
        );
    }
}
