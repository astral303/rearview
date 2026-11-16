use crate::error::{AppError, Result};
use crate::semantic::embed::{SemanticEmbedder, is_valid_embedding};
use crate::semantic::store::{PassageStore, passage_key};
#[cfg(test)]
use crate::semantic::types::EmbeddingBudget;
use crate::semantic::types::{
    DEFAULT_EMBEDDING_BATCH_SIZE, EmbeddedChunk, SemanticChunk, TimedBudget,
};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[cfg(test)]
pub fn embed_chunks_with_progress_and_save(
    embedder: &mut dyn SemanticEmbedder,
    chunks: Vec<SemanticChunk>,
    store: &mut PassageStore,
    cancellation: &crate::semantic::types::SemanticCancellationToken,
    progress: impl FnMut(usize, usize),
    save: impl FnMut(&PassageStore),
) -> Result<Vec<EmbeddedChunk>> {
    embed_chunks_with_budget_and_save(
        embedder,
        chunks,
        store,
        cancellation,
        EmbeddingBudget::unbounded().timed_on(&crate::clock::TestClock::default()),
        progress,
        save,
    )
    .map(|outcome| outcome.embedded)
}

pub struct EmbeddingOutcome {
    pub embedded: Vec<EmbeddedChunk>,
    /// Chunks left without an embedding by the budget, counted per chunk, so
    /// this and `embedded.len()` sum to the chunks passed in.
    pub missing_chunk_count: usize,
}

/// Embed the chunks missing from `store` within `budget`; the rest are
/// counted in the outcome. After each batch, check the time limit; once it
/// has passed, save the completed batches and stop.
pub fn embed_chunks_with_budget_and_save(
    embedder: &mut dyn SemanticEmbedder,
    chunks: Vec<SemanticChunk>,
    store: &mut PassageStore,
    cancellation: &crate::semantic::types::SemanticCancellationToken,
    budget: TimedBudget<'_>,
    mut progress: impl FnMut(usize, usize),
    mut save: impl FnMut(&PassageStore),
) -> Result<EmbeddingOutcome> {
    const SAVE_INTERVAL: usize = 256;

    if cancellation.is_cancelled() {
        return Err(AppError::SemanticSearchCancelled);
    }

    store.begin_run();
    let mut embedded = Vec::with_capacity(chunks.len());
    let mut hits = HashMap::<String, Arc<[f32]>>::new();
    let mut misses = Vec::<Vec<SemanticChunk>>::new();
    let mut miss_indices = HashMap::<String, usize>::new();

    for chunk in chunks {
        if cancellation.is_cancelled() {
            return Err(AppError::SemanticSearchCancelled);
        }
        let key = passage_key(&chunk.text);
        if let Some(embedding) = hits.get(&key) {
            embedded.push(embedded_chunk(&chunk, Arc::clone(embedding)));
        } else if let Some(stored) = store.use_embedding(&key) {
            let embedding = Arc::<[f32]>::from(stored);
            embedded.push(embedded_chunk(&chunk, Arc::clone(&embedding)));
            hits.insert(key, embedding);
        } else if let Some(index) = miss_indices.get(&key).copied() {
            misses[index].push(chunk);
        } else {
            miss_indices.insert(key, misses.len());
            misses.push(vec![chunk]);
        }
    }

    let dimensions = embedder.dimensions();
    let total_misses = budget.passages_within(misses.len());
    let mut missing_chunk_count: usize = misses[total_misses..].iter().map(Vec::len).sum();
    misses.truncate(total_misses);
    let mut completed = 0;
    let mut last_saved = 0;
    let started = budget.start();
    for batch in misses.chunks(DEFAULT_EMBEDDING_BATCH_SIZE) {
        if cancellation.is_cancelled() {
            save_pending_passages(store, completed, &mut last_saved, &mut save);
            return Err(AppError::SemanticSearchCancelled);
        }
        let texts = batch
            .iter()
            .map(|chunks| chunks[0].text.clone())
            .collect::<Vec<_>>();
        let embeddings = match embedder
            .embed_passages(&texts)
            .and_then(|embeddings| validate_passage_embeddings(&texts, embeddings, dimensions))
        {
            Ok(embeddings) => embeddings,
            Err(error) => {
                save_pending_passages(store, completed, &mut last_saved, &mut save);
                return Err(error);
            }
        };

        for (chunks, embedding) in batch.iter().zip(embeddings) {
            let shared = Arc::<[f32]>::from(embedding.as_slice());
            store.insert(passage_key(&chunks[0].text), embedding);
            for chunk in chunks {
                embedded.push(embedded_chunk(chunk, Arc::clone(&shared)));
            }
        }
        completed += batch.len();
        if completed == total_misses || completed - last_saved >= SAVE_INTERVAL {
            store.prune();
            save(store);
            last_saved = completed;
        }
        progress(completed, total_misses);
        if completed < total_misses && budget.time_is_up(started) {
            missing_chunk_count += misses[completed..].iter().map(Vec::len).sum::<usize>();
            save_pending_passages(store, completed, &mut last_saved, &mut save);
            break;
        }
    }

    Ok(EmbeddingOutcome {
        embedded,
        missing_chunk_count,
    })
}

/// One embedding per passage, each `dimensions` finite values; otherwise
/// none of the batch is cached.
fn validate_passage_embeddings(
    passages: &[String],
    embeddings: Vec<Vec<f32>>,
    dimensions: usize,
) -> Result<Vec<Vec<f32>>> {
    if embeddings.len() != passages.len() {
        return Err(AppError::InvalidEmbeddings(format!(
            "{} embeddings for {} passages",
            embeddings.len(),
            passages.len()
        )));
    }
    if !embeddings
        .iter()
        .all(|embedding| is_valid_embedding(embedding, dimensions))
    {
        return Err(AppError::InvalidEmbeddings(format!(
            "a passage embedding is not {dimensions} finite values"
        )));
    }
    Ok(embeddings)
}

fn save_pending_passages(
    store: &mut PassageStore,
    completed: usize,
    last_saved: &mut usize,
    save: &mut impl FnMut(&PassageStore),
) {
    if completed > *last_saved {
        store.prune();
        save(store);
        *last_saved = completed;
    }
}

fn embedded_chunk(chunk: &SemanticChunk, embedding: Arc<[f32]>) -> EmbeddedChunk {
    EmbeddedChunk {
        conversation_index: chunk.conversation_index,
        source: chunk.source,
        session: chunk.session.clone(),
        chunk_index: chunk.chunk_index,
        text: chunk.text.clone(),
        message_range: chunk.message_range,
        embedding,
    }
}

/// The chunks of `chunks` whose text has an embedding in `store`, and the
/// count of distinct texts without one.
pub fn cached_chunks(
    chunks: Vec<SemanticChunk>,
    store: &PassageStore,
    cancellation: &crate::semantic::types::SemanticCancellationToken,
) -> Result<(Vec<EmbeddedChunk>, usize)> {
    let mut embedded = Vec::with_capacity(chunks.len());
    let mut hits = HashMap::<String, Arc<[f32]>>::new();
    let mut misses = HashSet::new();

    for chunk in chunks {
        if cancellation.is_cancelled() {
            return Err(AppError::SemanticSearchCancelled);
        }
        let key = passage_key(&chunk.text);
        if let Some(embedding) = hits.get(&key) {
            embedded.push(embedded_chunk(&chunk, Arc::clone(embedding)));
        } else if let Some(stored) = store.embedding_of(&key) {
            let embedding = Arc::<[f32]>::from(stored);
            embedded.push(embedded_chunk(&chunk, Arc::clone(&embedding)));
            hits.insert(key, embedding);
        } else {
            misses.insert(key);
        }
    }

    Ok((embedded, misses.len()))
}

pub fn embedding_cache_file_path() -> Option<PathBuf> {
    embedding_cache_path()
}

/// The folder earlier releases downloaded the semantic model into.
pub(crate) fn old_model_cache_dir() -> PathBuf {
    semantic_cache_dir_with_fallback().join(OLD_MODEL_DIRECTORY)
}

/// Delete the semantic cache, skipping an entry that contains
/// `hugging_face_cache`. Returns the bytes freed, or `None` when there was
/// nothing to delete.
pub fn clear_semantic_cache_files(hugging_face_cache: &Path) -> std::io::Result<Option<u64>> {
    let Some(path) = semantic_cache_dir() else {
        return Ok(None);
    };
    clear_semantic_cache_in(&path, hugging_face_cache)
}

#[allow(clippy::disallowed_methods)]
fn clear_semantic_cache_in(
    semantic_dir: &Path,
    hugging_face_cache: &Path,
) -> std::io::Result<Option<u64>> {
    let entries = match std::fs::read_dir(semantic_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    // A missing Hugging Face cache isn't in any entry. `cache clear`
    // canonicalises it and each entry of rearview's own semantic cache once.
    let hugging_face_cache = match std::fs::canonicalize(hugging_face_cache) {
        Ok(path) => Some(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    let mut freed = None;
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if let Some(hugging_face_cache) = &hugging_face_cache
            && std::fs::canonicalize(&path).is_ok_and(|entry| hugging_face_cache.starts_with(entry))
        {
            continue;
        }
        let size = crate::cache_file::disk_usage(&path)?;
        if entry.file_type()?.is_dir() {
            std::fs::remove_dir_all(&path)?;
        } else {
            std::fs::remove_file(&path)?;
        }
        *freed.get_or_insert(0) += size;
    }
    Ok(freed)
}

const EMBEDDING_CACHE_FILE: &str = "embeddings-v1.bin";
const OLD_MODEL_DIRECTORY: &str = "fastembed";

fn embedding_cache_path() -> Option<PathBuf> {
    semantic_cache_dir().map(|path| path.join(EMBEDDING_CACHE_FILE))
}

fn semantic_cache_dir() -> Option<PathBuf> {
    crate::cache_file::user_cache_base().map(semantic_cache_dir_in)
}

/// Without a home folder or `REARVIEW_CACHE_DIR`, earlier releases
/// downloaded the model under the current directory.
fn semantic_cache_dir_with_fallback() -> PathBuf {
    semantic_cache_dir().unwrap_or_else(|| {
        semantic_cache_dir_in(crate::cache_file::cache_base_under(Path::new(".")))
    })
}

fn semantic_cache_dir_in(cache_base: PathBuf) -> PathBuf {
    cache_base.join("semantic")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::{Clock, TestClock};
    use crate::semantic::types::{
        ChunkConfig, DEFAULT_INTERACTIVE_EMBEDDING_SECONDS, SemanticCancellationToken,
    };
    use std::time::Duration;

    /// The length of every embedding in these tests.
    const FAKE_DIMENSIONS: usize = 2;

    struct FakeEmbedder {
        calls: usize,
    }

    impl SemanticEmbedder for FakeEmbedder {
        fn dimensions(&self) -> usize {
            FAKE_DIMENSIONS
        }

        fn embed_passages(&mut self, passages: &[String]) -> Result<Vec<Vec<f32>>> {
            self.calls += 1;
            Ok(passages
                .iter()
                .map(|text| vec![text.len() as f32, 1.0])
                .collect())
        }

        fn embed_query(&mut self, _query: &str) -> Result<Option<Vec<f32>>> {
            Ok(Some(vec![1.0, 0.0]))
        }
    }

    /// Spends `batch_takes` of `clock`'s time on each batch.
    struct SlowEmbedder<'c> {
        calls: usize,
        batch_takes: Duration,
        clock: &'c TestClock,
    }

    impl SemanticEmbedder for SlowEmbedder<'_> {
        fn dimensions(&self) -> usize {
            FAKE_DIMENSIONS
        }

        fn embed_passages(&mut self, passages: &[String]) -> Result<Vec<Vec<f32>>> {
            self.calls += 1;
            self.clock.sleep(self.batch_takes);
            Ok(passages.iter().map(|_| vec![1.0, 0.0]).collect())
        }

        fn embed_query(&mut self, _query: &str) -> Result<Option<Vec<f32>>> {
            Ok(Some(vec![1.0, 0.0]))
        }
    }

    struct CancellingEmbedder {
        cancellation: SemanticCancellationToken,
    }

    impl SemanticEmbedder for CancellingEmbedder {
        fn dimensions(&self) -> usize {
            FAKE_DIMENSIONS
        }

        fn embed_passages(&mut self, passages: &[String]) -> Result<Vec<Vec<f32>>> {
            self.cancellation.cancel();
            Ok(passages.iter().map(|_| vec![1.0, 0.0]).collect())
        }

        fn embed_query(&mut self, _query: &str) -> Result<Option<Vec<f32>>> {
            Ok(Some(vec![1.0, 0.0]))
        }
    }

    /// Returns `passages` for every batch, whatever it was given.
    struct FixedEmbedder {
        passages: Vec<Vec<f32>>,
    }

    impl SemanticEmbedder for FixedEmbedder {
        fn dimensions(&self) -> usize {
            FAKE_DIMENSIONS
        }

        fn embed_passages(&mut self, _passages: &[String]) -> Result<Vec<Vec<f32>>> {
            Ok(self.passages.clone())
        }

        fn embed_query(&mut self, _query: &str) -> Result<Option<Vec<f32>>> {
            Ok(Some(vec![1.0, 0.0]))
        }
    }

    /// A chunk of `text` from `identity`, written `session:chunk_index`.
    fn chunk(identity: &str, text: &str) -> SemanticChunk {
        let (session, chunk_index) = identity.rsplit_once(':').expect("session:chunk_index");
        SemanticChunk {
            conversation_index: 0,
            source: crate::semantic::types::SemanticChunkSource::VisibleDialogue,
            session: session.to_string(),
            chunk_index: chunk_index.parse().expect("chunk index"),
            text: text.to_string(),
            message_range: crate::agent::refs::MessageRange::single(1),
        }
    }

    fn store_text(store: &mut PassageStore, text: &str) {
        store.insert(passage_key(text), vec![0.5, 0.5]);
    }

    #[test]
    fn embed_chunks_respects_new_embedding_budget() {
        const BUDGET_BATCHES: usize = 4;
        const BUDGET: usize = BUDGET_BATCHES * DEFAULT_EMBEDDING_BATCH_SIZE;
        const MISSING: usize = BUDGET + 8;
        let config = ChunkConfig::default();
        let mut store = PassageStore::empty(config);
        store_text(&mut store, "cached text");
        let chunks = std::iter::once(chunk("session:0", "cached text"))
            .chain((1..=MISSING).map(|index| {
                chunk(
                    &format!("session:{index}"),
                    &format!("missing text {index}"),
                )
            }))
            .collect::<Vec<_>>();
        let mut embedder = FakeEmbedder { calls: 0 };

        let outcome = embed_chunks_with_budget_and_save(
            &mut embedder,
            chunks,
            &mut store,
            &SemanticCancellationToken::new(),
            EmbeddingBudget::passages(BUDGET).timed_on(&TestClock::default()),
            |_, _| {},
            |_| {},
        )
        .expect("embedding succeeds");

        assert_eq!(embedder.calls, BUDGET_BATCHES);
        assert_eq!(outcome.embedded.len(), BUDGET + 1);
        assert_eq!(outcome.missing_chunk_count, MISSING - BUDGET);
        assert_eq!(store.passage_count(), BUDGET + 1);
        assert!(store.contains_text(&format!("missing text {BUDGET}")));
        assert!(!store.contains_text(&format!("missing text {}", BUDGET + 1)));
    }

    #[test]
    fn missing_chunk_count_counts_every_chunk_of_repeated_text_beyond_budget() {
        let mut store = PassageStore::empty(ChunkConfig::default());
        let chunks = vec![
            chunk("session:0", "embedded text"),
            chunk("session:1", "repeated text"),
            chunk("session:2", "repeated text"),
        ];
        let mut embedder = FakeEmbedder { calls: 0 };

        let outcome = embed_chunks_with_budget_and_save(
            &mut embedder,
            chunks,
            &mut store,
            &SemanticCancellationToken::new(),
            EmbeddingBudget::passages(1).timed_on(&TestClock::default()),
            |_, _| {},
            |_| {},
        )
        .expect("embedding succeeds");

        assert_eq!(outcome.embedded.len(), 1);
        assert_eq!(outcome.missing_chunk_count, 2);
    }

    /// Each batch outlasts the default time limit: the first batch runs to
    /// its end and the second never starts.
    fn embed_past_the_time_limit(
        chunks: Vec<SemanticChunk>,
        store: &mut PassageStore,
        saved_passage_counts: &mut Vec<usize>,
    ) -> (EmbeddingOutcome, usize) {
        let clock = TestClock::default();
        let mut embedder = SlowEmbedder {
            calls: 0,
            batch_takes: Duration::from_secs(DEFAULT_INTERACTIVE_EMBEDDING_SECONDS + 1),
            clock: &clock,
        };
        let outcome = embed_chunks_with_budget_and_save(
            &mut embedder,
            chunks,
            store,
            &SemanticCancellationToken::new(),
            EmbeddingBudget::seconds(DEFAULT_INTERACTIVE_EMBEDDING_SECONDS).timed_on(&clock),
            |_, _| {},
            |store| saved_passage_counts.push(store.passage_count()),
        )
        .expect("embedding succeeds");
        (outcome, embedder.calls)
    }

    /// With a two-second limit and 1.5-second batches, the second batch starts
    /// at 1.5 s; at 3 s the limit has passed and the third does not start.
    #[test]
    fn batches_start_until_time_limit_has_passed() {
        const MISSING: usize = 4 * DEFAULT_EMBEDDING_BATCH_SIZE;
        let clock = TestClock::default();
        let mut embedder = SlowEmbedder {
            calls: 0,
            batch_takes: Duration::from_millis(1500),
            clock: &clock,
        };
        let mut store = PassageStore::empty(ChunkConfig::default());
        let chunks = (0..MISSING)
            .map(|index| chunk(&format!("session:{index}"), &format!("text {index}")))
            .collect();

        let outcome = embed_chunks_with_budget_and_save(
            &mut embedder,
            chunks,
            &mut store,
            &SemanticCancellationToken::new(),
            EmbeddingBudget::seconds(2).timed_on(&clock),
            |_, _| {},
            |_| {},
        )
        .expect("embedding succeeds");

        assert_eq!(embedder.calls, 2);
        assert_eq!(outcome.embedded.len(), 2 * DEFAULT_EMBEDDING_BATCH_SIZE);
        assert_eq!(
            outcome.missing_chunk_count,
            MISSING - 2 * DEFAULT_EMBEDDING_BATCH_SIZE
        );
    }

    #[test]
    fn time_limit_stops_embedding_between_batches_and_keeps_finished_batch() {
        const MISSING: usize = 3 * DEFAULT_EMBEDDING_BATCH_SIZE;
        let mut store = PassageStore::empty(ChunkConfig::default());
        let chunks = (0..MISSING)
            .map(|index| chunk(&format!("session:{index}"), &format!("text {index}")))
            .collect();
        let mut saved_passage_counts = Vec::new();

        let (outcome, calls) =
            embed_past_the_time_limit(chunks, &mut store, &mut saved_passage_counts);

        assert_eq!(calls, 1);
        assert_eq!(outcome.embedded.len(), DEFAULT_EMBEDDING_BATCH_SIZE);
        assert_eq!(
            outcome.missing_chunk_count,
            MISSING - DEFAULT_EMBEDDING_BATCH_SIZE
        );
        assert_eq!(store.passage_count(), DEFAULT_EMBEDDING_BATCH_SIZE);
        assert_eq!(saved_passage_counts, vec![DEFAULT_EMBEDDING_BATCH_SIZE]);
    }

    #[test]
    fn time_limit_counts_every_chunk_of_repeated_text_it_left_out() {
        let mut store = PassageStore::empty(ChunkConfig::default());
        let mut chunks = (0..DEFAULT_EMBEDDING_BATCH_SIZE)
            .map(|index| chunk(&format!("session:{index}"), &format!("text {index}")))
            .collect::<Vec<_>>();
        chunks.push(chunk("late:0", "repeated text"));
        chunks.push(chunk("late:1", "repeated text"));
        let mut saved_passage_counts = Vec::new();

        let (outcome, _) = embed_past_the_time_limit(chunks, &mut store, &mut saved_passage_counts);

        assert_eq!(outcome.embedded.len(), DEFAULT_EMBEDDING_BATCH_SIZE);
        assert_eq!(outcome.missing_chunk_count, 2);
    }

    #[test]
    fn zero_second_limit_embeds_nothing() {
        let mut store = PassageStore::empty(ChunkConfig::default());
        let mut embedder = FakeEmbedder { calls: 0 };

        let outcome = embed_chunks_with_budget_and_save(
            &mut embedder,
            vec![chunk("session:0", "new text")],
            &mut store,
            &SemanticCancellationToken::new(),
            EmbeddingBudget::seconds(0).timed_on(&TestClock::default()),
            |_, _| {},
            |_| {},
        )
        .expect("embedding succeeds");

        assert_eq!(embedder.calls, 0);
        assert_eq!(outcome.missing_chunk_count, 1);
        assert_eq!(store.passage_count(), 0);
    }

    #[test]
    fn embed_chunks_reuses_matching_cache_entry() {
        let config = ChunkConfig::default();
        let mut store = PassageStore::empty(config);
        store_text(&mut store, "cached text");
        let mut embedder = FakeEmbedder { calls: 0 };

        let embedded = embed_chunks_with_progress_and_save(
            &mut embedder,
            vec![chunk("session:0", "cached text")],
            &mut store,
            &SemanticCancellationToken::new(),
            |_, _| {},
            |_| {},
        )
        .expect("embedding succeeds");

        assert_eq!(embedder.calls, 0);
        assert_eq!(*embedded[0].embedding, [0.5, 0.5]);
        assert_eq!(embedded[0].chunk_index, 0);
    }

    #[test]
    fn embed_chunks_embeds_cache_misses() {
        let config = ChunkConfig::default();
        let mut store = PassageStore::empty(config);
        let mut embedder = FakeEmbedder { calls: 0 };

        let embedded = embed_chunks_with_progress_and_save(
            &mut embedder,
            vec![chunk("session:0", "new text")],
            &mut store,
            &SemanticCancellationToken::new(),
            |_, _| {},
            |_| {},
        )
        .expect("embedding succeeds");

        assert_eq!(embedder.calls, 1);
        assert_eq!(*embedded[0].embedding, [8.0, 1.0]);
        assert_eq!(embedded[0].chunk_index, 0);
        assert!(store.contains_text("new text"));
    }

    #[test]
    fn invalid_embedding_batch_is_error_and_doesnt_cache_any_of_it() {
        let one_short = vec![vec![1.0, 0.0]];
        let differing_lengths = vec![vec![1.0], vec![1.0, 0.0]];
        let all_longer = vec![vec![1.0, 0.0, 0.0], vec![0.0, 1.0, 0.0]];
        let non_finite = vec![vec![f32::NAN, 0.0], vec![1.0, 0.0]];

        for invalid in [one_short, differing_lengths, all_longer, non_finite] {
            let mut store = PassageStore::empty(ChunkConfig::default());
            let mut embedder = FixedEmbedder {
                passages: invalid.clone(),
            };

            let embedded = embed_chunks_with_progress_and_save(
                &mut embedder,
                vec![chunk("session:0", "alpha"), chunk("session:1", "beta")],
                &mut store,
                &SemanticCancellationToken::new(),
                |_, _| {},
                |_| {},
            );

            assert!(
                matches!(embedded, Err(AppError::InvalidEmbeddings(_))),
                "{invalid:?}"
            );
            assert_eq!(store.passage_count(), 0, "{invalid:?}");
        }
    }

    #[test]
    fn embed_chunks_reuses_content_across_sessions() {
        let config = ChunkConfig::default();
        let mut store = PassageStore::empty(config);
        store_text(&mut store, "shared text");
        let mut embedder = FakeEmbedder { calls: 0 };

        let embedded = embed_chunks_with_progress_and_save(
            &mut embedder,
            vec![
                chunk("first-session:0", "shared text"),
                chunk("second-session:4", "shared text"),
            ],
            &mut store,
            &SemanticCancellationToken::new(),
            |_, _| {},
            |_| {},
        )
        .expect("embedding succeeds");

        assert_eq!(embedder.calls, 0);
        assert_eq!(embedded.len(), 2);
        assert_eq!(store.passage_count(), 1);
        assert!(
            Arc::ptr_eq(&embedded[0].embedding, &embedded[1].embedding),
            "both chunks of the cached text share one embedding"
        );
    }

    #[test]
    fn embed_chunks_embeds_duplicate_misses_once() {
        let config = ChunkConfig::default();
        let mut store = PassageStore::empty(config);
        let mut embedder = FakeEmbedder { calls: 0 };
        let mut progress = Vec::new();

        let embedded = embed_chunks_with_progress_and_save(
            &mut embedder,
            vec![
                chunk("first-session:0", "duplicate text"),
                chunk("second-session:0", "duplicate text"),
            ],
            &mut store,
            &SemanticCancellationToken::new(),
            |completed, total| progress.push((completed, total)),
            |_| {},
        )
        .expect("embedding succeeds");

        assert_eq!(embedder.calls, 1);
        assert_eq!(embedded.len(), 2);
        assert_eq!(store.passage_count(), 1);
        assert_eq!(progress, vec![(1, 1)]);
        assert!(
            Arc::ptr_eq(&embedded[0].embedding, &embedded[1].embedding),
            "both chunks of the embedded text share one embedding"
        );
    }

    #[test]
    fn embed_chunks_checkpoints_large_runs_and_saves_final_batch() {
        let config = ChunkConfig::default();
        let mut store = PassageStore::empty(config);
        let mut embedder = FakeEmbedder { calls: 0 };
        let chunks = (0..300)
            .map(|index| chunk(&format!("session:{index}"), &format!("text {index}")))
            .collect();
        let mut saved_passage_counts = Vec::new();

        embed_chunks_with_progress_and_save(
            &mut embedder,
            chunks,
            &mut store,
            &SemanticCancellationToken::new(),
            |_, _| {},
            |store| saved_passage_counts.push(store.passage_count()),
        )
        .expect("embedding succeeds");

        assert_eq!(
            embedder.calls,
            300usize.div_ceil(DEFAULT_EMBEDDING_BATCH_SIZE)
        );
        assert_eq!(saved_passage_counts, vec![256, 300]);
    }

    #[test]
    fn embed_chunks_saves_completed_work_when_cancelled() {
        let config = ChunkConfig::default();
        let mut store = PassageStore::empty(config);
        let cancellation = SemanticCancellationToken::new();
        let mut embedder = CancellingEmbedder {
            cancellation: cancellation.clone(),
        };
        let chunks = (0..=DEFAULT_EMBEDDING_BATCH_SIZE)
            .map(|index| chunk(&format!("session:{index}"), &format!("text {index}")))
            .collect();
        let mut saved_passage_counts = Vec::new();

        let result = embed_chunks_with_progress_and_save(
            &mut embedder,
            chunks,
            &mut store,
            &cancellation,
            |_, _| {},
            |store| saved_passage_counts.push(store.passage_count()),
        );

        assert!(matches!(result, Err(AppError::SemanticSearchCancelled)));
        assert_eq!(saved_passage_counts, vec![DEFAULT_EMBEDDING_BATCH_SIZE]);
    }

    #[test]
    fn changed_text_creates_distinct_cache_entry() {
        let config = ChunkConfig::default();
        let mut store = PassageStore::empty(config);
        store_text(&mut store, "old text");
        let mut embedder = FakeEmbedder { calls: 0 };

        let embedded = embed_chunks_with_progress_and_save(
            &mut embedder,
            vec![chunk("session:0", "new text")],
            &mut store,
            &SemanticCancellationToken::new(),
            |_, _| {},
            |_| {},
        )
        .expect("embedding succeeds");

        assert_eq!(embedder.calls, 1);
        assert_eq!(*embedded[0].embedding, [8.0, 1.0]);
        assert_eq!(
            store.embedding_of(&passage_key("new text")),
            Some([8.0, 1.0].as_slice())
        );
        assert!(store.contains_text("old text"));
    }

    #[test]
    fn cache_miss_count_matches_cached_chunks() {
        let config = ChunkConfig::default();
        let mut store = PassageStore::empty(config);
        store_text(&mut store, "cached text");
        store_text(&mut store, "old text");
        let chunks = vec![
            chunk("session:0", "cached text"),
            chunk("session:1", "new text"),
            chunk("session:2", "uncached text"),
        ];

        let (_, cached_chunk_misses) =
            cached_chunks(chunks.clone(), &store, &SemanticCancellationToken::new()).unwrap();

        assert_eq!(store.missing_passage_count(&chunks), cached_chunk_misses);
        assert_eq!(store.missing_passage_count(&chunks), 2);
    }

    #[test]
    fn semantic_cache_paths_live_under_rearview_cache() {
        let cache_dir = semantic_cache_dir_in(crate::cache_file::cache_base_under(Path::new(
            "/home/example",
        )));

        assert_eq!(
            cache_dir.join(EMBEDDING_CACHE_FILE),
            PathBuf::from("/home/example/.cache/rearview/semantic/embeddings-v1.bin")
        );
        assert_eq!(
            cache_dir.join(OLD_MODEL_DIRECTORY),
            PathBuf::from("/home/example/.cache/rearview/semantic/fastembed")
        );
    }

    /// A semantic folder with an embedding cache file of 300 bytes, a shard
    /// folder containing 200 bytes and a `fastembed` folder containing 1,000.
    fn semantic_folder_with_cache_files() -> tempfile::TempDir {
        let semantic_dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(semantic_dir.path().join(EMBEDDING_CACHE_FILE), [0; 300]).unwrap();
        let shard_dir = semantic_dir.path().join("shards");
        std::fs::create_dir(&shard_dir).unwrap();
        std::fs::write(shard_dir.join("shard-0.bin"), [0; 200]).unwrap();
        let fastembed_dir = semantic_dir.path().join(OLD_MODEL_DIRECTORY);
        std::fs::create_dir(&fastembed_dir).unwrap();
        std::fs::write(fastembed_dir.join("leftover.bin"), [0; 1_000]).unwrap();
        semantic_dir
    }

    fn entry_names(folder: &Path) -> Vec<std::ffi::OsString> {
        std::fs::read_dir(folder)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect()
    }

    #[test]
    fn clear_semantic_cache_reports_bytes_freed_including_fastembed_folder() {
        let semantic_dir = semantic_folder_with_cache_files();
        let hugging_face_cache = semantic_dir.path().parent().unwrap().join("hub");

        let freed = clear_semantic_cache_in(semantic_dir.path(), &hugging_face_cache).unwrap();

        assert_eq!(
            freed,
            Some(1_500),
            "the bytes freed differ from the cache files' sizes"
        );
        assert_eq!(
            entry_names(semantic_dir.path()),
            Vec::<std::ffi::OsString>::new(),
            "a semantic cache file or folder remains"
        );
    }

    #[test]
    fn clear_semantic_cache_skips_entry_containing_hugging_face_cache() {
        let semantic_dir = semantic_folder_with_cache_files();
        let hugging_face_cache = semantic_dir.path().join("shared").join("hub");
        let model =
            hugging_face_cache.join(crate::semantic::fastembed::model_folder_name().unwrap());
        std::fs::create_dir_all(&model).unwrap();
        std::fs::write(model.join("model.onnx"), [0; 4_000]).unwrap();

        let freed = clear_semantic_cache_in(semantic_dir.path(), &hugging_face_cache).unwrap();

        assert_eq!(
            freed,
            Some(1_500),
            "the bytes freed count the Hugging Face cache or miss a cache file"
        );
        assert_eq!(
            entry_names(semantic_dir.path()),
            ["shared"],
            "the folder containing the Hugging Face cache is deleted, or a cache file remains"
        );
        assert!(
            model.join("model.onnx").is_file(),
            "the model in the Hugging Face cache is deleted"
        );
    }

    #[test]
    fn clear_semantic_cache_reports_empty_cache() {
        let root = tempfile::tempdir().expect("tempdir");
        let only_hugging_face_cache = root.path().join("semantic");
        let hugging_face_cache = only_hugging_face_cache.join("hub");
        std::fs::create_dir_all(&hugging_face_cache).unwrap();

        assert_eq!(
            clear_semantic_cache_in(&root.path().join("missing"), &hugging_face_cache).unwrap(),
            None,
            "a missing semantic folder is reported as cleared"
        );
        assert_eq!(
            clear_semantic_cache_in(&only_hugging_face_cache, &hugging_face_cache).unwrap(),
            None,
            "a semantic folder containing only the Hugging Face cache is reported as cleared"
        );
    }
}
