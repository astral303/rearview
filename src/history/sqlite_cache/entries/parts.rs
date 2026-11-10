use super::digest::Digest;
use super::{ReadPurpose, Selection};
use crate::agent::sanitize::split_terminal_codes;
use crate::agent::transcript::tool_result_item_text;
use crate::history::sqlite_cache::SearchField;
use crate::log_entry::{ContentBlock, Tool};
use rusqlite::{Connection, Row, Statement, params};
use serde_json::{Value, json};
use std::collections::HashMap;

pub(super) struct Writer<'connection> {
    part: Statement<'connection>,
    item: Statement<'connection>,
}

impl<'connection> Writer<'connection> {
    pub(super) fn new(connection: &'connection Connection) -> rusqlite::Result<Self> {
        Ok(Self {
            part: connection.prepare(
                "INSERT INTO parts (entry, part, kind, search_field, text, text_codes, \
                 call_id, name, tool, json, result_shape) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            )?,
            item: connection.prepare(
                "INSERT INTO result_items (part, position, kind, searched, text, text_codes, \
                 item_type) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )?,
        })
    }

    /// Writes `blocks`, each read by search in the field `search` holds for
    /// it.
    pub(super) fn write(
        &mut self,
        connection: &Connection,
        entry: i64,
        blocks: &[ContentBlock],
        search: &[Option<SearchField>],
    ) -> rusqlite::Result<()> {
        for (position, block) in blocks.iter().enumerate() {
            let columns = Columns::of(block);
            let field = search[position];
            let text = columns.text.map(split_terminal_codes);
            self.part.execute(params![
                entry,
                position as i64,
                columns.kind,
                field.map(SearchField::code),
                text.as_ref().map(|text| &*text.text),
                text.as_ref().and_then(|text| text.codes.as_deref()),
                columns.call_id,
                columns.name,
                columns.tool,
                columns.json,
                columns.result_shape
            ])?;
            let part = connection.last_insert_rowid();
            for (position, item) in result_items(block, &columns, field).enumerate() {
                let text = item.text.map(split_terminal_codes);
                self.item.execute(params![
                    part,
                    position as i64,
                    item.kind,
                    item.searched,
                    text.as_ref().map(|text| &*text.text),
                    text.as_ref().and_then(|text| text.codes.as_deref()),
                    item.item_type
                ])?;
            }
        }
        Ok(())
    }
}

/// Feeds `digest` what [`Writer::write`] stores of `blocks` and `search`.
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
        for item in result_items(block, &columns, *field) {
            items += 1;
            digest.text(Some(item.kind));
            digest.flag(Some(item.searched));
            digest.text(item.text);
            digest.text(item.item_type);
        }
        digest.number(Some(items));
    }
}

/// A `result_items` row's columns.
struct ItemColumns<'a> {
    kind: &'static str,
    searched: bool,
    text: Option<&'a str>,
    item_type: Option<&'a str>,
}

/// The items `result_items` holds for `block`, whose part's columns are
/// `columns` and whose search field is `field`: none unless the block is a
/// result whose items the part stores.
fn result_items<'a>(
    block: &'a ContentBlock,
    columns: &Columns<'_>,
    field: Option<SearchField>,
) -> impl Iterator<Item = ItemColumns<'a>> {
    let items = match block {
        ContentBlock::ToolResult {
            content: Some(Value::Array(items)),
            ..
        } if columns.result_shape == Some(LOSSLESS_ARRAY) => items.as_slice(),
        _ => &[],
    };
    items.iter().map(move |value| {
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
            searched: field.is_some() && tool_result_item_text(value).is_some(),
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
         p.result_shape FROM parts p WHERE p.entry IN ({}) ORDER BY p.entry, p.part",
        selected.ids,
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
