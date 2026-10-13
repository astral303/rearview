//! `file`, `already_read_file`, `directory` and `compact_file_reference`
//! attachments: files and directories attached with `@`, and the files Claude
//! Code attaches again after a compaction. Claude Code writes a file too large
//! to attach again as a `compact_file_reference`, with no content.

use serde_json::{Value, json};

use crate::log_entry::{ContentBlock, LogEntry, Tool, UserContent};

const QUOTED_ATTACHMENT_TYPES: [&str; 4] = [
    "\"file\"",
    "\"already_read_file\"",
    "\"directory\"",
    "\"compact_file_reference\"",
];

/// Claude Code lists an attached directory without calling a tool.
const DIRECTORY_LISTING_NAME: &str = "List dir";

/// Claude Code writes a file reference without calling a tool.
const FILE_REFERENCE_NAME: &str = "Reference";

/// An attached file, directory or file reference in `line` as a call under
/// `You`, with the file's text or the listing as its result. `None` for any
/// other record, and for an attachment without a `displayPath`.
pub(super) fn attached_file_message(line: &str) -> Option<LogEntry> {
    // Parse the line a second time only when it can hold one of these
    // attachments.
    if !QUOTED_ATTACHMENT_TYPES
        .iter()
        .any(|pattern| line.contains(pattern))
    {
        return None;
    }
    let record = serde_json::from_str::<Value>(line).ok()?;
    let attachment = record.get("attachment")?;
    let display_path = attachment.get("displayPath")?.as_str()?;
    let (name, tool, input, result) = match attachment.get("type")?.as_str()? {
        "file" | "already_read_file" => (
            "Read",
            Tool::Read,
            read_input(display_path, attachment.pointer("/content/file")),
            attachment.pointer("/content/file/content"),
        ),
        "directory" => (
            DIRECTORY_LISTING_NAME,
            Tool::DirectoryListing,
            json!({ "path": display_path }),
            attachment.get("content"),
        ),
        "compact_file_reference" => (
            FILE_REFERENCE_NAME,
            Tool::FileReference,
            json!({ "file_path": display_path }),
            None,
        ),
        _ => return None,
    };
    let field = |name: &str| record.get(name).and_then(Value::as_str).map(str::to_owned);
    let call_id = field("uuid").unwrap_or_default();
    let result = result.map(|text| ContentBlock::ToolResult {
        tool_use_id: call_id.clone(),
        content: Some(text.clone()),
        standalone_tool_name: None,
    });
    let call = ContentBlock::ToolUse {
        id: call_id,
        name: name.to_owned(),
        tool,
        input,
    };
    let blocks = std::iter::once(call).chain(result).collect();
    Some(super::user_record(UserContent::Blocks(blocks), field))
}

/// A `Read` call's input for `file`, the attachment's `content.file`. It
/// carries `offset` and `limit` only when the attachment holds part of the
/// file: a line range in the `@` mention, or a file too long to attach whole.
fn read_input(display_path: &str, file: Option<&Value>) -> Value {
    let mut input = json!({ "file_path": display_path });
    let number = |key: &str| file?.get(key)?.as_u64();
    if let (Some(start), Some(count), Some(total)) = (
        number("startLine"),
        number("numLines"),
        number("totalLines"),
    ) && (start > 1 || count < total)
    {
        input["offset"] = json!(start);
        input["limit"] = json!(count);
    }
    input
}
