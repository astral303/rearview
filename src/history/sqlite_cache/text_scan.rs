//! Lexical search's read of many sessions' search texts: `parts` is
//! scanned without joins, in row ranges across
//! threads, each range on a read connection of its own. A text's row holds
//! its session and its place; its entry and message are looked up only
//! within an active window, and only when the caller keeps the text.

use super::entries::{DERIVED, RESULT_TEXT};
use super::{CachedSession, SearchedFields, SqliteCache, entries};
use crate::error::{AppError, Result};
use crate::time_filter::TimeFilter;
use rayon::prelude::*;
use rusqlite::{Connection, OptionalExtension};
use std::collections::HashMap;
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

/// The caller's side of a scan, one per row range.
pub(crate) trait TextVisitor: Send {
    /// True when the scan is to look up `text`'s row and keep it.
    fn examine(&mut self, text: &str) -> bool;
    /// `text`, the text last examined, belongs to a scanned copy, inside the
    /// fields and the window scanned.
    fn keep(&mut self, row: &ScannedRow, text: &str);
}

/// The groups of `parts` holding search text, in the order a session's
/// joined search text lays them out.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum LayoutGroup {
    /// Blocks' parts.
    BlockParts,
    /// Parts of kind `result-text`.
    ResultTexts,
    /// Parts of kind `derived`.
    DerivedParts,
    /// Parts of kind `subagent-metadata`, outside every entry.
    SubagentMetadata,
}

impl LayoutGroup {
    /// The group of a part of `kind` holding search text.
    pub(super) fn of_part(kind: &str) -> Self {
        match kind {
            DERIVED => Self::DerivedParts,
            RESULT_TEXT => Self::ResultTexts,
            _ => Self::BlockParts,
        }
    }
}

/// Every session's sub-agent metadata parts, outside every entry.
const SUBAGENT_METADATA_SCAN: &str = "SELECT session, part, text FROM parts WHERE entry IS NULL";

/// Row ranges a scan of each large table is split into, per thread: rows
/// differ in size by orders of magnitude, so many small ranges keep every
/// thread busy until the scan ends.
const RANGES_PER_THREAD: i64 = 16;
/// The fewest rows a range holds, so a small table is one range.
const MIN_ROWS_PER_RANGE: i64 = 4096;

/// A scan's visitors, one per row range, and the copies it could not read.
pub(crate) struct ScannedTexts<V> {
    pub(crate) visitors: Vec<V>,
    /// The position of each copy the cache no longer holds, or holds another
    /// copy of, with the error reading it alone reports.
    pub(crate) missing: Vec<(usize, AppError)>,
}

/// Calls `visitor()`'s methods for each search text in `fields` of the
/// sessions `copies` read, within `window`: with `window` active, only the
/// texts of messages inside it, without the title, the summary or text
/// outside every message. Reads each copy as the cache holds it, whether or
/// not its transcripts changed since.
///
/// Fails when another copy of a scanned session replaces it during the scan.
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
        let (sessions, missing) = cache
            .with_read_snapshot(|connection| scanned_sessions(connection, copies, &positions))?;
        scanned.missing.extend(missing);
        if sessions.is_empty() {
            continue;
        }
        let scan = Scan {
            sessions,
            fields,
            window: StoredWindow::of(window),
            copies,
        };
        let tasks = scan.tasks(&cache)?;
        drop(cache);
        scanned.visitors.extend(
            tasks
                .into_par_iter()
                .map_init(
                    || None,
                    |reader: &mut Option<SqliteCache>, (group, rows)| {
                        let cache = open_once(reader, &first.base, first)?;
                        let mut visitor = visitor();
                        cache.with_read_snapshot(|connection| {
                            let mut range = RangeScan {
                                scan: &scan,
                                connection,
                                checked: HashMap::new(),
                                entry: None,
                                message: None,
                            };
                            range.scan(group, rows, &mut visitor)
                        })?;
                        Ok(visitor)
                    },
                )
                .collect::<Result<Vec<_>>>()?,
        );
    }
    Ok(scanned)
}

/// The scanned copies by their session's row id: each copy's position and
/// generation.
type SessionsById = HashMap<i64, (usize, i64)>;

/// One cache base's part of a scan.
struct Scan<'c> {
    sessions: SessionsById,
    fields: SearchedFields,
    window: Option<StoredWindow>,
    copies: &'c [&'c CachedSession],
}

/// The scanned sessions: the session row of each of the copies at
/// `positions`, and the position of each copy the cache does not hold, with
/// the error a read of it alone reports.
fn scanned_sessions(
    connection: &Connection,
    copies: &[&CachedSession],
    positions: &[usize],
) -> Result<(SessionsById, Vec<(usize, AppError)>)> {
    let mut sessions = HashMap::with_capacity(positions.len());
    let mut missing = Vec::new();
    for &position in positions {
        let copy = copies[position];
        match stored_session(connection, copy) {
            Ok(id) => {
                sessions.insert(id, (position, copy.generation));
            }
            Err(error) => missing.push((position, error)),
        }
    }
    Ok((sessions, missing))
}

/// The row id of the session `copy` reads.
fn stored_session(connection: &Connection, copy: &CachedSession) -> Result<i64> {
    let address = copy.address();
    if !super::is_storage_current(connection, address.storage)? {
        return Err(not_in_cache(copy));
    }
    let stored = entries::listed_session_of(
        connection,
        address.storage.directory,
        address.roots.key(),
        &address.cache_key,
    )?
    .ok_or_else(|| not_in_cache(copy))?;
    if stored.generation != copy.generation {
        return Err(generation_changed(copy));
    }
    Ok(stored.id)
}

fn not_in_cache(copy: &CachedSession) -> AppError {
    AppError::SessionCacheNotReady {
        session: copy.address().cache_key.clone(),
        reason: "the session is not in the session cache".to_owned(),
    }
}

fn generation_changed(copy: &CachedSession) -> AppError {
    AppError::SessionCacheNotReady {
        session: copy.address().cache_key.clone(),
        reason: "catalogue generation changed".to_owned(),
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

impl Scan<'_> {
    /// Each scan with the rows it reads: the entries' parts in row ranges,
    /// then the sub-agent metadata in one range when `fields` reads it.
    fn tasks(&self, cache: &SqliteCache) -> Result<Vec<(LayoutGroup, RangeInclusive<i64>)>> {
        let mut tasks = KeySplit::of(&cache.connection, "parts", "id")?
            .ranges()
            .map(|rows| (LayoutGroup::BlockParts, rows))
            .collect::<Vec<_>>();
        if self.window.is_none() && matches!(self.fields, SearchedFields::Agent) {
            tasks.push((LayoutGroup::SubagentMetadata, i64::MIN..=i64::MAX));
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

/// Of an entry, what a read needs to place its texts.
#[derive(Clone, Copy)]
pub(super) struct StoredEntry {
    pub(super) message: Option<i64>,
    pub(super) contributes: bool,
    pub(super) sequence: i64,
}

/// What a text's row holds of its entry, read from the first four columns:
/// `session`, `entry`, `entry_sequence`, `message_ordinal`.
struct TextOf {
    session: i64,
    entry: i64,
    /// The place without a window.
    place: EntryPlace,
}

impl TextOf {
    fn at(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        let ordinal: Option<i64> = row.get(3)?;
        Ok(Self {
            session: row.get(0)?,
            entry: row.get(1)?,
            place: EntryPlace {
                sequence: row.get(2)?,
                ordinal: ordinal.and_then(|ordinal| usize::try_from(ordinal).ok()),
            },
        })
    }
}

/// The place of an entry's texts in their session.
#[derive(Clone, Copy)]
struct EntryPlace {
    sequence: i64,
    /// As [`ScannedRow::ordinal`].
    ordinal: Option<usize>,
}

/// A message's number and times.
#[derive(Clone, Copy)]
pub(super) struct StoredMessage {
    pub(super) ordinal: Option<i64>,
    pub(super) first_ms: Option<i64>,
    pub(super) last_ms: Option<i64>,
}

/// The number of the message a text of `entry` belongs to, as search reads
/// it, `message` being the entry's message; `None` when `window` leaves the
/// text out.
pub(super) fn text_ordinal(
    entry: StoredEntry,
    message: Option<StoredMessage>,
    window: Option<StoredWindow>,
) -> Option<Option<usize>> {
    let ordinal = match window {
        Some(window) => {
            let inside = entry.contributes
                && message.is_some_and(|message| {
                    message.ordinal.is_some()
                        && message.last_ms.is_some_and(|last| last >= window.after)
                        && message.first_ms.is_some_and(|first| first <= window.before)
                });
            if !inside {
                return None;
            }
            message.and_then(|message| message.ordinal)
        }
        None if entry.contributes => message.and_then(|message| message.ordinal),
        None => None,
    };
    Some(ordinal.and_then(|ordinal| usize::try_from(ordinal).ok()))
}

/// One row range's scan, on its own connection and read snapshot.
struct RangeScan<'s> {
    scan: &'s Scan<'s>,
    connection: &'s Connection,
    /// Each session row found so far: its copy's position, when the copy is
    /// scanned.
    checked: HashMap<i64, Option<usize>>,
    /// The entry and the message queried last: consecutive rows tend to
    /// share them.
    entry: Option<(i64, StoredEntry)>,
    message: Option<(i64, StoredMessage)>,
}

impl RangeScan<'_> {
    fn scan(
        &mut self,
        group: LayoutGroup,
        rows: RangeInclusive<i64>,
        visitor: &mut impl TextVisitor,
    ) -> Result<()> {
        match group {
            LayoutGroup::BlockParts | LayoutGroup::ResultTexts | LayoutGroup::DerivedParts => {
                self.scan_parts(rows, visitor)
            }
            LayoutGroup::SubagentMetadata => self.scan_subagent_metadata(visitor),
        }
    }

    /// Scans the parts holding search text, of every kind.
    fn scan_parts(
        &mut self,
        rows: RangeInclusive<i64>,
        visitor: &mut impl TextVisitor,
    ) -> Result<()> {
        let mut statement = self.connection.prepare_cached(&format!(
            "SELECT session, entry, entry_sequence, message_ordinal, part, kind, text FROM parts \
             WHERE id BETWEEN ?1 AND ?2 AND search_field IN ({}) AND text IS NOT NULL",
            self.scan.fields.part_field_codes()
        ))?;
        let mut rows = statement.query([rows.start(), rows.end()])?;
        while let Some(row) = rows.next()? {
            let text = text_at(row, 6)?;
            if !visitor.examine(text) {
                continue;
            }
            let order = (LayoutGroup::of_part(text_at(row, 5)?), row.get(4)?);
            if let Some(scanned) = self.entry_row(TextOf::at(row)?, order)? {
                visitor.keep(&scanned, text);
            }
        }
        Ok(())
    }

    /// Scans the sub-agent metadata parts, outside every entry.
    fn scan_subagent_metadata(&mut self, visitor: &mut impl TextVisitor) -> Result<()> {
        let mut statement = self.connection.prepare_cached(SUBAGENT_METADATA_SCAN)?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            let text = text_at(row, 2)?;
            if !visitor.examine(text) {
                continue;
            }
            if let Some(copy) = self.copy_of(row.get(0)?)? {
                let scanned = ScannedRow {
                    copy,
                    ordinal: None,
                    order: ReadOrder {
                        group: LayoutGroup::SubagentMetadata,
                        sequence: 0,
                        part: row.get(1)?,
                    },
                };
                visitor.keep(&scanned, text);
            }
        }
        Ok(())
    }

    /// The row of the text `of` describes, in `group` at `part` in its entry;
    /// `None` when its session is not scanned, or the window leaves the text
    /// out.
    fn entry_row(
        &mut self,
        of: TextOf,
        (group, part): (LayoutGroup, i64),
    ) -> Result<Option<ScannedRow>> {
        let Some(copy) = self.copy_of(of.session)? else {
            return Ok(None);
        };
        let place = match self.scan.window {
            None => of.place,
            Some(_) => match self.windowed_place(of.entry)? {
                Some(place) => place,
                None => return Ok(None),
            },
        };
        Ok(Some(ScannedRow {
            copy,
            ordinal: place.ordinal,
            order: ReadOrder {
                group,
                sequence: place.sequence,
                part,
            },
        }))
    }

    /// The place of `entry`'s texts, from its entry and message rows, which
    /// hold the times the window compares; `None` when the window leaves the
    /// texts out, or the entry is gone.
    fn windowed_place(&mut self, entry: i64) -> Result<Option<EntryPlace>> {
        let Some(stored) = self.entry(entry)? else {
            return Ok(None);
        };
        let message = match stored.message {
            Some(message) => Some(self.message(message)?),
            None => None,
        };
        Ok(
            text_ordinal(stored, message, self.scan.window).map(|ordinal| EntryPlace {
                sequence: stored.sequence,
                ordinal,
            }),
        )
    }

    fn entry(&mut self, id: i64) -> Result<Option<StoredEntry>> {
        if let Some((last, stored)) = self.entry
            && last == id
        {
            return Ok(Some(stored));
        }
        let stored = self
            .connection
            .prepare_cached("SELECT message, contributes, sequence FROM entries WHERE id = ?1")?
            .query_row([id], |row| {
                Ok(StoredEntry {
                    message: row.get(0)?,
                    contributes: row.get(1)?,
                    sequence: row.get(2)?,
                })
            })
            .optional()?;
        if let Some(stored) = stored {
            self.entry = Some((id, stored));
        }
        Ok(stored)
    }

    fn message(&mut self, id: i64) -> Result<StoredMessage> {
        if let Some((last, stored)) = self.message
            && last == id
        {
            return Ok(stored);
        }
        let stored = self
            .connection
            .prepare_cached("SELECT ordinal, first_ms, last_ms FROM messages WHERE id = ?1")?
            .query_row([id], |row| {
                Ok(StoredMessage {
                    ordinal: row.get(0)?,
                    first_ms: row.get(1)?,
                    last_ms: row.get(2)?,
                })
            })
            .optional()?
            .unwrap_or(StoredMessage {
                ordinal: None,
                first_ms: None,
                last_ms: None,
            });
        self.message = Some((id, stored));
        Ok(stored)
    }

    /// The position of the scanned copy session row `session` holds; `None`
    /// for a session not scanned. Fails when this range's snapshot holds
    /// another copy of a scanned session than the scan started with.
    fn copy_of(&mut self, session: i64) -> Result<Option<usize>> {
        if let Some(copy) = self.checked.get(&session) {
            return Ok(*copy);
        }
        let copy = match self.scan.sessions.get(&session) {
            Some(&(position, generation)) => {
                let stored: Option<i64> = self
                    .connection
                    .prepare_cached("SELECT generation FROM sessions WHERE id = ?1")?
                    .query_row([session], |row| row.get(0))
                    .optional()?;
                if stored != Some(generation) {
                    return Err(generation_changed(self.scan.copies[position]));
                }
                Some(position)
            }
            None => None,
        };
        self.checked.insert(session, copy);
        Ok(copy)
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

    /// Every kept text with the position of its copy.
    #[derive(Default)]
    struct KeepEvery {
        kept: Vec<(usize, PlacedText)>,
    }

    impl TextVisitor for KeepEvery {
        fn examine(&mut self, _text: &str) -> bool {
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
                LayoutGroup::BlockParts,
                LayoutGroup::ResultTexts,
                LayoutGroup::DerivedParts,
                LayoutGroup::SubagentMetadata
            ]),
            "block parts, result texts, derived parts and sub-agent metadata parts each hold a text"
        );
        assert!(
            kept.iter().any(
                |(_, (order, ordinal, _))| order.group == LayoutGroup::BlockParts
                    && ordinal.is_none()
            ),
            "a part outside every message holds a text"
        );
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
