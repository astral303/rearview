//! An agent command ranks by meaning the passages the session list's
//! semantic search embeds. Over each registered agent's every-record
//! session, once the session list's candidates or
//! `--generate-semantic-cache`'s are embedded into a passage store, a
//! semantic or hybrid `agent within` or `agent search`, without a window and
//! with windows over middle messages, doesn't send the embedder any passage
//! and doesn't print a `partial-index` warning.

use crate::agent::diagnostic::{AgentWarning, AgentWarningKind};
use crate::agent::hit_read_coverage::windows_over_middle_messages;
use crate::agent::refs::{
    AgentConversationKey, ResolvedConversation, resolved_conversation_for_key,
};
use crate::agent::search::{AgentConversationInput, AgentSearchRequest, AgentWithinRequest};
use crate::agent::service::{
    AgentService, SemanticEngine, conversation_from_agent_transcript, run_agent_semantic_hits,
    run_agent_within_semantic,
};
use crate::agent::transcript::AgentTranscript;
use crate::clock::{Clock, TestClock};
use crate::error::Result;
use crate::history::Conversation;
use crate::history::environment::test_support::test_environment;
use crate::history::path::CanonicalDirectories;
use crate::history::provider::contract_tests::every_record_sessions;
use crate::search::mode::SearchMode;
use crate::semantic::chunk::builds_any_chunk;
use crate::semantic::embed::SemanticEmbedder;
use crate::semantic::index::{SemanticIndexCandidate, SemanticIndexRequest, SemanticIndexState};
use crate::semantic::store::test_support::{
    FAKE_DIMENSIONS, temporary_store_at, temporary_store_path,
};
use crate::semantic::types::{ChunkConfig, ChunkHits, EmbeddingBudget, SemanticCancellationToken};
use crate::semantic_cli::cache_generation_candidates;
use crate::time_filter::TimeFilter;
use crate::tui::session_list_candidates;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Counts in `embedded` the passages it is sent. Every stored passage
/// matches every query: each passage and each query gets the same embedding.
struct CountingEmbedder {
    embedded: Arc<AtomicUsize>,
}

impl SemanticEmbedder for CountingEmbedder {
    fn dimensions(&self) -> usize {
        FAKE_DIMENSIONS
    }

    fn embed_passages(&mut self, passages: &[String]) -> Result<Vec<Vec<f32>>> {
        self.embedded.fetch_add(passages.len(), Ordering::Relaxed);
        Ok(passages.iter().map(|_| vec![1.0, 0.0]).collect())
    }

    fn embed_query(&mut self, _query: &str) -> Result<Option<Vec<f32>>> {
        Ok(Some(vec![1.0, 0.0]))
    }
}

/// An index of its own over the passage store at `store`.
fn index_over(store: &Path, clock: &Arc<dyn Clock>) -> SemanticIndexState {
    SemanticIndexState::with_store(
        ChunkConfig::default(),
        temporary_store_at(store.to_path_buf(), Arc::clone(clock)),
        Arc::clone(clock),
    )
}

/// Each registered agent's every-record session, cached in `base`, as the
/// row the session list shows for it.
fn cached_rows(base: &Path) -> Vec<Conversation> {
    every_record_sessions()
        .iter()
        .map(|session| crate::history::sqlite_cache::tests::cached_row(base, session))
        .collect()
}

/// Embeds the candidates of the session list into the passage store at
/// `store`, as the list's semantic search does before a query. Returns the
/// number of passages embedded.
///
/// One list per row, because several agents' fixtures share one cache key:
/// the list reads one session's turns for rows of one root and key.
fn embed_session_list_passages(
    rows: &[Conversation],
    store: &Path,
    clock: &Arc<dyn Clock>,
) -> usize {
    rows.iter()
        .map(|row| {
            let candidates = session_list_candidates(std::slice::from_ref(row));
            embed_passages(&candidates, store, clock)
        })
        .sum()
}

/// Embeds the candidates of `--generate-semantic-cache` for `rows` into the
/// passage store at `store`. Returns the number of passages embedded.
fn embed_cache_generation_passages(
    rows: &[Conversation],
    store: &Path,
    clock: &Arc<dyn Clock>,
) -> usize {
    let candidates = cache_generation_candidates(&rows.iter().collect::<Vec<_>>());
    assert_eq!(
        candidates.len(),
        rows.len(),
        "cache generation reads every agent's session"
    );
    embed_passages(&candidates, store, clock)
}

/// A part of rearview that embeds passages before an agent command runs.
struct PassageSource {
    name: &'static str,
    /// Embeds this part's candidates for the rows into the passage store at
    /// the path. Returns the number of passages embedded.
    embed_passages: fn(&[Conversation], &Path, &Arc<dyn Clock>) -> usize,
}

impl PassageSource {
    fn every() -> [Self; 2] {
        [
            Self {
                name: "the session list",
                embed_passages: embed_session_list_passages,
            },
            Self {
                name: "--generate-semantic-cache",
                embed_passages: embed_cache_generation_passages,
            },
        ]
    }

    /// The path of a new passage store holding this part's passages for
    /// `rows`.
    fn store_of(&self, rows: &[Conversation], clock: &Arc<dyn Clock>) -> std::path::PathBuf {
        let store = temporary_store_path();
        assert!(
            (self.embed_passages)(rows, &store, clock) > 0,
            "{} embeds its passages first",
            self.name
        );
        store
    }
}

/// Embeds every passage of `candidates` missing from the passage store at
/// `store`. Returns the number of passages embedded.
fn embed_passages(
    candidates: &[SemanticIndexCandidate],
    store: &Path,
    clock: &Arc<dyn Clock>,
) -> usize {
    assert!(
        candidates
            .iter()
            .all(|candidate| builds_any_chunk(candidate.conversation())),
        "each agent's session holds a passage to embed"
    );
    let embedded = Arc::new(AtomicUsize::new(0));
    index_over(store, clock)
        .refresh_passages(
            &SemanticIndexRequest {
                query: "",
                literal_filters: &[],
                full_corpus: candidates,
                scope: candidates,
                prewarm: true,
                chunk_hits: ChunkHits::None,
                window: TimeFilter::default(),
            },
            &mut CountingEmbedder {
                embedded: Arc::clone(&embedded),
            },
            &SemanticCancellationToken::new(),
            |_| {},
        )
        .unwrap();
    embedded.load(Ordering::Relaxed)
}

/// An engine whose embedder counts the passages it is sent.
type CountingEngine = SemanticEngine<Box<dyn FnOnce() -> Result<CountingEmbedder>>>;

/// The passage store an agent command's semantic stage ranks from, and the
/// gaps the stages run over it left: each passage sent to the embedder and
/// each `partial-index` warning, under the stage's case.
struct EmbeddedStore<'s> {
    store: &'s Path,
    clock: &'s Arc<dyn Clock>,
    gaps: Vec<String>,
}

impl EmbeddedStore<'_> {
    /// The output of `stage`, a command's semantic stage named `case`, run
    /// with an engine over this store.
    fn run<Output>(
        &mut self,
        case: &str,
        stage: impl FnOnce(CountingEngine) -> Result<(Output, Vec<AgentWarning>)>,
    ) -> Output {
        let embedded = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&embedded);
        let (output, warnings) = stage(SemanticEngine {
            index: index_over(self.store, self.clock),
            open_embedder: Box::new(move || Ok(CountingEmbedder { embedded: counted })),
        })
        .unwrap();
        let embedded = embedded.load(Ordering::Relaxed);
        if embedded > 0 {
            self.gaps
                .push(format!("{case}: sent the embedder {embedded} passages"));
        }
        if warnings
            .iter()
            .any(|warning| warning.kind == AgentWarningKind::PartialIndex)
        {
            self.gaps
                .push(format!("{case}: printed a partial-index warning"));
        }
        output
    }
}

/// A cached session as `agent within` targets it.
struct WithinTarget {
    label: String,
    conversation: Conversation,
    resolved: ResolvedConversation,
    transcript: AgentTranscript,
    /// No window, then windows over middle messages.
    windows: Vec<TimeFilter>,
}

impl WithinTarget {
    fn of(row: &Conversation, clock: &dyn Clock) -> Self {
        let label = row.source.label().to_string();
        let key = AgentConversationKey::from_conversation(row, &CanonicalDirectories::of_system())
            .unwrap();
        let copy = row.cached_session.as_ref().expect("the row reads its copy");
        let transcript = AgentTranscript::from_stored(
            &row.path,
            copy.read_copy(|held| held.numbered_entries(None)).unwrap(),
        )
        .unwrap();
        let windows = windows_over_middle_messages(&transcript);
        assert!(windows.len() > 1, "{label}: the session has dated messages");
        Self {
            conversation: conversation_from_agent_transcript(&transcript, &key, clock),
            resolved: resolved_conversation_for_key(std::slice::from_ref(&key), &key),
            label,
            transcript,
            windows,
        }
    }
}

#[test]
fn within_doesnt_embed_once_list_or_cache_generation_passages_are_embedded_for_every_agent() {
    let clock: Arc<dyn Clock> = Arc::new(TestClock::default());
    let base = tempfile::tempdir().unwrap();
    let rows = cached_rows(base.path());
    let targets = rows
        .iter()
        .map(|row| WithinTarget::of(row, clock.as_ref()))
        .collect::<Vec<_>>();
    let service = AgentService::new(test_environment(Vec::new(), Some(base.path())));

    let mut gaps = Vec::new();
    for source in PassageSource::every() {
        let store = source.store_of(&rows, &clock);
        let mut embedded = EmbeddedStore {
            store: &store,
            clock: &clock,
            gaps: Vec::new(),
        };
        for target in &targets {
            for window in &target.windows {
                for mode in [SearchMode::Semantic, SearchMode::Hybrid] {
                    let request = AgentWithinRequest {
                        query: "anything said".to_owned(),
                        top: 10,
                        cli_mode: Some(mode),
                        config_mode: None,
                        tui_semantic_search: None,
                        budget: None,
                        window: *window,
                    };
                    let case = format!(
                        "{} {mode:?} within {window:?} after {}",
                        target.label, source.name
                    );
                    let output = embedded.run(&case, |engine| {
                        run_agent_within_semantic(
                            &service,
                            engine,
                            &request,
                            &target.conversation,
                            &target.resolved,
                            &target.transcript,
                        )
                    });
                    if !window.is_active() && output.every_hit().next().is_none() {
                        embedded
                            .gaps
                            .push(format!("{case}: didn't rank any stored passage"));
                    }
                }
            }
        }
        gaps.extend(embedded.gaps);
    }
    assert!(gaps.is_empty(), "{}", gaps.join("\n"));
}

/// The semantic stage of `agent search`, alone and as hybrid search's
/// semantic half, over every agent's session at once.
#[test]
fn search_doesnt_embed_once_list_or_cache_generation_passages_are_embedded_for_every_agent() {
    let clock: Arc<dyn Clock> = Arc::new(TestClock::default());
    let base = tempfile::tempdir().unwrap();
    let rows = cached_rows(base.path());
    let keys = rows
        .iter()
        .map(|row| {
            AgentConversationKey::from_conversation(row, &CanonicalDirectories::of_system())
                .unwrap()
        })
        .collect::<Vec<_>>();
    let inputs = rows
        .iter()
        .zip(&keys)
        .enumerate()
        .map(|(index, (row, key))| AgentConversationInput {
            conversation: row,
            resolved: resolved_conversation_for_key(&keys, key),
            original_index: index,
        })
        .collect::<Vec<_>>();
    let first_transcript = AgentTranscript::from_stored(
        &rows[0].path,
        rows[0]
            .cached_session
            .as_ref()
            .expect("the row reads its copy")
            .read_copy(|held| held.numbered_entries(None))
            .unwrap(),
    )
    .unwrap();
    let windows = windows_over_middle_messages(&first_transcript);
    assert!(windows.len() > 1, "the first session has dated messages");
    let service = AgentService::new(test_environment(Vec::new(), Some(base.path())));

    let mut gaps = Vec::new();
    for source in PassageSource::every() {
        let store = source.store_of(&rows, &clock);
        let mut embedded = EmbeddedStore {
            store: &store,
            clock: &clock,
            gaps: Vec::new(),
        };
        for window in &windows {
            for mode in [SearchMode::Semantic, SearchMode::Hybrid] {
                let request = AgentSearchRequest {
                    query: "anything said".to_owned(),
                    top: 10,
                    cli_mode: Some(mode),
                    config_mode: None,
                    tui_semantic_search: None,
                    flat: false,
                    hits_per_conversation: 3,
                    retrieval_hits_per_conversation: None,
                    all_hits: false,
                    budget: None,
                    session_offset: 0,
                    window: *window,
                };
                let case = format!("{mode:?} search {window:?} after {}", source.name);
                let semantic = embedded.run(&case, |engine| {
                    run_agent_semantic_hits(
                        &service,
                        engine,
                        &request,
                        &inputs,
                        EmbeddingBudget::unbounded(),
                    )
                });
                if !window.is_active() && semantic.ranked.conversations.len() != rows.len() {
                    embedded.gaps.push(format!(
                        "{case}: ranked {} of {} sessions",
                        semantic.ranked.conversations.len(),
                        rows.len()
                    ));
                }
            }
        }
        gaps.extend(embedded.gaps);
    }
    assert!(gaps.is_empty(), "{}", gaps.join("\n"));
}
