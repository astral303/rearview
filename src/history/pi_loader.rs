use super::parser::process_conversation_file;
use super::provider::SessionRoot;
use super::{Conversation, Source, format_short_name_from_path};
use crate::cli::DebugLevel;
use crate::debug;
use crate::error::{AppError, Result};
use serde_json::Value;
use std::path::{Path, PathBuf};

pub fn session_root() -> Result<SessionRoot> {
    session_root_from(
        std::env::var_os("PI_CODING_AGENT_DIR").map(PathBuf::from),
        std::env::var_os("PI_CODING_AGENT_SESSION_DIR").map(PathBuf::from),
        home::home_dir(),
        std::env::current_dir().ok(),
    )
}

fn session_root_from(
    agent_override: Option<PathBuf>,
    session_override: Option<PathBuf>,
    home_dir: Option<PathBuf>,
    cwd: Option<PathBuf>,
) -> Result<SessionRoot> {
    let agent_dir = if let Some(value) = agent_override {
        expand_path_with_home(value, home_dir.as_deref())?
    } else {
        home_dir
            .clone()
            .ok_or_else(|| {
                AppError::Io(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "Could not determine home directory",
                ))
            })?
            .join(".pi")
            .join("agent")
    };

    if let Some(value) = session_override {
        return Ok(SessionRoot::flat(expand_path_with_home(
            value,
            home_dir.as_deref(),
        )?));
    }

    let global_setting = configured_session_dir(&agent_dir.join("settings.json"));
    let project_setting = cwd
        .as_deref()
        .and_then(|cwd| configured_session_dir(&cwd.join(".pi/settings.json")));
    if let Some(Some(value)) = project_setting.or(global_setting) {
        return Ok(SessionRoot::flat(expand_path_with_home(
            value,
            home_dir.as_deref(),
        )?));
    }

    Ok(SessionRoot::child_directories(agent_dir.join("sessions")))
}

fn configured_session_dir(path: &Path) -> Option<Option<PathBuf>> {
    let contents = std::fs::read_to_string(path).ok()?;
    let settings = serde_json::from_str::<Value>(&contents).ok()?;
    let value = settings.as_object()?.get("sessionDir")?;
    Some(
        value
            .as_str()
            .filter(|value| !value.trim().is_empty())
            .map(PathBuf::from),
    )
}

fn expand_path_with_home(path: PathBuf, home_dir: Option<&Path>) -> Result<PathBuf> {
    let missing_home = || {
        AppError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "Could not determine home directory",
        ))
    };
    let expanded = if path == Path::new("~") {
        home_dir.map(Path::to_path_buf).ok_or_else(missing_home)?
    } else if let Ok(rest) = path.strip_prefix("~/") {
        home_dir.ok_or_else(missing_home)?.join(rest)
    } else {
        path
    };
    Ok(if expanded.is_absolute() {
        expanded
    } else {
        std::env::current_dir()?.join(expanded)
    })
}

pub fn load_pi_conversations(
    show_last: bool,
    debug_level: Option<DebugLevel>,
) -> Result<Vec<Conversation>> {
    let root = session_root()?;
    let files = root.discover_files()?;
    let cached = super::cache::read_session_cache(Source::Pi, &root.path).unwrap_or_default();
    let mut updated_cache = std::collections::HashMap::new();
    let mut conversations = Vec::new();
    for path in files {
        let Ok(metadata) = std::fs::metadata(&path) else {
            continue;
        };
        let modified = metadata.modified().ok();
        let cache_key = path
            .strip_prefix(&root.path)
            .unwrap_or(&path)
            .to_string_lossy()
            .into_owned();
        let mut conversation = modified
            .and_then(|mtime| {
                cached.get(&cache_key).filter(|entry| {
                    super::cache::entry_matches(&entry.metadata, metadata.len(), mtime)
                })
            })
            .map(|entry| {
                let mut conversation =
                    super::cache::conversation_from_entry(&entry.metadata, path.clone(), show_last);
                conversation.source = Source::Pi;
                conversation.session_id = entry.session_id.clone();
                conversation.cwd = Some(entry.project_path.clone());
                conversation.project_path = Some(entry.project_path.clone());
                conversation.project_name = Some(format_short_name_from_path(&entry.project_path));
                conversation
            });
        if conversation.is_none() {
            conversation = match process_conversation_file(path.clone(), modified, debug_level) {
                Ok(Some(conversation)) if conversation.source == Source::Pi => Some(conversation),
                Ok(_) => None,
                Err(error) => {
                    debug::warn(
                        debug_level,
                        &format!("Failed to parse Pi session {}: {error}", path.display()),
                    );
                    None
                }
            };
        }
        let Some(mut conversation) = conversation else {
            continue;
        };
        conversation.preview = if show_last {
            conversation.preview_last.clone()
        } else {
            conversation.preview_first.clone()
        };
        let project_path = conversation
            .cwd
            .clone()
            .unwrap_or_else(|| PathBuf::from("unknown"));
        conversation.project_name = Some(format_short_name_from_path(&project_path));
        conversation.project_path = Some(project_path.clone());
        if let Some(mtime) = modified {
            updated_cache.insert(
                cache_key,
                super::cache::SessionCacheEntry {
                    metadata: super::cache::entry_from_conversation(
                        &conversation,
                        metadata.len(),
                        mtime,
                    ),
                    session_id: conversation.session_id.clone(),
                    project_path,
                },
            );
        }
        conversations.push(conversation);
    }
    super::cache::write_session_cache(Source::Pi, &root.path, updated_cache);
    conversations.sort_by(|left, right| right.timestamp.cmp(&left.timestamp));
    for (index, conversation) in conversations.iter_mut().enumerate() {
        conversation.index = index;
    }
    Ok(conversations)
}

pub fn delete_session(path: &Path) -> Result<()> {
    if path.extension().and_then(|extension| extension.to_str()) != Some("jsonl")
        || !super::pi::is_pi_file(path)
    {
        return Err(AppError::SessionNotFound(path.display().to_string()));
    }
    std::fs::remove_file(path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_directory_precedence_matches_pi() {
        let home = tempfile::tempdir().unwrap();
        let agent = home.path().join("agent");
        std::fs::create_dir_all(&agent).unwrap();
        std::fs::write(
            agent.join("settings.json"),
            r#"{"sessionDir":"~/settings-sessions"}"#,
        )
        .unwrap();

        let settings = session_root_from(
            Some(agent.clone()),
            None,
            Some(home.path().to_path_buf()),
            None,
        )
        .unwrap();
        assert_eq!(
            settings,
            SessionRoot::flat(home.path().join("settings-sessions"))
        );

        let project = home.path().join("project");
        std::fs::create_dir_all(project.join(".pi")).unwrap();
        std::fs::write(
            project.join(".pi/settings.json"),
            r#"{"sessionDir":"./project-sessions"}"#,
        )
        .unwrap();
        let project_settings = session_root_from(
            Some(agent.clone()),
            None,
            Some(home.path().to_path_buf()),
            Some(project),
        )
        .unwrap();
        assert_eq!(
            project_settings,
            SessionRoot::flat(std::env::current_dir().unwrap().join("project-sessions"))
        );

        let environment = session_root_from(
            Some(agent),
            Some(PathBuf::from("~/environment-sessions")),
            Some(home.path().to_path_buf()),
            None,
        )
        .unwrap();
        assert_eq!(
            environment,
            SessionRoot::flat(home.path().join("environment-sessions"))
        );
    }

    #[test]
    fn discovers_flat_and_nested_roots_and_validates_headers() {
        let directory = tempfile::tempdir().unwrap();
        let nested = directory.path().join("nested");
        let project = nested.join("--tmp-project--");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::copy(
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/pi/v1.jsonl"),
            project.join("session.jsonl"),
        )
        .unwrap();
        std::fs::write(project.join("not-pi.jsonl"), "{\"type\":\"user\"}\n").unwrap();

        let nested_files = SessionRoot::child_directories(nested)
            .discover_files()
            .unwrap();
        assert_eq!(nested_files.len(), 2);
        assert!(
            crate::history::pi::is_pi_file(&nested_files[0])
                || crate::history::pi::is_pi_file(&nested_files[1])
        );

        let flat = directory.path().join("flat");
        std::fs::create_dir_all(&flat).unwrap();
        std::fs::copy(
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/pi/v2.jsonl"),
            flat.join("flat.jsonl"),
        )
        .unwrap();
        assert_eq!(SessionRoot::flat(flat).discover_files().unwrap().len(), 1);
    }

    #[test]
    fn delete_removes_only_the_valid_pi_jsonl() {
        let directory = tempfile::tempdir().unwrap();
        let session = directory.path().join("session.jsonl");
        std::fs::copy(
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/pi/v1.jsonl"),
            &session,
        )
        .unwrap();
        let sibling = directory.path().join("sibling.txt");
        std::fs::write(&sibling, "keep").unwrap();

        delete_session(&session).unwrap();
        assert!(!session.exists());
        assert!(sibling.exists());
    }
}
