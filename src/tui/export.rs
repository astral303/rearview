//! Conversation export functionality.
//!
//! This module provides functions to export conversations in different formats:
//! - Ledger format (formatted text with speaker names)
//! - Plain text (simple speaker: message format)
//! - Markdown (with headers for speakers)
//! - JSONL (raw format)
//!
//! Conversations can be exported to files or copied to the clipboard.
//! Export respects the current display settings for thinking blocks and tool calls.

use crate::claude::{self, AgentContent, ContentBlock, LogEntry, UserContent, UserMessage};
use crate::tool_format;
use crate::tui::parse_command_name_and_args;
use chrono::Local;
use crossterm::clipboard::CopyToClipboard;
use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::path::Path;
#[cfg(target_os = "linux")]
use std::process::{Command, Stdio};

/// Export format options
#[derive(Clone, Copy, Debug)]
pub enum ExportFormat {
    Ledger,
    Plain,
    Markdown,
    Jsonl,
}

impl ExportFormat {
    /// Get format from menu option index (0-3)
    pub fn from_index(index: usize) -> Option<Self> {
        match index {
            0 => Some(ExportFormat::Ledger),
            1 => Some(ExportFormat::Plain),
            2 => Some(ExportFormat::Markdown),
            3 => Some(ExportFormat::Jsonl),
            _ => None,
        }
    }

    /// Get file extension for this format
    fn extension(&self) -> &'static str {
        match self {
            ExportFormat::Ledger | ExportFormat::Plain => "txt",
            ExportFormat::Markdown => "md",
            ExportFormat::Jsonl => "jsonl",
        }
    }
}

/// Result of an export operation
pub struct ExportResult {
    pub message: String,
}

/// Options for export content generation
#[derive(Clone, Copy, Debug, Default)]
pub struct ExportOptions {
    pub show_tools: bool,
    pub show_thinking: bool,
}

/// Export conversation to file
pub fn export_to_file(
    source: crate::history::Source,
    source_path: &Path,
    format: ExportFormat,
    options: ExportOptions,
) -> ExportResult {
    let timestamp = Local::now().format("%Y-%m-%d-%H%M%S");
    let ext = format.extension();
    let filename = format!("conversation-{}.{}", timestamp, ext);

    let content = match generate_content(source, source_path, format, options) {
        Ok(c) => c,
        Err(e) => {
            return ExportResult {
                message: format!("Failed to read: {}", e),
            };
        }
    };

    match fs::write(&filename, &content) {
        Ok(_) => ExportResult {
            message: format!("Exported to {}", filename),
        },
        Err(e) => ExportResult {
            message: format!("Failed to write: {}", e),
        },
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClipboardDestination {
    System,
    Terminal,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ClipboardTransport {
    System,
    Osc52,
}

const CLIPBOARD_TRANSPORT_ENV: &str = "REARVIEW_CLIPBOARD";
const REMOTE_SESSION_ENV_VARS: [&str; 4] =
    ["SSH_CONNECTION", "SSH_CLIENT", "SSH_TTY", "MOSH_CONNECTION"];

fn clipboard_transport() -> Result<ClipboardTransport, String> {
    clipboard_transport_from_env(|name| std::env::var_os(name))
}

fn clipboard_transport_from_env(
    mut var: impl FnMut(&str) -> Option<OsString>,
) -> Result<ClipboardTransport, String> {
    let override_value = var(CLIPBOARD_TRANSPORT_ENV);
    let mut remote_transport = || {
        if REMOTE_SESSION_ENV_VARS
            .iter()
            .any(|name| var(name).is_some())
        {
            ClipboardTransport::Osc52
        } else {
            ClipboardTransport::System
        }
    };

    match override_value {
        None => Ok(remote_transport()),
        Some(value) => match value.to_str() {
            Some("auto") => Ok(remote_transport()),
            Some("system") => Ok(ClipboardTransport::System),
            Some("osc52") => Ok(ClipboardTransport::Osc52),
            _ => Err(format!(
                "Invalid {CLIPBOARD_TRANSPORT_ENV}: expected auto, system, or osc52"
            )),
        },
    }
}

fn copy_via_terminal(mut writer: impl Write, text: &str) -> Result<(), String> {
    crossterm::execute!(writer, CopyToClipboard::to_clipboard_from(text))
        .map_err(|e| format!("Terminal clipboard error: {e}"))
}

/// Copy text to the clipboard appropriate for this terminal session.
///
/// Remote sessions use OSC 52 so the terminal host receives the text. Local
/// sessions use the operating system clipboard. `REARVIEW_CLIPBOARD`
/// overrides selection with `auto`, `system`, or `osc52`.
pub fn copy_to_system_clipboard(text: &str) -> Result<ClipboardDestination, String> {
    if clipboard_transport()? == ClipboardTransport::Osc52 {
        copy_via_terminal(std::io::stderr(), text)?;
        return Ok(ClipboardDestination::Terminal);
    }

    #[cfg(target_os = "linux")]
    {
        let candidates = linux_clipboard_candidates();
        for (cmd, args) in &candidates {
            match copy_via_command(cmd, args, text) {
                Ok(Ok(())) => return Ok(ClipboardDestination::System),
                Ok(Err(_)) => continue,
                Err(()) => continue,
            }
        }
    }

    match arboard::Clipboard::new() {
        Ok(mut clipboard) => clipboard
            .set_text(text)
            .map(|()| ClipboardDestination::System)
            .map_err(|e| format!("Clipboard error: {e}")),
        Err(e) => Err(format!("Clipboard unavailable: {e}")),
    }
}

/// Return clipboard tool candidates based on the active display server.
#[cfg(target_os = "linux")]
fn linux_clipboard_candidates() -> Vec<(&'static str, &'static [&'static str])> {
    let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();
    let x11 = std::env::var_os("DISPLAY").is_some();

    let mut candidates = Vec::new();
    if wayland {
        candidates.push(("wl-copy", ["--type", "text/plain;charset=utf-8"].as_slice()));
    }
    if x11 {
        candidates.push(("xclip", ["-selection", "clipboard"].as_slice()));
        candidates.push(("xsel", ["--clipboard", "--input"].as_slice()));
    }
    candidates
}

/// Try to copy text via an external command (e.g. wl-copy, xclip, xsel).
/// Returns `Ok(Ok(()))` on success, `Ok(Err(msg))` if the command ran but failed,
/// or `Err(())` if the command was not found (caller should try next option).
#[cfg(target_os = "linux")]
fn copy_via_command(cmd: &str, args: &[&str], text: &str) -> Result<Result<(), String>, ()> {
    let mut child = Command::new(cmd)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| ())?; // command not available → try next

    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(text.as_bytes());
    }

    match child.wait() {
        Ok(status) if status.success() => Ok(Ok(())),
        Ok(status) => Ok(Err(format!("{} exited with {}", cmd, status))),
        Err(e) => Ok(Err(format!("{} error: {}", cmd, e))),
    }
}

/// Copy conversation to clipboard
pub fn export_to_clipboard(
    source: crate::history::Source,
    source_path: &Path,
    format: ExportFormat,
    options: ExportOptions,
) -> ExportResult {
    let content = match generate_content(source, source_path, format, options) {
        Ok(c) => c,
        Err(e) => {
            return ExportResult {
                message: format!("Failed to read: {}", e),
            };
        }
    };

    match copy_to_system_clipboard(&content) {
        Ok(ClipboardDestination::System) => ExportResult {
            message: "Copied to clipboard".to_string(),
        },
        Ok(ClipboardDestination::Terminal) => ExportResult {
            message: "Sent to terminal clipboard".to_string(),
        },
        Err(e) => ExportResult { message: e },
    }
}

/// Extract the text content of a single message by its entry index in the JSONL file.
/// Returns the message text suitable for clipboard copying.
pub fn extract_message_text(
    source: crate::history::Source,
    source_path: &Path,
    entry_index: usize,
    options: ExportOptions,
) -> Result<String, String> {
    let entries = crate::history::normalized_log_entries(source, source_path)
        .map_err(|e| format!("Failed to read: {e}"))?;
    entries
        .into_iter()
        .map(|(_, entry)| entry)
        .nth(entry_index)
        .map(|entry| format_entry_for_clipboard(&entry, options))
        .ok_or_else(|| "Message not found".to_string())
}

/// Format a single log entry as text for clipboard
/// Append text with blank-line separation if output is non-empty.
fn append_separated(output: &mut String, text: &str) {
    if !output.is_empty() {
        output.push_str("\n\n");
    }
    output.push_str(text);
}

/// Iterate content blocks and append formatted output for clipboard-style export.
/// Handles Text, ToolUse, ToolResult, and Thinking blocks guarded by options.
fn append_clipboard_blocks(output: &mut String, blocks: &[ContentBlock], options: &ExportOptions) {
    for block in blocks {
        match block {
            ContentBlock::Text { text } => {
                append_separated(output, text);
            }
            ContentBlock::ToolUse { name, input, .. } if options.show_tools => {
                append_separated(output, &format_tool_call_for_export(name, input));
            }
            ContentBlock::ToolResult { content, .. } if options.show_tools => {
                append_separated(output, &format_tool_result_for_export(content.as_ref()));
            }
            ContentBlock::Thinking { thinking, .. } if options.show_thinking => {
                append_separated(output, thinking);
            }
            _ => {}
        }
    }
}

/// Invoke `f` with the formatted content string for each ToolResult block
/// in a user message, when show_tools is enabled.
fn for_user_tool_results(message: &UserMessage, options: &ExportOptions, mut f: impl FnMut(&str)) {
    if options.show_tools
        && let UserContent::Blocks(blocks) = &message.content
    {
        for block in blocks {
            if let ContentBlock::ToolResult { content, .. } = block {
                let content_str = format_tool_result_for_export(content.as_ref());
                f(&content_str);
            }
        }
    }
}

fn format_entry_for_clipboard(entry: &LogEntry, options: ExportOptions) -> String {
    let mut output = String::new();
    match entry {
        LogEntry::User {
            message,
            parent_tool_use_id,
            ..
        } => {
            if let Some(text) = extract_user_text(message) {
                output.push_str(&text);
            }
            for_user_tool_results(message, &options, |content| {
                append_separated(&mut output, content);
            });
            let _ = parent_tool_use_id;
        }
        LogEntry::Assistant {
            message,
            parent_tool_use_id,
            ..
        } => {
            append_clipboard_blocks(&mut output, &message.content, &options);
            let _ = parent_tool_use_id;
        }
        LogEntry::PiMetadata {
            label,
            text,
            searchable: true,
            ..
        } => {
            output.push_str(&format!("[{label}] {text}"));
        }
        LogEntry::Progress { data, .. } => {
            if let Some(agent_progress) = claude::parse_agent_progress(data) {
                let AgentContent::Blocks(blocks) = &agent_progress.message.message.content;
                append_clipboard_blocks(&mut output, blocks, &options);
            }
        }
        _ => {}
    }
    output
}

/// Generate content in the specified format
fn generate_content(
    source: crate::history::Source,
    source_path: &Path,
    format: ExportFormat,
    options: ExportOptions,
) -> std::io::Result<String> {
    match format {
        ExportFormat::Jsonl => fs::read_to_string(source_path),
        ExportFormat::Plain => generate_plain(source, source_path, options),
        ExportFormat::Markdown => generate_markdown(source, source_path, options),
        ExportFormat::Ledger => generate_ledger(source, source_path, options),
    }
}

/// The entries of the conversation at `path`, read through `source`'s format.
fn export_entries(
    source: crate::history::Source,
    path: &Path,
) -> std::io::Result<Vec<(usize, LogEntry)>> {
    crate::history::normalized_log_entries(source, path)
        .map_err(|error| std::io::Error::other(error.to_string()))
}

fn generate_plain_or_markdown_content(
    entries: Vec<(usize, LogEntry)>,
    options: ExportOptions,
    mut handle_user_text: impl FnMut(&mut String, &str, &str),
    mut handle_user_tool_result: impl FnMut(&mut String, &str, &str),
    mut handle_assistant_text: impl FnMut(&mut String, &str, &str, &str),
    mut handle_assistant_tool_use: impl FnMut(&mut String, &str, &str, &serde_json::Value),
    mut handle_assistant_thinking: impl FnMut(&mut String, &str, &str),
) -> std::io::Result<String> {
    let mut output = String::new();

    for (_, entry) in entries {
        match entry {
            LogEntry::User {
                message,
                parent_tool_use_id,
                ..
            } => {
                if parent_tool_use_id.is_some() && !options.show_thinking {
                    continue;
                }
                let prefix = subagent_prefix(&parent_tool_use_id);
                if let Some(text) = extract_user_text(&message) {
                    handle_user_text(&mut output, &prefix, &text);
                }
                // Tool results
                for_user_tool_results(&message, &options, |content| {
                    handle_user_tool_result(&mut output, &prefix, content);
                });
            }
            LogEntry::Assistant {
                message,
                agent,
                parent_tool_use_id,
                ..
            } => {
                if parent_tool_use_id.is_some() && !options.show_thinking {
                    continue;
                }
                let prefix = subagent_prefix(&parent_tool_use_id);
                let speaker = agent.as_deref().unwrap_or("Claude");
                for block in &message.content {
                    match block {
                        ContentBlock::Text { text } => {
                            handle_assistant_text(&mut output, &prefix, speaker, text);
                        }
                        ContentBlock::ToolUse { name, input, .. } if options.show_tools => {
                            handle_assistant_tool_use(&mut output, &prefix, name, input);
                        }
                        ContentBlock::Thinking { thinking, .. } if options.show_thinking => {
                            handle_assistant_thinking(&mut output, &prefix, thinking);
                        }
                        _ => {}
                    }
                }
            }
            LogEntry::PiMetadata {
                label,
                text,
                searchable: true,
                ..
            } => {
                let rendered = if text.is_empty() {
                    format!("[{label}]")
                } else {
                    format!("[{label}] {text}")
                };
                handle_user_text(&mut output, "", &rendered);
            }
            _ => {}
        }
    }

    Ok(output)
}

/// Generate plain text format (simple "Speaker: message" lines)
fn generate_plain(
    source: crate::history::Source,
    path: &Path,
    options: ExportOptions,
) -> std::io::Result<String> {
    generate_plain_or_markdown_content(
        export_entries(source, path)?,
        options,
        |output, prefix, text| {
            output.push_str(&format!("{}You: {}\n\n", prefix, text));
        },
        |output, prefix, content| {
            output.push_str(&format!("{}Tool Result: {}\n\n", prefix, content));
        },
        |output, prefix, speaker, text| {
            output.push_str(&format!("{}{speaker}: {}\n\n", prefix, text));
        },
        |output, prefix, name, input| {
            let formatted = format_tool_call_for_export(name, input);
            output.push_str(&format!("{}Tool: {}\n\n", prefix, formatted));
        },
        |output, prefix, thinking| {
            output.push_str(&format!("{}Thinking: {}\n\n", prefix, thinking));
        },
    )
}

/// Generate markdown format (with ## headers for speakers)
fn generate_markdown(
    source: crate::history::Source,
    path: &Path,
    options: ExportOptions,
) -> std::io::Result<String> {
    generate_plain_or_markdown_content(
        export_entries(source, path)?,
        options,
        |output, prefix, text| {
            output.push_str(&format!("## {}You\n\n{}\n\n", prefix, text));
        },
        |output, prefix, content| {
            let fenced = markdown_code_fence(content);
            output.push_str(&format!("### {}Tool Result\n\n{}\n\n", prefix, fenced));
        },
        |output, prefix, speaker, text| {
            output.push_str(&format!("## {}{speaker}\n\n{}\n\n", prefix, text));
        },
        |output, prefix, name, input| {
            let formatted = format_tool_call_for_export(name, input);
            let fenced = markdown_code_fence(&formatted);
            output.push_str(&format!("### {}Tool: {}\n\n{}\n\n", prefix, name, fenced));
        },
        |output, prefix, thinking| {
            output.push_str(&format!("### {}Thinking\n\n{}\n\n", prefix, thinking));
        },
    )
}

/// Total line width for ledger export (including name column and separator)
const LEDGER_WIDTH: usize = 90;

/// Generate ledger-style format (formatted like the TUI viewer)
fn generate_ledger(
    source: crate::history::Source,
    path: &Path,
    options: ExportOptions,
) -> std::io::Result<String> {
    let entries = export_entries(source, path)?;
    let mut output = String::new();

    const NAME_WIDTH: usize = 9;
    // 3 for " │ " separator
    let content_width = LEDGER_WIDTH - NAME_WIDTH - 3;

    for (_, entry) in entries {
        match entry {
            LogEntry::User {
                message,
                parent_tool_use_id,
                ..
            } => {
                if parent_tool_use_id.is_some() && !options.show_thinking {
                    continue;
                }
                let speaker = match &parent_tool_use_id {
                    Some(id) => format!("↳{}", claude::short_parent_id(id)),
                    None => "You".to_string(),
                };
                if let Some(text) = extract_user_text(&message) {
                    let wrapped = wrap_plain_text(&text, content_width);
                    append_ledger_block(&mut output, &speaker, &wrapped, NAME_WIDTH);
                    output.push('\n');
                }
                // Tool results
                for_user_tool_results(&message, &options, |content| {
                    if !content.trim().is_empty() {
                        let wrapped = wrap_plain_text(content, content_width);
                        append_ledger_block(&mut output, "↳ Result", &wrapped, NAME_WIDTH);
                        output.push('\n');
                    }
                });
            }
            LogEntry::Assistant {
                message,
                agent,
                parent_tool_use_id,
                ..
            } => {
                if parent_tool_use_id.is_some() && !options.show_thinking {
                    continue;
                }
                let speaker = match &parent_tool_use_id {
                    Some(id) => format!("↳{}", claude::short_parent_id(id)),
                    None => agent.unwrap_or_else(|| "Claude".to_string()),
                };
                for block in &message.content {
                    match block {
                        ContentBlock::Text { text } => {
                            let rendered =
                                crate::markdown::render_markdown_plain(text, content_width);
                            let rendered = rendered.trim_end();
                            append_ledger_block(&mut output, &speaker, rendered, NAME_WIDTH);
                            output.push('\n');
                        }
                        ContentBlock::ToolUse { name, input, .. } if options.show_tools => {
                            let formatted = format_tool_call_for_ledger(name, input, content_width);
                            let tool_label = if parent_tool_use_id.is_some() {
                                &speaker
                            } else {
                                "Tool"
                            };
                            append_ledger_block(&mut output, tool_label, &formatted, NAME_WIDTH);
                            output.push('\n');
                        }
                        ContentBlock::Thinking { thinking, .. }
                            if options.show_thinking && !thinking.is_empty() =>
                        {
                            let rendered =
                                crate::markdown::render_markdown_plain(thinking, content_width);
                            let rendered = rendered.trim_end();
                            append_ledger_block(&mut output, "Thinking", rendered, NAME_WIDTH);
                            output.push('\n');
                        }
                        _ => {}
                    }
                }
            }
            LogEntry::PiMetadata {
                label,
                text,
                searchable: true,
                ..
            } => {
                append_ledger_block(&mut output, &label, &text, NAME_WIDTH);
                output.push('\n');
            }
            _ => {}
        }
    }

    Ok(output)
}

/// Append a ledger-formatted block to the output
fn append_ledger_block(output: &mut String, speaker: &str, text: &str, name_width: usize) {
    for (i, line) in text.lines().enumerate() {
        if i == 0 {
            output.push_str(&format!(
                "{:>width$} │ {}\n",
                speaker,
                line,
                width = name_width
            ));
        } else {
            output.push_str(&format!("{:>width$} │ {}\n", "", line, width = name_width));
        }
    }
}

/// Generate a prefix string for subagent entries in exports.
/// Returns "[↳ID] " for nested entries, empty string for top-level.
fn subagent_prefix(parent_tool_use_id: &Option<String>) -> String {
    match parent_tool_use_id {
        Some(id) => format!("[↳{}] ", claude::short_parent_id(id)),
        None => String::new(),
    }
}

/// Extract text from a user message, handling command messages
fn extract_user_text(message: &UserMessage) -> Option<String> {
    match &message.content {
        UserContent::String(s) => process_command_text(s),
        UserContent::Blocks(blocks) => {
            for block in blocks {
                if let ContentBlock::Text { text } = block
                    && let Some(processed) = process_command_text(text)
                {
                    return Some(processed);
                }
            }
            None
        }
    }
}

/// Process command message text, extracting content from XML tags
fn process_command_text(text: &str) -> Option<String> {
    let trimmed = text.trim();

    // Handle <local-command-stdout> tags
    if trimmed.starts_with("<local-command-stdout>") && trimmed.ends_with("</local-command-stdout>")
    {
        let inner = &trimmed
            ["<local-command-stdout>".len()..trimmed.len() - "</local-command-stdout>".len()];
        if inner.trim().is_empty() {
            return None;
        }
        return Some(inner.trim().to_string());
    }

    if let Some(processed) = parse_command_name_and_args(trimmed) {
        return Some(processed);
    }

    Some(text.to_string())
}

/// Wrap content in markdown code fence, handling nested backticks
fn markdown_code_fence(content: &str) -> String {
    // Find the longest run of backticks in content and use one more
    let max_backticks = content
        .split(|c| c != '`')
        .map(|s| s.len())
        .max()
        .unwrap_or(0);
    let fence_len = std::cmp::max(3, max_backticks + 1);
    let fence: String = std::iter::repeat_n('`', fence_len).collect();
    format!("{}\n{}\n{}", fence, content, fence)
}

/// Default width for non-ledger export (no wrapping needed for markdown export)
const EXPORT_WIDTH: usize = usize::MAX;

/// Format a tool call for export (non-ledger formats)
fn format_tool_call_for_export(name: &str, input: &serde_json::Value) -> String {
    let formatted = tool_format::format_tool_call(name, input, EXPORT_WIDTH);
    match formatted.body {
        Some(body) => format!("{}\n{}", formatted.header, body),
        None => formatted.header,
    }
}

/// Format a tool call for ledger export with line wrapping
fn format_tool_call_for_ledger(name: &str, input: &serde_json::Value, max_width: usize) -> String {
    let formatted = tool_format::format_tool_call(name, input, max_width);
    let text = match formatted.body {
        Some(body) => format!("{}\n{}", formatted.header, body),
        None => formatted.header,
    };
    // Wrap any remaining long lines
    wrap_plain_text(&text, max_width)
}

/// Wrap plain text to max_width, preserving existing line breaks
fn wrap_plain_text(text: &str, max_width: usize) -> String {
    let mut result = String::new();
    for (i, line) in text.lines().enumerate() {
        if i > 0 {
            result.push('\n');
        }
        if line.is_empty() {
            continue;
        }
        let wrapped: Vec<_> = textwrap::wrap(line, max_width)
            .into_iter()
            .map(|cow| cow.into_owned())
            .collect();
        for (j, w) in wrapped.iter().enumerate() {
            if j > 0 {
                result.push('\n');
            }
            result.push_str(w);
        }
    }
    result
}

/// Format tool result content for export
fn format_tool_result_for_export(content: Option<&serde_json::Value>) -> String {
    match content {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Array(arr)) => {
            // Handle array of content blocks
            let texts: Vec<&str> = arr
                .iter()
                .filter_map(|item| item.get("text").and_then(|t| t.as_str()))
                .collect();
            if !texts.is_empty() {
                texts.join("\n\n")
            } else {
                serde_json::to_string_pretty(&arr).unwrap_or_else(|_| "<error>".to_string())
            }
        }
        Some(value) => {
            serde_json::to_string_pretty(value).unwrap_or_else(|_| "<error>".to_string())
        }
        None => "<no content>".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn transport_for_env(entries: &[(&str, &str)]) -> Result<ClipboardTransport, String> {
        clipboard_transport_from_env(|name| {
            entries
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| OsString::from(value))
        })
    }

    #[test]
    fn local_session_uses_system_clipboard() {
        assert_eq!(transport_for_env(&[]), Ok(ClipboardTransport::System));
    }

    #[test]
    fn remote_session_uses_osc52() {
        for name in REMOTE_SESSION_ENV_VARS {
            assert_eq!(
                transport_for_env(&[(name, "present")]),
                Ok(ClipboardTransport::Osc52),
                "{name} should identify a remote session"
            );
        }
    }

    #[test]
    fn clipboard_transport_override_takes_precedence() {
        assert_eq!(
            transport_for_env(&[
                ("SSH_CONNECTION", "client server"),
                (CLIPBOARD_TRANSPORT_ENV, "system"),
            ]),
            Ok(ClipboardTransport::System)
        );
        assert_eq!(
            transport_for_env(&[(CLIPBOARD_TRANSPORT_ENV, "osc52")]),
            Ok(ClipboardTransport::Osc52)
        );
        assert_eq!(
            transport_for_env(&[("SSH_TTY", "/dev/pts/1"), (CLIPBOARD_TRANSPORT_ENV, "auto"),]),
            Ok(ClipboardTransport::Osc52)
        );
    }

    #[test]
    fn invalid_clipboard_transport_override_is_rejected() {
        let error = transport_for_env(&[(CLIPBOARD_TRANSPORT_ENV, "remote")]).unwrap_err();
        assert_eq!(
            error,
            "Invalid REARVIEW_CLIPBOARD: expected auto, system, or osc52"
        );
    }

    #[test]
    fn terminal_clipboard_uses_osc52_clipboard_selection() {
        let mut output = Vec::new();
        copy_via_terminal(&mut output, "hello 🌍").unwrap();

        assert_eq!(output, b"\x1b]52;c;aGVsbG8g8J+MjQ==\x1b\\");
    }

    #[test]
    fn terminal_clipboard_surfaces_write_errors() {
        struct BrokenWriter;

        impl Write for BrokenWriter {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("write failed"))
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        assert_eq!(
            copy_via_terminal(BrokenWriter, "text"),
            Err("Terminal clipboard error: write failed".to_string())
        );
    }

    #[test]
    fn test_wrap_plain_text_preserves_short_lines() {
        let result = wrap_plain_text("short line", 80);
        assert_eq!(result, "short line");
    }

    #[test]
    fn test_wrap_plain_text_wraps_long_line() {
        let long = "word ".repeat(20); // 100 chars
        let result = wrap_plain_text(long.trim(), 40);
        for line in result.lines() {
            assert!(line.len() <= 40, "Line exceeds max_width: {:?}", line);
        }
        // All words should be preserved
        assert_eq!(result.matches("word").count(), 20);
    }

    #[test]
    fn test_wrap_plain_text_preserves_existing_newlines() {
        let text = "line one\nline two\nline three";
        let result = wrap_plain_text(text, 80);
        assert_eq!(result.lines().count(), 3);
    }

    #[test]
    fn test_wrap_plain_text_preserves_empty_lines() {
        let text = "line one\n\nline three";
        let result = wrap_plain_text(text, 80);
        assert_eq!(result, "line one\n\nline three");
    }

    #[test]
    fn test_append_ledger_block_format() {
        let mut output = String::new();
        append_ledger_block(&mut output, "Claude", "Hello\nWorld", 9);
        let lines: Vec<&str> = output.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].starts_with("   Claude │ Hello"));
        assert!(lines[1].starts_with("          │ World"));
    }

    #[test]
    fn test_ledger_line_width() {
        // Verify that a wrapped line fits within LEDGER_WIDTH
        let name_width = 9;
        let content_width = LEDGER_WIDTH - name_width - 3;
        let long_text = "word ".repeat(20);
        let wrapped = wrap_plain_text(long_text.trim(), content_width);
        let mut output = String::new();
        append_ledger_block(&mut output, "Claude", &wrapped, name_width);
        for line in output.lines() {
            // Count display width (name + " │ " + content)
            let width = line.chars().count();
            assert!(
                width <= LEDGER_WIDTH,
                "Ledger line exceeds {} chars (got {}): {:?}",
                LEDGER_WIDTH,
                width,
                line
            );
        }
    }

    #[test]
    fn test_ledger_markdown_rendering() {
        // Verify that markdown is rendered (not raw) in ledger export
        let content_width = LEDGER_WIDTH - 9 - 3;
        let rendered =
            crate::markdown::render_markdown_plain("This has **bold** and `code`", content_width);
        // Should not contain markdown formatting markers for bold
        assert!(
            !rendered.contains("**"),
            "Should strip bold markers: {:?}",
            rendered
        );
        // Should contain backticks for inline code
        assert!(
            rendered.contains("`code`"),
            "Should keep inline code backticks: {:?}",
            rendered
        );
        // Should not contain ANSI codes
        assert!(
            !rendered.contains("\x1b"),
            "Should not contain ANSI codes: {:?}",
            rendered
        );
    }

    #[test]
    fn pi_exports_use_pi_assistant_label() {
        let path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/pi/v3-branched.jsonl");
        let options = ExportOptions::default();

        let plain = generate_plain(crate::history::Source::Pi, &path, options).unwrap();
        let markdown = generate_markdown(crate::history::Source::Pi, &path, options).unwrap();
        let ledger = generate_ledger(crate::history::Source::Pi, &path, options).unwrap();

        assert!(plain.contains("Pi: root answer"));
        assert!(markdown.contains("## Pi\n\nroot answer"));
        assert!(ledger.contains("Pi │ root answer"));
        assert!(!plain.contains("Claude: root answer"));
        for metadata in [
            "Branch summary",
            "Compaction",
            "Thinking level",
            "Model",
            "Label",
            "custom state searchable",
        ] {
            assert!(!plain.contains(metadata));
            assert!(!markdown.contains(metadata));
            assert!(!ledger.contains(metadata));
        }
    }

    #[test]
    fn test_generate_ledger_wraps_and_renders() {
        // Create a sample JSONL with a long assistant message containing markdown
        let long_text = "This is a **really long** sentence that should definitely wrap because it contains many words and exceeds the content width of the ledger format which is 68 characters.";
        let entry = serde_json::json!({
            "type": "assistant",
            "message": {
                "id": "test",
                "type": "message",
                "role": "assistant",
                "content": [{"type": "text", "text": long_text}],
                "model": "test",
                "stop_reason": "end_turn",
                "stop_sequence": null,
                "usage": {"input_tokens": 0, "output_tokens": 0}
            },
            "timestamp": "2024-01-01T00:00:00Z"
        });

        let tmpdir = std::env::temp_dir();
        let tmppath = tmpdir.join("rearview-test-ledger.jsonl");
        std::fs::write(&tmppath, format!("{}\n", entry)).unwrap();

        let result = generate_ledger(
            crate::history::Source::Claude,
            &tmppath,
            ExportOptions {
                show_tools: false,
                show_thinking: false,
            },
        )
        .unwrap();

        std::fs::remove_file(&tmppath).ok();

        eprintln!("Ledger output:\n{}", result);

        // Every line should fit within LEDGER_WIDTH
        for line in result.lines() {
            if line.is_empty() {
                continue;
            }
            let width = line.chars().count();
            assert!(
                width <= LEDGER_WIDTH,
                "Ledger line exceeds {} chars (got {}): {:?}",
                LEDGER_WIDTH,
                width,
                line
            );
        }

        // Should contain the speaker name
        assert!(result.contains("Claude"), "Should have speaker name");
        // Should not contain ANSI codes
        assert!(!result.contains("\x1b"), "Should not contain ANSI codes");
        // Bold markers should be stripped (markdown rendered)
        assert!(
            !result.contains("**"),
            "Should not contain raw bold markers"
        );
        // Content should be wrapped across multiple lines
        let content_lines: Vec<&str> = result.lines().filter(|l| !l.is_empty()).collect();
        assert!(
            content_lines.len() > 1,
            "Long text should wrap to multiple lines, got: {:?}",
            content_lines
        );
    }
}
