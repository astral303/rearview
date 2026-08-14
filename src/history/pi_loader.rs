use super::parser::process_conversation_file;
use super::{Conversation, Source, format_short_name_from_path};
use crate::cli::DebugLevel;
use crate::debug;
use crate::error::{AppError, Result};
use serde_json::Value;
use std::fs::read_dir;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PiSessionRoot {
    pub path: PathBuf,
    pub flat: bool,
}

pub fn session_root() -> Result<PiSessionRoot> {
    session_root_from(
        std::env::var_os("PI_CODING_AGENT_DIR").map(PathBuf::from),
        std::env::var_os("PI_CODING_AGENT_SESSION_DIR").map(PathBuf::from),
        home::home_dir(),
    )
}

fn session_root_from(
    agent_override: Option<PathBuf>,
    session_override: Option<PathBuf>,
    home_dir: Option<PathBuf>,
) -> Result<PiSessionRoot> {
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
        return Ok(PiSessionRoot {
            path: expand_path_with_home(value, home_dir.as_deref())?,
            flat: true,
        });
    }

    let settings_path = agent_dir.join("settings.json");
    if let Ok(contents) = std::fs::read_to_string(settings_path)
        && let Ok(settings) = serde_json::from_str::<Value>(&contents)
        && let Some(value) = settings.get("sessionDir").and_then(Value::as_str)
        && !value.trim().is_empty()
    {
        return Ok(PiSessionRoot {
            path: expand_path_with_home(PathBuf::from(value), home_dir.as_deref())?,
            flat: true,
        });
    }

    Ok(PiSessionRoot {
        path: agent_dir.join("sessions"),
        flat: false,
    })
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

pub fn discover_files(root: &PiSessionRoot) -> Result<Vec<PathBuf>> {
    if !root.path.exists() {
        return Ok(Vec::new());
    }
    let mut files = Vec::new();
    if root.flat {
        collect_jsonl(&root.path, &mut files)?;
    } else {
        for entry in read_dir(&root.path)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                collect_jsonl(&path, &mut files)?;
            }
        }
    }
    files.sort();
    Ok(files)
}

fn collect_jsonl(directory: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    for entry in read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) == Some("jsonl") {
            files.push(path);
        }
    }
    Ok(())
}

pub fn load_pi_conversations(
    show_last: bool,
    debug_level: Option<DebugLevel>,
) -> Result<Vec<Conversation>> {
    let root = session_root()?;
    let files = discover_files(&root)?;
    let cached = super::cache::read_pi_cache(&root.path).unwrap_or_default();
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
                super::cache::PiCacheEntry {
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
    super::cache::write_pi_cache(&root.path, updated_cache);
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

        let settings =
            session_root_from(Some(agent.clone()), None, Some(home.path().to_path_buf())).unwrap();
        assert_eq!(settings.path, home.path().join("settings-sessions"));
        assert!(settings.flat);

        let environment = session_root_from(
            Some(agent),
            Some(PathBuf::from("~/environment-sessions")),
            Some(home.path().to_path_buf()),
        )
        .unwrap();
        assert_eq!(environment.path, home.path().join("environment-sessions"));
        assert!(environment.flat);
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

        let nested_files = discover_files(&PiSessionRoot {
            path: nested,
            flat: false,
        })
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
        assert_eq!(
            discover_files(&PiSessionRoot {
                path: flat,
                flat: true,
            })
            .unwrap()
            .len(),
            1
        );
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

    #[cfg(unix)]
    #[test]
    fn nested_discovery_follows_symlinked_project_directories() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("target");
        let root = directory.path().join("sessions");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::create_dir_all(&root).unwrap();
        std::fs::copy(
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/pi/v3-branched.jsonl"),
            target.join("session.jsonl"),
        )
        .unwrap();
        symlink(&target, root.join("--linked--")).unwrap();

        let files = discover_files(&PiSessionRoot {
            path: root,
            flat: false,
        })
        .unwrap();
        assert_eq!(files.len(), 1);
        assert!(crate::history::pi::is_pi_file(&files[0]));
    }
}
