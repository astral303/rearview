use crate::clock::Clock;
use crate::error::Result;
use crate::history::Conversation;
use crate::history::sqlite_cache::{CachedSession, CopyKey, SessionKey};
use crate::search::ReadFailure;
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
    /// The listed sessions searched without semantic turns: their read
    /// failed, and none of an earlier copy are held.
    pub unreadable: usize,
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
                unreadable: 0,
            }));
            continue;
        }
        let LoadedCorpus {
            candidates: full_corpus,
            unreadable,
        } = worker.loaded_corpus_candidates();
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
        let mut response = match state.has_chunks(&index_request, &cancellation) {
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
        response.unreadable = unreadable;

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
    /// The semantic turns read for each session the corpus's rows list.
    semantic_turns: HashMap<SessionKey, HeldTurns>,
}

/// One session's semantic turns, as the semantic stage reads them.
#[derive(Default)]
struct HeldTurns {
    /// The newest turns read; `None` until a read of the session lands.
    turns: Option<TurnsRead>,
    /// The corpus version the last read of the row's copy failed in. Read
    /// the copy again once the corpus changes.
    failed_in: Option<u64>,
}

/// A copy's semantic turns.
struct TurnsRead {
    /// The generation of the copy read: the row's, or a newer copy's that
    /// replaced it.
    generation: i64,
    /// A row with the turns, as the semantic stage reads it. Rows of the same
    /// copy share it: the stage reads the turns, the path and the session ID
    /// alone, each the same for every row of the copy.
    loaded: Arc<Conversation>,
}

impl HeldTurns {
    /// The turns to search for a row of `copy`; `None` when they are not
    /// read, or are of an earlier copy.
    fn of_copy(&self, copy: &CopyKey) -> Option<&TurnsRead> {
        self.turns
            .as_ref()
            .filter(|turns| turns.generation >= copy.generation)
    }

    /// True when `copy`'s turns are to be read in corpus version
    /// `corpus_version`.
    fn needs_read(&self, copy: &CopyKey, corpus_version: u64) -> bool {
        self.of_copy(copy).is_none() && self.failed_in != Some(corpus_version)
    }

    /// Holds `read`, the read of a row's turns in corpus version
    /// `corpus_version`.
    fn finish_read(&mut self, read: TurnsReadResult, corpus_version: u64) {
        match read {
            Ok(turns) => {
                self.turns = Some(turns);
                self.failed_in = None;
            }
            Err(_) => self.failed_in = Some(corpus_version),
        }
    }
}

type TurnsReadResult = std::result::Result<TurnsRead, ReadFailure>;

/// Reads the semantic turns of each of `rows`, each a row and its copy, from
/// the copy the cache holds now: one result per row, in order.
fn read_turns(rows: &[(&Conversation, &CachedSession)]) -> Vec<TurnsReadResult> {
    let reads = match Conversation::load_held_semantic_turns_of(rows) {
        Ok(reads) => reads,
        Err(error) => {
            let failure = ReadFailure::of(&error);
            return rows.iter().map(|_| Err(failure.clone())).collect();
        }
    };
    reads
        .into_iter()
        .map(|read| match read {
            Ok(Some(copy)) => Ok(TurnsRead {
                generation: copy.generation,
                loaded: Arc::new(copy.row),
            }),
            Ok(None) => Err(ReadFailure::NoLongerCached),
            Err(error) => Err(ReadFailure::of(&error)),
        })
        .collect()
}

/// The corpus's rows as the semantic stage searches them.
struct LoadedCorpus {
    candidates: Vec<SemanticIndexCandidate>,
    /// The count of cached rows without semantic turns: their read failed,
    /// and the worker doesn't hold any of an earlier copy.
    unreadable: usize,
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
    /// turns. Read each session's turns once for its copy, reading the
    /// sessions not held yet together, and hold them while a row of the
    /// corpus lists the session. A row whose read fails keeps its session's
    /// earlier turns; a row without earlier turns counts as unreadable.
    fn loaded_corpus_candidates(&mut self) -> LoadedCorpus {
        let mut held = std::mem::take(&mut self.semantic_turns);
        let mut kept = HashMap::with_capacity(held.len());
        let mut sessions_to_read = Vec::new();
        let mut rows_to_read = Vec::new();
        for row in self.corpus.iter() {
            let Some(cached) = &row.cached_session else {
                continue;
            };
            let copy = cached.copy_key();
            if kept.contains_key(&copy.session) {
                continue;
            }
            let turns = held.remove(&copy.session).unwrap_or_default();
            if turns.needs_read(&copy, self.corpus_version) {
                sessions_to_read.push(copy.session.clone());
                rows_to_read.push((row.as_ref(), cached));
            }
            kept.insert(copy.session, turns);
        }
        let reads = sessions_to_read.into_iter().zip(read_turns(&rows_to_read));
        for (session, read) in reads {
            if let Some(turns) = kept.get_mut(&session) {
                turns.finish_read(read, self.corpus_version);
            }
        }
        self.semantic_turns = kept;

        let mut unreadable = 0;
        let mut candidates = self.full_corpus_candidates();
        for candidate in &mut candidates {
            let Some(cached) = &candidate.conversation.cached_session else {
                continue;
            };
            let copy = cached.copy_key();
            let turns = self.semantic_turns[&copy.session].turns.as_ref();
            match turns {
                Some(turns) => candidate.conversation = Arc::clone(&turns.loaded),
                None => unreadable += 1,
            }
        }
        LoadedCorpus {
            candidates,
            unreadable,
        }
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
        unreadable: 0,
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
        unreadable: 0,
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
        unreadable: 0,
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

    /// The longest a test waits for the worker's next message before it
    /// fails. The model's first load sends none, and beside a full test run
    /// it runs many times slower than alone.
    const WORKER_RESPONSE_TIMEOUT: Duration = Duration::from_secs(60);

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

    /// `rows`, as the corpus of version `version`.
    fn update_corpus(worker: &mut SemanticWorkerState, version: u64, rows: &[Conversation]) {
        worker.apply_command(
            SemanticWorkerCommand::UpdateCorpus {
                corpus_version: version,
                conversations: Arc::new(rows.iter().cloned().map(Arc::new).collect()),
            },
            &SemanticCancellationToken::new(),
        );
    }

    fn question(text: &str) -> serde_json::Value {
        crate::search::test_fixtures::claude_user_record(
            text,
            crate::search::test_fixtures::october(20, 9),
        )
    }

    fn turns_of(corpus: &LoadedCorpus) -> Vec<Vec<String>> {
        corpus
            .candidates
            .iter()
            .map(|candidate| candidate.conversation.semantic_turns.clone())
            .collect()
    }

    /// A copy's semantic turns are read once while a row lists its session:
    /// the next search, and a row replaced without a new copy, such as on a
    /// snapshot rebuild or a rename, search the row read for the copy.
    #[test]
    fn cached_rows_semantic_turns_are_read_once_per_copy() {
        let sessions = crate::search::test_fixtures::cached_claude_sessions(&[(
            "turns",
            vec![question("held question")],
        )]);
        let mut worker = SemanticWorkerState::default();
        update_corpus(&mut worker, 1, &sessions.rows);

        let first = worker.loaded_corpus_candidates();
        let second = worker.loaded_corpus_candidates();
        let mut renamed = sessions.rows[0].clone();
        renamed.custom_title = Some("renamed".to_owned());
        update_corpus(&mut worker, 2, &[renamed]);
        let after_rename = worker.loaded_corpus_candidates();

        assert_eq!(
            turns_of(&first),
            [["held question"]],
            "the candidate holds the copy's semantic turns"
        );
        for (search, corpus) in [("the next search", second), ("a renamed row", after_rename)] {
            assert!(
                Arc::ptr_eq(
                    &first.candidates[0].conversation,
                    &corpus.candidates[0].conversation
                ),
                "{search} searches the row read for the copy"
            );
        }
    }

    /// A row whose copy another write replaced searches the copy the cache
    /// holds; a row whose session is no longer in the session cache doesn't
    /// have any turns and counts as unreadable, and the other rows search
    /// theirs.
    #[test]
    fn replaced_copy_is_searched_and_removed_session_counts_unreadable() {
        let sessions = crate::search::test_fixtures::cached_claude_sessions(&[
            ("replaced", vec![question("first wording")]),
            ("removed", vec![question("gone question")]),
            ("kept", vec![question("kept question")]),
        ]);
        let mut worker = SemanticWorkerState::default();
        update_corpus(&mut worker, 1, &sessions.rows);
        sessions.rewrite(0, &[question("second wording")]);
        sessions.remove(1);

        let corpus = worker.loaded_corpus_candidates();

        assert_eq!(
            turns_of(&corpus),
            [
                vec!["second wording".to_owned()],
                Vec::new(),
                vec!["kept question".to_owned()]
            ],
            "the replaced row searches the copy the cache holds; the removed one, nothing"
        );
        assert_eq!(
            corpus.unreadable, 1,
            "the removed session counts unreadable"
        );
    }

    /// A row of a newer copy whose read fails keeps searching its session's
    /// earlier turns, and doesn't count as unreadable.
    #[test]
    fn failed_read_of_newer_copy_keeps_earlier_turns() {
        let sessions = crate::search::test_fixtures::cached_claude_sessions(&[
            ("changed", vec![question("first wording")]),
            ("kept", vec![question("kept question")]),
        ]);
        let mut worker = SemanticWorkerState::default();
        update_corpus(&mut worker, 1, &sessions.rows);
        let first = worker.loaded_corpus_candidates();
        sessions.rewrite(0, &[question("second wording")]);
        let newer = sessions.listed_row(0);
        sessions.remove(0);
        update_corpus(&mut worker, 2, &[newer, sessions.rows[1].clone()]);

        let corpus = worker.loaded_corpus_candidates();

        assert_eq!(
            (turns_of(&corpus), corpus.unreadable),
            (turns_of(&first), 0),
            "the failed read keeps the session's earlier turns"
        );
        assert!(
            Arc::ptr_eq(
                &first.candidates[1].conversation,
                &corpus.candidates[1].conversation
            ),
            "the other session's turns stay held"
        );
    }

    /// A failed read is not retried in its corpus version, and is retried in
    /// the next one.
    #[test]
    fn failed_read_is_retried_once_the_corpus_changes() {
        let sessions = crate::search::test_fixtures::cached_claude_sessions(&[
            ("returning", vec![question("first wording")]),
            ("kept", vec![question("kept question")]),
        ]);
        let mut worker = SemanticWorkerState::default();
        update_corpus(&mut worker, 1, &sessions.rows);
        sessions.remove(0);
        assert_eq!(
            worker.loaded_corpus_candidates().unreadable,
            1,
            "the read of the removed session fails"
        );
        sessions.rewrite(0, &[question("written again")]);

        let same_version = worker.loaded_corpus_candidates();
        update_corpus(&mut worker, 2, &sessions.rows);
        let next_version = worker.loaded_corpus_candidates();

        assert_eq!(
            (turns_of(&same_version)[0].clone(), same_version.unreadable),
            (Vec::<String>::new(), 1),
            "the failed read isn't retried in its corpus version"
        );
        assert_eq!(
            (turns_of(&next_version)[0].clone(), next_version.unreadable),
            (vec!["written again".to_owned()], 0),
            "the next corpus version reads the session again"
        );
    }

    /// A read that fails for one session of the batch fails that session
    /// alone: it keeps its earlier turns, isn't read again in its corpus
    /// version, and is read again in the next one.
    #[test]
    fn failed_read_of_one_session_in_batch_keeps_its_earlier_turns_until_next_corpus() {
        let sessions = crate::search::test_fixtures::cached_claude_sessions(&[
            ("failing", vec![question("first wording")]),
            ("read", vec![question("first question")]),
        ]);
        let mut worker = SemanticWorkerState::default();
        update_corpus(&mut worker, 1, &sessions.rows);
        let first = worker.loaded_corpus_candidates();
        sessions.rewrite(0, &[question("second wording")]);
        sessions.rewrite(1, &[question("second question")]);
        sessions.store_texts(0, &[0xff]);
        let newer = [sessions.listed_row(0), sessions.listed_row(1)];
        update_corpus(&mut worker, 2, &newer);

        let failed = worker.loaded_corpus_candidates();
        sessions.store_texts(0, b"second wording");
        let same_version = worker.loaded_corpus_candidates();
        update_corpus(&mut worker, 3, &newer);
        let next_version = worker.loaded_corpus_candidates();

        assert_eq!(
            (turns_of(&failed), failed.unreadable),
            (
                vec![
                    turns_of(&first)[0].clone(),
                    vec!["second question".to_owned()]
                ],
                0
            ),
            "the failing session keeps its earlier turns; the other reads its new ones"
        );
        assert_eq!(
            turns_of(&same_version)[0],
            turns_of(&first)[0],
            "the failed read isn't retried in its corpus version"
        );
        assert_eq!(
            turns_of(&next_version)[0],
            ["second wording"],
            "the next corpus version reads the session again"
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
