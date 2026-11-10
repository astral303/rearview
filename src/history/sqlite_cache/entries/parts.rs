use super::digest::Digest;
use super::search::EntrySearch;
use super::write::SearchPlace;
use super::{ReadPurpose, Selection};
use crate::agent::sanitize::{split_terminal_codes, without_terminal_codes};
use crate::agent::transcript::tool_result_item_text;
use crate::history::sqlite_cache::SearchField;
use crate::log_entry::{ContentBlock, Tool};
use rusqlite::{Connection, Row, Statement, params};
use serde_json::{Value, json};
use std::borrow::Cow;
use std::collections::HashMap;

pub(super) struct Writer<'connection> {
    part: Statement<'connection>,
    item: Statement<'connection>,
}

impl<'connection> Writer<'connection> {
    pub(super) fn new(connection: &'connection Connection) -> rusqlite::Result<Self> {
        Ok(Self {
            part: connection.prepare(
                "INSERT INTO parts (session, entry, part, kind, search_field, entry_sequence, \
                 message_ordinal, text, text_codes, call_id, name, tool, json, result_shape) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            )?,
            item: connection.prepare(
                "INSERT INTO result_items (part, position, kind, text, text_codes, item_type) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )?,
        })
    }

    /// Writes `session`'s entry `entry` as its parts at `place`: one part for
    /// each of `blocks`, in the field `search` gives it, then the search-only
    /// parts of `search` (see [`search_only_parts`]).
    pub(super) fn write(
        &mut self,
        connection: &Connection,
        session: i64,
        entry: i64,
        blocks: &[ContentBlock],
        search: &EntrySearch,
        place: SearchPlace,
    ) -> rusqlite::Result<()> {
        for (position, block) in blocks.iter().enumerate() {
            let columns = Columns::of(block);
            let field = block_field(&columns, search.blocks[position]);
            let text = columns.text.map(split_terminal_codes);
            self.part.execute(params![
                session,
                entry,
                position as i64,
                columns.kind,
                field.map(SearchField::code),
                field.map(|_| place.sequence),
                field.and(place.ordinal),
                text.as_ref().map(|text| &*text.text),
                text.as_ref().and_then(|text| text.codes.as_deref()),
                columns.call_id,
                columns.name,
                columns.tool,
                columns.json,
                columns.result_shape
            ])?;
            let part = connection.last_insert_rowid();
            for (position, item) in result_items(block, &columns).enumerate() {
                let text = item.text.map(split_terminal_codes);
                self.item.execute(params![
                    part,
                    position as i64,
                    item.kind,
                    text.as_ref().map(|text| &*text.text),
                    text.as_ref().and_then(|text| text.codes.as_deref()),
                    item.item_type
                ])?;
            }
        }
        for (offset, (kind, field, text)) in search_only_parts(blocks, search).enumerate() {
            let (kind, text) = kind.columns(&text);
            self.part.execute(params![
                session,
                entry,
                (blocks.len() + offset) as i64,
                kind,
                field.code(),
                place.sequence,
                place.ordinal,
                text,
                None::<&str>,
                None::<&str>,
                None::<&str>,
                None::<&str>,
                None::<&str>,
                None::<&str>
            ])?;
        }
        Ok(())
    }
}

/// The parts search alone reads, numbered after the entry's blocks, each as
/// its kind, field and text: a [`SearchOnlyKind::ResultText`] part for each
/// searched result with items in `result_items`, in block order, then a
/// [`SearchOnlyKind::Derived`] part for each derived text.
fn search_only_parts<'a>(
    blocks: &'a [ContentBlock],
    search: &'a EntrySearch,
) -> impl Iterator<Item = (SearchOnlyKind, SearchField, Cow<'a, str>)> {
    let results = blocks
        .iter()
        .zip(&search.blocks)
        .filter_map(|(block, field)| {
            let field = (*field)?;
            let items = stored_items(block, &Columns::of(block))?;
            Some((
                SearchOnlyKind::ResultText,
                field,
                Cow::Owned(searched_items_text(items)),
            ))
        });
    let derived = search.derived.iter().map(|derived| {
        (
            SearchOnlyKind::Derived,
            derived.field,
            Cow::Borrowed(derived.text.as_str()),
        )
    });
    results.chain(derived)
}

/// The field a block's own part is read in: `None` for a result with items
/// in `result_items`. Search reads that result through its
/// [`SearchOnlyKind::ResultText`] part.
fn block_field(columns: &Columns<'_>, field: Option<SearchField>) -> Option<SearchField> {
    field.filter(|_| columns.result_shape != Some(LOSSLESS_ARRAY))
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

/// Feeds `digest` what [`Writer::write`] stores of `blocks` and `search`'s
/// fields of them.
pub(super) fn digest(digest: &mut Digest, blocks: &[ContentBlock], search: &[Option<SearchField>]) {
    digest.number(Some(blocks.len() as i64));
    for (block, field) in blocks.iter().zip(search) {
        let columns = Columns::of(block);
        digest.text(Some(columns.kind));
        digest.number(field.map(SearchField::code));
        for text in [
            columns.text,
            columns.call_id,
            columns.name,
            columns.tool.as_deref(),
            columns.json.as_deref(),
            columns.result_shape,
        ] {
            digest.text(text);
        }
        let mut items = 0;
        for item in result_items(block, &columns) {
            items += 1;
            digest.text(Some(item.kind));
            digest.text(item.text);
            digest.text(item.item_type);
        }
        digest.number(Some(items));
    }
}

/// A `result_items` row's columns.
struct ItemColumns<'a> {
    kind: &'static str,
    text: Option<&'a str>,
    item_type: Option<&'a str>,
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

/// The items `result_items` holds for `block`. `columns` are the block's
/// part's columns.
fn result_items<'a>(
    block: &'a ContentBlock,
    columns: &Columns<'_>,
) -> impl Iterator<Item = ItemColumns<'a>> {
    let items = stored_items(block, columns).unwrap_or_default();
    items.iter().map(|value| {
        let (kind, text, item_type) = match value {
            Value::String(text) => ("string", Some(text.as_str()), None),
            Value::Object(object) => (
                "object",
                object.get("text").and_then(Value::as_str),
                object.get("type").and_then(Value::as_str),
            ),
            _ => ("external", None, None),
        };
        ItemColumns {
            kind,
            text,
            item_type,
        }
    })
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

/// [`SearchOnlyKind::Derived`]'s `kind` column.
pub(in crate::history::sqlite_cache) const DERIVED: &str = SearchOnlyKind::Derived.name();
/// [`SearchOnlyKind::ResultText`]'s `kind` column.
pub(in crate::history::sqlite_cache) const RESULT_TEXT: &str = SearchOnlyKind::ResultText.name();

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

pub(super) fn read(
    connection: &Connection,
    selected: &Selection,
    purpose: ReadPurpose,
) -> rusqlite::Result<Blocks> {
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
        let text = purpose.text(row, 2, 3)?;
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
        let text = purpose.text(row, 3, 4)?;
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
