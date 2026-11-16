use crate::clock::Clock;
use crate::error::{AppError, Result};
use crate::history::{Conversation, MessageTimes};
use crate::search::literal::Literal;
use crate::semantic::cache::{embed_missing, look_up_chunks, semantic_cache_dir};
#[cfg(test)]
use crate::semantic::chunk::build_chunks_with_sources;
use crate::semantic::chunk::builds_any_chunk;
use crate::semantic::embed::{SemanticEmbedder, embed_valid_query};
use crate::semantic::filter::matches_every_literal;
use crate::semantic::rank::rank_prepared;
use crate::semantic::store::{PassageStore, StoreUse};
use crate::semantic::types::{
    ChunkConfig, ChunkHits, EmbeddedChunk, EmbeddingBudget, SemanticCancellationToken,
    SemanticChunk, SemanticChunkSource, SemanticHit,
};
use crate::time_filter::TimeFilter;
use std::collections::HashMap;
use std::sync::Arc;

mod resident;
use resident::{CorpusSignature, ResidentIndex};

#[derive(Clone)]
pub struct SemanticIndexCandidate {
    pub index: usize,
    pub source: SemanticChunkSource,
    pub conversation: Arc<Conversation>,
}

pub struct SemanticIndexRequest<'a> {
    pub query: &'a str,
    pub literal_filters: &'a [Literal],
    pub full_corpus: &'a [SemanticIndexCandidate],
    pub scope: &'a [SemanticIndexCandidate],
    pub prewarm: bool,
    pub chunk_hits: ChunkHits,
    /// Rank only passages holding a message inside it.
    pub window: TimeFilter,
}

pub struct SemanticIndexResponse {
    pub hits: Vec<SemanticHit>,
    pub chunk_hits: Vec<SemanticHit>,
    pub indexed_chunk_count: usize,
    pub missing_chunk_count: usize,
    pub query_embedding_returned: bool,
    pub progress: SemanticIndexProgress,
    pub prewarm: bool,
    /// The passages the refresh read from the passage store and wrote to
    /// it; `None` when it did not open the store.
    pub store_use: Option<StoreUse>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SemanticIndexProgress {
    Embedding { completed: usize, total: usize },
    CacheReady,
    Ranking,
    Complete,
    EmptyCorpus,
}

/// One refresh's results from the passage store and the embedder.
#[derive(Default)]
struct StoreRun {
    embedded: Vec<EmbeddedChunk>,
    /// Chunks the budget left without an embedding.
    missing_chunk_count: usize,
    /// The passages the run set out to embed.
    embedding_count: usize,
    /// `None` when the refresh did not open the store.
    store_use: Option<StoreUse>,
}

pub struct SemanticIndexState {
    signature: Option<CorpusSignature>,
    resident: ResidentIndex,
    store: PassageStore,
    chunk_config: ChunkConfig,
    clock: Arc<dyn Clock>,
}

impl SemanticIndexState {
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        let chunk_config = ChunkConfig::default();
        Self {
            signature: None,
            resident: ResidentIndex::default(),
            store: PassageStore::in_folder(semantic_cache_dir().as_deref(), clock.clone()),
            chunk_config,
            clock,
        }
    }

    /// Fail when the passage store could not store the embeddings a refresh
    /// computes: its drive is low on space, or it does not open.
    pub fn fail_unless_store_is_writable(&self) -> Result<()> {
        self.store.fail_unless_writable()
    }

    /// True when `request` has chunks to work on: a prewarm in the full
    /// corpus, a ranking request in its scope. Answers from the resident
    /// index when it was refreshed for the request's corpus, and otherwise
    /// from the candidates' semantic turns, without building their chunks.
    pub fn has_chunks(
        &self,
        request: &SemanticIndexRequest<'_>,
        cancellation: &SemanticCancellationToken,
    ) -> Result<bool> {
        if cancellation.is_cancelled() {
            return Err(AppError::SemanticSearchCancelled);
        }
        let candidates = if request.prewarm {
            request.full_corpus
        } else {
            request.scope
        };
        if self.signature_matches(request) {
            return Ok(self.resident.has_chunks_in(candidates));
        }
        Ok(any_builds_a_chunk(candidates))
    }

    /// Records that `request`'s full corpus doesn't hold any chunks, once
    /// [`Self::has_chunks`] didn't find any for it. Keeps the index when a
    /// ranking request's scope doesn't hold any but the rest of the corpus
    /// does.
    pub fn clear_empty(
        &mut self,
        request: &SemanticIndexRequest<'_>,
        cancellation: &SemanticCancellationToken,
    ) -> Result<()> {
        if cancellation.is_cancelled() {
            return Err(AppError::SemanticSearchCancelled);
        }
        if !request.prewarm && any_builds_a_chunk(request.full_corpus) {
            return Ok(());
        }
        self.signature = Some(CorpusSignature::of(request.full_corpus));
        self.resident.clear();
        Ok(())
    }

    /// Drops the index built for the corpus; the passage store stays.
    pub fn clear_corpus(&mut self) {
        self.signature = None;
        self.resident.clear();
    }

    pub fn refresh_passages(
        &mut self,
        request: &SemanticIndexRequest<'_>,
        embedder: &mut dyn SemanticEmbedder,
        cancellation: &SemanticCancellationToken,
        progress: impl FnMut(SemanticIndexProgress),
    ) -> Result<SemanticIndexResponse> {
        self.refresh_passages_with_budget(
            request,
            embedder,
            cancellation,
            EmbeddingBudget::unbounded(),
            progress,
        )
    }

    /// Record the signature only when every corpus conversation has all its
    /// chunks embedded. When the time limit cuts a run short, the next
    /// request builds the incomplete conversations again and recounts their
    /// misses.
    fn refresh_passages_with_budget(
        &mut self,
        request: &SemanticIndexRequest<'_>,
        embedder: &mut dyn SemanticEmbedder,
        cancellation: &SemanticCancellationToken,
        budget: EmbeddingBudget,
        mut progress: impl FnMut(SemanticIndexProgress),
    ) -> Result<SemanticIndexResponse> {
        if cancellation.is_cancelled() {
            return Err(AppError::SemanticSearchCancelled);
        }
        if self.signature_matches(request) {
            progress(SemanticIndexProgress::CacheReady);
            return Ok(self.refresh_response(request, 0, None));
        }

        self.signature = None;
        let plan =
            self.resident
                .plan_refresh(request.full_corpus, self.chunk_config, cancellation)?;
        let (pending, chunks) = plan.into_parts();
        // A refresh without a chunk to look up does not open the store.
        let run = if chunks.is_empty() {
            StoreRun::default()
        } else {
            self.embed_from_store(chunks, embedder, cancellation, budget, &mut progress)?
        };
        self.resident.absorb(pending, run.embedded);
        if self.resident.is_complete() {
            self.signature = Some(CorpusSignature::of(request.full_corpus));
        }
        if run.embedding_count == 0 && self.resident.chunk_count() > 0 {
            progress(SemanticIndexProgress::CacheReady);
        }
        Ok(self.refresh_response(request, run.missing_chunk_count, run.store_use))
    }

    /// One run on the passage store: read the embeddings of `chunks`' texts,
    /// embed the missing texts within `budget` and store them.
    fn embed_from_store(
        &self,
        chunks: Vec<SemanticChunk>,
        embedder: &mut dyn SemanticEmbedder,
        cancellation: &SemanticCancellationToken,
        budget: EmbeddingBudget,
        mut progress: impl FnMut(SemanticIndexProgress),
    ) -> Result<StoreRun> {
        let mut store = self.store.open();
        let lookup = look_up_chunks(chunks, &mut store, cancellation)?;
        let embedding_count = budget.passages_within(lookup.missing_passage_count());
        if embedding_count > 0 {
            progress(SemanticIndexProgress::Embedding {
                completed: 0,
                total: embedding_count,
            });
        }
        let outcome = embed_missing(
            embedder,
            lookup,
            &mut store,
            cancellation,
            budget.timed_on(self.clock.as_ref()),
            |completed, total| {
                progress(SemanticIndexProgress::Embedding { completed, total });
            },
        )?;
        Ok(StoreRun {
            embedded: outcome.embedded,
            missing_chunk_count: outcome.missing_chunk_count,
            embedding_count,
            store_use: Some(store.finish()),
        })
    }

    pub fn rank_refreshed(
        &self,
        request: &SemanticIndexRequest<'_>,
        embedder: &mut dyn SemanticEmbedder,
        cancellation: &SemanticCancellationToken,
        mut progress: impl FnMut(SemanticIndexProgress),
    ) -> Result<SemanticIndexResponse> {
        if cancellation.is_cancelled() {
            return Err(AppError::SemanticSearchCancelled);
        }
        let mut scoped_chunks = self.resident.scoped(request.scope, cancellation)?;
        if scoped_chunks.is_empty() || request.prewarm {
            return Ok(SemanticIndexResponse {
                hits: Vec::new(),
                chunk_hits: Vec::new(),
                indexed_chunk_count: self.resident.chunk_count(),
                missing_chunk_count: 0,
                query_embedding_returned: true,
                progress: if scoped_chunks.is_empty() {
                    SemanticIndexProgress::EmptyCorpus
                } else {
                    SemanticIndexProgress::CacheReady
                },
                prewarm: request.prewarm,
                store_use: None,
            });
        }

        progress(SemanticIndexProgress::Ranking);
        let Some(query_embedding) = embed_valid_query(embedder, request.query)? else {
            return Ok(SemanticIndexResponse {
                hits: Vec::new(),
                chunk_hits: Vec::new(),
                indexed_chunk_count: self.resident.chunk_count(),
                missing_chunk_count: 0,
                query_embedding_returned: false,
                progress: SemanticIndexProgress::EmptyCorpus,
                prewarm: request.prewarm,
                store_use: None,
            });
        };

        scoped_chunks
            .retain(|chunk| matches_every_literal(&chunk.embedded().text, request.literal_filters));
        if request.window.is_active() {
            let message_times = message_times_by_index(request.scope);
            scoped_chunks.retain(|chunk| {
                let chunk = chunk.embedded();
                message_times
                    .get(&chunk.conversation_index)
                    .is_some_and(|times| {
                        times
                            .narrowed_to(chunk.message_range, &request.window)
                            .is_some()
                    })
            });
        }
        let ranked = rank_prepared(
            request.query,
            &query_embedding,
            &scoped_chunks,
            request.chunk_hits,
            cancellation,
        )?;
        let progress = SemanticIndexProgress::Complete;

        Ok(SemanticIndexResponse {
            hits: ranked.conversations,
            chunk_hits: ranked.chunks,
            indexed_chunk_count: self.resident.chunk_count(),
            missing_chunk_count: 0,
            query_embedding_returned: true,
            progress,
            prewarm: request.prewarm,
            store_use: None,
        })
    }

    pub fn refresh_or_prewarm(
        &mut self,
        request: &SemanticIndexRequest<'_>,
        embedder: &mut dyn SemanticEmbedder,
        cancellation: &SemanticCancellationToken,
        progress: impl FnMut(SemanticIndexProgress),
    ) -> Result<SemanticIndexResponse> {
        self.refresh_or_prewarm_with_budget(
            request,
            embedder,
            cancellation,
            EmbeddingBudget::unbounded(),
            progress,
        )
    }

    pub fn refresh_or_prewarm_with_budget(
        &mut self,
        request: &SemanticIndexRequest<'_>,
        embedder: &mut dyn SemanticEmbedder,
        cancellation: &SemanticCancellationToken,
        budget: EmbeddingBudget,
        mut progress: impl FnMut(SemanticIndexProgress),
    ) -> Result<SemanticIndexResponse> {
        let refreshed = self.refresh_passages_with_budget(
            request,
            embedder,
            cancellation,
            budget,
            &mut progress,
        )?;
        if refreshed.progress == SemanticIndexProgress::EmptyCorpus || request.prewarm {
            return Ok(refreshed);
        }
        let ranked = self.rank_refreshed(request, embedder, cancellation, progress)?;
        Ok(SemanticIndexResponse {
            missing_chunk_count: refreshed.missing_chunk_count,
            store_use: refreshed.store_use,
            ..ranked
        })
    }

    #[cfg(test)]
    pub(crate) fn with_store(
        chunk_config: ChunkConfig,
        store: PassageStore,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            signature: None,
            resident: ResidentIndex::default(),
            store,
            chunk_config,
            clock,
        }
    }
}

fn any_builds_a_chunk(candidates: &[SemanticIndexCandidate]) -> bool {
    candidates
        .iter()
        .any(|candidate| builds_any_chunk(&candidate.conversation))
}

/// Each candidate conversation's message times, by its index in the corpus.
fn message_times_by_index(candidates: &[SemanticIndexCandidate]) -> HashMap<usize, &MessageTimes> {
    candidates
        .iter()
        .map(|candidate| (candidate.index, &candidate.conversation.message_times))
        .collect()
}

impl SemanticIndexState {
    fn signature_matches(&self, request: &SemanticIndexRequest<'_>) -> bool {
        self.signature
            .as_ref()
            .is_some_and(|signature| signature.matches(request.full_corpus))
    }

    fn refresh_response(
        &self,
        request: &SemanticIndexRequest<'_>,
        missing_chunk_count: usize,
        store_use: Option<StoreUse>,
    ) -> SemanticIndexResponse {
        SemanticIndexResponse {
            hits: Vec::new(),
            chunk_hits: Vec::new(),
            indexed_chunk_count: self.resident.chunk_count(),
            missing_chunk_count,
            query_embedding_returned: true,
            progress: if self.resident.chunk_count() == 0 {
                SemanticIndexProgress::EmptyCorpus
            } else {
                SemanticIndexProgress::CacheReady
            },
            prewarm: request.prewarm,
            store_use,
        }
    }
}

#[cfg(test)]
fn semantic_chunks(
    request: &SemanticIndexRequest<'_>,
    chunk_config: ChunkConfig,
) -> Vec<SemanticChunk> {
    candidate_chunks(request.scope, chunk_config)
}

#[cfg(test)]
fn full_corpus_chunks(
    request: &SemanticIndexRequest<'_>,
    chunk_config: ChunkConfig,
) -> Vec<SemanticChunk> {
    candidate_chunks(request.full_corpus, chunk_config)
}

#[cfg(test)]
fn candidate_chunks(
    candidates: &[SemanticIndexCandidate],
    chunk_config: ChunkConfig,
) -> Vec<SemanticChunk> {
    build_chunks_with_sources(
        candidates.iter().map(|candidate| {
            (
                candidate.index,
                candidate.source,
                candidate.conversation.as_ref(),
            )
        }),
        chunk_config,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::TestClock;
    use crate::semantic::chunk::chunked_conversations;
    use crate::semantic::store::test_support::{
        store_passages, temporary_store, temporary_store_at, temporary_store_path,
    };
    use crate::semantic::test_fixtures::{SemanticConversationFixture, beta_hit_metadata};
    use crate::semantic::types::DEFAULT_EMBEDDING_BATCH_SIZE;
    use std::time::Duration;

    struct FakeEmbedder {
        passage_calls: usize,
        query_calls: usize,
        embedded_passages: Vec<Vec<String>>,
        query_embedding: Option<Vec<f32>>,
        drops_last_passage_embedding: bool,
    }

    impl FakeEmbedder {
        fn new() -> Self {
            Self {
                passage_calls: 0,
                query_calls: 0,
                embedded_passages: Vec::new(),
                query_embedding: Some(vec![1.0, 0.0]),
                drops_last_passage_embedding: false,
            }
        }
    }

    impl SemanticEmbedder for FakeEmbedder {
        fn dimensions(&self) -> usize {
            2
        }

        fn embed_passages(&mut self, passages: &[String]) -> Result<Vec<Vec<f32>>> {
            self.passage_calls += 1;
            self.embedded_passages.push(passages.to_vec());
            let mut embeddings = passages
                .iter()
                .map(|passage| match passage.as_str() {
                    "visible alpha" => vec![1.0, 0.0],
                    "visible beta" => vec![0.0, 1.0],
                    _ => vec![0.5, 0.5],
                })
                .collect::<Vec<_>>();
            if self.drops_last_passage_embedding {
                embeddings.pop();
            }
            Ok(embeddings)
        }

        fn embed_query(&mut self, query: &str) -> Result<Option<Vec<f32>>> {
            self.query_calls += 1;
            Ok(if query.contains("beta") {
                Some(vec![0.0, 1.0])
            } else {
                self.query_embedding.clone()
            })
        }
    }

    /// Spends `batch_takes` of `clock`'s time on each batch of passages.
    struct SlowEmbedder {
        clock: Arc<TestClock>,
        batch_takes: Duration,
        passage_calls: usize,
    }

    impl SemanticEmbedder for SlowEmbedder {
        fn dimensions(&self) -> usize {
            2
        }

        fn embed_passages(&mut self, passages: &[String]) -> Result<Vec<Vec<f32>>> {
            self.passage_calls += 1;
            self.clock.sleep(self.batch_takes);
            Ok(passages.iter().map(|_| vec![0.5, 0.5]).collect())
        }

        fn embed_query(&mut self, _query: &str) -> Result<Option<Vec<f32>>> {
            Ok(Some(vec![1.0, 0.0]))
        }
    }

    fn conversation(path: &str, semantic_turns: Vec<&str>) -> Conversation {
        SemanticConversationFixture::new(path, semantic_turns).build()
    }

    fn request(
        query: &str,
        conversations: Vec<Conversation>,
        candidate_indices: Vec<usize>,
    ) -> (String, Vec<SemanticIndexCandidate>) {
        let candidates = candidate_indices
            .into_iter()
            .map(|index| SemanticIndexCandidate {
                index,
                source: SemanticChunkSource::VisibleDialogue,
                conversation: Arc::new(conversations[index].clone()),
            })
            .collect();
        (query.to_string(), candidates)
    }

    fn index_request<'a>(
        query: &'a str,
        candidates: &'a [SemanticIndexCandidate],
    ) -> SemanticIndexRequest<'a> {
        SemanticIndexRequest {
            query,
            literal_filters: &[],
            full_corpus: candidates,
            scope: candidates,
            prewarm: false,
            chunk_hits: ChunkHits::None,
            window: TimeFilter::default(),
        }
    }

    /// A store of its own for the test's two-value embeddings.
    fn empty_store() -> PassageStore {
        temporary_store(Arc::new(TestClock::default()))
    }

    fn store_passage(store: &PassageStore, text: String, embedding: [f32; 2]) {
        store_passages(store, &[text.as_str()], embedding);
    }

    /// The distinct texts of `chunks` without an embedding in `store`.
    fn unstored_passage_count(store: &PassageStore, chunks: Vec<SemanticChunk>) -> usize {
        look_up_chunks(chunks, &mut store.open(), &SemanticCancellationToken::new())
            .expect("the lookup is not cancelled")
            .missing_passage_count()
    }

    fn candidates_from(conversations: &[Conversation]) -> Vec<SemanticIndexCandidate> {
        conversations
            .iter()
            .cloned()
            .enumerate()
            .map(|(index, conversation)| SemanticIndexCandidate {
                index,
                source: SemanticChunkSource::VisibleDialogue,
                conversation: Arc::new(conversation),
            })
            .collect()
    }

    fn store_request_passages(store: &PassageStore, request: &SemanticIndexRequest<'_>) {
        for chunk in full_corpus_chunks(request, ChunkConfig::default()) {
            let embedding = match chunk.text.as_str() {
                "visible beta" => [0.0, 1.0],
                "visible alpha" => [1.0, 0.0],
                _ => [0.5, 0.5],
            };
            store_passage(store, chunk.text, embedding);
        }
    }

    fn prepare_indexed_state(
        request: &SemanticIndexRequest<'_>,
        chunk_config: ChunkConfig,
    ) -> (SemanticIndexState, FakeEmbedder) {
        let store = empty_store();
        store_request_passages(&store, request);
        (
            SemanticIndexState::with_store(chunk_config, store, Arc::new(TestClock::default())),
            FakeEmbedder::new(),
        )
    }

    fn prepare_empty_state(chunk_config: ChunkConfig) -> (SemanticIndexState, FakeEmbedder) {
        (
            SemanticIndexState::with_store(
                chunk_config,
                empty_store(),
                Arc::new(TestClock::default()),
            ),
            FakeEmbedder::new(),
        )
    }

    fn run_refresh(
        state: &mut SemanticIndexState,
        request: &SemanticIndexRequest<'_>,
        embedder: &mut FakeEmbedder,
    ) -> Result<SemanticIndexResponse> {
        run_refresh_with_progress(state, request, embedder, |_| {})
    }

    fn run_refresh_with_progress(
        state: &mut SemanticIndexState,
        request: &SemanticIndexRequest<'_>,
        embedder: &mut FakeEmbedder,
        progress: impl FnMut(SemanticIndexProgress),
    ) -> Result<SemanticIndexResponse> {
        state.refresh_or_prewarm(
            request,
            embedder,
            &SemanticCancellationToken::new(),
            progress,
        )
    }

    fn run_refresh_passages(
        state: &mut SemanticIndexState,
        request: &SemanticIndexRequest<'_>,
        embedder: &mut FakeEmbedder,
    ) -> Result<SemanticIndexResponse> {
        state.refresh_passages(request, embedder, &SemanticCancellationToken::new(), |_| {})
    }

    fn assert_hit_indices(response: &SemanticIndexResponse, expected: &[usize]) {
        assert_eq!(
            response
                .hits
                .iter()
                .map(|hit| hit.conversation_index)
                .collect::<Vec<_>>(),
            expected
        );
    }

    #[test]
    fn ranks_original_indices_and_records_hits() {
        let conversations = vec![
            conversation("/projects/project-a/session-a.jsonl", vec!["visible alpha"]),
            conversation("/projects/project-a/session-b.jsonl", vec!["visible beta"]),
        ];
        let (query, candidates) = request("beta", conversations, vec![1, 0]);
        let request = index_request(&query, &candidates);
        let (mut state, mut embedder) = prepare_indexed_state(&request, ChunkConfig::default());

        let response = run_refresh(&mut state, &request, &mut embedder).expect("rank succeeds");

        assert_hit_indices(&response, &[1, 0]);
        let metadata = response
            .hits
            .iter()
            .find(|hit| hit.conversation_index == 1)
            .expect("beta hit");
        let (expected_score_breakdown, expected_explanation) = beta_hit_metadata(1, "session-b");
        assert_eq!(metadata.score_breakdown, expected_score_breakdown);
        assert_eq!(metadata.explanation, expected_explanation);
    }

    #[test]
    fn chunk_hits_are_built_only_when_request_asks_for_them() {
        let conversations = vec![
            conversation("/projects/project-a/session-a.jsonl", vec!["visible alpha"]),
            conversation("/projects/project-a/session-b.jsonl", vec!["visible beta"]),
        ];
        let (query, candidates) = request("alpha", conversations, vec![0, 1]);
        let mut request = index_request(&query, &candidates);
        let (mut state, mut embedder) = prepare_indexed_state(&request, ChunkConfig::default());

        let without = run_refresh(&mut state, &request, &mut embedder).expect("rank succeeds");
        request.chunk_hits = ChunkHits::TopConversations(2);
        let with = run_refresh(&mut state, &request, &mut embedder).expect("rank succeeds");

        assert_eq!(without.hits.len(), 2);
        assert!(without.chunk_hits.is_empty());
        assert_eq!(with.chunk_hits.len(), 2);
    }

    #[test]
    fn literal_filters_require_hit_local_text() {
        let mut first = conversation("/projects/project-a/session-a.jsonl", vec!["visible alpha"]);
        first.full_text = "visible alpha conversation-level literal needle".to_string();
        let conversations = vec![first];
        let all = candidates_from(&conversations);
        let literals = vec![Literal::new("literal needle".to_string())];
        let query = "alpha".to_string();
        let request = SemanticIndexRequest {
            query: &query,
            literal_filters: &literals,
            full_corpus: &all,
            scope: &all,
            prewarm: false,
            chunk_hits: ChunkHits::None,
            window: TimeFilter::default(),
        };
        let (mut state, mut embedder) = prepare_indexed_state(&request, ChunkConfig::default());

        let response = run_refresh(&mut state, &request, &mut embedder).expect("rank succeeds");

        assert_eq!(embedder.query_calls, 1);
        assert_eq!(response.progress, SemanticIndexProgress::Complete);
        assert!(response.hits.is_empty());
    }

    #[test]
    fn lower_scoring_literal_chunk_can_survive_filtering() {
        let conversations = vec![conversation(
            "/projects/project-a/session-a.jsonl",
            vec!["visible alpha", "visible gamma literal needle"],
        )];
        let all = candidates_from(&conversations);
        let literals = vec![Literal::new("literal needle".to_string())];
        let query = "alpha".to_string();
        let request = SemanticIndexRequest {
            query: &query,
            literal_filters: &literals,
            full_corpus: &all,
            scope: &all,
            prewarm: false,
            chunk_hits: ChunkHits::None,
            window: TimeFilter::default(),
        };
        let config = ChunkConfig {
            target_chars: 30,
            overlap_chars: 0,
            context_turns: 0,
        };
        let store = empty_store();
        for chunk in full_corpus_chunks(&request, config) {
            let embedding = if chunk.text.contains("alpha") {
                [1.0, 0.0]
            } else {
                [0.5, 0.5]
            };
            store_passage(&store, chunk.text, embedding);
        }
        let mut state =
            SemanticIndexState::with_store(config, store, Arc::new(TestClock::default()));
        let mut embedder = FakeEmbedder::new();

        let response = run_refresh(&mut state, &request, &mut embedder).expect("rank succeeds");

        assert_eq!(response.hits.len(), 1);
        assert_eq!(
            response.hits[0].explanation.evidence_preview,
            "visible gamma literal needle"
        );
        assert_eq!(
            response.hits[0].message_range,
            crate::agent::refs::MessageRange::single(2)
        );
    }

    /// m1 scores higher for the query; only m2 was written inside the window.
    #[test]
    fn passage_without_message_inside_window_is_dropped_before_ranking() {
        use crate::agent::time_span::TimeSpan;
        use chrono::TimeZone;

        let mut conversation = conversation(
            "/projects/project-a/session-a.jsonl",
            vec!["visible alpha", "visible gamma"],
        );
        let written_at = |time: &str| TimeSpan::of_timestamp(time);
        conversation.message_times = MessageTimes::new(&[
            written_at("2026-10-14T15:38:00Z"),
            written_at("2026-10-14T15:41:31Z"),
        ]);
        let all = candidates_from(&[conversation]);
        let query = "alpha".to_string();
        let local = |minute, second| {
            chrono::Utc
                .with_ymd_and_hms(2026, 10, 14, 15, minute, second)
                .unwrap()
                .with_timezone(&crate::clock::Zone::System)
        };
        let request = SemanticIndexRequest {
            window: TimeFilter {
                after: Some(local(41, 0)),
                before: Some(local(41, 59)),
            },
            ..index_request(&query, &all)
        };
        let config = ChunkConfig {
            target_chars: 15,
            overlap_chars: 0,
            context_turns: 0,
        };
        let store = empty_store();
        for chunk in full_corpus_chunks(&request, config) {
            let embedding = if chunk.text.contains("alpha") {
                [1.0, 0.0]
            } else {
                [0.5, 0.5]
            };
            store_passage(&store, chunk.text, embedding);
        }
        let mut state =
            SemanticIndexState::with_store(config, store, Arc::new(TestClock::default()));

        let response =
            run_refresh(&mut state, &request, &mut FakeEmbedder::new()).expect("rank succeeds");

        assert_eq!(response.hits.len(), 1);
        assert_eq!(
            response.hits[0].message_range,
            crate::agent::refs::MessageRange::single(2)
        );
    }

    #[test]
    fn literal_filter_no_match_is_not_empty_corpus() {
        let mut conversation =
            conversation("/projects/project-a/session-a.jsonl", vec!["visible alpha"]);
        conversation.full_text = "missing literal".to_string();
        let all = candidates_from(&[conversation]);
        let literals = vec![Literal::new("absent needle".to_string())];
        let query = "alpha".to_string();
        let request = SemanticIndexRequest {
            query: &query,
            literal_filters: &literals,
            full_corpus: &all,
            scope: &all,
            prewarm: false,
            chunk_hits: ChunkHits::None,
            window: TimeFilter::default(),
        };
        let (mut state, mut embedder) = prepare_indexed_state(&request, ChunkConfig::default());

        let response = run_refresh(&mut state, &request, &mut embedder).expect("rank succeeds");

        assert!(response.hits.is_empty());
        assert_eq!(response.progress, SemanticIndexProgress::Complete);
    }

    #[test]
    fn cache_hits_preserve_message_ranges() {
        let conversations = vec![conversation(
            "/projects/project-a/session-a.jsonl",
            vec!["visible alpha"],
        )];
        let (query, candidates) = request("alpha", conversations, vec![0]);
        let request = index_request(&query, &candidates);
        let (mut state, mut embedder) = prepare_indexed_state(&request, ChunkConfig::default());

        let response =
            run_refresh(&mut state, &request, &mut embedder).expect("cached rank succeeds");

        assert_eq!(embedder.passage_calls, 0);
        assert_eq!(
            response.hits[0].message_range,
            crate::agent::refs::MessageRange::single(1)
        );
    }

    #[test]
    fn cache_hits_preserve_candidate_source() {
        let conversations = vec![conversation(
            "/projects/project-a/session-a.jsonl",
            vec!["visible alpha"],
        )];
        let mut candidates = candidates_from(&conversations);
        candidates[0].source = SemanticChunkSource::AgentSubagentDialogue;
        let query = "alpha".to_string();
        let request = index_request(&query, &candidates);
        let (mut state, mut embedder) = prepare_indexed_state(&request, ChunkConfig::default());

        let response =
            run_refresh(&mut state, &request, &mut embedder).expect("cached rank succeeds");

        assert_eq!(embedder.passage_calls, 0);
        assert_eq!(
            response.hits[0].explanation.chunk.source,
            SemanticChunkSource::AgentSubagentDialogue
        );
    }

    #[test]
    fn source_aware_scope_filters_same_conversation_index_chunks() {
        let conversations = vec![conversation(
            "/projects/project-a/session-a.jsonl",
            vec!["visible alpha"],
        )];
        let visible = candidates_from(&conversations);
        let mut subagent = visible.clone();
        subagent[0].source = SemanticChunkSource::AgentSubagentDialogue;
        let mut all = visible.clone();
        all.extend(subagent);
        let query = "alpha".to_string();
        let request = SemanticIndexRequest {
            query: &query,
            literal_filters: &[],
            full_corpus: &all,
            scope: &visible,
            prewarm: false,
            chunk_hits: ChunkHits::None,
            window: TimeFilter::default(),
        };
        let (mut state, mut embedder) = prepare_indexed_state(&request, ChunkConfig::default());

        let response =
            run_refresh(&mut state, &request, &mut embedder).expect("scoped rank succeeds");

        assert!(!response.hits.is_empty());
        assert!(
            response
                .hits
                .iter()
                .all(|hit| hit.explanation.chunk.source == SemanticChunkSource::VisibleDialogue)
        );
    }

    #[test]
    fn reuses_passage_embeddings_for_same_candidate_signature() {
        let conversations = vec![
            conversation("/projects/project-a/session-a.jsonl", vec!["visible alpha"]),
            conversation("/projects/project-a/session-b.jsonl", vec!["visible beta"]),
        ];
        let (mut query, candidates) = request("alpha", conversations, vec![0, 1]);
        let mut request = index_request(&query, &candidates);
        let (mut state, mut embedder) = prepare_indexed_state(&request, ChunkConfig::default());

        run_refresh(&mut state, &request, &mut embedder).expect("first rank succeeds");
        query = "beta".to_string();
        request = index_request(&query, &candidates);
        run_refresh(&mut state, &request, &mut embedder).expect("second rank succeeds");

        assert_eq!(embedder.passage_calls, 0);
        assert_eq!(embedder.query_calls, 2);
    }

    #[test]
    fn unchanged_signature_reuses_embeddings_until_semantic_turns_change() {
        let (mut query, mut candidates) = request(
            "alpha",
            vec![conversation(
                "/projects/project-a/session-a.jsonl",
                vec!["visible alpha"],
            )],
            vec![0],
        );
        let mut request = index_request(&query, &candidates);
        let (mut state, mut embedder) = prepare_indexed_state(&request, ChunkConfig::default());

        run_refresh(&mut state, &request, &mut embedder).expect("first rank succeeds");
        query = "beta".to_string();
        request = index_request(&query, &candidates);
        run_refresh(&mut state, &request, &mut embedder).expect("same signature rank succeeds");
        candidates = vec![SemanticIndexCandidate {
            index: 0,
            source: SemanticChunkSource::VisibleDialogue,
            conversation: Arc::new(conversation(
                "/projects/project-a/session-a.jsonl",
                vec!["visible beta"],
            )),
        }];
        request = index_request(&query, &candidates);
        store_request_passages(&state.store, &request);
        run_refresh(&mut state, &request, &mut embedder).expect("changed signature rank succeeds");

        assert_eq!(embedder.passage_calls, 0);
        assert_eq!(embedder.query_calls, 3);
        assert!(embedder.embedded_passages.is_empty());
    }

    #[test]
    fn snippets_and_embeddings_use_only_semantic_turns() {
        let conversations = vec![conversation(
            "/projects/project-a/session-a.jsonl",
            vec!["visible alpha"],
        )];
        let (query, candidates) = request("alpha", conversations, vec![0]);
        let request = index_request(&query, &candidates);
        let (mut state, mut embedder) = prepare_indexed_state(&request, ChunkConfig::default());

        let response = run_refresh(&mut state, &request, &mut embedder).expect("rank succeeds");

        assert!(embedder.embedded_passages.is_empty());
        assert_eq!(
            response.hits[0].explanation.evidence_preview,
            "visible alpha"
        );
        assert!(
            !response.hits[0]
                .explanation
                .evidence_preview
                .contains("sentinel")
        );
    }

    #[test]
    fn missing_cached_passages_are_embedded_and_ranked() {
        let conversations = vec![conversation(
            "/projects/project-a/session-a.jsonl",
            vec!["visible alpha"],
        )];
        let (query, candidates) = request("alpha", conversations, vec![0]);
        let request = index_request(&query, &candidates);
        let (mut state, mut embedder) = prepare_empty_state(ChunkConfig::default());
        let mut progress = Vec::new();

        let response = run_refresh_with_progress(&mut state, &request, &mut embedder, |status| {
            progress.push(status)
        })
        .expect("missing cache embeds and ranks");

        assert_eq!(response.hits[0].conversation_index, 0);
        assert_eq!(response.progress, SemanticIndexProgress::Complete);
        assert_eq!(embedder.passage_calls, 1);
        assert_eq!(embedder.query_calls, 1);
        assert_eq!(
            response.store_use,
            Some(StoreUse {
                written: 1,
                ..StoreUse::default()
            }),
            "the refresh writes the passage it embedded"
        );
        assert_eq!(
            unstored_passage_count(
                &state.store,
                semantic_chunks(&request, ChunkConfig::default())
            ),
            0
        );
        assert!(progress.contains(&SemanticIndexProgress::Embedding {
            completed: 0,
            total: 1,
        }));
    }

    #[test]
    fn store_that_doesnt_open_still_ranks_the_passages_embedded() {
        use crate::semantic::store::SkipReason;
        let conversations = vec![
            conversation("/projects/project-a/session-a.jsonl", vec!["visible alpha"]),
            conversation("/projects/project-a/session-b.jsonl", vec!["visible beta"]),
        ];
        let (query, candidates) = request("beta", conversations, vec![0, 1]);
        let request = index_request(&query, &candidates);
        let clock = Arc::new(TestClock::default());
        let unopenable = temporary_store_path();
        // A folder where the store's file belongs: SQLite cannot open it.
        std::fs::create_dir_all(&unopenable).unwrap();
        let mut state = SemanticIndexState::with_store(
            ChunkConfig::default(),
            temporary_store_at(unopenable, clock.clone()),
            clock,
        );
        let mut embedder = FakeEmbedder::new();

        let response = run_refresh(&mut state, &request, &mut embedder).expect("rank succeeds");

        assert_hit_indices(&response, &[1, 0]);
        assert_eq!(
            response.store_use,
            Some(StoreUse {
                read: 0,
                written: 0,
                skipped: 2,
                skip_reason: Some(SkipReason::Error),
            }),
            "the refresh skips the write of both passages it embedded"
        );
    }

    #[test]
    fn refresh_of_unchanged_corpus_doesnt_open_the_store() {
        let conversations = vec![conversation(
            "/projects/project-a/session-a.jsonl",
            vec!["visible alpha"],
        )];
        let (query, candidates) = request("alpha", conversations, vec![0]);
        let request = index_request(&query, &candidates);
        let (mut state, mut embedder) = prepare_indexed_state(&request, ChunkConfig::default());

        let first = run_refresh(&mut state, &request, &mut embedder).expect("first rank succeeds");
        let second =
            run_refresh(&mut state, &request, &mut embedder).expect("second rank succeeds");

        assert_eq!(
            (first.store_use.map(|store| store.read), second.store_use),
            (Some(1), None),
            "the first refresh reads the passage; the second doesn't open the store"
        );
    }

    #[test]
    fn partial_cached_passages_embed_missing_chunks_and_rank_all() {
        let conversations = vec![
            conversation("/projects/project-a/session-a.jsonl", vec!["visible alpha"]),
            conversation("/projects/project-a/session-b.jsonl", vec!["visible beta"]),
        ];
        let (query, candidates) = request("alpha", conversations, vec![0, 1]);
        let request = index_request(&query, &candidates);
        let store = empty_store();
        let first_chunk = semantic_chunks(&request, ChunkConfig::default())
            .into_iter()
            .find(|chunk| chunk.text == "visible alpha")
            .expect("alpha chunk");
        store_passage(&store, first_chunk.text, [1.0, 0.0]);
        let mut state = SemanticIndexState::with_store(
            ChunkConfig::default(),
            store,
            Arc::new(TestClock::default()),
        );
        let mut embedder = FakeEmbedder::new();
        let mut progress = Vec::new();

        let response = run_refresh_with_progress(&mut state, &request, &mut embedder, |status| {
            progress.push(status)
        })
        .expect("partial cache embeds misses and ranks all chunks");

        let filtered = response
            .hits
            .iter()
            .map(|hit| hit.conversation_index)
            .collect::<Vec<_>>();
        assert_eq!(filtered, vec![0, 1]);
        assert_eq!(response.progress, SemanticIndexProgress::Complete);
        assert_eq!(embedder.passage_calls, 1);
        assert_eq!(embedder.query_calls, 1);
        assert_eq!(
            embedder.embedded_passages,
            vec![vec!["visible beta".to_string()]]
        );
        assert!(progress.contains(&SemanticIndexProgress::Embedding {
            completed: 0,
            total: 1,
        }));
    }

    #[test]
    fn invalid_embedding_batch_leaves_index_incomplete() {
        let conversations = vec![
            conversation("/projects/project-a/session-a.jsonl", vec!["visible alpha"]),
            conversation("/projects/project-a/session-b.jsonl", vec!["visible beta"]),
        ];
        let (query, candidates) = request("alpha", conversations, vec![0, 1]);
        let request = index_request(&query, &candidates);
        let (mut state, mut embedder) = prepare_empty_state(ChunkConfig::default());
        embedder.drops_last_passage_embedding = true;

        let failed = run_refresh(&mut state, &request, &mut embedder);
        embedder.drops_last_passage_embedding = false;
        let response = run_refresh(&mut state, &request, &mut embedder).expect("rank succeeds");

        assert!(matches!(failed, Err(AppError::InvalidEmbeddings(_))));
        assert_eq!(embedder.passage_calls, 2);
        assert_eq!(response.hits.len(), 2);
    }

    #[test]
    fn refresh_passages_can_skip_per_batch_cache_saves() {
        let conversations = vec![conversation(
            "/projects/project-a/session-a.jsonl",
            vec!["visible alpha"],
        )];
        let (query, candidates) = request("alpha", conversations, vec![0]);
        let request = index_request(&query, &candidates);
        let (mut state, mut embedder) = prepare_empty_state(ChunkConfig::default());

        let response =
            run_refresh_passages(&mut state, &request, &mut embedder).expect("refresh succeeds");

        assert_eq!(response.indexed_chunk_count, 1);
        assert_eq!(response.progress, SemanticIndexProgress::CacheReady);
        assert_eq!(embedder.passage_calls, 1);
        assert_eq!(embedder.query_calls, 0);
        assert_eq!(
            unstored_passage_count(
                &state.store,
                semantic_chunks(&request, ChunkConfig::default())
            ),
            0
        );
    }

    #[test]
    fn rank_refreshed_index_reports_missing_query_embedding() {
        let conversations = vec![conversation(
            "/projects/project-a/session-a.jsonl",
            vec!["visible alpha"],
        )];
        let (query, candidates) = request("alpha", conversations, vec![0]);
        let request = index_request(&query, &candidates);
        let (mut state, mut embedder) = prepare_empty_state(ChunkConfig::default());
        run_refresh_passages(&mut state, &request, &mut embedder).expect("refresh succeeds");
        embedder.query_embedding = None;

        let response = state
            .rank_refreshed(
                &request,
                &mut embedder,
                &SemanticCancellationToken::new(),
                |_| {},
            )
            .expect("rank succeeds");

        assert!(response.hits.is_empty());
        assert!(!response.query_embedding_returned);
        assert_eq!(response.indexed_chunk_count, 1);
        assert_eq!(embedder.passage_calls, 1);
        assert_eq!(embedder.query_calls, 1);
    }

    #[test]
    fn prewarm_request_builds_cache_without_ranking_query() {
        let conversations = vec![conversation(
            "/projects/project-a/session-a.jsonl",
            vec!["visible alpha"],
        )];
        let (query, candidates) = request("", conversations, vec![0]);
        let request = SemanticIndexRequest {
            query: &query,
            literal_filters: &[],
            full_corpus: &candidates,
            scope: &candidates,
            prewarm: true,
            chunk_hits: ChunkHits::None,
            window: TimeFilter::default(),
        };
        let (mut state, mut embedder) = prepare_empty_state(ChunkConfig::default());

        let response = run_refresh(&mut state, &request, &mut embedder).expect("prewarm succeeds");

        assert!(response.hits.is_empty());
        assert_eq!(response.indexed_chunk_count, 1);
        assert_eq!(response.progress, SemanticIndexProgress::CacheReady);
        assert_eq!(embedder.passage_calls, 1);
        assert_eq!(embedder.query_calls, 0);
        assert_eq!(
            unstored_passage_count(
                &state.store,
                semantic_chunks(&request, ChunkConfig::default())
            ),
            0
        );
    }

    #[test]
    fn reports_indexed_chunk_count() {
        let conversations = vec![
            conversation("/projects/project-a/session-a.jsonl", vec!["visible alpha"]),
            conversation("/projects/project-a/session-b.jsonl", vec!["visible beta"]),
        ];
        let (query, candidates) = request("beta", conversations, vec![1, 0]);
        let request = index_request(&query, &candidates);
        let (mut state, mut embedder) = prepare_indexed_state(&request, ChunkConfig::default());

        let response = run_refresh(&mut state, &request, &mut embedder).expect("rank succeeds");

        assert_eq!(response.indexed_chunk_count, 2);
        assert_hit_indices(&response, &[1, 0]);
    }

    #[test]
    fn semantic_index_ranks_more_than_legacy_limit_without_cap() {
        const LEGACY_LIMIT: usize = 100;
        let conversations = (0..LEGACY_LIMIT + 25)
            .map(|index| {
                conversation(
                    &format!("/projects/project-a/session-{index}.jsonl"),
                    vec!["visible alpha"],
                )
            })
            .collect::<Vec<_>>();
        let candidate_indices = (0..conversations.len()).collect::<Vec<_>>();
        let (query, candidates) = request("alpha", conversations, candidate_indices);
        let request = index_request(&query, &candidates);
        let (mut state, mut embedder) = prepare_indexed_state(&request, ChunkConfig::default());

        let response = run_refresh(&mut state, &request, &mut embedder).expect("rank succeeds");

        assert_eq!(response.indexed_chunk_count, LEGACY_LIMIT + 25);
        assert_eq!(response.hits.len(), LEGACY_LIMIT + 25);
        assert_hit_indices(&response, &(0..LEGACY_LIMIT + 25).collect::<Vec<_>>());
    }

    #[test]
    fn warm_full_corpus_reuses_embeddings_across_scope_toggles() {
        let conversations = [
            conversation("/projects/project-a/session-a.jsonl", vec!["visible alpha"]),
            conversation("/projects/project-a/session-b.jsonl", vec!["visible beta"]),
        ];
        let all = (0..conversations.len())
            .map(|index| SemanticIndexCandidate {
                index,
                source: SemanticChunkSource::VisibleDialogue,
                conversation: Arc::new(conversations[index].clone()),
            })
            .collect::<Vec<_>>();
        let alpha_scope = vec![all[0].clone()];
        let beta_scope = vec![all[1].clone()];
        let alpha_query = "alpha".to_string();
        let alpha_request = SemanticIndexRequest {
            query: &alpha_query,
            literal_filters: &[],
            full_corpus: &all,
            scope: &alpha_scope,
            prewarm: false,
            chunk_hits: ChunkHits::None,
            window: TimeFilter::default(),
        };
        let (mut state, mut embedder) =
            prepare_indexed_state(&alpha_request, ChunkConfig::default());

        let alpha =
            run_refresh(&mut state, &alpha_request, &mut embedder).expect("alpha scope ranks");
        let beta_query = "beta".to_string();
        let beta_request = SemanticIndexRequest {
            query: &beta_query,
            literal_filters: &[],
            full_corpus: &all,
            scope: &beta_scope,
            prewarm: false,
            chunk_hits: ChunkHits::None,
            window: TimeFilter::default(),
        };
        let beta = run_refresh(&mut state, &beta_request, &mut embedder).expect("beta scope ranks");

        assert_eq!(embedder.passage_calls, 0);
        assert_eq!(embedder.query_calls, 2);
        assert_eq!(alpha.indexed_chunk_count, 2);
        assert_eq!(beta.indexed_chunk_count, 2);
        assert_eq!(alpha.hits[0].conversation_index, 0);
        assert_eq!(beta.hits[0].conversation_index, 1);
    }

    #[test]
    fn empty_scope_reuses_warm_corpus_without_query_embedding() {
        let conversations = vec![conversation(
            "/projects/project-a/session-a.jsonl",
            vec!["visible alpha"],
        )];
        let (query, candidates) = request("alpha", conversations, vec![0]);
        let populated = index_request(&query, &candidates);
        let (mut state, mut embedder) = prepare_indexed_state(&populated, ChunkConfig::default());
        run_refresh(&mut state, &populated, &mut embedder).expect("warm corpus");
        let empty_request = SemanticIndexRequest {
            query: &query,
            literal_filters: &[],
            full_corpus: &candidates,
            scope: &[],
            prewarm: false,
            chunk_hits: ChunkHits::None,
            window: TimeFilter::default(),
        };

        let response =
            run_refresh(&mut state, &empty_request, &mut embedder).expect("empty scope returns");

        assert!(response.hits.is_empty());
        assert_eq!(response.indexed_chunk_count, 1);
        assert_eq!(response.progress, SemanticIndexProgress::EmptyCorpus);
        assert_eq!(embedder.passage_calls, 0);
        assert_eq!(embedder.query_calls, 1);
    }

    #[test]
    fn persistent_scoped_ranking_matches_request_scoped_ranking() {
        let conversations = vec![
            conversation("/projects/project-a/session-a.jsonl", vec!["visible alpha"]),
            conversation("/projects/project-a/session-b.jsonl", vec!["visible beta"]),
            conversation("/projects/project-a/session-c.jsonl", vec!["visible gamma"]),
        ];
        let all = candidates_from(&conversations);
        let scope = vec![all[1].clone(), all[0].clone()];
        let query = "beta".to_string();
        let persistent_request = SemanticIndexRequest {
            query: &query,
            literal_filters: &[],
            full_corpus: &all,
            scope: &scope,
            prewarm: false,
            chunk_hits: ChunkHits::None,
            window: TimeFilter::default(),
        };
        let scoped_request = index_request(&query, &scope);
        let (mut persistent_state, mut persistent_embedder) =
            prepare_indexed_state(&persistent_request, ChunkConfig::default());
        let (mut scoped_state, mut scoped_embedder) =
            prepare_indexed_state(&scoped_request, ChunkConfig::default());

        let persistent = run_refresh(
            &mut persistent_state,
            &persistent_request,
            &mut persistent_embedder,
        )
        .expect("persistent rank succeeds");
        let scoped = run_refresh(&mut scoped_state, &scoped_request, &mut scoped_embedder)
            .expect("scoped rank succeeds");

        assert_eq!(persistent.hits, scoped.hits);
    }

    #[test]
    fn corpus_reorder_updates_hit_indices_without_reembedding() {
        let first = vec![
            conversation("/projects/project-a/session-a.jsonl", vec!["visible alpha"]),
            conversation("/projects/project-a/session-b.jsonl", vec!["visible beta"]),
        ];
        let first_all = candidates_from(&first);
        let query = "alpha".to_string();
        let first_request = SemanticIndexRequest {
            query: &query,
            literal_filters: &[],
            full_corpus: &first_all,
            scope: &first_all,
            prewarm: false,
            chunk_hits: ChunkHits::None,
            window: TimeFilter::default(),
        };
        let (mut state, mut embedder) =
            prepare_indexed_state(&first_request, ChunkConfig::default());
        run_refresh(&mut state, &first_request, &mut embedder).expect("first corpus ranks");
        let reordered = vec![first[1].clone(), first[0].clone()];
        let reordered_all = candidates_from(&reordered);
        let reordered_request = SemanticIndexRequest {
            query: &query,
            literal_filters: &[],
            full_corpus: &reordered_all,
            scope: &reordered_all,
            prewarm: false,
            chunk_hits: ChunkHits::None,
            window: TimeFilter::default(),
        };

        let response = run_refresh(&mut state, &reordered_request, &mut embedder)
            .expect("reordered corpus ranks");

        assert_eq!(embedder.passage_calls, 0);
        assert_eq!(response.hits[0].conversation_index, 1);
        assert_eq!(response.hits[0].session, "session-a");
    }

    #[test]
    fn changed_and_new_conversations_embed_without_reembedding_unchanged() {
        let first = vec![conversation(
            "/projects/project-a/session-a.jsonl",
            vec!["visible alpha"],
        )];
        let first_all = candidates_from(&first);
        let query = "alpha".to_string();
        let first_request = SemanticIndexRequest {
            query: &query,
            literal_filters: &[],
            full_corpus: &first_all,
            scope: &first_all,
            prewarm: false,
            chunk_hits: ChunkHits::None,
            window: TimeFilter::default(),
        };
        let (mut state, mut embedder) =
            prepare_indexed_state(&first_request, ChunkConfig::default());
        run_refresh(&mut state, &first_request, &mut embedder).expect("first corpus ranks");
        let updated = vec![
            conversation("/projects/project-a/session-a.jsonl", vec!["visible delta"]),
            conversation("/projects/project-a/session-b.jsonl", vec!["visible beta"]),
            conversation("/projects/project-a/session-c.jsonl", vec!["visible gamma"]),
        ];
        let updated_all = candidates_from(&updated);
        let updated_request = SemanticIndexRequest {
            query: &query,
            literal_filters: &[],
            full_corpus: &updated_all,
            scope: &updated_all,
            prewarm: false,
            chunk_hits: ChunkHits::None,
            window: TimeFilter::default(),
        };

        run_refresh(&mut state, &updated_request, &mut embedder).expect("updated corpus ranks");

        assert_eq!(embedder.passage_calls, 1);
        assert_eq!(
            embedder.embedded_passages,
            vec![vec![
                "visible delta".to_string(),
                "visible beta".to_string(),
                "visible gamma".to_string()
            ]]
        );
    }

    #[test]
    fn empty_and_removed_conversations_are_excluded_after_corpus_update() {
        let first = vec![
            conversation("/projects/project-a/session-a.jsonl", vec!["visible alpha"]),
            conversation("/projects/project-a/session-b.jsonl", vec!["visible beta"]),
        ];
        let first_all = candidates_from(&first);
        let query = "alpha".to_string();
        let first_request = SemanticIndexRequest {
            query: &query,
            literal_filters: &[],
            full_corpus: &first_all,
            scope: &first_all,
            prewarm: false,
            chunk_hits: ChunkHits::None,
            window: TimeFilter::default(),
        };
        let (mut state, mut embedder) =
            prepare_indexed_state(&first_request, ChunkConfig::default());
        run_refresh(&mut state, &first_request, &mut embedder).expect("first corpus ranks");
        let updated = vec![conversation("/projects/project-a/session-a.jsonl", vec![])];
        let updated_all = candidates_from(&updated);
        let updated_request = SemanticIndexRequest {
            query: &query,
            literal_filters: &[],
            full_corpus: &updated_all,
            scope: &updated_all,
            prewarm: false,
            chunk_hits: ChunkHits::None,
            window: TimeFilter::default(),
        };

        let response = run_refresh(&mut state, &updated_request, &mut embedder)
            .expect("empty corpus update succeeds");

        assert!(response.hits.is_empty());
        assert_eq!(response.indexed_chunk_count, 0);
        assert_eq!(embedder.passage_calls, 0);
    }

    #[test]
    fn cached_signature_reports_cache_ready_before_ranking() {
        let conversations = vec![conversation(
            "/projects/project-a/session-a.jsonl",
            vec!["visible alpha"],
        )];
        let (query, candidates) = request("alpha", conversations, vec![0]);
        let request = index_request(&query, &candidates);
        let (mut state, mut embedder) = prepare_indexed_state(&request, ChunkConfig::default());
        run_refresh(&mut state, &request, &mut embedder).expect("first rank succeeds");
        let mut progress = Vec::new();

        run_refresh_with_progress(&mut state, &request, &mut embedder, |status| {
            progress.push(status)
        })
        .expect("second rank succeeds");

        assert_eq!(
            progress,
            vec![
                SemanticIndexProgress::CacheReady,
                SemanticIndexProgress::Ranking
            ]
        );
        assert_eq!(embedder.passage_calls, 0);
    }

    #[test]
    fn clear_empty_replaces_populated_index_state() {
        let populated = vec![conversation(
            "/projects/project-a/session-a.jsonl",
            vec!["visible alpha"],
        )];
        let (query, populated_candidates) = request("alpha", populated, vec![0]);
        let populated_request = index_request(&query, &populated_candidates);
        let (mut state, mut embedder) =
            prepare_indexed_state(&populated_request, ChunkConfig::default());
        run_refresh(&mut state, &populated_request, &mut embedder)
            .expect("populated index succeeds");
        let empty = vec![conversation("/projects/project-a/session-a.jsonl", vec![])];
        let (empty_query, empty_candidates) = request("alpha", empty, vec![0]);
        let empty_request = index_request(&empty_query, &empty_candidates);

        state
            .clear_empty(&empty_request, &SemanticCancellationToken::new())
            .unwrap();

        assert!(state.signature_matches(&empty_request));
        assert_eq!(state.resident.chunk_count(), 0);
        assert!(
            !state
                .has_chunks(&empty_request, &SemanticCancellationToken::new())
                .unwrap()
        );
    }

    /// Each repeated query asks whether its scope holds chunks before it
    /// waits for the model.
    #[test]
    fn has_chunks_answers_for_the_scope_without_building_chunks() {
        // Paths no other test uses: the record holds every test's chunk builds.
        let paths = [
            "/projects/has-chunks-without-building/session-a.jsonl",
            "/projects/has-chunks-without-building/session-b.jsonl",
        ];
        let conversations = vec![
            conversation(paths[0], vec!["visible alpha"]),
            conversation(paths[1], vec!["  "]),
        ];
        let (query, candidates) = request("alpha", conversations, vec![0, 1]);
        let full = index_request(&query, &candidates);
        let (mut state, mut embedder) = prepare_indexed_state(&full, ChunkConfig::default());
        let chunk_builds = || {
            paths
                .iter()
                .map(|path| chunked_conversations::count(std::path::Path::new(path)))
                .sum::<usize>()
        };
        let answers_by_scope = |state: &SemanticIndexState| {
            let chunked_before = chunk_builds();
            let answers = [&candidates[..1], &candidates[1..]].map(|scope| {
                let request = SemanticIndexRequest {
                    scope,
                    ..index_request(&query, &candidates)
                };
                state
                    .has_chunks(&request, &SemanticCancellationToken::new())
                    .unwrap()
            });
            let chunked = chunk_builds() - chunked_before;
            (answers, chunked)
        };

        let before_refresh = answers_by_scope(&state);
        run_refresh(&mut state, &full, &mut embedder).expect("the index refreshes");
        let refreshed = answers_by_scope(&state);

        assert_eq!(
            before_refresh,
            ([true, false], 0),
            "the index not refreshed for the corpus answers from the semantic turns"
        );
        assert!(state.signature_matches(&full));
        assert_eq!(
            refreshed,
            ([true, false], 0),
            "the index refreshed for the corpus answers from its chunks"
        );
    }

    #[test]
    fn empty_visible_dialogue_returns_without_embedding() {
        let conversations = vec![conversation("/projects/project-a/session-a.jsonl", vec![])];
        let (query, candidates) = request("alpha", conversations, vec![0]);
        let request = index_request(&query, &candidates);
        let (mut state, mut embedder) = prepare_empty_state(ChunkConfig::default());

        let response =
            run_refresh(&mut state, &request, &mut embedder).expect("empty corpus succeeds");

        assert!(response.hits.is_empty());
        assert_eq!(response.progress, SemanticIndexProgress::EmptyCorpus);
        assert_eq!(embedder.passage_calls, 0);
        assert_eq!(embedder.query_calls, 0);
        assert!(
            !state
                .has_chunks(&request, &SemanticCancellationToken::new())
                .unwrap()
        );
    }

    /// The hits a new index built on `request`'s corpus alone returns.
    fn fresh_index_hits(request: &SemanticIndexRequest<'_>) -> Vec<SemanticHit> {
        let (mut state, mut embedder) = prepare_indexed_state(request, ChunkConfig::default());
        run_refresh(&mut state, request, &mut embedder)
            .expect("fresh index ranks")
            .hits
    }

    /// Three sessions under `/projects/<project>`. A test that counts chunk
    /// builds names a project no other test uses: the record holds every
    /// test's chunk builds.
    fn three_sessions(project: &str) -> Vec<Conversation> {
        [
            ("session-a", "visible alpha"),
            ("session-b", "visible beta"),
            ("session-c", "visible gamma"),
        ]
        .into_iter()
        .map(|(session, text)| {
            conversation(&format!("/projects/{project}/{session}.jsonl"), vec![text])
        })
        .collect()
    }

    /// The times each of `sessions` had its chunks built so far in this test
    /// process. A refresh prepares a conversation's chunks by building them.
    fn chunk_builds(sessions: &[Conversation]) -> Vec<usize> {
        sessions
            .iter()
            .map(|session| chunked_conversations::count(&session.path))
            .collect()
    }

    /// The times each of `sessions` had its chunks built since `before` was
    /// taken.
    fn chunk_builds_since(before: &[usize], sessions: &[Conversation]) -> Vec<usize> {
        chunk_builds(sessions)
            .iter()
            .zip(before)
            .map(|(now, before)| now - before)
            .collect()
    }

    #[test]
    fn reorder_doesnt_prepare_any_chunks_and_ranks_as_fresh_index_does() {
        let first = three_sessions("reorder-prepares-nothing");
        let first_all = candidates_from(&first);
        let first_request = index_request("alpha", &first_all);
        let (mut state, mut embedder) =
            prepare_indexed_state(&first_request, ChunkConfig::default());
        run_refresh(&mut state, &first_request, &mut embedder).expect("first corpus ranks");
        let reordered = vec![first[2].clone(), first[0].clone(), first[1].clone()];
        let reordered_all = candidates_from(&reordered);
        let reordered_request = index_request("alpha", &reordered_all);
        let builds = chunk_builds(&reordered);

        let response = run_refresh(&mut state, &reordered_request, &mut embedder)
            .expect("reordered corpus ranks");

        assert_eq!(chunk_builds_since(&builds, &reordered), [0, 0, 0]);
        assert_eq!(
            response.store_use, None,
            "the refresh of the reordered corpus doesn't open the store"
        );
        assert_eq!(response.hits, fresh_index_hits(&reordered_request));
    }

    #[test]
    fn new_session_at_top_prepares_only_its_own_chunks() {
        let sessions = three_sessions("new-session-prepares-its-own");
        let first = sessions[..2].to_vec();
        let first_all = candidates_from(&first);
        let first_request = index_request("alpha", &first_all);
        let (mut state, mut embedder) =
            prepare_indexed_state(&first_request, ChunkConfig::default());
        run_refresh(&mut state, &first_request, &mut embedder).expect("first corpus ranks");
        let mut extended = vec![sessions[2].clone()];
        extended.extend(first);
        let extended_all = candidates_from(&extended);
        let extended_request = index_request("alpha", &extended_all);
        let builds = chunk_builds(&extended);

        let response = run_refresh(&mut state, &extended_request, &mut embedder)
            .expect("extended corpus ranks");

        assert_eq!(chunk_builds_since(&builds, &extended), [1, 0, 0]);
        assert_eq!(response.hits, fresh_index_hits(&extended_request));
    }

    #[test]
    fn rename_or_refresh_with_unchanged_text_doesnt_prepare_any_chunks() {
        let first = three_sessions("rename-prepares-nothing");
        let first_all = candidates_from(&first);
        let first_request = index_request("alpha", &first_all);
        let (mut state, mut embedder) =
            prepare_indexed_state(&first_request, ChunkConfig::default());
        run_refresh(&mut state, &first_request, &mut embedder).expect("first corpus ranks");
        let mut refreshed = first.clone();
        refreshed[0].custom_title = Some("renamed".to_string());
        let refreshed_all = candidates_from(&refreshed);
        let refreshed_request = index_request("alpha", &refreshed_all);
        let builds = chunk_builds(&refreshed);
        let mut progress = Vec::new();

        run_refresh_with_progress(&mut state, &refreshed_request, &mut embedder, |status| {
            progress.push(status)
        })
        .expect("refreshed corpus ranks");

        assert_eq!(chunk_builds_since(&builds, &refreshed), [0, 0, 0]);
        assert_eq!(
            progress,
            vec![
                SemanticIndexProgress::CacheReady,
                SemanticIndexProgress::Ranking
            ]
        );
        assert_eq!(embedder.passage_calls, 0);
    }

    #[test]
    fn session_cut_short_by_budget_is_completed_without_preparing_others_again() {
        let sessions = three_sessions("cut-short-session-completed")[..2].to_vec();
        let all = candidates_from(&sessions);
        let request = index_request("alpha", &all);
        let (mut state, mut embedder) = prepare_empty_state(ChunkConfig::default());
        let partial = state
            .refresh_or_prewarm_with_budget(
                &request,
                &mut embedder,
                &SemanticCancellationToken::new(),
                EmbeddingBudget::passages(1),
                |_| {},
            )
            .expect("partial index ranks");
        assert_eq!(partial.missing_chunk_count, 1);
        let builds = chunk_builds(&sessions);

        let response = run_refresh(&mut state, &request, &mut embedder).expect("index completes");

        assert_eq!(response.missing_chunk_count, 0);
        assert_eq!(chunk_builds_since(&builds, &sessions), [0, 1]);
        assert!(state.signature_matches(&request));
        assert_eq!(response.hits, fresh_index_hits(&request));
    }

    /// With a two-second limit and 1.5-second batches on the index's own
    /// clock, the second batch starts at 1.5 s; at 3 s the limit has passed
    /// and the third does not start.
    #[test]
    fn index_stops_embedding_once_time_limit_has_passed_on_its_clock() {
        const BATCHES: usize = 4;
        let texts = (0..BATCHES * DEFAULT_EMBEDDING_BATCH_SIZE)
            .map(|index| format!("visible passage {index}"))
            .collect::<Vec<_>>();
        let sessions = texts
            .iter()
            .enumerate()
            .map(|(index, text)| {
                conversation(
                    &format!("/projects/project-a/session-{index}.jsonl"),
                    vec![text.as_str()],
                )
            })
            .collect::<Vec<_>>();
        let all = candidates_from(&sessions);
        let request = index_request("passage", &all);
        let clock = Arc::new(TestClock::default());
        let mut state = SemanticIndexState::with_store(
            ChunkConfig::default(),
            temporary_store(clock.clone()),
            clock.clone(),
        );
        let mut embedder = SlowEmbedder {
            clock,
            batch_takes: Duration::from_millis(1500),
            passage_calls: 0,
        };

        let response = state
            .refresh_or_prewarm_with_budget(
                &request,
                &mut embedder,
                &SemanticCancellationToken::new(),
                EmbeddingBudget::seconds(2),
                |_| {},
            )
            .expect("the index ranks the passages it embedded");

        assert_eq!(embedder.passage_calls, 2);
        assert_eq!(
            response.missing_chunk_count,
            (BATCHES - 2) * DEFAULT_EMBEDDING_BATCH_SIZE
        );
        assert!(
            !state.signature_matches(&request),
            "an index cut short by the time limit is completed on the next request"
        );
    }

    #[test]
    fn session_listed_twice_ranks_at_both_positions() {
        let session = conversation("/projects/project-a/session-a.jsonl", vec!["visible alpha"]);
        let listed_twice = vec![session.clone(), session];
        let all = candidates_from(&listed_twice);
        let request = index_request("alpha", &all);
        let (mut state, mut embedder) = prepare_indexed_state(&request, ChunkConfig::default());

        let response = run_refresh(&mut state, &request, &mut embedder).expect("corpus ranks");

        assert_hit_indices(&response, &[0, 1]);
    }
}
