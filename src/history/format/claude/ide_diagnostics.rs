//! `diagnostics` attachments: IDE and language-server errors and warnings,
//! listed per file.

use serde_json::Value;

use crate::history::task_notification::{ReportKind, ReportSender, TaskReport};
use crate::log_entry::{LogEntry, UserContent};

const QUOTED_ATTACHMENT_TYPE: &str = "\"diagnostics\"";

/// The diagnostics in `line` as an `Issues` report: Claude Code's count line,
/// then each file's path followed by its issues. `None` for any other record,
/// and for an attachment listing no file. The count line counts every listed
/// file and issue, as Claude Code's does; the body leaves out an issue without
/// a position or a message.
pub(super) fn diagnostics_report(line: &str) -> Option<LogEntry> {
    if !line.contains(QUOTED_ATTACHMENT_TYPE) {
        return None;
    }
    let record = serde_json::from_str::<Value>(line).ok()?;
    let attachment = record.get("attachment")?;
    if attachment.get("type")?.as_str()? != "diagnostics" {
        return None;
    }
    let files = attachment.get("files")?.as_array()?;
    if files.is_empty() {
        return None;
    }
    let cwd = record.get("cwd").and_then(Value::as_str);
    let mut body = Vec::new();
    let mut issue_count = 0;
    for file in files {
        let issues = file
            .get("diagnostics")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default();
        issue_count += issues.len();
        if let Some(uri) = file.get("uri").and_then(Value::as_str) {
            body.push(format!("{}:", display_path(uri, cwd)));
        }
        body.extend(issues.iter().filter_map(issue_line));
    }
    let file_count = files.len();
    let report = TaskReport {
        kind: ReportKind::Diagnostics,
        summary: format!(
            "Found {issue_count} new diagnostic {} in {file_count} {}",
            if issue_count == 1 { "issue" } else { "issues" },
            if file_count == 1 { "file" } else { "files" },
        ),
        usage: None,
        body: Some(body.join("\n")),
        sender: ReportSender::default(),
    };
    let field = |name: &str| record.get(name).and_then(Value::as_str).map(str::to_owned);
    Some(super::user_record(
        UserContent::String(report.notification_text()),
        field,
    ))
}

/// `[Line <l>:<c>] <message> [<code>] (<source>)`, the line and column
/// counted from one, as Claude Code lists an issue.
fn issue_line(diagnostic: &Value) -> Option<String> {
    let position = |key: &str| {
        diagnostic
            .pointer(&format!("/range/start/{key}"))
            .and_then(Value::as_u64)
            .map(|position| position + 1)
    };
    let mut line = format!(
        "[Line {}:{}] {}",
        position("line")?,
        position("character")?,
        diagnostic.get("message")?.as_str()?
    );
    match diagnostic.get("code") {
        Some(Value::String(code)) => line.push_str(&format!(" [{code}]")),
        Some(Value::Number(code)) => line.push_str(&format!(" [{code}]")),
        _ => {}
    }
    if let Some(source) = diagnostic.get("source").and_then(Value::as_str) {
        line.push_str(&format!(" ({source})"));
    }
    Some(line)
}

/// The file `uri` names, without the `file://` or `_claude_fs_right:` prefix
/// Claude Code strips, relative to the session's working directory `cwd` when
/// the file is inside it.
fn display_path(uri: &str, cwd: Option<&str>) -> String {
    let path = ["file://", "_claude_fs_right:"]
        .into_iter()
        .find_map(|prefix| uri.strip_prefix(prefix))
        .unwrap_or(uri);
    cwd.and_then(|cwd| path.strip_prefix(cwd))
        .and_then(|relative| relative.strip_prefix(['/', '\\']))
        .unwrap_or(path)
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::display_path;

    #[test]
    fn a_files_path_drops_its_uri_prefix_and_the_working_directory() {
        for uri in [
            "file:///repo/src/main.rs",
            "_claude_fs_right:/repo/src/main.rs",
        ] {
            assert_eq!(display_path(uri, Some("/repo")), "src/main.rs", "{uri}");
        }
    }
}
