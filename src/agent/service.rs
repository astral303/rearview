use crate::agent;
use crate::agent::diagnostic::{AgentError, AgentErrorKind, AgentWarning, AgentWarningKind};
use crate::agent::perf::{CommandPerf, Stage};
use crate::agent::progress::{ProgressRecords, ProgressSchedule, run_reporting_progress};
use crate::agent::search::TranscriptRetention;
use crate::cli::{self, AgentCommand, AgentOutlineArgs, AgentReadArgs};
use crate::clock::{Clock, Zone};
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
/// The longest an agent command tries for refresh ownership while another
/// rearview refreshes the session cache; after it, the command reads the
/// cache as it is.
const REFRESH_OWNERSHIP_PATIENCE: std::time::Duration = std::time::Duration::from_secs(1);

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
    /// The cause of the load reading sessions from older copies.
    stale_cause: history::StaleCause,
}

impl LoadedSessions {
    /// Loads the history under `operation` without rereading a changed
    /// session whose cached row `rules` leave out.
    fn load(
        rules: &SelectionRules,
        perf: &CommandPerf,
        environment: &history::Environment,
        operation: &history::OperationScope,
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
                operation,
            )
        })?;
        Ok(Self {
            history,
            current_session_ids,
            workspace,
            stale_cause: operation.stale_cause(),
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
    /// The paths of the sessions the load read from the cache as it was
    /// while another rearview refreshed it, whose copy is older than the
    /// transcript or missing.
    stale_paths: HashSet<PathBuf>,
}

impl SessionSelection {
    /// The rows and keys of `loaded` that `rules` keep. Does not read a
    /// transcript: a key without a row is placed in the window by its file's
    /// modification time, and the skipped-transcript warnings are added by
    /// the caller. The stale-cache warning shows its time in `zone`.
    fn from_loaded(
        loaded: LoadedSessions,
        rules: &SelectionRules,
        perf: &CommandPerf,
        zone: Zone,
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
            stale_cause,
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
        let stale_copies: HashMap<&Path, history::provider::StaleCopy> = discovered
            .iter()
            .flat_map(|(_, sessions)| sessions)
            .filter_map(|session| Some((session.stub.locator.as_path(), session.stale?)))
            .collect();
        warnings.extend(stale_cache_warning(
            &keys,
            &stale_copies,
            &rules.window,
            zone,
            stale_cause,
        ));
        let stale_paths = stale_copies.keys().map(|path| path.to_path_buf()).collect();
        Ok(Self {
            conversations,
            scoped,
            keys,
            warnings,
            stale_paths,
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
    environment: history::Environment,
}

pub fn execute(command: AgentCommand, environment: history::Environment) -> Result<String> {
    AgentService::new(environment).execute(command)
}

/// `key`'s transcript as `environment`'s cache holds it, whether or not the
/// transcript changed since, its failure worded for the agent protocol.
/// Waits through another rearview's rebuild, publishing the wait on `wait`.
/// Parses nothing: a session the cache holds no copy of fails.
fn read_cached_transcript(
    environment: &history::Environment,
    wait: &history::WaitStatus,
    key: &agent::refs::AgentConversationKey,
) -> std::result::Result<agent::transcript::AgentTranscript, AgentError> {
    history::sqlite_cache::read_when_unlocked(
        environment.clock().as_ref(),
        wait,
        &history::Cancellation::default(),
        || {
            agent::transcript::AgentTranscript::load_cached_as_is(
                environment.cache_base(),
                key.source,
                &key.path,
                &key.subagents,
            )
        },
    )
    .map_err(|error| match error {
        AppError::Agent(error) => error,
        error => AgentError::io(Some(&key.path.to_string_lossy()), error.to_string()),
    })
}

/// `key`'s transcript parsed with the refresh owner's `permit`, for a session
/// the cache holds no copy of, its failure worded for the agent protocol.
fn parse_uncached_transcript(
    permit: history::ParsePermit<'_>,
    key: &agent::refs::AgentConversationKey,
) -> std::result::Result<agent::transcript::AgentTranscript, AgentError> {
    let path: &Path = &key.path;
    agent::transcript::AgentTranscript::parse_uncached(permit, key.source, path, &key.subagents)
        .map_err(|error| match error {
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
        })
}

/// The operation an agent command reads the session cache under: it tries
/// for refresh ownership for [`REFRESH_OWNERSHIP_PATIENCE`], then reads the
/// cache as it is. Waits are published on `meter`.
fn command_operation(base: &history::CacheBase, meter: &LoadMeter) -> history::OperationScope {
    history::OperationScope::new(
        base,
        history::Patience::AtMost(REFRESH_OWNERSHIP_PATIENCE),
        Arc::clone(meter.wait_status()),
    )
}

impl AgentService {
    pub fn new(environment: history::Environment) -> Self {
        Self {
            transcripts: RefCell::default(),
            bounded_transcripts: RefCell::default(),
            perf: CommandPerf::new(Arc::clone(environment.clock())),
            environment,
        }
    }

    /// `operation`, or the operation that replaced it, for a command that
    /// reads the sessions `targets` name, among `keys`, and the stale-cache
    /// record for them when it reads copies older than their transcripts.
    ///
    /// As the refresh owner, the command brings each target's copy up to
    /// date first. While another rearview keeps the cache past the
    /// operation's patience, or once the command's own cache writes stop, a
    /// target in the cache is read from its copy as it is, and named in the
    /// record when its transcript changed since and `window` reaches past
    /// the copy's last message. A target missing from the cache waits for
    /// ownership: only the owner parses transcripts.
    fn hold_cache_for_reading(
        &self,
        operation: history::OperationScope,
        targets: &[&agent::refs::AgentConversationKey],
        keys: &[agent::refs::AgentConversationKey],
        window: &TimeFilter,
    ) -> Result<(history::OperationScope, Option<AgentWarning>)> {
        let base = self.environment.cache_base();
        if let history::sqlite_cache::OperationAccess::Owner(_) = operation.access()? {
            self.refresh_targets(targets, &operation)?;
            if !operation.has_lost_write_permission() {
                return Ok((operation, None));
            }
        }
        let mut stale_copies = HashMap::new();
        for target in targets {
            let is_current = operation.read_when_unlocked(|| {
                history::sqlite_cache::find_cached_session_without_waiting(
                    base,
                    target.source,
                    &target.path,
                    &target.subagents,
                )?
                .map(|cached| cached.matches_source())
                .transpose()
            })?;
            match is_current {
                Some(true) => {}
                Some(false) => {
                    let cached_until = self.read_transcript(target).ok().and_then(|copy| {
                        let times = history::MessageTimes::of_messages(&copy.messages);
                        chrono::TimeZone::timestamp_opt(&chrono::Local, times.last_second()?, 0)
                            .single()
                    });
                    stale_copies.insert(
                        target.path.as_path(),
                        history::provider::StaleCopy { cached_until },
                    );
                }
                None => {
                    drop(operation);
                    let waiting = history::OperationScope::new(
                        base,
                        history::Patience::UntilAcquired,
                        Arc::clone(self.perf.load_meter().wait_status()),
                    );
                    waiting.access()?;
                    self.refresh_targets(targets, &waiting)?;
                    return Ok((waiting, None));
                }
            }
        }
        let warning = stale_cache_warning(
            keys,
            &stale_copies,
            window,
            self.clock().zone(),
            operation.stale_cause(),
        );
        Ok((operation, warning))
    }

    /// Brings the copy of each of `targets` up to date as `operation`'s
    /// refresh owner. A target its refresh does not list, such as a session
    /// holding no conversation, has no copy: it is parsed now, under the
    /// ownership, and kept for the command's reads.
    fn refresh_targets(
        &self,
        targets: &[&agent::refs::AgentConversationKey],
        operation: &history::OperationScope,
    ) -> Result<()> {
        for target in targets {
            let outcomes = history::provider::cache_sessions(
                &self.environment,
                target.source,
                &[target.path.as_path()],
                false,
                operation,
                self.perf.load_meter().agent(target.source),
            )?;
            let is_listed = outcomes
                .iter()
                .any(|outcome| matches!(outcome.read(), history::provider::SessionRead::Listed(_)));
            if !is_listed && let Some(permit) = operation.parse_permit()? {
                self.keep_uncached_transcript(permit, target);
            }
        }
        Ok(())
    }

    /// Parses `key`'s transcript with the refresh owner's `permit`, for a
    /// session the cache holds no copy of, and keeps it for the command's
    /// reads.
    fn keep_uncached_transcript(
        &self,
        permit: history::ParsePermit<'_>,
        key: &agent::refs::AgentConversationKey,
    ) {
        if self.transcripts.borrow().contains_key(&key.path) {
            return;
        }
        let parsed = self
            .perf
            .time_transcript_parse(key, || parse_uncached_transcript(permit, key));
        self.transcripts
            .borrow_mut()
            .insert(key.path.clone(), parsed);
    }

    /// `key`'s transcript as the session cache holds it.
    fn read_transcript(
        &self,
        key: &agent::refs::AgentConversationKey,
    ) -> std::result::Result<agent::transcript::AgentTranscript, AgentError> {
        read_cached_transcript(&self.environment, self.perf.load_meter().wait_status(), key)
    }

    fn clock(&self) -> &Arc<dyn Clock> {
        self.environment.clock()
    }

    /// `read`'s result once the session cache is readable: while another
    /// rearview's rebuild holds it exclusively locked, wait, publishing the
    /// wait on the command's meter.
    fn read_when_unlocked<T>(&self, read: impl FnMut() -> Result<T>) -> Result<T> {
        history::sqlite_cache::read_when_unlocked(
            self.clock().as_ref(),
            self.perf.load_meter().wait_status(),
            &history::Cancellation::default(),
            read,
        )
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
        let find_cached = || {
            history::sqlite_cache::find_cached_session_as_is_without_waiting(
                self.environment.cache_base(),
                key.source,
                &key.path,
                &key.subagents,
            )
        };
        if self.read_when_unlocked(find_cached)?.is_none() {
            return self.load_transcript(key);
        }
        let no_longer_cached = || AppError::SessionCacheNotReady {
            session: key.session_id.clone(),
            reason: "cache no longer available".to_owned(),
        };
        let transcript = self.perf.time_transcript_parse(key, || {
            self.read_when_unlocked(|| {
                find_cached()?
                    .ok_or_else(no_longer_cached)?
                    .agent_range(&key.path, range)?
                    .ok_or_else(no_longer_cached)
            })
        })?;
        self.bounded_transcripts
            .borrow_mut()
            .insert(cache_key, transcript.clone());
        Ok(transcript)
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
            .map_err(protocol_error)
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
        let loaded = self
            .perf
            .time_transcript_parse(key, || self.read_transcript(key));
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
            ..
        } = self.select_sessions(&SelectionRules::new(time, &args.scope, &agent_config))?;
        let (environment, wait) = (&self.environment, perf.load_meter().wait_status());
        let load_transcript = |key: &agent::refs::AgentConversationKey| {
            reads.time_read(key, || {
                read_cached_transcript(environment, wait, key).map_err(AppError::from)
            })
        };
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
                        clock.local_wall(),
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
                        clock.local_wall(),
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
                        clock.local_wall(),
                    )
                }))
            }
        }
    }

    /// Loads the history and the environment, applies `rules`, and adds the
    /// warnings for the selection's skipped and malformed transcripts.
    fn select_sessions(&self, rules: &SelectionRules) -> Result<SessionSelection> {
        let perf = &self.perf;
        let operation = command_operation(self.environment.cache_base(), perf.load_meter());
        let loaded = LoadedSessions::load(rules, perf, &self.environment, &operation)?;
        let mut selection =
            SessionSelection::from_loaded(loaded, rules, perf, self.clock().zone())?;
        let skipped = perf.time(Stage::SkippedTranscripts, || {
            warnings_for_skipped_transcripts(
                self,
                &operation,
                &selection.conversations,
                &selection.keys,
                &selection.stale_paths,
            )
        })?;
        operation.finish()?;
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
                self.clock().local_wall(),
            ))
        })
    }

    fn run_within(&self, args: &cli::AgentWithinArgs) -> Result<String> {
        let config = config::load_config()?;
        let search_config = config.search.unwrap_or_default();
        let agent_config = config.agent.unwrap_or_default();
        let window = args.time.resolve(self.clock().as_ref())?;
        let perf = &self.perf;
        let operation = command_operation(self.environment.cache_base(), perf.load_meter());
        let (keys, _) = perf.time(Stage::Keys, || {
            discover_agent_keys(&self.environment, perf.load_meter(), &operation)
        })?;
        let resolved = resolve_agent_conversation_arg(&args.conversation, &keys)?;
        let (operation, stale_cache) =
            self.hold_cache_for_reading(operation, &[&resolved.key], &keys, &window)?;
        let transcript = perf
            .time(Stage::Transcript, || self.load_transcript(&resolved.key))
            .map_err(|error| target_error(error, &resolved))?;
        let mut warnings = transcript_warning(&transcript, &resolved.reference.canonical())
            .into_iter()
            .collect::<Vec<_>>();
        warnings.extend(stale_cache);
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
        let rendered = perf.time(Stage::Render, || {
            agent::search::attach_transcript_metadata(&mut output, &resolved, &transcript);
            apply_configured_render_policy(&mut output, &agent_config);
            agent::search::format_agent_output_with_warnings(
                &output,
                &warnings,
                self.clock().local_wall(),
            )
        });
        operation.finish()?;
        Ok(rendered)
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
/// `meter` records what each provider's discovery and cache read. The cache
/// is read, and a root it holds no session of is cached, under `operation`.
fn discover_agent_keys(
    environment: &history::Environment,
    meter: &LoadMeter,
    operation: &history::OperationScope,
) -> Result<(Vec<agent::refs::AgentConversationKey>, Vec<AgentWarning>)> {
    let mut keys = Vec::new();
    let mut warnings = Vec::new();
    for &storage in environment.storages() {
        let provider = storage.source().provider();
        match provider_keys(
            environment,
            storage,
            meter.agent(storage.source()),
            operation,
        ) {
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

/// Every key under every root `storage` stores sessions in, the cache read
/// under `operation`.
fn provider_keys(
    environment: &history::Environment,
    storage: &dyn history::provider::SessionStorage,
    meter: AgentMeter,
    operation: &history::OperationScope,
) -> Result<Vec<agent::refs::AgentConversationKey>> {
    let mut keys = Vec::new();
    for root in storage.roots()? {
        keys.extend(root_keys(environment, storage, &root, meter, operation)?);
    }
    Ok(keys)
}

/// Keys for the sessions a fresh discovery lists under `root`, not for the
/// cached sessions: a cached session can outlive its transcripts.
///
/// A session whose path names its key is keyed without being read. Every
/// other session takes the directory and id it recorded from the session
/// cache, read under `operation`, so a corpus that can run to gigabytes is
/// not parsed again. The cache is read only for such a session.
fn root_keys(
    environment: &history::Environment,
    storage: &dyn history::provider::SessionStorage,
    root: &history::provider::SessionRoot,
    meter: AgentMeter,
    operation: &history::OperationScope,
) -> Result<Vec<agent::refs::AgentConversationKey>> {
    let provider = storage.source().provider();
    let clock = environment.clock().as_ref();
    meter.discovering();
    let discovering = clock.now();
    let stubs = storage.discover(root, environment.lister())?.stubs;
    meter.discovered(1, stubs.len(), clock.elapsed_since(discovering));
    let unkeyed: Vec<&Path> = stubs
        .iter()
        .filter(|stub| !agent::refs::is_keyed_by_path(provider, &stub.locator))
        .map(|stub| stub.locator.as_path())
        .collect();
    let recorded = if unkeyed.is_empty() {
        HashMap::new()
    } else {
        recorded_sessions(environment, storage, root, &unkeyed, meter, operation)?
    };
    Ok(stubs
        .iter()
        .filter_map(|stub| {
            let recorded = if agent::refs::is_keyed_by_path(provider, &stub.locator) {
                None
            } else {
                recorded.get(&stub.cache_key).cloned().flatten()
            };
            agent::refs::session_key(provider, stub, recorded.as_ref())
        })
        .collect())
}

/// The directory and id each session `storage` keeps under `root` recorded,
/// by cache key, from the session cache. When the cache holds no session of
/// the root, `operation`'s refresh owner first caches the sessions at
/// `unkeyed`; while another rearview keeps the cache, those sessions stay
/// without a record. A session the cache records as holding no conversation,
/// or as unreadable, has no record either.
fn recorded_sessions(
    environment: &history::Environment,
    storage: &dyn history::provider::SessionStorage,
    root: &history::provider::SessionRoot,
    unkeyed: &[&Path],
    meter: AgentMeter,
    operation: &history::OperationScope,
) -> Result<HashMap<String, Option<history::provider::RecordedSession>>> {
    let cached = cached_recorded_sessions(environment, storage, root, meter, operation)?;
    if !cached.is_empty() {
        return Ok(cached);
    }
    if matches!(
        operation.access()?,
        history::sqlite_cache::OperationAccess::AsIs
    ) {
        return Ok(cached);
    }
    history::provider::cache_sessions(
        environment,
        storage.source(),
        unkeyed,
        false,
        operation,
        meter,
    )?;
    cached_recorded_sessions(environment, storage, root, meter, operation)
}

/// The session cache database's record of each session `storage` keeps
/// under `root`, by cache key, as
/// [`history::sqlite_cache::SqliteCache::recorded_sessions`] reads it; empty
/// without a database holding the storage's sessions. Waits through another
/// rearview's rebuild, as `operation` reads. `meter` records the read.
fn cached_recorded_sessions(
    environment: &history::Environment,
    storage: &dyn history::provider::SessionStorage,
    root: &history::provider::SessionRoot,
    meter: AgentMeter,
    operation: &history::OperationScope,
) -> Result<HashMap<String, Option<history::provider::RecordedSession>>> {
    meter.reading_cache();
    let reading = environment.clock().now();
    let recorded =
        operation.read_when_unlocked(|| match history::sqlite_cache::SqliteCache::open_waiting(
            environment.cache_base(),
        )? {
            Some(cache) if cache.storage_is_current(storage.cache())? => {
                cache.recorded_sessions(storage.cache(), &root.path)
            }
            _ => Ok(HashMap::new()),
        })?;
    meter.cache_read(recorded.len(), environment.clock().elapsed_since(reading));
    Ok(recorded)
}

/// The stale-cache record for `keys` whose session the load read from the
/// cache as it was, for `cause`, as `stale_copies` names them by path: each
/// one missing from the cache, and each one whose copy's messages end
/// before `window` does. Its `cached-until` is the earliest such end, in
/// `zone`. `None` when no key counts.
fn stale_cache_warning(
    keys: &[agent::refs::AgentConversationKey],
    stale_copies: &HashMap<&Path, history::provider::StaleCopy>,
    window: &TimeFilter,
    zone: Zone,
    cause: history::StaleCause,
) -> Option<AgentWarning> {
    let mut refs = Vec::new();
    let mut earliest_end: Option<chrono::DateTime<chrono::Local>> = None;
    for key in keys {
        let Some(stale) = stale_copies.get(key.path.as_path()) else {
            continue;
        };
        if let Some(cached_until) = stale.cached_until {
            if window.before.is_some_and(|before| before <= cached_until) {
                continue;
            }
            earliest_end = Some(earliest_end.map_or(cached_until, |end| end.min(cached_until)));
        }
        refs.push(
            agent::refs::resolved_conversation_for_key(keys, key)
                .reference
                .canonical(),
        );
    }
    (!refs.is_empty()).then(|| {
        AgentWarning::stale_cache(
            &refs,
            earliest_end.map(|end| end.with_timezone(&zone)),
            cause,
        )
    })
}

/// Warnings for `keys` without a row, and for rows with malformed lines. A
/// key in `stale_paths` gets none: the stale-cache record names it. A key
/// without a row is parsed as `operation`'s refresh owner; while another
/// rearview keeps the cache, its warning reports the missing copy. Fails
/// when the operation's access to the cache fails.
fn warnings_for_skipped_transcripts(
    service: &AgentService,
    operation: &history::OperationScope,
    conversations: &[history::Conversation],
    keys: &[agent::refs::AgentConversationKey],
    stale_paths: &HashSet<PathBuf>,
) -> Result<Vec<AgentWarning>> {
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
        if known.contains(key.path.as_path()) || stale_paths.contains(&key.path) {
            continue;
        }
        let reference = agent::refs::resolved_conversation_for_key(keys, key)
            .reference
            .canonical();
        if let Some(permit) = operation.parse_permit()? {
            service.keep_uncached_transcript(permit, key);
        }
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
    Ok(warnings)
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

/// When the transcript at `path` was last written. `None` when the file cannot
/// be inspected, so callers keep it rather than silently dropping it.
fn transcript_last_write(path: &std::path::Path) -> Option<chrono::DateTime<chrono::Local>> {
    std::fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .map(chrono::DateTime::<chrono::Local>::from)
}

/// `error` as the `protocol agent-error` record an agent command prints.
pub fn protocol_error(error: AppError) -> AppError {
    match structured_agent_error(error) {
        AppError::Agent(agent_error) => {
            AppError::AgentProtocol(agent::diagnostic::format_error(&agent_error))
        }
        error => error,
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
        AppError::TimeFilter(error) => {
            AgentError::out_of_range(Some("command"), error.to_string()).into()
        }
        error @ (AppError::SessionCacheOutOfSpace { .. } | AppError::NoSessionCacheDirectory) => {
            AgentError::io(None, format!("{}: {error}", crate::APP_NAME)).into()
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
        let operation = command_operation(self.environment.cache_base(), perf.load_meter());
        let discovered;
        let keys = match keys {
            Some(keys) => keys,
            None => {
                discovered = perf
                    .time(Stage::Keys, || {
                        discover_agent_keys(&self.environment, perf.load_meter(), &operation)
                    })?
                    .0;
                &discovered
            }
        };
        let agent_config = config::load_config()?.agent.unwrap_or_default();
        let options = agent_protocol_options(&args.output, &agent_config, self.clock().as_ref())?;
        let (mut resolved_refs, focus) = resolve_agent_read_args(args, keys)?;
        let targets: Vec<&agent::refs::AgentConversationKey> = resolved_refs
            .iter()
            .map(|(_, resolved)| &resolved.key)
            .collect();
        let (operation, stale_cache) =
            self.hold_cache_for_reading(operation, &targets, keys, &TimeFilter::default())?;
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
        let mut warnings = resolved_refs
            .iter()
            .zip(transcripts.iter())
            .filter_map(|((_, resolved), transcript)| {
                transcript_warning(transcript, &resolved.reference.canonical())
            })
            .collect::<Vec<_>>();
        warnings.extend(stale_cache);
        let output = perf
            .time(Stage::Render, || {
                agent::protocol::format_read_with_warnings(
                    &requests,
                    priority,
                    slice.as_ref(),
                    options,
                    &warnings,
                    self.clock().local_wall(),
                )
            })
            .map_err(|error| match resolved_refs.first() {
                Some((_, resolved)) => target_error(error, resolved),
                None => structured_agent_error(error),
            })?;
        operation.finish()?;
        Ok(output)
    }

    pub(crate) fn run_outline(
        &self,
        args: &AgentOutlineArgs,
        keys: Option<&[agent::refs::AgentConversationKey]>,
    ) -> Result<String> {
        let perf = &self.perf;
        let operation = command_operation(self.environment.cache_base(), perf.load_meter());
        let discovered;
        let keys = match keys {
            Some(keys) => keys,
            None => {
                discovered = perf
                    .time(Stage::Keys, || {
                        discover_agent_keys(&self.environment, perf.load_meter(), &operation)
                    })?
                    .0;
                &discovered
            }
        };
        let agent_config = config::load_config()?.agent.unwrap_or_default();
        let options = agent_protocol_options(&args.output, &agent_config, self.clock().as_ref())?;
        let outline_ref = agent::refs::parse_outline_ref(&args.conversation)?;
        let resolved = outline_ref.resolve_conversation(keys)?;
        let (operation, stale_cache) =
            self.hold_cache_for_reading(operation, &[&resolved.key], keys, &TimeFilter::default())?;
        let transcript = perf
            .time(Stage::Transcript, || {
                self.load_transcript_for_range(
                    &resolved.key,
                    outline_ref.range.filter(|_| outline_ref.subagent.is_none()),
                )
            })
            .map_err(|error| target_error(error, &resolved))?;
        let mut warnings: Vec<AgentWarning> =
            transcript_warning(&transcript, &resolved.reference.canonical())
                .into_iter()
                .collect();
        warnings.extend(stale_cache);
        let request = agent::protocol::ReadRequest {
            resolved: &resolved,
            transcript: &transcript,
            range: outline_ref.range,
            subagent: outline_ref.subagent,
            call: None,
        };
        let output = perf
            .time(Stage::Render, || {
                agent::protocol::format_outline_with_warnings(
                    &request,
                    options,
                    &warnings,
                    self.clock().local_wall(),
                )
            })
            .map_err(|error| target_error(error, &resolved))?;
        operation.finish()?;
        Ok(output)
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

/// A service reading no agent's storage and this test's session cache, on
/// test time.
#[cfg(test)]
fn test_service() -> AgentService {
    AgentService::new(history::environment::test_support::test_environment(
        Vec::new(),
        None,
    ))
}

#[cfg(test)]
pub(crate) fn run_agent_read(
    args: &AgentReadArgs,
    keys: Option<&[agent::refs::AgentConversationKey]>,
) -> Result<String> {
    test_service().run_read(args, keys)
}

#[cfg(test)]
pub(crate) fn run_agent_outline(
    args: &AgentOutlineArgs,
    keys: Option<&[agent::refs::AgentConversationKey]>,
) -> Result<String> {
    test_service().run_outline(args, keys)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::test_support::{assistant_jsonl_line, user_jsonl_line};
    use crate::cli::AgentOutputFlags;
    use crate::clock::TestClock;
    use crate::history::environment::test_support::{test_environment, waiting_operation};

    /// `storage`'s sessions loaded through the session cache under
    /// `cache_base` on test time, leaving `left_out`'s rows unread.
    fn load_through(
        storage: &dyn history::provider::SessionStorage,
        cache_base: &Path,
        left_out: history::provider::LeftOutRows,
    ) -> history::provider::LoadedSessions {
        let environment = test_environment(Vec::new(), Some(cache_base));
        let operation = waiting_operation(&environment);
        let loaded = history::provider::load_sessions(
            &environment,
            storage,
            false,
            None,
            AgentMeter::UNMETERED,
            left_out,
            &operation,
            &mut |_, _| {},
        )
        .unwrap();
        operation.finish().unwrap();
        loaded
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

    /// This test's own environment, reading Pi sessions under `directory`
    /// alone.
    fn pi_environment(directory: &Path) -> history::Environment {
        let storage: &'static dyn history::provider::SessionStorage =
            Box::leak(Box::new(history::provider::RootedStorage {
                inner: history::Source::Pi.provider().storage(),
                root: history::provider::SessionRoot::new(directory),
            }));
        test_environment(vec![storage], None)
    }

    /// The keys a key discovery over `environment` finds, as one operation
    /// that waits for refresh ownership, metered by `meter`.
    fn discovered_keys(
        environment: &history::Environment,
        meter: &LoadMeter,
    ) -> Vec<agent::refs::AgentConversationKey> {
        let operation = waiting_operation(environment);
        let (keys, warnings) = discover_agent_keys(environment, meter, &operation).unwrap();
        operation.finish().unwrap();
        assert!(warnings.is_empty(), "{warnings:?}");
        keys
    }

    #[test]
    fn sessions_key_carries_transcript_size_discovery_found() {
        let directory = tempfile::tempdir().unwrap();
        let stub = pi_session_stub(directory.path());

        let keys = discovered_keys(&pi_environment(directory.path()), &LoadMeter::default());

        assert_eq!(keys.len(), 1, "the session gets a key");
        assert_eq!(keys[0].transcript_bytes, Some(stub.fingerprint.size));
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
        let environment = test_environment(Vec::new(), Some(cache_base.path()));
        let mut discovery_keys = root_keys(
            &environment,
            &storage,
            &storage.root,
            AgentMeter::UNMETERED,
            &waiting_operation(&environment),
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
                .with_timezone(&Zone::System)
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
            stale_cause: history::StaleCause::AnotherRearview,
        };

        let listing = SessionSelection::from_loaded(
            sessions,
            &rules,
            &CommandPerf::new(Arc::new(TestClock::default())),
            Zone::System,
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
                stale_cause: history::StaleCause::AnotherRearview,
            };
            let selection = SessionSelection::from_loaded(
                sessions,
                &rules,
                &CommandPerf::new(Arc::new(TestClock::default())),
                Zone::System,
            )
            .unwrap();
            agent::sessions::format_sessions_with_warnings(
                &selection.listing(&rules.window, 0, 10).unwrap(),
                None,
                &selection.warnings,
                agent::test_support::written_at(),
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

    /// Two Claude Code sessions recorded in one directory, `CHANGED_SESSION`
    /// at 06:30 and another at 06:31, cached by a load, and their storage.
    struct CachedClaudeSessions {
        _claude: history::provider::claude::tests::ConfigDir,
        directory: tempfile::TempDir,
        cache_base: tempfile::TempDir,
        changed: PathBuf,
        storage: &'static dyn history::provider::SessionStorage,
    }

    const CHANGED_SESSION: &str = "0f000000-0000-4000-8000-000000000001";

    impl CachedClaudeSessions {
        fn new() -> Self {
            let claude = history::provider::claude::tests::ConfigDir::new();
            let directory = tempfile::tempdir().unwrap();
            let changed = claude_session_in(&claude, directory.path(), CHANGED_SESSION, 30);
            claude_session_in(
                &claude,
                directory.path(),
                "0f000000-0000-4000-8000-000000000002",
                31,
            );
            let cache_base = tempfile::tempdir().unwrap();
            let storage: &'static dyn history::provider::SessionStorage =
                Box::leak(Box::new(history::provider::RootedStorage {
                    inner: history::Source::Claude.provider().storage(),
                    root: history::provider::SessionRoot::new(
                        changed.parent().unwrap().parent().unwrap(),
                    )
                    .in_agent_tree(),
                }));
            load_through(
                storage,
                cache_base.path(),
                history::provider::LeftOutRows::NONE,
            );
            Self {
                _claude: claude,
                directory,
                cache_base,
                changed,
                storage,
            }
        }

        /// Append a question at 06:40 UTC to `CHANGED_SESSION`.
        fn append_to_the_changed_session(&self) {
            let question = serde_json::json!({
                "type": "user", "timestamp": "2026-07-26T06:40:00.000Z",
                "cwd": self.directory.path(), "message": {"role": "user", "content": "one more"}
            });
            let mut transcript = std::fs::read_to_string(&self.changed).unwrap();
            transcript.push_str(&format!("{question}\n"));
            std::fs::write(&self.changed, transcript).unwrap();
        }

        /// The selection `rules` make while another rearview owns the
        /// cache's refresh.
        fn select_beside_another_refresh(&self, rules: &SelectionRules) -> SessionSelection {
            let other_refresh = history::sqlite_cache::refresh::RefreshOwnership::acquire_for_test(
                &history::CacheBase::new(
                    self.cache_base.path(),
                    history::ProcessIdentity::default(),
                    Arc::new(TestClock::default()),
                ),
            )
            .unwrap();
            let environment = test_environment(vec![self.storage], Some(self.cache_base.path()));
            let perf = CommandPerf::new(Arc::clone(environment.clock()));
            let operation = command_operation(environment.cache_base(), perf.load_meter());
            let loaded = LoadedSessions::load(rules, &perf, &environment, &operation).unwrap();
            operation.finish().unwrap();
            let selection =
                SessionSelection::from_loaded(loaded, rules, &perf, environment.clock().zone())
                    .unwrap();
            drop(other_refresh);
            selection
        }
    }

    fn every_session_in(window: TimeFilter) -> SelectionRules {
        SelectionRules {
            window,
            scope: agent::search::AgentSearchScope::Global,
            excludes_current_session: false,
            excluded_projects: history::ExcludedProjects::default(),
        }
    }

    /// The reference a selection's keys give `session_id`.
    fn reference_of(selection: &SessionSelection, session_id: &str) -> String {
        let key = selection
            .keys
            .iter()
            .find(|key| key.session_id == session_id)
            .unwrap();
        agent::refs::resolved_conversation_for_key(&selection.keys, key)
            .reference
            .canonical()
    }

    fn utc_at(minute: u32, second: u32) -> chrono::DateTime<Zone> {
        chrono::TimeZone::with_ymd_and_hms(&chrono::Utc, 2026, 7, 26, 6, minute, second)
            .unwrap()
            .with_timezone(&Zone::System)
    }

    const STALE_CACHE_SENTENCE: &str = "another rearview is refreshing these sessions; repeat the command after it finishes to include their newest messages";

    fn warning_records(warnings: &[AgentWarning]) -> Vec<String> {
        agent::diagnostic::format_warning_records(warnings).1
    }

    /// Rule 7: while another rearview refreshes, a selection reads the cache
    /// as it is and names the session whose transcript changed since.
    #[test]
    fn a_selection_beside_another_refresh_reads_the_cache_and_names_a_changed_session() {
        let sessions = CachedClaudeSessions::new();
        sessions.append_to_the_changed_session();

        let selection =
            sessions.select_beside_another_refresh(&every_session_in(TimeFilter::default()));

        let changed = selection
            .conversations
            .iter()
            .find(|row| row.session_id == CHANGED_SESSION)
            .unwrap();
        assert_eq!(changed.message_count, 2, "the row is the cached copy");
        assert_eq!(
            warning_records(&selection.warnings),
            [format!(
                "protocol agent-warning stale-cache refs={} cached-until={} : {STALE_CACHE_SENTENCE}\n",
                reference_of(&selection, CHANGED_SESSION),
                utc_at(30, 5).to_rfc3339_opts(chrono::SecondsFormat::Secs, false)
            )]
        );
    }

    /// Rule 7: a session the cache has never held counts as changed, without
    /// the end of cached data.
    #[test]
    fn a_selection_beside_another_refresh_names_a_session_the_cache_lacks_without_cached_until() {
        let sessions = CachedClaudeSessions::new();
        let claude_folder = sessions.changed.parent().unwrap();
        let new_session = "0f000000-0000-4000-8000-000000000003";
        let user = serde_json::json!({
            "type": "user", "timestamp": "2026-07-26T06:45:00.000Z",
            "cwd": sessions.directory.path(), "message": {"role": "user", "content": "new"}
        });
        std::fs::write(
            claude_folder.join(format!("{new_session}.jsonl")),
            format!("{user}\n"),
        )
        .unwrap();

        let selection =
            sessions.select_beside_another_refresh(&every_session_in(TimeFilter::default()));

        assert_eq!(
            warning_records(&selection.warnings),
            [format!(
                "protocol agent-warning stale-cache refs={} : {STALE_CACHE_SENTENCE}\n",
                reference_of(&selection, new_session)
            )]
        );
    }

    /// Rule 7: a window that ends inside the cached copy's messages misses
    /// nothing the cache lacks.
    #[test]
    fn a_selection_whose_window_ends_inside_the_cached_messages_doesnt_warn() {
        let sessions = CachedClaudeSessions::new();
        sessions.append_to_the_changed_session();

        let selection = sessions.select_beside_another_refresh(&every_session_in(TimeFilter {
            after: Some(utc_at(29, 0)),
            before: Some(utc_at(30, 3)),
        }));

        assert!(
            selection
                .conversations
                .iter()
                .any(|row| row.session_id == CHANGED_SESSION)
        );
        assert!(
            warning_records(&selection.warnings).is_empty(),
            "{:?}",
            selection.warnings
        );
    }

    /// Each of two zones shows its own offset and local times, whatever zone
    /// the machine is set to, and reads `--after` in that zone: only the
    /// session recorded at 06:31 UTC has a message from 06:31 on.
    #[test]
    fn agent_sessions_shows_and_reads_times_in_the_zone_of_its_clock() {
        let sessions = CachedClaudeSessions::new();
        for (hours_east, after, shown) in [
            (
                9,
                "2026-07-26T15:31",
                " at=2026-07-26T15:31:00 to=2026-07-26T15:31:05 ",
            ),
            (
                -3,
                "2026-07-26T03:31",
                " at=2026-07-26T03:31:00 to=2026-07-26T03:31:05 ",
            ),
        ] {
            let offset = chrono::FixedOffset::east_opt(hours_east * 3600).unwrap();
            let environment = history::environment::test_support::environment_on_clock(
                vec![sessions.storage],
                Some(sessions.cache_base.path()),
                Arc::new(TestClock::in_zone(Zone::Fixed(offset))),
            );

            let output = AgentService::new(environment)
                .execute(agent_command(&["sessions", "--all", "--after", after]))
                .unwrap();

            let header = output.lines().next().unwrap();
            assert!(
                header.contains(&format!(" tz={offset}")),
                "the header names {offset}:\n{output}"
            );
            assert!(
                output.contains("\nsessions matched=1 "),
                "--after {after} at {offset} holds one session:\n{output}"
            );
            assert!(
                output.contains(shown),
                "the session's times are shown at {offset}:\n{output}"
            );
        }
    }

    impl CachedClaudeSessions {
        /// The output of the agent command `args` while another rearview owns
        /// the cache's refresh, or its error report.
        fn run_beside_another_refresh(&self, args: &[&str]) -> String {
            let other_refresh = history::sqlite_cache::refresh::RefreshOwnership::acquire_for_test(
                &history::CacheBase::new(
                    self.cache_base.path(),
                    history::ProcessIdentity::default(),
                    Arc::new(TestClock::default()),
                ),
            )
            .unwrap();
            let environment = test_environment(vec![self.storage], Some(self.cache_base.path()));
            let output = AgentService::new(environment).execute(agent_command(args));
            drop(other_refresh);
            output.unwrap_or_else(crate::error_report)
        }

        /// The reference of `CHANGED_SESSION`.
        fn changed_reference(&self) -> String {
            reference_of(
                &self.select_beside_another_refresh(&every_session_in(TimeFilter::default())),
                CHANGED_SESSION,
            )
        }

        /// Start a sub-agent of `CHANGED_SESSION` after the session was
        /// cached.
        fn start_a_sub_agent_of_the_changed_session(&self) {
            let subagents = self.changed.with_extension("").join("subagents");
            std::fs::create_dir_all(&subagents).unwrap();
            let turn = serde_json::json!({
                "type": "user", "timestamp": "2026-07-26T06:41:00.000Z",
                "message": {"role": "user", "content": "explore"}
            });
            std::fs::write(subagents.join("agent-a.jsonl"), format!("{turn}\n")).unwrap();
            std::fs::write(
                subagents.join("agent-a.meta.json"),
                r#"{"agentType":"Explore"}"#,
            )
            .unwrap();
        }
    }

    /// `CHANGED_SESSION`'s `stale-cache` record, its copy cached until 06:30:05.
    fn changed_session_record(reference: &str) -> String {
        format!(
            "protocol agent-warning stale-cache refs={reference} cached-until={} : {STALE_CACHE_SENTENCE}\n",
            utc_at(30, 5).to_rfc3339_opts(chrono::SecondsFormat::Secs, false)
        )
    }

    /// `minute` and `second` past 06:00 UTC on the fixture's day, as a
    /// `--after` or `--before` value in this machine's zone.
    fn at(minute: u32, second: u32) -> String {
        utc_at(minute, second)
            .format("%Y-%m-%dT%H:%M:%S")
            .to_string()
    }

    /// Every command warns of the changed session's stale copy for each
    /// window that reaches past the copy's last message, and only for those.
    /// The copy ends at 06:30:05; the transcript gained a message at 06:40.
    #[test]
    #[ignore = "fixed in fix step 5"]
    fn every_command_warns_of_a_stale_copy_for_each_window_past_its_end() {
        let sessions = CachedClaudeSessions::new();
        let reference = sessions.changed_reference();
        sessions.append_to_the_changed_session();
        let windows: [(&str, Vec<String>, bool); 4] = [
            ("no window", Vec::new(), true),
            (
                "a window ending inside the copy",
                vec!["--after".into(), at(29, 0), "--before".into(), at(30, 3)],
                false,
            ),
            (
                "a window spanning the copy's end",
                vec!["--after".into(), at(29, 0), "--before".into(), at(45, 0)],
                true,
            ),
            (
                "a window starting after the copy's end",
                vec!["--after".into(), at(35, 0)],
                true,
            ),
        ];
        let commands: [(&str, Vec<&str>); 5] = [
            ("sessions", vec!["sessions", "--all"]),
            ("search", vec!["search", "question", "--all"]),
            ("within", vec!["within", &reference, "question"]),
            ("read", vec!["read", &reference]),
            ("outline", vec!["outline", &reference]),
        ];
        let record = changed_session_record(&reference);
        let mut wrong = Vec::new();
        for (command, command_args) in &commands {
            for (window, window_args, warns) in &windows {
                let args: Vec<&str> = command_args
                    .iter()
                    .copied()
                    .chain(window_args.iter().map(String::as_str))
                    .collect();
                let output = sessions.run_beside_another_refresh(&args);
                if output.contains(&record) != *warns {
                    wrong.push(format!(
                        "{command} with {window}: expected the record {}; got:\n{output}",
                        if *warns { "present" } else { "absent" }
                    ));
                }
            }
        }
        assert!(wrong.is_empty(), "{}", wrong.join("\n"));
    }

    /// A session that started a sub-agent since it was cached is read from
    /// its copy as it is beside another rearview's refresh: `read` doesn't
    /// wait for ownership, and `search` names it in the `stale-cache`
    /// record instead of an `io` warning.
    #[test]
    #[ignore = "fixed in fix step 5"]
    fn a_session_whose_sub_agents_changed_is_read_from_its_copy_beside_another_refresh() {
        let sessions = CachedClaudeSessions::new();
        let reference = sessions.changed_reference();
        sessions.start_a_sub_agent_of_the_changed_session();

        let read = sessions.run_beside_another_refresh(&["read", &reference]);
        let search = sessions.run_beside_another_refresh(&["search", "question", "--all"]);

        assert!(
            read.contains("protocol agent-warning stale-cache refs=")
                && read.contains("a question"),
            "read reads the stale copy:\n{read}"
        );
        assert!(
            !search.contains("not in the session cache"),
            "search doesn't report the copy as missing:\n{search}"
        );
        assert!(
            search.contains(&format!("stale-cache refs={reference}")),
            "search names the session in the stale-cache record:\n{search}"
        );
    }

    /// One unreadable directory is named in an `io` warning, and the cached
    /// sessions under it stay listed.
    #[test]
    #[ignore = "fixed in fix step 5"]
    fn an_unreadable_directory_is_named_in_an_io_warning_and_its_cached_sessions_stay_listed() {
        let sessions = CachedClaudeSessions::new();
        let folder = sessions.changed.parent().unwrap();
        let lister = Arc::new(history::provider::walk::test_support::ScriptedLister::default());
        lister.refuse(folder);
        let environment = history::Environment::new(
            vec![sessions.storage],
            sessions.cache_base.path().to_path_buf(),
            history::ProcessIdentity::default(),
            Arc::new(TestClock::default()),
            lister,
        );

        let output = AgentService::new(environment)
            .execute(agent_command(&["sessions", "--all"]))
            .unwrap_or_else(crate::error_report);

        let folder_name = folder.file_name().unwrap().to_string_lossy();
        assert!(
            output
                .lines()
                .any(|line| line.starts_with("protocol agent-warning io")
                    && line.contains(&*folder_name)),
            "an io warning names the unreadable directory:\n{output}"
        );
        assert!(
            output.contains("\nsessions matched=2 "),
            "the cached sessions under the unreadable directory are listed:\n{output}"
        );
    }

    /// Beside another rearview's refresh, search ranks a changed session on
    /// its cached copy: text only the transcript holds finds nothing, and the
    /// `stale-cache` record names the session.
    #[test]
    fn search_beside_another_refresh_ranks_a_changed_session_on_its_cached_copy() {
        let sessions = CachedClaudeSessions::new();
        let reference = sessions.changed_reference();
        sessions.append_to_the_changed_session();

        let output = sessions.run_beside_another_refresh(&["search", "one more", "--all"]);

        assert!(
            !output.contains("\nconversation rank="),
            "search doesn't rank text the cached copy lacks:\n{output}"
        );
        assert!(
            output.contains(&format!("stale-cache refs={reference}")),
            "a stale-cache record names the changed session:\n{output}"
        );
    }

    /// Beside another rearview's refresh, `read` of a session whose copy
    /// holds a tool result with an image beside its text reads the copy as
    /// it is, after its transcript changed.
    #[test]
    fn read_beside_another_refresh_reads_a_stale_copy_holding_a_mixed_tool_result() {
        let sessions = CachedClaudeSessions::new();
        let call = serde_json::json!({
            "type": "assistant", "timestamp": "2026-07-26T06:30:06.000Z",
            "message": {"role": "assistant", "content": [
                {"type": "tool_use", "id": "toolu_1", "name": "Read", "input": {"file_path": "a.png"}}
            ]}
        });
        let result = serde_json::json!({
            "type": "user", "timestamp": "2026-07-26T06:30:07.000Z",
            "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "toolu_1", "content": [
                {"type": "text", "text": "the image read"},
                {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AAAA"}}
            ]}]}
        });
        let mut transcript = std::fs::read_to_string(&sessions.changed).unwrap();
        transcript.push_str(&format!("{call}\n{result}\n"));
        std::fs::write(&sessions.changed, transcript).unwrap();
        load_through(
            sessions.storage,
            sessions.cache_base.path(),
            history::provider::LeftOutRows::NONE,
        );
        let reference = sessions.changed_reference();
        sessions.append_to_the_changed_session();

        let output = sessions.run_beside_another_refresh(&["read", &reference, "--tool-results"]);

        assert!(
            output.contains("the image read"),
            "read shows the cached tool result:\n{output}"
        );
        assert!(
            output.contains(&format!("stale-cache refs={reference}")),
            "a stale-cache record names the changed session:\n{output}"
        );
    }

    /// `args` after `rearview agent`, parsed as the binary parses them.
    fn agent_command(args: &[&str]) -> AgentCommand {
        let argv = ["rearview", "agent"]
            .into_iter()
            .chain(args.iter().copied());
        let parsed = <crate::cli::Args as clap::Parser>::try_parse_from(argv).unwrap();
        let Some(crate::cli::Commands::Agent { command }) = parsed.command else {
            panic!("{args:?} is not an agent command");
        };
        *command
    }

    /// Rule 8: a cache write that finds less than 1 GB free on the cache's
    /// drive stops the command with an `agent-error` record naming the free
    /// space, and the cache database is deleted.
    #[test]
    fn a_command_whose_cache_write_finds_less_than_a_gigabyte_free_reports_an_agent_error() {
        let sessions = CachedClaudeSessions::new();
        sessions.append_to_the_changed_session();
        let environment =
            test_environment(vec![sessions.storage], Some(sessions.cache_base.path()))
                .with_free_space(950_000_000);

        let outcome = AgentService::new(environment).execute(agent_command(&["sessions", "--all"]));

        let Err(AppError::AgentProtocol(record)) = outcome else {
            panic!("the command fails with an agent-error record: {outcome:?}");
        };
        assert_eq!(
            record,
            "protocol agent-error kind=io detail=rearview:%20not%20enough%20disk%20space%20for%20the%20session%20cache%20%280.9%20GB%20free%2C%20needs%20at%20least%201%20GB%29.%20Free%20some%20space%20and%20run%20rearview%20again.\n"
        );
        assert!(
            !sessions
                .cache_base
                .path()
                .join(history::sqlite_cache::DATABASE_FILE_NAME)
                .exists()
        );
    }

    /// Fail the first commit to the session cache in `base` that changes
    /// session data, marking `watch` then, until the result drops.
    fn fail_the_first_data_commit(
        base: &Path,
        watch: &history::provider::ParseWatch,
    ) -> history::sqlite_cache::connection::test_hooks::Installed {
        use std::sync::atomic::{AtomicBool, Ordering};
        let has_failed = Arc::new(AtomicBool::new(false));
        let watch = watch.clone();
        history::sqlite_cache::connection::test_hooks::on_every_connection(
            base,
            move |connection| {
                if connection.is_readonly(rusqlite::MAIN_DB).unwrap() {
                    return;
                }
                let changes_data = Arc::new(AtomicBool::new(false));
                let changed = Arc::clone(&changes_data);
                connection
                    .update_hook(Some(
                        move |_: rusqlite::hooks::Action, _: &str, table: &str, _: i64| {
                            if table != "refresh_state" {
                                changed.store(true, Ordering::SeqCst);
                            }
                        },
                    ))
                    .unwrap();
                let rolled_back = Arc::clone(&changes_data);
                connection
                    .rollback_hook(Some(move || rolled_back.store(false, Ordering::SeqCst)))
                    .unwrap();
                let (has_failed, watch) = (Arc::clone(&has_failed), watch.clone());
                connection
                    .commit_hook(Some(move || {
                        let fails = changes_data.swap(false, Ordering::SeqCst)
                            && !has_failed.swap(true, Ordering::SeqCst);
                        if fails {
                            watch.mark();
                        }
                        fails
                    }))
                    .unwrap();
            },
        )
    }

    /// The sentence a `stale-cache` record ends with after the command's own
    /// cache writes stopped.
    const WRITES_STOPPED_SENTENCE: &str = "rearview could not update these sessions in the session cache; repeat the command to include their newest messages";

    /// A command whose cache writes stop parses nothing more: it reads the
    /// sessions it has not refreshed from their copies, and names them as
    /// sessions it could not update.
    #[test]
    fn a_command_whose_cache_writes_stop_reads_the_rest_from_their_copies_and_names_them() {
        let sessions = crate::tui::claude_home::ChangedSessionsInTwoRoots::new();
        let _failing = fail_the_first_data_commit(sessions.cache.path(), &sessions.watch);

        let output = AgentService::new(sessions.environment())
            .execute(agent_command(&["sessions", "--all"]))
            .unwrap_or_else(crate::error_report);

        assert_eq!(
            sessions.watch.parses_after_mark(),
            0,
            "parsing stops at the failed commit:\n{output}"
        );
        assert!(
            output.lines().any(|line| {
                line.starts_with("protocol agent-warning stale-cache refs=")
                    && line.ends_with(&format!(" : {WRITES_STOPPED_SENTENCE}"))
            }),
            "a stale-cache record names the sessions the command could not update:\n{output}"
        );
    }

    /// A cache write that finds the drive short of space, at any write,
    /// stops the command before its next parse.
    #[test]
    fn a_command_whose_cache_write_finds_the_drive_short_parses_nothing_after_it() {
        let checks = {
            let sessions = crate::tui::claude_home::ChangedSessionsInTwoRoots::new();
            let drive = Arc::new(history::sqlite_cache::DriveRunningOut::after(
                usize::MAX,
                || {},
            ));
            AgentService::new(sessions.environment().with_disk_space(drive.clone()))
                .execute(agent_command(&["sessions", "--all"]))
                .unwrap();
            drive.answers()
        };
        assert!(checks > 0, "the command checks free space");

        for roomy_checks in 0..checks {
            let sessions = crate::tui::claude_home::ChangedSessionsInTwoRoots::new();
            let watch = sessions.watch.clone();
            let drive =
                history::sqlite_cache::DriveRunningOut::after(roomy_checks, move || watch.mark());

            let outcome =
                AgentService::new(sessions.environment().with_disk_space(Arc::new(drive)))
                    .execute(agent_command(&["sessions", "--all"]));

            assert!(
                matches!(&outcome, Err(AppError::AgentProtocol(record))
                    if record.starts_with("protocol agent-error kind=io detail=rearview:%20not%20enough%20disk%20space")),
                "running out after {roomy_checks} checks stops the command: {outcome:?}"
            );
            assert_eq!(
                sessions.watch.parses_after_mark(),
                0,
                "running out after {roomy_checks} checks stops parsing"
            );
        }
    }

    /// SQLite's exclusive lock on the session cache database in a base, as a
    /// rebuild holds it, taken the moment a command first opens a connection
    /// that can write, until released.
    struct RebuildAtFirstWriter {
        lock: Arc<std::sync::Mutex<Option<rusqlite::Connection>>>,
        _installed: history::sqlite_cache::connection::test_hooks::Installed,
    }

    impl RebuildAtFirstWriter {
        fn on(base: &Path) -> Self {
            let lock = Arc::new(std::sync::Mutex::new(None));
            let is_taken = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let (held, directory) = (Arc::clone(&lock), base.to_path_buf());
            let installed = history::sqlite_cache::connection::test_hooks::on_every_connection(
                base,
                move |connection| {
                    if connection.is_readonly(rusqlite::MAIN_DB).unwrap()
                        || is_taken.swap(true, std::sync::atomic::Ordering::SeqCst)
                    {
                        return;
                    }
                    *held.lock().unwrap() = Some(
                        history::sqlite_cache::refresh::tests::exclusive_lock(&directory),
                    );
                },
            );
            Self {
                lock,
                _installed: installed,
            }
        }

        fn release(&self) {
            self.lock.lock().unwrap().take();
        }
    }

    /// `read`, `outline` and `within` wait through another rearview's
    /// rebuild, publishing the wait, then read the session.
    #[test]
    fn targeted_commands_wait_through_a_rebuild_and_publish_it() {
        let sessions = CachedClaudeSessions::new();
        let reference = sessions.changed_reference();
        for command in [
            vec!["read", reference.as_str()],
            vec!["outline", reference.as_str()],
            vec!["within", reference.as_str(), "question"],
        ] {
            let rebuild = RebuildAtFirstWriter::on(sessions.cache_base.path());
            let environment =
                test_environment(vec![sessions.storage], Some(sessions.cache_base.path()));
            let (wait_sent, wait_received) = std::sync::mpsc::channel();
            let args: Vec<String> = command.iter().map(|arg| arg.to_string()).collect();
            let running = std::thread::spawn(move || {
                let service = AgentService::new(environment);
                wait_sent
                    .send(Arc::clone(service.perf.load_meter().wait_status()))
                    .unwrap();
                let args: Vec<&str> = args.iter().map(String::as_str).collect();
                service.execute(agent_command(&args))
            });
            let wait = wait_received.recv().unwrap();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while wait.current() != Some(history::OtherRearview::Rebuilding) {
                assert!(
                    std::time::Instant::now() < deadline,
                    "{command:?} waits for the rebuild"
                );
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            std::thread::sleep(std::time::Duration::from_millis(300));
            let still_waiting = wait.current() == Some(history::OtherRearview::Rebuilding);
            rebuild.release();
            assert!(
                still_waiting,
                "{command:?} publishes its wait for the rebuild"
            );

            let output = running.join().unwrap().unwrap_or_else(crate::error_report);
            assert!(
                output.contains("question") && !output.contains("agent-error"),
                "{command:?} reads the session after the rebuild:\n{output}"
            );
        }
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

    /// Key discovery over a root the cache lacks loads the root as the
    /// refresh owner, and keys its session from what that load recorded.
    #[test]
    fn a_session_in_a_root_without_a_cache_is_keyed_from_its_owners_load() {
        let directory = tempfile::tempdir().unwrap();
        let stub = pi_session_stub(directory.path());
        let environment = pi_environment(directory.path());
        let meter = LoadMeter::default();

        let keys = discovered_keys(&environment, &meter);

        assert_eq!(keys.len(), 1, "the session gets a key");
        let recorded = history::sqlite_cache::SqliteCache::open_waiting(environment.cache_base())
            .unwrap()
            .map(|cache| {
                cache
                    .recorded_sessions(
                        history::Source::Pi.provider().storage().cache(),
                        directory.path(),
                    )
                    .unwrap()
            })
            .unwrap_or_default();
        assert!(
            recorded.contains_key(&stub.cache_key),
            "key discovery records the session through a load as the refresh owner"
        );
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
        let environment = pi_environment(directory.path());
        discovered_keys(&environment, &LoadMeter::default());
        let later = stub.locator.with_file_name("later.jsonl");
        std::fs::copy(&stub.locator, &later).unwrap();

        let keys = discovered_keys(&environment, &LoadMeter::default());

        assert!(
            keys.iter().all(|key| key.path != later),
            "a session missing from a cache of the root's other sessions doesn't get a key: {keys:?}"
        );
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

        let output = test_service()
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
                .with_timezone(&Zone::System)
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
        let service = test_service();
        let rows_inside_window = [conversation_from_agent_transcript(
            &parse_uncached_transcript(crate::history::FOR_FIXTURES, &row_inside).unwrap(),
            &row_inside,
            service.clock().as_ref(),
        )];

        let outside = key_paths_outside_window(&keys, &rows_outside_window, &row_paths, &window);
        keys.retain(|key| !outside.contains(&key.path));
        let operation = waiting_operation(&service.environment);
        let warnings = warnings_for_skipped_transcripts(
            &service,
            &operation,
            &rows_inside_window,
            &keys,
            &HashSet::new(),
        )
        .unwrap();

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
        let service = test_service();

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
        assert_eq!(
            service
                .perf
                .records(None)
                .matches("perf transcript ")
                .count(),
            1,
            "the second read doesn't load the transcript again"
        );
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

        let output = test_service()
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

        let error = test_service()
            .run_outline(&args, Some(std::slice::from_ref(&key)))
            .unwrap_err();
        let AppError::Agent(error) = error else {
            panic!("expected typed agent error");
        };
        assert_eq!(error.kind, AgentErrorKind::MalformedTranscript);
        assert_eq!(error.reference.as_deref(), Some(reference.as_str()));
    }
}
