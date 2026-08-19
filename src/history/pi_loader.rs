use super::provider::SessionRoot;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::Source;

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
            crate::history::format::owns_transcript(Source::Pi, &nested_files[0])
                || crate::history::format::owns_transcript(Source::Pi, &nested_files[1])
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
}
