use crate::error::{AppError, Result};
use crate::semantic::embed::{SemanticEmbedder, is_valid_embedding};
use crate::semantic::store::{OpenPassageStore, PassageKey};
use crate::semantic::types::{
    DEFAULT_EMBEDDING_BATCH_SIZE, EmbeddedChunk, SemanticCancellationToken, SemanticChunk,
    TimedBudget,
};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The chunks of a lookup with a stored embedding, and the rest.
pub struct StoredLookup {
    pub embedded: Vec<EmbeddedChunk>,
    /// The chunks without a stored embedding, grouped by text, in the order
    /// each text first appears.
    pub missing: Vec<Vec<SemanticChunk>>,
}

impl StoredLookup {
    /// The distinct texts without a stored embedding.
    pub fn missing_passage_count(&self) -> usize {
        self.missing.len()
    }
}

/// Read the embeddings of `chunks`' texts from `store`, one read per
/// distinct text. Fail once `cancellation` is cancelled, during the read
/// too.
pub fn look_up_chunks(
    chunks: Vec<SemanticChunk>,
    store: &mut OpenPassageStore<'_>,
    cancellation: &SemanticCancellationToken,
) -> Result<StoredLookup> {
    if cancellation.is_cancelled() {
        return Err(AppError::SemanticSearchCancelled);
    }
    let keys = chunks
        .iter()
        .map(|chunk| PassageKey::of(&chunk.text))
        .collect::<Vec<_>>();
    let stored = store
        .read(keys.iter().collect::<HashSet<_>>(), cancellation)
        .ok_or(AppError::SemanticSearchCancelled)?;

    let mut lookup = StoredLookup {
        embedded: Vec::with_capacity(chunks.len()),
        missing: Vec::new(),
    };
    let mut missing_indices = HashMap::<PassageKey, usize>::new();
    for (chunk, key) in chunks.into_iter().zip(keys) {
        if cancellation.is_cancelled() {
            return Err(AppError::SemanticSearchCancelled);
        }
        if let Some(embedding) = stored.get(&key) {
            lookup
                .embedded
                .push(embedded_chunk(&chunk, Arc::clone(embedding)));
        } else if let Some(index) = missing_indices.get(&key).copied() {
            lookup.missing[index].push(chunk);
        } else {
            missing_indices.insert(key, lookup.missing.len());
            lookup.missing.push(vec![chunk]);
        }
    }
    Ok(lookup)
}

pub struct EmbeddingOutcome {
    pub embedded: Vec<EmbeddedChunk>,
    /// Chunks left without an embedding by the budget, counted per chunk, so
    /// this and `embedded.len()` sum to the chunks looked up.
    pub missing_chunk_count: usize,
}

/// Embed the texts `lookup` didn't find in `store` within `budget`; the
/// rest are counted in the outcome. Write each batch to `store` as it
/// finishes: a run stopped by cancellation, an error or its end keeps every
/// finished batch. After each batch, check the time limit; once it has
/// passed, stop.
pub fn embed_missing(
    embedder: &mut dyn SemanticEmbedder,
    lookup: StoredLookup,
    store: &mut OpenPassageStore<'_>,
    cancellation: &SemanticCancellationToken,
    budget: TimedBudget<'_>,
    mut progress: impl FnMut(usize, usize),
) -> Result<EmbeddingOutcome> {
    if cancellation.is_cancelled() {
        return Err(AppError::SemanticSearchCancelled);
    }
    let StoredLookup {
        mut embedded,
        mut missing,
    } = lookup;

    let dimensions = embedder.dimensions();
    let total_missing = budget.passages_within(missing.len());
    let mut missing_chunk_count: usize = missing[total_missing..].iter().map(Vec::len).sum();
    missing.truncate(total_missing);
    let mut completed = 0;
    let started = budget.start();
    for batch in missing.chunks(DEFAULT_EMBEDDING_BATCH_SIZE) {
        if cancellation.is_cancelled() {
            return Err(AppError::SemanticSearchCancelled);
        }
        let texts = batch
            .iter()
            .map(|chunks| chunks[0].text.clone())
            .collect::<Vec<_>>();
        let embeddings = embedder
            .embed_passages(&texts)
            .and_then(|embeddings| validate_passage_embeddings(&texts, embeddings, dimensions))?;

        let mut passages = Vec::with_capacity(batch.len());
        for (chunks, embedding) in batch.iter().zip(embeddings) {
            let embedding = Arc::<[f32]>::from(embedding);
            for chunk in chunks {
                embedded.push(embedded_chunk(chunk, Arc::clone(&embedding)));
            }
            passages.push((PassageKey::of(&chunks[0].text), embedding));
        }
        store.write(&passages);
        completed += batch.len();
        progress(completed, total_missing);
        if completed < total_missing && budget.time_is_up(started) {
            missing_chunk_count += missing[completed..].iter().map(Vec::len).sum::<usize>();
            break;
        }
    }

    Ok(EmbeddingOutcome {
        embedded,
        missing_chunk_count,
    })
}

/// One embedding per passage, each `dimensions` finite values; otherwise
/// none of the batch is stored.
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

const OLD_MODEL_DIRECTORY: &str = "fastembed";

/// The semantic cache folder; `None` without a cache folder.
pub(crate) fn semantic_cache_dir() -> Option<PathBuf> {
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
    use crate::semantic::store::test_support::{
        FAKE_DIMENSIONS, store_passages, stored_embedding, stored_passage_count, temporary_store,
    };
    use crate::semantic::store::{PASSAGE_STORE_FILE, PassageStore};
    use crate::semantic::types::{DEFAULT_INTERACTIVE_EMBEDDING_SECONDS, EmbeddingBudget};
    use std::time::Duration;

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

    fn empty_store() -> PassageStore {
        temporary_store(Arc::new(TestClock::default()))
    }

    /// One run on `store`, as the semantic index runs it: look `chunks` up,
    /// then embed the missing texts within `budget`.
    fn embed_chunks_with_budget(
        embedder: &mut dyn SemanticEmbedder,
        chunks: Vec<SemanticChunk>,
        store: &PassageStore,
        cancellation: &SemanticCancellationToken,
        budget: TimedBudget<'_>,
        progress: impl FnMut(usize, usize),
    ) -> Result<EmbeddingOutcome> {
        let mut open = store.open();
        let lookup = look_up_chunks(chunks, &mut open, cancellation)?;
        let outcome = embed_missing(embedder, lookup, &mut open, cancellation, budget, progress);
        open.finish();
        outcome
    }

    /// [`embed_chunks_with_budget`] without a budget.
    fn embed_chunks(
        embedder: &mut dyn SemanticEmbedder,
        chunks: Vec<SemanticChunk>,
        store: &PassageStore,
        cancellation: &SemanticCancellationToken,
        progress: impl FnMut(usize, usize),
    ) -> Result<Vec<EmbeddedChunk>> {
        embed_chunks_with_budget(
            embedder,
            chunks,
            store,
            cancellation,
            EmbeddingBudget::unbounded().timed_on(&TestClock::default()),
            progress,
        )
        .map(|outcome| outcome.embedded)
    }

    #[test]
    fn embed_chunks_respects_new_embedding_budget() {
        const BUDGET_BATCHES: usize = 4;
        const BUDGET: usize = BUDGET_BATCHES * DEFAULT_EMBEDDING_BATCH_SIZE;
        const MISSING: usize = BUDGET + 8;
        let store = empty_store();
        store_passages(&store, &["cached text"], [0.5, 0.5]);
        let chunks = std::iter::once(chunk("session:0", "cached text"))
            .chain((1..=MISSING).map(|index| {
                chunk(
                    &format!("session:{index}"),
                    &format!("missing text {index}"),
                )
            }))
            .collect::<Vec<_>>();
        let mut embedder = FakeEmbedder { calls: 0 };

        let outcome = embed_chunks_with_budget(
            &mut embedder,
            chunks,
            &store,
            &SemanticCancellationToken::new(),
            EmbeddingBudget::passages(BUDGET).timed_on(&TestClock::default()),
            |_, _| {},
        )
        .expect("embedding succeeds");

        assert_eq!(embedder.calls, BUDGET_BATCHES);
        assert_eq!(outcome.embedded.len(), BUDGET + 1);
        assert_eq!(outcome.missing_chunk_count, MISSING - BUDGET);
        assert_eq!(stored_passage_count(&store), BUDGET + 1);
        assert!(stored_embedding(&store, &format!("missing text {BUDGET}")).is_some());
        assert!(stored_embedding(&store, &format!("missing text {}", BUDGET + 1)).is_none());
    }

    #[test]
    fn missing_chunk_count_counts_every_chunk_of_repeated_text_beyond_budget() {
        let chunks = vec![
            chunk("session:0", "embedded text"),
            chunk("session:1", "repeated text"),
            chunk("session:2", "repeated text"),
        ];
        let mut embedder = FakeEmbedder { calls: 0 };

        let outcome = embed_chunks_with_budget(
            &mut embedder,
            chunks,
            &empty_store(),
            &SemanticCancellationToken::new(),
            EmbeddingBudget::passages(1).timed_on(&TestClock::default()),
            |_, _| {},
        )
        .expect("embedding succeeds");

        assert_eq!(outcome.embedded.len(), 1);
        assert_eq!(outcome.missing_chunk_count, 2);
    }

    /// Each batch outlasts the default time limit: the first batch runs to
    /// its end and the second never starts.
    fn embed_past_the_time_limit(
        chunks: Vec<SemanticChunk>,
        store: &PassageStore,
    ) -> (EmbeddingOutcome, usize) {
        let clock = TestClock::default();
        let mut embedder = SlowEmbedder {
            calls: 0,
            batch_takes: Duration::from_secs(DEFAULT_INTERACTIVE_EMBEDDING_SECONDS + 1),
            clock: &clock,
        };
        let outcome = embed_chunks_with_budget(
            &mut embedder,
            chunks,
            store,
            &SemanticCancellationToken::new(),
            EmbeddingBudget::seconds(DEFAULT_INTERACTIVE_EMBEDDING_SECONDS).timed_on(&clock),
            |_, _| {},
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
        let chunks = (0..MISSING)
            .map(|index| chunk(&format!("session:{index}"), &format!("text {index}")))
            .collect();

        let outcome = embed_chunks_with_budget(
            &mut embedder,
            chunks,
            &empty_store(),
            &SemanticCancellationToken::new(),
            EmbeddingBudget::seconds(2).timed_on(&clock),
            |_, _| {},
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
        let store = empty_store();
        let chunks = (0..MISSING)
            .map(|index| chunk(&format!("session:{index}"), &format!("text {index}")))
            .collect();

        let (outcome, calls) = embed_past_the_time_limit(chunks, &store);

        assert_eq!(calls, 1);
        assert_eq!(outcome.embedded.len(), DEFAULT_EMBEDDING_BATCH_SIZE);
        assert_eq!(
            outcome.missing_chunk_count,
            MISSING - DEFAULT_EMBEDDING_BATCH_SIZE
        );
        assert_eq!(stored_passage_count(&store), DEFAULT_EMBEDDING_BATCH_SIZE);
    }

    #[test]
    fn time_limit_counts_every_chunk_of_repeated_text_it_left_out() {
        let mut chunks = (0..DEFAULT_EMBEDDING_BATCH_SIZE)
            .map(|index| chunk(&format!("session:{index}"), &format!("text {index}")))
            .collect::<Vec<_>>();
        chunks.push(chunk("late:0", "repeated text"));
        chunks.push(chunk("late:1", "repeated text"));

        let (outcome, _) = embed_past_the_time_limit(chunks, &empty_store());

        assert_eq!(outcome.embedded.len(), DEFAULT_EMBEDDING_BATCH_SIZE);
        assert_eq!(outcome.missing_chunk_count, 2);
    }

    #[test]
    fn zero_second_limit_embeds_nothing() {
        let store = empty_store();
        let mut embedder = FakeEmbedder { calls: 0 };

        let outcome = embed_chunks_with_budget(
            &mut embedder,
            vec![chunk("session:0", "new text")],
            &store,
            &SemanticCancellationToken::new(),
            EmbeddingBudget::seconds(0).timed_on(&TestClock::default()),
            |_, _| {},
        )
        .expect("embedding succeeds");

        assert_eq!(embedder.calls, 0);
        assert_eq!(outcome.missing_chunk_count, 1);
        assert_eq!(stored_passage_count(&store), 0);
    }

    #[test]
    fn embed_chunks_reuses_stored_embedding() {
        let store = empty_store();
        store_passages(&store, &["cached text"], [0.5, 0.5]);
        let mut embedder = FakeEmbedder { calls: 0 };

        let embedded = embed_chunks(
            &mut embedder,
            vec![chunk("session:0", "cached text")],
            &store,
            &SemanticCancellationToken::new(),
            |_, _| {},
        )
        .expect("embedding succeeds");

        assert_eq!(embedder.calls, 0);
        assert_eq!(*embedded[0].embedding, [0.5, 0.5]);
        assert_eq!(embedded[0].chunk_index, 0);
    }

    #[test]
    fn embed_chunks_embeds_passages_missing_from_store() {
        let store = empty_store();
        let mut embedder = FakeEmbedder { calls: 0 };

        let embedded = embed_chunks(
            &mut embedder,
            vec![chunk("session:0", "new text")],
            &store,
            &SemanticCancellationToken::new(),
            |_, _| {},
        )
        .expect("embedding succeeds");

        assert_eq!(embedder.calls, 1);
        assert_eq!(*embedded[0].embedding, [8.0, 1.0]);
        assert_eq!(embedded[0].chunk_index, 0);
        assert!(stored_embedding(&store, "new text").is_some());
    }

    #[test]
    fn invalid_embedding_batch_is_error_and_doesnt_cache_any_of_it() {
        let one_short = vec![vec![1.0, 0.0]];
        let differing_lengths = vec![vec![1.0], vec![1.0, 0.0]];
        let all_longer = vec![vec![1.0, 0.0, 0.0], vec![0.0, 1.0, 0.0]];
        let non_finite = vec![vec![f32::NAN, 0.0], vec![1.0, 0.0]];

        for invalid in [one_short, differing_lengths, all_longer, non_finite] {
            let store = empty_store();
            let mut embedder = FixedEmbedder {
                passages: invalid.clone(),
            };

            let embedded = embed_chunks(
                &mut embedder,
                vec![chunk("session:0", "alpha"), chunk("session:1", "beta")],
                &store,
                &SemanticCancellationToken::new(),
                |_, _| {},
            );

            assert!(
                matches!(embedded, Err(AppError::InvalidEmbeddings(_))),
                "{invalid:?}"
            );
            assert_eq!(stored_passage_count(&store), 0, "{invalid:?}");
        }
    }

    #[test]
    fn embed_chunks_reuses_content_across_sessions() {
        let store = empty_store();
        store_passages(&store, &["shared text"], [0.5, 0.5]);
        let mut embedder = FakeEmbedder { calls: 0 };

        let embedded = embed_chunks(
            &mut embedder,
            vec![
                chunk("first-session:0", "shared text"),
                chunk("second-session:4", "shared text"),
            ],
            &store,
            &SemanticCancellationToken::new(),
            |_, _| {},
        )
        .expect("embedding succeeds");

        assert_eq!(embedder.calls, 0);
        assert_eq!(embedded.len(), 2);
        assert_eq!(stored_passage_count(&store), 1);
        assert!(
            Arc::ptr_eq(&embedded[0].embedding, &embedded[1].embedding),
            "both chunks of the cached text share one embedding"
        );
    }

    #[test]
    fn embed_chunks_embeds_duplicate_misses_once() {
        let store = empty_store();
        let mut embedder = FakeEmbedder { calls: 0 };
        let mut progress = Vec::new();

        let embedded = embed_chunks(
            &mut embedder,
            vec![
                chunk("first-session:0", "duplicate text"),
                chunk("second-session:0", "duplicate text"),
            ],
            &store,
            &SemanticCancellationToken::new(),
            |completed, total| progress.push((completed, total)),
        )
        .expect("embedding succeeds");

        assert_eq!(embedder.calls, 1);
        assert_eq!(embedded.len(), 2);
        assert_eq!(stored_passage_count(&store), 1);
        assert_eq!(progress, vec![(1, 1)]);
        assert!(
            Arc::ptr_eq(&embedded[0].embedding, &embedded[1].embedding),
            "both chunks of the embedded text share one embedding"
        );
    }

    #[test]
    fn embed_chunks_stores_each_batch_as_it_finishes() {
        const PASSAGES: usize = 2 * DEFAULT_EMBEDDING_BATCH_SIZE + 4;
        let store = empty_store();
        let mut embedder = FakeEmbedder { calls: 0 };
        let chunks = (0..PASSAGES)
            .map(|index| chunk(&format!("session:{index}"), &format!("text {index}")))
            .collect();
        let mut stored_after_each_batch = Vec::new();

        embed_chunks(
            &mut embedder,
            chunks,
            &store,
            &SemanticCancellationToken::new(),
            |_, _| stored_after_each_batch.push(stored_passage_count(&store)),
        )
        .expect("embedding succeeds");

        assert_eq!(
            stored_after_each_batch,
            vec![
                DEFAULT_EMBEDDING_BATCH_SIZE,
                2 * DEFAULT_EMBEDDING_BATCH_SIZE,
                PASSAGES
            ],
            "the store holds each batch once its progress is reported"
        );
    }

    #[test]
    fn embed_chunks_saves_completed_work_when_cancelled() {
        let store = empty_store();
        let cancellation = SemanticCancellationToken::new();
        let mut embedder = CancellingEmbedder {
            cancellation: cancellation.clone(),
        };
        let chunks = (0..=DEFAULT_EMBEDDING_BATCH_SIZE)
            .map(|index| chunk(&format!("session:{index}"), &format!("text {index}")))
            .collect();

        let result = embed_chunks(&mut embedder, chunks, &store, &cancellation, |_, _| {});

        assert!(matches!(result, Err(AppError::SemanticSearchCancelled)));
        assert_eq!(stored_passage_count(&store), DEFAULT_EMBEDDING_BATCH_SIZE);
    }

    #[test]
    fn changed_text_is_stored_as_distinct_passage() {
        let store = empty_store();
        store_passages(&store, &["old text"], [0.5, 0.5]);
        let mut embedder = FakeEmbedder { calls: 0 };

        let embedded = embed_chunks(
            &mut embedder,
            vec![chunk("session:0", "new text")],
            &store,
            &SemanticCancellationToken::new(),
            |_, _| {},
        )
        .expect("embedding succeeds");

        assert_eq!(embedder.calls, 1);
        assert_eq!(*embedded[0].embedding, [8.0, 1.0]);
        assert_eq!(
            stored_embedding(&store, "new text"),
            Some(Arc::from([8.0, 1.0]))
        );
        assert!(stored_embedding(&store, "old text").is_some());
    }

    #[test]
    fn lookup_counts_distinct_texts_without_stored_embedding() {
        let store = empty_store();
        store_passages(&store, &["cached text", "old text"], [0.5, 0.5]);
        let chunks = vec![
            chunk("session:0", "cached text"),
            chunk("session:1", "new text"),
            chunk("session:2", "uncached text"),
            chunk("session:3", "new text"),
        ];

        let lookup =
            look_up_chunks(chunks, &mut store.open(), &SemanticCancellationToken::new()).unwrap();

        assert_eq!(lookup.embedded.len(), 1);
        assert_eq!(lookup.missing_passage_count(), 2);
    }

    #[test]
    fn semantic_cache_paths_live_under_rearview_cache() {
        let cache_dir = semantic_cache_dir_in(crate::cache_file::cache_base_under(Path::new(
            "/home/example",
        )));

        assert_eq!(
            cache_dir.join(PASSAGE_STORE_FILE),
            PathBuf::from("/home/example/.cache/rearview/semantic/embeddings-v2.sqlite3")
        );
        assert_eq!(
            cache_dir.join(OLD_MODEL_DIRECTORY),
            PathBuf::from("/home/example/.cache/rearview/semantic/fastembed")
        );
    }

    /// A semantic folder with a passage store of 300 bytes and its
    /// write-ahead log of 100, a shard folder containing 200 bytes and a
    /// `fastembed` folder containing 1,000.
    fn semantic_folder_with_cache_files() -> tempfile::TempDir {
        let semantic_dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(semantic_dir.path().join(PASSAGE_STORE_FILE), [0; 300]).unwrap();
        std::fs::write(
            semantic_dir
                .path()
                .join(format!("{PASSAGE_STORE_FILE}-wal")),
            [0; 100],
        )
        .unwrap();
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
            Some(1_600),
            "the bytes freed are the sum of the cache files' sizes"
        );
        assert_eq!(
            entry_names(semantic_dir.path()),
            Vec::<std::ffi::OsString>::new(),
            "every semantic cache file and folder is deleted"
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
            Some(1_600),
            "the bytes freed are the cache files' sizes, without the Hugging Face cache"
        );
        assert_eq!(
            entry_names(semantic_dir.path()),
            ["shared"],
            "the folder containing the Hugging Face cache alone stays"
        );
        assert!(
            model.join("model.onnx").is_file(),
            "the model in the Hugging Face cache stays"
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
