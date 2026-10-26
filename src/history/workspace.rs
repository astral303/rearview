//! Scoping sessions to the repository holding a directory, or outside a
//! repository to the directory alone.

use super::Conversation;
use super::provider::RootEnvironment;
use std::path::{Component, Path, PathBuf};

/// The directories to scope sessions to: every checkout of the repository
/// holding a directory, or outside a repository the directory alone.
#[derive(Clone, Debug)]
pub struct Workspace {
    /// Spelled as the caller and git spell them: an agent that names a
    /// folder after the directory it ran in names it after this spelling.
    directories: Vec<PathBuf>,
    /// The `directories` that exist, canonicalized, so a session that
    /// recorded one through a symlink still matches.
    canonical_directories: Vec<PathBuf>,
    reach: Reach,
}

/// The recorded directories each of a workspace's directories includes.
#[derive(Clone, Copy, Debug)]
enum Reach {
    /// Outside a repository: the directory itself.
    Itself,
    /// A repository's checkout: the checkout and every directory inside it.
    Inside,
}

impl Reach {
    fn includes(self, directory: &Path, recorded: &Path) -> bool {
        match self {
            Self::Itself => recorded == directory,
            Self::Inside => recorded.starts_with(directory),
        }
    }
}

impl Workspace {
    pub fn at(directory: PathBuf) -> Self {
        Self::in_environment(directory, &RootEnvironment::of_process())
    }

    /// The workspace of `directory`, with the home directory `environment`
    /// names. Reads the repository's checkouts once, from its `.git` files.
    pub(crate) fn in_environment(directory: PathBuf, environment: &RootEnvironment) -> Self {
        match Repository::holding(&directory, environment.home()) {
            Some(repository) => Self::of(repository.checkouts(), Reach::Inside),
            None => Self::of(vec![directory], Reach::Itself),
        }
    }

    fn of(directories: Vec<PathBuf>, reach: Reach) -> Self {
        let canonical_directories = directories
            .iter()
            .filter_map(|directory| directory.canonicalize().ok())
            .collect();
        Self {
            directories,
            canonical_directories,
            reach,
        }
    }

    /// The checkouts' top levels, or outside a repository the directory, as
    /// the caller and git spell them.
    pub fn directories(&self) -> &[PathBuf] {
        &self.directories
    }

    /// True when `conversation` was recorded in this workspace, by the rule
    /// of the agent that recorded it.
    pub fn contains(&self, conversation: &Conversation) -> bool {
        conversation.source.provider().is_in_workspace(
            self,
            &conversation.path,
            conversation.cwd.as_deref(),
        )
    }

    /// True when `recorded` is one of the workspace's directories or, in a
    /// repository, inside a checkout, however each was spelled.
    pub fn includes(&self, recorded: &Path) -> bool {
        let is_included = |recorded: &Path, directories: &[PathBuf]| {
            directories
                .iter()
                .any(|directory| self.reach.includes(directory, recorded))
        };
        is_included(recorded, &self.directories)
            || recorded
                .canonicalize()
                .is_ok_and(|canonical| is_included(&canonical, &self.canonical_directories))
    }
}

/// The directories whose sessions belong to `directory`, nearest first:
/// `directory`, each parent up to the repository's top level, and from a
/// linked worktree the main checkout's top level. Outside a repository,
/// `directory` alone: walking further up would reach the home directory's
/// sessions.
pub fn candidate_directories(directory: &Path, home: Option<&Path>) -> Vec<PathBuf> {
    let Some(repository) = Repository::holding(directory, home) else {
        return vec![directory.to_path_buf()];
    };
    let mut candidates: Vec<PathBuf> = directory
        .ancestors()
        .take_while(|ancestor| *ancestor != repository.top_level)
        .map(Path::to_path_buf)
        .collect();
    candidates.push(repository.top_level.clone());
    candidates.extend(
        repository
            .main_checkout()
            .filter(|main_checkout| *main_checkout != repository.top_level),
    );
    candidates
}

/// A git repository, read from its `.git` files without running git.
struct Repository {
    /// The nearest directory holding a `.git` directory or file.
    top_level: PathBuf,
    /// The git directory the repository's checkouts share. `None` when the
    /// `.git` file names no git directory.
    common_dir: Option<PathBuf>,
}

impl Repository {
    /// The repository `directory` is in, `None` outside one. The search for
    /// `.git` stops below `home`: a `.git` there, such as a dotfiles
    /// repository's, would make every directory under home one repository.
    fn holding(directory: &Path, home: Option<&Path>) -> Option<Self> {
        let top_level = directory
            .ancestors()
            .take_while(|ancestor| Some(*ancestor) != home)
            .find(|ancestor| ancestor.join(".git").exists())?;
        Some(Self {
            top_level: top_level.to_path_buf(),
            common_dir: common_dir_of(top_level),
        })
    }

    /// The main checkout's top level. `None` for a bare repository or a
    /// submodule: no checkout's `.git` directory holds their git directory.
    fn main_checkout(&self) -> Option<PathBuf> {
        checkout_of_git_dir(self.common_dir.as_deref()?)
    }

    /// The top level of this checkout, the main checkout and every linked
    /// worktree.
    fn checkouts(&self) -> Vec<PathBuf> {
        let mut checkouts = vec![self.top_level.clone()];
        for checkout in self
            .main_checkout()
            .into_iter()
            .chain(self.linked_worktrees())
        {
            if !checkouts.contains(&checkout) {
                checkouts.push(checkout);
            }
        }
        checkouts
    }

    /// The top level of every linked worktree git records: its
    /// `<common dir>/worktrees/<name>/gitdir` names the worktree's `.git`
    /// file. A deleted worktree is recorded while `git worktree list` still
    /// lists it.
    fn linked_worktrees(&self) -> Vec<PathBuf> {
        let Some(common_dir) = &self.common_dir else {
            return Vec::new();
        };
        let Ok(entries) = std::fs::read_dir(common_dir.join("worktrees")) else {
            return Vec::new();
        };
        let mut worktrees: Vec<PathBuf> = entries
            .flatten()
            .filter_map(|entry| {
                let worktree_git_dir = entry.path();
                let git_file = std::fs::read_to_string(worktree_git_dir.join("gitdir")).ok()?;
                let git_file =
                    lexically_normal(&worktree_git_dir.join(git_file.lines().next()?.trim()));
                git_file.parent().map(Path::to_path_buf)
            })
            .collect();
        worktrees.sort();
        worktrees
    }
}

/// The git directory shared by the checkouts of the repository whose top
/// level is `top_level`: its `.git` directory, or the git directory its
/// `.git` file names, through that directory's `commondir` for a linked
/// worktree. A submodule's git directory has no `commondir`.
fn common_dir_of(top_level: &Path) -> Option<PathBuf> {
    let dot_git = top_level.join(".git");
    if dot_git.is_dir() {
        return Some(dot_git);
    }
    let git_file = std::fs::read_to_string(&dot_git).ok()?;
    let git_dir = top_level.join(git_file.lines().next()?.strip_prefix("gitdir:")?.trim());
    let common_dir = match std::fs::read_to_string(git_dir.join("commondir")) {
        Ok(common_dir) => git_dir.join(common_dir.trim()),
        Err(_) => git_dir,
    };
    Some(lexically_normal(&common_dir))
}

/// The checkout whose `.git` directory is `git_dir`. `None` for a bare
/// repository: its git directory is not named `.git`, and no checkout holds
/// it.
fn checkout_of_git_dir(git_dir: &Path) -> Option<PathBuf> {
    if git_dir.file_name()? != ".git" {
        return None;
    }
    git_dir.parent().map(Path::to_path_buf)
}

/// `path` with `.` and `..` components resolved by their names, and its
/// separators in the platform's spelling.
fn lexically_normal(path: &Path) -> PathBuf {
    let mut normal = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normal.pop();
            }
            other => normal.push(other),
        }
    }
    normal
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::history::Source;
    use crate::history::provider::claude::convert_path_to_project_dir_name;
    use crate::search::test_fixtures::one_message_conversation;
    use std::ffi::OsString;
    use strum::IntoEnumIterator;

    /// A home directory in a temporary directory. The search for `.git`
    /// from a directory under it stops at it, before the host's own
    /// directories.
    pub(crate) struct TestHome(tempfile::TempDir);

    impl TestHome {
        pub(crate) fn new() -> Self {
            Self(tempfile::tempdir().unwrap())
        }

        pub(crate) fn path(&self) -> &Path {
            self.0.path()
        }

        pub(crate) fn workspace(&self, directory: &Path) -> Workspace {
            workspace_with_home(directory, self.path())
        }

        pub(crate) fn candidate_directories(&self, directory: &Path) -> Vec<PathBuf> {
            candidate_directories(directory, Some(self.path()))
        }

        /// A main checkout with a `.git` directory, at `project` under home.
        pub(crate) fn repository(&self) -> PathBuf {
            let repository = self.path().join("project");
            std::fs::create_dir_all(repository.join(".git")).unwrap();
            repository
        }
    }

    /// The workspace of `directory` as the home directory, which is outside
    /// any repository: nothing is searched for `.git`.
    pub(crate) fn workspace_at_home(directory: &Path) -> Workspace {
        workspace_with_home(directory, directory)
    }

    fn workspace_with_home(directory: &Path, home: &Path) -> Workspace {
        let no_variable = |_: &str| -> Option<OsString> { None };
        let environment = RootEnvironment::new(&no_variable, Some(home.to_path_buf()), None);
        Workspace::in_environment(directory.to_path_buf(), &environment)
    }

    fn session(source: Source, path: &Path, cwd: Option<&Path>) -> Conversation {
        let mut conversation =
            one_message_conversation("hello", chrono::Local::now(), None, None, None);
        conversation.source = source;
        conversation.path = path.to_path_buf();
        conversation.cwd = cwd.map(Path::to_path_buf);
        conversation
    }

    /// A Claude session in the project folder Claude Code names for
    /// `project_dir`, recording no directory.
    fn claude_session_under(project_dir: &Path) -> Conversation {
        session(Source::Claude, &claude_transcript_under(project_dir), None)
    }

    fn claude_transcript_under(project_dir: &Path) -> PathBuf {
        Path::new("/claude/projects")
            .join(convert_path_to_project_dir_name(project_dir))
            .join("session.jsonl")
    }

    fn every_agent() -> Vec<Source> {
        Source::iter().collect()
    }

    fn no_agent() -> Vec<Source> {
        Vec::new()
    }

    /// The agents whose session recorded in `recorded` is in `workspace`.
    fn agents_whose_session_is_in(workspace: &Workspace, recorded: &Path) -> Vec<Source> {
        Source::iter()
            .filter(|source| {
                workspace.contains(&session(
                    *source,
                    Path::new("/sessions/session.jsonl"),
                    Some(recorded),
                ))
            })
            .collect()
    }

    /// A linked worktree of `main` at `worktree`, as `git worktree add`
    /// records it: Git for Windows writes both paths with forward slashes.
    pub(crate) fn link_worktree(main: &Path, worktree: &Path) {
        let worktree_git_dir = main
            .join(".git")
            .join("worktrees")
            .join(worktree.file_name().unwrap());
        std::fs::create_dir_all(&worktree_git_dir).unwrap();
        std::fs::write(worktree_git_dir.join("commondir"), "../..\n").unwrap();
        std::fs::write(
            worktree_git_dir.join("gitdir"),
            format!("{}\n", forward_slashed(&worktree.join(".git"))),
        )
        .unwrap();
        std::fs::create_dir_all(worktree).unwrap();
        std::fs::write(
            worktree.join(".git"),
            format!("gitdir: {}\n", forward_slashed(&worktree_git_dir)),
        )
        .unwrap();
    }

    fn forward_slashed(path: &Path) -> String {
        path.display().to_string().replace('\\', "/")
    }

    #[test]
    fn from_subdirectory_repositorys_sessions_are_in_workspace() {
        let home = TestHome::new();
        let repository = home.repository();
        let subdirectory = repository.join("target").join("debug");
        std::fs::create_dir_all(&subdirectory).unwrap();

        let workspace = home.workspace(&subdirectory);

        assert_eq!(
            agents_whose_session_is_in(&workspace, &repository),
            every_agent()
        );
    }

    #[test]
    fn session_recorded_in_subdirectory_is_in_top_levels_workspace() {
        let home = TestHome::new();
        let repository = home.repository();

        let workspace = home.workspace(&repository);

        assert_eq!(
            agents_whose_session_is_in(&workspace, &repository.join("src").join("bin")),
            every_agent()
        );
    }

    #[test]
    fn main_checkout_and_its_linked_worktrees_share_one_workspace() {
        let home = TestHome::new();
        let main = home.path().join("project");
        let lane = home.path().join("project-lane");
        let other_lane = home.path().join("project__worktrees").join("other");
        link_worktree(&main, &lane);
        link_worktree(&main, &other_lane);

        let from_lane = home.workspace(&lane);
        let from_main = home.workspace(&main);

        assert_eq!(agents_whose_session_is_in(&from_lane, &main), every_agent());
        assert_eq!(
            agents_whose_session_is_in(&from_lane, &other_lane),
            every_agent()
        );
        assert_eq!(
            agents_whose_session_is_in(&from_main, &other_lane.join("src")),
            every_agent()
        );
    }

    #[test]
    fn deleted_worktree_inside_main_checkout_stays_in_its_workspace() {
        let home = TestHome::new();
        let repository = home.repository();

        let workspace = home.workspace(&repository);

        for deleted in [".worktrees/fix", ".claude/worktrees/fix"] {
            assert_eq!(
                agents_whose_session_is_in(&workspace, &repository.join(deleted)),
                every_agent(),
                "{deleted}"
            );
        }
    }

    /// `rm -rf` leaves the worktree's record in the main checkout's git
    /// directory, and `git worktree prune` removes it.
    #[test]
    fn deleted_sibling_worktree_leaves_workspace_once_git_prunes_it() {
        let home = TestHome::new();
        let main = home.path().join("project");
        let lane = home.path().join("project-lane");
        link_worktree(&main, &lane);
        std::fs::remove_dir_all(&lane).unwrap();

        assert_eq!(
            agents_whose_session_is_in(&home.workspace(&main), &lane),
            every_agent()
        );

        std::fs::remove_dir_all(main.join(".git").join("worktrees")).unwrap();

        assert_eq!(
            agents_whose_session_is_in(&home.workspace(&main), &lane),
            no_agent()
        );
    }

    #[test]
    fn outside_repository_only_directory_itself_is_workspace() {
        let home = TestHome::new();
        let parent = home.path().join("code");
        let directory = parent.join("project");
        std::fs::create_dir_all(&directory).unwrap();

        let workspace = home.workspace(&directory);

        assert_eq!(
            agents_whose_session_is_in(&workspace, &directory),
            every_agent()
        );
        assert_eq!(
            agents_whose_session_is_in(&workspace, &directory.join("src")),
            no_agent()
        );
        assert_eq!(agents_whose_session_is_in(&workspace, &parent), no_agent());
    }

    /// A dotfiles repository puts a `.git` directory in the home directory.
    #[test]
    fn git_directory_in_home_makes_no_repository() {
        let home = TestHome::new();
        std::fs::create_dir(home.path().join(".git")).unwrap();
        let project = home.path().join("project");
        std::fs::create_dir(&project).unwrap();

        let workspace = home.workspace(&project);

        assert_eq!(
            agents_whose_session_is_in(&workspace, &project),
            every_agent()
        );
        assert_eq!(
            agents_whose_session_is_in(&workspace, &project.join("src")),
            no_agent()
        );
        assert_eq!(
            agents_whose_session_is_in(&workspace, home.path()),
            no_agent()
        );
        assert_eq!(home.candidate_directories(&project), [project]);
    }

    /// `<home>/project-lane` encodes to the repository's folder name followed
    /// by `-lane`, as `<home>/project/lane` would.
    #[test]
    fn sibling_claude_folder_sharing_repositorys_name_prefix_is_not_in_workspace() {
        let home = TestHome::new();
        let repository = home.repository();
        let sibling = home.path().join("project-lane");

        let workspace = home.workspace(&repository);

        assert!(!workspace.contains(&claude_session_under(&sibling)));
    }

    /// A Claude Code session in a checkout's project folder is in the
    /// workspace whatever directory it recorded: none, or for rearview's
    /// cross-project resume or fork copy, the directory of the original.
    #[test]
    fn claude_session_in_checkouts_project_folder_is_in_workspace() {
        let home = TestHome::new();
        let repository = home.repository();
        let claude_worktree = repository.join(".claude/worktrees/fix-search");
        link_worktree(&repository, &claude_worktree);

        let workspace = home.workspace(&repository);

        assert!(workspace.contains(&claude_session_under(&repository)));
        assert!(workspace.contains(&claude_session_under(&claude_worktree)));
        assert!(workspace.contains(&session(
            Source::Claude,
            &claude_transcript_under(&repository),
            Some(Path::new("/elsewhere")),
        )));
    }

    /// Git records none of these worktrees. workmux's older sibling layout,
    /// `<checkout>__worktrees/<branch>`, encodes as its `.worktrees/` does.
    #[test]
    fn claude_session_without_directory_in_deleted_worktrees_folder_is_in_workspace() {
        let home = TestHome::new();
        let repository = home.repository();

        let workspace = home.workspace(&repository);

        for deleted in [
            repository.join(".worktrees/fix"),
            repository.join(".claude/worktrees/fix"),
            home.path().join("project__worktrees/fix"),
        ] {
            assert!(
                workspace.contains(&claude_session_under(&deleted)),
                "{}",
                deleted.display()
            );
        }
    }

    #[test]
    fn subdirectorys_candidates_run_up_to_top_level() {
        let home = TestHome::new();
        let repository = home.repository();
        let subdirectory = repository.join("src").join("bin");
        std::fs::create_dir_all(&subdirectory).unwrap();

        assert_eq!(
            home.candidate_directories(&subdirectory),
            vec![subdirectory.clone(), repository.join("src"), repository]
        );
    }

    #[test]
    fn linked_worktrees_candidates_end_with_main_checkout() {
        let home = TestHome::new();
        let main = home.path().join("main");
        let worktree = home.path().join("lane");
        link_worktree(&main, &worktree);

        assert_eq!(home.candidate_directories(&worktree), vec![worktree, main]);
    }

    /// A submodule's `.git` file names its git directory inside the
    /// superproject's, which holds no `commondir`.
    #[test]
    fn submodules_candidates_end_at_its_own_top_level() {
        let home = TestHome::new();
        let superproject = home.repository();
        let module_git_dir = superproject.join(".git").join("modules").join("lib");
        std::fs::create_dir_all(&module_git_dir).unwrap();
        let submodule = superproject.join("lib");
        std::fs::create_dir(&submodule).unwrap();
        std::fs::write(
            submodule.join(".git"),
            format!("gitdir: {}\n", module_git_dir.display()),
        )
        .unwrap();

        assert_eq!(home.candidate_directories(&submodule), vec![submodule]);
    }

    #[test]
    fn outside_repository_directory_is_only_candidate() {
        let home = TestHome::new();
        let directory = home.path().join("project");
        std::fs::create_dir(&directory).unwrap();

        assert_eq!(home.candidate_directories(&directory), vec![directory]);
    }

    #[test]
    fn claude_session_from_another_project_is_not() {
        let workspace = workspace_at_home(Path::new("/Users/raine/code/project"));
        let session = claude_session_under(Path::new("/Users/raine/code/elsewhere"));

        assert!(!workspace.contains(&session));
    }

    #[test]
    fn another_agents_session_is_in_workspace_it_recorded() {
        let workspace = workspace_at_home(Path::new("/Users/raine/code/project"));
        let rollout = Path::new("/codex/rollout.jsonl");

        assert!(workspace.contains(&session(
            Source::Codex,
            rollout,
            Some(Path::new("/Users/raine/code/project")),
        )));
        assert!(!workspace.contains(&session(
            Source::Codex,
            rollout,
            Some(Path::new("/Users/raine/elsewhere")),
        )));
        assert!(!workspace.contains(&session(Source::Codex, rollout, None)));
    }
}
