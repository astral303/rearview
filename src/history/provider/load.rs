//! The load loop shared by every provider that stores sessions under roots.

use super::storage::{DiscoveredSessions, ResolvedSession, SessionStub};
use super::{SessionRoot, SessionStorage, SessionTitle};
use crate::cli::DebugLevel;
use crate::debug;
use crate::error::Result;
use crate::history::cache::{
    CachedFingerprint, ListedSessionEntry, SessionCacheEntry, SessionCacheStore,
    cached_conversation, conversation_from_cached,
};
use crate::history::{
    Conversation, FilterTerm, Source, format_short_name_from_path, process_conversation_file,
};
use std::collections::HashMap;
use std::path::PathBuf;

/// One provider's sessions, and what its roots hold that it ignores.
pub struct LoadedSessions {
    /// Newest first.
    pub conversations: Vec<Conversation>,
    /// One term per reason a root's sessions were ignored for, named for the
    /// user.
    pub ignored: Vec<FilterTerm>,
}

/// Every session `storage` holds, newest first.
///
/// Each root carries its own cache, so a session that has not changed since the
/// last run is rebuilt from cached metadata instead of reparsed, and one that
/// held no conversation is skipped without being read again. `progress` hears
/// `(done, total)` once every root is discovered and again after each session,
/// whatever became of it.
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

/// The session `session_id` names, from whichever provider stores it, as
/// the row the list would have shown: the provider's cache-or-parse step,
/// sub-agent transcripts merged, with the cache entry written back beside
/// the root's others. A provider with no storage (Claude) parses the stub's
/// locator alone. The preview is the opening one, as the list's default.
///
/// `None` when no provider stores the session, or it holds no conversation,
/// is over the provider's size limit, or cannot be read.
pub fn load_session_by_id(session_id: &str) -> Option<(Source, Conversation)> {
    let (source, ResolvedSession { root, stub }) = super::resolve_session_id(session_id)?;
    let conversation = match source.provider().storage() {
        Some(storage) => SessionLoader {
            storage,
            cache: &SessionCacheStore::in_user_cache(storage.cache()),
            show_last: false,
            debug_level: None,
        }
        .load_one(&root, &stub),
        None => process_conversation_file(stub.locator, stub.fingerprint.modified, None)
            .ok()
            .flatten(),
    }?;
    Some((source, conversation))
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

/// One provider's sessions, loaded against one cache with one set of options.
struct SessionLoader<'a> {
    storage: &'a dyn SessionStorage,
    cache: &'a SessionCacheStore,
    show_last: bool,
    debug_level: Option<DebugLevel>,
}

impl SessionLoader<'_> {
    fn load(&self, progress: &mut dyn FnMut(usize, usize)) -> Result<LoadedSessions> {
        let discovered = self.discover_every_root()?;
        let total = discovered.iter().map(|(_, found)| found.stubs.len()).sum();
        let mut done = 0;
        progress(done, total);

        let mut conversations = Vec::new();
        let mut ignored = Vec::new();
        for (root, found) in discovered {
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
            ignored.extend(
                found
                    .ignored
                    .iter()
                    .filter_map(|sessions| sessions.filter_term(self.storage.source())),
            );
            conversations.extend(self.load_root(&root, found.stubs, &mut || {
                done += 1;
                progress(done, total);
            }));
        }
        conversations.sort_by_key(|conversation| std::cmp::Reverse(conversation.timestamp));
        for (index, conversation) in conversations.iter_mut().enumerate() {
            conversation.index = index;
        }
        Ok(LoadedSessions {
            conversations,
            ignored,
        })
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

    /// The conversations among `stubs`, the sessions discovered under `root`.
    /// `on_session` is called once per stub, whatever became of it.
    fn load_root(
        &self,
        root: &SessionRoot,
        stubs: Vec<SessionStub>,
        on_session: &mut dyn FnMut(),
    ) -> Vec<Conversation> {
        let cached = self.cache.read(&root.path);
        let external_titles = self.storage.external_titles(root);
        let mut refreshed_cache = HashMap::new();
        let mut conversations = Vec::new();

        for stub in stubs {
            let outcome = self.restore_or_parse(root, &cached, &external_titles, &stub);
            if let Some(conversation) =
                self.cache_and_yield_row(&stub, outcome, &mut refreshed_cache)
            {
                conversations.push(conversation);
            }
            on_session();
        }

        self.cache.write(&root.path, refreshed_cache);
        conversations
    }

    /// One session under `root`, its cache entry refreshed in place among
    /// the root's others.
    fn load_one(&self, root: &SessionRoot, stub: &SessionStub) -> Option<Conversation> {
        let mut cached = self.cache.read(&root.path);
        let external_titles = self.storage.external_titles(root);
        let outcome = self.restore_or_parse(root, &cached, &external_titles, stub);
        let conversation = self.cache_and_yield_row(stub, outcome, &mut cached);
        self.cache.write(&root.path, cached);
        conversation
    }

    fn cache_and_yield_row(
        &self,
        stub: &SessionStub,
        outcome: SessionOutcome,
        refreshed_cache: &mut HashMap<String, SessionCacheEntry>,
    ) -> Option<Conversation> {
        match outcome {
            SessionOutcome::Restored(conversation) | SessionOutcome::Parsed(conversation) => {
                let conversation = self.resolve_preview_and_project(conversation);
                if let Some(fingerprint) = stub.fingerprint.stamp() {
                    let entry = listed_session_entry(&conversation, fingerprint);
                    refreshed_cache.insert(stub.cache_key.clone(), entry);
                }
                Some(conversation)
            }
            SessionOutcome::Empty => {
                if let Some(fingerprint) = stub.fingerprint.stamp() {
                    refreshed_cache.insert(
                        stub.cache_key.clone(),
                        SessionCacheEntry::Empty(fingerprint),
                    );
                }
                None
            }
            SessionOutcome::OverSizeLimit | SessionOutcome::Unreadable => None,
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

    /// Size limit first, then the cache, then the transcript: a session over
    /// the limit is never opened, whatever the cache holds for it.
    fn restore_or_parse(
        &self,
        root: &SessionRoot,
        cached: &HashMap<String, SessionCacheEntry>,
        external_titles: &HashMap<String, SessionTitle>,
        stub: &SessionStub,
    ) -> SessionOutcome {
        if exceeds_size_limit(
            self.storage,
            stub.fingerprint.size,
            &stub.locator,
            self.debug_level,
        ) {
            return SessionOutcome::OverSizeLimit;
        }
        let cached_entry = stub.fingerprint.stamp().and_then(|stamp| {
            cached
                .get(&stub.cache_key)
                .filter(|entry| entry.fingerprint() == stamp)
        });
        match cached_entry {
            Some(SessionCacheEntry::Empty(_)) => SessionOutcome::Empty,
            Some(SessionCacheEntry::Listed(entry)) => SessionOutcome::Restored(restore_from_cache(
                self.storage,
                entry,
                stub.locator.clone(),
                self.show_last,
                external_titles,
            )),
            None => parse_session(self.storage, stub, root, self.debug_level),
        }
    }
}

/// The resulting type of a discovered session.
///
/// The three outcomes that yield no conversation are separate variants because
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
    /// Over the provider's size limit, so it was never opened. Not cached: the
    /// limit is a setting that can change between runs, and a cached verdict
    /// would outlive the setting that produced it.
    OverSizeLimit,
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

fn exceeds_size_limit(
    storage: &dyn SessionStorage,
    size: u64,
    locator: &std::path::Path,
    debug_level: Option<DebugLevel>,
) -> bool {
    let Some(limit) = storage.max_session_bytes() else {
        return false;
    };
    if size <= limit {
        return false;
    }
    debug::warn(
        debug_level,
        &format!(
            "Skipping {}: {size} bytes exceeds the {limit} byte session limit",
            locator.display()
        ),
    );
    true
}

fn restore_from_cache(
    storage: &dyn SessionStorage,
    entry: &ListedSessionEntry,
    locator: PathBuf,
    show_last: bool,
    external_titles: &HashMap<String, SessionTitle>,
) -> Conversation {
    let mut conversation = conversation_from_cached(&entry.conversation, locator, show_last);
    conversation.source = storage.source();
    conversation.session_id = entry.session_id.clone();
    conversation.subagents = entry.subagents.clone();
    conversation.cwd = Some(entry.project_path.clone());
    conversation.project_path = Some(entry.project_path.clone());
    conversation.project_name = Some(format_short_name_from_path(&entry.project_path));
    // A sidecar title can change without the transcript changing, so the
    // cached one is only a fallback for sessions the sidecar does not name.
    match external_titles.get(&conversation.session_id) {
        Some(SessionTitle::Custom(title)) => conversation.custom_title = Some(title.clone()),
        Some(SessionTitle::Generated(title)) => conversation.summary = Some(title.clone()),
        None => {}
    }
    conversation
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
) -> SessionOutcome {
    match storage.parse_session(stub, root, debug_level) {
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
    use crate::history::cache;
    use crate::history::provider::{Fingerprint, IgnoredSessions, walk};
    use std::collections::HashSet;
    use std::sync::Mutex;
    use std::time::{Duration, UNIX_EPOCH};

    /// Records which transcripts the loop offered it, so a test can assert what
    /// the loop filtered before parsing was ever attempted.
    struct RecordingStorage {
        root: SessionRoot,
        max_session_bytes: Option<u64>,
        parsed: Mutex<Vec<PathBuf>>,
    }

    impl RecordingStorage {
        fn new(root: PathBuf, max_session_bytes: Option<u64>) -> Self {
            Self {
                root: SessionRoot::new(root),
                max_session_bytes,
                parsed: Mutex::new(Vec::new()),
            }
        }

        fn parsed_file_names(&self) -> Vec<String> {
            let mut names = self
                .parsed
                .lock()
                .unwrap()
                .iter()
                .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
                .collect::<Vec<_>>();
            names.sort();
            names
        }
    }

    impl SessionStorage for RecordingStorage {
        fn source(&self) -> Source {
            Source::Pi
        }

        fn cache(&self) -> super::super::SessionCache {
            super::super::SessionCache {
                directory: "recording-storage",
                magic: *b"RECORD01",
                schema_version: 1,
            }
        }

        fn roots(&self) -> Result<Vec<SessionRoot>> {
            Ok(vec![self.root.clone()])
        }

        fn discover(&self, root: &SessionRoot) -> Result<DiscoveredSessions> {
            Ok(DiscoveredSessions::complete(walk::file_stubs(
                root,
                walk::jsonl_files_at_depth(&root.path, 0)?,
            )))
        }

        fn parse_session(
            &self,
            stub: &SessionStub,
            _root: &SessionRoot,
            _debug_level: Option<DebugLevel>,
        ) -> Result<Option<Conversation>> {
            self.parsed.lock().unwrap().push(stub.locator.clone());
            Ok(None)
        }

        fn max_session_bytes(&self) -> Option<u64> {
            self.max_session_bytes
        }
    }

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
        holding_nothing: HashSet<String>,
        unreadable: HashSet<String>,
        max_session_bytes: Option<u64>,
        /// The sessions every root reports as ignored.
        ignored: Vec<IgnoredSessions>,
    }

    impl VirtualStorage {
        fn new(stubs: Vec<SessionStub>) -> Self {
            Self {
                roots: vec![SessionRoot::new("container.db")],
                stubs,
                parsed: Mutex::new(Vec::new()),
                holding_nothing: HashSet::new(),
                unreadable: HashSet::new(),
                max_session_bytes: None,
                ignored: Vec::new(),
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

        fn with_size_limit(mut self, limit: u64) -> Self {
            self.max_session_bytes = Some(limit);
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
            Ok(DiscoveredSessions {
                stubs: self
                    .stubs
                    .iter()
                    .filter(|stub| stub.locator.starts_with(&root.path))
                    .cloned()
                    .collect(),
                ignored: self.ignored.clone(),
                skipped: 0,
            })
        }

        fn parse_session(
            &self,
            stub: &SessionStub,
            _root: &SessionRoot,
            _debug_level: Option<DebugLevel>,
        ) -> Result<Option<Conversation>> {
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
            Ok(Some(conversation))
        }

        fn max_session_bytes(&self) -> Option<u64> {
            self.max_session_bytes
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

    fn write_transcript(directory: &std::path::Path, name: &str, bytes: usize) {
        std::fs::write(directory.join(name), "x".repeat(bytes)).unwrap();
    }

    fn parsed_files_for_limit(max_session_bytes: Option<u64>) -> Vec<String> {
        let directory = tempfile::tempdir().unwrap();
        let cache_base = tempfile::tempdir().unwrap();
        write_transcript(directory.path(), "small.jsonl", 10);
        write_transcript(directory.path(), "huge.jsonl", 5_000);

        let storage = RecordingStorage::new(directory.path().to_path_buf(), max_session_bytes);
        let cache = SessionCacheStore::under(cache_base.path(), storage.cache());
        load_sessions_with_cache(&storage, &cache, false, None).unwrap();

        storage.parsed_file_names()
    }

    fn session(id: &str, agent_text: &str) -> Conversation {
        let mut conversation = cache::conversation_from_cached(
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
            &mut || {},
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

    /// Skipped transcripts were discovered, so they count: the indicator must
    /// reach the total it announced.
    #[test]
    fn progress_counts_transcripts_skipped_for_size() {
        let directory = tempfile::tempdir().unwrap();
        let cache_base = tempfile::tempdir().unwrap();
        write_transcript(directory.path(), "small.jsonl", 10);
        write_transcript(directory.path(), "huge.jsonl", 5_000);
        let storage = RecordingStorage::new(directory.path().to_path_buf(), Some(1_000));
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

        assert_eq!(reports, vec![(0, 2), (1, 2), (2, 2)]);
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

    #[test]
    fn transcripts_over_the_size_limit_are_never_parsed() {
        assert_eq!(
            parsed_files_for_limit(Some(1_000)),
            vec!["small.jsonl".to_string()]
        );
    }

    #[test]
    fn no_size_limit_offers_every_transcript() {
        assert_eq!(
            parsed_files_for_limit(None),
            vec!["huge.jsonl".to_string(), "small.jsonl".to_string()]
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

    /// The size limit is a setting, not something the transcript says about
    /// itself. Recording a skip against the fingerprint would keep the session
    /// hidden after the limit was raised.
    #[test]
    fn a_session_over_the_size_limit_is_not_recorded_as_empty() {
        let cache_base = tempfile::tempdir().unwrap();
        let limited = VirtualStorage::new(vec![virtual_stub("ses_huge", 5_000, 1_000)])
            .with_size_limit(1_000);
        let cache = SessionCacheStore::under(cache_base.path(), limited.cache());
        assert!(
            load_sessions_with_cache(&limited, &cache, false, None)
                .unwrap()
                .is_empty()
        );
        assert!(
            limited.parsed_ids().is_empty(),
            "a session over the limit is never opened"
        );

        let raised = VirtualStorage::new(vec![virtual_stub("ses_huge", 5_000, 1_000)]);
        let listed = load_sessions_with_cache(&raised, &cache, false, None).unwrap();

        assert_eq!(
            listed.len(),
            1,
            "raising the limit lists it without the transcript changing"
        );
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
}
