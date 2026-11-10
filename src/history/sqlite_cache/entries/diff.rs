//! A later write of a session's entry rows: each new row matched with its
//! stored version, and only the differences written. A matched row keeps
//! its id; when only its numbers moved, as when turns are spliced in before
//! it, those columns are updated in place.
//!
//! An entry matches the stored entry at the same place in its transcript:
//! the same line, and the same index among the entries read from that line.
//! A message matches the stored message its first entry matched; a thread
//! matches the stored thread of the same origin; a transcript the stored
//! transcript of the same place: its root and its path below it.

use super::rows::SessionRows;
use super::threads::ThreadOrigin;
use super::write::{EntryPlace, RowIds, RowWriter, SearchPlace};
use crate::error::Result;
use crate::history::sqlite_cache::roots::{StoredPath, WrittenPlaces};
use rusqlite::{Connection, params};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Makes the stored entry rows of `session` the rows of `rows`, inside the
/// caller's transaction.
pub(in super::super) fn update(
    connection: &Connection,
    session: i64,
    places: &WrittenPlaces,
    rows: &SessionRows,
) -> Result<()> {
    update_scope(connection, session, places, rows, Scope::Session)
}

/// Makes the stored entries of `session` from `rows`' first sequence on the
/// rows of `rows`, inside the caller's transaction. Every entry before it
/// keeps its row, its message, its thread and its transcript.
pub(in super::super) fn update_tail(
    connection: &Connection,
    session: i64,
    places: &WrittenPlaces,
    rows: &SessionRows,
) -> Result<()> {
    update_scope(connection, session, places, rows, Scope::Tail)
}

/// Makes `rows`, the rows of the session's leading entries, the stored
/// rows at sequence 0 on, where `stored_count` rows held them: every later
/// entry moves by the change in their number, and every later entry of the
/// session's own thread moves its position by as much. An unchanged leading
/// row is kept, a changed one updated in place.
pub(in super::super) fn replace_leading(
    connection: &Connection,
    session: i64,
    places: &WrittenPlaces,
    rows: &SessionRows,
    stored_count: usize,
) -> Result<()> {
    let stored: Vec<(i64, i64)> = connection
        .prepare_cached(
            "SELECT id, digest FROM entries WHERE session = ?1 AND sequence < ?2 ORDER BY sequence",
        )?
        .query_map(params![session, stored_count as i64], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })?
        .collect::<rusqlite::Result<_>>()?;
    for (id, _) in stored.iter().skip(rows.entries.len()) {
        connection
            .prepare_cached("DELETE FROM entries WHERE id = ?1")?
            .execute([id])?;
    }
    let mut writer = RowWriter::new(connection, session, places)?;
    let delta = rows.entries.len() as i64 - stored_count as i64;
    if delta != 0 {
        // Through negative sequences, so no two entries hold one at once.
        connection
            .prepare_cached(
                "UPDATE entries SET sequence = -sequence - 1 WHERE session = ?1 AND sequence >= ?2",
            )?
            .execute(params![session, stored_count as i64])?;
        connection
            .prepare_cached(
                "UPDATE entries SET sequence = -sequence - 1 + ?2 WHERE session = ?1 AND sequence < 0",
            )?
            .execute(params![session, delta])?;
        connection
            .prepare_cached(
                "UPDATE entries SET position = position + ?3 WHERE session = ?1 \
                 AND sequence >= ?2 AND child IS NULL AND (message IS NULL OR message IN \
                 (SELECT m.id FROM messages m JOIN threads t ON t.id = m.thread \
                 WHERE m.session = ?1 AND t.number = 0))",
            )?
            .execute(params![session, stored_count as i64 + delta, delta])?;
        writer.shift_search_places(stored_count as i64, delta)?;
    }
    let mut ids = RowIds::new(rows);
    for index in 0..rows.entries.len() {
        let row = &rows.entries[index];
        match stored.get(index) {
            Some(&(_, digest)) if digest == row.digest => {}
            Some(&(id, _)) => {
                let place = EntryPlace::of(row, rows.sequence_of(index), &ids);
                writer.update_entry(row, &place, SearchPlace::of(rows, index), id)?;
            }
            None => {
                writer.insert_entry(rows, &mut ids, index)?;
            }
        }
    }
    Ok(())
}

/// Rewrites stored entries in place: each of `rows`' entries over the
/// stored entry `stored` gives at its index, its id, its message and its
/// digest. Each row keeps its place, and its message its thread and its
/// ordinal.
pub(in super::super) fn rewrite_in_place(
    connection: &Connection,
    session: i64,
    places: &WrittenPlaces,
    rows: &SessionRows,
    stored: &[(i64, Option<i64>, i64)],
) -> Result<()> {
    let mut writer = RowWriter::new(connection, session, places)?;
    for (index, (row, &(id, message, digest))) in rows.entries.iter().zip(stored).enumerate() {
        if row.digest == digest {
            continue;
        }
        writer.rewrite_content(row, id)?;
        if let (Some(new), Some(message)) = (row.message, message)
            && rows.messages[new].first_entry == index
        {
            writer.rewrite_message(rows, new, message)?;
        }
    }
    Ok(())
}

/// The stored rows a write compares with its rows.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Scope {
    /// Every row of the session: a stored row the write does not hold is
    /// deleted.
    Session,
    /// The entries from the rows' first sequence on, and their messages.
    /// Threads and transcripts are matched, never deleted.
    Tail,
}

fn update_scope(
    connection: &Connection,
    session: i64,
    places: &WrittenPlaces,
    rows: &SessionRows,
    scope: Scope,
) -> Result<()> {
    let mut ids = RowIds::new(rows);
    let mut writer = RowWriter::new(connection, session, places)?;
    let stored_transcripts = match_transcripts(connection, session, places, rows, &mut ids)?;
    for index in 0..rows.transcripts.len() {
        writer.transcript(rows, &mut ids, index)?;
    }
    let gone_threads = match_threads(connection, session, rows, &stored_transcripts, &mut ids)?;
    for index in 0..rows.threads.len() {
        writer.thread(rows, &mut ids, index)?;
    }
    let stored = StoredEntries::read(connection, session, rows, &stored_transcripts.matches)?;
    let gone_messages =
        update_messages(connection, session, rows, &stored, &writer, &mut ids, scope)?;
    for index in 0..rows.messages.len() {
        writer.message(rows, &mut ids, index)?;
    }
    update_entries(connection, rows, &stored, &mut writer, &mut ids)?;
    for id in gone_messages {
        connection
            .prepare_cached("DELETE FROM messages WHERE id = ?1")?
            .execute([id])?;
    }
    if scope == Scope::Tail {
        return Ok(());
    }
    for id in gone_threads {
        connection
            .prepare_cached("DELETE FROM threads WHERE id = ?1")?
            .execute([id])?;
    }
    for id in stored_transcripts.gone {
        connection
            .prepare_cached("DELETE FROM transcripts WHERE id = ?1")?
            .execute([id])?;
    }
    Ok(())
}

/// The session's stored transcripts.
struct StoredTranscripts {
    /// The path of each, by id.
    paths: HashMap<i64, Arc<Path>>,
    /// The index in the new rows of each stored transcript they hold, by id.
    matches: HashMap<i64, usize>,
    /// The ids of the stored transcripts the new rows do not hold.
    gone: Vec<i64>,
}

/// Matches each of the new transcripts with the stored one of the same
/// place among `places`, giving it that one's id in `ids`.
fn match_transcripts(
    connection: &Connection,
    session: i64,
    places: &WrittenPlaces,
    rows: &SessionRows,
    ids: &mut RowIds,
) -> Result<StoredTranscripts> {
    let new: HashMap<StoredPath, usize> = rows
        .transcripts
        .iter()
        .enumerate()
        .map(|(index, path)| Ok((places.place_written(path)?, index)))
        .collect::<Result<_>>()?;
    let mut stored = StoredTranscripts {
        paths: HashMap::new(),
        matches: HashMap::new(),
        gone: Vec::new(),
    };
    let mut statement =
        connection.prepare_cached("SELECT id, root, path FROM transcripts WHERE session = ?1")?;
    let mut found = statement.query([session])?;
    while let Some(row) = found.next()? {
        let id: i64 = row.get(0)?;
        let place = StoredPath {
            root: row.get(1)?,
            path: row.get(2)?,
        };
        let path = match new.get(&place) {
            Some(&index) => {
                ids.transcripts[index] = Some(id);
                stored.matches.insert(id, index);
                rows.transcripts[index].clone()
            }
            None => {
                stored.gone.push(id);
                // A gone transcript's threads must match no new thread:
                // the path below an unknown root matches no new path.
                let unplaced = PathBuf::from(&place.path);
                Arc::from(places.path(place).unwrap_or(unplaced))
            }
        };
        stored.paths.insert(id, path);
    }
    Ok(stored)
}

/// Matches each of the new threads with the stored one of the same origin,
/// giving it that one's id in `ids` and its new number. Returns the ids of
/// the stored threads the new rows do not hold.
fn match_threads(
    connection: &Connection,
    session: i64,
    rows: &SessionRows,
    transcripts: &StoredTranscripts,
    ids: &mut RowIds,
) -> Result<Vec<i64>> {
    let new: HashMap<&ThreadOrigin, usize> = rows
        .threads
        .iter()
        .enumerate()
        .map(|(index, thread)| (&thread.origin, index))
        .collect();
    // Read every stored thread before updating any: an updated number moves
    // the row within the index the read walks.
    let stored = connection
        .prepare_cached(
            "SELECT id, number, transcript, label, nickname, role, agent_path, agent_id, \
             launch_call FROM threads WHERE session = ?1",
        )?
        .query_map([session], |row| {
            let origin = ThreadOrigin {
                transcript: row
                    .get::<_, Option<i64>>(2)?
                    .and_then(|transcript| transcripts.paths.get(&transcript).cloned()),
                label: row.get(3)?,
                nickname: row.get(4)?,
                role: row.get(5)?,
                agent_path: row.get(6)?,
                agent_id: row.get(7)?,
                launch_call: row.get(8)?,
            };
            Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?, origin))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut gone = Vec::new();
    for (id, number, origin) in stored {
        match new.get(&origin) {
            Some(&index) if ids.threads[index].is_none() => {
                ids.threads[index] = Some(id);
                if rows.threads[index].number != number {
                    connection
                        .prepare_cached("UPDATE threads SET number = ?1 WHERE id = ?2")?
                        .execute(params![rows.threads[index].number, id])?;
                }
            }
            _ => gone.push(id),
        }
    }
    Ok(gone)
}

/// An entry's place in its transcript: the entry at `index` among those
/// read from `line` of the transcript at `transcript` in the new rows'
/// transcripts, `None` for the session's own transcript.
#[derive(Clone, Copy, Hash, PartialEq, Eq)]
struct EntryKey {
    transcript: Option<usize>,
    line: i64,
    index: u32,
}

/// Gives each entry its [`EntryKey`], in entry order.
#[derive(Default)]
struct EntryKeys(HashMap<(Option<usize>, i64), u32>);

impl EntryKeys {
    fn next(&mut self, transcript: Option<usize>, line: i64) -> EntryKey {
        let index = self.0.entry((transcript, line)).or_default();
        let key = EntryKey {
            transcript,
            line,
            index: *index,
        };
        *index += 1;
        key
    }
}

struct StoredEntry {
    id: i64,
    sequence: i64,
    message: Option<i64>,
    contributes: bool,
    position: i64,
    record_transcript: Option<i64>,
    digest: i64,
    child: Option<i64>,
    /// The index of the new entry at its place.
    matched: Option<usize>,
}

/// The session's stored entries, in entry order, each matched with the new
/// entry at its place.
struct StoredEntries {
    entries: Vec<StoredEntry>,
    /// True for each new entry a stored entry is matched with, by index.
    matched: Vec<bool>,
}

impl StoredEntries {
    /// `transcripts` gives the index in `rows` of each stored transcript the
    /// new rows hold, by id.
    fn read(
        connection: &Connection,
        session: i64,
        rows: &SessionRows,
        transcripts: &HashMap<i64, usize>,
    ) -> Result<Self> {
        let paths: HashMap<&Path, usize> = rows
            .transcripts
            .iter()
            .enumerate()
            .map(|(index, path)| (path.as_ref(), index))
            .collect();
        let mut keys = EntryKeys::default();
        let new: HashMap<EntryKey, usize> = rows
            .entries
            .iter()
            .enumerate()
            .map(|(index, row)| {
                let transcript = row.record.transcript.as_deref().map(|path| paths[path]);
                (keys.next(transcript, row.record.line as i64), index)
            })
            .collect();
        let mut keys = EntryKeys::default();
        let mut matched = vec![false; rows.entries.len()];
        let mut entries = Vec::new();
        // An entry's transcript is its own, or else its message's thread's,
        // as a read gives it.
        let mut statement = connection.prepare_cached(
            "SELECT e.id, e.sequence, e.message, e.contributes, e.position, e.record_transcript, \
             e.digest, e.record_line, COALESCE(e.child, e.record_transcript, t.transcript), \
             e.child \
             FROM entries e LEFT JOIN messages m ON m.id = e.message \
             LEFT JOIN threads t ON t.id = m.thread WHERE e.session = ?1 AND e.sequence >= ?2 \
             ORDER BY e.sequence",
        )?;
        let mut found = statement.query(params![session, rows.first_sequence as i64])?;
        while let Some(row) = found.next()? {
            let line: i64 = row.get(7)?;
            let place = match row.get::<_, Option<i64>>(8)? {
                Some(transcript) => transcripts.get(&transcript).map(|index| Some(*index)),
                None => Some(None),
            };
            let index = place
                .map(|transcript| keys.next(transcript, line))
                .and_then(|key| new.get(&key).copied());
            if let Some(index) = index {
                matched[index] = true;
            }
            entries.push(StoredEntry {
                id: row.get(0)?,
                sequence: row.get(1)?,
                message: row.get(2)?,
                contributes: row.get(3)?,
                position: row.get(4)?,
                record_transcript: row.get(5)?,
                digest: row.get(6)?,
                child: row.get(9)?,
                matched: index,
            });
        }
        Ok(Self { entries, matched })
    }
}

/// Matches each new message with the stored message its first entry's
/// stored entry started, giving it that one's id in `ids`, and updates each
/// matched message whose columns changed. Returns the ids of the stored
/// messages the new rows do not hold, whose ordinals are cleared.
fn update_messages(
    connection: &Connection,
    session: i64,
    rows: &SessionRows,
    stored: &StoredEntries,
    writer: &RowWriter,
    ids: &mut RowIds,
    scope: Scope,
) -> Result<Vec<i64>> {
    let mut first_entries: HashMap<i64, Option<usize>> = HashMap::new();
    for entry in &stored.entries {
        if let Some(message) = entry.message {
            first_entries.entry(message).or_insert(entry.matched);
        }
    }
    let mut gone = Vec::new();
    let mut changed = Vec::new();
    let stored_messages: Vec<(i64, i64, Option<i64>, i64)> = match scope {
        Scope::Session => connection
            .prepare_cached("SELECT id, thread, ordinal, digest FROM messages WHERE session = ?1")?
            .query_map([session], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })?
            .collect::<rusqlite::Result<_>>()?,
        Scope::Tail => {
            let mut statement = connection
                .prepare_cached("SELECT id, thread, ordinal, digest FROM messages WHERE id = ?1")?;
            let mut messages = Vec::with_capacity(first_entries.len());
            for &id in first_entries.keys() {
                messages.push(statement.query_row([id], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
                })?);
            }
            messages
        }
    };
    for (id, thread, ordinal, digest) in stored_messages {
        let new = first_entries.get(&id).copied().flatten().and_then(|entry| {
            rows.entries[entry]
                .message
                .filter(|&message| rows.messages[message].first_entry == entry)
        });
        let Some(index) = new else {
            gone.push((id, ordinal));
            continue;
        };
        ids.messages[index] = Some(id);
        let message = &rows.messages[index];
        if ordinal != message.ordinal
            || Some(thread) != ids.threads[message.thread]
            || digest != message.digest()
        {
            changed.push((id, index, ordinal, message.ordinal));
        }
    }
    // No two messages may hold one ordinal at once. Clear the ordinals of
    // gone messages and of messages losing theirs first, and give messages
    // gaining one theirs last, once every other message holds its own.
    let clear = gone.iter().filter(|(_, ordinal)| ordinal.is_some());
    for (id, _) in clear {
        connection
            .prepare_cached("UPDATE messages SET ordinal = NULL WHERE id = ?1")?
            .execute([id])?;
    }
    let renumbered = changed
        .iter()
        .filter(|(_, _, ordinal, new)| ordinal != new)
        .map(|&(id, _, _, new)| (id, new))
        .collect::<Vec<_>>();
    let mut kept = Vec::new();
    let mut moved = Vec::new();
    let mut gained = Vec::new();
    for (id, index, ordinal, new) in changed {
        match (ordinal, new) {
            (Some(ordinal), Some(new)) if ordinal != new => moved.push((ordinal, new, (id, index))),
            (None, Some(_)) => gained.push((id, index)),
            // Losing its ordinal, or keeping it: updated before any message
            // moves.
            _ => kept.push((id, index)),
        }
    }
    let moved = match collision_free_order(moved) {
        Ok(order) => order,
        Err(order) => {
            for (id, _) in &order {
                connection
                    .prepare_cached("UPDATE messages SET ordinal = NULL WHERE id = ?1")?
                    .execute([id])?;
            }
            order
        }
    };
    for (id, index) in kept.into_iter().chain(moved).chain(gained) {
        writer.update_message(rows, ids, index, id)?;
    }
    for (id, ordinal) in renumbered {
        writer.renumber_search_places(id, ordinal)?;
    }
    Ok(gone.into_iter().map(|(id, _)| id).collect())
}

/// The rows of `moves`, each with its number before and after the write, in
/// an order that never gives two rows one number when each row is updated
/// in turn: rows moving down first, from the lowest, then rows moving up,
/// from the highest. A row's target is then free, or held by a row moved
/// before it, provided the rows keep their relative order. `Err` with the
/// rows when they do not.
fn collision_free_order<T>(mut moves: Vec<(i64, i64, T)>) -> std::result::Result<Vec<T>, Vec<T>> {
    moves.sort_by_key(|(from, _, _)| *from);
    let keeps_order = moves.windows(2).all(|pair| pair[0].1 < pair[1].1);
    if !keeps_order {
        return Err(moves.into_iter().map(|(_, _, row)| row).collect());
    }
    let (down, up): (Vec<_>, Vec<_>) = moves.into_iter().partition(|(from, to, _)| to < from);
    Ok(down
        .into_iter()
        .chain(up.into_iter().rev())
        .map(|(_, _, row)| row)
        .collect())
}

/// Deletes the stored entries the new rows do not hold, updates each
/// matched entry that changed or moved, and inserts the new entries.
fn update_entries(
    connection: &Connection,
    rows: &SessionRows,
    stored: &StoredEntries,
    writer: &mut RowWriter,
    ids: &mut RowIds,
) -> Result<()> {
    for entry in stored
        .entries
        .iter()
        .filter(|entry| entry.matched.is_none())
    {
        connection
            .prepare_cached("DELETE FROM entries WHERE id = ?1")?
            .execute([entry.id])?;
    }
    let matched = stored
        .entries
        .iter()
        .filter_map(|entry| entry.matched.map(|index| (entry, index)));
    let (kept, moved): (Vec<_>, Vec<_>) =
        matched.partition(|(entry, index)| entry.sequence == rows.sequence_of(*index) as i64);
    let moved = moved
        .into_iter()
        .map(|(entry, index)| {
            (
                entry.sequence,
                rows.sequence_of(index) as i64,
                (entry, index),
            )
        })
        .collect();
    let moved = match collision_free_order(moved) {
        Ok(order) => order,
        Err(order) => {
            // Move each entry out of the way first, so no two entries hold
            // one sequence at once: to its id, negated, which no sequence
            // takes.
            for (entry, _) in &order {
                connection
                    .prepare_cached("UPDATE entries SET sequence = -id WHERE id = ?1")?
                    .execute([entry.id])?;
            }
            order
        }
    };
    for (entry, index) in kept.into_iter().chain(moved) {
        let row = &rows.entries[index];
        let place = EntryPlace::of(row, rows.sequence_of(index), ids);
        let search = SearchPlace::of(rows, index);
        if entry.digest != row.digest {
            writer.update_entry(row, &place, search, entry.id)?;
        } else if entry.sequence != place.sequence
            || entry.message != place.message
            || entry.record_transcript != place.transcript
            || entry.child != place.child
            || entry.contributes != row.contributes
            || entry.position != row.position
        {
            writer.place_entry(row, &place, search, entry.id)?;
        }
    }
    for index in (0..rows.entries.len()).filter(|index| !stored.matched[*index]) {
        writer.insert_entry(rows, ids, index)?;
    }
    Ok(())
}
