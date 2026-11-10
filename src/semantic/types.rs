use crate::agent::refs::MessageRange;
use crate::clock::{Clock, Moment};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use std::time::Duration;

pub const DEFAULT_CHUNK_TARGET_CHARS: usize = 2_400;
pub const DEFAULT_CHUNK_OVERLAP_CHARS: usize = 300;
pub const DEFAULT_CHUNK_CONTEXT_TURNS: usize = 1;
/// Throughput is flat from 8 to 128 passages per batch; peak memory grows
/// with the batch, about 55 MB per passage at the model's token limit.
pub const DEFAULT_EMBEDDING_BATCH_SIZE: usize = 8;
/// About two batches at the measured rate of 7 passages per second (on two
/// example machines).
pub const DEFAULT_INTERACTIVE_EMBEDDING_SECONDS: u64 = 2;
pub const MAX_WITHIN_INTERACTIVE_PASSAGE_EMBEDDINGS: usize = 32;
pub const CACHE_SCHEMA_VERSION: u32 = 7;
pub const MODEL_NAME: &str = "BGESmallENV15";
pub const MODEL_DIMENSIONS: usize = 384;

/// The passage count and time limit a search embeds within before it
/// answers. A limit that is `None` does not apply.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EmbeddingBudget {
    /// Distinct passages to embed at most.
    passages: Option<usize>,
    /// Counted from the first batch; once it has passed, no further batch
    /// starts. The first batch always runs.
    time: Option<Duration>,
}

impl EmbeddingBudget {
    pub fn unbounded() -> Self {
        Self {
            passages: None,
            time: None,
        }
    }

    pub fn passages(count: usize) -> Self {
        Self {
            passages: Some(count),
            time: None,
        }
    }

    /// A zero limit embeds nothing, without starting a clock.
    pub fn time(limit: Duration) -> Self {
        if limit.is_zero() {
            return Self::passages(0);
        }
        Self {
            passages: None,
            time: Some(limit),
        }
    }

    pub fn seconds(seconds: u64) -> Self {
        Self::time(Duration::from_secs(seconds))
    }

    pub fn embeds_nothing(&self) -> bool {
        self.passages == Some(0)
    }

    /// `missing`, capped at the passage limit.
    pub fn passages_within(&self, missing: usize) -> usize {
        self.passages.map_or(missing, |limit| missing.min(limit))
    }

    /// True once `elapsed`, the time since the first batch started, reaches
    /// the time limit.
    pub fn time_is_up(&self, elapsed: Duration) -> bool {
        self.time.is_some_and(|limit| elapsed >= limit)
    }

    /// This budget, its time limit measured on `clock`.
    pub fn timed_on(self, clock: &dyn Clock) -> TimedBudget<'_> {
        TimedBudget {
            budget: self,
            clock,
        }
    }
}

/// An [`EmbeddingBudget`] and the clock its time limit is measured on.
#[derive(Clone, Copy)]
pub struct TimedBudget<'a> {
    budget: EmbeddingBudget,
    clock: &'a dyn Clock,
}

impl TimedBudget<'_> {
    /// `missing`, capped at the passage limit.
    pub fn passages_within(&self, missing: usize) -> usize {
        self.budget.passages_within(missing)
    }

    /// The current moment on the budget's clock, for [`Self::time_is_up`].
    pub fn start(&self) -> Moment {
        self.clock.now()
    }

    /// True once the time limit has passed since `started`.
    pub fn time_is_up(&self, started: Moment) -> bool {
        self.budget.time_is_up(self.clock.elapsed_since(started))
    }
}

#[derive(Clone, Debug, Default)]
pub struct SemanticCancellationToken {
    generation: Arc<AtomicU64>,
    expected_generation: u64,
}

impl SemanticCancellationToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn child(&self) -> Self {
        Self {
            generation: self.generation.clone(),
            expected_generation: self.generation.load(Ordering::Relaxed),
        }
    }

    pub fn cancel(&self) {
        self.generation.fetch_add(1, Ordering::Relaxed);
    }

    pub fn is_cancelled(&self) -> bool {
        self.generation.load(Ordering::Relaxed) != self.expected_generation
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ChunkConfig {
    pub target_chars: usize,
    pub overlap_chars: usize,
    pub context_turns: usize,
}

impl Default for ChunkConfig {
    fn default() -> Self {
        Self {
            target_chars: DEFAULT_CHUNK_TARGET_CHARS,
            overlap_chars: DEFAULT_CHUNK_OVERLAP_CHARS,
            context_turns: DEFAULT_CHUNK_CONTEXT_TURNS,
        }
    }
}

/// The chunk hits a search returns beside each conversation's best chunk.
/// Ask only for the hits the caller prints: building explanations is most of
/// a query's ranking time.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ChunkHits {
    #[default]
    None,
    /// Every evidence chunk of the first `n` ranked conversations with
    /// evidence. An agent route ranks its conversation but is not evidence.
    TopConversations(usize),
}

#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub enum SemanticChunkSource {
    #[default]
    VisibleDialogue,
    AgentRoute,
    AgentTool,
    AgentThinking,
    AgentSubagentDialogue,
    AgentSubagentTool,
    AgentSubagentThinking,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SemanticChunk {
    pub conversation_index: usize,
    pub source: SemanticChunkSource,
    pub session: String,
    pub chunk_index: usize,
    pub text: String,
    pub message_range: MessageRange,
}

#[derive(Clone, Debug, PartialEq)]
pub struct EmbeddedChunk {
    pub conversation_index: usize,
    pub source: SemanticChunkSource,
    pub session: String,
    pub chunk_index: usize,
    pub text: String,
    pub message_range: MessageRange,
    pub embedding: Vec<f32>,
}

/// The bonus a chunk holding every query word adds to its similarity.
const WORD_MATCH_WEIGHT: f32 = 0.2;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SemanticScoreBreakdown {
    pub hybrid: f32,
    pub semantic: f32,
    /// The share of the query's words found in the chunk, 0 to 1.
    pub word_match: f32,
}

impl SemanticScoreBreakdown {
    pub fn new(semantic: f32, word_match: f32) -> Self {
        Self {
            hybrid: semantic + bonus_for(word_match),
            semantic,
            word_match,
        }
    }

    /// The bonus `word_match` adds to `semantic` in `hybrid`.
    pub fn word_match_bonus(self) -> f32 {
        bonus_for(self.word_match)
    }
}

fn bonus_for(word_match: f32) -> f32 {
    WORD_MATCH_WEIGHT * word_match
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SemanticQuality {
    Strong,
    Good,
    Fair,
    Weak,
}

impl SemanticQuality {
    pub fn label(self) -> &'static str {
        match self {
            Self::Strong => "strong",
            Self::Good => "good",
            Self::Fair => "fair",
            Self::Weak => "weak",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SemanticRationaleKind {
    SemanticOnly,
    LexicalBoosted,
    WeakMatch,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SemanticChunkIdentity {
    pub conversation_index: usize,
    pub source: SemanticChunkSource,
    pub session: String,
    pub chunk_index: usize,
    pub message_range: MessageRange,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SemanticExplanation {
    pub quality: SemanticQuality,
    pub quality_label: &'static str,
    pub matched_terms: Vec<String>,
    pub evidence_preview: String,
    pub rationale_kind: SemanticRationaleKind,
    pub chunk: SemanticChunkIdentity,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SemanticHit {
    pub conversation_index: usize,
    pub session: String,
    pub chunk_index: usize,
    pub semantic_score: f32,
    pub lexical_score: f32,
    pub hybrid_score: f32,
    pub score_breakdown: SemanticScoreBreakdown,
    pub explanation: SemanticExplanation,
    pub snippet: String,
    pub message_range: MessageRange,
}

impl SemanticHit {
    pub fn new(score_breakdown: SemanticScoreBreakdown, explanation: SemanticExplanation) -> Self {
        let chunk = &explanation.chunk;
        Self {
            conversation_index: chunk.conversation_index,
            session: chunk.session.clone(),
            chunk_index: chunk.chunk_index,
            semantic_score: score_breakdown.semantic,
            lexical_score: score_breakdown.word_match_bonus(),
            hybrid_score: score_breakdown.hybrid,
            snippet: explanation.evidence_preview.clone(),
            message_range: chunk.message_range,
            score_breakdown,
            explanation,
        }
    }
}

#[derive(Serialize, Deserialize)]
pub struct EmbeddingCache {
    pub schema_version: u32,
    pub model: String,
    pub chunk_target_chars: usize,
    pub chunk_overlap_chars: usize,
    pub chunk_context_turns: usize,
    pub access_counter: u64,
    pub entries: HashMap<String, CachedChunk>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct CachedChunk {
    pub embedding: Vec<f32>,
    pub last_used: u64,
    pub protected: bool,
}
