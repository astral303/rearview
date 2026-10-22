use crate::agent;
use crate::agent::diagnostic::{AgentError, AgentErrorKind, AgentWarning, AgentWarningKind};
use crate::agent::perf::{CommandPerf, Stage};
use crate::agent::progress::{ProgressRecords, ProgressSchedule, run_reporting_progress};
use crate::agent::search::TranscriptRetention;
use crate::cli::{self, AgentCommand, AgentOutlineArgs, AgentReadArgs};
use crate::clock::Clock;
use crate::config;
use crate::config::{AgentConfig, AgentScopeConfig, SemanticConfig};
use crate::error::{AppError, Result};
use crate::history;
use crate::history::load_meter::{AgentMeter, LoadMeter};
use crate::search;
use crate::search::mode::SearchMode;
use crate::semantic;
use crate::semantic::rank::RankedHits;
use crate::semantic::types::{ChunkHits, EmbeddingBudget};
use crate::time_filter::TimeFilter;
use rayon::prelude::*;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

type ResolvedReadRefs = Vec<(agent::refs::ReadRef, agent::refs::ResolvedConversation)>;

/// A `--focus` range and, when it names one, its resolved conversation.
#[derive(Debug)]
pub(crate) struct ResolvedFocus {
    conversation: Option<agent::refs::ResolvedConversation>,
    range: agent::refs::MessageRange,
}

const DEFAULT_OUTPUT_CHARS: usize = 6000;
const DEFAULT_SEARCH_TOP: usize = 10;
const DEFAULT_WITHIN_TOP: usize = 20;
const DEFAULT_SESSIONS_LIMIT: usize = 20;
const DEFAULT_HITS_PER_CONVERSATION: usize = 2;

fn configured_usize(cli_value: Option<usize>, default: usize, configured: Option<usize>) -> usize {
    cli_value.or(configured).unwrap_or(default)
}

fn configured_visibility(cli_value: bool, configured: Option<bool>) -> bool {
    cli_value || configured.unwrap_or(false)
}

fn configured_render_policy(config: &AgentConfig) -> agent::visibility::ContentVisibility {
    agent::visibility::ContentVisibility {
        tools: config.tools.unwrap_or(false),
        tool_results: config.tool_results.unwrap_or(false),
        thinking: config.thinking.unwrap_or(false),
        subagents: config.subagents.unwrap_or(false),
    }
}

fn apply_configured_render_policy(
    output: &mut agent::search::AgentSearchOutput,
    config: &AgentConfig,
) {
    let policy = configured_render_policy(config);
    for hit in &mut output.hits {
        hit.render_options.merge(policy);
    }
    for hit in output
        .groups
        .iter_mut()
        .flat_map(|group| group.hits.iter_mut())
    {
        hit.render_options.merge(policy);
    }
}

fn configured_budget(
    no_budget: bool,
    cli_budget: Option<usize>,
    configured: Option<usize>,
) -> Option<usize> {
    (!no_budget).then(|| configured_usize(cli_budget, DEFAULT_OUTPUT_CHARS, configured))
}

fn configured_embedding_budget(config: &SemanticConfig) -> EmbeddingBudget {
    EmbeddingBudget::seconds(
        config
            .interactive_embedding_seconds
            .unwrap_or(semantic::types::DEFAULT_INTERACTIVE_EMBEDDING_SECONDS),
    )
}

fn configured_scope(
    args: &cli::AgentScopeArgs,
    config: &AgentConfig,
) -> agent::search::AgentSearchScope {
    if args.local {
        agent::search::AgentSearchScope::Local
    } else if args.all {
        agent::search::AgentSearchScope::Global
    } else {
        match config.scope.unwrap_or(AgentScopeConfig::Global) {
            AgentScopeConfig::Global => agent::search::AgentSearchScope::Global,
            AgentScopeConfig::Local => agent::search::AgentSearchScope::Local,
        }
    }
}

/// The sessions this command was launched from, one per agent whose
/// session variable is set. An agent launched from another agent's shell
/// inherits the outer variable, so there can be more than one.
fn current_session_ids() -> Vec<String> {
    history::provider::current_session_variables()
        .filter_map(|name| std::env::var(name).ok())
        .filter(|session_id| !session_id.is_empty())
        .collect()
}

fn is_current_session(session_id: &str, current_session_ids: &[String]) -> bool {
    current_session_ids
        .iter()
        .any(|current| search::session_id_matches(session_id, current))
}

/// The keys `agent.exclude_projects` leaves out: each excluded row's key, and
/// each key without a row whose agent and reference project match an excluded
/// row's key. A key without a row has no project name to match.
fn excluded_key_paths(
    keys: &[agent::refs::AgentConversationKey],
    excluded_row_paths: &HashSet<PathBuf>,
    row_paths: &HashSet<PathBuf>,
) -> HashSet<PathBuf> {
    let excluded_reference_projects: HashSet<(history::Source, &str)> = keys
        .iter()
        .filter(|key| excluded_row_paths.contains(&key.path))
        .map(|key| (key.source, key.project_dir_name.as_str()))
        .collect();
    keys.iter()
        .filter(|key| {
            excluded_row_paths.contains(&key.path)
                || (!row_paths.contains(&key.path)
                    && excluded_reference_projects
                        .contains(&(key.source, key.project_dir_name.as_str())))
        })
        .map(|key| key.path.clone())
        .collect()
}

/// The keys `window` leaves out: each key whose row has no message inside it,
/// and each key without a row whose transcript was last written before it
/// opened. A key without a row that stays in shows its diagnostics.
fn key_paths_outside_window(
    keys: &[agent::refs::AgentConversationKey],
    rows_outside_window: &HashSet<PathBuf>,
    row_paths: &HashSet<PathBuf>,
    window: &TimeFilter,
) -> HashSet<PathBuf> {
    if !window.is_active() {
        return HashSet::new();
    }
    keys.iter()
        .filter(|key| {
            rows_outside_window.contains(&key.path)
                || (!row_paths.contains(&key.path)
                    && transcript_last_write(&key.path)
                        .is_some_and(|last_write| !window.may_hold_a_message_inside(last_write)))
        })
        .map(|key| key.path.clone())
        .collect()
}

/// The window, scope and exclusions `agent search` and `agent sessions`
/// apply, from the command's flags and `[agent]`.
struct SelectionRules {
    window: TimeFilter,
    scope: agent::search::AgentSearchScope,
    excludes_current_session: bool,
    excluded_projects: history::ExcludedProjects,
}

impl SelectionRules {
    fn new(window: TimeFilter, args: &cli::AgentScopeArgs, config: &AgentConfig) -> Self {
        Self {
            window,
            scope: configured_scope(args, config),
            excludes_current_session: !args.include_current_session
                && config.exclude_current_session != Some(false),
            excluded_projects: config.exclude_projects.iter().cloned().collect(),
        }
    }

    /// True when the selection leaves `row` out of every listing and search:
    /// one of `current_session_ids`, in an excluded project, or outside a
    /// local scope's `workspace`.
    fn leaves_out(
        &self,
        row: &history::Conversation,
        current_session_ids: &[String],
        workspace: Option<&history::Workspace>,
    ) -> bool {
        let is_outside_scope = self.scope == agent::search::AgentSearchScope::Local
            && workspace.is_some_and(|workspace| !workspace.contains(row));
        is_current_session(&row.session_id, current_session_ids)
            || self.excluded_projects.excludes(row)
            || is_outside_scope
    }
}

/// The history and environment a selection reads.
struct LoadedSessions {
    history: history::LoadedHistory,
    /// The sessions this command was launched from, when the rules leave them
    /// out; empty otherwise.
    current_session_ids: Vec<String>,
    /// The current workspace, for a local scope.
    workspace: Option<history::Workspace>,
}

impl LoadedSessions {
    /// Loads the history without rereading a changed session whose cached
    /// row `rules` leave out.
    fn load(
        rules: &SelectionRules,
        perf: &CommandPerf,
        environment: &history::Environment,
    ) -> Result<Self> {
        // Leave out the session this command runs in: the caller already
        // holds its text, and listing it or a hit in it would spend the
        // output budget on that text.
        let current_session_ids = if rules.excludes_current_session {
            current_session_ids()
        } else {
            Vec::new()
        };
        let workspace = (rules.scope == agent::search::AgentSearchScope::Local)
            .then(|| history::Workspace::current().ok())
            .flatten();
        let leaves_out = |row: &history::Conversation| {
            rules.leaves_out(row, &current_session_ids, workspace.as_ref())
        };
        let history = perf.time(Stage::Load, || {
            history::load_history(
                environment,
                false,
                None,
                Some(perf.load_meter()),
                history::provider::LeftOutRows::of(&leaves_out),
            )
        })?;
        Ok(Self {
            history,
            current_session_ids,
            workspace,
        })
    }
}

/// The sessions `agent search` and `agent sessions` cover, with warnings
/// about the sessions they could not cover.
struct SessionSelection {
    /// The rows with a message inside the window, newest first, without the
    /// current session and excluded projects. Rows outside the scope stay.
    conversations: Vec<history::Conversation>,
    /// The indices into `conversations` inside the scope.
    scoped: Vec<usize>,
    /// Each row's key, and each key without a row, inside the window and the
    /// scope and not excluded.
    keys: Vec<agent::refs::AgentConversationKey>,
    warnings: Vec<AgentWarning>,
}

impl SessionSelection {
    /// The rows and keys of `loaded` that `rules` keep. Does not read a
    /// transcript: a key without a row is placed in the window by its file's
    /// modification time, and the skipped-transcript warnings are added by
    /// the caller.
    fn from_loaded(
        loaded: LoadedSessions,
        rules: &SelectionRules,
        perf: &CommandPerf,
    ) -> Result<Self> {
        let LoadedSessions {
            history:
                history::LoadedHistory {
                    mut conversations,
                    ignored,
                    discovered,
                    failures,
                },
            current_session_ids,
            workspace,
        } = loaded;
        let row_paths: HashSet<PathBuf> = conversations
            .iter()
            .map(|conversation| conversation.path.clone())
            .collect();
        let excluded_row_paths: HashSet<PathBuf> = conversations
            .iter()
            .filter(|conversation| rules.excluded_projects.excludes(conversation))
            .map(|conversation| conversation.path.clone())
            .collect();
        let rows_outside_window: HashSet<PathBuf> = conversations
            .iter()
            .filter(|conversation| !conversation.message_times.has_message_within(&rules.window))
            .map(|conversation| conversation.path.clone())
            .collect();
        conversations.retain(|conversation| {
            !excluded_row_paths.contains(&conversation.path)
                && !is_current_session(&conversation.session_id, &current_session_ids)
                && !rows_outside_window.contains(&conversation.path)
        });
        conversations.sort_by_key(|conversation| std::cmp::Reverse(conversation.timestamp));
        let scoped = agent::search::scoped_conversation_inputs(
            &conversations,
            rules.scope,
            workspace.as_ref(),
        )?;
        let (mut keys, mut warnings) = perf.time(Stage::Discovery, || {
            loaded_agent_keys(&discovered, &failures, workspace.as_ref())
        });
        // Remove the keys of excluded rows and of rows outside the window too:
        // a key without a row is reported as a skipped transcript.
        let excluded_key_paths = excluded_key_paths(&keys, &excluded_row_paths, &row_paths);
        let key_paths_outside_window =
            key_paths_outside_window(&keys, &rows_outside_window, &row_paths, &rules.window);
        keys.retain(|key| {
            !excluded_key_paths.contains(&key.path)
                && !key_paths_outside_window.contains(&key.path)
                && !is_current_session(&key.session_id, &current_session_ids)
        });
        warnings.extend(
            ignored
                .iter()
                .map(|term| AgentWarning::ignored(term.to_string())),
        );
        Ok(Self {
            conversations,
            scoped,
            keys,
            warnings,
        })
    }

    /// The page of the scoped sessions `agent sessions` lists.
    fn listing(
        &self,
        window: &TimeFilter,
        offset: usize,
        limit: usize,
    ) -> Result<agent::sessions::SessionListing> {
        let inputs = agent_inputs_for_indices(&self.conversations, &self.keys, &self.scoped)?;
        Ok(agent::sessions::list_sessions(
            inputs
                .iter()
                .map(|input| (input.conversation, &input.resolved)),
            window,
            offset,
            limit,
        ))
    }
}

/// Runs one agent command. Created per command: `perf` times the command
/// from the service's creation.
pub struct AgentService {
    transcripts: RefCell<
        HashMap<PathBuf, std::result::Result<agent::transcript::AgentTranscript, AgentError>>,
    >,
    bounded_transcripts:
        RefCell<HashMap<(PathBuf, usize, usize), agent::transcript::AgentTranscript>>,
    perf: CommandPerf,
    #[cfg(test)]
    transcript_parse_count: std::cell::Cell<usize>,
    environment: history::Environment,
}

pub fn execute(command: AgentCommand, environment: history::Environment) -> Result<String> {
    AgentService::new(environment).execute(command)
}

/// `key`'s transcript read through the cache under `base` or from disk, its
/// failure worded for the agent protocol.
fn read_agent_transcript(
    base: Option<&history::CacheBase>,
    key: &agent::refs::AgentConversationKey,
) -> std::result::Result<agent::transcript::AgentTranscript, AgentError> {
    let path: &Path = &key.path;
    agent::transcript::AgentTranscript::load_owned(base, key.source, path, &key.subagents).map_err(
        |error| match error {
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
        },
    )
}

/// [`read_agent_transcript`] as a search's transcript loader.
fn load_agent_transcript(
    base: Option<&history::CacheBase>,
    key: &agent::refs::AgentConversationKey,
) -> Result<agent::transcript::AgentTranscript> {
    read_agent_transcript(base, key).map_err(AppError::from)
}

impl AgentService {
    pub fn new(environment: history::Environment) -> Self {
        Self {
            transcripts: RefCell::default(),
            bounded_transcripts: RefCell::default(),
            perf: CommandPerf::new(Arc::clone(environment.clock())),
            #[cfg(test)]
            transcript_parse_count: std::cell::Cell::default(),
            environment,
        }
    }

    fn clock(&self) -> &Arc<dyn Clock> {
        self.environment.clock()
    }

    fn load_transcript_for_range(
        &self,
        key: &agent::refs::AgentConversationKey,
        range: Option<agent::refs::MessageRange>,
    ) -> Result<agent::transcript::AgentTranscript> {
        let Some(range) = range else {
            return self.load_transcript(key);
        };
        if self.transcripts.borrow().contains_key(&key.path) {
            return self.load_transcript(key);
        }
        let cache_key = (key.path.clone(), range.start, range.end);
        if let Some(transcript) = self.bounded_transcripts.borrow().get(&cache_key) {
            return Ok(transcript.clone());
        }
        if let Ok(Some(cached)) = history::sqlite_cache::find_cached_session_waiting(
            self.environment.cache_base(),
            key.source,
            &key.path,
            &key.subagents,
        ) && let Ok(transcript) = self.perf.time_transcript_parse(key, || {
            cached
                .agent_range(&key.path, range)?
                .ok_or_else(|| AppError::SessionCacheNotReady {
                    session: key.session_id.clone(),
                    reason: "cache no longer available".to_owned(),
                })
        }) {
            #[cfg(test)]
            self.transcript_parse_count
                .set(self.transcript_parse_count.get() + 1);
            self.bounded_transcripts
                .borrow_mut()
                .insert(cache_key, transcript.clone());
            return Ok(transcript);
        }
        self.load_transcript(key)
    }

    /// Runs `command`, writing `protocol agent-progress` records to stderr
    /// while it runs and, with `--perf-stats`, its `perf` records after.
    pub fn execute(&self, command: AgentCommand) -> Result<String> {
        let output = run_reporting_progress(
            ProgressRecords::new(command.name(), &self.perf, ProgressSchedule::DEFAULT),
            std::io::stderr(),
            || self.execute_inner(&command),
        );
        if command.reports_perf_stats() {
            eprint!("{}", self.perf.records(agent::peak_memory::peak_bytes()));
        }
        output
            .and_then(ensure_complete_compact_output)
            .map_err(|error| {
                let error = structured_agent_error(error);
                if let AppError::Agent(agent_error) = error {
                    AppError::AgentProtocol(agent::diagnostic::format_error(&agent_error))
                } else {
                    error
                }
            })
    }

    fn execute_inner(&self, command: &AgentCommand) -> Result<String> {
        match command {
            AgentCommand::Sessions(args) => self.run_sessions(args),
            AgentCommand::Search(args) => self.search(args),
            AgentCommand::Within(args) => self.run_within(args),
            AgentCommand::Read(args) => self.run_read(args, None),
            AgentCommand::Outline(args) => self.run_outline(args, None),
        }
    }

    fn load_transcript(
        &self,
        key: &agent::refs::AgentConversationKey,
    ) -> Result<agent::transcript::AgentTranscript> {
        let path: &Path = &key.path;
        if let Some(cached) = self.transcripts.borrow().get(path) {
            return cached.clone().map_err(AppError::from);
        }
        #[cfg(test)]
        self.transcript_parse_count
            .set(self.transcript_parse_count.get() + 1);
        let loaded = self.perf.time_transcript_parse(key, || {
            read_agent_transcript(self.environment.cache_base(), key)
        });
        self.transcripts
            .borrow_mut()
            .insert(path.to_path_buf(), loaded.clone());
        loaded.map_err(AppError::from)
    }

    /// Keeps `transcripts` for later reads in this command.
    fn keep_transcripts(
        &self,
        transcripts: Vec<(
            agent::refs::AgentConversationKey,
            agent::transcript::AgentTranscript,
        )>,
    ) {
        self.transcripts.borrow_mut().extend(
            transcripts
                .into_iter()
                .map(|(key, transcript)| (key.path, Ok(transcript))),
        );
    }

    fn search(&self, args: &cli::AgentSearchArgs) -> Result<String> {
        let perf = &self.perf;
        let reads = perf.search_reads();
        let cache_base = self.environment.cache_base();
        let load_transcript = |key: &agent::refs::AgentConversationKey| {
            reads.time_read(key, || load_agent_transcript(cache_base, key))
        };
        let config = config::load_config()?;
        let search_config = config.search.unwrap_or_default();
        let agent_config = config.agent.unwrap_or_default();
        let embedding_budget = configured_embedding_budget(&config.semantic.unwrap_or_default());
        // Resolved before loading so an inverted range fails without paying for
        // a full corpus parse.
        let time = args.time.resolve(self.clock().as_ref())?;
        let SessionSelection {
            conversations,
            scoped,
            keys,
            warnings: base_warnings,
        } = self.select_sessions(&SelectionRules::new(time, &args.scope, &agent_config))?;
        let request = agent::search::AgentSearchRequest {
            query: args.query.clone(),
            top: configured_usize(args.top, DEFAULT_SEARCH_TOP, agent_config.top),
            cli_mode: args.mode_override(),
            config_mode: agent_config.mode.or(search_config.mode),
            tui_semantic_search: None,
            flat: args.flat,
            hits_per_conversation: configured_usize(
                args.hits_per_conv,
                DEFAULT_HITS_PER_CONVERSATION,
                agent_config.hits_per_conversation,
            ),
            retrieval_hits_per_conversation: None,
            all_hits: args.all_hits,
            budget: configured_budget(args.no_budget, args.budget, agent_config.output_chars),
            session_offset: args.offset,
            window: time,
        };
        let mode = request.mode();
        let clock = self.clock().as_ref();
        let rank = || {
            perf.time(Stage::Ranking, || {
                lexically_rank_scoped(&conversations, &args.query, &scoped, mode, &time, clock)
            })
        };
        let warnings = RefCell::new(base_warnings);
        let report_read_error = |key: &agent::refs::AgentConversationKey, error: &AppError| {
            warnings.borrow_mut().push(AgentWarning::from_app_error(
                error,
                Some(
                    &agent::refs::resolved_conversation_for_key(&keys, key)
                        .reference
                        .canonical(),
                ),
            ));
        };
        let search_transcripts = |request: &agent::search::AgentSearchRequest,
                                  retention: TranscriptRetention|
         -> Result<agent::search::LexicalSearchRun> {
            let ranked = rank()?;
            let run = perf.time(Stage::TranscriptSearch, || {
                agent::search::run_global_lexical_search_reporting(
                    request,
                    &conversations,
                    &keys,
                    &ranked,
                    retention,
                    load_transcript,
                    Some(reads),
                )
            })?;
            for (key, error) in &run.failed_reads {
                report_read_error(key, error);
            }
            Ok(run)
        };
        match mode {
            SearchMode::Lexical | SearchMode::Exact => {
                let run = search_transcripts(&request, TranscriptRetention::DropAfterSearch)?;
                let mut output = run.output;
                apply_configured_render_policy(&mut output, &agent_config);
                Ok(perf.time(Stage::Render, || {
                    agent::search::format_agent_output_with_warnings(
                        &output,
                        &warnings.into_inner(),
                        clock.wall(),
                    )
                }))
            }
            SearchMode::Semantic => {
                let (mut output, semantic_warnings) = perf.time(Stage::Semantic, || {
                    run_agent_semantic_search(
                        self,
                        &request,
                        &conversations,
                        &keys,
                        &scoped,
                        embedding_budget,
                    )
                })?;
                apply_configured_render_policy(&mut output, &agent_config);
                let mut warnings = warnings.into_inner();
                warnings.extend(semantic_warnings);
                Ok(perf.time(Stage::Render, || {
                    agent::search::format_agent_output_with_warnings(
                        &output,
                        &warnings,
                        clock.wall(),
                    )
                }))
            }
            SearchMode::Hybrid => {
                let inputs = agent_inputs_for_indices(&conversations, &keys, &scoped)?;
                let (semantic, lexical) = run_hybrid_halves(
                    || {
                        perf.time(Stage::Semantic, || {
                            run_agent_semantic_hits(
                                &args.query,
                                &inputs,
                                embedding_budget,
                                request.semantic_chunk_hits(),
                                request.window,
                                self.clock().clone(),
                            )
                        })
                    },
                    |pass| match pass {
                        HybridLexicalPass::ForFusion => search_transcripts(
                            &agent::search::hybrid_lexical_request(&request),
                            TranscriptRetention::KeepWithHits,
                        ),
                        HybridLexicalPass::Alone => {
                            search_transcripts(&request, TranscriptRetention::DropAfterSearch)
                        }
                    },
                )?;
                let mut output = match semantic {
                    Ok((semantic, semantic_warnings)) => {
                        warnings.borrow_mut().extend(semantic_warnings);
                        self.keep_transcripts(lexical.kept_transcripts);
                        let mut output = agent::search::run_global_hybrid_search(
                            &request,
                            lexical.output,
                            &semantic,
                            &inputs,
                        );
                        perf.time(Stage::HitMetadata, || {
                            attach_input_transcript_metadata(self, &mut output, &inputs)
                        });
                        output
                    }
                    Err(error) => {
                        warnings
                            .borrow_mut()
                            .push(AgentWarning::from_app_error(&error, None));
                        let mut output = lexical.output;
                        output.mode = SearchMode::Hybrid;
                        output
                    }
                };
                apply_configured_render_policy(&mut output, &agent_config);
                Ok(perf.time(Stage::Render, || {
                    agent::search::format_agent_output_with_warnings(
                        &output,
                        &warnings.into_inner(),
                        clock.wall(),
                    )
                }))
            }
        }
    }

    /// Loads the history and the environment, applies `rules`, and adds the
    /// warnings for the selection's skipped and malformed transcripts.
    fn select_sessions(&self, rules: &SelectionRules) -> Result<SessionSelection> {
        let perf = &self.perf;
        let loaded = LoadedSessions::load(rules, perf, &self.environment)?;
        let mut selection = SessionSelection::from_loaded(loaded, rules, perf)?;
        let skipped = perf.time(Stage::SkippedTranscripts, || {
            warnings_for_skipped_transcripts(self, &selection.conversations, &selection.keys)
        });
        selection.warnings.extend(skipped);
        Ok(selection)
    }

    fn run_sessions(&self, args: &cli::AgentSessionsArgs) -> Result<String> {
        let agent_config = config::load_config()?.agent.unwrap_or_default();
        let rules = SelectionRules::new(
            args.time.resolve(self.clock().as_ref())?,
            &args.scope,
            &agent_config,
        );
        let selection = self.select_sessions(&rules)?;
        self.perf.time(Stage::Render, || {
            let listing = selection.listing(
                &rules.window,
                args.offset,
                args.limit.unwrap_or(DEFAULT_SESSIONS_LIMIT),
            )?;
            Ok(agent::sessions::format_sessions_with_warnings(
                &listing,
                configured_budget(args.no_budget, args.budget, agent_config.output_chars),
                &selection.warnings,
                self.clock().wall(),
            ))
        })
    }

    fn run_within(&self, args: &cli::AgentWithinArgs) -> Result<String> {
        let config = config::load_config()?;
        let search_config = config.search.unwrap_or_default();
        let agent_config = config.agent.unwrap_or_default();
        let window = args.time.resolve(self.clock().as_ref())?;
        let perf = &self.perf;
        let (keys, _) = perf.time(Stage::Keys, || {
            discover_agent_keys(&self.environment, perf.load_meter())
        })?;
        let resolved = resolve_agent_conversation_arg(&args.conversation, &keys)?;
        let transcript = perf
            .time(Stage::Transcript, || self.load_transcript(&resolved.key))
            .map_err(|error| target_error(error, &resolved))?;
        let mut warnings = transcript_warning(&transcript, &resolved.reference.canonical())
            .into_iter()
            .collect::<Vec<_>>();
        let request = agent::search::AgentWithinRequest {
            query: args.query.clone(),
            top: configured_usize(args.top, DEFAULT_WITHIN_TOP, agent_config.within_top),
            cli_mode: args.mode_override(),
            config_mode: agent_config.mode.or(search_config.mode),
            tui_semantic_search: None,
            budget: configured_budget(args.no_budget, args.budget, agent_config.output_chars),
            window,
        };
        let mode = agent::search::effective_agent_mode(
            &request.query,
            request.cli_mode,
            request.config_mode,
            request.tui_semantic_search,
        );
        let mut output = perf.time(Stage::Search, || -> Result<_> {
            let conversation = conversation_from_agent_transcript(
                &transcript,
                &resolved.key,
                self.clock().as_ref(),
            );
            Ok(match mode {
                SearchMode::Lexical | SearchMode::Exact => agent::search::run_within_search(
                    &request,
                    &conversation,
                    &resolved,
                    &transcript,
                    &[],
                ),
                SearchMode::Semantic => {
                    let (output, semantic_warnings) = run_agent_within_semantic(
                        &request,
                        &conversation,
                        &resolved,
                        &transcript,
                        SemanticToolContent::All,
                        self.clock().clone(),
                    )?;
                    warnings.extend(semantic_warnings);
                    output
                }
                SearchMode::Hybrid => match run_agent_within_semantic(
                    &request,
                    &conversation,
                    &resolved,
                    &transcript,
                    SemanticToolContent::MatchingQuery(&request.query),
                    self.clock().clone(),
                ) {
                    Ok((output, semantic_warnings)) => {
                        warnings.extend(semantic_warnings);
                        output
                    }
                    Err(error) => {
                        warnings.push(AgentWarning::from_app_error(&error, None));
                        let mut output = agent::search::run_within_search(
                            &agent::search::AgentWithinRequest {
                                cli_mode: Some(SearchMode::Lexical),
                                ..request.clone()
                            },
                            &conversation,
                            &resolved,
                            &transcript,
                            &[],
                        );
                        output.mode = SearchMode::Hybrid;
                        output
                    }
                },
            })
        })?;
        perf.time(Stage::Render, || {
            agent::search::attach_transcript_metadata(&mut output, &resolved, &transcript);
            apply_configured_render_policy(&mut output, &agent_config);
            Ok(agent::search::format_agent_output_with_warnings(
                &output,
                &warnings,
                self.clock().wall(),
            ))
        })
    }
}

/// Keys for every provider's sessions, sorted by path.
///
/// A sub-agent transcript gets no key: discovery names it on its session's
/// stub, its content is reachable through that session, and a key of its own
/// would resolve to a row that does not exist.
///
/// A provider whose sessions cannot be listed contributes a warning rather
/// than vanishing: the other providers stay searchable, and the caller can
/// tell a broken store from an absent one.
///
/// `meter` records what each provider's discovery and cache read.
fn discover_agent_keys(
    environment: &history::Environment,
    meter: &LoadMeter,
) -> Result<(Vec<agent::refs::AgentConversationKey>, Vec<AgentWarning>)> {
    let mut keys = Vec::new();
    let mut warnings = Vec::new();
    for &storage in environment.storages() {
        let provider = storage.source().provider();
        match provider_keys(environment, storage, meter.agent(storage.source())) {
            Ok(candidates) => keys.extend(candidates),
            Err(error) => warnings.push(listing_failure_warning(provider, &error)),
        }
    }
    if keys.is_empty() && !any_history_root_exists(environment) {
        return Err(AgentError::io(
            None,
            format!(
                "no {} history storage is available",
                history::provider::display_names_in_prose()
            ),
        )
        .into());
    }
    keys.sort_by(|left, right| left.path.cmp(&right.path));
    Ok((keys, warnings))
}

/// [`discover_agent_keys`] from the sessions a load discovered and the
/// directory and id it recorded of each, so a search after a load neither
/// discovers the sessions nor reads the cache again. With a `workspace`, only
/// the sessions each agent's own rule places in it, by the directory the load
/// recorded, as [`history::Workspace::contains`] places their rows.
fn loaded_agent_keys(
    discovered: &[(history::Source, Vec<history::provider::DiscoveredSession>)],
    failures: &[(history::Source, AppError)],
    workspace: Option<&history::Workspace>,
) -> (Vec<agent::refs::AgentConversationKey>, Vec<AgentWarning>) {
    let mut keys = discovered
        .iter()
        .flat_map(|(source, sessions)| {
            let provider = source.provider();
            sessions
                .iter()
                .filter(move |session| {
                    let recorded_dir = session
                        .recorded
                        .as_ref()
                        .and_then(|recorded| recorded.project_dir.as_deref());
                    workspace.is_none_or(|workspace| {
                        provider.is_in_workspace(workspace, &session.stub.locator, recorded_dir)
                    })
                })
                .filter_map(move |session| {
                    agent::refs::loaded_session_key(
                        provider,
                        &session.stub,
                        session.recorded.as_ref(),
                    )
                })
        })
        .collect::<Vec<_>>();
    keys.sort_by(|left, right| left.path.cmp(&right.path));
    let warnings = failures
        .iter()
        .map(|(source, error)| listing_failure_warning(source.provider(), error))
        .collect();
    (keys, warnings)
}

fn any_history_root_exists(environment: &history::Environment) -> bool {
    environment.storages().iter().any(|storage| {
        storage
            .roots()
            .is_ok_and(|roots| roots.iter().any(|root| root.path.exists()))
    })
}

fn listing_failure_warning(
    provider: &dyn history::provider::SessionProvider,
    error: &AppError,
) -> AgentWarning {
    AgentWarning {
        kind: AgentWarningKind::Io,
        reference: None,
        detail: format!(
            "failed to list {} sessions: {error}",
            provider.labels().display
        ),
    }
}

/// Every key under every root `storage` stores sessions in.
fn provider_keys(
    environment: &history::Environment,
    storage: &dyn history::provider::SessionStorage,
    meter: AgentMeter,
) -> Result<Vec<agent::refs::AgentConversationKey>> {
    let mut keys = Vec::new();
    for root in storage.roots()? {
        keys.extend(root_keys(environment, storage, &root, meter)?);
    }
    Ok(keys)
}

/// Keys for the sessions a fresh discovery lists under `root`, not for the
/// cached sessions: a cached session can outlive its transcripts.
///
/// A session whose path names its key is keyed without being read. Every
/// other session takes the directory and id it recorded from the session
/// cache, which a search has just refreshed by loading and which read and
/// outline reuse from the previous run, so a corpus that can run to
/// gigabytes is not parsed again. The cache is read only for such a
/// session.
fn root_keys(
    environment: &history::Environment,
    storage: &dyn history::provider::SessionStorage,
    root: &history::provider::SessionRoot,
    meter: AgentMeter,
) -> Result<Vec<agent::refs::AgentConversationKey>> {
    let provider = storage.source().provider();
    let clock = environment.clock().as_ref();
    let recorded_sessions = std::cell::OnceCell::new();
    let cached = || {
        recorded_sessions
            .get_or_init(|| cached_recorded_sessions(environment, storage, root, meter))
    };
    meter.discovering();
    let discovering = clock.now();
    let stubs = storage.discover(root, environment.lister())?.stubs;
    meter.discovered(1, stubs.len(), clock.elapsed_since(discovering));
    Ok(stubs
        .iter()
        .filter_map(|stub| {
            let recorded = if agent::refs::is_keyed_by_path(provider, &stub.locator) {
                None
            } else {
                recorded_session(storage, stub, cached(), meter, clock)
            };
            agent::refs::session_key(provider, stub, recorded.as_ref())
        })
        .collect())
}

/// The session cache database's record of each session `storage` keeps
/// under `root`, by cache key, as
/// [`history::sqlite_cache::SqliteCache::recorded_sessions`] reads it; empty
/// without a database holding the storage's sessions, or when the read
/// fails. `meter` records the read.
fn cached_recorded_sessions(
    environment: &history::Environment,
    storage: &dyn history::provider::SessionStorage,
    root: &history::provider::SessionRoot,
    meter: AgentMeter,
) -> HashMap<String, Option<history::provider::RecordedSession>> {
    let Some(base) = environment.cache_base() else {
        return HashMap::new();
    };
    meter.reading_cache();
    let reading = environment.clock().now();
    let recorded = history::sqlite_cache::SqliteCache::open_waiting(base)
        .and_then(|cache| match cache {
            Some(cache) if cache.storage_is_current(storage.cache())? => {
                cache.recorded_sessions(storage.cache(), &root.path)
            }
            _ => Ok(HashMap::new()),
        })
        .unwrap_or_default();
    meter.cache_read(recorded.len(), environment.clock().elapsed_since(reading));
    recorded
}

/// The directory and id the session at `stub` recorded, from what `cached`
/// holds of it, or parsed the way the first load would parse it when the
/// database holds no session of the root. `None` when the session holds no
/// conversation, or is missing from a database that holds other sessions of
/// the root: a session the load could not read is never recorded, and
/// parsing it again on every call would find nothing new. `meter` records
/// the parse, timed on `clock`.
fn recorded_session(
    storage: &dyn history::provider::SessionStorage,
    stub: &history::provider::SessionStub,
    cached: &HashMap<String, Option<history::provider::RecordedSession>>,
    meter: AgentMeter,
    clock: &dyn Clock,
) -> Option<history::provider::RecordedSession> {
    if cached.is_empty() {
        let parsing = clock.now();
        let projection = history::format::parse_owned_transcript(storage.source(), &stub.locator);
        meter.session_parsed(1, own_transcript_bytes(stub), clock.elapsed_since(parsing));
        return projection
            .ok()
            .flatten()
            .map(|projection| history::provider::RecordedSession {
                project_dir: Some(projection.header.cwd),
                session_id: projection.header.id,
            });
    }
    cached.get(&stub.cache_key)?.clone()
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
                reference: key.as_ref().map(|key| {
                    agent::refs::resolved_conversation_for_key(keys, key)
                        .reference
                        .canonical()
                }),
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
        let reference = agent::refs::resolved_conversation_for_key(keys, key)
            .reference
            .canonical();
        match service.load_transcript(key) {
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

/// The row of `transcript`, dated by its file's modification time, or by
/// `clock`'s current time when the file cannot be inspected.
fn conversation_from_agent_transcript(
    transcript: &agent::transcript::AgentTranscript,
    key: &agent::refs::AgentConversationKey,
    clock: &dyn Clock,
) -> history::Conversation {
    let message_text = transcript
        .messages
        .iter()
        .map(|message| {
            message
                .parts
                .iter()
                .filter_map(agent::transcript::bounded_agent_part_text)
                .collect::<Vec<_>>()
                .join(" ")
        })
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>();
    let preview = message_text
        .iter()
        .take(3)
        .cloned()
        .collect::<Vec<_>>()
        .join(" ... ");
    let preview_last = message_text
        .iter()
        .rev()
        .take(3)
        .cloned()
        .collect::<Vec<_>>()
        .join(" ... ");
    let full_text = message_text.join(" ");
    let timestamp = std::fs::metadata(&transcript.path)
        .and_then(|metadata| metadata.modified())
        .map(chrono::DateTime::<chrono::Local>::from)
        .unwrap_or_else(|_| clock.wall().with_timezone(&chrono::Local));
    let semantic_route_text = history::semantic_route_text(&full_text, "");
    history::Conversation {
        source: key.source,
        subagents: Vec::new(),
        session_id: key.session_id.clone(),
        path: transcript.path.clone(),
        index: 0,
        timestamp,
        preview: preview.clone(),
        preview_first: preview,
        preview_last,
        search_text_lower: crate::search::normalize_for_search(&full_text),
        full_text,
        agent_search_text: String::new(),
        thinking_and_tool_call_text: String::new(),
        semantic_route_text,
        semantic_turns: Vec::new(),
        semantic_turn_ranges: Vec::new(),
        project_name: None,
        project_path: None,
        cwd: None,
        message_count: transcript.messages.len(),
        message_times: history::MessageTimes::of_messages(&transcript.messages),
        message_text_starts: history::MessageTextStarts::default(),
        assistant_messages: transcript
            .messages
            .iter()
            .filter(|message| message.role == agent::transcript::AgentMessageRole::Assistant)
            .count(),
        parse_errors: Vec::new(),
        summary: transcript.summary.clone(),
        custom_title: transcript.custom_title.clone(),
        model: None,
        total_tokens: 0,
        duration_minutes: None,
        fingerprint: None,
        has_transient_read_error: false,
        is_missing_earlier_turns: false,
        cached_session: None,
    }
}

fn transcript_warning(
    transcript: &agent::transcript::AgentTranscript,
    reference: &str,
) -> Option<AgentWarning> {
    transcript
        .malformed_warning_detail()
        .map(|detail| AgentWarning {
            kind: AgentWarningKind::MalformedTranscript,
            reference: Some(reference.to_string()),
            detail,
        })
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

fn ensure_complete_compact_output(output: String) -> Result<String> {
    if output.ends_with('\n') {
        Ok(output)
    } else {
        Err(AgentError::new(
            AgentErrorKind::BudgetTooSmall,
            None,
            "output budget cannot fit a complete compact record; increase --budget",
        )
        .into())
    }
}

/// The size of `stub`'s own transcript, without its sub-agents'. A locator
/// that names no file, such as a database row, counts its fingerprint's
/// size.
fn own_transcript_bytes(stub: &history::provider::SessionStub) -> u64 {
    std::fs::metadata(&stub.locator).map_or(stub.fingerprint.size, |metadata| metadata.len())
}

/// When the transcript at `path` was last written. `None` when the file cannot
/// be inspected, so callers keep it rather than silently dropping it.
fn transcript_last_write(path: &std::path::Path) -> Option<chrono::DateTime<chrono::Local>> {
    std::fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .map(chrono::DateTime::<chrono::Local>::from)
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
        AppError::TimeFilter(error) => {
            AgentError::out_of_range(Some("command"), error.to_string()).into()
        }
        error => AgentError::io(None, error.to_string()).into(),
    }
}

fn run_agent_semantic_search(
    service: &AgentService,
    request: &agent::search::AgentSearchRequest,
    conversations: &[history::Conversation],
    keys: &[agent::refs::AgentConversationKey],
    indices: &[usize],
    embedding_budget: EmbeddingBudget,
) -> Result<(agent::search::AgentSearchOutput, Vec<AgentWarning>)> {
    let inputs = agent_inputs_for_indices(conversations, keys, indices)?;
    let (semantic, warnings) = run_agent_semantic_hits(
        &request.query,
        &inputs,
        embedding_budget,
        request.semantic_chunk_hits(),
        request.window,
        service.clock().clone(),
    )?;
    let mut output = agent::search::run_global_semantic_search(request, &inputs, &semantic);
    attach_input_transcript_metadata(service, &mut output, &inputs);
    Ok((output, warnings))
}

fn attach_input_transcript_metadata(
    service: &AgentService,
    output: &mut agent::search::AgentSearchOutput,
    inputs: &[agent::search::AgentConversationInput<'_>],
) {
    let referenced = output
        .hits
        .iter()
        .chain(output.groups.iter().flat_map(|group| group.hits.iter()))
        .map(|hit| hit.conversation_ref.clone())
        .collect::<std::collections::HashSet<_>>();
    for input in inputs {
        if !referenced.contains(input.resolved.reference.canonical().as_str()) {
            continue;
        }
        if let Ok(transcript) = service.load_transcript(&input.resolved.key) {
            agent::search::attach_transcript_metadata(output, &input.resolved, &transcript);
        }
    }
}

/// The lexical pass a hybrid search runs after its semantic half.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HybridLexicalPass {
    /// Deep, to fuse with the semantic hits.
    ForFusion,
    /// The user's own request, after the semantic half failed.
    Alone,
}

/// Runs a hybrid search's semantic half first, since it reads no
/// transcripts, then one lexical pass, so each transcript is read at most
/// once. The semantic half's error is returned with the lexical result, not
/// raised.
fn run_hybrid_halves<S, L>(
    semantic: impl FnOnce() -> Result<S>,
    lexical: impl FnOnce(HybridLexicalPass) -> Result<L>,
) -> Result<(Result<S>, L)> {
    let semantic = semantic();
    let pass = if semantic.is_ok() {
        HybridLexicalPass::ForFusion
    } else {
        HybridLexicalPass::Alone
    };
    Ok((semantic, lexical(pass)?))
}

fn run_agent_semantic_hits(
    query: &str,
    inputs: &[agent::search::AgentConversationInput<'_>],
    embedding_budget: EmbeddingBudget,
    chunk_hits: ChunkHits,
    window: TimeFilter,
    clock: Arc<dyn Clock>,
) -> Result<(RankedHits, Vec<AgentWarning>)> {
    let mut candidates = Vec::with_capacity(inputs.len().saturating_mul(2));
    for input in inputs {
        let content = input.conversation.load_content()?;
        let conversation = content.as_ref();
        candidates.push(semantic::index::SemanticIndexCandidate {
            index: input.original_index,
            source: semantic::types::SemanticChunkSource::VisibleDialogue,
            conversation: std::sync::Arc::new(stripped_semantic_conversation(
                conversation,
                input.conversation.path.clone(),
                conversation.semantic_turns.clone(),
                conversation.semantic_turn_ranges.clone(),
            )),
        });
        if !conversation.semantic_turns.is_empty()
            && let Some(route) = agent_route_semantic_conversation(conversation)
        {
            candidates.push(semantic::index::SemanticIndexCandidate {
                index: input.original_index,
                source: semantic::types::SemanticChunkSource::AgentRoute,
                conversation: std::sync::Arc::new(route),
            });
        }
    }
    run_agent_semantic_hits_for_candidates(
        query,
        &candidates,
        embedding_budget,
        chunk_hits,
        window,
        clock,
    )
}

pub(crate) fn agent_route_semantic_conversation(
    conversation: &history::Conversation,
) -> Option<history::Conversation> {
    let route_text = if conversation.semantic_route_text.is_empty() {
        conversation
            .custom_title
            .as_deref()
            .or(conversation.summary.as_deref())?
            .to_string()
    } else {
        conversation.semantic_route_text.clone()
    };
    let file_name = conversation
        .path
        .file_name()
        .map(|name| format!("{}.agent-route-semantic", name.to_string_lossy()))?;
    Some(stripped_semantic_conversation(
        conversation,
        conversation.path.with_file_name(file_name),
        vec![route_text],
        vec![
            conversation
                .semantic_turn_ranges
                .first()
                .copied()
                .unwrap_or_else(|| agent::refs::MessageRange::single(1)),
        ],
    ))
}

fn stripped_semantic_conversation(
    conversation: &history::Conversation,
    path: PathBuf,
    semantic_turns: Vec<String>,
    semantic_turn_ranges: Vec<agent::refs::MessageRange>,
) -> history::Conversation {
    history::Conversation {
        source: conversation.source,
        subagents: Vec::new(),
        session_id: conversation.session_id.clone(),
        path,
        index: conversation.index,
        timestamp: conversation.timestamp,
        preview: String::new(),
        preview_first: String::new(),
        preview_last: String::new(),
        full_text: String::new(),
        agent_search_text: String::new(),
        thinking_and_tool_call_text: String::new(),
        semantic_route_text: String::new(),
        semantic_turns,
        semantic_turn_ranges,
        search_text_lower: String::new(),
        project_name: None,
        project_path: None,
        cwd: None,
        message_count: conversation.message_count,
        message_times: conversation.message_times.clone(),
        message_text_starts: history::MessageTextStarts::default(),
        assistant_messages: conversation.assistant_messages,
        parse_errors: Vec::new(),
        summary: None,
        custom_title: None,
        model: None,
        total_tokens: 0,
        duration_minutes: None,
        fingerprint: None,
        has_transient_read_error: false,
        is_missing_earlier_turns: false,
        cached_session: None,
    }
}

/// `candidates` ranked for `query`, the embedding budget timed on `clock`.
fn run_agent_semantic_hits_for_candidates(
    query: &str,
    candidates: &[semantic::index::SemanticIndexCandidate],
    embedding_budget: EmbeddingBudget,
    chunk_hits: ChunkHits,
    window: TimeFilter,
    clock: Arc<dyn Clock>,
) -> Result<(RankedHits, Vec<AgentWarning>)> {
    let parsed = search::query::ParsedQuery::parse(query);
    let request = semantic::index::SemanticIndexRequest {
        query: parsed.semantic_text(),
        literal_filters: parsed.literals(),
        full_corpus: candidates,
        scope: candidates,
        prewarm: false,
        chunk_hits,
        window,
    };
    let mut state = semantic::index::SemanticIndexState::new(clock.clone());
    let mut embedder = semantic::fastembed::FastembedEmbedder::new().map_err(|error| {
        AgentError::semantic_unavailable(format!("failed to initialize semantic search: {error}"))
    })?;
    let cancellation = semantic::types::SemanticCancellationToken::new();
    let response = state
        .refresh_or_prewarm_with_budget(
            &request,
            &mut embedder,
            &cancellation,
            embedding_budget,
            |_progress| {},
            |cache| semantic::cache::write_embedding_cache(cache, clock.as_ref()),
        )
        .map_err(|error| {
            AgentError::semantic_unavailable(format!("semantic search failed: {error}"))
        })?;
    let warnings = partial_index_warning(
        response.indexed_chunk_count,
        response.missing_chunk_count,
        embedding_budget,
    )
    .into_iter()
    .collect();
    let ranked = RankedHits {
        conversations: response.hits,
        chunks: response.chunk_hits,
    };
    Ok((ranked, warnings))
}

/// `None` when every passage in the corpus has an embedding.
fn partial_index_warning(
    indexed_passages: usize,
    missing_passages: usize,
    embedding_budget: EmbeddingBudget,
) -> Option<AgentWarning> {
    if missing_passages == 0 {
        return None;
    }
    let total_passages = indexed_passages + missing_passages;
    let remedy = if embedding_budget.embeds_nothing() {
        format!("run {} --generate-semantic-cache", crate::APP_NAME)
    } else {
        "rerun this search to index more".to_string()
    };
    Some(AgentWarning::partial_index(format!(
        "{missing_passages} of {total_passages} passages missing from the semantic index; {remedy}"
    )))
}

#[cfg(test)]
pub(crate) fn agent_semantic_candidates(
    inputs: &[agent::search::AgentConversationInput<'_>],
) -> (
    Vec<semantic::index::SemanticIndexCandidate>,
    Vec<AgentWarning>,
) {
    agent_semantic_candidates_with_loader(
        inputs,
        |path| agent::transcript::AgentTranscript::load(path),
        SemanticToolContent::All,
    )
}

#[cfg(test)]
pub(crate) fn agent_semantic_candidates_matching_tools(
    inputs: &[agent::search::AgentConversationInput<'_>],
    query: &str,
) -> (
    Vec<semantic::index::SemanticIndexCandidate>,
    Vec<AgentWarning>,
) {
    agent_semantic_candidates_with_loader(
        inputs,
        |path| agent::transcript::AgentTranscript::load(path),
        SemanticToolContent::MatchingQuery(query),
    )
}

#[cfg(test)]
fn agent_semantic_candidates_with_loader<'a>(
    inputs: &[agent::search::AgentConversationInput<'_>],
    load_transcript: impl Fn(&Path) -> Result<agent::transcript::AgentTranscript>,
    tool_content: SemanticToolContent<'a>,
) -> (
    Vec<semantic::index::SemanticIndexCandidate>,
    Vec<AgentWarning>,
) {
    let mut candidates = Vec::new();
    let mut warnings = Vec::new();
    for input in inputs {
        match load_transcript(&input.resolved.key.path) {
            Ok(transcript) => {
                if let Some(warning) =
                    transcript_warning(&transcript, &input.resolved.reference.canonical())
                {
                    warnings.push(warning);
                }
                push_agent_semantic_candidates(
                    &mut candidates,
                    input,
                    &transcript,
                    tool_content,
                    TimeFilter::default(),
                )
            }
            Err(error) => warnings.push(AgentWarning::from_app_error(
                &error,
                Some(&input.resolved.reference.canonical()),
            )),
        }
    }
    (candidates, warnings)
}

#[derive(Clone, Copy)]
enum SemanticToolContent<'a> {
    All,
    MatchingQuery(&'a str),
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum SemanticAgentPartKind {
    Dialogue,
    Tool,
    Thinking,
}

/// Push a candidate per kind of passage in `transcript`'s messages inside
/// `window`, so a run embeds only the passages it can rank.
fn push_agent_semantic_candidates<'a>(
    candidates: &mut Vec<semantic::index::SemanticIndexCandidate>,
    input: &agent::search::AgentConversationInput<'_>,
    transcript: &agent::transcript::AgentTranscript,
    tool_content: SemanticToolContent<'a>,
    window: TimeFilter,
) {
    let messages: Vec<&agent::transcript::AgentMessage> = transcript
        .messages
        .iter()
        .filter(|message| window.contains_message(agent::time_span::TimeSpan::of_message(message)))
        .collect();
    for (subagents, kind, source) in [
        (
            false,
            SemanticAgentPartKind::Dialogue,
            semantic::types::SemanticChunkSource::VisibleDialogue,
        ),
        (
            false,
            SemanticAgentPartKind::Tool,
            semantic::types::SemanticChunkSource::AgentTool,
        ),
        (
            false,
            SemanticAgentPartKind::Thinking,
            semantic::types::SemanticChunkSource::AgentThinking,
        ),
        (
            true,
            SemanticAgentPartKind::Dialogue,
            semantic::types::SemanticChunkSource::AgentSubagentDialogue,
        ),
        (
            true,
            SemanticAgentPartKind::Tool,
            semantic::types::SemanticChunkSource::AgentSubagentTool,
        ),
        (
            true,
            SemanticAgentPartKind::Thinking,
            semantic::types::SemanticChunkSource::AgentSubagentThinking,
        ),
    ] {
        push_agent_semantic_candidate(
            candidates,
            input,
            &messages,
            subagents,
            kind,
            source,
            tool_content,
        );
    }
}

fn push_agent_semantic_candidate<'a>(
    candidates: &mut Vec<semantic::index::SemanticIndexCandidate>,
    input: &agent::search::AgentConversationInput<'_>,
    messages: &[&agent::transcript::AgentMessage],
    subagents: bool,
    kind: SemanticAgentPartKind,
    source: semantic::types::SemanticChunkSource,
    tool_content: SemanticToolContent<'a>,
) {
    let Some(conversation) =
        agent_semantic_conversation(input.conversation, messages, subagents, kind, tool_content)
    else {
        return;
    };
    candidates.push(semantic::index::SemanticIndexCandidate {
        index: input.original_index,
        source,
        conversation: std::sync::Arc::new(conversation),
    });
}

fn agent_semantic_conversation<'a>(
    conversation: &history::Conversation,
    messages: &[&agent::transcript::AgentMessage],
    subagents: bool,
    kind: SemanticAgentPartKind,
    tool_content: SemanticToolContent<'a>,
) -> Option<history::Conversation> {
    let mut semantic_turns = Vec::new();
    let mut semantic_turn_ranges = Vec::new();
    for message in messages {
        if message.parent_tool_use_id.is_some() != subagents {
            continue;
        }
        let role = match message.role {
            agent::transcript::AgentMessageRole::User => semantic::filter::SemanticTurnRole::User,
            agent::transcript::AgentMessageRole::Assistant => {
                semantic::filter::SemanticTurnRole::Assistant
            }
        };
        for part in &message.parts {
            let matches_kind = matches!(
                (kind, part),
                (
                    SemanticAgentPartKind::Dialogue,
                    agent::transcript::AgentMessagePart::Text { .. }
                ) | (
                    SemanticAgentPartKind::Tool,
                    agent::transcript::AgentMessagePart::ToolUse { .. }
                        | agent::transcript::AgentMessagePart::ToolResult { .. }
                ) | (
                    SemanticAgentPartKind::Thinking,
                    agent::transcript::AgentMessagePart::Thinking { .. }
                )
            );
            if !matches_kind {
                continue;
            }
            let texts = match (kind, part) {
                (SemanticAgentPartKind::Tool, part) => {
                    agent::transcript::bounded_agent_part_text(part)
                        .map(|text| semantic_tool_texts(&text, tool_content))
                        .unwrap_or_default()
                }
                _ => agent::transcript::bounded_agent_part_text(part)
                    .into_iter()
                    .collect(),
            };
            for text in texts {
                if let Some(turn) = semantic::filter::filter_turn(role, &text) {
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
    let scope = if subagents { "subagent" } else { "main" };
    let content = match kind {
        SemanticAgentPartKind::Dialogue => "dialogue",
        SemanticAgentPartKind::Tool => "tool",
        SemanticAgentPartKind::Thinking => "thinking",
    };
    let file_name = conversation.path.file_name().map(|name| {
        format!(
            "{}.agent-{scope}-{content}-semantic",
            name.to_string_lossy()
        )
    })?;
    conversation.path = conversation.path.with_file_name(file_name);
    conversation.semantic_turns = semantic_turns;
    conversation.semantic_turn_ranges = semantic_turn_ranges;
    Some(conversation)
}

fn semantic_tool_texts(text: &str, tool_content: SemanticToolContent<'_>) -> Vec<String> {
    let segments = stable_text_segments(text, semantic::types::DEFAULT_CHUNK_TARGET_CHARS);
    match tool_content {
        SemanticToolContent::All => segments,
        SemanticToolContent::MatchingQuery(query) => {
            let terms = semantic_tool_query_terms(query);
            let required_matches = if terms.len() >= 3 { 2 } else { 1 };
            segments
                .into_iter()
                .filter_map(|segment| {
                    let matches = semantic_tool_text_match_count(&segment, &terms);
                    (matches >= required_matches).then_some((matches, segment))
                })
                .max_by_key(|(matches, _)| *matches)
                .map(|(_, segment)| vec![segment])
                .unwrap_or_default()
        }
    }
}

fn semantic_tool_text_match_count(text: &str, terms: &[String]) -> usize {
    let normalized = normalize_semantic_tool_text(text);
    terms
        .iter()
        .filter(|term| crate::text_match::contains_prefix_match(&normalized, term))
        .count()
}

fn normalize_semantic_tool_text(text: &str) -> String {
    let mut expanded = String::with_capacity(text.len());
    let mut previous_was_lowercase = false;
    for ch in text.chars() {
        if previous_was_lowercase && ch.is_uppercase() {
            expanded.push(' ');
        }
        expanded.push(ch);
        previous_was_lowercase = ch.is_lowercase();
    }
    crate::text_match::normalize_for_search(&expanded)
}

fn semantic_tool_query_terms(query: &str) -> Vec<String> {
    const STOP_WORDS: &[&str] = &[
        "after", "all", "an", "and", "are", "as", "at", "be", "before", "but", "by", "can", "do",
        "does", "for", "from", "has", "have", "how", "if", "in", "is", "it", "its", "my", "no",
        "not", "of", "on", "or", "our", "so", "than", "that", "the", "them", "then", "they",
        "this", "to", "up", "was", "we", "what", "when", "where", "which", "why", "with", "would",
        "you", "your",
    ];

    normalize_semantic_tool_text(query)
        .split_whitespace()
        .map(|term| term.trim_matches(|ch: char| !ch.is_alphanumeric()))
        .filter(|term| term.chars().count() >= 2)
        .filter(|term| !STOP_WORDS.contains(term))
        .map(str::to_string)
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .collect()
}

fn stable_text_segments(mut text: &str, max_bytes: usize) -> Vec<String> {
    let mut segments = Vec::new();
    while !text.is_empty() {
        let mut end = text.len().min(max_bytes);
        while end > 0 && !text.is_char_boundary(end) {
            end -= 1;
        }
        if end == 0 {
            end = text
                .char_indices()
                .nth(1)
                .map_or(text.len(), |(index, _)| index);
        }
        segments.push(text[..end].to_string());
        text = &text[end..];
    }
    segments
}

fn run_agent_within_semantic<'a>(
    request: &agent::search::AgentWithinRequest,
    conversation: &history::Conversation,
    resolved: &agent::refs::ResolvedConversation,
    transcript: &agent::transcript::AgentTranscript,
    tool_content: SemanticToolContent<'a>,
    clock: Arc<dyn Clock>,
) -> Result<(agent::search::AgentSearchOutput, Vec<AgentWarning>)> {
    let input = agent::search::AgentConversationInput {
        conversation,
        resolved: resolved.clone(),
        original_index: 0,
    };
    let mut candidates = Vec::new();
    push_agent_semantic_candidates(
        &mut candidates,
        &input,
        transcript,
        tool_content,
        request.window,
    );
    let (semantic, warnings) = run_agent_semantic_hits_for_candidates(
        &request.query,
        &candidates,
        EmbeddingBudget::passages(semantic::types::MAX_WITHIN_INTERACTIVE_PASSAGE_EMBEDDINGS),
        request.semantic_chunk_hits(),
        request.window,
        clock,
    )?;
    let output = agent::search::run_within_search(
        request,
        conversation,
        resolved,
        transcript,
        &semantic.chunks,
    );
    Ok((output, warnings))
}

fn agent_inputs_for_indices<'a>(
    conversations: &'a [history::Conversation],
    keys: &[agent::refs::AgentConversationKey],
    indices: &[usize],
) -> Result<Vec<agent::search::AgentConversationInput<'a>>> {
    let resolved_by_path = agent::refs::resolved_conversations_for_keys(keys)
        .into_iter()
        .map(|resolved| (resolved.key.path.clone(), resolved))
        .collect::<std::collections::HashMap<_, _>>();
    indices
        .iter()
        .filter_map(|index| {
            let conversation = conversations.get(*index)?;
            let resolved = resolved_by_path.get(&conversation.path)?;
            Some(Ok(agent::search::AgentConversationInput {
                conversation,
                resolved: resolved.clone(),
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
        let perf = &self.perf;
        let discovered;
        let keys = match keys {
            Some(keys) => keys,
            None => {
                discovered = perf
                    .time(Stage::Keys, || {
                        discover_agent_keys(&self.environment, perf.load_meter())
                    })?
                    .0;
                &discovered
            }
        };
        let agent_config = config::load_config()?.agent.unwrap_or_default();
        let options = agent_protocol_options(&args.output, &agent_config, self.clock().as_ref())?;
        let (mut resolved_refs, focus) = resolve_agent_read_args(args, keys)?;
        let transcripts = perf.time(Stage::Transcript, || {
            resolved_refs
                .iter()
                .map(|(read_ref, resolved)| {
                    let range = read_ref.range.filter(|_| {
                        args.anchor.is_none()
                            && read_ref.subagent.is_none()
                            && read_ref.call.is_none()
                    });
                    self.load_transcript_for_range(&resolved.key, range)
                        .map_err(|error| target_error(error, resolved))
                })
                .collect::<Result<Vec<_>>>()
        })?;
        if let Some(anchor) = args.anchor.as_deref() {
            let ordinal = transcripts[0].resolve_anchor(&resolved_refs[0].1, anchor)?;
            resolved_refs[0].0.range = Some(agent::refs::MessageRange::single(ordinal));
        }
        let requests = resolved_refs
            .iter()
            .zip(transcripts.iter())
            .map(
                |((read_ref, resolved), transcript)| agent::protocol::ReadRequest {
                    resolved,
                    transcript,
                    range: read_ref.range,
                    subagent: read_ref.subagent,
                    call: read_ref.call,
                },
            )
            .collect::<Vec<_>>();
        let priority = match (args.tail, focus) {
            (Some(tail), _) => agent::protocol::ReadPriority::Tail(tail),
            (None, Some(focus)) => {
                agent::protocol::ReadPriority::Focus(agent::protocol::ProtocolFocus {
                    conversation_full_ref: focus
                        .conversation
                        .map(|conversation| conversation.reference.full_ref()),
                    range: focus.range,
                })
            }
            (None, None) => agent::protocol::ReadPriority::Start,
        };
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
        let warnings = resolved_refs
            .iter()
            .zip(transcripts.iter())
            .filter_map(|((_, resolved), transcript)| {
                transcript_warning(transcript, &resolved.reference.canonical())
            })
            .collect::<Vec<_>>();
        perf.time(Stage::Render, || {
            agent::protocol::format_read_with_warnings(
                &requests,
                priority,
                slice.as_ref(),
                options,
                &warnings,
                self.clock().wall(),
            )
        })
        .map_err(|error| match resolved_refs.first() {
            Some((_, resolved)) => target_error(error, resolved),
            None => structured_agent_error(error),
        })
    }

    pub(crate) fn run_outline(
        &self,
        args: &AgentOutlineArgs,
        keys: Option<&[agent::refs::AgentConversationKey]>,
    ) -> Result<String> {
        let perf = &self.perf;
        let discovered;
        let keys = match keys {
            Some(keys) => keys,
            None => {
                discovered = perf
                    .time(Stage::Keys, || {
                        discover_agent_keys(&self.environment, perf.load_meter())
                    })?
                    .0;
                &discovered
            }
        };
        let agent_config = config::load_config()?.agent.unwrap_or_default();
        let options = agent_protocol_options(&args.output, &agent_config, self.clock().as_ref())?;
        let outline_ref = agent::refs::parse_outline_ref(&args.conversation)?;
        let resolved = outline_ref.resolve_conversation(keys)?;
        let transcript = perf
            .time(Stage::Transcript, || {
                self.load_transcript_for_range(
                    &resolved.key,
                    outline_ref.range.filter(|_| outline_ref.subagent.is_none()),
                )
            })
            .map_err(|error| target_error(error, &resolved))?;
        let warning = transcript_warning(&transcript, &resolved.reference.canonical());
        let request = agent::protocol::ReadRequest {
            resolved: &resolved,
            transcript: &transcript,
            range: outline_ref.range,
            subagent: outline_ref.subagent,
            call: None,
        };
        perf.time(Stage::Render, || {
            agent::protocol::format_outline_with_warnings(
                &request,
                options,
                warning.as_slice(),
                self.clock().wall(),
            )
        })
        .map_err(|error| target_error(error, &resolved))
    }
}

pub(crate) fn resolve_agent_read_args(
    args: &AgentReadArgs,
    keys: &[agent::refs::AgentConversationKey],
) -> Result<(ResolvedReadRefs, Option<ResolvedFocus>)> {
    let refs = args
        .refs
        .iter()
        .map(|reference| agent::refs::parse_read_ref(reference))
        .collect::<Result<Vec<_>>>()?;
    if args.anchor.is_some()
        && (refs.len() != 1 || refs[0].range.is_some() || refs[0].subagent.is_some())
    {
        return Err(AppError::ConfigError(
            "--anchor requires exactly one conversation ref without an mN range".to_string(),
        ));
    }
    if (args.lines.is_some() || args.match_query.is_some())
        && (refs.len() != 1
            || refs[0].call.is_some()
            || (!refs[0].range.is_some_and(|range| range.is_single()) && args.anchor.is_none()))
    {
        return Err(AppError::ConfigError(
            "--lines and --match require exactly one single-message ref such as ch_...:m7"
                .to_string(),
        ));
    }
    let resolved_refs = refs
        .iter()
        .map(|reference| {
            reference
                .resolve_conversation(keys)
                .map(|resolved| (reference.clone(), resolved))
        })
        .collect::<Result<Vec<_>>>()?;
    let focus = args
        .focus
        .as_deref()
        .map(|focus| -> Result<ResolvedFocus> {
            let focus = agent::refs::parse_focus_ref(focus)?;
            let conversation = focus
                .conversation
                .as_ref()
                .map(|conversation| agent::refs::resolve_conversation_ref(keys, conversation))
                .transpose()?;
            agent::refs::validate_resolved_focus_in_ranges(
                &resolved_refs,
                &focus,
                conversation.as_ref(),
            )?;
            Ok(ResolvedFocus {
                conversation,
                range: focus.range,
            })
        })
        .transpose()?;
    Ok((resolved_refs, focus))
}

fn agent_protocol_options(
    output: &cli::AgentOutputFlags,
    config: &AgentConfig,
    clock: &dyn Clock,
) -> Result<agent::protocol::ProtocolOptions> {
    Ok(agent::protocol::ProtocolOptions {
        budget: configured_budget(output.no_budget, output.budget, config.output_chars),
        tools: configured_visibility(output.tools, config.tools),
        tool_results: configured_visibility(output.tool_results, config.tool_results),
        thinking: configured_visibility(output.thinking, config.thinking),
        subagents: configured_visibility(output.subagents, config.subagents),
        window: output.time.resolve(clock)?,
    })
}

/// The sessions of `scoped` whose cached text matches `query`, best first,
/// freshness counted to `clock`'s current time; in exact mode, only those
/// whose cached text holds the whole query as one phrase. With `window`
/// active, each session is ranked on the text of its messages inside the
/// window.
fn lexically_rank_scoped(
    conversations: &[history::Conversation],
    query: &str,
    scoped: &[usize],
    mode: SearchMode,
    window: &TimeFilter,
    clock: &dyn Clock,
) -> Result<Vec<usize>> {
    let texts = search::precompute_agent_search_text(conversations, window);
    let now = clock.wall().with_timezone(&chrono::Local);
    let ranked_all = search::try_agent_search(conversations, &texts, query, now)?;
    let scoped_set = scoped
        .iter()
        .copied()
        .collect::<std::collections::HashSet<_>>();
    let ranked = ranked_all
        .into_iter()
        .filter(|index| scoped_set.contains(index))
        .collect::<Vec<_>>();
    if mode != SearchMode::Exact {
        return Ok(ranked);
    }
    let phrase = search::query::ParsedQuery::parse(&agent::search::exact_phrase_query(query));
    Ok(ranked
        .into_par_iter()
        .map(|index| -> Result<_> {
            let content = conversations[index].load_content()?;
            Ok(search::literal::agent_conversation_matches_all_literals(
                &content,
                phrase.literals(),
                window,
            )
            .then_some(index))
        })
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .flatten()
        .collect())
}

pub(crate) fn resolve_agent_conversation_arg(
    reference: &str,
    keys: &[agent::refs::AgentConversationKey],
) -> Result<agent::refs::ResolvedConversation> {
    agent::refs::resolve_conversation_ref(keys, reference)
}

/// A service reading every provider's storage and no session cache, on test
/// time.
#[cfg(test)]
fn service_without_session_cache() -> AgentService {
    AgentService::new(history::Environment::without_session_cache(Arc::new(
        crate::clock::TestClock::default(),
    )))
}

#[cfg(test)]
pub(crate) fn run_agent_read(
    args: &AgentReadArgs,
    keys: Option<&[agent::refs::AgentConversationKey]>,
) -> Result<String> {
    service_without_session_cache().run_read(args, keys)
}

#[cfg(test)]
pub(crate) fn run_agent_outline(
    args: &AgentOutlineArgs,
    keys: Option<&[agent::refs::AgentConversationKey]>,
) -> Result<String> {
    service_without_session_cache().run_outline(args, keys)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::test_support::{assistant_jsonl_line, user_jsonl_line};
    use crate::cli::AgentOutputFlags;
    use crate::clock::TestClock;
    use crate::history::environment::test_support::test_environment;

    fn service() -> AgentService {
        service_without_session_cache()
    }

    /// `storage`'s sessions loaded through the session cache under
    /// `cache_base` on test time, leaving `left_out`'s rows unread.
    /// `storage`'s sessions, loaded through the session cache database in
    /// `cache_base` once a first build has admitted it.
    fn load_through(
        storage: &dyn history::provider::SessionStorage,
        cache_base: &Path,
        left_out: history::provider::LeftOutRows,
    ) -> history::provider::LoadedSessions {
        history::sqlite_cache::SqliteCache::open(&history::sqlite_cache::tests::rearview_at(
            cache_base,
        ))
        .unwrap()
        .mark_admitted()
        .unwrap();
        history::provider::load_sessions(
            &test_environment(Vec::new(), Some(cache_base)),
            storage,
            false,
            None,
            AgentMeter::UNMETERED,
            left_out,
            &mut |_, _| {},
        )
        .unwrap()
    }

    fn output_flags() -> AgentOutputFlags {
        AgentOutputFlags {
            budget: Some(6000),
            no_budget: false,
            tools: false,
            tool_results: false,
            thinking: false,
            subagents: false,
            time: cli::MessageTimeArgs::default(),
        }
    }

    #[test]
    fn failed_semantic_half_runs_one_lexical_pass_alone() {
        let mut passes = Vec::new();

        let (semantic, lexical) = run_hybrid_halves(
            || -> Result<()> { Err(AgentError::semantic_unavailable("no model").into()) },
            |pass| {
                passes.push(pass);
                Ok("lexical hits")
            },
        )
        .unwrap();

        assert!(semantic.is_err());
        assert_eq!(lexical, "lexical hits");
        assert_eq!(passes, [HybridLexicalPass::Alone]);
    }

    #[test]
    fn semantic_half_that_succeeds_runs_one_lexical_pass_for_fusion() {
        let mut passes = Vec::new();

        let (semantic, _) = run_hybrid_halves(
            || Ok("semantic hits"),
            |pass| {
                passes.push(pass);
                Ok(())
            },
        )
        .unwrap();

        assert_eq!(semantic.unwrap(), "semantic hits");
        assert_eq!(passes, [HybridLexicalPass::ForFusion]);
    }

    fn read_args(reference: String) -> AgentReadArgs {
        AgentReadArgs {
            refs: vec![reference],
            anchor: None,
            focus: None,
            tail: None,
            lines: None,
            match_query: None,
            context: 3,
            perf_stats: false,
            output: output_flags(),
        }
    }

    fn pi_session_stub(directory: &Path) -> history::provider::SessionStub {
        let transcript = directory.join("project").join("session.jsonl");
        std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
        std::fs::copy(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/pi/v1.jsonl"),
            &transcript,
        )
        .unwrap();
        let root = history::provider::SessionRoot::new(directory);
        history::provider::walk::file_stubs(&root, vec![transcript]).remove(0)
    }

    #[test]
    fn sessions_key_carries_transcript_size_discovery_found() {
        let directory = tempfile::tempdir().unwrap();
        let stub = pi_session_stub(directory.path());
        let provider = history::Source::Pi.provider();
        let recorded = recorded_session(
            provider.storage(),
            &stub,
            &HashMap::new(),
            AgentMeter::UNMETERED,
            &TestClock::default(),
        );

        let key = agent::refs::session_key(provider, &stub, recorded.as_ref()).unwrap();

        assert_eq!(key.transcript_bytes, Some(stub.fingerprint.size));
    }

    /// The thread `tests/fixtures/codex/rollout.jsonl` holds.
    const LISTED_CODEX_THREAD: &str = "019f0000-0000-7000-8000-00000000000a";
    /// A thread whose rollout holds only its header, so no conversation.
    const EMPTY_CODEX_THREAD: &str = "019f0000-0000-7000-8000-00000000000c";

    /// A Codex sessions tree under `home` holding the fixture's rollout and
    /// an empty one, filed where Codex files rollouts.
    fn codex_sessions_with_a_listed_and_an_empty_thread(
        home: &Path,
    ) -> history::provider::SessionRoot {
        let fixture =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/codex/rollout.jsonl");
        let day = home.join("sessions").join("2026").join("08").join("01");
        std::fs::create_dir_all(&day).unwrap();
        std::fs::copy(
            &fixture,
            day.join(format!(
                "rollout-2026-08-01T10-00-00-{LISTED_CODEX_THREAD}.jsonl"
            )),
        )
        .unwrap();
        let header = std::fs::read_to_string(&fixture)
            .unwrap()
            .lines()
            .next()
            .unwrap()
            .replace(LISTED_CODEX_THREAD, EMPTY_CODEX_THREAD);
        std::fs::write(
            day.join(format!(
                "rollout-2026-08-01T11-00-00-{EMPTY_CODEX_THREAD}.jsonl"
            )),
            header + "\n",
        )
        .unwrap();
        history::provider::SessionRoot::new(home.join("sessions")).in_agent_tree()
    }

    #[test]
    fn search_keys_sessions_its_load_discovered_as_discovery_does() {
        let home = tempfile::tempdir().unwrap();
        let cache_base = tempfile::tempdir().unwrap();
        let provider = history::Source::Codex.provider();
        let storage = history::provider::RootedStorage {
            inner: provider.storage(),
            root: codex_sessions_with_a_listed_and_an_empty_thread(home.path()),
        };
        let loaded = load_through(
            &storage,
            cache_base.path(),
            history::provider::LeftOutRows::NONE,
        );
        let discovered = [(history::Source::Codex, loaded.discovered)];
        // The directory the fixture's listed thread recorded.
        let workspace = history::workspace::tests::workspace_at_home(Path::new("/tmp/project"));
        let mut discovery_keys = root_keys(
            &test_environment(Vec::new(), Some(cache_base.path())),
            &storage,
            &storage.root,
            AgentMeter::UNMETERED,
        )
        .unwrap();
        discovery_keys.sort_by(|left, right| left.path.cmp(&right.path));

        for workspace in [None, Some(&workspace)] {
            let (keys, warnings) = loaded_agent_keys(&discovered, &[], workspace);

            assert_eq!(keys, discovery_keys);
            assert!(
                keys.iter().any(|key| key.session_id == LISTED_CODEX_THREAD),
                "{keys:?}"
            );
            assert!(warnings.is_empty());
        }
    }

    /// A Claude Code key is made from its path alone, so only the directory
    /// the load recorded places it in a workspace, as it places the row.
    #[test]
    fn local_search_keys_and_scopes_claude_session_recorded_in_subdirectory() {
        let home = history::workspace::tests::TestHome::new();
        let repository = home.repository();
        let subdirectory = repository.join("src");
        let claude = history::provider::claude::tests::ConfigDir::new();
        let folder = claude.project_folder(&subdirectory);
        let transcript = folder.join("0f000000-0000-4000-8000-000000000001.jsonl");
        std::fs::create_dir_all(&folder).unwrap();
        let user = serde_json::json!({
            "type": "user", "timestamp": "2026-07-26T06:30:00.000Z", "cwd": subdirectory,
            "message": {"role": "user", "content": "a question"}
        });
        let assistant = serde_json::json!({
            "type": "assistant", "timestamp": "2026-07-26T06:30:05.000Z",
            "message": {"role": "assistant", "content": [{"type": "text", "text": "an answer"}]}
        });
        std::fs::write(&transcript, format!("{user}\n{assistant}\n")).unwrap();
        let cache_base = tempfile::tempdir().unwrap();
        let provider = history::Source::Claude.provider();
        let storage = history::provider::RootedStorage {
            inner: provider.storage(),
            root: history::provider::SessionRoot::new(folder.parent().unwrap()).in_agent_tree(),
        };
        let loaded = load_through(
            &storage,
            cache_base.path(),
            history::provider::LeftOutRows::NONE,
        );
        let discovered = [(history::Source::Claude, loaded.discovered)];
        let workspace = home.workspace(&repository);

        let (keys, _) = loaded_agent_keys(&discovered, &[], Some(&workspace));
        let scoped = agent::search::scoped_conversation_inputs(
            &loaded.conversations,
            agent::search::AgentSearchScope::Local,
            Some(&workspace),
        )
        .unwrap();

        assert_eq!(
            keys.iter().map(|key| &key.path).collect::<Vec<_>>(),
            [&transcript]
        );
        assert_eq!(scoped, [0]);
    }

    /// A Claude Code session `id` recorded in `directory`: a question at
    /// 06:`minute` UTC on 2026-07-26 and its answer five seconds later.
    fn claude_session_in(
        claude: &history::provider::claude::tests::ConfigDir,
        directory: &Path,
        id: &str,
        minute: u32,
    ) -> PathBuf {
        let folder = claude.project_folder(directory);
        std::fs::create_dir_all(&folder).unwrap();
        let transcript = folder.join(format!("{id}.jsonl"));
        let user = serde_json::json!({
            "type": "user", "timestamp": format!("2026-07-26T06:{minute:02}:00.000Z"),
            "cwd": directory, "message": {"role": "user", "content": "a question"}
        });
        let assistant = serde_json::json!({
            "type": "assistant", "timestamp": format!("2026-07-26T06:{minute:02}:05.000Z"),
            "message": {"role": "assistant", "content": [{"type": "text", "text": "an answer"}]}
        });
        std::fs::write(&transcript, format!("{user}\n{assistant}\n")).unwrap();
        transcript
    }

    #[test]
    fn local_selection_lists_repositorys_sessions_inside_window() {
        let home = history::workspace::tests::TestHome::new();
        let repository = home.repository();
        let claude = history::provider::claude::tests::ConfigDir::new();
        let inside = "0f000000-0000-4000-8000-000000000001";
        let transcript = claude_session_in(&claude, &repository.join("src"), inside, 30);
        claude_session_in(
            &claude,
            &home.path().join("elsewhere"),
            "0f000000-0000-4000-8000-000000000002",
            30,
        );
        claude_session_in(
            &claude,
            &repository,
            "0f000000-0000-4000-8000-000000000003",
            0,
        );
        let cache_base = tempfile::tempdir().unwrap();
        let provider = history::Source::Claude.provider();
        let storage = history::provider::RootedStorage {
            inner: provider.storage(),
            root: history::provider::SessionRoot::new(
                transcript.parent().unwrap().parent().unwrap(),
            )
            .in_agent_tree(),
        };
        let loaded = load_through(
            &storage,
            cache_base.path(),
            history::provider::LeftOutRows::NONE,
        );
        let utc = |minute| {
            chrono::TimeZone::with_ymd_and_hms(&chrono::Utc, 2026, 7, 26, 6, minute, 0)
                .unwrap()
                .with_timezone(&chrono::Local)
        };
        let rules = SelectionRules {
            window: TimeFilter {
                after: Some(utc(20)),
                before: Some(utc(40)),
            },
            scope: agent::search::AgentSearchScope::Local,
            excludes_current_session: false,
            excluded_projects: history::ExcludedProjects::default(),
        };
        let sessions = LoadedSessions {
            history: history::LoadedHistory {
                conversations: loaded.conversations,
                ignored: Vec::new(),
                discovered: vec![(history::Source::Claude, loaded.discovered)],
                failures: Vec::new(),
            },
            current_session_ids: Vec::new(),
            workspace: Some(home.workspace(&repository)),
        };

        let listing = SessionSelection::from_loaded(
            sessions,
            &rules,
            &CommandPerf::new(Arc::new(TestClock::default())),
        )
        .unwrap()
        .listing(&rules.window, 0, 10)
        .unwrap();

        assert_eq!(
            listing
                .sessions
                .iter()
                .map(|session| session.conversation_uuid.as_str())
                .collect::<Vec<_>>(),
            [inside]
        );
        assert_eq!(listing.sessions[0].in_window.count, 2);
    }

    /// The calling session changes with every tool call. A listing leaves it
    /// out, so the load keeps its cached row unread, and the listing prints
    /// what a load that reread it prints.
    #[test]
    fn a_changed_calling_session_stays_unread_and_the_listing_is_unchanged() {
        let claude = history::provider::claude::tests::ConfigDir::new();
        let directory = tempfile::tempdir().unwrap();
        let calling = "0f000000-0000-4000-8000-000000000001";
        let transcript = claude_session_in(&claude, directory.path(), calling, 30);
        claude_session_in(
            &claude,
            directory.path(),
            "0f000000-0000-4000-8000-000000000002",
            31,
        );
        let cache_base = tempfile::tempdir().unwrap();
        let provider = history::Source::Claude.provider();
        let storage = history::provider::RootedStorage {
            inner: provider.storage(),
            root: history::provider::SessionRoot::new(
                transcript.parent().unwrap().parent().unwrap(),
            )
            .in_agent_tree(),
        };
        load_through(
            &storage,
            cache_base.path(),
            history::provider::LeftOutRows::NONE,
        );
        let database = history::sqlite_cache::tests::rearview_at(cache_base.path());
        let cached_fingerprint = || {
            history::sqlite_cache::SqliteCache::open_waiting(&database)
                .unwrap()
                .unwrap()
                .catalogue(
                    provider.storage().cache(),
                    history::Source::Claude,
                    &storage.root.path,
                    false,
                )
                .unwrap()
                .into_values()
                .find_map(|session| match session {
                    history::sqlite_cache::CatalogueSession::Listed(row)
                        if row.session_id == calling =>
                    {
                        row.fingerprint
                    }
                    _ => None,
                })
        };
        let fingerprint_before = cached_fingerprint();
        let next_question = serde_json::json!({
            "type": "user", "timestamp": "2026-07-26T06:32:00.000Z",
            "cwd": directory.path(), "message": {"role": "user", "content": "one more"}
        });
        let mut appended = std::fs::read_to_string(&transcript).unwrap();
        appended.push_str(&format!("{next_question}\n"));
        std::fs::write(&transcript, appended).unwrap();
        let rules = SelectionRules {
            window: TimeFilter::default(),
            scope: agent::search::AgentSearchScope::Global,
            excludes_current_session: true,
            excluded_projects: history::ExcludedProjects::default(),
        };
        let current_session_ids = vec![calling.to_owned()];
        let listing_of = |conversations, discovered| {
            let sessions = LoadedSessions {
                history: history::LoadedHistory {
                    conversations,
                    ignored: Vec::new(),
                    discovered: vec![(history::Source::Claude, discovered)],
                    failures: Vec::new(),
                },
                current_session_ids: current_session_ids.clone(),
                workspace: None,
            };
            let selection = SessionSelection::from_loaded(
                sessions,
                &rules,
                &CommandPerf::new(Arc::new(TestClock::default())),
            )
            .unwrap();
            agent::sessions::format_sessions_with_warnings(
                &selection.listing(&rules.window, 0, 10).unwrap(),
                None,
                &selection.warnings,
                TestClock::start_wall(),
            )
        };
        let leaves_out =
            |row: &history::Conversation| rules.leaves_out(row, &current_session_ids, None);

        let left_out = load_through(
            &storage,
            cache_base.path(),
            history::provider::LeftOutRows::of(&leaves_out),
        );
        let left_out_listing = listing_of(left_out.conversations, left_out.discovered);

        assert_eq!(cached_fingerprint(), fingerprint_before);
        let reread = load_through(
            &storage,
            cache_base.path(),
            history::provider::LeftOutRows::NONE,
        );
        assert_ne!(cached_fingerprint(), fingerprint_before);
        assert_eq!(
            left_out_listing,
            listing_of(reread.conversations, reread.discovered)
        );
    }

    #[test]
    fn agent_whose_load_failed_to_list_its_sessions_is_search_warning() {
        let failures = [(
            history::Source::Codex,
            AppError::Io(std::io::Error::other("access denied")),
        )];

        let (keys, warnings) = loaded_agent_keys(&[], &failures, None);

        assert!(keys.is_empty());
        assert_eq!(warnings.len(), 1);
        assert!(
            warnings[0]
                .detail
                .starts_with("failed to list Codex sessions:"),
            "{}",
            warnings[0].detail
        );
    }

    #[test]
    fn a_session_in_a_root_without_a_cache_is_parsed_for_its_key_and_the_parse_recorded() {
        let directory = tempfile::tempdir().unwrap();
        let stub = pi_session_stub(directory.path());
        let meter = LoadMeter::default();

        let recorded = recorded_session(
            history::Source::Pi.provider().storage(),
            &stub,
            &HashMap::new(),
            meter.agent(history::Source::Pi),
            &TestClock::default(),
        );

        assert!(recorded.is_some());
        let refresh = meter.agents()[0].refresh;
        assert_eq!(
            (refresh.transcripts, refresh.bytes),
            (1, stub.fingerprint.size),
            "a session without sub-agents is its own transcript"
        );
    }

    #[test]
    fn session_missing_from_cache_of_other_sessions_doesnt_get_key() {
        let directory = tempfile::tempdir().unwrap();
        let stub = pi_session_stub(directory.path());
        let cached = HashMap::from([("other.jsonl".to_owned(), None)]);

        let recorded = recorded_session(
            history::Source::Pi.provider().storage(),
            &stub,
            &cached,
            AgentMeter::UNMETERED,
            &TestClock::default(),
        );

        assert_eq!(recorded, None);
    }

    #[test]
    fn exact_mode_doesnt_rank_session_whose_cached_text_lacks_phrase() {
        let clock = TestClock::default();
        let now = clock.wall().with_timezone(&chrono::Local);
        let conversation =
            |text| search::test_fixtures::one_message_conversation(text, now, None, None, None);
        let conversations = [
            conversation("ran it in isolated mode"),
            conversation("pass --isolated to the formatter"),
        ];

        let rank = |mode| {
            lexically_rank_scoped(
                &conversations,
                "--isolated",
                &[0, 1],
                mode,
                &TimeFilter::default(),
                &clock,
            )
            .unwrap()
        };
        let exact = rank(SearchMode::Exact);
        let lexical = rank(SearchMode::Lexical);

        assert_eq!(exact, [1]);
        assert_eq!(lexical.len(), 2);
    }

    #[test]
    fn exact_mode_in_window_ranks_session_only_on_phrase_inside_it() {
        use search::test_fixtures::{october, timed_messages_conversation};
        let conversations = [timed_messages_conversation(
            &[
                ("the flaky test failed", october(1, 9)),
                ("a flaky network", october(10, 11)),
                ("the test passed", october(10, 12)),
            ],
            None,
        )];
        let window_on_october_10 = search::test_fixtures::window_on_october_10();
        let clock = TestClock::default();
        let rank = |mode, window: &TimeFilter| {
            lexically_rank_scoped(&conversations, "flaky test", &[0], mode, window, &clock).unwrap()
        };

        assert_eq!(rank(SearchMode::Exact, &TimeFilter::default()), [0]);
        assert_eq!(rank(SearchMode::Lexical, &window_on_october_10), [0]);
        assert_eq!(
            rank(SearchMode::Exact, &window_on_october_10),
            Vec::<usize>::new()
        );
    }

    /// A Claude Code session that names the command only in the call that
    /// ran it.
    fn session_running(directory: &Path, command: &str) -> PathBuf {
        let path = directory.join("12345678-1234-4234-9234-123456789abc.jsonl");
        let record = |kind: &str, content: serde_json::Value| {
            serde_json::json!({
                "type": kind,
                "timestamp": "2024-01-01T00:00:01Z",
                "message": {"role": kind, "content": [content]}
            })
            .to_string()
        };
        let lines = [
            user_jsonl_line("Format the sheet sources"),
            record(
                "assistant",
                serde_json::json!({"type": "tool_use", "id": "toolu_fmt", "name": "PowerShell", "input": {"command": command, "description": "Format the file"}}),
            ),
            record(
                "user",
                serde_json::json!({"type": "tool_result", "tool_use_id": "toolu_fmt", "content": "Formatted 1 file"}),
            ),
            assistant_jsonl_line("Done."),
        ];
        std::fs::write(&path, lines.join("\n")).unwrap();
        path
    }

    #[test]
    fn exact_and_lexical_search_find_later_line_of_command() {
        let directory = tempfile::tempdir().unwrap();
        let query = "mise run fmt src/sheet_memory.hpp";
        let path = session_running(directory.path(), &format!("Set-Location src\n{query}"));
        let clock = TestClock::default();
        let conversations = [history::Source::Claude
            .provider()
            .format()
            .parse_conversation(&path, history::parser::RowDating::new(None, &clock), None)
            .unwrap()
            .expect("the session is listed")];
        let keys =
            [agent::refs::AgentConversationKey::from_conversation(&conversations[0]).unwrap()];

        for mode in [SearchMode::Exact, SearchMode::Lexical] {
            let request = agent::search::AgentSearchRequest {
                query: query.to_owned(),
                top: 1,
                cli_mode: Some(mode),
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
            let ranked =
                lexically_rank_scoped(&conversations, query, &[0], mode, &request.window, &clock)
                    .unwrap();
            let output = agent::search::run_global_lexical_search(
                &request,
                &conversations,
                &keys,
                &ranked,
                |key| agent::transcript::AgentTranscript::load(&key.path),
            )
            .unwrap();
            let rendered = agent::search::format_agent_output(&output);

            assert!(rendered.contains(" source=tool "), "{mode:?}: {rendered}");
            assert!(rendered.contains(" focus=m2..m2"), "{mode:?}: {rendered}");
            assert!(rendered.contains(query), "{mode:?}: {rendered}");
            assert!(rendered.contains(" tools=true "), "{mode:?}: {rendered}");
        }
    }

    #[test]
    fn complete_index_doesnt_raise_partial_index_warning() {
        assert_eq!(
            partial_index_warning(6529, 0, EmbeddingBudget::seconds(0)),
            None
        );
    }

    #[test]
    fn search_that_embeds_nothing_names_cache_command() {
        let warning = partial_index_warning(6054, 475, EmbeddingBudget::seconds(0))
            .expect("missing passages warn");
        assert_eq!(warning.kind, AgentWarningKind::PartialIndex);
        assert_eq!(
            warning.detail,
            "475 of 6529 passages missing from the semantic index; run rearview --generate-semantic-cache"
        );
    }

    #[test]
    fn bounded_search_names_rerun_as_remedy() {
        for budget in [EmbeddingBudget::passages(32), EmbeddingBudget::seconds(2)] {
            let warning = partial_index_warning(40, 8, budget).expect("missing passages warn");
            assert_eq!(
                warning.detail,
                "8 of 48 passages missing from the semantic index; rerun this search to index more"
            );
        }
    }

    #[test]
    fn embedding_budget_defaults_to_two_seconds_and_zero_turns_it_off() {
        assert_eq!(
            configured_embedding_budget(&SemanticConfig::default()),
            EmbeddingBudget::seconds(2)
        );
        assert!(
            configured_embedding_budget(&SemanticConfig {
                interactive_embedding_seconds: Some(0),
            })
            .embeds_nothing()
        );
    }

    #[test]
    fn agent_route_uses_compact_precomputed_text() {
        let mut conversation = crate::semantic::test_fixtures::SemanticConversationFixture::new(
            "/tmp/session.jsonl",
            ["dialogue that should not be copied into the route"],
        )
        .with_title("Align step calibration and treadmill UX")
        .with_summary(Some(
            "Calibrate automatic counting across treadmill sessions",
        ))
        .build();
        conversation.full_text =
            "calibration isolation and per-device capability memory from bounded tool evidence"
                .to_string();
        conversation.semantic_route_text =
            history::semantic_route_text(&conversation.full_text, "");

        let route = agent_route_semantic_conversation(&conversation).unwrap();

        assert_eq!(route.semantic_turns.len(), 1);
        assert!(route.semantic_turns[0].chars().count() <= 1_805);
        assert!(route.semantic_turns[0].contains("per-device capability memory"));
        assert_eq!(
            route.semantic_turn_ranges,
            conversation.semantic_turn_ranges[..1]
        );
        assert!(
            route
                .path
                .to_string_lossy()
                .ends_with("session.jsonl.agent-route-semantic")
        );
        assert!(!route.semantic_turns[0].contains("should not be copied"));
    }

    #[test]
    fn anchor_read_resolves_message_after_unrelated_prefix() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("session.jsonl");
        std::fs::write(
            &path,
            [user_jsonl_line("unrelated"), user_jsonl_line("target")].join("\n"),
        )
        .unwrap();
        let key = agent::refs::AgentConversationKey::new("project", "session.jsonl", path);
        let resolved = agent::refs::resolved_conversation_for_key(std::slice::from_ref(&key), &key);
        let transcript = agent::transcript::AgentTranscript::load(&resolved.key.path).unwrap();
        let anchor = transcript.message_anchor(&resolved, &transcript.messages[1]);
        let mut args = read_args(resolved.reference.canonical());
        args.anchor = Some(anchor.clone());

        let output = service()
            .run_read(&args, Some(std::slice::from_ref(&key)))
            .unwrap();

        let record = output
            .lines()
            .find(|line| line.starts_with("m2 user "))
            .unwrap_or_else(|| panic!("no record of m2: {output}"));
        assert!(record.ends_with(&format!(" anchor={anchor}")), "{output}");
        assert!(output.contains("| target\n"));
        assert!(!output.contains("| unrelated\n"));
    }

    #[test]
    fn agent_mode_ignores_tui_semantic_search() {
        let config: config::ConfigFile = toml::from_str(
            r#"
[search]
mode = "lexical"
[tui]
semantic_search = true
"#,
        )
        .unwrap();
        let search_config = config.search.unwrap_or_default();

        assert_eq!(
            agent::search::effective_agent_mode("needle", None, search_config.mode, None),
            SearchMode::Lexical
        );
    }

    #[test]
    fn agent_config_overrides_general_search_mode() {
        let config: config::ConfigFile = toml::from_str(
            r#"
[search]
mode = "lexical"
[agent]
mode = "hybrid"
"#,
        )
        .unwrap();

        assert_eq!(
            config.agent.unwrap().mode.or(config.search.unwrap().mode),
            Some(SearchMode::Hybrid)
        );
    }

    #[test]
    fn explicit_agent_values_override_agent_defaults() {
        assert_eq!(configured_usize(Some(7), 10, Some(12)), 7);
        assert_eq!(configured_usize(None, 10, Some(12)), 12);
        assert_eq!(configured_budget(false, Some(6000), Some(9000)), Some(6000));
        assert_eq!(configured_budget(false, None, Some(9000)), Some(9000));
        assert_eq!(configured_budget(true, Some(6000), Some(9000)), None);
    }

    /// A Claude Code transcript named `name`, last written at the UTC time
    /// `written`.
    fn transcript_written_at(
        directory: &Path,
        name: &str,
        written: &str,
    ) -> agent::refs::AgentConversationKey {
        let path = directory.join(name);
        std::fs::write(&path, user_jsonl_line("question")).unwrap();
        let written = chrono::DateTime::parse_from_rfc3339(written).unwrap();
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(written.into())
            .unwrap();
        agent::refs::AgentConversationKey::new("project", name, path)
    }

    /// A window from 15:40 to 15:45 UTC. The two rows' transcripts were last
    /// written at 15:42, and the two unlisted transcripts at 15:30 and 15:50.
    #[test]
    fn session_outside_window_isnt_reported_as_skipped_transcript() {
        let directory = tempfile::tempdir().unwrap();
        let at = |utc: &str| {
            chrono::DateTime::parse_from_rfc3339(utc)
                .unwrap()
                .with_timezone(&chrono::Local)
        };
        let window = TimeFilter {
            after: Some(at("2026-10-14T15:40:00Z")),
            before: Some(at("2026-10-14T15:45:00Z")),
        };
        let row_inside =
            transcript_written_at(directory.path(), "inside.jsonl", "2026-10-14T15:42:00Z");
        let row_outside =
            transcript_written_at(directory.path(), "outside.jsonl", "2026-10-14T15:42:00Z");
        let unlisted_written_before =
            transcript_written_at(directory.path(), "before.jsonl", "2026-10-14T15:30:00Z");
        let unlisted_written_after =
            transcript_written_at(directory.path(), "after.jsonl", "2026-10-14T15:50:00Z");
        let mut keys = vec![
            row_inside.clone(),
            row_outside.clone(),
            unlisted_written_before,
            unlisted_written_after.clone(),
        ];
        let row_paths = HashSet::from([row_inside.path.clone(), row_outside.path.clone()]);
        let rows_outside_window = HashSet::from([row_outside.path.clone()]);
        let service = service();
        let rows_inside_window = [conversation_from_agent_transcript(
            &service.load_transcript(&row_inside).unwrap(),
            &row_inside,
            service.clock().as_ref(),
        )];

        let outside = key_paths_outside_window(&keys, &rows_outside_window, &row_paths, &window);
        keys.retain(|key| !outside.contains(&key.path));
        let warnings = warnings_for_skipped_transcripts(&service, &rows_inside_window, &keys);

        let reported = warnings
            .iter()
            .filter_map(|warning| warning.reference.clone())
            .collect::<Vec<_>>();
        let unlisted_written_after_reference =
            agent::refs::resolved_conversation_for_key(&keys, &unlisted_written_after)
                .reference
                .canonical();
        assert_eq!(keys, [row_inside, unlisted_written_after]);
        assert_eq!(reported, [unlisted_written_after_reference]);
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
            anchor: None,
            focus: None,
            tail: None,
            lines: None,
            match_query: None,
            context: 3,
            perf_stats: false,
            output: output_flags(),
        };
        let service = service();

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
        assert_eq!(service.transcript_parse_count.get(), 1);
    }

    #[test]
    fn partial_target_reports_structured_warning_and_stable_ordinals() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("session.jsonl");
        std::fs::write(
            &path,
            [
                user_jsonl_line("first"),
                "{malformed".to_string(),
                user_jsonl_line("third"),
            ]
            .join("\n"),
        )
        .unwrap();
        let key = agent::refs::AgentConversationKey::new("project", "session.jsonl", path);
        let reference = key.conversation_ref().canonical();
        let args = AgentOutlineArgs {
            conversation: reference.clone(),
            perf_stats: false,
            output: output_flags(),
        };

        let output = service()
            .run_outline(&args, Some(std::slice::from_ref(&key)))
            .unwrap();

        assert!(output.contains("warnings=1"));
        assert!(output.contains("kind=malformed-transcript"));
        assert!(output.contains("lines%202"));
        assert!(output.contains("\nm1 user "), "{output}");
        assert!(output.contains("\nm2 user "), "{output}");
    }

    #[test]
    fn malformed_target_is_typed_service_failure() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("session.jsonl");
        std::fs::write(&path, "{malformed").unwrap();
        let key = agent::refs::AgentConversationKey::new("project", "session.jsonl", path);
        let reference = key.conversation_ref().canonical();
        let args = AgentOutlineArgs {
            conversation: reference.clone(),
            perf_stats: false,
            output: output_flags(),
        };

        let error = service()
            .run_outline(&args, Some(std::slice::from_ref(&key)))
            .unwrap_err();
        let AppError::Agent(error) = error else {
            panic!("expected typed agent error");
        };
        assert_eq!(error.kind, AgentErrorKind::MalformedTranscript);
        assert_eq!(error.reference.as_deref(), Some(reference.as_str()));
    }
}
