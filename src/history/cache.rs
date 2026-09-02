//! Per-project binary cache for parsed conversation metadata.
//!
//! Stores parsed conversation data in bincode format, keyed by session filename
//! and validated by mtime + file size. Eliminates redundant JSONL parsing and
//! search text normalization on startup for unchanged files.

use super::provider::SessionCache;
use super::{Conversation, ParseError};
use crate::agent::refs::MessageRange;
use chrono::{Local, TimeZone};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const CACHE_MAGIC: [u8; 8] = *b"CLHIST01";
const SCHEMA_VERSION: u32 = 14;

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

#[derive(Serialize, Deserialize)]
struct SessionCacheFile {
    magic: [u8; 8],
    schema_version: u32,
    entries: HashMap<String, SessionCacheEntry>,
}

/// One session's entry in a provider's whole-root cache.
///
/// A transcript that holds no conversation has a fingerprint and nothing else:
/// a session id and a project path are read out of a conversation, and an empty
/// parse yields none. They are absent from `Empty` rather than blank, so no
/// reader can build a row or an agent key out of placeholder identity.
#[derive(Serialize, Deserialize, Clone)]
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

#[derive(Serialize, Deserialize)]
struct ProjectCache {
    magic: [u8; 8],
    schema_version: u32,
    entries: HashMap<String, ProjectCacheEntry>,
}

/// One session's entry in a Claude project cache. Claude reads the identity a
/// provider entry stores from the transcript's own path and cwd, and names
/// the sub-agent transcripts from the session directory on every load, so a
/// listed entry here is its fingerprint and its content.
#[derive(Serialize, Deserialize, Clone)]
pub enum ProjectCacheEntry {
    Listed {
        /// Spans the session and its sub-agent transcripts since schema 14;
        /// see [`Fingerprint::spanning`](crate::history::provider::Fingerprint::spanning).
        fingerprint: CachedFingerprint,
        conversation: CachedConversation,
    },
    Empty(CachedFingerprint),
}

impl ProjectCacheEntry {
    pub fn fingerprint(&self) -> CachedFingerprint {
        match self {
            Self::Listed { fingerprint, .. } => *fingerprint,
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

/// Root of every cache this tool writes: `$REARVIEW_CACHE_DIR`, or
/// `~/.cache/rearview`, or `None` without a home directory.
///
/// The override exists so tests that spawn the binary keep cache writes out of
/// the user's real cache; it also lets a user relocate the cache outright.
fn user_cache_base() -> Option<PathBuf> {
    cache_base_from(std::env::var_os("REARVIEW_CACHE_DIR"), home::home_dir())
}

fn cache_base_from(
    override_dir: Option<std::ffi::OsString>,
    home: Option<PathBuf>,
) -> Option<PathBuf> {
    match override_dir.filter(|value| !value.is_empty()) {
        Some(directory) => Some(PathBuf::from(directory)),
        None => Some(home?.join(".cache").join(crate::APP_NAME)),
    }
}

/// The per-project Claude cache directory: `projects/` under the user cache
/// base, namespaced by `CLAUDE_CONFIG_DIR` so two config roots do not share
/// an entry. `None` without a home directory. The load path takes the
/// directory as a parameter, so a test can point it at one of its own.
pub fn project_cache_dir() -> Option<PathBuf> {
    let base = user_cache_base()?;
    if let Ok(config_dir) = std::env::var("CLAUDE_CONFIG_DIR") {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        std::hash::Hash::hash(&config_dir, &mut hasher);
        let hash = std::hash::Hasher::finish(&hasher);
        Some(base.join(format!("config-{:016x}", hash)).join("projects"))
    } else {
        Some(base.join("projects"))
    }
}

fn cache_path_for_project(cache_dir: &Path, project_dir_name: &str) -> PathBuf {
    cache_dir.join(format!("{project_dir_name}.bin"))
}

/// Read a project's cache file, returning entries keyed by session filename.
/// Returns None on any failure (missing, corrupt, version mismatch).
pub fn read_project_cache(
    cache_dir: &Path,
    project_dir_name: &str,
) -> Option<HashMap<String, ProjectCacheEntry>> {
    let path = cache_path_for_project(cache_dir, project_dir_name);
    let data = std::fs::read(&path).ok()?;
    if data.len() < 12 {
        return None;
    }
    if data[..8] != CACHE_MAGIC {
        return None;
    }
    let cache: ProjectCache = bincode::deserialize(&data).ok()?;
    if cache.schema_version != SCHEMA_VERSION {
        return None;
    }
    Some(cache.entries)
}

/// Write a project's cache file atomically (temp file + rename).
/// Uses tempfile for safe concurrent writes. Silently ignores failures.
pub fn write_project_cache(
    cache_dir: &Path,
    project_dir_name: &str,
    entries: HashMap<String, ProjectCacheEntry>,
) {
    write_cache_file(
        &cache_path_for_project(cache_dir, project_dir_name),
        &ProjectCache {
            magic: CACHE_MAGIC,
            schema_version: SCHEMA_VERSION,
            entries,
        },
    );
}

fn write_cache_file(path: &std::path::Path, cache: &impl Serialize) {
    let Some(parent) = path.parent() else {
        return;
    };
    let _ = std::fs::create_dir_all(parent);
    let Ok(data) = bincode::serialize(cache) else {
        return;
    };
    let Ok(mut tmp) = tempfile::NamedTempFile::new_in(parent) else {
        return;
    };
    if tmp.write_all(&data).is_err() {
        return;
    }
    let _ = tmp.persist(path);
}

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
            directory: user_cache_base().map(|base| base.join(identity.directory)),
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

    /// Where `root`'s cache file lives.
    ///
    /// Roots are hashed rather than embedded so two roots never collide and a
    /// moved root simply misses instead of reading a stale neighbour's entries.
    fn path_for_root(&self, root: &std::path::Path) -> Option<PathBuf> {
        use std::hash::{Hash, Hasher};

        let resolved = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        resolved.hash(&mut hasher);
        Some(
            self.directory
                .as_ref()?
                .join(format!("root-{:016x}", hasher.finish()))
                .join("sessions.bin"),
        )
    }

    /// Cached entries for every session under `root`, keyed by path relative to
    /// it. Absent, unreadable, and stamped for another provider or schema all
    /// read as nothing cached.
    pub fn read(&self, root: &std::path::Path) -> HashMap<String, SessionCacheEntry> {
        let stored = self
            .path_for_root(root)
            .and_then(|path| std::fs::read(path).ok())
            .and_then(|data| bincode::deserialize::<SessionCacheFile>(&data).ok())
            .filter(|cache| {
                cache.magic == self.identity.magic
                    && cache.schema_version == self.identity.schema_version
            });
        stored.map(|cache| cache.entries).unwrap_or_default()
    }

    pub fn write(&self, root: &std::path::Path, entries: HashMap<String, SessionCacheEntry>) {
        let Some(path) = self.path_for_root(root) else {
            return;
        };
        // Nothing to cache and nothing cached: leave no trace, so an absent or
        // empty root does not grow a cache directory. An existing file is still
        // overwritten, clearing entries for sessions that no longer exist.
        if entries.is_empty() && !path.exists() {
            return;
        }
        write_cache_file(
            &path,
            &SessionCacheFile {
                magic: self.identity.magic,
                schema_version: self.identity.schema_version,
                entries,
            },
        );
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

/// Reconstruct a Conversation from a CachedConversation
pub fn conversation_from_cached(
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
        source: super::Source::Claude,
        subagents: Vec::new(),
        session_id: path
            .file_stem()
            .and_then(|name| name.to_str())
            .unwrap_or_default()
            .to_owned(),
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
        }
    }

    fn identity(source: Source) -> SessionCache {
        source
            .provider()
            .storage()
            .expect("this source caches whole roots")
            .cache()
    }

    #[test]
    fn the_cache_base_override_replaces_the_home_derived_default() {
        let home = PathBuf::from("/home/user");
        assert_eq!(
            cache_base_from(None, Some(home.clone())),
            Some(home.join(".cache").join("rearview"))
        );
        assert_eq!(
            cache_base_from(Some("/isolated/cache".into()), Some(home.clone())),
            Some(PathBuf::from("/isolated/cache")),
            "the override wins even when a home directory exists"
        );
        assert_eq!(
            cache_base_from(Some("".into()), Some(home.clone())),
            Some(home.join(".cache").join("rearview")),
            "an empty override means unset"
        );
        assert_eq!(cache_base_from(None, None), None);
    }

    /// A run over an absent or empty root must not grow the cache directory:
    /// every isolated test run and every user without the agent installed
    /// would otherwise leave a `root-<hash>` directory behind.
    #[test]
    fn an_empty_cache_write_leaves_no_file_but_still_clears_an_existing_one() {
        let base = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let store = SessionCacheStore::under(base.path(), identity(Source::Pi));
        let path = store.path_for_root(root.path()).unwrap();

        store.write(root.path(), HashMap::new());
        assert!(!path.exists(), "nothing cached and nothing to cache");

        let mut entries = HashMap::new();
        entries.insert(
            "session.jsonl".to_owned(),
            SessionCacheEntry::Empty(CachedFingerprint::of(0, SystemTime::UNIX_EPOCH)),
        );
        store.write(root.path(), entries);
        assert!(path.exists());

        store.write(root.path(), HashMap::new());
        assert!(
            path.exists(),
            "an existing cache is cleared, not orphaned, when the root empties"
        );
        assert!(store.read(root.path()).is_empty());
    }

    #[test]
    fn session_cache_roots_are_isolated_from_claude_and_each_other() {
        let base = tempfile::tempdir().unwrap();
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        let pi = SessionCacheStore::under(base.path(), identity(Source::Pi));
        let omp = SessionCacheStore::under(base.path(), identity(Source::Omp));

        let first_path = pi.path_for_root(first.path()).unwrap();
        let second_path = pi.path_for_root(second.path()).unwrap();
        let omp_path = omp.path_for_root(first.path()).unwrap();
        let claude_path = cache_path_for_project(&project_cache_dir().unwrap(), "same-project");

        assert_ne!(first_path, second_path);
        assert_ne!(first_path, claude_path);
        assert_ne!(first_path, omp_path);
        assert!(contains_segments(&first_path, &["pi"]));
        assert!(contains_segments(&omp_path, &["omp"]));
        assert!(contains_segments(&claude_path, &["projects"]));
        assert!(
            file_stem_of_parent(&first_path).starts_with("root-"),
            "a session cache lives in a directory named for the hashed root"
        );
    }

    /// Where a root's cache lives is a compatibility contract: users carry caches
    /// across upgrades, and moving or renaming the file silently discards them.
    #[test]
    fn session_cache_filenames_keep_their_shape() {
        let root = tempfile::tempdir().unwrap();
        let store = SessionCacheStore::in_user_cache(identity(Source::Pi));
        let path = store
            .path_for_root(root.path())
            .expect("caching needs a home directory");

        assert_eq!(path.file_name().unwrap(), "sessions.bin");
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
            store.path_for_root(root.path()).as_ref(),
            Some(&path),
            "a root must resolve to the same file on every run, or its cache is lost"
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

        pi.write(root.path(), entries);
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
        let restored = conversation_from_cached(&cached, PathBuf::from("/test/conv.jsonl"), false);

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

        let first = conversation_from_cached(&cached, PathBuf::new(), false);
        assert_eq!(first.preview, "Hello world ... Hi there");

        let last = conversation_from_cached(&cached, PathBuf::new(), true);
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

    #[test]
    fn cache_file_roundtrip() {
        let cache_dir = tempfile::tempdir().unwrap();
        let conv = make_test_conversation();
        let mtime = UNIX_EPOCH + Duration::from_secs(1700000000);
        let mut entries = HashMap::new();
        entries.insert(
            "conv1.jsonl".to_string(),
            ProjectCacheEntry::Listed {
                fingerprint: CachedFingerprint::of(42000, mtime),
                conversation: cached_conversation(&conv),
            },
        );
        entries.insert(
            "empty.jsonl".to_string(),
            ProjectCacheEntry::Empty(CachedFingerprint::of(100, mtime)),
        );

        write_project_cache(cache_dir.path(), "roundtrip", entries);

        let loaded = read_project_cache(cache_dir.path(), "roundtrip");
        assert!(loaded.is_some(), "Cache file should be readable");

        let loaded = loaded.unwrap();
        assert_eq!(loaded.len(), 2);

        let conv_entry = loaded.get("conv1.jsonl").unwrap();
        assert!(conv_entry.fingerprint().matches(42000, mtime));
        let ProjectCacheEntry::Listed { conversation, .. } = conv_entry else {
            panic!("a listed session restores as listed");
        };
        assert_eq!(conversation.full_text, "Hello world Hi there");
        assert_eq!(conversation.agent_search_text, "subagent cache text");
        assert_eq!(conversation.total_tokens, 1500);

        let empty = loaded.get("empty.jsonl").unwrap();
        assert!(matches!(empty, ProjectCacheEntry::Empty(_)));
        assert!(empty.fingerprint().matches(100, mtime));
    }

    #[test]
    fn corrupt_cache_returns_none() {
        let cache_dir = tempfile::tempdir().unwrap();
        std::fs::write(
            cache_path_for_project(cache_dir.path(), "corrupt"),
            b"not a valid cache file",
        )
        .unwrap();

        assert!(read_project_cache(cache_dir.path(), "corrupt").is_none());
    }

    #[test]
    fn wrong_version_returns_none() {
        let cache_dir = tempfile::tempdir().unwrap();
        let cache = ProjectCache {
            magic: CACHE_MAGIC,
            schema_version: SCHEMA_VERSION + 1,
            entries: HashMap::new(),
        };
        std::fs::write(
            cache_path_for_project(cache_dir.path(), "version"),
            bincode::serialize(&cache).unwrap(),
        )
        .unwrap();

        assert!(read_project_cache(cache_dir.path(), "version").is_none());
    }

    #[test]
    fn wrong_magic_returns_none() {
        let cache_dir = tempfile::tempdir().unwrap();
        let cache = ProjectCache {
            magic: *b"BADMAGIC",
            schema_version: SCHEMA_VERSION,
            entries: HashMap::new(),
        };
        std::fs::write(
            cache_path_for_project(cache_dir.path(), "magic"),
            bincode::serialize(&cache).unwrap(),
        )
        .unwrap();

        assert!(read_project_cache(cache_dir.path(), "magic").is_none());
    }

    #[test]
    fn missing_cache_returns_none() {
        let cache_dir = tempfile::tempdir().unwrap();

        assert!(read_project_cache(cache_dir.path(), "nonexistent").is_none());
    }
}
