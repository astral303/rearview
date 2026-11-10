use crate::clock::Clock;
use crate::error::Result;
use crate::history::Conversation;
use crate::history::sqlite_cache::CopyKey;
use crate::search::query::ParsedQuery;
use crate::semantic::cache::write_embedding_cache;
use crate::semantic::fastembed::FastembedEmbedder;
use crate::semantic::index::{
    SemanticIndexCandidate, SemanticIndexProgress, SemanticIndexRequest, SemanticIndexResponse,
    SemanticIndexState,
};
use crate::semantic::types::{ChunkHits, SemanticCancellationToken};
use crate::tui::app::{SemanticProgress, SemanticResultMetadata};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::mpsc;

#[derive(Clone)]
pub enum SemanticWorkerCommand {
    UpdateCorpus {
        corpus_version: u64,
        conversations: Arc<Vec<Arc<Conversation>>>,
    },
    UpdateScope {
        corpus_version: u64,
        scope_version: u64,
        indices: Arc<Vec<usize>>,
    },
    Search {
        generation: u64,
        query: ParsedQuery,
        corpus_version: u64,
        scope_version: u64,
        prewarm: bool,
    },
}

#[derive(Clone)]
struct SemanticSearchRequest {
    generation: u64,
    query: ParsedQuery,
    corpus_version: u64,
    scope_version: u64,
    prewarm: bool,
}

pub enum SemanticSearchMessage {
    Progress {
        generation: u64,
        progress: SemanticProgress,
    },
    Complete(SemanticSearchResponse),
}

pub struct SemanticSearchResponse {
    pub generation: u64,
    pub filtered: Vec<usize>,
    pub metadata: HashMap<usize, SemanticResultMetadata>,
    pub error: Option<String>,
    pub progress: SemanticProgress,
    pub prewarm: bool,
}

pub fn spawn_semantic_worker(
    clock: Arc<dyn Clock>,
) -> (
    mpsc::Sender<SemanticWorkerCommand>,
    mpsc::Receiver<SemanticSearchMessage>,
    SemanticCancellationToken,
) {
    let (cmd_tx, cmd_rx) = mpsc::channel::<SemanticWorkerCommand>();
    let (res_tx, res_rx) = mpsc::channel::<SemanticSearchMessage>();
    let cancellation = SemanticCancellationToken::new();
    let worker_cancellation = cancellation.clone();

    std::thread::Builder::new()
        .name("semantic-search-worker".into())
        .spawn(move || run_semantic_worker(cmd_rx, res_tx, worker_cancellation, clock))
        .expect("failed to spawn semantic search worker thread");

    (cmd_tx, res_rx, cancellation)
}

fn run_semantic_worker(
    cmd_rx: mpsc::Receiver<SemanticWorkerCommand>,
    res_tx: mpsc::Sender<SemanticSearchMessage>,
    cancellation_source: SemanticCancellationToken,
    clock: Arc<dyn Clock>,
) {
    let mut worker = SemanticWorkerState::default();
    let mut state = SemanticIndexState::new(clock);
    let mut embedder: Option<FastembedEmbedder> = None;

    while let Ok(command) = cmd_rx.recv() {
        let mut request = worker.apply_command(command, &cancellation_source);
        while let Ok(pending) = cmd_rx.try_recv() {
            if let Some(search) = worker.apply_command(pending, &cancellation_source) {
                request = Some(search);
            }
        }

        let Some(request) = request else {
            continue;
        };
        let Some(version_state) = worker.version_state_for(&request) else {
            continue;
        };
        match version_state {
            VersionState::Future => {
                worker.pending_search = Some(request);
                continue;
            }
            VersionState::Stale => continue,
            VersionState::Current => {}
        }

        let cancellation = cancellation_source.child();
        if request.query.is_effectively_empty() && !request.prewarm {
            let _ = res_tx.send(SemanticSearchMessage::Complete(SemanticSearchResponse {
                generation: request.generation,
                filtered: Vec::new(),
                metadata: HashMap::new(),
                error: None,
                progress: SemanticProgress::Idle,
                prewarm: request.prewarm,
            }));
            continue;
        }
        let full_corpus = match worker.loaded_corpus_candidates() {
            Ok(corpus) => corpus,
            Err(error) => {
                let _ = res_tx.send(SemanticSearchMessage::Complete(failed_semantic_response(
                    request.generation,
                    request.prewarm,
                    error.to_string(),
                )));
                continue;
            }
        };
        let scope = worker
            .scope
            .iter()
            .filter_map(|index| full_corpus.get(*index).cloned())
            .collect::<Vec<_>>();
        let index_request = SemanticIndexRequest {
            query: request.query.semantic_text(),
            literal_filters: request.query.literals(),
            full_corpus: &full_corpus,
            scope: &scope,
            prewarm: request.prewarm,
            chunk_hits: ChunkHits::None,
            window: crate::time_filter::TimeFilter::default(),
        };
        let response = match state.has_chunks(&index_request, &cancellation) {
            Ok(true) => {
                if embedder.is_none() {
                    let _ = res_tx.send(SemanticSearchMessage::Progress {
                        generation: request.generation,
                        progress: SemanticProgress::InitializingModel,
                    });
                    embedder = match FastembedEmbedder::new_quiet() {
                        Ok(embedder) => Some(embedder),
                        Err(error) => {
                            let _ = res_tx.send(SemanticSearchMessage::Complete(
                                failed_semantic_response(
                                    request.generation,
                                    request.prewarm,
                                    error.to_string(),
                                ),
                            ));
                            continue;
                        }
                    };
                }
                rank_or_prewarm_semantic_request(
                    request.generation,
                    &index_request,
                    &request.query,
                    &mut state,
                    embedder.as_mut().unwrap(),
                    &cancellation,
                    &res_tx,
                )
                .unwrap_or_else(|error| {
                    failed_semantic_response(request.generation, request.prewarm, error.to_string())
                })
            }
            Ok(false) => match state.clear_empty(&index_request, &cancellation) {
                Ok(()) => empty_semantic_response(request.generation, request.prewarm),
                Err(error) => {
                    failed_semantic_response(request.generation, request.prewarm, error.to_string())
                }
            },
            Err(error) => {
                failed_semantic_response(request.generation, request.prewarm, error.to_string())
            }
        };

        let _ = res_tx.send(SemanticSearchMessage::Complete(response));
    }
}

#[derive(Default)]
struct SemanticWorkerState {
    corpus_version: u64,
    scope_version: u64,
    scope_corpus_version: u64,
    corpus: Arc<Vec<Arc<Conversation>>>,
    scope: Arc<Vec<usize>>,
    pending_search: Option<SemanticSearchRequest>,
    /// The semantic turns of the copies the corpus's rows read, by copy.
    semantic_turns: HashMap<CopyKey, HeldTurns>,
}

/// A row with the semantic turns read from its copy, and the row it was
/// built from.
struct HeldTurns {
    row: Arc<Conversation>,
    loaded: Arc<Conversation>,
}

impl HeldTurns {
    /// Reads `row`'s semantic turns from its copy.
    fn read(row: &Arc<Conversation>) -> Result<Self> {
        Ok(Self {
            row: row.clone(),
            loaded: Arc::new(row.load_semantic_turns()?.into_owned()),
        })
    }

    /// These turns on `row`, a row reading the same copy. A refresh can
    /// replace a row without writing its session, such as for a name kept
    /// beside the transcript.
    fn for_row(self, row: &Arc<Conversation>) -> Self {
        if Arc::ptr_eq(&self.row, row) {
            return self;
        }
        let mut loaded = row.as_ref().clone();
        loaded.semantic_turns = self.loaded.semantic_turns.clone();
        loaded.semantic_turn_ranges = self.loaded.semantic_turn_ranges.clone();
        loaded.cached_session = None;
        Self {
            row: row.clone(),
            loaded: Arc::new(loaded),
        }
    }
}

enum VersionState {
    Current,
    Future,
    Stale,
}

impl SemanticWorkerState {
    fn apply_command(
        &mut self,
        command: SemanticWorkerCommand,
        cancellation: &SemanticCancellationToken,
    ) -> Option<SemanticSearchRequest> {
        match command {
            SemanticWorkerCommand::UpdateCorpus {
                corpus_version,
                conversations,
            } => {
                if corpus_version >= self.corpus_version {
                    self.corpus_version = corpus_version;
                    self.corpus = conversations;
                    cancellation.cancel();
                }
                self.take_ready_pending()
            }
            SemanticWorkerCommand::UpdateScope {
                corpus_version,
                scope_version,
                indices,
            } => {
                if scope_version >= self.scope_version {
                    self.scope_corpus_version = corpus_version;
                    self.scope_version = scope_version;
                    self.scope = indices;
                    cancellation.cancel();
                }
                self.take_ready_pending()
            }
            SemanticWorkerCommand::Search {
                generation,
                query,
                corpus_version,
                scope_version,
                prewarm,
            } => {
                cancellation.cancel();
                Some(SemanticSearchRequest {
                    generation,
                    query,
                    corpus_version,
                    scope_version,
                    prewarm,
                })
            }
        }
    }

    fn take_ready_pending(&mut self) -> Option<SemanticSearchRequest> {
        let request = self.pending_search.take()?;
        match self.version_state_for(&request) {
            Some(VersionState::Current) | Some(VersionState::Stale) => Some(request),
            Some(VersionState::Future) | None => {
                self.pending_search = Some(request);
                None
            }
        }
    }

    fn version_state_for(&self, request: &SemanticSearchRequest) -> Option<VersionState> {
        if self.scope_corpus_version != self.corpus_version {
            return Some(VersionState::Future);
        }
        if self.corpus_version == request.corpus_version
            && self.scope_version == request.scope_version
        {
            Some(VersionState::Current)
        } else if self.corpus_version < request.corpus_version
            || self.scope_version < request.scope_version
        {
            Some(VersionState::Future)
        } else {
            Some(VersionState::Stale)
        }
    }

    fn full_corpus_candidates(&self) -> Vec<SemanticIndexCandidate> {
        self.corpus
            .iter()
            .enumerate()
            .map(|(index, conversation)| SemanticIndexCandidate {
                index,
                source: crate::semantic::types::SemanticChunkSource::VisibleDialogue,
                conversation: conversation.clone(),
            })
            .collect()
    }

    /// The corpus's rows as candidates, each cached row with its semantic
    /// turns. A copy's turns are read once, and held while a row of the
    /// corpus reads that copy.
    fn loaded_corpus_candidates(&mut self) -> Result<Vec<SemanticIndexCandidate>> {
        let mut held = std::mem::take(&mut self.semantic_turns);
        let mut kept = HashMap::with_capacity(held.len());
        let candidates = self
            .full_corpus_candidates()
            .into_iter()
            .map(|mut candidate| {
                if let Some(cached) = &candidate.conversation.cached_session {
                    let key = cached.copy_key();
                    let turns = match held.remove(&key).or_else(|| kept.remove(&key)) {
                        Some(turns) => turns.for_row(&candidate.conversation),
                        None => HeldTurns::read(&candidate.conversation)?,
                    };
                    candidate.conversation = turns.loaded.clone();
                    kept.insert(key, turns);
                }
                Ok(candidate)
            })
            .collect();
        self.semantic_turns = kept;
        candidates
    }

    #[cfg(test)]
    fn semantic_candidates(&self) -> Vec<SemanticIndexCandidate> {
        self.scope
            .iter()
            .filter_map(|&index| {
                self.corpus
                    .get(index)
                    .map(|conversation| SemanticIndexCandidate {
                        index,
                        source: crate::semantic::types::SemanticChunkSource::VisibleDialogue,
                        conversation: conversation.clone(),
                    })
            })
            .collect()
    }
}

fn rank_or_prewarm_semantic_request(
    generation: u64,
    request: &SemanticIndexRequest<'_>,
    parsed: &ParsedQuery,
    state: &mut SemanticIndexState,
    embedder: &mut FastembedEmbedder,
    cancellation: &SemanticCancellationToken,
    res_tx: &mpsc::Sender<SemanticSearchMessage>,
) -> Result<SemanticSearchResponse> {
    let clock = state.clock();
    let response = state.refresh_or_prewarm(
        request,
        embedder,
        cancellation,
        |progress| {
            let _ = res_tx.send(SemanticSearchMessage::Progress {
                generation,
                progress: semantic_progress(progress),
            });
        },
        |cache| write_embedding_cache(cache, clock.as_ref()),
    )?;
    Ok(semantic_search_response(
        generation,
        response,
        request.full_corpus,
        parsed,
    ))
}

fn semantic_search_response(
    generation: u64,
    response: SemanticIndexResponse,
    _conversations: &[SemanticIndexCandidate],
    _parsed: &ParsedQuery,
) -> SemanticSearchResponse {
    let filtered = response
        .hits
        .iter()
        .map(|hit| hit.conversation_index)
        .collect::<Vec<_>>();
    let metadata = response
        .hits
        .into_iter()
        .map(|hit| {
            let explanation = hit.explanation;
            (
                hit.conversation_index,
                SemanticResultMetadata {
                    score_breakdown: hit.score_breakdown,
                    explanation,
                },
            )
        })
        .collect();

    SemanticSearchResponse {
        generation,
        filtered,
        metadata,
        error: None,
        progress: semantic_progress(response.progress),
        prewarm: response.prewarm,
    }
}

fn empty_semantic_response(generation: u64, prewarm: bool) -> SemanticSearchResponse {
    SemanticSearchResponse {
        generation,
        filtered: Vec::new(),
        metadata: HashMap::new(),
        error: None,
        progress: SemanticProgress::EmptyCorpus,
        prewarm,
    }
}

fn failed_semantic_response(
    generation: u64,
    prewarm: bool,
    error: String,
) -> SemanticSearchResponse {
    SemanticSearchResponse {
        generation,
        filtered: Vec::new(),
        metadata: HashMap::new(),
        error: Some(error),
        progress: SemanticProgress::Failed,
        prewarm,
    }
}

fn semantic_progress(progress: SemanticIndexProgress) -> SemanticProgress {
    match progress {
        SemanticIndexProgress::Embedding { completed, total } => {
            SemanticProgress::Embedding { completed, total }
        }
        SemanticIndexProgress::CacheReady => SemanticProgress::CacheReady,
        SemanticIndexProgress::Ranking => SemanticProgress::Ranking,
        SemanticIndexProgress::Complete => SemanticProgress::Complete,
        SemanticIndexProgress::EmptyCorpus => SemanticProgress::EmptyCorpus,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::Conversation;
    use crate::semantic::test_fixtures::{SemanticConversationFixture, beta_hit_metadata};
    use std::time::Duration;

    const WORKER_RESPONSE_TIMEOUT: Duration = Duration::from_secs(10);

    fn conversation(path: &str, semantic_turns: Vec<&str>) -> Conversation {
        SemanticConversationFixture::new(path, semantic_turns)
            .with_normalized_search_text_lower()
            .build()
    }

    fn corpus(conversations: Vec<Conversation>) -> Arc<Vec<Arc<Conversation>>> {
        Arc::new(conversations.into_iter().map(Arc::new).collect())
    }

    fn send_worker_setup(
        tx: &mpsc::Sender<SemanticWorkerCommand>,
        semantic_turns: Vec<&str>,
        query: &str,
        prewarm: bool,
    ) {
        tx.send(SemanticWorkerCommand::UpdateCorpus {
            corpus_version: 1,
            conversations: corpus(vec![conversation(
                "/projects/project-a/session-a.jsonl",
                semantic_turns,
            )]),
        })
        .expect("send corpus");
        tx.send(SemanticWorkerCommand::UpdateScope {
            corpus_version: 1,
            scope_version: 1,
            indices: Arc::new(vec![0]),
        })
        .expect("send scope");
        tx.send(SemanticWorkerCommand::Search {
            generation: 1,
            query: ParsedQuery::parse(query),
            corpus_version: 1,
            scope_version: 1,
            prewarm,
        })
        .expect("send semantic request");
    }

    fn recv_empty_complete(rx: &mpsc::Receiver<SemanticSearchMessage>) -> SemanticSearchResponse {
        let message = rx
            .recv_timeout(WORKER_RESPONSE_TIMEOUT)
            .expect("empty response");
        match message {
            SemanticSearchMessage::Complete(response) => {
                assert!(response.filtered.is_empty());
                assert!(response.metadata.is_empty());
                assert_eq!(response.error, None);
                response
            }
            SemanticSearchMessage::Progress { progress, .. } => {
                panic!("unexpected progress before empty response: {progress:?}");
            }
        }
    }

    #[test]
    fn maps_domain_hits_to_original_indices_and_metadata() {
        let (expected_score_breakdown, expected_explanation) = beta_hit_metadata(42, "session-b");
        let response = semantic_search_response(
            7,
            SemanticIndexResponse {
                hits: vec![crate::semantic::types::SemanticHit::new(
                    expected_score_breakdown,
                    expected_explanation.clone(),
                )],
                chunk_hits: Vec::new(),
                indexed_chunk_count: 1,
                missing_chunk_count: 0,
                query_embedding_returned: true,
                progress: SemanticIndexProgress::Complete,
                prewarm: false,
            },
            &[],
            &ParsedQuery::parse("beta"),
        );

        assert_eq!(response.generation, 7);
        assert_eq!(response.filtered, vec![42]);
        assert_eq!(response.progress, SemanticProgress::Complete);
        let metadata = &response.metadata[&42];
        assert_eq!(metadata.score_breakdown, expected_score_breakdown);
        assert_eq!(metadata.explanation, expected_explanation);
    }

    #[test]
    fn empty_visible_dialogue_returns_before_embedder_initialization() {
        let (tx, rx, _cancellation) =
            spawn_semantic_worker(Arc::new(crate::clock::TestClock::default()));
        send_worker_setup(&tx, vec![], "alpha", false);
        let response = recv_empty_complete(&rx);
        assert_eq!(response.progress, SemanticProgress::EmptyCorpus);
    }

    #[test]
    fn empty_quoted_search_returns_idle_without_embedding() {
        let (tx, rx, _cancellation) =
            spawn_semantic_worker(Arc::new(crate::clock::TestClock::default()));
        send_worker_setup(&tx, vec!["visible dialogue"], "\"\"", false);
        let response = recv_empty_complete(&rx);
        assert_eq!(response.progress, SemanticProgress::Idle);
    }

    #[test]
    fn empty_prewarm_search_builds_cache_without_idle_short_circuit() {
        let (tx, rx, _cancellation) =
            spawn_semantic_worker(Arc::new(crate::clock::TestClock::default()));
        send_worker_setup(&tx, vec!["visible dialogue"], "\"\"", true);

        loop {
            match rx
                .recv_timeout(WORKER_RESPONSE_TIMEOUT)
                .expect("prewarm response")
            {
                SemanticSearchMessage::Progress { .. } => continue,
                SemanticSearchMessage::Complete(response) => {
                    assert!(response.prewarm);
                    assert!(response.filtered.is_empty());
                    assert!(response.metadata.is_empty());
                    assert_eq!(response.error, None);
                    assert_ne!(response.progress, SemanticProgress::Idle);
                    break;
                }
            }
        }
    }

    #[test]
    fn worker_builds_distinct_full_corpus_and_scoped_candidates() {
        let mut worker = SemanticWorkerState::default();
        let cancellation = SemanticCancellationToken::new();
        worker.apply_command(
            SemanticWorkerCommand::UpdateCorpus {
                corpus_version: 1,
                conversations: corpus(vec![
                    conversation("/projects/project-a/session-a.jsonl", vec!["hidden"]),
                    conversation("/projects/project-a/session-b.jsonl", vec!["visible"]),
                ]),
            },
            &cancellation,
        );
        worker.apply_command(
            SemanticWorkerCommand::UpdateScope {
                corpus_version: 1,
                scope_version: 1,
                indices: Arc::new(vec![1]),
            },
            &cancellation,
        );

        let full_corpus = worker.full_corpus_candidates();
        let candidates = worker.semantic_candidates();

        assert_eq!(full_corpus.len(), 2);
        assert_eq!(full_corpus[0].index, 0);
        assert_eq!(full_corpus[1].index, 1);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].index, 1);
        assert_eq!(candidates[0].conversation.semantic_turns, vec!["visible"]);
    }

    /// A copy's semantic turns are read once while a row reads it: the next
    /// search reuses the row built from them, and a row replaced without a
    /// new copy keeps the turns with its own fields.
    #[test]
    fn cached_rows_semantic_turns_are_read_once_per_copy() {
        let time = crate::search::test_fixtures::october(20, 9);
        let sessions = crate::search::test_fixtures::cached_claude_sessions(&[(
            "turns",
            vec![crate::search::test_fixtures::claude_user_record(
                "held question",
                time,
            )],
        )]);
        let row = Arc::new(sessions.rows[0].clone());
        let mut worker = SemanticWorkerState::default();
        let cancellation = SemanticCancellationToken::new();
        let update = |worker: &mut SemanticWorkerState, version, row: &Arc<Conversation>| {
            worker.apply_command(
                SemanticWorkerCommand::UpdateCorpus {
                    corpus_version: version,
                    conversations: Arc::new(vec![row.clone()]),
                },
                &cancellation,
            );
        };
        update(&mut worker, 1, &row);

        let first = worker.loaded_corpus_candidates().unwrap();
        let second = worker.loaded_corpus_candidates().unwrap();
        let mut renamed = row.as_ref().clone();
        renamed.custom_title = Some("renamed".to_owned());
        update(&mut worker, 2, &Arc::new(renamed));
        let after_rename = worker.loaded_corpus_candidates().unwrap();

        assert_eq!(
            first[0].conversation.semantic_turns,
            vec!["held question"],
            "the candidate holds the copy's semantic turns"
        );
        assert!(
            Arc::ptr_eq(&first[0].conversation, &second[0].conversation),
            "the next search reuses the row read for the copy"
        );
        assert_eq!(
            (
                after_rename[0].conversation.custom_title.as_deref(),
                &after_rename[0].conversation.semantic_turns
            ),
            (Some("renamed"), &first[0].conversation.semantic_turns),
            "a replaced row keeps its copy's turns with its own fields"
        );
    }

    #[test]
    fn stale_search_is_not_current_after_scope_advances() {
        let mut worker = SemanticWorkerState::default();
        let cancellation = SemanticCancellationToken::new();
        worker.apply_command(
            SemanticWorkerCommand::UpdateCorpus {
                corpus_version: 1,
                conversations: corpus(vec![conversation(
                    "/projects/project-a/session-a.jsonl",
                    vec!["visible"],
                )]),
            },
            &cancellation,
        );
        worker.apply_command(
            SemanticWorkerCommand::UpdateScope {
                corpus_version: 1,
                scope_version: 2,
                indices: Arc::new(vec![0]),
            },
            &cancellation,
        );
        let request = SemanticSearchRequest {
            generation: 1,
            query: ParsedQuery::parse("visible"),
            corpus_version: 1,
            scope_version: 1,
            prewarm: false,
        };

        assert!(matches!(
            worker.version_state_for(&request),
            Some(VersionState::Stale)
        ));
    }
}
