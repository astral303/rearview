//! The inputs one rearview reads: the providers' session storages, the
//! session cache base, the clock, the directory lister, the canonicaliser
//! and the working directory. Every production entry point accepts an
//! [`Environment`]; `lib.rs` passes the user's, and a test passes its own
//! fixtures, cache directory, clock, lister and working directory.

use super::Source;
use super::Workspace;
use super::path::CanonicalDirectories;
use super::provider::walk::{Canonicaliser, DirectoryLister, SystemCanonicaliser, SystemLister};
use super::provider::{RootEnvironment, SessionStorage};
use super::sqlite_cache::{CacheBase, ProcessIdentity};
use crate::clock::Clock;
use crate::error::{AppError, Result};
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Clone)]
pub struct Environment {
    /// In the order sources are presented to the user.
    storages: Vec<&'static dyn SessionStorage>,
    cache_base: CacheBase,
    clock: Arc<dyn Clock>,
    lister: Arc<dyn DirectoryLister>,
    /// Shared with the session cache base, whose reads and writes key session
    /// roots and files through it.
    canonicaliser: Arc<dyn Canonicaliser>,
    /// Or the error reading it: only a command scoped to the workspace fails
    /// on that error.
    working_directory: std::result::Result<PathBuf, Arc<std::io::Error>>,
    /// The directories one command canonicalises, each once; `None` until
    /// [`Self::for_one_command`] makes this environment for a command.
    command_directories: Option<Arc<CanonicalDirectories>>,
}

impl Environment {
    /// The user's: every provider's storage, the session cache under the
    /// user's `cache_base`, this process's identity, `clock`, the file
    /// system's listing, and the process's working directory. Fails without
    /// a `cache_base`.
    pub fn of_user(cache_base: Option<PathBuf>, clock: Arc<dyn Clock>) -> Result<Self> {
        let cache_base = cache_base.ok_or(AppError::NoSessionCacheDirectory)?;
        Ok(Self::new(
            super::provider::providers()
                .iter()
                .map(|provider| provider.storage())
                .collect(),
            cache_base,
            ProcessIdentity::default(),
            clock,
            Arc::new(SystemLister),
        ))
    }

    /// `storages` in presentation order, the session cache under
    /// `cache_base` as the rearview `process` reaches it, `clock`, `lister`,
    /// and the process's working directory.
    pub fn new(
        storages: Vec<&'static dyn SessionStorage>,
        cache_base: PathBuf,
        process: ProcessIdentity,
        clock: Arc<dyn Clock>,
        lister: Arc<dyn DirectoryLister>,
    ) -> Self {
        let canonicaliser = SystemCanonicaliser::of_process();
        Self {
            storages,
            cache_base: CacheBase::new(
                cache_base,
                process,
                Arc::clone(&clock),
                Arc::clone(&canonicaliser),
            ),
            clock,
            lister,
            canonicaliser,
            working_directory: std::env::current_dir().map_err(Arc::new),
            command_directories: None,
        }
    }

    pub fn storages(&self) -> &[&'static dyn SessionStorage] {
        &self.storages
    }

    /// The storage of `source`'s sessions; `None` for a source this
    /// environment does not read.
    pub fn storage_of(&self, source: Source) -> Option<&'static dyn SessionStorage> {
        self.storages
            .iter()
            .copied()
            .find(|storage| storage.source() == source)
    }

    pub fn cache_base(&self) -> &CacheBase {
        &self.cache_base
    }

    pub fn clock(&self) -> &Arc<dyn Clock> {
        &self.clock
    }

    pub fn lister(&self) -> &dyn DirectoryLister {
        self.lister.as_ref()
    }

    pub fn canonicaliser(&self) -> &dyn Canonicaliser {
        self.canonicaliser.as_ref()
    }

    pub(crate) fn shared_canonicaliser(&self) -> &Arc<dyn Canonicaliser> {
        &self.canonicaliser
    }

    /// The memo to canonicalise directories through. An environment
    /// [`Self::for_one_command`] made returns the command's memo. The
    /// command's loads, key building, root environment and workspace share
    /// it. Any other environment returns a new memo on each call.
    pub(crate) fn canonical_directories(&self) -> Arc<CanonicalDirectories> {
        match &self.command_directories {
            Some(directories) => Arc::clone(directories),
            None => Arc::new(CanonicalDirectories::new(Arc::clone(&self.canonicaliser))),
        }
    }

    /// This environment for one command. Canonicalisations through
    /// [`Self::canonical_directories`] share one memo and canonicalise each
    /// directory at most once.
    #[must_use]
    pub(crate) fn for_one_command(self) -> Self {
        let directories = CanonicalDirectories::new(Arc::clone(&self.canonicaliser));
        Self {
            command_directories: Some(Arc::new(directories)),
            ..self
        }
    }

    /// This environment, canonicalising paths through `canonicaliser`.
    #[cfg(test)]
    pub(crate) fn with_canonicaliser(self, canonicaliser: Arc<dyn Canonicaliser>) -> Self {
        Self {
            cache_base: self
                .cache_base
                .with_canonicaliser(Arc::clone(&canonicaliser)),
            canonicaliser,
            ..self
        }
    }

    /// The process's variables and directories, canonicalising directories
    /// through [`Self::canonical_directories`].
    pub(crate) fn root_environment(&self) -> RootEnvironment<'static> {
        RootEnvironment::of_process().with_directory_memo(self.canonical_directories())
    }

    /// The workspace of the working directory. Fails when the process could
    /// not read its working directory.
    pub fn workspace(&self) -> Result<Workspace> {
        match &self.working_directory {
            Ok(directory) => Ok(Workspace::in_environment(
                directory.clone(),
                &self.root_environment(),
            )),
            Err(error) => Err(std::io::Error::new(error.kind(), error.to_string()).into()),
        }
    }

    /// This environment with `directory` as its working directory.
    #[cfg(test)]
    pub(crate) fn with_working_directory(self, directory: PathBuf) -> Self {
        Self {
            working_directory: Ok(directory),
            ..self
        }
    }

    /// This environment with its session cache on a drive with `free_bytes`
    /// free, whatever its writes add.
    #[cfg(test)]
    pub(crate) fn with_free_space(self, free_bytes: u64) -> Self {
        Self {
            cache_base: self.cache_base.with_free_space(free_bytes),
            ..self
        }
    }

    /// This environment with its session cache on a drive; `disk` reports the
    /// drive's free space.
    #[cfg(test)]
    pub(crate) fn with_disk_space(
        self,
        disk: Arc<dyn crate::history::sqlite_cache::DiskSpace>,
    ) -> Self {
        Self {
            cache_base: self.cache_base.with_disk_space(disk),
            ..self
        }
    }

    /// This environment, each step of its session cache's low-space
    /// deletion waiting at most `wait`.
    #[cfg(test)]
    pub(crate) fn with_low_space_wait(self, wait: std::time::Duration) -> Self {
        Self {
            cache_base: self.cache_base.with_low_space_wait(wait),
            ..self
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::test_environment;
    use crate::history::provider::walk::test_support::CountingCanonicaliser;
    use std::sync::Arc;

    #[test]
    fn commands_loads_root_environment_and_workspace_canonicalise_a_directory_once() {
        let directory = tempfile::tempdir().unwrap();
        let canonicaliser = Arc::new(CountingCanonicaliser::default());
        let environment = test_environment(Vec::new(), None)
            .with_canonicaliser(canonicaliser.clone())
            .with_working_directory(directory.path().to_path_buf())
            .for_one_command();

        environment
            .canonical_directories()
            .canonical(directory.path());
        environment
            .root_environment()
            .directories()
            .canonical(directory.path());
        assert!(environment.workspace().unwrap().includes(directory.path()));

        // A temporary directory inside a repository also has the
        // repository's checkouts canonicalised for the workspace.
        let canonicalised = canonicaliser.canonicalised();
        let distinct: std::collections::HashSet<_> = canonicalised.iter().collect();
        assert!(
            canonicalised.first() == Some(&directory.path().to_path_buf())
                && distinct.len() == canonicalised.len(),
            "the command's loads, root environment and workspace canonicalise each directory once: {canonicalised:?}"
        );
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use crate::cli::DebugLevel;
    use crate::clock::TestClock;
    use crate::history::Conversation;
    use crate::history::provider::walk::test_support::CountingCanonicaliser;
    use crate::history::provider::walk::{SessionFiles, session_stubs};
    use crate::history::provider::{
        DiscoveredSessions, Fingerprint, RootEnvironment, SessionCache, SessionRoot, SessionStub,
    };
    use std::path::{Path, PathBuf};
    use std::time::{Duration, UNIX_EPOCH};

    /// `storages` in presentation order, the session cache under
    /// `cache_base`, or under this test's own cache directory without one, as
    /// a new rearview reaches it, on test time, listing the file system.
    pub(crate) fn test_environment(
        storages: Vec<&'static dyn SessionStorage>,
        cache_base: Option<&Path>,
    ) -> Environment {
        environment_on_clock(storages, cache_base, Arc::new(TestClock::default()))
    }

    /// This test's own session cache directory on `clock`, without any
    /// agent's storage, for an app over rows the test builds.
    pub(crate) fn environment_on(clock: Arc<dyn Clock>) -> Environment {
        environment_on_clock(Vec::new(), None, clock)
    }

    /// [`test_environment`] on the system clock, for a test beside a real
    /// process or thread that commits and waits in real time.
    pub(crate) fn real_time_environment(
        storages: Vec<&'static dyn SessionStorage>,
        cache_base: Option<&Path>,
    ) -> Environment {
        environment_on_clock(
            storages,
            cache_base,
            Arc::new(crate::clock::SystemClock::new()),
        )
    }

    /// [`test_environment`] on `clock`.
    pub(crate) fn environment_on_clock(
        storages: Vec<&'static dyn SessionStorage>,
        cache_base: Option<&Path>,
        clock: Arc<dyn Clock>,
    ) -> Environment {
        Environment::new(
            storages,
            cache_base.map_or_else(
                crate::cache_file::test_support::test_cache_directory,
                |base| base.to_path_buf(),
            ),
            ProcessIdentity::default(),
            clock,
            Arc::new(SystemLister),
        )
    }

    /// This test's own session cache, as a new rearview reaches it, on test
    /// time.
    pub(crate) fn test_cache_base() -> CacheBase {
        test_environment(Vec::new(), None).cache_base().clone()
    }

    /// One operation on `environment`'s session cache that waits for refresh
    /// ownership as long as another rearview keeps it, and doesn't publish
    /// its wait where a test reads it.
    pub(crate) fn waiting_operation(environment: &Environment) -> crate::history::OperationScope {
        crate::history::OperationScope::new(
            environment.cache_base(),
            crate::history::Patience::UntilAcquired,
            Arc::default(),
        )
    }

    /// Every session `storage` holds, newest first, loaded on test time
    /// through the session cache database under `cache_base`.
    pub(crate) fn load_rows(
        storage: &dyn SessionStorage,
        cache_base: &Path,
    ) -> Vec<crate::history::Conversation> {
        load_rows_in(&test_environment(Vec::new(), Some(cache_base)), storage)
    }

    /// The paths `open` canonicalises, in order and with repeats, on a
    /// command that starts from a cache its own load of `storage` filled.
    pub(crate) fn canonicalised_by_opening(
        storage: &'static dyn SessionStorage,
        open: &dyn Fn(&Environment),
    ) -> Vec<PathBuf> {
        canonicalised_by(storage, open, |environment| {
            load_rows_in(environment, storage);
        })
    }

    /// [`canonicalised_by_opening`] after the transcript at `changed` gets a
    /// later modification time, so its session's copy is older.
    pub(crate) fn canonicalised_by_opening_older_copy(
        storage: &'static dyn SessionStorage,
        changed: &Path,
        open: &dyn Fn(&Environment),
    ) -> Vec<PathBuf> {
        canonicalised_by(storage, open, |environment| {
            load_rows_in(environment, storage);
            set_later_modification_time(changed);
        })
    }

    /// Moves the modification time of the file at `path` one second later.
    /// A copy written before the move is then older than the file.
    pub(crate) fn set_later_modification_time(path: &Path) {
        let file = std::fs::File::options().write(true).open(path).unwrap();
        let modified = file.metadata().unwrap().modified().unwrap();
        file.set_modified(modified + std::time::Duration::from_secs(1))
            .unwrap();
    }

    /// The paths `open` canonicalises, in order and with repeats, on a
    /// command that starts from an admitted, empty cache.
    pub(crate) fn canonicalised_by_opening_on_empty_cache(
        storage: &'static dyn SessionStorage,
        open: &dyn Fn(&Environment),
    ) -> Vec<PathBuf> {
        canonicalised_by(storage, open, |environment| {
            crate::history::sqlite_cache::SqliteCache::open(environment.cache_base())
                .unwrap()
                .mark_admitted()
                .unwrap();
        })
    }

    /// The paths `open` canonicalises after `prepare` on a cache of its own.
    fn canonicalised_by(
        storage: &'static dyn SessionStorage,
        open: &dyn Fn(&Environment),
        prepare: impl FnOnce(&Environment),
    ) -> Vec<PathBuf> {
        let base = tempfile::tempdir().unwrap();
        let canonicaliser = Arc::new(CountingCanonicaliser::default());
        let environment = test_environment(vec![storage], Some(base.path()))
            .with_canonicaliser(canonicaliser.clone());
        prepare(&environment);
        let after_preparing = canonicaliser.canonicalised().len();
        open(&environment);
        canonicaliser.canonicalised()[after_preparing..].to_vec()
    }

    /// `agent read` of the session at `path`.
    pub(crate) fn agent_read_of(path: &Path) -> impl Fn(&Environment) {
        let path = path.to_path_buf();
        move |environment| {
            crate::agent::service::AgentService::new(environment.clone())
                .execute(crate::agent::test_support::agent_command(&[
                    "read",
                    &path.to_string_lossy(),
                ]))
                .unwrap();
        }
    }

    /// The TUI's open of the session at `path`, a file named on the command
    /// line: canonicalised, read by the background worker and shown. Fails
    /// unless the viewer's text contains `shown`.
    pub(crate) fn direct_open_of(path: &Path, shown: &str) -> impl Fn(&Environment) {
        let path = path.to_path_buf();
        let shown = shown.to_owned();
        move |environment| {
            let text = crate::tui::claude_home::text_of_file_opened_directly(&path, environment);
            assert!(
                text.contains(&shown),
                "the viewer shows the opened session: {text}"
            );
        }
    }

    /// The printout of `source`'s session at `path`.
    pub(crate) fn printout_of(
        source: crate::history::Source,
        path: &Path,
    ) -> impl Fn(&Environment) {
        let path = path.to_path_buf();
        move |environment| {
            crate::history::printed_log_entries(
                environment,
                source,
                &path,
                false,
                &mut std::io::sink(),
            )
            .unwrap();
        }
    }

    /// Every session `storage` holds, newest first, loaded through
    /// `environment`.
    pub(crate) fn load_rows_in(
        environment: &Environment,
        storage: &dyn SessionStorage,
    ) -> Vec<crate::history::Conversation> {
        crate::history::provider::load_sessions(
            environment,
            storage,
            crate::history::provider::LoadOptions::default(),
            &waiting_operation(environment),
            &mut |_, _| {},
        )
        .unwrap()
        .conversations
    }

    /// `source`'s session at `locator`, with the sub-agent transcripts
    /// `subagents`, cached as [`cache_sessions`] caches it. Returns its row
    /// as the load lists it.
    pub(crate) fn cache_session(
        source: Source,
        locator: &Path,
        subagents: &[PathBuf],
    ) -> Conversation {
        let session = SessionFiles {
            transcript: locator.to_path_buf(),
            subagents: subagents.to_vec(),
        };
        cache_sessions(source, vec![session])
            .pop()
            .expect("the load lists the session")
    }

    /// The copy of `read`, a drawing-thread read of a session a load cached.
    /// Panics when the copy is out of date or missing.
    pub(crate) fn current_copy<T>(read: crate::history::InteractiveRead<T>) -> T {
        match read.into_read() {
            crate::history::CachedRead::Current(copy) => copy.into_value(),
            crate::history::CachedRead::Older { .. } => {
                panic!("the cached copy is current after the load cached the session")
            }
            crate::history::CachedRead::Missing { .. } => {
                panic!("the cache holds a copy after the load cached the session")
            }
        }
    }

    /// `source`'s session at `locator`, with the sub-agent transcripts
    /// `subagents`, written into this test's own session cache as
    /// [`cache_sessions`] writes it, whether or not the load lists it.
    /// Returns its row; `None` when the load doesn't list it.
    pub(crate) fn cache_files(
        source: Source,
        locator: &Path,
        subagents: &[PathBuf],
    ) -> Option<Conversation> {
        cache_sessions(
            source,
            vec![SessionFiles {
                transcript: locator.to_path_buf(),
                subagents: subagents.to_vec(),
            }],
        )
        .pop()
    }

    /// The session each of `rows` names, with its sub-agent transcripts,
    /// cached as [`cache_sessions`] caches it. Each row the load lists gets
    /// the address the load lists it at.
    pub(crate) fn cache_rows(rows: &mut [Conversation]) {
        let mut sources: Vec<Source> = Vec::new();
        for row in rows.iter() {
            if !sources.contains(&row.source) {
                sources.push(row.source);
            }
        }
        for source in sources {
            let sessions = rows
                .iter()
                .filter(|row| row.source == source)
                .map(|row| SessionFiles {
                    transcript: row.path.clone(),
                    subagents: row.subagents.clone(),
                })
                .collect();
            for listed in cache_sessions(source, sessions) {
                for row in rows
                    .iter_mut()
                    .filter(|row| row.source == source && row.path == listed.path)
                {
                    row.cache_address = listed.cache_address.clone();
                }
            }
        }
    }

    /// This test's own environment, reading each of `rows`' sessions through
    /// a storage whose discovery finds them alone, as [`cache_rows`] caches
    /// them.
    pub(crate) fn environment_reading(rows: &[Conversation]) -> Environment {
        let mut storages: Vec<&'static dyn SessionStorage> = Vec::new();
        let mut sources: Vec<Source> = Vec::new();
        for row in rows {
            if !sources.contains(&row.source) {
                sources.push(row.source);
            }
        }
        for source in sources {
            let sessions = rows
                .iter()
                .filter(|row| row.source == source)
                .map(|row| SessionFiles {
                    transcript: row.path.clone(),
                    subagents: row.subagents.clone(),
                })
                .collect();
            storages.push(Box::leak(Box::new(SessionFilesStorage {
                source,
                sessions,
            })));
        }
        test_environment(storages, None)
    }

    /// `source`'s `sessions` cached in this test's own session cache by one
    /// load whose discovery finds them alone, each under the deepest
    /// directory holding all its files. Returns their rows as the load lists
    /// them. A later load of a directory removes the sessions this one
    /// cached there.
    fn cache_sessions(source: Source, sessions: Vec<SessionFiles>) -> Vec<Conversation> {
        load_rows(
            &SessionFilesStorage { source, sessions },
            &crate::cache_file::test_support::test_cache_directory(),
        )
    }

    /// The deepest directory holding every file of `session`.
    fn directory_holding(session: &SessionFiles) -> PathBuf {
        directory_holding_files(&session.transcript, &session.subagents)
    }

    /// The deepest directory holding `transcript` and `subagents`.
    pub(crate) fn directory_holding_files(transcript: &Path, subagents: &[PathBuf]) -> PathBuf {
        let mut directory = transcript
            .parent()
            .expect("a transcript in a directory")
            .to_path_buf();
        for subagent in subagents {
            while !subagent.starts_with(&directory) {
                directory = directory
                    .parent()
                    .expect("the session's files share a drive")
                    .to_path_buf();
            }
        }
        directory
    }

    /// The storage of `sessions`: discovery under the deepest directory
    /// holding each session's files finds the sessions there and no others,
    /// and `source`'s storage parses them.
    struct SessionFilesStorage {
        source: Source,
        sessions: Vec<SessionFiles>,
    }

    impl SessionFilesStorage {
        /// The stub of `session` under `root`, stamped as discovery stamps its
        /// files, or by its provider for a locator inside a database file.
        fn stub(&self, root: &SessionRoot, session: &SessionFiles) -> Result<Option<SessionStub>> {
            let files = SessionFiles {
                transcript: session.transcript.clone(),
                subagents: session.subagents.clone(),
            };
            if let Some(stub) = session_stubs(root, vec![files]).pop() {
                return Ok(Some(stub));
            }
            let stamp = self
                .source
                .provider()
                .storage()
                .current_fingerprint(root, &session.transcript)?;
            Ok(stamp.map(|stamp| SessionStub {
                locator: session.transcript.clone(),
                subagents: session.subagents.clone(),
                cache_key: session.transcript.to_string_lossy().into_owned(),
                fingerprint: Fingerprint {
                    size: stamp.file_size,
                    modified: Some(UNIX_EPOCH + Duration::new(stamp.mtime_secs, stamp.mtime_nsecs)),
                },
            }))
        }
    }

    impl SessionStorage for SessionFilesStorage {
        fn source(&self) -> Source {
            self.source
        }

        fn cache(&self) -> SessionCache {
            self.source.provider().storage().cache()
        }

        fn roots_in(&self, _environment: &RootEnvironment) -> Result<Vec<SessionRoot>> {
            let mut roots: Vec<SessionRoot> = Vec::new();
            for session in &self.sessions {
                let root = SessionRoot::new(directory_holding(session));
                if !roots.contains(&root) {
                    roots.push(root);
                }
            }
            Ok(roots)
        }

        fn discover(
            &self,
            root: &SessionRoot,
            _lister: &dyn DirectoryLister,
        ) -> Result<DiscoveredSessions> {
            let mut stubs = Vec::new();
            for session in &self.sessions {
                if SessionRoot::new(directory_holding(session)) == *root {
                    stubs.extend(self.stub(root, session)?);
                }
            }
            Ok(DiscoveredSessions::complete(stubs))
        }

        fn rediscover_unlisted_session(
            &self,
            _root: &SessionRoot,
            _locator: &Path,
            _lister: &dyn DirectoryLister,
        ) -> Result<Option<SessionStub>> {
            Ok(None)
        }

        fn parse_session(
            &self,
            permit: crate::history::ParsePermit<'_>,
            stub: &SessionStub,
            root: &SessionRoot,
            debug_level: Option<DebugLevel>,
            on_transcript_read: &(dyn Fn() + Sync),
            clock: &dyn Clock,
        ) -> Result<crate::history::parser::ParsedSession> {
            self.source.provider().storage().parse_session(
                permit,
                stub,
                root,
                debug_level,
                on_transcript_read,
                clock,
            )
        }
    }
}
