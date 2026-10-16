use crate::agent::diagnostic::{AgentWarning, format_warning_records};
use crate::agent::ordered_scan::{ScanStep, scan_in_order};
use crate::agent::refs::{AgentConversationKey, MessageRange, ResolvedConversation};
use crate::agent::retrieval::{
    AgentHitRenderOptions, AgentHitSource, AgentRetrievalOptions, AgentSearchHit as RetrievalHit,
    AgentTranscriptSearchTarget, format_evidence_preview, read_range_for_focus,
    retrieve_agent_hits_for_target,
};
use crate::agent::sanitize::sanitize_agent_text;
use crate::agent::time_span::{TimeSpan, time_atoms};
use crate::agent::transcript::{AgentMessage, AgentTranscript};
use crate::error::{AppError, Result};
use crate::history::Conversation;
use crate::search::mode::{SearchMode, SearchModeResolution, resolve_search_mode};
use crate::search::query::ParsedQuery;
use crate::semantic::rank::RankedHits;
use crate::semantic::types::{ChunkHits, SemanticChunkSource, SemanticHit, SemanticScoreBreakdown};
use crate::time_filter::TimeFilter;
use std::cmp::Ordering;
use std::collections::HashMap;

const SHORTLIST_MIN: usize = 50;
const SHORTLIST_FACTOR: usize = 5;
const SHORTLIST_MAX: usize = 500;
const MODALITY_CANDIDATE_MIN: usize = 50;
const MODALITY_CANDIDATE_FACTOR: usize = 8;
const MODALITY_CANDIDATE_MAX: usize = 1_000;
const RRF_K: f64 = 60.0;
const AGENT_SEARCH_TITLE_CHARS: usize = 240;
const AGENT_SEARCH_HIT_CHARS: usize = 500;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AgentSearchScope {
    Global,
    Local,
}

#[derive(Clone, Debug)]
pub struct AgentSearchRequest {
    pub query: String,
    pub top: usize,
    pub cli_mode: Option<SearchMode>,
    pub config_mode: Option<SearchMode>,
    pub tui_semantic_search: Option<bool>,
    pub flat: bool,
    pub hits_per_conversation: usize,
    pub retrieval_hits_per_conversation: Option<usize>,
    pub all_hits: bool,
    pub budget: Option<usize>,
    /// Matching sessions to pass over, in rank order, before the first one
    /// searched: `--offset`.
    pub session_offset: usize,
    /// Hits come only from the messages inside it.
    pub window: TimeFilter,
}

impl AgentSearchRequest {
    /// The mode the search runs in. A hybrid search with an offset runs
    /// lexical: only its lexical half pages, and its `continue search` record
    /// names a lexical search.
    pub fn mode(&self) -> SearchMode {
        match effective_agent_mode(
            &self.query,
            self.cli_mode,
            self.config_mode,
            self.tui_semantic_search,
        ) {
            SearchMode::Hybrid if self.session_offset > 0 => SearchMode::Lexical,
            mode => mode,
        }
    }

    /// Chunk hits for [`run_global_semantic_search`] and
    /// [`run_global_hybrid_search`]: every evidence chunk of as many
    /// top-ranked conversations as each keeps, `top` for semantic and the
    /// fusion depth for hybrid.
    pub fn semantic_chunk_hits(&self) -> ChunkHits {
        match self.mode() {
            SearchMode::Hybrid => ChunkHits::TopConversations(modality_candidate_depth(self)),
            _ => ChunkHits::TopConversations(self.top),
        }
    }
}

#[derive(Clone, Debug)]
pub struct AgentWithinRequest {
    pub query: String,
    pub top: usize,
    pub cli_mode: Option<SearchMode>,
    pub config_mode: Option<SearchMode>,
    pub tui_semantic_search: Option<bool>,
    pub budget: Option<usize>,
    /// Hits come only from the messages inside it.
    pub window: TimeFilter,
}

impl AgentWithinRequest {
    /// Chunk hits for [`run_within_search`]: every evidence chunk of its one
    /// conversation.
    pub fn semantic_chunk_hits(&self) -> ChunkHits {
        ChunkHits::TopConversations(1)
    }
}

#[derive(Clone)]
pub struct AgentConversationInput<'a> {
    pub conversation: &'a Conversation,
    pub resolved: ResolvedConversation,
    pub original_index: usize,
}

/// How far a search got through the sessions whose cached text matched its
/// query, in rank order.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SessionCoverage {
    pub matched: usize,
    /// Matching sessions passed over before the first one searched.
    pub offset: usize,
    /// Matching sessions taken after the offset, in rank order, including
    /// those counted in `failed`.
    pub searched: usize,
    /// Sessions taken but not searched: a transcript that failed to read, or
    /// a row without a key.
    pub failed: usize,
}

impl SessionCoverage {
    /// The offset of the first matching session left unsearched, when one is.
    pub fn next_offset(&self) -> Option<usize> {
        let next = self.offset + self.searched;
        (next < self.matched).then_some(next)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct AgentSearchOutput {
    pub protocol: AgentProtocolKind,
    pub target: Option<AgentConversationMetadata>,
    pub query: String,
    pub mode: SearchMode,
    pub hits: Vec<AgentOutputHit>,
    pub groups: Vec<AgentConversationGroup>,
    pub flat: bool,
    pub budget: Option<usize>,
    /// `None` for a search that opens no sessions by rank: within one
    /// conversation, or semantic.
    pub coverage: Option<SessionCoverage>,
}

/// Which transcripts a lexical search hands back after searching them.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TranscriptRetention {
    DropAfterSearch,
    /// Keep the transcripts of sessions with hits, for a caller that reads
    /// them again.
    KeepWithHits,
}

pub struct LexicalSearchRun {
    pub output: AgentSearchOutput,
    pub kept_transcripts: Vec<(AgentConversationKey, AgentTranscript)>,
    /// Time spent matching the searched transcripts, summed across threads.
    pub retrieval_time: std::time::Duration,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentConversationMetadata {
    pub project_id: String,
    pub conversation_uuid: String,
    pub conversation_ref: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AgentConversationGroup {
    pub conversation_ref: String,
    pub project_id: String,
    pub conversation_uuid: String,
    pub session: String,
    pub title: String,
    pub score: f64,
    pub total_hits: usize,
    pub hits: Vec<AgentOutputHit>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AgentProtocolKind {
    Search,
    Within,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AgentOutputHit {
    pub conversation_ref: String,
    pub project_id: String,
    pub conversation_uuid: String,
    pub session: String,
    pub anchors: Vec<String>,
    pub title: String,
    pub score: f64,
    pub evidence_score: f64,
    pub semantic_score_breakdown: Option<SemanticScoreBreakdown>,
    pub source: AgentHitKind,
    pub evidence_source: AgentHitSource,
    pub render_options: AgentHitRenderOptions,
    pub preview: String,
    pub focus_range: MessageRange,
    /// The times on the parts of the messages in `focus_range`. A semantic
    /// hit gets them with its anchors, once its transcript is read.
    pub times: Option<TimeSpan>,
    pub read_range: MessageRange,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AgentHitKind {
    Exact,
    Lexical,
    Semantic,
    Hybrid,
}

#[derive(Clone, Debug)]
struct RankedHit {
    hit: AgentOutputHit,
    lexical_rank: Option<usize>,
    semantic_rank: Option<usize>,
    exact: bool,
}

pub fn attach_transcript_metadata(
    output: &mut AgentSearchOutput,
    resolved: &ResolvedConversation,
    transcript: &AgentTranscript,
) {
    let reference = resolved.reference.canonical();
    if output.protocol == AgentProtocolKind::Within {
        output.target = Some(AgentConversationMetadata {
            project_id: resolved.key.project_id(),
            conversation_uuid: resolved.reference.uuid(),
            conversation_ref: reference.clone(),
        });
    }
    for hit in output
        .hits
        .iter_mut()
        .chain(
            output
                .groups
                .iter_mut()
                .flat_map(|group| group.hits.iter_mut()),
        )
        .filter(|hit| hit.conversation_ref == reference)
    {
        hit.project_id = resolved.key.project_id();
        hit.conversation_uuid = resolved.reference.uuid();
        hit.session = resolved.key.session_filename.clone();
        let focus = FocusMetadata::of(transcript, resolved, hit.focus_range);
        hit.anchors = focus.anchors;
        hit.times = focus.times;
    }
    for group in output
        .groups
        .iter_mut()
        .filter(|group| group.conversation_ref == reference)
    {
        group.project_id = resolved.key.project_id();
        group.conversation_uuid = resolved.reference.uuid();
        group.session = resolved.key.session_filename.clone();
    }
}

pub fn effective_agent_mode(
    query: &str,
    cli_mode: Option<SearchMode>,
    config_mode: Option<SearchMode>,
    tui_semantic_search: Option<bool>,
) -> SearchMode {
    let parsed = ParsedQuery::parse(query);
    if parsed.is_quoted_only() {
        SearchMode::Exact
    } else {
        resolve_search_mode(SearchModeResolution {
            cli_mode,
            config_mode,
            tui_semantic_search,
        })
    }
}

#[cfg(test)]
pub fn format_agent_output(output: &AgentSearchOutput) -> String {
    format_agent_output_with_warnings(output, &[])
}

pub fn format_agent_output_with_warnings(
    output: &AgentSearchOutput,
    warnings: &[AgentWarning],
) -> String {
    let protocol = match output.protocol {
        AgentProtocolKind::Search => "agent-search",
        AgentProtocolKind::Within => "agent-within",
    };
    let hits = output_hits(output);
    let (warning_count, warning_records) = format_warning_records(warnings);
    let warning_suffix = if warning_records.is_empty() {
        String::new()
    } else {
        format!(
            " warnings={warning_count} warning-records={}",
            warning_records.len()
        )
    };
    let mut rendered = if output.protocol == AgentProtocolKind::Search && !output.flat {
        format!(
            "protocol {protocol} mode={} cut=none chars={} policy=per-hit groups={} hits={}{}\n",
            mode_atom(output.mode),
            budget_atom(output.budget),
            output.groups.len(),
            hits.len(),
            warning_suffix
        )
    } else {
        format!(
            "protocol {protocol} mode={} cut=none chars={} policy=per-hit hits={}{}\n",
            mode_atom(output.mode),
            budget_atom(output.budget),
            hits.len(),
            warning_suffix
        )
    };
    rendered.push_str(&format!(
        "query text={} hits={}\n",
        crate::agent::protocol::escape_atom(&output.query),
        hits.len()
    ));
    if let Some(target) = &output.target {
        rendered.push_str(&format!(
            "conversation project={} uuid={} ref={}\n",
            crate::agent::protocol::escape_atom(&target.project_id),
            crate::agent::protocol::escape_atom(&target.conversation_uuid),
            crate::agent::protocol::escape_atom(&target.conversation_ref)
        ));
    }
    if let Some(coverage) = output.coverage {
        push_coverage_lines(&mut rendered, coverage, output.mode);
    }
    if output.protocol == AgentProtocolKind::Search && !output.flat {
        rendered.push_str(&format!("groups count={}\n", output.groups.len()));
        for (index, group) in output.groups.iter().enumerate() {
            rendered.push_str(&format!(
                "conversation rank={} project={} uuid={} ref={} score={:.6}{} hits={} total={} | {}\n",
                index + 1,
                crate::agent::protocol::escape_atom(&group.project_id),
                crate::agent::protocol::escape_atom(&group.conversation_uuid),
                crate::agent::protocol::escape_atom(&group.conversation_ref),
                group.score,
                score_breakdown_atoms(group.hits.first().and_then(|hit| hit.semantic_score_breakdown)),
                group.hits.len(),
                group.total_hits,
                protocol_snippet(&group.title, AGENT_SEARCH_TITLE_CHARS)
            ));
            for hit in &group.hits {
                push_hit_lines(&mut rendered, hit);
            }
        }
        for warning in &warning_records {
            rendered.push_str(warning);
        }
        return bound_agent_output(output, rendered, output.budget);
    }

    for hit in hits {
        rendered.push_str(&format!(
            "title project={} uuid={} ref={} | {}\n",
            crate::agent::protocol::escape_atom(&hit.project_id),
            crate::agent::protocol::escape_atom(&hit.conversation_uuid),
            crate::agent::protocol::escape_atom(&hit.conversation_ref),
            protocol_snippet(&hit.title, AGENT_SEARCH_TITLE_CHARS)
        ));
        push_hit_lines(&mut rendered, hit);
    }
    for warning in &warning_records {
        rendered.push_str(warning);
    }
    bound_agent_output(output, rendered, output.budget)
}

fn bound_agent_output(
    output: &AgentSearchOutput,
    rendered: String,
    budget: Option<usize>,
) -> String {
    let Some(budget) = budget else {
        return rendered;
    };
    if rendered.chars().count() <= budget {
        return rendered;
    }

    let lines = rendered.lines().collect::<Vec<_>>();
    let mut records = Vec::new();
    let mut index = 1;
    while index < lines.len() {
        let end = if lines[index].starts_with("hit ") && index + 1 < lines.len() {
            index + 2
        } else {
            index + 1
        };
        records.push(lines[index..end].join("\n") + "\n");
        index = end;
    }
    let recovery = if let Some(target) = &output.target {
        format!(
            "continue within ref={} action=narrow-query-or-increase-budget\n",
            crate::agent::protocol::escape_atom(&target.conversation_ref)
        )
    } else {
        "continue search action=narrow-scope-or-increase-budget\n".to_string()
    };
    while !records.is_empty() {
        let omitted = lines.len().saturating_sub(
            1 + records
                .iter()
                .map(|record| record.lines().count())
                .sum::<usize>(),
        );
        let header =
            lines[0].replace("cut=none", "cut=tail") + &format!(" omitted-lines={omitted}\n");
        let candidate = header + &records.concat() + &recovery;
        if candidate.chars().count() <= budget {
            return candidate;
        }
        records.pop();
    }
    let header = lines[0].replace("cut=none", "cut=tail")
        + &format!(" omitted-lines={}\n", lines.len().saturating_sub(1));
    let candidate = header + &recovery;
    if candidate.chars().count() <= budget {
        candidate
    } else {
        candidate.chars().take(budget).collect()
    }
}

/// The `sessions` record, and a `continue search` record naming the offset
/// that searches the matching sessions left unsearched. Hybrid's offset pages
/// through its lexical matches, so its next page is a lexical search.
fn push_coverage_lines(rendered: &mut String, coverage: SessionCoverage, mode: SearchMode) {
    rendered.push_str(&format!(
        "sessions matched={} offset={} searched={} failed={}\n",
        coverage.matched, coverage.offset, coverage.searched, coverage.failed
    ));
    if let Some(next_offset) = coverage.next_offset() {
        let next_mode = match mode {
            SearchMode::Hybrid => SearchMode::Lexical,
            mode => mode,
        };
        rendered.push_str(&format!(
            "continue search offset={next_offset} mode={} action=rerun-with-offset\n",
            mode_atom(next_mode)
        ));
    }
}

fn budget_atom(budget: Option<usize>) -> String {
    budget
        .map(|value| value.to_string())
        .unwrap_or_else(|| "none".to_string())
}

fn output_hits(output: &AgentSearchOutput) -> Vec<&AgentOutputHit> {
    if output.protocol == AgentProtocolKind::Search && !output.flat && !output.groups.is_empty() {
        output
            .groups
            .iter()
            .flat_map(|group| group.hits.iter())
            .collect()
    } else {
        output.hits.iter().collect()
    }
}

fn score_breakdown_atoms(breakdown: Option<SemanticScoreBreakdown>) -> String {
    breakdown.map_or_else(String::new, |score| {
        format!(
            " similarity={:.6} word-match={:.6}",
            score.semantic, score.word_match
        )
    })
}

fn push_hit_lines(rendered: &mut String, hit: &AgentOutputHit) {
    rendered.push_str(&format!(
        "hit project={} uuid={} ref={} anchors={} source={} score={:.6}{} focus=m{}..m{}{} | {}\n",
        crate::agent::protocol::escape_atom(&hit.project_id),
        crate::agent::protocol::escape_atom(&hit.conversation_uuid),
        crate::agent::protocol::escape_atom(&hit.conversation_ref),
        hit.anchors.join(","),
        output_source_atom(hit),
        hit.score,
        score_breakdown_atoms(hit.semantic_score_breakdown),
        hit.focus_range.start,
        hit.focus_range.end,
        time_atoms(hit.times),
        protocol_snippet(&hit.preview, AGENT_SEARCH_HIT_CHARS)
    ));
    rendered.push_str(&format!(
        "read ref={}:m{}..m{} focus=m{}..m{}{}\n",
        crate::agent::protocol::escape_atom(&hit.conversation_ref),
        hit.read_range.start,
        hit.read_range.end,
        hit.focus_range.start,
        hit.focus_range.end,
        render_option_atoms(hit.render_options)
    ));
}

fn protocol_snippet(text: &str, limit: usize) -> String {
    let sanitized = sanitize_agent_text(text);
    let normalized = sanitized.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.chars().count() <= limit {
        normalized
    } else {
        let mut snippet = normalized
            .chars()
            .take(limit.saturating_sub(3))
            .collect::<String>();
        snippet.push_str("...");
        snippet
    }
}

pub fn run_within_search(
    request: &AgentWithinRequest,
    conversation: &Conversation,
    resolved: &ResolvedConversation,
    transcript: &AgentTranscript,
    semantic_hits: &[SemanticHit],
) -> AgentSearchOutput {
    let mode = effective_agent_mode(
        &request.query,
        request.cli_mode,
        request.config_mode,
        request.tui_semantic_search,
    );
    let input = [AgentConversationInput {
        conversation,
        resolved: resolved.clone(),
        original_index: 0,
    }];
    let hits = match mode {
        SearchMode::Lexical | SearchMode::Exact => retrieval_hits(
            &request.query,
            request.top,
            conversation,
            resolved,
            transcript,
            mode,
            request.window,
        ),
        SearchMode::Semantic => {
            semantic_output_hits(semantic_hits, request.top, &input, request.window)
        }
        SearchMode::Hybrid => {
            let candidate_depth = modality_candidate_depth_for_hits(request.top);
            hybrid_hits(
                retrieval_hits(
                    &request.query,
                    candidate_depth,
                    conversation,
                    resolved,
                    transcript,
                    SearchMode::Lexical,
                    request.window,
                ),
                semantic_output_hits(semantic_hits, candidate_depth, &input, request.window),
                request.top,
            )
        }
    };

    let mut output = AgentSearchOutput {
        protocol: AgentProtocolKind::Within,
        target: None,
        query: request.query.clone(),
        mode,
        hits,
        groups: Vec::new(),
        flat: true,
        budget: request.budget,
        coverage: None,
    };
    attach_transcript_metadata(&mut output, resolved, transcript);
    output
}

#[cfg(test)]
pub fn run_global_lexical_search(
    request: &AgentSearchRequest,
    conversations: &[Conversation],
    keys: &[AgentConversationKey],
    ranked_indices: &[usize],
    load_transcript: impl Fn(&AgentConversationKey) -> Result<AgentTranscript> + Sync,
) -> Result<AgentSearchOutput> {
    run_global_lexical_search_reporting(
        request,
        conversations,
        keys,
        ranked_indices,
        TranscriptRetention::DropAfterSearch,
        load_transcript,
        |_, _| {},
    )
    .map(|run| run.output)
}

/// Searches the transcripts of `ranked_indices`, from `request`'s offset, in
/// rank order until `request` has enough hits or the page of sessions ends.
/// With a window, a session without hits inside it does not take a place on
/// the page: the next-ranked session is searched in its place.
///
/// Transcripts are read and searched on several threads, each as soon as it
/// is read, but their hits are taken in rank order, so the stop rule and the
/// hits match a search of one session at a time.
pub fn run_global_lexical_search_reporting(
    request: &AgentSearchRequest,
    conversations: &[Conversation],
    keys: &[AgentConversationKey],
    ranked_indices: &[usize],
    retention: TranscriptRetention,
    load_transcript: impl Fn(&AgentConversationKey) -> Result<AgentTranscript> + Sync,
    mut report_error: impl FnMut(&AgentConversationKey, &AppError),
) -> Result<LexicalSearchRun> {
    let mode = request.mode();
    let retrieval_mode = match mode {
        SearchMode::Exact => SearchMode::Exact,
        _ => SearchMode::Lexical,
    };
    let resolved_by_path = crate::agent::refs::resolved_conversations_for_keys(keys)
        .into_iter()
        .map(|resolved| (resolved.key.path.clone(), resolved))
        .collect::<HashMap<_, _>>();
    let places = shortlist_limit(request.top);
    let candidates = if request.window.is_active() {
        usize::MAX
    } else {
        places
    };
    let page = ranked_indices
        .iter()
        .skip(request.session_offset)
        .take(candidates)
        .map(|index| {
            let conversation = conversations.get(*index)?;
            Some((conversation, resolved_by_path.get(&conversation.path)?))
        })
        .collect::<Vec<_>>();
    let hits_per_session = request
        .retrieval_hits_per_conversation
        .unwrap_or_else(|| lexical_per_conversation_candidate_depth(request));

    let mut hits = Vec::new();
    let mut conversations_with_hits = std::collections::HashSet::new();
    let mut kept_transcripts = Vec::new();
    let mut retrieval_time = std::time::Duration::ZERO;
    let mut failed = 0;
    let mut places_taken = 0;
    let searched = scan_in_order(
        &page,
        sessions_still_wanted(request, 0, 0),
        |session| session.map_or(Some(0), |(_, resolved)| resolved.key.transcript_bytes),
        |session| session.map(|(_, resolved)| load_transcript(&resolved.key)),
        |session, read| {
            let (Some((conversation, resolved)), Some(read)) = (*session, read) else {
                return SessionSearch::Unsearchable;
            };
            let transcript = match read {
                Ok(transcript) => transcript,
                Err(error) => return SessionSearch::Failed { resolved, error },
            };
            let retrieval_started = std::time::Instant::now();
            let hits = retrieval_hits(
                &request.query,
                hits_per_session,
                conversation,
                resolved,
                &transcript,
                retrieval_mode,
                request.window,
            );
            let retrieval_time = retrieval_started.elapsed();
            let transcript = (retention == TranscriptRetention::KeepWithHits && !hits.is_empty())
                .then_some(transcript);
            SessionSearch::Searched {
                resolved,
                hits,
                transcript,
                retrieval_time,
            }
        },
        |session| {
            if session.takes_a_place(request.window) {
                places_taken += 1;
            }
            match session {
                SessionSearch::Unsearchable => failed += 1,
                SessionSearch::Failed { resolved, error } => {
                    failed += 1;
                    report_error(&resolved.key, &error);
                }
                SessionSearch::Searched {
                    resolved,
                    hits: session_hits,
                    transcript,
                    retrieval_time: session_retrieval_time,
                } => {
                    retrieval_time += session_retrieval_time;
                    if !session_hits.is_empty() {
                        conversations_with_hits.insert(resolved.reference.canonical());
                    }
                    if let Some(transcript) = transcript {
                        kept_transcripts.push((resolved.key.clone(), transcript));
                    }
                    hits.extend(session_hits);
                }
            }
            if places_taken >= places {
                return ScanStep::Stop;
            }
            match sessions_still_wanted(request, hits.len(), conversations_with_hits.len()) {
                0 => ScanStep::Stop,
                more_wanted => ScanStep::Continue { more_wanted },
            }
        },
    );

    sort_output_hits(&mut hits);
    let (hits, groups) = finalize_global_hits(hits, request);

    Ok(LexicalSearchRun {
        output: AgentSearchOutput {
            protocol: AgentProtocolKind::Search,
            target: None,
            query: request.query.clone(),
            mode,
            hits,
            groups,
            flat: request.flat,
            budget: request.budget,
            coverage: Some(SessionCoverage {
                matched: ranked_indices.len(),
                offset: request.session_offset,
                searched,
                failed,
            }),
        },
        kept_transcripts,
        retrieval_time,
    })
}

/// The fewest further sessions that could complete a search, given the hits
/// and the conversations with hits so far: each session is assumed to give
/// as many hits as it may. 0 once the search has enough. A session that gives
/// fewer lets more start.
fn sessions_still_wanted(
    request: &AgentSearchRequest,
    hits: usize,
    conversations_with_hits: usize,
) -> usize {
    let for_hits = match request.retrieval_hits_per_conversation {
        Some(hits_per_session) => request
            .top
            .saturating_sub(hits)
            .div_ceil(hits_per_session.max(1)),
        None => usize::MAX,
    };
    let for_conversations = if request.flat {
        usize::MAX
    } else {
        request.top.saturating_sub(conversations_with_hits)
    };
    for_hits.min(for_conversations)
}

/// One ranked session's search, as a search thread hands it back.
enum SessionSearch<'a> {
    /// The ranked index names no conversation, or one without a key. Counted
    /// as failed.
    Unsearchable,
    Failed {
        resolved: &'a ResolvedConversation,
        error: AppError,
    },
    Searched {
        resolved: &'a ResolvedConversation,
        hits: Vec<AgentOutputHit>,
        /// Kept only under [`TranscriptRetention::KeepWithHits`].
        transcript: Option<AgentTranscript>,
        retrieval_time: std::time::Duration,
    },
}

impl SessionSearch<'_> {
    /// False for a session searched without hits inside `window`, when it has
    /// bounds; the next-ranked session is searched in its place.
    fn takes_a_place(&self, window: TimeFilter) -> bool {
        match self {
            Self::Searched { hits, .. } => !window.is_active() || !hits.is_empty(),
            Self::Unsearchable | Self::Failed { .. } => true,
        }
    }
}

pub fn run_global_semantic_search(
    request: &AgentSearchRequest,
    inputs: &[AgentConversationInput<'_>],
    semantic: &RankedHits,
) -> AgentSearchOutput {
    let mode = request.mode();
    let semantic_order = semantic_conversation_order(&semantic.conversations, inputs);
    let mut hits = semantic_output_hit_candidates(&semantic.chunks, inputs, request.window);
    sort_output_hits(&mut hits);
    deduplicate_hits_by_identity(&mut hits);
    apply_semantic_conversation_order(&mut hits, &semantic_order);
    let (hits, groups) = finalize_global_hits(hits, request);
    AgentSearchOutput {
        protocol: AgentProtocolKind::Search,
        target: None,
        query: request.query.clone(),
        mode,
        hits,
        groups,
        flat: request.flat,
        budget: request.budget,
        coverage: None,
    }
}

pub fn run_global_hybrid_search(
    request: &AgentSearchRequest,
    lexical: AgentSearchOutput,
    semantic: &RankedHits,
    inputs: &[AgentConversationInput<'_>],
) -> AgentSearchOutput {
    let candidate_depth = modality_candidate_depth(request);
    let semantic_order = semantic_conversation_order(&semantic.conversations, inputs);
    let semantic_conversations = semantic_order
        .iter()
        .take(candidate_depth)
        .cloned()
        .collect::<std::collections::HashSet<_>>();
    let mut semantic_hits =
        semantic_output_hit_candidates(&semantic.chunks, inputs, request.window);
    semantic_hits.retain(|hit| semantic_conversations.contains(&hit.conversation_ref));
    sort_output_hits(&mut semantic_hits);
    deduplicate_hits_by_identity(&mut semantic_hits);
    let hits = hybrid_hits_with_semantic_order(
        lexical.hits,
        semantic_hits,
        &semantic_order,
        candidate_depth,
    );
    let (hits, groups) = finalize_global_hits(hits, request);
    AgentSearchOutput {
        protocol: AgentProtocolKind::Search,
        target: None,
        query: request.query.clone(),
        mode: SearchMode::Hybrid,
        hits,
        groups,
        flat: request.flat,
        budget: request.budget,
        coverage: lexical.coverage,
    }
}

/// The indices of `conversations` a search in `scope` covers: every one, or
/// for a local search, those each agent's own rule places in `workspace`.
pub fn scoped_conversation_inputs(
    conversations: &[Conversation],
    scope: AgentSearchScope,
    workspace: Option<&crate::history::Workspace>,
) -> Result<Vec<usize>> {
    if scope != AgentSearchScope::Local {
        return Ok((0..conversations.len()).collect());
    }
    let Some(workspace) = workspace else {
        return Err(AppError::ConfigError(
            "local agent search requires a current project".to_string(),
        ));
    };
    Ok(conversations
        .iter()
        .enumerate()
        .filter(|(_, conversation)| workspace.contains(conversation))
        .map(|(index, _)| index)
        .collect())
}

pub fn shortlist_limit(top: usize) -> usize {
    top.saturating_mul(SHORTLIST_FACTOR)
        .clamp(SHORTLIST_MIN, SHORTLIST_MAX)
}

/// The lexical half of a hybrid search: flat and deep, so fusion has enough
/// candidates to rank.
pub fn hybrid_lexical_request(request: &AgentSearchRequest) -> AgentSearchRequest {
    AgentSearchRequest {
        top: modality_candidate_depth(request),
        cli_mode: Some(SearchMode::Lexical),
        flat: true,
        retrieval_hits_per_conversation: Some(
            request.hits_per_conversation.saturating_mul(4).max(1),
        ),
        ..request.clone()
    }
}

pub fn modality_candidate_depth(request: &AgentSearchRequest) -> usize {
    let requested_hits = if request.flat {
        request.top
    } else {
        request
            .top
            .saturating_mul(request.hits_per_conversation.max(1))
    };
    modality_candidate_depth_for_hits(requested_hits)
}

fn modality_candidate_depth_for_hits(requested_hits: usize) -> usize {
    requested_hits
        .saturating_mul(MODALITY_CANDIDATE_FACTOR)
        .clamp(MODALITY_CANDIDATE_MIN, MODALITY_CANDIDATE_MAX)
}

fn lexical_per_conversation_candidate_depth(request: &AgentSearchRequest) -> usize {
    if request.flat {
        request.top
    } else {
        request.hits_per_conversation.saturating_mul(4)
    }
    .max(1)
}

fn retrieval_hits(
    query: &str,
    limit: usize,
    conversation: &Conversation,
    resolved: &ResolvedConversation,
    transcript: &AgentTranscript,
    mode: SearchMode,
    window: TimeFilter,
) -> Vec<AgentOutputHit> {
    let search_query = if mode == SearchMode::Exact {
        exact_phrase_query(query)
    } else {
        query.to_string()
    };
    retrieve_agent_hits_for_target(
        AgentTranscriptSearchTarget {
            transcript,
            conversation_ref: Some(&resolved.reference.canonical()),
            timestamp: Some(conversation.timestamp),
        },
        &search_query,
        AgentRetrievalOptions {
            limit,
            window,
            ..AgentRetrievalOptions::default()
        },
    )
    .into_iter()
    .map(|hit| retrieval_output_hit(hit, conversation, resolved, transcript, mode))
    .collect()
}

fn retrieval_output_hit(
    hit: RetrievalHit,
    conversation: &Conversation,
    resolved: &ResolvedConversation,
    transcript: &AgentTranscript,
    mode: SearchMode,
) -> AgentOutputHit {
    let focus = FocusMetadata::of(transcript, resolved, hit.focus_range);
    AgentOutputHit {
        conversation_ref: resolved.reference.canonical(),
        project_id: resolved.key.project_id(),
        conversation_uuid: resolved.reference.uuid(),
        session: resolved.key.session_filename.clone(),
        anchors: focus.anchors,
        title: title_for_conversation(conversation),
        score: hit.score,
        evidence_score: hit.score,
        semantic_score_breakdown: None,
        source: if mode == SearchMode::Exact {
            AgentHitKind::Exact
        } else {
            AgentHitKind::Lexical
        },
        evidence_source: hit.source,
        render_options: hit.render_options,
        preview: hit.preview,
        focus_range: hit.focus_range,
        times: focus.times,
        read_range: hit.read_range,
    }
}

/// The anchors and times a hit shows for the messages in its focus range.
struct FocusMetadata {
    anchors: Vec<String>,
    times: Option<TimeSpan>,
}

impl FocusMetadata {
    fn of(
        transcript: &AgentTranscript,
        resolved: &ResolvedConversation,
        focus: MessageRange,
    ) -> Self {
        let messages: Vec<&AgentMessage> = transcript
            .messages
            .iter()
            .filter(|message| focus.start <= message.ordinal && message.ordinal <= focus.end)
            .collect();
        Self {
            anchors: messages
                .iter()
                .map(|message| transcript.message_anchor(resolved, message))
                .collect(),
            times: TimeSpan::of_messages(messages),
        }
    }
}

fn semantic_output_hits(
    hits: &[SemanticHit],
    limit: usize,
    inputs: &[AgentConversationInput<'_>],
    window: TimeFilter,
) -> Vec<AgentOutputHit> {
    let mut output = semantic_output_hit_candidates(hits, inputs, window);
    sort_output_hits(&mut output);
    deduplicate_hits_by_identity(&mut output);
    output.truncate(limit);
    output
}

/// A hit for each of `hits` with a message inside `window`, its focus
/// narrowed to its first and last such message. A passage can hold messages
/// on both sides of a bound.
fn semantic_output_hit_candidates(
    hits: &[SemanticHit],
    inputs: &[AgentConversationInput<'_>],
    window: TimeFilter,
) -> Vec<AgentOutputHit> {
    hits.iter()
        .filter_map(|hit| {
            let input = inputs
                .iter()
                .find(|input| input.original_index == hit.conversation_index)?;
            let focus_range = input
                .conversation
                .message_times
                .narrowed_to(hit.message_range, &window)?;
            Some(AgentOutputHit {
                conversation_ref: input.resolved.reference.canonical(),
                project_id: input.resolved.key.project_id(),
                conversation_uuid: input.resolved.reference.uuid(),
                session: input.resolved.key.session_filename.clone(),
                anchors: Vec::new(),
                title: title_for_conversation(input.conversation),
                score: semantic_score(hit.score_breakdown),
                evidence_score: semantic_score(hit.score_breakdown),
                semantic_score_breakdown: Some(hit.score_breakdown),
                source: AgentHitKind::Semantic,
                evidence_source: semantic_evidence_source(hit.explanation.chunk.source),
                render_options: semantic_render_options(hit.explanation.chunk.source),
                preview: format_evidence_preview(&hit.snippet),
                focus_range,
                times: None,
                read_range: read_range_for_focus(
                    focus_range,
                    input.conversation.message_count.max(focus_range.end),
                    1,
                ),
            })
        })
        .collect()
}

fn semantic_conversation_order(
    hits: &[SemanticHit],
    inputs: &[AgentConversationInput<'_>],
) -> Vec<String> {
    let input_refs = inputs
        .iter()
        .map(|input| (input.original_index, input.resolved.reference.canonical()))
        .collect::<HashMap<_, _>>();
    let mut seen = std::collections::HashSet::new();
    hits.iter()
        .filter_map(|hit| input_refs.get(&hit.conversation_index))
        .filter(|reference| seen.insert((*reference).clone()))
        .cloned()
        .collect()
}

fn apply_semantic_conversation_order(hits: &mut [AgentOutputHit], order: &[String]) {
    let ranks = order
        .iter()
        .enumerate()
        .map(|(index, reference)| (reference.as_str(), index + 1))
        .collect::<HashMap<_, _>>();
    for hit in hits {
        if let Some(rank) = ranks.get(hit.conversation_ref.as_str()) {
            hit.score = rrf_score(None, Some(*rank));
        }
    }
}

fn semantic_evidence_source(source: SemanticChunkSource) -> AgentHitSource {
    match source {
        SemanticChunkSource::VisibleDialogue
        | SemanticChunkSource::AgentRoute
        | SemanticChunkSource::AgentSubagentDialogue => AgentHitSource::Dialogue,
        SemanticChunkSource::AgentTool | SemanticChunkSource::AgentSubagentTool => {
            AgentHitSource::Tool
        }
        SemanticChunkSource::AgentThinking | SemanticChunkSource::AgentSubagentThinking => {
            AgentHitSource::Thinking
        }
    }
}

fn semantic_render_options(source: SemanticChunkSource) -> AgentHitRenderOptions {
    AgentHitRenderOptions {
        tools: matches!(
            source,
            SemanticChunkSource::AgentTool | SemanticChunkSource::AgentSubagentTool
        ),
        tool_results: matches!(
            source,
            SemanticChunkSource::AgentTool | SemanticChunkSource::AgentSubagentTool
        ),
        thinking: matches!(
            source,
            SemanticChunkSource::AgentThinking | SemanticChunkSource::AgentSubagentThinking
        ),
        subagents: matches!(
            source,
            SemanticChunkSource::AgentSubagentDialogue
                | SemanticChunkSource::AgentSubagentTool
                | SemanticChunkSource::AgentSubagentThinking
        ),
    }
}

fn finalize_global_hits(
    mut hits: Vec<AgentOutputHit>,
    request: &AgentSearchRequest,
) -> (Vec<AgentOutputHit>, Vec<AgentConversationGroup>) {
    if request.flat {
        sort_output_hits(&mut hits);
        deduplicate_hits_by_identity(&mut hits);
        hits.truncate(request.top);
        return (hits, Vec::new());
    }

    let groups = build_conversation_groups(
        hits,
        request.top,
        request.hits_per_conversation,
        request.all_hits,
    );
    let output_hits = flatten_groups(&groups, request.top);
    (output_hits, groups)
}

fn deduplicate_hits_by_identity(hits: &mut Vec<AgentOutputHit>) {
    let mut unique = Vec::<AgentOutputHit>::with_capacity(hits.len());
    for hit in hits.drain(..) {
        if let Some(existing) = unique
            .iter_mut()
            .find(|existing| same_evidence_identity(existing, &hit))
        {
            merge_duplicate_hit(existing, &hit);
        } else {
            unique.push(hit);
        }
    }
    *hits = unique;
}

fn build_conversation_groups(
    hits: Vec<AgentOutputHit>,
    top: usize,
    hits_per_conversation: usize,
    all_hits: bool,
) -> Vec<AgentConversationGroup> {
    let mut by_ref = Vec::<AgentConversationGroup>::new();
    for hit in hits {
        if let Some(group) = by_ref
            .iter_mut()
            .find(|group| group.conversation_ref == hit.conversation_ref)
        {
            group.total_hits += 1;
            push_group_hit(group, hit, hits_per_conversation, all_hits);
        } else {
            let mut group = AgentConversationGroup {
                conversation_ref: hit.conversation_ref.clone(),
                project_id: hit.project_id.clone(),
                conversation_uuid: hit.conversation_uuid.clone(),
                session: hit.session.clone(),
                title: hit.title.clone(),
                score: hit.score,
                total_hits: 1,
                hits: Vec::new(),
            };
            push_group_hit(&mut group, hit, hits_per_conversation, all_hits);
            by_ref.push(group);
        }
    }
    for group in &mut by_ref {
        sort_group_hits(&mut group.hits);
        group.hits.truncate(hits_per_conversation);
        group.score = group
            .hits
            .first()
            .map(|hit| hit.score)
            .unwrap_or(group.score);
    }
    by_ref.retain(|group| !group.hits.is_empty());
    by_ref.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(Ordering::Equal)
            .then_with(|| a.conversation_ref.cmp(&b.conversation_ref))
    });
    by_ref.truncate(top);
    by_ref
}

fn push_group_hit(
    group: &mut AgentConversationGroup,
    hit: AgentOutputHit,
    hits_per_conversation: usize,
    all_hits: bool,
) {
    if !all_hits
        && let Some(existing) = group
            .hits
            .iter_mut()
            .find(|existing| same_evidence_identity(existing, &hit))
    {
        merge_duplicate_hit(existing, &hit);
        return;
    }
    group.hits.push(hit);
    sort_group_hits(&mut group.hits);
    group.hits.truncate(hits_per_conversation);
}

fn same_evidence_identity(existing: &AgentOutputHit, candidate: &AgentOutputHit) -> bool {
    existing.conversation_ref == candidate.conversation_ref
        && existing.focus_range == candidate.focus_range
        && existing.evidence_source == candidate.evidence_source
}

/// Keep the whole breakdown of the contributing chunk the semantic ranker
/// scores highest.
fn merge_score_breakdown(
    existing: &mut Option<SemanticScoreBreakdown>,
    candidate: Option<SemanticScoreBreakdown>,
) {
    let Some(candidate) = candidate else {
        return;
    };
    if existing.is_none_or(|current| {
        candidate
            .hybrid
            .total_cmp(&current.hybrid)
            .then_with(|| candidate.semantic.total_cmp(&current.semantic))
            .then_with(|| candidate.word_match.total_cmp(&current.word_match))
            .is_gt()
    }) {
        *existing = Some(candidate);
    }
}

fn merge_duplicate_hit(existing: &mut AgentOutputHit, candidate: &AgentOutputHit) {
    existing.render_options.merge(candidate.render_options);
    existing.read_range = existing.read_range.union(&candidate.read_range);
    existing.score = existing.score.max(candidate.score);
    existing.evidence_score = existing.evidence_score.max(candidate.evidence_score);
    merge_score_breakdown(
        &mut existing.semantic_score_breakdown,
        candidate.semantic_score_breakdown,
    );
}

fn sort_group_hits(hits: &mut [AgentOutputHit]) {
    hits.sort_by(|a, b| {
        score_bucket(b.score)
            .cmp(&score_bucket(a.score))
            .then_with(|| {
                evidence_source_rank(a.evidence_source)
                    .cmp(&evidence_source_rank(b.evidence_source))
            })
            .then_with(|| {
                b.evidence_score
                    .partial_cmp(&a.evidence_score)
                    .unwrap_or(Ordering::Equal)
            })
            .then_with(|| b.score.partial_cmp(&a.score).unwrap_or(Ordering::Equal))
            .then_with(|| a.focus_range.start.cmp(&b.focus_range.start))
            .then_with(|| source_rank(a.source).cmp(&source_rank(b.source)))
    });
}

fn score_bucket(score: f64) -> i64 {
    (score * 10.0).floor() as i64
}

fn evidence_source_rank(source: AgentHitSource) -> u8 {
    match source {
        AgentHitSource::Dialogue => 0,
        AgentHitSource::Tool => 1,
        AgentHitSource::Thinking => 2,
    }
}

fn flatten_groups(groups: &[AgentConversationGroup], limit: usize) -> Vec<AgentOutputHit> {
    let mut hits = groups
        .iter()
        .flat_map(|group| group.hits.iter().cloned())
        .collect::<Vec<_>>();
    sort_output_hits(&mut hits);
    hits.truncate(limit);
    hits
}

fn hybrid_hits(
    lexical_hits: Vec<AgentOutputHit>,
    semantic_hits: Vec<AgentOutputHit>,
    limit: usize,
) -> Vec<AgentOutputHit> {
    let semantic_order = semantic_hits
        .iter()
        .map(|hit| hit.conversation_ref.clone())
        .collect::<Vec<_>>();
    let mut hits =
        hybrid_hits_with_semantic_order(lexical_hits, semantic_hits, &semantic_order, limit);
    hits.truncate(limit);
    hits
}

fn hybrid_hits_with_semantic_order(
    lexical_hits: Vec<AgentOutputHit>,
    semantic_hits: Vec<AgentOutputHit>,
    semantic_order: &[String],
    _limit: usize,
) -> Vec<AgentOutputHit> {
    let mut conversation_ranks = std::collections::HashMap::<String, ConversationRanks>::new();
    let mut seen = std::collections::HashSet::new();
    for hit in &lexical_hits {
        if seen.insert(hit.conversation_ref.clone()) {
            let rank = seen.len();
            conversation_ranks
                .entry(hit.conversation_ref.clone())
                .or_default()
                .lexical = Some(rank);
        }
    }
    seen.clear();
    for reference in semantic_order {
        if seen.insert(reference.clone()) {
            let rank = seen.len();
            conversation_ranks
                .entry(reference.clone())
                .or_default()
                .semantic = Some(rank);
        }
    }

    let mut ranked = Vec::<RankedHit>::new();
    for (rank, hit) in lexical_hits.into_iter().enumerate() {
        ranked.push(RankedHit {
            exact: hit.source == AgentHitKind::Exact,
            hit,
            lexical_rank: Some(rank + 1),
            semantic_rank: None,
        });
    }
    for (rank, hit) in semantic_hits.into_iter().enumerate() {
        if let Some(existing) = ranked.iter_mut().find(|existing| {
            existing.hit.conversation_ref == hit.conversation_ref
                && existing.hit.focus_range == hit.focus_range
        }) {
            existing.semantic_rank = Some(rank + 1);
            existing.hit.source = AgentHitKind::Hybrid;
            merge_score_breakdown(
                &mut existing.hit.semantic_score_breakdown,
                hit.semantic_score_breakdown,
            );
            existing.hit.render_options.merge(hit.render_options);
            existing.hit.read_range = existing.hit.read_range.union(&hit.read_range);
        } else {
            ranked.push(RankedHit {
                hit,
                lexical_rank: None,
                semantic_rank: Some(rank + 1),
                exact: false,
            });
        }
    }
    for ranked_hit in &mut ranked {
        let ranks = conversation_ranks
            .get(&ranked_hit.hit.conversation_ref)
            .copied()
            .unwrap_or_default();
        ranked_hit.hit.score = rrf_score(ranks.lexical, ranks.semantic);
    }
    ranked.sort_by(|a, b| {
        b.hit
            .score
            .partial_cmp(&a.hit.score)
            .unwrap_or(Ordering::Equal)
            .then_with(|| source_priority(a).cmp(&source_priority(b)))
            .then_with(|| {
                b.hit
                    .evidence_score
                    .partial_cmp(&a.hit.evidence_score)
                    .unwrap_or(Ordering::Equal)
            })
            .then_with(|| a.hit.conversation_ref.cmp(&b.hit.conversation_ref))
            .then_with(|| a.hit.focus_range.start.cmp(&b.hit.focus_range.start))
    });
    ranked.into_iter().map(|ranked| ranked.hit).collect()
}

#[derive(Clone, Copy, Default)]
struct ConversationRanks {
    lexical: Option<usize>,
    semantic: Option<usize>,
}

fn source_priority(hit: &RankedHit) -> u8 {
    if hit.exact {
        0
    } else if hit.lexical_rank.is_some() {
        1
    } else {
        2
    }
}

fn rrf_score(lexical_rank: Option<usize>, semantic_rank: Option<usize>) -> f64 {
    lexical_rank.map_or(0.0, |rank| 1.0 / (RRF_K + rank as f64))
        + semantic_rank.map_or(0.0, |rank| 1.0 / (RRF_K + rank as f64))
}

fn semantic_score(score: SemanticScoreBreakdown) -> f64 {
    score.hybrid as f64
}

fn sort_output_hits(hits: &mut [AgentOutputHit]) {
    hits.sort_by(|a, b| {
        score_bucket(b.score)
            .cmp(&score_bucket(a.score))
            .then_with(|| {
                evidence_source_rank(a.evidence_source)
                    .cmp(&evidence_source_rank(b.evidence_source))
            })
            .then_with(|| b.score.partial_cmp(&a.score).unwrap_or(Ordering::Equal))
            .then_with(|| {
                b.evidence_score
                    .partial_cmp(&a.evidence_score)
                    .unwrap_or(Ordering::Equal)
            })
            .then_with(|| source_rank(a.source).cmp(&source_rank(b.source)))
            .then_with(|| a.conversation_ref.cmp(&b.conversation_ref))
            .then_with(|| a.focus_range.start.cmp(&b.focus_range.start))
    });
}

fn source_rank(source: AgentHitKind) -> u8 {
    match source {
        AgentHitKind::Exact => 0,
        AgentHitKind::Lexical => 1,
        AgentHitKind::Hybrid => 2,
        AgentHitKind::Semantic => 3,
    }
}

/// The phrase exact mode matches: the whole query as one quoted phrase,
/// unless it is already quoted.
pub fn exact_phrase_query(query: &str) -> String {
    if ParsedQuery::parse(query).is_quoted_only() {
        query.to_string()
    } else {
        format!("\"{}\"", query.replace('"', ""))
    }
}

fn title_for_conversation(conversation: &Conversation) -> String {
    conversation
        .custom_title
        .as_deref()
        .or(conversation.summary.as_deref())
        .unwrap_or(&conversation.preview)
        .to_string()
}

fn mode_atom(mode: SearchMode) -> &'static str {
    match mode {
        SearchMode::Lexical => "lexical",
        SearchMode::Semantic => "semantic",
        SearchMode::Exact => "exact",
        SearchMode::Hybrid => "hybrid",
    }
}

fn output_source_atom(hit: &AgentOutputHit) -> &'static str {
    match hit.evidence_source {
        AgentHitSource::Dialogue => hit_source_atom(hit.source),
        AgentHitSource::Tool => "tool",
        AgentHitSource::Thinking => "thinking",
    }
}

fn hit_source_atom(source: AgentHitKind) -> &'static str {
    match source {
        AgentHitKind::Exact => "exact",
        AgentHitKind::Lexical => "lexical",
        AgentHitKind::Semantic => "semantic",
        AgentHitKind::Hybrid => "hybrid",
    }
}

fn render_option_atoms(options: AgentHitRenderOptions) -> String {
    format!(
        " tools={} tool-results={} thinking={} subagents={}",
        options.tools, options.tool_results, options.thinking, options.subagents
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::refs::AgentConversationKey;
    use crate::agent::test_support::{text_message, text_message_at};
    use crate::agent::transcript::{AgentMessage, AgentMessageRole, AgentTranscript};
    use crate::semantic::rank::PreparedChunk;
    use crate::semantic::types::{
        ChunkHits, EmbeddedChunk, SemanticChunkIdentity, SemanticExplanation, SemanticQuality,
        SemanticRationaleKind,
    };
    use chrono::Local;
    use std::path::PathBuf;
    use std::sync::Mutex;
    use std::time::Duration;

    fn message(ordinal: usize, role: AgentMessageRole, text: &str) -> AgentMessage {
        text_message(ordinal, role, text)
    }

    const TEST_UUID: &str = "12345678-1234-4234-9234-123456789abc";

    fn transcript(messages: Vec<AgentMessage>) -> AgentTranscript {
        crate::agent::test_support::transcript(messages, "session.jsonl")
    }

    fn conversation(path: &str, title: &str) -> Conversation {
        Conversation {
            source: crate::history::Source::Claude,
            subagents: Vec::new(),
            session_id: String::new(),
            path: PathBuf::from(path),
            index: 0,
            timestamp: Local::now(),
            preview: title.to_string(),
            preview_first: title.to_string(),
            preview_last: title.to_string(),
            full_text: title.to_string(),
            agent_search_text: String::new(),
            thinking_and_tool_call_text: String::new(),
            semantic_route_text: String::new(),
            semantic_turns: vec![title.to_string()],
            semantic_turn_ranges: vec![MessageRange::single(1)],
            search_text_lower: title.to_string(),
            project_name: Some("project-a".to_string()),
            project_path: None,
            cwd: None,
            message_count: 1,
            message_times: crate::history::MessageTimes::default(),
            message_text_starts: crate::history::MessageTextStarts::default(),
            assistant_messages: 0,
            parse_errors: vec![],
            summary: None,
            custom_title: Some(title.to_string()),
            model: None,
            total_tokens: 0,
            duration_minutes: None,
            fingerprint: None,
            has_transient_read_error: false,
            is_missing_earlier_turns: false,
        }
    }

    fn resolved(path: &str) -> ResolvedConversation {
        let key = AgentConversationKey::new("project-a", path, PathBuf::from(path));
        ResolvedConversation {
            reference: key.conversation_ref(),
            key,
        }
    }

    /// Another agent's session recorded in the workspace is local too.
    #[test]
    fn a_local_search_covers_every_agents_sessions_recorded_in_the_workspace() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = crate::history::workspace::tests::workspace_at_home(directory.path());
        let claude_project =
            crate::history::provider::claude::convert_path_to_project_dir_name(directory.path());
        let claude_here = conversation(&format!("/projects/{claude_project}/one.jsonl"), "one");
        let claude_elsewhere = conversation("/projects/-elsewhere/two.jsonl", "two");
        let mut codex_here = conversation("/sessions/2026/10/05/rollout.jsonl", "three");
        codex_here.source = crate::history::Source::Codex;
        codex_here.cwd = Some(directory.path().to_path_buf());
        let mut codex_elsewhere = codex_here.clone();
        codex_elsewhere.cwd = Some(PathBuf::from("/elsewhere"));
        let conversations = [claude_here, claude_elsewhere, codex_here, codex_elsewhere];

        let scoped =
            scoped_conversation_inputs(&conversations, AgentSearchScope::Local, Some(&workspace))
                .unwrap();

        assert_eq!(scoped, [0, 2]);
    }

    fn request(query: &str, mode: Option<SearchMode>) -> AgentWithinRequest {
        AgentWithinRequest {
            query: query.to_string(),
            top: 10,
            cli_mode: mode,
            config_mode: None,
            tui_semantic_search: None,
            budget: None,
            window: TimeFilter::default(),
        }
    }

    fn global_request(query: &str, mode: SearchMode, top: usize, flat: bool) -> AgentSearchRequest {
        AgentSearchRequest {
            query: query.to_string(),
            top,
            cli_mode: Some(mode),
            config_mode: None,
            tui_semantic_search: None,
            flat,
            hits_per_conversation: 2,
            retrieval_hits_per_conversation: None,
            all_hits: false,
            budget: None,
            session_offset: 0,
            window: TimeFilter::default(),
        }
    }

    /// `chunks` as ranking returns them: each conversation ranked by its
    /// first chunk hit.
    fn ranked_hits(chunks: Vec<SemanticHit>) -> RankedHits {
        let mut ranked_conversations = std::collections::HashSet::new();
        let conversations = chunks
            .iter()
            .filter(|hit| ranked_conversations.insert(hit.conversation_index))
            .cloned()
            .collect();
        RankedHits {
            conversations,
            chunks,
        }
    }

    fn semantic_hit(index: usize, range: MessageRange, text: &str, score: f32) -> SemanticHit {
        semantic_hit_with_source(
            index,
            range,
            text,
            score,
            SemanticChunkSource::VisibleDialogue,
        )
    }

    fn semantic_hit_with_source(
        index: usize,
        range: MessageRange,
        text: &str,
        score: f32,
        source: SemanticChunkSource,
    ) -> SemanticHit {
        SemanticHit::new(
            SemanticScoreBreakdown::new(score, 0.0),
            SemanticExplanation {
                quality: SemanticQuality::Good,
                quality_label: "good",
                matched_terms: vec![],
                evidence_preview: text.to_string(),
                rationale_kind: SemanticRationaleKind::SemanticOnly,
                chunk: SemanticChunkIdentity {
                    conversation_index: index,
                    source,
                    session: "session".to_string(),
                    chunk_index: range.start,
                    message_range: range,
                },
            },
        )
    }

    fn test_uuid(_conv: &str) -> String {
        TEST_UUID.to_string()
    }

    fn lexical_dialogue_hit(
        conv: &str,
        title: &str,
        score: f64,
        preview: &str,
        focus_range: MessageRange,
        read_range: MessageRange,
    ) -> AgentOutputHit {
        AgentOutputHit {
            conversation_ref: conv.to_string(),
            project_id: "pr_test".to_string(),
            conversation_uuid: test_uuid(conv),
            session: "session.jsonl".to_string(),
            anchors: vec!["ma_0000000000000000".to_string()],
            title: title.to_string(),
            score,
            evidence_score: score,
            semantic_score_breakdown: None,
            source: AgentHitKind::Lexical,
            evidence_source: AgentHitSource::Dialogue,
            render_options: AgentHitRenderOptions::default(),
            preview: preview.to_string(),
            focus_range,
            times: None,
            read_range,
        }
    }

    fn lexical_tool_hit(
        conv: &str,
        title: &str,
        score: f64,
        preview: &str,
        focus_range: MessageRange,
        read_range: MessageRange,
    ) -> AgentOutputHit {
        AgentOutputHit {
            conversation_ref: conv.to_string(),
            project_id: "pr_test".to_string(),
            conversation_uuid: test_uuid(conv),
            session: "session.jsonl".to_string(),
            anchors: vec!["ma_0000000000000000".to_string()],
            title: title.to_string(),
            score,
            evidence_score: score,
            semantic_score_breakdown: None,
            source: AgentHitKind::Lexical,
            evidence_source: AgentHitSource::Tool,
            render_options: AgentHitRenderOptions::default(),
            preview: preview.to_string(),
            focus_range,
            times: None,
            read_range,
        }
    }

    fn semantic_dialogue_hit(
        conv: &str,
        title: &str,
        score: f64,
        preview: &str,
        focus_range: MessageRange,
        read_range: MessageRange,
    ) -> AgentOutputHit {
        AgentOutputHit {
            conversation_ref: conv.to_string(),
            project_id: "pr_test".to_string(),
            conversation_uuid: test_uuid(conv),
            session: "session.jsonl".to_string(),
            anchors: vec!["ma_0000000000000000".to_string()],
            title: title.to_string(),
            score,
            evidence_score: score,
            semantic_score_breakdown: Some(SemanticScoreBreakdown::new(score as f32, 0.0)),
            source: AgentHitKind::Semantic,
            evidence_source: AgentHitSource::Dialogue,
            render_options: AgentHitRenderOptions::default(),
            preview: preview.to_string(),
            focus_range,
            times: None,
            read_range,
        }
    }

    #[test]
    fn semantic_breakdown_is_kept_through_rank_fusion_in_every_layout() {
        let conv = conversation("a.jsonl", "cache warming");
        let resolved = resolved("a.jsonl");
        let inputs = [AgentConversationInput {
            conversation: &conv,
            resolved: resolved.clone(),
            original_index: 0,
        }];
        let transcript = transcript(vec![message(1, AgentMessageRole::User, "cache warming")]);
        for (breakdown, atoms) in [
            (
                SemanticScoreBreakdown::new(0.769, 1.0),
                " similarity=0.769000 word-match=1.000000",
            ),
            (
                SemanticScoreBreakdown::new(0.1, 0.0),
                " similarity=0.100000 word-match=0.000000",
            ),
        ] {
            let mut semantic = semantic_hit(0, MessageRange::single(1), "cache warming", 0.0);
            semantic.score_breakdown = breakdown;
            let semantic = [semantic];
            let ranked = ranked_hits(semantic.to_vec());
            for mode in [SearchMode::Semantic, SearchMode::Hybrid] {
                for flat in [false, true] {
                    let global = global_request("cache", mode, 10, flat);
                    let output = if mode == SearchMode::Semantic {
                        run_global_semantic_search(&global, &inputs, &ranked)
                    } else {
                        let lexical = run_within_search(
                            &request("cache", Some(SearchMode::Lexical)),
                            &conv,
                            &resolved,
                            &transcript,
                            &[],
                        );
                        run_global_hybrid_search(&global, lexical, &ranked, &inputs)
                    };
                    let expected_score =
                        rrf_score((mode == SearchMode::Hybrid).then_some(1), Some(1));
                    assert_eq!(output_hits(&output)[0].score, expected_score);
                    assert_eq!(
                        output_hits(&output)[0].semantic_score_breakdown,
                        Some(breakdown)
                    );
                    let rendered = format_agent_output(&output);
                    for record in rendered.lines().filter(|line| {
                        line.starts_with("hit ") || line.starts_with("conversation rank=")
                    }) {
                        assert!(record.contains(atoms), "{record}");
                    }
                }
                let output = run_within_search(
                    &request("cache", Some(mode)),
                    &conv,
                    &resolved,
                    &transcript,
                    &semantic,
                );
                assert_eq!(output.hits[0].semantic_score_breakdown, Some(breakdown));
                assert!(format_agent_output(&output).contains(atoms));
            }
        }
    }

    #[test]
    fn merging_keeps_one_chunks_whole_breakdown_and_ignores_a_missing_one() {
        let a = SemanticScoreBreakdown::new(0.5, 1.0);
        let b = SemanticScoreBreakdown::new(0.75, 0.0);
        for (first, second) in [(a, b), (b, a)] {
            let mut hit = semantic_dialogue_hit(
                "ch_a",
                "title",
                0.7,
                "preview",
                MessageRange::single(1),
                MessageRange::single(1),
            );
            hit.semantic_score_breakdown = Some(first);
            let mut candidate = hit.clone();
            candidate.semantic_score_breakdown = Some(second);
            merge_duplicate_hit(&mut hit, &candidate);
            assert_eq!(hit.semantic_score_breakdown, Some(b));
            candidate.semantic_score_breakdown = None;
            candidate.evidence_score = 20.0;
            merge_duplicate_hit(&mut hit, &candidate);
            assert_eq!(hit.semantic_score_breakdown, Some(b));
            assert_eq!(hit.evidence_score, 20.0);
        }
        assert_eq!(score_breakdown_atoms(None), "");
        assert_eq!(
            score_breakdown_atoms(Some(SemanticScoreBreakdown::new(-0.2, 1.0))),
            " similarity=-0.200000 word-match=1.000000"
        );
        assert_eq!(
            score_breakdown_atoms(Some(SemanticScoreBreakdown::new(0.9, 0.5))),
            " similarity=0.900000 word-match=0.500000"
        );
    }

    #[test]
    fn a_semantic_hit_shows_word_match_at_zero() {
        let conv = conversation("a.jsonl", "cache warming");
        let inputs = [AgentConversationInput {
            conversation: &conv,
            resolved: resolved("a.jsonl"),
            original_index: 0,
        }];
        let ranked = ranked_hits(vec![semantic_hit(
            0,
            MessageRange::single(1),
            "cache warming",
            0.8,
        )]);

        let output = run_global_semantic_search(
            &global_request("cache", SearchMode::Semantic, 10, false),
            &inputs,
            &ranked,
        );

        let rendered = format_agent_output(&output);
        let hit = rendered
            .lines()
            .find(|line| line.starts_with("hit "))
            .unwrap();
        assert!(
            hit.contains(" similarity=0.800000 word-match=0.000000"),
            "{hit}"
        );
    }

    #[test]
    fn conversation_breakdown_uses_first_retained_hit() {
        let mut lexical = lexical_dialogue_hit(
            "ch_a",
            "title",
            0.02,
            "lexical",
            MessageRange::single(1),
            MessageRange::single(1),
        );
        lexical.evidence_score = 10.0;
        let mut semantic = semantic_dialogue_hit(
            "ch_a",
            "title",
            0.9,
            "semantic",
            MessageRange::single(2),
            MessageRange::single(2),
        );
        semantic.score = 0.02;
        for all_hits in [false, true] {
            for limit in [1, 2] {
                let groups = build_conversation_groups(
                    vec![lexical.clone(), semantic.clone()],
                    1,
                    limit,
                    all_hits,
                );
                let output = AgentSearchOutput {
                    protocol: AgentProtocolKind::Search,
                    target: None,
                    query: "cache".into(),
                    mode: SearchMode::Hybrid,
                    hits: vec![],
                    groups,
                    flat: false,
                    budget: None,
                    coverage: None,
                };
                let rendered = format_agent_output(&output);
                let conversation = rendered
                    .lines()
                    .find(|line| line.starts_with("conversation rank="))
                    .unwrap();
                assert!(!conversation.contains("similarity="));
                assert_eq!(rendered.contains("similarity="), limit == 2);
            }
        }
    }

    #[test]
    fn quoted_query_forces_exact_mode() {
        assert_eq!(
            effective_agent_mode(
                "\"literal needle\"",
                Some(SearchMode::Semantic),
                Some(SearchMode::Hybrid),
                Some(true),
            ),
            SearchMode::Exact
        );
    }

    #[test]
    fn zero_matches_emit_protocol_and_query_only() {
        let output = AgentSearchOutput {
            protocol: AgentProtocolKind::Search,
            target: None,
            query: "missing".to_string(),
            mode: SearchMode::Lexical,
            hits: vec![],
            groups: vec![],
            flat: false,
            budget: None,
            coverage: None,
        };

        assert_eq!(
            format_agent_output(&output),
            "protocol agent-search mode=lexical cut=none chars=none policy=per-hit groups=0 hits=0\nquery text=missing hits=0\ngroups count=0\n"
        );
    }

    #[test]
    fn within_without_hits_still_emits_identity() {
        let conv = conversation(&format!("{TEST_UUID}.jsonl"), "title");
        let resolved = resolved(&format!("{TEST_UUID}.jsonl"));
        let transcript = transcript(vec![message(1, AgentMessageRole::User, "haystack")]);

        let output = run_within_search(
            &request("missing", Some(SearchMode::Lexical)),
            &conv,
            &resolved,
            &transcript,
            &[],
        );
        let rendered = format_agent_output(&output);

        assert!(rendered.contains("conversation project=pr_"));
        assert!(rendered.contains(&format!("uuid={TEST_UUID} ref=ch_")));
        assert!(rendered.contains(&format!("ref={}", resolved.reference.canonical())));
    }

    #[test]
    fn within_lexical_formats_title_hit_and_read_lines() {
        let conv = conversation(&format!("{TEST_UUID}.jsonl"), "cache title");
        let resolved = resolved(&format!("{TEST_UUID}.jsonl"));
        let transcript = transcript(vec![
            message(1, AgentMessageRole::User, "question"),
            message(2, AgentMessageRole::Assistant, "cache warming answer"),
        ]);

        let output = run_within_search(
            &request("cache warming", None),
            &conv,
            &resolved,
            &transcript,
            &[],
        );
        let rendered = format_agent_output(&output);

        assert!(rendered.starts_with(
            "protocol agent-within mode=lexical cut=none chars=none policy=per-hit hits=1\n"
        ));
        assert!(rendered.contains("title project=pr_"));
        assert!(rendered.contains(&format!("uuid={TEST_UUID} ref=ch_")));
        assert!(rendered.contains(" | cache title"));
        assert!(rendered.contains("hit project=pr_"));
        assert!(rendered.contains(&format!("uuid={TEST_UUID} ref=ch_")));
        assert!(rendered.contains(" | cache warming answer"));
        assert!(rendered.contains("read ref=ch_"));
        assert!(rendered.contains("focus=m2..m2"));
    }

    #[test]
    fn lexical_quoted_preview_stays_lexical_in_within_and_global_search() {
        let conv = conversation(&format!("{TEST_UUID}.jsonl"), "quoted title");
        let resolved = resolved(&format!("{TEST_UUID}.jsonl"));
        let transcript = transcript(vec![message(
            1,
            AgentMessageRole::Assistant,
            "\"quoted preview only\"",
        )]);
        let within = run_within_search(
            &request("quoted preview", Some(SearchMode::Lexical)),
            &conv,
            &resolved,
            &transcript,
            &[],
        );
        let global_request = global_request("quoted preview", SearchMode::Lexical, 1, true);
        let global = run_global_lexical_search(
            &global_request,
            std::slice::from_ref(&conv),
            std::slice::from_ref(&resolved.key),
            &[0],
            |_| Ok(transcript.clone()),
        )
        .unwrap();

        for output in [&within, &global] {
            assert_eq!(output.mode, SearchMode::Lexical);
            assert_eq!(output.hits.len(), 1);
            assert_eq!(output.hits[0].source, AgentHitKind::Lexical);
            assert!(output.hits[0].preview.starts_with('"'));
            let rendered = format_agent_output(output);
            assert!(rendered.contains("source=lexical"), "{rendered}");
            assert!(!rendered.contains("source=exact"), "{rendered}");
        }
    }

    #[test]
    fn exact_mode_and_quoted_only_queries_mark_hits_exact() {
        let conv = conversation(&format!("{TEST_UUID}.jsonl"), "exact title");
        let resolved = resolved(&format!("{TEST_UUID}.jsonl"));
        let transcript = transcript(vec![
            message(1, AgentMessageRole::Assistant, "\"quoted preview only\""),
            message(2, AgentMessageRole::Assistant, "cache warming answer"),
        ]);
        let exact = run_within_search(
            &request("cache warming", Some(SearchMode::Exact)),
            &conv,
            &resolved,
            &transcript,
            &[],
        );
        assert_eq!(exact.mode, SearchMode::Exact);
        assert_eq!(exact.hits.len(), 1);
        assert_eq!(exact.hits[0].focus_range, MessageRange::single(2));
        assert_eq!(exact.hits[0].source, AgentHitKind::Exact);
        assert!(format_agent_output(&exact).contains("source=exact"));

        let quoted_only = run_within_search(
            &request("\"quoted preview only\"", Some(SearchMode::Semantic)),
            &conv,
            &resolved,
            &transcript,
            &[],
        );
        assert_eq!(quoted_only.mode, SearchMode::Exact);
        assert_eq!(quoted_only.hits.len(), 1);
        assert_eq!(quoted_only.hits[0].focus_range, MessageRange::single(1));
        assert_eq!(quoted_only.hits[0].source, AgentHitKind::Exact);
        assert!(format_agent_output(&quoted_only).contains("source=exact"));
    }

    #[test]
    fn invalid_session_filename_emits_uuid_none() {
        let conv = conversation("session.jsonl", "cache title");
        let resolved = resolved("session.jsonl");
        let transcript = transcript(vec![
            message(1, AgentMessageRole::User, "question"),
            message(2, AgentMessageRole::Assistant, "cache warming answer"),
        ]);

        let output = run_within_search(
            &request("cache warming", None),
            &conv,
            &resolved,
            &transcript,
            &[],
        );
        let rendered = format_agent_output(&output);

        assert!(rendered.contains("title project=pr_"));
        assert!(rendered.contains("uuid=none ref=ch_"));
        assert!(rendered.contains("hit project=pr_"));
    }

    #[test]
    fn within_semantic_returns_message_level_hits_with_context_recipes() {
        let mut conv = conversation(&format!("{TEST_UUID}.jsonl"), "semantic title");
        conv.message_count = 4;
        let resolved = resolved(&format!("{TEST_UUID}.jsonl"));
        let transcript = transcript(vec![message(1, AgentMessageRole::User, "placeholder")]);
        let output = run_within_search(
            &request("semantic", Some(SearchMode::Semantic)),
            &conv,
            &resolved,
            &transcript,
            &[
                semantic_hit(0, MessageRange::single(1), "first", 0.8),
                semantic_hit(0, MessageRange::single(3), "third", 0.7),
            ],
        );

        assert_eq!(output.hits.len(), 2);
        assert_eq!(output.hits[0].focus_range, MessageRange::single(1));
        assert_eq!(output.hits[0].read_range, MessageRange { start: 1, end: 2 });
        assert_eq!(output.hits[1].focus_range, MessageRange::single(3));
        assert_eq!(output.hits[1].read_range, MessageRange { start: 2, end: 4 });
    }

    #[test]
    fn semantic_visible_multi_turn_range_does_not_enable_subagents() {
        let conv = conversation("session.jsonl", "semantic title");
        let resolved = resolved("session.jsonl");
        let hits = semantic_output_hits(
            &[semantic_hit(
                0,
                MessageRange { start: 1, end: 1 },
                "first",
                0.8,
            )],
            1,
            &[AgentConversationInput {
                conversation: &conv,
                resolved,
                original_index: 0,
            }],
            TimeFilter::default(),
        );

        assert!(!hits[0].render_options.subagents);
    }

    #[test]
    fn semantic_progress_source_enables_subagents_for_mixed_range() {
        let conv = conversation("session.jsonl", "semantic title");
        let resolved = resolved("session.jsonl");
        let hits = semantic_output_hits(
            &[semantic_hit_with_source(
                0,
                MessageRange { start: 2, end: 4 },
                "subagent",
                0.8,
                SemanticChunkSource::AgentSubagentDialogue,
            )],
            1,
            &[AgentConversationInput {
                conversation: &conv,
                resolved,
                original_index: 0,
            }],
            TimeFilter::default(),
        );

        assert!(hits[0].render_options.subagents);
    }

    #[test]
    fn semantic_tool_and_thinking_sources_emit_matching_read_policy() {
        let tool = semantic_render_options(SemanticChunkSource::AgentTool);
        assert!(tool.tools);
        assert!(tool.tool_results);
        assert!(!tool.thinking);
        assert!(!tool.subagents);
        assert_eq!(
            semantic_evidence_source(SemanticChunkSource::AgentTool),
            AgentHitSource::Tool
        );

        let thinking = semantic_render_options(SemanticChunkSource::AgentSubagentThinking);
        assert!(!thinking.tools);
        assert!(!thinking.tool_results);
        assert!(thinking.thinking);
        assert!(thinking.subagents);
        assert_eq!(
            semantic_evidence_source(SemanticChunkSource::AgentSubagentThinking),
            AgentHitSource::Thinking
        );
    }

    #[test]
    fn semantic_hits_use_shared_evidence_format_without_changing_recipes() {
        let mut conv = conversation("session.jsonl", "semantic title");
        conv.message_count = 5;
        let resolved = resolved("session.jsonl");
        let raw = format!("semantic\n\u{1b}[31m{} tail", "🙂 ".repeat(200));

        let hits = semantic_output_hits(
            &[semantic_hit_with_source(
                0,
                MessageRange::single(3),
                &raw,
                0.8,
                SemanticChunkSource::AgentSubagentTool,
            )],
            1,
            &[AgentConversationInput {
                conversation: &conv,
                resolved,
                original_index: 0,
            }],
            TimeFilter::default(),
        );

        assert_eq!(hits[0].preview, format_evidence_preview(&raw));
        assert_eq!(hits[0].preview.chars().count(), 160);
        assert_eq!(hits[0].evidence_source, AgentHitSource::Tool);
        assert_eq!(hits[0].focus_range, MessageRange::single(3));
        assert_eq!(hits[0].read_range, MessageRange { start: 2, end: 4 });
        assert!(hits[0].render_options.tools);
        assert!(hits[0].render_options.tool_results);
        assert!(hits[0].render_options.subagents);
    }

    #[test]
    fn hybrid_dedupes_same_focus_and_prefers_lexical_preview() {
        let lexical = vec![lexical_dialogue_hit(
            "ch_123456789abc",
            "title",
            10.0,
            "lexical preview",
            MessageRange::single(2),
            MessageRange { start: 1, end: 3 },
        )];
        let semantic = vec![semantic_dialogue_hit(
            "ch_123456789abc",
            "title",
            0.9,
            "semantic preview",
            MessageRange::single(2),
            MessageRange::single(2),
        )];

        let hits = hybrid_hits(lexical, semantic, 10);

        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].source, AgentHitKind::Hybrid);
        assert_eq!(hits[0].preview, "lexical preview");
        assert_eq!(hits[0].semantic_score_breakdown.unwrap().semantic, 0.9);
        assert_eq!(hits[0].read_range, MessageRange { start: 1, end: 3 });
    }

    #[test]
    fn hybrid_fuses_different_evidence_ranges_by_conversation() {
        let lexical = vec![
            lexical_tool_hit(
                "ch_aaaaaaaaaaaa",
                "lexical only",
                10.0,
                "lexical only preview",
                MessageRange::single(5),
                MessageRange::single(5),
            ),
            lexical_tool_hit(
                "ch_bbbbbbbbbbbb",
                "reinforced",
                9.0,
                "tool preview",
                MessageRange::single(7),
                MessageRange::single(7),
            ),
        ];
        let semantic = vec![semantic_dialogue_hit(
            "ch_bbbbbbbbbbbb",
            "reinforced",
            0.9,
            "dialogue preview",
            MessageRange::single(2),
            MessageRange::single(2),
        )];

        let hits = hybrid_hits(lexical, semantic, 10);

        assert_eq!(hits[0].conversation_ref, "ch_bbbbbbbbbbbb");
        assert_eq!(hits[0].focus_range, MessageRange::single(7));
        assert!(
            hits.iter()
                .any(|hit| hit.conversation_ref == "ch_bbbbbbbbbbbb"
                    && hit.focus_range == MessageRange::single(2))
        );
    }

    #[test]
    fn hybrid_preserves_tool_render_options() {
        let lexical = vec![AgentOutputHit {
            render_options: AgentHitRenderOptions {
                tool_results: true,
                ..AgentHitRenderOptions::default()
            },
            ..lexical_tool_hit(
                "ch_123456789abc",
                "title",
                10.0,
                "tool preview",
                MessageRange::single(2),
                MessageRange { start: 1, end: 3 },
            )
        }];
        let semantic = vec![semantic_dialogue_hit(
            "ch_123456789abc",
            "title",
            0.9,
            "semantic preview",
            MessageRange::single(2),
            MessageRange::single(2),
        )];

        let rendered = format_agent_output(&AgentSearchOutput {
            protocol: AgentProtocolKind::Within,
            target: None,
            query: "needle".to_string(),
            mode: SearchMode::Hybrid,
            hits: hybrid_hits(lexical, semantic, 10),
            groups: vec![],
            flat: true,
            budget: None,
            coverage: None,
        });

        assert!(rendered.contains("hit project=pr_test uuid=12345678-1234-4234-9234-123456789abc ref=ch_123456789abc anchors=ma_0000000000000000 source=tool"));
        assert!(
            rendered.contains("read ref=ch_123456789abc:m1..m3 focus=m2..m2 tools=false tool-results=true thinking=false subagents=false")
        );
    }

    #[test]
    fn grouped_search_caps_hits_per_conversation_and_prefers_dialogue_bucket() {
        let group = build_conversation_groups(
            vec![
                lexical_tool_hit(
                    "ch_a",
                    "title a",
                    10.02,
                    "tool evidence",
                    MessageRange::single(2),
                    MessageRange::single(2),
                ),
                lexical_dialogue_hit(
                    "ch_a",
                    "title a",
                    10.01,
                    "dialogue evidence",
                    MessageRange::single(1),
                    MessageRange::single(1),
                ),
                lexical_dialogue_hit(
                    "ch_a",
                    "title a",
                    9.0,
                    "lower evidence",
                    MessageRange::single(3),
                    MessageRange::single(3),
                ),
            ],
            10,
            2,
            false,
        )
        .pop()
        .unwrap();

        assert_eq!(group.total_hits, 3);
        assert_eq!(group.hits.len(), 2);
        assert_eq!(group.hits[0].preview, "dialogue evidence");
        assert_eq!(group.hits[1].preview, "tool evidence");
    }

    #[test]
    fn grouped_search_keeps_higher_bucket_tool_before_dialogue() {
        let group = build_conversation_groups(
            vec![
                lexical_tool_hit(
                    "ch_a",
                    "title a",
                    10.9,
                    "tool evidence",
                    MessageRange::single(2),
                    MessageRange::single(2),
                ),
                lexical_dialogue_hit(
                    "ch_a",
                    "title a",
                    10.1,
                    "dialogue evidence",
                    MessageRange::single(1),
                    MessageRange::single(1),
                ),
            ],
            10,
            2,
            false,
        )
        .pop()
        .unwrap();

        assert_eq!(group.hits[0].preview, "tool evidence");
    }

    #[test]
    fn grouped_search_preserves_duplicate_previews_at_distinct_messages() {
        let hit = |focus| AgentOutputHit {
            conversation_ref: "ch_a".to_string(),
            project_id: "pr_test".to_string(),
            conversation_uuid: "uuid-a".to_string(),
            session: "session.jsonl".to_string(),
            anchors: vec!["ma_0000000000000000".to_string()],
            title: "title a".to_string(),
            score: 10.0,
            evidence_score: 10.0,
            semantic_score_breakdown: None,
            source: AgentHitKind::Lexical,
            evidence_source: AgentHitSource::Tool,
            render_options: AgentHitRenderOptions {
                tool_results: true,
                ..AgentHitRenderOptions::default()
            },
            preview: "The file /tmp/a has been updated successfully.".to_string(),
            focus_range: MessageRange::single(focus),
            times: None,
            read_range: MessageRange::single(focus),
        };

        let groups = build_conversation_groups(vec![hit(1), hit(2)], 10, 10, false);

        assert_eq!(groups[0].hits.len(), 2);
        assert_eq!(groups[0].hits[0].focus_range, MessageRange::single(1));
        assert_eq!(groups[0].hits[1].focus_range, MessageRange::single(2));
    }

    #[test]
    fn grouped_search_suppresses_same_source_position() {
        let hit = lexical_dialogue_hit(
            "ch_a",
            "title a",
            10.0,
            "same evidence",
            MessageRange::single(1),
            MessageRange::single(1),
        );

        let deduped = build_conversation_groups(vec![hit.clone(), hit.clone()], 10, 10, false);
        let all = build_conversation_groups(vec![hit.clone(), hit], 10, 10, true);

        assert_eq!(deduped[0].hits.len(), 1);
        assert_eq!(all[0].hits.len(), 2);
    }

    #[test]
    fn global_grouped_output_uses_pipe_snippets() {
        let output = AgentSearchOutput {
            protocol: AgentProtocolKind::Search,
            target: None,
            query: "cache warming".to_string(),
            mode: SearchMode::Lexical,
            hits: vec![],
            groups: vec![AgentConversationGroup {
                conversation_ref: "ch_1234abcd5678".to_string(),
                project_id: "pr_test".to_string(),
                conversation_uuid: "12345678-1234-4234-9234-123456789abc".to_string(),
                session: "session.jsonl".to_string(),
                title: "cache session".to_string(),
                score: 12.5,
                total_hits: 3,
                hits: vec![lexical_dialogue_hit(
                    "ch_1234abcd5678",
                    "cache session",
                    12.5,
                    "cache warming answer",
                    MessageRange::single(2),
                    MessageRange { start: 1, end: 3 },
                )],
            }],
            flat: false,
            budget: None,
            coverage: None,
        };

        let rendered = format_agent_output(&output);

        assert!(rendered.starts_with("protocol agent-search mode=lexical cut=none chars=none policy=per-hit groups=1 hits=1\n"));
        assert!(rendered.contains("conversation rank=1 project=pr_test uuid=12345678-1234-4234-9234-123456789abc ref=ch_1234abcd5678 score=12.500000"));
        assert!(rendered.contains("hit project=pr_test uuid=12345678-1234-4234-9234-123456789abc ref=ch_1234abcd5678 anchors=ma_0000000000000000 source=lexical"));
        assert!(rendered.contains("read ref=ch_1234abcd5678:m1..m3 focus=m2..m2 tools=false tool-results=false thinking=false subagents=false\n"));
        assert!(!rendered.contains("similarity="));
        assert!(!rendered.contains("word-match="));
        assert!(!rendered.contains("preview="));
        assert!(!rendered.contains("title ref=ch_1234abcd5678 text="));
    }

    #[test]
    fn grouped_search_ranks_groups_by_best_retained_display_hit() {
        let groups = build_conversation_groups(
            vec![
                lexical_tool_hit(
                    "ch_a",
                    "title a",
                    10.09,
                    "best tool evidence",
                    MessageRange::single(2),
                    MessageRange::single(2),
                ),
                lexical_dialogue_hit(
                    "ch_a",
                    "title a",
                    10.01,
                    "display dialogue evidence",
                    MessageRange::single(1),
                    MessageRange::single(1),
                ),
                lexical_dialogue_hit(
                    "ch_b",
                    "title b",
                    10.05,
                    "other dialogue evidence",
                    MessageRange::single(1),
                    MessageRange::single(1),
                ),
            ],
            2,
            2,
            false,
        );

        assert_eq!(groups[0].conversation_ref, "ch_b");
        assert_eq!(groups[0].score, 10.05);
        assert_eq!(groups[1].hits[0].preview, "display dialogue evidence");
    }

    #[test]
    fn global_flat_output_uses_output_hits_not_group_order() {
        let first = lexical_dialogue_hit(
            "ch_a",
            "title a",
            12.0,
            "first flat hit",
            MessageRange::single(1),
            MessageRange::single(1),
        );
        let second = lexical_dialogue_hit(
            "ch_b",
            "title b",
            11.0,
            "second flat hit",
            MessageRange::single(1),
            MessageRange::single(1),
        );
        let output = AgentSearchOutput {
            protocol: AgentProtocolKind::Search,
            target: None,
            query: "cache warming".to_string(),
            mode: SearchMode::Lexical,
            hits: vec![first.clone()],
            groups: vec![AgentConversationGroup {
                conversation_ref: "ch_b".to_string(),
                project_id: "pr_test".to_string(),
                conversation_uuid: "uuid-b".to_string(),
                session: "session.jsonl".to_string(),
                title: "title b".to_string(),
                score: 11.0,
                total_hits: 1,
                hits: vec![second],
            }],
            flat: true,
            budget: None,
            coverage: None,
        };

        let rendered = format_agent_output(&output);

        assert!(rendered.starts_with(
            "protocol agent-search mode=lexical cut=none chars=none policy=per-hit hits=1\n"
        ));
        assert!(!rendered.contains("conversation rank="));
        assert!(rendered.contains(
            "title project=pr_test uuid=12345678-1234-4234-9234-123456789abc ref=ch_a | title a\n"
        ));
        assert!(rendered.contains("first flat hit"));
        assert!(!rendered.contains("second flat hit"));
    }

    #[test]
    fn search_output_has_a_hard_character_budget_and_atomic_recipes() {
        let hits = (1..=20)
            .map(|ordinal| {
                lexical_dialogue_hit(
                    "ch_1234abcd5678",
                    "title",
                    20.0 - ordinal as f64,
                    &format!("hit {ordinal} {}", "x".repeat(200)),
                    MessageRange::single(ordinal),
                    MessageRange::single(ordinal),
                )
            })
            .collect::<Vec<_>>();
        let output = AgentSearchOutput {
            protocol: AgentProtocolKind::Within,
            target: None,
            query: "needle".to_string(),
            mode: SearchMode::Lexical,
            hits,
            groups: vec![],
            flat: true,
            budget: Some(500),
            coverage: None,
        };

        let rendered = format_agent_output(&output);

        assert!(rendered.chars().count() <= 500);
        assert!(
            rendered.starts_with(
                "protocol agent-within mode=lexical cut=tail chars=500 policy=per-hit"
            )
        );
        assert!(rendered.contains("omitted-lines="));
        assert_eq!(
            rendered
                .lines()
                .filter(|line| line.starts_with("hit "))
                .count(),
            rendered
                .lines()
                .filter(|line| line.starts_with("read "))
                .count()
        );
    }

    #[test]
    fn search_sanitizes_previews_and_declares_recipe_visibility() {
        let output = AgentSearchOutput {
            protocol: AgentProtocolKind::Within,
            target: None,
            query: "needle".to_string(),
            mode: SearchMode::Lexical,
            hits: vec![AgentOutputHit {
                render_options: AgentHitRenderOptions {
                    tool_results: true,
                    ..AgentHitRenderOptions::default()
                },
                ..lexical_tool_hit(
                    "ch_1234abcd5678",
                    "safe\u{1b}[31mtitle",
                    1.0,
                    "tool\u{1b}]0;title\u{7} result",
                    MessageRange::single(1),
                    MessageRange::single(1),
                )
            }],
            groups: vec![],
            flat: true,
            budget: None,
            coverage: None,
        };

        let rendered = format_agent_output(&output);

        assert!(!rendered.contains('\u{1b}'));
        assert!(rendered.contains("| safetitle"));
        assert!(rendered.contains("| tool result"));
        assert!(rendered.contains("tools=false tool-results=true thinking=false subagents=false"));
    }

    #[test]
    fn a_conversation_ranked_by_its_route_leads_with_its_evidence() {
        let conv_a = conversation("a.jsonl", "title a");
        let conv_b = conversation("b.jsonl", "title b");
        let input_a = AgentConversationInput {
            conversation: &conv_a,
            resolved: resolved("a.jsonl"),
            original_index: 0,
        };
        let input_b = AgentConversationInput {
            conversation: &conv_b,
            resolved: resolved("b.jsonl"),
            original_index: 1,
        };
        let request = AgentSearchRequest {
            query: "semantic".to_string(),
            top: 2,
            cli_mode: Some(SearchMode::Semantic),
            config_mode: None,
            tui_semantic_search: None,
            flat: false,
            hits_per_conversation: 1,
            retrieval_hits_per_conversation: None,
            all_hits: false,
            budget: None,
            session_offset: 0,
            window: TimeFilter::default(),
        };
        let semantic = RankedHits {
            conversations: vec![
                semantic_hit_with_source(
                    1,
                    MessageRange::single(1),
                    "synthetic route",
                    1.0,
                    SemanticChunkSource::AgentRoute,
                ),
                semantic_hit(0, MessageRange::single(2), "evidence a", 0.9),
            ],
            chunks: vec![
                semantic_hit(0, MessageRange::single(2), "evidence a", 0.9),
                semantic_hit(1, MessageRange::single(3), "evidence b", 0.8),
            ],
        };

        let output = run_global_semantic_search(&request, &[input_a, input_b], &semantic);

        assert_eq!(
            output.groups[0].conversation_ref,
            resolved("b.jsonl").reference.canonical()
        );
        assert_eq!(output.groups[0].hits[0].preview, "evidence b");
        assert_eq!(
            output.groups[0].hits[0]
                .semantic_score_breakdown
                .unwrap()
                .semantic,
            0.8
        );
        assert!(
            format_agent_output(&output)
                .lines()
                .find(|line| line.starts_with("conversation rank=1 "))
                .unwrap()
                .contains("similarity=0.800000")
        );
    }

    /// Sixty sessions with two dialogue chunks each and a route on every
    /// fourth. Session 2 has only its route, and the route ranks it first.
    fn ranked_semantic_corpus(chunk_hits: ChunkHits) -> RankedHits {
        const QUERY_EMBEDDING: [f32; 2] = [1.0, 0.5];
        let varied = |seed: usize| vec![(seed % 11) as f32 - 5.0, (seed % 7) as f32 - 3.0];
        let chunk = |conversation_index: usize, source, chunk_index: usize, embedding| {
            PreparedChunk::new(EmbeddedChunk {
                conversation_index,
                source,
                session: format!("{conversation_index}"),
                chunk_index,
                text: format!("evidence {conversation_index} {chunk_index}"),
                message_range: MessageRange::single(chunk_index + 1),
                embedding,
            })
        };
        let mut chunks = vec![chunk(
            2,
            SemanticChunkSource::AgentRoute,
            0,
            QUERY_EMBEDDING.to_vec(),
        )];
        for index in (0..60).filter(|index| *index != 2) {
            for chunk_index in 0..2 {
                chunks.push(chunk(
                    index,
                    SemanticChunkSource::VisibleDialogue,
                    chunk_index,
                    varied(index * 7 + chunk_index * 3),
                ));
            }
            if index % 4 == 2 {
                chunks.push(chunk(
                    index,
                    SemanticChunkSource::AgentRoute,
                    0,
                    varied(index * 5),
                ));
            }
        }
        crate::semantic::rank::rank_prepared(
            "evidence",
            &QUERY_EMBEDDING,
            &chunks.iter().collect::<Vec<_>>(),
            chunk_hits,
            &crate::semantic::types::SemanticCancellationToken::new(),
        )
        .unwrap()
    }

    #[test]
    fn semantic_and_hybrid_output_need_only_the_top_conversations_chunk_hits() {
        let conversations = (0..60)
            .map(|index| conversation(&format!("{index}.jsonl"), &format!("title {index}")))
            .collect::<Vec<_>>();
        let inputs = conversations
            .iter()
            .enumerate()
            .map(|(index, conversation)| AgentConversationInput {
                conversation,
                resolved: resolved(&format!("{index}.jsonl")),
                original_index: index,
            })
            .collect::<Vec<_>>();
        let every_chunk = ranked_semantic_corpus(ChunkHits::TopConversations(usize::MAX));
        let lexical = |inputs: &[AgentConversationInput<'_>]| AgentSearchOutput {
            protocol: AgentProtocolKind::Search,
            target: None,
            query: "evidence".to_string(),
            mode: SearchMode::Lexical,
            hits: [0, 59]
                .into_iter()
                .map(|index| {
                    lexical_dialogue_hit(
                        &inputs[index].resolved.reference.canonical(),
                        "title",
                        10.0,
                        "lexical evidence",
                        MessageRange::single(1),
                        MessageRange::single(1),
                    )
                })
                .collect(),
            groups: Vec::new(),
            flat: true,
            budget: None,
            coverage: None,
        };

        for flat in [false, true] {
            let request = global_request("evidence", SearchMode::Semantic, 3, flat);
            let top = ranked_semantic_corpus(ChunkHits::TopConversations(request.top));
            assert_eq!(
                format_agent_output(&run_global_semantic_search(&request, &inputs, &top)),
                format_agent_output(&run_global_semantic_search(&request, &inputs, &every_chunk)),
                "semantic, flat={flat}"
            );

            let request = global_request("evidence", SearchMode::Hybrid, 3, flat);
            let top = ranked_semantic_corpus(ChunkHits::TopConversations(
                modality_candidate_depth(&request),
            ));
            assert_eq!(
                format_agent_output(&run_global_hybrid_search(
                    &request,
                    lexical(&inputs),
                    &top,
                    &inputs
                )),
                format_agent_output(&run_global_hybrid_search(
                    &request,
                    lexical(&inputs),
                    &every_chunk,
                    &inputs
                )),
                "hybrid, flat={flat}"
            );
        }
    }

    #[test]
    fn grouped_semantic_search_collects_until_top_conversations() {
        let conv_a = conversation("a.jsonl", "title a");
        let conv_b = conversation("b.jsonl", "title b");
        let input_a = AgentConversationInput {
            conversation: &conv_a,
            resolved: resolved("a.jsonl"),
            original_index: 0,
        };
        let input_b = AgentConversationInput {
            conversation: &conv_b,
            resolved: resolved("b.jsonl"),
            original_index: 1,
        };
        let request = AgentSearchRequest {
            query: "semantic".to_string(),
            top: 2,
            cli_mode: Some(SearchMode::Semantic),
            config_mode: None,
            tui_semantic_search: None,
            flat: false,
            hits_per_conversation: 2,
            retrieval_hits_per_conversation: None,
            all_hits: false,
            budget: None,
            session_offset: 0,
            window: TimeFilter::default(),
        };
        let mut hits = (1..=20)
            .map(|index| semantic_hit(0, MessageRange::single(index), "first", 1.0))
            .collect::<Vec<_>>();
        hits.push(semantic_hit(1, MessageRange::single(1), "second", 0.1));
        let expected = vec![
            input_a.resolved.reference.canonical(),
            input_b.resolved.reference.canonical(),
        ];

        let output = run_global_semantic_search(&request, &[input_a, input_b], &ranked_hits(hits));

        assert_eq!(
            output
                .groups
                .iter()
                .map(|group| group.conversation_ref.clone())
                .collect::<Vec<_>>(),
            expected
        );
    }

    #[test]
    fn duplicate_uuid_search_records_keep_project_identity() {
        let filename = format!("{TEST_UUID}.jsonl");
        let conversations = vec![
            conversation(&format!("project-a/{filename}"), "needle a"),
            conversation(&format!("project-b/{filename}"), "needle b"),
        ];
        let keys = vec![
            AgentConversationKey::new(
                "project-a",
                &filename,
                PathBuf::from(format!("project-a/{filename}")),
            ),
            AgentConversationKey::new(
                "project-b",
                &filename,
                PathBuf::from(format!("project-b/{filename}")),
            ),
        ];
        let request = global_request("needle", SearchMode::Lexical, 2, false);

        let output = run_global_lexical_search(&request, &conversations, &keys, &[0, 1], |_| {
            Ok(transcript(vec![message(
                1,
                AgentMessageRole::User,
                "needle evidence",
            )]))
        })
        .unwrap();

        assert_eq!(output.groups.len(), 2);
        assert_eq!(
            output.groups[0].conversation_uuid,
            output.groups[1].conversation_uuid
        );
        assert_ne!(output.groups[0].project_id, output.groups[1].project_id);
        assert_ne!(
            output.groups[0].conversation_ref,
            output.groups[1].conversation_ref
        );
    }

    #[test]
    fn global_lexical_loads_only_bounded_shortlist_for_evidence() {
        let conversations = (0..60)
            .map(|index| conversation(&format!("session-{index}.jsonl"), "needle title"))
            .collect::<Vec<_>>();
        let keys = conversations
            .iter()
            .map(|conversation| {
                AgentConversationKey::new(
                    "project-a",
                    conversation.path.file_name().unwrap().to_string_lossy(),
                    conversation.path.clone(),
                )
            })
            .collect::<Vec<_>>();
        let ranked = (0..60).collect::<Vec<_>>();
        let request = global_request("needle", SearchMode::Lexical, 3, false);
        let loads = std::sync::atomic::AtomicUsize::new(0);

        let output = run_global_lexical_search(&request, &conversations, &keys, &ranked, |_| {
            loads.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Ok(transcript(vec![message(
                1,
                AgentMessageRole::User,
                "needle evidence",
            )]))
        })
        .unwrap();

        assert_eq!(output.hits.len(), 3);
        assert!(loads.into_inner() <= 3 + read_pool());
        assert_eq!(
            output.coverage,
            Some(SessionCoverage {
                matched: 60,
                offset: 0,
                searched: 3,
                failed: 0,
            })
        );
    }

    /// How many sessions a search may read past those it needs.
    fn read_pool() -> usize {
        crate::agent::ordered_scan::ScanPools::for_this_machine().reads
    }

    fn numbered_sessions(count: usize) -> (Vec<Conversation>, Vec<AgentConversationKey>) {
        let conversations = (0..count)
            .map(|index| conversation(&format!("session-{index}.jsonl"), "needle title"))
            .collect::<Vec<_>>();
        let keys = conversations
            .iter()
            .map(|conversation| {
                AgentConversationKey::new(
                    "project-a",
                    conversation.path.file_name().unwrap().to_string_lossy(),
                    conversation.path.clone(),
                )
            })
            .collect();
        (conversations, keys)
    }

    fn needle_transcript() -> AgentTranscript {
        transcript(vec![message(1, AgentMessageRole::User, "needle evidence")])
    }

    #[test]
    fn grouped_search_reports_the_sessions_it_searched_and_the_next_offset() {
        let (conversations, keys) = numbered_sessions(60);
        let ranked = (0..60).collect::<Vec<_>>();
        let request = global_request("needle", SearchMode::Lexical, 3, false);

        let output = run_global_lexical_search(&request, &conversations, &keys, &ranked, |_| {
            Ok(needle_transcript())
        })
        .unwrap();
        let rendered = format_agent_output(&output);

        assert!(
            rendered.contains("sessions matched=60 offset=0 searched=3 failed=0\n"),
            "{rendered}"
        );
        assert!(
            rendered.contains("continue search offset=3 mode=lexical action=rerun-with-offset\n"),
            "{rendered}"
        );
    }

    #[test]
    fn a_failed_read_counts_as_failed_and_searched() {
        let (conversations, keys) = numbered_sessions(60);
        let ranked = (0..60).collect::<Vec<_>>();
        let request = global_request("needle", SearchMode::Lexical, 3, false);
        let unreadable = conversations[1].path.clone();

        let output = run_global_lexical_search(&request, &conversations, &keys, &ranked, |key| {
            if key.path == unreadable {
                Err(AppError::Io(std::io::Error::other("unreadable")))
            } else {
                Ok(needle_transcript())
            }
        })
        .unwrap();

        assert_eq!(
            output.coverage,
            Some(SessionCoverage {
                matched: 60,
                offset: 0,
                searched: 4,
                failed: 1,
            })
        );
        assert!(
            format_agent_output(&output)
                .contains("continue search offset=4 mode=lexical action=rerun-with-offset\n")
        );
    }

    /// From 15:40 to 15:45 UTC on 2026-10-14, both included.
    fn window_from_15_40_to_15_45() -> TimeFilter {
        let at = |utc: &str| {
            chrono::DateTime::parse_from_rfc3339(utc)
                .unwrap()
                .with_timezone(&Local)
        };
        TimeFilter {
            after: Some(at("2026-10-14T15:40:00Z")),
            before: Some(at("2026-10-14T15:45:00Z")),
        }
    }

    #[test]
    fn a_session_matching_only_outside_the_window_doesnt_take_a_place() {
        let (conversations, keys) = numbered_sessions(60);
        let ranked = (0..60).collect::<Vec<_>>();
        let mut request = global_request("needle", SearchMode::Lexical, 3, false);
        request.window = window_from_15_40_to_15_45();
        // The 55 best-ranked sessions match only before the window.
        let matching_inside = conversations[55..]
            .iter()
            .map(|conversation| conversation.path.clone())
            .collect::<std::collections::HashSet<_>>();

        let output = run_global_lexical_search(&request, &conversations, &keys, &ranked, |key| {
            let written = if matching_inside.contains(&key.path) {
                "2026-10-14T15:42:00Z"
            } else {
                "2026-10-14T15:30:00Z"
            };
            Ok(transcript(vec![text_message_at(
                1,
                AgentMessageRole::User,
                "needle evidence",
                written,
            )]))
        })
        .unwrap();

        assert_eq!(output.groups.len(), 3);
        assert_eq!(
            output.coverage,
            Some(SessionCoverage {
                matched: 60,
                offset: 0,
                searched: 58,
                failed: 0,
            })
        );
    }

    #[test]
    fn flat_exact_search_opens_one_page_of_matching_sessions() {
        let (conversations, keys) = numbered_sessions(60);
        let ranked = (0..60).collect::<Vec<_>>();
        let opened = Mutex::new(Vec::new());
        let request = global_request("needle", SearchMode::Exact, 3, true);

        let output = run_global_lexical_search(&request, &conversations, &keys, &ranked, |key| {
            opened.lock().unwrap().push(key.path.clone());
            Ok(needle_transcript())
        })
        .unwrap();

        let mut opened = opened.into_inner().unwrap();
        opened.sort();
        let mut first_page = conversations[..50]
            .iter()
            .map(|conversation| conversation.path.clone())
            .collect::<Vec<_>>();
        first_page.sort();
        assert_eq!(opened, first_page);
        assert_eq!(
            output.coverage,
            Some(SessionCoverage {
                matched: 60,
                offset: 0,
                searched: 50,
                failed: 0,
            })
        );
        assert!(
            format_agent_output(&output)
                .contains("continue search offset=50 mode=exact action=rerun-with-offset\n")
        );
    }

    #[test]
    fn an_offset_searches_the_matching_sessions_after_it() {
        let (conversations, keys) = numbered_sessions(60);
        let ranked = (0..60).collect::<Vec<_>>();
        let opened = Mutex::new(Vec::new());
        let mut request = global_request("needle", SearchMode::Exact, 3, true);
        request.session_offset = 50;

        let output = run_global_lexical_search(&request, &conversations, &keys, &ranked, |key| {
            opened.lock().unwrap().push(key.path.clone());
            Ok(needle_transcript())
        })
        .unwrap();

        let mut opened = opened.into_inner().unwrap();
        opened.sort();
        let mut last_page = conversations[50..]
            .iter()
            .map(|conversation| conversation.path.clone())
            .collect::<Vec<_>>();
        last_page.sort();
        assert_eq!(opened, last_page);
        assert_eq!(
            output.coverage,
            Some(SessionCoverage {
                matched: 60,
                offset: 50,
                searched: 10,
                failed: 0,
            })
        );
        assert!(!format_agent_output(&output).contains("continue search"));
    }

    #[test]
    fn hybrid_reports_its_lexical_coverage_and_a_lexical_next_page() {
        let (conversations, keys) = numbered_sessions(60);
        let ranked = (0..60).collect::<Vec<_>>();
        let request = global_request("needle", SearchMode::Hybrid, 3, false);
        let lexical = run_global_lexical_search(
            &hybrid_lexical_request(&request),
            &conversations,
            &keys,
            &ranked,
            |_| Ok(needle_transcript()),
        )
        .unwrap();

        let output = run_global_hybrid_search(&request, lexical, &ranked_hits(Vec::new()), &[]);
        let rendered = format_agent_output(&output);

        assert!(
            rendered.contains("sessions matched=60 offset=0 searched=50 failed=0\n"),
            "{rendered}"
        );
        assert!(
            rendered.contains("continue search offset=50 mode=lexical action=rerun-with-offset\n"),
            "{rendered}"
        );
    }

    #[test]
    fn a_hybrid_search_with_an_offset_runs_lexical() {
        let (conversations, keys) = numbered_sessions(60);
        let ranked = (0..60).collect::<Vec<_>>();
        let mut request = global_request("needle", SearchMode::Hybrid, 3, false);
        request.session_offset = 50;

        let output = run_global_lexical_search(&request, &conversations, &keys, &ranked, |_| {
            Ok(needle_transcript())
        })
        .unwrap();

        assert_eq!(request.mode(), SearchMode::Lexical);
        assert_eq!(output.mode, SearchMode::Lexical);
        assert_eq!(output.coverage.unwrap().offset, 50);
    }

    #[test]
    fn a_hybrid_search_without_an_offset_stays_hybrid() {
        let request = global_request("needle", SearchMode::Hybrid, 3, false);

        assert_eq!(request.mode(), SearchMode::Hybrid);
    }

    #[test]
    fn a_search_for_hits_reads_no_further_ahead_than_the_read_pool() {
        let (conversations, keys) = numbered_sessions(60);
        let ranked = (0..60).collect::<Vec<_>>();
        let mut request = global_request("needle", SearchMode::Lexical, 4, true);
        request.retrieval_hits_per_conversation = Some(2);
        let loads = std::sync::atomic::AtomicUsize::new(0);

        let output = run_global_lexical_search(&request, &conversations, &keys, &ranked, |_| {
            loads.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Ok(transcript(vec![
                message(1, AgentMessageRole::User, "needle one"),
                message(2, AgentMessageRole::User, "needle two"),
            ]))
        })
        .unwrap();

        assert_eq!(output.hits.len(), 4);
        assert_eq!(output.coverage.unwrap().searched, 2);
        assert!(loads.into_inner() <= 2 + read_pool());
    }

    #[test]
    fn a_slow_read_doesnt_change_which_sessions_give_the_hits() {
        let (conversations, keys) = numbered_sessions(12);
        let ranked = (0..12).collect::<Vec<_>>();
        let request = global_request("needle", SearchMode::Lexical, 2, false);
        let highest_ranked = [conversations[0].path.clone(), conversations[1].path.clone()];

        let with_slow_reads =
            run_global_lexical_search(&request, &conversations, &keys, &ranked, |key| {
                if highest_ranked.contains(&key.path) {
                    std::thread::sleep(Duration::from_millis(50));
                }
                Ok(needle_transcript())
            })
            .unwrap();
        let with_instant_reads =
            run_global_lexical_search(&request, &conversations, &keys, &ranked, |_| {
                Ok(needle_transcript())
            })
            .unwrap();

        assert_eq!(with_slow_reads, with_instant_reads);
        let mut group_paths = with_slow_reads
            .groups
            .iter()
            .map(|group| group.session.clone())
            .collect::<Vec<_>>();
        group_paths.sort();
        assert_eq!(group_paths, ["session-0.jsonl", "session-1.jsonl"]);
        assert_eq!(with_slow_reads.coverage.unwrap().searched, 2);
    }

    #[test]
    fn flat_top_counts_ranked_message_hits_and_keeps_same_conversation_hits() {
        let conversations = vec![conversation("session.jsonl", "needle title")];
        let keys = vec![AgentConversationKey::new(
            "project-a",
            "session.jsonl",
            PathBuf::from("session.jsonl"),
        )];
        let request = global_request("needle", SearchMode::Lexical, 2, true);

        let output = run_global_lexical_search(&request, &conversations, &keys, &[0], |_| {
            Ok(transcript(vec![
                message(1, AgentMessageRole::User, "needle one"),
                message(2, AgentMessageRole::User, "needle two"),
                message(3, AgentMessageRole::User, "needle three"),
            ]))
        })
        .unwrap();

        assert_eq!(output.hits.len(), 2);
        assert!(output.groups.is_empty());
        assert_eq!(
            output.hits[0].conversation_ref,
            output.hits[1].conversation_ref
        );
        assert_ne!(output.hits[0].focus_range, output.hits[1].focus_range);
    }

    #[test]
    fn hybrid_lexical_candidates_bound_evidence_per_conversation() {
        let conversations = vec![
            conversation("a.jsonl", "needle a"),
            conversation("b.jsonl", "needle b"),
            conversation("c.jsonl", "needle c"),
        ];
        let keys = conversations
            .iter()
            .map(|conversation| {
                AgentConversationKey::new(
                    "project-a",
                    conversation.path.file_name().unwrap().to_string_lossy(),
                    conversation.path.clone(),
                )
            })
            .collect::<Vec<_>>();
        let mut request = global_request("needle", SearchMode::Lexical, 2, true);
        request.retrieval_hits_per_conversation = Some(1);

        let output = run_global_lexical_search(&request, &conversations, &keys, &[0, 1, 2], |_| {
            Ok(transcript(vec![
                message(1, AgentMessageRole::User, "needle one"),
                message(2, AgentMessageRole::User, "needle two"),
            ]))
        })
        .unwrap();

        assert_eq!(output.hits.len(), 2);
        assert_eq!(output.coverage.unwrap().searched, 2);
        assert_ne!(
            output.hits[0].conversation_ref,
            output.hits[1].conversation_ref
        );
    }

    #[test]
    fn grouped_top_counts_conversations_and_keeps_per_conversation_hits() {
        let conversations = vec![
            conversation("a.jsonl", "needle a"),
            conversation("b.jsonl", "needle b"),
        ];
        let keys = conversations
            .iter()
            .map(|conversation| {
                AgentConversationKey::new(
                    "project-a",
                    conversation.path.file_name().unwrap().to_string_lossy(),
                    conversation.path.clone(),
                )
            })
            .collect::<Vec<_>>();
        let request = global_request("needle", SearchMode::Lexical, 1, false);

        let output = run_global_lexical_search(&request, &conversations, &keys, &[0, 1], |_| {
            Ok(transcript(vec![
                message(1, AgentMessageRole::User, "needle one"),
                message(2, AgentMessageRole::User, "needle two"),
            ]))
        })
        .unwrap();

        assert_eq!(output.groups.len(), 1);
        assert_eq!(output.groups[0].hits.len(), 2);
        assert_eq!(output.coverage.unwrap().searched, 1);
    }

    #[test]
    fn hybrid_keeps_semantic_only_candidate_outside_lexical_candidates() {
        let conv_a = conversation("a.jsonl", "title a");
        let conv_b = conversation("b.jsonl", "title b");
        let inputs = vec![
            AgentConversationInput {
                conversation: &conv_a,
                resolved: resolved("a.jsonl"),
                original_index: 0,
            },
            AgentConversationInput {
                conversation: &conv_b,
                resolved: resolved("b.jsonl"),
                original_index: 1,
            },
        ];
        let lexical_hit = lexical_dialogue_hit(
            &inputs[0].resolved.reference.canonical(),
            "title a",
            10.0,
            "literal candidate",
            MessageRange::single(1),
            MessageRange::single(1),
        );
        let lexical = AgentSearchOutput {
            protocol: AgentProtocolKind::Search,
            target: None,
            query: "concept".to_string(),
            mode: SearchMode::Lexical,
            hits: vec![lexical_hit],
            groups: Vec::new(),
            flat: true,
            budget: None,
            coverage: None,
        };
        let request = global_request("concept", SearchMode::Hybrid, 2, false);

        let output = run_global_hybrid_search(
            &request,
            lexical,
            &ranked_hits(vec![semantic_hit(
                1,
                MessageRange::single(4),
                "conceptual candidate",
                0.9,
            )]),
            &inputs,
        );

        assert_eq!(output.groups.len(), 2);
        assert!(
            output
                .groups
                .iter()
                .any(|group| group.conversation_ref == inputs[1].resolved.reference.canonical())
        );
    }

    #[test]
    fn hybrid_fuses_identity_before_ordering_distinct_hits() {
        let lexical = vec![
            lexical_dialogue_hit(
                "ch_a",
                "title",
                10.0,
                "same preview",
                MessageRange::single(1),
                MessageRange::single(1),
            ),
            lexical_dialogue_hit(
                "ch_a",
                "title",
                9.0,
                "same preview",
                MessageRange::single(2),
                MessageRange::single(2),
            ),
        ];
        let semantic = vec![semantic_dialogue_hit(
            "ch_a",
            "title",
            0.9,
            "semantic preview",
            MessageRange::single(1),
            MessageRange::single(1),
        )];

        let hits = hybrid_hits(lexical, semantic, 10);

        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].source, AgentHitKind::Hybrid);
        assert_eq!(hits[0].focus_range, MessageRange::single(1));
        assert_eq!(hits[1].focus_range, MessageRange::single(2));
    }

    #[test]
    fn flat_unicode_budget_preserves_visibility_recipes() {
        let mut request = global_request("needle", SearchMode::Lexical, 2, true);
        request.budget = Some(900);
        let candidate_hits = (1..=2)
            .map(|ordinal| AgentOutputHit {
                semantic_score_breakdown: Some(SemanticScoreBreakdown::new(0.769, 1.0)),
                source: AgentHitKind::Hybrid,
                render_options: AgentHitRenderOptions {
                    tool_results: true,
                    ..AgentHitRenderOptions::default()
                },
                ..lexical_tool_hit(
                    "ch_a",
                    "unicode title",
                    2.0 - ordinal as f64,
                    &"🙂".repeat(300),
                    MessageRange::single(ordinal),
                    MessageRange::single(ordinal),
                )
            })
            .collect();
        let (hits, groups) = finalize_global_hits(candidate_hits, &request);
        let rendered = format_agent_output(&AgentSearchOutput {
            protocol: AgentProtocolKind::Search,
            target: None,
            query: request.query.clone(),
            mode: SearchMode::Lexical,
            hits,
            groups,
            flat: true,
            budget: request.budget,
            coverage: None,
        });

        assert!(rendered.chars().count() <= 900);
        assert!(rendered.contains("cut=tail"));
        assert!(rendered.contains("tool-results=true"));
        assert!(rendered.contains("similarity=0.769000 word-match=1.000000"));
        assert_eq!(
            rendered
                .lines()
                .filter(|line| line.starts_with("hit "))
                .count(),
            rendered
                .lines()
                .filter(|line| line.starts_with("read "))
                .count()
        );
    }
}
