//! The load loop shared by every provider that stores sessions under roots.

use super::storage::{DiscoveredSessions, ResolvedSession, SessionStub};
use super::{SessionRoot, SessionStorage, SessionTitle};
use crate::cli::DebugLevel;
use crate::clock::Clock;
use crate::debug;
use crate::error::{AppError, Result, SessionDatabaseFailure};
use crate::history::cache::{
    CachedFingerprint, ListedSessionEntry, SessionCacheEntry, SessionCacheStore,
    cached_conversation, conversation_from_cached, shard_index,
};
use crate::history::format::same_file;
use crate::history::load_meter::AgentMeter;
use crate::history::sqlite_cache::refresh::{LockWait, RefreshOwnership, RefreshToken};
use crate::history::sqlite_cache::{
    self, CacheFollowUp, CacheWrite, CacheWriter, CatalogueSession, Fallback, InteractiveCache,
    InteractiveRead, SessionRecord, SessionStatus, SqliteCache,
};
use crate::history::{Conversation, FilterTerm, Source, format_short_name_from_path};
use rayon::prelude::*;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

mod cache_access;
mod targeted;

use cache_access::{
    CacheAccess, DatabaseReader, Deletions, OwnedRefresh, ReadOnly, ReadOnlyReason,
};
use targeted::{StubsAfterRediscovery, TargetedRead};

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
    pub read: Vec<(PathBuf, SessionRead)>,
    /// One term per reason a root's sessions were ignored for, named for the
    /// user.
    pub ignored: Vec<FilterTerm>,
    /// Titles stored beside the transcripts, by session id. A rename that
    /// writes only these leaves every fingerprint as it was.
    pub external_titles: HashMap<String, SessionTitle>,
    /// Missing paths beneath these directories cannot establish deletions.
    pub unreadable_directories: Vec<PathBuf>,
}

/// The outcome of reading one session.
pub enum SessionRead {
    /// The row the list shows for the session.
    Listed(Box<Conversation>),
    /// Read cleanly, and holds no conversation the provider lists.
    Empty,
    /// Could not be read.
    Failed(ReadError),
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

/// Every session `storage` holds, newest first.
///
/// Each root carries its own cache, so a session that has not changed since the
/// last run is rebuilt from cached metadata instead of reparsed, and one that
/// held no conversation is skipped without being read again. `progress` hears
/// `(done, total)` in transcripts, a session's own and each sub-agent's: once
/// every root is discovered, then as each transcript is read or its session
/// completes without reading it. Discovery repeated after waiting for refresh
/// ownership can replace the total before reading continues. `meter` records
/// what each step read. A changed session whose cached row `left_out` holds
/// keeps that row unread. A row without any time is dated `clock`'s current
/// time.
pub fn load_sessions(
    storage: &dyn SessionStorage,
    show_last: bool,
    debug_level: Option<DebugLevel>,
    meter: AgentMeter,
    left_out: LeftOutRows,
    progress: &mut dyn FnMut(usize, usize),
    clock: &dyn Clock,
) -> Result<LoadedSessions> {
    SessionLoader {
        storage,
        cache: &SessionCacheStore::in_user_cache(storage.cache()),
        show_last,
        debug_level,
        meter,
        clock,
    }
    .load(left_out, progress)
}

/// `storage`'s sessions as discovery finds them now, reading only those
/// `needs_reading` picks: through the cache, as a load reads them, rewriting
/// only the shards whose sessions missed it.
pub fn rediscover_sessions(
    storage: &dyn SessionStorage,
    show_last: bool,
    debug_level: Option<DebugLevel>,
    needs_reading: &dyn Fn(&SessionStub) -> bool,
    clock: &dyn Clock,
) -> Result<RediscoveredSessions> {
    SessionLoader {
        storage,
        cache: &SessionCacheStore::in_user_cache(storage.cache()),
        show_last,
        debug_level,
        meter: AgentMeter::UNMETERED,
        clock,
    }
    .rediscover(needs_reading)
}

/// The session `session_id` names, from whichever provider stores it, as
/// the row the list would have shown: the provider's cache-or-parse step,
/// sub-agent transcripts merged, with the preview `show_last` selects.
/// Read as the TUI's drawing thread reads: without acquiring refresh
/// ownership or writing any cache. A session read outside the database
/// carries its follow-up.
///
/// `None` when no provider stores the session, or it holds no conversation,
/// or cannot be read.
pub fn load_session_by_id(
    cache: &InteractiveCache,
    session_id: &str,
    show_last: bool,
    clock: &dyn Clock,
) -> InteractiveRead<Option<(Source, Conversation)>> {
    let Some((source, ResolvedSession { root, stub })) = super::resolve_session_id(session_id)
    else {
        return InteractiveRead::settled(None);
    };
    let storage = source.provider().storage();
    let read = SessionLoader {
        storage,
        cache: &SessionCacheStore::in_base(cache.base(), storage.cache()),
        show_last,
        debug_level: None,
        meter: AgentMeter::UNMETERED,
        clock,
    }
    .read_one(&root, stub);
    InteractiveRead {
        value: match read.value {
            SessionRead::Listed(conversation) => Some((source, *conversation)),
            SessionRead::Empty | SessionRead::Failed(_) => None,
        },
        follow_up: read.follow_up,
    }
}

/// The sessions at `locators`, one result each in order, as the list builds
/// their rows: the stub `source`'s discovery lists for each file, however
/// its path is spelled, read through the cache with its sub-agent
/// transcripts, with the preview `show_last` selects. A file discovery
/// leaves out, such as a sub-agent opened by its ID, is found through
/// [`SessionStorage::rediscover_unlisted_session`]. Another copy stored
/// under the same session id is not read. Discovery is shared across the
/// requests. Read as the TUI's drawing thread reads: without acquiring
/// refresh ownership or writing any cache. Each session read outside the
/// database carries its follow-up.
///
/// `None` when neither finds the file. A discovery that fails, or cannot
/// list the file's directory, reads as a [`ReadError::Transient`] failure:
/// the session may still be there, unchanged.
pub fn reread_sessions(
    cache: &InteractiveCache,
    source: Source,
    locators: &[&Path],
    show_last: bool,
    clock: &dyn Clock,
) -> Vec<InteractiveRead<Option<SessionRead>>> {
    reread_in(
        cache,
        source,
        locators,
        show_last,
        TargetedRead::Interactive,
        clock,
    )
}

/// [`reread_sessions`] on the TUI's background refresh thread: on a cache
/// miss, wait for refresh ownership as a load does, discover the files'
/// roots again under it, and cache each session the database does not
/// hold. A session read outside the database after the read lost write
/// permission waits for the refresh running then, if any.
pub fn cache_sessions(
    cache: &InteractiveCache,
    source: Source,
    locators: &[&Path],
    show_last: bool,
    clock: &dyn Clock,
) -> Vec<InteractiveRead<Option<SessionRead>>> {
    reread_in(
        cache,
        source,
        locators,
        show_last,
        TargetedRead::Caching,
        clock,
    )
}

/// The sessions at `locators` read again through `cache` with `source`'s
/// storage, as `targeted_read` selects.
fn reread_in(
    cache: &InteractiveCache,
    source: Source,
    locators: &[&Path],
    show_last: bool,
    targeted_read: TargetedRead,
    clock: &dyn Clock,
) -> Vec<InteractiveRead<Option<SessionRead>>> {
    let storage = source.provider().storage();
    SessionLoader {
        storage,
        cache: &SessionCacheStore::in_base(cache.base(), storage.cache()),
        show_last,
        debug_level: None,
        meter: AgentMeter::UNMETERED,
        clock,
    }
    .reread(locators, targeted_read)
}

/// [`reread_sessions`] for one session, against a caller-chosen storage and
/// cache, on test time.
#[cfg(test)]
pub(crate) fn reread_session_with_cache(
    storage: &dyn SessionStorage,
    cache: &SessionCacheStore,
    locator: &Path,
    show_last: bool,
) -> InteractiveRead<Option<SessionRead>> {
    read_session_with_cache(
        storage,
        cache,
        locator,
        show_last,
        TargetedRead::Interactive,
    )
}

/// [`cache_sessions`] for one session, against a caller-chosen storage and
/// cache.
#[cfg(test)]
pub(crate) fn cache_session_with_cache(
    storage: &dyn SessionStorage,
    cache: &SessionCacheStore,
    locator: &Path,
    show_last: bool,
) -> InteractiveRead<Option<SessionRead>> {
    read_session_with_cache(storage, cache, locator, show_last, TargetedRead::Caching)
}

#[cfg(test)]
fn read_session_with_cache(
    storage: &dyn SessionStorage,
    cache: &SessionCacheStore,
    locator: &Path,
    show_last: bool,
    targeted_read: TargetedRead,
) -> InteractiveRead<Option<SessionRead>> {
    SessionLoader {
        storage,
        cache,
        show_last,
        debug_level: None,
        meter: AgentMeter::UNMETERED,
        clock: &crate::clock::TestClock::default(),
    }
    .reread(&[locator], targeted_read)
    .pop()
    .expect("one result per locator")
}

/// The session `stub` names under `root`, read as [`load_session_by_id`]
/// reads a resolved session, against a caller-chosen storage and cache.
#[cfg(test)]
pub(crate) fn read_resolved_session_with_cache(
    storage: &dyn SessionStorage,
    cache: &SessionCacheStore,
    root: &SessionRoot,
    stub: SessionStub,
) -> InteractiveRead<SessionRead> {
    SessionLoader {
        storage,
        cache,
        show_last: false,
        debug_level: None,
        meter: AgentMeter::UNMETERED,
        clock: &crate::clock::TestClock::default(),
    }
    .read_one(root, stub)
}

/// [`load_sessions`] against a caller-chosen cache, on test time, reporting
/// nothing.
#[cfg(test)]
pub(crate) fn load_with_cache(
    storage: &dyn SessionStorage,
    cache: &SessionCacheStore,
    show_last: bool,
    debug_level: Option<DebugLevel>,
) -> Result<LoadedSessions> {
    load_leaving_out_with_cache(storage, cache, show_last, debug_level, LeftOutRows::NONE)
}

/// [`load_with_cache`], leaving the changed sessions whose cached row
/// `left_out` holds unread.
#[cfg(test)]
pub(crate) fn load_leaving_out_with_cache(
    storage: &dyn SessionStorage,
    cache: &SessionCacheStore,
    show_last: bool,
    debug_level: Option<DebugLevel>,
    left_out: LeftOutRows,
) -> Result<LoadedSessions> {
    SessionLoader {
        storage,
        cache,
        show_last,
        debug_level,
        meter: AgentMeter::UNMETERED,
        clock: &crate::clock::TestClock::default(),
    }
    .load(left_out, &mut |_, _| {})
}

/// [`load_with_cache`], recording what it read in `meter` and leaving the
/// changed sessions whose cached row `left_out` holds unread.
#[cfg(test)]
fn load_metered_with_cache(
    storage: &dyn SessionStorage,
    cache: &SessionCacheStore,
    meter: AgentMeter,
    left_out: LeftOutRows,
) -> Result<LoadedSessions> {
    SessionLoader {
        storage,
        cache,
        show_last: false,
        debug_level: None,
        meter,
        clock: &crate::clock::TestClock::default(),
    }
    .load(left_out, &mut |_, _| {})
}

/// [`load_with_cache`], keeping only the conversations.
#[cfg(test)]
pub(crate) fn load_sessions_with_cache(
    storage: &dyn SessionStorage,
    cache: &SessionCacheStore,
    show_last: bool,
    debug_level: Option<DebugLevel>,
) -> Result<Vec<Conversation>> {
    load_with_cache(storage, cache, show_last, debug_level).map(|loaded| loaded.conversations)
}

/// [`rediscover_sessions`] against a caller-chosen cache, on test time.
#[cfg(test)]
pub(crate) fn rediscover_sessions_with_cache(
    storage: &dyn SessionStorage,
    cache: &SessionCacheStore,
    show_last: bool,
    needs_reading: &dyn Fn(&SessionStub) -> bool,
) -> Result<RediscoveredSessions> {
    SessionLoader {
        storage,
        cache,
        show_last,
        debug_level: None,
        meter: AgentMeter::UNMETERED,
        clock: &crate::clock::TestClock::default(),
    }
    .rediscover(needs_reading)
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
    /// The shards only, writing nothing.
    const SHARDS_ONLY: Self = Self {
        access: CacheAccess::ShardsOnly,
        database: None,
        deletions: Deletions::Keep,
    };
}

/// One provider's sessions, loaded against one cache with one set of options.
struct SessionLoader<'a> {
    storage: &'a dyn SessionStorage,
    cache: &'a SessionCacheStore,
    show_last: bool,
    debug_level: Option<DebugLevel>,
    meter: AgentMeter<'a>,
    clock: &'a dyn Clock,
}

impl SessionLoader<'_> {
    /// Sessions are loaded on a worker thread, a root's sessions in parallel,
    /// while this thread reports each transcript read to `progress`: the
    /// callback stays on the caller's thread, and the count moves inside a
    /// session with many sub-agents instead of jumping when it completes.
    fn load(
        &self,
        left_out: LeftOutRows,
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
            let worker = scope.spawn(move || self.load_discovered(discovered, left_out, &read));
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
        });
        if let Some(base) = self.cache.base().filter(|base| base.is_dir()) {
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
        read: &mpsc::Sender<TranscriptProgress>,
    ) -> LoadedSessions {
        let mut conversations = Vec::new();
        let mut ignored = Vec::new();
        let mut skipped = SkippedSessions::default();
        let mut discovered_sessions = Vec::new();
        for (root, found) in discovered {
            let loaded_root = self.load_root(&root, found, left_out, read);
            ignored.extend(loaded_root.ignored);
            conversations.extend(loaded_root.conversations);
            skipped.extend(loaded_root.skipped);
            discovered_sessions.extend(loaded_root.discovered);
        }
        conversations.sort_by_key(|conversation| std::cmp::Reverse(conversation.timestamp));
        for (index, conversation) in conversations.iter_mut().enumerate() {
            conversation.index = index;
        }
        LoadedSessions {
            conversations,
            ignored,
            skipped,
            discovered: discovered_sessions,
        }
    }

    /// Every session discovery finds now, reading only those `needs_reading`
    /// picks.
    fn rediscover(
        &self,
        needs_reading: &dyn Fn(&SessionStub) -> bool,
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
            );
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
            rediscovered.read.extend(reads.read);
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
        let discovering = Instant::now();
        let mut discovered = Vec::new();
        for root in self.storage.roots()? {
            let found = self.storage.discover(&root)?;
            discovered.push((root, found));
        }
        self.meter.discovered(
            discovered.len(),
            discovered.iter().map(|(_, found)| found.stubs.len()).sum(),
            discovering.elapsed(),
        );
        Ok(discovered)
    }

    /// The conversations among `stubs`, the sessions discovered under `root`,
    /// restored or parsed in parallel. `read` hears every transcript of every
    /// stub exactly once, whatever became of it: a stub whose transcripts were
    /// not all read (restored from the cache, empty or unreadable) is topped
    /// up when it completes.
    ///
    /// Rewrite a shard only if one of its sessions was reread, recorded empty,
    /// or deleted. If the restored entry matches disk, skip the write.
    ///
    /// A changed session whose cached row `left_out` holds is restored from
    /// that entry, which stays in the cache unchanged.
    ///
    /// Beside the conversations, return the sessions that hold no
    /// conversation, the unreadable ones, every stub with what was recorded
    /// of it, and what the root's discovery ignored.
    fn load_root(
        &self,
        root: &SessionRoot,
        found: DiscoveredSessions,
        left_out: LeftOutRows,
        read: &mpsc::Sender<TranscriptProgress>,
    ) -> LoadedRoot {
        self.load_root_waiting(root, found, left_out, read, &LockWait::starting_now)
    }

    /// [`Self::load_root`], each wait for SQLite's writer lock starting as
    /// `new_lock_wait` makes it.
    fn load_root_waiting(
        &self,
        root: &SessionRoot,
        mut found: DiscoveredSessions,
        left_out: LeftOutRows,
        read: &mpsc::Sender<TranscriptProgress>,
        new_lock_wait: &dyn Fn() -> LockWait,
    ) -> LoadedRoot {
        let stubs = std::mem::take(&mut found.stubs);
        let mut loaded =
            self.load_root_sessions(root, stubs, &mut found, left_out, read, new_lock_wait);
        loaded.ignored = self.report_discovery(root, &found);
        loaded
    }

    /// [`Self::load_root`]'s sessions. `found` is the root's discovery
    /// without its stubs. A refresh replaces it with the discovery it repeats
    /// after acquiring refresh ownership, and reconciles deletions from that
    /// when it listed every directory.
    ///
    /// A database probe that stays busy past its timeout spends one
    /// [`LockWait`], and acquiring ownership continues it instead of starting
    /// another.
    fn load_root_sessions(
        &self,
        root: &SessionRoot,
        stubs: Vec<SessionStub>,
        found: &mut DiscoveredSessions,
        left_out: LeftOutRows,
        read: &mpsc::Sender<TranscriptProgress>,
        new_lock_wait: &dyn Fn() -> LockWait,
    ) -> LoadedRoot {
        if stubs.is_empty()
            && !self.cache.has_root_files(&root.path)
            && self
                .cache
                .base()
                .is_none_or(|base| !base.join(sqlite_cache::DATABASE_FILE_NAME).exists())
        {
            // Nothing to read, and no cache to remove sessions from.
            return self.load_root_writing(root, stubs, left_out, read, RootCaches::SHARDS_ONLY);
        }
        let deletions = Deletions::of(found);
        let probe_wait = new_lock_wait();
        let probed = self.open_database(root, &stubs, probe_wait.remaining());
        let is_database_busy = probed.as_ref().is_err_and(sqlite_cache::is_busy_error);
        match self.report_open_failure(probed) {
            Some(snapshot) if !self.needs_refresh(root, &stubs, left_out, &snapshot, deletions) => {
                let caches = RootCaches {
                    access: CacheAccess::ReadOnly(DatabaseReader::Waiting),
                    database: None,
                    deletions,
                };
                return self.load_database_root(root, stubs, left_out, read, snapshot, caches);
            }
            snapshot => self
                .meter
                .refreshing(transcripts_to_parse(&stubs, snapshot.as_ref())),
        }
        let lock_wait = if is_database_busy {
            probe_wait
        } else {
            new_lock_wait()
        };
        let OwnedRefresh {
            ownership,
            found: rediscovered,
        } = match self.own_refresh(root, lock_wait) {
            Ok(owned) => owned,
            Err(read_only) => {
                return self.load_root_read_only(root, stubs, found, read_only, left_out, read);
            }
        };
        let stubs = adopt_rediscovery(stubs, found, rediscovered, read);
        let owner = ownership.token();
        self.report_cache_update(
            owner.write_batch(|| self.cache.migrate_legacy(&root.path, self.clock)),
        );
        let snapshot = self.open_database_for_refresh(root, &stubs, owner);
        if !owner.can_publish() {
            drop(snapshot);
            drop(ownership);
            let read_only = ReadOnly::new(ReadOnlyReason::WritePermissionRevoked);
            return self.load_root_read_only(root, stubs, found, read_only, left_out, read);
        }
        let caches = RootCaches {
            access: CacheAccess::Refresh(owner),
            database: None,
            deletions: Deletions::of(found),
        };
        match snapshot {
            Some(snapshot) => {
                self.load_database_root(root, stubs, left_out, read, snapshot, caches)
            }
            None => self.load_root_writing(root, stubs, left_out, read, caches),
        }
    }

    /// [`Self::load_root`]'s sessions loaded without writing any cache, for
    /// `read_only`'s reason. Its discovery repeated after acquiring refresh
    /// ownership, when one completed, replaces `found` and `stubs`.
    fn load_root_read_only(
        &self,
        root: &SessionRoot,
        stubs: Vec<SessionStub>,
        found: &mut DiscoveredSessions,
        read_only: ReadOnly,
        left_out: LeftOutRows,
        read: &mpsc::Sender<TranscriptProgress>,
    ) -> LoadedRoot {
        let stubs = match read_only.rediscovered {
            Some(rediscovered) => adopt_rediscovery(stubs, found, rediscovered, read),
            None => stubs,
        };
        self.load_root_writing(root, stubs, left_out, read, RootCaches::SHARDS_ONLY)
    }

    fn report_cache_update<T>(&self, result: Result<T>) {
        if let Err(error) = result {
            debug::warn(
                self.debug_level,
                &format!("Failed to update the session cache: {error}"),
            );
        }
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
        let discovered_keys: HashSet<_> =
            stubs.iter().map(|stub| stub.cache_key.as_str()).collect();
        let removed_shards: BTreeSet<_> = stored
            .iter()
            .filter(|key| reconciles_deletions && !discovered_keys.contains(key.as_str()))
            .map(|key| shard_index(key))
            .collect();
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
                removed_shards: &removed_shards,
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

    /// The root's cache as it is now, read without refresh ownership, each
    /// statement waiting at most `timeout` for another connection's lock.
    fn open_database(
        &self,
        root: &SessionRoot,
        stubs: &[SessionStub],
        timeout: Duration,
    ) -> Result<Option<RootCache>> {
        if !self.may_hold_root(stubs) {
            return Ok(None);
        }
        self.open_database_reader(DatabaseReader::WaitingAtMost(timeout))?
            .map(|database| self.read_root_cache(root, database))
            .transpose()
    }

    /// The root's cache for `owner`'s refresh, read in one writer-locked
    /// batch. The batch's commit restarts the takeover window, so a slow
    /// comparison cannot hand ownership to a waiter before the first parse.
    fn open_database_for_refresh(
        &self,
        root: &SessionRoot,
        stubs: &[SessionStub],
        owner: &RefreshToken,
    ) -> Option<RootCache> {
        if !self.may_hold_root(stubs) {
            return None;
        }
        self.report_open_failure(self.open_database_writer(owner).and_then(|database| {
            let Some(database) = database else {
                return Ok(None);
            };
            let (snapshot, coordinated) = owner.read(|| self.read_root_cache(root, database));
            coordinated?;
            snapshot.map(Some)
        }))
    }

    /// False when the database cannot hold the root: a refused base, or no
    /// database file and no stubs to write.
    fn may_hold_root(&self, stubs: &[SessionStub]) -> bool {
        self.cache.base().is_some_and(|base| {
            !sqlite_cache::is_refused(base)
                && (!stubs.is_empty() || base.join(sqlite_cache::DATABASE_FILE_NAME).exists())
        })
    }

    /// Current fingerprints and all stored keys are separate: unfinished
    /// refreshes must remain eligible for deletion reconciliation.
    fn read_root_cache(&self, root: &SessionRoot, database: CacheDatabase) -> Result<RootCache> {
        let cache = database.reader();
        let current = cache.current_fingerprints(self.storage.cache(), &root.path)?;
        let stored = cache.stored_keys(self.storage.cache(), &root.path)?;
        let catalogue = cache.catalogue(
            self.storage.cache(),
            self.storage.source(),
            &root.path,
            self.show_last,
        )?;
        Ok(RootCache {
            database,
            current,
            stored,
            catalogue,
        })
    }

    fn report_open_failure(&self, opened: Result<Option<RootCache>>) -> Option<RootCache> {
        opened
            .inspect_err(|error| {
                debug::warn(
                    self.debug_level,
                    &format!("Failed to open the session cache database: {error}"),
                );
            })
            .ok()
            .flatten()
    }

    /// [`Self::load_root`]'s sessions, writing to `caches`'s database each
    /// session it parses, and reading every session that database does not
    /// hold as it is now. Without a database, every session restores from
    /// its shard entry or is read from its transcripts.
    ///
    /// Sessions restored from the database, a shard entry or a left-out row
    /// are restored in one pass without the writer lock. Sessions that parse
    /// are grouped by [`Self::parse_in_groups`].
    fn load_root_writing(
        &self,
        root: &SessionRoot,
        stubs: Vec<SessionStub>,
        left_out: LeftOutRows,
        read: &mpsc::Sender<TranscriptProgress>,
        caches: RootCaches<'_>,
    ) -> LoadedRoot {
        let database = caches.database;
        let (external_titles, cached) = self.read_under_writer_lock(caches.access.owner(), || {
            let external_titles = self.storage.external_titles(root);
            let cached = if let Some(database) = database {
                let mut shards = database.removed_shards.clone();
                for stub in &stubs {
                    if !database.holds(stub) {
                        let excluded =
                            database
                                .catalogue
                                .get(&stub.cache_key)
                                .is_some_and(|session| {
                                    let CatalogueSession::Listed(row) = session else {
                                        return false;
                                    };
                                    let mut row = (**row).clone();
                                    if let Some(title) = external_titles.get(&row.session_id) {
                                        apply_external_title(&mut row, title);
                                    }
                                    left_out.0.is_some_and(|leaves_out| leaves_out(&row))
                                });
                        if !excluded {
                            shards.insert(shard_index(&stub.cache_key));
                        }
                    }
                }
                shards
                    .into_iter()
                    .flat_map(|index| self.cache.read_existing_shard(&root.path, index))
                    .collect()
            } else {
                self.cache.read_existing_metered(&root.path, self.meter)
            };
            (external_titles, cached)
        });
        let mut entries = CacheEntries::read_from(cached);
        let is_current = |stub: &SessionStub| {
            database.map_or_else(
                || cached_entry(&entries.read, stub).is_some(),
                |database| database.holds(stub),
            )
        };
        let mut left_out_rows = self.left_out_rows(
            &entries.read,
            &stubs,
            left_out,
            &external_titles,
            &is_current,
        );
        if let Some(database) = database {
            for (stub, left_out_row) in stubs.iter().zip(&mut left_out_rows) {
                if !is_current(stub)
                    && let Some(leaves_out) = left_out.0
                    && let Some(CatalogueSession::Listed(row)) =
                        database.catalogue.get(&stub.cache_key)
                {
                    let mut row = (**row).clone();
                    if let Some(title) = external_titles.get(&row.session_id) {
                        apply_external_title(&mut row, title);
                    }
                    if leaves_out(&row) {
                        *left_out_row = Some(row);
                    }
                }
            }
        }
        let uses: Vec<EntryUse> = stubs
            .iter()
            .zip(&left_out_rows)
            .map(|(stub, left_out_row)| {
                if left_out_row.is_some() {
                    EntryUse::LeftOut
                } else if is_current(stub) {
                    EntryUse::Current
                } else {
                    EntryUse::Miss
                }
            })
            .collect();
        self.meter.refreshing(
            stubs
                .iter()
                .zip(&uses)
                .filter(|(_, entry_use)| **entry_use == EntryUse::Miss)
                .map(|(stub, _)| transcript_count(stub))
                .sum(),
        );
        let taken = entries.take_current_entries_outside_writable_shards(&stubs, &uses);
        let mut conversations = Vec::new();
        let mut skipped = SkippedSessions::default();
        let mut recorded = Vec::with_capacity(stubs.len());
        let mut changed_shards = BTreeSet::new();

        let mut outcomes: Vec<Option<SessionOutcome>> = stubs
            .par_iter()
            .zip(uses.par_iter())
            .zip(taken)
            .zip(left_out_rows)
            .map(|(((stub, &entry_use), taken), left_out_row)| {
                if entry_use == EntryUse::Miss {
                    return None;
                }
                let catalogue = database.and_then(|database| database.catalogue_of(stub));
                let restored = match (left_out_row, catalogue, taken) {
                    (Some(row), _, _) => SessionOutcome::Restored(row),
                    (None, Some(catalogue), _) => {
                        restore_from_catalogue(catalogue, &external_titles)
                    }
                    (None, None, Some(entry)) => self.restore(entry, stub, &external_titles),
                    (None, None, None) => self.restore_or_parse(
                        root,
                        cached_entry(&entries.read, stub),
                        &external_titles,
                        stub,
                        &|| {},
                    ),
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
                let outcome =
                    self.restore_or_parse(root, None, &external_titles, stub, &on_transcript_read);
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
                    outcomes[index] = Some(outcome);
                }
            },
        );

        let outcomes = outcomes
            .into_iter()
            .map(|outcome| outcome.expect("every session restored or parsed"));
        for ((stub, outcome), entry_use) in stubs.iter().zip(outcomes).zip(uses) {
            let found = FoundSession {
                source: self.storage.source(),
                fingerprint: stub.fingerprint.stamp(),
                has_transient_read_error: false,
            };
            recorded.push(match self.record(stub, outcome, &mut entries) {
                SessionRead::Listed(conversation) => {
                    let session = RecordedSession {
                        project_dir: conversation.cwd.clone(),
                        session_id: conversation.session_id.clone(),
                    };
                    conversations.push(*conversation);
                    Some(session)
                }
                SessionRead::Empty => {
                    skipped.unlisted.insert(stub.locator.clone(), found);
                    None
                }
                SessionRead::Failed(ReadError::Permanent) => {
                    skipped.unreadable.insert(stub.locator.clone(), found);
                    None
                }
                SessionRead::Failed(ReadError::Transient) => None,
            });
            if entry_use == EntryUse::Miss && entries.current.contains_key(&stub.cache_key) {
                changed_shards.insert(shard_index(&stub.cache_key));
            }
        }
        if let Some(owner) = caches.access.owner() {
            if caches.deletions == Deletions::Reconcile {
                changed_shards.extend(
                    entries
                        .dropped_keys()
                        .map(|cache_key| shard_index(cache_key)),
                );
            }
            for index in changed_shards {
                let written = owner.write_batch(|| match caches.deletions {
                    Deletions::Reconcile => {
                        self.cache
                            .write_shard(&root.path, index, &entries.current, self.clock)
                    }
                    Deletions::Keep => self.cache.merge_into_shard(
                        &root.path,
                        index,
                        entries.current_in_shard(index),
                        self.clock,
                    ),
                });
                let failed = written.is_err();
                self.report_cache_update(written);
                if failed {
                    break;
                }
            }
        }
        LoadedRoot {
            conversations,
            skipped,
            discovered: stubs
                .into_iter()
                .zip(recorded)
                .map(|(stub, recorded)| DiscoveredSession { stub, recorded })
                .collect(),
            ignored: Vec::new(),
        }
    }

    /// For each stub, the row its shard entry restores when the caches do not
    /// hold the transcript as it is now, as `is_current` tells, and
    /// `left_out` holds that row; `None` for every other stub.
    fn left_out_rows(
        &self,
        cached: &HashMap<String, SessionCacheEntry>,
        stubs: &[SessionStub],
        left_out: LeftOutRows,
        external_titles: &HashMap<String, SessionTitle>,
        is_current: &(dyn Fn(&SessionStub) -> bool + Sync),
    ) -> Vec<Option<Conversation>> {
        let Some(leaves_out) = left_out.0 else {
            return stubs.iter().map(|_| None).collect();
        };
        stubs
            .par_iter()
            .map(|stub| {
                let Some(SessionCacheEntry::Listed(entry)) = cached.get(&stub.cache_key) else {
                    return None;
                };
                if is_current(stub) {
                    return None;
                }
                let row = restore_from_cache(
                    self.storage,
                    entry.clone(),
                    stub.locator.clone(),
                    self.show_last,
                    external_titles,
                    self.clock,
                );
                leaves_out(&row).then_some(row)
            })
            .collect()
    }

    /// `outcome`'s row, with its entry put in `entries`: a restored session
    /// keeps the entry it was restored from. A failed read, or a row built
    /// without a transcript after a transient error, gets no entry, so the
    /// next launch reads the session again.
    fn record(
        &self,
        stub: &SessionStub,
        outcome: SessionOutcome,
        entries: &mut CacheEntries,
    ) -> SessionRead {
        match outcome {
            SessionOutcome::Restored(conversation) => {
                entries.keep(stub);
                SessionRead::Listed(Box::new(self.resolve_preview_and_project(conversation)))
            }
            SessionOutcome::Parsed(conversation) => {
                let conversation = self.listed_row(stub, conversation);
                if let Some(fingerprint) = conversation.fingerprint
                    && !conversation.has_transient_read_error
                {
                    entries.insert(stub, listed_session_entry(&conversation, fingerprint));
                }
                SessionRead::Listed(Box::new(conversation))
            }
            SessionOutcome::Empty => {
                if let Some(fingerprint) = stub.fingerprint.stamp() {
                    entries.insert(stub, SessionCacheEntry::Empty(fingerprint));
                }
                SessionRead::Empty
            }
            SessionOutcome::Failed(error) => SessionRead::Failed(error),
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

    /// `entry`, a cache entry for the transcript as it is now, first, then
    /// the transcript.
    fn restore_or_parse(
        &self,
        root: &SessionRoot,
        entry: Option<&SessionCacheEntry>,
        external_titles: &HashMap<String, SessionTitle>,
        stub: &SessionStub,
        on_transcript_read: &(dyn Fn() + Sync),
    ) -> SessionOutcome {
        match entry {
            Some(SessionCacheEntry::Empty(_)) => SessionOutcome::Empty,
            Some(SessionCacheEntry::Listed(entry)) => {
                self.restore(entry.clone(), stub, external_titles)
            }
            None => {
                let parsing = self.clock.now();
                let outcome = parse_session(
                    self.storage,
                    stub,
                    root,
                    self.debug_level,
                    on_transcript_read,
                    self.clock,
                );
                self.meter.session_parsed(
                    transcript_count(stub),
                    stub.fingerprint.size,
                    self.clock.elapsed_since(parsing),
                );
                outcome
            }
        }
    }

    /// `stub`'s row restored from `entry`, a cache entry whose fingerprint
    /// matches the transcript.
    fn restore(
        &self,
        entry: ListedSessionEntry,
        stub: &SessionStub,
        external_titles: &HashMap<String, SessionTitle>,
    ) -> SessionOutcome {
        SessionOutcome::Restored(restore_from_cache(
            self.storage,
            entry,
            stub.locator.clone(),
            self.show_last,
            external_titles,
            self.clock,
        ))
    }
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
    removed_shards: &'a BTreeSet<usize>,
}

impl DatabaseWrites<'_> {
    fn holds(&self, stub: &SessionStub) -> bool {
        holds_session(self.current, self.catalogue, stub)
    }

    /// `stub`'s catalogue row, when the database holds its transcript as it
    /// is now.
    fn catalogue_of(&self, stub: &SessionStub) -> Option<&CatalogueSession> {
        self.holds(stub)
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

/// The outcome `catalogue`, a database catalogue row, restores, with the
/// title stored beside the transcript when `external_titles` holds one.
fn restore_from_catalogue(
    catalogue: &CatalogueSession,
    external_titles: &HashMap<String, SessionTitle>,
) -> SessionOutcome {
    match catalogue {
        CatalogueSession::Listed(row) => {
            let mut row = (**row).clone();
            if let Some(title) = external_titles.get(&row.session_id) {
                apply_external_title(&mut row, title);
            }
            SessionOutcome::Restored(row)
        }
        CatalogueSession::Empty => SessionOutcome::Empty,
        CatalogueSession::Unreadable => SessionOutcome::Failed(ReadError::Permanent),
    }
}

/// The session cache database's projection of `outcome`, the read of
/// `storage`'s session at `stub` under `root`. `None` for a read it records
/// nothing of: a restored row, a transient failure, or a row built without a
/// transcript after a transient error, which the next load reads again.
fn cache_record(
    storage: &dyn SessionStorage,
    root: &SessionRoot,
    stub: &SessionStub,
    outcome: &SessionOutcome,
) -> Option<SessionRecord> {
    let unlisted =
        |status| SessionRecord::unlisted(storage.cache(), storage.source(), root, stub, status);
    match outcome {
        SessionOutcome::Parsed(conversation) if !conversation.has_transient_read_error => {
            SessionRecord::listed(storage.cache(), storage.source(), root, stub, conversation)
        }
        SessionOutcome::Empty => unlisted(SessionStatus::Empty),
        SessionOutcome::Failed(ReadError::Permanent) => unlisted(SessionStatus::Unreadable),
        SessionOutcome::Parsed(_)
        | SessionOutcome::Restored(_)
        | SessionOutcome::Failed(ReadError::Transient) => None,
    }
}

/// `storage`'s session at `stub` under `root`, read from its transcripts as
/// a load reads a changed session, written to the shard cache for the load
/// that follows. `meter` records the parse.
pub(crate) fn sample_record(
    storage: &dyn SessionStorage,
    root: &SessionRoot,
    stub: &SessionStub,
    owner: &RefreshToken,
    debug_level: Option<DebugLevel>,
    meter: AgentMeter,
    check_space: &dyn Fn() -> Result<()>,
) -> Result<Option<SessionRecord>> {
    let clock = crate::clock::SystemClock::new();
    let (parsed, coordinated) = owner.read(|| {
        check_space()?;
        let parsing = clock.now();
        let outcome = parse_session(
            storage,
            stub,
            root,
            debug_level,
            &|| meter.transcript_read(),
            &clock,
        );
        meter.session_parsed(
            transcript_count(stub),
            stub.fingerprint.size,
            clock.elapsed_since(parsing),
        );
        let record = cache_record(storage, root, stub, &outcome);
        let cache = SessionCacheStore::under(owner.base(), storage.cache());
        let loader = SessionLoader {
            storage,
            cache: &cache,
            show_last: false,
            debug_level,
            meter,
            clock: &clock,
        };
        let mut entries = CacheEntries::read_from(HashMap::new());
        loader.record(stub, outcome, &mut entries);
        check_space()?;
        Ok::<_, AppError>((record, entries.current))
    });
    coordinated?;
    let (record, entries) = parsed?;
    if !entries.is_empty() {
        check_space()?;
        let cache = SessionCacheStore::under(owner.base(), storage.cache());
        owner.write_batch(|| {
            cache.merge_into_shard(&root.path, shard_index(&stub.cache_key), entries, &clock);
        })?;
    }
    check_space()?;
    Ok(record)
}

/// `stub`'s cache entry, when the cache holds one for the transcript as it is
/// now.
fn cached_entry<'a>(
    cached: &'a HashMap<String, SessionCacheEntry>,
    stub: &SessionStub,
) -> Option<&'a SessionCacheEntry> {
    let stamp = stub.fingerprint.stamp()?;
    cached
        .get(&stub.cache_key)
        .filter(|entry| entry.fingerprint() == stamp)
}

/// How a load uses a stub's cache entry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EntryUse {
    /// The entry matches the transcript, and the row restores from it.
    Current,
    /// The transcript changed, but the command leaves the entry's row out:
    /// the row restores from the entry, which stays as it was.
    LeftOut,
    /// The cache holds no entry for the transcript as it is now, so the load
    /// parses it.
    Miss,
}

/// A root's or shard's cache entries as read, and the entries a write would
/// hold now. A restored session's entry moves from the first map to the
/// second rather than being rebuilt from its row. In a shard no write can
/// touch, a hit gives its text to the row instead; in a shard a write may
/// touch, it keeps its entry beside the copied row until the shard is written.
struct CacheEntries {
    read: HashMap<String, SessionCacheEntry>,
    current: HashMap<String, SessionCacheEntry>,
}

impl CacheEntries {
    fn read_from(read: HashMap<String, SessionCacheEntry>) -> Self {
        Self {
            read,
            current: HashMap::new(),
        }
    }

    /// Take out the current listed entries `stubs` restore from, one result
    /// per stub, in every shard a load cannot write, so each restored row
    /// owns its entry's text. `uses` holds each stub's [`EntryUse`].
    fn take_current_entries_outside_writable_shards(
        &mut self,
        stubs: &[SessionStub],
        uses: &[EntryUse],
    ) -> Vec<Option<ListedSessionEntry>> {
        let writable = self.shards_a_load_may_write(stubs, uses);
        stubs
            .iter()
            .zip(uses)
            .map(|(stub, &entry_use)| {
                let is_current_listed_entry = entry_use == EntryUse::Current
                    && matches!(
                        self.read.get(&stub.cache_key),
                        Some(SessionCacheEntry::Listed(_))
                    );
                if !is_current_listed_entry || writable.contains(&shard_index(&stub.cache_key)) {
                    return None;
                }
                match self.read.remove(&stub.cache_key) {
                    Some(SessionCacheEntry::Listed(entry)) => Some(entry),
                    _ => None,
                }
            })
            .collect()
    }

    /// The shards a load of `stubs` can rewrite: those holding a stub that
    /// misses the cache, or an entry read that no stub restores from. `uses`
    /// holds each stub's [`EntryUse`].
    fn shards_a_load_may_write(&self, stubs: &[SessionStub], uses: &[EntryUse]) -> HashSet<usize> {
        let mut restored_keys = HashSet::new();
        let mut shards = HashSet::new();
        for (stub, &entry_use) in stubs.iter().zip(uses) {
            if entry_use == EntryUse::Miss {
                shards.insert(shard_index(&stub.cache_key));
            } else {
                restored_keys.insert(stub.cache_key.as_str());
            }
        }
        shards.extend(
            self.read
                .keys()
                .filter(|cache_key| !restored_keys.contains(cache_key.as_str()))
                .map(|cache_key| shard_index(cache_key)),
        );
        shards
    }

    /// Move `stub`'s entry as read into the entries a write would hold.
    fn keep(&mut self, stub: &SessionStub) {
        if let Some(entry) = self.read.remove(&stub.cache_key) {
            self.current.insert(stub.cache_key.clone(), entry);
        }
    }

    fn insert(&mut self, stub: &SessionStub, entry: SessionCacheEntry) {
        self.current.insert(stub.cache_key.clone(), entry);
    }

    /// The entries a write would hold now that belong to shard `index`.
    fn current_in_shard(&self, index: usize) -> HashMap<String, SessionCacheEntry> {
        self.current
            .iter()
            .filter(|(cache_key, _)| shard_index(cache_key) == index)
            .map(|(cache_key, entry)| (cache_key.clone(), entry.clone()))
            .collect()
    }

    /// The keys read that a write would no longer hold: sessions deleted from
    /// disk, or whose read failed.
    fn dropped_keys(&self) -> impl Iterator<Item = &String> {
        self.read
            .keys()
            .filter(|cache_key| !self.current.contains_key(*cache_key))
    }
}

/// The resulting type of a discovered session.
///
/// The two outcomes that yield no conversation are separate variants because
/// they cache differently. Only `Empty` says something about the transcript's
/// content, and content is all a fingerprint can stand for.
enum SessionOutcome {
    /// Rebuilt from a cache entry whose fingerprint still matches.
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
///
/// The resolved field wins so the cache entry written for a row equals the
/// row itself in either call order, including if resolution ever overrides
/// the derived path.
fn project_path_of(conversation: &Conversation) -> PathBuf {
    conversation
        .project_path
        .clone()
        .or_else(|| conversation.cwd.clone())
        .unwrap_or_else(|| PathBuf::from("unknown"))
}

/// One entry per session, holding the row as parsed: sub-agent transcripts
/// merged, and named so a cache hit restores the same row.
fn listed_session_entry(
    conversation: &Conversation,
    fingerprint: CachedFingerprint,
) -> SessionCacheEntry {
    SessionCacheEntry::Listed(ListedSessionEntry {
        fingerprint,
        conversation: cached_conversation(conversation),
        session_id: conversation.session_id.clone(),
        subagents: conversation.subagents.clone(),
        project_path: project_path_of(conversation),
    })
}

/// `entry`'s row, its text moved out of the entry, carrying the fingerprint
/// the entry was written for.
fn restore_from_cache(
    storage: &dyn SessionStorage,
    entry: ListedSessionEntry,
    locator: PathBuf,
    show_last: bool,
    external_titles: &HashMap<String, SessionTitle>,
    clock: &dyn Clock,
) -> Conversation {
    let ListedSessionEntry {
        fingerprint,
        conversation: cached,
        session_id,
        subagents,
        project_path,
    } = entry;
    let mut conversation =
        conversation_from_cached(storage.source(), cached, locator, show_last, clock);
    conversation.fingerprint = Some(fingerprint);
    conversation.session_id = session_id;
    conversation.subagents = subagents;
    conversation.project_name = Some(format_short_name_from_path(&project_path));
    conversation.project_path = Some(project_path);
    // Use the title store's title when it names the session; the cached title
    // is the fallback, because the store can change while the transcript does
    // not.
    if let Some(title) = external_titles.get(&conversation.session_id) {
        apply_external_title(&mut conversation, title);
    }
    conversation
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
) -> SessionOutcome {
    match storage.parse_session(stub, root, debug_level, on_transcript_read, clock) {
        Ok(Some(conversation)) if conversation.source == storage.source() => {
            SessionOutcome::Parsed(conversation)
        }
        Ok(_) => SessionOutcome::Empty,
        Err(error) => {
            debug::warn(
                debug_level,
                &format!(
                    "Failed to parse {} session {}: {error}",
                    storage.source().list_label(),
                    stub.locator.display()
                ),
            );
            SessionOutcome::Failed(ReadError::of(&error))
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
    use crate::history::RefreshOwner;
    use crate::history::cache::{self, keys_in_distinct_shards};
    use crate::history::load_meter::{ItemCount, LoadMeter, LoadStep, StepProgress};
    use crate::history::parser::{SubagentContribution, SubagentReads};
    use crate::history::provider::{Fingerprint, IgnoredSessions};
    use crate::history::sqlite_cache::refresh::Acquisition;
    use std::collections::{BTreeMap, HashSet};
    use std::sync::Mutex;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

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
        /// Titles stored beside the transcripts, by session id.
        external_titles: HashMap<String, SessionTitle>,
        /// The sessions every root reports as ignored.
        ignored: Vec<IgnoredSessions>,
        /// The cache base each `remove_superseded_cache` call named, with the
        /// shard files under it at the time.
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
                external_titles: HashMap::new(),
                ignored: Vec::new(),
                superseded_cache_removals: Mutex::new(Vec::new()),
            }
        }

        /// `title` stored beside `stub`'s transcript, keyed by the session
        /// id its parsed row carries.
        fn with_external_title(mut self, stub: &SessionStub, title: &str) -> Self {
            self.external_titles.insert(
                stub.locator.to_string_lossy().into_owned(),
                SessionTitle::Custom(title.to_owned()),
            );
            self
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
                magic: *b"VIRTUAL1",
                schema_version: 1,
            }
        }

        fn roots_in(
            &self,
            _environment: &super::super::RootEnvironment,
        ) -> Result<Vec<SessionRoot>> {
            Ok(self.roots.clone())
        }

        fn discover(&self, root: &SessionRoot) -> Result<DiscoveredSessions> {
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
            self.external_titles.clone()
        }

        fn remove_superseded_cache(&self, cache_base: &std::path::Path) {
            self.superseded_cache_removals.lock().unwrap().push((
                cache_base.to_path_buf(),
                shard_files_under(cache_base).len(),
            ));
        }
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
        let mut conversation = cache::conversation_from_cached(
            Source::Pi,
            cache::CachedConversation::default(),
            PathBuf::new(),
            false,
            &crate::clock::TestClock::default(),
        );
        conversation.session_id = id.to_owned();
        conversation.agent_search_text = agent_text.to_owned();
        conversation.message_count = 1;
        conversation.total_tokens = 10;
        conversation
    }

    impl SessionLoader<'_> {
        /// [`SessionLoader::read_one`]'s row, when the session holds one.
        fn load_one(&self, root: &SessionRoot, stub: &SessionStub) -> Option<Conversation> {
            listed_row(self.read_one(root, stub.clone()).value)
        }

        /// The session `stub` names under `root`, read as the TUI's
        /// background refresh thread reads it to cache it.
        fn cache_one(&self, root: &SessionRoot, stub: SessionStub) -> InteractiveRead<SessionRead> {
            let external_titles = self.storage.external_titles(root);
            let mut reads = self.read_stubs(
                root,
                &external_titles,
                vec![stub],
                StubsAfterRediscovery::SameFiles,
            );
            let (locator, read) = reads.read.pop().expect("one read");
            InteractiveRead {
                follow_up: reads.follow_up_of(&locator),
                value: read,
            }
        }

        /// [`Self::cache_one`]'s row, when the session holds one.
        fn cache_row(&self, root: &SessionRoot, stub: &SessionStub) -> Option<Conversation> {
            listed_row(self.cache_one(root, stub.clone()).value)
        }
    }

    fn listed_row(read: SessionRead) -> Option<Conversation> {
        match read {
            SessionRead::Listed(conversation) => Some(*conversation),
            SessionRead::Empty | SessionRead::Failed(_) => None,
        }
    }

    #[test]
    fn a_load_reuses_every_providers_sampled_session() {
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
            let ownership = RefreshOwnership::acquire(base.path(), || ()).unwrap();
            let record = sample_record(
                storage,
                &root,
                &stub,
                ownership.token(),
                None,
                agent_meter,
                &|| Ok(()),
            )
            .unwrap()
            .unwrap_or_else(|| panic!("{:?}'s sample was not cached", row.source));
            let mut database = SqliteCache::open_for_refresh(ownership.token()).unwrap();
            database.mark_admitted().unwrap();
            database.admit_storage(storage.cache()).unwrap();
            database.write(&record).unwrap();
            drop(record);
            let cache = SessionCacheStore::under(base.path(), storage.cache());
            let shard = shard_file_of_session(base.path(), &stub.cache_key);
            std::fs::write(&shard, b"unreadable legacy shard").unwrap();
            let loader = SessionLoader {
                storage,
                cache: &cache,
                show_last: false,
                debug_level: None,
                meter: agent_meter,
                clock: &crate::clock::TestClock::default(),
            };
            let (read, _) = mpsc::channel();

            let loaded = loader.load_root(
                &root,
                DiscoveredSessions::complete(vec![stub.clone()]),
                LeftOutRows::NONE,
                &read,
            );

            assert_eq!(loaded.conversations.len(), 1, "{:?}", row.source);
            assert!(loaded.conversations[0].cached_session.is_some());
            assert!(loaded.conversations[0].full_text.is_empty());
            let by_id = loader.load_one(&root, &stub).expect("catalogue lookup");
            assert!(by_id.cached_session.is_some());
            assert_eq!(std::fs::read(&shard).unwrap(), b"unreadable legacy shard");
            assert_eq!(
                meter.agents()[0].refresh.transcripts,
                transcript_count(&stub),
                "{:?}'s sample was parsed again during the load",
                row.source
            );
            assert_eq!(
                loaded.conversations[0].session_id, row.session_id,
                "{:?}",
                row.source
            );
        }
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
        let cache = SessionCacheStore::under(base.path(), storage.cache());
        let root = SessionRoot::new("container.db");
        let database = SqliteCache::open(base.path()).unwrap();
        database.mark_admitted().unwrap();
        database.admit_storage(storage.cache()).unwrap();
        drop(database);
        let owner = OtherProcessOwner::hold(base.path());
        let meter = LoadMeter::default();
        let clock = crate::clock::TestClock::default();
        std::thread::scope(|scope| {
            let loader = SessionLoader {
                storage: &storage,
                cache: &cache,
                show_last: false,
                debug_level: None,
                meter: meter.agent(Source::Pi),
                clock: &clock,
            };
            let captured = DiscoveredSessions::complete(vec![old.clone()]);
            let root_ref = &root;
            let (read, updates) = mpsc::channel();
            let running =
                scope.spawn(move || loader.load_root(root_ref, captured, LeftOutRows::NONE, &read));
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
        let database = SqliteCache::open_waiting(base.path()).unwrap().unwrap();
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
        let cache = SessionCacheStore::under(base.path(), storage.cache());
        let root = SessionRoot::new("container.db");
        let mut database = SqliteCache::open(base.path()).unwrap();
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
            let cache = &cache;
            scope.spawn(move || {
                finished
                    .send(load_sessions_with_cache(storage, cache, false, None))
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
    fn a_shard_only_deletion_waits_and_reads_the_shards_once() {
        use crate::history::sqlite_cache::refresh::tests::OtherProcessOwner;
        let base = tempfile::tempdir().unwrap();
        let seeded = VirtualStorage::new(vec![virtual_stub("ses_deleted", 100, 1_000)]);
        let cache = SessionCacheStore::under(base.path(), seeded.cache());
        load_sessions_with_cache(&seeded, &cache, false, None).unwrap();
        let before = shard_files_under(base.path());
        let owner = OtherProcessOwner::hold(base.path());
        let deleted = VirtualStorage::new(Vec::new());
        let meter = LoadMeter::default();
        std::thread::scope(|scope| {
            let running = scope.spawn(|| {
                load_metered_with_cache(
                    &deleted,
                    &cache,
                    meter.agent(Source::Pi),
                    LeftOutRows::NONE,
                )
                .unwrap()
            });
            wait_for_refresh(&meter);
            assert_eq!(shard_files_under(base.path()), before);
            assert_eq!(
                meter.agents().first().map_or(0, |agent| agent.cache.bytes),
                0
            );
            owner.release();
            assert!(running.join().unwrap().conversations.is_empty());
        });
        assert!(
            cache
                .read(
                    &PathBuf::from("container.db"),
                    &crate::clock::TestClock::default()
                )
                .is_empty()
        );
        assert_eq!(meter.agents()[0].cache.files, before.len());
    }

    #[test]
    fn a_failed_ownership_acquisition_returns_parsed_results_without_rewriting_shards() {
        let base = tempfile::tempdir().unwrap();
        let stub = virtual_stub("ses_changed", 100, 1_000);
        let seeded = VirtualStorage::new(vec![stub]);
        let cache = SessionCacheStore::under(base.path(), seeded.cache());
        load_sessions_with_cache(&seeded, &cache, false, None).unwrap();
        let database = base.path().join(sqlite_cache::DATABASE_FILE_NAME);
        std::fs::remove_file(&database).unwrap();
        std::fs::create_dir(&database).unwrap();
        let before = shard_files_under(base.path());
        let changed = VirtualStorage::new(vec![virtual_stub("ses_changed", 200, 2_000)]);
        let rows = load_sessions_with_cache(&changed, &cache, false, None).unwrap();
        assert_eq!(changed.parse_count(), 1);
        assert_eq!(rows[0].fingerprint, changed.stubs[0].fingerprint.stamp());
        assert_eq!(shard_files_under(base.path()), before);
    }

    #[test]
    fn an_interactive_read_returns_the_transcript_at_once_while_another_process_refreshes() {
        use crate::history::sqlite_cache::refresh::tests::OtherProcessOwner;
        let base = tempfile::tempdir().unwrap();
        let target = virtual_stub("ses_target", 200, 2_000);
        let storage =
            VirtualStorage::new(vec![target.clone(), virtual_stub("ses_other", 300, 3_000)]);
        let cache = SessionCacheStore::under(base.path(), storage.cache());
        let root = SessionRoot::new("container.db");
        let database = SqliteCache::open(base.path()).unwrap();
        database.mark_admitted().unwrap();
        database.admit_storage(storage.cache()).unwrap();
        drop(database);
        let shards_before = shard_files_under(base.path());
        let owner = OtherProcessOwner::hold(base.path());
        let loader = SessionLoader {
            storage: &storage,
            cache: &cache,
            show_last: false,
            debug_level: None,
            meter: AgentMeter::UNMETERED,
            clock: &crate::clock::TestClock::default(),
        };

        let started = Instant::now();
        let read = loader.read_one(&root, target);

        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the read doesn't wait for the other refresh"
        );
        assert_eq!(
            read.follow_up,
            Some(CacheFollowUp::AwaitRefresh(RefreshOwner::AnotherProcess))
        );
        assert!(matches!(read.value, SessionRead::Listed(_)));
        assert_eq!(storage.parsed_ids(), ["ses_target"]);
        owner.release();
        let database = SqliteCache::open_waiting(base.path()).unwrap().unwrap();
        assert!(
            database
                .stored_keys(storage.cache(), &root.path)
                .unwrap()
                .is_empty(),
            "the read doesn't write the database"
        );
        assert_eq!(shard_files_under(base.path()), shards_before);
    }

    #[test]
    fn an_interactive_read_returns_the_transcript_at_once_during_another_processs_rebuild() {
        use crate::history::sqlite_cache::refresh::tests::{OtherProcessOwner, OwnerActivity};
        let base = tempfile::tempdir().unwrap();
        let target = virtual_stub("ses_target", 200, 2_000);
        let storage = VirtualStorage::new(vec![target.clone()]);
        let cache = SessionCacheStore::under(base.path(), storage.cache());
        let root = SessionRoot::new("container.db");
        admit_database(base.path());
        let rebuild = OtherProcessOwner::start(base.path(), OwnerActivity::InsideRebuild);
        let loader = SessionLoader {
            storage: &storage,
            cache: &cache,
            show_last: false,
            debug_level: None,
            meter: AgentMeter::UNMETERED,
            clock: &crate::clock::TestClock::default(),
        };

        let started = Instant::now();
        let read = loader.read_one(&root, target);

        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the read doesn't wait for the rebuild"
        );
        assert_eq!(
            read.follow_up,
            Some(CacheFollowUp::AwaitRefresh(RefreshOwner::AnotherProcess))
        );
        assert!(matches!(read.value, SessionRead::Listed(_)));
        assert_eq!(storage.parsed_ids(), ["ses_target"]);
        drop(rebuild);
    }

    #[test]
    fn a_targeted_read_whose_repeated_discovery_fails_reads_without_writes() {
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
        let cache = SessionCacheStore::under(base.path(), storage.cache());
        let root = SessionRoot::new("container.db");
        admit_database(base.path());
        let loader = SessionLoader {
            storage: &storage,
            cache: &cache,
            show_last: false,
            debug_level: None,
            meter: AgentMeter::UNMETERED,
            clock: &crate::clock::TestClock::default(),
        };

        let read = loader.cache_one(&root, stub);

        assert!(matches!(read.value, SessionRead::Listed(_)));
        assert_eq!(read.follow_up, None);
        assert_eq!(*storage.discoveries.lock().unwrap(), 1);
        assert_eq!(storage.parsed_ids(), ["ses_readable"]);
        assert!(
            stored_keys_under(base.path(), &storage).is_empty(),
            "the read doesn't write the database"
        );
        assert!(shard_files_under(base.path()).is_empty());
        assert!(
            matches!(
                RefreshOwnership::try_acquire_now(base.path()).unwrap(),
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
        let base = tempfile::tempdir().unwrap();
        let storage = VirtualStorage::new(stubs_in_distinct_shards(3));
        let cache = SessionCacheStore::under(base.path(), storage.cache());
        admit_database(base.path());
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
                        &cache,
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

    /// The cache holds one entry per session, with each sub-agent transcript
    /// inside its session's stub.
    #[test]
    fn every_cache_entry_names_session() {
        let cache_base = tempfile::tempdir().unwrap();
        let mut with_subagents = virtual_stub("ses_parent", 100, 1_000);
        with_subagents.subagents = vec![
            PathBuf::from("container.db").join("ses_child.jsonl"),
            PathBuf::from("container.db").join("ses_nested.jsonl"),
        ];
        let storage = VirtualStorage::new(vec![with_subagents, virtual_stub("ses_other", 50, 500)]);
        let cache = SessionCacheStore::under(cache_base.path(), storage.cache());

        let listed = load_sessions_with_cache(&storage, &cache, false, None).unwrap();

        assert_eq!(listed.len(), 2);
        assert_eq!(storage.parsed_ids(), vec!["ses_other", "ses_parent"]);
        let mut keys = cache
            .read(
                std::path::Path::new("container.db"),
                &crate::clock::TestClock::default(),
            )
            .into_keys()
            .collect::<Vec<_>>();
        keys.sort();
        assert_eq!(keys, vec!["ses_other", "ses_parent"]);

        let restored = load_sessions_with_cache(&storage, &cache, false, None).unwrap();
        let parent = restored
            .iter()
            .find(|conversation| conversation.path.ends_with("ses_parent.jsonl"))
            .unwrap();
        assert_eq!(
            parent.subagents,
            vec![
                PathBuf::from("container.db").join("ses_child.jsonl"),
                PathBuf::from("container.db").join("ses_nested.jsonl"),
            ],
            "a cache hit carries the sub-agent transcripts the row was built from"
        );
    }

    /// The background refresh thread caches a session opened by id with the
    /// same step as the list: its entry joins the root's cache, and the next
    /// load restores it unparsed.
    #[test]
    fn session_loaded_by_id_is_cached_beside_roots_others() {
        let cache_base = tempfile::tempdir().unwrap();
        let storage = VirtualStorage::new(vec![
            virtual_stub("ses_listed", 100, 1_000),
            virtual_stub("ses_by_id", 200, 2_000),
        ]);
        let cache = SessionCacheStore::under(cache_base.path(), storage.cache());
        let loader = SessionLoader {
            storage: &storage,
            cache: &cache,
            show_last: false,
            debug_level: None,
            meter: AgentMeter::UNMETERED,
            clock: &crate::clock::TestClock::default(),
        };
        let root = SessionRoot::new("container.db");
        loader
            .cache_row(&root, &virtual_stub("ses_listed", 100, 1_000))
            .unwrap();

        let by_id = loader
            .cache_row(&root, &virtual_stub("ses_by_id", 200, 2_000))
            .unwrap();

        assert!(by_id.path.ends_with("ses_by_id.jsonl"));
        assert_eq!(storage.parsed_ids(), vec!["ses_by_id", "ses_listed"]);
        let mut keys = cache
            .read(&root.path, loader.clock)
            .into_keys()
            .collect::<Vec<_>>();
        keys.sort();
        assert_eq!(keys, vec!["ses_by_id", "ses_listed"]);

        let warm = load_sessions_with_cache(&storage, &cache, false, None).unwrap();
        assert_eq!(warm.len(), 2);
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
        let cache = SessionCacheStore::under(cache_base.path(), storage.cache());
        let mut reports = Vec::new();

        SessionLoader {
            storage: &storage,
            cache: &cache,
            show_last: false,
            debug_level: None,
            meter: AgentMeter::UNMETERED,
            clock: &crate::clock::TestClock::default(),
        }
        .load(LeftOutRows::NONE, &mut |done, total| {
            reports.push((done, total))
        })
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
        let cache = SessionCacheStore::under(cache_base.path(), storage.cache());

        let loaded = SessionLoader {
            storage: &storage,
            cache: &cache,
            show_last: false,
            debug_level: None,
            meter: AgentMeter::UNMETERED,
            clock: &crate::clock::TestClock::default(),
        }
        .load(LeftOutRows::NONE, &mut |_, _| {})
        .unwrap();

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
        let cache_base = tempfile::tempdir().unwrap();
        let storage = VirtualStorage::new(vec![virtual_stub("ses_first", 100, 1_000)]);
        let cache = SessionCacheStore::under(cache_base.path(), storage.cache());
        let loader = SessionLoader {
            storage: &storage,
            cache: &cache,
            show_last: false,
            debug_level: None,
            meter: AgentMeter::UNMETERED,
            clock: &crate::clock::TestClock::default(),
        };

        loader.load(LeftOutRows::NONE, &mut |_, _| {}).unwrap();

        assert_eq!(
            *storage.superseded_cache_removals.lock().unwrap(),
            vec![(cache_base.path().to_path_buf(), 1)],
            "called once, with the session's shard already written"
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
        loader.load(LeftOutRows::NONE, &mut |_, _| {}).unwrap();
        assert_eq!(
            storage.superseded_cache_removals.lock().unwrap().len(),
            2,
            "a warm load retries superseded-cache cleanup"
        );
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
        let cache = SessionCacheStore::under(base.path(), storage.cache());
        let database = SqliteCache::open(base.path()).unwrap();
        database.mark_admitted().unwrap();
        database.admit_storage(storage.cache()).unwrap();
        drop(database);
        let loaded = load_sessions_with_cache(&storage, &cache, false, None).unwrap();
        assert_eq!(loaded.len(), 13);
        assert_eq!(storage.parse_count(), 13);
        let root = SessionRoot::new("container.db");
        let database = SqliteCache::open_waiting(base.path()).unwrap().unwrap();
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
        let loader = SessionLoader {
            storage: &storage,
            cache: &cache,
            show_last: false,
            debug_level: None,
            meter: AgentMeter::UNMETERED,
            clock: &crate::clock::TestClock::default(),
        };
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
        let base = tempfile::tempdir().unwrap();
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
        let cache = SessionCacheStore::under(base.path(), storage.cache());
        let loader = SessionLoader {
            storage: &storage,
            cache: &cache,
            show_last: false,
            debug_level: None,
            meter: AgentMeter::UNMETERED,
            clock: &crate::clock::TestClock::default(),
        };
        let loaded = loader.load(LeftOutRows::NONE, &mut |_, _| {}).unwrap();
        assert_eq!(
            loaded.ignored,
            vec![FilterTerm::new(
                "Pi",
                "7 ignored: compressed sessions unsupported"
            )]
        );
        assert_eq!(
            sorted_cache_keys(&cache),
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
        let cache = SessionCacheStore::under(base.path(), storage.cache());
        let loader = SessionLoader {
            storage: &storage,
            cache: &cache,
            show_last: false,
            debug_level: None,
            meter: AgentMeter::UNMETERED,
            clock: &crate::clock::TestClock::default(),
        };
        assert!(
            loader.reread(&[&stub.locator], TargetedRead::Caching)[0]
                .value
                .is_none()
        );
    }

    #[test]
    fn losing_ownership_before_discovery_falls_back_without_losing_the_source() {
        use crate::history::sqlite_cache::refresh::tests::replace_owner;
        let base = tempfile::tempdir().unwrap();
        let stub = virtual_stub("ses_readable", 100, 1_000);
        let storage = VirtualStorage::new(vec![stub.clone()]);
        let cache = SessionCacheStore::under(base.path(), storage.cache());
        let loader = SessionLoader {
            storage: &storage,
            cache: &cache,
            show_last: false,
            debug_level: None,
            meter: AgentMeter::UNMETERED,
            clock: &crate::clock::TestClock::default(),
        };
        let ownership = RefreshOwnership::acquire(base.path(), || ()).unwrap();
        let replacement = replace_owner(base.path());
        let Err(read_only) = loader.rediscover_owned(&SessionRoot::new("container.db"), ownership)
        else {
            panic!("the lost ownership was kept");
        };
        assert_eq!(read_only.reason, ReadOnlyReason::WritePermissionRevoked);
        assert_eq!(read_only.rediscovered.unwrap().stubs, vec![stub]);
        assert_eq!(*storage.discoveries.lock().unwrap(), 1);
        replacement.write_batch(|| ()).unwrap();
    }

    #[test]
    fn a_discovery_failing_after_acquisition_loads_the_earlier_discovery_without_writes() {
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
        let cache = SessionCacheStore::under(base.path(), storage.cache());
        let database = SqliteCache::open(base.path()).unwrap();
        database.mark_admitted().unwrap();
        database.admit_storage(storage.cache()).unwrap();
        drop(database);

        let loaded = load_with_cache(&storage, &cache, false, None).unwrap();

        assert_eq!(loaded.conversations.len(), 1);
        assert_eq!(*storage.discoveries.lock().unwrap(), 2);
        assert_eq!(storage.parsed_ids(), ["ses_readable"]);
        let database = SqliteCache::open_waiting(base.path()).unwrap().unwrap();
        assert!(
            database
                .stored_keys(storage.cache(), &SessionRoot::new("container.db").path)
                .unwrap()
                .is_empty(),
            "the load wrote the database without refresh ownership"
        );
        assert!(shard_files_under(base.path()).is_empty());
        assert!(
            matches!(
                RefreshOwnership::try_acquire_now(base.path()).unwrap(),
                Acquisition::Acquired(_)
            ),
            "the load releases refresh ownership"
        );
    }

    #[test]
    fn discovery_commit_failure_preserves_full_and_targeted_reads_without_writes() {
        for targeted in [false, true] {
            let base = tempfile::tempdir().unwrap();
            let old = virtual_stub("ses_readable", 100, 1_000);
            let seeded = VirtualStorage::new(vec![old.clone()]);
            let cache = SessionCacheStore::under(base.path(), seeded.cache());
            load_sessions_with_cache(&seeded, &cache, false, None).unwrap();
            let before = shard_files_under(base.path());
            fail_every_commit_after_acquisition(base.path());
            let fresh = virtual_stub("ses_readable", 200, 2_000);
            let storage = VirtualStorage::new(vec![fresh.clone()]);
            let loader = SessionLoader {
                storage: &storage,
                cache: &cache,
                show_last: false,
                debug_level: None,
                meter: AgentMeter::UNMETERED,
                clock: &crate::clock::TestClock::default(),
            };
            let row = if targeted {
                loader
                    .cache_row(&SessionRoot::new("container.db"), &old)
                    .unwrap()
            } else {
                loader
                    .load(LeftOutRows::NONE, &mut |_, _| {})
                    .unwrap()
                    .conversations
                    .remove(0)
            };
            assert_eq!(row.fingerprint, fresh.fingerprint.stamp());
            assert_eq!(storage.parse_count(), 1);
            assert_eq!(
                *storage.discoveries.lock().unwrap(),
                if targeted { 1 } else { 2 },
                "a completed discovery isn't repeated after its commit fails"
            );
            assert_eq!(shard_files_under(base.path()), before);
        }
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
        use crate::history::sqlite_cache::refresh::tests::OtherProcessOwner;
        let base = tempfile::tempdir().unwrap();
        admit_database(base.path());
        let seeded = VirtualStorage::new(vec![virtual_stub("ses_target", 100, 1_000)]);
        let cache = SessionCacheStore::under(base.path(), seeded.cache());
        load_sessions_with_cache(&seeded, &cache, false, None).unwrap();
        fail_every_commit_after_acquisition(base.path());
        let replacement = std::sync::Arc::new(Mutex::new(None));
        let started = replacement.clone();
        let changed = virtual_stub("ses_target", 200, 2_000);
        let mut storage = VirtualStorage::new(vec![changed.clone()]);
        let replacement_base = base.path().to_path_buf();
        storage.before_parse = Some(Box::new(move |_| {
            *started.lock().unwrap() = Some(OtherProcessOwner::hold(&replacement_base));
        }));
        let loader = SessionLoader {
            storage: &storage,
            cache: &cache,
            show_last: false,
            debug_level: None,
            meter: AgentMeter::UNMETERED,
            clock: &crate::clock::TestClock::default(),
        };

        let read = loader.cache_one(&SessionRoot::new("container.db"), changed);

        assert!(matches!(read.value, SessionRead::Listed(_)));
        assert!(
            replacement.lock().unwrap().is_some(),
            "the replacement refresh did not start"
        );
        assert_eq!(
            read.follow_up,
            Some(CacheFollowUp::AwaitRefresh(RefreshOwner::AnotherProcess)),
            "the row read outside the database waits for the replacement refresh"
        );
    }

    #[test]
    fn a_full_load_behind_exclusive_maintenance_falls_back_after_one_lock_wait() {
        use crate::history::sqlite_cache::refresh::tests::{OtherProcessOwner, OwnerActivity};
        let base = tempfile::tempdir().unwrap();
        let storage = VirtualStorage::new(vec![virtual_stub("ses_blocked", 100, 1_000)]);
        let cache = SessionCacheStore::under(base.path(), storage.cache());
        admit_database(base.path());
        let rebuild = OtherProcessOwner::start(base.path(), OwnerActivity::InsideRebuild);
        let loader = SessionLoader {
            storage: &storage,
            cache: &cache,
            show_last: false,
            debug_level: None,
            meter: AgentMeter::UNMETERED,
            clock: &crate::clock::TestClock::default(),
        };
        let limit = Duration::from_secs(3);
        let (read, _) = mpsc::channel();

        let started = Instant::now();
        let loaded = loader.load_root_waiting(
            &SessionRoot::new("container.db"),
            DiscoveredSessions::complete(storage.stubs.clone()),
            LeftOutRows::NONE,
            &read,
            &|| LockWait::starting_now_for(limit),
        );

        let elapsed = started.elapsed();
        assert_eq!(loaded.conversations.len(), 1);
        assert!(
            elapsed < limit * 2,
            "the load waited {elapsed:?}: a {limit:?} lock wait for the probe and another for acquisition"
        );
        drop(rebuild);
    }

    /// The observations of one [`load_a_changed_shard`].
    struct ChangedShardLoad {
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
        are_shards_unchanged: bool,
        /// Database rows at the fingerprint from before the change.
        unchanged_sessions: i64,
        sessions: i64,
    }

    /// Seed one shard with `READ_BATCH_LENGTH + 1` sessions, change every one
    /// of them, and load them again: through targeted reads or a full load.
    /// With `fail_from`, every refresh owner commit after that many fails,
    /// until the first parse after the first batch drops the failure.
    fn load_a_changed_shard(targeted: bool, fail_from: Option<i64>) -> ChangedShardLoad {
        let base = tempfile::tempdir().unwrap();
        let shard = shard_index("ses_0");
        let keys: Vec<_> = (0..)
            .map(|index| format!("ses_{index}"))
            .filter(|key| shard_index(key) == shard)
            .take(READ_BATCH_LENGTH + 1)
            .collect();
        let old: Vec<_> = keys
            .iter()
            .map(|key| virtual_stub(key, 100, 1_000))
            .collect();
        let seeded = VirtualStorage::new(old.clone());
        let cache = SessionCacheStore::under(base.path(), seeded.cache());
        let database = SqliteCache::open(base.path()).unwrap();
        database.mark_admitted().unwrap();
        database.admit_storage(seeded.cache()).unwrap();
        drop(database);
        load_sessions_with_cache(&seeded, &cache, false, None).unwrap();
        let shards_before = shard_files_under(base.path());
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
        let loader = SessionLoader {
            storage: &storage,
            cache: &cache,
            show_last: false,
            debug_level: None,
            meter: AgentMeter::UNMETERED,
            clock: &crate::clock::TestClock::default(),
        };
        let mut rows = if targeted {
            loader
                .read_stubs(
                    &SessionRoot::new("container.db"),
                    &HashMap::new(),
                    old.clone(),
                    StubsAfterRediscovery::SameFiles,
                )
                .read
                .into_iter()
                .map(|(_, read)| {
                    let SessionRead::Listed(row) = read else {
                        panic!("the readable session disappeared")
                    };
                    *row
                })
                .collect::<Vec<_>>()
        } else {
            loader
                .load(LeftOutRows::NONE, &mut |_, _| {})
                .unwrap()
                .conversations
        };
        rows.sort_by(|left, right| left.path.cmp(&right.path));
        let count = |sql: &str| {
            connection
                .query_row(sql, [], |row| row.get::<_, i64>(0))
                .unwrap()
        };
        ChangedShardLoad {
            rows,
            fresh,
            parses: storage.parse_count(),
            commits_before_first_parse: commits_before_first_parse.lock().unwrap().unwrap(),
            commits: commit_count(&connection),
            is_failure_dropped_without_the_lock: *is_failure_dropped.lock().unwrap(),
            are_shards_unchanged: shard_files_under(base.path()) == shards_before,
            unchanged_sessions: count(
                "SELECT COUNT(*) FROM sessions WHERE file_size = 100 AND mtime_secs = 1000",
            ),
            sessions: count("SELECT COUNT(*) FROM sessions"),
        }
    }

    #[test]
    fn parse_commit_failure_preserves_full_and_targeted_reads_without_later_writes() {
        for targeted in [false, true] {
            let setup_commits = load_a_changed_shard(targeted, None).commits_before_first_parse;

            let load = load_a_changed_shard(targeted, Some(setup_commits));

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
            assert!(load.are_shards_unchanged);
            assert_eq!(
                load.commits, setup_commits,
                "batches stop committing after the failure"
            );
            assert_eq!(load.sessions, (READ_BATCH_LENGTH + 1) as i64);
            assert_eq!(load.unchanged_sessions, load.sessions);
        }
    }

    #[test]
    fn a_sample_doesnt_publish_a_shard_when_its_parse_commit_fails() {
        let base = tempfile::tempdir().unwrap();
        let stub = virtual_stub("ses_sample", 100, 1_000);
        let root = SessionRoot::new("container.db");
        let storage = VirtualStorage::new(vec![stub.clone()]);
        let ownership = RefreshOwnership::acquire(base.path(), || ()).unwrap();
        ownership
            .token()
            .write_batch(|| storage.discover(&root))
            .unwrap()
            .unwrap();
        let connection =
            rusqlite::Connection::open(base.path().join(sqlite_cache::DATABASE_FILE_NAME)).unwrap();
        connection.execute_batch("CREATE TRIGGER fail_commit BEFORE UPDATE OF last_commit_ms ON refresh_state WHEN NEW.owner = OLD.owner BEGIN SELECT RAISE(ABORT, 'injected timestamp failure'); END;").unwrap();
        assert!(
            sample_record(
                &storage,
                &root,
                &stub,
                ownership.token(),
                None,
                AgentMeter::UNMETERED,
                &|| Ok(())
            )
            .is_err()
        );
        assert_eq!(storage.parse_count(), 1);
        assert!(shard_files_under(base.path()).is_empty());
        assert!(!ownership.token().can_publish());
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
        let cache = SessionCacheStore::under(base.path(), storage.cache());
        let result = rediscover_sessions_with_cache(&storage, &cache, false, &|stub| {
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
        assert_eq!(result.read.len(), 1);
        assert_eq!(result.read[0].0, added.locator);
        assert_eq!(storage.parsed_ids(), vec!["ses_added"]);
    }

    #[test]
    fn refresh_reports_fresh_ignored_counts_and_incomplete_directories() {
        let base = tempfile::tempdir().unwrap();
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
        let cache = SessionCacheStore::under(base.path(), storage.cache());
        let result = rediscover_sessions_with_cache(&storage, &cache, false, &|_| true).unwrap();
        assert_eq!(
            result.ignored,
            vec![FilterTerm::new(
                "Pi",
                "7 ignored: compressed sessions unsupported"
            )]
        );
        assert_eq!(result.unreadable_directories, vec![blocked]);
        assert_eq!(
            sorted_cache_keys(&cache),
            ["ses_readable"],
            "the incomplete walk's session is cached"
        );
    }

    #[test]
    fn sessions_that_are_not_files_load_and_cache_through_shared_loop() {
        let cache_base = tempfile::tempdir().unwrap();
        let storage = VirtualStorage::new(vec![
            virtual_stub("ses_first", 100, 1_000),
            virtual_stub("ses_second", 200, 2_000),
        ]);
        let cache = SessionCacheStore::under(cache_base.path(), storage.cache());

        let cold = load_sessions_with_cache(&storage, &cache, false, None).unwrap();
        assert_eq!(cold.len(), 2);
        assert_eq!(storage.parse_count(), 2);
        assert_eq!(
            cold.iter()
                .map(|conversation| conversation.path.clone())
                .collect::<Vec<_>>(),
            vec![
                PathBuf::from("container.db").join("ses_first.jsonl"),
                PathBuf::from("container.db").join("ses_second.jsonl"),
            ],
            "locators reach the conversations untouched"
        );

        let warm = load_sessions_with_cache(&storage, &cache, false, None).unwrap();
        assert_eq!(warm.len(), 2);
        assert_eq!(
            storage.parse_count(),
            2,
            "unchanged fingerprints must restore from the cache, not reparse"
        );

        let mut changed = VirtualStorage::new(vec![
            virtual_stub("ses_first", 100, 1_000),
            virtual_stub("ses_second", 250, 3_000),
        ]);
        changed.parsed = Mutex::new(storage.parsed_ids());
        let after_change = load_sessions_with_cache(&changed, &cache, false, None).unwrap();
        assert_eq!(after_change.len(), 2);
        assert_eq!(
            changed.parse_count(),
            3,
            "only the session whose fingerprint changed is reparsed"
        );
    }

    /// The row the tests of left-out rows leave out: `ses_current`'s.
    fn is_current(row: &Conversation) -> bool {
        row.path.ends_with("ses_current.jsonl")
    }

    #[test]
    fn a_changed_session_whose_cached_row_is_left_out_keeps_that_row_unread() {
        let cache_base = tempfile::tempdir().unwrap();
        let cached = virtual_stub("ses_current", 100, 1_000);
        let storage = VirtualStorage::new(vec![cached.clone(), virtual_stub("ses_other", 50, 500)]);
        let cache = SessionCacheStore::under(cache_base.path(), storage.cache());
        load_sessions_with_cache(&storage, &cache, false, None).unwrap();
        let changed = VirtualStorage::new(vec![
            virtual_stub("ses_current", 150, 2_000),
            virtual_stub("ses_other", 80, 3_000),
        ]);
        let meter = LoadMeter::default();

        let loaded = load_metered_with_cache(
            &changed,
            &cache,
            meter.agent(Source::Pi),
            LeftOutRows::of(&is_current),
        )
        .unwrap();

        assert_eq!(changed.parsed_ids(), ["ses_other"]);
        let row = loaded.conversations.iter().find(|row| is_current(row));
        assert_eq!(
            row.map(|row| row.fingerprint),
            Some(cached.fingerprint.stamp()),
            "the row carries the fingerprint its entry was written for"
        );
        let entry = &cache.read(
            Path::new("container.db"),
            &crate::clock::TestClock::default(),
        )["ses_current"];
        assert_eq!(Some(entry.fingerprint()), cached.fingerprint.stamp());
        assert_eq!(meter.agents()[0].refresh.transcripts, 1);

        load_sessions_with_cache(&changed, &cache, false, None).unwrap();
        assert_eq!(
            changed.parsed_ids(),
            ["ses_current", "ses_other"],
            "the next load that includes the session reads it"
        );
    }

    #[test]
    fn a_load_that_leaves_out_every_changed_session_doesnt_write_a_shard() {
        let cache_base = tempfile::tempdir().unwrap();
        let storage = VirtualStorage::new(vec![virtual_stub("ses_current", 100, 1_000)]);
        let cache = SessionCacheStore::under(cache_base.path(), storage.cache());
        load_sessions_with_cache(&storage, &cache, false, None).unwrap();
        let written = shard_files_under(cache_base.path());
        let changed = VirtualStorage::new(vec![virtual_stub("ses_current", 150, 2_000)]);

        load_metered_with_cache(
            &changed,
            &cache,
            AgentMeter::UNMETERED,
            LeftOutRows::of(&is_current),
        )
        .unwrap();

        assert_eq!(shard_files_under(cache_base.path()), written);
    }

    /// Until the shards are retired, a database without a session's rows is
    /// built from its transcript, whatever the shards hold.
    #[test]
    fn a_session_the_database_doesnt_hold_is_read_although_its_shard_does() {
        for fixture in super::super::contract_tests::registered_fixtures() {
            let home = tempfile::tempdir().unwrap();
            let base = tempfile::tempdir().unwrap();
            fixture.write_session(home.path(), fixture.ids().session);
            let root = fixture.root_under(home.path());
            let storage = fixture.storage().cache();
            SqliteCache::open(base.path())
                .unwrap()
                .mark_admitted()
                .unwrap();
            super::super::contract_tests::load_under(fixture, home.path(), base.path());
            let mut database = SqliteCache::open(base.path()).unwrap();
            let keys = database.stored_keys(storage, &root.path).unwrap();
            assert_eq!(keys.len(), 1);
            database.remove(storage, &root.path, &keys).unwrap();
            drop(database);

            super::super::contract_tests::load_under(fixture, home.path(), base.path());

            let database = SqliteCache::open_waiting(base.path()).unwrap().unwrap();
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
            SqliteCache::open(base.path())
                .unwrap()
                .mark_admitted()
                .unwrap();
            super::super::contract_tests::load_under(fixture, home.path(), base.path());
            let database = SqliteCache::open_waiting(base.path()).unwrap().unwrap();
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
        let cache = SessionCacheStore::under(cache_base.path(), both.cache());
        load_sessions_with_cache(&both, &cache, false, None).unwrap();
        let kept = VirtualStorage::new(vec![virtual_stub("ses_kept", 100, 1_000)])
            .holding_nothing(["ses_kept"]);

        load_sessions_with_cache(&kept, &cache, false, None).unwrap();

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
        let cache = SessionCacheStore::under(base.path(), seeded.cache());
        admit_database(base.path());
        load_sessions_with_cache(&seeded, &cache, false, None).unwrap();
        let connection =
            rusqlite::Connection::open(base.path().join(sqlite_cache::DATABASE_FILE_NAME)).unwrap();
        connection.execute_batch("CREATE TABLE refresh_commits (count INTEGER); INSERT INTO refresh_commits VALUES (0);
            CREATE TRIGGER count_commits AFTER UPDATE OF last_commit_ms ON refresh_state WHEN NEW.owner = OLD.owner BEGIN UPDATE refresh_commits SET count = count + 1; END;").unwrap();
        let mut changed = cached;
        changed[0] = virtual_stub("ses_0", 200, 2_000);
        let mut storage = VirtualStorage::new(changed);
        storage.holding_nothing = seeded.holding_nothing;

        load_sessions_with_cache(&storage, &cache, false, None).unwrap();

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
        let cache = SessionCacheStore::under(base.path(), seeded.cache());
        admit_database(base.path());
        load_sessions_with_cache(&seeded, &cache, false, None).unwrap();
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

        load_sessions_with_cache(&partial, &cache, false, None).unwrap();

        let database = SqliteCache::open_waiting(base.path()).unwrap().unwrap();
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
        assert_eq!(
            sorted_cache_keys(&cache),
            ["ses_added", "ses_changed", "ses_hidden"]
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
        let cache = SessionCacheStore::under(base.path(), storage.cache());
        admit_database(base.path());
        let loader = SessionLoader {
            storage: &storage,
            cache: &cache,
            show_last: false,
            debug_level: None,
            meter: AgentMeter::UNMETERED,
            clock: &crate::clock::TestClock::default(),
        };

        let read = loader.cache_one(&SessionRoot::new("container.db"), added);

        assert!(matches!(read.value, SessionRead::Empty));
        assert_eq!(stored_keys_under(base.path(), &storage), ["ses_added"]);
        assert_eq!(sorted_cache_keys(&cache), ["ses_added"]);
    }

    /// Complete disk admission of `base`'s database, as a first build does.
    fn admit_database(base: &Path) {
        SqliteCache::open(base).unwrap().mark_admitted().unwrap();
    }

    /// The cache keys `base`'s database stores for `storage`'s root.
    fn stored_keys_under(base: &Path, storage: &VirtualStorage) -> Vec<String> {
        SqliteCache::open_waiting(base)
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
        let cache = SessionCacheStore::under(base.path(), storage.cache());
        load_sessions_with_cache(&storage, &cache, false, None).unwrap();
        // An unfinished refresh is absent from current_fingerprints.
        let connection =
            rusqlite::Connection::open(base.path().join(sqlite_cache::DATABASE_FILE_NAME)).unwrap();
        connection
            .execute("UPDATE sessions SET status='writing'", [])
            .unwrap();
        let empty = VirtualStorage::new(Vec::new());

        load_sessions_with_cache(&empty, &cache, false, None).unwrap();

        assert!(stored_keys_under(base.path(), &storage).is_empty());
    }

    #[test]
    fn an_empty_root_doesnt_create_a_database() {
        let base = tempfile::tempdir().unwrap();
        let storage = VirtualStorage::new(Vec::new());
        let cache = SessionCacheStore::under(base.path(), storage.cache());
        load_sessions_with_cache(&storage, &cache, false, None).unwrap();
        assert!(!base.path().join(sqlite_cache::DATABASE_FILE_NAME).exists());
    }

    #[test]
    fn a_partial_discovery_doesnt_delete_unseen_database_sessions() {
        let base = tempfile::tempdir().unwrap();
        let storage = VirtualStorage::new(vec![virtual_stub("ses_hidden", 50, 500)])
            .holding_nothing(["ses_hidden"]);
        admit_database(base.path());
        let cache = SessionCacheStore::under(base.path(), storage.cache());
        load_sessions_with_cache(&storage, &cache, false, None).unwrap();
        let mut partial = DiscoveredSessions::complete(Vec::new());
        partial
            .unreadable_directories
            .push(super::super::storage::UnreadableDirectory {
                path: PathBuf::from("container.db"),
                error: "temporarily unavailable".to_owned(),
            });
        let loader = SessionLoader {
            storage: &storage,
            cache: &cache,
            show_last: false,
            debug_level: None,
            meter: AgentMeter::UNMETERED,
            clock: &crate::clock::TestClock::default(),
        };

        loader.load_discovered(
            vec![(SessionRoot::new("container.db"), partial)],
            LeftOutRows::NONE,
            &mpsc::channel().0,
        );

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
        SqliteCache::open(cache_base.path())
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
        let written = SqliteCache::open(cache_base.path())
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
    fn a_session_without_a_cache_entry_is_read_even_when_its_row_is_left_out() {
        let cache_base = tempfile::tempdir().unwrap();
        let storage = VirtualStorage::new(vec![virtual_stub("ses_current", 100, 1_000)]);
        let cache = SessionCacheStore::under(cache_base.path(), storage.cache());

        let loaded = load_metered_with_cache(
            &storage,
            &cache,
            AgentMeter::UNMETERED,
            LeftOutRows::of(&is_current),
        )
        .unwrap();

        assert_eq!(storage.parsed_ids(), ["ses_current"]);
        assert_eq!(loaded.conversations.len(), 1);
    }

    #[test]
    fn a_metered_load_records_what_discovery_the_cache_and_the_refresh_read() {
        let cache_base = tempfile::tempdir().unwrap();
        let mut with_subagent = virtual_stub("ses_parent", 100, 1_000);
        with_subagent.subagents = vec![PathBuf::from("container.db").join("ses_child.jsonl")];
        let storage = VirtualStorage::new(vec![with_subagent, virtual_stub("ses_other", 50, 500)]);
        let cache = SessionCacheStore::under(cache_base.path(), storage.cache());
        let load = || {
            let meter = LoadMeter::default();
            load_metered_with_cache(&storage, &cache, meter.agent(Source::Pi), LeftOutRows::NONE)
                .unwrap();
            let [agent] = meter.agents()[..] else {
                panic!("one record per agent: {:?}", meter.agents());
            };
            (agent, meter.progress())
        };

        let (cold, cold_progress) = load();
        let (warm, _) = load();

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
        assert_eq!(cold.cache.files, 0, "nothing was cached yet");
        assert_eq!(warm.refresh.transcripts, 0);
        assert_eq!(warm.cache.entries, 2);
        assert!(warm.cache.files > 0 && warm.cache.bytes > 0, "{warm:?}");
    }

    /// Shard files by path, each with its bytes and modification time.
    type ShardFiles = BTreeMap<PathBuf, (Vec<u8>, SystemTime)>;

    /// Every shard file under `base`, so a test can tell which shards a load
    /// rewrote.
    fn shard_files_under(base: &std::path::Path) -> ShardFiles {
        fn collect(directory: &std::path::Path, found: &mut ShardFiles) {
            let Ok(entries) = std::fs::read_dir(directory) else {
                return;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    collect(&path, found);
                } else if path
                    .file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with("shard-"))
                {
                    let bytes = std::fs::read(&path).unwrap();
                    let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
                    found.insert(path, (bytes, modified));
                }
            }
        }
        let mut found = BTreeMap::new();
        collect(base, &mut found);
        found
    }

    fn shard_file_of_session(base: &std::path::Path, cache_key: &str) -> PathBuf {
        let wanted = format!("shard-{:02}.bin", shard_index(cache_key));
        shard_files_under(base)
            .into_keys()
            .find(|path| path.file_name().is_some_and(|name| name == wanted.as_str()))
            .expect("the load wrote the session's shard")
    }

    /// `shard` was created or rewritten between `before` and `after`; every
    /// other shard file is unchanged.
    fn assert_only_this_shard_rewritten(
        mut before: ShardFiles,
        mut after: ShardFiles,
        shard: &std::path::Path,
    ) {
        assert_ne!(
            before.get(shard),
            after.get(shard),
            "{} was not rewritten",
            shard.display()
        );
        before.remove(shard);
        after.remove(shard);
        assert_eq!(
            after,
            before,
            "a shard other than {} changed",
            shard.display()
        );
    }

    /// `count` stubs under the default root whose cache keys each hash to a
    /// different shard, sized and dated from their index.
    fn stubs_in_distinct_shards(count: usize) -> Vec<SessionStub> {
        keys_in_distinct_shards(count)
            .iter()
            .enumerate()
            .map(|(index, key)| {
                let ordinal = index as u64 + 1;
                virtual_stub(key, 100 * ordinal, 1_000 * ordinal)
            })
            .collect()
    }

    fn sorted_cache_keys(cache: &SessionCacheStore) -> Vec<String> {
        let mut keys = cache
            .read(
                std::path::Path::new("container.db"),
                &crate::clock::TestClock::default(),
            )
            .into_keys()
            .collect::<Vec<_>>();
        keys.sort();
        keys
    }

    #[test]
    fn load_with_no_session_changed_doesnt_rewrite_any_shard() {
        let cache_base = tempfile::tempdir().unwrap();
        let storage = VirtualStorage::new(stubs_in_distinct_shards(2));
        let cache = SessionCacheStore::under(cache_base.path(), storage.cache());
        load_sessions_with_cache(&storage, &cache, false, None).unwrap();
        let written = shard_files_under(cache_base.path());
        assert_eq!(written.len(), 2, "one shard per session");

        load_sessions_with_cache(&storage, &cache, false, None).unwrap();

        assert_eq!(shard_files_under(cache_base.path()), written);
    }

    #[test]
    fn load_after_transcript_changed_rewrites_its_shard_and_no_other() {
        let cache_base = tempfile::tempdir().unwrap();
        let stubs = stubs_in_distinct_shards(2);
        let storage = VirtualStorage::new(stubs.clone());
        let cache = SessionCacheStore::under(cache_base.path(), storage.cache());
        load_sessions_with_cache(&storage, &cache, false, None).unwrap();
        let written = shard_files_under(cache_base.path());
        let changed_shard = shard_file_of_session(cache_base.path(), &stubs[1].cache_key);

        let changed = VirtualStorage::new(vec![
            stubs[0].clone(),
            virtual_stub(&stubs[1].cache_key, 250, 3_000),
        ]);
        load_sessions_with_cache(&changed, &cache, false, None).unwrap();

        assert_only_this_shard_rewritten(
            written,
            shard_files_under(cache_base.path()),
            &changed_shard,
        );
        assert_eq!(
            cache
                .read(
                    std::path::Path::new("container.db"),
                    &crate::clock::TestClock::default(),
                )
                .get(&stubs[1].cache_key)
                .map(|entry| entry.fingerprint().file_size),
            Some(250)
        );
    }

    #[test]
    fn load_after_shard_was_corrupted_reparses_its_sessions_and_rewrites_it_alone() {
        let cache_base = tempfile::tempdir().unwrap();
        let stubs = stubs_in_distinct_shards(2);
        let corrupted = &stubs[1].cache_key;
        let storage = VirtualStorage::new(stubs.clone());
        let cache = SessionCacheStore::under(cache_base.path(), storage.cache());
        load_sessions_with_cache(&storage, &cache, false, None).unwrap();
        let corrupted_shard = shard_file_of_session(cache_base.path(), corrupted);
        std::fs::write(&corrupted_shard, b"not a valid shard").unwrap();
        let written = shard_files_under(cache_base.path());

        let second = VirtualStorage::new(stubs.clone());
        let listed = load_sessions_with_cache(&second, &cache, false, None).unwrap();

        assert_eq!(listed.len(), 2, "every session is listed");
        assert_eq!(second.parsed_ids(), vec![corrupted.clone()]);
        assert_only_this_shard_rewritten(
            written,
            shard_files_under(cache_base.path()),
            &corrupted_shard,
        );
    }

    #[test]
    fn load_after_transcript_was_deleted_rewrites_its_shard_without_it() {
        let cache_base = tempfile::tempdir().unwrap();
        let stubs = stubs_in_distinct_shards(2);
        let (kept, gone) = (&stubs[0].cache_key, &stubs[1].cache_key);
        let storage = VirtualStorage::new(stubs.clone());
        let cache = SessionCacheStore::under(cache_base.path(), storage.cache());
        load_sessions_with_cache(&storage, &cache, false, None).unwrap();
        let written = shard_files_under(cache_base.path());
        let gone_shard = shard_file_of_session(cache_base.path(), gone);

        let after_delete = VirtualStorage::new(vec![stubs[0].clone()]);
        load_sessions_with_cache(&after_delete, &cache, false, None).unwrap();

        assert_only_this_shard_rewritten(
            written,
            shard_files_under(cache_base.path()),
            &gone_shard,
        );
        assert_eq!(sorted_cache_keys(&cache), vec![kept.clone()]);
        assert!(
            after_delete.parsed_ids().is_empty(),
            "the kept session restores from the cache"
        );
    }

    /// Two cache keys that hash to the same shard.
    fn keys_sharing_a_shard() -> (String, String) {
        let candidates: Vec<String> = (0..=cache::SHARD_COUNT)
            .map(|number| format!("session-{number}"))
            .collect();
        candidates
            .iter()
            .enumerate()
            .find_map(|(index, first)| {
                candidates[index + 1..]
                    .iter()
                    .find(|second| shard_index(second) == shard_index(first))
                    .map(|second| (first.clone(), second.clone()))
            })
            .expect("one key more than there are shards puts two in one shard")
    }

    #[test]
    fn load_after_transcript_was_deleted_keeps_restored_entry_sharing_its_shard() {
        let cache_base = tempfile::tempdir().unwrap();
        let (kept, gone) = keys_sharing_a_shard();
        let elsewhere = keys_in_distinct_shards(2)
            .into_iter()
            .find(|key| shard_index(key) != shard_index(&kept))
            .unwrap();
        let stubs = vec![
            virtual_stub(&kept, 100, 1_000),
            virtual_stub(&gone, 200, 2_000),
            virtual_stub(&elsewhere, 300, 3_000),
        ];
        let storage = VirtualStorage::new(stubs.clone());
        let cache = SessionCacheStore::under(cache_base.path(), storage.cache());
        load_sessions_with_cache(&storage, &cache, false, None).unwrap();
        let written = shard_files_under(cache_base.path());
        let shared_shard = shard_file_of_session(cache_base.path(), &gone);

        let after_delete = VirtualStorage::new(vec![stubs[0].clone(), stubs[2].clone()]);
        let listed = load_sessions_with_cache(&after_delete, &cache, false, None).unwrap();

        assert_eq!(listed.len(), 2);
        assert!(
            after_delete.parsed_ids().is_empty(),
            "both remaining sessions restore from the cache"
        );
        assert_only_this_shard_rewritten(
            written,
            shard_files_under(cache_base.path()),
            &shared_shard,
        );
        let mut expected = vec![kept, elsewhere];
        expected.sort();
        assert_eq!(sorted_cache_keys(&cache), expected);
    }

    #[test]
    fn load_with_new_transcript_keeps_restored_entry_sharing_its_shard() {
        let cache_base = tempfile::tempdir().unwrap();
        let (kept, new) = keys_sharing_a_shard();
        let kept_stub = virtual_stub(&kept, 100, 1_000);
        let storage = VirtualStorage::new(vec![kept_stub.clone()]);
        let cache = SessionCacheStore::under(cache_base.path(), storage.cache());
        load_sessions_with_cache(&storage, &cache, false, None).unwrap();
        let written = shard_files_under(cache_base.path());
        let shared_shard = shard_file_of_session(cache_base.path(), &kept);

        let with_new = VirtualStorage::new(vec![kept_stub, virtual_stub(&new, 200, 2_000)]);
        load_sessions_with_cache(&with_new, &cache, false, None).unwrap();

        assert_eq!(with_new.parsed_ids(), vec![new.clone()]);
        assert_only_this_shard_rewritten(
            written,
            shard_files_under(cache_base.path()),
            &shared_shard,
        );
        let mut expected = vec![kept, new];
        expected.sort();
        assert_eq!(sorted_cache_keys(&cache), expected);
    }

    /// Everything a restored row carries from its cache entry: the cached
    /// conversation as the cache serializes it, and the entry's fields beside it.
    fn restored_fields(conversation: &Conversation) -> impl PartialEq + std::fmt::Debug {
        (
            serde_json::to_string(&crate::history::cache::cached_conversation(conversation))
                .unwrap(),
            conversation.session_id.clone(),
            conversation.preview.clone(),
            conversation.project_name.clone(),
            conversation.project_path.clone(),
            conversation.subagents.clone(),
            conversation.fingerprint,
        )
    }

    #[test]
    fn warm_load_restores_rows_cold_load_listed_and_writes_no_shard() {
        let cache_base = tempfile::tempdir().unwrap();
        let (shared, sharing) = keys_sharing_a_shard();
        let mut stubs: Vec<SessionStub> = keys_in_distinct_shards(5)
            .into_iter()
            .filter(|key| *key != shared && *key != sharing)
            .take(3)
            .zip(1..)
            .map(|(key, ordinal)| virtual_stub(&key, 100 * ordinal, 1_000 * ordinal))
            .collect();
        stubs.push(virtual_stub(&shared, 400, 4_000));
        stubs.push(virtual_stub(&sharing, 500, 5_000));
        let cold = VirtualStorage::new(stubs.clone());
        let cache = SessionCacheStore::under(cache_base.path(), cold.cache());
        let mut listed = load_sessions_with_cache(&cold, &cache, false, None).unwrap();
        let written = shard_files_under(cache_base.path());

        let warm = VirtualStorage::new(stubs);
        let mut restored = load_sessions_with_cache(&warm, &cache, false, None).unwrap();

        assert!(warm.parsed_ids().is_empty(), "every session restores");
        assert_eq!(shard_files_under(cache_base.path()), written);
        listed.sort_by(|a, b| a.session_id.cmp(&b.session_id));
        restored.sort_by(|a, b| a.session_id.cmp(&b.session_id));
        assert_eq!(
            restored.iter().map(restored_fields).collect::<Vec<_>>(),
            listed.iter().map(restored_fields).collect::<Vec<_>>()
        );
    }

    #[test]
    fn session_restored_from_rewritten_shard_still_shows_its_external_title() {
        let cache_base = tempfile::tempdir().unwrap();
        let (titled, gone) = keys_sharing_a_shard();
        let titled_stub = virtual_stub(&titled, 100, 1_000);
        let gone_stub = virtual_stub(&gone, 200, 2_000);
        let title = "Renamed beside the transcript";
        let storage = VirtualStorage::new(vec![titled_stub.clone(), gone_stub])
            .with_external_title(&titled_stub, title);
        let cache = SessionCacheStore::under(cache_base.path(), storage.cache());
        load_sessions_with_cache(&storage, &cache, false, None).unwrap();
        let written = shard_files_under(cache_base.path());
        let shared_shard = shard_file_of_session(cache_base.path(), &titled);

        let after_delete =
            VirtualStorage::new(vec![titled_stub.clone()]).with_external_title(&titled_stub, title);
        load_sessions_with_cache(&after_delete, &cache, false, None).unwrap();
        assert_only_this_shard_rewritten(
            written,
            shard_files_under(cache_base.path()),
            &shared_shard,
        );

        let next =
            VirtualStorage::new(vec![titled_stub.clone()]).with_external_title(&titled_stub, title);
        let listed = load_sessions_with_cache(&next, &cache, false, None).unwrap();

        assert!(
            next.parsed_ids().is_empty(),
            "the titled session restores from the rewritten shard"
        );
        assert_eq!(listed[0].custom_title.as_deref(), Some(title));
    }

    #[test]
    fn load_with_new_transcript_writes_its_shard_and_no_other() {
        let cache_base = tempfile::tempdir().unwrap();
        let stubs = stubs_in_distinct_shards(2);
        let (first, new) = (&stubs[0].cache_key, &stubs[1].cache_key);
        let storage = VirtualStorage::new(vec![stubs[0].clone()]);
        let cache = SessionCacheStore::under(cache_base.path(), storage.cache());
        load_sessions_with_cache(&storage, &cache, false, None).unwrap();
        let written = shard_files_under(cache_base.path());

        let with_new = VirtualStorage::new(stubs.clone());
        load_sessions_with_cache(&with_new, &cache, false, None).unwrap();

        assert_only_this_shard_rewritten(
            written,
            shard_files_under(cache_base.path()),
            &shard_file_of_session(cache_base.path(), new),
        );
        let mut expected = vec![first.clone(), new.clone()];
        expected.sort();
        assert_eq!(sorted_cache_keys(&cache), expected);
        assert_eq!(with_new.parsed_ids(), vec![new.clone()]);
    }

    /// Caching a session opened by ID writes that session's shard and leaves
    /// the root's other shards as they were.
    #[test]
    fn session_loaded_by_id_writes_its_shard_and_no_other() {
        let cache_base = tempfile::tempdir().unwrap();
        let stubs = stubs_in_distinct_shards(2);
        let (listed, by_id) = (&stubs[0], &stubs[1]);
        let storage = VirtualStorage::new(stubs.clone());
        let cache = SessionCacheStore::under(cache_base.path(), storage.cache());
        let loader = SessionLoader {
            storage: &storage,
            cache: &cache,
            show_last: false,
            debug_level: None,
            meter: AgentMeter::UNMETERED,
            clock: &crate::clock::TestClock::default(),
        };
        let root = SessionRoot::new("container.db");
        loader.cache_row(&root, listed).unwrap();
        let written = shard_files_under(cache_base.path());

        loader.cache_row(&root, by_id);

        assert_only_this_shard_rewritten(
            written,
            shard_files_under(cache_base.path()),
            &shard_file_of_session(cache_base.path(), &by_id.cache_key),
        );
    }

    /// Opening by ID, or after a rename, decodes only the session's own
    /// shard, and a session cached there is restored, not parsed.
    #[test]
    fn session_loaded_by_id_reads_its_shard_and_no_other() {
        let cache_base = tempfile::tempdir().unwrap();
        let stubs = stubs_in_distinct_shards(2);
        let by_id = &stubs[1];
        let storage = VirtualStorage::new(stubs.clone());
        let cache = SessionCacheStore::under(cache_base.path(), storage.cache());
        let loader = SessionLoader {
            storage: &storage,
            cache: &cache,
            show_last: false,
            debug_level: None,
            meter: AgentMeter::UNMETERED,
            clock: &crate::clock::TestClock::default(),
        };
        let root = SessionRoot::new("container.db");
        loader.load_root(
            &root,
            DiscoveredSessions::complete(stubs.clone()),
            LeftOutRows::NONE,
            &mpsc::channel().0,
        );
        let parsed_by_the_load = storage.parsed_ids().len();

        let shard = cache.read_shard(&root.path, shard_index(&by_id.cache_key), loader.clock);
        loader.load_one(&root, by_id).unwrap();

        assert_eq!(shard.keys().collect::<Vec<_>>(), [&by_id.cache_key]);
        assert_eq!(
            storage.parsed_ids().len(),
            parsed_by_the_load,
            "the session is restored from its shard"
        );
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
        let cache = SessionCacheStore::under(cache_base.path(), storage.cache());
        let loader = SessionLoader {
            storage: &storage,
            cache: &cache,
            show_last: false,
            debug_level: None,
            meter: AgentMeter::UNMETERED,
            clock: &crate::clock::TestClock::default(),
        };

        let loaded = loader.load(LeftOutRows::NONE, &mut |_, _| {}).unwrap();

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
    fn rediscovery_reads_only_sessions_it_picks_and_rewrites_only_their_shards() {
        let cache_base = tempfile::tempdir().unwrap();
        let stubs = stubs_in_distinct_shards(3);
        let (unchanged, changed, added) = (&stubs[0], &stubs[1], &stubs[2]);
        let storage = VirtualStorage::new(vec![unchanged.clone(), changed.clone()]);
        let cache = SessionCacheStore::under(cache_base.path(), storage.cache());
        load_sessions_with_cache(&storage, &cache, false, None).unwrap();
        let unchanged_shard = shard_file_of_session(cache_base.path(), &unchanged.cache_key);
        let unchanged_bytes = shard_files_under(cache_base.path())[&unchanged_shard].clone();

        let grown = virtual_stub(&changed.cache_key, 250, 9_000);
        let now = VirtualStorage::new(vec![unchanged.clone(), grown.clone(), added.clone()]);
        let rediscovered = rediscover_sessions_with_cache(&now, &cache, false, &|stub| {
            stub.cache_key != unchanged.cache_key
        })
        .unwrap();

        assert_eq!(rediscovered.found.len(), 3, "every session is reported");
        let mut read = rediscovered
            .read
            .iter()
            .map(|(locator, read)| {
                assert!(matches!(read, SessionRead::Listed(_)));
                locator.clone()
            })
            .collect::<Vec<_>>();
        read.sort();
        let mut expected = vec![grown.locator.clone(), added.locator.clone()];
        expected.sort();
        assert_eq!(read, expected);
        let mut parsed = vec![grown.cache_key.clone(), added.cache_key.clone()];
        parsed.sort();
        assert_eq!(now.parsed_ids(), parsed);
        assert_eq!(
            shard_files_under(cache_base.path())[&unchanged_shard],
            unchanged_bytes,
            "the unchanged session's shard is not rewritten"
        );
        assert_eq!(
            cache
                .read(
                    std::path::Path::new("container.db"),
                    &crate::clock::TestClock::default(),
                )
                .get(&grown.cache_key)
                .map(|entry| entry.fingerprint().file_size),
            Some(250)
        );
    }

    #[test]
    fn session_read_again_previews_its_last_messages_when_asked() {
        let cache_base = tempfile::tempdir().unwrap();
        let stub = virtual_stub("ses_reread", 100, 1_000);
        let storage = VirtualStorage::new(vec![stub.clone()]);
        let cache = SessionCacheStore::under(cache_base.path(), storage.cache());

        let preview_of =
            |show_last| match cache_session_with_cache(&storage, &cache, &stub.locator, show_last)
                .value
            {
                Some(SessionRead::Listed(read)) => read.preview,
                _ => panic!("the session was not read as a listed session"),
            };

        assert_eq!(preview_of(true), "closing", "parsed");
        assert_eq!(preview_of(true), "closing", "restored from the cache");
        assert_eq!(preview_of(false), "opening");
    }

    #[test]
    fn sessions_read_again_together_share_discovery_before_and_after_acquiring_ownership() {
        let cache_base = tempfile::tempdir().unwrap();
        let first = virtual_stub("ses_first", 100, 1_000);
        let second = virtual_stub("ses_second", 100, 2_000);
        let storage = VirtualStorage::new(vec![first.clone(), second.clone()]);
        let cache = SessionCacheStore::under(cache_base.path(), storage.cache());

        let reads = SessionLoader {
            storage: &storage,
            cache: &cache,
            show_last: false,
            debug_level: None,
            meter: AgentMeter::UNMETERED,
            clock: &crate::clock::TestClock::default(),
        }
        .reread(&[&second.locator, &first.locator], TargetedRead::Caching);

        assert_eq!(
            *storage.discoveries.lock().unwrap(),
            2,
            "one discovery selects both requests; one refreshes both after acquiring ownership"
        );
        assert!(reads.iter().all(|read| read.follow_up.is_none()));
        let paths: Vec<_> = reads
            .iter()
            .map(|read| match &read.value {
                Some(SessionRead::Listed(read)) => read.path.clone(),
                _ => panic!("a session was not read as a listed session"),
            })
            .collect();
        assert_eq!(paths, [second.locator, first.locator], "in the order asked");
    }

    #[test]
    fn rediscovered_new_session_previews_its_last_messages_when_asked() {
        let cache_base = tempfile::tempdir().unwrap();
        let storage = VirtualStorage::new(vec![virtual_stub("ses_new", 100, 1_000)]);
        let cache = SessionCacheStore::under(cache_base.path(), storage.cache());

        let rediscovered =
            rediscover_sessions_with_cache(&storage, &cache, true, &|_| true).unwrap();

        let [(_, SessionRead::Listed(read))] = rediscovered.read.as_slice() else {
            panic!("the new session was not read as a listed session");
        };
        assert_eq!(read.preview, "closing");
    }

    /// Another process, such as `agent search`, can cache a session before a
    /// refresh picks it.
    #[test]
    fn rediscovered_session_cache_already_holds_is_restored_not_parsed() {
        let cache_base = tempfile::tempdir().unwrap();
        let storage = VirtualStorage::new(vec![virtual_stub("ses_cached", 100, 1_000)]);
        let cache = SessionCacheStore::under(cache_base.path(), storage.cache());
        load_sessions_with_cache(&storage, &cache, false, None).unwrap();
        let written = shard_files_under(cache_base.path());

        let again = VirtualStorage::new(vec![virtual_stub("ses_cached", 100, 1_000)]);
        let rediscovered =
            rediscover_sessions_with_cache(&again, &cache, false, &|_| true).unwrap();

        assert!(matches!(
            rediscovered.read.as_slice(),
            [(_, SessionRead::Listed(_))]
        ));
        assert_eq!(again.parse_count(), 0);
        assert_eq!(shard_files_under(cache_base.path()), written);
    }

    /// Without a record of it, a transcript that holds no conversation is read
    /// in full on every load and contributes nothing. Most of a Codex corpus is
    /// sub-agent threads whose own content is empty, so this is the bulk of a
    /// warm load.
    #[test]
    fn session_that_holds_no_conversation_is_read_once() {
        let cache_base = tempfile::tempdir().unwrap();
        let storage = VirtualStorage::new(vec![
            virtual_stub("ses_listed", 100, 1_000),
            virtual_stub("ses_empty", 200, 2_000),
        ])
        .holding_nothing(["ses_empty"]);
        let cache = SessionCacheStore::under(cache_base.path(), storage.cache());

        let cold = load_sessions_with_cache(&storage, &cache, false, None).unwrap();
        assert_eq!(cold.len(), 1, "the empty session is not listed");
        assert_eq!(storage.parsed_ids(), vec!["ses_empty", "ses_listed"]);

        let warm = load_sessions_with_cache(&storage, &cache, false, None).unwrap();
        assert_eq!(warm.len(), 1, "and it is still not listed");
        assert_eq!(
            storage.parsed_ids(),
            vec!["ses_empty", "ses_listed"],
            "neither session is read a second time"
        );
    }

    /// The record stands for the transcript's content, so it lasts exactly as
    /// long as the fingerprint does.
    #[test]
    fn session_that_gains_content_is_read_again() {
        let cache_base = tempfile::tempdir().unwrap();
        let empty = VirtualStorage::new(vec![virtual_stub("ses_grows", 100, 1_000)])
            .holding_nothing(["ses_grows"]);
        let cache = SessionCacheStore::under(cache_base.path(), empty.cache());
        assert!(
            load_sessions_with_cache(&empty, &cache, false, None)
                .unwrap()
                .is_empty()
        );

        let grown = VirtualStorage::new(vec![virtual_stub("ses_grows", 400, 2_000)]);
        let listed = load_sessions_with_cache(&grown, &cache, false, None).unwrap();

        assert_eq!(listed.len(), 1);
        assert_eq!(grown.parsed_ids(), vec!["ses_grows"]);
    }

    /// A read can fail for a reason outside the transcript — a file held open,
    /// a partial write. Recording that as empty would hide the session until it
    /// changed on disk.
    #[test]
    fn session_that_could_not_be_read_is_not_recorded_as_empty() {
        let cache_base = tempfile::tempdir().unwrap();
        let failing = VirtualStorage::new(vec![virtual_stub("ses_locked", 100, 1_000)])
            .unreadable(["ses_locked"]);
        let cache = SessionCacheStore::under(cache_base.path(), failing.cache());
        assert!(
            load_sessions_with_cache(&failing, &cache, false, None)
                .unwrap()
                .is_empty()
        );
        assert_eq!(failing.parsed_ids(), vec!["ses_locked"]);

        let readable = VirtualStorage::new(vec![virtual_stub("ses_locked", 100, 1_000)]);
        let listed = load_sessions_with_cache(&readable, &cache, false, None).unwrap();

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
        let cache = SessionCacheStore::under(cache_base.path(), storage.cache());
        let loader = SessionLoader {
            storage: &storage,
            cache: &cache,
            show_last: false,
            debug_level: None,
            meter: AgentMeter::UNMETERED,
            clock: &crate::clock::TestClock::default(),
        };

        let rediscovered = loader.rediscover(&|_| true).unwrap();

        let read_of = |id: &str| {
            rediscovered
                .read
                .iter()
                .find(|(locator, _)| locator.ends_with(format!("{id}.jsonl")))
                .map(|(_, read)| read)
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
        let cache_base = tempfile::tempdir().unwrap();
        let busy = VirtualStorage::new(vec![stub_with_subagents("ses_parent", 1_000, 1)])
            .failing_transiently(["ses_parent_0"]);
        let cache = SessionCacheStore::under(cache_base.path(), busy.cache());

        let listed = load_sessions_with_cache(&busy, &cache, false, None).unwrap();
        load_sessions_with_cache(&busy, &cache, false, None).unwrap();

        assert_eq!(
            listed[0].total_tokens, 10,
            "the sub-agent's tokens are missing"
        );
        assert!(listed[0].has_transient_read_error);
        assert!(
            cache
                .read(
                    Path::new("container.db"),
                    &crate::clock::TestClock::default()
                )
                .is_empty()
        );
        assert_eq!(busy.parse_count(), 2, "the next load parses it again");
    }

    #[test]
    fn session_is_cached_once_its_subagent_reads() {
        let cache_base = tempfile::tempdir().unwrap();
        let stubs = vec![stub_with_subagents("ses_parent", 1_000, 1)];
        let busy = VirtualStorage::new(stubs.clone()).failing_transiently(["ses_parent_0"]);
        let cache = SessionCacheStore::under(cache_base.path(), busy.cache());
        load_sessions_with_cache(&busy, &cache, false, None).unwrap();
        let readable = VirtualStorage::new(stubs);

        let listed = load_sessions_with_cache(&readable, &cache, false, None).unwrap();
        load_sessions_with_cache(&readable, &cache, false, None).unwrap();

        assert_eq!(
            listed[0].total_tokens, 20,
            "the sub-agent's tokens are merged"
        );
        assert!(!listed[0].has_transient_read_error);
        assert_eq!(readable.parse_count(), 1, "the next load restores it");
    }

    #[test]
    fn session_whose_subagent_hits_permanent_error_is_cached_without_it() {
        let cache_base = tempfile::tempdir().unwrap();
        let broken = VirtualStorage::new(vec![stub_with_subagents("ses_parent", 1_000, 1)])
            .unreadable(["ses_parent_0"]);
        let cache = SessionCacheStore::under(cache_base.path(), broken.cache());

        let listed = load_sessions_with_cache(&broken, &cache, false, None).unwrap();
        load_sessions_with_cache(&broken, &cache, false, None).unwrap();

        assert_eq!(
            listed[0].total_tokens, 10,
            "the sub-agent's tokens are missing"
        );
        assert!(!listed[0].has_transient_read_error);
        assert_eq!(broken.parse_count(), 1, "the next load restores it");
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
            .load(LeftOutRows::NONE, &mut |done, total| {
                reports.push((done, total))
            })
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
        let cache = SessionCacheStore::under(cache_base.path(), storage.cache());
        let loader = SessionLoader {
            storage: &storage,
            cache: &cache,
            show_last: false,
            debug_level: None,
            meter: AgentMeter::UNMETERED,
            clock: &crate::clock::TestClock::default(),
        };

        let reports = progress_of_load(&loader);

        assert_eq!(reports, vec![(0, 4), (1, 4), (2, 4), (3, 4), (4, 4)]);
    }

    /// A session restored from the cache, or recorded as holding no
    /// conversation, reads none of its transcripts; the count still reaches
    /// the total.
    #[test]
    fn progress_reaches_total_when_sessions_are_not_read() {
        let cache_base = tempfile::tempdir().unwrap();
        let storage = VirtualStorage::new(vec![
            stub_with_subagents("ses_parent", 1_000, 2),
            virtual_stub("ses_empty", 5_000, 500),
        ])
        .holding_nothing(["ses_empty"]);
        let cache = SessionCacheStore::under(cache_base.path(), storage.cache());
        let loader = SessionLoader {
            storage: &storage,
            cache: &cache,
            show_last: false,
            debug_level: None,
            meter: AgentMeter::UNMETERED,
            clock: &crate::clock::TestClock::default(),
        };
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
        let cache = SessionCacheStore::under(cache_base.path(), storage.cache());

        let listed = load_sessions_with_cache(&storage, &cache, false, None).unwrap();

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
