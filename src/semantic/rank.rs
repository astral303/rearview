use crate::error::{AppError, Result};
use crate::semantic::evidence::{evidence_preview, matched_terms};
use crate::semantic::types::{
    ChunkHits, EmbeddedChunk, SemanticCancellationToken, SemanticChunkIdentity,
    SemanticChunkSource, SemanticExplanation, SemanticHit, SemanticQuality, SemanticRationaleKind,
    SemanticScoreBreakdown,
};
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

/// An indexed chunk with the parts of its score that do not depend on the
/// query: its lowercase text for the word overlap and its embedding's norm.
#[derive(Clone)]
pub struct PreparedChunk {
    embedded: EmbeddedChunk,
    text_lower: Box<str>,
    norm: f32,
}

impl PreparedChunk {
    pub fn new(embedded: EmbeddedChunk) -> Self {
        Self {
            text_lower: embedded.text.to_lowercase().into_boxed_str(),
            norm: vector_norm(&embedded.embedding),
            embedded,
        }
    }

    pub fn embedded(&self) -> &EmbeddedChunk {
        &self.embedded
    }

    /// Move the chunk to its conversation's new position in the corpus.
    pub fn set_conversation_index(&mut self, conversation_index: usize) {
        self.embedded.conversation_index = conversation_index;
    }
}

/// Each conversation's best chunk, best first, and the chunk hits a
/// [`ChunkHits`] asked for, best first.
pub struct RankedHits {
    pub conversations: Vec<SemanticHit>,
    pub chunks: Vec<SemanticHit>,
}

#[derive(Clone, Copy)]
struct ScoredChunk<'a> {
    chunk: &'a EmbeddedChunk,
    score: SemanticScoreBreakdown,
}

struct Query<'a> {
    text: &'a str,
    embedding: &'a [f32],
    norm: f32,
    words_lower: Vec<String>,
}

impl<'a> Query<'a> {
    fn new(text: &'a str, embedding: &'a [f32]) -> Self {
        Self {
            text,
            embedding,
            norm: vector_norm(embedding),
            words_lower: text
                .split_whitespace()
                .map(|word| word.to_lowercase())
                .collect(),
        }
    }

    fn score<'c>(&self, prepared: &'c PreparedChunk) -> Result<ScoredChunk<'c>> {
        let chunk = &prepared.embedded;
        if chunk.embedding.len() != self.embedding.len() || !prepared.norm.is_finite() {
            return Err(AppError::InvalidEmbeddings(format!(
                "the embedding for {}:{} is not {} finite values",
                chunk.session,
                chunk.chunk_index,
                self.embedding.len()
            )));
        }
        let semantic = cosine(self.embedding, self.norm, &chunk.embedding, prepared.norm);
        let word_match = word_match(&self.words_lower, &prepared.text_lower);
        Ok(ScoredChunk {
            chunk,
            score: SemanticScoreBreakdown::new(semantic, word_match),
        })
    }

    fn hit(&self, scored: ScoredChunk<'_>) -> SemanticHit {
        let chunk = scored.chunk;
        let quality = quality_for_score(scored.score.hybrid);
        SemanticHit::new(
            scored.score,
            SemanticExplanation {
                quality,
                quality_label: quality.label(),
                matched_terms: matched_terms(self.text, &chunk.text),
                evidence_preview: evidence_preview(&chunk.text),
                rationale_kind: rationale_kind(scored.score),
                chunk: SemanticChunkIdentity {
                    conversation_index: chunk.conversation_index,
                    source: chunk.source,
                    session: chunk.session.clone(),
                    chunk_index: chunk.chunk_index,
                    message_range: chunk.message_range,
                },
            },
        )
    }

    fn hits(
        &self,
        scored: &[ScoredChunk<'_>],
        cancellation: &SemanticCancellationToken,
    ) -> Result<Vec<SemanticHit>> {
        scored
            .iter()
            .map(|scored| {
                if cancellation.is_cancelled() {
                    return Err(AppError::SemanticSearchCancelled);
                }
                Ok(self.hit(*scored))
            })
            .collect()
    }
}

/// Score every chunk, then build hits only for each conversation's best
/// chunk and for the chunk hits `chunk_hits` asks for.
pub fn rank_prepared(
    query: &str,
    query_embedding: &[f32],
    chunks: &[&PreparedChunk],
    chunk_hits: ChunkHits,
    cancellation: &SemanticCancellationToken,
) -> Result<RankedHits> {
    if query_embedding.is_empty() || query_embedding.iter().any(|value| !value.is_finite()) {
        return Err(AppError::InvalidEmbeddings(
            "the query embedding is empty or holds a non-finite value".to_string(),
        ));
    }
    let query = Query::new(query, query_embedding);
    let mut scored = Vec::with_capacity(chunks.len());
    for chunk in chunks {
        if cancellation.is_cancelled() {
            return Err(AppError::SemanticSearchCancelled);
        }
        scored.push(query.score(chunk)?);
    }

    let winners = best_chunk_per_conversation(&scored);
    let chunk_hits = match chunk_hits {
        ChunkHits::None => Vec::new(),
        ChunkHits::TopConversations(count) => {
            query.hits(&top_evidence_chunks(&scored, &winners, count), cancellation)?
        }
    };
    Ok(RankedHits {
        conversations: query.hits(&winners, cancellation)?,
        chunks: chunk_hits,
    })
}

/// Rank chunks that were not indexed, preparing each first.
pub fn rank_chunks(
    query: &str,
    query_embedding: &[f32],
    chunks: &[EmbeddedChunk],
    cancellation: &SemanticCancellationToken,
) -> Result<Vec<SemanticHit>> {
    let prepared = chunks
        .iter()
        .cloned()
        .map(PreparedChunk::new)
        .collect::<Vec<_>>();
    let prepared = prepared.iter().collect::<Vec<_>>();
    let ranked = rank_prepared(
        query,
        query_embedding,
        &prepared,
        ChunkHits::None,
        cancellation,
    )?;
    Ok(ranked.conversations)
}

fn best_chunk_per_conversation<'a>(scored: &[ScoredChunk<'a>]) -> Vec<ScoredChunk<'a>> {
    let mut best = HashMap::<usize, ScoredChunk<'a>>::new();
    for candidate in scored {
        best.entry(candidate.chunk.conversation_index)
            .and_modify(|current| {
                if compare_scored(candidate, current).is_lt() {
                    *current = *candidate;
                }
            })
            .or_insert(*candidate);
    }
    let mut winners = best.into_values().collect::<Vec<_>>();
    winners.sort_by(compare_scored);
    winners
}

/// The evidence chunks of the first `count` conversations in `winners` with
/// evidence, best first.
fn top_evidence_chunks<'a>(
    scored: &[ScoredChunk<'a>],
    winners: &[ScoredChunk<'a>],
    count: usize,
) -> Vec<ScoredChunk<'a>> {
    let is_evidence =
        |scored: &ScoredChunk<'_>| scored.chunk.source != SemanticChunkSource::AgentRoute;
    let with_evidence = scored
        .iter()
        .filter(|scored| is_evidence(scored))
        .map(|scored| scored.chunk.conversation_index)
        .collect::<HashSet<_>>();
    let selected = winners
        .iter()
        .map(|winner| winner.chunk.conversation_index)
        .filter(|index| with_evidence.contains(index))
        .take(count)
        .collect::<HashSet<_>>();
    let mut evidence = scored
        .iter()
        .filter(|scored| is_evidence(scored) && selected.contains(&scored.chunk.conversation_index))
        .copied()
        .collect::<Vec<_>>();
    evidence.sort_by(compare_scored);
    evidence
}

fn compare_scored(a: &ScoredChunk<'_>, b: &ScoredChunk<'_>) -> Ordering {
    b.score
        .hybrid
        .total_cmp(&a.score.hybrid)
        .then_with(|| b.score.semantic.total_cmp(&a.score.semantic))
        .then_with(|| b.score.word_match.total_cmp(&a.score.word_match))
        .then_with(|| a.chunk.conversation_index.cmp(&b.chunk.conversation_index))
        .then_with(|| a.chunk.session.cmp(&b.chunk.session))
        .then_with(|| a.chunk.chunk_index.cmp(&b.chunk.chunk_index))
}

fn quality_for_score(hybrid_score: f32) -> SemanticQuality {
    if hybrid_score >= 0.85 {
        SemanticQuality::Strong
    } else if hybrid_score >= 0.65 {
        SemanticQuality::Good
    } else if hybrid_score >= 0.35 {
        SemanticQuality::Fair
    } else {
        SemanticQuality::Weak
    }
}

fn rationale_kind(score_breakdown: SemanticScoreBreakdown) -> SemanticRationaleKind {
    if quality_for_score(score_breakdown.hybrid) == SemanticQuality::Weak {
        SemanticRationaleKind::WeakMatch
    } else if score_breakdown.word_match_bonus() > 0.0 {
        SemanticRationaleKind::LexicalBoosted
    } else {
        SemanticRationaleKind::SemanticOnly
    }
}

fn vector_norm(values: &[f32]) -> f32 {
    values
        .iter()
        .fold(0.0_f32, |sum, value| sum + value * value)
        .sqrt()
}

fn cosine(a: &[f32], norm_a: f32, b: &[f32], norm_b: f32) -> f32 {
    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }
    let dot = a.iter().zip(b).fold(0.0_f32, |dot, (x, y)| dot + x * y);
    dot / (norm_a * norm_b)
}

/// The share of the query's words found in `text_lower`, 0 to 1.
fn word_match(query_words_lower: &[String], text_lower: &str) -> f32 {
    if query_words_lower.is_empty() {
        return 0.0;
    }
    let matched = query_words_lower
        .iter()
        .filter(|word| text_lower.contains(word.as_str()))
        .count();
    matched as f32 / query_words_lower.len() as f32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::refs::MessageRange;

    fn embedded(
        session: &str,
        conversation_index: usize,
        chunk_index: usize,
        text: &str,
        embedding: Vec<f32>,
    ) -> EmbeddedChunk {
        EmbeddedChunk {
            conversation_index,
            source: crate::semantic::types::SemanticChunkSource::VisibleDialogue,
            session: session.to_string(),
            chunk_index,
            text: text.to_string(),
            message_range: crate::agent::refs::MessageRange::single(chunk_index + 1),
            embedding,
        }
    }

    fn embedded_from(
        conversation_index: usize,
        source: SemanticChunkSource,
        chunk_index: usize,
        embedding: Vec<f32>,
    ) -> EmbeddedChunk {
        EmbeddedChunk {
            source,
            ..embedded(
                &format!("session-{conversation_index}"),
                conversation_index,
                chunk_index,
                "text",
                embedding,
            )
        }
    }

    fn rank(
        chunks: &[EmbeddedChunk],
        query_embedding: &[f32],
        chunk_hits: ChunkHits,
    ) -> RankedHits {
        let prepared = chunks
            .iter()
            .cloned()
            .map(PreparedChunk::new)
            .collect::<Vec<_>>();
        rank_prepared(
            "query",
            query_embedding,
            &prepared.iter().collect::<Vec<_>>(),
            chunk_hits,
            &SemanticCancellationToken::new(),
        )
        .unwrap()
    }

    fn conversation_indices(hits: &[SemanticHit]) -> Vec<usize> {
        hits.iter().map(|hit| hit.conversation_index).collect()
    }

    #[test]
    fn an_invalid_embedding_is_an_error_not_a_ranking() {
        let valid = embedded("session-a", 0, 0, "text", vec![1.0, 0.0]);
        let non_finite = embedded("session-a", 0, 0, "text", vec![f32::INFINITY, 0.0]);
        let longer = embedded("session-a", 0, 0, "text", vec![1.0, 0.0, 0.0]);
        let cases = [
            (valid.clone(), vec![f32::NAN, 0.0]),
            (valid, Vec::new()),
            (non_finite, vec![1.0, 0.0]),
            (longer, vec![1.0, 0.0]),
        ];

        for (chunk, query_embedding) in cases {
            let ranked = rank_chunks(
                "text",
                &query_embedding,
                std::slice::from_ref(&chunk),
                &SemanticCancellationToken::new(),
            );

            assert!(
                matches!(ranked, Err(AppError::InvalidEmbeddings(_))),
                "{:?} against {query_embedding:?}",
                chunk.embedding
            );
        }
    }

    #[test]
    fn chunk_hits_are_built_only_when_asked_for() {
        let chunks = vec![
            embedded("session-a", 0, 0, "rust cache", vec![1.0, 0.0]),
            embedded("session-b", 1, 0, "rust", vec![0.5, 0.5]),
        ];

        let ranked = rank(&chunks, &[1.0, 0.0], ChunkHits::None);

        assert_eq!(conversation_indices(&ranked.conversations), vec![0, 1]);
        assert!(ranked.chunks.is_empty());
    }

    #[test]
    fn chunk_hits_cover_every_chunk_of_the_top_conversations_only() {
        let chunks = vec![
            embedded("session-a", 0, 0, "first", vec![1.0, 0.0]),
            embedded("session-a", 0, 1, "second", vec![0.0, 1.0]),
            embedded("session-b", 1, 0, "other", vec![0.5, 0.5]),
        ];

        let ranked = rank(&chunks, &[1.0, 0.0], ChunkHits::TopConversations(1));

        assert_eq!(
            ranked
                .chunks
                .iter()
                .map(|hit| (hit.conversation_index, hit.chunk_index))
                .collect::<Vec<_>>(),
            vec![(0, 0), (0, 1)]
        );
    }

    #[test]
    fn an_agent_route_ranks_its_conversation_but_is_not_a_chunk_hit() {
        let chunks = vec![
            embedded_from(0, SemanticChunkSource::AgentRoute, 0, vec![1.0, 0.0]),
            embedded_from(0, SemanticChunkSource::VisibleDialogue, 0, vec![0.0, 1.0]),
            embedded_from(1, SemanticChunkSource::VisibleDialogue, 0, vec![0.5, 0.5]),
        ];

        let ranked = rank(&chunks, &[1.0, 0.0], ChunkHits::TopConversations(2));

        assert_eq!(conversation_indices(&ranked.conversations), vec![0, 1]);
        assert_eq!(
            ranked.conversations[0].explanation.chunk.source,
            SemanticChunkSource::AgentRoute
        );
        assert_eq!(conversation_indices(&ranked.chunks), vec![1, 0]);
        assert!(
            ranked
                .chunks
                .iter()
                .all(|hit| hit.explanation.chunk.source != SemanticChunkSource::AgentRoute)
        );
    }

    #[test]
    fn top_conversations_count_only_conversations_with_evidence() {
        let chunks = vec![
            embedded_from(0, SemanticChunkSource::AgentRoute, 0, vec![1.0, 0.0]),
            embedded_from(1, SemanticChunkSource::VisibleDialogue, 0, vec![0.8, 0.2]),
            embedded_from(2, SemanticChunkSource::VisibleDialogue, 0, vec![0.5, 0.5]),
        ];

        let ranked = rank(&chunks, &[1.0, 0.0], ChunkHits::TopConversations(1));

        assert_eq!(conversation_indices(&ranked.conversations), vec![0, 1, 2]);
        assert_eq!(conversation_indices(&ranked.chunks), vec![1]);
    }

    const FIXTURE_QUERY_EMBEDDING: [f32; 4] = [0.5, 1.0, 0.0, -0.5];

    /// A corpus with exact score ties within and across conversations,
    /// sessions shared by several conversations, a route tied with its
    /// conversation's first dialogue chunk, and a route-only conversation
    /// that ranks first.
    fn tied_fixture_corpus() -> Vec<EmbeddedChunk> {
        const VALUES: [f32; 5] = [-1.0, -0.5, 0.0, 0.5, 1.0];
        const WORDS: [&str; 5] = ["alpha", "beta", "gamma", "Delta", "zeta"];
        let mut seed = 7_u32;
        let mut next = move |modulus: u32| {
            seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            ((seed >> 16) % modulus) as usize
        };
        let mut chunks = Vec::new();
        for conversation_index in 0..40 {
            let session = format!("session-{}", conversation_index % 7);
            let mut dialogue = Vec::new();
            let dialogue_count = if conversation_index == 3 {
                0
            } else {
                1 + next(4)
            };
            for chunk_index in 0..dialogue_count {
                let embedding = (0..4).map(|_| VALUES[next(5)]).collect();
                let text = (0..=next(3))
                    .map(|_| WORDS[next(5)])
                    .collect::<Vec<_>>()
                    .join(" ");
                dialogue.push(EmbeddedChunk {
                    conversation_index,
                    source: SemanticChunkSource::VisibleDialogue,
                    session: session.clone(),
                    chunk_index,
                    text,
                    message_range: MessageRange::single(chunk_index + 1),
                    embedding,
                });
            }
            let route = match dialogue.first() {
                None => Some(EmbeddedChunk {
                    conversation_index,
                    source: SemanticChunkSource::AgentRoute,
                    session: session.clone(),
                    chunk_index: 0,
                    text: "route".to_string(),
                    message_range: MessageRange::single(1),
                    embedding: FIXTURE_QUERY_EMBEDDING.to_vec(),
                }),
                Some(first) if conversation_index % 5 == 0 => Some(EmbeddedChunk {
                    source: SemanticChunkSource::AgentRoute,
                    ..first.clone()
                }),
                _ if conversation_index % 3 == 0 => Some(EmbeddedChunk {
                    conversation_index,
                    source: SemanticChunkSource::AgentRoute,
                    session: session.clone(),
                    chunk_index: 0,
                    text: WORDS[next(5)].to_string(),
                    message_range: MessageRange::single(1),
                    embedding: (0..4).map(|_| VALUES[next(5)]).collect(),
                }),
                _ => None,
            };
            chunks.extend(dialogue);
            chunks.extend(route);
        }
        chunks
    }

    /// The ranking `rank_prepared` replaced: score and explain every chunk,
    /// sort them all, and keep each conversation's first.
    fn reference_chunk_hits(
        query: &str,
        query_embedding: &[f32],
        chunks: &[EmbeddedChunk],
    ) -> Vec<SemanticHit> {
        let explainer = Query::new(query, query_embedding);
        let mut hits = chunks
            .iter()
            .map(|chunk| {
                let semantic = reference_cosine(query_embedding, &chunk.embedding);
                let word_match = reference_word_match(query, &chunk.text);
                let score = SemanticScoreBreakdown {
                    hybrid: semantic + 0.2 * word_match,
                    semantic,
                    word_match,
                };
                explainer.hit(ScoredChunk { chunk, score })
            })
            .collect::<Vec<_>>();
        hits.sort_by(reference_compare);
        hits
    }

    fn reference_conversation_hits(chunk_hits: &[SemanticHit]) -> Vec<SemanticHit> {
        let mut seen = HashMap::new();
        for hit in chunk_hits {
            seen.entry(hit.conversation_index)
                .or_insert_with(|| hit.clone());
        }
        let mut hits = seen.into_values().collect::<Vec<_>>();
        hits.sort_by(reference_compare);
        hits
    }

    fn reference_compare(a: &SemanticHit, b: &SemanticHit) -> Ordering {
        b.score_breakdown
            .hybrid
            .total_cmp(&a.score_breakdown.hybrid)
            .then_with(|| {
                b.score_breakdown
                    .semantic
                    .total_cmp(&a.score_breakdown.semantic)
            })
            .then_with(|| {
                b.score_breakdown
                    .word_match
                    .total_cmp(&a.score_breakdown.word_match)
            })
            .then_with(|| a.conversation_index.cmp(&b.conversation_index))
            .then_with(|| a.session.cmp(&b.session))
            .then_with(|| a.chunk_index.cmp(&b.chunk_index))
    }

    fn reference_cosine(a: &[f32], b: &[f32]) -> f32 {
        let mut dot = 0.0;
        let mut norm_a = 0.0;
        let mut norm_b = 0.0;
        for (x, y) in a.iter().zip(b) {
            dot += x * y;
            norm_a += x * x;
            norm_b += y * y;
        }
        if norm_a == 0.0 || norm_b == 0.0 {
            0.0
        } else {
            dot / (norm_a.sqrt() * norm_b.sqrt())
        }
    }

    fn reference_word_match(query: &str, text: &str) -> f32 {
        let query_words = query
            .split_whitespace()
            .map(|word| word.to_lowercase())
            .collect::<Vec<_>>();
        if query_words.is_empty() {
            return 0.0;
        }
        let text = text.to_lowercase();
        let matches = query_words
            .iter()
            .filter(|word| text.contains(word.as_str()))
            .count();
        matches as f32 / query_words.len() as f32
    }

    #[test]
    fn ranking_matches_scoring_and_explaining_every_chunk() {
        let chunks = tied_fixture_corpus();
        let query = "alpha DELTA omega";
        let query_embedding = FIXTURE_QUERY_EMBEDDING;
        let reference = reference_chunk_hits(query, &query_embedding, &chunks);
        let reference_conversations = reference_conversation_hits(&reference);
        let prepared = chunks
            .iter()
            .cloned()
            .map(PreparedChunk::new)
            .collect::<Vec<_>>();
        let prepared = prepared.iter().collect::<Vec<_>>();

        for count in [0, 1, 5, 100] {
            let ranked = rank_prepared(
                query,
                &query_embedding,
                &prepared,
                ChunkHits::TopConversations(count),
                &SemanticCancellationToken::new(),
            )
            .unwrap();
            let with_evidence = reference
                .iter()
                .filter(|hit| hit.explanation.chunk.source != SemanticChunkSource::AgentRoute)
                .map(|hit| hit.conversation_index)
                .collect::<HashSet<_>>();
            let top = reference_conversations
                .iter()
                .map(|hit| hit.conversation_index)
                .filter(|index| with_evidence.contains(index))
                .take(count)
                .collect::<HashSet<_>>();
            let expected_chunks = reference
                .iter()
                .filter(|hit| {
                    hit.explanation.chunk.source != SemanticChunkSource::AgentRoute
                        && top.contains(&hit.conversation_index)
                })
                .cloned()
                .collect::<Vec<_>>();

            assert_eq!(ranked.conversations, reference_conversations);
            assert_eq!(ranked.chunks, expected_chunks, "top {count} conversations");
        }
    }

    #[test]
    fn ranking_keeps_best_chunk_per_session() {
        let chunks = vec![
            embedded("session-a", 0, 0, "rust cache", vec![1.0, 0.0]),
            embedded("session-a", 0, 1, "unrelated", vec![0.0, 1.0]),
            embedded("session-b", 1, 0, "rust", vec![0.5, 0.5]),
        ];

        let hits = rank_chunks(
            "rust cache",
            &[1.0, 0.0],
            &chunks,
            &SemanticCancellationToken::new(),
        )
        .unwrap();

        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].session, "session-a");
        assert_eq!(hits[0].snippet, "rust cache");
        assert!(hits[0].semantic_score > hits[1].semantic_score);
        assert_eq!(hits[0].lexical_score, 0.2);
    }

    #[test]
    fn ranking_preserves_message_range() {
        let chunks = vec![embedded(
            "session-a",
            0,
            3,
            "range evidence",
            vec![1.0, 0.0],
        )];

        let hits = rank_chunks(
            "range",
            &[1.0, 0.0],
            &chunks,
            &SemanticCancellationToken::new(),
        )
        .unwrap();

        assert_eq!(
            hits[0].message_range,
            crate::agent::refs::MessageRange::single(4)
        );
        assert_eq!(
            hits[0].explanation.chunk.message_range,
            crate::agent::refs::MessageRange::single(4)
        );
    }

    #[test]
    fn ranking_uses_explicit_query_embedding() {
        let chunks = vec![
            embedded("session-a", 0, 0, "same words", vec![0.0, 1.0]),
            embedded("session-b", 1, 0, "same words", vec![1.0, 0.0]),
        ];

        let hits = rank_chunks(
            "same words",
            &[0.0, 1.0],
            &chunks,
            &SemanticCancellationToken::new(),
        )
        .unwrap();

        assert_eq!(hits[0].session, "session-a");
        assert_eq!(hits[0].semantic_score, 1.0);
    }

    #[test]
    fn empty_query_has_no_lexical_boost() {
        let chunks = vec![
            embedded("session-a", 0, 0, "same words", vec![0.0, 1.0]),
            embedded("session-b", 1, 0, "same words", vec![1.0, 0.0]),
        ];

        let hits = rank_chunks(
            "   ",
            &[0.0, 1.0],
            &chunks,
            &SemanticCancellationToken::new(),
        )
        .unwrap();

        assert_eq!(hits[0].session, "session-a");
        assert!(hits.iter().all(|hit| hit.lexical_score == 0.0));
    }

    #[test]
    fn semantic_only_match_records_no_lexical_terms() {
        let chunks = vec![embedded(
            "session-a",
            0,
            0,
            "vector-only evidence",
            vec![1.0, 0.0],
        )];

        let hits = rank_chunks(
            "unrelated",
            &[1.0, 0.0],
            &chunks,
            &SemanticCancellationToken::new(),
        )
        .unwrap();
        let explanation = &hits[0].explanation;

        assert_eq!(hits[0].lexical_score, 0.0);
        assert_eq!(
            explanation.rationale_kind,
            SemanticRationaleKind::SemanticOnly
        );
        assert!(explanation.matched_terms.is_empty());
    }

    #[test]
    fn lexical_overlap_contributes_to_hybrid_ranking() {
        let chunks = vec![
            embedded("session-a", 0, 0, "unrelated", vec![1.0, 0.0]),
            embedded("session-b", 1, 0, "rust cache", vec![1.0, 0.0]),
        ];

        let hits = rank_chunks(
            "rust cache",
            &[1.0, 0.0],
            &chunks,
            &SemanticCancellationToken::new(),
        )
        .unwrap();

        assert_eq!(hits[0].session, "session-b");
        assert!(hits[0].lexical_score > hits[1].lexical_score);
        assert!(hits[0].hybrid_score > hits[1].hybrid_score);
        assert_eq!(hits[0].score_breakdown.word_match, 1.0);
        assert_eq!(hits[1].score_breakdown.word_match, 0.0);
        assert_eq!(
            hits[0].explanation.rationale_kind,
            SemanticRationaleKind::LexicalBoosted
        );
    }

    #[test]
    fn word_match_is_the_share_of_query_words_and_its_bonus_is_weighted() {
        let chunks = vec![embedded("session", 0, 0, "rust tooling", vec![1.0, 0.0])];

        let hits = rank_chunks(
            "rust cache",
            &[1.0, 0.0],
            &chunks,
            &SemanticCancellationToken::new(),
        )
        .unwrap();

        assert_eq!(hits[0].score_breakdown.word_match, 0.5);
        assert_eq!(hits[0].score_breakdown.word_match_bonus(), 0.1);
    }

    #[test]
    fn ranking_keeps_copied_sessions_separate() {
        let chunks = vec![
            embedded("session", 0, 0, "same words", vec![1.0, 0.0]),
            embedded("session", 1, 0, "same words", vec![1.0, 0.0]),
        ];

        let hits = rank_chunks(
            "same words",
            &[1.0, 0.0],
            &chunks,
            &SemanticCancellationToken::new(),
        )
        .unwrap();

        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].conversation_index, 0);
        assert_eq!(hits[1].conversation_index, 1);
    }

    #[test]
    fn ranking_uses_stable_tiebreaks_for_sessions_and_chunks() {
        let chunks = vec![
            embedded("session-b", 1, 0, "same words", vec![1.0, 0.0]),
            embedded("session-a", 0, 1, "same words", vec![1.0, 0.0]),
            embedded("session-a", 0, 0, "same words", vec![1.0, 0.0]),
        ];

        let hits = rank_chunks(
            "same words",
            &[1.0, 0.0],
            &chunks,
            &SemanticCancellationToken::new(),
        )
        .unwrap();

        assert_eq!(hits[0].conversation_index, 0);
        assert_eq!(hits[0].chunk_index, 0);
        assert_eq!(hits[1].conversation_index, 1);
    }

    #[test]
    fn score_breakdown_mirrors_compatibility_fields() {
        let chunks = vec![embedded("session-a", 0, 0, "rust cache", vec![1.0, 0.0])];

        let hits = rank_chunks(
            "rust cache",
            &[1.0, 0.0],
            &chunks,
            &SemanticCancellationToken::new(),
        )
        .unwrap();
        let hit = &hits[0];

        assert_eq!(hit.score_breakdown.hybrid, hit.hybrid_score);
        assert_eq!(hit.score_breakdown.semantic, hit.semantic_score);
        assert_eq!(hit.score_breakdown.word_match_bonus(), hit.lexical_score);
        assert_eq!(hit.snippet, hit.explanation.evidence_preview);
    }

    #[test]
    fn explanation_records_matched_terms_in_query_order() {
        let chunks = vec![embedded(
            "session-a",
            0,
            0,
            "The audio_generation cache uses Rust code",
            vec![1.0, 0.0],
        )];

        let hits = rank_chunks(
            "rust audio-generation audio",
            &[1.0, 0.0],
            &chunks,
            &SemanticCancellationToken::new(),
        )
        .unwrap();

        assert_eq!(
            hits[0].explanation.matched_terms,
            vec![
                "rust".to_string(),
                "audio".to_string(),
                "generation".to_string()
            ]
        );
    }

    #[test]
    fn explanation_records_cjk_matched_terms_in_query_order() {
        let chunks = vec![embedded(
            "session-a",
            0,
            0,
            "日本語の検索と意味検索について",
            vec![1.0, 0.0],
        )];

        let hits = rank_chunks(
            "検索 日本語",
            &[1.0, 0.0],
            &chunks,
            &SemanticCancellationToken::new(),
        )
        .unwrap();

        assert_eq!(
            hits[0].explanation.matched_terms,
            vec!["検索".to_string(), "日本語".to_string()]
        );
    }

    #[test]
    fn explanation_assigns_quality_and_rationale_deterministically() {
        assert_eq!(quality_for_score(0.85), SemanticQuality::Strong);
        assert_eq!(quality_for_score(0.65), SemanticQuality::Good);
        assert_eq!(quality_for_score(0.35), SemanticQuality::Fair);
        assert_eq!(quality_for_score(0.349), SemanticQuality::Weak);
        let chunks = vec![embedded("session-a", 0, 0, "semantic text", vec![1.0, 0.0])];
        let hits = rank_chunks(
            "semantic",
            &[1.0, 0.0],
            &chunks,
            &SemanticCancellationToken::new(),
        )
        .unwrap();

        assert_eq!(hits[0].explanation.quality_label, "strong");
        assert_eq!(SemanticQuality::Good.label(), "good");
        assert_eq!(SemanticQuality::Fair.label(), "fair");
        assert_eq!(SemanticQuality::Weak.label(), "weak");
        assert_eq!(
            rationale_kind(SemanticScoreBreakdown::new(0.0, 1.0)),
            SemanticRationaleKind::WeakMatch
        );
        assert_eq!(
            rationale_kind(SemanticScoreBreakdown::new(0.5, 1.0)),
            SemanticRationaleKind::LexicalBoosted
        );
        assert_eq!(
            rationale_kind(SemanticScoreBreakdown::new(0.7, 0.0)),
            SemanticRationaleKind::SemanticOnly
        );
    }

    #[test]
    fn explanation_uses_sanitized_evidence_preview() {
        let chunks = vec![embedded(
            "session-a",
            0,
            0,
            "alpha\n<system-reminder>hidden</system-reminder>\tVec<T> x < y",
            vec![1.0, 0.0],
        )];

        let hits = rank_chunks(
            "alpha",
            &[1.0, 0.0],
            &chunks,
            &SemanticCancellationToken::new(),
        )
        .unwrap();

        assert_eq!(hits[0].explanation.evidence_preview, "alpha Vec<T> x < y");
        assert_eq!(hits[0].snippet, "alpha Vec<T> x < y");
    }
}
