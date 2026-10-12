use super::SemanticIndexCandidate;
use crate::error::{AppError, Result};
use crate::history::Conversation;
use crate::semantic::chunk::build_chunks_with_sources;
use crate::semantic::rank::PreparedChunk;
use crate::semantic::types::{
    ChunkConfig, EmbeddedChunk, SemanticCancellationToken, SemanticChunk, SemanticChunkSource,
};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// A corpus position: the conversation's index in the list and its chunk
/// source.
type Position = (usize, SemanticChunkSource);

/// A conversation's identity in the resident index: its transcript path and
/// chunk source, numbered in corpus order when a corpus lists the same pair
/// more than once.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct ConversationKey {
    path: PathBuf,
    source: SemanticChunkSource,
    occurrence: usize,
}

/// Each candidate's key, in corpus order.
fn conversation_keys(candidates: &[SemanticIndexCandidate]) -> Vec<ConversationKey> {
    let mut occurrences = HashMap::<(&Path, SemanticChunkSource), usize>::new();
    candidates
        .iter()
        .map(|candidate| {
            let path = candidate.conversation.path.as_path();
            let occurrence = occurrences.entry((path, candidate.source)).or_default();
            let key = ConversationKey {
                path: path.to_path_buf(),
                source: candidate.source,
                occurrence: *occurrence,
            };
            *occurrence += 1;
            key
        })
        .collect()
}

/// True when both conversations build the same chunks.
fn builds_same_chunks(stored: &Arc<Conversation>, candidate: &Arc<Conversation>) -> bool {
    Arc::ptr_eq(stored, candidate)
        || (stored.session_id == candidate.session_id
            && stored.semantic_turns == candidate.semantic_turns
            && stored.semantic_turn_ranges == candidate.semantic_turn_ranges)
}

/// The corpus a complete resident index was last refreshed for. A request
/// for the same corpus skips the refresh.
pub(super) struct CorpusSignature {
    conversations: Vec<(Position, Arc<Conversation>)>,
}

impl CorpusSignature {
    pub(super) fn of(full_corpus: &[SemanticIndexCandidate]) -> Self {
        Self {
            conversations: full_corpus
                .iter()
                .map(|candidate| {
                    (
                        (candidate.index, candidate.source),
                        Arc::clone(&candidate.conversation),
                    )
                })
                .collect(),
        }
    }

    pub(super) fn matches(&self, full_corpus: &[SemanticIndexCandidate]) -> bool {
        self.conversations.len() == full_corpus.len()
            && self.conversations.iter().zip(full_corpus).all(
                |((position, conversation), candidate)| {
                    *position == (candidate.index, candidate.source)
                        && conversation.path == candidate.conversation.path
                        && builds_same_chunks(conversation, &candidate.conversation)
                },
            )
    }
}

struct ResidentConversation {
    /// The conversation these chunks were built from; `None` while any of
    /// its chunks lacks an embedding, so the next refresh builds it again.
    built_from: Option<Arc<Conversation>>,
    chunks: Vec<PreparedChunk>,
}

/// Every corpus conversation's prepared chunks, kept by path and source, so
/// a refresh prepares only conversations whose chunks changed and moves the
/// rest to their new positions.
#[derive(Default)]
pub(super) struct ResidentIndex {
    conversations: HashMap<ConversationKey, ResidentConversation>,
    /// The corpus as of the last refresh, in corpus order.
    corpus: Vec<(Position, ConversationKey)>,
    chunk_count: usize,
}

impl ResidentIndex {
    pub(super) fn clear(&mut self) {
        self.conversations.clear();
        self.corpus.clear();
        self.chunk_count = 0;
    }

    pub(super) fn chunk_count(&self) -> usize {
        self.chunk_count
    }

    /// Drop the conversations `full_corpus` no longer lists, move unchanged
    /// ones to their new positions, and return the chunks of every new,
    /// changed or incomplete conversation to embed.
    pub(super) fn plan_refresh(
        &mut self,
        full_corpus: &[SemanticIndexCandidate],
        chunk_config: ChunkConfig,
        cancellation: &SemanticCancellationToken,
    ) -> Result<RefreshPlan> {
        let keys = conversation_keys(full_corpus);
        let live = keys.iter().collect::<HashSet<_>>();
        let mut removed_chunks = 0;
        self.conversations.retain(|key, conversation| {
            let keep = live.contains(key);
            if !keep {
                removed_chunks += conversation.chunks.len();
            }
            keep
        });
        self.chunk_count -= removed_chunks;
        self.corpus = full_corpus
            .iter()
            .map(|candidate| (candidate.index, candidate.source))
            .zip(keys.iter().cloned())
            .collect();

        let mut plan = RefreshPlan::default();
        for (candidate, key) in full_corpus.iter().zip(keys) {
            if cancellation.is_cancelled() {
                return Err(AppError::SemanticSearchCancelled);
            }
            if let Some(resident) = self.conversations.get_mut(&key)
                && let Some(built_from) = &mut resident.built_from
                && builds_same_chunks(built_from, &candidate.conversation)
            {
                *built_from = Arc::clone(&candidate.conversation);
                for chunk in &mut resident.chunks {
                    chunk.set_conversation_index(candidate.index);
                }
                continue;
            }
            let chunks = build_chunks_with_sources(
                std::iter::once((
                    candidate.index,
                    candidate.source,
                    candidate.conversation.as_ref(),
                )),
                chunk_config,
            );
            plan.conversations.push(PendingConversation {
                key,
                position: (candidate.index, candidate.source),
                conversation: Arc::clone(&candidate.conversation),
                chunk_count: chunks.len(),
            });
            plan.chunks.extend(chunks);
        }
        Ok(plan)
    }

    /// Store the pending `conversations` with their chunks from `embedded`; a
    /// conversation is complete only when all its chunks are there.
    pub(super) fn absorb(
        &mut self,
        conversations: Vec<PendingConversation>,
        embedded: Vec<EmbeddedChunk>,
    ) {
        let slots = conversations
            .iter()
            .enumerate()
            .map(|(slot, pending)| (pending.position, slot))
            .collect::<HashMap<_, _>>();
        let mut grouped = conversations
            .iter()
            .map(|_| Vec::new())
            .collect::<Vec<Vec<EmbeddedChunk>>>();
        for chunk in embedded {
            if let Some(&slot) = slots.get(&(chunk.conversation_index, chunk.source)) {
                grouped[slot].push(chunk);
            }
        }

        for (pending, mut chunks) in conversations.into_iter().zip(grouped) {
            chunks.sort_by_key(|chunk| chunk.chunk_index);
            let complete = chunks.len() == pending.chunk_count;
            let chunks = chunks
                .into_iter()
                .map(PreparedChunk::new)
                .collect::<Vec<_>>();
            self.chunk_count += chunks.len();
            let resident = ResidentConversation {
                built_from: complete.then_some(pending.conversation),
                chunks,
            };
            if let Some(previous) = self.conversations.insert(pending.key, resident) {
                self.chunk_count -= previous.chunks.len();
            }
        }
    }

    /// True when every conversation of the last refresh's corpus has all its
    /// chunks embedded.
    pub(super) fn is_complete(&self) -> bool {
        self.corpus.iter().all(|(_, key)| {
            self.conversations
                .get(key)
                .is_some_and(|conversation| conversation.built_from.is_some())
        })
    }

    /// The prepared chunks of `scope`'s conversations, in corpus order.
    pub(super) fn scoped(
        &self,
        scope: &[SemanticIndexCandidate],
        cancellation: &SemanticCancellationToken,
    ) -> Result<Vec<&PreparedChunk>> {
        let scope = scope
            .iter()
            .map(|candidate| (candidate.index, candidate.source))
            .collect::<HashSet<_>>();
        let mut chunks = Vec::new();
        for (position, key) in &self.corpus {
            if cancellation.is_cancelled() {
                return Err(AppError::SemanticSearchCancelled);
            }
            if scope.contains(position)
                && let Some(conversation) = self.conversations.get(key)
            {
                chunks.extend(conversation.chunks.iter());
            }
        }
        Ok(chunks)
    }
}

/// The conversations a refresh builds again, and their chunks to embed.
#[derive(Default)]
pub(super) struct RefreshPlan {
    conversations: Vec<PendingConversation>,
    chunks: Vec<SemanticChunk>,
}

impl RefreshPlan {
    pub(super) fn chunks(&self) -> &[SemanticChunk] {
        &self.chunks
    }

    /// The conversations to store once their chunks are embedded, and the
    /// chunks to embed.
    pub(super) fn into_parts(self) -> (Vec<PendingConversation>, Vec<SemanticChunk>) {
        (self.conversations, self.chunks)
    }
}

pub(super) struct PendingConversation {
    key: ConversationKey,
    position: Position,
    conversation: Arc<Conversation>,
    chunk_count: usize,
}

/// True when any candidate in `full_corpus` builds at least one chunk.
pub(super) fn corpus_has_chunks(
    full_corpus: &[SemanticIndexCandidate],
    chunk_config: ChunkConfig,
) -> bool {
    full_corpus.iter().any(|candidate| {
        !build_chunks_with_sources(
            std::iter::once((
                candidate.index,
                candidate.source,
                candidate.conversation.as_ref(),
            )),
            chunk_config,
        )
        .is_empty()
    })
}
