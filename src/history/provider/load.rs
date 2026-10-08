//! The load loop shared by every provider that stores sessions under roots.

use super::storage::{DiscoveredSessions, ResolvedSession, SessionStub};
use super::{SessionRoot, SessionStorage, SessionTitle};
use crate::cli::DebugLevel;
use crate::debug;
use crate::error::Result;
use crate::history::cache::{
    CachedFingerprint, ListedSessionEntry, SessionCacheEntry, SessionCacheStore,
    cached_conversation, conversation_from_cached, shard_index,
};
use crate::history::format::same_file;
use crate::history::{Conversation, FilterTerm, Source, format_short_name_from_path};
use rayon::prelude::*;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;

/// One provider's sessions, and what its roots hold that it ignores.
pub struct LoadedSessions {
    /// Newest first.
    pub conversations: Vec<Conversation>,
    /// One term per reason a root's sessions were ignored for, named for the
    /// user.
    pub ignored: Vec<FilterTerm>,
    /// The sessions that hold no conversation, each with the fingerprint
    /// discovery found. A refresh skips each one until its fingerprint
    /// changes.
    pub empty: Vec<(PathBuf, Option<CachedFingerprint>)>,
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
}

/// The outcome of reading one session.
pub enum SessionRead {
    /// The row the list shows for the session.
    Listed(Box<Conversation>),
    /// Read cleanly, and holds no conversation the provider lists.
    Empty,
    /// Could not be read. Not cached, so the next read tries again.
    Unreadable,
}

/// Every session `storage` holds, newest first.
///
/// Each root carries its own cache, so a session that has not changed since the
/// last run is rebuilt from cached metadata instead of reparsed, and one that
/// held no conversation is skipped without being read again. `progress` hears
/// `(done, total)` in transcripts, a session's own and each sub-agent's: once
/// every root is discovered, then as each transcript is read or its session
/// completes without reading it.
pub fn load_sessions(
    storage: &dyn SessionStorage,
    show_last: bool,
    debug_level: Option<DebugLevel>,
    progress: &mut dyn FnMut(usize, usize),
) -> Result<LoadedSessions> {
    SessionLoader {
        storage,
        cache: &SessionCacheStore::in_user_cache(storage.cache()),
        show_last,
        debug_level,
    }
    .load(progress)
}

/// `storage`'s sessions as discovery finds them now, reading only those
/// `needs_reading` picks: through the cache, as a load reads them, rewriting
/// only the shards whose sessions missed it.
pub fn rediscover_sessions(
    storage: &dyn SessionStorage,
    show_last: bool,
    debug_level: Option<DebugLevel>,
    needs_reading: &dyn Fn(&SessionStub) -> bool,
) -> Result<RediscoveredSessions> {
    SessionLoader {
        storage,
        cache: &SessionCacheStore::in_user_cache(storage.cache()),
        show_last,
        debug_level,
    }
    .rediscover(needs_reading)
}

/// The session `session_id` names, from whichever provider stores it, as
/// the row the list would have shown: the provider's cache-or-parse step,
/// sub-agent transcripts merged, with the cache entry written back beside
/// the root's others, and the preview `show_last` selects.
///
/// `None` when no provider stores the session, or it holds no conversation,
/// or cannot be read.
pub fn load_session_by_id(session_id: &str, show_last: bool) -> Option<(Source, Conversation)> {
    let (source, ResolvedSession { root, stub }) = super::resolve_session_id(session_id)?;
    let storage = source.provider().storage();
    let conversation = SessionLoader {
        storage,
        cache: &SessionCacheStore::in_user_cache(storage.cache()),
        show_last,
        debug_level: None,
    }
    .load_one(&root, &stub)?;
    Some((source, conversation))
}

/// The sessions at `locators`, one result each in order, as the list builds
/// their rows: the stub `source`'s discovery lists for each file, however
/// its path is spelled, read through the cache with its sub-agent
/// transcripts, with the preview `show_last` selects. Another copy stored
/// under the same session id is not read. Discovery runs once for all of
/// them.
///
/// `None` when discovery no longer lists the file. A discovery that fails,
/// or cannot list the file's directory, reads as [`SessionRead::Unreadable`]:
/// the session may still be there.
pub fn reread_sessions(
    source: Source,
    locators: &[&Path],
    show_last: bool,
) -> Vec<Option<SessionRead>> {
    let storage = source.provider().storage();
    SessionLoader {
        storage,
        cache: &SessionCacheStore::in_user_cache(storage.cache()),
        show_last,
        debug_level: None,
    }
    .reread(locators)
}

/// [`reread_sessions`] for one session, against a caller-chosen storage and
/// cache.
#[cfg(test)]
pub(crate) fn reread_session_with_cache(
    storage: &dyn SessionStorage,
    cache: &SessionCacheStore,
    locator: &Path,
    show_last: bool,
) -> Option<SessionRead> {
    SessionLoader {
        storage,
        cache,
        show_last,
        debug_level: None,
    }
    .reread(&[locator])
    .pop()
    .flatten()
}

/// [`load_sessions`] against a caller-chosen cache, reporting nothing and
/// keeping only the conversations.
#[cfg(test)]
pub(crate) fn load_sessions_with_cache(
    storage: &dyn SessionStorage,
    cache: &SessionCacheStore,
    show_last: bool,
    debug_level: Option<DebugLevel>,
) -> Result<Vec<Conversation>> {
    SessionLoader {
        storage,
        cache,
        show_last,
        debug_level,
    }
    .load(&mut |_, _| {})
    .map(|loaded| loaded.conversations)
}

/// [`rediscover_sessions`] against a caller-chosen cache.
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
    }
    .rediscover(needs_reading)
}

/// One provider's sessions, loaded against one cache with one set of options.
struct SessionLoader<'a> {
    storage: &'a dyn SessionStorage,
    cache: &'a SessionCacheStore,
    show_last: bool,
    debug_level: Option<DebugLevel>,
}

impl SessionLoader<'_> {
    /// Sessions are loaded on a worker thread, a root's sessions in parallel,
    /// while this thread reports each transcript read to `progress`: the
    /// callback stays on the caller's thread, and the count moves inside a
    /// session with many sub-agents instead of jumping when it completes.
    fn load(&self, progress: &mut dyn FnMut(usize, usize)) -> Result<LoadedSessions> {
        let discovered = self.discover_every_root()?;
        let total = discovered
            .iter()
            .flat_map(|(_, found)| &found.stubs)
            .map(transcript_count)
            .sum();
        progress(0, total);

        let (read, transcripts_read) = mpsc::channel::<usize>();
        let loaded = std::thread::scope(|scope| {
            let worker = scope.spawn(move || self.load_discovered(discovered, &read));
            let mut done = 0;
            for count in transcripts_read {
                done += count;
                progress(done, total);
            }
            worker
                .join()
                .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
        });
        if let Some(cache_base) = self.cache.base() {
            self.storage.remove_superseded_cache(cache_base);
        }
        Ok(loaded)
    }

    /// Every discovered root's sessions, newest first, each transcript read
    /// announced on `read`.
    fn load_discovered(
        &self,
        discovered: Vec<(SessionRoot, DiscoveredSessions)>,
        read: &mpsc::Sender<usize>,
    ) -> LoadedSessions {
        let mut conversations = Vec::new();
        let mut ignored = Vec::new();
        let mut empty = Vec::new();
        for (root, found) in discovered {
            ignored.extend(self.report_discovery(&root, &found));
            let (listed, held_nothing) = self.load_root(&root, found.stubs, read);
            conversations.extend(listed);
            empty.extend(held_nothing);
        }
        conversations.sort_by_key(|conversation| std::cmp::Reverse(conversation.timestamp));
        for (index, conversation) in conversations.iter_mut().enumerate() {
            conversation.index = index;
        }
        LoadedSessions {
            conversations,
            ignored,
            empty,
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
            rediscovered
                .ignored
                .extend(self.report_discovery(&root, &found));
            let external_titles = self.storage.external_titles(&root);
            rediscovered.found.extend(
                found
                    .stubs
                    .iter()
                    .map(|stub| (stub.locator.clone(), stub.fingerprint.stamp())),
            );
            let picked = found
                .stubs
                .into_iter()
                .filter(|stub| needs_reading(stub))
                .collect();
            rediscovered
                .read
                .extend(self.read_stubs(&root, &external_titles, picked));
            rediscovered.external_titles.extend(external_titles);
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
        let mut discovered = Vec::new();
        for root in self.storage.roots()? {
            let found = self.storage.discover(&root)?;
            discovered.push((root, found));
        }
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
    /// The second list holds the sessions that hold no conversation, with
    /// their fingerprints.
    fn load_root(
        &self,
        root: &SessionRoot,
        stubs: Vec<SessionStub>,
        read: &mpsc::Sender<usize>,
    ) -> (Vec<Conversation>, Vec<(PathBuf, Option<CachedFingerprint>)>) {
        let cached = self.cache.read(&root.path);
        let external_titles = self.storage.external_titles(root);
        let mut refreshed_cache = HashMap::new();
        let mut conversations = Vec::new();
        let mut empty = Vec::new();
        let mut changed_shards = BTreeSet::new();

        let outcomes: Vec<SessionOutcome> = stubs
            .par_iter()
            .map(|stub| {
                let announced = AtomicUsize::new(0);
                let on_transcript_read = || {
                    announced.fetch_add(1, Ordering::Relaxed);
                    let _ = read.send(1);
                };
                let outcome = self.restore_or_parse(
                    root,
                    &cached,
                    &external_titles,
                    stub,
                    &on_transcript_read,
                );
                let unannounced =
                    transcript_count(stub).saturating_sub(announced.load(Ordering::Relaxed));
                if unannounced > 0 {
                    let _ = read.send(unannounced);
                }
                outcome
            })
            .collect();

        for (stub, outcome) in stubs.iter().zip(outcomes) {
            let hit = cached_entry(&cached, stub).is_some();
            match self.record(stub, outcome, &mut refreshed_cache) {
                SessionRead::Listed(conversation) => conversations.push(*conversation),
                SessionRead::Empty => empty.push((stub.locator.clone(), stub.fingerprint.stamp())),
                SessionRead::Unreadable => {}
            }
            if !hit && refreshed_cache.contains_key(&stub.cache_key) {
                changed_shards.insert(shard_index(&stub.cache_key));
            }
        }
        changed_shards.extend(
            cached
                .keys()
                .filter(|cache_key| !refreshed_cache.contains_key(*cache_key))
                .map(|cache_key| shard_index(cache_key)),
        );

        for index in changed_shards {
            self.cache.write_shard(&root.path, index, &refreshed_cache);
        }
        (conversations, empty)
    }

    /// The sessions discovery lists for the files at `locators`, one result
    /// each in order, read through the cache after one discovery. `None` for
    /// a file no root lists.
    fn reread(&self, locators: &[&Path]) -> Vec<Option<SessionRead>> {
        let Ok(discovered) = self.discover_every_root() else {
            return locators
                .iter()
                .map(|_| Some(SessionRead::Unreadable))
                .collect();
        };
        let mut reads: Vec<Option<SessionRead>> = locators.iter().map(|_| None).collect();
        for (root, found) in discovered {
            let mut picked = Vec::new();
            for (index, locator) in locators.iter().enumerate() {
                if reads[index].is_some() {
                    continue;
                }
                if found
                    .unreadable_directories
                    .iter()
                    .any(|directory| locator.starts_with(&directory.path))
                {
                    reads[index] = Some(SessionRead::Unreadable);
                } else if let Some(stub) = found
                    .stubs
                    .iter()
                    .find(|stub| same_file(&stub.locator, locator))
                {
                    picked.push((index, stub.clone()));
                }
            }
            if picked.is_empty() {
                continue;
            }
            let external_titles = self.storage.external_titles(&root);
            let stubs = picked.iter().map(|(_, stub)| stub.clone()).collect();
            let mut read: HashMap<PathBuf, SessionRead> = self
                .read_stubs(&root, &external_titles, stubs)
                .into_iter()
                .collect();
            for (index, stub) in picked {
                reads[index] = Some(
                    read.remove(&stub.locator)
                        .unwrap_or(SessionRead::Unreadable),
                );
            }
        }
        reads
    }

    /// [`Self::read_one`]'s row, when the session holds one.
    fn load_one(&self, root: &SessionRoot, stub: &SessionStub) -> Option<Conversation> {
        match self.read_one(root, stub.clone()) {
            SessionRead::Listed(conversation) => Some(*conversation),
            SessionRead::Empty | SessionRead::Unreadable => None,
        }
    }

    /// One session under `root`, its cache entry read from and written back
    /// to its own shard alone.
    fn read_one(&self, root: &SessionRoot, stub: SessionStub) -> SessionRead {
        let external_titles = self.storage.external_titles(root);
        self.read_stubs(root, &external_titles, vec![stub])
            .pop()
            .map_or(SessionRead::Unreadable, |(_, read)| read)
    }

    /// `stubs`, sessions under `root`, read through the cache shard by shard.
    /// Only the shards holding one of them are read, and only those holding
    /// one that missed the cache are rewritten, merging their entries into
    /// the shard as it is when written.
    fn read_stubs(
        &self,
        root: &SessionRoot,
        external_titles: &HashMap<String, SessionTitle>,
        stubs: Vec<SessionStub>,
    ) -> Vec<(PathBuf, SessionRead)> {
        let mut by_shard = BTreeMap::<usize, Vec<SessionStub>>::new();
        for stub in stubs {
            by_shard
                .entry(shard_index(&stub.cache_key))
                .or_default()
                .push(stub);
        }
        let mut read = Vec::new();
        for (shard, stubs) in by_shard {
            let cached = self.cache.read_shard(&root.path, shard);
            let outcomes: Vec<SessionOutcome> = stubs
                .par_iter()
                .map(|stub| self.restore_or_parse(root, &cached, external_titles, stub, &|| {}))
                .collect();
            let mut entries = HashMap::new();
            let mut missed = false;
            for (stub, outcome) in stubs.iter().zip(outcomes) {
                missed |= cached_entry(&cached, stub).is_none();
                read.push((
                    stub.locator.clone(),
                    self.record(stub, outcome, &mut entries),
                ));
            }
            if missed {
                self.cache.merge_into_shard(&root.path, shard, entries);
            }
        }
        read
    }

    /// `outcome`'s row, carrying the fingerprint of the stub it was read
    /// from, with its entry put in `cache`. An unreadable session gets no
    /// entry, so the next read tries it again.
    fn record(
        &self,
        stub: &SessionStub,
        outcome: SessionOutcome,
        cache: &mut HashMap<String, SessionCacheEntry>,
    ) -> SessionRead {
        match outcome {
            SessionOutcome::Restored(conversation) | SessionOutcome::Parsed(conversation) => {
                let mut conversation = self.resolve_preview_and_project(conversation);
                conversation.fingerprint = stub.fingerprint.stamp();
                if let Some(fingerprint) = conversation.fingerprint {
                    let entry = listed_session_entry(&conversation, fingerprint);
                    cache.insert(stub.cache_key.clone(), entry);
                }
                SessionRead::Listed(Box::new(conversation))
            }
            SessionOutcome::Empty => {
                if let Some(fingerprint) = stub.fingerprint.stamp() {
                    cache.insert(
                        stub.cache_key.clone(),
                        SessionCacheEntry::Empty(fingerprint),
                    );
                }
                SessionRead::Empty
            }
            SessionOutcome::Unreadable => SessionRead::Unreadable,
        }
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

    /// The cache first, then the transcript.
    fn restore_or_parse(
        &self,
        root: &SessionRoot,
        cached: &HashMap<String, SessionCacheEntry>,
        external_titles: &HashMap<String, SessionTitle>,
        stub: &SessionStub,
        on_transcript_read: &(dyn Fn() + Sync),
    ) -> SessionOutcome {
        match cached_entry(cached, stub) {
            Some(SessionCacheEntry::Empty(_)) => SessionOutcome::Empty,
            Some(SessionCacheEntry::Listed(entry)) => SessionOutcome::Restored(restore_from_cache(
                self.storage,
                entry,
                stub.locator.clone(),
                self.show_last,
                external_titles,
            )),
            None => parse_session(
                self.storage,
                stub,
                root,
                self.debug_level,
                on_transcript_read,
            ),
        }
    }
}

/// How many transcripts `stub` names: its own and each sub-agent's.
fn transcript_count(stub: &SessionStub) -> usize {
    1 + stub.subagents.len()
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
    /// Could not be read or parsed. Not cached: an unreadable file is often a
    /// transient condition, and caching the failure would hide the transcript
    /// until it changed on disk.
    Unreadable,
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

fn restore_from_cache(
    storage: &dyn SessionStorage,
    entry: &ListedSessionEntry,
    locator: PathBuf,
    show_last: bool,
    external_titles: &HashMap<String, SessionTitle>,
) -> Conversation {
    let mut conversation =
        conversation_from_cached(storage.source(), &entry.conversation, locator, show_last);
    conversation.session_id = entry.session_id.clone();
    conversation.subagents = entry.subagents.clone();
    conversation.project_path = Some(entry.project_path.clone());
    conversation.project_name = Some(format_short_name_from_path(&entry.project_path));
    // A sidecar title can change without the transcript changing, so the
    // cached one is only a fallback for sessions the sidecar does not name.
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
/// redirected session directory can hold a sibling agent's files. It reads as
/// empty, so this root stops opening it — the provider that owns it lists it
/// under its own root.
fn parse_session(
    storage: &dyn SessionStorage,
    stub: &SessionStub,
    root: &SessionRoot,
    debug_level: Option<DebugLevel>,
    on_transcript_read: &(dyn Fn() + Sync),
) -> SessionOutcome {
    match storage.parse_session(stub, root, debug_level, on_transcript_read) {
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
            SessionOutcome::Unreadable
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::cache::{self, keys_in_distinct_shards};
    use crate::history::provider::{Fingerprint, IgnoredSessions};
    use std::collections::{BTreeMap, HashSet};
    use std::sync::Mutex;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    /// A storage whose locators name no file on disk. What it pins: the load
    /// loop consumes stubs as given — it never stats, opens, or interprets a
    /// locator — so a provider can back sessions with something other than
    /// files and still get listing and caching from the shared loop.
    struct VirtualStorage {
        roots: Vec<SessionRoot>,
        stubs: Vec<SessionStub>,
        /// One entry per `parse_session` call, by session id, so a test can
        /// assert that a load did not read a session at all.
        parsed: Mutex<Vec<String>>,
        /// How many times `discover` ran.
        discoveries: Mutex<usize>,
        holding_nothing: HashSet<String>,
        unreadable: HashSet<String>,
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
                holding_nothing: HashSet::new(),
                unreadable: HashSet::new(),
                ignored: Vec::new(),
                superseded_cache_removals: Mutex::new(Vec::new()),
            }
        }

        fn with_ignored(mut self, count: usize, reason: &'static str) -> Self {
            self.ignored.push(IgnoredSessions { count, reason });
            self
        }

        /// Sessions whose `parse_session` succeeds and yields no conversation.
        fn holding_nothing<const N: usize>(mut self, ids: [&str; N]) -> Self {
            self.holding_nothing = ids.iter().map(|id| (*id).to_owned()).collect();
            self
        }

        /// Sessions whose `parse_session` fails.
        fn unreadable<const N: usize>(mut self, ids: [&str; N]) -> Self {
            self.unreadable = ids.iter().map(|id| (*id).to_owned()).collect();
            self
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

        fn roots(&self) -> Result<Vec<SessionRoot>> {
            Ok(self.roots.clone())
        }

        fn discover(&self, root: &SessionRoot) -> Result<DiscoveredSessions> {
            *self.discoveries.lock().unwrap() += 1;
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

        fn parse_session(
            &self,
            stub: &SessionStub,
            _root: &SessionRoot,
            _debug_level: Option<DebugLevel>,
            on_transcript_read: &(dyn Fn() + Sync),
        ) -> Result<Option<Conversation>> {
            for _ in 0..transcript_count(stub) {
                on_transcript_read();
            }
            let locator = stub.locator.clone();
            let id = locator.file_stem().unwrap().to_string_lossy().into_owned();
            self.parsed.lock().unwrap().push(id.clone());
            if self.unreadable.contains(&id) {
                return Err(crate::error::AppError::ConfigError(format!(
                    "cannot read {id}"
                )));
            }
            if self.holding_nothing.contains(&id) {
                return Ok(None);
            }
            let mut conversation = session(&locator.to_string_lossy(), "text");
            conversation.source = Source::Pi;
            conversation.path = locator;
            conversation.subagents = stub.subagents.clone();
            conversation.preview_first = "opening".to_owned();
            conversation.preview_last = "closing".to_owned();
            Ok(Some(conversation))
        }

        fn remove_superseded_cache(&self, cache_base: &std::path::Path) {
            self.superseded_cache_removals.lock().unwrap().push((
                cache_base.to_path_buf(),
                shard_files_under(cache_base).len(),
            ));
        }
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
            &cache::CachedConversation::default(),
            PathBuf::new(),
            false,
        );
        conversation.session_id = id.to_owned();
        conversation.agent_search_text = agent_text.to_owned();
        conversation.message_count = 1;
        conversation.total_tokens = 10;
        conversation
    }

    /// A sub-agent transcript is part of its session's stub, not a stub of
    /// its own, so the cache holds one entry per session.
    #[test]
    fn every_cache_entry_names_a_session() {
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
            .read(std::path::Path::new("container.db"))
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

    /// A session opened by id runs the same step as the list, so its entry
    /// joins the root's cache and the next load restores it unparsed.
    #[test]
    fn a_session_loaded_by_id_is_cached_beside_the_roots_others() {
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
        };
        let root = SessionRoot::new("container.db");
        loader.load_root(
            &root,
            vec![virtual_stub("ses_listed", 100, 1_000)],
            &mpsc::channel().0,
        );

        let by_id = loader
            .load_one(&root, &virtual_stub("ses_by_id", 200, 2_000))
            .unwrap();

        assert!(by_id.path.ends_with("ses_by_id.jsonl"));
        assert_eq!(storage.parsed_ids(), vec!["ses_by_id", "ses_listed"]);
        let mut keys = cache.read(&root.path).into_keys().collect::<Vec<_>>();
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
        }
        .load(&mut |done, total| reports.push((done, total)))
        .unwrap();

        assert_eq!(reports, vec![(0, 3), (1, 3), (2, 3), (3, 3)]);
    }

    /// A root can hold sessions the provider found but ignores. The load
    /// words them for the user, one term per reason, so the list can show why
    /// it holds less than the disk does; a reason nothing was ignored for
    /// makes no term.
    #[test]
    fn a_roots_ignored_sessions_are_reported_as_terms_with_its_sessions() {
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
        }
        .load(&mut |_, _| {})
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
    fn a_load_removes_the_superseded_cache_after_writing_its_own() {
        let cache_base = tempfile::tempdir().unwrap();
        let storage = VirtualStorage::new(vec![virtual_stub("ses_first", 100, 1_000)]);
        let cache = SessionCacheStore::under(cache_base.path(), storage.cache());
        let loader = SessionLoader {
            storage: &storage,
            cache: &cache,
            show_last: false,
            debug_level: None,
        };

        loader.load(&mut |_, _| {}).unwrap();

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
    }

    #[test]
    fn sessions_that_are_not_files_load_and_cache_through_the_shared_loop() {
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
            .read(std::path::Path::new("container.db"))
            .into_keys()
            .collect::<Vec<_>>();
        keys.sort();
        keys
    }

    #[test]
    fn a_load_with_no_session_changed_doesnt_rewrite_any_shard() {
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
    fn a_load_after_a_transcript_changed_rewrites_its_shard_and_no_other() {
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
                .read(std::path::Path::new("container.db"))
                .get(&stubs[1].cache_key)
                .map(|entry| entry.fingerprint().file_size),
            Some(250)
        );
    }

    #[test]
    fn a_load_after_a_shard_was_corrupted_reparses_its_sessions_and_rewrites_it_alone() {
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
    fn a_load_after_a_transcript_was_deleted_rewrites_its_shard_without_it() {
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

    #[test]
    fn a_load_with_a_new_transcript_writes_its_shard_and_no_other() {
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

    /// Opening by ID refreshes one session, so it writes that session's shard
    /// and leaves the root's other shards as they were.
    #[test]
    fn a_session_loaded_by_id_writes_its_shard_and_no_other() {
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
        };
        let root = SessionRoot::new("container.db");
        loader.load_root(&root, vec![listed.clone()], &mpsc::channel().0);
        let written = shard_files_under(cache_base.path());

        loader.load_one(&root, by_id);

        assert_only_this_shard_rewritten(
            written,
            shard_files_under(cache_base.path()),
            &shard_file_of_session(cache_base.path(), &by_id.cache_key),
        );
    }

    /// Opening by ID, or after a rename, decodes only the session's own
    /// shard, and a session cached there is restored, not parsed.
    #[test]
    fn a_session_loaded_by_id_reads_its_shard_and_no_other() {
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
        };
        let root = SessionRoot::new("container.db");
        loader.load_root(&root, stubs.clone(), &mpsc::channel().0);
        let parsed_by_the_load = storage.parsed_ids().len();

        let shard = cache.read_shard(&root.path, shard_index(&by_id.cache_key));
        loader.load_one(&root, by_id).unwrap();

        assert_eq!(shard.keys().collect::<Vec<_>>(), [&by_id.cache_key]);
        assert_eq!(
            storage.parsed_ids().len(),
            parsed_by_the_load,
            "the session is restored from its shard"
        );
    }

    #[test]
    fn a_load_reports_the_sessions_that_hold_no_conversation_with_their_fingerprints() {
        let cache_base = tempfile::tempdir().unwrap();
        let storage = VirtualStorage::new(vec![
            virtual_stub("ses_listed", 100, 1_000),
            virtual_stub("ses_empty", 200, 2_000),
        ])
        .holding_nothing(["ses_empty"]);
        let cache = SessionCacheStore::under(cache_base.path(), storage.cache());
        let loader = SessionLoader {
            storage: &storage,
            cache: &cache,
            show_last: false,
            debug_level: None,
        };

        let loaded = loader.load(&mut |_, _| {}).unwrap();

        let empty = virtual_stub("ses_empty", 200, 2_000);
        assert_eq!(
            loaded.empty,
            vec![(empty.locator.clone(), empty.fingerprint.stamp())]
        );
        assert_eq!(
            loaded.conversations[0].fingerprint,
            virtual_stub("ses_listed", 100, 1_000).fingerprint.stamp(),
            "a row carries the fingerprint it was read with"
        );
    }

    #[test]
    fn a_rediscovery_reads_only_the_sessions_it_picks_and_rewrites_only_their_shards() {
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
                .read(std::path::Path::new("container.db"))
                .get(&grown.cache_key)
                .map(|entry| entry.fingerprint().file_size),
            Some(250)
        );
    }

    #[test]
    fn a_session_read_again_previews_its_last_messages_when_asked() {
        let cache_base = tempfile::tempdir().unwrap();
        let stub = virtual_stub("ses_reread", 100, 1_000);
        let storage = VirtualStorage::new(vec![stub.clone()]);
        let cache = SessionCacheStore::under(cache_base.path(), storage.cache());

        let preview_of =
            |show_last| match reread_session_with_cache(&storage, &cache, &stub.locator, show_last)
            {
                Some(SessionRead::Listed(read)) => read.preview,
                _ => panic!("the session was not read as a listed session"),
            };

        assert_eq!(preview_of(true), "closing", "parsed");
        assert_eq!(preview_of(true), "closing", "restored from the cache");
        assert_eq!(preview_of(false), "opening");
    }

    #[test]
    fn sessions_read_again_together_share_one_discovery() {
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
        }
        .reread(&[&second.locator, &first.locator]);

        assert_eq!(*storage.discoveries.lock().unwrap(), 1);
        let paths: Vec<_> = reads
            .iter()
            .map(|read| match read {
                Some(SessionRead::Listed(read)) => read.path.clone(),
                _ => panic!("a session was not read as a listed session"),
            })
            .collect();
        assert_eq!(paths, [second.locator, first.locator], "in the order asked");
    }

    #[test]
    fn a_rediscovered_new_session_previews_its_last_messages_when_asked() {
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
    fn a_rediscovered_session_the_cache_already_holds_is_restored_not_parsed() {
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
    fn a_session_that_holds_no_conversation_is_read_once() {
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
    fn a_session_that_gains_content_is_read_again() {
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
    fn a_session_that_could_not_be_read_is_not_recorded_as_empty() {
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

    /// Progress for one load, `(done, total)` per report.
    fn progress_of_load(loader: &SessionLoader<'_>) -> Vec<(usize, usize)> {
        let mut reports = Vec::new();
        loader
            .load(&mut |done, total| reports.push((done, total)))
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
    fn progress_counts_each_transcript_a_session_reads() {
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
        };

        let reports = progress_of_load(&loader);

        assert_eq!(reports, vec![(0, 4), (1, 4), (2, 4), (3, 4), (4, 4)]);
    }

    /// A session restored from the cache, or recorded as holding no
    /// conversation, reads none of its transcripts; the count still reaches
    /// the total.
    #[test]
    fn progress_reaches_the_total_when_sessions_are_not_read() {
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
    fn a_parallel_load_lists_sessions_in_discovery_order_within_a_timestamp() {
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
