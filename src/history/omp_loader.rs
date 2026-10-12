use super::pi_loader::{AGENT_DIR_VARIABLE, SESSION_DIR_VARIABLE, unset_if_empty};
use super::provider::walk::FileRoot;
use super::provider::{RootEnvironment, SessionRoot};
use crate::error::{AppError, Result};
use std::path::{Path, PathBuf};

/// Moves OMP's configuration directory, relative to the home directory.
const CONFIG_DIR_VARIABLE: &str = "PI_CONFIG_DIR";
const OMP_PROFILE_VARIABLE: &str = "OMP_PROFILE";
const PI_PROFILE_VARIABLE: &str = "PI_PROFILE";
/// Moves the data directory OMP prefers when it exists.
const DATA_HOME_VARIABLE: &str = "XDG_DATA_HOME";

/// Every environment variable [`session_root_in`] reads.
pub const STORAGE_ENV_VARS: &[&str] = &[
    CONFIG_DIR_VARIABLE,
    AGENT_DIR_VARIABLE,
    SESSION_DIR_VARIABLE,
    OMP_PROFILE_VARIABLE,
    PI_PROFILE_VARIABLE,
    DATA_HOME_VARIABLE,
];

/// Resolve OMP's session root together with its walk depth: a redirected
/// session directory holds transcripts beside itself, every other arrangement
/// holds them in a directory per project. Only this resolution knows which is
/// which, so the pairing is returned rather than guessed from the path later.
pub fn session_root() -> Result<FileRoot> {
    session_root_in(&RootEnvironment::of_process())
}

/// [`session_root`] as resolved from `environment`.
pub fn session_root_in(environment: &RootEnvironment) -> Result<FileRoot> {
    session_root_from(
        environment.var_os(CONFIG_DIR_VARIABLE).map(PathBuf::from),
        environment.var_os(AGENT_DIR_VARIABLE).map(PathBuf::from),
        environment.var_os(SESSION_DIR_VARIABLE).map(PathBuf::from),
        environment
            .var_os(OMP_PROFILE_VARIABLE)
            .or(environment.var_os(PI_PROFILE_VARIABLE)),
        environment.var_os(DATA_HOME_VARIABLE).map(PathBuf::from),
        environment.home().map(Path::to_path_buf),
        environment.current_dir().map(Path::to_path_buf),
    )
}

/// Resolve OMP's session root, and record whether it is OMP's own tree.
///
/// The two environment overrides point OMP somewhere the user chose, where Pi
/// may write too, so those roots stay redirected. The config tree and the XDG
/// data directory are OMP's own, and untitled transcripts in them are OMP's.
/// Only this function can tell the two apart — the resulting paths look alike,
/// and a home directory can itself be named `omp`.
pub(crate) fn session_root_from(
    config_dir: Option<PathBuf>,
    agent_override: Option<PathBuf>,
    session_override: Option<PathBuf>,
    profile: Option<std::ffi::OsString>,
    xdg_data_home: Option<PathBuf>,
    home_dir: Option<PathBuf>,
    cwd: Option<PathBuf>,
) -> Result<FileRoot> {
    let config_dir = unset_if_empty(config_dir);
    let agent_override = unset_if_empty(agent_override);
    let session_override = unset_if_empty(session_override);
    let home = home_dir.ok_or_else(AppError::home_not_found)?;
    let expand = |path| expand_path_with_home(path, &home, cwd.as_deref());
    if let Some(path) = session_override {
        return Ok(FileRoot {
            root: SessionRoot::new(expand(path)),
            depth: 0,
        });
    }

    let profile = profile
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
            .map(expand)
            .unwrap_or_else(|| default_agent.clone())
    } else {
        default_agent.clone()
    };
    if agent_dir != default_agent {
        return Ok(FileRoot {
            root: SessionRoot::new(agent_dir.join("sessions")),
            depth: 1,
        });
    }

    let xdg_profile_root = xdg_data_home.map(|root| {
        let root = root.join("omp");
        profile
            .map(|profile| root.join("profiles").join(profile))
            .unwrap_or(root)
    });
    let path = xdg_profile_root
        .filter(|root| root.exists())
        .unwrap_or(agent_dir)
        .join("sessions");
    Ok(FileRoot {
        root: SessionRoot::new(path).in_agent_tree(),
        depth: 1,
    })
}

fn expand_path_with_home(path: PathBuf, home: &Path, cwd: Option<&Path>) -> PathBuf {
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
        cwd.unwrap_or(Path::new(".")).join(expanded)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::provider::RootOrigin;

    #[test]
    fn resolves_default_profile_overrides_and_xdg_roots() {
        let home = tempfile::tempdir().unwrap();
        let default = session_root_from(
            None,
            None,
            None,
            None,
            None,
            Some(home.path().to_path_buf()),
            None,
        )
        .unwrap();
        assert_eq!(
            default,
            FileRoot {
                root: SessionRoot::new(home.path().join(".omp/agent/sessions")).in_agent_tree(),
                depth: 1,
            }
        );

        let profile = session_root_from(
            None,
            Some(PathBuf::from("~/ignored")),
            None,
            Some("work".into()),
            None,
            Some(home.path().to_path_buf()),
            None,
        )
        .unwrap();
        assert_eq!(
            profile.root.path,
            home.path().join(".omp/profiles/work/agent/sessions")
        );

        let xdg = home.path().join("xdg/omp");
        std::fs::create_dir_all(&xdg).unwrap();
        let xdg_root = session_root_from(
            None,
            None,
            None,
            None,
            Some(home.path().join("xdg")),
            Some(home.path().to_path_buf()),
            None,
        )
        .unwrap();
        assert_eq!(xdg_root.root.path, xdg.join("sessions"));
        assert_eq!(xdg_root.root.origin(), RootOrigin::AgentTree);

        let custom = session_root_from(
            None,
            None,
            Some(PathBuf::from("~/sessions")),
            None,
            None,
            Some(home.path().to_path_buf()),
            None,
        )
        .unwrap();
        assert_eq!(
            custom,
            FileRoot {
                root: SessionRoot::new(home.path().join("sessions")),
                depth: 0,
            }
        );
    }

    /// The old test for this asked whether the path had a component called `omp`,
    /// which a home directory can supply on its own.
    #[test]
    fn a_home_directory_named_omp_does_not_make_a_redirected_root_omps() {
        let parent = tempfile::tempdir().unwrap();
        let home = parent.path().join("omp");
        std::fs::create_dir_all(&home).unwrap();

        for redirect in [
            // PI_CODING_AGENT_SESSION_DIR
            session_root_from(
                None,
                None,
                Some(PathBuf::from("~/shared-sessions")),
                None,
                None,
                Some(home.clone()),
                None,
            ),
            // PI_CODING_AGENT_DIR
            session_root_from(
                None,
                Some(PathBuf::from("~/shared-agent")),
                None,
                None,
                None,
                Some(home.clone()),
                None,
            ),
        ] {
            let root = redirect.unwrap().root;
            assert_eq!(
                root.origin(),
                RootOrigin::Redirected,
                "{} was chosen by the user, so Pi may write there too",
                root.path.display()
            );
        }

        assert_eq!(
            session_root_from(None, None, None, None, None, Some(home), None)
                .unwrap()
                .root
                .origin(),
            RootOrigin::AgentTree
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
            FileRoot {
                root: SessionRoot::new(current_dir.join("relative-sessions")),
                depth: 0,
            }
        );
    }

    #[test]
    fn an_empty_config_or_agent_directory_variable_keeps_omps_own_tree() {
        let home = tempfile::tempdir().unwrap();
        let home = Some(home.path().to_path_buf());
        let default = session_root_from(None, None, None, None, None, home.clone(), None).unwrap();

        let empty_config_dir = session_root_from(
            Some(PathBuf::new()),
            None,
            None,
            None,
            None,
            home.clone(),
            None,
        );
        let empty_agent_dir =
            session_root_from(None, Some(PathBuf::new()), None, None, None, home, None);

        assert_eq!(empty_config_dir.unwrap(), default, "PI_CONFIG_DIR");
        assert_eq!(empty_agent_dir.unwrap(), default, "PI_CODING_AGENT_DIR");
    }

    #[test]
    fn a_profile_root_stays_omps_own_tree_even_beside_an_agent_override() {
        let home = tempfile::tempdir().unwrap();
        let profile = session_root_from(
            None,
            Some(PathBuf::from("~/ignored")),
            None,
            Some("work".into()),
            None,
            Some(home.path().to_path_buf()),
            None,
        )
        .unwrap();

        assert_eq!(profile.root.origin(), RootOrigin::AgentTree);
    }
}
