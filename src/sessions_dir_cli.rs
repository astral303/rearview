//! `--show-dir` and `--show-all-dirs`: the directory an agent stores the
//! current directory's sessions in.

use crate::error::{AppError, Result};
use crate::history::provider::{
    RootEnvironment, SessionProvider, SourceLabels, found_sessions_dir, providers,
};
use std::io::Write;
use std::path::{Path, PathBuf};

/// Separates the agent from the path in a `--show-all-dirs` line. A bare `:`
/// would also split a Windows path at its drive letter.
const AGENT_PATH_SEPARATOR: &str = ": ";

/// The sessions directory of `provider` that holds sessions for `directory`.
pub(crate) fn agent_sessions_dir(
    provider: &dyn SessionProvider,
    environment: &RootEnvironment,
    directory: &Path,
) -> Result<PathBuf> {
    found_sessions_dir(provider, environment, directory)?.ok_or_else(|| {
        AppError::AgentStoresNoSessions {
            agent: provider.labels().display,
            directory: directory.display().to_string(),
        }
    })
}

/// Print an `<agent>: <path>` line to `out` for each agent storing sessions
/// for `directory`, in registration order. An agent whose lookup fails gets
/// a warning on `warnings`, and the others still print.
pub(crate) fn print_all_sessions_dirs(
    environment: &RootEnvironment,
    directory: &Path,
    out: &mut impl Write,
    warnings: &mut impl Write,
) -> Result<()> {
    let found = providers().iter().map(|provider| {
        (
            provider.labels(),
            found_sessions_dir(*provider, environment, directory),
        )
    });
    print_found(found, directory, out, warnings)
}

/// Print each found sessions directory, and warn of each failed lookup.
/// Fails when no line printed.
fn print_found(
    found: impl IntoIterator<Item = (SourceLabels, Result<Option<PathBuf>>)>,
    directory: &Path,
    out: &mut impl Write,
    warnings: &mut impl Write,
) -> Result<()> {
    let mut printed = false;
    for (labels, lookup) in found {
        match lookup {
            Ok(Some(sessions_dir)) => {
                writeln!(
                    out,
                    "{}{AGENT_PATH_SEPARATOR}{}",
                    labels.name,
                    sessions_dir.display()
                )?;
                printed = true;
            }
            Ok(None) => {}
            Err(error) => writeln!(
                warnings,
                "Warning: could not look up {}'s sessions directory: {error}",
                labels.display
            )?,
        }
    }
    if printed {
        Ok(())
    } else {
        Err(AppError::NoAgentStoresSessions(
            directory.display().to_string(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::Source;
    use crate::history::provider::claude::tests::ConfigDir;

    /// What `--show-all-dirs` printed to standard output and standard error.
    fn show_all_dirs(claude: &ConfigDir, directory: &Path) -> (Result<()>, String, String) {
        let variable = claude.variable();
        let environment = RootEnvironment::new(&variable, claude.home(), None);
        let (mut out, mut warnings) = (Vec::new(), Vec::new());
        let printed = print_all_sessions_dirs(&environment, directory, &mut out, &mut warnings);
        (
            printed,
            String::from_utf8(out).unwrap(),
            String::from_utf8(warnings).unwrap(),
        )
    }

    fn show_dir(claude: &ConfigDir, directory: &Path) -> Result<PathBuf> {
        let variable = claude.variable();
        let environment = RootEnvironment::new(&variable, claude.home(), None);
        agent_sessions_dir(Source::Claude.provider(), &environment, directory)
    }

    /// A directory named `name` under the home directory of `claude`: the
    /// search for `.git` stops at home.
    fn directory_under(claude: &ConfigDir, name: &str) -> PathBuf {
        let directory = claude.home().unwrap().join(name);
        std::fs::create_dir_all(&directory).unwrap();
        directory
    }

    #[test]
    fn show_all_dirs_prints_the_agent_a_colon_a_space_and_the_path() {
        let claude = ConfigDir::new();
        let working_dir = directory_under(&claude, "work");
        claude.write_session_recorded_in(&working_dir);

        let (printed, out, warnings) = show_all_dirs(&claude, &working_dir);

        printed.unwrap();
        assert_eq!(
            out,
            format!(
                "claude: {}\n",
                claude.project_folder(&working_dir).display()
            )
        );
        assert_eq!(warnings, "");
    }

    #[test]
    fn show_all_dirs_fails_when_no_agent_stores_sessions_for_the_directory() {
        let claude = ConfigDir::new();
        let working_dir = directory_under(&claude, "work");
        let elsewhere = directory_under(&claude, "elsewhere");
        claude.write_session_recorded_in(&elsewhere);

        let (printed, out, _) = show_all_dirs(&claude, &working_dir);

        assert!(matches!(printed, Err(AppError::NoAgentStoresSessions(_))));
        assert_eq!(out, "");
    }

    #[test]
    fn an_agent_whose_lookup_fails_is_warned_of_and_the_others_still_print() {
        let (mut out, mut warnings) = (Vec::new(), Vec::new());
        let found = [
            (
                Source::Claude.provider().labels(),
                Err(AppError::ConfigError("unreadable".to_owned())),
            ),
            (
                Source::Pi.provider().labels(),
                Ok(Some(PathBuf::from("/sessions/--srv-app--"))),
            ),
        ];

        print_found(found, Path::new("/srv/app"), &mut out, &mut warnings).unwrap();

        assert_eq!(
            String::from_utf8(out).unwrap(),
            "pi: /sessions/--srv-app--\n"
        );
        assert_eq!(
            String::from_utf8(warnings).unwrap(),
            "Warning: could not look up Claude's sessions directory: Configuration error: unreadable\n"
        );
    }

    #[test]
    fn show_dir_prints_the_agents_sessions_directory() {
        let claude = ConfigDir::new();
        let working_dir = directory_under(&claude, "work");
        claude.write_session_recorded_in(&working_dir);

        assert_eq!(
            show_dir(&claude, &working_dir).unwrap(),
            claude.project_folder(&working_dir)
        );
    }

    #[test]
    fn show_dir_names_an_agent_storing_no_sessions_for_the_directory() {
        let claude = ConfigDir::new();
        let working_dir = directory_under(&claude, "work");
        let elsewhere = directory_under(&claude, "elsewhere");
        claude.write_session_recorded_in(&elsewhere);

        let error = show_dir(&claude, &working_dir).unwrap_err();

        assert!(
            error
                .to_string()
                .starts_with("Claude stores no sessions for"),
            "{error}"
        );
    }
}
