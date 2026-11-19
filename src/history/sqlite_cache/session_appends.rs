//! The `session_appends` table: a resumed write that kept every stored row
//! before its checkpoint records which search texts it rewrote. Every other
//! write of the session clears the record. A reader holding texts of the
//! resumed copy reads only the rewritten texts.

use crate::error::Result;
use rusqlite::{Connection, OptionalExtension, params};

/// A resumed write's record in `session_appends`. The write rewrote the
/// search texts of messages from `first_message` on, those outside every
/// message of entries from `first_entry` on, and the sub-agent metadata; it
/// kept every other search text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct StoredAppend {
    /// The write rewrote the texts of the messages numbered from this
    /// ordinal on; `None` when it didn't rewrite any message's texts.
    pub(crate) first_message: Option<i64>,
    /// The write rewrote the texts outside every message of the entries
    /// from this sequence on.
    pub(crate) first_entry: i64,
}

/// The record of a resumed write of the session row `session`, begun before
/// the write replaces the stored entries from `first_entry` on.
pub(super) struct AppendRecord {
    session: i64,
    previous_generation: i64,
    first_entry: i64,
    /// The least ordinal among the messages of the stored entries the write
    /// replaces.
    first_stored_message: Option<i64>,
}

impl AppendRecord {
    /// Begins the record of a write resuming the copy `previous_generation`
    /// at the entry `first_entry`.
    pub(super) fn begin(
        connection: &Connection,
        session: i64,
        previous_generation: i64,
        first_entry: i64,
    ) -> Result<Self> {
        let first_stored_message = connection
            .prepare_cached(LEAST_MESSAGE_FROM_ENTRY)?
            .query_row(params![session, first_entry], |row| row.get(0))?;
        Ok(Self {
            session,
            previous_generation,
            first_entry,
            first_stored_message,
        })
    }

    /// Records the write, once it kept every stored row before
    /// `first_entry`. `first_written_message` is the least ordinal among the
    /// messages of the entries it wrote.
    pub(super) fn record(
        self,
        connection: &Connection,
        first_written_message: Option<i64>,
    ) -> Result<()> {
        let first_message = self
            .first_stored_message
            .into_iter()
            .chain(first_written_message)
            .min();
        connection
            .prepare_cached(
                "INSERT INTO session_appends \
                 (session, previous_generation, first_message, first_entry) \
                 VALUES (?1, ?2, ?3, ?4)",
            )?
            .execute(params![
                self.session,
                self.previous_generation,
                first_message,
                self.first_entry
            ])?;
        Ok(())
    }
}

/// The least ordinal among the messages of the session `?1`'s stored
/// entries at sequence `?2` on, read through the index on the entries'
/// sequence.
const LEAST_MESSAGE_FROM_ENTRY: &str = "SELECT MIN(m.ordinal) FROM entries e \
     JOIN messages m ON m.id = e.message WHERE e.session = ?1 AND e.sequence >= ?2";

/// Clears the session row `session`'s record, before a write records its
/// own or leaves none.
pub(super) fn clear(connection: &Connection, session: i64) -> Result<()> {
    connection
        .prepare_cached("DELETE FROM session_appends WHERE session = ?1")?
        .execute([session])?;
    Ok(())
}

/// The record of the resumed write that wrote the session row `session`'s
/// copy over the copy `previous_generation`; `None` when another write wrote
/// it.
pub(super) fn read(
    connection: &Connection,
    session: i64,
    previous_generation: i64,
) -> Result<Option<StoredAppend>> {
    Ok(connection
        .prepare_cached(
            "SELECT first_message, first_entry FROM session_appends \
             WHERE session = ?1 AND previous_generation = ?2",
        )?
        .query_row([session, previous_generation], |row| {
            Ok(StoredAppend {
                first_message: row.get(0)?,
                first_entry: row.get(1)?,
            })
        })
        .optional()?)
}
