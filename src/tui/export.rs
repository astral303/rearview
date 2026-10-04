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

use crate::history::{TASK_LABEL, user_task_report};
use crate::log_entry::{AssistantMessage, ContentBlock, LogEntry, Tool, UserContent, UserMessage};
use crate::tool_format;
use crate::tui::viewer::{BlockLocation, SubagentRoster, user_text};
use chrono::Local;
use crossterm::clipboard::CopyToClipboard;
use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
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
    subagents: &[PathBuf],
    format: ExportFormat,
    options: ExportOptions,
) -> ExportResult {
    let timestamp = Local::now().format("%Y-%m-%d-%H%M%S");
    let ext = format.extension();
    let filename = format!("conversation-{}.{}", timestamp, ext);

    let content = match generate_content(source, source_path, subagents, format, options) {
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

/// Extract the text content of a single message by its entry index in the JSONL file.
/// Returns the message text suitable for clipboard copying.
pub fn extract_message_text(
    source: crate::history::Source,
    source_path: &Path,
    subagents: &[PathBuf],
    entry_index: usize,
    options: ExportOptions,
) -> Result<String, String> {
    let displayed = crate::history::display_log_entries(source, source_path, subagents)
        .map_err(|e| format!("Failed to read: {e}"))?;
    displayed
        .entries
        .get(entry_index)
        .map(|entry| format_entry_for_clipboard(entry, options))
        .ok_or_else(|| "Message not found".to_string())
}

/// The text of one tool call for the clipboard: its header and full input,
/// then the full text of the result that answers it, when there is one.
pub fn extract_call_text(
    source: crate::history::Source,
    source_path: &Path,
    subagents: &[PathBuf],
    input: BlockLocation,
    result: Option<BlockLocation>,
) -> Result<String, String> {
    let entries = crate::history::display_log_entries(source, source_path, subagents)
        .map_err(|e| format!("Failed to read: {e}"))?
        .entries;
    let mut output = match content_block_at(&entries, input) {
        Some(ContentBlock::ToolUse {
            name, tool, input, ..
        }) => format_tool_call_for_export(name, *tool, input),
        _ => return Err("Call not found".to_string()),
    };
    if let Some(result) = result {
        match content_block_at(&entries, result) {
            Some(ContentBlock::ToolResult { content, .. }) => {
                append_separated(
                    &mut output,
                    &format_tool_result_for_export(content.as_ref()),
                );
            }
            _ => return Err("Result not found".to_string()),
        }
    }
    Ok(output)
}

fn content_block_at(entries: &[LogEntry], location: BlockLocation) -> Option<&ContentBlock> {
    let blocks = match entries.get(location.entry_index)? {
        LogEntry::Assistant { message, .. } => message.content.as_slice(),
        LogEntry::User {
            message:
                UserMessage {
                    content: UserContent::Blocks(blocks),
                    ..
                },
            ..
        } => blocks.as_slice(),
        _ => return None,
    };
    blocks.get(location.block_index)
}

/// Format a single log entry as text for clipboard
/// Append text with blank-line separation if output is non-empty.
fn append_separated(output: &mut String, text: &str) {
    if !output.is_empty() {
        output.push_str("\n\n");
    }
    output.push_str(text);
}

/// True when `options` show thinking and the block has text; Claude Code
/// sessions hold many empty thinking blocks.
fn shows_thinking(thinking: &str, options: &ExportOptions) -> bool {
    options.show_thinking && !thinking.is_empty()
}

/// Iterate content blocks and append formatted output for clipboard-style export.
/// Handles Text, ToolUse, ToolResult, and Thinking blocks guarded by options.
fn append_clipboard_blocks(output: &mut String, blocks: &[ContentBlock], options: &ExportOptions) {
    for block in blocks {
        match block {
            ContentBlock::Text { text } => {
                append_separated(output, text);
            }
            ContentBlock::ToolUse {
                name, tool, input, ..
            } if options.show_tools => {
                append_separated(output, &format_tool_call_for_export(name, *tool, input));
            }
            ContentBlock::ToolResult { content, .. } if options.show_tools => {
                append_separated(output, &format_tool_result_for_export(content.as_ref()));
            }
            ContentBlock::Thinking { thinking, .. } if shows_thinking(thinking, options) => {
                append_separated(output, thinking);
            }
            _ => {}
        }
    }
}

/// Invoke `f` with each call a user message holds — a command the user ran
/// themselves — when show_tools is enabled.
fn for_user_tool_calls(
    message: &UserMessage,
    options: &ExportOptions,
    mut f: impl FnMut(&str, Tool, &serde_json::Value),
) {
    if options.show_tools
        && let UserContent::Blocks(blocks) = &message.content
    {
        for block in blocks {
            if let ContentBlock::ToolUse {
                name, tool, input, ..
            } = block
            {
                f(name, *tool, input);
            }
        }
    }
}

/// Invoke `f` with each result of a user message and the tool a standalone
/// result names for itself.
fn for_user_tool_results(
    message: &UserMessage,
    options: &ExportOptions,
    mut f: impl FnMut(&str, Option<&str>),
) {
    if options.show_tools
        && let UserContent::Blocks(blocks) = &message.content
    {
        for block in blocks {
            if let ContentBlock::ToolResult {
                content,
                standalone_tool_name,
                ..
            } = block
            {
                let content_str = format_tool_result_for_export(content.as_ref());
                f(&content_str, standalone_tool_name.as_deref());
            }
        }
    }
}

fn format_entry_for_clipboard(entry: &LogEntry, options: ExportOptions) -> String {
    let mut output = String::new();
    match ExportedEntry::of(entry) {
        Some(ExportedEntry::User { message, .. }) => {
            if let (_, Some(text)) = user_speaker_and_text(message) {
                output.push_str(&text);
            }
            for_user_tool_calls(message, &options, |name, tool, input| {
                append_separated(&mut output, &format_tool_call_for_export(name, tool, input));
            });
            for_user_tool_results(message, &options, |content, _| {
                append_separated(&mut output, content);
            });
        }
        Some(ExportedEntry::Assistant { message, .. }) => {
            append_clipboard_blocks(&mut output, &message.content, &options);
        }
        Some(ExportedEntry::Metadata { label, text }) => {
            output.push_str(&format!("[{label}] {text}"));
        }
        None => {}
    }
    output
}

/// The part of an entry an export or a clipboard copy writes.
enum ExportedEntry<'a> {
    User {
        message: &'a UserMessage,
        parent_tool_use_id: &'a Option<String>,
    },
    Assistant {
        message: &'a AssistantMessage,
        agent: &'a Option<String>,
        parent_tool_use_id: &'a Option<String>,
    },
    Metadata {
        label: &'a str,
        text: &'a str,
    },
}

impl<'a> ExportedEntry<'a> {
    /// `None` for the metadata entries exports skip. The match names each
    /// skipped variant instead of ending in a catch-all, so a new variant
    /// fails to compile until it is classified here.
    fn of(entry: &'a LogEntry) -> Option<Self> {
        match entry {
            LogEntry::User {
                message,
                parent_tool_use_id,
                ..
            } => Some(Self::User {
                message,
                parent_tool_use_id,
            }),
            LogEntry::Assistant {
                message,
                agent,
                parent_tool_use_id,
                ..
            } => Some(Self::Assistant {
                message,
                agent,
                parent_tool_use_id,
            }),
            LogEntry::PiMetadata {
                label,
                text,
                searchable: true,
                ..
            } => Some(Self::Metadata { label, text }),
            LogEntry::Summary { .. }
            | LogEntry::FileHistorySnapshot { .. }
            | LogEntry::Progress { .. }
            | LogEntry::System { .. }
            | LogEntry::CustomTitle { .. }
            | LogEntry::AiTitle { .. }
            | LogEntry::AgentName { .. }
            | LogEntry::PermissionMode { .. }
            | LogEntry::PiMetadata {
                searchable: false, ..
            }
            | LogEntry::Unknown => None,
        }
    }
}

/// Generate content in the specified format
pub(crate) fn generate_content(
    source: crate::history::Source,
    source_path: &Path,
    subagents: &[PathBuf],
    format: ExportFormat,
    options: ExportOptions,
) -> std::io::Result<String> {
    match format {
        ExportFormat::Jsonl => fs::read_to_string(source_path),
        ExportFormat::Plain => generate_plain(source, source_path, subagents, options),
        ExportFormat::Markdown => generate_markdown(source, source_path, subagents, options),
        ExportFormat::Ledger => generate_ledger(source, source_path, subagents, options),
    }
}

/// A conversation as the exports read it: its entries, and the viewer's label
/// for each sub-agent's rows.
struct ExportedConversation {
    entries: Vec<LogEntry>,
    roster: SubagentRoster,
}

impl ExportedConversation {
    /// `[↳label] ` before a sub-agent's row; empty for the session's own.
    fn subagent_prefix(&self, parent_tool_use_id: &Option<String>) -> String {
        match parent_tool_use_id {
            Some(id) => format!("[{}] ", self.roster.label(id)),
            None => String::new(),
        }
    }
}

/// The conversation at `path`, read through `source`'s format with the row's
/// sub-agent transcripts spliced in.
fn export_conversation(
    source: crate::history::Source,
    path: &Path,
    subagents: &[PathBuf],
) -> std::io::Result<ExportedConversation> {
    let displayed = crate::history::display_log_entries(source, path, subagents)
        .map_err(|error| std::io::Error::other(error.to_string()))?;
    Ok(ExportedConversation {
        roster: SubagentRoster::from_identities(displayed.subagent_identities),
        entries: displayed.entries,
    })
}

/// The row one export format writes for each kind of content the shared walk
/// in [`generate_rows`] visits. `prefix` marks a sub-agent's entry.
trait ExportRowWriter {
    fn text(&self, output: &mut String, prefix: &str, speaker: &str, text: &str);
    fn user_tool_call(&self, output: &mut String, prefix: &str, formatted: &str);
    fn tool_result(&self, output: &mut String, prefix: &str, label: &str, content: &str);
    fn assistant_tool_call(&self, output: &mut String, prefix: &str, name: &str, formatted: &str);
    fn thinking(&self, output: &mut String, prefix: &str, thinking: &str);
}

const UNNAMED_TOOL_RESULT_LABEL: &str = "Tool Result";

fn generate_rows(
    conversation: &ExportedConversation,
    options: ExportOptions,
    writer: &impl ExportRowWriter,
) -> std::io::Result<String> {
    let mut output = String::new();

    for entry in &conversation.entries {
        match ExportedEntry::of(entry) {
            Some(ExportedEntry::User {
                message,
                parent_tool_use_id,
            }) => {
                if parent_tool_use_id.is_some() && !options.show_thinking {
                    continue;
                }
                let prefix = conversation.subagent_prefix(parent_tool_use_id);
                if let (speaker, Some(text)) = user_speaker_and_text(message) {
                    writer.text(&mut output, &prefix, speaker, &text);
                }
                for_user_tool_calls(message, &options, |name, tool, input| {
                    let formatted = format_tool_call_for_export(name, tool, input);
                    writer.user_tool_call(&mut output, &prefix, &formatted);
                });
                for_user_tool_results(message, &options, |content, name| {
                    let label = name.unwrap_or(UNNAMED_TOOL_RESULT_LABEL);
                    writer.tool_result(&mut output, &prefix, label, content);
                });
            }
            Some(ExportedEntry::Assistant {
                message,
                agent,
                parent_tool_use_id,
            }) => {
                if parent_tool_use_id.is_some() && !options.show_thinking {
                    continue;
                }
                let prefix = conversation.subagent_prefix(parent_tool_use_id);
                let speaker = agent.as_deref().unwrap_or("Claude");
                for block in &message.content {
                    match block {
                        ContentBlock::Text { text } => {
                            writer.text(&mut output, &prefix, speaker, text);
                        }
                        ContentBlock::ToolUse {
                            name, tool, input, ..
                        } if options.show_tools => {
                            let formatted = format_tool_call_for_export(name, *tool, input);
                            writer.assistant_tool_call(&mut output, &prefix, name, &formatted);
                        }
                        ContentBlock::Thinking { thinking, .. }
                            if shows_thinking(thinking, &options) =>
                        {
                            writer.thinking(&mut output, &prefix, thinking);
                        }
                        _ => {}
                    }
                }
            }
            Some(ExportedEntry::Metadata { label, text }) => {
                let rendered = if text.is_empty() {
                    format!("[{label}]")
                } else {
                    format!("[{label}] {text}")
                };
                writer.text(&mut output, "", "You", &rendered);
            }
            None => {}
        }
    }

    Ok(output)
}

/// `Speaker: message` lines.
struct PlainRowWriter;

impl ExportRowWriter for PlainRowWriter {
    fn text(&self, output: &mut String, prefix: &str, speaker: &str, text: &str) {
        output.push_str(&format!("{prefix}{speaker}: {text}\n\n"));
    }

    fn user_tool_call(&self, output: &mut String, prefix: &str, formatted: &str) {
        output.push_str(&format!("{prefix}You: {formatted}\n\n"));
    }

    fn tool_result(&self, output: &mut String, prefix: &str, label: &str, content: &str) {
        output.push_str(&format!("{prefix}{label}: {content}\n\n"));
    }

    fn assistant_tool_call(&self, output: &mut String, prefix: &str, _name: &str, formatted: &str) {
        output.push_str(&format!("{prefix}Tool: {formatted}\n\n"));
    }

    fn thinking(&self, output: &mut String, prefix: &str, thinking: &str) {
        output.push_str(&format!("{prefix}Thinking: {thinking}\n\n"));
    }
}

/// `##` headers for speakers, `###` for calls, results and thinking, with
/// call and result bodies fenced.
struct MarkdownRowWriter;

impl ExportRowWriter for MarkdownRowWriter {
    fn text(&self, output: &mut String, prefix: &str, speaker: &str, text: &str) {
        output.push_str(&format!("## {prefix}{speaker}\n\n{text}\n\n"));
    }

    fn user_tool_call(&self, output: &mut String, prefix: &str, formatted: &str) {
        let fenced = markdown_code_fence(formatted);
        output.push_str(&format!("## {prefix}You\n\n{fenced}\n\n"));
    }

    fn tool_result(&self, output: &mut String, prefix: &str, label: &str, content: &str) {
        let fenced = markdown_code_fence(content);
        output.push_str(&format!("### {prefix}{label}\n\n{fenced}\n\n"));
    }

    fn assistant_tool_call(&self, output: &mut String, prefix: &str, name: &str, formatted: &str) {
        let fenced = markdown_code_fence(formatted);
        output.push_str(&format!("### {prefix}Tool: {name}\n\n{fenced}\n\n"));
    }

    fn thinking(&self, output: &mut String, prefix: &str, thinking: &str) {
        output.push_str(&format!("### {prefix}Thinking\n\n{thinking}\n\n"));
    }
}

fn generate_plain(
    source: crate::history::Source,
    path: &Path,
    subagents: &[PathBuf],
    options: ExportOptions,
) -> std::io::Result<String> {
    generate_rows(
        &export_conversation(source, path, subagents)?,
        options,
        &PlainRowWriter,
    )
}

fn generate_markdown(
    source: crate::history::Source,
    path: &Path,
    subagents: &[PathBuf],
    options: ExportOptions,
) -> std::io::Result<String> {
    generate_rows(
        &export_conversation(source, path, subagents)?,
        options,
        &MarkdownRowWriter,
    )
}

/// Total line width for ledger export (including name column and separator)
const LEDGER_WIDTH: usize = 90;

/// Generate ledger-style format (formatted like the TUI viewer)
fn generate_ledger(
    source: crate::history::Source,
    path: &Path,
    subagents: &[PathBuf],
    options: ExportOptions,
) -> std::io::Result<String> {
    let conversation = export_conversation(source, path, subagents)?;
    let mut output = String::new();

    const NAME_WIDTH: usize = 9;
    // 3 for " │ " separator
    let content_width = LEDGER_WIDTH - NAME_WIDTH - 3;

    for entry in &conversation.entries {
        match ExportedEntry::of(entry) {
            Some(ExportedEntry::User {
                message,
                parent_tool_use_id,
            }) => {
                if parent_tool_use_id.is_some() && !options.show_thinking {
                    continue;
                }
                let (user_speaker, text) = user_speaker_and_text(message);
                let speaker = match parent_tool_use_id {
                    Some(id) => conversation.roster.label(id),
                    None => user_speaker.to_string(),
                };
                if let Some(text) = text {
                    let rendered = crate::markdown::render_markdown_plain(&text, content_width);
                    append_ledger_block(&mut output, &speaker, rendered.trim_end(), NAME_WIDTH);
                    output.push('\n');
                }
                // A user's own call keeps their label, as the viewer prints it.
                for_user_tool_calls(message, &options, |name, tool, input| {
                    let formatted = format_tool_call_for_ledger(name, tool, input, content_width);
                    append_ledger_block(&mut output, &speaker, &formatted, NAME_WIDTH);
                    output.push('\n');
                });
                for_user_tool_results(message, &options, |content, name| {
                    if !content.trim().is_empty() {
                        // The tool goes in the content, where the viewer puts
                        // it: the name column is a fixed width and a tool name
                        // is not, and `append_ledger_block` pads without
                        // truncating.
                        let named = match name {
                            Some(tool) => format!("{tool}: {content}"),
                            None => content.to_owned(),
                        };
                        let wrapped = wrap_plain_text(&named, content_width);
                        append_ledger_block(&mut output, "↳ Result", &wrapped, NAME_WIDTH);
                        output.push('\n');
                    }
                });
            }
            Some(ExportedEntry::Assistant {
                message,
                agent,
                parent_tool_use_id,
            }) => {
                if parent_tool_use_id.is_some() && !options.show_thinking {
                    continue;
                }
                let speaker = match parent_tool_use_id {
                    Some(id) => conversation.roster.label(id),
                    None => agent.as_deref().unwrap_or("Claude").to_owned(),
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
                        ContentBlock::ToolUse {
                            name, tool, input, ..
                        } if options.show_tools => {
                            let formatted =
                                format_tool_call_for_ledger(name, *tool, input, content_width);
                            let tool_label = if parent_tool_use_id.is_some() {
                                &speaker
                            } else {
                                "Tool"
                            };
                            append_ledger_block(&mut output, tool_label, &formatted, NAME_WIDTH);
                            output.push('\n');
                        }
                        ContentBlock::Thinking { thinking, .. }
                            if shows_thinking(thinking, &options) =>
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
            Some(ExportedEntry::Metadata { label, text }) => {
                append_ledger_block(&mut output, label, text, NAME_WIDTH);
                output.push('\n');
            }
            None => {}
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

/// The speaker a top-level user message exports under and its text, from one
/// parse: a background task's report whole under `Task`, or what the user
/// wrote under `You`.
fn user_speaker_and_text(message: &UserMessage) -> (&'static str, Option<String>) {
    match user_task_report(&message.content) {
        Some(report) => (TASK_LABEL, Some(report.display_text())),
        None => ("You", user_text(&message.content)),
    }
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

/// Format a tool call for export (non-ledger formats)
fn format_tool_call_for_export(name: &str, tool: Tool, input: &serde_json::Value) -> String {
    let formatted = tool_format::format_tool_call(name, tool, input, tool_format::NO_WRAP);
    match &formatted.body {
        Some(body) => format!("{}\n{}", formatted.header(), body.text),
        None => formatted.header(),
    }
}

/// Format a tool call for ledger export with line wrapping
fn format_tool_call_for_ledger(
    name: &str,
    tool: Tool,
    input: &serde_json::Value,
    max_width: usize,
) -> String {
    let formatted = tool_format::format_tool_call(name, tool, input, max_width);
    let text = match &formatted.body {
        Some(body) => format!("{}\n{}", formatted.header(), body.text),
        None => formatted.header(),
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
    use crate::tui::viewer::process_command_message;

    fn pi_fixture() -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/pi/v3-branched.jsonl")
    }

    const WITH_TOOLS: ExportOptions = ExportOptions {
        show_tools: true,
        show_thinking: false,
    };

    const WITH_THINKING: ExportOptions = ExportOptions {
        show_tools: false,
        show_thinking: true,
    };

    const WITH_TOOLS_AND_THINKING: ExportOptions = ExportOptions {
        show_tools: true,
        show_thinking: true,
    };

    /// The formats that render rows; JSONL copies the file as is.
    const RENDERED_FORMATS: [ExportFormat; 3] = [
        ExportFormat::Ledger,
        ExportFormat::Plain,
        ExportFormat::Markdown,
    ];

    const ASSISTANT_TEXT: &str = "listing the directory";
    const TOOL_NAME: &str = "Bash";
    const ASSISTANT_TOOL_CALL: &str = "Bash: ls";
    const THINKING_BLOCK: &str = "plan the listing";

    /// One Claude assistant entry holding a thinking block, a text block and a
    /// `Bash` call, written under `dir` so parallel test runs do not share it.
    fn claude_assistant_fixture(dir: &tempfile::TempDir) -> std::path::PathBuf {
        let path = dir.path().join("assistant.jsonl");
        let entry = serde_json::json!({
            "type": "assistant",
            "timestamp": "2024-01-01T00:00:01Z",
            "message": {
                "role": "assistant",
                "content": [
                    {"type": "thinking", "thinking": THINKING_BLOCK, "signature": ""},
                    {"type": "text", "text": ASSISTANT_TEXT},
                    {"type": "tool_use", "id": "toolu_01", "name": TOOL_NAME, "input": {"command": "ls"}}
                ]
            }
        })
        .to_string();
        std::fs::write(&path, format!("{entry}\n")).unwrap();
        path
    }

    fn export_claude_fixture(path: &Path, format: ExportFormat, options: ExportOptions) -> String {
        generate_content(crate::history::Source::Claude, path, &[], format, options)
            .unwrap_or_else(|error| panic!("{format:?} export fails: {error}"))
    }

    /// One Claude user entry per text, written under `dir`.
    fn claude_user_texts_fixture(dir: &tempfile::TempDir, texts: &[&str]) -> std::path::PathBuf {
        let path = dir.path().join("user.jsonl");
        let lines: String = texts
            .iter()
            .map(|text| {
                let entry = serde_json::json!({
                    "type": "user",
                    "timestamp": "2024-01-01T00:00:01Z",
                    "message": {"role": "user", "content": text}
                });
                format!("{entry}\n")
            })
            .collect();
        std::fs::write(&path, lines).unwrap();
        path
    }

    const CAVEAT_NOTICE: &str = "<local-command-caveat>Caveat: The messages below were generated by the user while running local commands.</local-command-caveat>";
    const CLEAR_COMMAND: &str = "<command-name>/clear</command-name>\n<command-message>clear</command-message>\n<command-args></command-args>";
    const SKILL_TEXT: &str = "Base directory for this skill: /skills/write-commit-messages\n\n# Write Commit Messages\n\nThe message carries why.";
    const SKILL_TEXT_BODY: &str = "The message carries why.";

    /// The `Skill:` line the viewer shows for `SKILL_TEXT`, as Markdown.
    fn skill_line() -> String {
        process_command_message(SKILL_TEXT).expect("skill text shows a line")
    }

    #[test]
    fn exports_skip_the_caveat_notice_and_clear() {
        let dir = tempfile::tempdir().unwrap();
        let path = claude_user_texts_fixture(&dir, &[CAVEAT_NOTICE, CLEAR_COMMAND]);

        for format in RENDERED_FORMATS {
            let exported = export_claude_fixture(&path, format, ExportOptions::default());
            assert!(!exported.contains("Caveat"), "{format:?}:\n{exported}");
            assert!(!exported.contains("/clear"), "{format:?}:\n{exported}");
        }
    }

    #[test]
    fn exports_print_skill_text_as_one_skill_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = claude_user_texts_fixture(&dir, &[SKILL_TEXT]);
        let skill_line = skill_line();
        let rendered_skill_line = crate::markdown::render_markdown_plain(&skill_line, 80);

        for (format, expected) in [
            (
                ExportFormat::Ledger,
                format!("You │ {}", rendered_skill_line.trim()),
            ),
            (ExportFormat::Plain, format!("You: {skill_line}")),
            (ExportFormat::Markdown, skill_line.clone()),
        ] {
            let exported = export_claude_fixture(&path, format, ExportOptions::default());
            assert!(exported.contains(&expected), "{format:?}:\n{exported}");
            assert!(
                !exported.contains(SKILL_TEXT_BODY),
                "{format:?}:\n{exported}"
            );
        }
    }

    #[test]
    fn a_ledger_export_renders_markdown_in_the_users_messages() {
        let dir = tempfile::tempdir().unwrap();
        let path = claude_user_texts_fixture(&dir, &["Keep **this** line"]);

        let exported = export_claude_fixture(&path, ExportFormat::Ledger, ExportOptions::default());

        assert!(exported.contains("You │ Keep this line"), "{exported}");
    }

    #[test]
    fn copying_skill_text_copies_its_skill_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = claude_user_texts_fixture(&dir, &[SKILL_TEXT]);
        let entries = export_conversation(crate::history::Source::Claude, &path, &[])
            .expect("the fixture parses")
            .entries;
        let skill_entry = entries.first().expect("the fixture holds one entry");

        let copied = format_entry_for_clipboard(skill_entry, ExportOptions::default());

        assert_eq!(copied, skill_line());
    }

    /// A Codex session with two sub-agents whose thread IDs share their first
    /// seven characters, each running one shell command under its nickname.
    fn codex_session_with_concurrent_sub_agents(
        dir: &tempfile::TempDir,
    ) -> (std::path::PathBuf, Vec<std::path::PathBuf>) {
        let record = |seconds: u32, record_type: &str, payload: &str| {
            format!(
                r#"{{"timestamp":"2026-08-01T10:00:{seconds:02}.000Z","type":"{record_type}","payload":{payload}}}"#
            )
        };
        let parent_thread = "019f0000-0000-7000-8000-0000000000a1";
        let parent = dir.path().join("rollout-parent.jsonl");
        let parent_meta = format!(
            r#"{{"id":"{parent_thread}","timestamp":"2026-08-01T10:00:00.000Z","cwd":"/tmp/project"}}"#
        );
        std::fs::write(&parent, record(0, "session_meta", &parent_meta) + "\n").unwrap();
        let mut sub_agents = Vec::new();
        for (thread, nickname, command) in [
            (
                "019f0000-0000-7000-8000-0000000000b2",
                "Lorentz",
                "cargo test",
            ),
            (
                "019f0000-0000-7000-8000-0000000000b3",
                "Galileo",
                "cargo build",
            ),
        ] {
            let meta = format!(
                r#"{{"id":"{thread}","timestamp":"2026-08-01T10:00:02.000Z","cwd":"/tmp/project","parent_thread_id":"{parent_thread}","agent_nickname":"{nickname}","agent_role":"suite_runner","agent_path":"/root/{nickname}"}}"#
            );
            let call = format!(
                r#"{{"type":"custom_tool_call","call_id":"call_{nickname}","name":"exec","input":"await tools.shell_command({{\"command\":\"{command}\"}})"}}"#
            );
            let output = format!(
                r#"{{"type":"custom_tool_call_output","call_id":"call_{nickname}","output":"ok"}}"#
            );
            let lines = [
                record(2, "session_meta", &meta),
                record(3, "response_item", &call),
                record(3, "response_item", &output),
            ];
            let path = dir.path().join(format!("rollout-{nickname}.jsonl"));
            std::fs::write(&path, lines.join("\n") + "\n").unwrap();
            sub_agents.push(path);
        }
        (parent, sub_agents)
    }

    #[test]
    fn concurrent_codex_sub_agents_export_under_their_nicknames() {
        let dir = tempfile::tempdir().unwrap();
        let (parent, sub_agents) = codex_session_with_concurrent_sub_agents(&dir);
        let export = |format| {
            generate_content(
                crate::history::Source::Codex,
                &parent,
                &sub_agents,
                format,
                WITH_TOOLS_AND_THINKING,
            )
            .unwrap_or_else(|error| panic!("{format:?} export fails: {error}"))
        };

        for format in [ExportFormat::Plain, ExportFormat::Markdown] {
            let exported = export(format);
            assert!(exported.contains("[↳Lorentz] Tool"), "{exported}");
            assert!(exported.contains("[↳Galileo] Tool"), "{exported}");
            assert!(!exported.contains("↳019f000"), "{exported}");
        }
        let ledger = export(ExportFormat::Ledger);
        assert!(ledger.contains("↳Lorentz │ "), "{ledger}");
        assert!(ledger.contains("↳Galileo │ "), "{ledger}");
        assert!(!ledger.contains("↳019f000"), "{ledger}");
    }

    #[test]
    fn exports_remove_terminal_styling_from_local_command_stdout() {
        let dir = tempfile::tempdir().unwrap();
        let path = claude_user_texts_fixture(
            &dir,
            &[
                "<local-command-stdout>\u{1b}[2mCompacted (ctrl+o to see full summary)\u{1b}[22m</local-command-stdout>",
            ],
        );

        for format in RENDERED_FORMATS {
            let exported = export_claude_fixture(&path, format, ExportOptions::default());
            assert!(
                exported.contains("Compacted (ctrl+o to see full summary)"),
                "{format:?}:\n{exported}"
            );
            assert!(!exported.contains('\u{1b}'), "{format:?}:\n{exported}");
        }
    }

    #[test]
    fn exports_print_an_assistant_tool_call_under_the_tool_label() {
        let dir = tempfile::tempdir().unwrap();
        let path = claude_assistant_fixture(&dir);

        for (format, expected) in [
            (
                ExportFormat::Ledger,
                format!("Tool │ {ASSISTANT_TOOL_CALL}"),
            ),
            (ExportFormat::Plain, format!("Tool: {ASSISTANT_TOOL_CALL}")),
            (
                ExportFormat::Markdown,
                format!("### Tool: {TOOL_NAME}\n\n```\n{ASSISTANT_TOOL_CALL}\n```"),
            ),
        ] {
            let exported = export_claude_fixture(&path, format, WITH_TOOLS);
            assert!(exported.contains(&expected), "{format:?}:\n{exported}");
        }
    }

    #[test]
    fn exports_print_a_thinking_block_under_the_thinking_label() {
        let dir = tempfile::tempdir().unwrap();
        let path = claude_assistant_fixture(&dir);

        for (format, expected) in [
            (ExportFormat::Ledger, format!("Thinking │ {THINKING_BLOCK}")),
            (ExportFormat::Plain, format!("Thinking: {THINKING_BLOCK}")),
            (
                ExportFormat::Markdown,
                format!("### Thinking\n\n{THINKING_BLOCK}"),
            ),
        ] {
            let exported = export_claude_fixture(&path, format, WITH_THINKING);
            assert!(exported.contains(&expected), "{format:?}:\n{exported}");
        }
    }

    #[test]
    fn exports_omit_assistant_tool_calls_and_thinking_blocks_by_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = claude_assistant_fixture(&dir);

        for format in RENDERED_FORMATS {
            let exported = export_claude_fixture(&path, format, ExportOptions::default());
            assert!(exported.contains(ASSISTANT_TEXT), "{format:?}:\n{exported}");
            assert!(
                !exported.contains(ASSISTANT_TOOL_CALL),
                "{format:?}:\n{exported}"
            );
            assert!(
                !exported.contains(THINKING_BLOCK),
                "{format:?}:\n{exported}"
            );
        }
    }

    #[test]
    fn the_clipboard_carries_an_assistant_tool_call_and_thinking_block() {
        let dir = tempfile::tempdir().unwrap();
        let path = claude_assistant_fixture(&dir);
        let entries = export_conversation(crate::history::Source::Claude, &path, &[])
            .expect("the fixture parses")
            .entries;
        let assistant_entry = entries.first().expect("the fixture holds one entry");

        let copied = format_entry_for_clipboard(assistant_entry, WITH_TOOLS_AND_THINKING);

        assert!(copied.contains(ASSISTANT_TOOL_CALL), "{copied}");
        assert!(copied.contains(THINKING_BLOCK), "{copied}");
    }

    /// The first entry of the Claude session at `path`, as exports read it.
    fn first_exported_entry(path: &Path) -> LogEntry {
        export_conversation(crate::history::Source::Claude, path, &[])
            .expect("the fixture parses")
            .entries
            .into_iter()
            .next()
            .expect("the fixture holds an entry")
    }

    const SECOND_TEXT: &str = "the prompt after the review comment";

    /// One Claude user entry holding two text blocks, the second carrying
    /// `SECOND_TEXT`.
    fn claude_two_text_blocks_fixture(dir: &tempfile::TempDir) -> std::path::PathBuf {
        let path = dir.path().join("two-texts.jsonl");
        let entry = serde_json::json!({
            "type": "user",
            "timestamp": "2024-01-01T00:00:01Z",
            "message": {
                "role": "user",
                "content": [
                    {"type": "text", "text": "a review comment on line 12"},
                    {"type": "text", "text": SECOND_TEXT}
                ]
            }
        })
        .to_string();
        std::fs::write(&path, format!("{entry}\n")).unwrap();
        path
    }

    #[test]
    fn exports_and_copies_carry_every_text_block_of_a_message() {
        let dir = tempfile::tempdir().unwrap();
        let path = claude_two_text_blocks_fixture(&dir);

        for format in RENDERED_FORMATS {
            let exported = export_claude_fixture(&path, format, ExportOptions::default());
            assert!(exported.contains(SECOND_TEXT), "{format:?}:\n{exported}");
        }
        let copied =
            format_entry_for_clipboard(&first_exported_entry(&path), ExportOptions::default());
        assert!(copied.contains(SECOND_TEXT), "{copied}");
    }

    const TEXT_BEFORE_EMPTY_THINKING: &str = "Reading the parser first.";

    /// One Claude assistant entry holding text, an empty thinking block, then
    /// more text: an empty block between two texts is the one that left an
    /// empty paragraph in a copy.
    fn claude_empty_thinking_fixture(dir: &tempfile::TempDir) -> std::path::PathBuf {
        let path = dir.path().join("empty-thinking.jsonl");
        let entry = serde_json::json!({
            "type": "assistant",
            "timestamp": "2024-01-01T00:00:01Z",
            "message": {
                "role": "assistant",
                "content": [
                    {"type": "text", "text": TEXT_BEFORE_EMPTY_THINKING},
                    {"type": "thinking", "thinking": "", "signature": "c2lnbmF0dXJl"},
                    {"type": "text", "text": ASSISTANT_TEXT}
                ]
            }
        })
        .to_string();
        std::fs::write(&path, format!("{entry}\n")).unwrap();
        path
    }

    #[test]
    fn exports_and_copies_skip_an_empty_thinking_block() {
        let dir = tempfile::tempdir().unwrap();
        let path = claude_empty_thinking_fixture(&dir);

        for format in RENDERED_FORMATS {
            let exported = export_claude_fixture(&path, format, WITH_THINKING);
            assert!(!exported.contains("Thinking"), "{format:?}:\n{exported}");
        }
        let copied = format_entry_for_clipboard(&first_exported_entry(&path), WITH_THINKING);
        assert_eq!(
            copied,
            format!("{TEXT_BEFORE_EMPTY_THINKING}\n\n{ASSISTANT_TEXT}")
        );
    }

    /// Two sub-agent turns recorded as `agent_progress`: a reply holding a
    /// `Bash` call, then the result answering it.
    fn sub_agent_progress_fixture(dir: &tempfile::TempDir) -> std::path::PathBuf {
        let path = dir.path().join("sub-agent.jsonl");
        let progress = |message_type: &str, content: serde_json::Value| {
            serde_json::json!({
                "type": "progress",
                "timestamp": "2024-01-01T00:00:01Z",
                "data": {
                    "type": "agent_progress",
                    "agentId": "agent-abcdef",
                    "message": {
                        "type": message_type,
                        "message": { "role": message_type, "content": content },
                    },
                },
            })
            .to_string()
        };
        let reply = progress(
            "assistant",
            serde_json::json!([
                {"type": "text", "text": ASSISTANT_TEXT},
                {"type": "tool_use", "id": "toolu_01", "name": TOOL_NAME, "input": {"command": "ls"}}
            ]),
        );
        let result = progress(
            "user",
            serde_json::json!([
                {"type": "tool_result", "tool_use_id": "toolu_01", "content": "Cargo.toml"}
            ]),
        );
        std::fs::write(&path, format!("{reply}\n{result}\n")).unwrap();
        path
    }

    #[test]
    fn yank_copies_a_sub_agents_call_and_message_recorded_as_agent_progress() {
        let dir = tempfile::tempdir().unwrap();
        let path = sub_agent_progress_fixture(&dir);
        let call = BlockLocation {
            entry_index: 0,
            block_index: 1,
        };
        let result = BlockLocation {
            entry_index: 1,
            block_index: 0,
        };

        let copied_call = extract_call_text(
            crate::history::Source::Claude,
            &path,
            &[],
            call,
            Some(result),
        )
        .expect("the sub-agent's call is found");
        assert!(copied_call.contains(ASSISTANT_TOOL_CALL), "{copied_call}");
        assert!(copied_call.contains("Cargo.toml"), "{copied_call}");

        let copied_message =
            extract_message_text(crate::history::Source::Claude, &path, &[], 0, WITH_TOOLS)
                .expect("the sub-agent's reply is found");
        assert!(copied_message.contains(ASSISTANT_TEXT), "{copied_message}");
    }

    /// The line `needle` sits on, whatever the shape indents or pads around it.
    fn line_with<'a>(text: &'a str, needle: &str) -> &'a str {
        text.lines()
            .find(|line| line.contains(needle))
            .unwrap_or_else(|| panic!("no line holds {needle:?}: {text}"))
    }

    /// The Markdown heading the line holding `needle` sits under.
    fn heading_above<'a>(text: &'a str, needle: &str) -> &'a str {
        let above = text.split(needle).next().expect("split yields the prefix");
        above
            .lines()
            .rev()
            .find(|line| line.starts_with('#'))
            .unwrap_or_else(|| panic!("no heading above {needle:?}: {text}"))
    }

    /// The ledger, plain and Markdown exports render the commands the user
    /// ran, which only the agent's own calls used to reach, and attribute them
    /// to the user rather than to a tool.
    #[test]
    fn exports_attribute_a_user_run_command_to_the_user() {
        for format in [
            ExportFormat::Ledger,
            ExportFormat::Plain,
            ExportFormat::Markdown,
        ] {
            let exported = generate_content(
                crate::history::Source::Pi,
                &pi_fixture(),
                &[],
                format,
                WITH_TOOLS,
            )
            .unwrap();
            let attribution = match format {
                ExportFormat::Markdown => heading_above(&exported, "ran false"),
                _ => line_with(&exported, "ran false"),
            };

            assert!(
                attribution.contains("You"),
                "{format:?} does not attribute the command to the user: {attribution:?}"
            );
            assert!(
                !attribution.contains("Tool") && !attribution.contains("bash"),
                "{format:?} names a tool the user did not call: {attribution:?}"
            );
        }
    }

    #[test]
    fn exports_show_a_skill_load_without_its_skill_text() {
        use crate::history::skill_text::test_support::{SKILL_CALL_LOAD, SLASH_COMMAND_LOAD};

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        std::fs::write(
            &path,
            [&SKILL_CALL_LOAD[..], &SLASH_COMMAND_LOAD]
                .concat()
                .join("\n"),
        )
        .unwrap();

        for format in RENDERED_FORMATS {
            let exported = export_claude_fixture(&path, format, WITH_TOOLS);

            assert!(
                exported.contains("/frontend-design:frontend-design"),
                "{format:?}:\n{exported}"
            );
            assert!(
                !exported.contains("Skill: frontend-design"),
                "the slash command's skill text: {format:?}:\n{exported}"
            );
            assert_eq!(
                exported.matches("Skill: write-commit-messages").count(),
                1,
                "only the `Skill` call's header: {format:?}:\n{exported}"
            );
        }
    }

    /// A standalone result names its tool in all three export shapes. The
    /// ledger prefixes it to the result text: its name column is a fixed nine
    /// columns and a tool name is not.
    #[test]
    fn exports_name_the_tool_a_received_result_carries() {
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/codex/rollout.jsonl");

        for (format, expected) in [
            (
                ExportFormat::Ledger,
                "send_message_to_thread: delegated task searchable",
            ),
            (
                ExportFormat::Plain,
                "send_message_to_thread: delegated task searchable",
            ),
            (ExportFormat::Markdown, "### send_message_to_thread"),
        ] {
            let exported = generate_content(
                crate::history::Source::Codex,
                &path,
                &[],
                format,
                WITH_TOOLS,
            )
            .unwrap();
            assert!(
                exported.contains(expected),
                "{format:?} does not carry {expected:?}: {exported}"
            );
            if !matches!(format, ExportFormat::Ledger) {
                continue;
            }
            for line in exported.lines() {
                assert!(
                    line.chars().count() <= LEDGER_WIDTH,
                    "a ledger row is wider than {LEDGER_WIDTH} columns: {line:?}"
                );
            }
        }
    }

    /// The clipboard carries one focused entry, so a user's command reaches it
    /// through a path of its own.
    #[test]
    fn the_clipboard_carries_a_user_run_command() {
        let entries = export_conversation(crate::history::Source::Pi, &pi_fixture(), &[])
            .expect("the fixture parses")
            .entries;
        let bash_entry = entries
            .iter()
            .find(|entry| {
                matches!(entry, LogEntry::User { message, .. }
                    if matches!(&message.content, UserContent::Blocks(blocks)
                        if blocks.iter().any(|block| matches!(block, ContentBlock::ToolUse { .. }))))
            })
            .expect("the fixture holds a command the user ran");

        let copied = format_entry_for_clipboard(bash_entry, WITH_TOOLS);

        assert!(copied.contains("ran false"), "{copied}");
        assert!(copied.contains("bash output searchable"), "{copied}");
    }

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

        let plain = generate_plain(crate::history::Source::Pi, &path, &[], options).unwrap();
        let markdown = generate_markdown(crate::history::Source::Pi, &path, &[], options).unwrap();
        let ledger = generate_ledger(crate::history::Source::Pi, &path, &[], options).unwrap();

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
            &[],
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

    #[test]
    fn exports_print_a_task_report_whole_under_the_task_label() {
        use crate::history::task_notification::test_support::*;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("report.jsonl");
        let notification = serde_json::json!({
            "type": "user",
            "timestamp": "2024-01-01T00:00:02Z",
            "message": {"role": "user", "content": AGENT_REPORT}
        })
        .to_string();
        std::fs::write(&path, format!("{notification}\n")).unwrap();

        for (format, label) in [
            (ExportFormat::Ledger, format!("Task │ {AGENT_SUMMARY}")),
            (ExportFormat::Plain, format!("Task: {AGENT_SUMMARY}")),
            (
                ExportFormat::Markdown,
                format!("## Task\n\n{AGENT_SUMMARY}"),
            ),
        ] {
            let exported = generate_content(
                crate::history::Source::Claude,
                &path,
                &[],
                format,
                ExportOptions::default(),
            )
            .unwrap();
            assert!(exported.contains(&label), "{format:?}:\n{exported}");
            assert!(
                exported.contains(AGENT_USAGE_LINE),
                "{format:?}:\n{exported}"
            );
            assert!(
                exported.contains(AGENT_REPORT_LAST_LINE),
                "{format:?}:\n{exported}"
            );
            assert!(!exported.contains("task-id"), "{format:?}:\n{exported}");
            assert!(!exported.contains("You"), "{format:?}:\n{exported}");
        }
    }
}
