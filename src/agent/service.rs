use crate::agent;
use crate::agent::diagnostic::{AgentError, AgentErrorKind, AgentWarning, AgentWarningKind};
use crate::cli::{self, AgentCommand, AgentOutlineArgs, AgentReadArgs};
use crate::config;
use crate::error::{AppError, Result};
use crate::history;
use crate::search;
use crate::search::mode::SearchMode;
use crate::semantic;
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

type ResolvedReadRefs = Vec<(agent::refs::ReadRef, agent::refs::ResolvedConversation)>;

#[derive(Default)]
pub struct AgentService {
    transcripts: RefCell<
        HashMap<PathBuf, std::result::Result<agent::transcript::AgentTranscript, AgentError>>,
    >,
}

pub fn execute(command: AgentCommand) -> Result<String> {
    AgentService::default().execute(command)
}

impl AgentService {
    pub fn execute(&mut self, command: AgentCommand) -> Result<String> {
        self.execute_inner(command).map_err(structured_agent_error)
    }

    fn execute_inner(&mut self, command: AgentCommand) -> Result<String> {
        match command {
            AgentCommand::Search(args) => self.run_search(&args),
            AgentCommand::Within(args) => self.run_within(&args),
            AgentCommand::Read(args) => self.run_read(&args, None),
            AgentCommand::Outline(args) => self.run_outline(&args, None),
        }
    }

    fn load_transcript(&self, path: &Path) -> Result<agent::transcript::AgentTranscript> {
        if let Some(cached) = self.transcripts.borrow().get(path) {
            return cached.clone().map_err(AppError::from);
        }
        let loaded = agent::transcript::AgentTranscript::load(path).map_err(|error| match error {
            AppError::Agent(error) => error,
            AppError::Io(error) => AgentError::io(
                Some(&path.to_string_lossy()),
                format!("failed to read transcript: {error}"),
            ),
            AppError::Json(error) => AgentError::malformed_transcript(
                Some(&path.to_string_lossy()),
                format!("failed to parse transcript JSONL: {error}"),
            ),
            error => {
                AgentError::malformed_transcript(Some(&path.to_string_lossy()), error.to_string())
            }
        });
        self.transcripts
            .borrow_mut()
            .insert(path.to_path_buf(), loaded.clone());
        loaded.map_err(AppError::from)
    }

    fn run_search(&self, args: &cli::AgentSearchArgs) -> Result<String> {
        let config = config::load_config()?;
        let search_config = config.search.unwrap_or_default();
        let tui_config = config.tui.unwrap_or_default();
        let mut conversations = history::load_all_conversations(false, None)?;
        conversations.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
        let current_project_dir_name = if args.local {
            std::env::current_dir()
                .ok()
                .map(|dir| history::convert_path_to_project_dir_name(&dir))
        } else {
            None
        };
        let scope = match args.scope() {
            cli::AgentScope::Local => agent::search::AgentSearchScope::Local,
            cli::AgentScope::Global => agent::search::AgentSearchScope::Global,
        };
        let scoped = agent::search::scoped_conversation_inputs(
            &conversations,
            scope,
            current_project_dir_name.as_deref(),
        )?;
        let request = agent::search::AgentSearchRequest {
            query: args.query.clone(),
            top: args.top,
            cli_mode: args.mode_override(),
            config_mode: search_config.mode,
            tui_semantic_search: tui_config.semantic_search,
            flat: args.flat,
            hits_per_conversation: args.hits_per_conv,
            all_hits: args.all_hits,
            budget: (!args.no_budget).then_some(args.budget),
        };
        let mode = agent::search::effective_agent_mode(
            &request.query,
            request.cli_mode,
            request.config_mode,
            request.tui_semantic_search,
        );
        let (keys, mut base_warnings) = discover_agent_keys(current_project_dir_name.as_deref())?;
        base_warnings.extend(warnings_for_skipped_transcripts(
            self,
            &conversations,
            &keys,
        ));
        match mode {
            SearchMode::Lexical | SearchMode::Exact => {
                let ranked = lexically_rank_scoped(&conversations, &args.query, &scoped);
                let warnings = RefCell::new(base_warnings.clone());
                let output = agent::search::run_global_lexical_search_reporting(
                    &request,
                    &conversations,
                    &keys,
                    &ranked,
                    |key| self.load_transcript(&key.path),
                    |key, error| {
                        warnings.borrow_mut().push(AgentWarning::from_app_error(
                            error,
                            Some(&key.conversation_ref().canonical()),
                        ));
                    },
                )?;
                return Ok(agent::search::format_agent_output_with_warnings(
                    &output,
                    &warnings.into_inner(),
                ));
            }
            SearchMode::Semantic => {
                let (output, mut warnings) =
                    run_agent_semantic_search(self, &request, &conversations, &keys, &scoped)?;
                warnings.splice(0..0, base_warnings);
                return Ok(agent::search::format_agent_output_with_warnings(
                    &output, &warnings,
                ));
            }
            SearchMode::Hybrid => {
                let lexical_request = agent::search::AgentSearchRequest {
                    top: agent::search::modality_candidate_depth(&request),
                    cli_mode: Some(SearchMode::Lexical),
                    flat: true,
                    ..request.clone()
                };
                let ranked = lexically_rank_scoped(&conversations, &args.query, &scoped);
                let warnings = RefCell::new(base_warnings.clone());
                let lexical = agent::search::run_global_lexical_search_reporting(
                    &lexical_request,
                    &conversations,
                    &keys,
                    &ranked,
                    |key| self.load_transcript(&key.path),
                    |key, error| {
                        warnings.borrow_mut().push(AgentWarning::from_app_error(
                            error,
                            Some(&key.conversation_ref().canonical()),
                        ));
                    },
                )?;
                let inputs = agent_inputs_for_indices(&conversations, &keys, &scoped)?;
                match run_agent_semantic_hits(self, &args.query, &inputs) {
                    Ok((semantic, semantic_warnings)) => {
                        warnings.borrow_mut().extend(semantic_warnings);
                        let output = agent::search::run_global_hybrid_search(
                            &request, lexical, &semantic, &inputs,
                        );
                        return Ok(agent::search::format_agent_output_with_warnings(
                            &output,
                            &warnings.into_inner(),
                        ));
                    }
                    Err(error) => {
                        warnings
                            .borrow_mut()
                            .push(AgentWarning::from_app_error(&error, None));
                        let lexical = agent::search::run_global_lexical_search_reporting(
                            &request,
                            &conversations,
                            &keys,
                            &ranked,
                            |key| self.load_transcript(&key.path),
                            |key, error| {
                                warnings.borrow_mut().push(AgentWarning::from_app_error(
                                    error,
                                    Some(&key.conversation_ref().canonical()),
                                ));
                            },
                        )?;
                        let mut output = lexical;
                        output.mode = SearchMode::Hybrid;
                        return Ok(agent::search::format_agent_output_with_warnings(
                            &output,
                            &warnings.into_inner(),
                        ));
                    }
                }
            }
        }
    }

    fn run_within(&self, args: &cli::AgentWithinArgs) -> Result<String> {
        let config = config::load_config()?;
        let search_config = config.search.unwrap_or_default();
        let tui_config = config.tui.unwrap_or_default();
        let conversations = history::load_all_conversations(false, None)?;
        let (keys, _) = discover_agent_keys(None)?;
        let resolved = resolve_agent_conversation_arg(&args.conversation, Some(&keys))?;
        let conversation = conversations
            .iter()
            .find(|conversation| conversation.path == resolved.key.path)
            .ok_or_else(|| {
                AgentError::new(
                    AgentErrorKind::NotFound,
                    Some(&args.conversation),
                    "conversation metadata was not found",
                )
            })?;
        let transcript = self
            .load_transcript(&resolved.key.path)
            .map_err(|error| target_error(error, &resolved))?;
        let request = agent::search::AgentWithinRequest {
            query: args.query.clone(),
            top: args.top,
            cli_mode: args.mode_override(),
            config_mode: search_config.mode,
            tui_semantic_search: tui_config.semantic_search,
            budget: (!args.no_budget).then_some(args.budget),
        };
        let mode = agent::search::effective_agent_mode(
            &request.query,
            request.cli_mode,
            request.config_mode,
            request.tui_semantic_search,
        );
        let output = match mode {
            SearchMode::Lexical | SearchMode::Exact => agent::search::run_within_search(
                &request,
                conversation,
                &resolved,
                &transcript,
                &[],
            ),
            SearchMode::Semantic => {
                run_agent_within_semantic(&request, conversation, &resolved, &transcript)?
            }
            SearchMode::Hybrid => {
                match run_agent_within_semantic(&request, conversation, &resolved, &transcript) {
                    Ok(output) => output,
                    Err(error) => {
                        let mut output = agent::search::run_within_search(
                            &agent::search::AgentWithinRequest {
                                cli_mode: Some(SearchMode::Lexical),
                                ..request.clone()
                            },
                            conversation,
                            &resolved,
                            &transcript,
                            &[],
                        );
                        output.mode = SearchMode::Hybrid;
                        return Ok(agent::search::format_agent_output_with_warnings(
                            &output,
                            &[AgentWarning::from_app_error(&error, None)],
                        ));
                    }
                }
            }
        };
        Ok(agent::search::format_agent_output(&output))
    }
}

fn discover_agent_keys(
    project_filter: Option<&str>,
) -> Result<(Vec<agent::refs::AgentConversationKey>, Vec<AgentWarning>)> {
    let root = history::get_claude_projects_root().map_err(structured_agent_error)?;
    let projects = std::fs::read_dir(&root).map_err(|error| {
        AgentError::io(
            Some(&root.to_string_lossy()),
            format!("failed to list projects: {error}"),
        )
    })?;
    let mut keys = Vec::new();
    let mut warnings = Vec::new();
    for project in projects {
        let project = match project {
            Ok(project) => project,
            Err(error) => {
                warnings.push(AgentWarning {
                    kind: AgentWarningKind::Io,
                    reference: None,
                    detail: format!("failed to read project entry: {error}"),
                });
                continue;
            }
        };
        let project_path = project.path();
        if !project_path.is_dir() {
            continue;
        }
        let Some(project_name) = project_path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if project_filter.is_some_and(|filter| !history::is_same_project(project_name, filter)) {
            continue;
        }
        let entries = match std::fs::read_dir(&project_path) {
            Ok(entries) => entries,
            Err(error) => {
                warnings.push(AgentWarning {
                    kind: AgentWarningKind::Io,
                    reference: None,
                    detail: format!(
                        "failed to list project transcripts at {}: {error}",
                        project_path.display()
                    ),
                });
                continue;
            }
        };
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    warnings.push(AgentWarning {
                        kind: AgentWarningKind::Io,
                        reference: None,
                        detail: format!(
                            "failed to read transcript entry in {}: {error}",
                            project_path.display()
                        ),
                    });
                    continue;
                }
            };
            let path = entry.path();
            let Some(filename) = path
                .file_name()
                .and_then(|name| name.to_str())
                .map(str::to_string)
            else {
                continue;
            };
            if path.extension().and_then(|extension| extension.to_str()) == Some("jsonl")
                && !filename.starts_with("agent-")
            {
                keys.push(agent::refs::AgentConversationKey::new(
                    project_name,
                    filename,
                    path,
                ));
            }
        }
    }
    keys.sort_by(|left, right| left.path.cmp(&right.path));
    Ok((keys, warnings))
}

fn warnings_for_skipped_transcripts(
    service: &AgentService,
    conversations: &[history::Conversation],
    keys: &[agent::refs::AgentConversationKey],
) -> Vec<AgentWarning> {
    let key_paths = keys
        .iter()
        .map(|key| key.path.as_path())
        .collect::<std::collections::HashSet<_>>();
    let known = conversations
        .iter()
        .map(|conversation| conversation.path.as_path())
        .collect::<std::collections::HashSet<_>>();
    let mut warnings = Vec::new();
    for conversation in conversations {
        if key_paths.contains(conversation.path.as_path()) && !conversation.parse_errors.is_empty()
        {
            let key = agent::refs::AgentConversationKey::from_conversation(conversation).ok();
            warnings.push(AgentWarning {
                kind: crate::agent::diagnostic::AgentWarningKind::MalformedTranscript,
                reference: key.map(|key| key.conversation_ref().canonical()),
                detail: format!(
                    "transcript contains {} malformed JSONL record(s)",
                    conversation.parse_errors.len()
                ),
            });
        }
    }
    for key in keys {
        if known.contains(key.path.as_path()) {
            continue;
        }
        let reference = key.conversation_ref().canonical();
        match service.load_transcript(&key.path) {
            Ok(transcript) => warnings.push(AgentWarning::skipped(
                Some(&reference),
                if transcript.is_empty() {
                    "transcript has no visible messages"
                } else {
                    "transcript has no searchable conversation metadata"
                },
            )),
            Err(error) => warnings.push(AgentWarning::from_app_error(&error, Some(&reference))),
        }
    }
    warnings
}

fn target_error(error: AppError, resolved: &agent::refs::ResolvedConversation) -> AppError {
    match error {
        AppError::Agent(mut error) => {
            error.reference = Some(resolved.reference.canonical());
            error.into()
        }
        error => structured_agent_error(error),
    }
}

fn structured_agent_error(error: AppError) -> AppError {
    match error {
        AppError::Agent(_) => error,
        AppError::SessionNotFound(reference) => AgentError::new(
            AgentErrorKind::NotFound,
            Some(&reference),
            "conversation was not found",
        )
        .into(),
        AppError::Json(error) => {
            AgentError::malformed_transcript(None, format!("failed to parse transcript: {error}"))
                .into()
        }
        AppError::Io(error) => AgentError::io(None, error.to_string()).into(),
        AppError::ConfigError(detail) => AgentError::invalid_ref("command", detail).into(),
        error => AgentError::io(None, error.to_string()).into(),
    }
}

fn run_agent_semantic_search(
    service: &AgentService,
    request: &agent::search::AgentSearchRequest,
    conversations: &[history::Conversation],
    keys: &[agent::refs::AgentConversationKey],
    indices: &[usize],
) -> Result<(agent::search::AgentSearchOutput, Vec<AgentWarning>)> {
    let inputs = agent_inputs_for_indices(conversations, keys, indices)?;
    let (semantic, warnings) = run_agent_semantic_hits(service, &request.query, &inputs)?;
    Ok((
        agent::search::run_global_semantic_search(request, &inputs, &semantic),
        warnings,
    ))
}

fn run_agent_semantic_hits(
    service: &AgentService,
    query: &str,
    inputs: &[agent::search::AgentConversationInput<'_>],
) -> Result<(Vec<semantic::types::SemanticHit>, Vec<AgentWarning>)> {
    let (candidates, warnings) =
        agent_semantic_candidates_with_loader(inputs, |path| service.load_transcript(path));
    run_agent_semantic_hits_for_candidates(query, &candidates).map(|hits| (hits, warnings))
}

fn run_agent_semantic_hits_for_candidates(
    query: &str,
    candidates: &[semantic::index::SemanticIndexCandidate],
) -> Result<Vec<semantic::types::SemanticHit>> {
    let parsed = search::query::ParsedQuery::parse(query);
    let request = semantic::index::SemanticIndexRequest {
        query: parsed.semantic_text(),
        literal_filters: parsed.literals(),
        full_corpus: candidates,
        scope: candidates,
        corpus_version: 3,
        prewarm: false,
    };
    let mut state = semantic::index::SemanticIndexState::new();
    let mut embedder = semantic::fastembed::FastembedEmbedder::new().map_err(|error| {
        AgentError::semantic_unavailable(format!("failed to initialize semantic search: {error}"))
    })?;
    let cancellation = semantic::types::SemanticCancellationToken::new();
    let response = state
        .refresh_or_prewarm(
            &request,
            &mut embedder,
            &cancellation,
            |progress| eprintln!("Semantic search: {progress:?}"),
            semantic::cache::write_embedding_cache,
        )
        .map_err(|error| {
            AgentError::semantic_unavailable(format!("semantic search failed: {error}"))
        })?;
    Ok(response.chunk_hits)
}

#[cfg(test)]
pub(crate) fn agent_semantic_candidates(
    inputs: &[agent::search::AgentConversationInput<'_>],
) -> (
    Vec<semantic::index::SemanticIndexCandidate>,
    Vec<AgentWarning>,
) {
    agent_semantic_candidates_with_loader(inputs, |path| {
        agent::transcript::AgentTranscript::load(path)
    })
}

fn agent_semantic_candidates_with_loader(
    inputs: &[agent::search::AgentConversationInput<'_>],
    load_transcript: impl Fn(&Path) -> Result<agent::transcript::AgentTranscript>,
) -> (
    Vec<semantic::index::SemanticIndexCandidate>,
    Vec<AgentWarning>,
) {
    let mut candidates = Vec::new();
    let mut warnings = Vec::new();
    for input in inputs {
        push_visible_agent_semantic_candidate(&mut candidates, input);
        if input.conversation.agent_search_text.is_empty() {
            continue;
        }
        match load_transcript(&input.resolved.key.path) {
            Ok(transcript) => {
                push_progress_agent_semantic_candidate(&mut candidates, input, &transcript)
            }
            Err(error) => warnings.push(AgentWarning::from_app_error(
                &error,
                Some(&input.resolved.reference.canonical()),
            )),
        }
    }
    (candidates, warnings)
}

fn push_agent_semantic_candidates(
    candidates: &mut Vec<semantic::index::SemanticIndexCandidate>,
    input: &agent::search::AgentConversationInput<'_>,
    transcript: &agent::transcript::AgentTranscript,
) {
    push_visible_agent_semantic_candidate(candidates, input);
    push_progress_agent_semantic_candidate(candidates, input, transcript);
}

fn push_visible_agent_semantic_candidate(
    candidates: &mut Vec<semantic::index::SemanticIndexCandidate>,
    input: &agent::search::AgentConversationInput<'_>,
) {
    candidates.push(semantic::index::SemanticIndexCandidate {
        index: input.original_index,
        source: semantic::types::SemanticChunkSource::VisibleDialogue,
        conversation: std::sync::Arc::new(input.conversation.clone()),
    });
}

fn push_progress_agent_semantic_candidate(
    candidates: &mut Vec<semantic::index::SemanticIndexCandidate>,
    input: &agent::search::AgentConversationInput<'_>,
    transcript: &agent::transcript::AgentTranscript,
) {
    if !agent::visibility::ContentVisibility::SEARCH.subagents {
        return;
    }
    if let Some(progress_conversation) =
        agent_progress_semantic_conversation(input.conversation, transcript)
    {
        candidates.push(semantic::index::SemanticIndexCandidate {
            index: input.original_index,
            source: semantic::types::SemanticChunkSource::AgentSubagentDialogue,
            conversation: std::sync::Arc::new(progress_conversation),
        });
    }
}

fn agent_progress_semantic_conversation(
    conversation: &history::Conversation,
    transcript: &agent::transcript::AgentTranscript,
) -> Option<history::Conversation> {
    let mut semantic_turns = Vec::new();
    let mut semantic_turn_ranges = Vec::new();
    for message in &transcript.messages {
        if message.parent_tool_use_id.is_none() {
            continue;
        }
        for part in &message.parts {
            if let agent::transcript::AgentMessagePart::Text { text, .. } = part {
                let role = match message.role {
                    agent::transcript::AgentMessageRole::User => {
                        semantic::filter::SemanticTurnRole::User
                    }
                    agent::transcript::AgentMessageRole::Assistant => {
                        semantic::filter::SemanticTurnRole::Assistant
                    }
                };
                if let Some(turn) = semantic::filter::filter_turn(role, text) {
                    semantic_turns.push(turn);
                    semantic_turn_ranges.push(agent::refs::MessageRange::single(message.ordinal));
                }
            }
        }
    }
    if semantic_turns.is_empty() {
        return None;
    }
    let mut conversation = conversation.clone();
    let file_name = conversation
        .path
        .file_name()
        .map(|name| format!("{}.agent-semantic", name.to_string_lossy()))?;
    conversation.path = conversation.path.with_file_name(file_name);
    conversation.semantic_turns = semantic_turns;
    conversation.semantic_turn_ranges = semantic_turn_ranges;
    Some(conversation)
}

fn run_agent_within_semantic(
    request: &agent::search::AgentWithinRequest,
    conversation: &history::Conversation,
    resolved: &agent::refs::ResolvedConversation,
    transcript: &agent::transcript::AgentTranscript,
) -> Result<agent::search::AgentSearchOutput> {
    let input = agent::search::AgentConversationInput {
        conversation,
        resolved: resolved.clone(),
        original_index: 0,
    };
    let mut candidates = Vec::new();
    push_agent_semantic_candidates(&mut candidates, &input, transcript);
    let semantic = run_agent_semantic_hits_for_candidates(&request.query, &candidates)?;
    Ok(agent::search::run_within_search(
        request,
        conversation,
        resolved,
        transcript,
        &semantic,
    ))
}

fn agent_inputs_for_indices<'a>(
    conversations: &'a [history::Conversation],
    keys: &[agent::refs::AgentConversationKey],
    indices: &[usize],
) -> Result<Vec<agent::search::AgentConversationInput<'a>>> {
    let key_by_path = keys
        .iter()
        .map(|key| (key.path.clone(), key.clone()))
        .collect::<std::collections::HashMap<_, _>>();
    indices
        .iter()
        .filter_map(|index| {
            let conversation = conversations.get(*index)?;
            let key = key_by_path.get(&conversation.path)?;
            Some(Ok(agent::search::AgentConversationInput {
                conversation,
                resolved: agent::refs::ResolvedConversation {
                    key: key.clone(),
                    reference: key.conversation_ref(),
                },
                original_index: *index,
            }))
        })
        .collect()
}

impl AgentService {
    pub(crate) fn run_read(
        &self,
        args: &AgentReadArgs,
        keys: Option<&[agent::refs::AgentConversationKey]>,
    ) -> Result<String> {
        let discovered;
        let keys = match keys {
            Some(keys) => keys,
            None => {
                discovered = discover_agent_keys(None)?.0;
                &discovered
            }
        };
        let (resolved_refs, focus) = resolve_agent_read_args(args, Some(keys))?;
        let options = agent_protocol_options(
            args.output.no_budget,
            args.output.budget,
            args.output.tools,
            args.output.tool_results,
            args.output.thinking,
            args.output.subagents,
        );
        let transcripts = resolved_refs
            .iter()
            .map(|(_, resolved)| {
                self.load_transcript(&resolved.key.path)
                    .map_err(|error| target_error(error, resolved))
            })
            .collect::<Result<Vec<_>>>()?;
        let requests = resolved_refs
            .iter()
            .zip(transcripts.iter())
            .map(
                |((read_ref, resolved), transcript)| agent::protocol::ReadRequest {
                    resolved,
                    transcript,
                    range: read_ref.range,
                },
            )
            .collect::<Vec<_>>();
        let protocol_focus = focus.map(|focus| {
            let conversation_full_ref = focus.conversation.as_ref().and_then(|conversation| {
                resolved_refs
                    .iter()
                    .find(|(_, resolved)| resolved.reference.full_ref().starts_with(conversation))
                    .map(|(_, resolved)| resolved.reference.full_ref())
            });
            agent::protocol::ProtocolFocus {
                conversation_full_ref,
                range: focus.range,
            }
        });
        let slice = if let Some(range) = args.lines {
            Some(agent::protocol::ReadSlice::Lines(range))
        } else {
            args.match_query
                .as_ref()
                .map(|query| agent::protocol::ReadSlice::Match {
                    query: query.clone(),
                    context: args.context,
                })
        };
        agent::protocol::format_read(&requests, protocol_focus, slice.as_ref(), options).map_err(
            |error| match resolved_refs.first() {
                Some((_, resolved)) => target_error(error, resolved),
                None => structured_agent_error(error),
            },
        )
    }

    pub(crate) fn run_outline(
        &self,
        args: &AgentOutlineArgs,
        keys: Option<&[agent::refs::AgentConversationKey]>,
    ) -> Result<String> {
        let discovered;
        let keys = match keys {
            Some(keys) => keys,
            None => {
                discovered = discover_agent_keys(None)?.0;
                &discovered
            }
        };
        let resolved = resolve_agent_conversation_arg(&args.conversation, Some(keys))?;
        let transcript = self
            .load_transcript(&resolved.key.path)
            .map_err(|error| target_error(error, &resolved))?;
        Ok(agent::protocol::format_outline(
            &resolved,
            &transcript,
            agent_protocol_options(
                args.output.no_budget,
                args.output.budget,
                args.output.tools,
                args.output.tool_results,
                args.output.thinking,
                args.output.subagents,
            ),
        ))
    }
}

pub(crate) fn resolve_agent_read_args(
    args: &AgentReadArgs,
    keys: Option<&[agent::refs::AgentConversationKey]>,
) -> Result<(ResolvedReadRefs, Option<agent::refs::FocusRef>)> {
    let refs = args
        .refs
        .iter()
        .map(|reference| agent::refs::parse_read_ref(reference))
        .collect::<Result<Vec<_>>>()?;
    if (args.lines.is_some() || args.match_query.is_some())
        && (refs.len() != 1 || !refs[0].range.is_some_and(|range| range.start == range.end))
    {
        return Err(AppError::ConfigError(
            "--lines and --match require exactly one single-message ref such as ch_...:m7"
                .to_string(),
        ));
    }
    let loaded_keys;
    let keys = if let Some(keys) = keys {
        keys
    } else {
        let conversations = history::load_all_conversations(false, None)?;
        loaded_keys = agent::refs::conversation_keys_from_conversations(&conversations)?;
        &loaded_keys
    };
    let resolved_refs = refs
        .iter()
        .map(|reference| {
            agent::refs::resolve_conversation_ref(keys, &reference.conversation)
                .map(|resolved| (reference.clone(), resolved))
        })
        .collect::<Result<Vec<_>>>()?;
    let focus = args
        .focus
        .as_deref()
        .map(agent::refs::parse_focus_ref)
        .transpose()?;
    if let Some(focus) = &focus {
        let focus_conversation = focus
            .conversation
            .as_ref()
            .map(|conversation| agent::refs::resolve_conversation_ref(keys, conversation))
            .transpose()?;
        agent::refs::validate_resolved_focus_in_ranges(
            &resolved_refs,
            focus,
            focus_conversation.as_ref(),
        )?;
    }
    Ok((resolved_refs, focus))
}

fn agent_protocol_options(
    no_budget: bool,
    budget: usize,
    tools: bool,
    tool_results: bool,
    thinking: bool,
    subagents: bool,
) -> agent::protocol::ProtocolOptions {
    agent::protocol::ProtocolOptions {
        budget: (!no_budget).then_some(budget),
        tools,
        tool_results,
        thinking,
        subagents,
    }
}

fn lexically_rank_scoped(
    conversations: &[history::Conversation],
    query: &str,
    scoped: &[usize],
) -> Vec<usize> {
    let searchable = search::precompute_agent_search_text(conversations);
    let ranked_all = search::agent_search(conversations, &searchable, query, chrono::Local::now());
    let scoped_set = scoped
        .iter()
        .copied()
        .collect::<std::collections::HashSet<_>>();
    ranked_all
        .into_iter()
        .filter(|index| scoped_set.contains(index))
        .collect()
}

pub(crate) fn resolve_agent_conversation_arg(
    reference: &str,
    keys: Option<&[agent::refs::AgentConversationKey]>,
) -> Result<agent::refs::ResolvedConversation> {
    let loaded_keys;
    let keys = if let Some(keys) = keys {
        keys
    } else {
        let conversations = history::load_all_conversations(false, None)?;
        loaded_keys = agent::refs::conversation_keys_from_conversations(&conversations)?;
        &loaded_keys
    };
    agent::refs::resolve_conversation_ref(keys, reference)
}

#[cfg(test)]
pub(crate) fn run_agent_read(
    args: &AgentReadArgs,
    keys: Option<&[agent::refs::AgentConversationKey]>,
) -> Result<String> {
    AgentService::default().run_read(args, keys)
}

#[cfg(test)]
pub(crate) fn run_agent_outline(
    args: &AgentOutlineArgs,
    keys: Option<&[agent::refs::AgentConversationKey]>,
) -> Result<String> {
    AgentService::default().run_outline(args, keys)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::test_support::user_jsonl_line;
    use crate::cli::AgentOutputFlags;

    fn output_flags() -> AgentOutputFlags {
        AgentOutputFlags {
            budget: 6000,
            no_budget: false,
            tools: false,
            tool_results: false,
            thinking: false,
            subagents: false,
        }
    }

    #[test]
    fn invocation_cache_reuses_loaded_target_transcript() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("session.jsonl");
        std::fs::write(&path, user_jsonl_line("cached message")).unwrap();
        let key = agent::refs::AgentConversationKey::new("project", "session.jsonl", path.clone());
        let reference = key.conversation_ref().canonical();
        let args = AgentReadArgs {
            refs: vec![format!("{reference}:m1")],
            focus: None,
            lines: None,
            match_query: None,
            context: 3,
            output: output_flags(),
        };
        let service = AgentService::default();

        assert!(
            service
                .run_read(&args, Some(std::slice::from_ref(&key)))
                .is_ok()
        );
        std::fs::write(&path, "{malformed").unwrap();
        let output = service
            .run_read(&args, Some(std::slice::from_ref(&key)))
            .unwrap();

        assert!(output.contains("cached message"));
    }

    #[test]
    fn malformed_target_is_a_typed_service_failure() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("session.jsonl");
        std::fs::write(&path, "{malformed").unwrap();
        let key = agent::refs::AgentConversationKey::new("project", "session.jsonl", path);
        let reference = key.conversation_ref().canonical();
        let args = AgentOutlineArgs {
            conversation: reference.clone(),
            output: output_flags(),
        };

        let error = AgentService::default()
            .run_outline(&args, Some(std::slice::from_ref(&key)))
            .unwrap_err();
        let AppError::Agent(error) = error else {
            panic!("expected typed agent error");
        };
        assert_eq!(error.kind, AgentErrorKind::MalformedTranscript);
        assert_eq!(error.reference.as_deref(), Some(reference.as_str()));
    }
}
