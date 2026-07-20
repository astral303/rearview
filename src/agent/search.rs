use crate::agent::refs::{AgentConversationKey, MessageRange, ResolvedConversation};
use crate::agent::retrieval::{
    AgentHitRenderOptions, AgentHitSource, AgentRetrievalOptions, AgentSearchHit as RetrievalHit,
    AgentTranscriptSearchTarget, retrieve_agent_hits_for_target,
};
use crate::agent::sanitize::sanitize_agent_text;
use crate::agent::transcript::AgentTranscript;
use crate::error::{AppError, Result};
use crate::history::Conversation;
use crate::search::mode::{SearchMode, SearchModeResolution, resolve_search_mode};
use crate::search::query::ParsedQuery;
use crate::semantic::types::{SemanticChunkSource, SemanticHit, SemanticScoreBreakdown};
use std::cmp::Ordering;
use std::collections::HashMap;

const SHORTLIST_MIN: usize = 50;
const SHORTLIST_FACTOR: usize = 5;
const SHORTLIST_MAX: usize = 500;
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
    pub _scope: AgentSearchScope,
    pub cli_mode: Option<SearchMode>,
    pub config_mode: Option<SearchMode>,
    pub tui_semantic_search: Option<bool>,
    pub flat: bool,
    pub hits_per_conversation: usize,
    pub all_hits: bool,
    pub budget: Option<usize>,
}

#[derive(Clone, Debug)]
pub struct AgentWithinRequest {
    pub query: String,
    pub top: usize,
    pub cli_mode: Option<SearchMode>,
    pub config_mode: Option<SearchMode>,
    pub tui_semantic_search: Option<bool>,
    pub budget: Option<usize>,
}

#[derive(Clone)]
pub struct AgentConversationInput<'a> {
    pub conversation: &'a Conversation,
    pub resolved: ResolvedConversation,
    pub original_index: usize,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AgentSearchStats {
    pub shortlisted: usize,
    pub transcripts_loaded: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AgentSearchOutput {
    pub protocol: AgentProtocolKind,
    pub query: String,
    pub mode: SearchMode,
    pub hits: Vec<AgentOutputHit>,
    pub groups: Vec<AgentConversationGroup>,
    pub flat: bool,
    pub budget: Option<usize>,
    pub stats: AgentSearchStats,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AgentConversationGroup {
    pub conversation_ref: String,
    pub conversation_uuid: String,
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
    pub conversation_uuid: String,
    pub title: String,
    pub score: f64,
    pub source: AgentHitKind,
    pub evidence_source: AgentHitSource,
    pub render_options: AgentHitRenderOptions,
    pub preview: String,
    pub focus_range: MessageRange,
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

pub fn format_agent_output(output: &AgentSearchOutput) -> String {
    let protocol = match output.protocol {
        AgentProtocolKind::Search => "agent-search",
        AgentProtocolKind::Within => "agent-within",
    };
    let hits = output_hits(output);
    let mut rendered = if output.protocol == AgentProtocolKind::Search && !output.flat {
        format!(
            "protocol {protocol} v=3 mode={} cut=none chars={} policy=per-hit groups={} hits={}\n",
            mode_atom(output.mode),
            budget_atom(output.budget),
            output.groups.len(),
            hits.len()
        )
    } else {
        format!(
            "protocol {protocol} v=3 mode={} cut=none chars={} policy=per-hit hits={}\n",
            mode_atom(output.mode),
            budget_atom(output.budget),
            hits.len()
        )
    };
    rendered.push_str(&format!(
        "query text={} hits={}\n",
        crate::agent::protocol::escape_atom(&output.query),
        hits.len()
    ));
    if output.protocol == AgentProtocolKind::Search && !output.flat {
        rendered.push_str(&format!("groups count={}\n", output.groups.len()));
        for (index, group) in output.groups.iter().enumerate() {
            rendered.push_str(&format!(
                "conversation rank={} uuid={} ref={} score={:.6} hits={} total={} | {}\n",
                index + 1,
                crate::agent::protocol::escape_atom(&group.conversation_uuid),
                crate::agent::protocol::escape_atom(&group.conversation_ref),
                group.score,
                group.hits.len(),
                group.total_hits,
                protocol_snippet(&group.title, AGENT_SEARCH_TITLE_CHARS)
            ));
            for hit in &group.hits {
                push_hit_lines(&mut rendered, hit);
            }
        }
        return bound_agent_output(rendered, output.budget);
    }

    for hit in hits {
        rendered.push_str(&format!(
            "title uuid={} ref={} | {}\n",
            crate::agent::protocol::escape_atom(&hit.conversation_uuid),
            crate::agent::protocol::escape_atom(&hit.conversation_ref),
            protocol_snippet(&hit.title, AGENT_SEARCH_TITLE_CHARS)
        ));
        push_hit_lines(&mut rendered, hit);
    }
    bound_agent_output(rendered, output.budget)
}

fn bound_agent_output(rendered: String, budget: Option<usize>) -> String {
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
    while !records.is_empty() {
        let omitted = lines.len().saturating_sub(
            1 + records
                .iter()
                .map(|record| record.lines().count())
                .sum::<usize>(),
        );
        let header =
            lines[0].replace("cut=none", "cut=tail") + &format!(" omitted-lines={omitted}\n");
        let candidate = header + &records.concat();
        if candidate.chars().count() <= budget {
            return candidate;
        }
        records.pop();
    }
    let header = lines[0].replace("cut=none", "cut=tail")
        + &format!(" omitted-lines={}\n", lines.len().saturating_sub(1));
    header.chars().take(budget).collect()
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

fn push_hit_lines(rendered: &mut String, hit: &AgentOutputHit) {
    rendered.push_str(&format!(
        "hit uuid={} ref={} source={} score={:.6} focus=m{}..m{} | {}\n",
        crate::agent::protocol::escape_atom(&hit.conversation_uuid),
        crate::agent::protocol::escape_atom(&hit.conversation_ref),
        output_source_atom(hit),
        hit.score,
        hit.focus_range.start,
        hit.focus_range.end,
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
    let hits = match mode {
        SearchMode::Lexical | SearchMode::Exact => retrieval_hits(
            &request.query,
            request.top,
            conversation,
            resolved,
            transcript,
            mode,
        ),
        SearchMode::Semantic => semantic_output_hits(
            semantic_hits,
            request.top,
            &[AgentConversationInput {
                conversation,
                resolved: resolved.clone(),
                original_index: 0,
            }],
        ),
        SearchMode::Hybrid => hybrid_hits(
            retrieval_hits(
                &request.query,
                request.top,
                conversation,
                resolved,
                transcript,
                SearchMode::Lexical,
            ),
            semantic_output_hits(
                semantic_hits,
                request.top,
                &[AgentConversationInput {
                    conversation,
                    resolved: resolved.clone(),
                    original_index: 0,
                }],
            ),
            request.top,
        ),
    };

    AgentSearchOutput {
        protocol: AgentProtocolKind::Within,
        query: request.query.clone(),
        mode,
        hits,
        groups: Vec::new(),
        flat: true,
        budget: request.budget,
        stats: AgentSearchStats {
            shortlisted: 1,
            transcripts_loaded: 1,
        },
    }
}

pub fn run_global_lexical_search(
    request: &AgentSearchRequest,
    conversations: &[Conversation],
    keys: &[AgentConversationKey],
    ranked_indices: &[usize],
    load_transcript: impl Fn(&AgentConversationKey) -> Result<AgentTranscript>,
) -> Result<AgentSearchOutput> {
    let mode = effective_agent_mode(
        &request.query,
        request.cli_mode,
        request.config_mode,
        request.tui_semantic_search,
    );
    let retrieval_mode = match mode {
        SearchMode::Exact => SearchMode::Exact,
        _ => SearchMode::Lexical,
    };
    let limit = shortlist_limit(request.top).min(ranked_indices.len());
    let key_by_path = keys
        .iter()
        .map(|key| (key.path.clone(), key.clone()))
        .collect::<HashMap<_, _>>();
    let mut hits = Vec::new();
    let mut transcripts_loaded = 0;

    for index in ranked_indices.iter().take(limit).copied() {
        let Some(conversation) = conversations.get(index) else {
            continue;
        };
        let Some(key) = key_by_path.get(&conversation.path) else {
            continue;
        };
        let transcript = load_transcript(key)?;
        transcripts_loaded += 1;
        let resolved = ResolvedConversation {
            key: key.clone(),
            reference: key.conversation_ref(),
        };
        hits.extend(retrieval_hits(
            &request.query,
            retrieval_candidate_limit(request),
            conversation,
            &resolved,
            &transcript,
            retrieval_mode,
        ));
        let conversation_count = hits
            .iter()
            .map(|hit| hit.conversation_ref.as_str())
            .collect::<std::collections::HashSet<_>>()
            .len();
        if conversation_count >= request.top {
            break;
        }
    }

    sort_output_hits(&mut hits);
    let groups = build_conversation_groups(
        hits,
        request.top,
        request.hits_per_conversation,
        request.all_hits,
    );
    let flat_hits = flatten_groups(&groups, request.top);

    Ok(AgentSearchOutput {
        protocol: AgentProtocolKind::Search,
        query: request.query.clone(),
        mode,
        hits: flat_hits,
        groups,
        flat: request.flat,
        budget: request.budget,
        stats: AgentSearchStats {
            shortlisted: limit,
            transcripts_loaded,
        },
    })
}

pub fn run_global_semantic_search(
    request: &AgentSearchRequest,
    inputs: &[AgentConversationInput<'_>],
    semantic_hits: &[SemanticHit],
) -> AgentSearchOutput {
    let mode = effective_agent_mode(
        &request.query,
        request.cli_mode,
        request.config_mode,
        request.tui_semantic_search,
    );
    let hits = semantic_output_hits_for_grouped_search(semantic_hits, request.top, inputs);
    let groups = build_conversation_groups(
        hits,
        request.top,
        request.hits_per_conversation,
        request.all_hits,
    );
    let flat_hits = flatten_groups(&groups, request.top);
    AgentSearchOutput {
        protocol: AgentProtocolKind::Search,
        query: request.query.clone(),
        mode,
        hits: flat_hits,
        groups,
        flat: request.flat,
        budget: request.budget,
        stats: AgentSearchStats {
            shortlisted: inputs.len(),
            transcripts_loaded: 0,
        },
    }
}

pub fn run_global_hybrid_search(
    request: &AgentSearchRequest,
    lexical: AgentSearchOutput,
    semantic_hits: &[SemanticHit],
    inputs: &[AgentConversationInput<'_>],
) -> AgentSearchOutput {
    let semantic = semantic_output_hits_for_grouped_search(semantic_hits, request.top, inputs);
    let lexical_hits = flatten_groups(&lexical.groups, retrieval_candidate_limit(request));
    let hits = hybrid_hits(lexical_hits, semantic, retrieval_candidate_limit(request));
    let groups = build_conversation_groups(
        hits,
        request.top,
        request.hits_per_conversation,
        request.all_hits,
    );
    let flat_hits = flatten_groups(&groups, request.top);
    AgentSearchOutput {
        protocol: AgentProtocolKind::Search,
        query: request.query.clone(),
        mode: SearchMode::Hybrid,
        hits: flat_hits,
        groups,
        flat: request.flat,
        budget: request.budget,
        stats: lexical.stats,
    }
}

pub fn scoped_conversation_inputs(
    conversations: &[Conversation],
    scope: AgentSearchScope,
    current_project_dir_name: Option<&str>,
) -> Result<Vec<usize>> {
    let mut indices = Vec::new();
    for (index, conversation) in conversations.iter().enumerate() {
        if scope == AgentSearchScope::Local {
            let Some(project) = current_project_dir_name else {
                return Err(AppError::ConfigError(
                    "local agent search requires a current project".to_string(),
                ));
            };
            let matches = conversation
                .path
                .parent()
                .and_then(|p| p.file_name())
                .is_some_and(|name| {
                    crate::history::is_same_project(&name.to_string_lossy(), project)
                });
            if !matches {
                continue;
            }
        }
        indices.push(index);
    }
    Ok(indices)
}

pub fn shortlist_limit(top: usize) -> usize {
    top.saturating_mul(SHORTLIST_FACTOR)
        .clamp(SHORTLIST_MIN, SHORTLIST_MAX)
}

fn retrieval_candidate_limit(request: &AgentSearchRequest) -> usize {
    request
        .top
        .saturating_mul(request.hits_per_conversation.max(1))
        .saturating_mul(4)
        .max(request.top)
}

fn retrieval_hits(
    query: &str,
    limit: usize,
    conversation: &Conversation,
    resolved: &ResolvedConversation,
    transcript: &AgentTranscript,
    mode: SearchMode,
) -> Vec<AgentOutputHit> {
    let search_query = if mode == SearchMode::Exact && !ParsedQuery::parse(query).is_quoted_only() {
        quote_query(query)
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
            ..AgentRetrievalOptions::default()
        },
    )
    .into_iter()
    .map(|hit| retrieval_output_hit(hit, conversation, resolved, mode))
    .collect()
}

fn retrieval_output_hit(
    hit: RetrievalHit,
    conversation: &Conversation,
    resolved: &ResolvedConversation,
    mode: SearchMode,
) -> AgentOutputHit {
    AgentOutputHit {
        conversation_ref: resolved.reference.canonical(),
        conversation_uuid: resolved.reference.uuid(),
        title: title_for_conversation(conversation),
        score: hit.score,
        source: if mode == SearchMode::Exact || ParsedQuery::parse(&hit.preview).is_quoted_only() {
            AgentHitKind::Exact
        } else {
            AgentHitKind::Lexical
        },
        evidence_source: hit.source,
        render_options: hit.render_options,
        preview: hit.preview,
        focus_range: hit.focus_range,
        read_range: hit.read_range,
    }
}

fn semantic_output_hits(
    hits: &[SemanticHit],
    limit: usize,
    inputs: &[AgentConversationInput<'_>],
) -> Vec<AgentOutputHit> {
    let mut output = semantic_output_hit_candidates(hits, inputs);
    sort_output_hits(&mut output);
    output.truncate(limit);
    output
}

fn semantic_output_hits_for_grouped_search(
    hits: &[SemanticHit],
    top: usize,
    inputs: &[AgentConversationInput<'_>],
) -> Vec<AgentOutputHit> {
    let mut output = semantic_output_hit_candidates(hits, inputs);
    sort_output_hits(&mut output);
    let mut selected = Vec::new();
    let mut conversation_refs = std::collections::HashSet::new();
    for hit in output {
        conversation_refs.insert(hit.conversation_ref.clone());
        selected.push(hit);
        if conversation_refs.len() >= top {
            break;
        }
    }
    selected
}

fn semantic_output_hit_candidates(
    hits: &[SemanticHit],
    inputs: &[AgentConversationInput<'_>],
) -> Vec<AgentOutputHit> {
    hits.iter()
        .filter_map(|hit| {
            let input = inputs
                .iter()
                .find(|input| input.original_index == hit.conversation_index)?;
            Some(AgentOutputHit {
                conversation_ref: input.resolved.reference.canonical(),
                conversation_uuid: input.resolved.reference.uuid(),
                title: title_for_conversation(input.conversation),
                score: semantic_score(hit.score_breakdown),
                source: AgentHitKind::Semantic,
                evidence_source: AgentHitSource::Dialogue,
                render_options: AgentHitRenderOptions {
                    subagents: hit.explanation.chunk.source
                        == SemanticChunkSource::AgentSubagentDialogue,
                    ..AgentHitRenderOptions::default()
                },
                preview: hit.snippet.clone(),
                focus_range: hit.message_range,
                read_range: hit.message_range,
            })
        })
        .collect()
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
                conversation_uuid: hit.conversation_uuid.clone(),
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
            .iter()
            .map(|hit| hit.score)
            .max_by(|a, b| a.partial_cmp(b).unwrap_or(Ordering::Equal))
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
            .find(|existing| duplicate_hit(existing, &hit))
    {
        existing.render_options.merge(hit.render_options);
        existing.read_range = existing.read_range.union(&hit.read_range);
        existing.score = existing.score.max(hit.score);
        return;
    }
    group.hits.push(hit);
    sort_group_hits(&mut group.hits);
    group.hits.truncate(hits_per_conversation);
}

fn duplicate_hit(existing: &AgentOutputHit, candidate: &AgentOutputHit) -> bool {
    existing.focus_range == candidate.focus_range
        || existing.preview == candidate.preview
        || (is_file_update_boilerplate(&existing.preview)
            && is_file_update_boilerplate(&candidate.preview))
}

fn is_file_update_boilerplate(preview: &str) -> bool {
    preview.starts_with("The file ") && preview.ends_with(" has been updated successfully.")
}

fn sort_group_hits(hits: &mut [AgentOutputHit]) {
    hits.sort_by(|a, b| {
        score_bucket(b.score)
            .cmp(&score_bucket(a.score))
            .then_with(|| {
                evidence_source_rank(a.evidence_source)
                    .cmp(&evidence_source_rank(b.evidence_source))
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
            existing.hit.score = rrf_score(existing.lexical_rank, existing.semantic_rank);
            existing.hit.source = AgentHitKind::Hybrid;
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
        ranked_hit.hit.score = rrf_score(ranked_hit.lexical_rank, ranked_hit.semantic_rank);
    }
    ranked.sort_by(|a, b| {
        b.hit
            .score
            .partial_cmp(&a.hit.score)
            .unwrap_or(Ordering::Equal)
            .then_with(|| source_priority(a).cmp(&source_priority(b)))
            .then_with(|| a.hit.conversation_ref.cmp(&b.hit.conversation_ref))
            .then_with(|| a.hit.focus_range.start.cmp(&b.hit.focus_range.start))
    });
    ranked.truncate(limit);
    ranked.into_iter().map(|ranked| ranked.hit).collect()
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
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(Ordering::Equal)
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

fn quote_query(query: &str) -> String {
    format!("\"{}\"", query.replace('"', ""))
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
    use crate::agent::test_support::text_message;
    use crate::agent::transcript::{AgentMessage, AgentMessageRole, AgentTranscript};
    use crate::semantic::types::{
        SemanticChunkIdentity, SemanticExplanation, SemanticQuality, SemanticRationaleKind,
    };
    use chrono::Local;
    use std::path::PathBuf;

    fn message(ordinal: usize, role: AgentMessageRole, text: &str) -> AgentMessage {
        text_message(ordinal, role, text)
    }

    const TEST_UUID: &str = "12345678-1234-4234-9234-123456789abc";

    fn transcript(messages: Vec<AgentMessage>) -> AgentTranscript {
        crate::agent::test_support::transcript(messages, "session.jsonl")
    }

    fn conversation(path: &str, title: &str) -> Conversation {
        Conversation {
            path: PathBuf::from(path),
            index: 0,
            timestamp: Local::now(),
            preview: title.to_string(),
            preview_first: title.to_string(),
            preview_last: title.to_string(),
            full_text: title.to_string(),
            agent_search_text: String::new(),
            semantic_turns: vec![title.to_string()],
            semantic_turn_ranges: vec![MessageRange::single(1)],
            search_text_lower: title.to_string(),
            project_name: Some("project-a".to_string()),
            project_path: None,
            cwd: None,
            message_count: 1,
            parse_errors: vec![],
            summary: None,
            custom_title: Some(title.to_string()),
            model: None,
            total_tokens: 0,
            duration_minutes: None,
        }
    }

    fn resolved(path: &str) -> ResolvedConversation {
        let key = AgentConversationKey::new("project-a", path, PathBuf::from(path));
        ResolvedConversation {
            reference: key.conversation_ref(),
            key,
        }
    }

    fn request(query: &str, mode: Option<SearchMode>) -> AgentWithinRequest {
        AgentWithinRequest {
            query: query.to_string(),
            top: 10,
            cli_mode: mode,
            config_mode: None,
            tui_semantic_search: None,
            budget: None,
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
            SemanticScoreBreakdown {
                hybrid: score,
                semantic: score,
                lexical: 0.0,
            },
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
            conversation_uuid: test_uuid(conv),
            title: title.to_string(),
            score,
            source: AgentHitKind::Lexical,
            evidence_source: AgentHitSource::Dialogue,
            render_options: AgentHitRenderOptions::default(),
            preview: preview.to_string(),
            focus_range,
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
            conversation_uuid: test_uuid(conv),
            title: title.to_string(),
            score,
            source: AgentHitKind::Lexical,
            evidence_source: AgentHitSource::Tool,
            render_options: AgentHitRenderOptions::default(),
            preview: preview.to_string(),
            focus_range,
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
            conversation_uuid: test_uuid(conv),
            title: title.to_string(),
            score,
            source: AgentHitKind::Semantic,
            evidence_source: AgentHitSource::Dialogue,
            render_options: AgentHitRenderOptions::default(),
            preview: preview.to_string(),
            focus_range,
            read_range,
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
            query: "missing".to_string(),
            mode: SearchMode::Lexical,
            hits: vec![],
            groups: vec![],
            flat: false,
            budget: None,
            stats: AgentSearchStats::default(),
        };

        assert_eq!(
            format_agent_output(&output),
            "protocol agent-search v=3 mode=lexical cut=none chars=none policy=per-hit groups=0 hits=0\nquery text=missing hits=0\ngroups count=0\n"
        );
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
            "protocol agent-within v=3 mode=lexical cut=none chars=none policy=per-hit hits=1\n"
        ));
        assert!(rendered.contains(&format!("title uuid={TEST_UUID} ref=ch_")));
        assert!(rendered.contains(" | cache title"));
        assert!(rendered.contains(&format!("hit uuid={TEST_UUID} ref=ch_")));
        assert!(rendered.contains(" | cache warming answer"));
        assert!(rendered.contains("read ref=ch_"));
        assert!(rendered.contains("focus=m2..m2"));
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

        assert!(rendered.contains("title uuid=none ref=ch_"));
        assert!(rendered.contains("hit uuid=none ref=ch_"));
    }

    #[test]
    fn within_semantic_returns_message_level_hits() {
        let conv = conversation(&format!("{TEST_UUID}.jsonl"), "semantic title");
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
        assert_eq!(output.hits[1].focus_range, MessageRange::single(3));
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
        );

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
        assert_eq!(hits[0].read_range, MessageRange { start: 1, end: 3 });
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
            query: "needle".to_string(),
            mode: SearchMode::Hybrid,
            hits: hybrid_hits(lexical, semantic, 10),
            groups: vec![],
            flat: true,
            budget: None,
            stats: AgentSearchStats::default(),
        });

        assert!(rendered.contains(
            "hit uuid=12345678-1234-4234-9234-123456789abc ref=ch_123456789abc source=tool"
        ));
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
    fn grouped_search_dedupes_boilerplate_unless_all_hits() {
        let hit = |focus, preview: &str| AgentOutputHit {
            conversation_ref: "ch_a".to_string(),
            conversation_uuid: "uuid-a".to_string(),
            title: "title a".to_string(),
            score: 10.0,
            source: AgentHitKind::Lexical,
            evidence_source: AgentHitSource::Tool,
            render_options: AgentHitRenderOptions {
                tool_results: true,
                ..AgentHitRenderOptions::default()
            },
            preview: preview.to_string(),
            focus_range: MessageRange::single(focus),
            read_range: MessageRange::single(focus),
        };
        let hits = vec![
            hit(1, "The file /tmp/a has been updated successfully."),
            hit(2, "The file /tmp/b has been updated successfully."),
        ];

        let deduped = build_conversation_groups(hits.clone(), 10, 10, false);
        let all = build_conversation_groups(hits, 10, 10, true);

        assert_eq!(deduped[0].hits.len(), 1);
        assert_eq!(all[0].hits.len(), 2);
        assert!(deduped[0].hits[0].render_options.tool_results);
    }

    #[test]
    fn global_grouped_output_uses_pipe_snippets() {
        let output = AgentSearchOutput {
            protocol: AgentProtocolKind::Search,
            query: "cache warming".to_string(),
            mode: SearchMode::Lexical,
            hits: vec![],
            groups: vec![AgentConversationGroup {
                conversation_ref: "ch_1234abcd5678".to_string(),
                conversation_uuid: "12345678-1234-4234-9234-123456789abc".to_string(),
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
            stats: AgentSearchStats::default(),
        };

        let rendered = format_agent_output(&output);

        assert!(rendered.starts_with("protocol agent-search v=3 mode=lexical cut=none chars=none policy=per-hit groups=1 hits=1\n"));
        assert!(rendered.contains("conversation rank=1 uuid=12345678-1234-4234-9234-123456789abc ref=ch_1234abcd5678 score=12.500000 hits=1 total=3 | cache session\n"));
        assert!(rendered.contains("hit uuid=12345678-1234-4234-9234-123456789abc ref=ch_1234abcd5678 source=lexical score=12.500000 focus=m2..m2 | cache warming answer\n"));
        assert!(rendered.contains("read ref=ch_1234abcd5678:m1..m3 focus=m2..m2 tools=false tool-results=false thinking=false subagents=false\n"));
        assert!(!rendered.contains("preview="));
        assert!(!rendered.contains("title ref=ch_1234abcd5678 text="));
    }

    #[test]
    fn grouped_search_ranks_groups_by_best_retained_score() {
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

        assert_eq!(groups[0].conversation_ref, "ch_a");
        assert_eq!(groups[0].score, 10.09);
        assert_eq!(groups[0].hits[0].preview, "display dialogue evidence");
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
            query: "cache warming".to_string(),
            mode: SearchMode::Lexical,
            hits: vec![first.clone()],
            groups: vec![AgentConversationGroup {
                conversation_ref: "ch_b".to_string(),
                conversation_uuid: "uuid-b".to_string(),
                title: "title b".to_string(),
                score: 11.0,
                total_hits: 1,
                hits: vec![second],
            }],
            flat: true,
            budget: None,
            stats: AgentSearchStats::default(),
        };

        let rendered = format_agent_output(&output);

        assert!(rendered.starts_with(
            "protocol agent-search v=3 mode=lexical cut=none chars=none policy=per-hit hits=1\n"
        ));
        assert!(!rendered.contains("conversation rank="));
        assert!(
            rendered
                .contains("title uuid=12345678-1234-4234-9234-123456789abc ref=ch_a | title a\n")
        );
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
            query: "needle".to_string(),
            mode: SearchMode::Lexical,
            hits,
            groups: vec![],
            flat: true,
            budget: Some(500),
            stats: AgentSearchStats::default(),
        };

        let rendered = format_agent_output(&output);

        assert!(rendered.chars().count() <= 500);
        assert!(rendered.starts_with(
            "protocol agent-within v=3 mode=lexical cut=tail chars=500 policy=per-hit"
        ));
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
            stats: AgentSearchStats::default(),
        };

        let rendered = format_agent_output(&output);

        assert!(!rendered.contains('\u{1b}'));
        assert!(rendered.contains("| safetitle"));
        assert!(rendered.contains("| tool result"));
        assert!(rendered.contains("tools=false tool-results=true thinking=false subagents=false"));
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
            _scope: AgentSearchScope::Global,
            cli_mode: Some(SearchMode::Semantic),
            config_mode: None,
            tui_semantic_search: None,
            flat: false,
            hits_per_conversation: 2,
            all_hits: false,
            budget: None,
        };
        let mut hits = (1..=20)
            .map(|index| semantic_hit(0, MessageRange::single(index), "first", 1.0))
            .collect::<Vec<_>>();
        hits.push(semantic_hit(1, MessageRange::single(1), "second", 0.1));
        let expected = vec![
            input_a.resolved.reference.canonical(),
            input_b.resolved.reference.canonical(),
        ];

        let output = run_global_semantic_search(&request, &[input_a, input_b], &hits);

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
        let request = AgentSearchRequest {
            query: "needle".to_string(),
            top: 3,
            _scope: AgentSearchScope::Global,
            cli_mode: Some(SearchMode::Lexical),
            config_mode: None,
            tui_semantic_search: None,
            flat: false,
            hits_per_conversation: 2,
            all_hits: false,
            budget: None,
        };

        let output = run_global_lexical_search(&request, &conversations, &keys, &ranked, |_| {
            Ok(transcript(vec![message(
                1,
                AgentMessageRole::User,
                "needle evidence",
            )]))
        })
        .unwrap();

        assert_eq!(output.hits.len(), 3);
        assert_eq!(output.stats.shortlisted, 50);
        assert_eq!(output.stats.transcripts_loaded, 3);
    }
}
