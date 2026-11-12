//! The semantic search's results for a query of quoted phrases alone: the
//! lexical search's hits, each marked an exact match, with the first range
//! of its lexical evidence as its preview.

use super::SemanticResultMetadata;
use crate::history::Conversation;
use crate::search::LexicalEvidence;
use crate::search::query::ParsedQuery;
use crate::semantic::types::{
    SemanticChunkIdentity, SemanticChunkSource, SemanticExplanation, SemanticQuality,
    SemanticRationaleKind, SemanticScoreBreakdown,
};
use std::collections::HashMap;

/// The metadata of each of `hits`, indices into `conversations`, for the
/// quoted-only query `parsed`, previewing each hit from its entry of
/// `evidence`.
pub(super) fn exact_literal_results(
    conversations: &[Conversation],
    evidence: &HashMap<usize, LexicalEvidence>,
    hits: &[usize],
    parsed: &ParsedQuery,
) -> HashMap<usize, SemanticResultMetadata> {
    hits.iter()
        .map(|&index| {
            let conversation = &conversations[index];
            let preview = evidence
                .get(&index)
                .and_then(|evidence| first_range_window(conversation, evidence))
                .unwrap_or_else(|| conversation.preview.clone());
            (
                index,
                exact_literal_metadata(conversation, index, parsed, preview),
            )
        })
        .collect()
}

/// The first of `evidence`'s ranges, with up to 80 characters of its texts
/// on each side; `None` without a range.
fn first_range_window(conversation: &Conversation, evidence: &LexicalEvidence) -> Option<String> {
    const CONTEXT_CHARS: usize = 80;
    let &(start, end) = evidence.context_ranges.first()?;
    let texts = evidence.texts(conversation);
    let prefix_start = texts.start_before(start, CONTEXT_CHARS);
    let suffix_end = texts.start_after(end, CONTEXT_CHARS);
    let mut preview = String::new();
    if prefix_start > 0 {
        preview.push('…');
    }
    preview.push_str(texts.slice(prefix_start..suffix_end).trim());
    if suffix_end < texts.len() {
        preview.push('…');
    }
    Some(preview)
}

/// `conversation_index` is the conversation's position in the corpus. The
/// row's own `index` can name its position in an earlier corpus version,
/// because a refresh shares unchanged rows between versions.
fn exact_literal_metadata(
    conversation: &Conversation,
    conversation_index: usize,
    parsed: &ParsedQuery,
    evidence_preview: String,
) -> SemanticResultMetadata {
    SemanticResultMetadata {
        score_breakdown: SemanticScoreBreakdown::new(0.0, 0.0),
        explanation: SemanticExplanation {
            quality: SemanticQuality::Strong,
            quality_label: "exact",
            matched_terms: parsed
                .literals()
                .iter()
                .map(|literal| literal.text().to_string())
                .collect(),
            evidence_preview,
            rationale_kind: SemanticRationaleKind::LexicalBoosted,
            chunk: SemanticChunkIdentity {
                conversation_index,
                source: SemanticChunkSource::VisibleDialogue,
                session: conversation.session_id.clone(),
                chunk_index: 0,
                message_range: crate::agent::refs::MessageRange::single(1),
            },
        },
    }
}
