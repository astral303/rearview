//! Tables whose rows a session numbers by position: `(session, position)`
//! is the key. A write matches each new row with the stored row of the same
//! key, keeps an unchanged one, moves one whose position changed, updates
//! one whose columns changed, and deletes or inserts the rest.

use crate::error::Result;
use rusqlite::Connection;
use rusqlite::types::Value;
use std::collections::HashMap;
use std::hash::Hash;

pub(super) struct PositionalTable {
    pub(super) name: &'static str,
    /// The columns after `session` and `position`.
    pub(super) columns: &'static [&'static str],
}

/// A row's columns after `session` and `position`, and what it matches a
/// stored row by. Rows of one key match in order.
pub(super) struct PositionalRow<K> {
    pub(super) key: K,
    pub(super) values: Vec<Value>,
}

pub(super) const SESSION_FILES: PositionalTable = PositionalTable {
    name: "session_files",
    columns: &[
        "kind",
        "path",
        "key",
        "file_size",
        "mtime_secs",
        "mtime_nsecs",
    ],
};

pub(super) const TRANSCRIPT_READS: PositionalTable = PositionalTable {
    name: "transcript_reads",
    columns: &[
        "path",
        "cursor",
        "stable_cursor",
        "identity",
        "rewrite_check",
        "parser_state",
        "stable_line",
        "fold_state",
    ],
};

pub(super) const PARSE_ERRORS: PositionalTable = PositionalTable {
    name: "parse_errors",
    columns: &[
        "line_number",
        "line_content",
        "error_message",
        "context_before",
        "context_after",
    ],
};

pub(super) const MALFORMED_LINES: PositionalTable = PositionalTable {
    name: "malformed_lines",
    columns: &[
        "line_number",
        "line_content",
        "error_message",
        "context_before",
        "context_after",
    ],
};

pub(super) const SUBAGENT_SEARCH_PARTS: PositionalTable = PositionalTable {
    name: "subagent_search_parts",
    columns: &["file_position", "text"],
};

impl PositionalTable {
    /// The stored rows of `session`, each with its position, in position
    /// order.
    pub(super) fn stored(
        &self,
        connection: &Connection,
        session: i64,
    ) -> Result<Vec<(i64, Vec<Value>)>> {
        let mut statement = connection.prepare_cached(&format!(
            "SELECT position, {} FROM {} WHERE session = ?1 ORDER BY position",
            self.columns.join(", "),
            self.name
        ))?;
        let mut found = statement.query([session])?;
        let mut rows = Vec::new();
        while let Some(row) = found.next()? {
            let values = (1..=self.columns.len())
                .map(|index| row.get(index))
                .collect::<rusqlite::Result<_>>()?;
            rows.push((row.get(0)?, values));
        }
        Ok(rows)
    }

    /// Makes `rows` the rows of `session`, matching `stored`, the session's
    /// stored rows from [`Self::stored`], by the key `key_of` gives each.
    pub(super) fn write<K: Hash + Eq + Clone>(
        &self,
        connection: &Connection,
        session: i64,
        rows: &[PositionalRow<K>],
        stored: Vec<(i64, Vec<Value>)>,
        key_of: impl Fn(&[Value]) -> K,
    ) -> Result<()> {
        let mut by_key: HashMap<(K, usize), (i64, Vec<Value>)> = HashMap::new();
        let mut occurrences: HashMap<K, usize> = HashMap::new();
        for (position, values) in stored {
            let key = key_of(&values);
            let occurrence = occurrences.entry(key.clone()).or_default();
            by_key.insert((key, *occurrence), (position, values));
            *occurrence += 1;
        }
        let mut occurrences: HashMap<&K, usize> = HashMap::new();
        let mut moves = Vec::new();
        let mut writes = Vec::new();
        for (position, row) in rows.iter().enumerate() {
            let position = position as i64;
            let occurrence = occurrences.entry(&row.key).or_default();
            let index = *occurrence;
            *occurrence += 1;
            match by_key.remove(&(row.key.clone(), index)) {
                Some((stored_position, values)) => {
                    if stored_position != position {
                        moves.push((stored_position, position));
                    }
                    if values != row.values {
                        writes.push(position);
                    }
                }
                None => writes.push(position),
            }
        }
        for (_, (position, _)) in by_key {
            connection
                .prepare_cached(&format!(
                    "DELETE FROM {} WHERE session = ?1 AND position = ?2",
                    self.name
                ))?
                .execute([session, position])?;
        }
        // Move each row to a negative position first, so no two rows hold
        // one position at once.
        let mut statement = connection.prepare_cached(&format!(
            "UPDATE {} SET position = ?3 WHERE session = ?1 AND position = ?2",
            self.name
        ))?;
        for &(from, to) in &moves {
            statement.execute([session, from, -1 - to])?;
        }
        for &(_, to) in &moves {
            statement.execute([session, -1 - to, to])?;
        }
        let assignments: Vec<String> = self
            .columns
            .iter()
            .map(|column| format!("{column} = excluded.{column}"))
            .collect();
        let parameters: Vec<String> = (3..self.columns.len() + 3)
            .map(|index| format!("?{index}"))
            .collect();
        let mut statement = connection.prepare_cached(&format!(
            "INSERT INTO {table} (session, position, {columns}) VALUES (?1, ?2, {parameters}) \
             ON CONFLICT (session, position) DO UPDATE SET {assignments}",
            table = self.name,
            columns = self.columns.join(", "),
            parameters = parameters.join(", "),
            assignments = assignments.join(", "),
        ))?;
        for position in writes {
            statement.raw_bind_parameter(1, session)?;
            statement.raw_bind_parameter(2, position)?;
            for (offset, value) in rows[position as usize].values.iter().enumerate() {
                statement.raw_bind_parameter(offset + 3, value)?;
            }
            statement.raw_execute()?;
        }
        Ok(())
    }
}
