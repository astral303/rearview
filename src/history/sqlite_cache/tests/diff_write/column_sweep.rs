//! Each column a write stores from a session's parse, changed in the parse
//! and written over the stored session. The write matches a fresh write of
//! the changed parse, and the column holds the changed value. When the
//! change alters one input of a row's digest alone, a digest that misses
//! that input keeps the old value; the row of a "bind only" reach is
//! rewritten for the reason the reach gives.

mod perturbations;

use super::super::stored_rows::StoredTable;
use super::*;
use perturbations::{Claim, ExpectedValue, Perturbation, Reach};
use rusqlite::types::Value;

/// The cache holding a fixture's session as the sweep writes it, and a
/// second database for the session's fresh writes.
struct SweptCaches {
    stored: OwnedCache,
    _base: tempfile::TempDir,
    fresh: FreshWrite,
}

impl SweptCaches {
    /// Writes `record`, `fixture`'s session, into the fresh database in
    /// place of its earlier write; returns the stored rows.
    fn write_fresh(&mut self, fixture: &Fixture, record: &SessionRecord) -> Vec<StoredTable> {
        let cache = &mut self.fresh.cache;
        cache
            .remove(
                fixture.storage(),
                &fixture.root,
                std::slice::from_ref(&fixture.stub.cache_key),
            )
            .unwrap();
        cache.write(record).unwrap();
        rows_of(cache, fixture)
    }

    /// Writes `record`, `fixture`'s session, over the stored session, and
    /// asserts the stored rows are a fresh write's; returns those.
    fn write_over(
        &mut self,
        fixture: &Fixture,
        record: &SessionRecord,
        claim: &str,
    ) -> Vec<StoredTable> {
        self.stored.write(record).unwrap();
        let fresh = self.write_fresh(fixture, record);
        stored_rows::assert_same_lines(
            &stored_rows::lines(&rows_of(&self.stored, fixture)),
            &stored_rows::lines(&fresh),
            &format!("{}: {claim}", fixture.label()),
        );
        fresh
    }
}

/// A fixture session, written once and kept as its unchanged parse
/// between writes.
struct SweptSession {
    caches: SweptCaches,
    unchanged: SessionRecord,
    unchanged_rows: Vec<StoredTable>,
}

impl SweptSession {
    fn of(fixture: &Fixture) -> Self {
        let unchanged = fixture.record();
        let base = tempfile::tempdir().unwrap();
        let stored = cache_with(base.path(), fixture, &unchanged);
        let fresh = FreshWrite::of(fixture, &unchanged);
        let unchanged_rows = rows_of(&fresh.cache, fixture);
        Self {
            caches: SweptCaches {
                stored,
                _base: base,
                fresh,
            },
            unchanged,
            unchanged_rows,
        }
    }

    /// Writes `changed`, `fixture`'s parse changed by `group`, over the
    /// stored session, then the unchanged parse. A group holding a seed
    /// first stores the parse with its seeds alone.
    fn sweep(&mut self, fixture: &Fixture, group: &[Perturbation], changed: &SessionRecord) {
        let name = group
            .iter()
            .map(Perturbation::name)
            .collect::<Vec<_>>()
            .join(", ");
        let before = match seeded_parse(fixture, group) {
            Some(seeded) => self.caches.write_over(
                fixture,
                &seeded,
                &format!("the write of the seeds of {name} matches a fresh write"),
            ),
            None => self.unchanged_rows.clone(),
        };
        let after = self.caches.write_over(
            fixture,
            changed,
            &format!("the write of {name} changed matches a fresh write"),
        );
        for perturbation in group {
            for reach in &perturbation.reaches {
                assert_stored(fixture, reach, &before, &after);
            }
            for &(table, column) in &perturbation.keeps {
                assert_eq!(
                    column_of(&before, table, column),
                    column_of(&after, table, column),
                    "{}: {table}.{column} keeps its values through the change of {}",
                    fixture.label(),
                    perturbation.name()
                );
            }
        }
        self.caches.write_over(
            fixture,
            &self.unchanged,
            &format!("the write of {name} changed back matches a fresh write"),
        );
    }
}

/// `fixture`'s parse with the seeds of `group` applied; `None` when `group`
/// doesn't hold a seed.
fn seeded_parse(fixture: &Fixture, group: &[Perturbation]) -> Option<SessionRecord> {
    let mut seeds = group
        .iter()
        .filter_map(|perturbation| Some((perturbation.name(), perturbation.seed.as_ref()?)))
        .peekable();
    seeds.peek()?;
    let mut record = fixture.record();
    for (name, seed) in seeds {
        assert!(
            seed(&mut record),
            "{}: the seed of {name} applies to the parse its change applied to",
            fixture.label()
        );
    }
    Some(record)
}

/// Asserts `reach`'s column holds its changed value in `after` against
/// `before`. `after` holds the rows of a fresh write of the changed parse.
fn assert_stored(fixture: &Fixture, reach: &Reach, before: &[StoredTable], after: &[StoredTable]) {
    let claim = format!(
        "{}: {} after its perturbation ({})",
        fixture.label(),
        reach.name(),
        reach.proves
    );
    let value = match &reach.stored {
        ExpectedValue::Text(text) => Value::Text((*text).to_owned()),
        ExpectedValue::Integer(number) => Value::Integer(*number),
        ExpectedValue::Blob(bytes) => Value::Blob(bytes.to_vec()),
        ExpectedValue::InRowsOfText { text, values } => {
            let held = values_in_rows_of_text(table_of(after, reach.table), reach.column, text);
            assert_eq!(
                held.iter().map(|(_, value)| value).collect::<Vec<_>>(),
                values.iter().collect::<Vec<_>>(),
                "{claim}: the rows holding {text:?} hold these values, by natural key: {held:?}"
            );
            return;
        }
        ExpectedValue::Changed => {
            assert_ne!(
                column_of(before, reach.table, reach.column),
                column_of(after, reach.table, reach.column),
                "{claim}: the column's values change"
            );
            return;
        }
    };
    let holding = |tables: &[StoredTable]| {
        column_of(tables, reach.table, reach.column)
            .iter()
            .filter(|held| **held == value)
            .count()
    };
    assert!(
        holding(after) > holding(before),
        "{claim}: one row more holds {value:?}: {:?}",
        column_of(after, reach.table, reach.column)
    );
}

/// The natural key and the value of `column` of each row of `table` whose
/// `text` column holds `text`, in the stored order.
fn values_in_rows_of_text(table: &StoredTable, column: &str, text: &str) -> Vec<(Value, Value)> {
    let text = Value::Text(text.to_owned());
    table
        .column(stored_rows::NATURAL_KEY_COLUMN)
        .into_iter()
        .zip(table.column(column))
        .zip(table.column("text"))
        .filter(|(_, held_text)| **held_text == text)
        .map(|((key, value), _)| (key.clone(), value.clone()))
        .collect()
}

/// The perturbations of `pending` one write of `fixture`'s parse holds, in
/// order, with the parse they changed and the rest of `pending`. A
/// perturbation joins the group when:
///
/// - it applies to the parse;
/// - no perturbation of the group reaches one of its tables;
/// - at most one perturbation of the group reaches [`ENTRY_TABLES`];
/// - neither it nor a perturbation of the group needs a write of its own.
fn next_group(
    fixture: &Fixture,
    pending: Vec<Perturbation>,
) -> (Vec<Perturbation>, SessionRecord, Vec<Perturbation>) {
    let reaches_entry_rows = |perturbation: &Perturbation| {
        perturbation
            .tables()
            .any(|table| ENTRY_TABLES.contains(&table))
    };
    let mut record = fixture.record();
    let mut group: Vec<Perturbation> = Vec::new();
    let mut rest = Vec::new();
    for perturbation in pending {
        let fits = group.iter().all(|other| {
            !other.needs_own_write
                && !perturbation.needs_own_write
                && !other
                    .tables()
                    .any(|table| perturbation.tables().any(|own| own == table))
                && !(reaches_entry_rows(other) && reaches_entry_rows(&perturbation))
        });
        if fits && perturbation.apply(&mut record) {
            group.push(perturbation);
        } else {
            rest.push(perturbation);
        }
    }
    (group, record, rest)
}

fn rows_of(cache: &CacheWriter, fixture: &Fixture) -> Vec<StoredTable> {
    stored_rows::stored_rows(&cache.connection, fixture.session_id(cache))
}

fn table_of<'t>(tables: &'t [StoredTable], table: &str) -> &'t StoredTable {
    tables
        .iter()
        .find(|stored| stored.name == table)
        .unwrap_or_else(|| panic!("stored_rows reads {table}"))
}

fn column_of(tables: &[StoredTable], table: &str, column: &str) -> Vec<Value> {
    table_of(tables, table)
        .column(column)
        .into_iter()
        .cloned()
        .collect()
}

#[test]
fn each_swept_column_changed_in_the_parse_is_written_over_the_stored_session() {
    let mut pending = perturbations::all();
    // Each perturbation is swept in the first every-record fixture it
    // applies to: the cache's write code doesn't branch on the agent.
    for session in every_record_sessions() {
        let fixture = Fixture::of(session);
        let mut swept: Option<SweptSession> = None;
        loop {
            let (group, changed, rest) = next_group(&fixture, pending);
            pending = rest;
            if group.is_empty() {
                break;
            }
            swept
                .get_or_insert_with(|| SweptSession::of(&fixture))
                .sweep(&fixture, &group, &changed);
        }
    }
    let names: Vec<String> = pending.iter().map(Perturbation::name).collect();
    assert!(
        names.is_empty(),
        "an every-record fixture holds a parsed field each of these perturbations changes: \
         {names:?}"
    );
}

#[test]
fn each_compared_column_is_perturbed_or_has_a_reason() {
    let connection = Connection::open_in_memory().unwrap();
    connection.execute_batch(schema::CREATE).unwrap();
    let compared: Vec<(&str, String)> = stored_rows::compared_columns(&connection)
        .into_iter()
        .flat_map(|(table, columns)| columns.into_iter().map(move |column| (table, column)))
        .collect();
    let perturbations = perturbations::all();
    let reaches: Vec<&Reach> = perturbations
        .iter()
        .flat_map(|perturbation| &perturbation.reaches)
        .collect();
    let listed: Vec<(&str, &str)> = reaches
        .iter()
        .map(|reach| (reach.table, reach.column))
        .chain(
            perturbations::NOT_PERTURBED
                .iter()
                .flat_map(|(table, columns, _)| {
                    columns.iter().map(move |column| (*table, *column))
                }),
        )
        .collect();

    for (table, column) in &compared {
        assert!(
            listed.contains(&(*table, column.as_str())),
            "{table}.{column} has a perturbation or a reason in NOT_PERTURBED, in \
             column_sweep/perturbations.rs"
        );
    }
    for &(table, column) in &listed {
        assert!(
            compared
                .iter()
                .any(|(other_table, other)| *other_table == table && other == column),
            "{table}.{column}, listed in column_sweep/perturbations.rs, is a column stored_rows \
             compares"
        );
    }
    for reach in reaches {
        assert!(
            matches!(reach.proves, Claim::BindOnly(_))
                || perturbations::DIGESTED_TABLES.contains(&reach.table),
            "{}, proving digest and bind, is a column of a table DIGESTED_TABLES lists",
            reach.name()
        );
    }
}
