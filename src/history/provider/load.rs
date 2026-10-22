//! The load loop shared by every provider that stores sessions under roots.

use super::outcome::{SessionOutcome, SessionRead};
use super::storage::{DiscoveredSessions, ResolvedSession, SessionStub};
use super::{SessionRoot, SessionStorage, SessionTitle};
use crate::cli::DebugLevel;
use crate::clock::Clock;
use crate::debug;
use crate::error::{AppError, Result, SessionDatabaseFailure};
use crate::history::fingerprint::CachedFingerprint;
use crate::history::format::same_file;
use crate::history::load_meter::AgentMeter;
use crate::history::shard_files;
use crate::history::sqlite_cache::refresh::RefreshToken;
use crate::history::sqlite_cache::{
    self, CacheBase, CacheWrite, CacheWriter, CatalogueSession, InteractiveCache, NextStep,
    OperationAccess, OperationScope, SessionRecord, SessionStatus, SqliteCache,
};
use crate::history::{Conversation, Environment, FilterTerm, Source, format_short_name_from_path};
use chrono::{DateTime, Local, TimeZone};
use rayon::prelude::*;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;

mod cache_access;
mod targeted;

use cache_access::{CacheAccess, DatabaseReader, Deletions};
use targeted::StubsAfterRediscovery;

/// One provider's sessions, and what its roots hold that it ignores.
pub struct LoadedSessions {
    /// Newest first.
    pub conversations: Vec<Conversation>,
    /// One term per reason a root's sessions were ignored for, named for the
    /// user.
    pub ignored: Vec<FilterTerm>,
    /// The sessions that hold no conversation, and the unreadable ones.
    pub skipped: SkippedSessions,
    /// Every session discovery found, in discovery order, with what the load
    /// recorded of it.
    pub discovered: Vec<DiscoveredSession>,
}

/// A session discovery found, and the directory and id of the row the load
/// listed for it: `None` when the load listed no row, because the session
/// holds no conversation or could not be read.
pub struct DiscoveredSession {
    pub stub: SessionStub,
    pub recorded: Option<RecordedSession>,
    /// The copy a load that read the cache as it is found older than the
    /// transcript, or did not find; `None` when the load read the session
    /// as it is now, or the command leaves its cached row out.
    pub stale: Option<StaleCopy>,
}

/// The session cache's copy of a session older than its transcript, as a load
/// that read the cache as it is found it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StaleCopy {
    /// The time of the copy's last message; `None` when the copy is missing
    /// from the cache or its messages are undated.
    pub cached_until: Option<DateTime<Local>>,
}

/// The directory a listed session ran in and the id it recorded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordedSession {
    /// `None` for a session that recorded no directory.
    pub project_dir: Option<PathBuf>,
    pub session_id: String,
}

/// One provider's sessions as discovery finds them now, for a refresh.
#[derive(Default)]
pub struct RediscoveredSessions {
    /// Every session discovery found, with its fingerprint.
    pub found: Vec<(PathBuf, Option<CachedFingerprint>)>,
    /// The sessions the refresh picked, read through the cache as a load
    /// reads them.
    pub outcomes: Vec<SessionOutcome>,
    /// One term per reason a root's sessions were ignored for, named for the
    /// user.
    pub ignored: Vec<FilterTerm>,
    /// Titles stored beside the transcripts, by session id. A rename that
    /// writes only these leaves every fingerprint as it was.
    pub external_titles: HashMap<String, SessionTitle>,
    /// Missing paths beneath these directories cannot establish deletions.
    pub unreadable_directories: Vec<PathBuf>,
}

/// The kind of error a failed read hit, which decides when a refresh reads
/// the session again.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReadError {
    /// An error `is_transient` accepts. A refresh reads the session again,
    /// because the next read can succeed with the transcript unchanged.
    Transient,
    /// Any other error. A refresh records the session as unreadable and skips
    /// it until its fingerprint changes, because an unchanged transcript fails
    /// the same way.
    Permanent,
}

impl ReadError {
    pub(crate) fn of(error: &AppError) -> Self {
        if is_transient(error) {
            Self::Transient
        } else {
            Self::Permanent
        }
    }
}

/// A session discovery found: the agent whose discovery found it, and the
/// fingerprint (transcript size and modification time) it found.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FoundSession {
    pub source: Source,
    pub fingerprint: Option<CachedFingerprint>,
    pub has_transient_read_error: bool,
}

impl FoundSession {
    /// The session `row` was read from, as discovery found it.
    pub fn of(row: &Conversation) -> Self {
        Self {
            source: row.source,
            fingerprint: row.fingerprint,
            has_transient_read_error: row.has_transient_read_error,
        }
    }

    /// True when `source`'s discovery finds `fingerprint` for this session
    /// again, and its read hit no transient error. A session without a
    /// fingerprint is always read again; a load caches no entry for it.
    pub(crate) fn is_unchanged(
        &self,
        source: Source,
        fingerprint: Option<CachedFingerprint>,
    ) -> bool {
        self.source == source
            && !self.has_transient_read_error
            && self.fingerprint.is_some()
            && self.fingerprint == fingerprint
    }
}

/// The rows a command leaves out of its result. A load does not reread a
/// changed session whose cached row the command leaves out: the session keeps
/// that row, and the next load that includes it reads it again.
#[derive(Clone, Copy)]
pub struct LeftOutRows<'a>(Option<&'a (dyn Fn(&Conversation) -> bool + Sync)>);

impl<'a> LeftOutRows<'a> {
    /// Every row stays in: a load rereads every changed session.
    pub const NONE: LeftOutRows<'static> = LeftOutRows(None);

    pub fn of(leaves_out: &'a (dyn Fn(&Conversation) -> bool + Sync)) -> Self {
        Self(Some(leaves_out))
    }
}

type SessionsByLocator = HashMap<PathBuf, FoundSession>;

/// The sessions a refresh skips until discovery finds a new fingerprint for
/// them.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SkippedSessions {
    /// Sessions holding no conversation, sessions outside the time filter, and
    /// a second agent's view of a listed file.
    pub unlisted: SessionsByLocator,
    /// Unreadable sessions, listed or not: their read hit a permanent error.
    /// A listed unreadable session keeps its row as it was.
    pub unreadable: SessionsByLocator,
}

impl SkippedSessions {
    pub fn is_empty(&self) -> bool {
        self.unlisted.is_empty() && self.unreadable.is_empty()
    }

    pub fn extend(&mut self, other: Self) {
        self.unlisted.extend(other.unlisted);
        self.unreadable.extend(other.unreadable);
    }

    pub(crate) fn get(&self, locator: &Path) -> Option<&FoundSession> {
        self.unlisted
            .get(locator)
            .or_else(|| self.unreadable.get(locator))
    }

    /// True when `source` already read `locator` at `fingerprint` and found it
    /// unlisted or unreadable.
    pub(crate) fn is_unchanged(
        &self,
        source: Source,
        locator: &Path,
        fingerprint: Option<CachedFingerprint>,
    ) -> bool {
        [&self.unlisted, &self.unreadable].into_iter().any(|set| {
            set.get(locator)
                .is_some_and(|found| found.is_unchanged(source, fingerprint))
        })
    }

    /// Copy `previous`'s entry for `locator` into the same set, while
    /// `source`'s discovery finds `fingerprint` for it again.
    pub(crate) fn keep_unchanged(
        &mut self,
        previous: &Self,
        source: Source,
        locator: &Path,
        fingerprint: Option<CachedFingerprint>,
    ) {
        for (kept, earlier) in self.sets_beside(previous) {
            if let Some(found) = earlier
                .get(locator)
                .filter(|found| found.is_unchanged(source, fingerprint))
            {
                kept.insert(locator.to_path_buf(), *found);
            }
        }
    }

    /// Copy every entry of `previous` that `source` found, for an agent whose
    /// discovery failed.
    pub(crate) fn keep_agent(&mut self, previous: &Self, source: Source) {
        for (kept, earlier) in self.sets_beside(previous) {
            kept.extend(
                earlier
                    .iter()
                    .filter(|(_, found)| found.source == source)
                    .map(|(locator, found)| (locator.clone(), *found)),
            );
        }
    }

    /// Each set paired with the same set in `other`.
    fn sets_beside<'a>(
        &'a mut self,
        other: &'a Self,
    ) -> [(&'a mut SessionsByLocator, &'a SessionsByLocator); 2] {
        [
            (&mut self.unlisted, &other.unlisted),
            (&mut self.unreadable, &other.unreadable),
        ]
    }
}

/// Every session `storage` holds, newest first, read through the session
/// cache as `scope` allows.
///
/// A session the session cache database holds as it is now is restored from
/// it instead of reparsed, and one that held no conversation is skipped
/// without being read again. A root holding a session the database does not
/// hold as it is now is read under `scope`'s refresh ownership, or read as
/// the cache holds it when `scope` reads the cache as it is. `progress`
/// hears `(done, total)` in transcripts, a session's own and each
/// sub-agent's: once every root is discovered, then as each transcript is
/// read or its session completes without reading it. Discovery repeated
/// under refresh ownership can replace the total before reading continues.
/// `meter` records what each step read. A changed session whose cached row
/// `left_out` holds keeps that row unread. A row without any time is dated
/// `environment`'s current time.
#[allow(clippy::too_many_arguments)]
pub fn load_sessions(
    environment: &Environment,
    storage: &dyn SessionStorage,
    show_last: bool,
    debug_level: Option<DebugLevel>,
    meter: AgentMeter,
    left_out: LeftOutRows,
    scope: &OperationScope,
    progress: &mut dyn FnMut(usize, usize),
) -> Result<LoadedSessions> {
    SessionLoader::new(environment, storage, show_last, debug_level, meter)
        .load(left_out, scope, progress)
}

/// `storage`'s sessions as discovery finds them now, reading only those
/// `needs_reading` picks: through the cache under `scope`'s refresh
/// ownership, as a load reads them, writing only the sessions the database
/// did not hold.
pub fn rediscover_sessions(
    environment: &Environment,
    storage: &dyn SessionStorage,
    show_last: bool,
    debug_level: Option<DebugLevel>,
    needs_reading: &dyn Fn(&SessionStub) -> bool,
    scope: &OperationScope,
) -> Result<RediscoveredSessions> {
    SessionLoader::new(
        environment,
        storage,
        show_last,
        debug_level,
        AgentMeter::UNMETERED,
    )
    .rediscover(needs_reading, scope)
}

/// The session `session_id` names, from whichever provider stores it, read
/// as [`read_resolved_session`] reads it. `None` when no provider resolves
/// the ID.
pub fn load_session_by_id(
    environment: &Environment,
    session_id: &str,
    show_last: bool,
) -> Option<SessionOutcome> {
    let (source, resolved) = super::resolve_session_id(session_id)?;
    Some(read_resolved_session(
        environment,
        source,
        resolved,
        show_last,
    ))
}

/// `source`'s session `resolved` names, as the row the list would have
/// shown: the provider's cache-or-parse step, sub-agent transcripts merged,
/// with the preview `show_last` selects. Read as the TUI's drawing thread
/// reads: without acquiring refresh ownership or writing any cache.
/// [`SessionRead::Missing`] when `environment` does not read `source`'s
/// storage.
pub fn read_resolved_session(
    environment: &Environment,
    source: Source,
    resolved: ResolvedSession,
    show_last: bool,
) -> SessionOutcome {
    let Some(storage) = environment.storage_of(source) else {
        return SessionOutcome::missing(&resolved.stub.locator);
    };
    SessionLoader::new(environment, storage, show_last, None, AgentMeter::UNMETERED)
        .read_one(&resolved.root, resolved.stub)
}

/// The sessions at `locators`, one outcome each in order, as the list
/// builds their rows: the stub `source`'s discovery lists for each file,
/// however its path is spelled, read through the cache with its sub-agent
/// transcripts, with the preview `show_last` selects. A file discovery
/// leaves out, such as a sub-agent opened by its ID, is found through
/// [`SessionStorage::rediscover_unlisted_session`]. Another copy stored
/// under the same session id is not read. Discovery is shared across the
/// requests. Read as the TUI's drawing thread reads: without acquiring
/// refresh ownership or writing any cache.
///
/// [`SessionRead::Missing`] when neither finds the file. A discovery that
/// fails, or cannot list the file's directory, reads as a
/// [`ReadError::Transient`] failure: the session may still be there,
/// unchanged.
pub fn reread_sessions(
    environment: &Environment,
    source: Source,
    locators: &[&Path],
    show_last: bool,
) -> Vec<SessionOutcome> {
    let Some(storage) = environment.storage_of(source) else {
        return all_missing(locators);
    };
    let loader = SessionLoader::new(environment, storage, show_last, None, AgentMeter::UNMETERED);
    let reread = loader.reread(locators, |root, external_titles, stubs| {
        Ok::<_, std::convert::Infallible>(loader.read_stubs_interactively(
            root,
            external_titles,
            stubs,
        ))
    });
    match reread {
        Ok(outcomes) => outcomes,
        Err(never) => match never {},
    }
}

/// [`reread_sessions`] on the TUI's background refresh thread: on a cache
/// miss, wait for `scope`'s refresh ownership as a load does, discover the
/// files' roots again under it, and cache each session the database does
/// not hold. A session read outside the database after the read lost write
/// permission waits for the refresh running then, if any. Fails on a
/// database error other than a busy lock.
pub fn cache_sessions(
    environment: &Environment,
    source: Source,
    locators: &[&Path],
    show_last: bool,
    operation: &OperationScope,
) -> Result<Vec<SessionOutcome>> {
    let Some(storage) = environment.storage_of(source) else {
        return Ok(all_missing(locators));
    };
    let loader = SessionLoader::new(environment, storage, show_last, None, AgentMeter::UNMETERED);
    loader.reread(locators, |root, external_titles, stubs| {
        loader.read_stubs(
            root,
            external_titles,
            stubs,
            StubsAfterRediscovery::SameFiles,
            operation,
        )
    })
}

/// [`SessionRead::Missing`] for each of `locators`: the environment does not
/// read their provider's storage.
fn all_missing(locators: &[&Path]) -> Vec<SessionOutcome> {
    locators
        .iter()
        .map(|locator| SessionOutcome::missing(locator))
        .collect()
}

/// One root's share of [`LoadedSessions`].
struct LoadedRoot {
    conversations: Vec<Conversation>,
    skipped: SkippedSessions,
    discovered: Vec<DiscoveredSession>,
    /// One term per reason the root's discovery ignored sessions for.
    ignored: Vec<FilterTerm>,
}

enum TranscriptProgress {
    Completed(usize),
    Rediscovered { previous: usize, current: usize },
}

/// The session cache database as a load opened it: for reading, or for
/// writing under refresh ownership.
enum CacheDatabase {
    Reader(SqliteCache),
    Writer(CacheWriter),
}

impl CacheDatabase {
    fn reader(&self) -> &SqliteCache {
        match self {
            Self::Reader(cache) => cache,
            Self::Writer(writer) => writer,
        }
    }
}

struct RootCache {
    database: CacheDatabase,
    current: HashMap<String, CachedFingerprint>,
    stored: Vec<String>,
    catalogue: HashMap<String, CatalogueSession>,
}

/// The caches a root load reads and writes.
#[derive(Clone, Copy)]
struct RootCaches<'a> {
    access: CacheAccess<'a>,
    /// The database's catalogue as the load began, and its write queue;
    /// `None` without an admitted database.
    database: Option<&'a DatabaseWrites<'a>>,
    deletions: Deletions,
}

impl RootCaches<'_> {
    /// No cache: every session reads from its transcripts, and nothing is
    /// written.
    const UNCACHED: Self = Self {
        access: CacheAccess::Uncached,
        database: None,
        deletions: Deletions::Keep,
    };
}

/// One provider's sessions, loaded in one environment with one set of
/// options.
struct SessionLoader<'a> {
    environment: &'a Environment,
    storage: &'a dyn SessionStorage,
    show_last: bool,
    debug_level: Option<DebugLevel>,
    meter: AgentMeter<'a>,
}

impl<'a> SessionLoader<'a> {
    fn new(
        environment: &'a Environment,
        storage: &'a dyn SessionStorage,
        show_last: bool,
        debug_level: Option<DebugLevel>,
        meter: AgentMeter<'a>,
    ) -> Self {
        Self {
            environment,
            storage,
            show_last,
            debug_level,
            meter,
        }
    }

    fn clock(&self) -> &'a dyn Clock {
        self.environment.clock().as_ref()
    }

    /// The session cache base this load reads and writes; `None` without one.
    fn cache_base(&self) -> Option<&'a CacheBase> {
        self.environment.cache_base()
    }
}

impl SessionLoader<'_> {
    /// Sessions are loaded on a worker thread, a root's sessions in parallel,
    /// while this thread reports each transcript read to `progress`: the
    /// callback stays on the caller's thread, and the count moves inside a
    /// session with many sub-agents instead of jumping when it completes.
    fn load(
        &self,
        left_out: LeftOutRows,
        operation: &OperationScope,
        progress: &mut dyn FnMut(usize, usize),
    ) -> Result<LoadedSessions> {
        let discovered = self.discover_every_root()?;
        let mut total = discovered
            .iter()
            .flat_map(|(_, found)| &found.stubs)
            .map(transcript_count)
            .sum();
        progress(0, total);

        let (read, transcripts_read) = mpsc::channel::<TranscriptProgress>();
        let loaded = std::thread::scope(|scope| {
            let worker =
                scope.spawn(move || self.load_discovered(discovered, left_out, operation, &read));
            let mut done = 0;
            for update in transcripts_read {
                match update {
                    TranscriptProgress::Completed(count) => done += count,
                    TranscriptProgress::Rediscovered { previous, current } => {
                        total = total.saturating_sub(previous).saturating_add(current)
                    }
                }
                progress(done, total);
            }
            worker
                .join()
                .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
        })?;
        if let Some(base) = self
            .cache_base()
            .map(CacheBase::path)
            .filter(|base| base.is_dir())
        {
            self.storage.remove_superseded_cache(base);
        }
        Ok(loaded)
    }

    /// Every discovered root's sessions, newest first, each transcript read
    /// announced on `read`.
    fn load_discovered(
        &self,
        discovered: Vec<(SessionRoot, DiscoveredSessions)>,
        left_out: LeftOutRows,
        operation: &OperationScope,
        read: &mpsc::Sender<TranscriptProgress>,
    ) -> Result<LoadedSessions> {
        let mut conversations = Vec::new();
        let mut ignored = Vec::new();
        let mut skipped = SkippedSessions::default();
        let mut discovered_sessions = Vec::new();
        for (root, found) in discovered {
            let loaded_root = self.load_root(&root, found, left_out, operation, read)?;
            ignored.extend(loaded_root.ignored);
            conversations.extend(loaded_root.conversations);
            skipped.extend(loaded_root.skipped);
            discovered_sessions.extend(loaded_root.discovered);
        }
        conversations.sort_by_key(|conversation| std::cmp::Reverse(conversation.timestamp));
        for (index, conversation) in conversations.iter_mut().enumerate() {
            conversation.index = index;
        }
        Ok(LoadedSessions {
            conversations,
            ignored,
            skipped,
            discovered: discovered_sessions,
        })
    }

    /// Every session discovery finds now, reading only those `needs_reading`
    /// picks, under `operation`'s refresh ownership.
    fn rediscover(
        &self,
        needs_reading: &dyn Fn(&SessionStub) -> bool,
        operation: &OperationScope,
    ) -> Result<RediscoveredSessions> {
        let mut rediscovered = RediscoveredSessions::default();
        for (root, found) in self.discover_every_root()? {
            let external_titles = self.storage.external_titles(&root);
            let picked = found
                .stubs
                .iter()
                .filter(|stub| needs_reading(stub))
                .cloned()
                .collect();
            let reads = self.read_stubs(
                &root,
                &external_titles,
                picked,
                StubsAfterRediscovery::Picked(needs_reading),
                operation,
            )?;
            let found = reads.rediscovered.unwrap_or(found);
            rediscovered
                .ignored
                .extend(self.report_discovery(&root, &found));
            rediscovered.unreadable_directories.extend(
                found
                    .unreadable_directories
                    .iter()
                    .map(|directory| directory.path.clone()),
            );
            rediscovered.found.extend(
                found
                    .stubs
                    .iter()
                    .map(|stub| (stub.locator.clone(), stub.fingerprint.stamp())),
            );
            rediscovered.outcomes.extend(reads.outcomes);
            rediscovered.external_titles.extend(reads.external_titles);
        }
        Ok(rediscovered)
    }

    /// Log what discovery skipped under `root` for `--debug`, and name the
    /// sessions it ignores for the user, one term per reason.
    fn report_discovery(&self, root: &SessionRoot, found: &DiscoveredSessions) -> Vec<FilterTerm> {
        if found.skipped > 0 {
            debug::debug(
                self.debug_level,
                &format!(
                    "Skipped {} {} transcripts under {} that are neither sessions nor sub-agents of one",
                    found.skipped,
                    self.storage.source().display_label(),
                    root.path.display()
                ),
            );
        }
        for directory in &found.unreadable_directories {
            debug::warn(
                self.debug_level,
                &format!(
                    "Failed to list {} sessions in {}: {}",
                    self.storage.source().display_label(),
                    directory.path.display(),
                    directory.error
                ),
            );
        }
        found
            .ignored
            .iter()
            .filter_map(|sessions| sessions.filter_term(self.storage.source()))
            .collect()
    }

    /// Every root's sessions before any root is loaded, so a progress total
    /// spans the provider instead of restarting at each root.
    fn discover_every_root(&self) -> Result<Vec<(SessionRoot, DiscoveredSessions)>> {
        self.meter.discovering();
        let discovering = self.clock().now();
        let mut discovered = Vec::new();
        for root in self.storage.roots()? {
            let found = self.storage.discover(&root, self.environment.lister())?;
            discovered.push((root, found));
        }
        self.meter.discovered(
            discovered.len(),
            discovered.iter().map(|(_, found)| found.stubs.len()).sum(),
            self.clock().elapsed_since(discovering),
        );
        Ok(discovered)
    }

    /// The conversations among `stubs`, the sessions discovered under `root`,
    /// restored or parsed in parallel. `read` hears every transcript of every
    /// stub exactly once, whatever became of it: a stub whose transcripts were
    /// not all read (restored from the cache, empty or unreadable) is topped
    /// up when it completes.
    ///
    /// A changed session whose cached row `left_out` holds is restored from
    /// that row; the cache keeps the row unchanged.
    ///
    /// Beside the conversations, return the sessions that hold no
    /// conversation, the unreadable ones, every stub with what was recorded
    /// of it, and what the root's discovery ignored.
    fn load_root(
        &self,
        root: &SessionRoot,
        mut found: DiscoveredSessions,
        left_out: LeftOutRows,
        operation: &OperationScope,
        read: &mpsc::Sender<TranscriptProgress>,
    ) -> Result<LoadedRoot> {
        let stubs = std::mem::take(&mut found.stubs);
        let mut loaded =
            self.load_root_sessions(root, stubs, &mut found, left_out, operation, read)?;
        loaded.ignored = self.report_discovery(root, &found);
        Ok(loaded)
    }

    /// [`Self::load_root`]'s sessions. `found` is the root's discovery
    /// without its stubs.
    ///
    /// When the database holds every stub as it is now, each restores from it
    /// without refresh ownership. Otherwise the root is read as `operation`
    /// allows: from the cache as it is, or under its refresh ownership. Under
    /// ownership the load discovers the root again, replaces `found` with
    /// that discovery and reconciles deletions from it when it listed every
    /// directory.
    fn load_root_sessions(
        &self,
        root: &SessionRoot,
        stubs: Vec<SessionStub>,
        found: &mut DiscoveredSessions,
        left_out: LeftOutRows,
        operation: &OperationScope,
        read: &mpsc::Sender<TranscriptProgress>,
    ) -> Result<LoadedRoot> {
        if stubs.is_empty()
            && self
                .cache_base()
                .is_none_or(|base| !base.path().join(sqlite_cache::DATABASE_FILE_NAME).exists())
        {
            // Nothing to read, and no cache to remove sessions from.
            return Ok(self.load_root_writing(root, stubs, left_out, read, RootCaches::UNCACHED));
        }
        let deletions = Deletions::of(found);
        let probed = match self.open_database(root, &stubs, DatabaseReader::WithoutWaiting) {
            Err(error) if sqlite_cache::is_busy_error(&error) => None,
            probed => probed?,
        };
        match probed {
            Some(snapshot) if !self.needs_refresh(root, &stubs, left_out, &snapshot, deletions) => {
                let caches = RootCaches {
                    access: CacheAccess::AsIs,
                    database: None,
                    deletions,
                };
                return Ok(self.load_database_root(root, stubs, left_out, read, snapshot, caches));
            }
            snapshot => self
                .meter
                .refreshing(transcripts_to_parse(&stubs, snapshot.as_ref())),
        }
        match operation.access()? {
            OperationAccess::Uncached => {
                Ok(self.load_root_writing(root, stubs, left_out, read, RootCaches::UNCACHED))
            }
            OperationAccess::AsIs => self.load_root_as_is(root, stubs, left_out, operation, read),
            OperationAccess::Owner(owner) => {
                self.load_root_owned(root, stubs, found, left_out, &owner, read)
            }
        }
    }

    /// [`Self::load_root_sessions`] under `owner`'s refresh ownership: the
    /// root discovered again under the writer lock, every session the
    /// database does not hold as it is now parsed and written. A discovery
    /// that fails keeps the one from before. A write batch that fails
    /// revokes `owner`'s write permission, and the root's other sessions are
    /// read without writing.
    fn load_root_owned(
        &self,
        root: &SessionRoot,
        stubs: Vec<SessionStub>,
        found: &mut DiscoveredSessions,
        left_out: LeftOutRows,
        owner: &RefreshToken,
        read: &mpsc::Sender<TranscriptProgress>,
    ) -> Result<LoadedRoot> {
        let stubs = match self.rediscover_owned(root, owner) {
            Some(rediscovered) => adopt_rediscovery(stubs, found, rediscovered, read),
            None => stubs,
        };
        let snapshot = self.open_database_for_refresh(root, &stubs, owner)?;
        let caches = RootCaches {
            access: CacheAccess::Refresh(owner),
            database: None,
            deletions: Deletions::of(found),
        };
        let Some(snapshot) = snapshot else {
            return Ok(self.load_root_writing(root, stubs, left_out, read, caches));
        };
        let loaded = self.load_database_root(root, stubs, left_out, read, snapshot, caches);
        if owner.can_publish()
            && let Some(base) = self.cache_base()
        {
            shard_files::remove(base.path(), self.storage.cache(), &root.path);
        }
        Ok(loaded)
    }

    /// [`Self::load_root_sessions`] from the cache as it is: every session
    /// the database holds restores from it, whether or not its transcript
    /// changed since, and nothing is parsed. Waits while the database is
    /// exclusively locked.
    fn load_root_as_is(
        &self,
        root: &SessionRoot,
        stubs: Vec<SessionStub>,
        left_out: LeftOutRows,
        operation: &OperationScope,
        read: &mpsc::Sender<TranscriptProgress>,
    ) -> Result<LoadedRoot> {
        let snapshot = operation.read_when_unlocked(|| {
            self.open_database(root, &stubs, DatabaseReader::WithoutWaiting)
        })?;
        let caches = RootCaches {
            access: CacheAccess::AsIs,
            database: None,
            deletions: Deletions::Keep,
        };
        Ok(match snapshot {
            Some(snapshot) => {
                self.load_database_root(root, stubs, left_out, read, snapshot, caches)
            }
            None => self.load_root_writing(root, stubs, left_out, read, caches),
        })
    }

    /// True when `snapshot` does not hold every stub as it is now, or holds
    /// a session to remove, as `deletions` decides.
    fn needs_refresh(
        &self,
        root: &SessionRoot,
        stubs: &[SessionStub],
        left_out: LeftOutRows,
        snapshot: &RootCache,
        deletions: Deletions,
    ) -> bool {
        let keys: HashSet<_> = stubs.iter().map(|stub| stub.cache_key.as_str()).collect();
        if deletions == Deletions::Reconcile
            && snapshot
                .stored
                .iter()
                .any(|key| !keys.contains(key.as_str()))
        {
            return true;
        }
        let titles = self.storage.external_titles(root);
        stubs.iter().any(|stub| {
            if snapshot.holds(stub) {
                return false;
            }
            if let Some(leaves_out) = left_out.0
                && let Some(CatalogueSession::Listed(row)) = snapshot.catalogue.get(&stub.cache_key)
            {
                let mut row = (**row).clone();
                if let Some(title) = titles.get(&row.session_id) {
                    apply_external_title(&mut row, title);
                }
                if leaves_out(&row) {
                    return false;
                }
            }
            true
        })
    }

    /// [`Self::load_root_writing`] against `snapshot`, the root's database
    /// as it is now, as `caches` reads and writes it.
    fn load_database_root(
        &self,
        root: &SessionRoot,
        stubs: Vec<SessionStub>,
        left_out: LeftOutRows,
        read: &mpsc::Sender<TranscriptProgress>,
        snapshot: RootCache,
        caches: RootCaches,
    ) -> LoadedRoot {
        let RootCache {
            database,
            current,
            stored,
            catalogue,
        } = snapshot;
        let reconciles_deletions = caches.deletions == Deletions::Reconcile;
        let debug_level = self.debug_level;
        std::thread::scope(|scope| {
            let (writes, received) = mpsc::sync_channel(DATABASE_WRITE_QUEUE_LENGTH);
            let writer = match database {
                CacheDatabase::Writer(writer) if caches.access.owner().is_some() => Some(
                    scope.spawn(move || sqlite_cache::apply_writes(writer, received, debug_level)),
                ),
                CacheDatabase::Writer(_) | CacheDatabase::Reader(_) => {
                    drop(received);
                    None
                }
            };
            let database = DatabaseWrites {
                current: &current,
                writes: &writes,
                catalogue: &catalogue,
            };
            let loaded = self.load_root_writing(
                root,
                stubs,
                left_out,
                read,
                RootCaches {
                    database: Some(&database),
                    ..caches
                },
            );
            let discovered: HashSet<&str> = loaded
                .discovered
                .iter()
                .map(|session| session.stub.cache_key.as_str())
                .collect();
            let removed: Vec<String> = stored
                .iter()
                .filter(|cache_key| !discovered.contains(cache_key.as_str()))
                .cloned()
                .collect();
            if reconciles_deletions && !removed.is_empty() && caches.access.owner().is_some() {
                let _ = writes.send(CacheWrite::Removed {
                    storage: self.storage.cache(),
                    root: root.path.clone(),
                    cache_keys: removed,
                });
            }
            drop(writes);
            if let Some(writer) = writer {
                writer
                    .join()
                    .unwrap_or_else(|panic| std::panic::resume_unwind(panic));
            }
            loaded
        })
    }

    /// The root's cache as it is now, read without refresh ownership through
    /// `reader`.
    fn open_database(
        &self,
        root: &SessionRoot,
        stubs: &[SessionStub],
        reader: DatabaseReader,
    ) -> Result<Option<RootCache>> {
        if !self.may_hold_root(stubs) {
            return Ok(None);
        }
        self.open_database_reader(reader)?
            .map(|database| self.read_root_cache(root, database))
            .transpose()
    }

    /// The root's cache for `owner`'s refresh, read in one writer-locked
    /// batch. The batch's commit restarts the takeover window, so a slow
    /// comparison cannot hand ownership to a waiter before the first parse.
    /// `None` once `owner`'s write permission is revoked: the root is then
    /// read from the cache as it is.
    fn open_database_for_refresh(
        &self,
        root: &SessionRoot,
        stubs: &[SessionStub],
        owner: &RefreshToken,
    ) -> Result<Option<RootCache>> {
        if !self.may_hold_root(stubs) {
            return Ok(None);
        }
        let Some(database) = self.open_database_writer(owner)? else {
            return Ok(None);
        };
        let (snapshot, coordinated) = owner.read(|| self.read_root_cache(root, database));
        self.report_read_coordination(coordinated);
        let snapshot = snapshot?;
        Ok(owner.can_publish().then_some(snapshot))
    }

    /// False when the database cannot hold the root: no cache base, or no
    /// database file and no stubs to write.
    fn may_hold_root(&self, stubs: &[SessionStub]) -> bool {
        self.cache_base().is_some_and(|base| {
            !stubs.is_empty() || base.path().join(sqlite_cache::DATABASE_FILE_NAME).exists()
        })
    }

    /// Current fingerprints and all stored keys are separate: unfinished
    /// refreshes must remain eligible for deletion reconciliation.
    fn read_root_cache(&self, root: &SessionRoot, database: CacheDatabase) -> Result<RootCache> {
        self.meter.reading_cache();
        let reading = self.clock().now();
        let cache = database.reader();
        let current = cache.current_fingerprints(self.storage.cache(), &root.path)?;
        let stored = cache.stored_keys(self.storage.cache(), &root.path)?;
        let catalogue = cache.catalogue(
            self.storage.cache(),
            self.storage.source(),
            &root.path,
            self.show_last,
        )?;
        self.meter
            .cache_read(catalogue.len(), self.clock().elapsed_since(reading));
        Ok(RootCache {
            database,
            current,
            stored,
            catalogue,
        })
    }

    /// [`Self::load_root`]'s sessions, writing to `caches`'s database each
    /// session it parses, and reading every session that database does not
    /// hold as it is now from its transcripts. Without a database, every
    /// session is read from its transcripts. A load that reads the cache as
    /// it is parses nothing: it restores what the database holds, and lists
    /// no row for a session the database lacks.
    ///
    /// Sessions restored from the database or a left-out row are restored in
    /// one pass without the writer lock. Sessions that parse are grouped by
    /// [`Self::parse_in_groups`].
    fn load_root_writing(
        &self,
        root: &SessionRoot,
        stubs: Vec<SessionStub>,
        left_out: LeftOutRows,
        read: &mpsc::Sender<TranscriptProgress>,
        caches: RootCaches<'_>,
    ) -> LoadedRoot {
        let database = caches.database;
        let reads_as_is = matches!(caches.access, CacheAccess::AsIs);
        let external_titles = self
            .read_under_writer_lock(caches.access.owner(), || self.storage.external_titles(root));
        let row_sources: Vec<RowSource> = stubs
            .iter()
            .map(|stub| row_source(stub, database, left_out, &external_titles, reads_as_is))
            .collect();
        self.meter.refreshing(
            stubs
                .iter()
                .zip(&row_sources)
                .filter(|(_, source)| matches!(source, RowSource::Transcripts))
                .map(|(stub, _)| transcript_count(stub))
                .sum(),
        );
        let mut conversations = Vec::new();
        let mut skipped = SkippedSessions::default();
        let mut recorded = Vec::with_capacity(stubs.len());

        let mut outcomes: Vec<Option<RootRead>> = stubs
            .par_iter()
            .zip(row_sources)
            .map(|(stub, source)| {
                let restored = match source {
                    RowSource::Transcripts => return None,
                    RowSource::Database(catalogue) => {
                        RootRead::Read(restore_from_catalogue(catalogue, stub, &external_titles))
                    }
                    RowSource::LeftOut(row) => RootRead::Read(RawRead::Restored(*row)),
                    RowSource::Stale(catalogue) => {
                        RootRead::Stale(restore_from_catalogue(catalogue, stub, &external_titles))
                    }
                    RowSource::NotCached => RootRead::NotCached,
                };
                let _ = read.send(TranscriptProgress::Completed(transcript_count(stub)));
                Some(restored)
            })
            .collect();
        let to_parse: Vec<usize> = outcomes
            .iter()
            .enumerate()
            .filter(|(_, outcome)| outcome.is_none())
            .map(|(index, _)| index)
            .collect();
        self.parse_in_groups(
            caches.access,
            &to_parse,
            |&index| {
                let stub = &stubs[index];
                let announced = AtomicUsize::new(0);
                let on_transcript_read = || {
                    announced.fetch_add(1, Ordering::Relaxed);
                    self.meter.transcript_read();
                    let _ = read.send(TranscriptProgress::Completed(1));
                };
                let outcome = self.parse(root, stub, &on_transcript_read);
                let record =
                    database.and_then(|_| cache_record(self.storage, root, stub, &outcome));
                let unannounced =
                    transcript_count(stub).saturating_sub(announced.load(Ordering::Relaxed));
                if unannounced > 0 {
                    let _ = read.send(TranscriptProgress::Completed(unannounced));
                }
                (outcome, record)
            },
            |group, parsed| {
                for (&index, (outcome, record)) in group.iter().zip(parsed) {
                    if caches.access.owner().is_some()
                        && let (Some(database), Some(record)) = (database, record)
                    {
                        database.write(record);
                    }
                    outcomes[index] = Some(RootRead::Read(outcome));
                }
            },
        );

        let outcomes = outcomes
            .into_iter()
            .map(|outcome| outcome.expect("every session restored or parsed"));
        let mut stale_copies = Vec::with_capacity(stubs.len());
        for (stub, outcome) in stubs.iter().zip(outcomes) {
            let (raw, is_stale) = match outcome {
                RootRead::Read(raw) => (raw, false),
                RootRead::Stale(raw) => (raw, true),
                RootRead::NotCached => {
                    recorded.push(None);
                    stale_copies.push(Some(StaleCopy { cached_until: None }));
                    continue;
                }
            };
            let found = FoundSession {
                source: self.storage.source(),
                fingerprint: stub.fingerprint.stamp(),
                has_transient_read_error: false,
            };
            let mut stale_copy = is_stale.then_some(StaleCopy { cached_until: None });
            recorded.push(match self.session_read(stub, raw) {
                SessionRead::Listed(conversation) => {
                    let session = RecordedSession {
                        project_dir: conversation.cwd.clone(),
                        session_id: conversation.session_id.clone(),
                    };
                    if let Some(stale_copy) = &mut stale_copy {
                        stale_copy.cached_until = Some(last_message_time(&conversation));
                    }
                    conversations.push(*conversation);
                    Some(session)
                }
                // A stale copy stays out of the skipped sessions, so a
                // refresh reads the session again.
                _ if is_stale => None,
                SessionRead::Empty => {
                    skipped.unlisted.insert(stub.locator.clone(), found);
                    None
                }
                SessionRead::Failed(ReadError::Permanent) => {
                    skipped.unreadable.insert(stub.locator.clone(), found);
                    None
                }
                SessionRead::Failed(ReadError::Transient) | SessionRead::Missing => None,
            });
            stale_copies.push(stale_copy);
        }
        LoadedRoot {
            conversations,
            skipped,
            discovered: stubs
                .into_iter()
                .zip(recorded)
                .zip(stale_copies)
                .map(|((stub, recorded), stale)| DiscoveredSession {
                    stub,
                    recorded,
                    stale,
                })
                .collect(),
            ignored: Vec::new(),
        }
    }

    /// `raw`, the read of the session at `stub`, as the list shows it.
    fn session_read(&self, stub: &SessionStub, raw: RawRead) -> SessionRead {
        match raw {
            RawRead::Restored(conversation) => {
                SessionRead::Listed(Box::new(self.resolve_preview_and_project(conversation)))
            }
            RawRead::Parsed(conversation) => {
                SessionRead::Listed(Box::new(self.listed_row(stub, conversation)))
            }
            RawRead::Empty => SessionRead::Empty,
            RawRead::Failed(error) => SessionRead::Failed(error),
        }
    }

    /// `conversation`, parsed from `stub`'s transcripts, as the list shows it,
    /// carrying the fingerprint of the stub it was read from.
    fn listed_row(&self, stub: &SessionStub, conversation: Conversation) -> Conversation {
        let mut conversation = self.resolve_preview_and_project(conversation);
        conversation.fingerprint = stub.fingerprint.stamp();
        conversation
    }

    /// Fills the two fields neither the parser nor the cache can: the preview
    /// the `--first` / `--last` option selects, and the project the row is
    /// filed under.
    fn resolve_preview_and_project(&self, mut conversation: Conversation) -> Conversation {
        conversation.preview = if self.show_last {
            conversation.preview_last.clone()
        } else {
            conversation.preview_first.clone()
        };
        let project_path = project_path_of(&conversation);
        conversation.project_name = Some(format_short_name_from_path(&project_path));
        conversation.project_path = Some(project_path);
        conversation
    }

    /// The session at `stub` read from its transcripts, the parse recorded in
    /// the meter.
    fn parse(
        &self,
        root: &SessionRoot,
        stub: &SessionStub,
        on_transcript_read: &(dyn Fn() + Sync),
    ) -> RawRead {
        let parsing = self.clock().now();
        let outcome = parse_session(
            self.storage,
            stub,
            root,
            self.debug_level,
            on_transcript_read,
            self.clock(),
        );
        self.meter.session_parsed(
            transcript_count(stub),
            stub.fingerprint.size,
            self.clock().elapsed_since(parsing),
        );
        outcome
    }
}

/// The source of a session's row in a root load.
enum RowSource<'a> {
    /// The database holds the transcript as it is now.
    Database(&'a CatalogueSession),
    /// The transcript changed, but the command leaves the database's row
    /// out: the row restores as the database holds it, and stays there.
    LeftOut(Box<Conversation>),
    /// The transcript changed since the database's copy, and the load reads
    /// the cache as it is: the copy restores.
    Stale(&'a CatalogueSession),
    /// The copy is missing from the database, and the load reads the cache
    /// as it is.
    NotCached,
    /// The session is read from its transcripts.
    Transcripts,
}

/// One root session's read, as [`SessionLoader::load_root_writing`] collects
/// it.
enum RootRead {
    /// Restored from a copy that matches the transcript, or parsed.
    Read(RawRead),
    /// Restored from a copy older than the transcript.
    Stale(RawRead),
    /// The copy is missing from the cache, and nothing parsed the
    /// transcript.
    NotCached,
}

/// The source of `stub`'s row in a root load, against `database`, the root's
/// database as the load began; `left_out` and the titles stored beside the
/// transcripts, `external_titles`, decide a changed session's. A load that
/// `reads_as_is` takes a changed session's row from the database's copy.
fn row_source<'a>(
    stub: &SessionStub,
    database: Option<&'a DatabaseWrites<'a>>,
    left_out: LeftOutRows,
    external_titles: &HashMap<String, SessionTitle>,
    reads_as_is: bool,
) -> RowSource<'a> {
    let Some(database) = database else {
        return if reads_as_is {
            RowSource::NotCached
        } else {
            RowSource::Transcripts
        };
    };
    if let Some(catalogue) = database.catalogue_of(stub) {
        return RowSource::Database(catalogue);
    }
    if let Some(leaves_out) = left_out.0
        && let Some(CatalogueSession::Listed(row)) = database.catalogue.get(&stub.cache_key)
    {
        let row = restored_row(row, stub, external_titles);
        if leaves_out(&row) {
            return RowSource::LeftOut(Box::new(row));
        }
    }
    if !reads_as_is {
        return RowSource::Transcripts;
    }
    database
        .catalogue
        .get(&stub.cache_key)
        .map_or(RowSource::NotCached, RowSource::Stale)
}

/// The time of `row`'s last message, or the row's own time when no message
/// has one.
fn last_message_time(row: &Conversation) -> DateTime<Local> {
    row.message_times
        .last_second()
        .and_then(|second| Local.timestamp_opt(second, 0).single())
        .unwrap_or(row.timestamp)
}

/// How many transcripts `stub` names: its own and each sub-agent's.
fn transcript_count(stub: &SessionStub) -> usize {
    1 + stub.subagents.len()
}

/// Replace `found`, the discovery `stubs` came from, with `rediscovered`,
/// the root's discovery repeated after acquiring refresh ownership, and
/// return its stubs. The progress total follows the change.
fn adopt_rediscovery(
    stubs: Vec<SessionStub>,
    found: &mut DiscoveredSessions,
    mut rediscovered: DiscoveredSessions,
    read: &mpsc::Sender<TranscriptProgress>,
) -> Vec<SessionStub> {
    let previous = stubs.iter().map(transcript_count).sum();
    let current = rediscovered.stubs.iter().map(transcript_count).sum();
    if previous != current {
        let _ = read.send(TranscriptProgress::Rediscovered { previous, current });
    }
    let stubs = std::mem::take(&mut rediscovered.stubs);
    *found = rediscovered;
    stubs
}

/// The transcripts a refresh of `stubs` parses: those of each stub that
/// `snapshot`, the cache read before acquiring refresh ownership, does not
/// hold as it is now, or of every stub without a snapshot.
fn transcripts_to_parse(stubs: &[SessionStub], snapshot: Option<&RootCache>) -> usize {
    stubs
        .iter()
        .filter(|stub| snapshot.is_none_or(|snapshot| !snapshot.holds(stub)))
        .map(transcript_count)
        .sum()
}

impl RootCache {
    /// True when the database holds `stub`'s transcript as it is now, with
    /// its catalogue row.
    fn holds(&self, stub: &SessionStub) -> bool {
        holds_session(&self.current, &self.catalogue, stub)
    }
}

/// Sessions read ahead of the database writer, so a load holds no more than
/// this many sessions' entries waiting to be written.
const DATABASE_WRITE_QUEUE_LENGTH: usize = 4;

/// A root load's writes to the session cache database, and the fingerprint
/// the database held for each session under the root when the load began.
struct DatabaseWrites<'a> {
    current: &'a HashMap<String, CachedFingerprint>,
    writes: &'a mpsc::SyncSender<CacheWrite>,
    catalogue: &'a HashMap<String, CatalogueSession>,
}

impl<'a> DatabaseWrites<'a> {
    /// `stub`'s catalogue row, when the database holds its transcript as it
    /// is now.
    fn catalogue_of(&self, stub: &SessionStub) -> Option<&'a CatalogueSession> {
        holds_session(self.current, self.catalogue, stub)
            .then(|| self.catalogue.get(&stub.cache_key))
            .flatten()
    }

    fn write(&self, record: SessionRecord) {
        let _ = self.writes.send(CacheWrite::Session(Box::new(record)));
    }
}

/// True when `current`, the database's fingerprints by cache key, holds
/// `stub`'s transcript as it is now.
fn database_holds(current: &HashMap<String, CachedFingerprint>, stub: &SessionStub) -> bool {
    stub.fingerprint
        .stamp()
        .is_some_and(|stamp| current.get(&stub.cache_key) == Some(&stamp))
}

/// [`database_holds`], with the session's catalogue row in `catalogue`.
fn holds_session(
    current: &HashMap<String, CachedFingerprint>,
    catalogue: &HashMap<String, CatalogueSession>,
    stub: &SessionStub,
) -> bool {
    database_holds(current, stub) && catalogue.contains_key(&stub.cache_key)
}

/// The read `catalogue`, the database's catalogue row of the session at
/// `stub`, restores, as [`restored_row`] restores a listed row.
fn restore_from_catalogue(
    catalogue: &CatalogueSession,
    stub: &SessionStub,
    external_titles: &HashMap<String, SessionTitle>,
) -> RawRead {
    match catalogue {
        CatalogueSession::Listed(row) => {
            RawRead::Restored(restored_row(row, stub, external_titles))
        }
        CatalogueSession::Empty => RawRead::Empty,
        CatalogueSession::Unreadable => RawRead::Failed(ReadError::Permanent),
    }
}

/// `row`, the database's catalogue row of the session at `stub`, with the
/// title stored beside the transcript when `external_titles` holds one, and
/// its sub-agent transcripts spelled as `stub` lists them.
fn restored_row(
    row: &Conversation,
    stub: &SessionStub,
    external_titles: &HashMap<String, SessionTitle>,
) -> Conversation {
    let mut row = row.clone();
    row.subagents = sqlite_cache::subagents_as_listed(stub, std::mem::take(&mut row.subagents));
    if let Some(title) = external_titles.get(&row.session_id) {
        apply_external_title(&mut row, title);
    }
    row
}

/// The session cache database's projection of `raw`, the read of
/// `storage`'s session at `stub` under `root`. `None` for a read it records
/// nothing of: a restored row, a transient failure, or a row built without a
/// transcript after a transient error, which the next load reads again.
fn cache_record(
    storage: &dyn SessionStorage,
    root: &SessionRoot,
    stub: &SessionStub,
    raw: &RawRead,
) -> Option<SessionRecord> {
    let unlisted =
        |status| SessionRecord::unlisted(storage.cache(), storage.source(), root, stub, status);
    match raw {
        RawRead::Parsed(conversation) if !conversation.has_transient_read_error => {
            SessionRecord::listed(storage.cache(), storage.source(), root, stub, conversation)
        }
        RawRead::Empty => unlisted(SessionStatus::Empty),
        RawRead::Failed(ReadError::Permanent) => unlisted(SessionStatus::Unreadable),
        RawRead::Parsed(_) | RawRead::Restored(_) | RawRead::Failed(ReadError::Transient) => None,
    }
}

/// A discovered session's read, before the list's preview and project are
/// filled in.
///
/// The two reads that yield no conversation are separate variants because
/// they cache differently. Only `Empty` says something about the transcript's
/// content, and content is all a fingerprint can stand for.
enum RawRead {
    /// Restored from the database's row of it.
    Restored(Conversation),
    /// Read from the transcript.
    Parsed(Conversation),
    /// Read cleanly and holds no conversation this provider lists, or the cache
    /// already records it as such. Cached against the fingerprint, so the next
    /// load skips it unopened. A change to a provider's parser therefore needs
    /// a `SessionCache::schema_version` bump to be seen, as it already does for
    /// a session that parsed into a row.
    Empty,
    /// Could not be read or parsed. Not cached, so the next launch reads it
    /// again.
    Failed(ReadError),
}

/// A session's project directory: the resolved `project_path` if set, else
/// the transcript's own `cwd`, else a placeholder.
fn project_path_of(conversation: &Conversation) -> PathBuf {
    conversation
        .project_path
        .clone()
        .or_else(|| conversation.cwd.clone())
        .unwrap_or_else(|| PathBuf::from("unknown"))
}

/// Show `title`, a title stored beside the transcript, on `conversation`.
/// True when the row's title changed.
pub fn apply_external_title(conversation: &mut Conversation, title: &SessionTitle) -> bool {
    let (shown, title) = match title {
        SessionTitle::Custom(title) => (&mut conversation.custom_title, title),
        SessionTitle::Generated(title) => (&mut conversation.summary, title),
    };
    if shown.as_deref() == Some(title.as_str()) {
        return false;
    }
    *shown = Some(title.clone());
    true
}

/// A session another provider owns is not an error: roots can overlap, and a
/// session directory override can hold a sibling agent's files. It reads as
/// empty, so this root stops opening it — the provider that owns it lists it
/// under its own root.
fn parse_session(
    storage: &dyn SessionStorage,
    stub: &SessionStub,
    root: &SessionRoot,
    debug_level: Option<DebugLevel>,
    on_transcript_read: &(dyn Fn() + Sync),
    clock: &dyn Clock,
) -> RawRead {
    match storage.parse_session(stub, root, debug_level, on_transcript_read, clock) {
        Ok(Some(conversation)) if conversation.source == storage.source() => {
            RawRead::Parsed(conversation)
        }
        Ok(_) => RawRead::Empty,
        Err(error) => {
            debug::warn(
                debug_level,
                &format!(
                    "Failed to parse {} session {}: {error}",
                    storage.source().list_label(),
                    stub.locator.display()
                ),
            );
            RawRead::Failed(ReadError::of(&error))
        }
    }
}

/// True when `error` can clear with the transcript unchanged, such as a
/// locked transcript or database.
fn is_transient(error: &AppError) -> bool {
    match error {
        AppError::Io(error) => {
            is_locked_by_another_program(error)
                || matches!(
                    error.kind(),
                    std::io::ErrorKind::ResourceBusy
                        | std::io::ErrorKind::WouldBlock
                        | std::io::ErrorKind::TimedOut
                        | std::io::ErrorKind::Interrupted
                )
        }
        AppError::SessionListUnreadable {
            reason: SessionDatabaseFailure::Locked,
            ..
        } => true,
        _ => false,
    }
}

/// Windows' error for a file another program opened without sharing it.
#[cfg(windows)]
const ERROR_SHARING_VIOLATION: i32 = 32;
/// Windows' error for a file range another program locked.
#[cfg(windows)]
const ERROR_LOCK_VIOLATION: i32 = 33;

/// True for Windows' refusal to read a file another program locked.
#[cfg(windows)]
fn is_locked_by_another_program(error: &std::io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(ERROR_SHARING_VIOLATION | ERROR_LOCK_VIOLATION)
    )
}

/// Unix file locks are advisory: they never fail a read.
#[cfg(not(windows))]
fn is_locked_by_another_program(_: &std::io::Error) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::cache_access::READ_BATCH_LENGTH;
    use super::*;
    use crate::clock::SystemClock;
    use crate::history::environment::test_support::waiting_operation;
    use crate::history::load_meter::{AgentLoad, ItemCount, LoadMeter, LoadStep, StepProgress};
    use crate::history::parser::{SubagentContribution, SubagentReads};
    use crate::history::provider::walk::{DirectoryLister, SystemLister};
    use crate::history::provider::{Fingerprint, IgnoredSessions};
    use crate::history::sqlite_cache::refresh::{Acquisition, RefreshOwnership};
    use crate::history::{AwaitedRefresh, OtherRearview, ProcessIdentity};
    use chrono::TimeZone;
    use std::collections::HashSet;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant, UNIX_EPOCH};

    /// The environment of a load through the session cache under
    /// `cache_base`, on the system clock: these tests interleave loads with
    /// real threads and processes. A load reads only the storage it is
    /// given.
    fn environment_of(cache_base: &Path) -> Environment {
        Environment::new(
            Vec::new(),
            Some(cache_base.to_path_buf()),
            ProcessIdentity::default(),
            Arc::new(SystemClock::new()),
            Arc::new(SystemLister),
        )
    }

    /// The cache in `directory` as a rearview of its own reaches it.
    fn rearview(directory: &Path) -> CacheBase {
        crate::history::sqlite_cache::refresh::tests::rearview(directory)
    }

    /// A cache base holding an admitted database without sessions, as a
    /// first build leaves it: a load through it caches what it reads.
    fn admitted_cache_base() -> tempfile::TempDir {
        let base = tempfile::tempdir().unwrap();
        admit_database(base.path());
        base
    }

    fn load_with_cache(
        storage: &dyn SessionStorage,
        cache_base: &Path,
        show_last: bool,
        debug_level: Option<DebugLevel>,
    ) -> Result<LoadedSessions> {
        load_leaving_out_with_cache(
            storage,
            cache_base,
            show_last,
            debug_level,
            LeftOutRows::NONE,
        )
    }

    fn load_leaving_out_with_cache(
        storage: &dyn SessionStorage,
        cache_base: &Path,
        show_last: bool,
        debug_level: Option<DebugLevel>,
        left_out: LeftOutRows,
    ) -> Result<LoadedSessions> {
        let environment = environment_of(cache_base);
        let operation = waiting_operation(&environment);
        let loaded = load_sessions(
            &environment,
            storage,
            show_last,
            debug_level,
            AgentMeter::UNMETERED,
            left_out,
            &operation,
            &mut |_, _| {},
        );
        operation.finish().and(loaded)
    }

    fn load_metered_with_cache(
        storage: &dyn SessionStorage,
        cache_base: &Path,
        meter: AgentMeter,
        left_out: LeftOutRows,
    ) -> Result<LoadedSessions> {
        let environment = environment_of(cache_base);
        let operation = waiting_operation(&environment);
        let loaded = load_sessions(
            &environment,
            storage,
            false,
            None,
            meter,
            left_out,
            &operation,
            &mut |_, _| {},
        );
        operation.finish().and(loaded)
    }

    fn load_sessions_with_cache(
        storage: &dyn SessionStorage,
        cache_base: &Path,
        show_last: bool,
        debug_level: Option<DebugLevel>,
    ) -> Result<Vec<Conversation>> {
        load_with_cache(storage, cache_base, show_last, debug_level)
            .map(|loaded| loaded.conversations)
    }

    fn rediscover_sessions_with_cache(
        storage: &dyn SessionStorage,
        cache_base: &Path,
        show_last: bool,
        needs_reading: &dyn Fn(&SessionStub) -> bool,
    ) -> Result<RediscoveredSessions> {
        let environment = environment_of(cache_base);
        let operation = waiting_operation(&environment);
        let rediscovered = rediscover_sessions(
            &environment,
            storage,
            show_last,
            None,
            needs_reading,
            &operation,
        );
        operation.finish().and(rediscovered)
    }

    /// [`cache_sessions`] for one session in the environment of
    /// `cache_base`, through `storage` whatever its source.
    fn cache_session_with_cache(
        storage: &dyn SessionStorage,
        cache_base: &Path,
        locator: &Path,
        show_last: bool,
    ) -> SessionOutcome {
        let environment = environment_of(cache_base);
        SessionLoader::new(
            &environment,
            storage,
            show_last,
            None,
            AgentMeter::UNMETERED,
        )
        .reread_caching(&[locator])
        .pop()
        .expect("one result per locator")
    }

    impl SessionLoader<'_> {
        /// [`cache_sessions`]'s reread of `locators` through this loader,
        /// under an operation of its own.
        fn reread_caching(&self, locators: &[&Path]) -> Vec<SessionOutcome> {
            let operation = waiting_operation(self.environment);
            let outcomes = self
                .reread(locators, |root, external_titles, stubs| {
                    self.read_stubs(
                        root,
                        external_titles,
                        stubs,
                        StubsAfterRediscovery::SameFiles,
                        &operation,
                    )
                })
                .unwrap();
            operation.finish().unwrap();
            outcomes
        }

        /// [`Self::load`] under an operation of its own.
        fn load_waiting(&self) -> Result<LoadedSessions> {
            let operation = waiting_operation(self.environment);
            let loaded = self.load(LeftOutRows::NONE, &operation, &mut |_, _| {});
            operation.finish().and(loaded)
        }
    }

    /// Virtual locators verify that the shared load loop leaves a provider's
    /// storage interpretation to the provider.
    struct VirtualStorage {
        roots: Vec<SessionRoot>,
        stubs: Vec<SessionStub>,
        /// One entry per `parse_session` call, by session id, so a test can
        /// assert that a load did not read a session at all.
        parsed: Mutex<Vec<String>>,
        discoveries: Mutex<usize>,
        /// The results of the next discoveries, in order, before discovery
        /// falls back to `stubs`.
        discovery_results: Mutex<std::collections::VecDeque<Result<DiscoveredSessions>>>,
        /// Called before each parse with the number of parses before it, so
        /// a test can observe or change the cache while the load parses.
        before_parse: Option<Box<dyn Fn(usize) + Sync>>,
        holding_nothing: HashSet<String>,
        failing_transiently: HashSet<String>,
        unreadable: HashSet<String>,
        /// The sessions every root reports as ignored.
        ignored: Vec<IgnoredSessions>,
        /// The cache base each `remove_superseded_cache` call named, with the
        /// sessions its database held at the time.
        superseded_cache_removals: Mutex<Vec<(PathBuf, usize)>>,
    }

    impl VirtualStorage {
        fn new(stubs: Vec<SessionStub>) -> Self {
            Self {
                roots: vec![SessionRoot::new("container.db")],
                stubs,
                parsed: Mutex::new(Vec::new()),
                discoveries: Mutex::new(0),
                discovery_results: Mutex::default(),
                before_parse: None,
                holding_nothing: HashSet::new(),
                failing_transiently: HashSet::new(),
                unreadable: HashSet::new(),
                ignored: Vec::new(),
                superseded_cache_removals: Mutex::new(Vec::new()),
            }
        }

        fn with_ignored(mut self, count: usize, reason: &'static str) -> Self {
            self.ignored.push(IgnoredSessions { count, reason });
            self
        }

        /// Transcripts, a session's own or a sub-agent's, that read and hold no
        /// conversation.
        fn holding_nothing<const N: usize>(mut self, ids: [&str; N]) -> Self {
            self.holding_nothing = ids.iter().map(|id| (*id).to_owned()).collect();
            self
        }

        /// Transcripts, a session's own or a sub-agent's, that fail with a
        /// transient error.
        fn failing_transiently<const N: usize>(mut self, ids: [&str; N]) -> Self {
            self.failing_transiently = ids.iter().map(|id| (*id).to_owned()).collect();
            self
        }

        /// Transcripts, a session's own or a sub-agent's, that fail with a
        /// permanent error.
        fn unreadable<const N: usize>(mut self, ids: [&str; N]) -> Self {
            self.unreadable = ids.iter().map(|id| (*id).to_owned()).collect();
            self
        }

        /// A session's own transcript or a sub-agent's, failing or holding
        /// nothing as the test named it.
        fn parse_transcript(&self, transcript: &Path) -> Result<Option<Conversation>> {
            let name = transcript_name(transcript);
            if self.failing_transiently.contains(&name) {
                return Err(crate::error::AppError::Io(std::io::Error::from(
                    std::io::ErrorKind::ResourceBusy,
                )));
            }
            if self.unreadable.contains(&name) {
                return Err(crate::error::AppError::ConfigError(format!(
                    "cannot read {name}"
                )));
            }
            if self.holding_nothing.contains(&name) {
                return Ok(None);
            }
            let mut conversation = session(&transcript.to_string_lossy(), "text");
            conversation.source = Source::Pi;
            conversation.path = transcript.to_path_buf();
            conversation.preview_first = "opening".to_owned();
            conversation.preview_last = "closing".to_owned();
            Ok(Some(conversation))
        }

        /// The row for `stub`. [`Self::parse_transcript`] reads its transcript,
        /// then each sub-agent transcript, and each sub-agent's tokens join
        /// the row. No sub-agent turn is numbered. `on_transcript_read` is
        /// called once for each transcript read.
        fn session_row(
            &self,
            stub: &SessionStub,
            on_transcript_read: &(dyn Fn() + Sync),
        ) -> Result<Option<Conversation>> {
            let session = self.parse_transcript(&stub.locator);
            on_transcript_read();
            let Some(mut conversation) = session? else {
                return Ok(None);
            };
            let mut contributions = Vec::new();
            let mut unreadable = Vec::new();
            for subagent in &stub.subagents {
                let transcript = self.parse_transcript(subagent);
                on_transcript_read();
                match transcript {
                    Ok(row) => contributions.extend(row.map(|row| SubagentContribution {
                        total_tokens: row.total_tokens,
                        ..SubagentContribution::default()
                    })),
                    Err(error) => unreadable.push((subagent.clone(), error)),
                }
            }
            SubagentReads::new(contributions, Vec::new(), unreadable).merge_into(
                &mut conversation,
                &stub.subagents,
                None,
            );
            Ok(Some(conversation))
        }

        fn parse_count(&self) -> usize {
            self.parsed.lock().unwrap().len()
        }

        fn parsed_ids(&self) -> Vec<String> {
            let mut ids = self.parsed.lock().unwrap().clone();
            ids.sort();
            ids
        }
    }

    impl SessionStorage for VirtualStorage {
        fn source(&self) -> Source {
            Source::Pi
        }

        fn cache(&self) -> super::super::SessionCache {
            super::super::SessionCache {
                directory: "virtual-storage",
                schema_version: 1,
            }
        }

        fn roots_in(
            &self,
            _environment: &super::super::RootEnvironment,
        ) -> Result<Vec<SessionRoot>> {
            Ok(self.roots.clone())
        }

        fn discover(
            &self,
            root: &SessionRoot,
            _lister: &dyn DirectoryLister,
        ) -> Result<DiscoveredSessions> {
            *self.discoveries.lock().unwrap() += 1;
            if let Some(found) = self.discovery_results.lock().unwrap().pop_front() {
                return found;
            }
            Ok(DiscoveredSessions {
                stubs: self
                    .stubs
                    .iter()
                    .filter(|stub| stub.locator.starts_with(&root.path))
                    .cloned()
                    .collect(),
                ignored: self.ignored.clone(),
                skipped: 0,
                unreadable_directories: Vec::new(),
            })
        }

        /// Discovery lists every virtual session.
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
            stub: &SessionStub,
            _root: &SessionRoot,
            _debug_level: Option<DebugLevel>,
            on_transcript_read: &(dyn Fn() + Sync),
            _clock: &dyn Clock,
        ) -> Result<Option<Conversation>> {
            let parsed_before = {
                let mut parsed = self.parsed.lock().unwrap();
                parsed.push(transcript_name(&stub.locator));
                parsed.len() - 1
            };
            if let Some(before_parse) = &self.before_parse {
                before_parse(parsed_before);
            }
            self.session_row(stub, on_transcript_read)
        }

        fn external_titles(&self, _root: &SessionRoot) -> HashMap<String, SessionTitle> {
            HashMap::new()
        }

        fn remove_superseded_cache(&self, cache_base: &std::path::Path) {
            self.superseded_cache_removals
                .lock()
                .unwrap()
                .push((cache_base.to_path_buf(), cached_session_count(cache_base)));
        }
    }

    /// The sessions the database in `cache_base` holds; 0 without one.
    fn cached_session_count(cache_base: &Path) -> usize {
        let database = cache_base.join(sqlite_cache::DATABASE_FILE_NAME);
        if !database.is_file() {
            return 0;
        }
        rusqlite::Connection::open(database)
            .unwrap()
            .query_row("SELECT COUNT(*) FROM sessions", [], |row| {
                row.get::<_, i64>(0)
            })
            .map_or(0, |count| count as usize)
    }

    fn transcript_name(transcript: &Path) -> String {
        transcript
            .file_stem()
            .unwrap()
            .to_string_lossy()
            .into_owned()
    }

    fn virtual_stub(session_id: &str, size: u64, modified_secs: u64) -> SessionStub {
        virtual_stub_under("container.db", session_id, size, modified_secs)
    }

    fn virtual_stub_under(
        root: &str,
        session_id: &str,
        size: u64,
        modified_secs: u64,
    ) -> SessionStub {
        SessionStub {
            locator: PathBuf::from(root).join(format!("{session_id}.jsonl")),
            subagents: Vec::new(),
            cache_key: session_id.to_owned(),
            fingerprint: Fingerprint {
                size,
                modified: Some(UNIX_EPOCH + Duration::from_secs(modified_secs)),
            },
        }
    }

    fn session(id: &str, agent_text: &str) -> Conversation {
        let mut conversation = crate::search::test_fixtures::one_message_conversation(
            "",
            chrono::Local.timestamp_opt(0, 0).unwrap(),
            None,
            None,
            None,
        );
        conversation.source = Source::Pi;
        conversation.session_id = id.to_owned();
        conversation.agent_search_text = agent_text.to_owned();
        conversation.total_tokens = 10;
        conversation
    }

    impl SessionLoader<'_> {
        /// [`SessionLoader::read_one`]'s row, when the session holds one.
        fn load_one(&self, root: &SessionRoot, stub: &SessionStub) -> Option<Conversation> {
            listed_row(self.read_one(root, stub.clone()).into_read())
        }

        /// The session `stub` names under `root`, read as the TUI's
        /// background refresh thread reads it to cache it, under an
        /// operation of its own.
        fn cache_one(&self, root: &SessionRoot, stub: SessionStub) -> SessionOutcome {
            let external_titles = self.storage.external_titles(root);
            let operation = waiting_operation(self.environment);
            let outcome = self
                .read_stubs(
                    root,
                    &external_titles,
                    vec![stub],
                    StubsAfterRediscovery::SameFiles,
                    &operation,
                )
                .unwrap()
                .outcomes
                .pop()
                .expect("one read");
            operation.finish().unwrap();
            outcome
        }

        /// [`Self::load_root`] under an operation of its own.
        fn load_root_waiting(
            &self,
            root: &SessionRoot,
            found: DiscoveredSessions,
            read: &mpsc::Sender<TranscriptProgress>,
        ) -> LoadedRoot {
            let operation = waiting_operation(self.environment);
            let loaded = self
                .load_root(root, found, LeftOutRows::NONE, &operation, read)
                .unwrap();
            operation.finish().unwrap();
            loaded
        }

        /// [`Self::cache_one`]'s row, when the session holds one.
        fn cache_row(&self, root: &SessionRoot, stub: &SessionStub) -> Option<Conversation> {
            listed_row(self.cache_one(root, stub.clone()).into_read())
        }
    }

    fn listed_row(read: SessionRead) -> Option<Conversation> {
        match read {
            SessionRead::Listed(conversation) => Some(*conversation),
            SessionRead::Empty | SessionRead::Missing | SessionRead::Failed(_) => None,
        }
    }

    #[test]
    fn a_load_reuses_every_providers_cached_session() {
        for session in super::super::contract_tests::every_record_sessions() {
            let row = &session.row;
            let storage = row.source.provider().storage();
            let root = SessionRoot::new(row.path.parent().unwrap());
            let stub = SessionStub {
                locator: row.path.clone(),
                subagents: row.subagents.clone(),
                cache_key: row.session_id.clone(),
                fingerprint: Fingerprint {
                    size: 1_000,
                    modified: Some(UNIX_EPOCH + Duration::from_secs(1_000)),
                },
            };
            let base = tempfile::tempdir().unwrap();
            let meter = LoadMeter::default();
            let agent_meter = meter.agent(row.source);
            let environment = environment_of(base.path());
            let ownership =
                RefreshOwnership::acquire_for_test(environment.cache_base().unwrap()).unwrap();
            let parsed = parse_session(
                storage,
                &stub,
                &root,
                None,
                &|| {},
                environment.clock().as_ref(),
            );
            let record = cache_record(storage, &root, &stub, &parsed)
                .unwrap_or_else(|| panic!("{:?}'s session has a record", row.source));
            let mut database = SqliteCache::open_for_refresh(ownership.token()).unwrap();
            database.admit_storage(storage.cache()).unwrap();
            database.write(&record).unwrap();
            drop(database);
            drop(ownership);
            let loader = SessionLoader::new(&environment, storage, false, None, agent_meter);
            let (read, _) = mpsc::channel();

            let loaded = loader.load_root_waiting(
                &root,
                DiscoveredSessions::complete(vec![stub.clone()]),
                &read,
            );

            assert_eq!(loaded.conversations.len(), 1, "{:?}", row.source);
            assert!(loaded.conversations[0].cached_session.is_some());
            assert!(loaded.conversations[0].full_text.is_empty());
            let by_id = loader.load_one(&root, &stub).expect("catalogue lookup");
            assert!(by_id.cached_session.is_some());
            assert_eq!(
                meter
                    .agents()
                    .first()
                    .map_or(0, |agent| agent.refresh.transcripts),
                0,
                "the load doesn't parse {:?}'s cached session again",
                row.source
            );
            assert_eq!(
                loaded.conversations[0].session_id, row.session_id,
                "{:?}",
                row.source
            );
        }
    }

    /// An operation on `environment`'s session cache that waits for refresh
    /// ownership as long as another rearview keeps it, publishing its wait
    /// where `meter` shows it.
    fn operation_shown_by(environment: &Environment, meter: &LoadMeter) -> OperationScope {
        OperationScope::new(
            environment.cache_base(),
            crate::history::Patience::UntilAcquired,
            Arc::clone(meter.wait_status()),
        )
    }

    fn wait_for_refresh(meter: &LoadMeter) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !meter.is_waiting_for_refresh() {
            assert!(
                Instant::now() < deadline,
                "the load waits for refresh ownership"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn a_waiter_rediscovers_sessions_before_reconciling_deletions() {
        use crate::history::sqlite_cache::refresh::tests::OtherProcessOwner;
        let base = tempfile::tempdir().unwrap();
        let old = virtual_stub("ses_old", 100, 1_000);
        let added = virtual_stub("ses_added", 200, 2_000);
        let storage = VirtualStorage::new(vec![old.clone(), added.clone()]);
        let root = SessionRoot::new("container.db");
        let database = SqliteCache::open(&rearview(base.path())).unwrap();
        database.mark_admitted().unwrap();
        database.admit_storage(storage.cache()).unwrap();
        drop(database);
        let owner = OtherProcessOwner::hold(base.path());
        let meter = LoadMeter::default();
        let environment = environment_of(base.path());
        std::thread::scope(|scope| {
            let loader =
                SessionLoader::new(&environment, &storage, false, None, meter.agent(Source::Pi));
            let captured = DiscoveredSessions::complete(vec![old.clone()]);
            let root_ref = &root;
            let (read, updates) = mpsc::channel();
            let operation = operation_shown_by(&environment, &meter);
            let running = scope.spawn(move || {
                let loaded =
                    loader.load_root(root_ref, captured, LeftOutRows::NONE, &operation, &read);
                operation.finish().and(loaded).unwrap()
            });
            wait_for_refresh(&meter);
            assert_eq!(storage.parse_count(), 0);
            let mut writer = SqliteCache::open_for_refresh(&owner.token()).unwrap();
            for stub in [&old, &added] {
                writer
                    .write(
                        &SessionRecord::unlisted(
                            storage.cache(),
                            Source::Pi,
                            &root,
                            stub,
                            SessionStatus::Empty,
                        )
                        .unwrap(),
                    )
                    .unwrap();
            }
            drop(writer);
            owner.release();
            let loaded = running.join().unwrap();
            assert_eq!(loaded.discovered.len(), 2);
            assert!(matches!(
                updates.recv().unwrap(),
                TranscriptProgress::Rediscovered {
                    previous: 1,
                    current: 2
                }
            ));
            assert_eq!(
                storage.parse_count(),
                0,
                "the waiter's cache snapshot includes the owner's writes"
            );
        });
        let database = SqliteCache::open_waiting(&rearview(base.path()))
            .unwrap()
            .unwrap();
        let keys: HashSet<_> = database
            .stored_keys(storage.cache(), &root.path)
            .unwrap()
            .into_iter()
            .collect();
        assert_eq!(keys, HashSet::from([old.cache_key, added.cache_key]));
    }

    #[test]
    fn a_warm_sqlite_load_doesnt_wait_for_another_process() {
        use crate::history::sqlite_cache::refresh::tests::OtherProcessOwner;
        let base = tempfile::tempdir().unwrap();
        let stub = virtual_stub("ses_warm", 100, 1_000);
        let storage = VirtualStorage::new(vec![stub.clone()]);
        let root = SessionRoot::new("container.db");
        let mut database = SqliteCache::open(&rearview(base.path())).unwrap();
        database.mark_admitted().unwrap();
        database.admit_storage(storage.cache()).unwrap();
        database
            .write(
                &SessionRecord::unlisted(
                    storage.cache(),
                    Source::Pi,
                    &root,
                    &stub,
                    SessionStatus::Empty,
                )
                .unwrap(),
            )
            .unwrap();
        drop(database);
        let owner = OtherProcessOwner::hold(base.path());
        std::thread::scope(|scope| {
            let (finished, result) = mpsc::channel();
            let storage = &storage;
            let cache_base = base.path();
            scope.spawn(move || {
                finished
                    .send(load_sessions_with_cache(storage, cache_base, false, None))
                    .unwrap()
            });
            let warm = result.recv_timeout(Duration::from_secs(5));
            owner.release();
            assert!(
                warm.expect("the warm load doesn't wait for the refresh owner")
                    .unwrap()
                    .is_empty()
            );
        });
        assert_eq!(storage.parse_count(), 0);
    }

    #[test]
    fn a_deletion_waits_for_the_refresh_owner_then_leaves_the_database() {
        use crate::history::sqlite_cache::refresh::tests::OtherProcessOwner;
        let base = admitted_cache_base();
        let seeded = VirtualStorage::new(vec![virtual_stub("ses_deleted", 100, 1_000)]);
        load_sessions_with_cache(&seeded, base.path(), false, None).unwrap();
        let owner = OtherProcessOwner::hold(base.path());
        let deleted = VirtualStorage::new(Vec::new());
        let meter = LoadMeter::default();
        let environment = environment_of(base.path());
        std::thread::scope(|scope| {
            let running = scope.spawn(|| {
                let operation = operation_shown_by(&environment, &meter);
                let loaded = load_sessions(
                    &environment,
                    &deleted,
                    false,
                    None,
                    meter.agent(Source::Pi),
                    LeftOutRows::NONE,
                    &operation,
                    &mut |_, _| {},
                );
                operation.finish().and(loaded).unwrap()
            });
            wait_for_refresh(&meter);
            assert_eq!(stored_keys_under(base.path(), &seeded), ["ses_deleted"]);
            owner.release();
            assert!(running.join().unwrap().conversations.is_empty());
        });
        assert!(stored_keys_under(base.path(), &seeded).is_empty());
    }

    #[test]
    fn a_session_cache_that_cannot_be_opened_fails_the_load_without_parsing() {
        let base = admitted_cache_base();
        let stub = virtual_stub("ses_changed", 100, 1_000);
        let seeded = VirtualStorage::new(vec![stub]);
        load_sessions_with_cache(&seeded, base.path(), false, None).unwrap();
        let database = base.path().join(sqlite_cache::DATABASE_FILE_NAME);
        std::fs::remove_file(&database).unwrap();
        std::fs::create_dir(&database).unwrap();
        let changed = VirtualStorage::new(vec![virtual_stub("ses_changed", 200, 2_000)]);

        let loaded = load_sessions_with_cache(&changed, base.path(), false, None);

        assert!(loaded.is_err(), "the load fails without its session cache");
        assert_eq!(changed.parse_count(), 0);
    }

    #[test]
    fn an_interactive_read_returns_the_transcript_at_once_while_another_process_refreshes() {
        use crate::history::sqlite_cache::refresh::tests::OtherProcessOwner;
        let base = tempfile::tempdir().unwrap();
        let target = virtual_stub("ses_target", 200, 2_000);
        let storage =
            VirtualStorage::new(vec![target.clone(), virtual_stub("ses_other", 300, 3_000)]);
        let root = SessionRoot::new("container.db");
        let database = SqliteCache::open(&rearview(base.path())).unwrap();
        database.mark_admitted().unwrap();
        database.admit_storage(storage.cache()).unwrap();
        drop(database);
        let owner = OtherProcessOwner::hold(base.path());
        let environment = environment_of(base.path());
        let loader = SessionLoader::new(&environment, &storage, false, None, AgentMeter::UNMETERED);

        let started = Instant::now();
        let read = loader.read_one(&root, target);

        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the read doesn't wait for the other refresh"
        );
        assert_eq!(
            read.next(),
            NextStep::AwaitRefresh(AwaitedRefresh::AnotherRearview(OtherRearview::Refreshing))
        );
        assert!(matches!(read.read(), SessionRead::Listed(_)));
        assert_eq!(storage.parsed_ids(), ["ses_target"]);
        owner.release();
        let database = SqliteCache::open_waiting(&rearview(base.path()))
            .unwrap()
            .unwrap();
        assert!(
            database
                .stored_keys(storage.cache(), &root.path)
                .unwrap()
                .is_empty(),
            "the read doesn't write the database"
        );
    }

    #[test]
    fn an_interactive_read_returns_the_transcript_at_once_during_another_processs_rebuild() {
        use crate::history::sqlite_cache::refresh::tests::{OtherProcessOwner, OwnerActivity};
        let base = tempfile::tempdir().unwrap();
        let target = virtual_stub("ses_target", 200, 2_000);
        let storage = VirtualStorage::new(vec![target.clone()]);
        let root = SessionRoot::new("container.db");
        admit_database(base.path());
        let rebuild = OtherProcessOwner::start(base.path(), OwnerActivity::InsideRebuild);
        let environment = environment_of(base.path());
        let loader = SessionLoader::new(&environment, &storage, false, None, AgentMeter::UNMETERED);

        let started = Instant::now();
        let read = loader.read_one(&root, target);

        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the read doesn't wait for the rebuild"
        );
        assert_eq!(
            read.next(),
            NextStep::AwaitRefresh(AwaitedRefresh::AnotherRearview(OtherRearview::Rebuilding))
        );
        assert!(matches!(read.read(), SessionRead::Listed(_)));
        assert_eq!(storage.parsed_ids(), ["ses_target"]);
        drop(rebuild);
    }

    #[test]
    fn a_targeted_read_whose_repeated_discovery_fails_caches_the_requested_session() {
        let base = tempfile::tempdir().unwrap();
        let stub = virtual_stub("ses_readable", 100, 1_000);
        let storage = VirtualStorage::new(vec![stub.clone()]);
        storage
            .discovery_results
            .lock()
            .unwrap()
            .push_back(Err(AppError::SessionListUnreadable {
                reason: SessionDatabaseFailure::Locked,
                detail: "the session list is locked".to_owned(),
            }));
        let root = SessionRoot::new("container.db");
        admit_database(base.path());
        let environment = environment_of(base.path());
        let loader = SessionLoader::new(&environment, &storage, false, None, AgentMeter::UNMETERED);

        let read = loader.cache_one(&root, stub);

        assert!(matches!(read.read(), SessionRead::Listed(_)));
        assert_eq!(read.next(), NextStep::Settled);
        assert_eq!(*storage.discoveries.lock().unwrap(), 1);
        assert_eq!(storage.parsed_ids(), ["ses_readable"]);
        assert_eq!(stored_keys_under(base.path(), &storage), ["ses_readable"]);
        assert!(
            matches!(
                RefreshOwnership::try_acquire_now(&rearview(base.path())).unwrap(),
                Acquisition::Acquired(_)
            ),
            "the read releases refresh ownership"
        );
    }

    #[test]
    fn an_uncontended_load_never_reports_waiting_for_refresh_ownership() {
        use crate::agent::perf::CommandPerf;
        use crate::agent::progress::{ProgressRecords, ProgressSchedule, run_reporting_progress};
        use std::sync::atomic::AtomicBool;
        let base = admitted_cache_base();
        let storage = VirtualStorage::new(vec![
            virtual_stub("ses_first", 100, 1_000),
            virtual_stub("ses_second", 200, 2_000),
            virtual_stub("ses_third", 300, 3_000),
        ]);
        let perf = CommandPerf::new(std::sync::Arc::new(crate::clock::TestClock::default()));
        let every_millisecond = ProgressSchedule {
            first: Duration::ZERO,
            every: Duration::from_millis(1),
        };
        let mut records = Vec::new();
        let meter_reported_waiting = AtomicBool::new(false);
        let loaded = AtomicBool::new(false);

        std::thread::scope(|scope| {
            scope.spawn(|| {
                while !loaded.load(Ordering::Relaxed) {
                    if perf.load_meter().is_waiting_for_refresh() {
                        meter_reported_waiting.store(true, Ordering::Relaxed);
                    }
                }
            });
            run_reporting_progress(
                ProgressRecords::new("search", &perf, every_millisecond),
                &mut records,
                || {
                    load_metered_with_cache(
                        &storage,
                        base.path(),
                        perf.load_meter().agent(Source::Pi),
                        LeftOutRows::NONE,
                    )
                    .unwrap()
                },
            );
            loaded.store(true, Ordering::Relaxed);
        });

        assert_eq!(
            storage.parse_count(),
            3,
            "the load parses every session"
        );
        assert!(!meter_reported_waiting.load(Ordering::Relaxed));
        let records = String::from_utf8(records).unwrap();
        assert!(!records.is_empty());
        assert!(!records.contains("waiting="), "{records}");
    }

    /// The database holds one row per session, with each sub-agent
    /// transcript inside its session's stub.
    #[test]
    fn every_cached_session_is_one_stub() {
        let cache_base = admitted_cache_base();
        let mut with_subagents = virtual_stub("ses_parent", 100, 1_000);
        with_subagents.subagents = vec![
            PathBuf::from("container.db").join("ses_child.jsonl"),
            PathBuf::from("container.db").join("ses_nested.jsonl"),
        ];
        let storage = VirtualStorage::new(vec![with_subagents, virtual_stub("ses_other", 50, 500)]);

        let listed = load_sessions_with_cache(&storage, cache_base.path(), false, None).unwrap();

        assert_eq!(listed.len(), 2);
        assert_eq!(storage.parsed_ids(), vec!["ses_other", "ses_parent"]);
        let mut keys = stored_keys_under(cache_base.path(), &storage);
        keys.sort();
        assert_eq!(keys, vec!["ses_other", "ses_parent"]);
    }

    /// The background refresh thread caches a session opened by id with the
    /// same step as the list: it joins the root's sessions in the database,
    /// and the next load restores it unparsed. The sessions hold no
    /// conversation, the one kind of virtual session the database restores.
    #[test]
    fn session_loaded_by_id_is_cached_beside_roots_others() {
        let cache_base = admitted_cache_base();
        let storage = VirtualStorage::new(vec![
            virtual_stub("ses_listed", 100, 1_000),
            virtual_stub("ses_by_id", 200, 2_000),
        ])
        .holding_nothing(["ses_listed", "ses_by_id"]);
        let environment = environment_of(cache_base.path());
        let loader = SessionLoader::new(&environment, &storage, false, None, AgentMeter::UNMETERED);
        let root = SessionRoot::new("container.db");
        let _listed = loader.cache_one(&root, virtual_stub("ses_listed", 100, 1_000));

        let by_id = loader.cache_one(&root, virtual_stub("ses_by_id", 200, 2_000));

        assert!(matches!(by_id.read(), SessionRead::Empty));
        assert_eq!(storage.parsed_ids(), vec!["ses_by_id", "ses_listed"]);
        let mut keys = stored_keys_under(cache_base.path(), &storage);
        keys.sort();
        assert_eq!(keys, vec!["ses_by_id", "ses_listed"]);

        load_sessions_with_cache(&storage, cache_base.path(), false, None).unwrap();
        assert_eq!(
            storage.parsed_ids(),
            vec!["ses_by_id", "ses_listed"],
            "the session loaded by id restores from the cache"
        );
    }

    /// One count for the whole provider: a total that restarted at each root
    /// would show the indicator going backwards.
    #[test]
    fn progress_counts_every_discovered_session_across_all_roots() {
        let cache_base = tempfile::tempdir().unwrap();
        let mut storage = VirtualStorage::new(vec![
            virtual_stub("ses_first", 100, 1_000),
            virtual_stub("ses_second", 200, 2_000),
            virtual_stub_under("other.db", "ses_third", 300, 3_000),
        ]);
        storage.roots.push(SessionRoot::new("other.db"));
        let mut reports = Vec::new();
        let environment = environment_of(cache_base.path());

        load_sessions(
            &environment,
            &storage,
            false,
            None,
            AgentMeter::UNMETERED,
            LeftOutRows::NONE,
            &waiting_operation(&environment),
            &mut |done, total| reports.push((done, total)),
        )
        .unwrap();

        assert_eq!(reports, vec![(0, 3), (1, 3), (2, 3), (3, 3)]);
    }

    /// A root can hold sessions the provider found but ignores. The load
    /// words them for the user, one term per reason, so the list can show why
    /// it holds less than the disk does; a reason nothing was ignored for
    /// makes no term.
    #[test]
    fn roots_ignored_sessions_are_reported_as_terms_with_its_sessions() {
        let cache_base = tempfile::tempdir().unwrap();
        let storage = VirtualStorage::new(vec![virtual_stub("ses_first", 100, 1_000)])
            .with_ignored(3, "sessions unsupported")
            .with_ignored(0, "sessions archived");

        let loaded = load_with_cache(&storage, cache_base.path(), false, None).unwrap();

        assert_eq!(loaded.conversations.len(), 1);
        assert_eq!(
            loaded.ignored,
            vec![FilterTerm::new("Pi", "3 ignored: sessions unsupported")]
        );
    }

    /// Removing the cache an earlier release kept waits for this cache to be
    /// written, so a downgrade before then still finds its own.
    #[test]
    fn load_removes_superseded_cache_after_writing_its_own() {
        let cache_base = admitted_cache_base();
        let storage = VirtualStorage::new(vec![virtual_stub("ses_first", 100, 1_000)]);
        let environment = environment_of(cache_base.path());
        let loader = SessionLoader::new(&environment, &storage, false, None, AgentMeter::UNMETERED);

        loader.load_waiting().unwrap();

        assert_eq!(
            *storage.superseded_cache_removals.lock().unwrap(),
            vec![(cache_base.path().to_path_buf(), 1)],
            "called once, with the session already in the database"
        );
        loader.load_one(
            &SessionRoot::new("container.db"),
            &virtual_stub("ses_first", 100, 1_000),
        );
        assert_eq!(
            storage.superseded_cache_removals.lock().unwrap().len(),
            1,
            "opening one session by id removes nothing"
        );
        loader.load_waiting().unwrap();
        assert_eq!(
            storage.superseded_cache_removals.lock().unwrap().len(),
            2,
            "a warm load retries superseded-cache cleanup"
        );
    }

    /// The directory releases before the session cache database kept
    /// `root`'s cache files in, under `base`, as they named it.
    fn shard_directory_of_earlier_releases(
        base: &Path,
        storage: &dyn SessionStorage,
        root: &Path,
    ) -> PathBuf {
        use std::hash::{Hash, Hasher};
        let resolved = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        resolved.hash(&mut hasher);
        base.join(storage.cache().directory)
            .join(format!("root-{:016x}", hasher.finish()))
    }

    #[test]
    fn a_load_through_the_database_removes_the_cache_files_of_earlier_releases() {
        let base = tempfile::tempdir().unwrap();
        admit_database(base.path());
        let storage = VirtualStorage::new(vec![virtual_stub("ses_first", 100, 1_000)]);
        let earlier_cache =
            shard_directory_of_earlier_releases(base.path(), &storage, Path::new("container.db"));
        std::fs::create_dir_all(&earlier_cache).unwrap();
        std::fs::write(earlier_cache.join("shard-00.bin"), b"an earlier release").unwrap();

        load_sessions_with_cache(&storage, base.path(), false, None).unwrap();

        assert!(
            !base.path().join(storage.cache().directory).exists(),
            "the load removes the cache files of earlier releases"
        );
        assert_eq!(stored_keys_under(base.path(), &storage), ["ses_first"]);
    }

    /// True while some connection holds the writer lock of `base`'s database.
    fn is_writer_locked(base: &Path) -> bool {
        let connection =
            rusqlite::Connection::open(base.join(sqlite_cache::DATABASE_FILE_NAME)).unwrap();
        connection.busy_timeout(Duration::ZERO).unwrap();
        matches!(
            rusqlite::Transaction::new_unchecked(&connection, rusqlite::TransactionBehavior::Immediate),
            Err(rusqlite::Error::SqliteFailure(error, _))
                if error.code == rusqlite::ErrorCode::DatabaseBusy
        )
    }

    #[test]
    fn reads_and_parses_hold_the_writer_lock_without_blocking_the_write_queue() {
        let base = tempfile::tempdir().unwrap();
        let mut storage = VirtualStorage::new(
            (0..13)
                .map(|index| virtual_stub(&format!("ses_{index}"), 100, 1_000))
                .collect(),
        );
        let locked_during_parse = std::sync::Arc::new(Mutex::new(Vec::new()));
        let observed = locked_during_parse.clone();
        let observed_base = base.path().to_path_buf();
        storage.before_parse = Some(Box::new(move |_| {
            observed
                .lock()
                .unwrap()
                .push(is_writer_locked(&observed_base));
        }));
        let database = SqliteCache::open(&rearview(base.path())).unwrap();
        database.mark_admitted().unwrap();
        database.admit_storage(storage.cache()).unwrap();
        drop(database);
        let loaded = load_sessions_with_cache(&storage, base.path(), false, None).unwrap();
        assert_eq!(loaded.len(), 13);
        assert_eq!(storage.parse_count(), 13);
        let root = SessionRoot::new("container.db");
        let database = SqliteCache::open_waiting(&rearview(base.path()))
            .unwrap()
            .unwrap();
        assert_eq!(
            database
                .stored_keys(storage.cache(), &root.path)
                .unwrap()
                .len(),
            13
        );
        // The background refresh thread's targeted reads use the same
        // writer lock.
        let extra = virtual_stub("ses_extra", 100, 1_000);
        storage.stubs.push(extra.clone());
        let environment = environment_of(base.path());
        let loader = SessionLoader::new(&environment, &storage, false, None, AgentMeter::UNMETERED);
        assert!(loader.cache_row(&root, &extra).is_some());
        let locked_during_parse = locked_during_parse.lock().unwrap();
        assert_eq!(locked_during_parse.len(), 14);
        assert!(
            locked_during_parse.iter().all(|&is_locked| is_locked),
            "every transcript is parsed while holding the writer lock"
        );
    }

    #[test]
    fn rediscovery_replaces_ignored_counts_and_reports_incomplete_walks() {
        let base = admitted_cache_base();
        let stub = virtual_stub("ses_first", 100, 1_000);
        let storage = VirtualStorage::new(vec![stub.clone()]);
        let mut before = DiscoveredSessions::complete(vec![stub.clone()]);
        before.ignored.push(IgnoredSessions {
            count: 2,
            reason: "compressed sessions unsupported",
        });
        let mut after = DiscoveredSessions::complete(vec![stub]);
        after.ignored.push(IgnoredSessions {
            count: 7,
            reason: "compressed sessions unsupported",
        });
        after
            .unreadable_directories
            .push(super::super::storage::UnreadableDirectory {
                path: PathBuf::from("container.db/blocked"),
                error: "unreadable".to_owned(),
            });
        storage
            .discovery_results
            .lock()
            .unwrap()
            .extend([Ok(before), Ok(after)]);
        let environment = environment_of(base.path());
        let loader = SessionLoader::new(&environment, &storage, false, None, AgentMeter::UNMETERED);
        let loaded = loader.load_waiting().unwrap();
        assert_eq!(
            loaded.ignored,
            vec![FilterTerm::new(
                "Pi",
                "7 ignored: compressed sessions unsupported"
            )]
        );
        assert_eq!(
            stored_keys_under(base.path(), &storage),
            ["ses_first"],
            "the incomplete walk's session is cached"
        );
    }

    #[test]
    fn a_session_deleted_between_discovery_and_acquisition_is_absent_on_reread() {
        let base = tempfile::tempdir().unwrap();
        let stub = virtual_stub("ses_deleted", 100, 1_000);
        let storage = VirtualStorage::new(Vec::new());
        storage.discovery_results.lock().unwrap().extend([
            Ok(DiscoveredSessions::complete(vec![stub.clone()])),
            Ok(DiscoveredSessions::complete(Vec::new())),
        ]);
        let environment = environment_of(base.path());
        let loader = SessionLoader::new(&environment, &storage, false, None, AgentMeter::UNMETERED);
        assert!(matches!(
            loader.reread_caching(&[&stub.locator])[0].read(),
            SessionRead::Missing
        ));
    }

    #[test]
    fn losing_ownership_before_discovery_returns_the_discovery_without_write_permission() {
        use crate::history::sqlite_cache::refresh::tests::replace_owner;
        let base = tempfile::tempdir().unwrap();
        let stub = virtual_stub("ses_readable", 100, 1_000);
        let storage = VirtualStorage::new(vec![stub.clone()]);
        let environment = environment_of(base.path());
        let loader = SessionLoader::new(&environment, &storage, false, None, AgentMeter::UNMETERED);
        let ownership = RefreshOwnership::acquire_for_test(&rearview(base.path())).unwrap();
        let replacement = replace_owner(&rearview(base.path()));

        let rediscovered =
            loader.rediscover_owned(&SessionRoot::new("container.db"), ownership.token());

        assert_eq!(
            rediscovered.expect("the discovery is returned").stubs,
            vec![stub]
        );
        assert!(
            !ownership.token().can_publish(),
            "the lost ownership doesn't regain write permission"
        );
        assert_eq!(*storage.discoveries.lock().unwrap(), 1);
        replacement.write_batch(|| ()).unwrap();
    }

    #[test]
    fn a_discovery_failing_after_acquisition_caches_the_earlier_discovery() {
        let base = tempfile::tempdir().unwrap();
        let stub = virtual_stub("ses_readable", 100, 1_000);
        let storage = VirtualStorage::new(vec![stub.clone()]);
        storage.discovery_results.lock().unwrap().extend([
            Ok(DiscoveredSessions::complete(vec![stub.clone()])),
            Err(AppError::SessionListUnreadable {
                reason: SessionDatabaseFailure::Locked,
                detail: "the session list is locked".to_owned(),
            }),
        ]);
        let database = SqliteCache::open(&rearview(base.path())).unwrap();
        database.mark_admitted().unwrap();
        database.admit_storage(storage.cache()).unwrap();
        drop(database);

        let loaded = load_with_cache(&storage, base.path(), false, None).unwrap();

        assert_eq!(loaded.conversations.len(), 1);
        assert_eq!(*storage.discoveries.lock().unwrap(), 2);
        assert_eq!(storage.parsed_ids(), ["ses_readable"]);
        assert_eq!(stored_keys_under(base.path(), &storage), ["ses_readable"]);
        assert!(
            matches!(
                RefreshOwnership::try_acquire_now(&rearview(base.path())).unwrap(),
                Acquisition::Acquired(_)
            ),
            "the load releases refresh ownership"
        );
    }

    #[test]
    fn discovery_commit_failure_preserves_full_and_targeted_reads_without_writes() {
        for targeted in [false, true] {
            let base = admitted_cache_base();
            let old = virtual_stub("ses_readable", 100, 1_000);
            let seeded = VirtualStorage::new(vec![old.clone()]);
            load_sessions_with_cache(&seeded, base.path(), false, None).unwrap();
            let before = cached_fingerprints_under(base.path(), &seeded);
            fail_every_commit_after_acquisition(base.path());
            let fresh = virtual_stub("ses_readable", 200, 2_000);
            let storage = VirtualStorage::new(vec![fresh.clone()]);
            let environment = environment_of(base.path());
            let loader =
                SessionLoader::new(&environment, &storage, false, None, AgentMeter::UNMETERED);
            let row = if targeted {
                loader
                    .cache_row(&SessionRoot::new("container.db"), &old)
                    .unwrap()
            } else {
                loader.load_waiting().unwrap().conversations.remove(0)
            };
            assert_eq!(row.fingerprint, fresh.fingerprint.stamp());
            assert_eq!(storage.parse_count(), 1);
            assert_eq!(
                *storage.discoveries.lock().unwrap(),
                if targeted { 1 } else { 2 },
                "a completed discovery isn't repeated after its commit fails"
            );
            assert_eq!(cached_fingerprints_under(base.path(), &seeded), before);
        }
    }

    /// The fingerprint `cache_base`'s database holds of each of `storage`'s
    /// sessions under its root, by cache key.
    fn cached_fingerprints_under(
        cache_base: &Path,
        storage: &VirtualStorage,
    ) -> HashMap<String, CachedFingerprint> {
        SqliteCache::open_waiting(&rearview(cache_base))
            .unwrap()
            .unwrap()
            .current_fingerprints(storage.cache(), Path::new("container.db"))
            .unwrap()
    }

    /// Make every refresh commit after acquisition fail in `base`'s
    /// database, revoking the owner's write permission.
    fn fail_every_commit_after_acquisition(base: &Path) {
        rusqlite::Connection::open(base.join(sqlite_cache::DATABASE_FILE_NAME))
            .unwrap()
            .execute_batch(
                "CREATE TRIGGER fail_commit BEFORE UPDATE OF last_commit_ms ON refresh_state \
                 WHEN NEW.owner = OLD.owner \
                 BEGIN SELECT RAISE(ABORT, 'injected timestamp failure'); END;",
            )
            .unwrap();
    }

    #[test]
    fn a_read_revoked_while_a_replacement_refresh_starts_waits_for_that_refresh() {
        use crate::history::sqlite_cache::refresh::tests::replace_owner;
        let base = tempfile::tempdir().unwrap();
        admit_database(base.path());
        let seeded = VirtualStorage::new(vec![virtual_stub("ses_target", 100, 1_000)]);
        load_sessions_with_cache(&seeded, base.path(), false, None).unwrap();
        fail_every_commit_after_acquisition(base.path());
        let replacement = std::sync::Arc::new(Mutex::new(None));
        let started = replacement.clone();
        let changed = virtual_stub("ses_target", 200, 2_000);
        let mut storage = VirtualStorage::new(vec![changed.clone()]);
        let replacement_base = rearview(base.path());
        storage.before_parse = Some(Box::new(move |_| {
            *started.lock().unwrap() = Some(replace_owner(&replacement_base));
        }));
        let environment = environment_of(base.path());
        let loader = SessionLoader::new(&environment, &storage, false, None, AgentMeter::UNMETERED);

        let read = loader.cache_one(&SessionRoot::new("container.db"), changed);

        assert!(matches!(read.read(), SessionRead::Listed(_)));
        assert!(
            replacement.lock().unwrap().is_some(),
            "the replacement refresh did not start"
        );
        assert_eq!(
            read.next(),
            NextStep::AwaitRefresh(AwaitedRefresh::AnotherRearview(OtherRearview::Refreshing)),
            "the row read outside the database didn't wait for the replacement refresh"
        );
    }

    /// The observations of one [`load_changed_sessions`].
    struct ChangedSessionsLoad {
        rows: Vec<Conversation>,
        fresh: Vec<SessionStub>,
        parses: usize,
        /// The refresh owner's commits before the first parse began.
        commits_before_first_parse: i64,
        /// The refresh owner's commits by the end of the load.
        commits: i64,
        /// True when dropping the commit failure, at the first parse after
        /// the first batch, found the writer lock free; `None` without a
        /// failure.
        is_failure_dropped_without_the_lock: Option<bool>,
        /// Database rows at the fingerprint from before the change.
        unchanged_sessions: i64,
        sessions: i64,
    }

    /// Cache `READ_BATCH_LENGTH + 1` sessions, change every one of them, and
    /// load them again: through targeted reads or a full load. With
    /// `fail_from`, every refresh owner commit after that many fails, until
    /// the first parse after the first batch drops the failure.
    fn load_changed_sessions(targeted: bool, fail_from: Option<i64>) -> ChangedSessionsLoad {
        let base = tempfile::tempdir().unwrap();
        let keys: Vec<_> = (0..READ_BATCH_LENGTH + 1)
            .map(|index| format!("ses_{index}"))
            .collect();
        let old: Vec<_> = keys
            .iter()
            .map(|key| virtual_stub(key, 100, 1_000))
            .collect();
        let seeded = VirtualStorage::new(old.clone());
        let database = SqliteCache::open(&rearview(base.path())).unwrap();
        database.mark_admitted().unwrap();
        database.admit_storage(seeded.cache()).unwrap();
        drop(database);
        load_sessions_with_cache(&seeded, base.path(), false, None).unwrap();
        let database_path = base.path().join(sqlite_cache::DATABASE_FILE_NAME);
        let connection = rusqlite::Connection::open(&database_path).unwrap();
        connection.execute_batch("CREATE TABLE refresh_commits (count INTEGER); INSERT INTO refresh_commits VALUES (0);
            CREATE TRIGGER count_commits AFTER UPDATE OF last_commit_ms ON refresh_state WHEN NEW.owner = OLD.owner BEGIN UPDATE refresh_commits SET count = count + 1; END;").unwrap();
        if let Some(fail_from) = fail_from {
            connection.execute_batch(&format!("CREATE TRIGGER fail_commit BEFORE UPDATE OF last_commit_ms ON refresh_state WHEN NEW.owner = OLD.owner AND (SELECT count FROM refresh_commits) >= {fail_from} BEGIN SELECT RAISE(ABORT, 'injected parse timestamp failure'); END;")).unwrap();
        }
        let commit_count = |connection: &rusqlite::Connection| {
            connection
                .query_row("SELECT count FROM refresh_commits", [], |row| {
                    row.get::<_, i64>(0)
                })
                .unwrap()
        };
        let fresh: Vec<_> = keys
            .iter()
            .map(|key| virtual_stub(key, 200, 2_000))
            .collect();
        let mut storage = VirtualStorage::new(fresh.clone());
        let commits_before_first_parse = std::sync::Arc::new(Mutex::new(None));
        let is_failure_dropped = std::sync::Arc::new(Mutex::new(None));
        let (first_parse, dropped) = (
            commits_before_first_parse.clone(),
            is_failure_dropped.clone(),
        );
        storage.before_parse = Some(Box::new(move |parsed_before| {
            let connection = rusqlite::Connection::open(&database_path).unwrap();
            connection.busy_timeout(Duration::ZERO).unwrap();
            if parsed_before == 0 {
                *first_parse.lock().unwrap() = Some(commit_count(&connection));
            }
            if parsed_before == READ_BATCH_LENGTH && fail_from.is_some() {
                *dropped.lock().unwrap() =
                    Some(connection.execute_batch("DROP TRIGGER fail_commit").is_ok());
            }
        }));
        let environment = environment_of(base.path());
        let loader = SessionLoader::new(&environment, &storage, false, None, AgentMeter::UNMETERED);
        let mut rows = if targeted {
            let operation = waiting_operation(&environment);
            let reads = loader.read_stubs(
                &SessionRoot::new("container.db"),
                &HashMap::new(),
                old.clone(),
                StubsAfterRediscovery::SameFiles,
                &operation,
            );
            operation
                .finish()
                .and(reads)
                .unwrap()
                .outcomes
                .into_iter()
                .map(|outcome| {
                    let SessionRead::Listed(row) = outcome.into_read() else {
                        panic!("the readable session disappeared")
                    };
                    *row
                })
                .collect::<Vec<_>>()
        } else {
            loader.load_waiting().unwrap().conversations
        };
        rows.sort_by(|left, right| left.path.cmp(&right.path));
        let count = |sql: &str| {
            connection
                .query_row(sql, [], |row| row.get::<_, i64>(0))
                .unwrap()
        };
        ChangedSessionsLoad {
            rows,
            fresh,
            parses: storage.parse_count(),
            commits_before_first_parse: commits_before_first_parse.lock().unwrap().unwrap(),
            commits: commit_count(&connection),
            is_failure_dropped_without_the_lock: *is_failure_dropped.lock().unwrap(),
            unchanged_sessions: count(
                "SELECT COUNT(*) FROM sessions WHERE file_size = 100 AND mtime_secs = 1000",
            ),
            sessions: count("SELECT COUNT(*) FROM sessions"),
        }
    }

    #[test]
    fn parse_commit_failure_preserves_full_and_targeted_reads_without_later_writes() {
        for targeted in [false, true] {
            let setup_commits = load_changed_sessions(targeted, None).commits_before_first_parse;

            let load = load_changed_sessions(targeted, Some(setup_commits));

            assert_eq!(
                load.commits_before_first_parse, setup_commits,
                "the commit failure starts at the first parse batch"
            );
            assert_eq!(load.rows.len(), load.fresh.len());
            assert!(
                load.rows
                    .iter()
                    .all(|row| row.fingerprint == load.fresh[0].fingerprint.stamp())
            );
            assert_eq!(
                load.parses,
                load.fresh.len(),
                "a failed commit repeated a source parse"
            );
            assert_eq!(
                load.is_failure_dropped_without_the_lock,
                Some(true),
                "parsing after the failed batch held the writer lock"
            );
            assert_eq!(
                load.commits, setup_commits,
                "batches stop committing after the failure"
            );
            assert_eq!(load.sessions, (READ_BATCH_LENGTH + 1) as i64);
            assert_eq!(load.unchanged_sessions, load.sessions);
        }
    }

    #[test]
    fn refresh_reapplies_selection_to_the_fresh_discovery() {
        let base = tempfile::tempdir().unwrap();
        let old = virtual_stub("ses_old", 100, 1_000);
        let shrunk = virtual_stub("ses_old", 10, 2_000);
        let added = virtual_stub("ses_added", 200, 2_000);
        let storage = VirtualStorage::new(Vec::new());
        storage.discovery_results.lock().unwrap().extend([
            Ok(DiscoveredSessions::complete(vec![old])),
            Ok(DiscoveredSessions::complete(vec![
                shrunk.clone(),
                added.clone(),
            ])),
        ]);
        let result = rediscover_sessions_with_cache(&storage, base.path(), false, &|stub| {
            stub.fingerprint.size > 50
        })
        .unwrap();
        assert_eq!(
            result.found,
            vec![
                (shrunk.locator, shrunk.fingerprint.stamp()),
                (added.locator.clone(), added.fingerprint.stamp())
            ]
        );
        assert_eq!(result.outcomes.len(), 1);
        assert_eq!(result.outcomes[0].locator(), added.locator);
        assert_eq!(storage.parsed_ids(), vec!["ses_added"]);
    }

    #[test]
    fn refresh_reports_fresh_ignored_counts_and_incomplete_directories() {
        let base = admitted_cache_base();
        let stub = virtual_stub("ses_readable", 100, 1_000);
        let storage = VirtualStorage::new(vec![stub.clone()]);
        let mut before = DiscoveredSessions::complete(vec![stub.clone()]);
        before.ignored.push(IgnoredSessions {
            count: 2,
            reason: "compressed sessions unsupported",
        });
        let mut after = DiscoveredSessions::complete(vec![stub]);
        after.ignored.push(IgnoredSessions {
            count: 7,
            reason: "compressed sessions unsupported",
        });
        let blocked = PathBuf::from("container.db/blocked");
        after
            .unreadable_directories
            .push(super::super::storage::UnreadableDirectory {
                path: blocked.clone(),
                error: "unreadable".to_owned(),
            });
        storage
            .discovery_results
            .lock()
            .unwrap()
            .extend([Ok(before), Ok(after)]);
        let result =
            rediscover_sessions_with_cache(&storage, base.path(), false, &|_| true).unwrap();
        assert_eq!(
            result.ignored,
            vec![FilterTerm::new(
                "Pi",
                "7 ignored: compressed sessions unsupported"
            )]
        );
        assert_eq!(result.unreadable_directories, vec![blocked]);
        assert_eq!(
            stored_keys_under(base.path(), &storage),
            ["ses_readable"],
            "the incomplete walk's session is cached"
        );
    }

    #[test]
    fn sessions_that_are_not_files_load_through_the_shared_loop() {
        let cache_base = admitted_cache_base();
        let storage = VirtualStorage::new(vec![
            virtual_stub("ses_first", 100, 1_000),
            virtual_stub("ses_second", 200, 2_000),
        ]);

        let listed = load_sessions_with_cache(&storage, cache_base.path(), false, None).unwrap();

        assert_eq!(storage.parse_count(), 2);
        assert_eq!(
            listed
                .iter()
                .map(|conversation| conversation.path.clone())
                .collect::<Vec<_>>(),
            vec![
                PathBuf::from("container.db").join("ses_first.jsonl"),
                PathBuf::from("container.db").join("ses_second.jsonl"),
            ],
            "locators reach the conversations untouched"
        );
    }

    /// The row the tests of left-out rows leave out: `ses_current`'s.
    fn is_current(row: &Conversation) -> bool {
        row.path.ends_with("ses_current.jsonl")
    }

    /// Claude sessions written under `home` and cached in `cache_base`: the
    /// fixture's session, whose row the left-out tests leave out, and its
    /// other session. Returns the storage pinned to `home`, its root, and
    /// both transcripts, the left-out one first.
    fn cached_claude_sessions(
        home: &Path,
        cache_base: &Path,
    ) -> (&'static dyn SessionStorage, PathBuf, [PathBuf; 2]) {
        let fixture: &'static dyn super::super::contract_tests::ProviderFixture =
            &super::super::claude::tests::ClaudeFixture;
        let left_out = fixture.write_session(home, fixture.ids().session);
        let other = fixture.write_session(home, fixture.ids().other);
        let storage = super::super::contract_tests::kept_storage_under(fixture, home, None);
        load_sessions_with_cache(storage, cache_base, false, None).unwrap();
        (storage, fixture.root_under(home).path, [left_out, other])
    }

    /// Add an empty line to the transcript at `path`, changing its
    /// fingerprint and not its content.
    fn grow(path: &Path) {
        let mut transcript = std::fs::OpenOptions::new().append(true).open(path).unwrap();
        std::io::Write::write_all(&mut transcript, b"\n").unwrap();
    }

    /// The fingerprint `cache_base`'s database recorded the session at
    /// `transcript` at, one `storage` keeps under `root`, whether or not the
    /// transcript still matches it.
    fn recorded_fingerprint_of(
        cache_base: &Path,
        storage: &dyn SessionStorage,
        root: &Path,
        transcript: &Path,
    ) -> Option<CachedFingerprint> {
        let cache_key = transcript.strip_prefix(root).unwrap().to_string_lossy();
        let recorded = SqliteCache::open_waiting(&rearview(cache_base))
            .unwrap()
            .unwrap()
            .catalogue(storage.cache(), storage.source(), root, false)
            .unwrap()
            .remove(cache_key.as_ref());
        match recorded {
            Some(CatalogueSession::Listed(row)) => row.fingerprint,
            _ => None,
        }
    }

    #[test]
    fn a_changed_session_whose_cached_row_is_left_out_keeps_that_row_unread() {
        let home = tempfile::tempdir().unwrap();
        let cache_base = admitted_cache_base();
        let (storage, root, [left_out, other]) =
            cached_claude_sessions(home.path(), cache_base.path());
        let cached = recorded_fingerprint_of(cache_base.path(), storage, &root, &left_out);
        assert!(cached.is_some());
        grow(&left_out);
        grow(&other);
        let is_left_out = |row: &Conversation| row.path == left_out;
        let meter = LoadMeter::default();

        let loaded = load_metered_with_cache(
            storage,
            cache_base.path(),
            meter.agent(Source::Claude),
            LeftOutRows::of(&is_left_out),
        )
        .unwrap();

        assert_eq!(meter.agents()[0].refresh.transcripts, 1);
        let row = loaded.conversations.iter().find(|row| is_left_out(row));
        assert_eq!(
            row.and_then(|row| row.fingerprint),
            cached,
            "the row carries the fingerprint the database recorded it at"
        );
        assert_eq!(
            recorded_fingerprint_of(cache_base.path(), storage, &root, &left_out),
            cached
        );

        let meter = LoadMeter::default();
        load_metered_with_cache(
            storage,
            cache_base.path(),
            meter.agent(Source::Claude),
            LeftOutRows::NONE,
        )
        .unwrap();
        assert_eq!(
            meter.agents()[0].refresh.transcripts,
            1,
            "the next load that includes the session reads it"
        );
    }

    #[test]
    fn a_load_that_leaves_out_every_changed_session_doesnt_write_the_database() {
        let home = tempfile::tempdir().unwrap();
        let cache_base = admitted_cache_base();
        let (storage, root, [left_out, _]) = cached_claude_sessions(home.path(), cache_base.path());
        let cached = recorded_fingerprint_of(cache_base.path(), storage, &root, &left_out);
        grow(&left_out);
        let is_left_out = |row: &Conversation| row.path == left_out;

        load_metered_with_cache(
            storage,
            cache_base.path(),
            AgentMeter::UNMETERED,
            LeftOutRows::of(&is_left_out),
        )
        .unwrap();

        assert_eq!(
            recorded_fingerprint_of(cache_base.path(), storage, &root, &left_out),
            cached
        );
    }

    /// A session whose rows left the database is read from its transcript
    /// again, and its rows written back.
    #[test]
    fn a_session_the_database_doesnt_hold_is_read_again() {
        for fixture in super::super::contract_tests::registered_fixtures() {
            let home = tempfile::tempdir().unwrap();
            let base = tempfile::tempdir().unwrap();
            fixture.write_session(home.path(), fixture.ids().session);
            let root = fixture.root_under(home.path());
            let storage = fixture.storage().cache();
            SqliteCache::open(&rearview(base.path()))
                .unwrap()
                .mark_admitted()
                .unwrap();
            super::super::contract_tests::load_under(fixture, home.path(), base.path());
            let mut database = SqliteCache::open(&rearview(base.path())).unwrap();
            let keys = database.stored_keys(storage, &root.path).unwrap();
            assert_eq!(keys.len(), 1);
            database.remove(storage, &root.path, &keys).unwrap();
            drop(database);

            super::super::contract_tests::load_under(fixture, home.path(), base.path());

            let database = SqliteCache::open_waiting(&rearview(base.path()))
                .unwrap()
                .unwrap();
            assert!(
                !database
                    .entries(storage, &root.path, &keys[0])
                    .unwrap()
                    .unwrap()
                    .entries
                    .is_empty()
            );
        }
    }

    #[test]
    fn a_failed_entry_projection_is_retried_with_an_unchanged_discovery_fingerprint() {
        for fixture in super::super::contract_tests::registered_fixtures() {
            let home = tempfile::tempdir().unwrap();
            let base = tempfile::tempdir().unwrap();
            fixture.write_session(home.path(), fixture.ids().session);
            let root = fixture.root_under(home.path());
            let storage = fixture.storage().cache();
            SqliteCache::open(&rearview(base.path()))
                .unwrap()
                .mark_admitted()
                .unwrap();
            super::super::contract_tests::load_under(fixture, home.path(), base.path());
            let database = SqliteCache::open_waiting(&rearview(base.path()))
                .unwrap()
                .unwrap();
            let fingerprint = database.current_fingerprints(storage, &root.path).unwrap();
            assert_eq!(fingerprint.len(), 1);
            let connection =
                rusqlite::Connection::open(base.path().join(sqlite_cache::DATABASE_FILE_NAME))
                    .unwrap();
            let previous: i64 = connection
                .query_row("SELECT id FROM sessions", [], |row| row.get(0))
                .unwrap();
            connection
                .execute("UPDATE sessions SET entries_error='temporary failure'", [])
                .unwrap();
            assert!(
                database
                    .current_fingerprints(storage, &root.path)
                    .unwrap()
                    .is_empty()
            );

            let rows = super::super::contract_tests::load_under(fixture, home.path(), base.path());

            assert_eq!(rows.len(), 1);
            assert_eq!(
                database.current_fingerprints(storage, &root.path).unwrap(),
                fingerprint
            );
            let (replacement, error): (i64, Option<String>) = connection
                .query_row("SELECT id, entries_error FROM sessions", [], |row| {
                    Ok((row.get(0)?, row.get(1)?))
                })
                .unwrap();
            assert_ne!(replacement, previous);
            assert!(error.is_none());
        }
    }

    #[test]
    fn a_session_deleted_from_disk_leaves_the_database() {
        let cache_base = tempfile::tempdir().unwrap();
        let both = VirtualStorage::new(vec![
            virtual_stub("ses_kept", 100, 1_000),
            virtual_stub("ses_deleted", 50, 500),
        ])
        .holding_nothing(["ses_kept", "ses_deleted"]);
        admit_database(cache_base.path());
        load_sessions_with_cache(&both, cache_base.path(), false, None).unwrap();
        let kept = VirtualStorage::new(vec![virtual_stub("ses_kept", 100, 1_000)])
            .holding_nothing(["ses_kept"]);

        load_sessions_with_cache(&kept, cache_base.path(), false, None).unwrap();

        assert_eq!(stored_keys_under(cache_base.path(), &kept), ["ses_kept"]);
    }

    /// The refresh owner's commits while a load reads a root of `count`
    /// sessions the database holds, one of them changed. The sessions hold
    /// no conversation, the one kind of virtual session the database records.
    fn commits_reading_a_root_with_one_change(count: usize) -> i64 {
        let base = tempfile::tempdir().unwrap();
        let names: Vec<_> = (0..count).map(|index| format!("ses_{index}")).collect();
        let cached: Vec<_> = names
            .iter()
            .map(|name| virtual_stub(name, 100, 1_000))
            .collect();
        let mut seeded = VirtualStorage::new(cached.clone());
        seeded.holding_nothing = names.iter().cloned().collect();
        admit_database(base.path());
        load_sessions_with_cache(&seeded, base.path(), false, None).unwrap();
        let connection =
            rusqlite::Connection::open(base.path().join(sqlite_cache::DATABASE_FILE_NAME)).unwrap();
        connection.execute_batch("CREATE TABLE refresh_commits (count INTEGER); INSERT INTO refresh_commits VALUES (0);
            CREATE TRIGGER count_commits AFTER UPDATE OF last_commit_ms ON refresh_state WHEN NEW.owner = OLD.owner BEGIN UPDATE refresh_commits SET count = count + 1; END;").unwrap();
        let mut changed = cached;
        changed[0] = virtual_stub("ses_0", 200, 2_000);
        let mut storage = VirtualStorage::new(changed);
        storage.holding_nothing = seeded.holding_nothing;

        load_sessions_with_cache(&storage, base.path(), false, None).unwrap();

        assert_eq!(storage.parsed_ids(), ["ses_0"]);
        connection
            .query_row("SELECT count FROM refresh_commits", [], |row| row.get(0))
            .unwrap()
    }

    #[test]
    fn restoring_unchanged_sessions_doesnt_take_the_writer_lock() {
        assert_eq!(
            commits_reading_a_root_with_one_change(12 * READ_BATCH_LENGTH),
            commits_reading_a_root_with_one_change(READ_BATCH_LENGTH),
            "unchanged sessions are restored outside writer-locked batches"
        );
    }

    /// A discovery of `stubs` that could not list `container.db/blocked`.
    fn discovery_missing_the_blocked_directory(stubs: Vec<SessionStub>) -> DiscoveredSessions {
        let mut found = DiscoveredSessions::complete(stubs);
        found
            .unreadable_directories
            .push(super::super::storage::UnreadableDirectory {
                path: PathBuf::from("container.db/blocked"),
                error: "unreadable".to_owned(),
            });
        found
    }

    /// The sessions hold no conversation: the database records a virtual
    /// session only then, since it has no transcript to read entries from.
    #[test]
    fn a_root_with_an_unreadable_directory_caches_its_new_and_changed_sessions() {
        let base = tempfile::tempdir().unwrap();
        let hidden = virtual_stub_under("container.db/blocked", "ses_hidden", 50, 500);
        let changed_before = virtual_stub("ses_changed", 100, 1_000);
        let seeded = VirtualStorage::new(vec![hidden.clone(), changed_before])
            .holding_nothing(["ses_hidden", "ses_changed"]);
        admit_database(base.path());
        load_sessions_with_cache(&seeded, base.path(), false, None).unwrap();
        let changed = virtual_stub("ses_changed", 200, 2_000);
        let added = virtual_stub("ses_added", 300, 3_000);
        let partial = VirtualStorage::new(Vec::new()).holding_nothing(["ses_changed", "ses_added"]);
        partial.discovery_results.lock().unwrap().extend([
            Ok(discovery_missing_the_blocked_directory(vec![
                changed.clone(),
                added.clone(),
            ])),
            Ok(discovery_missing_the_blocked_directory(vec![
                changed.clone(),
                added.clone(),
            ])),
        ]);

        load_sessions_with_cache(&partial, base.path(), false, None).unwrap();

        let database = SqliteCache::open_waiting(&rearview(base.path()))
            .unwrap()
            .unwrap();
        let current = database
            .current_fingerprints(partial.cache(), Path::new("container.db"))
            .unwrap();
        assert_eq!(
            current.get("ses_changed").copied(),
            changed.fingerprint.stamp()
        );
        assert_eq!(current.get("ses_added").copied(), added.fingerprint.stamp());
        assert_eq!(
            current.get("ses_hidden").copied(),
            hidden.fingerprint.stamp(),
            "the session under the unreadable directory keeps its row"
        );
    }

    #[test]
    fn a_targeted_read_whose_repeated_discovery_misses_a_directory_caches_the_session() {
        let base = tempfile::tempdir().unwrap();
        let added = virtual_stub("ses_added", 300, 3_000);
        let storage = VirtualStorage::new(vec![added.clone()]).holding_nothing(["ses_added"]);
        storage.discovery_results.lock().unwrap().push_back(Ok(
            discovery_missing_the_blocked_directory(vec![added.clone()]),
        ));
        admit_database(base.path());
        let environment = environment_of(base.path());
        let loader = SessionLoader::new(&environment, &storage, false, None, AgentMeter::UNMETERED);

        let read = loader.cache_one(&SessionRoot::new("container.db"), added);

        assert!(matches!(read.read(), SessionRead::Empty));
        assert_eq!(stored_keys_under(base.path(), &storage), ["ses_added"]);
    }

    /// Complete disk admission of `base`'s database, as a first build does.
    fn admit_database(base: &Path) {
        SqliteCache::open(&rearview(base))
            .unwrap()
            .mark_admitted()
            .unwrap();
    }

    /// The cache keys `base`'s database stores for `storage`'s root.
    fn stored_keys_under(base: &Path, storage: &VirtualStorage) -> Vec<String> {
        SqliteCache::open_waiting(&rearview(base))
            .unwrap()
            .unwrap()
            .stored_keys(storage.cache(), Path::new("container.db"))
            .unwrap()
    }

    #[test]
    fn an_empty_root_removes_its_last_unfinished_session() {
        let base = tempfile::tempdir().unwrap();
        let storage = VirtualStorage::new(vec![virtual_stub("ses_deleted", 50, 500)])
            .holding_nothing(["ses_deleted"]);
        admit_database(base.path());
        load_sessions_with_cache(&storage, base.path(), false, None).unwrap();
        // An unfinished refresh is absent from current_fingerprints.
        let connection =
            rusqlite::Connection::open(base.path().join(sqlite_cache::DATABASE_FILE_NAME)).unwrap();
        connection
            .execute("UPDATE sessions SET status='writing'", [])
            .unwrap();
        let empty = VirtualStorage::new(Vec::new());

        load_sessions_with_cache(&empty, base.path(), false, None).unwrap();

        assert!(stored_keys_under(base.path(), &storage).is_empty());
    }

    #[test]
    fn an_empty_root_doesnt_create_a_database() {
        let base = tempfile::tempdir().unwrap();
        let storage = VirtualStorage::new(Vec::new());
        load_sessions_with_cache(&storage, base.path(), false, None).unwrap();
        assert!(!base.path().join(sqlite_cache::DATABASE_FILE_NAME).exists());
    }

    #[test]
    fn a_partial_discovery_doesnt_delete_unseen_database_sessions() {
        let base = tempfile::tempdir().unwrap();
        let storage = VirtualStorage::new(vec![virtual_stub("ses_hidden", 50, 500)])
            .holding_nothing(["ses_hidden"]);
        admit_database(base.path());
        load_sessions_with_cache(&storage, base.path(), false, None).unwrap();
        let mut partial = DiscoveredSessions::complete(Vec::new());
        partial
            .unreadable_directories
            .push(super::super::storage::UnreadableDirectory {
                path: PathBuf::from("container.db"),
                error: "temporarily unavailable".to_owned(),
            });
        let environment = environment_of(base.path());
        let loader = SessionLoader::new(&environment, &storage, false, None, AgentMeter::UNMETERED);

        loader
            .load_discovered(
                vec![(SessionRoot::new("container.db"), partial)],
                LeftOutRows::NONE,
                &waiting_operation(&environment),
                &mpsc::channel().0,
            )
            .unwrap();

        assert_eq!(stored_keys_under(base.path(), &storage), ["ses_hidden"]);
    }

    /// A load writes a session's entries as `session_entries` reads them.
    #[test]
    fn a_load_writes_each_sessions_entries_to_the_database() {
        let home = tempfile::tempdir().unwrap();
        let cache_base = tempfile::tempdir().unwrap();
        let fixture = crate::history::provider::contract_tests::registered_fixtures()[0];
        let locator = fixture.write_session(home.path(), fixture.ids().session);
        let root = fixture.root_under(home.path());
        let storage = fixture.storage();
        SqliteCache::open(&rearview(cache_base.path()))
            .unwrap()
            .mark_admitted()
            .unwrap();

        let conversations = crate::history::provider::contract_tests::load_under(
            fixture,
            home.path(),
            cache_base.path(),
        );

        let [row] = conversations.as_slice() else {
            panic!("one session loads: {}", conversations.len());
        };
        let cache_key = locator
            .strip_prefix(&root.path)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let written = SqliteCache::open(&rearview(cache_base.path()))
            .unwrap()
            .entries(storage.cache(), &root.path, &cache_key)
            .unwrap()
            .expect("the session's rows are written");
        let expected =
            crate::history::normalized_session(row.source, &row.path, &row.subagents).unwrap();
        assert_eq!(
            format!("{:?}", written.entries),
            format!("{:?}", expected.entries)
        );
    }

    /// A new session has no cached row to leave out, so it is read.
    #[test]
    fn a_new_session_is_read_even_when_its_row_is_left_out() {
        let cache_base = admitted_cache_base();
        let storage = VirtualStorage::new(vec![virtual_stub("ses_current", 100, 1_000)]);

        let loaded = load_metered_with_cache(
            &storage,
            cache_base.path(),
            AgentMeter::UNMETERED,
            LeftOutRows::of(&is_current),
        )
        .unwrap();

        assert_eq!(storage.parsed_ids(), ["ses_current"]);
        assert_eq!(loaded.conversations.len(), 1);
    }

    /// The totals one metered load of `storage` through `cache_base` records,
    /// and the step its progress ends on.
    fn metered_load(storage: &VirtualStorage, cache_base: &Path) -> (AgentLoad, StepProgress) {
        let meter = LoadMeter::default();
        load_metered_with_cache(
            storage,
            cache_base,
            meter.agent(Source::Pi),
            LeftOutRows::NONE,
        )
        .unwrap();
        let [agent] = meter.agents()[..] else {
            panic!("one record per agent: {:?}", meter.agents());
        };
        (agent, meter.progress())
    }

    /// The warm load's sessions hold no conversation, the one kind of virtual
    /// session the database restores.
    #[test]
    fn a_metered_load_records_what_discovery_the_cache_and_the_refresh_read() {
        let mut with_subagent = virtual_stub("ses_parent", 100, 1_000);
        with_subagent.subagents = vec![PathBuf::from("container.db").join("ses_child.jsonl")];
        let stubs = vec![with_subagent, virtual_stub("ses_other", 50, 500)];
        let listed = VirtualStorage::new(stubs.clone());
        let empty = VirtualStorage::new(stubs).holding_nothing(["ses_parent", "ses_other"]);
        let cold_base = admitted_cache_base();
        let warm_base = admitted_cache_base();

        let (cold, cold_progress) = metered_load(&listed, cold_base.path());
        metered_load(&empty, warm_base.path());
        let (warm, _) = metered_load(&empty, warm_base.path());

        assert_eq!((cold.discovery.roots, cold.discovery.sessions), (1, 2));
        assert_eq!(
            (cold.refresh.transcripts, cold.refresh.bytes),
            (3, 150),
            "the parent's transcript, its sub-agent's and the other session's"
        );
        assert_eq!(
            cold_progress,
            StepProgress {
                step: LoadStep::Refresh,
                items: Some(ItemCount { done: 3, total: 3 }),
            }
        );
        assert_eq!(cold.cache.sessions, 0, "nothing was cached yet");
        assert_eq!(warm.refresh.transcripts, 0);
        assert_eq!((warm.cache.reads, warm.cache.sessions), (1, 2));
    }

    #[test]
    fn load_reports_empty_and_unreadable_sessions_but_not_ones_failing_transiently() {
        let cache_base = tempfile::tempdir().unwrap();
        let storage = VirtualStorage::new(vec![
            virtual_stub("ses_listed", 100, 1_000),
            virtual_stub("ses_empty", 200, 2_000),
            virtual_stub("ses_unreadable", 300, 3_000),
            virtual_stub("ses_busy", 400, 4_000),
        ])
        .holding_nothing(["ses_empty"])
        .unreadable(["ses_unreadable"])
        .failing_transiently(["ses_busy"]);
        let environment = environment_of(cache_base.path());
        let loader = SessionLoader::new(&environment, &storage, false, None, AgentMeter::UNMETERED);

        let loaded = loader.load_waiting().unwrap();

        let found = |name: &str, size: u64, modified_secs: u64| {
            let stub = virtual_stub(name, size, modified_secs);
            let found = FoundSession {
                source: Source::Pi,
                fingerprint: stub.fingerprint.stamp(),
                has_transient_read_error: false,
            };
            HashMap::from([(stub.locator, found)])
        };
        assert_eq!(
            loaded.skipped,
            SkippedSessions {
                unlisted: found("ses_empty", 200, 2_000),
                unreadable: found("ses_unreadable", 300, 3_000),
            }
        );
        assert_eq!(
            loaded.conversations[0].fingerprint,
            virtual_stub("ses_listed", 100, 1_000).fingerprint.stamp(),
            "a row carries the fingerprint it was read with"
        );
    }

    #[test]
    fn a_rediscovery_reads_only_the_sessions_it_picks() {
        let cache_base = admitted_cache_base();
        let unchanged = virtual_stub("ses_unchanged", 100, 1_000);
        let changed = virtual_stub("ses_changed", 200, 2_000);
        let added = virtual_stub("ses_added", 300, 3_000);
        let storage = VirtualStorage::new(vec![unchanged.clone(), changed.clone()]);
        load_sessions_with_cache(&storage, cache_base.path(), false, None).unwrap();

        let grown = virtual_stub(&changed.cache_key, 250, 9_000);
        let now = VirtualStorage::new(vec![unchanged.clone(), grown.clone(), added.clone()]);
        let rediscovered =
            rediscover_sessions_with_cache(&now, cache_base.path(), false, &|stub| {
                stub.cache_key != unchanged.cache_key
            })
            .unwrap();

        assert_eq!(rediscovered.found.len(), 3, "every session is reported");
        let mut read = rediscovered
            .outcomes
            .iter()
            .map(|outcome| {
                assert!(matches!(outcome.read(), SessionRead::Listed(_)));
                outcome.locator().to_path_buf()
            })
            .collect::<Vec<_>>();
        read.sort();
        let mut expected = vec![grown.locator.clone(), added.locator.clone()];
        expected.sort();
        assert_eq!(read, expected);
        let mut parsed = vec![grown.cache_key.clone(), added.cache_key.clone()];
        parsed.sort();
        assert_eq!(now.parsed_ids(), parsed);
    }

    #[test]
    fn session_read_again_previews_its_last_messages_when_asked() {
        let cache_base = admitted_cache_base();
        let stub = virtual_stub("ses_reread", 100, 1_000);
        let storage = VirtualStorage::new(vec![stub.clone()]);

        let preview_of = |show_last| match cache_session_with_cache(
            &storage,
            cache_base.path(),
            &stub.locator,
            show_last,
        )
        .into_read()
        {
            SessionRead::Listed(read) => read.preview,
            _ => panic!("the session reads as a listed session"),
        };

        assert_eq!(preview_of(true), "closing");
        assert_eq!(preview_of(false), "opening");
    }

    #[test]
    fn sessions_read_again_together_share_discovery_before_and_after_acquiring_ownership() {
        let cache_base = tempfile::tempdir().unwrap();
        let first = virtual_stub("ses_first", 100, 1_000);
        let second = virtual_stub("ses_second", 100, 2_000);
        let storage = VirtualStorage::new(vec![first.clone(), second.clone()]);

        let environment = environment_of(cache_base.path());
        let reads = SessionLoader::new(&environment, &storage, false, None, AgentMeter::UNMETERED)
            .reread_caching(&[&second.locator, &first.locator]);

        assert_eq!(
            *storage.discoveries.lock().unwrap(),
            2,
            "one discovery selects both requests; one refreshes both after acquiring ownership"
        );
        assert!(reads.iter().all(|read| read.next() == NextStep::Settled));
        let paths: Vec<_> = reads
            .iter()
            .map(|read| match read.read() {
                SessionRead::Listed(read) => read.path.clone(),
                _ => panic!("a session was not read as a listed session"),
            })
            .collect();
        assert_eq!(paths, [second.locator, first.locator], "in the order asked");
    }

    #[test]
    fn rediscovered_new_session_previews_its_last_messages_when_asked() {
        let cache_base = tempfile::tempdir().unwrap();
        let storage = VirtualStorage::new(vec![virtual_stub("ses_new", 100, 1_000)]);

        let rediscovered =
            rediscover_sessions_with_cache(&storage, cache_base.path(), true, &|_| true).unwrap();

        let [outcome] = rediscovered.outcomes.as_slice() else {
            panic!("the refresh read {} sessions", rediscovered.outcomes.len());
        };
        let SessionRead::Listed(read) = outcome.read() else {
            panic!("the new session was not read as a listed session");
        };
        assert_eq!(read.preview, "closing");
    }

    /// Another process, such as `agent search`, can cache a session before a
    /// refresh picks it. The session holds no conversation, the one kind of
    /// virtual session the database restores.
    #[test]
    fn rediscovered_session_cache_already_holds_is_restored_not_parsed() {
        let cache_base = admitted_cache_base();
        let storage = VirtualStorage::new(vec![virtual_stub("ses_cached", 100, 1_000)])
            .holding_nothing(["ses_cached"]);
        load_sessions_with_cache(&storage, cache_base.path(), false, None).unwrap();

        let again = VirtualStorage::new(vec![virtual_stub("ses_cached", 100, 1_000)])
            .holding_nothing(["ses_cached"]);
        let rediscovered =
            rediscover_sessions_with_cache(&again, cache_base.path(), false, &|_| true).unwrap();

        assert!(matches!(
            rediscovered
                .outcomes
                .iter()
                .map(SessionOutcome::read)
                .collect::<Vec<_>>()
                .as_slice(),
            [SessionRead::Empty]
        ));
        assert_eq!(again.parse_count(), 0);
    }

    /// Without a record of it, a transcript that holds no conversation is read
    /// in full on every load and contributes nothing. Most of a Codex corpus is
    /// sub-agent threads whose own content is empty, so this is the bulk of a
    /// warm load.
    #[test]
    fn session_that_holds_no_conversation_is_read_once() {
        let cache_base = admitted_cache_base();
        let storage = VirtualStorage::new(vec![virtual_stub("ses_empty", 200, 2_000)])
            .holding_nothing(["ses_empty"]);

        let cold = load_sessions_with_cache(&storage, cache_base.path(), false, None).unwrap();
        assert!(cold.is_empty(), "the empty session stays off the list");
        assert_eq!(storage.parsed_ids(), vec!["ses_empty"]);

        let warm = load_sessions_with_cache(&storage, cache_base.path(), false, None).unwrap();
        assert!(warm.is_empty(), "the warm load leaves it off the list");
        assert_eq!(
            storage.parsed_ids(),
            vec!["ses_empty"],
            "the session is read once"
        );
    }

    /// The record stands for the transcript's content, so it lasts exactly as
    /// long as the fingerprint does.
    #[test]
    fn session_that_gains_content_is_read_again() {
        let cache_base = admitted_cache_base();
        let empty = VirtualStorage::new(vec![virtual_stub("ses_grows", 100, 1_000)])
            .holding_nothing(["ses_grows"]);
        assert!(
            load_sessions_with_cache(&empty, cache_base.path(), false, None)
                .unwrap()
                .is_empty()
        );

        let grown = VirtualStorage::new(vec![virtual_stub("ses_grows", 400, 2_000)]);
        let listed = load_sessions_with_cache(&grown, cache_base.path(), false, None).unwrap();

        assert_eq!(listed.len(), 1);
        assert_eq!(grown.parsed_ids(), vec!["ses_grows"]);
    }

    /// A read can fail for a reason outside the transcript — a file held open,
    /// a partial write. Recording that as empty would hide the session until it
    /// changed on disk.
    #[test]
    fn session_that_could_not_be_read_is_not_recorded_as_empty() {
        let cache_base = admitted_cache_base();
        let failing = VirtualStorage::new(vec![virtual_stub("ses_locked", 100, 1_000)])
            .failing_transiently(["ses_locked"]);
        assert!(
            load_sessions_with_cache(&failing, cache_base.path(), false, None)
                .unwrap()
                .is_empty()
        );
        assert_eq!(failing.parsed_ids(), vec!["ses_locked"]);

        let readable = VirtualStorage::new(vec![virtual_stub("ses_locked", 100, 1_000)]);
        let listed = load_sessions_with_cache(&readable, cache_base.path(), false, None).unwrap();

        assert_eq!(listed.len(), 1, "the same fingerprint is read again");
        assert_eq!(readable.parsed_ids(), vec!["ses_locked"]);
    }

    #[test]
    fn rediscovery_reads_each_failure_as_transient_or_permanent() {
        let cache_base = tempfile::tempdir().unwrap();
        let storage = VirtualStorage::new(vec![
            virtual_stub("ses_busy", 100, 1_000),
            virtual_stub("ses_unreadable", 200, 2_000),
        ])
        .failing_transiently(["ses_busy"])
        .unreadable(["ses_unreadable"]);
        let environment = environment_of(cache_base.path());
        let loader = SessionLoader::new(&environment, &storage, false, None, AgentMeter::UNMETERED);

        let rediscovered = loader
            .rediscover(&|_| true, &waiting_operation(&environment))
            .unwrap();

        let read_of = |id: &str| {
            rediscovered
                .outcomes
                .iter()
                .find(|outcome| outcome.locator().ends_with(format!("{id}.jsonl")))
                .map(SessionOutcome::read)
        };
        assert!(matches!(
            read_of("ses_busy"),
            Some(SessionRead::Failed(ReadError::Transient))
        ));
        assert!(matches!(
            read_of("ses_unreadable"),
            Some(SessionRead::Failed(ReadError::Permanent))
        ));
    }

    #[test]
    fn session_whose_subagent_hits_transient_error_is_listed_without_it_and_isnt_cached() {
        let cache_base = admitted_cache_base();
        let busy = VirtualStorage::new(vec![stub_with_subagents("ses_parent", 1_000, 1)])
            .failing_transiently(["ses_parent_0"]);

        let listed = load_sessions_with_cache(&busy, cache_base.path(), false, None).unwrap();
        load_sessions_with_cache(&busy, cache_base.path(), false, None).unwrap();

        assert_eq!(
            listed[0].total_tokens, 10,
            "the sub-agent's tokens are missing"
        );
        assert!(listed[0].has_transient_read_error);
        assert!(stored_keys_under(cache_base.path(), &busy).is_empty());
        assert_eq!(busy.parse_count(), 2, "the next load parses it again");
    }

    #[test]
    fn session_is_cached_once_its_subagent_reads() {
        let cache_base = admitted_cache_base();
        let stubs = vec![stub_with_subagents("ses_parent", 1_000, 1)];
        let busy = VirtualStorage::new(stubs.clone()).failing_transiently(["ses_parent_0"]);
        load_sessions_with_cache(&busy, cache_base.path(), false, None).unwrap();
        let readable = VirtualStorage::new(stubs);

        let listed = load_sessions_with_cache(&readable, cache_base.path(), false, None).unwrap();

        assert_eq!(
            listed[0].total_tokens, 20,
            "the sub-agent's tokens are merged"
        );
        assert!(!listed[0].has_transient_read_error);
        assert_eq!(
            stored_keys_under(cache_base.path(), &readable),
            ["ses_parent"]
        );
    }

    #[test]
    fn session_whose_subagent_hits_permanent_error_is_cached_without_it() {
        let cache_base = admitted_cache_base();
        let broken = VirtualStorage::new(vec![stub_with_subagents("ses_parent", 1_000, 1)])
            .unreadable(["ses_parent_0"]);

        let listed = load_sessions_with_cache(&broken, cache_base.path(), false, None).unwrap();

        assert_eq!(
            listed[0].total_tokens, 10,
            "the sub-agent's tokens are missing"
        );
        assert!(!listed[0].has_transient_read_error);
        assert_eq!(
            stored_keys_under(cache_base.path(), &broken),
            ["ses_parent"]
        );
    }

    #[cfg(windows)]
    #[test]
    fn sharing_or_lock_violation_is_transient() {
        for code in [ERROR_SHARING_VIOLATION, ERROR_LOCK_VIOLATION] {
            let error = AppError::Io(std::io::Error::from_raw_os_error(code));
            assert!(is_transient(&error), "{error}");
        }
    }

    #[test]
    fn io_error_marked_busy_would_block_timed_out_or_interrupted_is_transient() {
        use std::io::ErrorKind;
        for kind in [
            ErrorKind::ResourceBusy,
            ErrorKind::WouldBlock,
            ErrorKind::TimedOut,
            ErrorKind::Interrupted,
        ] {
            assert!(is_transient(&AppError::Io(kind.into())), "{kind:?}");
        }
    }

    fn database_failure(code: std::ffi::c_int) -> AppError {
        crate::history::provider::sqlite::unusable_database(
            Path::new("opencode.db"),
            SessionDatabaseFailure::CannotBeRead,
            &rusqlite::Error::SqliteFailure(rusqlite::ffi::Error::new(code), None),
        )
    }

    #[test]
    fn busy_or_locked_session_database_is_transient() {
        for code in [rusqlite::ffi::SQLITE_BUSY, rusqlite::ffi::SQLITE_LOCKED] {
            assert!(is_transient(&database_failure(code)), "{code}");
        }
    }

    #[test]
    fn any_other_error_is_permanent() {
        use std::io::ErrorKind;
        for error in [
            AppError::Io(ErrorKind::NotFound.into()),
            AppError::Io(ErrorKind::PermissionDenied.into()),
            AppError::Io(ErrorKind::InvalidData.into()),
            AppError::Json(serde_json::from_str::<serde_json::Value>("{").unwrap_err()),
            database_failure(rusqlite::ffi::SQLITE_CORRUPT),
        ] {
            assert_eq!(ReadError::of(&error), ReadError::Permanent, "{error}");
        }
    }

    /// Progress for one load, `(done, total)` per report.
    fn progress_of_load(loader: &SessionLoader<'_>) -> Vec<(usize, usize)> {
        let mut reports = Vec::new();
        loader
            .load(
                LeftOutRows::NONE,
                &waiting_operation(loader.environment),
                &mut |done, total| reports.push((done, total)),
            )
            .unwrap();
        reports
    }

    fn stub_with_subagents(session_id: &str, modified_secs: u64, subagents: usize) -> SessionStub {
        let mut stub = virtual_stub(session_id, 100, modified_secs);
        stub.subagents = (0..subagents)
            .map(|index| PathBuf::from("container.db").join(format!("{session_id}_{index}.jsonl")))
            .collect();
        stub
    }

    /// A session with sub-agents moves the count once per transcript read,
    /// not once when the whole session completes.
    #[test]
    fn progress_counts_each_transcript_session_reads() {
        let cache_base = tempfile::tempdir().unwrap();
        let storage = VirtualStorage::new(vec![
            stub_with_subagents("ses_parent", 1_000, 2),
            virtual_stub("ses_other", 50, 500),
        ]);
        let environment = environment_of(cache_base.path());
        let loader = SessionLoader::new(&environment, &storage, false, None, AgentMeter::UNMETERED);

        let reports = progress_of_load(&loader);

        assert_eq!(reports, vec![(0, 4), (1, 4), (2, 4), (3, 4), (4, 4)]);
    }

    /// A session the database restores reads none of its transcripts; the
    /// count still reaches the total. The sessions hold no conversation, the
    /// one kind of virtual session the database restores.
    #[test]
    fn progress_reaches_total_when_sessions_are_not_read() {
        let cache_base = admitted_cache_base();
        let storage = VirtualStorage::new(vec![
            stub_with_subagents("ses_parent", 1_000, 2),
            virtual_stub("ses_empty", 5_000, 500),
        ])
        .holding_nothing(["ses_parent", "ses_empty"]);
        let environment = environment_of(cache_base.path());
        let loader = SessionLoader::new(&environment, &storage, false, None, AgentMeter::UNMETERED);
        progress_of_load(&loader);

        let reports = progress_of_load(&loader);

        assert_eq!(reports.first(), Some(&(0, 4)));
        assert_eq!(reports.last(), Some(&(4, 4)));
        assert_eq!(storage.parse_count(), 2, "the second load reads nothing");
    }

    /// Sessions load in parallel and still list in the order a sequential
    /// load gives: newest first, then discovery order, each with its index.
    #[test]
    fn parallel_load_lists_sessions_in_discovery_order_within_timestamp() {
        let cache_base = tempfile::tempdir().unwrap();
        let ids: Vec<String> = (0..64).map(|index| format!("ses_{index:02}")).collect();
        let storage = VirtualStorage::new(
            ids.iter()
                .map(|id| stub_with_subagents(id, 1_000, 3))
                .collect(),
        );

        let listed = load_sessions_with_cache(&storage, cache_base.path(), false, None).unwrap();

        let listed_ids: Vec<String> = listed
            .iter()
            .map(|conversation| {
                conversation
                    .path
                    .file_stem()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        assert_eq!(listed_ids, ids);
        assert!(
            listed
                .iter()
                .enumerate()
                .all(|(position, conversation)| conversation.index == position)
        );
    }
}
