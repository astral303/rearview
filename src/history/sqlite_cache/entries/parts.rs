use super::digest::Digest;
use super::search::{ENTRY_SEARCH_FIELDS, EntrySearch, TextPlace};
use super::write::SearchPlace;
use super::{Selection, text_with_codes};
use crate::agent::sanitize::{split_terminal_codes, without_terminal_codes};
use crate::agent::transcript::tool_result_item_text;
use crate::log_entry::{ContentBlock, Tool};
use rusqlite::types::{ToSqlOutput, ValueRef};
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

/// The entry row a write's parts belong to.
#[derive(Clone, Copy)]
pub(super) struct PartsOf {
    pub(super) session: i64,
    pub(super) entry: i64,
}

/// The columns [`bind_row`] binds: a part's search place, its digest, then
/// [`StoredValues::values`].
const ROW_COLUMNS: [&str; 14] = [
    "entry_sequence",
    "message_ordinal",
    "search_position",
    "digest",
    "kind",
    "search_field",
    "text",
    "text_codes",
    "call_id",
    "name",
    "tool",
    "json",
    "result_shape",
    "call_context",
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
                "INSERT INTO result_items (part, position, kind, text, text_codes, item_type) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )?,
        })
    }

    /// Inserts `parts`, the parts of `owner`, at `place`.
    pub(super) fn write(
        &mut self,
        owner: PartsOf,
        parts: &EntryParts<'_>,
        place: SearchPlace,
    ) -> rusqlite::Result<()> {
        for part in &parts.rows {
            self.insert(owner, part, place)?;
        }
        Ok(())
    }

    /// Makes the stored parts of `owner` `parts` at `place`, matching each
    /// stored part with the new part of its number. A matched part whose
    /// digest is unchanged keeps its row and its result items; its search
    /// position is updated when it moved. A changed one is updated in place
    /// with its result items replaced. A new part is inserted, and a stored
    /// part matching none is deleted.
    pub(super) fn update(
        &mut self,
        owner: PartsOf,
        parts: &EntryParts<'_>,
        place: SearchPlace,
    ) -> rusqlite::Result<()> {
        let mut stored: HashMap<i64, StoredPart> = self
            .connection
            .prepare_cached("SELECT part, id, digest, search_position FROM parts WHERE entry = ?1")?
            .query_map([owner.entry], |row| {
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
        for part in &parts.rows {
            match stored.remove(&part.number) {
                Some(kept) if kept.digest == part.digest => {
                    if kept.position != part.position() {
                        self.connection
                            .prepare_cached("UPDATE parts SET search_position = ?2 WHERE id = ?1")?
                            .execute(params![kept.id, part.position()])?;
                    }
                }
                Some(changed) => {
                    self.connection
                        .prepare_cached("DELETE FROM result_items WHERE part = ?1")?
                        .execute([changed.id])?;
                    self.update.raw_bind_parameter(1, changed.id)?;
                    bind_row(&mut self.update, 2, part, place)?;
                    self.update.raw_execute()?;
                    self.insert_items(changed.id, part)?;
                }
                None => self.insert(owner, part, place)?,
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
        owner: PartsOf,
        part: &PartRow<'_>,
        place: SearchPlace,
    ) -> rusqlite::Result<()> {
        self.insert.raw_bind_parameter(1, owner.session)?;
        self.insert.raw_bind_parameter(2, owner.entry)?;
        self.insert.raw_bind_parameter(3, part.number)?;
        bind_row(&mut self.insert, 4, part, place)?;
        self.insert.raw_execute()?;
        self.insert_items(self.connection.last_insert_rowid(), part)
    }

    fn insert_items(&mut self, id: i64, part: &PartRow<'_>) -> rusqlite::Result<()> {
        for (position, item) in (0_i64..).zip(&part.items) {
            let [kind, text, codes, item_type] = item.values();
            self.item
                .execute(params![id, position, kind, text, codes, item_type])?;
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
    part: &PartRow<'_>,
    place: SearchPlace,
) -> rusqlite::Result<()> {
    let searched = part.searched;
    let placed = [
        Stored::Integer(searched.map(|_| place.sequence)),
        Stored::Integer(searched.and(place.ordinal)),
        Stored::Integer(part.position()),
        Stored::Integer(Some(part.digest)),
    ];
    for (offset, value) in placed.into_iter().chain(part.stored.values()).enumerate() {
        statement.raw_bind_parameter(first + offset, value)?;
    }
    Ok(())
}

/// One stored column's value, as a write binds it and a part's digest
/// hashes it.
#[derive(Clone, Copy)]
enum Stored<'v> {
    Text(Option<&'v str>),
    Integer(Option<i64>),
    Blob(Option<&'v [u8]>),
}

impl Stored<'_> {
    fn add_to(self, digest: &mut Digest) {
        match self {
            Self::Text(text) => digest.text(text),
            Self::Integer(number) => digest.number(number),
            Self::Blob(bytes) => digest.bytes(bytes),
        }
    }
}

impl ToSql for Stored<'_> {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::Borrowed(match *self {
            Self::Text(Some(text)) => ValueRef::Text(text.as_bytes()),
            Self::Integer(Some(number)) => ValueRef::Integer(number),
            Self::Blob(Some(bytes)) => ValueRef::Blob(bytes),
            Self::Text(None) | Self::Integer(None) | Self::Blob(None) => ValueRef::Null,
        }))
    }
}

/// An entry's part rows, in the order a write inserts them, and the digest
/// of the entry row holding them.
pub(super) struct EntryParts<'a> {
    rows: Vec<PartRow<'a>>,
    digest: i64,
}

impl<'a> EntryParts<'a> {
    /// The part rows of an entry whose content is `blocks`, with `search`'s
    /// places of their texts, and the entry row's digest: of
    /// `columns_digest`, the hash of its other columns, and of each part's
    /// number, search position and digest.
    pub(super) fn of(
        blocks: &'a [ContentBlock],
        search: &'a EntrySearch,
        columns_digest: i64,
    ) -> Self {
        let rows = part_rows(blocks, search);
        let mut digest = Digest::default();
        digest.number(Some(columns_digest));
        digest.number(Some(rows.len() as i64));
        for row in &rows {
            digest.number(Some(row.number));
            digest.number(row.position());
            digest.number(Some(row.digest));
        }
        Self {
            rows,
            digest: digest.finish(),
        }
    }

    /// The digest of the entry row holding these parts: an entry whose
    /// stored digest equals it is not written.
    pub(super) fn digest(&self) -> i64 {
        self.digest
    }
}

/// One part row of an entry as a write stores it: one of the entry's
/// blocks, or a part only search reads.
struct PartRow<'a> {
    /// The part's `part` column: a block's index in its entry, or for a
    /// part only search reads, [`search_only_number`].
    number: i64,
    /// Where search reads the part's text.
    searched: Option<TextPlace>,
    stored: StoredValues<'a>,
    items: Vec<ItemValues<'a>>,
    /// The hash of `stored` and `items`.
    digest: i64,
}

impl<'a> PartRow<'a> {
    fn new(
        number: i64,
        searched: Option<TextPlace>,
        stored: StoredValues<'a>,
        items: Vec<ItemValues<'a>>,
    ) -> Self {
        let mut digest = Digest::default();
        for value in stored.values() {
            value.add_to(&mut digest);
        }
        digest.number(Some(items.len() as i64));
        for value in items.iter().flat_map(ItemValues::values) {
            value.add_to(&mut digest);
        }
        Self {
            number,
            searched,
            stored,
            items,
            digest: digest.finish(),
        }
    }

    /// The part of the block at `index` in its entry, whose part's columns
    /// are `columns`, read by search at `place`.
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
        let text = columns.text.map(split_terminal_codes);
        let (text, text_codes) = match text {
            Some(text) => (Some(text.text), text.codes),
            None => (None, None),
        };
        let stored = StoredValues {
            kind: columns.kind,
            search_field: searched.map(|place| place.field.code()),
            text,
            text_codes,
            call_id: columns.call_id,
            name: columns.name,
            tool: columns.tool,
            json: columns.json,
            result_shape: columns.result_shape,
            call_context: columns.call_context,
        };
        Self::new(index as i64, searched, stored, items)
    }

    /// The part numbered `number` only search reads, of `kind`, holding
    /// `text` at `place`, without its terminal codes.
    fn search_only(
        number: i64,
        kind: SearchOnlyKind,
        place: TextPlace,
        text: Cow<'a, str>,
    ) -> Self {
        let stored = StoredValues {
            kind: kind.name(),
            search_field: Some(place.field.code()),
            text: Some(without_codes(text)),
            text_codes: None,
            call_id: None,
            name: None,
            tool: None,
            json: None,
            result_shape: None,
            call_context: None,
        };
        Self::new(number, Some(place), stored, Vec::new())
    }

    fn position(&self) -> Option<i64> {
        self.searched.map(|place| place.position)
    }
}

/// `text` without its terminal codes.
fn without_codes(text: Cow<'_, str>) -> Cow<'_, str> {
    match text {
        Cow::Borrowed(text) => split_terminal_codes(text).text,
        Cow::Owned(text) => {
            let stripped = match split_terminal_codes(&text).text {
                Cow::Owned(stripped) => Some(stripped),
                Cow::Borrowed(_) => None,
            };
            Cow::Owned(stripped.unwrap_or(text))
        }
    }
}

/// What a part row stores besides its entry, number, search place and
/// digest.
struct StoredValues<'a> {
    kind: &'static str,
    search_field: Option<i64>,
    /// Without terminal codes; `text_codes` holds them.
    text: Option<Cow<'a, str>>,
    text_codes: Option<Vec<u8>>,
    call_id: Option<&'a str>,
    name: Option<&'a str>,
    tool: Option<String>,
    json: Option<String>,
    result_shape: Option<&'static str>,
    call_context: Option<String>,
}

impl StoredValues<'_> {
    /// The values of the last ten of [`ROW_COLUMNS`], in their order.
    fn values(&self) -> [Stored<'_>; 10] {
        [
            Stored::Text(Some(self.kind)),
            Stored::Integer(self.search_field),
            Stored::Text(self.text.as_deref()),
            Stored::Blob(self.text_codes.as_deref()),
            Stored::Text(self.call_id),
            Stored::Text(self.name),
            Stored::Text(self.tool.as_deref()),
            Stored::Text(self.json.as_deref()),
            Stored::Text(self.result_shape),
            Stored::Text(self.call_context.as_deref()),
        ]
    }
}

/// A `result_items` row's columns besides its part and position.
struct ItemValues<'a> {
    kind: &'static str,
    /// Without terminal codes; `text_codes` holds them.
    text: Option<Cow<'a, str>>,
    text_codes: Option<Vec<u8>>,
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
        let (text, text_codes) = match text.map(split_terminal_codes) {
            Some(text) => (Some(text.text), text.codes),
            None => (None, None),
        };
        Self {
            kind,
            text,
            text_codes,
            item_type,
        }
    }

    /// The values of `kind`, `text`, `text_codes` and `item_type`.
    fn values(&self) -> [Stored<'_>; 4] {
        [
            Stored::Text(Some(self.kind)),
            Stored::Text(self.text.as_deref()),
            Stored::Blob(self.text_codes.as_deref()),
            Stored::Text(self.item_type),
        ]
    }
}

/// The number of the part only search reads at `index` in the group
/// `group` of [`SEARCH_ONLY_GROUPS`]: below zero, apart from the blocks'
/// parts, each group in its own residue. A block appended to the entry
/// leaves every other part's number, so a later write matches each stored
/// part with the new part of its number.
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
                Cow::Owned(searched_items_text(items)),
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
            Cow::Borrowed(derived.text.as_str()),
        ));
    }
    rows
}

/// The search place of a block's own part; `None` for a result with items
/// in `result_items`, which search reads through its
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
    json: Option<String>,
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
                json: Some(input.to_string()),
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
                    Some(other) => (None, Some(other.to_string()), Some(VERBATIM_JSON)),
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
