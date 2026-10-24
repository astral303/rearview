//! The JSONL transcript format Pi and OMP share.

mod tools;

use super::dropped::{DroppedRecords, SkippedKinds};
use super::{
    EarlierTurns, SessionFormat, SessionHeader, SessionProjection, append_exit_code,
    append_output_note, at_own_lines, rfc3339_from_millis, trim_blank_lines,
};
use crate::agent::transcript::tool_result_text;
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
/// `session_id`. A UUID matches in any case; an id the user chose matches as
/// typed.
///
/// An id is not unique across logs — a branch copied within a project keeps
/// the id it came from — so this returns all of them rather than the first.
pub(crate) fn sessions_with_id(
    root: &Path,
    depth: usize,
    session_id: &str,
) -> Result<Vec<PathBuf>> {
    use crate::history::provider::walk;
    Ok(
        walk::jsonl_files_at_depth(&walk::SystemLister, root, depth)?
            .into_iter()
            .filter(|path| {
                header_session_id(path)
                    .is_some_and(|stored| crate::search::session_id_matches(&stored, session_id))
            })
            .collect(),
    )
}

/// The session id the Pi-family log at `path` states in a header the reader
/// accepts, and the agent the list attributes the log to, as
/// [`AcceptedHeader::source`] decides it. `None` when the log has no such
/// header. Reads only up to the header, skipping blank and malformed lines as
/// the reader does.
pub(crate) fn read_header(path: &Path, default_source: Source) -> Result<Option<(String, Source)>> {
    let file = File::open(path)?;
    let mut records = Vec::with_capacity(2);
    for line in BufReader::new(file).lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(value) = serde_json::from_str::<Value>(&line) {
            records.push(value);
            if records.len() == 2 {
                break;
            }
        }
    }
    let Some(first) = records.first() else {
        return Ok(None);
    };
    Ok(AcceptedHeader::of(first, records.get(1))
        .map(|header| (header.id.to_owned(), header.source(default_source))))
}

/// A Pi-family log's header as the reader accepts it: a `session` record
/// stating an id, a timestamp and a directory, in a supported format
/// version, either first or after an OMP title slot.
struct AcceptedHeader<'a> {
    title_slot: Option<&'a Map<String, Value>>,
    object: &'a Map<String, Value>,
    id: &'a str,
    timestamp: &'a str,
    cwd: &'a str,
    version: u64,
}

impl<'a> AcceptedHeader<'a> {
    /// The header among a log's first two records, `first` and `second`.
    fn of(first: &'a Value, second: Option<&'a Value>) -> Option<Self> {
        let title_slot = first
            .as_object()
            .filter(|object| object.get("type").and_then(Value::as_str) == Some("title"));
        let object = if title_slot.is_some() { second? } else { first }.as_object()?;
        if object.get("type").and_then(Value::as_str) != Some("session") {
            return None;
        }
        let text = |field| object.get(field).and_then(Value::as_str);
        let version = object.get("version").and_then(Value::as_u64).unwrap_or(1);
        if !(1..=MAX_SUPPORTED_VERSION).contains(&version) {
            return None;
        }
        Some(Self {
            title_slot,
            object,
            id: text("id")?,
            timestamp: text("timestamp")?,
            cwd: text("cwd")?,
            version,
        })
    }

    /// OMP for a log with a title slot, whichever directory holds it;
    /// `default_source` otherwise, since such a log does not name its agent.
    fn source(&self, default_source: Source) -> Source {
        if self.title_slot.is_some() {
            Source::Omp
        } else {
            default_source
        }
    }

    /// The title the slot states, else the one the header states; `None`
    /// when neither states one that is not empty.
    fn title(&self) -> Option<String> {
        self.title_slot
            .and_then(|slot| slot.get("title"))
            .and_then(Value::as_str)
            .filter(|title| !title.is_empty())
            .or_else(|| self.object.get("title").and_then(Value::as_str))
            .filter(|title| !title.is_empty())
            .map(str::to_owned)
    }

    /// How many records the header and its title slot take up.
    fn record_count(&self) -> usize {
        1 + usize::from(self.title_slot.is_some())
    }
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
    /// A Pi-family thread splices in under its file stem: OMP names a
    /// sub-agent transcript `<outputId>.jsonl`, after the name the parent's
    /// tool call gave the sub-agent.
    fn read_transcript(
        &self,
        _admitted: crate::history::Admitted,
        path: &Path,
        dropped: DroppedRecords,
    ) -> Result<Option<SessionProjection>> {
        let file = File::open(path)?;
        Ok(
            parse_reader(BufReader::new(file), self.default_source, dropped)?.map(
                |mut projection| {
                    projection.header.thread_label = path
                        .file_stem()
                        .and_then(|stem| stem.to_str())
                        .map(str::to_owned);
                    projection
                },
            ),
        )
    }
}

fn parse_reader(
    reader: impl BufRead,
    default_source: Source,
    mut dropped: DroppedRecords,
) -> Result<Option<SessionProjection>> {
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

    let Some(header) = parsed
        .first()
        .and_then(|(_, first)| AcceptedHeader::of(first, parsed.get(1).map(|(_, second)| second)))
    else {
        return Ok(None);
    };
    let source = header.source(default_source);
    let title = header.title();
    let id = header.id.to_owned();
    let timestamp = header.timestamp.to_owned();
    let cwd = header.cwd.to_owned();
    let version = header.version;
    let header_records = header.record_count();
    // The header records the session file's format, not the release that
    // wrote it.
    dropped.record_version(|| Some(format!("session format {version}")));

    let mut raw_entries = Vec::new();
    let mut previous_id: Option<String> = None;
    for (line, value) in parsed.into_iter().skip(header_records) {
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
        normalize_entry(&raw.value, &raw.id, source, &mut dropped).map(|entry| (raw.line, entry))
    }));

    Ok(Some(SessionProjection {
        source,
        header: SessionHeader {
            version,
            id,
            timestamp,
            cwd: PathBuf::from(cwd),
            thread_label: None,
            subagent_identity: Default::default(),
        },
        title,
        entries: at_own_lines(entries),
        leaf_id,
        malformed_lines,
        dropped,
        earlier_turns: EarlierTurns::Read,
    }))
}

fn normalize_entry(
    value: &Value,
    entry_id: &str,
    source: Source,
    dropped: &mut DroppedRecords,
) -> Option<LogEntry> {
    let object = value.as_object()?;
    let kind = object.get("type").and_then(Value::as_str);
    let entry_type = kind.unwrap_or("unknown");
    let timestamp = object
        .get("timestamp")
        .and_then(Value::as_str)
        .map(str::to_owned);
    match entry_type {
        "message" => {
            normalize_message(object.get("message")?, entry_id, timestamp, source, dropped)
        }
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
        // Tokens spent outside the conversation, such as prompt-cache warming
        // during a long tool run. Pi adds entries of every `kind` to its
        // `/session` totals.
        "usage" => metadata_with_usage(
            "Usage",
            "",
            timestamp,
            false,
            object.get("usage").and_then(pi_usage),
        ),
        // OMP's replay starts after the latest `/clear`; rearview keeps the
        // earlier turns and marks the boundary between them.
        "reset_boundary" => metadata("/clear", "Context cleared", timestamp, true),
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
        _ => {
            SkippedKinds {
                field: "type",
                values: skipped_kinds(source).entry_types,
            }
            .count(kind, dropped);
            None
        }
    }
}

struct PiLogSkippedKinds {
    entry_types: &'static [&'static str],
    message_roles: &'static [&'static str],
}

static SKIPPED_FOR_PI: PiLogSkippedKinds = PiLogSkippedKinds {
    entry_types: &[
        // Settings, bookmarks and extension state, with no conversation text.
        "thinking_level_change",
        "label",
        "custom",
        // `context_edit` changes only what Pi sends the model; Pi shows the
        // original messages.
        "context_edit",
    ],
    // `system` messages hold the system prompt and tool changes; Pi does not
    // show them.
    message_roles: &["system"],
};

static SKIPPED_FOR_OMP: PiLogSkippedKinds = PiLogSkippedKinds {
    entry_types: &[
        // Settings, bookmarks and extension state, with no conversation text.
        "thinking_level_change",
        "label",
        "custom",
        // Session state OMP restores when it reopens a session, and does not show.
        "service_tier_change",
        "ttsr_injection",
        "mode_change",
        "credential_pin",
        // A sub-agent's starting context; its task also arrives as the first
        // user message.
        "session_init",
    ],
    // `developer` messages are instructions to the model; OMP does not show
    // them.
    message_roles: &["developer"],
};

fn skipped_kinds(source: Source) -> &'static PiLogSkippedKinds {
    match source {
        Source::Pi => &SKIPPED_FOR_PI,
        _ => &SKIPPED_FOR_OMP,
    }
}

/// `entry_id` is the record's own id, which only the `bashExecution` arm
/// needs: it pairs the call it builds with the result beside it.
fn normalize_message(
    message: &Value,
    entry_id: &str,
    timestamp: Option<String>,
    source: Source,
    dropped: &mut DroppedRecords,
) -> Option<LogEntry> {
    let object = message.as_object()?;
    let Some(role) = object.get("role").and_then(Value::as_str) else {
        dropped.unrecognised("message.role", None);
        return None;
    };
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
            source_tool_use_id: None,
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
            timestamp,
            uuid: None,
            cwd: None,
            parent_tool_use_id: None,
            source_tool_use_id: None,
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
            source_tool_use_id: None,
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
        _ => {
            SkippedKinds {
                field: "message.role",
                values: skipped_kinds(source).message_roles,
            }
            .count(Some(role), dropped);
            None
        }
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
    rfc3339_from_millis(value?.as_i64()?)
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
    let mut text = tool_result_text(&Value::Array(blocks)).unwrap_or_default();
    if object.get("isError").and_then(Value::as_bool) == Some(true) {
        text = format!("Error: {text}");
    }
    json!(text)
}

/// The command's output, plus the lines Pi records beside it: a non-zero exit
/// and a cancellation. The command itself is the call's input, not part of its
/// result.
fn bash_output(object: &Map<String, Value>) -> String {
    let mut text =
        trim_blank_lines(object.get("output").and_then(Value::as_str).unwrap_or("")).to_owned();
    if let Some(code) = object.get("exitCode").and_then(Value::as_i64)
        && code != 0
    {
        append_exit_code(&mut text, code);
    }
    if object.get("cancelled").and_then(Value::as_bool) == Some(true) {
        append_output_note(&mut text, "Command cancelled");
    }
    tool_result_text(&Value::String(text)).unwrap_or_default()
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

/// Append a `session_info` record titling the Pi session at `path`, read
/// with `permit`, written at `renamed_at`.
pub fn append_session_rename(
    permit: crate::history::ParsePermit<'_>,
    path: &Path,
    title: &str,
    renamed_at: DateTime<Utc>,
) -> Result<()> {
    let projection = PI_LOG.parse_transcript(permit, path)?.ok_or_else(|| {
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
    let id = unused_record_id(&projection.header.id, renamed_at, &ids);
    let entry = json!({
        "type": "session_info",
        "id": id,
        "parentId": projection.leaf_id,
        "timestamp": renamed_at.to_rfc3339_opts(SecondsFormat::Millis, true),
        "name": title.replace(['\r', '\n'], " ").trim(),
    });
    let mut file = OpenOptions::new().append(true).open(path)?;
    writeln!(file, "{entry}")?;
    Ok(())
}

/// Write the OMP session at `path`, read with `permit`, with a title slot and
/// a `title_change` record naming `title`, both stamped `renamed_at`.
pub fn append_omp_session_rename(
    permit: crate::history::ParsePermit<'_>,
    path: &Path,
    title: &str,
    renamed_at: DateTime<Utc>,
) -> Result<()> {
    const TITLE_SLOT_BYTES: usize = 256;

    let projection = OMP_LOG.parse_transcript(permit, path)?.ok_or_else(|| {
        AppError::ConfigError(format!("{} is not a valid OMP session", path.display()))
    })?;
    let title = title.replace(['\r', '\n'], " ").trim().to_owned();
    let timestamp = renamed_at.to_rfc3339_opts(SecondsFormat::Millis, true);
    let mut ids = HashSet::new();
    let contents = std::fs::read_to_string(path)?;
    for line in contents.lines() {
        if let Ok(value) = serde_json::from_str::<Value>(line)
            && let Some(id) = value.get("id").and_then(Value::as_str)
        {
            ids.insert(id.to_owned());
        }
    }
    let id = unused_record_id(&projection.header.id, renamed_at, &ids);
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

/// The first eight-hex-digit record id derived from `session_id` and
/// `written_at` that `taken` does not hold.
fn unused_record_id(
    session_id: &str,
    written_at: DateTime<Utc>,
    taken: &HashSet<String>,
) -> String {
    let nanos = written_at.timestamp_nanos_opt().unwrap_or(0);
    (0u64..)
        .map(|counter| {
            let seed = format!("{session_id}:{nanos}:{counter}");
            blake3::hash(seed.as_bytes()).to_hex()[..8].to_owned()
        })
        .find(|id| !taken.contains(id))
        .expect("unbounded ID sequence has a free value")
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

/// Fixtures for a session directory Pi and OMP share.
#[cfg(test)]
pub(crate) mod test_support {
    use std::path::{Path, PathBuf};

    /// The OMP session fixture copied twice into `directory`, as written and
    /// without its title slot, so both state one header id and the list shows
    /// the first under OMP and the second under Pi.
    pub(crate) fn write_titled_and_untitled(directory: &Path) -> (PathBuf, PathBuf) {
        let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/omp/v3.jsonl");
        std::fs::create_dir_all(directory).unwrap();
        let titled = directory.join("titled.jsonl");
        std::fs::copy(&fixture, &titled).unwrap();
        let untitled = directory.join("untitled.jsonl");
        copy_without_title_slot(&fixture, &untitled);
        (titled, untitled)
    }

    fn copy_without_title_slot(fixture: &Path, destination: &Path) {
        let contents = std::fs::read_to_string(fixture).unwrap();
        let (title_slot, rest) = contents.split_once('\n').unwrap();
        assert!(
            title_slot.contains(r#""type":"title""#),
            "{fixture:?} has no title slot"
        );
        std::fs::write(destination, rest).unwrap();
    }
}

#[cfg(test)]
mod tests {

    fn omp_fixture() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/omp/v3.jsonl")
    }

    #[test]
    fn header_states_session_id_whatever_file_is_called() {
        assert_eq!(
            header_session_id(&fixture("v1.jsonl")).as_deref(),
            Some("custom_v1_id")
        );
    }

    /// OMP writes a title slot ahead of the header, so the id is on the second
    /// record rather than the first.
    #[test]
    fn title_slot_ahead_of_header_does_not_hide_session_id() {
        assert_eq!(
            header_session_id(&omp_fixture()).as_deref(),
            Some("omp_session_custom_id")
        );
    }

    #[test]
    fn file_that_is_not_pi_family_log_states_no_session_id() {
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

    /// The header attributes every Pi-family fixture as the full parse does:
    /// by every registered format in turn, as a shared session directory
    /// reads a log, and by each agent's own reader, as its own tree does.
    #[test]
    fn a_header_attributes_a_log_as_its_parse_does() {
        let fixtures = [
            fixture("v1.jsonl"),
            fixture("v2.jsonl"),
            fixture("v3-branched.jsonl"),
            omp_fixture(),
            omp_fixture().with_file_name("subagent.jsonl"),
        ];
        let attribution = |projection: Option<SessionProjection>| {
            projection.map(|projection| (projection.header.id, projection.source))
        };
        for path in fixtures {
            let shown = path.display();
            assert_eq!(
                read_header(&path, Source::Pi).unwrap(),
                attribution(
                    crate::history::format::parse_transcript(crate::history::FOR_FIXTURES, &path)
                        .unwrap()
                ),
                "registry: {shown}"
            );
            assert_eq!(
                read_header(&path, Source::Pi).unwrap(),
                attribution(
                    PI_LOG
                        .parse_transcript(crate::history::FOR_FIXTURES, &path)
                        .unwrap()
                ),
                "Pi's reader: {shown}"
            );
            assert_eq!(
                read_header(&path, Source::Omp).unwrap(),
                attribution(
                    OMP_LOG
                        .parse_transcript(crate::history::FOR_FIXTURES, &path)
                        .unwrap()
                ),
                "OMP's reader: {shown}"
            );
        }
    }

    /// Two logs in one project can state the same id — a branch keeps the id
    /// it was copied from — so every match comes back.
    #[test]
    fn every_log_stating_id_is_found() {
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

    /// Pi generates a UUID unless the user chose an id, so a UUID is found in
    /// any case while a chosen id is found as typed.
    #[test]
    fn uuid_in_header_is_found_in_any_case_and_chosen_id_as_typed() {
        let directory = tempfile::tempdir().unwrap();
        let project = directory.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let generated = project.join("generated.jsonl");
        std::fs::write(
            &generated,
            "{\"type\":\"session\",\"version\":3,\"id\":\"01a05936-420b-7f5a-bb7a-1cb44cb94841\",\"timestamp\":\"2026-08-31T19:05:23.211Z\",\"cwd\":\"/tmp\"}\n",
        )
        .unwrap();
        std::fs::copy(fixture("v1.jsonl"), project.join("chosen.jsonl")).unwrap();

        assert_eq!(
            sessions_with_id(directory.path(), 1, "01A05936-420B-7F5A-BB7A-1CB44CB94841").unwrap(),
            vec![generated]
        );
        assert!(
            sessions_with_id(directory.path(), 1, "CUSTOM_V1_ID")
                .unwrap()
                .is_empty()
        );
    }
    use super::*;
    use crate::agent::transcript::AgentTranscript;
    use crate::clock::TestClock;
    use crate::history::format::dropped::DropOutcome;
    use crate::history::{Source, parser::process_conversation_file};

    /// The same session with OMP's `title` slot in front. The slot attributes
    /// the session to OMP.
    fn with_omp_title_slot(session: &str) -> String {
        format!("{}\n{session}", r#"{"type":"title","v":1,"title":"Named"}"#)
    }

    /// Reads `content` with `Source::Pi` as the default, then asserts the
    /// source the reader attributes it to and the kinds it drops, each counted
    /// once under `session format 3`, in the report's order.
    fn assert_counted_drops(content: &str, source: Source, drops: &[(&str, DropOutcome)]) {
        let projection = parse_reader(
            std::io::Cursor::new(content),
            Source::Pi,
            DroppedRecords::counting(),
        )
        .unwrap()
        .expect("a session");

        assert_eq!(projection.source, source);
        let expected = drops
            .iter()
            .map(|&(kind, outcome)| (Some("session format 3"), kind, outcome, 1))
            .collect::<Vec<_>>();
        assert_eq!(
            projection.dropped.counts().collect::<Vec<_>>(),
            expected,
            "{source:?}"
        );
    }

    #[test]
    fn counting_names_skipped_and_unrecognised_entry_types_and_roles_for_pi_and_omp() {
        let session = [
            r#"{"type":"session","version":3,"id":"s","timestamp":"2024-01-01T00:00:00Z","cwd":"/tmp"}"#,
            r#"{"type":"message","id":"a","parentId":null,"message":{"role":"user","content":"hi"}}"#,
            r#"{"type":"teleport","id":"b","parentId":"a"}"#,
            r#"{"type":"label","id":"c","parentId":"b"}"#,
            r#"{"type":"message","id":"d","parentId":"c","message":{"role":"oracle","content":"x"}}"#,
        ]
        .join("\n");

        for (content, source) in [
            (session.clone(), Source::Pi),
            (with_omp_title_slot(&session), Source::Omp),
        ] {
            assert_counted_drops(
                &content,
                source,
                &[
                    ("message.role=oracle", DropOutcome::Unrecognised),
                    ("type=label", DropOutcome::Skipped),
                    ("type=teleport", DropOutcome::Unrecognised),
                ],
            );
        }
    }

    #[test]
    fn kinds_pi_review_decided_against_count_as_skipped_for_pi_only() {
        let session = [
            r#"{"type":"session","version":3,"id":"s","timestamp":"2024-01-01T00:00:00Z","cwd":"/tmp"}"#,
            r#"{"type":"message","id":"a","parentId":null,"message":{"role":"user","content":"hi"}}"#,
            r#"{"type":"label","id":"b","parentId":"a"}"#,
            r#"{"type":"context_edit","id":"c","parentId":"b"}"#,
            r#"{"type":"message","id":"d","parentId":"c","message":{"role":"system","content":"prompt"}}"#,
        ]
        .join("\n");

        for (content, source, decided_outcome) in [
            (session.clone(), Source::Pi, DropOutcome::Skipped),
            (
                with_omp_title_slot(&session),
                Source::Omp,
                DropOutcome::Unrecognised,
            ),
        ] {
            assert_counted_drops(
                &content,
                source,
                &[
                    ("message.role=system", decided_outcome),
                    ("type=context_edit", decided_outcome),
                    ("type=label", DropOutcome::Skipped),
                ],
            );
        }
    }

    #[test]
    fn kinds_omp_review_decided_against_count_as_skipped_for_omp_only_and_model_usage_stays_unrecognised()
     {
        let session = [
            r#"{"type":"session","version":3,"id":"s","timestamp":"2024-01-01T00:00:00Z","cwd":"/tmp"}"#,
            r#"{"type":"message","id":"a","parentId":null,"message":{"role":"user","content":"hi"}}"#,
            r#"{"type":"mode_change","id":"b","parentId":"a","mode":"plan"}"#,
            r#"{"type":"session_init","id":"c","parentId":"b","task":"review"}"#,
            r#"{"type":"model_usage","id":"d","parentId":"c","usage":{"input":1,"output":2}}"#,
            r#"{"type":"message","id":"e","parentId":"d","message":{"role":"developer","content":"rules"}}"#,
        ]
        .join("\n");

        for (content, source, decided_outcome) in [
            (session.clone(), Source::Pi, DropOutcome::Unrecognised),
            (
                with_omp_title_slot(&session),
                Source::Omp,
                DropOutcome::Skipped,
            ),
        ] {
            assert_counted_drops(
                &content,
                source,
                &[
                    ("message.role=developer", decided_outcome),
                    ("type=mode_change", decided_outcome),
                    ("type=model_usage", DropOutcome::Unrecognised),
                    ("type=session_init", decided_outcome),
                ],
            );
        }
    }

    #[test]
    fn omp_clear_shows_context_cleared_row_between_turns_before_and_after_it() {
        let session = [
            r#"{"type":"session","version":3,"id":"s","timestamp":"2024-01-01T00:00:00Z","cwd":"/tmp"}"#,
            r#"{"type":"message","id":"a","parentId":null,"timestamp":"2024-01-01T00:00:01Z","message":{"role":"user","content":"before"}}"#,
            r#"{"type":"reset_boundary","id":"b","parentId":"a","timestamp":"2024-01-01T00:00:02Z"}"#,
            r#"{"type":"message","id":"c","parentId":"b","timestamp":"2024-01-01T00:00:03Z","message":{"role":"user","content":"after"}}"#,
        ]
        .join("\n");
        let projection = parse_reader(
            std::io::Cursor::new(with_omp_title_slot(&session)),
            Source::Pi,
            DroppedRecords::counting(),
        )
        .unwrap()
        .expect("a session");

        let entries = projection
            .entries
            .iter()
            .map(|(_, entry)| entry)
            .collect::<Vec<_>>();
        assert!(
            matches!(
                entries.as_slice(),
                [
                    LogEntry::CustomTitle { .. },
                    LogEntry::User {
                        message: UserMessage {
                            content: UserContent::String(before),
                            ..
                        },
                        ..
                    },
                    LogEntry::PiMetadata {
                        label,
                        text,
                        timestamp: Some(timestamp),
                        searchable: true,
                        ..
                    },
                    LogEntry::User {
                        message: UserMessage {
                            content: UserContent::String(after),
                            ..
                        },
                        ..
                    },
                ] if before == "before"
                    && label == "/clear"
                    && text == "Context cleared"
                    && timestamp == "2024-01-01T00:00:02Z"
                    && after == "after"
            ),
            "{entries:#?}"
        );
        assert_eq!(projection.dropped.counts().count(), 0);
    }

    fn fixture(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/pi")
            .join(name)
    }

    /// The blocks and timestamp of the first user entry whose blocks satisfy
    /// `wanted`.
    fn first_user_entry_with(
        projection: &SessionProjection,
        wanted: impl Fn(&[ContentBlock]) -> bool,
    ) -> Option<(&[ContentBlock], Option<&str>)> {
        projection
            .entries
            .iter()
            .find_map(|(_, entry)| match entry {
                LogEntry::User {
                    message, timestamp, ..
                } => match &message.content {
                    UserContent::Blocks(blocks) if wanted(blocks) => {
                        Some((blocks.as_slice(), timestamp.as_deref()))
                    }
                    _ => None,
                },
                _ => None,
            })
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
            let projection = PI_LOG
                .parse_transcript(crate::history::FOR_FIXTURES, &fixture(name))
                .unwrap()
                .unwrap();
            assert_eq!(projection.header.version, version);
            assert_eq!(projection.header.id, id);
            assert_eq!(std::fs::read(fixture(name)).unwrap(), before);
        }
    }

    #[test]
    fn projects_only_active_branch_and_sanitizes_images() {
        let projection = PI_LOG
            .parse_transcript(crate::history::FOR_FIXTURES, &fixture("v3-branched.jsonl"))
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
    fn user_run_command_reads_as_call_and_its_result() {
        let projection = PI_LOG
            .parse_transcript(crate::history::FOR_FIXTURES, &fixture("v3-branched.jsonl"))
            .unwrap()
            .unwrap();

        let (blocks, timestamp) = first_user_entry_with(&projection, |blocks| {
            blocks
                .iter()
                .any(|block| matches!(block, ContentBlock::ToolUse { .. }))
        })
        .expect("the bash execution renders as a user entry holding a call");

        assert_eq!(
            timestamp,
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
        ] = blocks
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

    /// Pi writes a `toolResult` record when the result arrives, so the
    /// projected result carries that record's stamp and ends the run.
    #[test]
    fn tool_result_is_stamped_from_its_record() {
        let projection = PI_LOG
            .parse_transcript(crate::history::FOR_FIXTURES, &fixture("v3-branched.jsonl"))
            .unwrap()
            .unwrap();

        let (_, timestamp) = first_user_entry_with(&projection, |blocks| {
            blocks
                .iter()
                .all(|block| matches!(block, ContentBlock::ToolResult { .. }))
        })
        .expect("the tool result renders as a user entry holding only the result");

        assert_eq!(timestamp, Some("2024-03-01T00:06:00.000Z"));
    }

    /// A cancelled command and a failed one each say so in the result, where
    /// the command's own output ends.
    #[test]
    fn command_that_failed_or_was_cancelled_says_so_in_its_result() {
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
    fn semantic_ranges_and_agent_ordinals_share_pi_projection() {
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
    fn malformed_lines_and_parent_chains_are_bounded_to_leaf_suffix() {
        let content = concat!(
            "{\"type\":\"session\",\"version\":3,\"id\":\"s\",\"timestamp\":\"2024-01-01T00:00:00Z\",\"cwd\":\"/tmp\"}\n",
            "{\"type\":\"message\",\"id\":\"root\",\"parentId\":null,\"timestamp\":\"2024-01-01T00:00:01Z\",\"message\":{\"role\":\"user\",\"content\":\"lost root\"}}\n",
            "malformed\n",
            "{\"type\":\"branch_summary\",\"id\":\"leaf\",\"parentId\":\"missing\",\"timestamp\":\"2024-01-01T00:00:02Z\",\"summary\":\"leaf suffix\"}\n"
        );
        let projection = parse_reader(
            std::io::Cursor::new(content),
            Source::Pi,
            DroppedRecords::default(),
        )
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
        assert!(
            parse_reader(invalid, Source::Pi, DroppedRecords::default())
                .unwrap()
                .is_none()
        );

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
    fn empty_session_name_clears_active_title() {
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
    fn skips_v2_and_v3_entries_without_ids_before_resolving_leaf() {
        let content = concat!(
            "{\"type\":\"session\",\"version\":3,\"id\":\"s\",\"timestamp\":\"2024-01-01T00:00:00Z\",\"cwd\":\"/tmp\"}\n",
            "{\"type\":\"message\",\"id\":\"root\",\"parentId\":null,\"timestamp\":\"2024-01-01T00:00:01Z\",\"message\":{\"role\":\"user\",\"content\":\"question\"}}\n",
            "{\"type\":\"message\",\"parentId\":\"root\",\"timestamp\":\"2024-01-01T00:00:02Z\",\"message\":{\"role\":\"user\",\"content\":\"invalid leaf\"}}\n"
        );
        let projection = parse_reader(
            std::io::Cursor::new(content),
            Source::Pi,
            DroppedRecords::default(),
        )
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
        let projection = parse_reader(
            std::io::Cursor::new(content),
            Source::Pi,
            DroppedRecords::default(),
        )
        .unwrap()
        .unwrap();
        assert!(matches!(
            &projection.entries[1].1,
            LogEntry::PiMetadata { label, .. } if label == "extension notice"
        ));
    }

    #[test]
    fn usage_entries_of_every_kind_add_to_token_count() {
        let content = [
            r#"{"type":"session","version":3,"id":"s","timestamp":"2024-01-01T00:00:00Z","cwd":"/tmp"}"#,
            r#"{"type":"message","id":"u","parentId":null,"timestamp":"2024-01-01T00:00:01Z","message":{"role":"user","content":"question"}}"#,
            r#"{"type":"message","id":"a","parentId":"u","timestamp":"2024-01-01T00:00:02Z","message":{"role":"assistant","content":[{"type":"text","text":"answer"}],"usage":{"input":1,"output":2,"cacheRead":3,"cacheWrite":4}}}"#,
            r#"{"type":"usage","id":"w","parentId":"a","timestamp":"2024-01-01T00:00:03Z","kind":"cache_warm","provider":"anthropic","model":"m","usage":{"input":10,"output":0,"cacheRead":100,"cacheWrite":0,"totalTokens":110}}"#,
            r#"{"type":"usage","id":"x","parentId":"w","timestamp":"2024-01-01T00:00:04Z","kind":"another_kind","provider":"anthropic","model":"m","usage":{"input":5,"output":0,"cacheRead":0,"cacheWrite":0,"totalTokens":5}}"#,
        ]
        .join("\n");
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("usage.jsonl");
        std::fs::write(&path, &content).unwrap();

        let conversation = process_conversation_file(path, None, None)
            .unwrap()
            .unwrap();
        assert_eq!(conversation.total_tokens, 10 + 110 + 5);

        let projection = parse_reader(
            std::io::Cursor::new(content),
            Source::Pi,
            DroppedRecords::counting(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            projection.dropped.counts().count(),
            0,
            "the report doesn't list a usage entry"
        );
    }

    #[test]
    fn rename_appends_native_session_info_to_leaf() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("session.jsonl");
        std::fs::copy(fixture("v3-branched.jsonl"), &path).unwrap();

        append_session_rename(
            crate::history::FOR_FIXTURES,
            &path,
            "renamed\nPi",
            TestClock::start_wall(),
        )
        .unwrap();
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
        assert_eq!(value["timestamp"], "2026-07-01T12:00:00.000Z");
        assert_eq!(value["id"].as_str().unwrap().len(), 8);
    }

    #[test]
    fn parses_omp_title_slot_and_active_branch() {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/omp/v3.jsonl");
        let projection = PI_LOG
            .parse_transcript(crate::history::FOR_FIXTURES, &path)
            .unwrap()
            .unwrap();
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

        append_omp_session_rename(
            crate::history::FOR_FIXTURES,
            &path,
            "renamed\nOMP",
            TestClock::start_wall(),
        )
        .unwrap();
        let contents = std::fs::read_to_string(&path).unwrap();
        let first_line = contents.split_inclusive('\n').next().unwrap();
        assert_eq!(first_line.len(), 256);
        let slot: Value = serde_json::from_str(first_line).unwrap();
        assert_eq!(slot["type"], "title");
        assert_eq!(slot["title"], "renamed OMP");
        assert_eq!(slot["updatedAt"], "2026-07-01T12:00:00.000Z");
        let last: Value = serde_json::from_str(contents.lines().last().unwrap()).unwrap();
        assert_eq!(last["type"], "title_change");
        assert_eq!(last["parentId"], "a2");
        assert_eq!(last["title"], "renamed OMP");
        assert_eq!(last["timestamp"], "2026-07-01T12:00:00.000Z");
        let reparsed = OMP_LOG
            .parse_transcript(crate::history::FOR_FIXTURES, &path)
            .unwrap()
            .unwrap();
        assert_eq!(reparsed.title.as_deref(), Some("renamed OMP"));
    }

    /// A nested output id is dot-qualified, `Parent.Child.jsonl`, and the
    /// whole of it is the label; only the extension comes off.
    #[test]
    fn threads_label_is_its_file_stem() {
        let directory = tempfile::tempdir().unwrap();
        let source =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/omp/subagent.jsonl");
        for (name, label) in [
            ("worker.jsonl", "worker"),
            ("worker.reviewer.jsonl", "worker.reviewer"),
        ] {
            let path = directory.path().join(name);
            std::fs::copy(&source, &path).unwrap();

            let projection = OMP_LOG
                .parse_transcript(crate::history::FOR_FIXTURES, &path)
                .unwrap()
                .unwrap();

            assert_eq!(projection.header.thread_label(), label);
            assert_eq!(projection.header.id, "omp_subagent_id");
        }
    }
}
