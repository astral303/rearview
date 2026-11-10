//! Provider session locations.

use super::walk::{Canonicaliser, SystemCanonicaliser};
use crate::history::path::{CanonicalDirectories, PathBelowRoot, with_platform_separators};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

/// The environment variables and the home, current and temporary
/// directories a provider resolves its roots from, each spelled with the
/// platform's separator. Tests build their own, since a parallel test run
/// cannot set the process environment.
pub struct RootEnvironment<'a> {
    variable: &'a dyn Fn(&str) -> Option<OsString>,
    home: Option<PathBuf>,
    current_dir: Option<PathBuf>,
    temp_dir: Option<PathBuf>,
    /// The memo this environment canonicalises directories through: the
    /// command's, when an [`Environment`](crate::history::Environment) made
    /// for one command made this environment.
    directories: Arc<CanonicalDirectories>,
}

impl RootEnvironment<'static> {
    /// The process's environment. In a test build, an environment without
    /// the developer's agent stores: no variable is set, and the home is an
    /// empty directory of the running test's own. A test that needs sessions
    /// passes fixture storages or its own environment.
    #[allow(clippy::disallowed_methods)]
    pub fn of_process() -> Self {
        #[cfg(test)]
        return Self::with_directories(
            &no_variable,
            Some(test_home::directory()),
            std::env::current_dir().ok(),
            Some(std::env::temp_dir()),
        );
        #[cfg(not(test))]
        Self::with_directories(
            &process_variable,
            home::home_dir(),
            std::env::current_dir().ok(),
            Some(std::env::temp_dir()),
        )
    }
}

#[cfg(not(test))]
#[allow(clippy::disallowed_methods)]
fn process_variable(name: &str) -> Option<OsString> {
    std::env::var_os(name)
}

#[cfg(test)]
fn no_variable(_name: &str) -> Option<OsString> {
    None
}

#[cfg(test)]
mod test_home {
    use std::path::PathBuf;

    thread_local! {
        /// libtest runs each test on a thread of its own, so the directory
        /// is the test's, and it is removed when the test ends.
        static HOME: tempfile::TempDir = tempfile::tempdir().expect("a temporary home directory");
    }

    /// The running test's own empty home directory.
    pub(super) fn directory() -> PathBuf {
        HOME.with(|home| home.path().to_path_buf())
    }
}

impl<'a> RootEnvironment<'a> {
    /// An environment without a temporary directory.
    #[cfg(test)]
    pub fn new(
        variable: &'a dyn Fn(&str) -> Option<OsString>,
        home: Option<PathBuf>,
        current_dir: Option<PathBuf>,
    ) -> Self {
        Self::with_directories(variable, home, current_dir, None)
    }

    /// This environment with `temp_dir` as the temporary directory.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn with_temp_dir(self, temp_dir: PathBuf) -> Self {
        Self {
            temp_dir: Some(with_platform_separators(temp_dir)),
            ..self
        }
    }

    fn with_directories(
        variable: &'a dyn Fn(&str) -> Option<OsString>,
        home: Option<PathBuf>,
        current_dir: Option<PathBuf>,
        temp_dir: Option<PathBuf>,
    ) -> Self {
        Self {
            variable,
            home: home.map(with_platform_separators),
            current_dir: current_dir.map(with_platform_separators),
            temp_dir: temp_dir.map(with_platform_separators),
            directories: Arc::new(CanonicalDirectories::new(SystemCanonicaliser::of_process())),
        }
    }

    /// This environment, canonicalising directories through `canonicaliser`.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn with_canonicaliser(self, canonicaliser: Arc<dyn Canonicaliser>) -> Self {
        self.with_directory_memo(Arc::new(CanonicalDirectories::new(canonicaliser)))
    }

    /// This environment, canonicalising directories through `directories`.
    #[must_use]
    pub(in crate::history) fn with_directory_memo(
        self,
        directories: Arc<CanonicalDirectories>,
    ) -> Self {
        Self {
            directories,
            ..self
        }
    }

    pub(crate) fn directories(&self) -> &Arc<CanonicalDirectories> {
        &self.directories
    }

    /// The variable's value with the platform's separator: every variable a
    /// provider reads names a directory, or a name joined into one.
    pub fn var_os(&self, name: &str) -> Option<OsString> {
        (self.variable)(name).map(|value| with_platform_separators(value).into_os_string())
    }

    /// The variable's value, or `None` when it is unset or not UTF-8.
    pub fn var(&self, name: &str) -> Option<String> {
        self.var_os(name)?.into_string().ok()
    }

    pub fn home(&self) -> Option<&Path> {
        self.home.as_deref()
    }

    pub fn current_dir(&self) -> Option<&Path> {
        self.current_dir.as_deref()
    }

    pub fn temp_dir(&self) -> Option<&Path> {
        self.temp_dir.as_deref()
    }
}

/// Where a root came from, which decides whether a transcript in it that names
/// no agent belongs to the provider that listed the root.
///
/// Only the code that resolves a root knows this. It cannot be recovered from
/// the path afterwards: a home directory can be named after an agent, and a
/// redirected directory can sit anywhere.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RootOrigin {
    /// The agent's own installation tree. A transcript here is that agent's even
    /// when the file itself says nothing about which agent wrote it.
    AgentTree,
    /// A directory the user pointed the agent at, through configuration or the
    /// environment. Pi-family agents share one wire format and can be aimed at
    /// the same directory, so an unmarked transcript here names no owner.
    Redirected,
}

/// One place a provider stores sessions — a directory tree for file-backed
/// providers, but any container a provider can enumerate: each root carries its
/// own session cache, keyed by its [canonical path](Self::canonical_path):
/// every spelling of one directory shares that cache. How sessions are found
/// inside it is the provider's own knowledge, stated in its
/// [`discover`](super::SessionStorage::discover).
///
/// Roots are [`RootOrigin::Redirected`] unless [`SessionRoot::in_agent_tree`]
/// says otherwise. That default is the cautious one: a provider that does not
/// declare a root as its own cannot claim transcripts that name no agent.
#[derive(Clone, Debug)]
pub struct SessionRoot {
    pub path: PathBuf,
    origin: RootOrigin,
    /// `path` as [`Self::canonical_path`] canonicalised it. Each load gets
    /// roots of its own from the storage and canonicalises each of them once.
    canonical: OnceLock<Option<PathBuf>>,
    /// Directories beside `path` that hold some files of the sessions
    /// listed under it, such as Codex's archived threads. Discovery does not
    /// list a session under them. The session cache stores a file beneath
    /// one of them by its path below that directory, as it stores a file
    /// beneath `path` by its path below `path`.
    beside: Vec<SessionRoot>,
}

impl PartialEq for SessionRoot {
    fn eq(&self, other: &Self) -> bool {
        self.path == other.path && self.origin == other.origin
    }
}

impl Eq for SessionRoot {}

impl SessionRoot {
    /// The root at `path`, spelled with the platform's separator.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: with_platform_separators(path.into()),
            origin: RootOrigin::Redirected,
            canonical: OnceLock::new(),
            beside: Vec::new(),
        }
    }

    /// This root, with the directory at `path` beside it holding some files
    /// of its sessions.
    #[must_use]
    pub fn with_directory_beside(mut self, path: impl Into<PathBuf>) -> Self {
        self.beside.push(Self::new(path));
        self
    }

    /// The directories beside this root that hold some files of its
    /// sessions.
    pub(crate) fn directories_beside(&self) -> &[SessionRoot] {
        &self.beside
    }

    /// This root, then each directory beside it, in the order positions
    /// count them.
    fn and_directories_beside(&self) -> impl Iterator<Item = &SessionRoot> {
        std::iter::once(self).chain(&self.beside)
    }

    /// The directory `path` lies below, by its position (0 for this root,
    /// then each directory beside it in order), and the path below it, as
    /// [`PathBelowRoot`] places it: a string comparison, without a
    /// file-system call. `None` for a path below neither.
    pub(crate) fn position_of(&self, path: &Path) -> Option<(usize, PathBelowRoot)> {
        self.and_directories_beside()
            .enumerate()
            .find_map(|(position, root)| Some((position, PathBelowRoot::of(path, &root.path)?)))
    }

    /// [`Self::position_of`] `path`, a path below the canonical path of this
    /// root or of a directory beside it, each canonicalised once per root.
    pub(crate) fn canonical_position_of(
        &self,
        path: &Path,
        canonicaliser: &dyn Canonicaliser,
    ) -> Option<(usize, PathBelowRoot)> {
        self.and_directories_beside()
            .enumerate()
            .find_map(|(position, root)| {
                let below = PathBelowRoot::of(path, root.canonical_path(canonicaliser)?)?;
                Some((position, below))
            })
    }

    /// True when `path` lies below this root or a directory beside it.
    pub(crate) fn holds(&self, path: &Path) -> bool {
        self.position_of(path).is_some()
    }

    /// `path` canonicalised through `canonicaliser`; `None` when it doesn't
    /// canonicalise, as for a directory the agent hasn't created yet.
    /// Canonicalised on the first call; later calls on this root, and on
    /// clones made after it, return the same answer.
    pub(crate) fn canonical_path(&self, canonicaliser: &dyn Canonicaliser) -> Option<&Path> {
        self.canonical
            .get_or_init(|| canonicaliser.canonicalise(&self.path).ok())
            .as_deref()
    }

    /// This root, keeping the canonical path found so far of itself and of
    /// each directory beside it. A directory that did not canonicalise is
    /// canonicalised again on its next use, because the agent may have
    /// created it since.
    pub(crate) fn forgetting_failed_canonicalisations(&self) -> Self {
        let canonical = match self.canonical.get() {
            Some(None) => OnceLock::new(),
            _ => self.canonical.clone(),
        };
        Self {
            path: self.path.clone(),
            origin: self.origin,
            canonical,
            beside: self
                .beside
                .iter()
                .map(Self::forgetting_failed_canonicalisations)
                .collect(),
        }
    }

    /// Mark this root as part of the agent's own tree rather than a directory
    /// the user redirected it to.
    #[must_use]
    pub fn in_agent_tree(self) -> Self {
        Self {
            origin: RootOrigin::AgentTree,
            ..self
        }
    }

    pub fn origin(&self) -> RootOrigin {
        self.origin
    }
}
