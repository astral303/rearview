//! A session's stored rows: every column the schema gives each table, read
//! from the database's own column lists, with each reference to a row's id
//! replaced by that row's natural key. Leave out row ids: the two databases
//! of a comparison give one row different ids.

use super::*;
use rusqlite::types::Value;
use std::collections::HashMap;

/// The SQL of the natural key of the row of `table` whose id `id` gives,
/// compared in place of the id; `None` for a table without one. A row of
/// one of these tables also compares by its own natural key, in
/// [`NATURAL_KEY_COLUMN`]. A subquery with an alias uses its table's
/// singular: a key nested in another key reads its own row, not the outer
/// one.
fn natural_key_of(table: &str, id: &str) -> Option<String> {
    Some(match table {
        "roots" => format!("(SELECT key FROM roots WHERE id = {id})"),
        "transcripts" => format!(
            "(SELECT json_array({}, transcript.path) FROM transcripts transcript \
             WHERE transcript.id = {id})",
            natural_key_of("roots", "transcript.root")?
        ),
        "threads" => format!(
            "(SELECT json_array(thread.number, {}, thread.label, thread.nickname, thread.role, \
             thread.agent_path, thread.agent_id, thread.launch_call) FROM threads thread \
             WHERE thread.id = {id})",
            natural_key_of("transcripts", "thread.transcript")?
        ),
        "messages" => {
            format!("(SELECT MIN(entry.sequence) FROM entries entry WHERE entry.message = {id})")
        }
        "entries" => format!("(SELECT sequence FROM entries WHERE id = {id})"),
        "parts" => format!(
            "(SELECT json_array({}, part.part) FROM parts part WHERE part.id = {id})",
            natural_key_of("entries", "part.entry")?
        ),
        _ => return None,
    })
}

/// The first column of a table with a natural key ([`natural_key_of`]):
/// the row's own natural key. A comparison pairs the rows of one key: rows
/// sort by it first.
pub(super) const NATURAL_KEY_COLUMN: &str = "natural_key";

/// The tables whose rows sessions share or a semantic search writes, each
/// with the reason. [`stored_rows`] doesn't read them.
const SHARED_OR_SEARCH_WRITTEN_TABLES: &[(&str, &str)] = &[
    (
        "meta",
        "every session shares the database's versions and counters",
    ),
    ("roots", "every session under a root shares its row"),
    (
        "semantic_routes",
        "a semantic search stores its row, not a write of the session",
    ),
];

/// The columns two writes of one session store differently, each with the
/// reason.
const NOT_COMPARED: &[(&str, &str, &str)] = &[(
    "sessions",
    "generation",
    "each write gives the row a number no earlier write carried",
)];

/// One table's rows of a session, in the order of their values.
#[derive(Clone)]
pub(super) struct StoredTable {
    pub(super) name: &'static str,
    pub(super) columns: Vec<String>,
    pub(super) rows: Vec<Vec<Value>>,
}

impl StoredTable {
    pub(super) fn remove_column(&mut self, column: &str) {
        let index = self.index_of(column);
        self.columns.remove(index);
        for row in &mut self.rows {
            row.remove(index);
        }
    }

    pub(super) fn column(&self, column: &str) -> Vec<&Value> {
        let index = self.index_of(column);
        self.rows.iter().map(|row| &row[index]).collect()
    }

    fn index_of(&self, column: &str) -> usize {
        self.columns
            .iter()
            .position(|name| name == column)
            .unwrap_or_else(|| panic!("{} compares the column {column}", self.name))
    }
}

/// The rows of the session row `session` in each table but
/// [`SHARED_OR_SEARCH_WRITTEN_TABLES`]. Panics when one of the rows
/// references a row of another session.
pub(super) fn stored_rows(connection: &Connection, session: i64) -> Vec<StoredTable> {
    session_tables()
        .map(|table| stored_table(connection, table, session))
        .collect()
}

/// Each table but [`SHARED_OR_SEARCH_WRITTEN_TABLES`], with the columns of
/// it [`stored_rows`] compares, read from the schema `connection` holds.
pub(super) fn compared_columns(connection: &Connection) -> Vec<(&'static str, Vec<String>)> {
    session_tables()
        .map(|table| (table, compared_columns_of(connection, table)))
        .collect()
}

/// Each row of `tables` as one line naming its table and each column.
pub(super) fn lines(tables: &[StoredTable]) -> Vec<String> {
    tables
        .iter()
        .flat_map(|table| {
            table.rows.iter().map(|row| {
                let values = table
                    .columns
                    .iter()
                    .zip(row)
                    .map(|(column, value)| format!("{column}={value:?}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("{} {values}", table.name)
            })
        })
        .collect()
}

/// Asserts `actual` holds `expected`'s lines, naming the first line that
/// differs.
pub(super) fn assert_same_lines(actual: &[String], expected: &[String], claim: &str) {
    for (actual, expected) in actual.iter().zip(expected) {
        assert_eq!(
            actual, expected,
            "{claim}. Add a column to NOT_COMPARED in stored_rows.rs only when each write gives \
             it a new value"
        );
    }
    assert_eq!(actual.len(), expected.len(), "{claim}: as many lines");
}

fn session_tables() -> impl Iterator<Item = &'static str> {
    schema::TABLES.iter().copied().filter(|table| {
        !SHARED_OR_SEARCH_WRITTEN_TABLES
            .iter()
            .any(|(name, _)| name == table)
    })
}

/// A column of a table, as `pragma_table_info` reports it.
struct Column {
    name: String,
    is_row_id: bool,
}

fn stored_table(connection: &Connection, table: &'static str, session: i64) -> StoredTable {
    let references = id_references(connection, table);
    let owner = session_expression(connection, table, "t").unwrap_or_else(|| {
        panic!(
            "stored_rows reads a session's rows of {table}: {table} has a session column or \
             references a row of a table with one, or SHARED_OR_SEARCH_WRITTEN_TABLES in \
             stored_rows.rs lists it when sessions share its rows or a semantic search writes \
             them"
        )
    });
    assert_references_stay_in_session(connection, table, &owner, &references, session);
    let mut columns = Vec::new();
    let mut selected = Vec::new();
    if let Some(key) = natural_key_of(table, "t.id") {
        columns.push(NATURAL_KEY_COLUMN.to_owned());
        selected.push(key);
    }
    for column in compared_columns_of(connection, table) {
        selected.push(match references.get(&column) {
            Some(referenced) => {
                natural_key_of(referenced, &format!("t.{column}")).unwrap_or_else(|| {
                    panic!(
                        "{table}.{column} references a row of {referenced} by its id: \
                         natural_key_of in stored_rows.rs gives {referenced}'s natural key"
                    )
                })
            }
            None => format!("t.{column}"),
        });
        columns.push(column);
    }
    let order: Vec<String> = (1..=selected.len())
        .map(|index| index.to_string())
        .collect();
    let sql = format!(
        "SELECT {} FROM {table} t WHERE {owner} = ?1 ORDER BY {}",
        selected.join(", "),
        order.join(", ")
    );
    let rows = connection
        .prepare(&sql)
        .unwrap()
        .query_map([session], |row| {
            (0..selected.len())
                .map(|index| row.get::<_, Value>(index))
                .collect::<rusqlite::Result<Vec<_>>>()
        })
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    StoredTable {
        name: table,
        columns,
        rows,
    }
}

/// `table`'s columns [`stored_rows`] compares: every column but the row's
/// id, its `session` and the [`NOT_COMPARED`] columns.
fn compared_columns_of(connection: &Connection, table: &str) -> Vec<String> {
    table_columns(connection, table)
        .into_iter()
        .filter(|column| {
            let not_compared = NOT_COMPARED
                .iter()
                .any(|(name, other, _)| *name == table && *other == column.name);
            !column.is_row_id && column.name != "session" && !not_compared
        })
        .map(|column| column.name)
        .collect()
}

/// Asserts that each reference of `table`'s rows of `session` names a row
/// of `session`. `owner` gives the session of a row of `table`. The natural
/// key a comparison reads leaves the referenced row's session out.
fn assert_references_stay_in_session(
    connection: &Connection,
    table: &str,
    owner: &str,
    references: &HashMap<String, String>,
    session: i64,
) {
    for (column, referenced) in references {
        let Some(referenced_owner) = session_expression(connection, referenced, "r") else {
            continue;
        };
        let elsewhere: i64 = connection
            .query_row(
                &format!(
                    "SELECT COUNT(*) FROM {table} t WHERE {owner} = ?1 AND t.{column} IS NOT NULL \
                     AND (SELECT {referenced_owner} FROM {referenced} r WHERE r.id = t.{column}) \
                     IS NOT ?1"
                ),
                [session],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            elsewhere, 0,
            "each {table}.{column} of session row {session} references a row of {referenced} of \
             the same session"
        );
    }
}

/// `table`'s columns, each marked when it is the table's `INTEGER PRIMARY
/// KEY`, the row's id.
fn table_columns(connection: &Connection, table: &str) -> Vec<Column> {
    let has_row_id: bool = connection
        .query_row(
            "SELECT NOT wr FROM pragma_table_list WHERE name = ?1",
            [table],
            |row| row.get(0),
        )
        .unwrap();
    let columns: Vec<(String, String, i64)> = connection
        .prepare("SELECT name, type, pk FROM pragma_table_info(?1) ORDER BY cid")
        .unwrap()
        .query_map([table], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    let key_columns = columns.iter().filter(|(_, _, pk)| *pk > 0).count();
    columns
        .into_iter()
        .map(|(name, kind, pk)| Column {
            is_row_id: has_row_id
                && key_columns == 1
                && pk == 1
                && kind.eq_ignore_ascii_case("INTEGER"),
            name,
        })
        .collect()
}

/// Each column of `table` referencing a row's `id`, with the table it
/// references.
fn id_references(connection: &Connection, table: &str) -> HashMap<String, String> {
    connection
        .prepare(r#"SELECT "from", "table" FROM pragma_foreign_key_list(?1) WHERE "to" = 'id'"#)
        .unwrap()
        .query_map([table], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

/// The id of the session the row `alias` of `table` belongs to: its own id
/// in `sessions`, its `session` column, or the `session` of the row it
/// references. `None` for a table without either, whose rows sessions
/// share.
fn session_expression(connection: &Connection, table: &str, alias: &str) -> Option<String> {
    if table == "sessions" {
        return Some(format!("{alias}.id"));
    }
    let has_session = |table: &str| {
        table_columns(connection, table)
            .iter()
            .any(|column| column.name == "session")
    };
    if has_session(table) {
        return Some(format!("{alias}.session"));
    }
    let mut owners: Vec<_> = id_references(connection, table)
        .into_iter()
        .filter(|(_, referenced)| has_session(referenced))
        .collect();
    owners.sort();
    owners
        .first()
        .map(|(column, owner)| format!("(SELECT session FROM {owner} WHERE id = {alias}.{column})"))
}
