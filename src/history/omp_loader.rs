use super::parser::process_omp_conversation_file;
use super::provider::SessionRoot;
use super::{Conversation, Source, format_short_name_from_path};
use crate::cli::DebugLevel;
use crate::debug;
use crate::error::{AppError, Result};
use std::path::{Path, PathBuf};

pub fn session_root() -> Result<SessionRoot> {
    session_root_from(
        std::env::var_os("PI_CONFIG_DIR").map(PathBuf::from),
        std::env::var_os("PI_CODING_AGENT_DIR").map(PathBuf::from),
        std::env::var_os("PI_CODING_AGENT_SESSION_DIR").map(PathBuf::from),
        std::env::var_os("OMP_PROFILE"),
        std::env::var_os("PI_PROFILE"),
        std::env::var_os("XDG_DATA_HOME").map(PathBuf::from),
        home::home_dir(),
    )
}

fn session_root_from(
    config_dir: Option<PathBuf>,
    agent_override: Option<PathBuf>,
    session_override: Option<PathBuf>,
    omp_profile: Option<std::ffi::OsString>,
    pi_profile: Option<std::ffi::OsString>,
    xdg_data_home: Option<PathBuf>,
    home_dir: Option<PathBuf>,
) -> Result<SessionRoot> {
    let home = home_dir.ok_or_else(|| {
        AppError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "Could not determine home directory",
        ))
    })?;
    if let Some(path) = session_override {
        return Ok(SessionRoot::flat(expand_path_with_home(path, &home)));
    }

    let profile_value = omp_profile.or(pi_profile);
    let profile = profile_value
        .as_deref()
        .and_then(|value| value.to_str())
        .map(str::trim)
        .filter(|value| !value.is_empty() && *value != "default");
    let config_root = home.join(config_dir.unwrap_or_else(|| PathBuf::from(".omp")));
    let profile_root = profile
        .map(|profile| config_root.join("profiles").join(profile))
        .unwrap_or(config_root);
    let default_agent = profile_root.join("agent");
    let agent_dir = if profile.is_none() {
        agent_override
            .map(|path| expand_path_with_home(path, &home))
            .unwrap_or(default_agent)
    } else {
        default_agent
    };

    let xdg_profile_root = xdg_data_home.map(|root| {
        let root = root.join("omp");
        profile
            .map(|profile| root.join("profiles").join(profile))
            .unwrap_or(root)
    });
    let path = if agent_dir == profile_root.join("agent")
        && let Some(xdg_root) = xdg_profile_root
        && xdg_root.exists()
    {
        xdg_root.join("sessions")
    } else {
        agent_dir.join("sessions")
    };
    Ok(SessionRoot::child_directories(path))
}

fn expand_path_with_home(path: PathBuf, home: &Path) -> PathBuf {
    let expanded = if path == Path::new("~") {
        home.to_path_buf()
    } else if let Ok(rest) = path.strip_prefix("~/") {
        home.join(rest)
    } else {
        path
    };
    if expanded.is_absolute() {
        expanded
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(expanded)
    }
}

pub fn load_omp_conversations(
    show_last: bool,
    debug_level: Option<DebugLevel>,
) -> Result<Vec<Conversation>> {
    let root = session_root()?;
    let files = root.discover_files()?;
    let cached = super::cache::read_omp_cache(&root.path).unwrap_or_default();
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
                conversation.source = Source::Omp;
                conversation.session_id = entry.session_id.clone();
                conversation.cwd = Some(entry.project_path.clone());
                conversation.project_path = Some(entry.project_path.clone());
                conversation.project_name = Some(format_short_name_from_path(&entry.project_path));
                conversation
            });
        if conversation.is_none() {
            let root_is_omp_owned = root
                .path
                .components()
                .any(|component| matches!(component.as_os_str().to_str(), Some(".omp" | "omp")));
            let parsed = if root_is_omp_owned {
                process_omp_conversation_file(path.clone(), modified, debug_level)
            } else {
                super::parser::process_conversation_file(path.clone(), modified, debug_level)
            };
            conversation = match parsed {
                Ok(Some(conversation)) if conversation.source == Source::Omp => Some(conversation),
                Ok(_) => None,
                Err(error) => {
                    debug::warn(
                        debug_level,
                        &format!("Failed to parse OMP session {}: {error}", path.display()),
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
    super::cache::write_omp_cache(&root.path, updated_cache);
    conversations.sort_by(|left, right| right.timestamp.cmp(&left.timestamp));
    for (index, conversation) in conversations.iter_mut().enumerate() {
        conversation.index = index;
    }
    Ok(conversations)
}

pub fn delete_session(path: &Path) -> Result<()> {
    if path.extension().and_then(|extension| extension.to_str()) != Some("jsonl")
        || !super::pi::is_omp_file(path)
    {
        return Err(AppError::SessionNotFound(path.display().to_string()));
    }
    std::fs::remove_file(path)?;
    let artifacts = path.with_extension("");
    if artifacts.is_dir() {
        std::fs::remove_dir_all(artifacts)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_default_profile_overrides_and_xdg_roots() {
        let home = tempfile::tempdir().unwrap();
        let default = session_root_from(
            None,
            None,
            None,
            None,
            None,
            None,
            Some(home.path().to_path_buf()),
        )
        .unwrap();
        assert_eq!(
            default,
            SessionRoot::child_directories(home.path().join(".omp/agent/sessions"))
        );

        let profile = session_root_from(
            None,
            Some(PathBuf::from("~/ignored")),
            None,
            Some("work".into()),
            None,
            None,
            Some(home.path().to_path_buf()),
        )
        .unwrap();
        assert_eq!(
            profile.path,
            home.path().join(".omp/profiles/work/agent/sessions")
        );

        let xdg = home.path().join("xdg/omp");
        std::fs::create_dir_all(&xdg).unwrap();
        let xdg_root = session_root_from(
            None,
            None,
            None,
            None,
            None,
            Some(home.path().join("xdg")),
            Some(home.path().to_path_buf()),
        )
        .unwrap();
        assert_eq!(xdg_root.path, xdg.join("sessions"));

        let custom = session_root_from(
            None,
            None,
            Some(PathBuf::from("~/sessions")),
            None,
            None,
            None,
            Some(home.path().to_path_buf()),
        )
        .unwrap();
        assert_eq!(custom, SessionRoot::flat(home.path().join("sessions")));
    }
}
