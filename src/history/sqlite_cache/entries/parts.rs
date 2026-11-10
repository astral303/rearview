use super::Selection;
use crate::log_entry::{ContentBlock, Tool};
use rusqlite::{Connection, Row, Statement, params};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};

pub(super) struct Writer<'connection> {
    part: Statement<'connection>,
    item: Statement<'connection>,
}

impl<'connection> Writer<'connection> {
    pub(super) fn new(connection: &'connection Connection) -> rusqlite::Result<Self> {
        Ok(Self {
            part: connection.prepare(
                "INSERT INTO parts (entry, part, kind, text, call_id, name, tool, json, result_shape) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            )?,
            item: connection.prepare(
                "INSERT INTO result_items (part, position, kind, text, item_type) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            )?,
        })
    }

    pub(super) fn write(
        &mut self,
        connection: &Connection,
        entry: i64,
        blocks: &[ContentBlock],
    ) -> rusqlite::Result<()> {
        for (position, block) in blocks.iter().enumerate() {
            let columns = Columns::of(block);
            self.part.execute(params![
                entry,
                position as i64,
                columns.kind,
                columns.text,
                columns.call_id,
                columns.name,
                columns.tool,
                columns.json,
                columns.result_shape
            ])?;
            if let ContentBlock::ToolResult {
                content: Some(Value::Array(items)),
                ..
            } = block
            {
                let part = connection.last_insert_rowid();
                for (position, value) in items.iter().enumerate() {
                    let (kind, text, item_type) = match value {
                        Value::String(text) => ("string", Some(text.as_str()), None),
                        Value::Object(object) => (
                            "object",
                            object.get("text").and_then(Value::as_str),
                            object.get("type").and_then(Value::as_str),
                        ),
                        _ => ("external", None, None),
                    };
                    self.item
                        .execute(params![part, position as i64, kind, text, item_type])?;
                }
            }
        }
        Ok(())
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
                let (text, result_shape) = match content {
                    Some(Value::String(text)) => (Some(text.as_str()), None),
                    Some(Value::Array(items)) if items.iter().all(is_lossless_item) => {
                        (None, Some("array"))
                    }
                    Some(Value::Array(items))
                        if items
                            .iter()
                            .any(|item| item.get("text").and_then(Value::as_str).is_some()) =>
                    {
                        (None, Some("array-source"))
                    }
                    Some(_) => (None, Some("external")),
                    None => (None, None),
                };
                Self {
                    kind: "result",
                    text,
                    call_id: Some(tool_use_id),
                    name: standalone_tool_name.as_deref(),
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
        "SELECT i.part, i.kind, i.text, i.item_type FROM result_items i \
         JOIN parts p ON p.id = i.part \
         WHERE p.entry IN ({}) ORDER BY i.part, i.position",
        selected.ids,
    ))?;
    let mut rows = statement.query(rusqlite::params_from_iter(&selected.parameters))?;
    while let Some(row) = rows.next()? {
        let kind: String = row.get(1)?;
        let text: Option<String> = row.get(2)?;
        let item_type: Option<String> = row.get(3)?;
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
        "SELECT p.entry, p.id, p.kind, p.text, p.call_id, p.name, p.tool, p.json, p.result_shape \
         FROM parts p WHERE p.entry IN ({}) ORDER BY p.entry, p.part",
        selected.ids,
    ))?;
    let mut rows = statement.query(rusqlite::params_from_iter(&selected.parameters))?;
    while let Some(row) = rows.next()? {
        let array = items.remove(&row.get::<_, i64>(1)?);
        blocks
            .entry(row.get(0)?)
            .or_default()
            .push(block_of(row, array)?);
    }
    Ok(blocks)
}

pub(super) fn external_results(
    connection: &Connection,
    selected: &Selection,
) -> rusqlite::Result<HashSet<(i64, usize)>> {
    connection
        .prepare(&format!(
            "SELECT p.entry, p.part FROM parts p WHERE p.entry IN ({}) \
             AND p.result_shape IN ('external', 'array-source')",
            selected.ids,
        ))?
        .query_map(rusqlite::params_from_iter(&selected.parameters), |row| {
            Ok((row.get(0)?, row.get::<_, i64>(1)? as usize))
        })?
        .collect()
}

fn block_of(row: &Row, array: Option<Vec<Value>>) -> rusqlite::Result<ContentBlock> {
    let kind: String = row.get(2)?;
    let text: Option<String> = row.get(3)?;
    Ok(match kind.as_str() {
        "text" => ContentBlock::Text {
            text: text.unwrap_or_default(),
        },
        "thinking" => ContentBlock::Thinking {
            thinking: text.unwrap_or_default(),
            signature: String::new(),
        },
        "call" => ContentBlock::ToolUse {
            id: row.get::<_, Option<String>>(4)?.unwrap_or_default(),
            name: row.get::<_, Option<String>>(5)?.unwrap_or_default(),
            tool: row
                .get::<_, Option<String>>(6)?
                .and_then(|tool| serde_json::from_value(Value::String(tool)).ok())
                .unwrap_or_default(),
            input: super::json_value(row.get::<_, Option<String>>(7)?.as_deref())?
                .unwrap_or(Value::Null),
        },
        "result" => ContentBlock::ToolResult {
            tool_use_id: row.get::<_, Option<String>>(4)?.unwrap_or_default(),
            standalone_tool_name: row.get(5)?,
            content: match text {
                Some(text) => Some(Value::String(text)),
                None => match row.get::<_, Option<String>>(8)?.as_deref() {
                    Some("array" | "array-source") => Some(Value::Array(array.unwrap_or_default())),
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
