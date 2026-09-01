//! The JSONL transcript format Pi and OMP share.

mod tools;

use super::{
    SessionFormat, SessionHeader, SessionProjection, append_exit_code, append_output_note,
};
use crate::agent::transcript::bounded_tool_result_text;
use crate::error::{AppError, Result};
use crate::history::Source;
use crate::log_entry::{
    AssistantMessage, ContentBlock, LogEntry, TokenUsage, Tool, UserContent, UserMessage,
};
use chrono::{DateTime, SecondsFormat, Utc};
use serde_json::{Map, Value, json};
use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

const MAX_SUPPORTED_VERSION: u64 = 3;

/// The session id `path` states in its header, or `None` when it is not a
/// Pi-family log.
///
/// The header is the first record, or the second when an OMP title slot comes
/// before it, so two lines settle it and the log itself is never parsed.
pub(crate) fn header_session_id(path: &Path) -> Option<String> {
    let file = File::open(path).ok()?;
    let mut records = BufReader::new(file)
        .lines()
        .map_while(std::result::Result::ok)
        .filter(|line| !line.trim().is_empty())
        .take(2)
        .filter_map(|line| serde_json::from_str::<Value>(&line).ok());
    let first = records.next()?;
    let header = match first.get("type").and_then(Value::as_str) {
        Some("session") => first,
        Some("title") => records.next()?,
        _ => return None,
    };
    if header.get("type").and_then(Value::as_str) != Some("session") {
        return None;
    }
    header.get("id").and_then(Value::as_str).map(str::to_owned)
}

/// Every Pi-family log `depth` levels under `root` whose header states
/// `session_id`.
///
/// An id is not unique across logs — a branch copied within a project keeps
/// the id it came from — so this returns all of them rather than the first.
pub(crate) fn sessions_with_id(
    root: &Path,
    depth: usize,
    session_id: &str,
) -> Result<Vec<PathBuf>> {
    Ok(
        crate::history::provider::walk::jsonl_files_at_depth(root, depth)?
            .into_iter()
            .filter(|path| header_session_id(path).as_deref() == Some(session_id))
            .collect(),
    )
}

#[derive(Debug)]
struct RawEntry {
    line: usize,
    id: String,
    parent_id: Option<String>,
    value: Value,
}

/// One reader of the Pi-family log, bound to the source it speaks for.
///
/// An OMP session announces itself with a leading `title` slot. Without one the
/// file is indistinguishable from a Pi session, so it is attributed to
/// `default_source` — in practice, whichever agent's root it was found under. The
/// attribution is not cosmetic: it decides whether assistant turns are labelled
/// `Pi` or `OMP`, so it has to be settled before the entries are normalized.
pub struct PiLogFormat {
    default_source: Source,
}

pub static PI_LOG: PiLogFormat = PiLogFormat {
    default_source: Source::Pi,
};

pub static OMP_LOG: PiLogFormat = PiLogFormat {
    default_source: Source::Omp,
};

impl SessionFormat for PiLogFormat {
    fn parse_transcript(&self, path: &Path) -> Result<Option<SessionProjection>> {
        let file = File::open(path)?;
        parse_reader(BufReader::new(file), self.default_source)
    }
}

fn parse_reader(reader: impl BufRead, default_source: Source) -> Result<Option<SessionProjection>> {
    let mut parsed = Vec::new();
    let mut malformed_lines = Vec::new();
    for (index, line) in reader.lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Value>(&line) {
            Ok(value) => parsed.push((index + 1, value)),
            Err(_) => malformed_lines.push(index + 1),
        }
    }

    let Some((_, first_value)) = parsed.first() else {
        return Ok(None);
    };
    let title_slot = first_value
        .as_object()
        .filter(|object| object.get("type").and_then(Value::as_str) == Some("title"));
    let header_index = usize::from(title_slot.is_some());
    let Some((_, header_value)) = parsed.get(header_index) else {
        return Ok(None);
    };
    let Some(header_object) = header_value.as_object() else {
        return Ok(None);
    };
    if header_object.get("type").and_then(Value::as_str) != Some("session") {
        return Ok(None);
    }
    let source = if title_slot.is_some() {
        Source::Omp
    } else {
        default_source
    };
    let title = title_slot
        .and_then(|slot| slot.get("title"))
        .and_then(Value::as_str)
        .filter(|title| !title.is_empty())
        .or_else(|| header_object.get("title").and_then(Value::as_str))
        .filter(|title| !title.is_empty())
        .map(str::to_owned);
    let Some(id) = header_object
        .get("id")
        .and_then(Value::as_str)
        .map(str::to_owned)
    else {
        return Ok(None);
    };
    let Some(timestamp) = header_object
        .get("timestamp")
        .and_then(Value::as_str)
        .map(str::to_owned)
    else {
        return Ok(None);
    };
    let Some(cwd) = header_object
        .get("cwd")
        .and_then(Value::as_str)
        .map(str::to_owned)
    else {
        return Ok(None);
    };
    let version = header_object
        .get("version")
        .and_then(Value::as_u64)
        .unwrap_or(1);
    if !(1..=MAX_SUPPORTED_VERSION).contains(&version) {
        return Ok(None);
    }

    let mut raw_entries = Vec::new();
    let mut previous_id: Option<String> = None;
    for (line, value) in parsed.into_iter().skip(header_index + 1) {
        let Some(object) = value.as_object() else {
            malformed_lines.push(line);
            continue;
        };
        let entry_id = if version == 1 {
            format!("v1-{line}")
        } else if let Some(id) = object
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
        {
            id.to_owned()
        } else {
            malformed_lines.push(line);
            continue;
        };
        let parent_id = if version == 1 {
            previous_id.clone()
        } else {
            object
                .get("parentId")
                .and_then(Value::as_str)
                .map(str::to_owned)
        };
        previous_id = Some(entry_id.clone());
        raw_entries.push(RawEntry {
            line,
            id: entry_id,
            parent_id,
            value,
        });
    }

    let leaf_id = raw_entries.last().map(|entry| entry.id.clone());
    let by_id = raw_entries
        .iter()
        .enumerate()
        .map(|(index, entry)| (entry.id.as_str(), index))
        .collect::<HashMap<_, _>>();
    let mut active_indices = Vec::new();
    let mut current = raw_entries.len().checked_sub(1);
    let mut visited = HashSet::new();
    while let Some(index) = current {
        let entry = &raw_entries[index];
        if !visited.insert(entry.id.clone()) {
            break;
        }
        active_indices.push(index);
        current = entry
            .parent_id
            .as_deref()
            .and_then(|parent| by_id.get(parent).copied());
    }
    active_indices.reverse();

    let mut entries = Vec::new();
    if let Some(title) = title.as_ref() {
        entries.push((
            1,
            LogEntry::CustomTitle {
                custom_title: title.clone(),
            },
        ));
    }
    entries.extend(active_indices.into_iter().filter_map(|index| {
        let raw = &raw_entries[index];
        normalize_entry(&raw.value, &raw.id, source).map(|entry| (raw.line, entry))
    }));

    Ok(Some(SessionProjection {
        source,
        parent_session_id: None,
        header: SessionHeader {
            version,
            id,
            timestamp,
            cwd: PathBuf::from(cwd),
        },
        title,
        entries,
        leaf_id,
        malformed_lines,
    }))
}

fn normalize_entry(value: &Value, entry_id: &str, source: Source) -> Option<LogEntry> {
    let object = value.as_object()?;
    let entry_type = object
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let timestamp = object
        .get("timestamp")
        .and_then(Value::as_str)
        .map(str::to_owned);
    match entry_type {
        "message" => normalize_message(object.get("message")?, entry_id, timestamp, source),
        "compaction" => metadata_with_usage(
            "Compaction",
            object.get("summary").and_then(Value::as_str).unwrap_or(""),
            timestamp,
            false,
            object.get("usage").and_then(pi_usage),
        ),
        "branch_summary" => metadata_with_usage(
            "Branch summary",
            object.get("summary").and_then(Value::as_str).unwrap_or(""),
            timestamp,
            false,
            object.get("usage").and_then(pi_usage),
        ),
        "session_info" | "title_change" => Some(LogEntry::CustomTitle {
            custom_title: object
                .get(if entry_type == "title_change" {
                    "title"
                } else {
                    "name"
                })
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned(),
        }),
        "model_change" => {
            let model = object
                .get("model")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .unwrap_or_else(|| {
                    format!(
                        "{}/{}",
                        object
                            .get("provider")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown"),
                        object
                            .get("modelId")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown")
                    )
                });
            metadata("Model", &model, timestamp, false)
        }
        "thinking_level_change" | "label" | "custom" => None,
        "custom_message" => {
            if object.get("display").and_then(Value::as_bool) == Some(false) {
                return None;
            }
            let text = content_text(object.get("content"));
            metadata(
                object
                    .get("customType")
                    .and_then(Value::as_str)
                    .unwrap_or("Custom"),
                &text,
                timestamp,
                true,
            )
        }
        _ => None,
    }
}

/// `entry_id` is the record's own id, which only the `bashExecution` arm
/// needs: it pairs the call it builds with the result beside it.
fn normalize_message(
    message: &Value,
    entry_id: &str,
    timestamp: Option<String>,
    source: Source,
) -> Option<LogEntry> {
    let object = message.as_object()?;
    let role = object.get("role").and_then(Value::as_str)?;
    let timestamp = timestamp.or_else(|| millis_timestamp(object.get("timestamp")));
    match role {
        "user" => Some(LogEntry::User {
            message: UserMessage {
                role: "user".to_owned(),
                content: user_content(object.get("content")),
            },
            timestamp,
            uuid: None,
            cwd: None,
            parent_tool_use_id: None,
            usage: None,
        }),
        "assistant" => Some(LogEntry::Assistant {
            agent: Some(
                match source {
                    Source::Omp => "OMP",
                    _ => "Pi",
                }
                .to_owned(),
            ),
            message: AssistantMessage {
                role: "assistant".to_owned(),
                content: assistant_content(object.get("content"), source),
                model: object
                    .get("model")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                usage: object.get("usage").and_then(pi_usage),
                id: None,
            },
            timestamp,
            uuid: None,
            parent_tool_use_id: None,
        }),
        "toolResult" => Some(LogEntry::User {
            message: UserMessage {
                role: "user".to_owned(),
                content: UserContent::Blocks(vec![ContentBlock::ToolResult {
                    tool_use_id: object
                        .get("toolCallId")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown")
                        .to_owned(),
                    content: Some(tool_result_content(object)),
                    standalone_tool_name: None,
                }]),
            },
            timestamp: None,
            uuid: None,
            cwd: None,
            parent_tool_use_id: None,
            usage: object.get("usage").and_then(pi_usage),
        }),
        // A command the user ran themselves: Pi records it as one row, which
        // reads as the call it was and the result it produced.
        "bashExecution" => Some(LogEntry::User {
            message: UserMessage {
                role: "user".to_owned(),
                content: UserContent::Blocks(vec![
                    ContentBlock::ToolUse {
                        id: entry_id.to_owned(),
                        name: "bash".to_owned(),
                        tool: Tool::UserShell,
                        input: json!({
                            "command": object
                                .get("command")
                                .and_then(Value::as_str)
                                .unwrap_or(""),
                        }),
                    },
                    ContentBlock::ToolResult {
                        tool_use_id: entry_id.to_owned(),
                        content: Some(json!(bash_output(object))),
                        standalone_tool_name: None,
                    },
                ]),
            },
            // The row records one command, which ran when the record was
            // written, so the run it opens is stamped like any other.
            timestamp,
            uuid: None,
            cwd: None,
            parent_tool_use_id: None,
            usage: None,
        }),
        "custom" | "hookMessage" => {
            if object.get("display").and_then(Value::as_bool) == Some(false) {
                None
            } else {
                let label = object
                    .get("customType")
                    .and_then(Value::as_str)
                    .unwrap_or("Custom");
                let text = content_text(object.get("content"));
                metadata(label, &text, timestamp, true)
            }
        }
        _ => None,
    }
}

fn metadata(
    label: &str,
    text: &str,
    timestamp: Option<String>,
    searchable: bool,
) -> Option<LogEntry> {
    metadata_with_usage(label, text, timestamp, searchable, None)
}

fn metadata_with_usage(
    label: &str,
    text: &str,
    timestamp: Option<String>,
    searchable: bool,
    usage: Option<TokenUsage>,
) -> Option<LogEntry> {
    Some(LogEntry::PiMetadata {
        label: label.to_owned(),
        text: text.to_owned(),
        timestamp,
        searchable,
        usage,
    })
}

fn millis_timestamp(value: Option<&Value>) -> Option<String> {
    let millis = value?.as_i64()?;
    DateTime::<Utc>::from_timestamp_millis(millis)
        .map(|time| time.to_rfc3339_opts(SecondsFormat::Millis, true))
}

fn user_content(value: Option<&Value>) -> UserContent {
    if let Some(text) = value.and_then(Value::as_str) {
        return UserContent::String(text.to_owned());
    }
    UserContent::Blocks(content_blocks(value))
}

fn assistant_content(value: Option<&Value>, source: Source) -> Vec<ContentBlock> {
    let Some(values) = value.and_then(Value::as_array) else {
        return Vec::new();
    };
    values
        .iter()
        .filter_map(Value::as_object)
        .flat_map(|object| assistant_blocks(object, source))
        .collect()
}

fn assistant_blocks(object: &Map<String, Value>, source: Source) -> Vec<ContentBlock> {
    match block_type(object) {
        "thinking" => vec![ContentBlock::Thinking {
            thinking: string_or_empty(object, "thinking"),
            signature: string_or_empty(object, "thinkingSignature"),
        }],
        "toolCall" => tools::tool_use_blocks(
            object
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("unknown"),
            object
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("unknown"),
            object
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({})),
            source,
        ),
        _ => content_block(object).into_iter().collect(),
    }
}

/// The text and image blocks either role can carry.
fn content_blocks(value: Option<&Value>) -> Vec<ContentBlock> {
    let Some(values) = value.and_then(Value::as_array) else {
        return Vec::new();
    };
    values
        .iter()
        .filter_map(Value::as_object)
        .filter_map(content_block)
        .collect()
}

fn content_block(object: &Map<String, Value>) -> Option<ContentBlock> {
    match block_type(object) {
        "text" => Some(ContentBlock::Text {
            text: string_or_empty(object, "text"),
        }),
        "image" => Some(ContentBlock::Text {
            text: format!(
                "[Image: {}]",
                object
                    .get("mimeType")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown type")
            ),
        }),
        _ => None,
    }
}

fn block_type(object: &Map<String, Value>) -> &str {
    object
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
}

fn string_or_empty(object: &Map<String, Value>, key: &str) -> String {
    object
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned()
}

fn content_text(value: Option<&Value>) -> String {
    if let Some(text) = value.and_then(Value::as_str) {
        return text.to_owned();
    }
    content_blocks(value)
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn tool_result_content(object: &Map<String, Value>) -> Value {
    let blocks = content_blocks(object.get("content"))
        .into_iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(json!({"type": "text", "text": text})),
            _ => None,
        })
        .collect::<Vec<_>>();
    let mut text = bounded_tool_result_text(&Value::Array(blocks)).unwrap_or_default();
    if object.get("isError").and_then(Value::as_bool) == Some(true) {
        text = format!("Error: {text}");
    }
    json!(text)
}

/// The command's output, plus the lines Pi records beside it: a non-zero exit
/// and a cancellation. The command itself is the call's input, not part of its
/// result.
fn bash_output(object: &Map<String, Value>) -> String {
    let mut text = object
        .get("output")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    if let Some(code) = object.get("exitCode").and_then(Value::as_i64)
        && code != 0
    {
        append_exit_code(&mut text, code);
    }
    if object.get("cancelled").and_then(Value::as_bool) == Some(true) {
        append_output_note(&mut text, "Command cancelled");
    }
    bounded_tool_result_text(&json!(text)).unwrap_or_default()
}

fn pi_usage(value: &Value) -> Option<TokenUsage> {
    let object = value.as_object()?;
    Some(TokenUsage {
        input_tokens: object.get("input").and_then(Value::as_u64).unwrap_or(0),
        output_tokens: object.get("output").and_then(Value::as_u64).unwrap_or(0),
        cache_creation_input_tokens: object
            .get("cacheWrite")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        cache_read_input_tokens: object.get("cacheRead").and_then(Value::as_u64).unwrap_or(0),
    })
}

pub fn append_session_rename(path: &Path, title: &str) -> Result<()> {
    let projection = PI_LOG.parse_transcript(path)?.ok_or_else(|| {
        AppError::ConfigError(format!("{} is not a valid Pi session", path.display()))
    })?;
    let mut ids = HashSet::new();
    let file = File::open(path)?;
    for line in BufReader::new(file).lines() {
        let Ok(line) = line else { continue };
        if let Ok(value) = serde_json::from_str::<Value>(&line)
            && let Some(id) = value.get("id").and_then(Value::as_str)
        {
            ids.insert(id.to_owned());
        }
    }
    let id = (0u64..)
        .map(|counter| {
            let seed = format!(
                "{}:{}:{}",
                projection.header.id,
                Utc::now().timestamp_nanos_opt().unwrap_or(0),
                counter
            );
            blake3::hash(seed.as_bytes()).to_hex()[..8].to_owned()
        })
        .find(|id| !ids.contains(id))
        .expect("unbounded ID sequence has a free value");
    let entry = json!({
        "type": "session_info",
        "id": id,
        "parentId": projection.leaf_id,
        "timestamp": Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
        "name": title.replace(['\r', '\n'], " ").trim(),
    });
    let mut file = OpenOptions::new().append(true).open(path)?;
    writeln!(file, "{entry}")?;
    Ok(())
}

pub fn append_omp_session_rename(path: &Path, title: &str) -> Result<()> {
    const TITLE_SLOT_BYTES: usize = 256;

    let projection = OMP_LOG.parse_transcript(path)?.ok_or_else(|| {
        AppError::ConfigError(format!("{} is not a valid OMP session", path.display()))
    })?;
    let title = title.replace(['\r', '\n'], " ").trim().to_owned();
    let timestamp = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
    let mut ids = HashSet::new();
    let contents = std::fs::read_to_string(path)?;
    for line in contents.lines() {
        if let Ok(value) = serde_json::from_str::<Value>(line)
            && let Some(id) = value.get("id").and_then(Value::as_str)
        {
            ids.insert(id.to_owned());
        }
    }
    let id = (0u64..)
        .map(|counter| {
            let seed = format!(
                "{}:{}:{}",
                projection.header.id,
                Utc::now().timestamp_nanos_opt().unwrap_or(0),
                counter
            );
            blake3::hash(seed.as_bytes()).to_hex()[..8].to_owned()
        })
        .find(|id| !ids.contains(id))
        .expect("unbounded ID sequence has a free value");
    let entry = json!({
        "type": "title_change",
        "id": id,
        "parentId": projection.leaf_id,
        "timestamp": timestamp,
        "title": title,
        "previousTitle": projection.title,
        "source": "user",
    });
    let slot = omp_title_slot(&title, &timestamp, TITLE_SLOT_BYTES)?;
    let mut lines = contents.split_inclusive('\n');
    let first = lines.next().unwrap_or_default();
    let mut rewritten = String::new();
    if serde_json::from_str::<Value>(first.trim_end_matches('\n'))
        .ok()
        .and_then(|value| value.get("type").and_then(Value::as_str).map(str::to_owned))
        .as_deref()
        == Some("title")
    {
        rewritten.push_str(&slot);
    } else {
        rewritten.push_str(&slot);
        rewritten.push_str(first);
    }
    rewritten.extend(lines);
    if !rewritten.ends_with('\n') {
        rewritten.push('\n');
    }
    rewritten.push_str(&entry.to_string());
    rewritten.push('\n');

    let parent = path.parent().ok_or_else(|| {
        AppError::ConfigError(format!("{} has no parent directory", path.display()))
    })?;
    let permissions = std::fs::metadata(path)?.permissions();
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    temp.as_file_mut().set_permissions(permissions)?;
    temp.write_all(rewritten.as_bytes())?;
    temp.as_file_mut().sync_all()?;
    temp.persist(path)
        .map_err(|error| AppError::Io(error.error))?;
    Ok(())
}

fn omp_title_slot(title: &str, timestamp: &str, bytes: usize) -> Result<String> {
    let mut title = title.to_owned();
    loop {
        let empty = json!({
            "type": "title",
            "v": 1,
            "title": title,
            "source": "user",
            "updatedAt": timestamp,
            "pad": "",
        });
        let base = format!("{empty}\n");
        if base.len() <= bytes {
            let padding = " ".repeat(bytes - base.len());
            let line = json!({
                "type": "title",
                "v": 1,
                "title": title,
                "source": "user",
                "updatedAt": timestamp,
                "pad": padding,
            });
            let serialized = format!("{line}\n");
            if serialized.len() == bytes {
                return Ok(serialized);
            }
        }
        if title.pop().is_none() {
            return Err(AppError::ConfigError(
                "OMP title slot metadata exceeds 256 bytes".to_owned(),
            ));
        }
    }
}

#[cfg(test)]
mod tests {

    fn omp_fixture() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/omp/v3.jsonl")
    }

    #[test]
    fn a_header_states_the_session_id_whatever_the_file_is_called() {
        assert_eq!(
            header_session_id(&fixture("v1.jsonl")).as_deref(),
            Some("custom_v1_id")
        );
    }

    /// OMP writes a title slot ahead of the header, so the id is on the second
    /// record rather than the first.
    #[test]
    fn a_title_slot_ahead_of_the_header_does_not_hide_the_session_id() {
        assert_eq!(
            header_session_id(&omp_fixture()).as_deref(),
            Some("omp_session_custom_id")
        );
    }

    #[test]
    fn a_file_that_is_not_a_pi_family_log_states_no_session_id() {
        let directory = tempfile::tempdir().unwrap();
        let stray = directory.path().join("stray.jsonl");
        std::fs::write(
            &stray,
            "{\"type\":\"user\",\"message\":{}}
",
        )
        .unwrap();

        assert_eq!(header_session_id(&stray), None);
        assert_eq!(
            header_session_id(&directory.path().join("absent.jsonl")),
            None
        );
    }

    /// Two logs in one project can state the same id — a branch keeps the id
    /// it was copied from — so every match comes back.
    #[test]
    fn every_log_stating_the_id_is_found() {
        let directory = tempfile::tempdir().unwrap();
        let project = directory.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let first = project.join("session.jsonl");
        let copy = project.join("branch.jsonl");
        std::fs::copy(fixture("v1.jsonl"), &first).unwrap();
        std::fs::copy(fixture("v1.jsonl"), &copy).unwrap();

        let mut found = sessions_with_id(directory.path(), 1, "custom_v1_id").unwrap();
        found.sort();

        assert_eq!(found, vec![copy, first]);
        assert!(
            sessions_with_id(directory.path(), 1, "no_such_id")
                .unwrap()
                .is_empty()
        );
    }
    use super::*;
    use crate::agent::transcript::AgentTranscript;
    use crate::history::{Source, parser::process_conversation_file};

    fn fixture(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/pi")
            .join(name)
    }

    #[test]
    fn parses_versions_one_through_three_without_rewriting() {
        for (name, version, id) in [
            ("v1.jsonl", 1, "custom_v1_id"),
            ("v2.jsonl", 2, "custom_id_with_underscores"),
            (
                "v3-branched.jsonl",
                3,
                "01912345-6789-7abc-8def-0123456789ab",
            ),
        ] {
            let before = std::fs::read(fixture(name)).unwrap();
            let projection = PI_LOG.parse_transcript(&fixture(name)).unwrap().unwrap();
            assert_eq!(projection.header.version, version);
            assert_eq!(projection.header.id, id);
            assert_eq!(std::fs::read(fixture(name)).unwrap(), before);
        }
    }

    #[test]
    fn projects_only_the_active_branch_and_sanitizes_images() {
        let projection = PI_LOG
            .parse_transcript(&fixture("v3-branched.jsonl"))
            .unwrap()
            .unwrap();
        let json = projection
            .entries
            .iter()
            .map(|(_, entry)| serde_json::to_string(entry).unwrap())
            .collect::<Vec<_>>()
            .join(" ");

        assert!(json.contains("active root question"));
        assert!(json.contains("branch summary searchable"));
        assert!(json.contains("compaction summary searchable"));
        assert!(json.contains("tool output searchable"));
        assert!(json.contains("bash output searchable"));
        assert!(json.contains("[Image: image/jpeg]"));
        assert!(!json.contains("ABANDONED_BRANCH_SENTINEL"));
        assert!(!json.contains("ABANDONED_ANSWER_SENTINEL"));
        assert!(!json.contains("HIDDEN_CUSTOM_SENTINEL"));
        assert!(!json.contains("BASE64_SECRET"));
    }

    /// Pi records a command the user ran as one row holding the command and
    /// what it printed. Both halves are the user's, so they pair as a call
    /// and its result under the user's own label.
    #[test]
    fn a_user_run_command_reads_as_a_call_and_its_result() {
        let projection = PI_LOG
            .parse_transcript(&fixture("v3-branched.jsonl"))
            .unwrap()
            .unwrap();

        let (blocks, timestamp) = projection
            .entries
            .iter()
            .find_map(|(_, entry)| match entry {
                LogEntry::User {
                    message, timestamp, ..
                } => match &message.content {
                    UserContent::Blocks(blocks)
                        if blocks
                            .iter()
                            .any(|block| matches!(block, ContentBlock::ToolUse { .. })) =>
                    {
                        Some((blocks, timestamp))
                    }
                    _ => None,
                },
                _ => None,
            })
            .expect("the bash execution renders as a user entry holding a call");

        assert_eq!(
            timestamp.as_deref(),
            Some("2024-03-01T00:07:00.000Z"),
            "the entry opens a run, which is stamped from the record"
        );

        let [
            ContentBlock::ToolUse {
                id,
                name,
                tool,
                input,
            },
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                ..
            },
        ] = blocks.as_slice()
        else {
            panic!("expected a call and its result, got {blocks:?}");
        };
        assert_eq!(*tool, Tool::UserShell);
        assert_eq!(name, "bash");
        assert_eq!(input, &json!({ "command": "false" }));
        assert_eq!(id, "ran-a-command", "the call takes the record's own id");
        assert_eq!(tool_use_id, id, "the result answers that call");

        let result = content.as_ref().unwrap().as_str().unwrap();
        assert!(result.contains("bash output searchable"));
        assert!(result.contains("Exit code: 1"));
        assert!(
            !result.contains("$ false"),
            "the command is the call's input, not part of its result: {result:?}"
        );
    }

    /// A cancelled command and a failed one each say so in the result, where
    /// the command's own output ends.
    #[test]
    fn a_command_that_failed_or_was_cancelled_says_so_in_its_result() {
        let row = |extra: &str| {
            let object = serde_json::from_str::<Value>(&format!(
                r#"{{"command":"sleep 5","output":"partial"{extra}}}"#
            ))
            .unwrap();
            bash_output(object.as_object().unwrap())
        };

        assert_eq!(row(""), "partial");
        assert_eq!(row(r#","exitCode":0"#), "partial");
        assert_eq!(row(r#","exitCode":130"#), "partial\nExit code: 130");
        assert_eq!(row(r#","cancelled":true"#), "partial\nCommand cancelled");
    }

    #[test]
    fn extracts_pi_conversation_metadata_from_active_dialogue() {
        let conversation = process_conversation_file(fixture("v3-branched.jsonl"), None, None)
            .unwrap()
            .unwrap();

        assert_eq!(conversation.source, Source::Pi);
        assert_eq!(
            conversation.session_id,
            "01912345-6789-7abc-8def-0123456789ab"
        );
        assert_eq!(conversation.custom_title.as_deref(), Some("Final Pi title"));
        assert_eq!(conversation.model.as_deref(), Some("openai/gpt-5"));
        assert_eq!(conversation.total_tokens, 190);
        // The session's last activity is the command the user ran at 00:07,
        // which the reader now stamps, rather than the answer before it.
        assert_eq!(conversation.duration_minutes, Some(6));
        assert_eq!(conversation.timestamp.timestamp(), 1_709_251_620);
        for searchable in [
            "tool output searchable",
            "bash output searchable",
            "visible custom searchable",
        ] {
            assert!(
                conversation.full_text.contains(searchable),
                "missing {searchable:?} from {:?}",
                conversation.full_text
            );
        }
        for hidden_metadata in [
            "compaction summary searchable",
            "branch summary searchable",
            "custom state searchable",
        ] {
            assert!(!conversation.full_text.contains(hidden_metadata));
        }
        assert!(!conversation.full_text.contains("HIDDEN_CUSTOM_SENTINEL"));
        assert!(!conversation.full_text.contains("ABANDONED_BRANCH_SENTINEL"));
        assert!(
            !conversation
                .preview
                .contains("compaction summary searchable")
        );
    }

    #[test]
    fn semantic_ranges_and_agent_ordinals_share_the_pi_projection() {
        let path = fixture("v3-branched.jsonl");
        let conversation = process_conversation_file(path.clone(), None, None)
            .unwrap()
            .unwrap();
        let transcript = AgentTranscript::load(path).unwrap();

        assert_eq!(conversation.message_count, 5);
        assert_eq!(transcript.messages.len(), 5);
        assert_eq!(
            conversation.semantic_turn_ranges,
            [1, 2, 5]
                .into_iter()
                .map(crate::agent::refs::MessageRange::single)
                .collect::<Vec<_>>()
        );
        assert_eq!(
            transcript
                .messages
                .iter()
                .map(|message| message.ordinal)
                .collect::<Vec<_>>(),
            (1..=5).collect::<Vec<_>>()
        );
    }

    #[test]
    fn malformed_lines_and_parent_chains_are_bounded_to_the_leaf_suffix() {
        let content = concat!(
            "{\"type\":\"session\",\"version\":3,\"id\":\"s\",\"timestamp\":\"2024-01-01T00:00:00Z\",\"cwd\":\"/tmp\"}\n",
            "{\"type\":\"message\",\"id\":\"root\",\"parentId\":null,\"timestamp\":\"2024-01-01T00:00:01Z\",\"message\":{\"role\":\"user\",\"content\":\"lost root\"}}\n",
            "malformed\n",
            "{\"type\":\"branch_summary\",\"id\":\"leaf\",\"parentId\":\"missing\",\"timestamp\":\"2024-01-01T00:00:02Z\",\"summary\":\"leaf suffix\"}\n"
        );
        let projection = parse_reader(std::io::Cursor::new(content), Source::Pi)
            .unwrap()
            .unwrap();
        assert_eq!(projection.malformed_lines, vec![3]);
        assert_eq!(projection.entries.len(), 1);
        assert!(matches!(
            &projection.entries[0].1,
            LogEntry::PiMetadata { text, .. } if text == "leaf suffix"
        ));
    }

    #[test]
    fn rejects_header_only_and_invalid_headers_as_conversations() {
        let invalid = std::io::Cursor::new("{\"type\":\"user\",\"id\":\"x\"}\n");
        assert!(parse_reader(invalid, Source::Pi).unwrap().is_none());

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("header.jsonl");
        std::fs::write(
            &path,
            "{\"type\":\"session\",\"version\":3,\"id\":\"s\",\"timestamp\":\"2024-01-01T00:00:00Z\",\"cwd\":\"/tmp\"}\n",
        )
        .unwrap();
        assert!(
            process_conversation_file(path, None, None)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn empty_session_name_clears_an_active_title() {
        let content = concat!(
            "{\"type\":\"session\",\"version\":3,\"id\":\"s\",\"timestamp\":\"2024-01-01T00:00:00Z\",\"cwd\":\"/tmp\"}\n",
            "{\"type\":\"message\",\"id\":\"u\",\"parentId\":null,\"timestamp\":\"2024-01-01T00:00:01Z\",\"message\":{\"role\":\"user\",\"content\":\"question\"}}\n",
            "{\"type\":\"session_info\",\"id\":\"n1\",\"parentId\":\"u\",\"timestamp\":\"2024-01-01T00:00:02Z\",\"name\":\"title\"}\n",
            "{\"type\":\"session_info\",\"id\":\"n2\",\"parentId\":\"n1\",\"timestamp\":\"2024-01-01T00:00:03Z\",\"name\":\"\"}\n"
        );
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("cleared.jsonl");
        std::fs::write(&path, content).unwrap();
        let conversation = process_conversation_file(path, None, None)
            .unwrap()
            .unwrap();
        assert_eq!(conversation.custom_title, None);
    }

    #[test]
    fn skips_v2_and_v3_entries_without_ids_before_resolving_the_leaf() {
        let content = concat!(
            "{\"type\":\"session\",\"version\":3,\"id\":\"s\",\"timestamp\":\"2024-01-01T00:00:00Z\",\"cwd\":\"/tmp\"}\n",
            "{\"type\":\"message\",\"id\":\"root\",\"parentId\":null,\"timestamp\":\"2024-01-01T00:00:01Z\",\"message\":{\"role\":\"user\",\"content\":\"question\"}}\n",
            "{\"type\":\"message\",\"parentId\":\"root\",\"timestamp\":\"2024-01-01T00:00:02Z\",\"message\":{\"role\":\"user\",\"content\":\"invalid leaf\"}}\n"
        );
        let projection = parse_reader(std::io::Cursor::new(content), Source::Pi)
            .unwrap()
            .unwrap();

        assert_eq!(projection.leaf_id.as_deref(), Some("root"));
        assert_eq!(projection.malformed_lines, vec![3]);
        assert_eq!(projection.entries.len(), 1);
    }

    #[test]
    fn migrates_v2_hook_messages_in_memory_and_counts_non_assistant_usage() {
        let content = concat!(
            "{\"type\":\"session\",\"version\":2,\"id\":\"s\",\"timestamp\":\"2024-01-01T00:00:00Z\",\"cwd\":\"/tmp\"}\n",
            "{\"type\":\"message\",\"id\":\"u\",\"parentId\":null,\"timestamp\":\"2024-01-01T00:00:01Z\",\"message\":{\"role\":\"user\",\"content\":\"question\"}}\n",
            "{\"type\":\"message\",\"id\":\"h\",\"parentId\":\"u\",\"timestamp\":\"2024-01-01T00:00:02Z\",\"message\":{\"role\":\"hookMessage\",\"customType\":\"extension notice\",\"content\":\"visible hook\",\"display\":true}}\n",
            "{\"type\":\"compaction\",\"id\":\"c\",\"parentId\":\"h\",\"timestamp\":\"2024-01-01T00:00:03Z\",\"summary\":\"summary\",\"firstKeptEntryId\":\"u\",\"tokensBefore\":1,\"usage\":{\"input\":2,\"output\":3,\"cacheRead\":5,\"cacheWrite\":7}}\n",
            "{\"type\":\"message\",\"id\":\"t\",\"parentId\":\"c\",\"timestamp\":\"2024-01-01T00:00:04Z\",\"message\":{\"role\":\"toolResult\",\"toolCallId\":\"call\",\"content\":[],\"usage\":{\"input\":11,\"output\":13,\"cacheRead\":17,\"cacheWrite\":19}}}\n"
        );
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("usage.jsonl");
        std::fs::write(&path, content).unwrap();

        let conversation = process_conversation_file(path, None, None)
            .unwrap()
            .unwrap();
        assert!(conversation.full_text.contains("visible hook"));
        assert_eq!(conversation.total_tokens, 77);
        let projection = parse_reader(std::io::Cursor::new(content), Source::Pi)
            .unwrap()
            .unwrap();
        assert!(matches!(
            &projection.entries[1].1,
            LogEntry::PiMetadata { label, .. } if label == "extension notice"
        ));
    }

    #[test]
    fn rename_appends_native_session_info_to_the_leaf() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("session.jsonl");
        std::fs::copy(fixture("v3-branched.jsonl"), &path).unwrap();

        append_session_rename(&path, "renamed\nPi").unwrap();
        let last = std::fs::read_to_string(&path)
            .unwrap()
            .lines()
            .last()
            .map(str::to_owned)
            .unwrap();
        let value: Value = serde_json::from_str(&last).unwrap();
        assert_eq!(value["type"], "session_info");
        assert_eq!(value["parentId"], "name2");
        assert_eq!(value["name"], "renamed Pi");
        assert_eq!(value["id"].as_str().unwrap().len(), 8);
    }

    #[test]
    fn parses_omp_title_slot_and_active_branch() {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/omp/v3.jsonl");
        let projection = PI_LOG.parse_transcript(&path).unwrap().unwrap();
        assert_eq!(projection.source, Source::Omp);
        assert_eq!(projection.header.id, "omp_session_custom_id");
        assert_eq!(projection.title.as_deref(), Some("OMP fixture title"));
        let conversation = process_conversation_file(path, None, None)
            .unwrap()
            .unwrap();
        assert_eq!(conversation.source, Source::Omp);
        assert_eq!(
            conversation.custom_title.as_deref(),
            Some("Updated OMP title")
        );
        assert_eq!(conversation.model.as_deref(), Some("openai/gpt-5.2"));
        assert!(conversation.full_text.contains("OMP active question"));
        assert!(!conversation.full_text.contains("OMP_ABANDONED_SENTINEL"));
    }

    #[test]
    fn omp_rename_updates_title_slot_and_appends_audit_entry() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("session.jsonl");
        std::fs::copy(
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/omp/v3.jsonl"),
            &path,
        )
        .unwrap();

        append_omp_session_rename(&path, "renamed\nOMP").unwrap();
        let contents = std::fs::read_to_string(&path).unwrap();
        let first_line = contents.split_inclusive('\n').next().unwrap();
        assert_eq!(first_line.len(), 256);
        let slot: Value = serde_json::from_str(first_line).unwrap();
        assert_eq!(slot["type"], "title");
        assert_eq!(slot["title"], "renamed OMP");
        let last: Value = serde_json::from_str(contents.lines().last().unwrap()).unwrap();
        assert_eq!(last["type"], "title_change");
        assert_eq!(last["parentId"], "a2");
        assert_eq!(last["title"], "renamed OMP");
        let reparsed = OMP_LOG.parse_transcript(&path).unwrap().unwrap();
        assert_eq!(reparsed.title.as_deref(), Some("renamed OMP"));
    }
}
