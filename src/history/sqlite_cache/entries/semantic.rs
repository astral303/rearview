//! Each entry's semantic turn in `semantic_turns`: computed by
//! [`crate::history::parser::entry_semantic_turn`] as the entry's rows are
//! built, written with its content, its ordinal set where the parts' search
//! places are set, and read without joins or [`crate::semantic::filter_turn`].

use crate::error::Result;
use rusqlite::{Connection, params};
use std::ops::RangeInclusive;

/// The ordinal and text of each semantic turn with an ordinal of the session
/// `?1`, with a sequence from `?2` to `?3`, in the session's order.
pub(in super::super) const SEMANTIC_TURNS: &str = "SELECT ordinal, text FROM semantic_turns \
     WHERE session = ?1 AND sequence BETWEEN ?2 AND ?3 AND ordinal IS NOT NULL ORDER BY sequence";

pub(super) fn insert(
    connection: &Connection,
    session: i64,
    sequence: i64,
    ordinal: Option<i64>,
    turn: &str,
) -> rusqlite::Result<()> {
    connection
        .prepare_cached(
            "INSERT INTO semantic_turns (session, sequence, ordinal, text) \
             VALUES (?1, ?2, ?3, ?4)",
        )?
        .execute(params![session, sequence, ordinal, turn])?;
    Ok(())
}

/// Makes `turn`, with the ordinal `ordinal`, the semantic turn of the stored
/// entry `entry`, writing the row only when it changes; deletes the entry's
/// turn for `None`.
pub(super) fn replace_of_entry(
    connection: &Connection,
    entry: i64,
    ordinal: Option<i64>,
    turn: Option<&str>,
) -> rusqlite::Result<()> {
    match turn {
        Some(turn) => connection
            .prepare_cached(
                "INSERT INTO semantic_turns (session, sequence, ordinal, text) \
                 SELECT session, sequence, ?2, ?3 FROM entries WHERE id = ?1 \
                 ON CONFLICT (session, sequence) DO UPDATE SET ordinal = excluded.ordinal, \
                 text = excluded.text \
                 WHERE ordinal IS NOT excluded.ordinal OR text IS NOT excluded.text",
            )?
            .execute(params![entry, ordinal, turn])?,
        None => connection
            .prepare_cached(
                "DELETE FROM semantic_turns WHERE (session, sequence) = \
                 (SELECT session, sequence FROM entries WHERE id = ?1)",
            )?
            .execute([entry])?,
    };
    Ok(())
}

/// Moves the turn of the stored entry `entry` of `session` to the place
/// `sequence`, with the ordinal `ordinal`. Call it before the entry moves:
/// the turn is found at the entry's stored place.
pub(super) fn place(
    connection: &Connection,
    session: i64,
    entry: i64,
    sequence: i64,
    ordinal: Option<i64>,
) -> rusqlite::Result<()> {
    connection
        .prepare_cached(
            "UPDATE semantic_turns SET sequence = ?3, ordinal = ?4 \
             WHERE session = ?1 AND sequence = (SELECT sequence FROM entries WHERE id = ?2) \
             AND (sequence IS NOT ?3 OR ordinal IS NOT ?4)",
        )?
        .execute(params![session, entry, sequence, ordinal])?;
    Ok(())
}

/// Moves the turn of the stored entry `entry` of `session` to the place
/// `sequence`, keeping its ordinal. Call it before the entry moves, as
/// [`place`].
pub(super) fn move_to(
    connection: &Connection,
    session: i64,
    entry: i64,
    sequence: i64,
) -> rusqlite::Result<()> {
    connection
        .prepare_cached(
            "UPDATE semantic_turns SET sequence = ?3 \
             WHERE session = ?1 AND sequence = (SELECT sequence FROM entries WHERE id = ?2)",
        )?
        .execute(params![session, entry, sequence])?;
    Ok(())
}

/// Sets `ordinal` on the turns of the stored entries contributing to
/// `message`.
pub(super) fn renumber(
    connection: &Connection,
    message: i64,
    ordinal: Option<i64>,
) -> rusqlite::Result<()> {
    connection
        .prepare_cached(
            "UPDATE semantic_turns SET ordinal = ?2 WHERE (session, sequence) IN \
             (SELECT session, sequence FROM entries WHERE message = ?1 AND contributes)",
        )?
        .execute(params![message, ordinal])?;
    Ok(())
}

/// Calls `visit` with the ordinal and text of each semantic turn with an
/// ordinal of `session`, in the session's order.
pub(in super::super) fn read_semantic_turns(
    connection: &Connection,
    session: i64,
    visit: impl FnMut(usize, String),
) -> Result<()> {
    read_semantic_turns_in(connection, session, i64::MIN..=i64::MAX, visit)
}

/// [`read_semantic_turns`] of the entries of `session` with a sequence in
/// `sequences`.
pub(in super::super) fn read_semantic_turns_in(
    connection: &Connection,
    session: i64,
    sequences: RangeInclusive<i64>,
    mut visit: impl FnMut(usize, String),
) -> Result<()> {
    let mut statement = connection.prepare_cached(SEMANTIC_TURNS)?;
    let mut rows = statement.query([session, *sequences.start(), *sequences.end()])?;
    while let Some(row) = rows.next()? {
        let ordinal: i64 = row.get(0)?;
        let text = row.get_ref(1)?.as_str().map_err(rusqlite::Error::from)?;
        visit(ordinal as usize, text.to_owned());
    }
    Ok(())
}
