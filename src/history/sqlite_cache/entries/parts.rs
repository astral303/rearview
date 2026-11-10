use super::digest::Digest;
use super::search::{ENTRY_SEARCH_FIELDS, EntrySearch, TextPlace};
use super::write::SearchPlace;
use super::{Selection, text_with_codes};
use crate::agent::sanitize::{split_terminal_codes, without_terminal_codes};
use crate::agent::transcript::tool_result_item_text;
use crate::log_entry::{ContentBlock, Tool};
use rusqlite::types::{ToSqlOutput, Value as SqlValue, ValueRef};
use rusqlite::{Connection, Row, Statement, ToSql, params};
use serde_json::{Value, json};
use std::borrow::Cow;
use std::collections::HashMap;

pub(super) struct Writer<'connection> {
    connection: &'connection Connection,
    insert: Statement<'connection>,
    update: Statement<'connection>,
    item: Statement<'connection>,
}

/// The ids of the session and the entry row a write's parts belong to.
#[derive(Clone, Copy)]
pub(super) struct EntryIds {
    pub(super) session: i64,
    pub(super) entry: i64,
}

/// The columns [`bind_row`] binds: a part's search place and digest, then
/// [`StoredValues::values`], then the text and its terminal codes.
const ROW_COLUMNS: [&str; 14] = [
    "entry_sequence",
    "message_ordinal",
    "search_position",
    "digest",
    "kind",
    "search_field",
    "call_id",
    "name",
    "tool",
    "json",
    "result_shape",
    "call_context",
    "text",
    "text_codes",
];

/// `INSERT` of a part's session (`?1`), entry (`?2`), number (`?3`) and
/// [`ROW_COLUMNS`] (`?4` on).
fn insert_sql() -> String {
    let parameters: Vec<String> = (4..ROW_COLUMNS.len() + 4)
        .map(|index| format!("?{index}"))
        .collect();
    format!(
        "INSERT INTO parts (session, entry, part, {}) VALUES (?1, ?2, ?3, {})",
        ROW_COLUMNS.join(", "),
        parameters.join(", ")
    )
}

/// `UPDATE` of [`ROW_COLUMNS`] (`?2` on) on the part whose id is `?1`.
fn update_sql() -> String {
    let assignments: Vec<String> = ROW_COLUMNS
        .iter()
        .zip(2..)
        .map(|(column, index)| format!("{column} = ?{index}"))
        .collect();
    format!("UPDATE parts SET {} WHERE id = ?1", assignments.join(", "))
}

impl<'connection> Writer<'connection> {
    pub(super) fn new(connection: &'connection Connection) -> rusqlite::Result<Self> {
        Ok(Self {
            connection,
            insert: connection.prepare(&insert_sql())?,
            update: connection.prepare(&update_sql())?,
            item: connection.prepare(
                "INSERT INTO result_items (part, position, kind, item_type, text, text_codes) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )?,
        })
    }

    pub(super) fn write(
        &mut self,
        ids: EntryIds,
        parts: &EntryParts<'_>,
        place: SearchPlace,
    ) -> rusqlite::Result<()> {
        for part in parts.rows() {
            self.insert(ids, part, place)?;
        }
        Ok(())
    }

    /// Replaces the stored parts of the entry `ids` with `parts`, writing
    /// only what changed: a part whose digest is unchanged keeps its row and
    /// result items, with its search position updated when it moved; a
    /// changed part is updated in place, its result items replaced; a new
    /// part is inserted, and a stored part without a new part of its number
    /// is deleted.
    pub(super) fn update(
        &mut self,
        ids: EntryIds,
        parts: &EntryParts<'_>,
        place: SearchPlace,
    ) -> rusqlite::Result<()> {
        let mut stored: HashMap<i64, StoredPart> = self
            .connection
            .prepare_cached("SELECT part, id, digest, search_position FROM parts WHERE entry = ?1")?
            .query_map([ids.entry], |row| {
                Ok((
                    row.get(0)?,
                    StoredPart {
                        id: row.get(1)?,
                        digest: row.get(2)?,
                        position: row.get(3)?,
                    },
                ))
            })?
            .collect::<rusqlite::Result<_>>()?;
        for part in parts.rows() {
            match stored.remove(&part.row.number) {
                Some(kept) if kept.digest == part.digest => {
                    if kept.position != part.row.position() {
                        self.connection
                            .prepare_cached("UPDATE parts SET search_position = ?2 WHERE id = ?1")?
                            .execute(params![kept.id, part.row.position()])?;
                    }
                }
                Some(changed) => {
                    self.connection
                        .prepare_cached("DELETE FROM result_items WHERE part = ?1")?
                        .execute([changed.id])?;
                    self.update.raw_bind_parameter(1, changed.id)?;
                    bind_row(&mut self.update, 2, part, place)?;
                    self.update.raw_execute()?;
                    self.insert_items(changed.id, part.row)?;
                }
                None => self.insert(ids, part, place)?,
            }
        }
        for gone in stored.into_values() {
            self.connection
                .prepare_cached("DELETE FROM parts WHERE id = ?1")?
                .execute([gone.id])?;
        }
        Ok(())
    }

    fn insert(
        &mut self,
        ids: EntryIds,
        part: WrittenPart<'_, '_>,
        place: SearchPlace,
    ) -> rusqlite::Result<()> {
        self.insert.raw_bind_parameter(1, ids.session)?;
        self.insert.raw_bind_parameter(2, ids.entry)?;
        self.insert.raw_bind_parameter(3, part.row.number)?;
        bind_row(&mut self.insert, 4, part, place)?;
        self.insert.raw_execute()?;
        self.insert_items(self.connection.last_insert_rowid(), part.row)
    }

    fn insert_items(&mut self, id: i64, part: &PartRow<'_>) -> rusqlite::Result<()> {
        for (position, item) in (0_i64..).zip(&part.items) {
            let [kind, item_type] = item.values();
            let split = item.text.map(split_terminal_codes);
            let text = split.as_ref().map(|split| split.text.as_ref());
            let codes = split.as_ref().and_then(|split| split.codes.as_deref());
            self.item
                .execute(params![id, position, kind, item_type, text, codes])?;
        }
        Ok(())
    }
}

/// A stored part as [`Writer::update`] matches it.
struct StoredPart {
    id: i64,
    digest: i64,
    position: Option<i64>,
}

/// Binds `part`'s [`ROW_COLUMNS`] at `place` from parameter `first` on.
fn bind_row(
    statement: &mut Statement<'_>,
    first: usize,
    part: WrittenPart<'_, '_>,
    place: SearchPlace,
) -> rusqlite::Result<()> {
    let row = part.row;
    let searched = row.searched;
    let placed = [
        ColumnValue::Integer(searched.map(|_| place.sequence)),
        ColumnValue::Integer(searched.and(place.ordinal)),
        ColumnValue::Integer(row.position()),
        ColumnValue::Integer(Some(part.digest)),
    ];
    let text = row.stored.text.map(PartText::stored);
    let text = [
        ColumnValue::Text(text.as_ref().map(|(text, _)| text.as_ref())),
        ColumnValue::Blob(text.as_ref().and_then(|(_, codes)| codes.as_deref())),
    ];
    let values = placed.into_iter().chain(row.stored.values()).chain(text);
    for (offset, value) in values.enumerate() {
        statement.raw_bind_parameter(first + offset, value)?;
    }
    Ok(())
}

/// One stored column's value, as a write binds it and a part's digest
/// hashes it.
#[derive(Clone, Copy)]
enum ColumnValue<'v> {
    Text(Option<&'v str>),
    Integer(Option<i64>),
    Blob(Option<&'v [u8]>),
    /// Stored as its JSON text. Only a write builds the text.
    Json(Option<&'v Value>),
}

impl ColumnValue<'_> {
    fn add_to(self, digest: &mut Digest) {
        match self {
            Self::Text(text) => digest.text(text),
            Self::Integer(number) => digest.number(number),
            Self::Blob(bytes) => digest.bytes(bytes),
            Self::Json(value) => digest.json(value),
        }
    }
}

impl ToSql for ColumnValue<'_> {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(match *self {
            Self::Json(Some(value)) => ToSqlOutput::Owned(SqlValue::Text(value.to_string())),
            Self::Text(Some(text)) => ToSqlOutput::Borrowed(ValueRef::Text(text.as_bytes())),
            Self::Integer(Some(number)) => ToSqlOutput::Borrowed(ValueRef::Integer(number)),
            Self::Blob(Some(bytes)) => ToSqlOutput::Borrowed(ValueRef::Blob(bytes)),
            Self::Text(None) | Self::Integer(None) | Self::Blob(None) | Self::Json(None) => {
                ToSqlOutput::Borrowed(ValueRef::Null)
            }
        })
    }
}

/// The digests of an entry row and of each of its part rows, computed before
/// the write's transaction from the values a write binds. A compare reads
/// them; only a written entry builds its part rows again.
#[derive(Default)]
pub(super) struct EntryDigests {
    /// The hash of `columns_digest` and of each part's number, search
    /// position and digest. A write skips an entry whose stored digest
    /// equals it.
    pub(super) entry: i64,
    /// Each part row's digest, in [`part_rows`] order.
    parts: Vec<i64>,
}

impl EntryDigests {
    /// The digests of an entry whose content is `blocks`, with `search`'s
    /// places of their texts, and whose other columns hash to
    /// `columns_digest`.
    pub(super) fn of(blocks: &[ContentBlock], search: &EntrySearch, columns_digest: i64) -> Self {
        let rows = part_rows(blocks, search);
        let parts: Vec<i64> = rows.iter().map(PartRow::digest).collect();
        let mut entry = Digest::default();
        entry.number(Some(columns_digest));
        entry.number(Some(rows.len() as i64));
        for (row, &part) in rows.iter().zip(&parts) {
            entry.number(Some(row.number));
            entry.number(row.position());
            entry.number(Some(part));
        }
        Self {
            entry: entry.finish(),
            parts,
        }
    }
}

/// An entry's part rows, in the order a write inserts them, each with its
/// digest from [`EntryDigests`].
pub(super) struct EntryParts<'a> {
    rows: Vec<PartRow<'a>>,
    digests: &'a [i64],
}

impl<'a> EntryParts<'a> {
    /// The part rows of an entry whose content is `blocks`, with `search`'s
    /// places of their texts, and whose digests are `digests`.
    pub(super) fn of(
        blocks: &'a [ContentBlock],
        search: &'a EntrySearch,
        digests: &'a EntryDigests,
    ) -> Self {
        let rows = part_rows(blocks, search);
        debug_assert_eq!(rows.len(), digests.parts.len(), "one digest per part row");
        Self {
            rows,
            digests: &digests.parts,
        }
    }

    fn rows(&self) -> impl Iterator<Item = WrittenPart<'_, 'a>> {
        self.rows
            .iter()
            .zip(self.digests)
            .map(|(row, &digest)| WrittenPart { row, digest })
    }
}

/// A part row a write binds, with its digest.
#[derive(Clone, Copy)]
struct WrittenPart<'p, 'a> {
    row: &'p PartRow<'a>,
    digest: i64,
}

/// One part row of an entry as a write stores it: one of the entry's
/// blocks, or a part only search reads.
struct PartRow<'a> {
    /// The part's `part` column: a block's index in its entry, or for a
    /// part only search reads, [`search_only_number`].
    number: i64,
    /// The place search reads the part's text at.
    searched: Option<TextPlace>,
    stored: StoredValues<'a>,
    items: Vec<ItemValues<'a>>,
}

impl<'a> PartRow<'a> {
    /// The hash of `stored` and `items`.
    fn digest(&self) -> i64 {
        let mut digest = Digest::default();
        for value in self.stored.values() {
            value.add_to(&mut digest);
        }
        PartText::add_to(self.stored.text, &mut digest);
        digest.number(Some(self.items.len() as i64));
        for item in &self.items {
            for value in item.values() {
                value.add_to(&mut digest);
            }
            digest.text(item.text);
        }
        digest.finish()
    }

    /// The part of the block at `index` in its entry, with the part columns
    /// `columns`; search reads it at `place`.
    fn of_block(
        index: usize,
        block: &'a ContentBlock,
        columns: Columns<'a>,
        place: Option<TextPlace>,
    ) -> Self {
        let items = stored_items(block, &columns)
            .unwrap_or_default()
            .iter()
            .map(ItemValues::of)
            .collect();
        let searched = block_place(&columns, place);
        let stored = StoredValues {
            kind: columns.kind,
            search_field: searched.map(|place| place.field.code()),
            text: columns.text.map(PartText::Block),
            call_id: columns.call_id,
            name: columns.name,
            tool: columns.tool,
            json: columns.json,
            result_shape: columns.result_shape,
            call_context: columns.call_context,
        };
        Self {
            number: index as i64,
            searched,
            stored,
            items,
        }
    }

    /// The part numbered `number` only search reads, of `kind`, holding
    /// `text` at `place`.
    fn search_only(
        number: i64,
        kind: SearchOnlyKind,
        place: TextPlace,
        text: PartText<'a>,
    ) -> Self {
        let stored = StoredValues {
            kind: kind.name(),
            search_field: Some(place.field.code()),
            text: Some(text),
            call_id: None,
            name: None,
            tool: None,
            json: None,
            result_shape: None,
            call_context: None,
        };
        Self {
            number,
            searched: Some(place),
            stored,
            items: Vec::new(),
        }
    }

    fn position(&self) -> Option<i64> {
        self.searched.map(|place| place.position)
    }
}

/// A part's text as the parse holds it. A part's digest hashes it as held:
/// the held text determines the stored text and its terminal codes. A write
/// splits the codes off only for a part it writes.
#[derive(Clone, Copy)]
enum PartText<'a> {
    /// A block's text: its part stores the codes in `text_codes`.
    Block(&'a str),
    /// A search-only part's text, stored without its codes.
    Searched(&'a str),
    /// A result's items: its search-only part stores their
    /// [`searched_items_text`].
    Items(&'a [Value]),
}

impl<'a> PartText<'a> {
    fn add_to(text: Option<Self>, digest: &mut Digest) {
        match text {
            None => digest.number(None),
            Some(Self::Block(text)) => {
                digest.number(Some(0));
                digest.text(Some(text));
            }
            Some(Self::Searched(text)) => {
                digest.number(Some(1));
                digest.text(Some(text));
            }
            Some(Self::Items(items)) => {
                digest.number(Some(2));
                digest.number(Some(items.len() as i64));
                for item in items {
                    digest.text(tool_result_item_text(item));
                }
            }
        }
    }

    /// The stored text and its terminal codes.
    fn stored(self) -> (Cow<'a, str>, Option<Vec<u8>>) {
        match self {
            Self::Block(text) => {
                let split = split_terminal_codes(text);
                (split.text, split.codes)
            }
            Self::Searched(text) => (split_terminal_codes(text).text, None),
            Self::Items(items) => (Cow::Owned(searched_items_text(items)), None),
        }
    }
}

/// A part row's columns besides its entry, number, search place and digest.
struct StoredValues<'a> {
    kind: &'static str,
    search_field: Option<i64>,
    text: Option<PartText<'a>>,
    call_id: Option<&'a str>,
    name: Option<&'a str>,
    tool: Option<String>,
    json: Option<&'a Value>,
    result_shape: Option<&'static str>,
    call_context: Option<String>,
}

impl StoredValues<'_> {
    /// The values of [`ROW_COLUMNS`] from `kind` to `call_context`, in their
    /// order.
    fn values(&self) -> [ColumnValue<'_>; 8] {
        [
            ColumnValue::Text(Some(self.kind)),
            ColumnValue::Integer(self.search_field),
            ColumnValue::Text(self.call_id),
            ColumnValue::Text(self.name),
            ColumnValue::Text(self.tool.as_deref()),
            ColumnValue::Json(self.json),
            ColumnValue::Text(self.result_shape),
            ColumnValue::Text(self.call_context.as_deref()),
        ]
    }
}

/// A `result_items` row's columns besides its part and position.
struct ItemValues<'a> {
    kind: &'static str,
    /// Held with its terminal codes; a write stores them in `text_codes`.
    text: Option<&'a str>,
    item_type: Option<&'a str>,
}

impl<'a> ItemValues<'a> {
    fn of(value: &'a Value) -> Self {
        let (kind, text, item_type) = match value {
            Value::String(text) => ("string", Some(text.as_str()), None),
            Value::Object(object) => (
                "object",
                object.get("text").and_then(Value::as_str),
                object.get("type").and_then(Value::as_str),
            ),
            _ => ("external", None, None),
        };
        Self {
            kind,
            text,
            item_type,
        }
    }

    fn values(&self) -> [ColumnValue<'_>; 2] {
        [
            ColumnValue::Text(Some(self.kind)),
            ColumnValue::Text(self.item_type),
        ]
    }
}

/// The number of the part only search reads at `index` in the group
/// `group` of [`SEARCH_ONLY_GROUPS`]: below zero, apart from the blocks'
/// parts, with each group's numbers equal modulo [`SEARCH_ONLY_GROUPS`]. A
/// block appended to the entry keeps every other part's number: a later
/// write then matches each stored part with the new part of its number.
fn search_only_number(group: usize, index: usize) -> i64 {
    -1 - (index * SEARCH_ONLY_GROUPS + group) as i64
}

/// The groups of the parts only search reads: [`SearchOnlyKind::ResultText`]
/// parts, numbered by their block's index, then the
/// [`SearchOnlyKind::Derived`] parts of each of [`ENTRY_SEARCH_FIELDS`],
/// numbered by their index among that field's.
const SEARCH_ONLY_GROUPS: usize = 1 + ENTRY_SEARCH_FIELDS.len();

/// The part rows of an entry whose content is `blocks`, with `search`'s
/// places of their texts: one for each block, then a
/// [`SearchOnlyKind::ResultText`] part for each searched result with items
/// in `result_items`, in block order, then a [`SearchOnlyKind::Derived`]
/// part for each derived text.
fn part_rows<'a>(blocks: &'a [ContentBlock], search: &'a EntrySearch) -> Vec<PartRow<'a>> {
    let mut rows = Vec::with_capacity(blocks.len() + search.derived.len());
    let mut result_texts = Vec::new();
    for (index, (block, &place)) in blocks.iter().zip(&search.blocks).enumerate() {
        let columns = Columns::of(block);
        if let (Some(place), Some(items)) = (place, stored_items(block, &columns)) {
            result_texts.push(PartRow::search_only(
                search_only_number(0, index),
                SearchOnlyKind::ResultText,
                place,
                PartText::Items(items),
            ));
        }
        rows.push(PartRow::of_block(index, block, columns, place));
    }
    rows.append(&mut result_texts);
    let mut derived_in_field = [0; ENTRY_SEARCH_FIELDS.len()];
    for derived in &search.derived {
        let rank = ENTRY_SEARCH_FIELDS
            .iter()
            .position(|&field| field == derived.place.field)
            .expect("a derived text is in one of an entry's search fields");
        let index = derived_in_field[rank];
        derived_in_field[rank] += 1;
        rows.push(PartRow::search_only(
            search_only_number(1 + rank, index),
            SearchOnlyKind::Derived,
            derived.place,
            PartText::Searched(&derived.text),
        ));
    }
    rows
}

/// The search place of a block's own part; `None` for a result with items
/// in `result_items`. Search reads such a result through its
/// [`SearchOnlyKind::ResultText`] part.
fn block_place(columns: &Columns<'_>, place: Option<TextPlace>) -> Option<TextPlace> {
    place.filter(|_| columns.result_shape != Some(LOSSLESS_ARRAY))
}

/// The search text of a result with `items`: each text item without
/// terminal codes, joined by line breaks.
pub(super) fn searched_items_text(items: &[Value]) -> String {
    items
        .iter()
        .filter_map(tool_result_item_text)
        .map(without_terminal_codes)
        .collect::<Vec<_>>()
        .join("\n")
}

/// The items of `block` when it is a result with items in `result_items`.
/// `columns` are the block's part's columns.
fn stored_items<'a>(block: &'a ContentBlock, columns: &Columns<'_>) -> Option<&'a [Value]> {
    match block {
        ContentBlock::ToolResult {
            content: Some(Value::Array(items)),
            ..
        } if columns.result_shape == Some(LOSSLESS_ARRAY) => Some(items),
        _ => None,
    }
}

#[derive(Default)]
struct Columns<'a> {
    kind: &'static str,
    text: Option<&'a str>,
    call_id: Option<&'a str>,
    name: Option<&'a str>,
    tool: Option<String>,
    json: Option<&'a Value>,
    result_shape: Option<&'static str>,
    call_context: Option<String>,
}

impl<'a> Columns<'a> {
    fn of(block: &'a ContentBlock) -> Self {
        match block {
            ContentBlock::Text { text } => Self {
                kind: "text",
                text: Some(text),
                ..Self::default()
            },
            ContentBlock::Thinking { thinking, .. } => Self {
                kind: "thinking",
                text: Some(thinking),
                ..Self::default()
            },
            ContentBlock::ToolUse {
                id,
                name,
                tool,
                input,
            } => Self {
                kind: "call",
                call_id: Some(id),
                name: Some(name),
                tool: Some(tool_text(*tool)),
                json: Some(input),
                call_context: super::context::stored_call_context(*tool, input),
                ..Self::default()
            },
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                standalone_tool_name,
            } => {
                let (text, json, result_shape) = match content {
                    Some(Value::String(text)) => (Some(text.as_str()), None, None),
                    Some(Value::Array(items)) if stores_items(items) => {
                        (None, None, Some(LOSSLESS_ARRAY))
                    }
                    Some(other) => (None, Some(other), Some(VERBATIM_JSON)),
                    None => (None, None, None),
                };
                Self {
                    kind: "result",
                    text,
                    call_id: Some(tool_use_id),
                    name: standalone_tool_name.as_deref(),
                    json,
                    result_shape,
                    ..Self::default()
                }
            }
            ContentBlock::Image { .. } => Self {
                kind: "image",
                ..Self::default()
            },
            ContentBlock::Other => Self {
                kind: "other",
                ..Self::default()
            },
        }
    }
}

/// The kind of a part only search reads: a display read leaves it out.
/// Writers store such a part through [`Self::columns`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::history::sqlite_cache) enum SearchOnlyKind {
    /// A search text no block holds as written.
    Derived,
    /// The search text of a result with items in `result_items`.
    ResultText,
    /// A sub-agent transcript's text outside its spliced turns, outside
    /// every entry. `agent search` reads it.
    SubagentMetadata,
}

impl SearchOnlyKind {
    const ALL: [Self; 3] = [Self::Derived, Self::ResultText, Self::SubagentMetadata];

    /// The part's `kind` column.
    pub(in crate::history::sqlite_cache) const fn name(self) -> &'static str {
        match self {
            Self::Derived => "derived",
            Self::ResultText => "result-text",
            Self::SubagentMetadata => "subagent-metadata",
        }
    }

    /// The `kind` and `text` columns of a part of this kind holding `text`:
    /// the text without its terminal codes, as search reads it.
    pub(in crate::history::sqlite_cache) fn columns(
        self,
        text: &str,
    ) -> (&'static str, Cow<'_, str>) {
        (self.name(), split_terminal_codes(text).text)
    }
}

/// A result array whose every item is a string or a text item:
/// `result_items` holds it.
const LOSSLESS_ARRAY: &str = "array";
/// Any other result content: `parts.json` holds it as recorded.
const VERBATIM_JSON: &str = "json";

/// True when a result whose content is `items` stores them in
/// `result_items`.
pub(super) fn stores_items(items: &[Value]) -> bool {
    items.iter().all(is_lossless_item)
}

fn is_lossless_item(item: &Value) -> bool {
    match item {
        Value::String(_) => true,
        Value::Object(object) => object
            .iter()
            .all(|(key, value)| matches!(key.as_str(), "text" | "type") && value.is_string()),
        _ => false,
    }
}

fn tool_text(tool: Tool) -> String {
    match serde_json::to_value(tool) {
        Ok(Value::String(name)) => name,
        _ => unreachable!("a tool serializes as its name"),
    }
}

pub(super) type Blocks = HashMap<i64, Vec<ContentBlock>>;

pub(super) fn read(connection: &Connection, selected: &Selection) -> rusqlite::Result<Blocks> {
    let mut items: HashMap<i64, Vec<Value>> = HashMap::new();
    let mut statement = connection.prepare(&format!(
        "SELECT i.part, i.kind, i.text, i.text_codes, i.item_type FROM result_items i \
         JOIN parts p ON p.id = i.part \
         WHERE p.entry IN ({}) ORDER BY i.part, i.position",
        selected.ids,
    ))?;
    let mut rows = statement.query(rusqlite::params_from_iter(&selected.parameters))?;
    while let Some(row) = rows.next()? {
        let kind: String = row.get(1)?;
        let text = text_with_codes(row, 2, 3)?;
        let item_type: Option<String> = row.get(4)?;
        let value = if kind == "string" {
            json!(text.unwrap_or_default())
        } else {
            let mut object = json!({});
            if let Some(text) = text {
                object["text"] = json!(text);
            }
            if let Some(item_type) = item_type {
                object["type"] = json!(item_type);
            }
            object
        };
        items.entry(row.get(0)?).or_default().push(value);
    }
    let mut blocks: Blocks = HashMap::new();
    let mut statement = connection.prepare(&format!(
        "SELECT p.entry, p.id, p.kind, p.text, p.text_codes, p.call_id, p.name, p.tool, p.json, \
         p.result_shape FROM parts p WHERE p.entry IN ({}) AND p.kind NOT IN ({}) \
         ORDER BY p.entry, p.part",
        selected.ids,
        SearchOnlyKind::ALL
            .map(|kind| format!("'{}'", kind.name()))
            .join(", "),
    ))?;
    let mut rows = statement.query(rusqlite::params_from_iter(&selected.parameters))?;
    while let Some(row) = rows.next()? {
        let array = items.remove(&row.get::<_, i64>(1)?);
        let text = text_with_codes(row, 3, 4)?;
        blocks
            .entry(row.get(0)?)
            .or_default()
            .push(block_of(row, text, array)?);
    }
    Ok(blocks)
}

fn block_of(
    row: &Row,
    text: Option<String>,
    array: Option<Vec<Value>>,
) -> rusqlite::Result<ContentBlock> {
    let kind: String = row.get(2)?;
    Ok(match kind.as_str() {
        "text" => ContentBlock::Text {
            text: text.unwrap_or_default(),
        },
        "thinking" => ContentBlock::Thinking {
            thinking: text.unwrap_or_default(),
            signature: String::new(),
        },
        "call" => ContentBlock::ToolUse {
            id: row.get::<_, Option<String>>(5)?.unwrap_or_default(),
            name: row.get::<_, Option<String>>(6)?.unwrap_or_default(),
            tool: row
                .get::<_, Option<String>>(7)?
                .and_then(|tool| serde_json::from_value(Value::String(tool)).ok())
                .unwrap_or_default(),
            input: super::json_value(row.get::<_, Option<String>>(8)?.as_deref())?
                .unwrap_or(Value::Null),
        },
        "result" => ContentBlock::ToolResult {
            tool_use_id: row.get::<_, Option<String>>(5)?.unwrap_or_default(),
            standalone_tool_name: row.get(6)?,
            content: match text {
                Some(text) => Some(Value::String(text)),
                None => match row.get::<_, Option<String>>(9)?.as_deref() {
                    Some(LOSSLESS_ARRAY) => Some(Value::Array(array.unwrap_or_default())),
                    Some(VERBATIM_JSON) => {
                        super::json_value(row.get::<_, Option<String>>(8)?.as_deref())?
                    }
                    _ => None,
                },
            },
        },
        "image" => ContentBlock::Image {
            source: Value::Null,
        },
        _ => ContentBlock::Other,
    })
}
