//! Claude Code sessions, stored under `~/.claude/projects/<encoded-cwd>/`.

use super::{SessionLaunch, SessionLauncher, SessionProvider, SessionStorage, SourceLabels};
use crate::error::{AppError, Result};
use crate::history::Source;
use crate::history::format::SessionFormat;
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
        }
    }

    /// Claude's transcripts are partitioned by project directory rather than
    /// gathered under a session root: discovery excludes `agent-*.jsonl`, caching
    /// is per project, and loading streams project batches to the TUI. None of
    /// that fits [`SessionStorage`], so Claude keeps its own loader.
    fn storage(&self) -> Option<&dyn SessionStorage> {
        None
    }

    /// Claude writes [`LogEntry`](crate::claude::LogEntry) records directly, with
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
