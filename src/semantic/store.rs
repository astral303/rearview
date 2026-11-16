use crate::clock::Clock;
use crate::semantic::cache::embedding_cache_file_path;
use crate::semantic::embed::is_valid_embedding;
use crate::semantic::types::{
    CACHE_SCHEMA_VERSION, ChunkConfig, MODEL_DIMENSIONS, MODEL_NAME, SemanticChunk,
};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::Path;

pub(crate) const MAX_CACHE_ENTRIES: usize = 50_000;

/// The key of the passage `text` in a [`PassageStore`].
pub fn passage_key(text: &str) -> String {
    blake3::hash(text.as_bytes()).to_hex().to_string()
}

/// The stored embedding of each passage, by [`passage_key`].
pub struct PassageStore {
    cache: EmbeddingCache,
}

#[derive(Serialize, Deserialize)]
struct EmbeddingCache {
    schema_version: u32,
    model: String,
    chunk_target_chars: usize,
    chunk_overlap_chars: usize,
    chunk_context_turns: usize,
    access_counter: u64,
    entries: HashMap<String, CachedChunk>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
struct CachedChunk {
    embedding: Vec<f32>,
    last_used: u64,
    protected: bool,
}

impl PassageStore {
    /// The store for `config`, read after sweeping the cache directory of
    /// temp files stale at `clock`'s time.
    pub fn read(config: ChunkConfig, clock: &dyn Clock) -> Self {
        let Some(path) = embedding_cache_file_path() else {
            return Self::empty(config);
        };
        Self::read_from_path(&path, config, MODEL_DIMENSIONS, clock)
    }

    fn read_from_path(
        path: &Path,
        config: ChunkConfig,
        dimensions: usize,
        clock: &dyn Clock,
    ) -> Self {
        let Some(data) =
            crate::cache_file::read_if_header_matches(path, &embedding_cache_header(config), clock)
        else {
            return Self::empty(config);
        };
        let Ok(cache) = bincode::deserialize::<EmbeddingCache>(&data) else {
            return Self::empty(config);
        };
        let mut store = Self { cache };
        store.drop_invalid_entries(dimensions);
        if store.cache.entries.len() > MAX_CACHE_ENTRIES {
            store.prune();
            store.write_to_path(path, clock);
        }
        store
    }

    pub fn empty(config: ChunkConfig) -> Self {
        Self {
            cache: EmbeddingCache {
                schema_version: CACHE_SCHEMA_VERSION,
                model: MODEL_NAME.to_string(),
                chunk_target_chars: config.target_chars,
                chunk_overlap_chars: config.overlap_chars,
                chunk_context_turns: config.context_turns,
                access_counter: 0,
                entries: HashMap::new(),
            },
        }
    }

    /// Write the store, sweeping the cache directory of temp files stale at
    /// `clock`'s time.
    pub fn write(&self, clock: &dyn Clock) {
        let Some(path) = embedding_cache_file_path() else {
            return;
        };
        self.write_to_path(&path, clock);
    }

    fn write_to_path(&self, path: &Path, clock: &dyn Clock) {
        if let Ok(data) = bincode::serialize(&self.cache) {
            crate::cache_file::write_atomically(path, &data, clock);
        }
    }

    pub fn passage_count(&self) -> usize {
        self.cache.entries.len()
    }

    pub fn contains_text(&self, text: &str) -> bool {
        self.cache.entries.contains_key(&passage_key(text))
    }

    /// The distinct texts of `chunks` without a stored embedding.
    pub fn missing_passage_count(&self, chunks: &[SemanticChunk]) -> usize {
        chunks
            .iter()
            .map(|chunk| passage_key(&chunk.text))
            .filter(|key| !self.cache.entries.contains_key(key))
            .collect::<HashSet<_>>()
            .len()
    }

    pub fn embedding_of(&self, key: &str) -> Option<&[f32]> {
        self.cache
            .entries
            .get(key)
            .map(|entry| entry.embedding.as_slice())
    }

    /// Start a run. [`Self::prune`] keeps the passages this run uses or
    /// inserts ahead of the passages earlier runs used.
    pub fn begin_run(&mut self) {
        self.cache.access_counter = self.cache.access_counter.saturating_add(1);
    }

    /// The embedding stored under `key`, marked as used by this run.
    pub fn use_embedding(&mut self, key: &str) -> Option<&[f32]> {
        let entry = self.cache.entries.get_mut(key)?;
        entry.last_used = self.cache.access_counter;
        Some(&entry.embedding)
    }

    /// Store `embedding` under `key`, marked as used by this run.
    pub fn insert(&mut self, key: String, embedding: Vec<f32>) {
        self.cache.entries.insert(
            key,
            CachedChunk {
                embedding,
                last_used: self.cache.access_counter,
                protected: false,
            },
        );
    }

    /// Delete the passages over [`MAX_CACHE_ENTRIES`]: unprotected before
    /// protected, the least recently used first.
    pub fn prune(&mut self) {
        self.prune_to_limit(MAX_CACHE_ENTRIES);
    }

    fn prune_to_limit(&mut self, max_entries: usize) {
        let excess = self.cache.entries.len().saturating_sub(max_entries);
        if excess == 0 {
            return;
        }
        let mut oldest = self
            .cache
            .entries
            .iter()
            .map(|(key, entry)| (entry.protected, entry.last_used, key.clone()))
            .collect::<Vec<_>>();
        oldest.sort_unstable();
        for (_, _, key) in oldest.into_iter().take(excess) {
            self.cache.entries.remove(&key);
        }
    }

    /// Remove the protection of every passage whose key is not in `keys`.
    pub fn unprotect_all_but(&mut self, keys: &HashSet<String>) {
        for (key, entry) in &mut self.cache.entries {
            entry.protected &= keys.contains(key);
        }
    }

    /// Protect the stored passages of `keys`.
    pub fn protect(&mut self, keys: &HashSet<String>) {
        for key in keys {
            if let Some(entry) = self.cache.entries.get_mut(key) {
                entry.protected = true;
            }
        }
    }

    /// Drop an entry unless its embedding is `dimensions` finite values. Its
    /// passage is then embedded again instead of failing every ranking.
    fn drop_invalid_entries(&mut self, dimensions: usize) {
        self.cache
            .entries
            .retain(|_, entry| is_valid_embedding(&entry.embedding, dimensions));
    }
}

/// The bytes an embedding cache for `config` opens with: `EmbeddingCache`'s
/// schema version, model and chunking, laid out as bincode writes the
/// struct's leading fields. A cache for any other opens differently and is
/// rejected after its first bytes.
fn embedding_cache_header(config: ChunkConfig) -> Vec<u8> {
    bincode::serialize(&(
        CACHE_SCHEMA_VERSION,
        MODEL_NAME,
        config.target_chars,
        config.overlap_chars,
        config.context_turns,
    ))
    .expect("integers and a string serialize")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::TestClock;

    /// The length of every embedding in these tests.
    const FAKE_DIMENSIONS: usize = 2;

    fn cached() -> CachedChunk {
        CachedChunk {
            embedding: vec![0.5, 0.5],
            last_used: 0,
            protected: false,
        }
    }

    fn store_text(store: &mut PassageStore, text: &str) {
        store.cache.entries.insert(passage_key(text), cached());
    }

    #[test]
    fn cache_pruning_keeps_most_recent_entries() {
        let mut store = PassageStore::empty(ChunkConfig::default());
        for (text, last_used) in [("oldest", 1), ("middle", 2), ("newest", 3)] {
            store.cache.entries.insert(
                passage_key(text),
                CachedChunk {
                    embedding: vec![last_used as f32],
                    last_used,
                    protected: false,
                },
            );
        }

        store.prune_to_limit(2);

        assert_eq!(store.passage_count(), 2);
        assert!(!store.contains_text("oldest"));
        assert!(store.contains_text("middle"));
        assert!(store.contains_text("newest"));
    }

    #[test]
    fn cache_pruning_preserves_protected_core_entries() {
        let mut store = PassageStore::empty(ChunkConfig::default());
        store_text(&mut store, "adaptive newest");
        store
            .cache
            .entries
            .get_mut(&passage_key("adaptive newest"))
            .unwrap()
            .last_used = 10;
        store_text(&mut store, "core oldest");
        let core = store
            .cache
            .entries
            .get_mut(&passage_key("core oldest"))
            .unwrap();
        core.last_used = 1;
        core.protected = true;

        store.prune_to_limit(1);

        assert!(store.contains_text("core oldest"));
        assert!(!store.contains_text("adaptive newest"));
    }

    /// The read checks an embedding cache's header at its first bytes, so a
    /// written cache must open with that header.
    #[test]
    fn embedding_cache_opens_with_its_schema_model_and_chunking() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("cache.bin");
        let config = ChunkConfig::default();
        let mut store = PassageStore::empty(config);
        store_text(&mut store, "cached text");
        store.write_to_path(&path, &TestClock::default());

        let bytes = std::fs::read(&path).unwrap();

        assert!(bytes.starts_with(&embedding_cache_header(config)));
    }

    #[test]
    fn embedding_cache_read_or_write_removes_leftover_temp_files() {
        use crate::cache_file::test_support::TempFileFixture;
        let config = ChunkConfig::default();
        for write in [false, true] {
            let dir = tempfile::tempdir().expect("tempdir");
            let path = dir.path().join("cache.bin");
            let clock = TestClock::default();
            let fixture = TempFileFixture::in_directory(dir.path(), &clock);

            if write {
                PassageStore::empty(config).write_to_path(&path, &clock);
            } else {
                PassageStore::read_from_path(&path, config, FAKE_DIMENSIONS, &clock);
            }

            fixture.assert_swept();
        }
    }

    #[test]
    fn wrong_schema_cache_is_ignored() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("cache.bin");
        let config = ChunkConfig::default();
        let mut store = PassageStore::empty(config);
        store.cache.schema_version = CACHE_SCHEMA_VERSION - 1;
        store_text(&mut store, "stale");
        store.write_to_path(&path, &TestClock::default());

        let restored =
            PassageStore::read_from_path(&path, config, FAKE_DIMENSIONS, &TestClock::default());

        assert_eq!(restored.cache.schema_version, CACHE_SCHEMA_VERSION);
        assert_eq!(restored.passage_count(), 0);
    }

    #[test]
    fn cache_for_another_chunking_is_ignored() {
        let written = ChunkConfig::default();
        let other_chunkings = [
            ChunkConfig {
                target_chars: written.target_chars + 1,
                ..written
            },
            ChunkConfig {
                overlap_chars: written.overlap_chars + 1,
                ..written
            },
            ChunkConfig {
                context_turns: written.context_turns + 1,
                ..written
            },
        ];
        for read in other_chunkings {
            let dir = tempfile::tempdir().expect("tempdir");
            let path = dir.path().join("cache.bin");
            let mut store = PassageStore::empty(written);
            store_text(&mut store, "chunked differently");
            store.write_to_path(&path, &TestClock::default());

            let restored =
                PassageStore::read_from_path(&path, read, FAKE_DIMENSIONS, &TestClock::default());

            assert_eq!(restored.passage_count(), 0, "{read:?}");
            assert_eq!(restored.cache.chunk_target_chars, read.target_chars);
            assert_eq!(restored.cache.chunk_overlap_chars, read.overlap_chars);
            assert_eq!(restored.cache.chunk_context_turns, read.context_turns);
        }
    }

    #[test]
    fn previous_cache_layout_is_ignored() {
        #[derive(serde::Serialize)]
        struct PreviousCache {
            schema_version: u32,
            model: String,
            chunk_target_chars: usize,
            chunk_overlap_chars: usize,
            chunk_context_turns: usize,
            entries: HashMap<String, PreviousEntry>,
        }

        #[derive(serde::Serialize)]
        struct PreviousEntry {
            file_size: u64,
            mtime_secs: u64,
            mtime_nsecs: u32,
            text: String,
            embedding: Vec<f32>,
        }

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("cache.bin");
        let config = ChunkConfig::default();
        let previous = PreviousCache {
            schema_version: CACHE_SCHEMA_VERSION - 1,
            model: MODEL_NAME.to_string(),
            chunk_target_chars: config.target_chars,
            chunk_overlap_chars: config.overlap_chars,
            chunk_context_turns: config.context_turns,
            entries: HashMap::from([(
                "session:0".to_string(),
                PreviousEntry {
                    file_size: 10,
                    mtime_secs: 20,
                    mtime_nsecs: 30,
                    text: "cached text".to_string(),
                    embedding: vec![0.5, 0.5],
                },
            )]),
        };
        std::fs::write(&path, bincode::serialize(&previous).unwrap()).unwrap();

        let restored =
            PassageStore::read_from_path(&path, config, FAKE_DIMENSIONS, &TestClock::default());

        assert_eq!(restored.cache.schema_version, CACHE_SCHEMA_VERSION);
        assert_eq!(restored.passage_count(), 0);
    }

    #[test]
    fn corrupt_cache_is_ignored() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("cache.bin");
        let config = ChunkConfig::default();
        std::fs::write(&path, b"not bincode").expect("write corrupt cache");

        let restored =
            PassageStore::read_from_path(&path, config, FAKE_DIMENSIONS, &TestClock::default());

        assert_eq!(restored.cache.schema_version, CACHE_SCHEMA_VERSION);
        assert_eq!(restored.passage_count(), 0);
    }

    #[test]
    fn mismatched_config_cache_is_ignored_when_read() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("cache.bin");
        let config = ChunkConfig::default();
        let mut store = PassageStore::empty(config);
        store.cache.chunk_overlap_chars += 1;
        store_text(&mut store, "stale");
        store.write_to_path(&path, &TestClock::default());

        let restored =
            PassageStore::read_from_path(&path, config, FAKE_DIMENSIONS, &TestClock::default());

        assert_eq!(restored.cache.chunk_overlap_chars, config.overlap_chars);
        assert_eq!(restored.passage_count(), 0);
    }

    #[test]
    fn cache_round_trips_when_config_matches() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("cache.bin");
        let config = ChunkConfig::default();
        let mut store = PassageStore::empty(config);
        store_text(&mut store, "cached text");

        store.write_to_path(&path, &TestClock::default());
        let restored =
            PassageStore::read_from_path(&path, config, FAKE_DIMENSIONS, &TestClock::default());

        assert!(restored.contains_text("cached text"));
    }

    #[test]
    fn invalid_cached_embeddings_are_dropped_when_cache_is_read() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("cache.bin");
        let config = ChunkConfig::default();
        let mut store = PassageStore::empty(config);
        store_text(&mut store, "valid");
        store_text(&mut store, "also valid");
        let invalid = [
            ("not a number", vec![f32::NAN, 0.5]),
            ("empty", Vec::new()),
            ("shorter", vec![0.5]),
        ];
        for (text, embedding) in invalid.clone() {
            store.cache.entries.insert(
                passage_key(text),
                CachedChunk {
                    embedding,
                    ..cached()
                },
            );
        }
        store.write_to_path(&path, &TestClock::default());

        let restored =
            PassageStore::read_from_path(&path, config, FAKE_DIMENSIONS, &TestClock::default());

        assert!(restored.contains_text("valid"));
        assert!(restored.contains_text("also valid"));
        for (text, _) in invalid {
            assert!(!restored.contains_text(text), "{text}");
        }
    }

    #[test]
    fn cached_embeddings_of_another_length_are_dropped_when_most_entries_share_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("cache.bin");
        let config = ChunkConfig::default();
        let mut store = PassageStore::empty(config);
        store_text(&mut store, "valid");
        let longer = ["longer one", "longer two", "longer three"];
        for text in longer {
            store.cache.entries.insert(
                passage_key(text),
                CachedChunk {
                    embedding: vec![0.5, 0.5, 0.5],
                    ..cached()
                },
            );
        }
        store.write_to_path(&path, &TestClock::default());

        let restored =
            PassageStore::read_from_path(&path, config, FAKE_DIMENSIONS, &TestClock::default());

        assert!(restored.contains_text("valid"));
        for text in longer {
            assert!(!restored.contains_text(text), "{text}");
        }
    }
}
