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

/// Resolve Pi's session root together with its walk depth: a session
/// directory override holds transcripts directly inside it, every other root
/// holds them in a directory per working directory. Only this resolution
/// knows which is which, so the pairing is returned rather than guessed from
/// the path later.
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
        return Ok(session_directory_override(expand(value)?));
    }

    let global_setting = configured_session_dir(&agent_dir.join("settings.json"));
    let project_setting = cwd
        .as_deref()
        .and_then(|cwd| configured_session_dir(&cwd.join(".pi/settings.json")));
    if let Some(Some(value)) = project_setting.or(global_setting) {
        return Ok(session_directory_override(expand(value)?));
    }

    let sessions = SessionRoot::new(agent_dir.join("sessions"));
    let root = if redirected_agent_dir {
        sessions
    } else {
        sessions.in_agent_tree()
    };
    Ok(FileRoot { root, depth: 1 })
}

/// The directory under `root` Pi writes the sessions recorded in `directory`
/// to: a session directory override itself, else the
/// [`whole_path_dir_name`] of `directory` (`getDefaultSessionDirPath` in
/// Pi's `session-manager.ts`).
pub(crate) fn sessions_dir_under(root: &FileRoot, directory: &Path) -> PathBuf {
    if is_session_directory_override(root) {
        return root.root.path.clone();
    }
    root.root
        .path
        .join(whole_path_dir_name(&directory.to_string_lossy()))
}

/// True for a root Pi or OMP resolved from a session directory override: its
/// sessions sit directly inside it, at depth 0.
pub(super) fn is_session_directory_override(root: &FileRoot) -> bool {
    root.depth == 0
}

/// `path` without one leading separator and with its separators as dashes,
/// between `--` and `--`. Pi names every working directory's sessions
/// directory this way; OMP names one outside the home and temporary
/// directories this way.
pub(super) fn whole_path_dir_name(path: &str) -> String {
    let path = path.strip_prefix(['/', '\\']).unwrap_or(path);
    format!("--{}--", with_separators_as_dashes(path))
}

/// `path` with every `/`, `\` and `:` replaced by `-`.
pub(super) fn with_separators_as_dashes(path: &str) -> String {
    path.chars()
        .map(|character| match character {
            '/' | '\\' | ':' => '-',
            other => other,
        })
        .collect()
}

/// Pi and OMP skip a directory variable that is set but empty; a value of
/// spaces is a path to them.
pub(super) fn unset_if_empty(variable: Option<PathBuf>) -> Option<PathBuf> {
    variable.filter(|path| !path.as_os_str().is_empty())
}

/// The root `PI_CODING_AGENT_SESSION_DIR` or `sessionDir` names: every
/// working directory's sessions sit directly inside it.
fn session_directory_override(path: PathBuf) -> FileRoot {
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
        assert_eq!(
            settings,
            session_directory_override(home.path().join("settings-sessions"))
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
            Some(project.clone()),
        )
        .unwrap();
        assert_eq!(
            project_settings,
            session_directory_override(project.join("project-sessions"))
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
            session_directory_override(home.path().join("environment-sessions"))
        );
    }

    #[test]
    fn relative_session_directory_resolves_against_environments_current_directory() {
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
            session_directory_override(current_dir.join("relative-sessions"))
        );
    }

    /// The first name is in Pi's own test fixtures; the rest follow its
    /// encoding by hand.
    #[test]
    fn working_directory_gets_directory_pi_names_for_it() {
        let root = FileRoot {
            root: SessionRoot::new("/sessions"),
            depth: 1,
        };
        for (directory, name) in [
            (
                "/Users/badlogic/workspaces/pi-mono",
                "--Users-badlogic-workspaces-pi-mono--",
            ),
            (r"C:\Users\me\proj", "--C--Users-me-proj--"),
            (r"\\server\share\dir", "---server-share-dir--"),
            ("/", "----"),
        ] {
            assert_eq!(
                sessions_dir_under(&root, Path::new(directory)),
                Path::new("/sessions").join(name),
                "{directory}"
            );
        }
    }

    #[test]
    fn session_directory_override_holds_every_working_directorys_sessions() {
        assert_eq!(
            sessions_dir_under(
                &session_directory_override(PathBuf::from("/shared")),
                Path::new("/srv/app")
            ),
            Path::new("/shared")
        );
    }

    #[test]
    fn empty_agent_directory_variable_keeps_pis_own_tree() {
        let home = tempfile::tempdir().unwrap();
        let home = Some(home.path().to_path_buf());

        assert_eq!(
            session_root_from(Some(PathBuf::new()), None, home.clone(), None).unwrap(),
            session_root_from(None, None, home, None).unwrap()
        );
    }

    #[test]
    fn discovers_session_directory_override_and_default_tree_and_validates_headers() {
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

        let session_directory = directory.path().join("session-directory-override");
        std::fs::create_dir_all(&session_directory).unwrap();
        std::fs::copy(
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/pi/v2.jsonl"),
            session_directory.join("session.jsonl"),
        )
        .unwrap();
        assert_eq!(
            crate::history::provider::walk::jsonl_files_at_depth(&session_directory, 0)
                .unwrap()
                .len(),
            1
        );
    }
}
