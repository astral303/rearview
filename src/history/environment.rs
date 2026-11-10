//! The inputs one rearview reads: the providers' session storages, the
//! session cache base, the clock, and the directory lister. Every production
//! entry point takes an [`Environment`]; `lib.rs` passes the user's, and a
//! test passes its own fixtures, cache directory, clock and lister.

use super::Source;
use super::provider::SessionStorage;
use super::provider::walk::{DirectoryLister, SystemLister};
use super::sqlite_cache::{CacheBase, ProcessIdentity};
use crate::clock::Clock;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Clone)]
pub struct Environment {
    /// In the order sources are presented to the user.
    storages: Vec<&'static dyn SessionStorage>,
    /// `None` without a directory to cache sessions in.
    cache_base: Option<CacheBase>,
    clock: Arc<dyn Clock>,
    lister: Arc<dyn DirectoryLister>,
}

impl Environment {
    /// The user's: every provider's storage, the session cache in the user's
    /// cache directory, this process's identity, `clock`, and the file
    /// system's listing.
    pub fn of_user(clock: Arc<dyn Clock>) -> Self {
        Self::new(
            super::provider::providers()
                .iter()
                .map(|provider| provider.storage())
                .collect(),
            crate::cache_file::session_cache_base(),
            ProcessIdentity::default(),
            clock,
            Arc::new(SystemLister),
        )
    }

    /// Every provider's storage without a session cache, on `clock`: what a
    /// view of rows loaded beforehand reads.
    pub fn without_session_cache(clock: Arc<dyn Clock>) -> Self {
        Self::new(
            super::provider::providers()
                .iter()
                .map(|provider| provider.storage())
                .collect(),
            None,
            ProcessIdentity::default(),
            clock,
            Arc::new(SystemLister),
        )
    }

    /// `storages` in presentation order, the session cache under
    /// `cache_base` as the rearview `process` reaches it, `clock`, and
    /// `lister`.
    pub fn new(
        storages: Vec<&'static dyn SessionStorage>,
        cache_base: Option<PathBuf>,
        process: ProcessIdentity,
        clock: Arc<dyn Clock>,
        lister: Arc<dyn DirectoryLister>,
    ) -> Self {
        Self {
            storages,
            cache_base: cache_base.map(|path| CacheBase::new(path, process, Arc::clone(&clock))),
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

    pub fn cache_base(&self) -> Option<&CacheBase> {
        self.cache_base.as_ref()
    }

    pub fn clock(&self) -> &Arc<dyn Clock> {
        &self.clock
    }

    pub fn lister(&self) -> &dyn DirectoryLister {
        self.lister.as_ref()
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use crate::clock::TestClock;
    use std::path::Path;

    /// `storages` in presentation order, the session cache under
    /// `cache_base` as a new rearview reaches it, on test time, listing the
    /// file system.
    pub(crate) fn test_environment(
        storages: Vec<&'static dyn SessionStorage>,
        cache_base: Option<&Path>,
    ) -> Environment {
        Environment::new(
            storages,
            cache_base.map(Path::to_path_buf),
            ProcessIdentity::default(),
            Arc::new(TestClock::default()),
            Arc::new(SystemLister),
        )
    }

    /// No agent's storage and no session cache, on `clock`: what an app over
    /// rows a test builds reads.
    pub(crate) fn environment_on(clock: Arc<dyn Clock>) -> Environment {
        Environment::new(
            Vec::new(),
            None,
            ProcessIdentity::default(),
            clock,
            Arc::new(SystemLister),
        )
    }

    /// [`test_environment`] on the system clock, for a test beside a real
    /// process or thread that commits and waits in real time.
    pub(crate) fn real_time_environment(
        storages: Vec<&'static dyn SessionStorage>,
        cache_base: Option<&Path>,
    ) -> Environment {
        Environment::new(
            storages,
            cache_base.map(Path::to_path_buf),
            ProcessIdentity::default(),
            Arc::new(crate::clock::SystemClock::new()),
            Arc::new(SystemLister),
        )
    }

    /// Every session `storage` holds, newest first, loaded through the
    /// session cache under `cache_base` on test time.
    pub(crate) fn load_rows(
        storage: &dyn SessionStorage,
        cache_base: &Path,
    ) -> Vec<crate::history::Conversation> {
        crate::history::provider::load_sessions(
            &test_environment(Vec::new(), Some(cache_base)),
            storage,
            false,
            None,
            crate::history::load_meter::AgentMeter::UNMETERED,
            crate::history::provider::LeftOutRows::NONE,
            &mut |_, _| {},
        )
        .unwrap()
        .conversations
    }
}
