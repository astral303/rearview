//! Tables whose rows a session numbers by position: `(session, position)`
//! is the key. A write matches each new row with the stored row of the same
//! key, keeps an unchanged one, moves one whose position changed, updates
//! one whose columns changed, and deletes or inserts the rest.

use crate::error::{AppError, Result};
use rusqlite::Connection;
use rusqlite::types::Value;
use std::collections::HashMap;
use std::hash::Hash;

pub(super) struct PositionalTable {
    pub(super) name: &'static str,
    /// The column holding a row's position.
    pub(super) position: &'static str,
    /// The condition selecting the rows a session numbers, for a table
    /// holding other rows too. The table's unique index on the session and
    /// the position covers only the rows it selects.
    pub(super) numbered: Option<&'static str>,
    /// The columns after `session` and the position.
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
    position: "position",
    numbered: None,
    columns: &[
        "kind",
        "root",
        "path",
        "key",
        "file_size",
        "mtime_secs",
        "mtime_nsecs",
    ],
};

pub(super) const TRANSCRIPT_READS: PositionalTable = PositionalTable {
    name: "transcript_reads",
    position: "position",
    numbered: None,
    columns: &[
        "root",
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
    position: "position",
    numbered: None,
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
    position: "position",
    numbered: None,
    columns: &[
        "line_number",
        "line_content",
        "error_message",
        "context_before",
        "context_after",
    ],
};

/// The session's sub-agent metadata parts, outside every entry.
pub(super) const SUBAGENT_METADATA_PARTS: PositionalTable = PositionalTable {
    name: "parts",
    position: "part",
    numbered: Some("entry IS NULL"),
    columns: &["kind", "search_field", "file_position", "text"],
};

impl PositionalTable {
    /// The integer in `column` of `values`, a stored row of this table.
    /// Fails for a value of another type.
    pub(super) fn integer(&self, values: &[Value], column: &str) -> Result<i64> {
        match self.value(values, column) {
            (_, Value::Integer(integer)) => Ok(*integer),
            (index, other) => Err(self.unexpected_type(index, column, other)),
        }
    }

    /// The text in `column` of `values`, a stored row of this table. Fails
    /// for a value of another type.
    pub(super) fn text(&self, values: &[Value], column: &str) -> Result<String> {
        match self.value(values, column) {
            (_, Value::Text(text)) => Ok(text.clone()),
            (index, other) => Err(self.unexpected_type(index, column, other)),
        }
    }

    /// [`Self::numbered`] as a condition added to a `WHERE` clause.
    fn and_numbered(&self) -> String {
        self.numbered
            .map(|numbered| format!(" AND {numbered}"))
            .unwrap_or_default()
    }

    fn value<'v>(&self, values: &'v [Value], column: &str) -> (usize, &'v Value) {
        let index = self
            .columns
            .iter()
            .position(|candidate| *candidate == column)
            .unwrap_or_else(|| panic!("{} doesn't have the column {column}", self.name));
        (index, &values[index])
    }

    fn unexpected_type(&self, index: usize, column: &str, value: &Value) -> AppError {
        AppError::SessionCache(rusqlite::Error::InvalidColumnType(
            index,
            format!("{}.{column}", self.name),
            value.data_type(),
        ))
    }

    /// The stored rows of `session`, each with its position, in position
    /// order.
    pub(super) fn stored(
        &self,
        connection: &Connection,
        session: i64,
    ) -> Result<Vec<(i64, Vec<Value>)>> {
        let mut statement = connection.prepare_cached(&format!(
            "SELECT {position}, {columns} FROM {table} WHERE session = ?1{numbered} \
             ORDER BY {position}",
            position = self.position,
            columns = self.columns.join(", "),
            table = self.name,
            numbered = self.and_numbered(),
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
        key_of: impl Fn(&[Value]) -> Result<K>,
    ) -> Result<()> {
        let mut by_key: HashMap<(K, usize), (i64, Vec<Value>)> = HashMap::new();
        let mut occurrences: HashMap<K, usize> = HashMap::new();
        for (position, values) in stored {
            let key = key_of(&values)?;
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
                    "DELETE FROM {} WHERE session = ?1 AND {} = ?2{}",
                    self.name,
                    self.position,
                    self.and_numbered()
                ))?
                .execute([session, position])?;
        }
        // Move each row to a negative position first, so no two rows hold
        // one position at once.
        let mut statement = connection.prepare_cached(&format!(
            "UPDATE {table} SET {position} = ?3 WHERE session = ?1 AND {position} = ?2{numbered}",
            table = self.name,
            position = self.position,
            numbered = self.and_numbered()
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
            "INSERT INTO {table} (session, {position}, {columns}) VALUES (?1, ?2, {parameters}) \
             ON CONFLICT (session, {position}){numbered} DO UPDATE SET {assignments}",
            table = self.name,
            position = self.position,
            columns = self.columns.join(", "),
            parameters = parameters.join(", "),
            numbered = self
                .numbered
                .map(|numbered| format!(" WHERE {numbered}"))
                .unwrap_or_default(),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stored_column_of_another_type_fails_naming_the_column() {
        let stored_row = vec![
            Value::Text("locator".to_owned()),
            Value::Null,
            Value::Text("session.jsonl".to_owned()),
        ];

        let failure = SESSION_FILES
            .integer(&stored_row, "root")
            .unwrap_err()
            .to_string();

        assert!(
            failure.contains("session_files.root"),
            "the failure names the table and column: {failure}"
        );
        assert_eq!(
            SESSION_FILES.text(&stored_row, "kind").unwrap(),
            "locator",
            "a column of the expected type reads as itself"
        );
    }
}
