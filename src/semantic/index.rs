use crate::error::{AppError, Result};
use crate::history::{Conversation, MessageTimes};
use crate::search::literal::Literal;
use crate::semantic::cache::{
    cache_miss_count, embed_chunks_with_budget_and_save, read_embedding_cache,
};
#[cfg(test)]
use crate::semantic::chunk::build_chunks_with_sources;
use crate::semantic::embed::{SemanticEmbedder, embed_valid_query};
use crate::semantic::filter::matches_every_literal;
use crate::semantic::rank::rank_prepared;
#[cfg(test)]
use crate::semantic::types::SemanticChunk;
use crate::semantic::types::{
    ChunkConfig, ChunkHits, EmbeddingBudget, EmbeddingCache, SemanticCancellationToken,
    SemanticChunkSource, SemanticHit,
};
use crate::time_filter::TimeFilter;
use std::collections::HashMap;
use std::sync::Arc;

mod resident;
use resident::{CorpusSignature, ResidentIndex, corpus_has_chunks};

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
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SemanticIndexProgress {
    Embedding { completed: usize, total: usize },
    CacheReady,
    Ranking,
    Complete,
    EmptyCorpus,
}

pub struct SemanticIndexState {
    signature: Option<CorpusSignature>,
    resident: ResidentIndex,
    pub cache: EmbeddingCache,
    chunk_config: ChunkConfig,
}

impl SemanticIndexState {
    pub fn new() -> Self {
        Self::with_chunk_config(ChunkConfig::default())
    }

    pub fn with_chunk_config(chunk_config: ChunkConfig) -> Self {
        Self {
            signature: None,
            resident: ResidentIndex::default(),
            cache: read_embedding_cache(chunk_config),
            chunk_config,
        }
    }

    pub fn has_chunks(
        &self,
        request: &SemanticIndexRequest<'_>,
        cancellation: &SemanticCancellationToken,
    ) -> Result<bool> {
        if cancellation.is_cancelled() {
            return Err(AppError::SemanticSearchCancelled);
        }
        if self.signature_matches(request) {
            return Ok(self.resident.chunk_count() > 0);
        }
        Ok(corpus_has_chunks(request.full_corpus, self.chunk_config))
    }

    pub fn clear_empty(
        &mut self,
        request: &SemanticIndexRequest<'_>,
        cancellation: &SemanticCancellationToken,
    ) -> Result<()> {
        if cancellation.is_cancelled() {
            return Err(AppError::SemanticSearchCancelled);
        }
        self.signature = Some(CorpusSignature::of(request.full_corpus));
        self.resident.clear();
        Ok(())
    }

    pub fn refresh_passages(
        &mut self,
        request: &SemanticIndexRequest<'_>,
        embedder: &mut dyn SemanticEmbedder,
        cancellation: &SemanticCancellationToken,
        progress: impl FnMut(SemanticIndexProgress),
        save_cache: impl FnMut(&EmbeddingCache),
    ) -> Result<SemanticIndexResponse> {
        self.refresh_passages_with_budget(
            request,
            embedder,
            cancellation,
            EmbeddingBudget::unbounded(),
            progress,
            save_cache,
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
        save_cache: impl FnMut(&EmbeddingCache),
    ) -> Result<SemanticIndexResponse> {
        if cancellation.is_cancelled() {
            return Err(AppError::SemanticSearchCancelled);
        }
        if self.signature_matches(request) {
            progress(SemanticIndexProgress::CacheReady);
            return Ok(self.refresh_response(request, 0));
        }

        self.signature = None;
        let plan =
            self.resident
                .plan_refresh(request.full_corpus, self.chunk_config, cancellation)?;
        let miss_count = cache_miss_count(plan.chunks(), &self.cache);
        let embedding_count = budget.passages_within(miss_count);
        if embedding_count > 0 {
            progress(SemanticIndexProgress::Embedding {
                completed: 0,
                total: embedding_count,
            });
        }
        let (pending, chunks) = plan.into_parts();
        let outcome = embed_chunks_with_budget_and_save(
            embedder,
            chunks,
            &mut self.cache,
            cancellation,
            budget,
            |completed, total| {
                progress(SemanticIndexProgress::Embedding { completed, total });
            },
            save_cache,
        )?;
        self.resident.absorb(pending, outcome.embedded);
        if self.resident.is_complete() {
            self.signature = Some(CorpusSignature::of(request.full_corpus));
        }
        if embedding_count == 0 && self.resident.chunk_count() > 0 {
            progress(SemanticIndexProgress::CacheReady);
        }
        Ok(self.refresh_response(request, outcome.missing_chunk_count))
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
        })
    }

    pub fn refresh_or_prewarm(
        &mut self,
        request: &SemanticIndexRequest<'_>,
        embedder: &mut dyn SemanticEmbedder,
        cancellation: &SemanticCancellationToken,
        progress: impl FnMut(SemanticIndexProgress),
        save_cache: impl FnMut(&EmbeddingCache),
    ) -> Result<SemanticIndexResponse> {
        self.refresh_or_prewarm_with_budget(
            request,
            embedder,
            cancellation,
            EmbeddingBudget::unbounded(),
            progress,
            save_cache,
        )
    }

    pub fn refresh_or_prewarm_with_budget(
        &mut self,
        request: &SemanticIndexRequest<'_>,
        embedder: &mut dyn SemanticEmbedder,
        cancellation: &SemanticCancellationToken,
        budget: EmbeddingBudget,
        mut progress: impl FnMut(SemanticIndexProgress),
        save_cache: impl FnMut(&EmbeddingCache),
    ) -> Result<SemanticIndexResponse> {
        let response = self.refresh_passages_with_budget(
            request,
            embedder,
            cancellation,
            budget,
            &mut progress,
            save_cache,
        )?;
        if response.progress == SemanticIndexProgress::EmptyCorpus || request.prewarm {
            return Ok(response);
        }
        let missing_chunk_count = response.missing_chunk_count;
        let mut response = self.rank_refreshed(request, embedder, cancellation, progress)?;
        response.missing_chunk_count = missing_chunk_count;
        Ok(response)
    }

    #[cfg(test)]
    pub(crate) fn with_cache(chunk_config: ChunkConfig, cache: EmbeddingCache) -> Self {
        Self {
            signature: None,
            resident: ResidentIndex::default(),
            cache,
            chunk_config,
        }
    }
}

impl Default for SemanticIndexState {
    fn default() -> Self {
        Self::new()
    }
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
    use crate::semantic::cache::{cache_miss_count, embedding_cache_key, empty_embedding_cache};
    use crate::semantic::test_fixtures::{SemanticConversationFixture, beta_hit_metadata};
    use crate::semantic::types::CachedChunk;
    use std::collections::{HashMap, HashSet};

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

    fn cache_passage(cache: &mut EmbeddingCache, text: String, embedding: Vec<f32>) {
        cache.entries.insert(
            crate::semantic::cache::embedding_cache_key(&text),
            CachedChunk {
                embedding,
                last_used: 0,
                protected: false,
            },
        );
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

    fn cache_request_passages(cache: &mut EmbeddingCache, request: &SemanticIndexRequest<'_>) {
        for chunk in full_corpus_chunks(request, ChunkConfig::default()) {
            let embedding = match chunk.text.as_str() {
                "visible beta" => vec![0.0, 1.0],
                "visible alpha" => vec![1.0, 0.0],
                _ => vec![0.5, 0.5],
            };
            cache_passage(cache, chunk.text, embedding);
        }
    }

    fn prepare_indexed_state(
        request: &SemanticIndexRequest<'_>,
        chunk_config: ChunkConfig,
    ) -> (SemanticIndexState, FakeEmbedder) {
        let mut cache = empty_embedding_cache(chunk_config);
        cache_request_passages(&mut cache, request);
        (
            SemanticIndexState::with_cache(chunk_config, cache),
            FakeEmbedder::new(),
        )
    }

    fn prepare_empty_state(chunk_config: ChunkConfig) -> (SemanticIndexState, FakeEmbedder) {
        (
            SemanticIndexState::with_cache(chunk_config, empty_embedding_cache(chunk_config)),
            FakeEmbedder::new(),
        )
    }

    fn run_refresh(
        state: &mut SemanticIndexState,
        request: &SemanticIndexRequest<'_>,
        embedder: &mut FakeEmbedder,
    ) -> Result<SemanticIndexResponse> {
        state.refresh_or_prewarm(
            request,
            embedder,
            &SemanticCancellationToken::new(),
            |_| {},
            |_| {},
        )
    }

    fn run_refresh_with_observers(
        state: &mut SemanticIndexState,
        request: &SemanticIndexRequest<'_>,
        embedder: &mut FakeEmbedder,
        progress: impl FnMut(SemanticIndexProgress),
        save_cache: impl FnMut(&EmbeddingCache),
    ) -> Result<SemanticIndexResponse> {
        state.refresh_or_prewarm(
            request,
            embedder,
            &SemanticCancellationToken::new(),
            progress,
            save_cache,
        )
    }

    fn run_refresh_passages(
        state: &mut SemanticIndexState,
        request: &SemanticIndexRequest<'_>,
        embedder: &mut FakeEmbedder,
    ) -> Result<SemanticIndexResponse> {
        state.refresh_passages(
            request,
            embedder,
            &SemanticCancellationToken::new(),
            |_| {},
            |_| {},
        )
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
    fn chunk_hits_are_built_only_when_the_request_asks_for_them() {
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
        let mut cache = empty_embedding_cache(config);
        for chunk in full_corpus_chunks(&request, config) {
            let embedding = if chunk.text.contains("alpha") {
                vec![1.0, 0.0]
            } else {
                vec![0.5, 0.5]
            };
            cache_passage(&mut cache, chunk.text, embedding);
        }
        let mut state = SemanticIndexState::with_cache(config, cache);
        let mut embedder = FakeEmbedder::new();

        let response = state
            .refresh_or_prewarm(
                &request,
                &mut embedder,
                &SemanticCancellationToken::new(),
                |_| {},
                |_| {},
            )
            .expect("rank succeeds");

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
    fn a_passage_without_a_message_inside_the_window_is_dropped_before_ranking() {
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
                .with_timezone(&chrono::Local)
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
        let mut cache = empty_embedding_cache(config);
        for chunk in full_corpus_chunks(&request, config) {
            let embedding = if chunk.text.contains("alpha") {
                vec![1.0, 0.0]
            } else {
                vec![0.5, 0.5]
            };
            cache_passage(&mut cache, chunk.text, embedding);
        }
        let mut state = SemanticIndexState::with_cache(config, cache);

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
        cache_request_passages(&mut state.cache, &request);
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
        let mut save_calls = 0;
        let mut progress = Vec::new();

        let response = run_refresh_with_observers(
            &mut state,
            &request,
            &mut embedder,
            |status| progress.push(status),
            |_| save_calls += 1,
        )
        .expect("missing cache embeds and ranks");

        assert_eq!(response.hits[0].conversation_index, 0);
        assert_eq!(response.progress, SemanticIndexProgress::Complete);
        assert_eq!(embedder.passage_calls, 1);
        assert_eq!(embedder.query_calls, 1);
        assert_eq!(save_calls, 1);
        assert_eq!(
            cache_miss_count(
                &semantic_chunks(&request, ChunkConfig::default()),
                &state.cache
            ),
            0
        );
        assert!(progress.contains(&SemanticIndexProgress::Embedding {
            completed: 0,
            total: 1,
        }));
    }

    #[test]
    fn partial_cached_passages_embed_missing_chunks_and_rank_all() {
        let conversations = vec![
            conversation("/projects/project-a/session-a.jsonl", vec!["visible alpha"]),
            conversation("/projects/project-a/session-b.jsonl", vec!["visible beta"]),
        ];
        let (query, candidates) = request("alpha", conversations, vec![0, 1]);
        let request = index_request(&query, &candidates);
        let mut cache = empty_embedding_cache(ChunkConfig::default());
        let first_chunk = semantic_chunks(&request, ChunkConfig::default())
            .into_iter()
            .find(|chunk| chunk.text == "visible alpha")
            .expect("alpha chunk");
        cache_passage(&mut cache, first_chunk.text, vec![1.0, 0.0]);
        let mut state = SemanticIndexState::with_cache(ChunkConfig::default(), cache);
        let mut embedder = FakeEmbedder::new();
        let mut progress = Vec::new();

        let response = state
            .refresh_or_prewarm(
                &request,
                &mut embedder,
                &SemanticCancellationToken::new(),
                |status| progress.push(status),
                |_| {},
            )
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
    fn an_invalid_embedding_batch_leaves_the_index_incomplete() {
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
            cache_miss_count(
                &semantic_chunks(&request, ChunkConfig::default()),
                &state.cache
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
            cache_miss_count(
                &semantic_chunks(&request, ChunkConfig::default()),
                &state.cache
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

        run_refresh_with_observers(
            &mut state,
            &request,
            &mut embedder,
            |status| progress.push(status),
            |_| {},
        )
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

    fn three_sessions() -> Vec<Conversation> {
        vec![
            conversation("/projects/project-a/session-a.jsonl", vec!["visible alpha"]),
            conversation("/projects/project-a/session-b.jsonl", vec!["visible beta"]),
            conversation("/projects/project-a/session-c.jsonl", vec!["visible gamma"]),
        ]
    }

    /// Each cached passage's `last_used` mark, by cache key.
    fn last_used_marks(state: &SemanticIndexState) -> HashMap<String, u64> {
        state
            .cache
            .entries
            .iter()
            .map(|(key, entry)| (key.clone(), entry.last_used))
            .collect()
    }

    /// The cache keys of the passages prepared since `marks` was taken.
    /// Preparing a chunk looks up its passage in the cache and moves the
    /// passage's mark.
    fn prepared_since(marks: &HashMap<String, u64>, state: &SemanticIndexState) -> HashSet<String> {
        last_used_marks(state)
            .into_iter()
            .filter(|(key, mark)| marks.get(key) != Some(mark))
            .map(|(key, _)| key)
            .collect()
    }

    fn cache_keys(chunks: &[SemanticChunk]) -> HashSet<String> {
        chunks
            .iter()
            .map(|chunk| embedding_cache_key(&chunk.text))
            .collect()
    }

    #[test]
    fn a_reorder_doesnt_prepare_any_chunks_and_ranks_as_a_fresh_index_does() {
        let first = three_sessions();
        let first_all = candidates_from(&first);
        let first_request = index_request("alpha", &first_all);
        let (mut state, mut embedder) =
            prepare_indexed_state(&first_request, ChunkConfig::default());
        run_refresh(&mut state, &first_request, &mut embedder).expect("first corpus ranks");
        let marks = last_used_marks(&state);
        let reordered = vec![first[2].clone(), first[0].clone(), first[1].clone()];
        let reordered_all = candidates_from(&reordered);
        let reordered_request = index_request("alpha", &reordered_all);

        let response = run_refresh(&mut state, &reordered_request, &mut embedder)
            .expect("reordered corpus ranks");

        assert_eq!(prepared_since(&marks, &state), HashSet::new());
        assert_eq!(response.hits, fresh_index_hits(&reordered_request));
    }

    #[test]
    fn a_new_session_at_the_top_prepares_only_its_own_chunks() {
        let first = three_sessions()[..2].to_vec();
        let first_all = candidates_from(&first);
        let first_request = index_request("alpha", &first_all);
        let (mut state, mut embedder) =
            prepare_indexed_state(&first_request, ChunkConfig::default());
        run_refresh(&mut state, &first_request, &mut embedder).expect("first corpus ranks");
        let marks = last_used_marks(&state);
        let mut extended = vec![three_sessions()[2].clone()];
        extended.extend(first);
        let extended_all = candidates_from(&extended);
        let extended_request = index_request("alpha", &extended_all);
        let new_session_chunks = candidate_chunks(&extended_all[..1], ChunkConfig::default());

        let response = run_refresh(&mut state, &extended_request, &mut embedder)
            .expect("extended corpus ranks");

        assert_eq!(
            prepared_since(&marks, &state),
            cache_keys(&new_session_chunks)
        );
        assert_eq!(response.hits, fresh_index_hits(&extended_request));
    }

    #[test]
    fn a_rename_or_a_refresh_with_unchanged_text_doesnt_prepare_any_chunks() {
        let first = three_sessions();
        let first_all = candidates_from(&first);
        let first_request = index_request("alpha", &first_all);
        let (mut state, mut embedder) =
            prepare_indexed_state(&first_request, ChunkConfig::default());
        run_refresh(&mut state, &first_request, &mut embedder).expect("first corpus ranks");
        let marks = last_used_marks(&state);
        let mut refreshed = first.clone();
        refreshed[0].custom_title = Some("renamed".to_string());
        let refreshed_all = candidates_from(&refreshed);
        let refreshed_request = index_request("alpha", &refreshed_all);
        let mut progress = Vec::new();

        run_refresh_with_observers(
            &mut state,
            &refreshed_request,
            &mut embedder,
            |status| progress.push(status),
            |_| {},
        )
        .expect("refreshed corpus ranks");

        assert_eq!(prepared_since(&marks, &state), HashSet::new());
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
    fn a_session_cut_short_by_the_budget_is_completed_without_preparing_the_others_again() {
        let sessions = three_sessions()[..2].to_vec();
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
                |_| {},
            )
            .expect("partial index ranks");
        assert_eq!(partial.missing_chunk_count, 1);
        let marks = last_used_marks(&state);
        let cut_short_chunks = candidate_chunks(&all[1..], ChunkConfig::default());

        let response = run_refresh(&mut state, &request, &mut embedder).expect("index completes");

        assert_eq!(response.missing_chunk_count, 0);
        assert_eq!(
            prepared_since(&marks, &state),
            cache_keys(&cut_short_chunks)
        );
        assert!(state.signature_matches(&request));
        assert_eq!(response.hits, fresh_index_hits(&request));
    }

    #[test]
    fn a_session_listed_twice_ranks_at_both_positions() {
        let session = conversation("/projects/project-a/session-a.jsonl", vec!["visible alpha"]);
        let listed_twice = vec![session.clone(), session];
        let all = candidates_from(&listed_twice);
        let request = index_request("alpha", &all);
        let (mut state, mut embedder) = prepare_indexed_state(&request, ChunkConfig::default());

        let response = run_refresh(&mut state, &request, &mut embedder).expect("corpus ranks");

        assert_hit_indices(&response, &[0, 1]);
    }
}
