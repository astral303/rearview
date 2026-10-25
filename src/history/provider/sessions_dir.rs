//! The directory an agent stores a working directory's sessions in.

use super::{Capability, RootEnvironment, SessionProvider};
use crate::error::Result;
use crate::history::workspace::candidate_directories;
use std::path::{Path, PathBuf};

/// The lookup a Capable
/// [`workspace_sessions_dir`](SessionProvider::workspace_sessions_dir)
/// declaration carries: the sessions directory the agent names for a working
/// directory, under the roots the environment resolves. The directory may not
/// exist.
pub type SessionsDirLookup = fn(&RootEnvironment, &Path) -> Result<PathBuf>;

/// The sessions directory of `provider` that holds sessions for `directory`:
/// the sessions directory of the first of [`candidate_directories`] that
/// holds a session the provider's discovery lists. `None` when none does, or
/// when the provider declares `workspace_sessions_dir` Not Capable.
pub fn found_sessions_dir(
    provider: &dyn SessionProvider,
    environment: &RootEnvironment,
    directory: &Path,
) -> Result<Option<PathBuf>> {
    let Capability::Capable(sessions_dir_of) = provider.workspace_sessions_dir() else {
        return Ok(None);
    };
    let mut existing = Vec::new();
    for candidate in candidate_directories(directory, environment.home()) {
        let sessions_dir = sessions_dir_of(environment, &candidate)?;
        if sessions_dir.is_dir() {
            existing.push(sessions_dir);
        }
    }
    if existing.is_empty() {
        return Ok(None);
    }
    let listed = listed_sessions(provider, environment)?;
    Ok(existing.into_iter().find(|sessions_dir| {
        listed
            .iter()
            .any(|session| session.starts_with(sessions_dir))
    }))
}

/// The locator of every session the provider's discovery lists under the
/// roots `environment` resolves.
fn listed_sessions(
    provider: &dyn SessionProvider,
    environment: &RootEnvironment,
) -> Result<Vec<PathBuf>> {
    let storage = provider.storage();
    let mut locators = Vec::new();
    for root in storage.roots_in(environment)? {
        locators.extend(
            storage
                .discover(&root)?
                .stubs
                .into_iter()
                .map(|stub| stub.locator),
        );
    }
    Ok(locators)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::Source;
    use crate::history::provider::claude::tests::ConfigDir;
    use crate::history::workspace::tests::link_worktree;

    fn found_for(claude: &ConfigDir, directory: &Path) -> Option<PathBuf> {
        let variable = claude.variable();
        let environment = RootEnvironment::new(&variable, claude.home(), None);
        found_sessions_dir(Source::Claude.provider(), &environment, directory).unwrap()
    }

    /// A main checkout with a `.git` directory, under the home directory of
    /// `claude`: the search for `.git` stops at home.
    fn repository_under(claude: &ConfigDir) -> PathBuf {
        let repository = claude.home().unwrap().join("repository");
        std::fs::create_dir_all(repository.join(".git")).unwrap();
        repository
    }

    #[test]
    fn subdirectory_finds_top_levels_sessions_directory() {
        let claude = ConfigDir::new();
        let repository = repository_under(&claude);
        let subdirectory = repository.join("src/bin");
        std::fs::create_dir_all(&subdirectory).unwrap();
        claude.write_session_recorded_in(&repository);

        assert_eq!(
            found_for(&claude, &subdirectory),
            Some(claude.project_folder(&repository))
        );
    }

    #[test]
    fn subdirectorys_own_sessions_directory_wins_over_top_levels() {
        let claude = ConfigDir::new();
        let repository = repository_under(&claude);
        let subdirectory = repository.join("src");
        std::fs::create_dir(&subdirectory).unwrap();
        claude.write_session_recorded_in(&repository);
        claude.write_session_recorded_in(&subdirectory);

        assert_eq!(
            found_for(&claude, &subdirectory),
            Some(claude.project_folder(&subdirectory))
        );
    }

    /// Claude Code writes `.session-aliases` into a worktree's project folder
    /// on `/add-dir`, even for a worktree it never ran in; that folder holds
    /// no session.
    #[test]
    fn linked_worktree_finds_main_checkouts_sessions_directory() {
        let claude = ConfigDir::new();
        let home = claude.home().unwrap();
        let main = home.join("main");
        let worktree = home.join("lane");
        link_worktree(&main, &worktree);
        claude.write_session_recorded_in(&main);

        assert_eq!(
            found_for(&claude, &worktree),
            Some(claude.project_folder(&main))
        );

        let worktree_folder = claude.project_folder(&worktree);
        std::fs::create_dir_all(&worktree_folder).unwrap();
        std::fs::write(
            worktree_folder.join(".session-aliases"),
            format!("{}\n", claude.project_folder(&main).display()),
        )
        .unwrap();

        assert_eq!(
            found_for(&claude, &worktree),
            Some(claude.project_folder(&main))
        );
    }

    #[test]
    fn linked_worktrees_own_sessions_directory_wins_over_main_checkouts() {
        let claude = ConfigDir::new();
        let home = claude.home().unwrap();
        let main = home.join("main");
        let worktree = home.join("lane");
        link_worktree(&main, &worktree);
        claude.write_session_recorded_in(&main);
        claude.write_session_recorded_in(&worktree);

        assert_eq!(
            found_for(&claude, &worktree),
            Some(claude.project_folder(&worktree))
        );
    }

    #[test]
    fn outside_repository_only_directorys_own_sessions_directory_counts() {
        let claude = ConfigDir::new();
        let parent = claude.home().unwrap().join("code");
        let directory = parent.join("project");
        std::fs::create_dir_all(&directory).unwrap();
        claude.write_session_recorded_in(&parent);

        assert_eq!(found_for(&claude, &directory), None);

        claude.write_session_recorded_in(&directory);

        assert_eq!(
            found_for(&claude, &directory),
            Some(claude.project_folder(&directory))
        );
    }
}
