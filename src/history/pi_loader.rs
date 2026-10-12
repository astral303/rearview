use super::provider::walk::FileRoot;
use super::provider::{RootEnvironment, SessionRoot};
use crate::error::{AppError, Result};
use serde_json::Value;
use std::path::{Path, PathBuf};

/// Moves Pi's agent directory, `~/.pi/agent` by default.
pub(super) const AGENT_DIR_VARIABLE: &str = "PI_CODING_AGENT_DIR";
/// Names the session directory itself, ahead of every other setting.
pub(super) const SESSION_DIR_VARIABLE: &str = "PI_CODING_AGENT_SESSION_DIR";

/// Every environment variable [`session_root_in`] reads.
pub const STORAGE_ENV_VARS: &[&str] = &[AGENT_DIR_VARIABLE, SESSION_DIR_VARIABLE];

/// Resolve Pi's session root together with its walk depth: a redirected
/// session directory holds transcripts beside itself, the default tree holds
/// them in a directory per project. Only this resolution knows which is which,
/// so the pairing is returned rather than guessed from the path later.
pub fn session_root() -> Result<FileRoot> {
    session_root_in(&RootEnvironment::of_process())
}

/// [`session_root`] as resolved from `environment`.
pub fn session_root_in(environment: &RootEnvironment) -> Result<FileRoot> {
    session_root_from(
        environment.var_os(AGENT_DIR_VARIABLE).map(PathBuf::from),
        environment.var_os(SESSION_DIR_VARIABLE).map(PathBuf::from),
        environment.home().map(Path::to_path_buf),
        environment.current_dir().map(Path::to_path_buf),
    )
}

/// A root the user redirected Pi to can hold another Pi-family agent's
/// transcripts, so only the default location is marked as Pi's own.
pub(crate) fn session_root_from(
    agent_override: Option<PathBuf>,
    session_override: Option<PathBuf>,
    home_dir: Option<PathBuf>,
    cwd: Option<PathBuf>,
) -> Result<FileRoot> {
    let agent_override = unset_if_empty(agent_override);
    let session_override = unset_if_empty(session_override);
    let redirected_agent_dir = agent_override.is_some();
    let expand = |path| expand_path_with_home(path, home_dir.as_deref(), cwd.as_deref());
    let agent_dir = if let Some(value) = agent_override {
        expand(value)?
    } else {
        home_dir
            .as_deref()
            .ok_or_else(AppError::home_not_found)?
            .join(".pi")
            .join("agent")
    };

    if let Some(value) = session_override {
        return Ok(flat_root(expand(value)?));
    }

    let global_setting = configured_session_dir(&agent_dir.join("settings.json"));
    let project_setting = cwd
        .as_deref()
        .and_then(|cwd| configured_session_dir(&cwd.join(".pi/settings.json")));
    if let Some(Some(value)) = project_setting.or(global_setting) {
        return Ok(flat_root(expand(value)?));
    }

    let sessions = SessionRoot::new(agent_dir.join("sessions"));
    let root = if redirected_agent_dir {
        sessions
    } else {
        sessions.in_agent_tree()
    };
    Ok(FileRoot { root, depth: 1 })
}

/// Pi and OMP skip a directory variable that is set but empty; a value of
/// spaces is a path to them.
pub(super) fn unset_if_empty(variable: Option<PathBuf>) -> Option<PathBuf> {
    variable.filter(|path| !path.as_os_str().is_empty())
}

fn flat_root(path: PathBuf) -> FileRoot {
    FileRoot {
        root: SessionRoot::new(path),
        depth: 0,
    }
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

fn expand_path_with_home(
    path: PathBuf,
    home_dir: Option<&Path>,
    cwd: Option<&Path>,
) -> Result<PathBuf> {
    let expanded = if path == Path::new("~") {
        home_dir.ok_or_else(AppError::home_not_found)?.to_path_buf()
    } else if let Ok(rest) = path.strip_prefix("~/") {
        home_dir.ok_or_else(AppError::home_not_found)?.join(rest)
    } else {
        path
    };
    Ok(if expanded.is_absolute() {
        expanded
    } else {
        cwd.ok_or_else(current_dir_not_found)?.join(expanded)
    })
}

fn current_dir_not_found() -> AppError {
    AppError::Io(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        "Could not determine the current directory",
    ))
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
        assert_eq!(settings, flat_root(home.path().join("settings-sessions")));

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
            Some(project.clone()),
        )
        .unwrap();
        assert_eq!(
            project_settings,
            flat_root(project.join("project-sessions"))
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
            flat_root(home.path().join("environment-sessions"))
        );
    }

    #[test]
    fn a_relative_session_directory_resolves_against_the_environments_current_directory() {
        let directory = tempfile::tempdir().unwrap();
        let current_dir = directory.path().join("current");
        let variable =
            |name: &str| (name == SESSION_DIR_VARIABLE).then(|| "relative-sessions".into());
        let environment = RootEnvironment::new(
            &variable,
            Some(directory.path().join("home")),
            Some(current_dir.clone()),
        );

        assert_eq!(
            session_root_in(&environment).unwrap(),
            flat_root(current_dir.join("relative-sessions"))
        );
    }

    #[test]
    fn an_empty_agent_directory_variable_keeps_pis_own_tree() {
        let home = tempfile::tempdir().unwrap();
        let home = Some(home.path().to_path_buf());

        assert_eq!(
            session_root_from(Some(PathBuf::new()), None, home.clone(), None).unwrap(),
            session_root_from(None, None, home, None).unwrap()
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

        let nested_files =
            crate::history::provider::walk::jsonl_files_at_depth(&nested, 1).unwrap();
        assert_eq!(nested_files.len(), 2);
        assert!(
            nested_files.iter().any(|path| {
                crate::history::format::parse_owned_transcript(Source::Pi, path)
                    .unwrap()
                    .is_some()
            }),
            "the Pi transcript written into the project directory was not discovered"
        );

        let flat = directory.path().join("flat");
        std::fs::create_dir_all(&flat).unwrap();
        std::fs::copy(
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/pi/v2.jsonl"),
            flat.join("flat.jsonl"),
        )
        .unwrap();
        assert_eq!(
            crate::history::provider::walk::jsonl_files_at_depth(&flat, 0)
                .unwrap()
                .len(),
            1
        );
    }
}
