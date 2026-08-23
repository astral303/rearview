//! Claude Code sessions, stored under `~/.claude/projects/<encoded-cwd>/`.

use super::{
    RefNamespaces, SessionLaunch, SessionLauncher, SessionProvider, SessionStorage, SourceLabels,
};
use crate::error::{AppError, Result};
use crate::history::Source;
use crate::history::format::SessionFormat;
use crate::log_entry::{ContentBlock, LogEntry, Tool};
use serde_json::{Map, Value, json};
use std::path::{Path, PathBuf};
use std::process::Command;

pub struct ClaudeProvider;

impl SessionProvider for ClaudeProvider {
    fn source(&self) -> Source {
        Source::Claude
    }

    fn labels(&self) -> SourceLabels {
        SourceLabels {
            name: "claude",
            list: "CC",
            display: "Claude",
        }
    }

    fn ref_namespaces(&self) -> RefNamespaces {
        RefNamespaces {
            conversation: None,
            project: "agent-project-v1",
        }
    }

    /// Claude's transcripts are partitioned by project directory rather than
    /// gathered under a session root: discovery excludes `agent-*.jsonl`, caching
    /// is per project, and loading streams project batches to the TUI. None of
    /// that fits [`SessionStorage`], so Claude keeps its own loader.
    fn storage(&self) -> Option<&dyn SessionStorage> {
        None
    }

    /// Claude writes [`LogEntry`](crate::log_entry::LogEntry) records directly, with
    /// no session header stating an id, start time or cwd. There is nothing to
    /// project, so a file no other format claims is read as a Claude transcript.
    fn format(&self) -> Option<&dyn SessionFormat> {
        None
    }

    fn launcher(&self) -> &dyn SessionLauncher {
        &ClaudeLauncher
    }

    fn rename_session(&self, path: &Path, title: &str) -> Result<()> {
        crate::history::append_session_rename(path, title)
    }

    /// Claude deletes by session id rather than by path: the same transcript can
    /// exist under several project directories, and all of its copies go.
    fn delete_session(&self, path: &Path) -> Result<()> {
        let session_id = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or("");
        crate::history::delete_session_by_uuid(session_id).map(|_| ())
    }
}

/// Claude's tool names mapped onto the canonical [`Tool`] set.
///
/// Claude has no format module: its records deserialize straight into
/// [`LogEntry`] with every `tool` at `Other`, so this runs on each entry after
/// deserializing. Sub-agent turns inside a `Progress` payload are assigned in
/// place in the JSON, which keeps the rest of the payload as written.
pub(crate) fn assign_canonical_tools(entry: &mut LogEntry) {
    match entry {
        LogEntry::Assistant { message, .. } => {
            for block in &mut message.content {
                if let ContentBlock::ToolUse {
                    name, tool, input, ..
                } = block
                {
                    *tool = canonical_tool(name);
                    canonicalize_input(*tool, input);
                }
            }
        }
        LogEntry::Progress { data, .. } => {
            for block in agent_progress_tool_use_blocks(data) {
                let Some(name) = block.get("name").and_then(Value::as_str) else {
                    continue;
                };
                let tool = canonical_tool(name);
                if let Some(input) = block.get_mut("input") {
                    canonicalize_input(tool, input);
                }
                block.insert("tool".to_owned(), json!(tool));
            }
        }
        _ => {}
    }
}

fn canonical_tool(name: &str) -> Tool {
    match name {
        "Bash" | "PowerShell" => Tool::Shell,
        "Read" => Tool::Read,
        "Edit" => Tool::Edit,
        "Write" => Tool::Write,
        "Grep" => Tool::Grep,
        "Glob" => Tool::Glob,
        "WebFetch" => Tool::WebFetch,
        "WebSearch" => Tool::WebSearch,
        "Task" | "Agent" => Tool::Agent,
        "SendMessage" => Tool::AgentMessage,
        "TaskOutput" => Tool::Wait,
        "TaskCreate" | "TaskUpdate" | "TodoWrite" => Tool::TaskList,
        _ => Tool::Other,
    }
}

/// Claude's inputs already use the canonical keys, except that `SendMessage`
/// addresses its `recipient` as `to`.
fn canonicalize_input(tool: Tool, input: &mut Value) {
    if tool == Tool::AgentMessage
        && let Some(object) = input.as_object_mut()
        && let Some(recipient) = object.remove("to")
    {
        object.insert("recipient".to_owned(), recipient);
    }
}

/// The `tool_use` blocks of an `agent_progress` payload, at the path
/// [`parse_agent_progress`](crate::log_entry::parse_agent_progress) reads them from.
fn agent_progress_tool_use_blocks(
    data: &mut Value,
) -> impl Iterator<Item = &mut Map<String, Value>> {
    data.pointer_mut("/message/message/content")
        .and_then(Value::as_array_mut)
        .into_iter()
        .flatten()
        .filter_map(Value::as_object_mut)
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("tool_use"))
}

struct ClaudeLauncher;

impl SessionLauncher for ClaudeLauncher {
    fn resume_command(&self, launch: &SessionLaunch) -> Result<Command> {
        claude_command(launch, false)
    }

    fn fork_command(&self, launch: &SessionLaunch) -> Result<Command> {
        claude_command(launch, true)
    }
}

/// Claude resumes by conversation id and finds the transcript by the project
/// directory it runs in, so the command is only half the work: when the session
/// lives under a project Claude would not look in, its files are copied to one it
/// would.
fn claude_command(launch: &SessionLaunch, fork_session: bool) -> Result<Command> {
    let conversation_id = launch
        .path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .ok_or_else(|| {
            AppError::ClaudeExecutionError("Conversation filename is not valid Unicode".to_string())
        })?
        .to_owned();

    let cwd = std::env::current_dir().map_err(|error| {
        AppError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("Failed to get current directory: {error}"),
        ))
    })?;

    let conv_projects_dir = conversation_projects_dir(launch.path)?;

    let mut command = Command::new("claude");
    command.args(["--resume", &conversation_id]);
    match resolve_resume_action(launch.path, launch.project_path, &cwd, fork_session)? {
        ResumeAction::CopyToCurrent { cwd_projects_dir } => {
            std::fs::create_dir_all(&cwd_projects_dir).map_err(AppError::Io)?;
            copy_session_files(
                launch.path,
                &conversation_id,
                conv_projects_dir,
                &cwd_projects_dir,
            )?;
            command.args(launch.configured_args);
            command.current_dir(&cwd);
        }
        ResumeAction::Run { current_dir } => {
            if fork_session {
                command.arg("--fork-session");
            }
            command.args(launch.configured_args);
            command.current_dir(current_dir);
        }
    }
    Ok(command)
}

fn conversation_projects_dir(selected_path: &Path) -> Result<&Path> {
    selected_path.parent().ok_or_else(|| {
        AppError::ClaudeExecutionError(
            "Cannot determine conversation's project directory".to_string(),
        )
    })
}

#[derive(Debug, PartialEq, Eq)]
enum ResumeAction {
    Run { current_dir: PathBuf },
    CopyToCurrent { cwd_projects_dir: PathBuf },
}

fn resolve_resume_action(
    selected_path: &Path,
    project_path: Option<&Path>,
    cwd: &Path,
    fork_session: bool,
) -> Result<ResumeAction> {
    let conv_projects_dir = conversation_projects_dir(selected_path)?;
    let cwd_projects_dir = crate::history::get_claude_projects_dir(cwd)?;
    let project_dir = project_path.filter(|path| path.exists() && path.is_dir());

    if project_dir.is_none() || (fork_session && cwd_projects_dir != conv_projects_dir) {
        return Ok(ResumeAction::CopyToCurrent { cwd_projects_dir });
    }

    if cwd_projects_dir == conv_projects_dir {
        return Ok(ResumeAction::Run {
            current_dir: cwd.to_path_buf(),
        });
    }

    let project_dir = project_dir.unwrap();
    let project_projects_dir = crate::history::get_claude_projects_dir(project_dir)?;
    if project_projects_dir == conv_projects_dir {
        Ok(ResumeAction::Run {
            current_dir: project_dir.to_path_buf(),
        })
    } else {
        Ok(ResumeAction::CopyToCurrent { cwd_projects_dir })
    }
}

/// Copy a session into another project directory so a cross-project fork can find
/// it: the transcript, plus the session subdirectory holding tool results and
/// sub-agent transcripts.
///
/// `~/.claude/file-history/<uuid>/` is global rather than per project, and Claude
/// finds it by session id, so it needs no copy.
fn copy_session_files(
    jsonl_path: &Path,
    session_id: &str,
    source_projects_dir: &Path,
    target_projects_dir: &Path,
) -> Result<()> {
    let target_jsonl = target_projects_dir.join(jsonl_path.file_name().unwrap());
    std::fs::copy(jsonl_path, &target_jsonl).map_err(AppError::Io)?;

    let session_dir = source_projects_dir.join(session_id);
    if session_dir.is_dir() {
        copy_dir_recursive(&session_dir, &target_projects_dir.join(session_id))?;
    }

    Ok(())
}

fn copy_dir_recursive(source: &Path, destination: &Path) -> Result<()> {
    std::fs::create_dir_all(destination).map_err(AppError::Io)?;
    for entry in std::fs::read_dir(source).map_err(AppError::Io)? {
        let entry = entry.map_err(AppError::Io)?;
        let source_path = entry.path();
        let destination_path = destination.join(entry.file_name());
        if source_path.is_dir() {
            copy_dir_recursive(&source_path, &destination_path)?;
        } else {
            std::fs::copy(&source_path, &destination_path).map_err(AppError::Io)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::get_claude_projects_dir;

    fn assistant_entry_with_tool_uses(blocks: Vec<Value>) -> LogEntry {
        serde_json::from_value(json!({
            "type": "assistant",
            "message": {"role": "assistant", "content": blocks}
        }))
        .unwrap()
    }

    fn tool_use(name: &str, input: Value) -> Value {
        json!({"type": "tool_use", "id": "toolu_1", "name": name, "input": input})
    }

    fn assigned_tools(entry: &LogEntry) -> Vec<Tool> {
        let LogEntry::Assistant { message, .. } = entry else {
            panic!("expected an assistant entry");
        };
        message
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::ToolUse { tool, .. } => Some(*tool),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn every_claude_tool_name_lands_in_its_bucket() {
        let expected = [
            ("Bash", Tool::Shell),
            ("PowerShell", Tool::Shell),
            ("Read", Tool::Read),
            ("Edit", Tool::Edit),
            ("Write", Tool::Write),
            ("Grep", Tool::Grep),
            ("Glob", Tool::Glob),
            ("WebFetch", Tool::WebFetch),
            ("WebSearch", Tool::WebSearch),
            ("Task", Tool::Agent),
            ("Agent", Tool::Agent),
            ("SendMessage", Tool::AgentMessage),
            ("TaskOutput", Tool::Wait),
            ("TaskCreate", Tool::TaskList),
            ("TaskUpdate", Tool::TaskList),
            ("TodoWrite", Tool::TaskList),
            ("ExitPlanMode", Tool::Other),
            ("EnterPlanMode", Tool::Other),
            ("Skill", Tool::Other),
            ("ToolSearch", Tool::Other),
            ("AskUserQuestion", Tool::Other),
            ("TaskStop", Tool::Other),
            ("Artifact", Tool::Other),
            ("ReportFindings", Tool::Other),
            ("mcp__rustrover__ide_find_references", Tool::Other),
        ];
        let mut entry = assistant_entry_with_tool_uses(
            expected
                .iter()
                .map(|(name, _)| tool_use(name, json!({})))
                .collect(),
        );

        assign_canonical_tools(&mut entry);

        let tools: Vec<Tool> = expected.iter().map(|(_, tool)| *tool).collect();
        assert_eq!(assigned_tools(&entry), tools);
    }

    #[test]
    fn send_message_input_names_its_recipient() {
        let mut entry = assistant_entry_with_tool_uses(vec![tool_use(
            "SendMessage",
            json!({"to": "worker-1", "message": "status?", "summary": "ask"}),
        )]);

        assign_canonical_tools(&mut entry);

        let LogEntry::Assistant { message, .. } = &entry else {
            unreachable!()
        };
        let ContentBlock::ToolUse { input, .. } = &message.content[0] else {
            unreachable!()
        };
        assert_eq!(
            input,
            &json!({"recipient": "worker-1", "message": "status?", "summary": "ask"})
        );
    }

    #[test]
    fn agent_progress_tool_uses_are_assigned_in_the_payload() {
        let mut entry: LogEntry = serde_json::from_value(json!({
            "type": "progress",
            "data": {
                "type": "agent_progress",
                "agentId": "agent-1",
                "prompt": "look around",
                "message": {
                    "type": "assistant",
                    "message": {"role": "assistant", "content": [
                        {"type": "text", "text": "checking"},
                        tool_use("Grep", json!({"pattern": "fn main"})),
                        tool_use("SendMessage", json!({"to": "lead", "message": "done"})),
                    ]}
                }
            }
        }))
        .unwrap();

        assign_canonical_tools(&mut entry);

        let LogEntry::Progress { data, .. } = &entry else {
            unreachable!()
        };
        assert_eq!(data["prompt"], json!("look around"));
        let content = &data["message"]["message"]["content"];
        assert_eq!(content[0], json!({"type": "text", "text": "checking"}));
        assert_eq!(content[1]["tool"], json!("grep"));
        assert_eq!(content[2]["tool"], json!("agent_message"));
        assert_eq!(content[2]["input"]["recipient"], json!("lead"));
        let progress = crate::log_entry::parse_agent_progress(data).unwrap();
        let crate::log_entry::AgentContent::Blocks(blocks) = &progress.message.message.content;
        assert!(matches!(
            blocks[1],
            ContentBlock::ToolUse {
                tool: Tool::Grep,
                ..
            }
        ));
    }

    fn transcript_in_project_of(directory: &Path) -> PathBuf {
        get_claude_projects_dir(directory)
            .unwrap()
            .join("12345678-1234-4234-9234-123456789abc.jsonl")
    }

    #[test]
    fn resume_action_uses_cwd_when_it_maps_to_selected_project_dir() {
        let cwd = tempfile::tempdir().unwrap();
        let stale_project = tempfile::tempdir().unwrap();

        let action = resolve_resume_action(
            &transcript_in_project_of(cwd.path()),
            Some(stale_project.path()),
            cwd.path(),
            false,
        )
        .unwrap();

        assert_eq!(
            action,
            ResumeAction::Run {
                current_dir: cwd.path().to_path_buf()
            }
        );
    }

    #[test]
    fn resume_action_uses_project_path_when_it_maps_to_selected_project_dir() {
        let cwd = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();

        let action = resolve_resume_action(
            &transcript_in_project_of(project.path()),
            Some(project.path()),
            cwd.path(),
            false,
        )
        .unwrap();

        assert_eq!(
            action,
            ResumeAction::Run {
                current_dir: project.path().to_path_buf()
            }
        );
    }

    #[test]
    fn resume_action_copies_selected_transcript_when_project_path_maps_elsewhere() {
        let cwd = tempfile::tempdir().unwrap();
        let selected_project = tempfile::tempdir().unwrap();
        let stale_project = tempfile::tempdir().unwrap();

        let action = resolve_resume_action(
            &transcript_in_project_of(selected_project.path()),
            Some(stale_project.path()),
            cwd.path(),
            false,
        )
        .unwrap();

        assert_eq!(
            action,
            ResumeAction::CopyToCurrent {
                cwd_projects_dir: get_claude_projects_dir(cwd.path()).unwrap()
            }
        );
    }
}
