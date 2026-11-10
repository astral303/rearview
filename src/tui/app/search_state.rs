use super::exact_literal::exact_literal_results;
use super::{App, ListSearchMode, SemanticProgress, SemanticResultMetadata};
use crate::clock::Clock;
use crate::history::Conversation;
use crate::history::listing::SessionOrigin;
use crate::search::query::ParsedQuery;
use crate::search::{
    self, CopiesToRead, SearchTextsInMemory, SearchableConversation, SessionsWithoutTexts,
    TextsRead,
};
use crate::semantic::types::SemanticCancellationToken;
use crate::tui::semantic_worker::{
    SemanticSearchMessage, SemanticWorkerCommand, spawn_semantic_worker,
};
use chrono::{DateTime, Local};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc;
use std::time::Duration;

#[allow(dead_code)]
#[derive(Default)]
pub(super) struct SemanticSearchState {
    pub(super) available: bool,
    pub(super) pending_generation: Option<u64>,
    pub(super) pending_status: Option<SemanticProgress>,
    pub(super) prewarm_generation: Option<u64>,
    pub(super) prewarm_status: Option<SemanticProgress>,
    pub(super) last_status: SemanticProgress,
    pub(super) error: Option<String>,
    pub(super) results: HashMap<usize, SemanticResultMetadata>,
    pub(super) worker_tx: Option<mpsc::Sender<SemanticWorkerCommand>>,
    pub(super) worker_rx: Option<mpsc::Receiver<SemanticSearchMessage>>,
    pub(super) cancellation: Option<SemanticCancellationToken>,
}

/// What a query that could name a session turned out to be.
enum SessionLookup {
    Listed(usize),
    /// A session the cache lacks, at this path, until the list applies its
    /// refresh.
    Loading(PathBuf),
    Unresolved,
    NotAnId,
}

/// The query as the session id it named, once the lookup accepted it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum SessionIdQuery {
    Listed(String),
    /// The query named the session at `path`. Its row joins the list when
    /// the list applies its refresh.
    Loading {
        query: String,
        path: PathBuf,
    },
    Unresolved(String),
}

pub(super) enum SearchCommand {
    UpdateData {
        conversations: Arc<Vec<Arc<Conversation>>>,
        searchable: Arc<Vec<SearchableConversation>>,
    },
    Search {
        query: String,
        generation: u64,
        mode: ListSearchMode,
    },
}

pub(super) struct SearchResponse {
    pub(super) filtered: Vec<usize>,
    pub(super) generation: u64,
    pub(super) mode: ListSearchMode,
    pub(super) evidence: HashMap<usize, search::LexicalEvidence>,
    /// For a semantic query of quoted phrases alone, the semantic search's
    /// results: `filtered`, each an exact match. `None` for a lexical search
    /// and for the semantic search's lexical fallback.
    pub(super) exact_literal_results: Option<HashMap<usize, SemanticResultMetadata>>,
}

/// A search the worker runs once it has applied every command waiting.
#[derive(Clone)]
struct SearchRequest {
    query: String,
    generation: u64,
    mode: ListSearchMode,
}

/// What the search worker searches: the rows, their search data, and the
/// search texts read into memory so far.
struct SearchWorkerState {
    conversations: Arc<Vec<Arc<Conversation>>>,
    searchable: Arc<Vec<SearchableConversation>>,
    texts: SearchTextsInMemory,
    copies_to_read: mpsc::Sender<CopiesToRead>,
    texts_read: mpsc::Receiver<Vec<TextsRead>>,
    /// The batches of copies sent to the reader whose texts have not come
    /// back.
    batches_reading: usize,
    /// The search last run, run again when texts arrive.
    last_search: Option<SearchRequest>,
    /// The `UpdateData` commands applied.
    updates_applied: u64,
}

/// The sessions without texts held, as the worker counts them once it has
/// applied `updates_applied` `UpdateData` commands.
#[derive(Clone, Copy)]
pub(super) struct TextsHeld {
    pub(super) updates_applied: u64,
    pub(super) without: SessionsWithoutTexts,
}

/// How often the worker looks for texts read while a batch is being read.
const TEXTS_READ_POLL: Duration = Duration::from_millis(50);

/// What applying the commands and texts waiting asks the worker to do.
#[derive(Default)]
struct Applied {
    search: Option<SearchRequest>,
    /// True when the count of sessions without texts may have changed.
    texts_changed: bool,
}

impl SearchWorkerState {
    /// The next command, waiting for it; `None` once the app is gone. While
    /// a batch is being read, the wait ends every [`TEXTS_READ_POLL`] with
    /// `Some(None)`, so the texts read are held without a command.
    fn next_command(
        &self,
        commands: &mpsc::Receiver<SearchCommand>,
    ) -> Option<Option<SearchCommand>> {
        if self.batches_reading == 0 {
            return commands.recv().ok().map(Some);
        }
        match commands.recv_timeout(TEXTS_READ_POLL) {
            Ok(command) => Some(Some(command)),
            Err(mpsc::RecvTimeoutError::Timeout) => Some(None),
            Err(mpsc::RecvTimeoutError::Disconnected) => None,
        }
    }

    /// Applies `command` to `applied`.
    fn apply(&mut self, command: SearchCommand, applied: &mut Applied) {
        match command {
            SearchCommand::UpdateData {
                conversations,
                searchable,
            } => {
                let to_read = self.texts.keep_copies_of(&conversations);
                if !to_read.copies.is_empty() && self.copies_to_read.send(to_read).is_ok() {
                    self.batches_reading += 1;
                }
                self.conversations = conversations;
                self.searchable = searchable;
                self.updates_applied += 1;
                applied.texts_changed = true;
            }
            SearchCommand::Search {
                query,
                generation,
                mode,
            } => {
                applied.search = Some(SearchRequest {
                    query,
                    generation,
                    mode,
                });
            }
        }
    }

    /// Holds the texts read since the last call; the last search runs again
    /// over them unless a newer one waits.
    fn hold_texts_read(&mut self, applied: &mut Applied) {
        while let Ok(reads) = self.texts_read.try_recv() {
            self.batches_reading -= 1;
            for (copy, texts) in reads {
                self.texts.finish_read(copy, texts);
            }
            applied.texts_changed = true;
            if applied.search.is_none() {
                applied.search = self.last_search.clone();
            }
        }
    }

    fn search(&mut self, request: SearchRequest, now: DateTime<Local>) -> SearchResponse {
        let conversations = self.conversations.as_slice();
        let filtered = search::search_with_texts(
            conversations,
            &self.searchable,
            &self.texts,
            &request.query,
            now,
        );
        let parsed = ParsedQuery::parse(&request.query);
        let evidence =
            search::build_session_list_evidence(conversations, &self.texts, &filtered, &parsed);
        let exact_literal_results = (request.mode == ListSearchMode::Semantic
            && parsed.is_quoted_only())
        .then(|| exact_literal_results(conversations, &self.texts, &filtered, &parsed));
        let response = SearchResponse {
            filtered,
            generation: request.generation,
            mode: request.mode,
            evidence,
            exact_literal_results,
        };
        self.last_search = Some(request);
        response
    }
}

/// Start the search worker, ranking freshness against `clock`'s time. The
/// rows' search texts are read into memory on a reader thread of its own;
/// a session's search reads its title, summary and project name alone until
/// its texts arrive, and the last search runs again when they do.
pub(super) fn spawn_search_worker(clock: Arc<dyn Clock>) -> SearchWorker {
    let (cmd_tx, cmd_rx) = mpsc::channel::<SearchCommand>();
    let (res_tx, res_rx) = mpsc::channel::<SearchResponse>();
    let (texts_held_tx, texts_held_rx) = mpsc::channel::<TextsHeld>();

    std::thread::Builder::new()
        .name("search-worker".into())
        .spawn(move || {
            let (copies_to_read, texts_read) = spawn_text_reader();
            let mut state = SearchWorkerState {
                conversations: Arc::new(Vec::new()),
                searchable: Arc::new(Vec::new()),
                texts: SearchTextsInMemory::default(),
                copies_to_read,
                texts_read,
                batches_reading: 0,
                last_search: None,
                updates_applied: 0,
            };
            while let Some(command) = state.next_command(&cmd_rx) {
                let mut applied = Applied::default();
                if let Some(command) = command {
                    state.apply(command, &mut applied);
                }
                while let Ok(pending) = cmd_rx.try_recv() {
                    state.apply(pending, &mut applied);
                }
                state.hold_texts_read(&mut applied);
                // The results go first: once the app counts no session
                // loading, the results over every session's texts are on
                // their channel.
                if let Some(request) = applied.search {
                    let now = clock.wall().with_timezone(&Local);
                    if res_tx.send(state.search(request, now)).is_err() {
                        return;
                    }
                }
                let held = TextsHeld {
                    updates_applied: state.updates_applied,
                    without: state.texts.sessions_without_texts(),
                };
                if applied.texts_changed && texts_held_tx.send(held).is_err() {
                    return;
                }
            }
        })
        .expect("failed to spawn search worker thread");

    SearchWorker {
        commands: cmd_tx,
        results: res_rx,
        texts_held: texts_held_rx,
    }
}

/// The app's ends of the search worker's channels.
pub(super) struct SearchWorker {
    pub(super) commands: mpsc::Sender<SearchCommand>,
    pub(super) results: mpsc::Receiver<SearchResponse>,
    /// The sessions without texts held, whenever the count may have changed.
    pub(super) texts_held: mpsc::Receiver<TextsHeld>,
}

/// Start the thread reading the search texts of each batch of copies sent
/// to it, in parallel, and sending back each batch's reads together. It
/// stops once the search worker is gone.
fn spawn_text_reader() -> (mpsc::Sender<CopiesToRead>, mpsc::Receiver<Vec<TextsRead>>) {
    let (copies_tx, copies_rx) = mpsc::channel::<CopiesToRead>();
    let (texts_tx, texts_rx) = mpsc::channel::<Vec<TextsRead>>();
    std::thread::Builder::new()
        .name("search-text-reader".into())
        .spawn(move || {
            while let Ok(copies) = copies_rx.recv() {
                if texts_tx.send(copies.read()).is_err() {
                    return;
                }
            }
        })
        .expect("failed to spawn search text reader thread");
    (copies_tx, texts_rx)
}

impl App {
    pub(super) fn invalidate_search_generation(&mut self) {
        self.search_generation += 1;
        self.search_started_at = None;
        self.dispatched_query = None;
        self.shown_results_settled = false;
        self.lexical_evidence.clear();
        self.semantic_search.pending_generation = None;
        self.semantic_search.pending_status = None;
        self.semantic_search.prewarm_generation = None;
        self.semantic_search.prewarm_status = None;
        self.semantic_search.last_status = SemanticProgress::Idle;
        self.semantic_search.error = None;
        self.semantic_search.results.clear();
    }

    fn ensure_semantic_worker(&mut self) {
        if self.semantic_search.worker_tx.is_none() || self.semantic_search.worker_rx.is_none() {
            let (tx, rx, cancellation) = spawn_semantic_worker(self.environment.clock().clone());
            self.semantic_search.worker_tx = Some(tx);
            self.semantic_search.worker_rx = Some(rx);
            self.semantic_search.cancellation = Some(cancellation);
            self.semantic_sent_corpus_version = 0;
            self.semantic_sent_scope_signature = None;
        }
    }

    /// Copy every row into a new snapshot for the workers.
    pub(super) fn rebuild_conversations_snapshot(&mut self) {
        self.replace_conversations_snapshot(Self::conversation_snapshot(&self.conversations));
    }

    /// Hand `snapshot`, the rows in list order, to the workers: the semantic
    /// worker sees a new corpus version, and the search worker gets it with
    /// the search data. Also recount the loaded sessions missing earlier
    /// turns, because every change to the rows reaches this function.
    pub(super) fn replace_conversations_snapshot(&mut self, snapshot: Arc<Vec<Arc<Conversation>>>) {
        self.conversations_snapshot = snapshot;
        self.semantic_corpus_version += 1;
        self.semantic_sent_scope_signature = None;
        self.recount_missing_earlier_turns();
    }

    /// Send the snapshot and the search data to the search worker.
    pub(super) fn send_search_data(&mut self) {
        let sent = self.search_tx.send(SearchCommand::UpdateData {
            conversations: self.conversations_snapshot.clone(),
            searchable: Arc::new(self.searchable.clone()),
        });
        if sent.is_ok() {
            self.search_data_sent += 1;
        }
    }

    /// True until the search worker has counted the sessions of the rows
    /// last sent, and while some of those sessions' first read runs: the
    /// list's search results can still change without a keystroke.
    pub fn is_reading_search_texts(&self) -> bool {
        self.texts_held.updates_applied < self.search_data_sent
            || self.texts_held.without.loading > 0
    }

    pub(super) fn semantic_scope_indices(&self) -> Arc<Vec<usize>> {
        Arc::new(self.filter_indices(0..self.conversations.len()))
    }

    fn reset_semantic_worker(&mut self) {
        let (tx, rx, cancellation) = spawn_semantic_worker(self.environment.clock().clone());
        self.semantic_search.worker_tx = Some(tx);
        self.semantic_search.worker_rx = Some(rx);
        self.semantic_search.cancellation = Some(cancellation);
        self.semantic_sent_corpus_version = 0;
        self.semantic_sent_scope_signature = None;
    }

    fn send_semantic_command(&mut self, command: SemanticWorkerCommand) -> bool {
        self.ensure_semantic_worker();
        self.semantic_search
            .worker_tx
            .as_ref()
            .is_some_and(|tx| tx.send(command).is_ok())
    }

    fn send_semantic_state(&mut self) -> Option<(u64, u64)> {
        if self.semantic_sent_corpus_version != self.semantic_corpus_version {
            if !self.send_semantic_command(SemanticWorkerCommand::UpdateCorpus {
                corpus_version: self.semantic_corpus_version,
                conversations: self.conversations_snapshot.clone(),
            }) {
                return None;
            }
            self.semantic_sent_corpus_version = self.semantic_corpus_version;
            self.semantic_sent_scope_signature = None;
        }

        let scope = self.semantic_scope_indices();
        let current_signature = (self.semantic_corpus_version, scope.clone());
        let scope_changed =
            self.semantic_sent_scope_signature
                .as_ref()
                .is_none_or(|(corpus_version, previous)| {
                    *corpus_version != self.semantic_corpus_version
                        || previous.as_ref() != scope.as_ref()
                });
        if scope_changed {
            self.semantic_scope_version += 1;
            if !self.send_semantic_command(SemanticWorkerCommand::UpdateScope {
                corpus_version: self.semantic_corpus_version,
                scope_version: self.semantic_scope_version,
                indices: scope,
            }) {
                return None;
            }
            self.semantic_sent_scope_signature = Some(current_signature);
        }

        Some((self.semantic_corpus_version, self.semantic_scope_version))
    }

    pub(super) fn dispatch_search(&mut self) {
        let query = self.query.trim().to_string();

        if ParsedQuery::parse(&query).is_effectively_empty() {
            let prewarm_generation = self.semantic_search.prewarm_generation;
            let prewarm_status = self.semantic_search.prewarm_status.clone();
            self.invalidate_search_generation();
            if self.list_search_mode == ListSearchMode::Semantic {
                self.semantic_search.prewarm_generation = prewarm_generation;
                self.semantic_search.prewarm_status = prewarm_status;
            }
            self.semantic_search.error = None;
            self.semantic_search.results.clear();
            self.update_filter();
            return;
        }

        if self.apply_session_lookup(&query) {
            self.invalidate_search_generation();
            self.semantic_search.error = None;
            return;
        }

        if self.dispatched_query.as_deref() == Some(query.as_str()) {
            return;
        }

        // Those rows are the answer, so the search in flight is abandoned
        // rather than paid for again.
        if self.shown_results_settled
            && self.shown_results_query.as_deref() == Some(query.as_str())
            && self.dispatched_query.is_some()
        {
            self.abandon_search_in_flight();
            self.dispatched_query = Some(query);
            return;
        }

        if self.list_search_mode == ListSearchMode::Semantic {
            self.dispatch_semantic_search(query, false);
            return;
        }

        self.dispatch_lexical_search(query);
    }

    /// Search for `query` on the search worker. The worker reads the rows'
    /// text from the session cache, so the drawing thread does not wait
    /// behind another rearview's rebuild.
    fn dispatch_lexical_search(&mut self, query: String) {
        self.dispatched_query = Some(query.clone());
        self.semantic_search.results.clear();
        self.search_generation += 1;
        self.search_started_at = Some(self.clock().now());
        self.semantic_search.error = None;
        let _ = self.search_tx.send(SearchCommand::Search {
            query,
            generation: self.search_generation,
            mode: self.list_search_mode,
        });
    }

    /// Abandons the search in flight and keeps the results on screen: its late
    /// response fails the generation check, and the semantic worker's
    /// search is cancelled.
    fn abandon_search_in_flight(&mut self) {
        self.search_generation += 1;
        self.search_started_at = None;
        self.semantic_search.pending_generation = None;
        self.semantic_search.pending_status = None;
        self.semantic_search.error = None;
        if let Some(cancellation) = &self.semantic_search.cancellation {
            cancellation.cancel();
        }
    }

    fn dispatch_semantic_search(&mut self, query: String, prewarm: bool) {
        self.search_generation += 1;
        self.search_started_at = (!prewarm).then(|| self.clock().now());
        if !prewarm {
            self.dispatched_query = Some(query.clone());
        }
        self.semantic_search.pending_generation = Some(self.search_generation);
        // Embedding activity remains visible while a replacement query waits for the worker.
        if !matches!(
            self.semantic_search.pending_status,
            Some(SemanticProgress::InitializingModel | SemanticProgress::Embedding { .. })
        ) {
            self.semantic_search.pending_status = None;
        }
        if prewarm {
            self.semantic_search.prewarm_generation = Some(self.search_generation);
            self.semantic_search.prewarm_status = None;
        }
        self.semantic_search.last_status = SemanticProgress::Idle;
        self.semantic_search.error = None;
        if prewarm {
            self.semantic_search.results.clear();
        }
        if let Some(cancellation) = &self.semantic_search.cancellation {
            cancellation.cancel();
        }
        let generation = self.search_generation;
        if !prewarm && ParsedQuery::parse(&query).is_quoted_only() {
            // The search worker answers it over the texts it holds.
            let _ = self.search_tx.send(SearchCommand::Search {
                query,
                generation,
                mode: ListSearchMode::Semantic,
            });
            return;
        }
        let (corpus_version, scope_version) = match self.send_semantic_state() {
            Some(versions) => versions,
            None => {
                self.reset_semantic_worker();
                let Some(versions) = self.send_semantic_state() else {
                    self.semantic_search.error = Some("semantic worker unavailable".to_string());
                    self.semantic_search.pending_status = Some(SemanticProgress::Failed);
                    return;
                };
                versions
            }
        };
        if !prewarm {
            let _ = self.search_tx.send(SearchCommand::Search {
                query: query.clone(),
                generation,
                mode: ListSearchMode::Semantic,
            });
        }
        if !self.send_semantic_command(SemanticWorkerCommand::Search {
            generation,
            query: ParsedQuery::parse(&query),
            corpus_version,
            scope_version,
            prewarm,
        }) {
            self.reset_semantic_worker();
            let Some((corpus_version, scope_version)) = self.send_semantic_state() else {
                self.semantic_search.error = Some("semantic worker unavailable".to_string());
                self.semantic_search.pending_status = Some(SemanticProgress::Failed);
                return;
            };
            if !self.send_semantic_command(SemanticWorkerCommand::Search {
                generation,
                query: ParsedQuery::parse(&query),
                corpus_version,
                scope_version,
                prewarm,
            }) {
                self.semantic_search.error = Some("semantic worker unavailable".to_string());
                self.semantic_search.pending_status = Some(SemanticProgress::Failed);
            }
        }
    }

    pub(super) fn prewarm_semantic_cache(&mut self) {
        if self.list_search_mode == ListSearchMode::Semantic
            && self.query.trim().is_empty()
            && !self.is_loading()
        {
            self.dispatch_semantic_search(String::new(), true);
        }
    }

    pub fn receive_search_results(&mut self) -> bool {
        let mut applied = false;
        while let Ok(held) = self.texts_held_rx.try_recv() {
            applied |= self.texts_held.without != held.without;
            self.texts_held = held;
        }
        while let Ok(mut response) = self.search_rx.try_recv() {
            if let Some(results) = response.exact_literal_results.take() {
                if response.generation == self.search_generation
                    && self.list_search_mode == ListSearchMode::Semantic
                {
                    self.apply_exact_literal_results(response, results);
                    applied = true;
                }
                continue;
            }
            let semantic_fallback_pending = response.mode == ListSearchMode::Semantic
                && self.semantic_search.pending_generation == Some(response.generation);
            if response.generation == self.search_generation
                && response.mode == self.list_search_mode
                && (response.mode == ListSearchMode::Lexical || semantic_fallback_pending)
            {
                let filtered = self.filter_indices(response.filtered);
                // Keep `self.filtered` until the semantic result replaces it.
                if response.mode == ListSearchMode::Semantic && filtered.is_empty() {
                    continue;
                }
                self.lexical_evidence = response.evidence;
                if response.mode == ListSearchMode::Semantic {
                    self.semantic_search.results.clear();
                }
                self.shown_results_query = self.dispatched_query.clone();
                self.shown_results_settled = response.mode == ListSearchMode::Lexical;
                self.apply_filtered(filtered);
                if response.mode == ListSearchMode::Lexical {
                    self.search_started_at = None;
                }
                applied = true;
            }
        }
        if let Some(rx) = self.semantic_search.worker_rx.take() {
            let active_generation = self.search_generation;
            while let Ok(message) = rx.try_recv() {
                match message {
                    SemanticSearchMessage::Progress {
                        generation,
                        progress,
                    } => {
                        if self.list_search_mode == ListSearchMode::Semantic {
                            if Some(generation) == self.semantic_search.prewarm_generation {
                                self.semantic_search.prewarm_status = Some(progress.clone());
                                self.semantic_search.pending_status = Some(progress);
                                applied = true;
                            } else if generation == active_generation {
                                self.semantic_search.prewarm_generation = None;
                                self.semantic_search.prewarm_status = None;
                                self.semantic_search.pending_status = Some(progress);
                                applied = true;
                            }
                        }
                    }
                    SemanticSearchMessage::Complete(response) => {
                        if self.list_search_mode == ListSearchMode::Semantic {
                            if response.prewarm
                                && Some(response.generation)
                                    == self.semantic_search.prewarm_generation
                            {
                                self.semantic_search.prewarm_generation = None;
                                self.semantic_search.prewarm_status = None;
                                if response.generation == active_generation {
                                    self.semantic_search.pending_generation = None;
                                    self.semantic_search.pending_status = None;
                                    self.semantic_search.last_status = response.progress;
                                    self.semantic_search.error = response.error;
                                    self.semantic_search.results = response.metadata;
                                    if !self.query.trim().is_empty() {
                                        self.dispatch_semantic_search(
                                            self.query.trim().to_string(),
                                            false,
                                        );
                                    }
                                }
                                applied = true;
                            } else if response.generation == active_generation {
                                self.search_started_at = None;
                                self.semantic_search.pending_generation = None;
                                self.semantic_search.pending_status = None;
                                self.semantic_search.last_status = response.progress;
                                self.semantic_search.error = response.error;
                                self.semantic_search.results = response.metadata;
                                let filtered = self.filter_indices(response.filtered);
                                self.shown_results_query = self.dispatched_query.clone();
                                self.shown_results_settled = true;
                                self.apply_filtered(filtered);
                                applied = true;
                            }
                        }
                    }
                }
            }
            self.semantic_search.worker_rx = Some(rx);
        }
        applied
    }

    /// Shows `response`'s rows as the semantic search's results, each with
    /// its entry of `results`.
    fn apply_exact_literal_results(
        &mut self,
        response: SearchResponse,
        results: HashMap<usize, SemanticResultMetadata>,
    ) {
        self.search_started_at = None;
        self.semantic_search.pending_generation = None;
        self.semantic_search.pending_status = None;
        self.semantic_search.last_status = SemanticProgress::Complete;
        self.semantic_search.error = None;
        self.semantic_search.results = results;
        self.lexical_evidence = response.evidence;
        let filtered = self.filter_indices(response.filtered);
        self.shown_results_query = self.dispatched_query.clone();
        self.shown_results_settled = true;
        self.apply_filtered(filtered);
    }

    pub fn has_search_work_in_flight(&self) -> bool {
        self.search_started_at.is_some()
            || self.semantic_search.pending_generation.is_some()
            || self.semantic_search.prewarm_generation.is_some()
    }

    #[cfg(test)]
    pub(super) fn search_generation(&self) -> u64 {
        self.search_generation
    }

    #[cfg(test)]
    pub(super) fn semantic_search_error(&self) -> Option<&str> {
        self.semantic_search.error.as_deref()
    }

    /// The rows are taken to answer `query`, as they would after its search
    /// landed; a test of the in-flight state overrides that with
    /// [`set_shown_results_query_for_test`](Self::set_shown_results_query_for_test).
    #[cfg(test)]
    pub fn set_query_for_test(&mut self, query: &str) {
        self.query = query.to_string();
        self.cursor_pos = self.query.chars().count();
        let trimmed = query.trim();
        self.shown_results_query = (!trimmed.is_empty()).then(|| trimmed.to_string());
        self.shown_results_settled = true;
    }

    #[cfg(test)]
    pub fn set_shown_results_query_for_test(&mut self, query: &str) {
        self.shown_results_query = (!query.is_empty()).then(|| query.to_string());
        self.shown_results_settled = true;
    }

    #[cfg(test)]
    pub(crate) fn update_filter_for_test(&mut self) {
        self.update_filter();
    }

    /// Apply the search worker's results for the lexical search in flight,
    /// failing after five seconds without them.
    #[cfg(test)]
    pub(crate) fn settle_search_for_test(&mut self) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while self.search_started_at.is_some() {
            assert!(
                std::time::Instant::now() < deadline,
                "the search worker answers"
            );
            self.receive_search_results();
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }

    #[cfg(test)]
    pub fn set_semantic_receiver_for_test(
        &mut self,
        generation: u64,
        worker_rx: mpsc::Receiver<SemanticSearchMessage>,
    ) {
        self.search_generation = generation;
        self.dispatched_query = Some(self.query.trim().to_string());
        self.semantic_search.pending_generation = Some(generation);
        self.semantic_search.prewarm_generation = None;
        self.semantic_search.prewarm_status = None;
        self.semantic_search.worker_rx = Some(worker_rx);
    }

    #[cfg(test)]
    pub fn set_semantic_prewarm_generation_for_test(&mut self, generation: u64) {
        self.semantic_search.prewarm_generation = Some(generation);
    }

    pub fn semantic_result_metadata(
        &self,
        conversation_index: usize,
    ) -> Option<&SemanticResultMetadata> {
        self.semantic_search.results.get(&conversation_index)
    }

    pub fn lexical_evidence(&self, conversation_index: usize) -> Option<&search::LexicalEvidence> {
        self.lexical_evidence.get(&conversation_index)
    }

    #[cfg(test)]
    pub fn set_lexical_evidence_for_test(
        &mut self,
        conversation_index: usize,
        evidence: search::LexicalEvidence,
    ) {
        self.lexical_evidence.insert(conversation_index, evidence);
    }

    pub fn semantic_result_metadata_for_selection(&self) -> Option<&SemanticResultMetadata> {
        if self.list_search_mode != ListSearchMode::Semantic {
            return None;
        }
        let selected = self.selected?;
        let conversation_index = *self.filtered.get(selected)?;
        self.semantic_result_metadata(conversation_index)
    }

    pub fn semantic_status_text(&self) -> Option<String> {
        if self.list_search_mode != ListSearchMode::Semantic {
            return None;
        }
        if self.semantic_search.error.is_some() {
            return Some("sem failed".to_string());
        }
        let status = self
            .semantic_search
            .pending_status
            .as_ref()
            .unwrap_or(&self.semantic_search.last_status);
        match status {
            SemanticProgress::EmptyCorpus => Some("sem no text".to_string()),
            SemanticProgress::Failed => Some("sem failed".to_string()),
            _ => None,
        }
    }

    pub fn semantic_activity_status_text(&self) -> Option<String> {
        if self.list_search_mode != ListSearchMode::Semantic || self.semantic_search.error.is_some()
        {
            return None;
        }
        let status = self
            .semantic_search
            .prewarm_status
            .as_ref()
            .or(self.semantic_search.pending_status.as_ref())?;
        match status {
            SemanticProgress::InitializingModel => Some("sem preparing embeddings".to_string()),
            SemanticProgress::Embedding { completed, total } => {
                let percent = if *total == 0 {
                    100
                } else {
                    completed.min(total).saturating_mul(100) / total
                };
                Some(format!(
                    "sem embedding {percent}%  {}/{total} chunks",
                    completed.min(total)
                ))
            }
            _ => None,
        }
    }

    /// Turn the workspace filter on or off. With a query, the rows on screen
    /// narrow at once, and the search runs again on its worker.
    pub(super) fn toggle_workspace_filter(&mut self) {
        if self.workspace.is_some() {
            self.workspace_filter = !self.workspace_filter;
            self.invalidate_search_generation();
            if !self.query.trim().is_empty() {
                let shown = std::mem::take(&mut self.filtered);
                let narrowed = self.filter_indices(shown);
                self.apply_filtered(narrowed);
            }
            if self.list_search_mode == ListSearchMode::Semantic && !self.query.trim().is_empty() {
                self.dispatch_search();
            } else {
                self.update_filter();
            }
        }
    }

    pub(super) fn toggle_list_search_mode(&mut self) {
        if !self.semantic_search.available {
            return;
        }
        self.list_search_mode = match self.list_search_mode {
            ListSearchMode::Lexical => ListSearchMode::Semantic,
            ListSearchMode::Semantic => ListSearchMode::Lexical,
        };
        self.invalidate_search_generation();
        if self.list_search_mode == ListSearchMode::Semantic && self.query.trim().is_empty() {
            self.prewarm_semantic_cache();
        } else {
            self.dispatch_search();
        }
    }

    /// Answer `query` as a session id, reporting whether it was one. A query
    /// that is not an id is left to ordinary search.
    pub(super) fn apply_session_lookup(&mut self, query: &str) -> bool {
        match self.look_up_session(query) {
            SessionLookup::Listed(index) => {
                self.session_id_query = Some(SessionIdQuery::Listed(query.to_owned()));
                self.shown_results_query = Some(query.to_owned());
                self.shown_results_settled = true;
                self.apply_filtered(vec![index]);
                true
            }
            SessionLookup::Loading(path) => {
                self.session_id_query = Some(SessionIdQuery::Loading {
                    query: query.to_owned(),
                    path,
                });
                self.shown_results_query = Some(query.to_owned());
                self.shown_results_settled = true;
                self.apply_filtered(Vec::new());
                true
            }
            SessionLookup::Unresolved => {
                self.session_id_query = Some(SessionIdQuery::Unresolved(query.to_owned()));
                self.shown_results_query = Some(query.to_owned());
                self.shown_results_settled = true;
                self.apply_filtered(Vec::new());
                true
            }
            SessionLookup::NotAnId => {
                self.session_id_query = None;
                false
            }
        }
    }

    /// Show every row the filters keep, newest first: the list without a
    /// query. Reads no session cache.
    fn show_unsearched_list(&mut self) {
        let now = self.clock().wall().with_timezone(&Local);
        let filtered = search::search_with_texts(
            &self.conversations,
            &self.searchable,
            &SearchTextsInMemory::default(),
            "",
            now,
        );
        let filtered = self.filter_indices(filtered);
        self.shown_results_query = None;
        self.shown_results_settled = true;
        self.apply_filtered(filtered);
    }

    /// Filter the list again with the current query. A lexical query
    /// searches on the search worker; the rows on screen stay until its
    /// results arrive.
    pub(super) fn update_filter(&mut self) {
        let query = self.query.trim().to_string();

        if ParsedQuery::parse(&query).is_effectively_empty() {
            self.semantic_search.error = None;
            self.session_id_query = None;
            self.show_unsearched_list();
            return;
        }

        if self.apply_session_lookup(&query) {
            self.semantic_search.error = None;
            return;
        }

        if self.list_search_mode == ListSearchMode::Semantic {
            return;
        }

        self.dispatch_lexical_search(query);
    }

    fn look_up_session(&mut self, query: &str) -> SessionLookup {
        if let Some(index) = self
            .conversations
            .iter()
            .position(|conversation| search::session_id_matches(&conversation.session_id, query))
        {
            return SessionLookup::Listed(index);
        }
        if let Some(lookup) = self.load_session_by_id(query) {
            return lookup;
        }
        // A miss is reported only for the shape some agent's session ids
        // have. Anything else is likelier text the user meant to search for.
        if crate::history::provider::is_session_id_shape(query) {
            SessionLookup::Unresolved
        } else {
            SessionLookup::NotAnId
        }
    }

    /// Load a session the list does not hold — one an active `--since` window
    /// excludes — from whichever agent stores it, as the cache holds its row.
    /// A session the cache lacks is refreshed as an opened one is, and its
    /// row joins the list when the list applies the refresh. `None` when no
    /// agent resolves the ID, or its session is gone or empty.
    fn load_session_by_id(&mut self, session_id: &str) -> Option<SessionLookup> {
        use crate::history::provider::SessionRead;

        let (source, loaded) = crate::history::provider::load_session_by_id(
            &self.environment,
            session_id,
            self.show_last,
        )?;
        let path = loaded.locator().to_path_buf();
        match self.take_read(loaded) {
            SessionRead::Listed(conv) if self.lists(SessionOrigin::FoundById, &conv) => {
                let idx = self.conversations.len();
                self.conversations.push(*conv);
                self.searchable = search::precompute_search_text(&self.conversations);
                self.rebuild_conversations_snapshot();
                self.send_search_data();
                Some(SessionLookup::Listed(idx))
            }
            SessionRead::NotCached => {
                self.store.track_lookup(source, &path);
                Some(SessionLookup::Loading(path))
            }
            SessionRead::Listed(_)
            | SessionRead::Empty(_)
            | SessionRead::Missing
            | SessionRead::Failed(..) => None,
        }
    }

    /// The session found by ID at `path` has no row to list: the lookup
    /// that waited for it reports the ID unresolved.
    pub(super) fn report_lookup_unresolved(&mut self, path: &Path) {
        if let Some(SessionIdQuery::Loading {
            query,
            path: awaited,
        }) = &self.session_id_query
            && awaited == path
        {
            self.session_id_query = Some(SessionIdQuery::Unresolved(query.clone()));
        }
    }

    pub(super) fn refresh_search_data(&mut self) {
        self.rebuild_conversations_snapshot();
        self.searchable = search::precompute_search_text(&self.conversations);
        self.send_search_data();
        self.invalidate_search_generation();
    }
}
