//! The load loop shared by every provider that stores sessions under roots.

use super::outcome::{SessionOutcome, SessionRead};
use super::storage::{DiscoveredSessions, ResolvedSession, SessionStub, UnreadableDirectory};
use super::{Fingerprint, SessionRoot, SessionStorage, SessionTitle};
use crate::agent::diagnostic::AgentWarning;
use crate::agent::transcript::{read_failure_warning, unlisted_warning};
use crate::cli::DebugLevel;
use crate::clock::Clock;
use crate::debug;
use crate::error::{AppError, Result, SessionDatabaseFailure};
use crate::history::fingerprint::CachedFingerprint;
use crate::history::format::same_file;
use crate::history::load_meter::AgentMeter;
use crate::history::parser::{ParsedSession, UnlistedReason};
use crate::history::shard_files;
use crate::history::sqlite_cache::refresh::RefreshToken;
use crate::history::sqlite_cache::{
    self, CacheBase, CacheWrite, CacheWriter, CatalogueSession, InteractiveCache, LiveOwner,
    NextStep, OperationAccess, OperationScope, SessionRecord, SessionStatus, SqliteCache,
};
use crate::history::{
    Conversation, Environment, FilterTerm, ParsePermit, Source, format_short_name_from_path,
};
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
    /// The directories each root's last discovery could not list, in root
    /// order.
    pub unreadable_directories: Vec<UnreadableDirectory>,
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
    /// The warning agent commands print for a session the load read as
    /// holding no conversation or failing to read; `None` for a listed
    /// session, a stale copy, or a row the cache recorded without one.
    pub warning: Option<AgentWarning>,
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

impl RecordedSession {
    fn of(row: &Conversation) -> Self {
        Self {
            project_dir: row.cwd.clone(),
            session_id: row.session_id.clone(),
        }
    }
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

/// The session `session_id` names, from whichever of `environment`'s
/// storages holds it, read as [`read_resolved_session`] reads it, with the
/// agent that stores it. `None` when no storage resolves the ID.
pub fn load_session_by_id(
    environment: &Environment,
    session_id: &str,
    show_last: bool,
) -> Option<(Source, SessionOutcome)> {
    let (source, resolved) = super::resolve_session_id(environment, session_id)?;
    let read = read_resolved_session(environment, source, resolved, show_last);
    Some((source, read))
}

/// `source`'s session `resolved` names, as the row the list would have
/// shown, with the preview `show_last` selects. Read as the TUI's drawing
/// thread reads: from the session cache alone (see
/// [`SessionLoader::read_stubs_interactively`]). [`SessionRead::Missing`]
/// when `environment` does not read `source`'s storage.
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
/// requests. Read as the TUI's drawing thread reads: from the session cache
/// alone.
///
/// [`SessionRead::Missing`] when neither finds the file. A discovery that
/// fails reads as a [`ReadError::Transient`] failure: the session may still
/// be there, unchanged. A file under a directory discovery cannot list reads
/// as its cached copy, settled, or as that failure without a copy.
#[cfg(test)]
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
    let reread = loader.reread(
        locators,
        DatabaseReader::Interactive,
        |root, external_titles, stubs| {
            Ok::<_, std::convert::Infallible>(loader.read_stubs_interactively(
                root,
                external_titles,
                stubs,
            ))
        },
    );
    match reread {
        Ok(outcomes) => outcomes,
        Err(never) => match never {},
    }
}

/// The sessions at `locators`, one outcome each in order, as the list
/// builds their rows: the stub `source`'s discovery lists for each file,
/// however its path is spelled, with its sub-agent transcripts and the
/// preview `show_last` selects, or the file found through
/// [`SessionStorage::rediscover_unlisted_session`]. On a cache miss, wait
/// for `operation`'s refresh ownership as a load does, discover the files'
/// roots again under it, and cache each session the database does not
/// hold. Each session the database does not hold as read gets its
/// [`NextStep::after_read_without_writing`] as the read ends: wait for the
/// refresh running then, or a session refresh. A file under a directory
/// discovery cannot list reads as its cached copy, settled, or as a
/// [`ReadError::Transient`] failure without a copy. Fails on a database
/// error other than a busy lock. `meter` records what the read parsed.
pub fn cache_sessions(
    environment: &Environment,
    source: Source,
    locators: &[&Path],
    show_last: bool,
    operation: &OperationScope,
    meter: AgentMeter,
) -> Result<Vec<SessionOutcome>> {
    let Some(storage) = environment.storage_of(source) else {
        return Ok(all_missing(locators));
    };
    let loader = SessionLoader::new(environment, storage, show_last, None, meter);
    let reader = DatabaseReader::WithoutWaiting;
    loader.reread(locators, reader, |root, external_titles, stubs| {
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
    unreadable_directories: Vec<UnreadableDirectory>,
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
    deletions: Deletions<'a>,
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

    /// The session cache base this load reads and writes.
    fn cache_base(&self) -> &'a CacheBase {
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
        // Keep a provider's superseded cache, such as Claude's v0.3.1
        // project cache, when this load's writes stopped, so a user who
        // downgrades after a failed upgrade still finds it.
        let base = self.cache_base().path();
        if base.is_dir() && !operation.has_lost_write_permission() {
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
        let mut unreadable_directories = Vec::new();
        for (root, found) in discovered {
            let loaded_root = self.load_root(&root, found, left_out, operation, read)?;
            ignored.extend(loaded_root.ignored);
            conversations.extend(loaded_root.conversations);
            skipped.extend(loaded_root.skipped);
            discovered_sessions.extend(loaded_root.discovered);
            unreadable_directories.extend(loaded_root.unreadable_directories);
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
            unreadable_directories,
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
        loaded.unreadable_directories = found.unreadable_directories;
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
        let deletions = Deletions::of(found);
        if !self.may_hold_root(&stubs) {
            // Nothing to read, and no database to remove sessions from.
            let caches = RootCaches {
                access: CacheAccess::AsIs,
                database: None,
                deletions,
            };
            return Ok(self.load_root_writing(root, stubs, left_out, read, caches));
        }
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
            OperationAccess::AsIs => {
                self.load_root_as_is(root, stubs, left_out, deletions, operation, read)
            }
            OperationAccess::Owner(owner) => {
                self.load_root_owned(root, stubs, found, left_out, &owner, operation, read)
            }
        }
    }

    /// [`Self::load_root_sessions`] under `owner`'s refresh ownership: the
    /// root discovered again under the writer lock, every session the
    /// database does not hold as it is now parsed and written. A discovery
    /// that fails keeps the one from before. Once `owner` is no longer live,
    /// as after a write batch that fails, the root's other sessions are read
    /// from the cache as it is.
    #[allow(clippy::too_many_arguments)]
    fn load_root_owned(
        &self,
        root: &SessionRoot,
        stubs: Vec<SessionStub>,
        found: &mut DiscoveredSessions,
        left_out: LeftOutRows,
        owner: &LiveOwner<'_>,
        operation: &OperationScope,
        read: &mpsc::Sender<TranscriptProgress>,
    ) -> Result<LoadedRoot> {
        let stubs = match self.rediscover_owned(root, owner.token()) {
            Some(rediscovered) => adopt_rediscovery(stubs, found, rediscovered, read),
            None => stubs,
        };
        let snapshot = if owner.is_live() {
            self.open_database_for_refresh(root, &stubs, owner.token())?
        } else {
            None
        };
        let Some(snapshot) = snapshot else {
            return self.load_root_as_is(
                root,
                stubs,
                left_out,
                Deletions::of(found),
                operation,
                read,
            );
        };
        let caches = RootCaches {
            access: CacheAccess::Refresh(owner),
            database: None,
            deletions: Deletions::of(found),
        };
        let loaded = self.load_database_root(root, stubs, left_out, read, snapshot, caches);
        if owner.is_live() {
            shard_files::remove(self.cache_base().path(), self.storage.cache(), &root.path);
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
        deletions: Deletions,
        operation: &OperationScope,
        read: &mpsc::Sender<TranscriptProgress>,
    ) -> Result<LoadedRoot> {
        let snapshot = operation.read_when_unlocked(|| {
            self.open_database(root, &stubs, DatabaseReader::WithoutWaiting)
        })?;
        let caches = RootCaches {
            access: CacheAccess::AsIs,
            database: None,
            deletions,
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
        let Some(database) = self.open_database_writer(owner) else {
            return Ok(None);
        };
        let (snapshot, coordinated) = owner.read(|| self.read_root_cache(root, database));
        self.report_read_coordination(coordinated);
        let snapshot = snapshot?;
        Ok(owner.can_publish().then_some(snapshot))
    }

    /// False when the database cannot hold the root: no database file and no
    /// stubs to write.
    fn may_hold_root(&self, stubs: &[SessionStub]) -> bool {
        !stubs.is_empty()
            || self
                .cache_base()
                .path()
                .join(sqlite_cache::DATABASE_FILE_NAME)
                .exists()
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
    /// hold as it is now from its transcripts while the owner is live; once
    /// the owner is no longer live, the rest restore from their copies. A
    /// load that reads the cache as it is parses nothing: it restores what
    /// the database holds, and lists no row for a session the database
    /// lacks. The database's listed rows under a directory discovery could
    /// not list restore as they are.
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
        let rows_under_unlisted_directories = match (database, caches.deletions) {
            (Some(database), Deletions::Keep(directories)) => {
                listed_rows_under(database.catalogue, &stubs, directories)
            }
            _ => Vec::new(),
        };
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
        let unparsed = self.parse_in_groups(
            caches.access,
            &to_parse,
            |permit, &index| {
                let stub = &stubs[index];
                let announced = AtomicUsize::new(0);
                let on_transcript_read = || {
                    announced.fetch_add(1, Ordering::Relaxed);
                    self.meter.transcript_read();
                    let _ = read.send(TranscriptProgress::Completed(1));
                };
                let raw = self.parse(permit, root, stub, &on_transcript_read);
                let record =
                    database.and_then(|_| cache_record(permit, self.storage, root, stub, &raw));
                let unannounced =
                    transcript_count(stub).saturating_sub(announced.load(Ordering::Relaxed));
                if unannounced > 0 {
                    let _ = read.send(TranscriptProgress::Completed(unannounced));
                }
                (raw, record)
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
        for &index in unparsed {
            let stub = &stubs[index];
            outcomes[index] = Some(copy_as_it_is(stub, database, &external_titles));
            let _ = read.send(TranscriptProgress::Completed(transcript_count(stub)));
        }

        let outcomes = outcomes
            .into_iter()
            .map(|outcome| outcome.expect("every session restored or parsed"));
        let mut stale_copies = Vec::with_capacity(stubs.len());
        let mut warnings = Vec::with_capacity(stubs.len());
        for (stub, outcome) in stubs.iter().zip(outcomes) {
            let (raw, is_stale) = match outcome {
                RootRead::Read(raw) => (raw, false),
                RootRead::Stale(raw) => (raw, true),
                RootRead::NotCached => {
                    recorded.push(None);
                    stale_copies.push(Some(StaleCopy { cached_until: None }));
                    warnings.push(None);
                    continue;
                }
            };
            warnings.push(if is_stale {
                None
            } else {
                raw.warning().cloned()
            });
            let found = FoundSession {
                source: self.storage.source(),
                fingerprint: stub.fingerprint.stamp(),
                has_transient_read_error: false,
            };
            let mut stale_copy = is_stale.then_some(StaleCopy { cached_until: None });
            recorded.push(match self.session_read(stub, raw) {
                SessionRead::Listed(conversation) => {
                    let session = RecordedSession::of(&conversation);
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
                SessionRead::Failed(ReadError::Transient)
                | SessionRead::Missing
                | SessionRead::NotCached => None,
            });
            stale_copies.push(stale_copy);
        }
        let mut discovered: Vec<DiscoveredSession> = stubs
            .into_iter()
            .zip(recorded)
            .zip(stale_copies)
            .zip(warnings)
            .map(|(((stub, recorded), stale), warning)| DiscoveredSession {
                stub,
                recorded,
                stale,
                warning,
            })
            .collect();
        for (stub, row) in rows_under_unlisted_directories {
            let conversation =
                self.resolve_preview_and_project(restored_row(row, &stub, &external_titles));
            discovered.push(DiscoveredSession {
                stub,
                recorded: Some(RecordedSession::of(&conversation)),
                stale: None,
                warning: None,
            });
            conversations.push(conversation);
        }
        LoadedRoot {
            conversations,
            skipped,
            discovered,
            ignored: Vec::new(),
            unreadable_directories: Vec::new(),
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
            RawRead::Empty(_) => SessionRead::Empty,
            RawRead::Failed(error, _) => SessionRead::Failed(error),
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

    /// The session at `stub` read from its transcripts with `permit`, the
    /// parse recorded in the meter.
    fn parse(
        &self,
        permit: ParsePermit<'_>,
        root: &SessionRoot,
        stub: &SessionStub,
        on_transcript_read: &(dyn Fn() + Sync),
    ) -> RawRead {
        let parsing = self.clock().now();
        let outcome = parse_session(
            self.storage,
            permit,
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
        RawRead::of_parse(self.storage.source(), &stub.locator, outcome)
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
/// Without a database, the load reads the cache as it is and lists no row.
fn row_source<'a>(
    stub: &SessionStub,
    database: Option<&'a DatabaseWrites<'a>>,
    left_out: LeftOutRows,
    external_titles: &HashMap<String, SessionTitle>,
    reads_as_is: bool,
) -> RowSource<'a> {
    let Some(database) = database else {
        return RowSource::NotCached;
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

/// The read of `stub`'s session from its copy in `database`, whether or not
/// its transcript changed since: [`RootRead::NotCached`] without a copy.
fn copy_as_it_is(
    stub: &SessionStub,
    database: Option<&DatabaseWrites<'_>>,
    external_titles: &HashMap<String, SessionTitle>,
) -> RootRead {
    match database.and_then(|database| database.catalogue.get(&stub.cache_key)) {
        Some(catalogue) => {
            RootRead::Stale(restore_from_catalogue(catalogue, stub, external_titles))
        }
        None => RootRead::NotCached,
    }
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
        CatalogueSession::Empty(warning) => RawRead::Empty(warning.clone()),
        CatalogueSession::Unreadable(warning) => {
            RawRead::Failed(ReadError::Permanent, warning.clone())
        }
    }
}

/// The listed rows `catalogue` holds under `directories`, the directories
/// discovery could not list, of the sessions `stubs` leaves out, in path
/// order, each with a stub built from the row.
fn listed_rows_under<'c>(
    catalogue: &'c HashMap<String, CatalogueSession>,
    stubs: &[SessionStub],
    directories: &[UnreadableDirectory],
) -> Vec<(SessionStub, &'c Conversation)> {
    if directories.is_empty() {
        return Vec::new();
    }
    let discovered: HashSet<&str> = stubs.iter().map(|stub| stub.cache_key.as_str()).collect();
    let mut rows: Vec<(SessionStub, &Conversation)> = catalogue
        .iter()
        .filter(|(cache_key, _)| !discovered.contains(cache_key.as_str()))
        .filter_map(|(cache_key, session)| match session {
            CatalogueSession::Listed(row)
                if directories
                    .iter()
                    .any(|directory| row.path.starts_with(&directory.path)) =>
            {
                let stub = SessionStub {
                    locator: row.path.clone(),
                    subagents: row.subagents.clone(),
                    cache_key: cache_key.clone(),
                    fingerprint: Fingerprint::UNKNOWN,
                };
                Some((stub, &**row))
            }
            _ => None,
        })
        .collect();
    rows.sort_by(|(left, _), (right, _)| left.locator.cmp(&right.locator));
    rows
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

/// The session cache database's projection of `raw`, the parse of
/// `storage`'s session at `stub` under `root`. `None` for a transient
/// failure and for a row built without a transcript after a transient
/// error: the next load reads the session again. An empty or
/// unreadable session records the warning the parse built. `permit` admits
/// the parse of a listed session's entries.
fn cache_record(
    permit: ParsePermit<'_>,
    storage: &dyn SessionStorage,
    root: &SessionRoot,
    stub: &SessionStub,
    raw: &RawRead,
) -> Option<SessionRecord> {
    let unlisted = |status, warning: &AgentWarning| {
        SessionRecord::unlisted(
            storage.cache(),
            storage.source(),
            root,
            stub,
            status,
            warning.to_recorded(),
        )
    };
    match raw {
        RawRead::Parsed(conversation) if !conversation.has_transient_read_error => {
            SessionRecord::listed(
                permit,
                storage.cache(),
                storage.source(),
                root,
                stub,
                conversation,
            )
        }
        RawRead::Empty(Some(warning)) => unlisted(SessionStatus::Empty, warning),
        RawRead::Failed(ReadError::Permanent, Some(warning)) => {
            unlisted(SessionStatus::Unreadable, warning)
        }
        RawRead::Parsed(_) | RawRead::Restored(_) | RawRead::Empty(None) | RawRead::Failed(..) => {
            None
        }
    }
}

/// A discovered session's read, before the list's preview and project are
/// filled in.
///
/// The two reads that yield no conversation are separate variants because
/// they cache differently. Only `Empty` says something about the transcript's
/// content, and content is all a fingerprint can stand for. Both carry the
/// warning agent commands print for the session: built from the parse, or
/// as the cache recorded it (`None` for a row recorded without one).
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
    Empty(Option<AgentWarning>),
    /// Could not be read or parsed. Not cached, so the next launch reads it
    /// again.
    Failed(ReadError, Option<AgentWarning>),
}

impl RawRead {
    /// The read `parsed`, the parse of `source`'s session at `path`, yields.
    fn of_parse(source: Source, path: &Path, parsed: Result<ParsedSession>) -> Self {
        match parsed {
            Ok(Ok(conversation)) => Self::Parsed(conversation),
            Ok(Err(reason)) => Self::Empty(Some(unlisted_warning(source, reason))),
            Err(error) => Self::Failed(
                ReadError::of(&error),
                Some(read_failure_warning(path, &error)),
            ),
        }
    }

    /// The warning agent commands print for a session read as holding no
    /// conversation or failing to read; `None` for a listed session.
    fn warning(&self) -> Option<&AgentWarning> {
        match self {
            Self::Empty(warning) | Self::Failed(_, warning) => warning.as_ref(),
            Self::Restored(_) | Self::Parsed(_) => None,
        }
    }
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
    permit: ParsePermit<'_>,
    stub: &SessionStub,
    root: &SessionRoot,
    debug_level: Option<DebugLevel>,
    on_transcript_read: &(dyn Fn() + Sync),
    clock: &dyn Clock,
) -> Result<ParsedSession> {
    let parsed = storage.parse_session(permit, stub, root, debug_level, on_transcript_read, clock);
    match parsed {
        Ok(Ok(conversation)) if conversation.source != storage.source() => {
            Ok(Err(UnlistedReason::ForeignTranscript))
        }
        Err(error) => {
            debug::warn(
                debug_level,
                &format!(
                    "Failed to parse {} session {}: {error}",
                    storage.source().list_label(),
                    stub.locator.display()
                ),
            );
            Err(error)
        }
        parsed => parsed,
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
    use crate::history::provider::Fingerprint;
    use crate::history::provider::walk::SystemLister;
    use crate::history::provider::walk::test_support::ScriptedLister;
    use crate::history::sqlite_cache::refresh::{Acquisition, RefreshOwnership};
    use crate::history::{AwaitedRefresh, OtherRearview, ProcessIdentity};
    use crate::tui::claude_home::ClaudeHome;
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
            cache_base.to_path_buf(),
            ProcessIdentity::default(),
            Arc::new(SystemClock::new()),
            Arc::new(SystemLister),
        )
    }

    /// [`environment_of`] listing directories through `home`'s lister.
    fn environment_listing_home(cache_base: &Path, home: &ClaudeHome) -> Environment {
        home.environment_with_cache(
            cache_base,
            ProcessIdentity::default(),
            Arc::new(SystemClock::new()),
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
                .reread(
                    locators,
                    DatabaseReader::WithoutWaiting,
                    |root, external_titles, stubs| {
                        self.read_stubs(
                            root,
                            external_titles,
                            stubs,
                            StubsAfterRediscovery::SameFiles,
                            &operation,
                        )
                    },
                )
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

    impl SessionLoader<'_> {
        /// [`SessionLoader::read_one`]'s row, when the session holds one.
        fn load_one(&self, root: &SessionRoot, stub: &SessionStub) -> Option<Conversation> {
            listed_row(self.read_one(root, stub.clone()))
        }

        /// The session `stub` names under `root`, read as a session refresh
        /// on the TUI's background worker reads it to cache it, under an
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
            listed_row(self.cache_one(root, stub.clone()))
        }
    }

    /// `outcome`'s row, when the session holds one.
    fn listed_row(outcome: SessionOutcome) -> Option<Conversation> {
        match outcome.into_parts().0 {
            SessionRead::Listed(conversation) => Some(*conversation),
            SessionRead::Empty
            | SessionRead::Missing
            | SessionRead::Failed(_)
            | SessionRead::NotCached => None,
        }
    }

    #[test]
    fn load_reuses_every_providers_cached_session() {
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
            let ownership = RefreshOwnership::acquire_for_test(environment.cache_base()).unwrap();
            let permit = crate::history::FOR_FIXTURES;
            let parsed = parse_session(
                storage,
                permit,
                &stub,
                &root,
                None,
                &|| {},
                environment.clock().as_ref(),
            );
            let raw = RawRead::of_parse(storage.source(), &stub.locator, parsed);
            let record = cache_record(permit, storage, &root, &stub, &raw)
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
    fn waiter_rediscovers_sessions_before_reconciling_deletions() {
        use crate::history::sqlite_cache::refresh::tests::AnotherRearviewsOwner;
        let base = tempfile::tempdir().unwrap();
        let home = ClaudeHome::new();
        let old = discovered_stub(&home, &home.write("old", 0, &["a question"]));
        let added = discovered_stub(&home, &home.write("added", 1, &["a question"]));
        let storage = home.storage();
        let root = home.root();
        let database = SqliteCache::open(&rearview(base.path())).unwrap();
        database.mark_admitted().unwrap();
        database.admit_storage(storage.cache()).unwrap();
        drop(database);
        let owner = AnotherRearviewsOwner::hold(base.path());
        let meter = LoadMeter::default();
        let environment = environment_of(base.path());
        std::thread::scope(|scope| {
            let loader = SessionLoader::new(
                &environment,
                storage,
                false,
                None,
                meter.agent(Source::Claude),
            );
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
            assert_eq!(home.watch().parses_begun(), 0);
            let mut writer = SqliteCache::open_for_refresh(&owner.token()).unwrap();
            for stub in [&old, &added] {
                writer
                    .write(
                        &SessionRecord::unlisted(
                            storage.cache(),
                            Source::Claude,
                            &root,
                            stub,
                            SessionStatus::Empty,
                            "skipped transcript has no visible messages".to_owned(),
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
                home.watch().parses_begun(),
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
    fn warm_sqlite_load_doesnt_wait_for_another_process() {
        use crate::history::sqlite_cache::refresh::tests::AnotherRearviewsOwner;
        let base = tempfile::tempdir().unwrap();
        let home = ClaudeHome::new();
        let stub = discovered_stub(&home, &home.write("warm", 0, &["a question"]));
        let storage = home.storage();
        let root = home.root();
        let mut database = SqliteCache::open(&rearview(base.path())).unwrap();
        database.mark_admitted().unwrap();
        database.admit_storage(storage.cache()).unwrap();
        database
            .write(
                &SessionRecord::unlisted(
                    storage.cache(),
                    Source::Claude,
                    &root,
                    &stub,
                    SessionStatus::Empty,
                    "skipped transcript has no visible messages".to_owned(),
                )
                .unwrap(),
            )
            .unwrap();
        drop(database);
        let owner = AnotherRearviewsOwner::hold(base.path());
        std::thread::scope(|scope| {
            let (finished, result) = mpsc::channel();
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
        assert_eq!(home.watch().parses_begun(), 0);
    }

    #[test]
    fn deletion_waits_for_refresh_owner_then_leaves_database() {
        use crate::history::sqlite_cache::refresh::tests::AnotherRearviewsOwner;
        let base = admitted_cache_base();
        let home = ClaudeHome::new();
        let transcript = home.write("deleted", 0, &["a question"]);
        let key = discovered_stub(&home, &transcript).cache_key;
        load_sessions_with_cache(home.storage(), base.path(), false, None).unwrap();
        let owner = AnotherRearviewsOwner::hold(base.path());
        std::fs::remove_file(&transcript).unwrap();
        let meter = LoadMeter::default();
        let environment = environment_of(base.path());
        std::thread::scope(|scope| {
            let running = scope.spawn(|| {
                let operation = operation_shown_by(&environment, &meter);
                let loaded = load_sessions(
                    &environment,
                    home.storage(),
                    false,
                    None,
                    meter.agent(Source::Claude),
                    LeftOutRows::NONE,
                    &operation,
                    &mut |_, _| {},
                );
                operation.finish().and(loaded).unwrap()
            });
            wait_for_refresh(&meter);
            assert_eq!(stored_keys_in(base.path(), &home), [key]);
            owner.release();
            assert!(running.join().unwrap().conversations.is_empty());
        });
        assert!(stored_keys_in(base.path(), &home).is_empty());
    }

    #[test]
    fn session_cache_that_cannot_be_opened_fails_load_without_parsing() {
        let base = admitted_cache_base();
        let home = ClaudeHome::new();
        let transcript = home.write("changed", 0, &["a question"]);
        load_sessions_with_cache(home.storage(), base.path(), false, None).unwrap();
        let database = base.path().join(sqlite_cache::DATABASE_FILE_NAME);
        std::fs::remove_file(&database).unwrap();
        std::fs::create_dir(&database).unwrap();
        home.append(&transcript, 1, "a later question");
        home.watch().mark();

        let loaded = load_sessions_with_cache(home.storage(), base.path(), false, None);

        assert!(loaded.is_err(), "the load fails without its session cache");
        assert_eq!(home.watch().parses_after_mark(), 0);
    }

    #[test]
    fn interactive_read_doesnt_wait_or_parse_while_another_process_refreshes() {
        use crate::history::sqlite_cache::refresh::tests::AnotherRearviewsOwner;
        let base = tempfile::tempdir().unwrap();
        let home = ClaudeHome::new();
        let target = discovered_stub(&home, &home.write("target", 0, &["a question"]));
        home.write("other", 1, &["a question"]);
        let storage = home.storage();
        let root = home.root();
        let database = SqliteCache::open(&rearview(base.path())).unwrap();
        database.mark_admitted().unwrap();
        database.admit_storage(storage.cache()).unwrap();
        drop(database);
        let owner = AnotherRearviewsOwner::hold(base.path());
        let environment = environment_of(base.path());
        let loader = SessionLoader::new(&environment, storage, false, None, AgentMeter::UNMETERED);

        let started = Instant::now();
        let (read, next) = loader.read_one(&root, target).into_parts();

        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the read doesn't wait for the other refresh"
        );
        assert_eq!(
            next,
            NextStep::AwaitRefresh(AwaitedRefresh::AnotherRearview(OtherRearview::Refreshing))
        );
        assert!(matches!(read, SessionRead::NotCached));
        assert!(
            home.watch().parsed_sessions().is_empty(),
            "the read doesn't parse the transcript"
        );
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
    fn interactive_read_doesnt_wait_or_parse_during_another_processs_rebuild() {
        use crate::history::sqlite_cache::refresh::tests::{AnotherRearviewsOwner, OwnerActivity};
        let base = tempfile::tempdir().unwrap();
        let home = ClaudeHome::new();
        let target = discovered_stub(&home, &home.write("target", 0, &["a question"]));
        let root = home.root();
        admit_database(base.path());
        let rebuild = AnotherRearviewsOwner::start(base.path(), OwnerActivity::InsideRebuild);
        let environment = environment_of(base.path());
        let loader = SessionLoader::new(
            &environment,
            home.storage(),
            false,
            None,
            AgentMeter::UNMETERED,
        );

        let started = Instant::now();
        let (read, next) = loader.read_one(&root, target).into_parts();

        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the read doesn't wait for the rebuild"
        );
        assert_eq!(
            next,
            NextStep::AwaitRefresh(AwaitedRefresh::AnotherRearview(OtherRearview::Rebuilding))
        );
        assert!(matches!(read, SessionRead::NotCached));
        assert!(
            home.watch().parsed_sessions().is_empty(),
            "the read doesn't parse the transcript"
        );
        drop(rebuild);
    }

    #[test]
    fn targeted_read_whose_repeated_discovery_fails_caches_requested_session() {
        let base = admitted_cache_base();
        let home = ClaudeHome::new();
        let stub = discovered_stub(&home, &home.write("readable", 0, &["a question"]));
        let root = home.root();
        home.refuse(&root.path);
        let environment = environment_listing_home(base.path(), &home);
        let loader = SessionLoader::new(
            &environment,
            home.storage(),
            false,
            None,
            AgentMeter::UNMETERED,
        );

        let (read, next) = loader.cache_one(&root, stub.clone()).into_parts();

        assert!(matches!(read, SessionRead::Listed(_)));
        assert_eq!(next, NextStep::Settled);
        assert_eq!(home.times_listed(&root.path), 1);
        assert_eq!(home.watch().parsed_sessions(), [stub.locator]);
        assert_eq!(stored_keys_in(base.path(), &home), [stub.cache_key]);
        assert!(
            matches!(
                RefreshOwnership::try_acquire_now(&rearview(base.path())).unwrap(),
                Acquisition::Acquired(_)
            ),
            "the read releases refresh ownership"
        );
    }

    #[test]
    fn uncontended_load_never_reports_waiting_for_refresh_ownership() {
        use crate::agent::perf::CommandPerf;
        use crate::agent::progress::{ProgressRecords, ProgressSchedule, run_reporting_progress};
        use std::sync::atomic::AtomicBool;
        let base = admitted_cache_base();
        let home = ClaudeHome::new();
        for (minute, name) in ["first", "second", "third"].into_iter().enumerate() {
            home.write(name, minute as u32, &["a question"]);
        }
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
                        home.storage(),
                        base.path(),
                        perf.load_meter().agent(Source::Claude),
                        LeftOutRows::NONE,
                    )
                    .unwrap()
                },
            );
            loaded.store(true, Ordering::Relaxed);
        });

        assert_eq!(
            home.watch().parses_begun(),
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
        let home = ClaudeHome::new();
        let parent = home.write("parent", 1, &["a question"]);
        home.write_subagent(&parent, "child", 1, &["a sub-agent question"]);
        home.write_subagent(&parent, "nested", 1, &["a nested question"]);
        let other = home.write("other", 0, &["a question"]);

        let listed =
            load_sessions_with_cache(home.storage(), cache_base.path(), false, None).unwrap();

        assert_eq!(listed.len(), 2);
        let mut sessions = vec![
            discovered_stub(&home, &other),
            discovered_stub(&home, &parent),
        ];
        sessions.sort_by(|left, right| left.locator.cmp(&right.locator));
        assert_eq!(
            home.watch().parsed_sessions(),
            sessions
                .iter()
                .map(|stub| stub.locator.clone())
                .collect::<Vec<_>>()
        );
        let mut keys: Vec<_> = sessions.into_iter().map(|stub| stub.cache_key).collect();
        keys.sort();
        assert_eq!(stored_keys_in(cache_base.path(), &home), keys);
    }

    /// A session refresh caches a session opened by id with the
    /// same step as the list: it joins the root's sessions in the database,
    /// and the next load restores it unparsed.
    #[test]
    fn session_loaded_by_id_is_cached_beside_roots_others() {
        let cache_base = admitted_cache_base();
        let home = ClaudeHome::new();
        let listed = discovered_stub(&home, &home.write("listed", 0, &["a question"]));
        let by_id = discovered_stub(&home, &home.write("by-id", 1, &["a question"]));
        let environment = environment_of(cache_base.path());
        let loader = SessionLoader::new(
            &environment,
            home.storage(),
            false,
            None,
            AgentMeter::UNMETERED,
        );
        let root = home.root();
        let _listed = loader.cache_one(&root, listed.clone());

        let (read, _) = loader.cache_one(&root, by_id.clone()).into_parts();

        assert!(matches!(read, SessionRead::Listed(_)));
        let mut parsed = vec![by_id.locator, listed.locator];
        parsed.sort();
        assert_eq!(home.watch().parsed_sessions(), parsed);
        let mut keys = vec![by_id.cache_key, listed.cache_key];
        keys.sort();
        assert_eq!(stored_keys_in(cache_base.path(), &home), keys);

        load_sessions_with_cache(home.storage(), cache_base.path(), false, None).unwrap();
        assert_eq!(
            home.watch().parsed_sessions(),
            parsed,
            "the session loaded by id restores from the cache"
        );
    }

    /// One count for the whole provider: a total that restarted at each root
    /// would show the indicator going backwards.
    #[test]
    fn progress_counts_every_discovered_session_across_all_roots() {
        let cache_base = tempfile::tempdir().unwrap();
        let (first, second) = (ClaudeHome::new(), ClaudeHome::new());
        first.write("first", 0, &["a question"]);
        first.write("second", 1, &["a question"]);
        second.write("third", 2, &["a question"]);
        let storage = crate::history::provider::WatchedStorage {
            inner: Source::Claude.provider().storage(),
            roots: vec![first.root(), second.root()],
            watch: crate::history::provider::ParseWatch::default(),
        };
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
        let home = CodexHome::new();
        home.write_rollout();
        let loaded = load_with_cache(home.storage, cache_base.path(), false, None).unwrap();
        assert_eq!(loaded.conversations.len(), 1);
        assert!(
            loaded.ignored.is_empty(),
            "a reason nothing was ignored for doesn't make a term"
        );

        home.write_compressed_rollouts(0..3);
        let loaded = load_with_cache(home.storage, cache_base.path(), false, None).unwrap();

        assert_eq!(loaded.conversations.len(), 1);
        assert_eq!(
            loaded.ignored,
            vec![FilterTerm::new(
                "Codex",
                "3 ignored: compressed sessions unsupported"
            )]
        );
    }

    /// Codex rollouts of one day in a temporary home, read through Codex's
    /// storage pinned to the home's `sessions` root and listed through a
    /// lister the test scripts.
    struct CodexHome {
        directory: tempfile::TempDir,
        lister: Arc<ScriptedLister>,
        storage: &'static dyn SessionStorage,
    }

    impl CodexHome {
        fn new() -> Self {
            let directory = tempfile::tempdir().unwrap();
            let storage: &'static dyn SessionStorage =
                Box::leak(Box::new(crate::history::provider::WatchedStorage {
                    inner: Source::Codex.provider().storage(),
                    roots: vec![SessionRoot::new(directory.path().join("sessions"))],
                    watch: crate::history::provider::ParseWatch::default(),
                }));
            let home = Self {
                directory,
                lister: Arc::default(),
                storage,
            };
            std::fs::create_dir_all(home.day()).unwrap();
            home
        }

        fn root(&self) -> SessionRoot {
            SessionRoot::new(self.directory.path().join("sessions"))
        }

        /// The directory holding the day's rollouts.
        fn day(&self) -> PathBuf {
            self.directory.path().join("sessions/2026/08/01")
        }

        /// Write the rollout fixture where Codex files it. Returns its path.
        fn write_rollout(&self) -> PathBuf {
            let rollout = self
                .day()
                .join("rollout-2026-08-01T10-00-00-019f0000-0000-7000-8000-00000000000a.jsonl");
            std::fs::copy(
                Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/codex/rollout.jsonl"),
                &rollout,
            )
            .unwrap();
            rollout
        }

        /// [`write_compressed_rollouts_into`] the day's directory.
        fn write_compressed_rollouts(&self, threads: std::ops::Range<u32>) {
            write_compressed_rollouts_into(&self.day(), threads);
        }

        /// Codex's storage pinned here, listed through this home's lister,
        /// with the session cache under `cache_base`.
        fn environment(&self, cache_base: &Path) -> Environment {
            Environment::new(
                vec![self.storage],
                cache_base.to_path_buf(),
                ProcessIdentity::default(),
                Arc::new(SystemClock::new()),
                self.lister.clone(),
            )
        }

        /// A discovery after the first finds the day's compressed rollouts
        /// grown from two to seven and a second day directory it cannot list.
        /// Returns that directory.
        fn change_between_first_and_second_discovery(&self) -> PathBuf {
            self.write_compressed_rollouts(100..102);
            let blocked = self.directory.path().join("sessions/2026/08/02");
            std::fs::create_dir_all(&blocked).unwrap();
            let day = self.day();
            self.lister.run_before_listing(&self.day(), 2, move || {
                write_compressed_rollouts_into(&day, 102..107);
            });
            self.lister.refuse_from_listing(&blocked, 2);
            blocked
        }
    }

    /// Write into the day directory `day` one rollout as Codex's compression
    /// leaves it for each of `threads`; discovery counts them and never
    /// decodes them.
    fn write_compressed_rollouts_into(day: &Path, threads: std::ops::Range<u32>) {
        for thread in threads {
            let name = format!(
                "rollout-2026-08-01T09-00-00-019f0000-0000-7000-8000-{thread:012}.jsonl.zst"
            );
            std::fs::write(day.join(name), "never decoded").unwrap();
        }
    }

    /// Removing the per-project cache of releases up to v0.3.1 waits for
    /// this cache to be written, so a downgrade before then still finds its
    /// own.
    #[test]
    fn load_removes_superseded_cache_after_writing_its_own() {
        let base = admitted_cache_base();
        let home = ClaudeHome::new();
        let stub = discovered_stub(&home, &home.write("first", 0, &["a question"]));
        let earlier_cache = base.path().join("projects").join("-tmp-project.bin");
        std::fs::create_dir_all(earlier_cache.parent().unwrap()).unwrap();
        std::fs::write(
            &earlier_cache,
            [
                &super::super::claude::PROJECT_CACHE_MAGIC[..],
                &16u32.to_le_bytes(),
            ]
            .concat(),
        )
        .unwrap();
        let environment = environment_of(base.path());
        let loader = SessionLoader::new(
            &environment,
            home.storage(),
            false,
            None,
            AgentMeter::UNMETERED,
        );

        fail_every_commit_after_acquisition(base.path());
        loader.load_waiting().unwrap();
        assert!(
            stored_keys_in(base.path(), &home).is_empty(),
            "the load doesn't store a session while its commits fail"
        );
        assert!(
            earlier_cache.exists(),
            "a load whose commits fail keeps the earlier release's cache"
        );

        allow_every_commit(base.path());
        let (read, _) = loader.cache_one(&home.root(), stub.clone()).into_parts();
        assert!(matches!(read, SessionRead::Listed(_)));
        assert_eq!(stored_keys_in(base.path(), &home), [stub.cache_key]);
        assert!(
            earlier_cache.exists(),
            "opening one session by ID keeps the earlier release's cache"
        );

        home.watch().mark();
        loader.load_waiting().unwrap();
        assert_eq!(
            home.watch().parses_after_mark(),
            0,
            "the last load doesn't parse"
        );
        assert!(
            !earlier_cache.exists(),
            "a warm load removes the earlier release's cache"
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
    fn load_through_database_removes_cache_files_of_earlier_releases() {
        let base = tempfile::tempdir().unwrap();
        admit_database(base.path());
        let home = ClaudeHome::new();
        let key = discovered_stub(&home, &home.write("first", 0, &["a question"])).cache_key;
        let storage = home.storage();
        let earlier_cache =
            shard_directory_of_earlier_releases(base.path(), storage, &home.root().path);
        std::fs::create_dir_all(&earlier_cache).unwrap();
        std::fs::write(earlier_cache.join("shard-00.bin"), b"an earlier release").unwrap();

        load_sessions_with_cache(storage, base.path(), false, None).unwrap();

        assert!(
            !base.path().join(storage.cache().directory).exists(),
            "the load removes the cache files of earlier releases"
        );
        assert_eq!(stored_keys_in(base.path(), &home), [key]);
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
    fn reads_and_parses_hold_writer_lock_without_blocking_write_queue() {
        let base = tempfile::tempdir().unwrap();
        let home = ClaudeHome::new();
        for index in 0..13 {
            home.write(&format!("session-{index:02}"), 0, &["a question"]);
        }
        let locked_during_parse = std::sync::Arc::new(Mutex::new(Vec::new()));
        let observed = locked_during_parse.clone();
        let observed_base = base.path().to_path_buf();
        home.watch().run_as_each_parse_begins(move || {
            observed
                .lock()
                .unwrap()
                .push(is_writer_locked(&observed_base));
        });
        let database = SqliteCache::open(&rearview(base.path())).unwrap();
        database.mark_admitted().unwrap();
        database.admit_storage(home.storage().cache()).unwrap();
        drop(database);
        let loaded = load_sessions_with_cache(home.storage(), base.path(), false, None).unwrap();
        assert_eq!(loaded.len(), 13);
        assert_eq!(home.watch().parses_begun(), 13);
        assert_eq!(stored_keys_in(base.path(), &home).len(), 13);
        // A session refresh's targeted reads use the same
        // writer lock.
        let extra = discovered_stub(&home, &home.write("extra", 0, &["a question"]));
        let environment = environment_of(base.path());
        let loader = SessionLoader::new(
            &environment,
            home.storage(),
            false,
            None,
            AgentMeter::UNMETERED,
        );
        assert!(loader.cache_row(&home.root(), &extra).is_some());
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
        let home = CodexHome::new();
        let rollout = home.write_rollout();
        home.change_between_first_and_second_discovery();
        let environment = home.environment(base.path());
        let loader = SessionLoader::new(
            &environment,
            home.storage,
            false,
            None,
            AgentMeter::UNMETERED,
        );
        let loaded = loader.load_waiting().unwrap();
        assert_eq!(
            loaded.ignored,
            vec![FilterTerm::new(
                "Codex",
                "7 ignored: compressed sessions unsupported"
            )]
        );
        assert_eq!(
            stored_keys_under(base.path(), home.storage, &home.root().path),
            [codex_cache_key(&home, &rollout)],
            "the incomplete walk's session is cached"
        );
    }

    /// The cache key Codex's discovery gives the rollout at `rollout`.
    fn codex_cache_key(home: &CodexHome, rollout: &Path) -> String {
        home.storage
            .discover(&home.root(), &SystemLister)
            .unwrap()
            .stubs
            .into_iter()
            .find(|stub| stub.locator == rollout)
            .expect("discovery lists the rollout")
            .cache_key
    }

    #[test]
    fn session_deleted_between_discovery_and_acquisition_is_absent_on_reread() {
        let base = tempfile::tempdir().unwrap();
        let home = ClaudeHome::new();
        let transcript = home.write("deleted", 0, &["a question"]);
        let deleted = transcript.clone();
        home.run_before_listing(&home.project(), 2, move || {
            std::fs::remove_file(&deleted).unwrap();
        });
        let environment = environment_listing_home(base.path(), &home);
        let loader = SessionLoader::new(
            &environment,
            home.storage(),
            false,
            None,
            AgentMeter::UNMETERED,
        );
        assert!(loader.reread_caching(&[&transcript])[0].is_missing());
    }

    #[test]
    fn losing_ownership_before_discovery_returns_discovery_without_write_permission() {
        use crate::history::sqlite_cache::refresh::tests::replace_owner;
        let base = tempfile::tempdir().unwrap();
        let home = ClaudeHome::new();
        let stub = discovered_stub(&home, &home.write("readable", 0, &["a question"]));
        let environment = environment_listing_home(base.path(), &home);
        let loader = SessionLoader::new(
            &environment,
            home.storage(),
            false,
            None,
            AgentMeter::UNMETERED,
        );
        let ownership = RefreshOwnership::acquire_for_test(&rearview(base.path())).unwrap();
        let replacement = replace_owner(&rearview(base.path()));
        let root = home.root();

        let rediscovered = loader.rediscover_owned(&root, ownership.token());

        assert_eq!(
            rediscovered.expect("the discovery is returned").stubs,
            vec![stub]
        );
        assert!(
            !ownership.token().can_publish(),
            "the lost ownership doesn't regain write permission"
        );
        assert_eq!(home.times_listed(&root.path), 1);
        replacement.write_batch(|| ()).unwrap();
    }

    #[test]
    fn discovery_failing_after_acquisition_caches_earlier_discovery() {
        let base = tempfile::tempdir().unwrap();
        let home = ClaudeHome::new();
        let stub = discovered_stub(&home, &home.write("readable", 0, &["a question"]));
        let root = home.root();
        home.refuse_from_listing(&root.path, 2);
        let database = SqliteCache::open(&rearview(base.path())).unwrap();
        database.mark_admitted().unwrap();
        database.admit_storage(home.storage().cache()).unwrap();
        drop(database);
        let environment = environment_listing_home(base.path(), &home);

        let loaded = SessionLoader::new(
            &environment,
            home.storage(),
            false,
            None,
            AgentMeter::UNMETERED,
        )
        .load_waiting()
        .unwrap();

        assert_eq!(loaded.conversations.len(), 1);
        assert_eq!(home.times_listed(&root.path), 2);
        assert_eq!(home.watch().parsed_sessions(), [stub.locator]);
        assert_eq!(stored_keys_in(base.path(), &home), [stub.cache_key]);
        assert!(
            matches!(
                RefreshOwnership::try_acquire_now(&rearview(base.path())).unwrap(),
                Acquisition::Acquired(_)
            ),
            "the load releases refresh ownership"
        );
    }

    /// A discovery batch whose commit fails stops full and targeted reads:
    /// they parse nothing, repeat no discovery and leave the cache as it was.
    #[test]
    fn discovery_batch_whose_commit_fails_stops_read_without_parsing_or_writing() {
        for targeted in [false, true] {
            let base = admitted_cache_base();
            let home = ClaudeHome::new();
            let transcript = home.write("readable", 0, &["a question"]);
            let old = discovered_stub(&home, &transcript);
            load_sessions_with_cache(home.storage(), base.path(), false, None).unwrap();
            let recorded = || {
                recorded_fingerprint_of(base.path(), home.storage(), &home.root().path, &transcript)
            };
            let before = recorded();
            assert!(before.is_some(), "the seeding load caches the session");
            fail_every_commit_after_acquisition(base.path());
            home.append(&transcript, 1, "a later question");
            home.watch().mark();
            let environment = environment_listing_home(base.path(), &home);
            let loader = SessionLoader::new(
                &environment,
                home.storage(),
                false,
                None,
                AgentMeter::UNMETERED,
            );
            if targeted {
                let _ = loader.cache_one(&home.root(), old.clone());
            } else {
                loader.load_waiting().unwrap();
            }
            assert_eq!(
                home.watch().parses_after_mark(),
                0,
                "a read whose writes stop doesn't parse"
            );
            assert_eq!(
                home.times_listed(&home.root().path),
                if targeted { 1 } else { 2 },
                "a completed discovery isn't repeated after its commit fails"
            );
            assert_eq!(
                recorded(),
                before,
                "a read whose writes stop doesn't change the cache (targeted: {targeted})"
            );
        }
    }

    /// The fingerprint `cache_base`'s database holds of each of `home`'s
    /// sessions, by cache key.
    fn cached_fingerprints_in(
        cache_base: &Path,
        home: &ClaudeHome,
    ) -> HashMap<String, CachedFingerprint> {
        SqliteCache::open_waiting(&rearview(cache_base))
            .unwrap()
            .unwrap()
            .current_fingerprints(home.storage().cache(), &home.root().path)
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

    /// Undo [`fail_every_commit_after_acquisition`] in `base`'s database.
    fn allow_every_commit(base: &Path) {
        rusqlite::Connection::open(base.join(sqlite_cache::DATABASE_FILE_NAME))
            .unwrap()
            .execute_batch("DROP TRIGGER fail_commit")
            .unwrap();
    }

    /// A read whose ownership another rearview took over reads the copy as
    /// it is and waits for that rearview's refresh.
    #[test]
    fn read_whose_ownership_another_rearview_took_over_waits_for_its_refresh() {
        use crate::history::sqlite_cache::refresh::tests::replace_owner;
        let sessions = crate::tui::claude_home::ChangedSessionsInTwoRoots::new();
        let environment = sessions.environment();
        let operation = waiting_operation(&environment);
        assert!(matches!(
            operation.access().unwrap(),
            OperationAccess::Owner(_)
        ));
        let _replacement = replace_owner(&rearview(sessions.cache.path()));
        sessions.watch.mark();

        let outcome = cache_sessions(
            &environment,
            Source::Claude,
            &[sessions.transcripts[0].as_path()],
            false,
            &operation,
            AgentMeter::UNMETERED,
        )
        .unwrap()
        .pop()
        .expect("one read");
        operation.finish().unwrap();

        let (read, next) = outcome.into_parts();
        let SessionRead::Listed(row) = read else {
            panic!("the cached copy is read");
        };
        assert_eq!(row.message_count, 1, "the row is the cached copy");
        assert_eq!(
            sessions.watch.parses_after_mark(),
            0,
            "a replaced owner doesn't parse"
        );
        assert_eq!(
            next,
            NextStep::AwaitRefresh(AwaitedRefresh::AnotherRearview(OtherRearview::Refreshing)),
            "the copy waits for the replacement refresh"
        );
    }

    /// A read whose ownership another rearview took over and released
    /// before the read ended requests a session refresh: no refresh is left
    /// to bring the copy up to date.
    #[test]
    fn read_whose_takeover_owner_released_requests_session_refresh() {
        use crate::history::sqlite_cache::refresh::tests::expire_last_commit;
        let sessions = crate::tui::claude_home::ChangedSessionsInTwoRoots::new();
        let environment = sessions.environment();
        let operation = waiting_operation(&environment);
        assert!(matches!(
            operation.access().unwrap(),
            OperationAccess::Owner(_)
        ));
        expire_last_commit(sessions.cache.path());
        drop(RefreshOwnership::acquire_for_test(&rearview(sessions.cache.path())).unwrap());
        sessions.watch.mark();

        let outcome = cache_sessions(
            &environment,
            Source::Claude,
            &[sessions.transcripts[0].as_path()],
            false,
            &operation,
            AgentMeter::UNMETERED,
        )
        .unwrap()
        .pop()
        .expect("one read");
        operation.finish().unwrap();

        let (read, next) = outcome.into_parts();
        let SessionRead::Listed(row) = read else {
            panic!("the cached copy is read");
        };
        assert_eq!(row.message_count, 1, "the row is the cached copy");
        assert_eq!(
            sessions.watch.parses_after_mark(),
            0,
            "a replaced owner doesn't parse"
        );
        assert_eq!(
            next,
            NextStep::CacheInBackground,
            "an out-of-date copy without a refresh to wait for requests one"
        );
    }

    /// Without a replacement, a read whose writes fail waits for this
    /// rearview's own refresh, never another rearview's.
    #[test]
    fn read_revoked_without_replacement_waits_for_this_rearviews_refresh() {
        let base = tempfile::tempdir().unwrap();
        admit_database(base.path());
        let home = ClaudeHome::new();
        let transcript = home.write("target", 0, &["a question"]);
        load_sessions_with_cache(home.storage(), base.path(), false, None).unwrap();
        fail_every_commit_after_acquisition(base.path());
        home.append(&transcript, 1, "a later question");
        let changed = discovered_stub(&home, &transcript);
        let environment = environment_of(base.path());
        let loader = SessionLoader::new(
            &environment,
            home.storage(),
            false,
            None,
            AgentMeter::UNMETERED,
        );

        let (_, next) = loader.cache_one(&home.root(), changed).into_parts();

        assert_eq!(
            next,
            NextStep::AwaitRefresh(AwaitedRefresh::ThisProcess),
            "this rearview's refresh is reported as its own"
        );
    }

    /// The observations of one [`load_changed_sessions`].
    struct ChangedSessionsLoad {
        rows: Vec<Conversation>,
        /// Each session's fingerprint after the change, by transcript.
        fresh: HashMap<PathBuf, Option<CachedFingerprint>>,
        parses: usize,
        /// The refresh owner's commits before the first parse began.
        commits_before_first_parse: i64,
        /// The refresh owner's commits by the end of the load.
        commits: i64,
        /// Database rows at the transcript size from before the change.
        unchanged_sessions: i64,
        sessions: i64,
    }

    impl ChangedSessionsLoad {
        /// The rows read at their fingerprint after the change.
        fn parsed_rows(&self) -> usize {
            self.rows
                .iter()
                .filter(|row| self.fresh.get(&row.path) == Some(&row.fingerprint))
                .count()
        }
    }

    /// Cache `READ_BATCH_LENGTH + 1` sessions, change every one of them, and
    /// load them again: through targeted reads or a full load. With
    /// `fail_from`, every refresh owner commit after that many fails.
    fn load_changed_sessions(targeted: bool, fail_from: Option<i64>) -> ChangedSessionsLoad {
        let base = tempfile::tempdir().unwrap();
        let home = ClaudeHome::new();
        // One content for every session, so every transcript has one size
        // before the change and one after it.
        let transcripts: Vec<_> = (0..READ_BATCH_LENGTH + 1)
            .map(|index| home.write(&format!("session-{index}"), 0, &["a question"]))
            .collect();
        let old_size = std::fs::metadata(&transcripts[0]).unwrap().len();
        let database = SqliteCache::open(&rearview(base.path())).unwrap();
        database.mark_admitted().unwrap();
        database.admit_storage(home.storage().cache()).unwrap();
        drop(database);
        load_sessions_with_cache(home.storage(), base.path(), false, None).unwrap();
        let old: Vec<_> = transcripts
            .iter()
            .map(|transcript| discovered_stub(&home, transcript))
            .collect();
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
        for transcript in &transcripts {
            home.append(transcript, 1, "a later question");
        }
        let fresh = transcripts
            .iter()
            .map(|transcript| {
                let stub = discovered_stub(&home, transcript);
                (stub.locator, stub.fingerprint.stamp())
            })
            .collect();
        let commits_before_first_parse = std::sync::Arc::new(Mutex::new(None));
        let first_parse = commits_before_first_parse.clone();
        home.watch().mark();
        home.watch().run_as_each_parse_begins(move || {
            let connection = rusqlite::Connection::open(&database_path).unwrap();
            connection.busy_timeout(Duration::ZERO).unwrap();
            first_parse
                .lock()
                .unwrap()
                .get_or_insert_with(|| commit_count(&connection));
        });
        let environment = environment_of(base.path());
        let loader = SessionLoader::new(
            &environment,
            home.storage(),
            false,
            None,
            AgentMeter::UNMETERED,
        );
        let mut rows = if targeted {
            let operation = waiting_operation(&environment);
            let reads = loader.read_stubs(
                &home.root(),
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
                .filter_map(|outcome| match outcome.into_parts() {
                    (SessionRead::Listed(row), _) => Some(*row),
                    _ => None,
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
            parses: home.watch().parses_after_mark(),
            commits_before_first_parse: commits_before_first_parse.lock().unwrap().unwrap(),
            commits: commit_count(&connection),
            unchanged_sessions: count(&format!(
                "SELECT COUNT(*) FROM sessions WHERE file_size = {old_size}"
            )),
            sessions: count("SELECT COUNT(*) FROM sessions"),
        }
    }

    /// A parse batch whose commit fails stops the read: the sessions it
    /// parsed are read outside the cache, and nothing more is parsed or
    /// written.
    #[test]
    fn parse_batch_whose_commit_fails_stops_read() {
        for targeted in [false, true] {
            let setup_commits = load_changed_sessions(targeted, None).commits_before_first_parse;

            let load = load_changed_sessions(targeted, Some(setup_commits));

            assert_eq!(
                load.commits_before_first_parse, setup_commits,
                "the commit failure starts at the first parse batch"
            );
            assert_eq!(
                load.parsed_rows(),
                READ_BATCH_LENGTH,
                "the failed batch's reads are kept"
            );
            assert_eq!(
                load.parses, READ_BATCH_LENGTH,
                "parsing stops after the failed batch"
            );
            assert_eq!(
                load.commits, setup_commits,
                "batches stop committing after the failure"
            );
            assert_eq!(load.sessions, (READ_BATCH_LENGTH + 1) as i64);
            assert_eq!(load.unchanged_sessions, load.sessions);
        }
    }

    /// The refresh owner's commits to the cache in `base`: commits that
    /// stamp `refresh_state`. From the one numbered `failing_from`, every
    /// one fails and the data changes made after the first failure count;
    /// `watch` is marked at that first failure.
    struct OwnerCommits {
        _hooks: sqlite_cache::connection::test_hooks::Installed,
        commits: std::sync::Arc<AtomicUsize>,
        writes_after_the_failure: std::sync::Arc<AtomicUsize>,
    }

    impl OwnerCommits {
        fn counted(
            base: &Path,
            failing_from: Option<usize>,
            watch: &crate::history::provider::ParseWatch,
        ) -> Self {
            use std::sync::Arc;
            use std::sync::atomic::AtomicBool;
            let commits = Arc::new(AtomicUsize::new(0));
            let writes_after_the_failure = Arc::new(AtomicUsize::new(0));
            let has_failed = Arc::new(AtomicBool::new(false));
            let (counted, after, watch) = (
                Arc::clone(&commits),
                Arc::clone(&writes_after_the_failure),
                watch.clone(),
            );
            let hooks = sqlite_cache::connection::test_hooks::on_every_connection(
                base,
                move |connection| {
                    if connection.is_readonly(rusqlite::MAIN_DB).unwrap() {
                        return;
                    }
                    let stamps_ownership = Arc::new(AtomicBool::new(false));
                    let (stamping, failed, after) = (
                        Arc::clone(&stamps_ownership),
                        Arc::clone(&has_failed),
                        Arc::clone(&after),
                    );
                    connection
                        .update_hook(Some(
                            move |_: rusqlite::hooks::Action, _: &str, table: &str, _: i64| {
                                if table == "refresh_state" {
                                    stamping.store(true, Ordering::SeqCst);
                                } else if failed.load(Ordering::SeqCst) {
                                    after.fetch_add(1, Ordering::SeqCst);
                                }
                            },
                        ))
                        .unwrap();
                    let rolled_back = Arc::clone(&stamps_ownership);
                    connection
                        .rollback_hook(Some(move || rolled_back.store(false, Ordering::SeqCst)))
                        .unwrap();
                    let (counted, has_failed, watch) =
                        (Arc::clone(&counted), Arc::clone(&has_failed), watch.clone());
                    connection
                        .commit_hook(Some(move || {
                            if !stamps_ownership.swap(false, Ordering::SeqCst) {
                                return false;
                            }
                            let number = counted.fetch_add(1, Ordering::SeqCst);
                            let fails = failing_from.is_some_and(|first| number >= first);
                            if fails && !has_failed.swap(true, Ordering::SeqCst) {
                                watch.mark();
                            }
                            fails
                        }))
                        .unwrap();
                },
            );
            Self {
                _hooks: hooks,
                commits,
                writes_after_the_failure,
            }
        }
    }

    /// Run a full load, or a targeted read of every session, of `sessions`,
    /// with every refresh owner commit from the one numbered `failing_from`
    /// failing. Returns the sessions listed and the owner's commits.
    fn read_changed_sessions_in_two_roots(
        sessions: &crate::tui::claude_home::ChangedSessionsInTwoRoots,
        targeted: bool,
        failing_from: Option<usize>,
    ) -> (usize, OwnerCommits) {
        let commits = OwnerCommits::counted(sessions.cache.path(), failing_from, &sessions.watch);
        let environment = sessions.environment();
        let operation = waiting_operation(&environment);
        let listed = if targeted {
            let paths: Vec<&Path> = sessions.transcripts.iter().map(PathBuf::as_path).collect();
            cache_sessions(
                &environment,
                Source::Claude,
                &paths,
                false,
                &operation,
                AgentMeter::UNMETERED,
            )
            .unwrap()
            .into_iter()
            .filter(SessionOutcome::is_listed)
            .count()
        } else {
            sessions.load(&environment, &operation).len()
        };
        let _ = operation.finish();
        (listed, commits)
    }

    /// A refresh owner's commit that fails, at each of a full load's or a
    /// targeted read's commits after the acquisition, stops the read: no
    /// parse begins and nothing is written after it, and every session is
    /// listed, parsed before the failure or from its copy.
    #[test]
    fn every_failed_owner_commit_stops_read_and_keeps_every_session_listed() {
        let parallelism = std::thread::available_parallelism().map_or(1, usize::from);
        let template = crate::tui::claude_home::ChangedSessionsTemplate::new();
        for targeted in [false, true] {
            let sessions = template.case();
            let (_, clean) = read_changed_sessions_in_two_roots(&sessions, targeted, None);
            let session_count = sessions.transcripts.len();
            // Commit 0 records the acquisition: failing it fails the read.
            let failing: Vec<usize> = (1..clean.commits.load(Ordering::SeqCst)).collect();
            for cases in failing.chunks(parallelism) {
                std::thread::scope(|scope| {
                    for &failing_from in cases {
                        let template = &template;
                        scope.spawn(move || {
                            let case =
                                format!("targeted {targeted}, failing from commit {failing_from}");
                            let sessions = template.case();

                            let (listed, commits) = read_changed_sessions_in_two_roots(
                                &sessions,
                                targeted,
                                Some(failing_from),
                            );

                            assert_eq!(
                                sessions.watch.parses_after_mark(),
                                0,
                                "{case}: parsing stops at the failed commit"
                            );
                            assert_eq!(
                                commits.writes_after_the_failure.load(Ordering::SeqCst),
                                0,
                                "{case}: the read doesn't write the cache after the failed commit"
                            );
                            assert_eq!(listed, session_count, "{case}: every session is listed");
                        });
                    }
                });
            }
        }
    }

    #[test]
    fn refresh_reapplies_selection_to_fresh_discovery() {
        let base = tempfile::tempdir().unwrap();
        let home = ClaudeHome::new();
        let shrunk = home.write("shrunk", 0, &FOUR_MESSAGES);
        let four_messages_size = std::fs::metadata(&shrunk).unwrap().len();
        let added = home.transcript("added");
        let (shrinking, adding) = (shrunk.clone(), added.clone());
        home.run_before_listing(&home.project(), 2, move || {
            std::fs::copy(&shrinking, &adding).unwrap();
            let lines = std::fs::read_to_string(&shrinking).unwrap();
            std::fs::write(&shrinking, lines.lines().last().unwrap().to_owned() + "\n").unwrap();
        });
        let environment = environment_listing_home(base.path(), &home);
        let operation = waiting_operation(&environment);

        let result = rediscover_sessions(
            &environment,
            home.storage(),
            false,
            None,
            &|stub| stub.fingerprint.size >= four_messages_size,
            &operation,
        )
        .unwrap();
        operation.finish().unwrap();

        let fresh: Vec<_> = home
            .storage()
            .discover(&home.root(), &SystemLister)
            .unwrap()
            .stubs
            .into_iter()
            .map(|stub| (stub.locator, stub.fingerprint.stamp()))
            .collect();
        let mut fresh_sessions: Vec<_> = fresh.iter().map(|(locator, _)| locator.clone()).collect();
        fresh_sessions.sort();
        let mut both = vec![shrunk, added.clone()];
        both.sort();
        assert_eq!(fresh_sessions, both);
        assert_eq!(result.found, fresh);
        assert_eq!(result.outcomes.len(), 1);
        assert_eq!(result.outcomes[0].locator(), added);
        assert_eq!(home.watch().parsed_sessions(), [added]);
    }

    #[test]
    fn refresh_reports_fresh_ignored_counts_and_incomplete_directories() {
        let base = admitted_cache_base();
        let home = CodexHome::new();
        let rollout = home.write_rollout();
        let blocked = home.change_between_first_and_second_discovery();
        let environment = home.environment(base.path());
        let operation = waiting_operation(&environment);
        let result = rediscover_sessions(
            &environment,
            home.storage,
            false,
            None,
            &|_| true,
            &operation,
        )
        .unwrap();
        operation.finish().unwrap();
        assert_eq!(
            result.ignored,
            vec![FilterTerm::new(
                "Codex",
                "7 ignored: compressed sessions unsupported"
            )]
        );
        assert_eq!(result.unreadable_directories, vec![blocked]);
        assert_eq!(
            stored_keys_under(base.path(), home.storage, &home.root().path),
            [codex_cache_key(&home, &rollout)],
            "the incomplete walk's session is cached"
        );
    }

    /// OpenCode's locators name rows of its database, not files.
    #[test]
    fn sessions_that_are_not_files_load_through_shared_loop() {
        use super::super::contract_tests::ProviderFixture as _;
        let cache_base = admitted_cache_base();
        let home = tempfile::tempdir().unwrap();
        let fixture = super::super::opencode::tests::OpenCodeFixture;
        let mut locators = vec![
            fixture.write_session(home.path(), fixture.ids().session),
            fixture.write_session(home.path(), fixture.ids().other),
        ];
        locators.sort();
        assert!(
            locators.iter().all(|locator| !locator.exists()),
            "the locators name database rows, not files"
        );
        let watch = crate::history::provider::ParseWatch::default();
        let storage = crate::history::provider::WatchedStorage {
            inner: Source::OpenCode.provider().storage(),
            roots: vec![fixture.root_under(home.path())],
            watch: watch.clone(),
        };

        let listed = load_sessions_with_cache(&storage, cache_base.path(), false, None).unwrap();

        assert_eq!(watch.parses_begun(), 2);
        let mut paths: Vec<PathBuf> = listed
            .iter()
            .map(|conversation| conversation.path.clone())
            .collect();
        paths.sort();
        assert_eq!(
            paths, locators,
            "locators reach the conversations untouched"
        );
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
        let storage = super::super::contract_tests::kept_storage_under(fixture, home);
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
    fn changed_session_whose_cached_row_is_left_out_keeps_that_row_unread() {
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
    fn load_that_leaves_out_every_changed_session_doesnt_write_database() {
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
    fn session_database_doesnt_hold_is_read_again() {
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
    fn failed_entry_projection_is_retried_with_unchanged_discovery_fingerprint() {
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
    fn session_deleted_from_disk_leaves_database() {
        let cache_base = tempfile::tempdir().unwrap();
        let home = ClaudeHome::new();
        let kept = home.write("kept", 1, &["a question"]);
        let deleted = home.write("deleted", 0, &["a question"]);
        let kept_key = discovered_stub(&home, &kept).cache_key;
        admit_database(cache_base.path());
        load_sessions_with_cache(home.storage(), cache_base.path(), false, None).unwrap();
        std::fs::remove_file(&deleted).unwrap();

        load_sessions_with_cache(home.storage(), cache_base.path(), false, None).unwrap();

        assert_eq!(stored_keys_in(cache_base.path(), &home), [kept_key]);
    }

    /// The refresh owner's commits while a load reads a root of `count`
    /// sessions the database holds, one of them changed.
    fn commits_reading_a_root_with_one_change(count: usize) -> i64 {
        let base = tempfile::tempdir().unwrap();
        let home = ClaudeHome::new();
        let transcripts: Vec<_> = (0..count)
            .map(|index| home.write(&format!("session-{index}"), 0, &["a question"]))
            .collect();
        admit_database(base.path());
        load_sessions_with_cache(home.storage(), base.path(), false, None).unwrap();
        let connection =
            rusqlite::Connection::open(base.path().join(sqlite_cache::DATABASE_FILE_NAME)).unwrap();
        connection.execute_batch("CREATE TABLE refresh_commits (count INTEGER); INSERT INTO refresh_commits VALUES (0);
            CREATE TRIGGER count_commits AFTER UPDATE OF last_commit_ms ON refresh_state WHEN NEW.owner = OLD.owner BEGIN UPDATE refresh_commits SET count = count + 1; END;").unwrap();
        home.append(&transcripts[0], 1, "a later question");
        home.watch().mark();

        load_sessions_with_cache(home.storage(), base.path(), false, None).unwrap();

        assert_eq!(
            home.watch().sessions_parsed_after_mark(),
            [transcripts[0].clone()]
        );
        connection
            .query_row("SELECT count FROM refresh_commits", [], |row| row.get(0))
            .unwrap()
    }

    #[test]
    fn restoring_unchanged_sessions_doesnt_take_writer_lock() {
        assert_eq!(
            commits_reading_a_root_with_one_change(12 * READ_BATCH_LENGTH),
            commits_reading_a_root_with_one_change(READ_BATCH_LENGTH),
            "unchanged sessions are restored outside writer-locked batches"
        );
    }

    /// The project directory the unreadable-directory tests write a session
    /// to, then refuse to list.
    const BLOCKED_PROJECT: &str = "-tmp-blocked";

    #[test]
    fn root_with_unreadable_directory_caches_its_new_and_changed_sessions() {
        let base = admitted_cache_base();
        let home = ClaudeHome::new();
        let hidden = discovered_stub(
            &home,
            &home.write_in(BLOCKED_PROJECT, "hidden", 0, &["a question"]),
        );
        let changed = home.write("changed", 1, &["a question"]);
        load_sessions_with_cache(home.storage(), base.path(), false, None).unwrap();
        home.append(&changed, 2, "a later question");
        let changed = discovered_stub(&home, &changed);
        let added = discovered_stub(&home, &home.write("added", 3, &["a question"]));
        home.refuse(hidden.locator.parent().unwrap());

        let environment = environment_listing_home(base.path(), &home);
        SessionLoader::new(
            &environment,
            home.storage(),
            false,
            None,
            AgentMeter::UNMETERED,
        )
        .load_waiting()
        .unwrap();

        let current = cached_fingerprints_in(base.path(), &home);
        assert_eq!(
            current.get(&changed.cache_key).copied(),
            changed.fingerprint.stamp()
        );
        assert_eq!(
            current.get(&added.cache_key).copied(),
            added.fingerprint.stamp()
        );
        assert_eq!(
            current.get(&hidden.cache_key).copied(),
            hidden.fingerprint.stamp(),
            "the session under the unreadable directory keeps its row"
        );
    }

    #[test]
    fn targeted_read_whose_repeated_discovery_misses_directory_caches_session() {
        let base = admitted_cache_base();
        let home = ClaudeHome::new();
        let added = discovered_stub(&home, &home.write("added", 0, &["a question"]));
        let blocked = home.write_in(BLOCKED_PROJECT, "hidden", 1, &["a question"]);
        home.refuse(blocked.parent().unwrap());
        let environment = environment_listing_home(base.path(), &home);
        let loader = SessionLoader::new(
            &environment,
            home.storage(),
            false,
            None,
            AgentMeter::UNMETERED,
        );

        let (read, _) = loader.cache_one(&home.root(), added.clone()).into_parts();

        assert!(matches!(read, SessionRead::Listed(_)));
        assert_eq!(stored_keys_in(base.path(), &home), [added.cache_key]);
    }

    /// Complete disk admission of `base`'s database, as a first build does.
    fn admit_database(base: &Path) {
        SqliteCache::open(&rearview(base))
            .unwrap()
            .mark_admitted()
            .unwrap();
    }

    /// The stub discovery lists for the session at `transcript` in `home`.
    fn discovered_stub(home: &ClaudeHome, transcript: &Path) -> SessionStub {
        home.storage()
            .discover(&home.root(), &SystemLister)
            .unwrap()
            .stubs
            .into_iter()
            .find(|stub| stub.locator == transcript)
            .unwrap_or_else(|| panic!("discovery lists {}", transcript.display()))
    }

    /// The cache keys `base`'s database stores for `home`'s root, sorted.
    fn stored_keys_in(base: &Path, home: &ClaudeHome) -> Vec<String> {
        stored_keys_under(base, home.storage(), &home.root().path)
    }

    /// The cache keys `base`'s database stores for `storage`'s root at
    /// `root`, sorted.
    fn stored_keys_under(base: &Path, storage: &dyn SessionStorage, root: &Path) -> Vec<String> {
        let mut keys = SqliteCache::open_waiting(&rearview(base))
            .unwrap()
            .unwrap()
            .stored_keys(storage.cache(), root)
            .unwrap();
        keys.sort();
        keys
    }

    #[test]
    fn empty_root_removes_its_last_unfinished_session() {
        let base = tempfile::tempdir().unwrap();
        let home = ClaudeHome::new();
        let transcript = home.write("deleted", 0, &["a question"]);
        admit_database(base.path());
        load_sessions_with_cache(home.storage(), base.path(), false, None).unwrap();
        // An unfinished refresh is absent from current_fingerprints.
        let connection =
            rusqlite::Connection::open(base.path().join(sqlite_cache::DATABASE_FILE_NAME)).unwrap();
        connection
            .execute("UPDATE sessions SET status='writing'", [])
            .unwrap();
        std::fs::remove_file(&transcript).unwrap();

        load_sessions_with_cache(home.storage(), base.path(), false, None).unwrap();

        assert!(stored_keys_in(base.path(), &home).is_empty());
    }

    #[test]
    fn empty_root_doesnt_create_database() {
        let base = tempfile::tempdir().unwrap();
        let home = ClaudeHome::new();
        std::fs::create_dir_all(home.project()).unwrap();
        load_sessions_with_cache(home.storage(), base.path(), false, None).unwrap();
        assert!(!base.path().join(sqlite_cache::DATABASE_FILE_NAME).exists());
    }

    #[test]
    fn partial_discovery_doesnt_delete_unseen_database_sessions() {
        let base = admitted_cache_base();
        let home = ClaudeHome::new();
        let hidden = discovered_stub(&home, &home.write("hidden", 0, &["a question"]));
        load_sessions_with_cache(home.storage(), base.path(), false, None).unwrap();
        home.refuse_project();
        let environment = environment_listing_home(base.path(), &home);

        let loaded = SessionLoader::new(
            &environment,
            home.storage(),
            false,
            None,
            AgentMeter::UNMETERED,
        )
        .load_waiting()
        .unwrap();

        assert_eq!(
            loaded.unreadable_directories.len(),
            1,
            "discovery reports the directory it could not list"
        );
        assert_eq!(stored_keys_in(base.path(), &home), [hidden.cache_key]);
    }

    /// A load writes a session's entries as `session_entries` reads them.
    #[test]
    fn load_writes_each_sessions_entries_to_database() {
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
        let expected = crate::history::normalized_session(
            crate::history::FOR_FIXTURES,
            row.source,
            &row.path,
            &row.subagents,
        )
        .unwrap();
        assert_eq!(
            format!("{:?}", written.entries),
            format!("{:?}", expected.entries)
        );
    }

    /// A new session has no cached row to leave out, so it is read.
    #[test]
    fn new_session_is_read_even_when_its_row_is_left_out() {
        let cache_base = admitted_cache_base();
        let home = ClaudeHome::new();
        let current = home.write("current", 0, &["a question"]);
        let is_current = |row: &Conversation| row.path == current;

        let loaded = load_metered_with_cache(
            home.storage(),
            cache_base.path(),
            AgentMeter::UNMETERED,
            LeftOutRows::of(&is_current),
        )
        .unwrap();

        assert_eq!(
            home.watch().parsed_sessions(),
            std::slice::from_ref(&current)
        );
        assert_eq!(loaded.conversations.len(), 1);
    }

    /// The totals one metered load of `storage` through `cache_base` records,
    /// and the step its progress ends on.
    fn metered_load(storage: &dyn SessionStorage, cache_base: &Path) -> (AgentLoad, StepProgress) {
        let meter = LoadMeter::default();
        load_metered_with_cache(
            storage,
            cache_base,
            meter.agent(storage.source()),
            LeftOutRows::NONE,
        )
        .unwrap();
        let [agent] = meter.agents()[..] else {
            panic!("one record per agent: {:?}", meter.agents());
        };
        (agent, meter.progress())
    }

    #[test]
    fn metered_load_records_what_discovery_cache_and_refresh_read() {
        let home = ClaudeHome::new();
        let parent = home.write("parent", 1, &["a question"]);
        let child = home.write_subagent(&parent, "child", 1, &["a sub-agent question"]);
        let other = home.write("other", 0, &["a question"]);
        let bytes: u64 = [&parent, &child, &other]
            .iter()
            .map(|transcript| std::fs::metadata(transcript).unwrap().len())
            .sum();
        let cache_base = admitted_cache_base();

        let (cold, cold_progress) = metered_load(home.storage(), cache_base.path());
        let (warm, _) = metered_load(home.storage(), cache_base.path());

        assert_eq!((cold.discovery.roots, cold.discovery.sessions), (1, 2));
        assert_eq!(
            (cold.refresh.transcripts, cold.refresh.bytes),
            (3, bytes),
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
    fn load_reports_empty_and_unreadable_sessions() {
        let cache_base = tempfile::tempdir().unwrap();
        let home = ClaudeHome::new();
        let listed = home.write("listed", 2, &["a question"]);
        let empty = home.write("empty", 1, &[WITHOUT_VISIBLE_MESSAGES]);
        let unreadable = home.write("unreadable", 0, &["a question"]);
        make_unreadable(&unreadable);
        let environment = environment_of(cache_base.path());
        let loader = SessionLoader::new(
            &environment,
            home.storage(),
            false,
            None,
            AgentMeter::UNMETERED,
        );

        let loaded = loader.load_waiting().unwrap();

        assert_eq!(
            loaded.skipped,
            SkippedSessions {
                unlisted: found_session(&home, &empty),
                unreadable: found_session(&home, &unreadable),
            }
        );
        assert_eq!(
            loaded.conversations[0].fingerprint,
            discovered_stub(&home, &listed).fingerprint.stamp(),
            "a row carries the fingerprint it was read with"
        );
    }

    #[cfg(windows)]
    #[test]
    fn load_doesnt_report_session_failing_transiently() {
        let cache_base = tempfile::tempdir().unwrap();
        let home = ClaudeHome::new();
        let unreadable = home.write("unreadable", 1, &["a question"]);
        make_unreadable(&unreadable);
        let locked = home.write("locked", 0, &["a question"]);
        let _lock = lock(&locked);
        let environment = environment_of(cache_base.path());
        let loader = SessionLoader::new(
            &environment,
            home.storage(),
            false,
            None,
            AgentMeter::UNMETERED,
        );

        let loaded = loader.load_waiting().unwrap();

        assert_eq!(
            home.watch().parsed_sessions(),
            [locked, unreadable.clone()],
            "the load reads both sessions"
        );
        assert_eq!(
            loaded.skipped,
            SkippedSessions {
                unlisted: HashMap::new(),
                unreadable: found_session(&home, &unreadable),
            }
        );
    }

    /// The session at `transcript`, as a load reports it skipped.
    fn found_session(home: &ClaudeHome, transcript: &Path) -> HashMap<PathBuf, FoundSession> {
        let stub = discovered_stub(home, transcript);
        let found = FoundSession {
            source: Source::Claude,
            fingerprint: stub.fingerprint.stamp(),
            has_transient_read_error: false,
        };
        HashMap::from([(stub.locator, found)])
    }

    /// Overwrite the transcript at `transcript` with bytes that are not
    /// UTF-8: every read of it fails, and the failure is permanent.
    fn make_unreadable(transcript: &Path) {
        std::fs::write(transcript, [0xff, 0xfe, b'\n']).unwrap();
    }

    /// Open the transcript at `transcript` without sharing it, as another
    /// program can on Windows: every read of it fails transiently until the
    /// returned file is dropped.
    #[cfg(windows)]
    fn lock(transcript: &Path) -> std::fs::File {
        use std::os::windows::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(transcript)
            .unwrap()
    }

    #[test]
    fn rediscovery_reads_only_sessions_it_picks() {
        let cache_base = admitted_cache_base();
        let home = ClaudeHome::new();
        let unchanged = home.write("unchanged", 0, &["a question"]);
        let changed = home.write("changed", 1, &["a question"]);
        load_sessions_with_cache(home.storage(), cache_base.path(), false, None).unwrap();
        home.append(&changed, 2, "a later question");
        let added = home.write("added", 3, &["a question"]);
        home.watch().mark();

        let rediscovered =
            rediscover_sessions_with_cache(home.storage(), cache_base.path(), false, &|stub| {
                stub.locator != unchanged
            })
            .unwrap();

        assert_eq!(rediscovered.found.len(), 3, "every session is reported");
        let mut read = rediscovered
            .outcomes
            .iter()
            .map(|outcome| {
                assert!(outcome.is_listed());
                outcome.locator().to_path_buf()
            })
            .collect::<Vec<_>>();
        read.sort();
        let mut expected = vec![changed, added];
        expected.sort();
        assert_eq!(read, expected);
        assert_eq!(home.watch().sessions_parsed_after_mark(), expected);
    }

    #[test]
    fn session_read_again_previews_its_last_messages_when_asked() {
        let cache_base = admitted_cache_base();
        let home = ClaudeHome::new();
        let transcript = home.write("reread", 0, &FOUR_MESSAGES);

        let preview_of = |show_last| match cache_session_with_cache(
            home.storage(),
            cache_base.path(),
            &transcript,
            show_last,
        )
        .into_parts()
        .0
        {
            SessionRead::Listed(read) => read.preview,
            _ => panic!("the session reads as a listed session"),
        };

        assert_eq!(preview_of(true), LAST_MESSAGES_PREVIEW);
        assert_eq!(preview_of(false), FIRST_MESSAGES_PREVIEW);
    }

    /// The messages of a session whose first and last previews differ.
    const FOUR_MESSAGES: [&str; 4] = ["opening", "second", "third", "closing"];

    /// The preview of [`FOUR_MESSAGES`] from its first messages.
    const FIRST_MESSAGES_PREVIEW: &str = "opening ... second ... third";

    /// The preview of [`FOUR_MESSAGES`] from its last messages, the newest
    /// first.
    const LAST_MESSAGES_PREVIEW: &str = "closing ... third ... second";

    #[test]
    fn sessions_read_again_together_share_discovery_before_and_after_acquiring_ownership() {
        let cache_base = tempfile::tempdir().unwrap();
        let home = ClaudeHome::new();
        let first = discovered_stub(&home, &home.write("first", 0, &["a question"]));
        let second = discovered_stub(&home, &home.write("second", 1, &["a question"]));

        let environment = environment_listing_home(cache_base.path(), &home);
        let reads = SessionLoader::new(
            &environment,
            home.storage(),
            false,
            None,
            AgentMeter::UNMETERED,
        )
        .reread_caching(&[&second.locator, &first.locator]);

        assert_eq!(
            home.times_listed(&home.root().path),
            2,
            "one discovery selects both requests; one refreshes both after acquiring ownership"
        );
        let paths: Vec<_> = reads
            .into_iter()
            .map(|outcome| match outcome.into_parts() {
                (SessionRead::Listed(read), NextStep::Settled) => read.path,
                (SessionRead::Listed(_), next) => panic!("every session read is settled: {next:?}"),
                _ => panic!("a session was not read as a listed session"),
            })
            .collect();
        assert_eq!(paths, [second.locator, first.locator], "in the order asked");
    }

    #[test]
    fn rediscovered_new_session_previews_its_last_messages_when_asked() {
        let cache_base = tempfile::tempdir().unwrap();
        let home = ClaudeHome::new();
        home.write("new", 0, &FOUR_MESSAGES);

        let rediscovered =
            rediscover_sessions_with_cache(home.storage(), cache_base.path(), true, &|_| true)
                .unwrap();

        let [outcome]: [SessionOutcome; 1] =
            rediscovered
                .outcomes
                .try_into()
                .unwrap_or_else(|outcomes: Vec<_>| {
                    panic!("the refresh reads one session, not {}", outcomes.len())
                });
        let (SessionRead::Listed(read), _) = outcome.into_parts() else {
            panic!("the new session was not read as a listed session");
        };
        assert_eq!(read.preview, LAST_MESSAGES_PREVIEW);
    }

    /// Another process, such as `agent search`, can cache a session before a
    /// refresh picks it.
    #[test]
    fn rediscovered_session_cache_already_holds_is_restored_not_parsed() {
        let cache_base = admitted_cache_base();
        let home = ClaudeHome::new();
        home.write("cached", 0, &[WITHOUT_VISIBLE_MESSAGES]);
        load_sessions_with_cache(home.storage(), cache_base.path(), false, None).unwrap();
        home.watch().mark();

        let rediscovered =
            rediscover_sessions_with_cache(home.storage(), cache_base.path(), false, &|_| true)
                .unwrap();

        assert!(matches!(
            rediscovered
                .outcomes
                .into_iter()
                .map(|outcome| outcome.into_parts().0)
                .collect::<Vec<_>>()
                .as_slice(),
            [SessionRead::Empty]
        ));
        assert_eq!(home.watch().parses_after_mark(), 0);
    }

    /// The text of a Claude user message that leaves its session without
    /// visible messages.
    const WITHOUT_VISIBLE_MESSAGES: &str = "";

    /// Without a record of it, a transcript that holds no conversation is read
    /// in full on every load and contributes nothing. Most of a Codex corpus is
    /// sub-agent threads whose own content is empty, so this is the bulk of a
    /// warm load.
    #[test]
    fn session_that_holds_no_conversation_is_read_once() {
        let cache_base = admitted_cache_base();
        let home = ClaudeHome::new();
        let empty = home.write("empty", 0, &[WITHOUT_VISIBLE_MESSAGES]);

        let cold =
            load_sessions_with_cache(home.storage(), cache_base.path(), false, None).unwrap();
        assert!(cold.is_empty(), "the empty session stays off the list");
        assert_eq!(home.watch().parsed_sessions(), std::slice::from_ref(&empty));

        let warm =
            load_sessions_with_cache(home.storage(), cache_base.path(), false, None).unwrap();
        assert!(warm.is_empty(), "the warm load leaves it off the list");
        assert_eq!(
            home.watch().parsed_sessions(),
            [empty],
            "the session is read once"
        );
    }

    /// The record stands for the transcript's content, so it lasts exactly as
    /// long as the fingerprint does.
    #[test]
    fn session_that_gains_content_is_read_again() {
        let cache_base = admitted_cache_base();
        let home = ClaudeHome::new();
        let grows = home.write("grows", 0, &[WITHOUT_VISIBLE_MESSAGES]);
        assert!(
            load_sessions_with_cache(home.storage(), cache_base.path(), false, None)
                .unwrap()
                .is_empty()
        );
        home.append(&grows, 1, "a question");
        home.watch().mark();

        let listed =
            load_sessions_with_cache(home.storage(), cache_base.path(), false, None).unwrap();

        assert_eq!(listed.len(), 1);
        assert_eq!(home.watch().sessions_parsed_after_mark(), [grows]);
    }

    /// A read can fail for a reason outside the transcript — a file held open,
    /// a partial write. Recording that as empty would hide the session until it
    /// changed on disk.
    #[cfg(windows)]
    #[test]
    fn session_that_could_not_be_read_is_not_recorded_as_empty() {
        let cache_base = admitted_cache_base();
        let home = ClaudeHome::new();
        let locked = home.write("locked", 0, &["a question"]);
        let lock = lock(&locked);
        assert!(
            load_sessions_with_cache(home.storage(), cache_base.path(), false, None)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            home.watch().parsed_sessions(),
            std::slice::from_ref(&locked)
        );
        drop(lock);
        home.watch().mark();

        let listed =
            load_sessions_with_cache(home.storage(), cache_base.path(), false, None).unwrap();

        assert_eq!(listed.len(), 1, "the same fingerprint is read again");
        assert_eq!(home.watch().sessions_parsed_after_mark(), [locked]);
    }

    #[test]
    fn rediscovery_reads_unreadable_session_as_permanent_failure() {
        let unreadable =
            |home: &ClaudeHome| make_unreadable(&home.write("unreadable", 0, &["a question"]));
        assert!(matches!(
            rediscovered_read(unreadable),
            SessionRead::Failed(ReadError::Permanent)
        ));
    }

    #[cfg(windows)]
    #[test]
    fn rediscovery_reads_locked_session_as_transient_failure() {
        let locked = |home: &ClaudeHome| lock(&home.write("locked", 0, &["a question"]));
        assert!(matches!(
            rediscovered_read(locked),
            SessionRead::Failed(ReadError::Transient)
        ));
    }

    /// The read of the one session `write_session` writes to a new home, as
    /// a rediscovery that reads every session reads it while the value
    /// `write_session` returns is held.
    fn rediscovered_read<Held>(write_session: impl FnOnce(&ClaudeHome) -> Held) -> SessionRead {
        let cache_base = tempfile::tempdir().unwrap();
        let home = ClaudeHome::new();
        let _held = write_session(&home);
        let environment = environment_of(cache_base.path());
        let loader = SessionLoader::new(
            &environment,
            home.storage(),
            false,
            None,
            AgentMeter::UNMETERED,
        );
        let operation = waiting_operation(&environment);
        let mut outcomes = loader.rediscover(&|_| true, &operation).unwrap().outcomes;
        operation.finish().unwrap();
        assert_eq!(outcomes.len(), 1, "the rediscovery reads one session");
        outcomes.pop().unwrap().into_parts().0
    }

    /// The tokens of each model reply [`write_parent_with_subagent`] writes.
    const REPLY_TOKENS: u64 = 10;

    /// Write session `parent` in `home` with a model reply, and one
    /// sub-agent with a model reply. Returns the transcripts of the session
    /// and of its sub-agent.
    fn write_parent_with_subagent(home: &ClaudeHome) -> (PathBuf, PathBuf) {
        let parent = home.write("parent", 0, &["a question"]);
        home.append_reply(&parent, 0, "an answer", REPLY_TOKENS);
        let subagent = home.write_subagent(&parent, "0", 0, &["a sub-agent question"]);
        home.append_reply(&subagent, 0, "a sub-agent answer", REPLY_TOKENS);
        (parent, subagent)
    }

    #[cfg(windows)]
    #[test]
    fn session_whose_subagent_hits_transient_error_is_listed_without_it_and_isnt_cached() {
        let cache_base = admitted_cache_base();
        let home = ClaudeHome::new();
        let (_, subagent) = write_parent_with_subagent(&home);
        let _lock = lock(&subagent);

        let listed =
            load_sessions_with_cache(home.storage(), cache_base.path(), false, None).unwrap();
        load_sessions_with_cache(home.storage(), cache_base.path(), false, None).unwrap();

        assert_eq!(
            listed[0].total_tokens, REPLY_TOKENS,
            "the sub-agent's tokens are missing"
        );
        assert!(listed[0].has_transient_read_error);
        assert!(stored_keys_in(cache_base.path(), &home).is_empty());
        assert_eq!(
            home.watch().parses_begun(),
            2,
            "the next load parses it again"
        );
    }

    #[cfg(windows)]
    #[test]
    fn session_is_cached_once_its_subagent_reads() {
        let cache_base = admitted_cache_base();
        let home = ClaudeHome::new();
        let (parent, subagent) = write_parent_with_subagent(&home);
        let lock = lock(&subagent);
        load_sessions_with_cache(home.storage(), cache_base.path(), false, None).unwrap();
        drop(lock);

        let listed =
            load_sessions_with_cache(home.storage(), cache_base.path(), false, None).unwrap();

        assert_eq!(
            listed[0].total_tokens,
            2 * REPLY_TOKENS,
            "the sub-agent's tokens are merged"
        );
        assert!(!listed[0].has_transient_read_error);
        assert_eq!(
            stored_keys_in(cache_base.path(), &home),
            [discovered_stub(&home, &parent).cache_key]
        );
    }

    #[test]
    fn session_whose_subagent_hits_permanent_error_is_cached_without_it() {
        let cache_base = admitted_cache_base();
        let home = ClaudeHome::new();
        let (parent, subagent) = write_parent_with_subagent(&home);
        make_unreadable(&subagent);

        let listed =
            load_sessions_with_cache(home.storage(), cache_base.path(), false, None).unwrap();

        assert_eq!(
            listed[0].total_tokens, REPLY_TOKENS,
            "the sub-agent's tokens are missing"
        );
        assert!(!listed[0].has_transient_read_error);
        assert_eq!(
            stored_keys_in(cache_base.path(), &home),
            [discovered_stub(&home, &parent).cache_key]
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

    /// Write session `name` in `home` with `subagents` sub-agent transcripts,
    /// every message sent `minute` past noon. Returns its transcript.
    fn write_with_subagents(
        home: &ClaudeHome,
        name: &str,
        minute: u32,
        subagents: usize,
    ) -> PathBuf {
        let transcript = home.write(name, minute, &["a question"]);
        for index in 0..subagents {
            home.write_subagent(
                &transcript,
                &index.to_string(),
                minute,
                &["a sub-agent question"],
            );
        }
        transcript
    }

    /// A session with sub-agents moves the count once per transcript read,
    /// not once when the whole session completes.
    #[test]
    fn progress_counts_each_transcript_session_reads() {
        let cache_base = tempfile::tempdir().unwrap();
        let home = ClaudeHome::new();
        write_with_subagents(&home, "parent", 1, 2);
        home.write("other", 0, &["a question"]);
        let environment = environment_of(cache_base.path());
        let loader = SessionLoader::new(
            &environment,
            home.storage(),
            false,
            None,
            AgentMeter::UNMETERED,
        );

        let reports = progress_of_load(&loader);

        assert_eq!(reports, vec![(0, 4), (1, 4), (2, 4), (3, 4), (4, 4)]);
    }

    /// A session the database restores reads none of its transcripts; the
    /// count still reaches the total.
    #[test]
    fn progress_reaches_total_when_sessions_are_not_read() {
        let cache_base = admitted_cache_base();
        let home = ClaudeHome::new();
        write_with_subagents(&home, "parent", 1, 2);
        home.write("other", 0, &["a question"]);
        let environment = environment_of(cache_base.path());
        let loader = SessionLoader::new(
            &environment,
            home.storage(),
            false,
            None,
            AgentMeter::UNMETERED,
        );
        progress_of_load(&loader);

        let reports = progress_of_load(&loader);

        assert_eq!(reports.first(), Some(&(0, 4)));
        assert_eq!(reports.last(), Some(&(4, 4)));
        assert_eq!(
            home.watch().parses_begun(),
            2,
            "the second load reads nothing"
        );
    }

    /// Sessions load in parallel and still list in the order a sequential
    /// load gives: newest first, then discovery order, each with its index.
    #[test]
    fn parallel_load_lists_sessions_in_discovery_order_within_timestamp() {
        let cache_base = tempfile::tempdir().unwrap();
        let home = ClaudeHome::new();
        for index in 0..64 {
            write_with_subagents(&home, &format!("session-{index:02}"), 0, 3);
        }
        let discovery_order: Vec<PathBuf> = home
            .storage()
            .discover(&home.root(), &SystemLister)
            .unwrap()
            .stubs
            .into_iter()
            .map(|stub| stub.locator)
            .collect();

        let listed =
            load_sessions_with_cache(home.storage(), cache_base.path(), false, None).unwrap();

        let listed_paths: Vec<PathBuf> = listed
            .iter()
            .map(|conversation| conversation.path.clone())
            .collect();
        assert_eq!(listed_paths, discovery_order);
        assert!(
            listed
                .iter()
                .enumerate()
                .all(|(position, conversation)| conversation.index == position)
        );
    }
}
