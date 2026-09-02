use crate::agent::transcript::bounded_tool_result_text;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type")]
#[serde(rename_all = "lowercase")]
pub enum LogEntry {
    Summary {
        summary: String,
    },
    User {
        message: UserMessage,
        /// ISO 8601 timestamp when this message was sent
        #[serde(default)]
        timestamp: Option<String>,
        /// UUID for linking with turn_duration entries
        #[allow(dead_code)]
        uuid: Option<String>,
        /// The working directory when this message was sent
        cwd: Option<String>,
        /// When set, this message is part of a subagent conversation
        /// spawned by the Task tool call with this ID
        #[serde(default, rename = "parent_tool_use_id")]
        parent_tool_use_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<TokenUsage>,
    },
    Assistant {
        message: AssistantMessage,
        #[serde(default)]
        agent: Option<String>,
        /// ISO 8601 timestamp when this message was sent
        #[serde(default)]
        timestamp: Option<String>,
        /// UUID for linking with turn_duration entries
        #[allow(dead_code)]
        uuid: Option<String>,
        /// When set, this message is part of a subagent conversation
        /// spawned by the Task tool call with this ID
        #[serde(default, rename = "parent_tool_use_id")]
        parent_tool_use_id: Option<String>,
    },
    #[serde(rename = "file-history-snapshot")]
    #[allow(dead_code)]
    FileHistorySnapshot {
        #[serde(rename = "messageId")]
        message_id: String,
        snapshot: serde_json::Value,
        #[serde(rename = "isSnapshotUpdate")]
        is_snapshot_update: bool,
    },
    Progress {
        data: serde_json::Value,
        #[allow(dead_code)]
        #[serde(flatten)]
        extra: serde_json::Value,
    },
    #[allow(dead_code)]
    System {
        subtype: String,
        level: Option<String>,
        /// Duration in milliseconds for turn_duration entries
        #[serde(rename = "durationMs")]
        duration_ms: Option<u64>,
        /// Parent UUID for linking turn_duration to preceding message
        #[serde(rename = "parentUuid")]
        parent_uuid: Option<String>,
        #[serde(flatten)]
        extra: serde_json::Value,
    },
    #[serde(rename = "custom-title")]
    CustomTitle {
        #[serde(rename = "customTitle")]
        custom_title: String,
    },
    #[serde(rename = "ai-title")]
    AiTitle {
        #[serde(rename = "aiTitle")]
        ai_title: String,
    },
    #[serde(rename = "agent-name")]
    AgentName {
        #[allow(dead_code)]
        #[serde(rename = "agentName")]
        agent_name: String,
    },
    #[serde(rename = "permission-mode")]
    PermissionMode {
        #[allow(dead_code)]
        #[serde(flatten)]
        extra: serde_json::Value,
    },
    #[serde(rename = "pi-metadata")]
    PiMetadata {
        label: String,
        text: String,
        #[serde(default)]
        timestamp: Option<String>,
        #[serde(default = "default_true")]
        searchable: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<TokenUsage>,
    },
    #[serde(other)]
    Unknown,
}

impl LogEntry {
    /// The timestamp the provider recorded for the entry. Only user,
    /// assistant and metadata entries carry one.
    pub fn timestamp(&self) -> Option<&str> {
        match self {
            LogEntry::User { timestamp, .. }
            | LogEntry::Assistant { timestamp, .. }
            | LogEntry::PiMetadata { timestamp, .. } => timestamp.as_deref(),
            _ => None,
        }
    }
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct UserMessage {
    #[allow(dead_code)]
    pub role: String,
    pub content: UserContent,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(untagged)]
pub enum UserContent {
    String(String),
    Blocks(Vec<ContentBlock>),
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct AssistantMessage {
    #[allow(dead_code)]
    pub role: String,
    pub content: Vec<ContentBlock>,
    pub model: Option<String>,
    pub usage: Option<TokenUsage>,
    /// Unique message ID to deduplicate streaming entries
    pub id: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, Clone, Default)]
pub struct TokenUsage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub cache_creation_input_tokens: u64,
    #[serde(default)]
    pub cache_read_input_tokens: u64,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Tool {
    Shell,
    /// A shell command the user ran themselves through the agent, which the
    /// model did not call. Its header reads `ran {command}`, with no tool name
    /// to print.
    UserShell,
    Read,
    Edit,
    Write,
    Grep,
    Glob,
    Agent,
    AgentMessage,
    Wait,
    TaskList,
    WebFetch,
    WebSearch,
    /// A result the agent received without calling anything, which a client
    /// handed it under a tool's authority. It classifies a `ToolResult`, not a
    /// call, so it reaches no header.
    ToolResultReceipt,
    #[default]
    Other,
}

/// The words a run's collapsed row uses for one kind of tool activity.
pub struct ToolSummaryPhrase {
    /// Render order, lowest first. Steps of ten leave room for a new kind
    /// between two others.
    pub order: u32,
    pub verb: &'static str,
    pub noun: &'static str,
}

impl Tool {
    /// Whether the canonical input names the file the tool works on as
    /// `file_path`. `Grep` and `Glob` take `path` instead, the directory they
    /// search, so a provider's own path key is renamed only for these.
    pub fn takes_file_path(self) -> bool {
        matches!(self, Tool::Read | Tool::Edit | Tool::Write)
    }

    /// The kind this tool counts as in a run's row. A command the user ran
    /// counts as a shell command: the row names what happened, not who asked
    /// for it.
    pub fn summary_kind(self) -> Self {
        match self {
            Tool::UserShell => Tool::Shell,
            named => named,
        }
    }

    /// The verb and noun a run's row uses for this tool, once folded into its
    /// kind.
    pub fn summary_phrase(self) -> ToolSummaryPhrase {
        let (order, verb, noun) = match self.summary_kind() {
            Tool::Grep => (10, "Searched for", "pattern"),
            Tool::Glob => (20, "Searched for", "file pattern"),
            Tool::Read => (30, "read", "file"),
            Tool::Shell | Tool::UserShell => (40, "ran", "shell command"),
            Tool::Edit => (50, "edited", "file"),
            Tool::Write => (60, "wrote", "file"),
            Tool::Agent => (70, "started", "agent"),
            Tool::AgentMessage => (80, "messaged", "agent"),
            Tool::Wait => (90, "waited", "time"),
            Tool::TaskList => (100, "updated the task list", "time"),
            Tool::WebFetch => (110, "fetched", "URL"),
            Tool::WebSearch => (120, "searched", "web"),
            Tool::ToolResultReceipt => (130, "received", "tool result"),
            Tool::Other => (140, "called", "tool"),
        };
        ToolSummaryPhrase { order, verb, noun }
    }
}

/// The replaced lines then the replacement, each signed as git prints a hunk:
/// the diff body of an `Edit`, whether the renderer builds it from
/// `old_string` and `new_string` or a provider from its own replacement pairs.
pub fn replacement_diff(old: &str, new: &str) -> String {
    old.lines()
        .map(|line| format!("-{line}"))
        .chain(new.lines().map(|line| format!("+{line}")))
        .collect::<Vec<_>>()
        .join("\n")
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(tag = "type")]
#[serde(rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    ToolUse {
        #[allow(dead_code)]
        id: String,
        /// The provider's own name for the tool; what headers print.
        name: String,
        /// Claude's transcripts carry no `tool` field, so a block read from one
        /// is `Other` until the Claude provider assigns it.
        #[serde(default)]
        tool: Tool,
        /// In the canonical shape for `tool` (`command` for `Shell`, `file_path`
        /// for `Read`, …); the provider reshapes its own keys when it maps.
        input: serde_json::Value,
    },
    ToolResult {
        #[allow(dead_code)]
        tool_use_id: String,
        #[serde(default)]
        content: Option<serde_json::Value>, // Optional in some user tool result entries
        /// The tool a standalone result names for itself. A client can hand the
        /// agent an output under a tool's authority without the agent having
        /// called anything; such a result answers no call and names its own
        /// tool. `None` for a result that answers a call, which the call above
        /// it names.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        standalone_tool_name: Option<String>,
    },
    Thinking {
        thinking: String,
        #[allow(dead_code)]
        signature: String,
    },
    #[allow(dead_code)]
    Image {
        source: serde_json::Value,
    },
    /// Unknown content block type.
    #[serde(other)]
    Other,
}

/// Extract only Text blocks (for previews and user-facing display)
pub fn extract_text_from_blocks(blocks: &[ContentBlock]) -> String {
    blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Extract Text blocks plus ToolResult content (for search indexing)
pub fn extract_search_text_from_blocks(blocks: &[ContentBlock]) -> String {
    let mut parts = Vec::new();

    for block in blocks {
        match block {
            ContentBlock::Text { text } => {
                parts.push(text.clone());
            }
            ContentBlock::ToolResult {
                content: Some(content),
                ..
            } => {
                if let Some(text) = bounded_tool_result_text(content) {
                    parts.push(text);
                }
            }
            _ => {}
        }
    }

    parts.join(" ")
}

pub fn extract_text_from_user(message: &UserMessage) -> String {
    match &message.content {
        UserContent::String(text) => text.clone(),
        UserContent::Blocks(blocks) => extract_text_from_blocks(blocks),
    }
}

pub fn extract_search_text_from_user(message: &UserMessage) -> String {
    match &message.content {
        UserContent::String(text) => text.clone(),
        UserContent::Blocks(blocks) => extract_search_text_from_blocks(blocks),
    }
}

pub fn extract_text_from_assistant(message: &AssistantMessage) -> String {
    extract_text_from_blocks(&message.content)
}

pub fn extract_search_text_from_assistant(message: &AssistantMessage) -> String {
    extract_search_text_from_blocks(&message.content)
}

/// Agent progress data from subagent conversations
#[derive(Debug, Deserialize, Serialize)]
pub struct AgentProgressData {
    #[allow(dead_code)]
    #[serde(rename = "type")]
    pub progress_type: String,
    #[serde(rename = "agentId")]
    pub agent_id: String,
    pub message: AgentMessage,
    #[allow(dead_code)]
    pub prompt: Option<String>,
}

/// Individual message within an agent conversation
#[derive(Debug, Deserialize, Serialize)]
pub struct AgentMessage {
    #[serde(rename = "type")]
    pub message_type: String, // "user" or "assistant"
    pub message: AgentMessageContent,
}

/// Content of an agent message (mirrors UserMessage/AssistantMessage structure)
#[derive(Debug, Deserialize, Serialize)]
pub struct AgentMessageContent {
    #[allow(dead_code)]
    pub role: String,
    pub content: AgentContent,
}

/// Agent message content is always an array of content blocks
#[derive(Debug, Deserialize, Serialize)]
#[serde(untagged)]
pub enum AgentContent {
    Blocks(Vec<ContentBlock>),
}

/// Format a parent_tool_use_id into a short display ID.
/// Strips the "toolu_" prefix and takes the first 7 characters.
pub fn short_parent_id(parent_tool_use_id: &str) -> String {
    let stripped = parent_tool_use_id
        .strip_prefix("toolu_")
        .unwrap_or(parent_tool_use_id);
    stripped[..stripped.len().min(7)].to_string()
}

/// Attempt to parse agent progress data from a Progress entry
pub fn parse_agent_progress(data: &serde_json::Value) -> Option<AgentProgressData> {
    // Check if this is an agent_progress type
    if data.get("type").and_then(|t| t.as_str()) != Some("agent_progress") {
        return None;
    }
    serde_json::from_value(data.clone()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn extract_text_from_blocks_only_text() {
        let blocks = vec![
            ContentBlock::Text {
                text: "hello".into(),
            },
            ContentBlock::ToolResult {
                tool_use_id: "id".into(),
                standalone_tool_name: None,
                content: Some(json!("tool output")),
            },
        ];
        assert_eq!(extract_text_from_blocks(&blocks), "hello");
    }

    #[test]
    fn extract_search_text_includes_tool_result_string() {
        let blocks = vec![
            ContentBlock::Text {
                text: "hello".into(),
            },
            ContentBlock::ToolResult {
                tool_use_id: "id".into(),
                standalone_tool_name: None,
                content: Some(json!("tool output here")),
            },
        ];
        let result = extract_search_text_from_blocks(&blocks);
        assert!(result.contains("hello"));
        assert!(result.contains("tool output here"));
    }

    #[test]
    fn extract_search_text_includes_tool_result_array() {
        let blocks = vec![ContentBlock::ToolResult {
            tool_use_id: "id".into(),
            standalone_tool_name: None,
            content: Some(json!([
                {"type": "text", "text": "line one"},
                {"type": "text", "text": "line two"}
            ])),
        }];
        let result = extract_search_text_from_blocks(&blocks);
        assert!(result.contains("line one"));
        assert!(result.contains("line two"));
    }

    #[test]
    fn extract_search_text_ignores_non_text_blocks_in_array() {
        let blocks = vec![ContentBlock::ToolResult {
            tool_use_id: "id".into(),
            standalone_tool_name: None,
            content: Some(json!([
                {"type": "text", "text": "visible"},
                {"type": "image", "source": {"data": "base64..."}}
            ])),
        }];
        let result = extract_search_text_from_blocks(&blocks);
        assert!(result.contains("visible"));
        assert!(!result.contains("base64"));
    }

    #[test]
    fn extract_search_text_handles_none_content() {
        let blocks = vec![ContentBlock::ToolResult {
            tool_use_id: "id".into(),
            standalone_tool_name: None,
            content: None,
        }];
        assert_eq!(extract_search_text_from_blocks(&blocks), "");
    }

    #[test]
    fn extract_search_text_handles_empty_string_content() {
        let blocks = vec![ContentBlock::ToolResult {
            tool_use_id: "id".into(),
            standalone_tool_name: None,
            content: Some(json!("")),
        }];
        assert_eq!(extract_search_text_from_blocks(&blocks), "");
    }

    #[test]
    fn bounded_tool_result_text_array_with_plain_strings() {
        let content = json!(["line one", "line two"]);
        let result = bounded_tool_result_text(&content);
        assert_eq!(result, Some("line one\nline two".into()));
    }

    #[test]
    fn bounded_tool_result_text_object_without_type() {
        let content = json!([{"text": "no type field"}]);
        let result = bounded_tool_result_text(&content);
        assert_eq!(result, Some("no type field".into()));
    }

    #[test]
    fn deserializes_user_message_with_document_block() {
        // Unknown block types parse as `Other` without hiding text blocks.
        let message: UserMessage = serde_json::from_value(json!({
            "role": "user",
            "content": [
                {
                    "type": "document",
                    "source": {
                        "type": "base64",
                        "media_type": "application/pdf",
                        "data": "JVBERi0xLjcK..."
                    }
                },
                { "type": "text", "text": "summarize this" }
            ]
        }))
        .expect("document block should deserialize as Other");

        match &message.content {
            UserContent::Blocks(blocks) => {
                assert_eq!(blocks.len(), 2);
                assert!(matches!(blocks[0], ContentBlock::Other));
            }
            other => panic!("expected blocks, got {other:?}"),
        }
        assert_eq!(extract_text_from_user(&message), "summarize this");
    }

    #[test]
    fn tool_use_block_without_tool_field_deserializes_as_other() {
        let block: ContentBlock = serde_json::from_value(json!({
            "type": "tool_use",
            "id": "toolu_1",
            "name": "Bash",
            "input": {"command": "pwd"}
        }))
        .unwrap();

        assert!(matches!(
            block,
            ContentBlock::ToolUse {
                tool: Tool::Other,
                ..
            }
        ));
    }

    #[test]
    fn tool_field_round_trips_through_json() {
        let block = ContentBlock::ToolUse {
            id: "toolu_1".into(),
            name: "shell_command".into(),
            tool: Tool::Shell,
            input: json!({"command": "pwd"}),
        };

        let json = serde_json::to_value(&block).unwrap();
        assert_eq!(json["tool"], json!("shell"));
        let restored: ContentBlock = serde_json::from_value(json).unwrap();
        assert!(matches!(
            restored,
            ContentBlock::ToolUse {
                tool: Tool::Shell,
                ..
            }
        ));
    }
}
