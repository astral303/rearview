//! The semantic search's results for a query of quoted phrases alone: the
//! lexical search's hits, each marked an exact match, with the first match
//! of a phrase as its evidence. The search worker builds them from the
//! texts it holds in memory.

use super::SemanticResultMetadata;
use crate::history::Conversation;
use crate::search::SearchTextsInMemory;
use crate::search::literal::Literal;
use crate::search::query::ParsedQuery;
use crate::search::separated_texts::SeparatedTexts;
use crate::semantic::types::{
    SemanticChunkIdentity, SemanticChunkSource, SemanticExplanation, SemanticQuality,
    SemanticRationaleKind, SemanticScoreBreakdown,
};
use std::collections::HashMap;
use std::sync::Arc;

/// The metadata of each of `hits`, indices into `conversations`, for the
/// quoted-only query `parsed`.
pub(super) fn exact_literal_results(
    conversations: &[Arc<Conversation>],
    texts: &SearchTextsInMemory,
    hits: &[usize],
    parsed: &ParsedQuery,
) -> HashMap<usize, SemanticResultMetadata> {
    hits.iter()
        .map(|&index| {
            let conversation = &conversations[index];
            let body = texts
                .get(conversation)
                .map_or(conversation.full_text.as_str(), |texts| &**texts.joined());
            let preview = literal_evidence_preview(conversation, body, parsed);
            (
                index,
                exact_literal_metadata(conversation, index, parsed, preview),
            )
        })
        .collect()
}

/// The first match of one of `parsed`'s phrases in `conversation`'s title
/// and summary, then `body`, its search texts, then its project name; the
/// row's preview when none matches.
fn literal_evidence_preview(
    conversation: &Conversation,
    body: &str,
    parsed: &ParsedQuery,
) -> String {
    let texts = SeparatedTexts::new(conversation.title_and_summary_texts().chain([body]));
    let project = SeparatedTexts::new(conversation.project_name.as_deref());
    parsed
        .literals()
        .iter()
        .find_map(|literal| {
            first_match_window(&texts, literal).or_else(|| first_match_window(&project, literal))
        })
        .unwrap_or_else(|| conversation.preview.clone())
}

/// The first match of `literal` in one of `texts`, with up to 80
/// characters of the joined text on each side.
fn first_match_window(texts: &SeparatedTexts<'_>, literal: &Literal) -> Option<String> {
    const CONTEXT_CHARS: usize = 80;
    let (start, end) = texts.texts().find_map(|(offset, text)| {
        let &(start, end) = literal.match_ranges(text).first()?;
        Some((offset + start, offset + end))
    })?;
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

/// `conversation_index` is the conversation's position in the corpus: a
/// refresh shares unchanged rows between corpus versions, so the row's own
/// `index` can name its position in an earlier one.
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
