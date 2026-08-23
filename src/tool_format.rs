//! Tool-specific formatting for nicer display of tool calls.
//!
//! Instead of showing raw JSON, this module formats each tool's input
//! in a human-readable way that highlights the most relevant information.
//! The layout is chosen by the call's canonical [`Tool`]; the header always
//! opens with the provider's own tool name.

use crate::log_entry::{Tool, replacement_diff};
use serde_json::Value;

/// Formatted tool call representation
pub struct FormattedToolCall {
    /// The header line (e.g., "Task (Explore): description" or "Bash: command")
    pub header: String,
    /// Optional continuation lines (e.g., prompt text, diff lines)
    pub body: Option<ToolBody>,
}

/// The continuation lines of a formatted tool call.
pub struct ToolBody {
    pub text: String,
    pub kind: ToolBodyKind,
}

impl ToolBody {
    fn plain(text: String) -> Self {
        Self {
            text,
            kind: ToolBodyKind::Plain,
        }
    }

    fn diff(text: String) -> Self {
        Self {
            text,
            kind: ToolBodyKind::Diff,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolBodyKind {
    Plain,
    /// Lines carry git's bare `+` / `-` signs. Only these bodies are coloured,
    /// so a markdown bullet inside a plain body never reads as a removal.
    Diff,
}

/// The side of a diff a body line belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiffSide {
    Added,
    Removed,
}

impl ToolBodyKind {
    /// `None` for context lines and for every line of a plain body.
    pub fn diff_side(self, line: &str) -> Option<DiffSide> {
        match self {
            Self::Plain => None,
            Self::Diff if line.starts_with('+') => Some(DiffSide::Added),
            Self::Diff if line.starts_with('-') => Some(DiffSide::Removed),
            Self::Diff => None,
        }
    }
}

/// Format a tool call for display
///
/// The `max_width` parameter controls line wrapping for tools with long content (e.g., shell commands).
pub fn format_tool_call(
    name: &str,
    tool: Tool,
    input: &Value,
    max_width: usize,
) -> FormattedToolCall {
    match tool {
        Tool::Shell => format_shell(name, input, max_width),
        Tool::Read => format_read(name, input),
        Tool::Edit => format_edit(name, input),
        Tool::Write => format_write(name, input),
        Tool::Grep => format_grep(name, input),
        Tool::Glob => format_glob(name, input),
        Tool::Agent => format_agent(name, input),
        Tool::AgentMessage => format_agent_message(name, input),
        Tool::WebFetch => format_web_fetch(name, input),
        Tool::WebSearch => format_web_search(name, input),
        Tool::Wait | Tool::TaskList | Tool::Other => format_fallback(name, input),
    }
}

fn string_field<'a>(input: &'a Value, key: &str) -> Option<&'a str> {
    input.get(key).and_then(Value::as_str)
}

fn format_agent(name: &str, input: &Value) -> FormattedToolCall {
    let description = string_field(input, "description").unwrap_or("");
    let header = match string_field(input, "subagent_type") {
        Some(subagent_type) => format!("{name} ({subagent_type}): {description}"),
        None => format!("{name}: {description}"),
    };

    FormattedToolCall {
        header,
        body: string_field(input, "prompt").map(|prompt| ToolBody::plain(prompt.to_owned())),
    }
}

fn format_agent_message(name: &str, input: &Value) -> FormattedToolCall {
    let recipient = string_field(input, "recipient").unwrap_or("");

    FormattedToolCall {
        header: format!("{name}: {recipient}"),
        body: string_field(input, "message").map(|message| ToolBody::plain(message.to_owned())),
    }
}

fn format_shell(name: &str, input: &Value, max_width: usize) -> FormattedToolCall {
    let command = string_field(input, "command").unwrap_or("");
    let prefix = format!("{name}: ");

    // Available width for command text (accounting for prefix on first line)
    let available_width = max_width.saturating_sub(prefix.chars().count());

    let wrapped: Vec<_> = if command.contains('\n') {
        command
            .lines()
            .flat_map(|line| {
                if available_width == 0 || line.chars().count() <= available_width {
                    vec![line.to_string()]
                } else {
                    textwrap::wrap(line, available_width)
                        .into_iter()
                        .map(|cow| cow.into_owned())
                        .collect()
                }
            })
            .collect()
    } else if available_width == 0 || command.chars().count() <= available_width {
        return FormattedToolCall {
            header: format!("{prefix}{command}"),
            body: None,
        };
    } else {
        textwrap::wrap(command, available_width)
            .into_iter()
            .map(|cow| cow.into_owned())
            .collect()
    };

    if wrapped.len() <= 1 {
        return FormattedToolCall {
            header: format!("{prefix}{command}"),
            body: None,
        };
    }

    // First line goes in header, rest in body
    FormattedToolCall {
        header: format!("{prefix}{}", wrapped[0]),
        body: Some(ToolBody::plain(wrapped[1..].join("\n"))),
    }
}

fn format_read(name: &str, input: &Value) -> FormattedToolCall {
    let file_path = string_field(input, "file_path").unwrap_or("");
    let offset = input.get("offset").and_then(Value::as_u64);
    let limit = input.get("limit").and_then(Value::as_u64);

    let header = match (offset, limit) {
        (Some(offset), Some(limit)) => {
            format!("{name}: {file_path}:{offset}-{}", offset + limit)
        }
        (Some(offset), None) => format!("{name}: {file_path}:{offset}"),
        _ => format!("{name}: {file_path}"),
    };

    FormattedToolCall { header, body: None }
}

fn format_grep(name: &str, input: &Value) -> FormattedToolCall {
    let pattern = string_field(input, "pattern").unwrap_or("");
    let path = string_field(input, "path");
    let glob = string_field(input, "glob");

    let location = match (path, glob) {
        (Some(path), Some(glob)) => format!("{path}/{glob}"),
        (Some(path), None) => path.to_string(),
        (None, Some(glob)) => glob.to_string(),
        (None, None) => ".".to_string(),
    };

    FormattedToolCall {
        header: format!("{name}: \"{pattern}\" in {location}"),
        body: None,
    }
}

fn format_glob(name: &str, input: &Value) -> FormattedToolCall {
    let pattern = string_field(input, "pattern").unwrap_or("");

    let header = match string_field(input, "path") {
        Some(path) => format!("{name}: {pattern} in {path}"),
        None => format!("{name}: {pattern}"),
    };

    FormattedToolCall { header, body: None }
}

fn format_edit(name: &str, input: &Value) -> FormattedToolCall {
    let file_path = string_field(input, "file_path").unwrap_or("");

    let body = match (
        string_field(input, "old_string"),
        string_field(input, "new_string"),
    ) {
        (Some(old), Some(new)) => Some(ToolBody::diff(replacement_diff(old, new))),
        _ => string_field(input, "patch").map(|patch| ToolBody::diff(patch.to_owned())),
    };

    FormattedToolCall {
        header: format!("{name}: {file_path}"),
        body,
    }
}

fn format_write(name: &str, input: &Value) -> FormattedToolCall {
    let file_path = string_field(input, "file_path").unwrap_or("");

    FormattedToolCall {
        header: format!("{name}: {file_path}"),
        body: None,
    }
}

fn format_web_fetch(name: &str, input: &Value) -> FormattedToolCall {
    let url = string_field(input, "url").unwrap_or("");

    FormattedToolCall {
        header: format!("{name}: {url}"),
        body: string_field(input, "prompt").map(|prompt| ToolBody::plain(prompt.to_owned())),
    }
}

fn format_web_search(name: &str, input: &Value) -> FormattedToolCall {
    let query = string_field(input, "query").unwrap_or("");

    FormattedToolCall {
        header: format!("{name}: \"{query}\""),
        body: None,
    }
}

fn format_fallback(name: &str, input: &Value) -> FormattedToolCall {
    FormattedToolCall {
        header: format!("{name}:"),
        body: serde_json::to_string_pretty(input)
            .ok()
            .map(ToolBody::plain),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn body_text(call: &FormattedToolCall) -> Option<&str> {
        call.body.as_ref().map(|body| body.text.as_str())
    }

    fn body_kind(call: &FormattedToolCall) -> Option<ToolBodyKind> {
        call.body.as_ref().map(|body| body.kind)
    }

    #[test]
    fn test_format_task() {
        let input = json!({
            "subagent_type": "Explore",
            "description": "Find the bug",
            "prompt": "Look for issues in the code"
        });
        let result = format_tool_call("Task", Tool::Agent, &input, 80);
        assert_eq!(result.header, "Task (Explore): Find the bug");
        assert_eq!(body_text(&result), Some("Look for issues in the code"));
        assert_eq!(body_kind(&result), Some(ToolBodyKind::Plain));
    }

    #[test]
    fn agent_without_subagent_type_omits_the_parentheses() {
        let input = json!({
            "description": "classifier_state_machine",
            "prompt": "Classify every state."
        });
        let result = format_tool_call("spawn_agent", Tool::Agent, &input, 80);
        assert_eq!(result.header, "spawn_agent: classifier_state_machine");
    }

    #[test]
    fn agent_message_names_the_recipient_and_carries_the_message() {
        let input = json!({"recipient": "worker-1", "message": "How far along are you?"});
        let result = format_tool_call("SendMessage", Tool::AgentMessage, &input, 80);
        assert_eq!(result.header, "SendMessage: worker-1");
        assert_eq!(body_text(&result), Some("How far along are you?"));
    }

    #[test]
    fn test_format_bash() {
        let input = json!({
            "command": "git status",
            "description": "Check repo status"
        });
        let result = format_tool_call("Bash", Tool::Shell, &input, 80);
        assert_eq!(result.header, "Bash: git status");
        assert!(result.body.is_none());
    }

    #[test]
    fn header_opens_with_the_provider_name() {
        let input = json!({"command": "git status"});
        let result = format_tool_call("PowerShell", Tool::Shell, &input, 80);
        assert_eq!(result.header, "PowerShell: git status");
        let result = format_tool_call("exec_command", Tool::Shell, &input, 80);
        assert_eq!(result.header, "exec_command: git status");
    }

    #[test]
    fn test_format_bash_wrapping() {
        let long_command = "cargo build --release --features 'feature1 feature2 feature3' --target x86_64-unknown-linux-gnu";
        let input = json!({
            "command": long_command
        });
        // With width 40, command should wrap (available width is 40 - 6 = 34 for command text)
        let result = format_tool_call("Bash", Tool::Shell, &input, 40);
        assert!(result.header.starts_with("Bash: cargo"));
        assert!(
            result.body.is_some(),
            "Long command should have body for continuation"
        );
    }

    #[test]
    fn test_format_bash_no_wrap_when_fits() {
        let input = json!({
            "command": "ls -la"
        });
        let result = format_tool_call("Bash", Tool::Shell, &input, 80);
        assert_eq!(result.header, "Bash: ls -la");
        assert!(result.body.is_none());
    }

    #[test]
    fn test_format_bash_multiline_command_has_body() {
        let input = json!({
            "command": "one\ntwo"
        });
        let result = format_tool_call("Bash", Tool::Shell, &input, 80);
        assert_eq!(result.header, "Bash: one");
        assert_eq!(body_text(&result), Some("two"));
    }

    #[test]
    fn test_format_bash_wraps_long_lines_in_multiline_command() {
        let input = json!({
            "command": "alpha beta gamma\nnext"
        });
        let result = format_tool_call("Bash", Tool::Shell, &input, 12);
        assert_eq!(result.header, "Bash: alpha");
        assert_eq!(body_text(&result), Some("beta\ngamma\nnext"));
    }

    #[test]
    fn test_format_read_with_range() {
        let input = json!({
            "file_path": "/src/main.rs",
            "offset": 100,
            "limit": 50
        });
        let result = format_tool_call("Read", Tool::Read, &input, 80);
        assert_eq!(result.header, "Read: /src/main.rs:100-150");
    }

    #[test]
    fn test_format_grep() {
        let input = json!({
            "pattern": "fn main",
            "path": "src",
            "glob": "*.rs"
        });
        let result = format_tool_call("Grep", Tool::Grep, &input, 80);
        assert_eq!(result.header, "Grep: \"fn main\" in src/*.rs");
    }

    #[test]
    fn test_format_edit() {
        let input = json!({
            "file_path": "/src/lib.rs",
            "old_string": "old code",
            "new_string": "new code"
        });
        let result = format_tool_call("Edit", Tool::Edit, &input, 80);
        assert_eq!(result.header, "Edit: /src/lib.rs");
        assert_eq!(body_text(&result), Some("-old code\n+new code"));
        assert_eq!(body_kind(&result), Some(ToolBodyKind::Diff));
    }

    #[test]
    fn edit_with_a_patch_keeps_it_as_the_diff_body() {
        let input = json!({
            "file_path": "src/lib.rs",
            "patch": "@@ -1,2 +1,2 @@\n-old\n+new"
        });
        let result = format_tool_call("apply_patch", Tool::Edit, &input, 80);
        assert_eq!(result.header, "apply_patch: src/lib.rs");
        assert_eq!(body_text(&result), Some("@@ -1,2 +1,2 @@\n-old\n+new"));
        assert_eq!(body_kind(&result), Some(ToolBodyKind::Diff));
    }

    #[test]
    fn unmapped_tool_falls_back_to_its_name_and_input() {
        let input = json!({"plan": "- step one"});
        let result = format_tool_call("ExitPlanMode", Tool::Other, &input, 80);
        assert_eq!(result.header, "ExitPlanMode:");
        assert_eq!(body_kind(&result), Some(ToolBodyKind::Plain));
        assert!(
            body_text(&result)
                .unwrap()
                .contains("\"plan\": \"- step one\"")
        );
    }

    #[test]
    fn only_diff_bodies_have_signed_lines() {
        assert_eq!(ToolBodyKind::Plain.diff_side("- a markdown bullet"), None);
        assert_eq!(ToolBodyKind::Plain.diff_side("+1 for that"), None);
        assert_eq!(
            ToolBodyKind::Diff.diff_side("+added"),
            Some(DiffSide::Added)
        );
        assert_eq!(
            ToolBodyKind::Diff.diff_side("-removed"),
            Some(DiffSide::Removed)
        );
        assert_eq!(ToolBodyKind::Diff.diff_side(" context"), None);
        assert_eq!(ToolBodyKind::Diff.diff_side("@@ -1 +1 @@"), None);
    }
}
