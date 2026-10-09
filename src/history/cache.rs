//! Binary cache for parsed conversation metadata, one per provider root.
//!
//! Stores parsed conversation data in bincode format, keyed by each session's
//! path within its root and validated by mtime + file size. Eliminates
//! redundant transcript parsing and search text normalization on startup for
//! unchanged files.

use super::provider::SessionCache;
use super::{Conversation, ParseError, Source};
use crate::agent::refs::MessageRange;
use chrono::{Local, TimeZone};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// The bytes every session cache file opens with: its 8-byte magic, then its
/// schema version as a little-endian `u32`. bincode writes a struct's fields
/// in order with fixed-width integers, and both stamps lead every cache struct.
fn session_cache_header(magic: [u8; 8], schema_version: u32) -> [u8; 12] {
    let mut header = [0; 12];
    header[..8].copy_from_slice(&magic);
    header[8..].copy_from_slice(&schema_version.to_le_bytes());
    header
}

/// The `(size, mtime)` stamp every cache entry is validated against. A cached
/// session is reused while its transcript still stamps the same.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub struct CachedFingerprint {
    pub file_size: u64,
    pub mtime_secs: u64,
    pub mtime_nsecs: u32,
}

impl CachedFingerprint {
    pub fn of(file_size: u64, mtime: SystemTime) -> Self {
        let since_epoch = mtime.duration_since(UNIX_EPOCH).unwrap_or_default();
        Self {
            file_size,
            mtime_secs: since_epoch.as_secs(),
            mtime_nsecs: since_epoch.subsec_nanos(),
        }
    }

    pub fn matches(&self, file_size: u64, mtime: SystemTime) -> bool {
        *self == Self::of(file_size, mtime)
    }
}

/// The on-disk shape of one shard of a provider's root cache, and of
/// `sessions.bin`. Generic over the map so a write can serialize borrowed
/// entries without cloning a shard's text.
#[derive(Serialize, Deserialize)]
struct SessionCacheFile<Entries> {
    magic: [u8; 8],
    schema_version: u32,
    entries: Entries,
}

/// Shards per root. A session's shard is the hash of its cache key modulo
/// this, so changing it, or the hasher, moves every session to another shard
/// and needs a `SessionCache::schema_version` bump in every provider.
const SHARD_COUNT: usize = 16;

/// `sessions.bin` is the one file per root that releases before sharding
/// wrote. A root that still has one is migrated on its next read.
const SESSIONS_BIN_FILE_NAME: &str = "sessions.bin";

/// The shard `cache_key` belongs to, in `0..SHARD_COUNT`.
pub fn shard_index(cache_key: &str) -> usize {
    (stable_hash(&cache_key) % SHARD_COUNT as u64) as usize
}

fn shard_file_name(index: usize) -> String {
    format!("shard-{index:02}.bin")
}

/// `count` cache keys that each hash to a different shard, so a test can
/// tell one shard's write from another's whatever the hasher does.
#[cfg(test)]
pub(crate) fn keys_in_distinct_shards(count: usize) -> Vec<String> {
    let mut keys: Vec<String> = Vec::new();
    for candidate in (0..).map(|number| format!("session-{number}")) {
        if keys.len() == count {
            break;
        }
        let shard = shard_index(&candidate);
        if keys.iter().all(|key| shard_index(key) != shard) {
            keys.push(candidate);
        }
    }
    keys
}

/// `DefaultHasher::new()` hashes with fixed keys, so a root's directory and a
/// session's shard, both derived from it, stay put between runs of one
/// release. std does not fix the algorithm across releases; the pinned
/// indexes in `a_cache_key_hashes_to_the_same_shard_on_every_run` catch a
/// change before it reads every session as a miss.
fn stable_hash(value: &impl std::hash::Hash) -> u64 {
    use std::hash::Hasher;

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
}

/// One session's entry in a provider's whole-root cache.
///
/// A transcript that holds no conversation has a fingerprint and nothing else:
/// a session id and a project path are read out of a conversation, and an empty
/// parse yields none. They are absent from `Empty` rather than blank, so no
/// reader can build a row or an agent key out of placeholder identity.
///
/// `Listed` is the common case, so boxing it would cost one allocation per
/// listed session to save bytes on the rare `Empty` entry.
#[derive(Serialize, Deserialize, Clone)]
#[allow(clippy::large_enum_variant)]
pub enum SessionCacheEntry {
    Listed(ListedSessionEntry),
    Empty(CachedFingerprint),
}

#[derive(Serialize, Deserialize, Clone)]
pub struct ListedSessionEntry {
    /// See [`Fingerprint::spanning`](crate::history::provider::Fingerprint::spanning).
    pub fingerprint: CachedFingerprint,
    pub conversation: CachedConversation,
    pub session_id: String,
    /// The sub-agent transcripts merged into the row, so a cache hit carries
    /// what the viewer splices without looking anything up.
    pub subagents: Vec<PathBuf>,
    pub project_path: PathBuf,
}

impl SessionCacheEntry {
    pub fn fingerprint(&self) -> CachedFingerprint {
        match self {
            Self::Listed(listed) => listed.fingerprint,
            Self::Empty(fingerprint) => *fingerprint,
        }
    }
}

/// Cached conversation data — a dedicated DTO separate from Conversation
/// to avoid schema churn from UI/runtime field changes.
#[derive(Serialize, Deserialize, Clone, Default)]
pub struct CachedConversation {
    pub preview_first: String,
    pub preview_last: String,
    pub full_text: String,
    #[serde(default)]
    pub agent_search_text: String,
    pub semantic_route_text: String,
    #[serde(default)]
    pub semantic_turns: Vec<String>,
    #[serde(default)]
    pub semantic_turn_ranges: Vec<MessageRange>,
    pub search_text_lower: String,
    pub cwd: Option<PathBuf>,
    pub message_count: usize,
    /// New in schema 13. Entries written before it hold no value here; the
    /// version bump stops them from being read against this layout.
    #[serde(default)]
    pub assistant_messages: usize,
    pub parse_errors: Vec<CachedParseError>,
    pub summary: Option<String>,
    pub custom_title: Option<String>,
    pub model: Option<String>,
    pub total_tokens: u64,
    pub duration_minutes: Option<u64>,
    pub timestamp_epoch_ms: i64,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct CachedParseError {
    pub line_number: usize,
    pub line_content: String,
    pub error_message: String,
    pub context_before: Vec<String>,
    pub context_after: Vec<String>,
}

fn write_cache_file(path: &std::path::Path, cache: &impl Serialize) {
    if let Ok(data) = bincode::serialize(cache) {
        crate::cache_file::write_atomically(path, &data);
    }
}

/// Held across each shard's read and write in
/// [`SessionCacheStore::merge_into_shard`], so the refresh thread and the
/// list's own single-session reads take turns at a shard. Entries for other
/// sessions stay; for the same session, the later merge's entry wins.
/// Another process can still interleave; it costs that session a parse on
/// the next load.
static SHARD_MERGE: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// One provider's whole-root session caches on disk.
///
/// Carries both the identity stamped into every file it writes and the directory
/// those files live in. The directory is held rather than looked up so that a
/// test can run a load against a temporary tree and leave the user's cache
/// untouched.
pub struct SessionCacheStore {
    /// `None` when there is no home directory to cache in. Reads then miss and
    /// writes are dropped, rather than the load failing.
    directory: Option<PathBuf>,
    identity: SessionCache,
}

impl SessionCacheStore {
    pub fn in_user_cache(identity: SessionCache) -> Self {
        Self {
            directory: crate::cache_file::user_cache_base()
                .map(|base| base.join(identity.directory)),
            identity,
        }
    }

    #[cfg(test)]
    pub fn under(base: &std::path::Path, identity: SessionCache) -> Self {
        Self {
            directory: Some(base.join(identity.directory)),
            identity,
        }
    }

    /// The directory every provider's cache lives in: `$REARVIEW_CACHE_DIR`,
    /// or `~/.cache/rearview`, outside tests.
    pub fn base(&self) -> Option<&Path> {
        self.directory.as_deref()?.parent()
    }

    /// The directory `root`'s shards live in.
    ///
    /// Roots are hashed rather than embedded so two roots never collide and a
    /// moved root simply misses instead of reading a stale neighbour's entries.
    fn directory_for_root(&self, root: &std::path::Path) -> Option<PathBuf> {
        let resolved = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
        Some(
            self.directory
                .as_ref()?
                .join(format!("root-{:016x}", stable_hash(&resolved))),
        )
    }

    fn shard_path(&self, root: &std::path::Path, index: usize) -> Option<PathBuf> {
        Some(self.directory_for_root(root)?.join(shard_file_name(index)))
    }

    /// Cached entries for every session under `root`, keyed by path relative to
    /// it, from every shard. A shard that is absent, unreadable, or stamped for
    /// another provider or schema reads as nothing cached.
    ///
    /// A root still holding `sessions.bin` is migrated first, so every reader,
    /// including the by-ID and agent paths, sees one layout.
    pub fn read(&self, root: &std::path::Path) -> HashMap<String, SessionCacheEntry> {
        let Some(directory) = self.directory_for_root(root) else {
            return HashMap::new();
        };
        if let Some(entries) = self.migrate_sessions_bin(root, &directory) {
            return entries;
        }
        (0..SHARD_COUNT)
            .filter_map(|index| self.read_file(&directory.join(shard_file_name(index))))
            .flatten()
            .collect()
    }

    /// Cached entries for the sessions under `root` that shard `index`
    /// holds, read from that shard alone. A root still holding
    /// `sessions.bin` is migrated first, as [`read`](Self::read) does.
    pub fn read_shard(
        &self,
        root: &std::path::Path,
        index: usize,
    ) -> HashMap<String, SessionCacheEntry> {
        let Some(directory) = self.directory_for_root(root) else {
            return HashMap::new();
        };
        if let Some(entries) = self.migrate_sessions_bin(root, &directory) {
            return entries
                .into_iter()
                .filter(|(cache_key, _)| shard_index(cache_key) == index)
                .collect();
        }
        self.read_file(&directory.join(shard_file_name(index)))
            .unwrap_or_default()
    }

    /// The entries of `root`'s `sessions.bin`, migrated into the shards.
    /// `None` when the root has no `sessions.bin`. A `sessions.bin` stamped
    /// for another provider or schema is removed without writing a shard, so
    /// the shards on disk stay as they were.
    fn migrate_sessions_bin(
        &self,
        root: &std::path::Path,
        directory: &Path,
    ) -> Option<HashMap<String, SessionCacheEntry>> {
        let path = directory.join(SESSIONS_BIN_FILE_NAME);
        if !path.exists() {
            return None;
        }
        let entries = self.read_file(&path);
        if let Some(entries) = &entries {
            self.write_every_shard(root, entries);
        }
        let _ = std::fs::remove_file(&path);
        entries
    }

    /// `None` when the file is absent, unreadable, or stamped for another
    /// provider or schema.
    fn read_file(&self, path: &Path) -> Option<HashMap<String, SessionCacheEntry>> {
        let data = crate::cache_file::read_if_header_matches(
            path,
            &session_cache_header(self.identity.magic, self.identity.schema_version),
        )?;
        let file =
            bincode::deserialize::<SessionCacheFile<HashMap<String, SessionCacheEntry>>>(&data)
                .ok()?;
        Some(file.entries)
    }

    fn write_every_shard(
        &self,
        root: &std::path::Path,
        entries: &HashMap<String, SessionCacheEntry>,
    ) {
        for index in 0..SHARD_COUNT {
            self.write_shard(root, index, entries);
        }
    }

    /// Write shard `index` of `root` from the entries in `entries` that belong
    /// to it; the rest of the map is left to the other shards. A shard's file is
    /// replaced whole, so a session missing from `entries` is dropped from it.
    pub fn write_shard(
        &self,
        root: &std::path::Path,
        index: usize,
        entries: &HashMap<String, SessionCacheEntry>,
    ) {
        let Some(path) = self.shard_path(root, index) else {
            return;
        };
        let shard_entries: HashMap<&str, &SessionCacheEntry> = entries
            .iter()
            .filter(|(cache_key, _)| shard_index(cache_key) == index)
            .map(|(cache_key, entry)| (cache_key.as_str(), entry))
            .collect();
        // Nothing to cache and nothing cached: leave no trace, so an absent or
        // empty root does not grow a cache directory. An existing shard is
        // still overwritten, clearing entries for sessions that no longer exist.
        if shard_entries.is_empty() && !path.exists() {
            return;
        }
        write_cache_file(
            &path,
            &SessionCacheFile {
                magic: self.identity.magic,
                schema_version: self.identity.schema_version,
                entries: shard_entries,
            },
        );
    }

    /// Add `entries` to shard `index` of `root`: read the shard again and
    /// write it back with them, under [`SHARD_MERGE`]. Entries another
    /// thread wrote for other sessions since the caller read the shard stay;
    /// for a session in `entries`, the caller's entry replaces the shard's.
    pub fn merge_into_shard(
        &self,
        root: &std::path::Path,
        index: usize,
        entries: HashMap<String, SessionCacheEntry>,
    ) {
        let _turn = SHARD_MERGE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut shard = self.read_shard(root, index);
        shard.extend(entries);
        self.write_shard(root, index, &shard);
    }
}

/// Create a CachedConversation from a parsed Conversation
pub fn cached_conversation(conv: &Conversation) -> CachedConversation {
    CachedConversation {
        preview_first: conv.preview_first.clone(),
        preview_last: conv.preview_last.clone(),
        full_text: conv.full_text.clone(),
        agent_search_text: conv.agent_search_text.clone(),
        semantic_route_text: conv.semantic_route_text.clone(),
        semantic_turns: conv.semantic_turns.clone(),
        semantic_turn_ranges: conv.semantic_turn_ranges.clone(),
        search_text_lower: conv.search_text_lower.clone(),
        cwd: conv.cwd.clone(),
        message_count: conv.message_count,
        assistant_messages: conv.assistant_messages,
        parse_errors: conv
            .parse_errors
            .iter()
            .map(|e| CachedParseError {
                line_number: e.line_number,
                line_content: e.line_content.clone(),
                error_message: e.error_message.clone(),
                context_before: e.context_before.clone(),
                context_after: e.context_after.clone(),
            })
            .collect(),
        summary: conv.summary.clone(),
        custom_title: conv.custom_title.clone(),
        model: conv.model.clone(),
        total_tokens: conv.total_tokens,
        duration_minutes: conv.duration_minutes,
        timestamp_epoch_ms: conv.timestamp.timestamp_millis(),
    }
}

/// Reconstruct `source`'s conversation at `path` from a CachedConversation
pub fn conversation_from_cached(
    source: Source,
    cached: &CachedConversation,
    path: PathBuf,
    show_last: bool,
) -> Conversation {
    let timestamp = Local
        .timestamp_millis_opt(cached.timestamp_epoch_ms)
        .single()
        .unwrap_or_else(Local::now);
    let preview = if show_last {
        cached.preview_last.clone()
    } else {
        cached.preview_first.clone()
    };
    Conversation {
        source,
        subagents: Vec::new(),
        // Set by the caller, from the session id its cache entry holds.
        session_id: String::new(),
        path,
        index: 0,
        timestamp,
        preview,
        preview_first: cached.preview_first.clone(),
        preview_last: cached.preview_last.clone(),
        full_text: cached.full_text.clone(),
        agent_search_text: cached.agent_search_text.clone(),
        semantic_route_text: cached.semantic_route_text.clone(),
        semantic_turns: cached.semantic_turns.clone(),
        semantic_turn_ranges: cached.semantic_turn_ranges.clone(),
        search_text_lower: cached.search_text_lower.clone(),
        project_name: None,
        project_path: None,
        cwd: cached.cwd.clone(),
        message_count: cached.message_count,
        assistant_messages: cached.assistant_messages,
        parse_errors: cached
            .parse_errors
            .iter()
            .map(|e| ParseError {
                line_number: e.line_number,
                line_content: e.line_content.clone(),
                error_message: e.error_message.clone(),
                context_before: e.context_before.clone(),
                context_after: e.context_after.clone(),
            })
            .collect(),
        summary: cached.summary.clone(),
        custom_title: cached.custom_title.clone(),
        model: cached.model.clone(),
        total_tokens: cached.total_tokens,
        duration_minutes: cached.duration_minutes,
        fingerprint: None,
        has_transient_subagent_error: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache_file::test_support::TempFileFixture;
    use crate::history::Source;
    use crate::search::normalize_for_search;
    use std::time::Duration;

    fn make_test_conversation() -> Conversation {
        let timestamp = Local::now();
        Conversation {
            source: crate::history::Source::Claude,
            subagents: Vec::new(),
            session_id: "conv".to_owned(),
            path: PathBuf::from("/test/conv.jsonl"),
            index: 0,
            timestamp,
            preview: "Hello world ... Hi there".to_string(),
            preview_first: "Hello world ... Hi there".to_string(),
            preview_last: "Hi there ... Hello world".to_string(),
            full_text: "Hello world Hi there".to_string(),
            agent_search_text: "subagent cache text".to_string(),
            semantic_route_text: "semantic route text".to_string(),
            semantic_turns: vec!["Hello world".to_string(), "Hi there".to_string()],
            semantic_turn_ranges: vec![MessageRange::single(1), MessageRange::single(2)],
            search_text_lower: normalize_for_search("Hello world Hi there"),
            project_name: Some("test-project".to_string()),
            project_path: Some(PathBuf::from("/test/project")),
            cwd: Some(PathBuf::from("/test/cwd")),
            message_count: 2,
            assistant_messages: 1,
            parse_errors: vec![],
            summary: Some("Test summary".to_string()),
            custom_title: Some("My Session".to_string()),
            model: Some("claude-opus-4-5-20251101".to_string()),
            total_tokens: 1500,
            duration_minutes: Some(10),
            fingerprint: None,
            has_transient_subagent_error: false,
        }
    }

    fn identity(source: Source) -> SessionCache {
        source.provider().storage().cache()
    }

    fn empty_entry(cache_key: &str) -> (String, SessionCacheEntry) {
        (
            cache_key.to_owned(),
            SessionCacheEntry::Empty(CachedFingerprint::of(0, SystemTime::UNIX_EPOCH)),
        )
    }

    /// `count` empty entries whose keys each hash to a different shard, with
    /// the keys in the order they were made.
    fn empty_entries_in_distinct_shards(
        count: usize,
    ) -> (Vec<String>, HashMap<String, SessionCacheEntry>) {
        let keys = keys_in_distinct_shards(count);
        let entries = keys.iter().map(|key| empty_entry(key)).collect();
        (keys, entries)
    }

    fn sorted_keys(entries: &HashMap<String, SessionCacheEntry>) -> Vec<&str> {
        let mut keys = entries.keys().map(String::as_str).collect::<Vec<_>>();
        keys.sort_unstable();
        keys
    }

    /// A run over an absent or empty root must not grow the cache directory:
    /// every isolated test run and every user without the agent installed
    /// would otherwise leave a `root-<hash>` directory behind.
    #[test]
    fn an_empty_shard_write_to_an_absent_root_leaves_no_file() {
        let base = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let store = SessionCacheStore::under(base.path(), identity(Source::Pi));
        let path = store.shard_path(root.path(), 0).unwrap();

        store.write_shard(root.path(), 0, &HashMap::new());

        assert!(!path.exists(), "nothing cached and nothing to cache");
    }

    #[test]
    fn an_empty_shard_write_clears_an_existing_shard() {
        let base = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let store = SessionCacheStore::under(base.path(), identity(Source::Pi));
        let (cache_key, entry) = empty_entry("session.jsonl");
        let index = shard_index(&cache_key);
        let path = store.shard_path(root.path(), index).unwrap();
        store.write_shard(root.path(), index, &HashMap::from([(cache_key, entry)]));

        store.write_shard(root.path(), index, &HashMap::new());

        assert!(
            path.exists(),
            "an existing shard is cleared, not orphaned, when its sessions are gone"
        );
        assert!(store.read(root.path()).is_empty());
    }

    #[test]
    fn session_cache_roots_are_isolated_from_each_other() {
        let base = tempfile::tempdir().unwrap();
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        let pi = SessionCacheStore::under(base.path(), identity(Source::Pi));
        let omp = SessionCacheStore::under(base.path(), identity(Source::Omp));

        let first_path = pi.shard_path(first.path(), 0).unwrap();
        let second_path = pi.shard_path(second.path(), 0).unwrap();
        let omp_path = omp.shard_path(first.path(), 0).unwrap();

        assert_ne!(first_path, second_path);
        assert_ne!(first_path, omp_path);
        assert!(contains_segments(&first_path, &["pi"]));
        assert!(contains_segments(&omp_path, &["omp"]));
        assert!(
            file_stem_of_parent(&first_path).starts_with("root-"),
            "a session cache lives in a directory named for the hashed root"
        );
    }

    /// Where a root's shards live is a compatibility contract: users carry
    /// caches across upgrades, and moving or renaming a file silently discards
    /// it.
    #[test]
    fn session_cache_filenames_keep_their_shape() {
        let root = tempfile::tempdir().unwrap();
        let store = SessionCacheStore::in_user_cache(identity(Source::Pi));
        let path = store
            .shard_path(root.path(), 0)
            .expect("caching needs a home directory");

        assert_eq!(path.file_name().unwrap(), "shard-00.bin");
        assert_eq!(
            store
                .shard_path(root.path(), SHARD_COUNT - 1)
                .unwrap()
                .file_name()
                .unwrap(),
            "shard-15.bin"
        );
        assert!(contains_segments(&path, &["rearview", "pi"]));
        let directory = file_stem_of_parent(&path);
        let digest = directory
            .strip_prefix("root-")
            .expect("a root's directory is named root-<digest>");
        assert_eq!(digest.len(), 16);
        assert!(
            digest
                .chars()
                .all(|character| character.is_ascii_hexdigit())
        );

        assert_eq!(
            store.shard_path(root.path(), 0).as_ref(),
            Some(&path),
            "a root must resolve to the same directory on every run, or its cache is lost"
        );
    }

    /// Which shard a session lands in is a compatibility contract too: a
    /// hasher that moved every key would read every session as a miss.
    #[test]
    fn a_cache_key_hashes_to_the_same_shard_on_every_run() {
        assert_eq!(
            [
                shard_index("nested/session.jsonl"),
                shard_index("2026/09/13/rollout-a.jsonl"),
                shard_index("ses_0123456789abcdef"),
            ],
            [9, 6, 13]
        );
    }

    #[test]
    fn a_shard_write_holds_only_the_sessions_hashed_to_it() {
        let base = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let store = SessionCacheStore::under(base.path(), identity(Source::Pi));
        let (keys, entries) = empty_entries_in_distinct_shards(2);

        store.write_shard(root.path(), shard_index(&keys[0]), &entries);

        let restored = store.read(root.path());
        assert!(restored.contains_key(&keys[0]));
        assert!(
            !restored.contains_key(&keys[1]),
            "a session in another shard is left to that shard's write"
        );
    }

    /// Two cache keys that hash to one shard.
    fn two_keys_in_one_shard() -> [String; 2] {
        let first = "session-0.jsonl".to_owned();
        let second = (1..)
            .map(|number| format!("session-{number}.jsonl"))
            .find(|key| shard_index(key) == shard_index(&first))
            .unwrap();
        [first, second]
    }

    /// The refresh thread reads a shard, the list's own load adds an entry
    /// to it, then the refresh writes what it read: the list's entry stays.
    #[test]
    fn a_merge_keeps_the_entries_another_merge_added_since_the_shard_was_read() {
        let base = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let store = SessionCacheStore::under(base.path(), identity(Source::Pi));
        let [refreshed, opened] = two_keys_in_one_shard();
        let index = shard_index(&refreshed);
        let read_by_the_refresh = store.read_shard(root.path(), index);
        assert!(read_by_the_refresh.is_empty());

        store.merge_into_shard(root.path(), index, HashMap::from([empty_entry(&opened)]));
        store.merge_into_shard(root.path(), index, HashMap::from([empty_entry(&refreshed)]));

        let mut expected = [refreshed.as_str(), opened.as_str()];
        expected.sort_unstable();
        assert_eq!(sorted_keys(&store.read_shard(root.path(), index)), expected);
    }

    /// A shard stamped for another provider or schema is skipped on its own;
    /// the other shards still restore.
    #[test]
    fn a_shard_at_another_magic_or_schema_reads_as_nothing_cached() {
        let base = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let store = SessionCacheStore::under(base.path(), identity(Source::Pi));
        let (keys, entries) = empty_entries_in_distinct_shards(3);
        store.write_every_shard(root.path(), &entries);
        let stamped = |magic, schema_version| SessionCacheFile {
            magic,
            schema_version,
            entries: HashMap::<String, SessionCacheEntry>::new(),
        };
        write_cache_file(
            &store
                .shard_path(root.path(), shard_index(&keys[1]))
                .unwrap(),
            &stamped(*b"BADMAGIC", store.identity.schema_version),
        );
        write_cache_file(
            &store
                .shard_path(root.path(), shard_index(&keys[2]))
                .unwrap(),
            &stamped(store.identity.magic, store.identity.schema_version + 1),
        );

        let restored = store.read(root.path());

        assert_eq!(sorted_keys(&restored), vec![keys[0].as_str()]);
    }

    /// A shard cut short by a crash mid-write, or overwritten by something
    /// else, is skipped on its own; the other shards still restore.
    #[test]
    fn a_corrupted_shard_reads_as_nothing_cached_and_the_others_restore() {
        let base = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let store = SessionCacheStore::under(base.path(), identity(Source::Pi));
        let (keys, entries) = empty_entries_in_distinct_shards(3);
        store.write_every_shard(root.path(), &entries);
        let garbage = store
            .shard_path(root.path(), shard_index(&keys[1]))
            .unwrap();
        std::fs::write(&garbage, b"not a valid shard").unwrap();
        let truncated = store
            .shard_path(root.path(), shard_index(&keys[2]))
            .unwrap();
        let bytes = std::fs::read(&truncated).unwrap();
        std::fs::write(&truncated, &bytes[..bytes.len() / 2]).unwrap();

        let restored = store.read(root.path());

        assert_eq!(sorted_keys(&restored), vec![keys[0].as_str()]);
    }

    /// The read checks a cache file's header at fixed offsets, so a written
    /// shard must open with its magic and little-endian schema version.
    #[test]
    fn a_cache_file_opens_with_its_magic_and_schema_version() {
        let base = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let store = SessionCacheStore::under(base.path(), identity(Source::Pi));
        let (keys, entries) = empty_entries_in_distinct_shards(1);
        store.write_every_shard(root.path(), &entries);
        let shard = store
            .shard_path(root.path(), shard_index(&keys[0]))
            .unwrap();

        let shard_bytes = std::fs::read(shard).unwrap();

        let shard_header =
            session_cache_header(store.identity.magic, store.identity.schema_version);
        assert!(shard_bytes.starts_with(&shard_header));
    }

    /// Reading or writing a root's shards removes the temp files an
    /// interrupted write left in its directory, and leaves a write in
    /// progress and every other file alone.
    #[test]
    fn reading_or_writing_shards_removes_leftover_temp_files() {
        for write in [false, true] {
            let base = tempfile::tempdir().unwrap();
            let root = tempfile::tempdir().unwrap();
            let store = SessionCacheStore::under(base.path(), identity(Source::Pi));
            let directory = store.directory_for_root(root.path()).unwrap();
            let fixture = TempFileFixture::in_directory(&directory);

            if write {
                let (_, entries) = empty_entries_in_distinct_shards(1);
                store.write_every_shard(root.path(), &entries);
            } else {
                store.read(root.path());
            }

            fixture.assert_swept();
        }
    }

    fn write_sessions_bin(
        store: &SessionCacheStore,
        root: &Path,
        entries: &HashMap<String, SessionCacheEntry>,
    ) -> PathBuf {
        write_sessions_bin_at_schema(store, root, store.identity.schema_version, entries)
    }

    fn write_sessions_bin_at_schema(
        store: &SessionCacheStore,
        root: &Path,
        schema_version: u32,
        entries: &HashMap<String, SessionCacheEntry>,
    ) -> PathBuf {
        let path = store
            .directory_for_root(root)
            .unwrap()
            .join(SESSIONS_BIN_FILE_NAME);
        write_cache_file(
            &path,
            &SessionCacheFile {
                magic: store.identity.magic,
                schema_version,
                entries: entries.clone(),
            },
        );
        path
    }

    /// The first read after upgrading finds `sessions.bin`. Ignoring it would
    /// cost a full reparse of the root, so it is migrated into the shards.
    #[test]
    fn a_sessions_bin_is_migrated_into_shards_and_removed() {
        let base = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let store = SessionCacheStore::under(base.path(), identity(Source::Pi));
        let (_, entries) = empty_entries_in_distinct_shards(3);
        let sessions_bin = write_sessions_bin(&store, root.path(), &entries);

        let migrated = store.read(root.path());

        assert_eq!(sorted_keys(&migrated), sorted_keys(&entries));
        assert!(
            !sessions_bin.exists(),
            "`sessions.bin` is gone once its shards are written"
        );
        assert_eq!(sorted_keys(&store.read(root.path())), sorted_keys(&entries));
    }

    /// A release before sharding, run after shards were written, writes
    /// `sessions.bin` again with what it listed. That file is the newer record,
    /// so it replaces the shards.
    #[test]
    fn a_sessions_bin_written_after_the_shards_replaces_them() {
        let base = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let store = SessionCacheStore::under(base.path(), identity(Source::Pi));
        let keys = keys_in_distinct_shards(2);
        let (in_shards, in_sessions_bin) = (&keys[0], &keys[1]);
        store.write_every_shard(root.path(), &HashMap::from([empty_entry(in_shards)]));
        let sessions_bin = write_sessions_bin(
            &store,
            root.path(),
            &HashMap::from([empty_entry(in_sessions_bin)]),
        );

        let migrated = store.read(root.path());

        assert_eq!(sorted_keys(&migrated), vec![in_sessions_bin.as_str()]);
        assert!(!sessions_bin.exists());
        assert_eq!(
            sorted_keys(&store.read(root.path())),
            vec![in_sessions_bin.as_str()],
            "the shard written before `sessions.bin` is cleared"
        );
    }

    /// A downgrade past a future schema bump leaves a `sessions.bin` this
    /// release cannot read beside shards it can. The shards keep the cache.
    #[test]
    fn a_sessions_bin_at_another_schema_is_removed_and_the_shards_still_restore() {
        let base = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let store = SessionCacheStore::under(base.path(), identity(Source::Pi));
        let entries = HashMap::from([empty_entry("session.jsonl")]);
        store.write_every_shard(root.path(), &entries);
        let sessions_bin = write_sessions_bin_at_schema(
            &store,
            root.path(),
            store.identity.schema_version + 1,
            &HashMap::from([empty_entry("newer.jsonl")]),
        );

        let first_read = store.read(root.path());
        let second_read = store.read(root.path());

        assert_eq!(sorted_keys(&first_read), vec!["session.jsonl"]);
        assert!(!sessions_bin.exists());
        assert_eq!(
            sorted_keys(&second_read),
            vec!["session.jsonl"],
            "the shards restore once `sessions.bin` is gone"
        );
    }

    #[test]
    fn session_cache_round_trips_through_disk() {
        let base = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let pi = SessionCacheStore::under(base.path(), identity(Source::Pi));
        let omp = SessionCacheStore::under(base.path(), identity(Source::Omp));
        let mtime = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let file_size = 4_096;
        let mut entries = HashMap::new();
        entries.insert(
            "nested/session.jsonl".to_owned(),
            SessionCacheEntry::Listed(ListedSessionEntry {
                fingerprint: CachedFingerprint::of(file_size, mtime),
                conversation: cached_conversation(&make_test_conversation()),
                session_id: "session-1".to_owned(),
                subagents: vec![PathBuf::from("/sessions/agents/agent-0/wire.jsonl")],
                project_path: PathBuf::from("/tmp/project"),
            }),
        );
        entries.insert(
            "nested/holds-nothing.jsonl".to_owned(),
            SessionCacheEntry::Empty(CachedFingerprint::of(64, mtime)),
        );

        pi.write_every_shard(root.path(), &entries);
        let restored = pi.read(root.path());

        let entry = restored
            .get("nested/session.jsonl")
            .expect("entries are keyed by path relative to the root");
        assert!(entry.fingerprint().matches(file_size, mtime));
        let SessionCacheEntry::Listed(listed) = entry else {
            panic!("a listed session restores as listed");
        };
        assert_eq!(listed.session_id, "session-1");
        assert_eq!(
            listed.subagents,
            vec![PathBuf::from("/sessions/agents/agent-0/wire.jsonl")],
            "the sub-agent transcripts must survive a cache hit, or the view has nothing to splice"
        );
        assert_eq!(listed.project_path, PathBuf::from("/tmp/project"));
        assert_eq!(listed.conversation.full_text, "Hello world Hi there");

        let empty = restored
            .get("nested/holds-nothing.jsonl")
            .expect("a session that holds no conversation keeps its record");
        assert!(matches!(empty, SessionCacheEntry::Empty(_)));
        assert!(empty.fingerprint().matches(64, mtime));

        assert!(
            omp.read(root.path()).is_empty(),
            "OMP must not read Pi's cache for the same root"
        );
    }

    fn contains_segments(path: &std::path::Path, expected: &[&str]) -> bool {
        let segments = path
            .components()
            .map(|component| component.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        expected
            .iter()
            .all(|wanted| segments.iter().any(|segment| segment == wanted))
    }

    fn file_stem_of_parent(path: &std::path::Path) -> String {
        path.parent()
            .and_then(|parent| parent.file_name())
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    #[test]
    fn roundtrip_cached_conversation_preserves_data() {
        let conv = make_test_conversation();

        let cached = cached_conversation(&conv);

        // Roundtrip back to Conversation
        let restored = conversation_from_cached(
            Source::Claude,
            &cached,
            PathBuf::from("/test/conv.jsonl"),
            false,
        );

        assert_eq!(restored.preview, conv.preview_first);
        assert_eq!(restored.preview_first, conv.preview_first);
        assert_eq!(restored.preview_last, conv.preview_last);
        assert_eq!(restored.full_text, conv.full_text);
        assert_eq!(restored.agent_search_text, conv.agent_search_text);
        assert_eq!(restored.semantic_turns, conv.semantic_turns);
        assert_eq!(restored.semantic_turn_ranges, conv.semantic_turn_ranges);
        assert_eq!(restored.search_text_lower, conv.search_text_lower);
        assert_eq!(restored.cwd, conv.cwd);
        assert_eq!(restored.message_count, conv.message_count);
        assert_eq!(restored.summary, conv.summary);
        assert_eq!(restored.custom_title, conv.custom_title);
        assert_eq!(restored.model, conv.model);
        assert_eq!(restored.total_tokens, conv.total_tokens);
        assert_eq!(restored.duration_minutes, conv.duration_minutes);
        // Timestamp roundtrips through milliseconds
        assert_eq!(
            restored.timestamp.timestamp_millis(),
            conv.timestamp.timestamp_millis()
        );
    }

    #[test]
    fn show_last_selects_correct_preview() {
        let cached = cached_conversation(&make_test_conversation());

        let first = conversation_from_cached(Source::Claude, &cached, PathBuf::new(), false);
        assert_eq!(first.preview, "Hello world ... Hi there");

        let last = conversation_from_cached(Source::Claude, &cached, PathBuf::new(), true);
        assert_eq!(last.preview, "Hi there ... Hello world");
    }

    /// The stamp stands for one file's contents at one moment: a size or an
    /// mtime that moved must not match, or a changed transcript restores stale.
    #[test]
    fn a_fingerprint_matches_only_the_size_and_mtime_it_was_taken_from() {
        let mtime = UNIX_EPOCH + Duration::from_secs(1700000000) + Duration::from_nanos(123456789);
        let fingerprint = CachedFingerprint::of(500, mtime);

        assert!(fingerprint.matches(500, mtime));
        assert!(!fingerprint.matches(501, mtime));
        assert!(!fingerprint.matches(500, mtime + Duration::from_secs(1)));
        assert!(
            !fingerprint.matches(500, UNIX_EPOCH + Duration::from_secs(1700000000)),
            "the sub-second part of the mtime is part of the stamp"
        );
    }
}
