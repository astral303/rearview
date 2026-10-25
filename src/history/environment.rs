//! The inputs one rearview reads: the providers' session storages, the
//! session cache base, the clock, and the directory lister. Every production
//! entry point takes an [`Environment`]; `lib.rs` passes the user's, and a
//! test passes its own fixtures, cache directory, clock and lister.

use super::Source;
use super::provider::SessionStorage;
use super::provider::walk::{DirectoryLister, SystemLister};
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
}

impl Environment {
    /// The user's: every provider's storage, the session cache in the user's
    /// cache directory, this process's identity, `clock`, and the file
    /// system's listing. Fails without a directory for the session cache.
    pub fn of_user(clock: Arc<dyn Clock>) -> Result<Self> {
        let cache_base =
            crate::cache_file::session_cache_base().ok_or(AppError::NoSessionCacheDirectory)?;
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
    /// `cache_base` as the rearview `process` reaches it, `clock`, and
    /// `lister`.
    pub fn new(
        storages: Vec<&'static dyn SessionStorage>,
        cache_base: PathBuf,
        process: ProcessIdentity,
        clock: Arc<dyn Clock>,
        lister: Arc<dyn DirectoryLister>,
    ) -> Self {
        Self {
            storages,
            cache_base: CacheBase::new(cache_base, process, Arc::clone(&clock)),
            clock,
            lister,
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

    /// This environment with its session cache on a drive with `free_bytes`
    /// free, whatever its writes add.
    #[cfg(test)]
    pub(crate) fn with_free_space(self, free_bytes: u64) -> Self {
        Self {
            cache_base: self.cache_base.with_free_space(free_bytes),
            ..self
        }
    }

    /// This environment with its session cache on a drive whose free space
    /// `disk` answers.
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
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use crate::cli::DebugLevel;
    use crate::clock::TestClock;
    use crate::history::Conversation;
    use crate::history::provider::walk::{SessionFiles, session_stubs};
    use crate::history::provider::{
        DiscoveredSessions, Fingerprint, RootEnvironment, SessionCache, SessionRoot, SessionStub,
    };
    use std::path::Path;
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

    /// No agent's storage and this test's own session cache directory, on
    /// `clock`: what an app over rows a test builds reads.
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
    /// ownership as long as another rearview keeps it, publishing its wait
    /// nowhere a test reads.
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
        let environment = test_environment(Vec::new(), Some(cache_base));
        crate::history::provider::load_sessions(
            &environment,
            storage,
            false,
            None,
            crate::history::load_meter::AgentMeter::UNMETERED,
            crate::history::provider::LeftOutRows::NONE,
            &waiting_operation(&environment),
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
    pub(crate) fn cache_files(source: Source, locator: &Path, subagents: &[PathBuf]) {
        cache_sessions(
            source,
            vec![SessionFiles {
                transcript: locator.to_path_buf(),
                subagents: subagents.to_vec(),
            }],
        );
    }

    /// The session each of `rows` names, with its sub-agent transcripts,
    /// cached as [`cache_sessions`] caches it.
    pub(crate) fn cache_rows(rows: &[Conversation]) {
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
            cache_sessions(source, sessions);
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
    /// load whose discovery finds them alone, each under the directory of
    /// its transcript. Returns their rows as the load lists them. A later
    /// load of a directory removes the sessions this one cached there.
    fn cache_sessions(source: Source, sessions: Vec<SessionFiles>) -> Vec<Conversation> {
        load_rows(
            &SessionFilesStorage { source, sessions },
            &crate::cache_file::test_support::test_cache_directory(),
        )
    }

    /// The storage of `sessions`: discovery under the directory of each
    /// session's transcript finds the sessions there and no others, and
    /// `source`'s storage parses them.
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
                let root = SessionRoot::new(
                    session
                        .transcript
                        .parent()
                        .expect("a transcript in a directory"),
                );
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
                if session.transcript.parent() == Some(root.path.as_path()) {
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
        ) -> Result<Option<Conversation>> {
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
