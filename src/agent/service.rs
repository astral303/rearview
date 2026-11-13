use crate::agent;
use crate::agent::diagnostic::{AgentError, AgentErrorKind, AgentWarning, AgentWarningKind};
use crate::agent::perf::{CommandPerf, Stage};
use crate::agent::progress::{ProgressRecords, ProgressSchedule, run_reporting_progress};
use crate::cli::{self, AgentCommand, AgentOutlineArgs, AgentReadArgs};
use crate::clock::{Clock, Zone};
use crate::config;
use crate::config::{AgentConfig, AgentScopeConfig, SemanticConfig};
use crate::error::{AppError, Result};
use crate::history;
use crate::history::listing::{self, SessionOrigin, SessionTimes};
use crate::history::load_meter::{AgentMeter, LoadMeter};
use crate::history::path::CanonicalDirectories;
use crate::history::provider::walk::Canonicaliser;
use crate::history::sqlite_cache::WriteErrorClass;
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
// The variables of the session the command runs in, not a provider's root.
#[allow(clippy::disallowed_methods)]
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

/// The keys `window` leaves out: the keys of `rows_outside_window`, and each
/// key without a row whose transcript `window` does not list. A key without
/// a row that stays in shows its diagnostics.
fn key_paths_outside_window(
    keys: &[agent::refs::AgentConversationKey],
    rows_outside_window: &HashSet<PathBuf>,
    row_paths: &HashSet<PathBuf>,
    window: &TimeFilter,
) -> HashSet<PathBuf> {
    keys.iter()
        .filter(|key| {
            rows_outside_window.contains(&key.path)
                || (!row_paths.contains(&key.path)
                    && !listing::is_listed(
                        SessionOrigin::Discovered,
                        SessionTimes::LastWrite(transcript_last_write(&key.path)),
                        window,
                    ))
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
    /// The directories the command's keys canonicalise, each once.
    project_directories: Arc<CanonicalDirectories>,
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
            .then(|| environment.workspace().ok())
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
            project_directories: environment.canonical_directories(),
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
    /// The paths of the stale sessions: the load read them from the cache as
    /// it was while another rearview refreshed it, and their copy is older
    /// than the transcript or missing.
    stale_paths: HashSet<PathBuf>,
    /// The warning for each session the load read as not holding a
    /// conversation or as failing to read, by transcript path.
    unlisted_warnings: HashMap<PathBuf, AgentWarning>,
}

impl SessionSelection {
    /// The rows and keys of `loaded` that `rules` keep, and the stale
    /// sessions the command covers. Does not read a transcript: a key
    /// without a row is placed in the window by its file's modification
    /// time, and the skipped-transcript warnings are added by the caller.
    ///
    /// The stale sessions are collected before the window applies: a session
    /// whose copy ends before the window starts doesn't have a row in it, and
    /// the record reports it. The scope, the excluded projects and the current session
    /// leave it out.
    fn from_loaded(
        loaded: LoadedSessions,
        rules: &SelectionRules,
        perf: &CommandPerf,
    ) -> Result<(Self, StaleCoverage)> {
        let LoadedSessions {
            history:
                history::LoadedHistory {
                    mut conversations,
                    ignored,
                    discovered,
                    failures,
                    unreadable_directories,
                },
            current_session_ids,
            workspace,
            stale_cause,
            project_directories,
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
            .filter(|conversation| {
                !listing::is_listed(
                    SessionOrigin::Discovered,
                    SessionTimes::Messages(&conversation.message_times),
                    &rules.window,
                )
            })
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
            loaded_agent_keys(
                &discovered,
                &failures,
                &unreadable_directories,
                workspace.as_ref(),
                &project_directories,
            )
        });
        let excluded_key_paths = excluded_key_paths(&keys, &excluded_row_paths, &row_paths);
        let stale_copies: HashMap<&Path, history::provider::StaleCopy> = discovered
            .iter()
            .flat_map(|(_, sessions)| sessions)
            .filter_map(|session| Some((session.stub.locator.as_path(), session.stale?)))
            .collect();
        let mut stale_coverage = StaleCoverage::new(stale_cause);
        if !stale_copies.is_empty() {
            let every_key = if workspace.is_some() {
                std::borrow::Cow::Owned(perf.time(Stage::Discovery, || {
                    loaded_agent_keys(&discovered, &[], &[], None, &project_directories).0
                }))
            } else {
                std::borrow::Cow::Borrowed(keys.as_slice())
            };
            stale_coverage.add_stale_keys(
                keys.iter().filter(|key| {
                    !excluded_key_paths.contains(&key.path)
                        && !is_current_session(&key.session_id, &current_session_ids)
                }),
                &every_key,
                &stale_copies,
                &rules.window,
            );
        }
        // Remove the keys of excluded rows and of rows outside the window too:
        // a key without a row is reported as a skipped transcript.
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
        let stale_paths = stale_copies.keys().map(|path| path.to_path_buf()).collect();
        let unlisted_warnings = discovered
            .into_iter()
            .flat_map(|(_, sessions)| sessions)
            .filter_map(|session| Some((session.stub.locator, session.warning?)))
            .collect();
        let selection = Self {
            conversations,
            scoped,
            keys,
            warnings,
            stale_paths,
            unlisted_warnings,
        };
        Ok((selection, stale_coverage))
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
    /// The title and summary of each target the command's read listed, by
    /// path, as the listed row shows them. A transcript's own are its last
    /// parse's: a name kept outside the transcript can change without the
    /// transcript.
    target_titles_and_summaries: RefCell<HashMap<PathBuf, history::provider::TitleAndSummary>>,
    /// The stale sessions the command has met so far; the command prints
    /// them in one record when it renders.
    stale_coverage: RefCell<Option<StaleCoverage>>,
    perf: CommandPerf,
    environment: history::Environment,
}

pub fn execute(command: AgentCommand, environment: history::Environment) -> Result<String> {
    AgentService::new(environment).execute(command)
}

/// `key`'s transcript as `environment`'s cache holds it, whether or not the
/// transcript changed since. Waits through another rearview's rebuild,
/// publishing the wait on `wait`. Doesn't parse the transcript.
fn read_cached_session(
    environment: &history::Environment,
    wait: &history::WaitStatus,
    key: &agent::refs::AgentConversationKey,
) -> Result<agent::transcript::CachedTranscript> {
    history::sqlite_cache::read_when_unlocked(
        environment.clock().as_ref(),
        wait,
        &history::Cancellation::default(),
        || {
            agent::transcript::AgentTranscript::load_cached_as_is(
                environment.cache_base(),
                &key.address,
                &key.path,
            )
        },
    )
}

/// The copy [`read_cached_session`] reads of `key`'s transcript, its failure
/// worded for the agent protocol. The read fails when the session is not in
/// the session cache.
fn read_cached_transcript(
    environment: &history::Environment,
    wait: &history::WaitStatus,
    key: &agent::refs::AgentConversationKey,
) -> std::result::Result<agent::transcript::AgentTranscript, AgentError> {
    read_cached_session(environment, wait, key)
        .and_then(|cached| cached.into_copy(&key.session_id))
        .map_err(|error| match error {
            AppError::Agent(error) => error,
            error => AgentError::io(Some(&key.path.to_string_lossy()), error.to_string()),
        })
}

/// `row`'s key among `keys`, by its transcript's path.
fn key_of_row<'k>(
    keys: &'k [agent::refs::AgentConversationKey],
    row: &history::Conversation,
) -> Option<&'k agent::refs::AgentConversationKey> {
    keys.iter().find(|key| key.path == row.path)
}

/// The hit messages `hits` of `conversation`, a ranked session, read alone,
/// with its failure worded for the agent protocol. When another write
/// replaced the copy the ranking counted, the hit messages of the copy the
/// cache holds are counted and read, in one read snapshot. `None` when the
/// session is no longer in the session cache or its cached copy is not
/// ready. A query without terms doesn't read a message.
pub(crate) fn read_hit_messages(
    clock: &dyn Clock,
    wait: &history::WaitStatus,
    ranking: &search::AgentRanking,
    conversation: &history::Conversation,
    hits: &search::SessionHitMessages,
    key: &agent::refs::AgentConversationKey,
) -> std::result::Result<Option<agent::search::SearchedTranscript>, AgentError> {
    let Some(copy) = conversation.cached_session.clone() else {
        return Ok(None);
    };
    if ranking.counts_no_terms() {
        return Ok(Some(agent::search::SearchedTranscript {
            transcript: agent::transcript::AgentTranscript {
                path: key.path.clone(),
                messages: Vec::new(),
                malformed_lines: Vec::new(),
                summary: None,
                custom_title: None,
            },
            message_count: conversation.message_count,
            copy_generation: Some(hits.generation),
        }));
    }
    let read = history::sqlite_cache::read_copy_when_unlocked(
        clock,
        wait,
        &history::Cancellation::default(),
        || {
            copy.read_copy(|held| {
                if held.generation() == hits.generation {
                    held.agent_messages(&key.path, &hits.ordinals)
                } else {
                    held.agent_messages(&key.path, &ranking.hit_messages_in(held)?.ordinals)
                }
            })
        },
    );
    history::sqlite_cache::found(read).map_err(|error| match error {
        AppError::Agent(error) => error,
        error => AgentError::io(Some(&key.path.to_string_lossy()), error.to_string()),
    })
}

/// The estimated bytes the hit read of `conversation`, the ranked session at
/// `index`, reads: its transcript's size in the share of its messages that
/// hold hits; the whole size for a session read whole.
fn hit_read_bytes(
    ranking: &search::AgentRanking,
    conversation: &history::Conversation,
    index: usize,
    key: &agent::refs::AgentConversationKey,
) -> Option<u64> {
    let bytes = key.transcript_bytes?;
    let Some(hits) = ranking.hit_messages.of(index) else {
        return Some(bytes);
    };
    let messages = conversation.message_count.max(1) as u64;
    Some(bytes.saturating_mul(hits.ordinals.len() as u64) / messages)
}

/// `key`'s transcript parsed with the refresh owner's `permit`, for a session
/// not in the session cache, its failure worded for the agent protocol.
fn parse_uncached_transcript(
    permit: history::ParsePermit<'_>,
    key: &agent::refs::AgentConversationKey,
) -> std::result::Result<agent::transcript::AgentTranscript, AgentError> {
    agent::transcript::AgentTranscript::parse_uncached(
        permit,
        key.source,
        &key.path,
        &key.subagents,
    )
    .map_err(|error| agent::transcript::agent_read_error(&key.path, &error))
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

/// The keys of the sessions a command reads, by path, as holding the cache
/// for reading left them.
#[derive(Default)]
struct HeldTargets {
    keys: HashMap<PathBuf, agent::refs::AgentConversationKey>,
    /// The root of each target read as the cache holds it, by path. The
    /// copy's title is its last parse's.
    roots_read_as_is: HashMap<PathBuf, history::provider::SessionRoot>,
}

impl HeldTargets {
    fn keep(&mut self, key: agent::refs::AgentConversationKey) {
        self.keys.insert(key.path.clone(), key);
    }

    fn into_keys(self) -> impl Iterator<Item = agent::refs::AgentConversationKey> {
        self.keys.into_values()
    }

    fn holds(&self, target: &agent::refs::AgentConversationKey) -> bool {
        self.keys.contains_key(&target.path)
    }

    /// The held key of `target`, a session the cache was held for.
    fn of_target(
        &self,
        target: &agent::refs::AgentConversationKey,
    ) -> &agent::refs::AgentConversationKey {
        self.keys
            .get(&target.path)
            .expect("the cache was held for the target")
    }

    /// The root `target` was read under as the cache holds it; `None` for a
    /// target the command read and listed or parsed.
    fn root_read_as_is(
        &self,
        target: &agent::refs::AgentConversationKey,
    ) -> Option<&history::provider::SessionRoot> {
        self.roots_read_as_is.get(&target.path)
    }

    /// Records `root` as the root `target` is read under as the cache holds
    /// it.
    fn record_root_read_as_is(
        &mut self,
        target: &agent::refs::AgentConversationKey,
        root: history::provider::SessionRoot,
    ) {
        self.roots_read_as_is.insert(target.path.clone(), root);
    }
}

/// The targets [`AgentService::refresh_and_check_targets`] held, with what
/// checking their copies found.
#[derive(Default)]
struct CheckedTargets {
    held: HeldTargets,
    /// The targets read from copies older than their keys, by path.
    stale_copies: HashMap<PathBuf, history::provider::StaleCopy>,
    /// The targets without a copy or a parse, by path.
    missing_copies: HashSet<PathBuf>,
}

impl CheckedTargets {
    fn add(&mut self, checked: TargetCopy) {
        match checked {
            TargetCopy::Current(key) => self.held.keep(key),
            TargetCopy::Older(key, stale) => {
                self.stale_copies.insert(key.path.clone(), stale);
                self.held.keep(key);
            }
            TargetCopy::Missing(key) => {
                self.missing_copies.insert(key.path.clone());
                self.held.keep(key);
            }
        }
    }

    /// True when the command neither published nor parsed `path`'s target:
    /// its copy is older or missing.
    fn lacks_current_content(&self, path: &Path) -> bool {
        self.stale_copies.contains_key(path) || self.missing_copies.contains(path)
    }

    /// These targets, with `retried` in place of their earlier check.
    fn with_retried(mut self, retried: Self) -> Self {
        let HeldTargets {
            keys,
            roots_read_as_is,
        } = retried.held;
        for key in keys.into_values() {
            self.stale_copies.remove(&key.path);
            self.missing_copies.remove(&key.path);
            self.held.roots_read_as_is.remove(&key.path);
            self.held.keep(key);
        }
        self.held.roots_read_as_is.extend(roots_read_as_is);
        self.stale_copies.extend(retried.stale_copies);
        self.missing_copies.extend(retried.missing_copies);
        self
    }
}

/// A target's key, as [`AgentService::check_target_copy`] checks it against
/// its copy.
enum TargetCopy {
    /// The copy is current for the key, the key adopted another copy, or
    /// the command holds the target's transcript.
    Current(agent::refs::AgentConversationKey),
    /// The copy is older than the key.
    Older(
        agent::refs::AgentConversationKey,
        history::provider::StaleCopy,
    ),
    /// The session is not in the session cache, or its copy isn't ready to
    /// read.
    Missing(agent::refs::AgentConversationKey),
}

impl TargetCopy {
    fn key(&self) -> &agent::refs::AgentConversationKey {
        match self {
            Self::Current(key) | Self::Older(key, _) | Self::Missing(key) => key,
        }
    }
}

/// A target's key, as [`AgentService::check_copy_without_reading`] checks
/// it against its copy: an older copy is not read.
enum CopyCheck {
    /// The copy is current for the key, or the key adopted another copy.
    Current(agent::refs::AgentConversationKey),
    /// The copy is older than the key.
    Older(agent::refs::AgentConversationKey),
    /// The session is not in the session cache, or its copy isn't ready to
    /// read.
    Missing(agent::refs::AgentConversationKey),
}

/// The rule that decides whether a copy is current for a key.
type CopyCurrency = fn(
    &history::sqlite_cache::HeldCopy<'_>,
    &history::sqlite_cache::Publication,
) -> Result<history::sqlite_cache::Currency>;

fn currency_against_transcripts(
    copy: &history::sqlite_cache::HeldCopy<'_>,
    publication: &history::sqlite_cache::Publication,
) -> Result<history::sqlite_cache::Currency> {
    copy.currency(publication)
}

/// A targeted read of one session as a command keeps it for its key and
/// its reads: without the listed row's search texts.
enum TargetRead {
    /// The read listed the session: the row's address, sub-agents,
    /// publication of its copy, and title and summary.
    Listed {
        address: history::sqlite_cache::SessionAddress,
        subagents: Vec<PathBuf>,
        publication: history::sqlite_cache::Publication,
        title_and_summary: history::provider::TitleAndSummary,
        unwritten: history::provider::UnwrittenParse,
    },
    /// The read did not list the session.
    Unlisted {
        read: history::provider::SessionRead,
        unwritten: history::provider::UnwrittenParse,
    },
}

impl TargetRead {
    fn of(outcome: history::provider::SessionOutcome) -> Self {
        match outcome.into_read_and_unwritten() {
            (history::provider::SessionRead::Listed(row), unwritten) => Self::Listed {
                address: row
                    .cache_address
                    .expect("a row a targeted read lists has its cache address"),
                subagents: row.subagents,
                publication: row.publication,
                title_and_summary: history::provider::TitleAndSummary {
                    custom_title: row.custom_title,
                    summary: row.summary,
                },
                unwritten,
            },
            (read, unwritten) => Self::Unlisted { read, unwritten },
        }
    }
}

impl AgentService {
    pub fn new(environment: history::Environment) -> Self {
        let environment = environment.for_one_command();
        Self {
            transcripts: RefCell::default(),
            bounded_transcripts: RefCell::default(),
            target_titles_and_summaries: RefCell::default(),
            stale_coverage: RefCell::default(),
            perf: CommandPerf::new(Arc::clone(environment.clock())),
            environment,
        }
    }

    /// Every agent's keys, discovered under `operation` as
    /// [`discover_agent_keys`] discovers them, and timed as the command's
    /// key stage.
    fn discover_keys(&self, operation: &history::OperationScope) -> Result<DiscoveredKeys> {
        self.perf.time(Stage::Keys, || {
            discover_agent_keys(&self.environment, self.perf.load_meter(), operation)
        })
    }

    /// Adds `coverage`'s sessions to the command's stale sessions.
    fn cover_stale_sessions(&self, coverage: StaleCoverage) {
        let mut covered = self.stale_coverage.borrow_mut();
        match covered.as_mut() {
            Some(covered) => covered.join(coverage),
            None => *covered = Some(coverage),
        }
    }

    /// Adds the sessions of `changed` to the command's stale sessions:
    /// another rearview removed, rewrote or was writing them while the
    /// command read them. Resolves each reference against `keys`.
    fn cover_sessions_changed_while_read<'k>(
        &self,
        changed: impl IntoIterator<Item = &'k agent::refs::AgentConversationKey>,
        keys: &[agent::refs::AgentConversationKey],
    ) {
        let mut coverage = StaleCoverage::new(history::StaleCause::AnotherRearview);
        for key in changed {
            let reference = agent::refs::resolved_conversation_for_key(keys, key)
                .reference
                .canonical();
            coverage.add(reference, None);
        }
        if !coverage.sessions.is_empty() {
            self.cover_stale_sessions(coverage);
        }
    }

    /// `warnings` and the `stale-cache` record of the command's stale
    /// sessions, for `output`. Empties those sessions, so a second render
    /// does not name them again.
    ///
    /// The page of `agent sessions` and the hits of `agent search` decide
    /// which sessions the output shows before the budget fits them: a
    /// session the budget then drops stays named.
    fn with_stale_cache_record(
        &self,
        mut warnings: Vec<AgentWarning>,
        output: &dyn ShowsSessions,
    ) -> Vec<AgentWarning> {
        let coverage = self.stale_coverage.borrow_mut().take();
        warnings.extend(coverage.and_then(|coverage| coverage.record(self.clock().zone(), output)));
        warnings
    }

    /// `operation`, or the operation that replaced it, and the keys of the
    /// sessions `targets` name, for a command that reads them, among `keys`.
    /// Adds the targets it reads from copies older than their keys to the
    /// command's stale sessions, unless `window`, the command's own, ends at
    /// or before the copy's last message.
    ///
    /// While another rearview keeps the cache and a target is missing from
    /// it, the command waits for ownership, checks again each target it
    /// neither published nor parsed, and refreshes those whose copy is older
    /// or missing: only the owner parses transcripts.
    /// Once the command's own writes stop, it does not refresh again, and a
    /// missing target is reported as not in the session cache. The refresh
    /// reads below `reads`' roots and doesn't reread a target in `reads`.
    fn hold_cache_for_reading(
        &self,
        operation: history::OperationScope,
        targets: &[&agent::refs::AgentConversationKey],
        keys: &[agent::refs::AgentConversationKey],
        window: &TimeFilter,
        mut reads: KeyedReads,
    ) -> Result<(history::OperationScope, HeldTargets)> {
        let checked = self.refresh_and_check_targets(&operation, targets, &mut reads)?;
        let waits_for_another_rearview = !checked.missing_copies.is_empty()
            && operation.stale_cause() == history::StaleCause::AnotherRearview;
        let (operation, checked) = if waits_for_another_rearview {
            drop(operation);
            let waiting = history::OperationScope::new(
                self.environment.cache_base(),
                history::Patience::UntilAcquired,
                Arc::clone(self.perf.load_meter().wait_status()),
            );
            let retried: Vec<&agent::refs::AgentConversationKey> = targets
                .iter()
                .copied()
                .filter(|target| checked.lacks_current_content(&target.path))
                .collect();
            for target in &retried {
                self.transcripts.borrow_mut().remove(&target.path);
            }
            let rechecked = self.refresh_and_check_targets(&waiting, &retried, &mut reads)?;
            (waiting, checked.with_retried(rechecked))
        } else {
            (operation, checked)
        };
        let stale_copies: HashMap<&Path, history::provider::StaleCopy> = checked
            .stale_copies
            .iter()
            .map(|(path, stale)| (path.as_path(), *stale))
            .collect();
        let mut coverage = StaleCoverage::new(operation.stale_cause());
        coverage.add_stale_keys(keys, keys, &stale_copies, window);
        self.cover_stale_sessions(coverage);
        Ok((operation, checked.held))
    }

    /// The keys of `targets`, each checked against its copy as
    /// [`Self::check_target_copy`] checks it. A target key discovery read
    /// keeps that read, from `keyed`, whatever the command's access now. As
    /// `operation`'s refresh owner, the command reads each other target
    /// whose copy is current without refreshing it, and brings the rest up
    /// to date first, reading each distinct session once whatever the
    /// number of targets naming it. A target this command read is
    /// checked as [`history::sqlite_cache::HeldCopy::currency_for_publisher`]
    /// checks it, as the printout checks its copy; one it reads as the
    /// cache holds it, against its transcripts.
    fn refresh_and_check_targets(
        &self,
        operation: &history::OperationScope,
        targets: &[&agent::refs::AgentConversationKey],
        keyed: &mut KeyedReads,
    ) -> Result<CheckedTargets> {
        let mut held = HeldTargets::default();
        let mut unread: Vec<&agent::refs::AgentConversationKey> = Vec::new();
        for &target in targets {
            if held.holds(target) || unread.iter().any(|kept| kept.path == target.path) {
                continue;
            }
            match keyed.sessions.remove(&target.path) {
                Some(read) => held.keep(self.hold_read(operation, target, read)?),
                None => unread.push(target),
            }
        }
        let read_as_is: Vec<TargetCopy> = match operation.acquire_access()? {
            history::sqlite_cache::OperationAccess::Owner(_) => {
                let (current, to_refresh) = self.check_copies_before_refresh(operation, unread);
                if !to_refresh.is_empty() {
                    self.perf.record_refresh_round(to_refresh.len());
                    let refreshed = self.perf.time(Stage::Refresh, || {
                        self.refresh_targets(&to_refresh, operation, keyed)
                    })?;
                    for key in refreshed {
                        held.keep(key);
                    }
                }
                current.into_iter().map(TargetCopy::Current).collect()
            }
            history::sqlite_cache::OperationAccess::AsIs => unread
                .into_iter()
                .map(|target| {
                    self.check_target_copy(operation, target.clone(), currency_against_transcripts)
                })
                .collect::<Result<_>>()?,
        };
        let mut checked = CheckedTargets::default();
        for key in held.into_keys() {
            checked.add(self.check_target_copy(operation, key, |held, publication| {
                held.currency_for_publisher(publication)
            })?);
        }
        for copy in read_as_is {
            let target = copy.key();
            let root = keyed
                .roots
                .get(&target.source)
                .and_then(|roots| roots.iter().find(|root| root.holds(&target.path)));
            if let Some(root) = root {
                checked.held.record_root_read_as_is(target, root.clone());
            }
            checked.add(copy);
        }
        Ok(checked)
    }

    /// The keys of `targets` whose copies are current against their
    /// transcripts, then the targets for the owner's refresh: those whose
    /// copies are older or missing, or whose check failed. A failed check
    /// is dropped; the refresh reports what fails while it reads.
    fn check_copies_before_refresh<'k>(
        &self,
        operation: &history::OperationScope,
        targets: Vec<&'k agent::refs::AgentConversationKey>,
    ) -> (
        Vec<agent::refs::AgentConversationKey>,
        Vec<&'k agent::refs::AgentConversationKey>,
    ) {
        let mut current = Vec::new();
        let mut to_refresh = Vec::new();
        for target in targets {
            match self.check_copy_without_reading(
                operation,
                target.clone(),
                currency_against_transcripts,
            ) {
                Ok(CopyCheck::Current(key)) => current.push(key),
                Ok(CopyCheck::Older(_) | CopyCheck::Missing(_)) | Err(_) => to_refresh.push(target),
            }
        }
        (current, to_refresh)
    }

    /// `target` checked against its copy by `currency`, without parsing:
    /// [`Self::check_copy_without_reading`], then [`Self::read_older_copy`]
    /// for an older copy. A target whose transcript the command already has
    /// is not checked.
    fn check_target_copy(
        &self,
        operation: &history::OperationScope,
        target: agent::refs::AgentConversationKey,
        currency: CopyCurrency,
    ) -> Result<TargetCopy> {
        if self.transcripts.borrow().contains_key(&target.path) {
            return Ok(TargetCopy::Current(target));
        }
        Ok(
            match self.check_copy_without_reading(operation, target, currency)? {
                CopyCheck::Current(key) => TargetCopy::Current(key),
                CopyCheck::Older(key) => self.read_older_copy(key),
                CopyCheck::Missing(key) => TargetCopy::Missing(key),
            },
        )
    }

    /// `target` checked against its copy for its key by `currency`, without
    /// reading the copy's entries. A copy to adopt is read as current: the
    /// key uses its generation and sub-agents.
    fn check_copy_without_reading(
        &self,
        operation: &history::OperationScope,
        mut target: agent::refs::AgentConversationKey,
        currency: CopyCurrency,
    ) -> Result<CopyCheck> {
        let base = self.environment.cache_base();
        let currency = operation.read_when_unlocked(|| {
            history::sqlite_cache::found(history::sqlite_cache::read_copy_without_waiting(
                base,
                &target.address,
                |held| currency(held, &target.publication),
            ))
        })?;
        Ok(match currency {
            Some(history::sqlite_cache::Currency::Current) => CopyCheck::Current(target),
            Some(history::sqlite_cache::Currency::Adopt {
                generation,
                subagents,
            }) => {
                target.publication = history::sqlite_cache::Publication::Published { generation };
                target.subagents = subagents;
                CopyCheck::Current(target)
            }
            Some(history::sqlite_cache::Currency::Older) => CopyCheck::Older(target),
            None => CopyCheck::Missing(target),
        })
    }

    /// `target`'s older copy, read as it is once and kept for the command's
    /// reads.
    fn read_older_copy(&self, target: agent::refs::AgentConversationKey) -> TargetCopy {
        let copy = self
            .perf
            .time_transcript_parse(&target, || self.read_transcript(&target));
        let cached_until = copy.as_ref().ok().and_then(|copy| {
            let times = history::MessageTimes::of_messages(&copy.messages);
            chrono::TimeZone::timestamp_opt(&chrono::Local, times.last_second()?, 0).single()
        });
        self.transcripts
            .borrow_mut()
            .insert(target.path.clone(), copy);
        TargetCopy::Older(target, history::provider::StaleCopy { cached_until })
    }

    /// The keys of `targets`, distinct sessions, each copy brought up to
    /// date as `operation`'s refresh owner, one targeted read per agent
    /// below `keyed`'s roots, and held as [`Self::hold_read`] holds its read.
    fn refresh_targets(
        &self,
        targets: &[&agent::refs::AgentConversationKey],
        operation: &history::OperationScope,
        keyed: &KeyedReads,
    ) -> Result<Vec<agent::refs::AgentConversationKey>> {
        let mut sources: Vec<history::Source> = Vec::new();
        for target in targets {
            if !sources.contains(&target.source) {
                sources.push(target.source);
            }
        }
        let mut held = Vec::with_capacity(targets.len());
        for source in sources {
            let of_source: Vec<&agent::refs::AgentConversationKey> = targets
                .iter()
                .copied()
                .filter(|target| target.source == source)
                .collect();
            let paths: Vec<&Path> = of_source
                .iter()
                .map(|target| target.path.as_path())
                .collect();
            let outcomes = self.read_targets_of(source, &paths, operation, keyed)?;
            for (target, outcome) in of_source.into_iter().zip(outcomes) {
                held.push(self.hold_read(operation, target, TargetRead::of(outcome))?);
            }
        }
        Ok(held)
    }

    /// `target`'s key as `read`, this command's read of its session, left
    /// it. A target the read lists is kept at its row's address, sub-agents
    /// and publication, and the entries the read parsed and its copy does
    /// not hold are kept for the command's reads. A target the read doesn't
    /// list, such as an empty session, doesn't have a copy:
    /// [`Self::keep_unlisted_read`] keeps what the read found of it.
    fn hold_read(
        &self,
        operation: &history::OperationScope,
        target: &agent::refs::AgentConversationKey,
        read: TargetRead,
    ) -> Result<agent::refs::AgentConversationKey> {
        match read {
            TargetRead::Listed {
                address,
                subagents,
                publication,
                title_and_summary,
                unwritten,
            } => {
                let key = agent::refs::AgentConversationKey {
                    address,
                    subagents,
                    publication,
                    ..target.clone()
                };
                self.target_titles_and_summaries
                    .borrow_mut()
                    .insert(key.path.clone(), title_and_summary);
                if let history::provider::UnwrittenParse::Entries(entries) = unwritten {
                    self.keep_parsed_transcript(&key, entries);
                }
                Ok(key)
            }
            TargetRead::Unlisted { read, unwritten } => {
                self.keep_unlisted_read(operation, target, read, unwritten)?;
                Ok(target.clone())
            }
        }
    }

    /// The title `key`'s agent keeps outside the session's transcript, read
    /// with every other such title under `root`, the session's root.
    fn read_external_title(
        &self,
        key: &agent::refs::AgentConversationKey,
        root: &history::provider::SessionRoot,
    ) -> Option<history::provider::SessionTitle> {
        self.environment
            .storages()
            .iter()
            .find(|storage| storage.source() == key.source)?
            .external_titles(root)
            .remove(&key.session_id)
    }

    /// `transcript`, the transcript of `key`'s session, under the title and
    /// summary the command's read listed for the session, when it listed it.
    fn titled_as_listed(
        &self,
        key: &agent::refs::AgentConversationKey,
        transcript: agent::transcript::AgentTranscript,
    ) -> agent::transcript::AgentTranscript {
        match self.target_titles_and_summaries.borrow().get(&key.path) {
            Some(shown) => transcript.titled(shown.clone()),
            None => transcript,
        }
    }

    /// The targeted read of `source`'s sessions at `paths`, one outcome
    /// each, below the roots key discovery canonicalised, as `keyed` holds
    /// them, or below the storage's roots without them.
    fn read_targets_of(
        &self,
        source: history::Source,
        paths: &[&Path],
        operation: &history::OperationScope,
        keyed: &KeyedReads,
    ) -> Result<Vec<history::provider::SessionOutcome>> {
        let meter = self.perf.load_meter().agent(source);
        match keyed.roots.get(&source) {
            Some(roots) => history::provider::cache_sessions_in_roots(
                &self.environment,
                source,
                roots
                    .iter()
                    .map(history::provider::SessionRoot::forgetting_failed_canonicalisations)
                    .collect(),
                paths,
                false,
                operation,
                meter,
            ),
            None => history::provider::cache_sessions(
                &self.environment,
                source,
                paths,
                false,
                operation,
                meter,
            ),
        }
    }

    /// Keeps `entries`, the entries a refresh parsed of `key`'s session and
    /// its copy does not hold, as the session's transcript for the
    /// command's reads.
    fn keep_parsed_transcript(
        &self,
        key: &agent::refs::AgentConversationKey,
        entries: history::TranscriptEntries,
    ) {
        let transcript = self.perf.time_transcript_parse(key, || {
            agent::transcript::AgentTranscript::from_transcript_entries(&key.path, entries)
                .map_err(|error| agent::transcript::agent_read_error(&key.path, &error))
        });
        self.transcripts
            .borrow_mut()
            .insert(key.path.clone(), transcript);
    }

    /// Keeps the transcript of `key`'s session for the command's reads when
    /// the refresh read the session as `read` without listing it: the
    /// entries its parse read, or the failure its warning reports for a file
    /// the parse rejected or could not read. The refresh owner parses, under
    /// `operation`, the transcript of a file discovery doesn't find and of an
    /// empty session restored from the cache, because the cache doesn't
    /// store an empty session's entries.
    /// Without ownership, such a session fails as its warning reports, or
    /// is reported as not in the session cache. A session missing from the
    /// cache, or one a failed discovery kept from being read, does not keep
    /// a transcript and is reported as not in the session cache.
    fn keep_unlisted_read(
        &self,
        operation: &history::OperationScope,
        key: &agent::refs::AgentConversationKey,
        read: history::provider::SessionRead,
        parsed: history::provider::UnwrittenParse,
    ) -> Result<()> {
        use history::provider::{SessionRead, UnwrittenParse};
        let reference = key.path.to_string_lossy();
        let keep_failure = |warning: &AgentWarning| {
            if let Some(failure) = AgentError::of_read_warning(warning, &reference) {
                self.transcripts
                    .borrow_mut()
                    .insert(key.path.clone(), Err(failure));
            }
        };
        let warning = match (&read, parsed) {
            (_, UnwrittenParse::Entries(entries)) => {
                self.keep_parsed_transcript(key, entries);
                return Ok(());
            }
            (SessionRead::Failed(_, Some(warning)), _)
            | (SessionRead::Empty(Some(warning)), UnwrittenParse::Rejected) => {
                keep_failure(warning);
                return Ok(());
            }
            (SessionRead::Empty(warning), _) => warning.as_ref(),
            (SessionRead::Missing, _) => None,
            (SessionRead::Listed(_) | SessionRead::Failed(_, None) | SessionRead::NotCached, _) => {
                return Ok(());
            }
        };
        if self.transcripts.borrow().contains_key(&key.path) {
            return Ok(());
        }
        let parsed = operation.parse_as_owner(|permit| {
            self.perf
                .time_transcript_parse(key, || parse_uncached_transcript(permit, key))
        })?;
        let parsed = match parsed {
            Some(parsed) => parsed,
            None => {
                if let Some(warning) = warning {
                    keep_failure(warning);
                }
                return Ok(());
            }
        };
        self.transcripts
            .borrow_mut()
            .insert(key.path.clone(), parsed);
        Ok(())
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

    /// `read`'s result over one copy once the session cache is readable:
    /// while another rearview's rebuild holds it exclusively locked, wait,
    /// publishing the wait on the command's meter.
    fn read_copy_when_unlocked<T>(
        &self,
        read: impl FnMut() -> history::sqlite_cache::CopyRead<T>,
    ) -> history::sqlite_cache::CopyRead<T> {
        history::sqlite_cache::read_copy_when_unlocked(
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
            return Ok(self.titled_as_listed(key, transcript.clone()));
        }
        let read = self.perf.time_transcript_parse(key, || {
            self.read_copy_when_unlocked(|| {
                history::sqlite_cache::read_copy_without_waiting(
                    self.environment.cache_base(),
                    &key.address,
                    |held| held.agent_range(&key.path, range),
                )
            })
        });
        let Some(transcript) = history::sqlite_cache::found(read)? else {
            return self.load_transcript(key);
        };
        self.bounded_transcripts
            .borrow_mut()
            .insert(cache_key, transcript.clone());
        Ok(self.titled_as_listed(key, transcript))
    }

    /// Runs `command`, writing `protocol agent-progress` records to stderr
    /// while it runs and, with `--perf-stats`, its `perf` records after.
    /// Once a write of the command found the cache's drive low on space,
    /// the command deletes the cache database and fails with the low-space
    /// error, whatever it returned, so its record reports the deletion's
    /// outcome. When the drive doesn't have room for a deleted cache, the
    /// command fails the same way, without a deletion.
    pub fn execute(&self, command: AgentCommand) -> Result<String> {
        let output = run_reporting_progress(
            ProgressRecords::new(command.name(), &self.perf, ProgressSchedule::DEFAULT),
            std::io::stderr(),
            || {
                let output = self.execute_inner(&command);
                let latch = self.environment.cache_base().low_space();
                if latch.free_bytes_awaiting_deletion().is_some() {
                    self.perf.time(Stage::CacheDeletion, || {
                        history::sqlite_cache::low_space::run_deletion(
                            self.environment.cache_base(),
                        );
                    });
                }
                match latch.error() {
                    Some(low_space) => Err(low_space),
                    None => output,
                }
            },
        );
        if command.reports_perf_stats() {
            eprint!("{}", self.perf.records(agent::peak_memory::peak_bytes()));
        }
        output
            .and_then(CommandOutput::records_if_complete)
            .map_err(protocol_error)
    }

    fn execute_inner(&self, command: &AgentCommand) -> Result<CommandOutput> {
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
        let held = self.transcripts.borrow().get(path).cloned();
        let loaded = match held {
            Some(held) => held,
            None => {
                let loaded = self
                    .perf
                    .time_transcript_parse(key, || self.read_transcript(key));
                self.transcripts
                    .borrow_mut()
                    .insert(path.to_path_buf(), loaded.clone());
                loaded
            }
        };
        loaded
            .map(|transcript| self.titled_as_listed(key, transcript))
            .map_err(AppError::from)
    }

    fn search(&self, args: &cli::AgentSearchArgs) -> Result<CommandOutput> {
        let perf = &self.perf;
        let reads = perf.search_reads();
        let config = config::load_config()?;
        let search_config = config.search.unwrap_or_default();
        let agent_config = config.agent.unwrap_or_default();
        let embedding_budget = configured_embedding_budget(&config.semantic.unwrap_or_default());
        // Resolved before loading so an inverted range fails before the
        // whole corpus is parsed.
        let time = args.time.resolve(self.clock().as_ref())?;
        let rules = SelectionRules::new(time, &args.scope, &agent_config);
        let SessionSelection {
            conversations,
            scoped,
            keys,
            warnings: base_warnings,
            ..
        } = self.select_sessions(&rules)?;
        let (environment, wait) = (&self.environment, perf.load_meter().wait_status());
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
                lexically_rank_scoped(
                    &conversations,
                    &args.query,
                    ScopedSessions {
                        indices: &scoped,
                        scope: rules.scope,
                    },
                    mode,
                    &time,
                    clock,
                )
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
        let search_transcripts = |request: &agent::search::AgentSearchRequest|
         -> Result<agent::search::LexicalSearchRun> {
            let ranking = rank()?;
            self.cover_sessions_changed_while_read(
                ranking
                    .no_longer_cached
                    .iter()
                    .filter_map(|&index| key_of_row(&keys, &conversations[index])),
                &keys,
            );
            for (index, error) in &ranking.failed_reads {
                if let Some(key) = key_of_row(&keys, &conversations[*index]) {
                    report_read_error(key, error);
                }
            }
            let read_session = |index: usize, key: &agent::refs::AgentConversationKey| {
                reads.time_read(key, || {
                    match ranking.hit_messages.of(index) {
                        Some(hits) => read_hit_messages(
                            environment.clock().as_ref(),
                            wait,
                            &ranking,
                            &conversations[index],
                            hits,
                            key,
                        ),
                        None => read_cached_transcript(environment, wait, key)
                            .map(|transcript| Some(transcript.into())),
                    }
                    .map_err(AppError::from)
                })
            };
            let run = perf.time(Stage::TranscriptSearch, || {
                agent::search::run_global_lexical_search_reporting(
                    request,
                    &conversations,
                    &keys,
                    &ranking.ranked,
                    agent::search::SessionReader {
                        read: read_session,
                        bytes: |index, key: &agent::refs::AgentConversationKey| {
                            hit_read_bytes(&ranking, &conversations[index], index, key)
                        },
                    },
                    Some(reads),
                )
            })?;
            for (key, error) in &run.failed_reads {
                report_read_error(key, error);
            }
            self.cover_sessions_changed_while_read(run.no_longer_cached.iter(), &keys);
            Ok(run)
        };
        match mode {
            SearchMode::Lexical | SearchMode::Exact => {
                let run = search_transcripts(&request)?;
                let mut output = run.output;
                apply_configured_render_policy(&mut output, &agent_config);
                let warnings = self.with_stale_cache_record(warnings.into_inner(), &output);
                Ok(self.render_search(&output, warnings))
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
                let warnings = self.with_stale_cache_record(warnings, &output);
                Ok(self.render_search(&output, warnings))
            }
            SearchMode::Hybrid => {
                let inputs = agent_inputs_for_indices(&conversations, &keys, &scoped)?;
                let (semantic, lexical) = run_hybrid_halves(
                    || {
                        perf.time(Stage::Semantic, || {
                            run_agent_semantic_hits(
                                self,
                                &args.query,
                                &inputs,
                                embedding_budget,
                                request.semantic_chunk_hits(),
                                request.window,
                            )
                        })
                    },
                    |pass| match pass {
                        HybridLexicalPass::ForFusion => {
                            search_transcripts(&agent::search::hybrid_lexical_request(&request))
                        }
                        HybridLexicalPass::Alone => search_transcripts(&request),
                    },
                )?;
                let mut output = match semantic {
                    Ok((semantic, semantic_warnings)) => {
                        warnings.borrow_mut().extend(semantic_warnings);
                        let (output, failed_reads) = self.fuse_semantic_hits(
                            semantic,
                            &lexical.copies_read,
                            &inputs,
                            &keys,
                            |semantic, inputs| {
                                agent::search::run_global_hybrid_search(
                                    &request,
                                    lexical.output.clone(),
                                    semantic,
                                    inputs,
                                )
                            },
                        );
                        warnings.borrow_mut().extend(failed_reads);
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
                let warnings = self.with_stale_cache_record(warnings.into_inner(), &output);
                Ok(self.render_search(&output, warnings))
            }
        }
    }

    fn render_search(
        &self,
        output: &agent::search::AgentSearchOutput,
        warnings: Vec<AgentWarning>,
    ) -> CommandOutput {
        let records = self.perf.time(Stage::Render, || {
            agent::search::format_agent_output_with_warnings(
                output,
                &warnings,
                self.clock().local_wall(),
            )
        });
        CommandOutput { records, warnings }
    }

    /// Loads the history and the environment, applies `rules`, adds the
    /// warnings for the selection's skipped and malformed transcripts, and
    /// adds its stale sessions to the command's.
    fn select_sessions(&self, rules: &SelectionRules) -> Result<SessionSelection> {
        let perf = &self.perf;
        let operation = command_operation(self.environment.cache_base(), perf.load_meter());
        let loaded = LoadedSessions::load(rules, perf, &self.environment, &operation)?;
        let (mut selection, stale_coverage) = SessionSelection::from_loaded(loaded, rules, perf)?;
        self.cover_stale_sessions(stale_coverage);
        let skipped = perf.time(Stage::SkippedTranscripts, || {
            warnings_for_skipped_transcripts(
                self,
                &operation,
                &selection.conversations,
                &selection.keys,
                &selection.stale_paths,
                &selection.unlisted_warnings,
            )
        });
        operation.finish().fail_if_low_space()?;
        selection.warnings.extend(skipped);
        Ok(selection)
    }

    fn run_sessions(&self, args: &cli::AgentSessionsArgs) -> Result<CommandOutput> {
        let agent_config = config::load_config()?.agent.unwrap_or_default();
        let rules = SelectionRules::new(
            args.time.resolve(self.clock().as_ref())?,
            &args.scope,
            &agent_config,
        );
        let mut selection = self.select_sessions(&rules)?;
        self.perf.time(Stage::Render, || {
            let listing = selection.listing(
                &rules.window,
                args.offset,
                args.limit.unwrap_or(DEFAULT_SESSIONS_LIMIT),
            )?;
            let warnings =
                self.with_stale_cache_record(std::mem::take(&mut selection.warnings), &listing);
            let records = agent::sessions::format_sessions_with_warnings(
                &listing,
                configured_budget(args.no_budget, args.budget, agent_config.output_chars),
                &warnings,
                self.clock().local_wall(),
            );
            Ok(CommandOutput { records, warnings })
        })
    }

    fn run_within(&self, args: &cli::AgentWithinArgs) -> Result<CommandOutput> {
        let config = config::load_config()?;
        let search_config = config.search.unwrap_or_default();
        let agent_config = config.agent.unwrap_or_default();
        let window = args.time.resolve(self.clock().as_ref())?;
        let perf = &self.perf;
        let operation = command_operation(self.environment.cache_base(), perf.load_meter());
        let DiscoveredKeys {
            keys,
            mut warnings,
            reads,
        } = self.discover_keys(&operation)?;
        let resolved = resolve_agent_conversation_arg(
            &args.conversation,
            &keys,
            self.environment.shared_canonicaliser(),
        )
        .map_err(|error| failure_after_warnings(error, &warnings))?;
        let (operation, targets) =
            self.hold_cache_for_reading(operation, &[&resolved.key], &keys, &window, reads)?;
        let key = targets.of_target(&resolved.key);
        let transcript = perf
            .time(Stage::Transcript, || {
                let transcript = self.load_transcript(key)?;
                Ok(match targets.root_read_as_is(key) {
                    Some(root) => transcript
                        .titled_with_external_title(self.read_external_title(key, root).as_ref()),
                    None => transcript,
                })
            })
            .map_err(|error| target_error(error, &resolved))?;
        warnings.extend(transcript_warning(
            &transcript,
            &resolved.reference.canonical(),
        ));
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
        let warnings = self.with_stale_cache_record(warnings, &CommandTargets);
        let records = perf.time(Stage::Render, || {
            agent::search::attach_transcript_metadata(&mut output, &resolved, &transcript);
            apply_configured_render_policy(&mut output, &agent_config);
            agent::search::format_agent_output_with_warnings(
                &output,
                &warnings,
                self.clock().local_wall(),
            )
        });
        operation.finish().fail_if_low_space()?;
        Ok(CommandOutput { records, warnings })
    }
}

/// Keys for every provider's sessions, sorted by path.
///
/// A sub-agent transcript doesn't get a key: discovery names it on its session's
/// stub, its content is reachable through that session, and a key of its own
/// would resolve to a row that does not exist.
///
/// A provider whose sessions cannot be listed, or whose keys the session
/// cache fails to give, does not contribute any keys or roots, and gets a
/// warning naming that step: the other providers' sessions stay readable.
/// Each directory discovery could not list gets a warning, and the cached
/// sessions under it keep their keys.
///
/// `meter` records what each provider's discovery and cache read. Under
/// `operation`, the cache is read, and a root without a session in the
/// session cache is cached.
fn discover_agent_keys(
    environment: &history::Environment,
    meter: &LoadMeter,
    operation: &history::OperationScope,
) -> Result<DiscoveredKeys> {
    let mut keys = Vec::new();
    let mut warnings = Vec::new();
    let mut reads = KeyedReads::default();
    let project_directories = environment.canonical_directories();
    for &storage in environment.storages() {
        let provider = storage.source().provider();
        let found = match provider_keys(
            environment,
            storage,
            meter.agent(storage.source()),
            operation,
            &project_directories,
        ) {
            Ok(found) => found,
            Err(failure) => {
                warnings.push(failure.warning(provider));
                continue;
            }
        };
        keys.extend(found.keys);
        warnings.extend(
            found
                .unreadable_directories
                .iter()
                .map(|directory| unreadable_directory_warning(provider, directory)),
        );
        reads.roots.insert(storage.source(), found.roots);
        reads.sessions.extend(found.sessions);
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
    Ok(DiscoveredKeys {
        keys,
        warnings,
        reads,
    })
}

struct DiscoveredKeys {
    keys: Vec<agent::refs::AgentConversationKey>,
    /// A warning for each agent whose keys could not be discovered and for
    /// each directory discovery could not list, in agent order.
    warnings: Vec<AgentWarning>,
    reads: KeyedReads,
}

/// The step of an agent's key discovery that failed.
#[derive(Debug)]
enum KeyDiscoveryFailure {
    /// Listing the agent's sessions.
    Listing(AppError),
    /// Reading or writing the session cache for the records the keys need.
    SessionCache(AppError),
}

impl KeyDiscoveryFailure {
    /// The warning an agent command prints for `provider`'s failure.
    fn warning(&self, provider: &dyn history::provider::SessionProvider) -> AgentWarning {
        match self {
            Self::Listing(error) => listing_failure_warning(provider, error),
            Self::SessionCache(error) => AgentWarning {
                kind: AgentWarningKind::Io,
                reference: None,
                detail: format!(
                    "failed to read or write the session cache for {} sessions: {error}",
                    provider.labels().display
                ),
            },
        }
    }
}

/// The roots key discovery canonicalised and the sessions it read. The
/// command's refresh reads below these roots, and holds a session read here
/// without reading it again.
#[derive(Default)]
struct KeyedReads {
    /// Each listed agent's roots, canonicalised as the keys canonicalised
    /// them.
    roots: HashMap<history::Source, Vec<history::provider::SessionRoot>>,
    /// The reads of the sessions key discovery cached to key them when the
    /// session cache didn't hold a session of their root, by path: written
    /// or not.
    sessions: HashMap<PathBuf, TargetRead>,
}

/// [`discover_agent_keys`] from the sessions a load discovered and the
/// directory and session ID it recorded of each, so a search after a load
/// neither discovers the sessions nor reads the cache again. With a
/// `workspace`, only
/// the sessions each agent's own rule places in it, by the directory the load
/// recorded, as [`history::Workspace::contains`] places their rows. The
/// warnings name each agent in `failures`, then each directory in
/// `unreadable_directories`.
fn loaded_agent_keys(
    discovered: &[(history::Source, Vec<history::provider::DiscoveredSession>)],
    failures: &[(history::Source, AppError)],
    unreadable_directories: &[(history::Source, history::provider::UnreadableDirectory)],
    workspace: Option<&history::Workspace>,
    project_directories: &CanonicalDirectories,
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
                        project_directories,
                        session.address.clone(),
                    )
                })
        })
        .collect::<Vec<_>>();
    keys.sort_by(|left, right| left.path.cmp(&right.path));
    let warnings =
        failures
            .iter()
            .map(|(source, error)| listing_failure_warning(source.provider(), error))
            .chain(unreadable_directories.iter().map(|(source, directory)| {
                unreadable_directory_warning(source.provider(), directory)
            }))
            .collect();
    (keys, warnings)
}

fn any_history_root_exists(environment: &history::Environment) -> bool {
    let root_environment = environment.root_environment();
    environment.storages().iter().any(|storage| {
        storage
            .roots_in(&root_environment)
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

fn unreadable_directory_warning(
    provider: &dyn history::provider::SessionProvider,
    directory: &history::provider::UnreadableDirectory,
) -> AgentWarning {
    AgentWarning {
        kind: AgentWarningKind::Io,
        reference: None,
        detail: format!(
            "failed to list {} {} in {}: {}",
            provider.labels().display,
            directory.listed_for.noun(),
            directory.path.display(),
            directory.error
        ),
    }
}

struct ProviderKeys {
    keys: Vec<agent::refs::AgentConversationKey>,
    unreadable_directories: Vec<history::provider::UnreadableDirectory>,
    /// The agent's roots, canonicalised as the keys canonicalised them.
    roots: Vec<history::provider::SessionRoot>,
    /// The reads of the sessions cached to key them, by path.
    sessions: Vec<(PathBuf, TargetRead)>,
}

/// Every key under every root `storage` stores sessions in, the cache read
/// under `operation`, and the directories discovery could not list.
fn provider_keys(
    environment: &history::Environment,
    storage: &dyn history::provider::SessionStorage,
    meter: AgentMeter,
    operation: &history::OperationScope,
    project_directories: &CanonicalDirectories,
) -> std::result::Result<ProviderKeys, KeyDiscoveryFailure> {
    let mut found = ProviderKeys {
        keys: Vec::new(),
        unreadable_directories: Vec::new(),
        roots: storage
            .roots_in(&environment.root_environment())
            .map_err(KeyDiscoveryFailure::Listing)?,
        sessions: Vec::new(),
    };
    for root in &found.roots {
        let RootKeys {
            keys,
            unreadable_directories,
            sessions,
        } = root_keys(&KeyedRoot {
            environment,
            storage,
            root,
            meter,
            operation,
            project_directories,
        })?;
        found.keys.extend(keys);
        found.unreadable_directories.extend(unreadable_directories);
        found.sessions.extend(sessions);
    }
    Ok(found)
}

struct RootKeys {
    keys: Vec<agent::refs::AgentConversationKey>,
    unreadable_directories: Vec<history::provider::UnreadableDirectory>,
    /// The reads of the sessions cached to key them, by path.
    sessions: Vec<(PathBuf, TargetRead)>,
}

/// One root whose sessions a command keys: the storage that lists them, the
/// cache read under `operation`, `meter`, and the directories the command's
/// keys canonicalise, each once.
#[derive(Clone, Copy)]
struct KeyedRoot<'a> {
    environment: &'a history::Environment,
    storage: &'a dyn history::provider::SessionStorage,
    root: &'a history::provider::SessionRoot,
    meter: AgentMeter<'a>,
    operation: &'a history::OperationScope,
    project_directories: &'a CanonicalDirectories,
}

/// Keys for the sessions a fresh discovery lists under `root`, not for every
/// cached session: a cached session can outlive its transcripts. The cached
/// sessions under a directory discovery could not list keep their keys, from
/// their copies; those directories are returned beside the keys.
///
/// A session whose path names its key is keyed without being read. Every
/// other session reads the directory and session ID it recorded from the
/// session cache, read under `operation`, so a corpus that can run to
/// gigabytes is not parsed again. The cache is read only for such a session, or for a
/// directory discovery could not list.
fn root_keys(keyed: &KeyedRoot<'_>) -> std::result::Result<RootKeys, KeyDiscoveryFailure> {
    let KeyedRoot {
        environment,
        storage,
        root,
        meter,
        project_directories,
        ..
    } = *keyed;
    let provider = storage.source().provider();
    let clock = environment.clock().as_ref();
    meter.discovering();
    let discovering = clock.now();
    let history::provider::DiscoveredSessions {
        stubs,
        unreadable_directories,
        ..
    } = storage
        .discover(root, environment.lister())
        .map_err(KeyDiscoveryFailure::Listing)?;
    meter.discovered(1, stubs.len(), clock.elapsed_since(discovering));
    let roots = history::sqlite_cache::SessionRoots::of(root, environment.canonicaliser());
    let unkeyed: Vec<&history::provider::SessionStub> = stubs
        .iter()
        .filter(|stub| !agent::refs::is_keyed_by_path(provider, &stub.locator))
        .collect();
    let RecordedSessions { recorded, sessions } = if unkeyed.is_empty() {
        RecordedSessions::default()
    } else {
        recorded_sessions(keyed, &unkeyed).map_err(KeyDiscoveryFailure::SessionCache)?
    };
    let mut keys: Vec<agent::refs::AgentConversationKey> = stubs
        .iter()
        .filter_map(|stub| {
            let recorded = if agent::refs::is_keyed_by_path(provider, &stub.locator) {
                None
            } else {
                recorded.get(&stub.cache_key).cloned().flatten()
            };
            agent::refs::session_key(
                provider,
                stub,
                recorded.as_ref(),
                project_directories,
                address_in(storage, &roots, &stub.cache_key),
            )
        })
        .collect();
    // Cached sessions lie under a directory of sessions alone: a sub-agent
    // directory that cannot be listed hides no session.
    let directories_of_sessions: Vec<_> = unreadable_directories
        .iter()
        .filter(|directory| directory.listed_for == history::provider::ListedFor::Sessions)
        .cloned()
        .collect();
    if !directories_of_sessions.is_empty() {
        keys.extend(
            cached_keys_under(keyed, &roots, &stubs, &directories_of_sessions)
                .map_err(KeyDiscoveryFailure::SessionCache)?,
        );
    }
    Ok(RootKeys {
        keys,
        unreadable_directories,
        sessions,
    })
}

/// Keys for the listed sessions the session cache holds under `directories`,
/// which discovery under `root` could not list, other than the sessions
/// `stubs` names, from what each session recorded of itself, as a load keys
/// them, each kept at the address `roots` gives it. Waits through another
/// rearview's rebuild, as `operation` reads. `meter` records the read.
fn cached_keys_under(
    keyed: &KeyedRoot<'_>,
    roots: &history::sqlite_cache::SessionRoots,
    stubs: &[history::provider::SessionStub],
    directories: &[history::provider::UnreadableDirectory],
) -> Result<Vec<agent::refs::AgentConversationKey>> {
    let KeyedRoot {
        environment,
        storage,
        root,
        meter,
        operation,
        project_directories,
    } = *keyed;
    let provider = storage.source().provider();
    let directories: Vec<&Path> = directories
        .iter()
        .map(|directory| directory.path.as_path())
        .collect();
    meter.reading_cache();
    let reading = environment.clock().now();
    let listed =
        operation.read_when_unlocked(|| match history::sqlite_cache::SqliteCache::open_waiting(
            environment.cache_base(),
        )? {
            Some(cache) if cache.storage_is_current(storage.cache())? => {
                cache.listed_sessions_under(storage.cache(), root, &directories)
            }
            _ => Ok(Vec::new()),
        })?;
    meter.cache_read(listed.len(), environment.clock().elapsed_since(reading));
    let discovered: HashSet<&str> = stubs.iter().map(|stub| stub.cache_key.as_str()).collect();
    Ok(listed
        .into_iter()
        .filter(|session| !discovered.contains(session.cache_key.as_str()))
        .filter_map(|session| {
            let stub = history::provider::SessionStub {
                locator: session.locator,
                subagents: session.subagents,
                cache_key: session.cache_key,
                fingerprint: history::provider::Fingerprint::UNKNOWN,
            };
            agent::refs::loaded_session_key(
                provider,
                &stub,
                Some(&session.recorded),
                project_directories,
                address_in(storage, roots, &stub.cache_key),
            )
        })
        .collect())
}

fn address_in(
    storage: &dyn history::provider::SessionStorage,
    roots: &history::sqlite_cache::SessionRoots,
    cache_key: &str,
) -> history::sqlite_cache::SessionAddress {
    history::sqlite_cache::SessionAddress::new(
        storage.cache(),
        storage.source(),
        roots.clone(),
        cache_key.to_owned(),
    )
}

#[derive(Default)]
struct RecordedSessions {
    /// The directory and session ID each session recorded, by cache key;
    /// `None` for a session the cache records as empty or as unreadable.
    recorded: HashMap<String, Option<history::provider::RecordedSession>>,
    /// The reads of the sessions cached to find their records, by path.
    sessions: Vec<(PathBuf, TargetRead)>,
}

/// The directory and session ID each session `keyed`'s storage keeps under
/// its root recorded, from the session cache. When the session cache
/// doesn't hold a session of the root, `keyed`'s refresh owner first caches the sessions `unkeyed`
/// names under that root, and their reads give the records, whether or not
/// their writes landed. While another rearview keeps the cache, those
/// sessions stay without a record.
fn recorded_sessions(
    keyed: &KeyedRoot<'_>,
    unkeyed: &[&history::provider::SessionStub],
) -> Result<RecordedSessions> {
    let KeyedRoot {
        environment,
        storage,
        root,
        meter,
        operation,
        ..
    } = *keyed;
    let recorded = cached_recorded_sessions(environment, storage, root, meter, operation)?;
    if !recorded.is_empty()
        || matches!(
            operation.acquire_access()?,
            history::sqlite_cache::OperationAccess::AsIs
        )
    {
        return Ok(RecordedSessions {
            recorded,
            sessions: Vec::new(),
        });
    }
    let paths: Vec<&Path> = unkeyed.iter().map(|stub| stub.locator.as_path()).collect();
    let read = history::provider::cache_sessions_in_roots(
        environment,
        storage.source(),
        vec![root.forgetting_failed_canonicalisations()],
        &paths,
        false,
        operation,
        meter,
    )?;
    let mut recorded = HashMap::new();
    let mut sessions = Vec::with_capacity(read.len());
    for (stub, outcome) in unkeyed.iter().zip(read) {
        let record = outcome
            .listed_row()
            .map(history::provider::RecordedSession::of);
        recorded.insert(stub.cache_key.clone(), record);
        sessions.push((stub.locator.clone(), TargetRead::of(outcome)));
    }
    Ok(RecordedSessions { recorded, sessions })
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
                cache.recorded_sessions(storage.cache(), root)
            }
            _ => Ok(HashMap::new()),
        })?;
    meter.cache_read(recorded.len(), environment.clock().elapsed_since(reading));
    Ok(recorded)
}

/// The sessions a command read from the cache as it was, for `cause`, whose
/// newest messages its output may lack: the content of its one
/// `stale-cache` record.
struct StaleCoverage {
    cause: history::StaleCause,
    /// Each session's reference, and its copy's last message time: `None`
    /// when the session is uncached or its messages are undated.
    sessions: Vec<(String, Option<chrono::DateTime<chrono::Local>>)>,
}

impl StaleCoverage {
    fn new(cause: history::StaleCause) -> Self {
        Self {
            cause,
            sessions: Vec::new(),
        }
    }

    /// Adds each of `keys` that `stale_copies` names by path, unless
    /// `window` ends at or before its copy's last message. Resolves each
    /// reference against `every_key`, the keys before any filter, as `read`
    /// resolves it.
    fn add_stale_keys<'k>(
        &mut self,
        keys: impl IntoIterator<Item = &'k agent::refs::AgentConversationKey>,
        every_key: &[agent::refs::AgentConversationKey],
        stale_copies: &HashMap<&Path, history::provider::StaleCopy>,
        window: &TimeFilter,
    ) {
        for key in keys {
            let Some(stale) = stale_copies.get(key.path.as_path()) else {
                continue;
            };
            if stale.cached_until.is_some_and(|cached_until| {
                window.before.is_some_and(|before| before <= cached_until)
            }) {
                continue;
            }
            let reference = agent::refs::resolved_conversation_for_key(every_key, key)
                .reference
                .canonical();
            self.add(reference, stale.cached_until);
        }
    }

    /// Adds `other`'s sessions; the cause stays this coverage's.
    fn join(&mut self, other: StaleCoverage) {
        for (reference, cached_until) in other.sessions {
            self.add(reference, cached_until);
        }
    }

    /// Adds the session `reference` names, once.
    fn add(&mut self, reference: String, cached_until: Option<chrono::DateTime<chrono::Local>>) {
        if !self.sessions.iter().any(|(added, _)| *added == reference) {
            self.sessions.push((reference, cached_until));
        }
    }

    /// The `stale-cache` record for `output`, `None` without a session. It
    /// names the sessions `output` shows, and the sessions it leaves out
    /// while there are at most [`LEFT_OUT_SESSIONS_NAMED_UP_TO`]; beyond
    /// that, `omitted-sessions=` counts the ones it leaves out.
    /// `cached-until` is the earliest end, in `zone`, of the named sessions'
    /// copies.
    fn record(&self, zone: Zone, output: &dyn ShowsSessions) -> Option<AgentWarning> {
        let shown: Vec<bool> = self
            .sessions
            .iter()
            .map(|(reference, _)| output.shows_session(reference))
            .collect();
        let left_out = shown.iter().filter(|is_shown| !**is_shown).count();
        let names_left_out = left_out <= LEFT_OUT_SESSIONS_NAMED_UP_TO;
        let named: Vec<_> = self
            .sessions
            .iter()
            .zip(&shown)
            .filter(|(_, is_shown)| names_left_out || **is_shown)
            .map(|(session, _)| session)
            .collect();
        let references: Vec<String> = named
            .iter()
            .map(|(reference, _)| reference.clone())
            .collect();
        let earliest_end = named
            .iter()
            .filter_map(|(_, cached_until)| *cached_until)
            .min();
        AgentWarning::stale_cache(
            &references,
            earliest_end.map(|end| end.with_timezone(&zone)),
            if names_left_out { 0 } else { left_out },
            self.cause,
        )
    }
}

/// A `stale-cache` record names at most this many stale sessions the output
/// leaves out; more refs would use the output's budget. `agent read <ref>`
/// of a named one waits for the other refresh when the session is uncached,
/// and reads a cached copy that is out of date at once, with its own
/// `stale-cache` record.
const LEFT_OUT_SESSIONS_NAMED_UP_TO: usize = 8;

trait ShowsSessions {
    fn shows_session(&self, reference: &str) -> bool;
}

impl ShowsSessions for agent::sessions::SessionListing {
    fn shows_session(&self, reference: &str) -> bool {
        self.sessions
            .iter()
            .any(|session| session.reference == reference)
    }
}

impl ShowsSessions for agent::search::AgentSearchOutput {
    fn shows_session(&self, reference: &str) -> bool {
        self.every_hit()
            .any(|hit| hit.conversation_ref == reference)
    }
}

/// The sessions `read`, `outline` and `within` show: every one they read,
/// each a target the command was given.
struct CommandTargets;

impl ShowsSessions for CommandTargets {
    fn shows_session(&self, _reference: &str) -> bool {
        true
    }
}

/// Warnings for `keys` without a row, and for rows with malformed lines. A
/// key in `stale_paths` doesn't get one: the stale-cache record reports it. A key
/// without a row gets its warning from `unlisted_warnings`, by transcript
/// path; a key missing there joins the command's stale sessions, with
/// `operation`'s cause.
fn warnings_for_skipped_transcripts(
    service: &AgentService,
    operation: &history::OperationScope,
    conversations: &[history::Conversation],
    keys: &[agent::refs::AgentConversationKey],
    stale_paths: &HashSet<PathBuf>,
    unlisted_warnings: &HashMap<PathBuf, AgentWarning>,
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
    let project_directories = service.environment.canonical_directories();
    for conversation in conversations {
        if key_paths.contains(conversation.path.as_path()) && !conversation.parse_errors.is_empty()
        {
            let key = agent::refs::AgentConversationKey::from_conversation(
                conversation,
                &project_directories,
            )
            .ok();
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
    let mut not_cached = StaleCoverage::new(operation.stale_cause());
    for key in keys {
        if known.contains(key.path.as_path()) || stale_paths.contains(&key.path) {
            continue;
        }
        let reference = agent::refs::resolved_conversation_for_key(keys, key)
            .reference
            .canonical();
        match unlisted_warnings.get(&key.path) {
            Some(warning) => warnings.push(AgentWarning {
                reference: Some(reference),
                ..warning.clone()
            }),
            None => not_cached.add(reference, None),
        }
    }
    service.cover_stale_sessions(not_cached);
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
        external_title: None,
        model: None,
        total_tokens: 0,
        duration_minutes: None,
        fingerprint: None,
        has_transient_read_error: false,
        is_missing_earlier_turns: false,
        cached_session: None,
        cache_address: None,
        publication: crate::history::sqlite_cache::Publication::Unpublished {
            discovered: Vec::new(),
        },
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
        // Formatted once `execute` has run its low-space deletion.
        error if WriteErrorClass::of(&error) == WriteErrorClass::LowSpace => error,
        error => structured_agent_error(error),
    }
}

/// The records a command prints on success, and the command's warnings, for
/// its failure when the budget cut the records short.
#[derive(Debug)]
pub(crate) struct CommandOutput {
    records: String,
    warnings: Vec<AgentWarning>,
}

impl CommandOutput {
    /// The records; or, when the budget cut them short, the
    /// `budget-too-small` error printed after the warning records.
    fn records_if_complete(self) -> Result<String> {
        if self.records.ends_with('\n') {
            return Ok(self.records);
        }
        let error = AgentError::new(
            AgentErrorKind::BudgetTooSmall,
            None,
            "output budget cannot fit a complete compact record; increase --budget",
        );
        Err(failure_after_warnings(error.into(), &self.warnings))
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

/// `error` printed after the records of the command's `warnings`, as a
/// successful output prints them. A reference not found follows the
/// listing failure that explains it.
fn failure_after_warnings(error: AppError, warnings: &[AgentWarning]) -> AppError {
    if warnings.is_empty() {
        return error;
    }
    match structured_agent_error(error) {
        AppError::Agent(error) => {
            let (_, warning_records) = agent::diagnostic::format_warning_records(warnings);
            let mut output = warning_records.concat();
            output.push_str(&agent::diagnostic::format_error(&error));
            AppError::AgentProtocol(output)
        }
        error => error,
    }
}

fn structured_agent_error(error: AppError) -> AppError {
    match error {
        AppError::Agent(_) | AppError::AgentProtocol(_) => error,
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
        error
            if WriteErrorClass::of(&error) == WriteErrorClass::LowSpace
                || matches!(error, AppError::NoSessionCacheDirectory) =>
        {
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
    let (semantic, mut warnings) = run_agent_semantic_hits(
        service,
        &request.query,
        &inputs,
        embedding_budget,
        request.semantic_chunk_hits(),
        request.window,
    )?;
    let (output, failed_reads) = service.fuse_semantic_hits(
        semantic,
        &HashMap::new(),
        &inputs,
        keys,
        |semantic, inputs| agent::search::run_global_semantic_search(request, inputs, semantic),
    );
    warnings.extend(failed_reads);
    Ok((output, warnings))
}

/// A semantic search's ranked hits and the copies it read them from.
struct SemanticHalf {
    ranked: RankedHits,
    reads: SemanticReads,
}

/// The copies the semantic stage read its sessions from.
#[derive(Default)]
struct SemanticReads {
    /// The generation of the cached copy whose messages each session's hits
    /// number, by conversation reference. A session read without a copy is
    /// missing.
    copies: HashMap<String, i64>,
    /// The row of each session whose copy another write replaced before the
    /// semantic stage read it, by input index, its messages counted and
    /// timed as the copy read holds them.
    replaced_rows: HashMap<usize, history::Conversation>,
    /// The sessions no longer in the session cache, or whose cached copy was
    /// not ready, when the semantic stage read them, without semantic hits.
    no_longer_cached: Vec<agent::refs::AgentConversationKey>,
    /// One `io` warning per session whose read failed, without semantic
    /// hits.
    failed: Vec<AgentWarning>,
}

impl AgentService {
    /// `fuse`'s output over `semantic`'s hits and `inputs`, each semantic-only
    /// hit given the anchors and times of its focus messages in the copy its
    /// session's semantic hits number. A session's semantic hits are dropped,
    /// and the session added to the command's stale sessions, when the
    /// lexical hits number another copy of it (`lexical_copies`), or when
    /// that copy is no longer in the session cache as the anchors are read.
    /// The sessions no longer in the session cache at the semantic stage are
    /// added too. Returns a warning for each session whose read at the
    /// semantic stage or whose anchor read failed; the hits of the latter
    /// don't keep any anchors. The anchor reads, not the fusion, are timed as
    /// the `hit-metadata` stage.
    fn fuse_semantic_hits(
        &self,
        semantic: SemanticHalf,
        lexical_copies: &HashMap<String, i64>,
        inputs: &[agent::search::AgentConversationInput<'_>],
        keys: &[agent::refs::AgentConversationKey],
        fuse: impl Fn(
            &RankedHits,
            &[agent::search::AgentConversationInput<'_>],
        ) -> agent::search::AgentSearchOutput,
    ) -> (agent::search::AgentSearchOutput, Vec<AgentWarning>) {
        let SemanticHalf {
            mut ranked,
            reads:
                SemanticReads {
                    copies,
                    replaced_rows,
                    no_longer_cached,
                    failed: mut failed_reads,
                },
        } = semantic;
        let inputs: Vec<agent::search::AgentConversationInput<'_>> = inputs
            .iter()
            .map(|input| agent::search::AgentConversationInput {
                conversation: replaced_rows
                    .get(&input.original_index)
                    .unwrap_or(input.conversation),
                resolved: input.resolved.clone(),
                original_index: input.original_index,
            })
            .collect();
        let mut changed: Vec<&agent::search::AgentConversationInput<'_>> = inputs
            .iter()
            .filter(|input| {
                let reference = input.resolved.reference.canonical();
                ranked.holds_conversation(input.original_index)
                    && matches!(
                        (copies.get(&reference), lexical_copies.get(&reference)),
                        (Some(semantic), Some(lexical)) if semantic != lexical
                    )
            })
            .collect();
        let mut anchor_read = std::time::Duration::ZERO;
        let output = loop {
            ranked.drop_conversations(&changed.iter().map(|input| input.original_index).collect());
            let mut output = fuse(&ranked, &inputs);
            let started = self.clock().now();
            let reads = attach_semantic_hit_anchors(self, &mut output, &inputs, &copies);
            anchor_read += self.clock().elapsed_since(started);
            if reads.no_longer_cached.is_empty() {
                failed_reads.extend(reads.failed);
                break output;
            }
            // Fuse again without them, so the output fills their places from
            // the other sessions' hits.
            changed.extend(reads.no_longer_cached);
        };
        self.perf.record(Stage::HitMetadata, anchor_read);
        self.cover_sessions_changed_while_read(
            no_longer_cached
                .iter()
                .chain(changed.iter().map(|input| &input.resolved.key)),
            keys,
        );
        (output, failed_reads)
    }
}

/// The sessions [`attach_semantic_hit_anchors`] found without anchors.
struct AnchorReads<'i, 'c> {
    /// The sessions whose copy is no longer in the session cache, in
    /// `inputs`' order.
    no_longer_cached: Vec<&'i agent::search::AgentConversationInput<'c>>,
    /// One warning per session whose read failed.
    failed: Vec<AgentWarning>,
}

/// Gives each semantic-only hit in `output` the anchors and times of its
/// focus messages, read from the copy `copies` names for its session. A read
/// that fails leaves its hits without anchors.
fn attach_semantic_hit_anchors<'i, 'c>(
    service: &AgentService,
    output: &mut agent::search::AgentSearchOutput,
    inputs: &'i [agent::search::AgentConversationInput<'c>],
    copies: &HashMap<String, i64>,
) -> AnchorReads<'i, 'c> {
    let mut focused: HashMap<String, std::collections::BTreeSet<usize>> = HashMap::new();
    for hit in output
        .every_hit()
        .filter(|hit| hit.source == agent::search::AgentHitKind::Semantic)
    {
        focused
            .entry(hit.conversation_ref.clone())
            .or_default()
            .extend(hit.focus_range.start..=hit.focus_range.end);
    }
    let mut reads = AnchorReads {
        no_longer_cached: Vec::new(),
        failed: Vec::new(),
    };
    for input in inputs {
        let reference = input.resolved.reference.canonical();
        let Some(ordinals) = focused.get(&reference) else {
            continue;
        };
        let ordinals = ordinals.iter().copied().collect::<Vec<_>>();
        match read_semantic_messages(service, input, copies.get(&reference).copied(), &ordinals) {
            Ok(transcript) => {
                agent::search::attach_semantic_hit_metadata(output, &input.resolved, &transcript);
            }
            Err(UnreadMessages::NoLongerCached) => reads.no_longer_cached.push(input),
            Err(UnreadMessages::Failed(error)) => reads
                .failed
                .push(AgentWarning::from_app_error(&error, Some(&reference))),
        }
    }
    reads
}

/// A read of a semantic hit's messages that didn't find any to attach.
enum UnreadMessages {
    /// The copy the hits number is no longer in the session cache: another
    /// write removed or replaced it, or is writing it.
    NoLongerCached,
    Failed(AppError),
}

/// The messages numbered `ordinals` of `input`'s session, read alone from its
/// copy of `generation`; the whole transcript for a row without a copy.
fn read_semantic_messages(
    service: &AgentService,
    input: &agent::search::AgentConversationInput<'_>,
    generation: Option<i64>,
    ordinals: &[usize],
) -> std::result::Result<agent::transcript::AgentTranscript, UnreadMessages> {
    let key = &input.resolved.key;
    let (Some(copy), Some(generation)) = (input.conversation.cached_session.clone(), generation)
    else {
        return service.load_transcript(key).map_err(UnreadMessages::Failed);
    };
    service.perf.time_transcript_parse(key, || {
        let read = service.read_copy_when_unlocked(|| {
            copy.read_copy(|held| {
                if held.generation() != generation {
                    return Ok(None);
                }
                held.agent_messages(&key.path, ordinals).map(Some)
            })
        });
        history::sqlite_cache::found(read)
            .map_err(UnreadMessages::Failed)?
            .flatten()
            .map(|read| read.transcript)
            .ok_or(UnreadMessages::NoLongerCached)
    })
}

/// The lexical pass a hybrid search runs after its semantic half.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HybridLexicalPass {
    /// Deep, to fuse with the semantic hits.
    ForFusion,
    /// The user's own request, after the semantic half failed.
    Alone,
}

/// Runs a hybrid search's semantic half first, because it doesn't read
/// transcripts, then one lexical pass, so each transcript is read at most
/// once. The semantic half's error is returned with the lexical result, not
/// raised, except a cache write's finding the drive low on space: that stops
/// the search, as it stops every command.
fn run_hybrid_halves<S, L>(
    semantic: impl FnOnce() -> Result<S>,
    lexical: impl FnOnce(HybridLexicalPass) -> Result<L>,
) -> Result<(Result<S>, L)> {
    let semantic = match semantic() {
        Err(error) if WriteErrorClass::of(&error) == WriteErrorClass::LowSpace => {
            return Err(error);
        }
        semantic => semantic,
    };
    let pass = if semantic.is_ok() {
        HybridLexicalPass::ForFusion
    } else {
        HybridLexicalPass::Alone
    };
    Ok((semantic, lexical(pass)?))
}

/// The semantic hits of `inputs`' sessions for `query`, with the copies they
/// were read from. Writes the session cache: stores the route texts the read
/// computed ([`AgentService::store_semantic_routes`]) before ranking.
fn run_agent_semantic_hits(
    service: &AgentService,
    query: &str,
    inputs: &[agent::search::AgentConversationInput<'_>],
    embedding_budget: EmbeddingBudget,
    chunk_hits: ChunkHits,
    window: TimeFilter,
) -> Result<(SemanticHalf, Vec<AgentWarning>)> {
    let SemanticCandidates {
        candidates,
        reads,
        computed_routes,
    } = semantic_candidates(inputs)?;
    service.store_semantic_routes(&computed_routes)?;
    run_agent_semantic_hits_for_candidates(
        query,
        &candidates,
        embedding_budget,
        chunk_hits,
        window,
        service.clock().clone(),
    )
    .map(|(ranked, warnings)| (SemanticHalf { ranked, reads }, warnings))
}

impl AgentService {
    /// Stores `routes`, the route texts the semantic stage computed, when
    /// this command can take refresh ownership without waiting. Fails when
    /// the drive is low on space. Any other skipped or failed write leaves
    /// the output as it is: the next semantic search computes those routes
    /// again. `--perf-stats` reports what was stored, or why nothing was.
    fn store_semantic_routes(&self, routes: &[history::SemanticRoute]) -> Result<()> {
        if routes.is_empty() {
            return Ok(());
        }
        let skipped = match history::store_semantic_routes(self.environment.cache_base(), routes) {
            Ok(history::RoutesStored::Stored) => None,
            Ok(history::RoutesStored::NotOwner) => {
                Some(agent::perf::RoutesSkipped::AnotherRearview)
            }
            Err(error) if WriteErrorClass::of(&error) == WriteErrorClass::LowSpace => {
                return Err(error);
            }
            Err(error) => Some(agent::perf::RoutesSkipped::WriteFailed(error.to_string())),
        };
        self.perf.record_semantic_routes(routes.len(), skipped);
        Ok(())
    }
}

/// The semantic stage's candidates, and the copies it read them from.
struct SemanticCandidates {
    candidates: Vec<semantic::index::SemanticIndexCandidate>,
    reads: SemanticReads,
    /// The route texts computed from the copies' texts, to store with them.
    computed_routes: Vec<history::SemanticRoute>,
}

/// The semantic candidates of `inputs`' sessions, each read from the copy
/// the cache holds now, whichever generation it is. A missed read doesn't
/// give any candidates. The sessions are read on the rayon pool; a failed
/// read of every session fails the read.
fn semantic_candidates(
    inputs: &[agent::search::AgentConversationInput<'_>],
) -> Result<SemanticCandidates> {
    let sessions = history::sqlite_cache::unless_every_read_failed(
        inputs
            .par_iter()
            .map(session_candidates)
            .collect::<Vec<_>>(),
    )?;
    let mut candidates = Vec::with_capacity(inputs.len().saturating_mul(2));
    let mut reads = SemanticReads::default();
    let mut computed_routes = Vec::new();
    for (input, session) in inputs.iter().zip(sessions) {
        let session = match history::sqlite_cache::found(session) {
            Ok(Some(session)) => session,
            Ok(None) => {
                reads.no_longer_cached.push(input.resolved.key.clone());
                continue;
            }
            Err(error) => {
                let reference = input.resolved.reference.canonical();
                reads
                    .failed
                    .push(AgentWarning::from_app_error(&error, Some(&reference)));
                continue;
            }
        };
        if let Some(generation) = session.generation {
            reads
                .copies
                .insert(input.resolved.reference.canonical(), generation);
        }
        if let Some(row) = session.replaced_row {
            reads.replaced_rows.insert(input.original_index, row);
        }
        computed_routes.extend(session.computed_route);
        candidates.extend(session.candidates);
    }
    Ok(SemanticCandidates {
        candidates,
        reads,
        computed_routes,
    })
}

/// One session's semantic candidates, read by [`semantic_candidates`].
struct SessionCandidates {
    candidates: Vec<semantic::index::SemanticIndexCandidate>,
    /// The generation of the copy read; `None` for a row without a copy.
    generation: Option<i64>,
    /// The row, its messages counted and timed as the copy read holds them,
    /// when another write replaced the row's copy.
    replaced_row: Option<history::Conversation>,
    /// The route text computed from the copy's texts, to store with it.
    computed_route: Option<history::SemanticRoute>,
}

/// `input`'s semantic candidates from the copy the cache holds now.
fn session_candidates(
    input: &agent::search::AgentConversationInput<'_>,
) -> history::sqlite_cache::CopyRead<SessionCandidates> {
    let history::SemanticContent {
        held,
        route_text,
        computed_route,
    } = input.conversation.load_semantic_content()?;
    let conversation = held.row.as_ref();
    let row_copy = input.conversation.cached_session.as_ref();
    let replaced_row = held
        .generation
        .filter(|&generation| row_copy.is_some_and(|copy| copy.generation() != generation))
        .map(|_| history::Conversation {
            message_count: conversation.message_count,
            message_times: conversation.message_times.clone(),
            ..input.conversation.clone()
        });
    let mut candidates = vec![semantic::index::SemanticIndexCandidate {
        index: input.original_index,
        source: semantic::types::SemanticChunkSource::VisibleDialogue,
        conversation: Arc::new(stripped_semantic_conversation(
            conversation,
            input.conversation.path.clone(),
            conversation.semantic_turns.clone(),
            conversation.semantic_turn_ranges.clone(),
        )),
    }];
    if let Some(route) = route_text.and_then(|text| agent_route_of(conversation, text)) {
        candidates.push(semantic::index::SemanticIndexCandidate {
            index: input.original_index,
            source: semantic::types::SemanticChunkSource::AgentRoute,
            conversation: Arc::new(route),
        });
    }
    Ok(SessionCandidates {
        candidates,
        generation: held.generation,
        replaced_row,
        computed_route,
    })
}

/// The agent route's candidate conversation of `conversation`, whose
/// [`history::Conversation::semantic_route_text`] is `route_text`.
pub(crate) fn agent_route_of(
    conversation: &history::Conversation,
    mut route_text: String,
) -> Option<history::Conversation> {
    if route_text.is_empty() {
        route_text = conversation
            .custom_title
            .as_deref()
            .or(conversation.summary.as_deref())?
            .to_string();
    }
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
        external_title: None,
        model: None,
        total_tokens: 0,
        duration_minutes: None,
        fingerprint: None,
        has_transient_read_error: false,
        is_missing_earlier_turns: false,
        cached_session: None,
        cache_address: None,
        publication: crate::history::sqlite_cache::Publication::Unpublished {
            discovered: Vec::new(),
        },
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
        conversation: Arc::new(conversation),
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
    ) -> Result<CommandOutput> {
        let perf = &self.perf;
        let operation = command_operation(self.environment.cache_base(), perf.load_meter());
        let discovered;
        let (keys, discovery_warnings, reads) = match keys {
            Some(keys) => (keys, Vec::new(), KeyedReads::default()),
            None => {
                discovered = self.discover_keys(&operation)?;
                (
                    discovered.keys.as_slice(),
                    discovered.warnings.clone(),
                    discovered.reads,
                )
            }
        };
        let agent_config = config::load_config()?.agent.unwrap_or_default();
        let options = agent_protocol_options(&args.output, &agent_config, self.clock().as_ref())?;
        let (mut resolved_refs, focus) =
            resolve_agent_read_args(args, keys, self.environment.shared_canonicaliser())
                .map_err(|error| failure_after_warnings(error, &discovery_warnings))?;
        let targets: Vec<&agent::refs::AgentConversationKey> = resolved_refs
            .iter()
            .map(|(_, resolved)| &resolved.key)
            .collect();
        let (operation, refreshed_targets) =
            self.hold_cache_for_reading(operation, &targets, keys, &options.window, reads)?;
        let transcripts = perf.time(Stage::Transcript, || {
            resolved_refs
                .iter()
                .map(|(read_ref, resolved)| {
                    let target = refreshed_targets.of_target(&resolved.key);
                    let range = read_ref.range.filter(|_| {
                        args.anchor.is_none()
                            && read_ref.subagent.is_none()
                            && read_ref.call.is_none()
                    });
                    self.load_transcript_for_range(target, range)
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
        let mut warnings = discovery_warnings;
        warnings.extend(resolved_refs.iter().zip(transcripts.iter()).filter_map(
            |((_, resolved), transcript)| {
                transcript_warning(transcript, &resolved.reference.canonical())
            },
        ));
        let warnings = self.with_stale_cache_record(warnings, &CommandTargets);
        let records = perf
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
        operation.finish().fail_if_low_space()?;
        Ok(CommandOutput { records, warnings })
    }

    pub(crate) fn run_outline(
        &self,
        args: &AgentOutlineArgs,
        keys: Option<&[agent::refs::AgentConversationKey]>,
    ) -> Result<CommandOutput> {
        let perf = &self.perf;
        let operation = command_operation(self.environment.cache_base(), perf.load_meter());
        let discovered;
        let (keys, discovery_warnings, reads) = match keys {
            Some(keys) => (keys, Vec::new(), KeyedReads::default()),
            None => {
                discovered = self.discover_keys(&operation)?;
                (
                    discovered.keys.as_slice(),
                    discovered.warnings.clone(),
                    discovered.reads,
                )
            }
        };
        let agent_config = config::load_config()?.agent.unwrap_or_default();
        let options = agent_protocol_options(&args.output, &agent_config, self.clock().as_ref())?;
        let outline_ref = agent::refs::parse_outline_ref(&args.conversation)?;
        let resolved = outline_ref
            .resolve_conversation(keys, self.environment.shared_canonicaliser())
            .map_err(|error| failure_after_warnings(error, &discovery_warnings))?;
        let (operation, targets) =
            self.hold_cache_for_reading(operation, &[&resolved.key], keys, &options.window, reads)?;
        let transcript = perf
            .time(Stage::Transcript, || {
                self.load_transcript_for_range(
                    targets.of_target(&resolved.key),
                    outline_ref.range.filter(|_| outline_ref.subagent.is_none()),
                )
            })
            .map_err(|error| target_error(error, &resolved))?;
        let mut warnings = discovery_warnings;
        warnings.extend(transcript_warning(
            &transcript,
            &resolved.reference.canonical(),
        ));
        let warnings = self.with_stale_cache_record(warnings, &CommandTargets);
        let request = agent::protocol::ReadRequest {
            resolved: &resolved,
            transcript: &transcript,
            range: outline_ref.range,
            subagent: outline_ref.subagent,
            call: None,
        };
        let records = perf
            .time(Stage::Render, || {
                agent::protocol::format_outline_with_warnings(
                    &request,
                    options,
                    &warnings,
                    self.clock().local_wall(),
                )
            })
            .map_err(|error| target_error(error, &resolved))?;
        operation.finish().fail_if_low_space()?;
        Ok(CommandOutput { records, warnings })
    }
}

pub(crate) fn resolve_agent_read_args(
    args: &AgentReadArgs,
    keys: &[agent::refs::AgentConversationKey],
    canonicaliser: &Arc<dyn Canonicaliser>,
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
                .resolve_conversation(keys, canonicaliser)
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
                .map(|conversation| {
                    agent::refs::resolve_conversation_ref(keys, conversation, canonicaliser)
                })
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

/// The sessions an agent command selected: their positions in the
/// conversations, and the scope the command selected them by.
#[derive(Clone, Copy)]
pub(crate) struct ScopedSessions<'s> {
    pub(crate) indices: &'s [usize],
    pub(crate) scope: agent::search::AgentSearchScope,
}

/// The sessions of `scoped` whose cached text matches `query`, best first,
/// freshness counted to `clock`'s current time; in exact mode, only those
/// whose cached text holds the whole query as one phrase. With `window`
/// active, each session is ranked on the text of its messages inside the
/// window. The ranking also holds each ranked session's hit messages, the
/// messages its hit read reads.
pub(crate) fn lexically_rank_scoped(
    conversations: &[history::Conversation],
    query: &str,
    scoped: ScopedSessions<'_>,
    mode: SearchMode,
    window: &TimeFilter,
    clock: &dyn Clock,
) -> Result<search::AgentRanking> {
    let texts = search::precompute_agent_search_text(conversations, window);
    let now = clock.wall().with_timezone(&chrono::Local);
    let scoped_set = scoped
        .indices
        .iter()
        .copied()
        .collect::<std::collections::HashSet<_>>();
    search::try_agent_search(
        conversations,
        &texts,
        query,
        retrieval_terms(query, mode),
        now,
        |index| scoped_set.contains(&index),
        match scoped.scope {
            agent::search::AgentSearchScope::Local => search::ScanScope::Project,
            agent::search::AgentSearchScope::Global => search::ScanScope::AllWorkspaces,
        },
    )
}

/// The terms `agent search`'s retrieval requires of a hit message for
/// `query` in `mode`: in exact mode the literals of the query as one phrase.
pub(crate) fn retrieval_terms(query: &str, mode: SearchMode) -> search::RetrievalTerms {
    match mode {
        SearchMode::Exact => search::RetrievalTerms::Phrase(
            search::query::ParsedQuery::parse(&agent::search::exact_phrase_query(query))
                .literals()
                .to_vec(),
        ),
        _ => search::RetrievalTerms::WordsAndLiterals,
    }
}

pub(crate) fn resolve_agent_conversation_arg(
    reference: &str,
    keys: &[agent::refs::AgentConversationKey],
    canonicaliser: &Arc<dyn Canonicaliser>,
) -> Result<agent::refs::ResolvedConversation> {
    agent::refs::resolve_conversation_ref(keys, reference, canonicaliser)
}

/// A service on test time that reads this test's session cache and doesn't
/// read any agent's storage.
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
    test_service()
        .run_read(args, keys)
        .map(|output| output.records)
}

#[cfg(test)]
pub(crate) fn run_agent_outline(
    args: &AgentOutlineArgs,
    keys: Option<&[agent::refs::AgentConversationKey]>,
) -> Result<String> {
    test_service()
        .run_outline(args, keys)
        .map(|output| output.records)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::test_support::{agent_command, assistant_jsonl_line, user_jsonl_line};
    use crate::cli::AgentOutputFlags;
    use crate::clock::TestClock;
    use crate::history::environment::test_support::{test_environment, waiting_operation};
    use crate::history::provider::walk::SystemCanonicaliser;
    use crate::history::sqlite_cache::connection::test_hooks::fail_the_first_data_commit;

    /// `storage`'s sessions loaded through the session cache under
    /// `cache_base` on test time, leaving `left_out`'s rows unread.
    fn load_through(
        storage: &dyn history::provider::SessionStorage,
        cache_base: &Path,
        left_out: history::provider::LeftOutRows,
    ) -> history::provider::LoadedSessions {
        let environment = test_environment(Vec::new(), Some(cache_base));
        let operation = waiting_operation(&environment);
        let options = history::provider::LoadOptions {
            left_out,
            ..history::provider::LoadOptions::default()
        };
        let loaded = history::provider::load_sessions(
            &environment,
            storage,
            options,
            &operation,
            &mut |_, _| {},
        )
        .unwrap();
        operation.finish().fail_if_low_space().unwrap();
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
    fn semantic_half_finding_drive_low_on_space_stops_search_before_lexical_pass() {
        let mut passes = Vec::new();

        let halves = run_hybrid_halves(
            || -> Result<()> {
                Err(AppError::SessionCacheLowSpace {
                    free_bytes: 0,
                    floor_bytes: 1,
                    outcome: crate::error::CacheDeletion::Pending,
                })
            },
            |pass| {
                passes.push(pass);
                Ok(())
            },
        );

        assert!(
            matches!(halves, Err(AppError::SessionCacheLowSpace { .. })),
            "the search fails with the low-space error: {halves:?}"
        );
        assert!(
            passes.is_empty(),
            "the lexical pass doesn't run: {passes:?}"
        );
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
        let DiscoveredKeys {
            keys,
            warnings: directory_warnings,
            reads,
        } = discover_agent_keys(environment, meter, &operation).unwrap();
        operation.finish().fail_if_low_space().unwrap();
        assert_eq!(
            reads.roots.len(),
            environment.storages().len(),
            "every agent lists its sessions"
        );
        assert!(
            directory_warnings.is_empty(),
            "every directory is listed: {directory_warnings:?}"
        );
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
    /// A thread whose rollout holds only its header and doesn't hold a
    /// conversation.
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
        let mut discovery_keys = root_keys(&KeyedRoot {
            environment: &environment,
            storage: &storage,
            root: &storage.root,
            meter: AgentMeter::UNMETERED,
            operation: &waiting_operation(&environment),
            project_directories: &crate::history::path::CanonicalDirectories::of_system(),
        })
        .unwrap()
        .keys;
        discovery_keys.sort_by(|left, right| left.path.cmp(&right.path));

        for workspace in [None, Some(&workspace)] {
            let (keys, warnings) = loaded_agent_keys(
                &discovered,
                &[],
                &[],
                workspace,
                &crate::history::path::CanonicalDirectories::of_system(),
            );

            assert_eq!(keys, discovery_keys);
            assert!(
                keys.iter().any(|key| key.session_id == LISTED_CODEX_THREAD),
                "the listed thread gets a key: {keys:?}"
            );
            assert!(warnings.is_empty());
        }
    }

    /// Only the directory the load recorded places a Claude Code key in a
    /// workspace, as it places the row, because the key is made from its
    /// path alone.
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

        let (keys, _) = loaded_agent_keys(
            &discovered,
            &[],
            &[],
            Some(&workspace),
            &crate::history::path::CanonicalDirectories::of_system(),
        );
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
                unreadable_directories: Vec::new(),
            },
            current_session_ids: Vec::new(),
            workspace: Some(home.workspace(&repository)),
            stale_cause: history::StaleCause::AnotherRearview,
            project_directories: crate::history::path::CanonicalDirectories::of_system().into(),
        };

        let listing = SessionSelection::from_loaded(
            sessions,
            &rules,
            &CommandPerf::new(Arc::new(TestClock::default())),
        )
        .unwrap()
        .0
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

    #[test]
    fn local_sessions_command_lists_only_workspace_of_environments_working_directory() {
        let claude = history::provider::claude::tests::ConfigDir::new();
        let workspaces = tempfile::tempdir().unwrap();
        let here = workspaces.path().join("here");
        let elsewhere = workspaces.path().join("elsewhere");
        for repository in [&here, &elsewhere] {
            std::fs::create_dir_all(repository.join(".git")).unwrap();
        }
        let here_session = "0f000000-0000-4000-8000-000000000001";
        let transcript = claude_session_in(&claude, &here, here_session, 30);
        claude_session_in(
            &claude,
            &elsewhere,
            "0f000000-0000-4000-8000-000000000002",
            31,
        );
        let storage: &'static dyn history::provider::SessionStorage =
            Box::leak(Box::new(history::provider::RootedStorage {
                inner: history::Source::Claude.provider().storage(),
                root: history::provider::SessionRoot::new(
                    transcript.parent().unwrap().parent().unwrap(),
                )
                .in_agent_tree(),
            }));
        let environment = test_environment(vec![storage], None).with_working_directory(here);

        let output = AgentService::new(environment)
            .execute(agent_command(&["sessions", "--local"]))
            .unwrap_or_else(crate::error_report);

        assert!(
            output.contains("\nsessions matched=1 ")
                && output.contains(&format!(" uuid={here_session} ")),
            "the listing is the working directory's one session:\n{output}"
        );
    }

    /// A listing leaves out the calling session, which changes with every
    /// tool call: the load doesn't reread its cached row, and the listing
    /// prints what a load that reread it prints.
    #[test]
    fn changed_calling_session_stays_unread_and_listing_is_unchanged() {
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
                    &storage.root,
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
                    unreadable_directories: Vec::new(),
                },
                current_session_ids: current_session_ids.clone(),
                workspace: None,
                stale_cause: history::StaleCause::AnotherRearview,
                project_directories: crate::history::path::CanonicalDirectories::of_system().into(),
            };
            let (selection, stale_coverage) = SessionSelection::from_loaded(
                sessions,
                &rules,
                &CommandPerf::new(Arc::new(TestClock::default())),
            )
            .unwrap();
            let mut warnings = selection.warnings.clone();
            warnings.extend(stale_coverage.record(Zone::System, &CommandTargets));
            agent::sessions::format_sessions_with_warnings(
                &selection.listing(&rules.window, 0, 10).unwrap(),
                None,
                &warnings,
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
        claude: history::provider::claude::tests::ConfigDir,
        directory: tempfile::TempDir,
        cache_base: tempfile::TempDir,
        changed: PathBuf,
        storage: &'static dyn history::provider::SessionStorage,
        new_sessions_written: std::cell::Cell<usize>,
    }

    const CHANGED_SESSION: &str = "0f000000-0000-4000-8000-000000000001";
    const OTHER_SESSION: &str = "0f000000-0000-4000-8000-000000000002";
    /// The last group of the first new session's ID, as a number.
    const FIRST_NEW_SESSION_NUMBER: usize = 3;

    impl CachedClaudeSessions {
        fn new() -> Self {
            let claude = history::provider::claude::tests::ConfigDir::new();
            let directory = tempfile::tempdir().unwrap();
            let changed = claude_session_in(&claude, directory.path(), CHANGED_SESSION, 30);
            claude_session_in(&claude, directory.path(), OTHER_SESSION, 31);
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
                claude,
                directory,
                cache_base,
                changed,
                storage,
                new_sessions_written: std::cell::Cell::new(0),
            }
        }

        /// Append a question at 06:40 UTC to `CHANGED_SESSION`.
        fn append_to_the_changed_session(&self) {
            self.append_question_to(&self.changed);
        }

        /// Append a question at 06:40 UTC to the session at `transcript`.
        fn append_question_to(&self, transcript: &Path) {
            let question = serde_json::json!({
                "type": "user", "timestamp": "2026-07-26T06:40:00.000Z",
                "cwd": self.directory.path(), "message": {"role": "user", "content": "one more"}
            });
            let mut text = std::fs::read_to_string(transcript).unwrap();
            text.push_str(&format!("{question}\n"));
            std::fs::write(transcript, text).unwrap();
        }

        /// Write `count` more sessions after the cache's load, each a
        /// question at 06:45, and return their IDs.
        fn write_new_sessions(&self, count: usize) -> Vec<String> {
            let written = self.new_sessions_written.get();
            self.new_sessions_written.set(written + count);
            let session_ids: Vec<String> = (written..written + count)
                .map(|index| {
                    format!(
                        "0f000000-0000-4000-8000-{:012}",
                        FIRST_NEW_SESSION_NUMBER + index
                    )
                })
                .collect();
            for session_id in &session_ids {
                let user = serde_json::json!({
                    "type": "user", "timestamp": "2026-07-26T06:45:00.000Z",
                    "cwd": self.directory.path(), "message": {"role": "user", "content": "new"}
                });
                std::fs::write(self.transcript_of(session_id), format!("{user}\n")).unwrap();
            }
            session_ids
        }

        /// The transcript of the session `session_id` names.
        fn transcript_of(&self, session_id: &str) -> PathBuf {
            self.changed.with_file_name(format!("{session_id}.jsonl"))
        }

        /// Cache every session written so far.
        fn cache_sessions(&self) {
            load_through(
                self.storage,
                self.cache_base.path(),
                history::provider::LeftOutRows::NONE,
            );
        }

        /// The references of the sessions `session_ids` name.
        fn references_of(&self, session_ids: &[String]) -> Vec<String> {
            let selection =
                self.select_beside_another_refresh(&every_session_in(TimeFilter::default()));
            session_ids
                .iter()
                .map(|session_id| reference_of(&selection, session_id))
                .collect()
        }

        /// The selection `rules` make while another rearview owns the
        /// cache's refresh, its warnings ending with the `stale-cache` record
        /// naming every stale session.
        fn select_beside_another_refresh(&self, rules: &SelectionRules) -> SessionSelection {
            self.select_from_sessions_beside_another_refresh(rules, Vec::new())
        }

        /// [`Self::select_beside_another_refresh`] for a command launched
        /// from the sessions `current_session_ids` name.
        fn select_from_sessions_beside_another_refresh(
            &self,
            rules: &SelectionRules,
            current_session_ids: Vec<String>,
        ) -> SessionSelection {
            let other_refresh = history::sqlite_cache::refresh::RefreshOwnership::acquire_for_test(
                &history::CacheBase::new(
                    self.cache_base.path(),
                    history::ProcessIdentity::default(),
                    Arc::new(TestClock::default()),
                    SystemCanonicaliser::shared(),
                ),
            )
            .unwrap();
            let environment = test_environment(vec![self.storage], Some(self.cache_base.path()));
            let perf = CommandPerf::new(Arc::clone(environment.clock()));
            let operation = command_operation(environment.cache_base(), perf.load_meter());
            let mut loaded = LoadedSessions::load(rules, &perf, &environment, &operation).unwrap();
            operation.finish().fail_if_low_space().unwrap();
            loaded.current_session_ids = current_session_ids;
            let (mut selection, stale_coverage) =
                SessionSelection::from_loaded(loaded, rules, &perf).unwrap();
            drop(other_refresh);
            selection
                .warnings
                .extend(stale_coverage.record(environment.clock().zone(), &CommandTargets));
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

    #[test]
    fn selection_beside_another_refresh_reads_cache_and_names_changed_session() {
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

    /// An uncached session counts as changed. A record naming it leaves out
    /// `cached-until`.
    #[test]
    fn selection_beside_another_refresh_names_uncached_session_without_cached_until() {
        let sessions = CachedClaudeSessions::new();
        let uncached = sessions.write_new_sessions(1);

        let selection =
            sessions.select_beside_another_refresh(&every_session_in(TimeFilter::default()));

        assert_eq!(
            warning_records(&selection.warnings),
            [format!(
                "protocol agent-warning stale-cache refs={} : {STALE_CACHE_SENTENCE}\n",
                reference_of(&selection, &uncached[0])
            )]
        );
    }

    /// A window that ends inside the cached copy's messages ends before the
    /// messages missing from the copy.
    #[test]
    fn selection_whose_window_ends_inside_cached_messages_doesnt_warn() {
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
            "the selection doesn't warn: {:?}",
            selection.warnings
        );
    }

    /// A window starting after the changed session's copy ends names the
    /// session, unless the selection leaves it out as the current session or
    /// as part of an excluded project.
    #[test]
    fn window_after_stale_copy_doesnt_name_current_session_or_excluded_project() {
        let sessions = CachedClaudeSessions::new();
        let project = sessions
            .select_beside_another_refresh(&every_session_in(TimeFilter::default()))
            .conversations
            .into_iter()
            .find(|row| row.session_id == CHANGED_SESSION)
            .and_then(|row| row.project_name)
            .unwrap();
        sessions.append_to_the_changed_session();
        let after_the_copy = TimeFilter {
            after: Some(utc_at(35, 0)),
            before: None,
        };
        let names_the_stale_copy = |rules: &SelectionRules, current_session_ids: Vec<String>| {
            let selection =
                sessions.select_from_sessions_beside_another_refresh(rules, current_session_ids);
            warning_records(&selection.warnings)
                .iter()
                .any(|record| record.contains(" stale-cache "))
        };

        assert!(
            names_the_stale_copy(&every_session_in(after_the_copy), Vec::new()),
            "the stale copy is named for a window starting after its end"
        );
        assert!(
            !names_the_stale_copy(
                &SelectionRules {
                    excludes_current_session: true,
                    ..every_session_in(after_the_copy)
                },
                vec![CHANGED_SESSION.to_owned()],
            ),
            "the current session's stale copy isn't named"
        );
        assert!(
            !names_the_stale_copy(
                &SelectionRules {
                    excluded_projects: [project].into_iter().collect(),
                    ..every_session_in(after_the_copy)
                },
                Vec::new(),
            ),
            "an excluded project's stale copy isn't named"
        );
    }

    /// Each of two zones shows its own offset and local times, whatever zone
    /// the machine is set to, and reads `--after` in that zone: only the
    /// session recorded at 06:31 UTC has a message from 06:31 on.
    #[test]
    fn agent_sessions_shows_and_reads_times_in_zone_of_its_clock() {
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
            self.execute_beside_another_refresh(args)
                .unwrap_or_else(crate::error_report)
        }

        /// The result of the agent command `args` while another rearview owns
        /// the cache's refresh.
        fn execute_beside_another_refresh(&self, args: &[&str]) -> Result<String> {
            let other_refresh = history::sqlite_cache::refresh::RefreshOwnership::acquire_for_test(
                &history::CacheBase::new(
                    self.cache_base.path(),
                    history::ProcessIdentity::default(),
                    Arc::new(TestClock::default()),
                    SystemCanonicaliser::shared(),
                ),
            )
            .unwrap();
            let environment = test_environment(vec![self.storage], Some(self.cache_base.path()));
            let output = AgentService::new(environment).execute(agent_command(args));
            drop(other_refresh);
            output
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
            self.start_a_sub_agent_of(&self.changed);
        }

        /// The transcript of `OTHER_SESSION`.
        fn other(&self) -> PathBuf {
            self.transcript_of(OTHER_SESSION)
        }

        /// Start a sub-agent of the session at `transcript` after the session
        /// was cached.
        fn start_a_sub_agent_of(&self, transcript: &Path) {
            let subagents = transcript.with_extension("").join("subagents");
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
    fn every_command_warns_of_stale_copy_for_each_window_past_its_end() {
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
                let is_right = if *warns {
                    output.contains(&record)
                } else {
                    !output.contains("stale-cache")
                };
                if !is_right {
                    wrong.push(format!(
                        "{command} with {window}: expected the record {}; got:\n{output}",
                        if *warns { "present" } else { "absent" }
                    ));
                }
            }
        }
        assert!(wrong.is_empty(), "{}", wrong.join("\n"));
    }

    /// `CHANGED_SESSION`'s `stale-cache` record when it also names
    /// `left_out`, references of stale sessions the output leaves out.
    fn record_naming_changed_session_and(reference: &str, left_out: &[String]) -> String {
        let references: Vec<&str> = std::iter::once(reference)
            .chain(left_out.iter().map(String::as_str))
            .collect();
        changed_session_record(&references.join(","))
    }

    /// The `stale-cache` record that counts `count` sessions the output
    /// leaves out and doesn't name any.
    fn record_counting(count: usize) -> String {
        format!(
            "protocol agent-warning stale-cache omitted-sessions={count} : {STALE_CACHE_SENTENCE}\n"
        )
    }

    /// Both commands show `CHANGED_SESSION` and leave out the new sessions,
    /// which are uncached.
    #[test]
    fn sessions_and_search_name_eight_left_out_uncached_sessions_and_count_nine() {
        let sessions = CachedClaudeSessions::new();
        let reference = sessions.changed_reference();
        sessions.append_to_the_changed_session();
        let outputs_without = |record: &str| -> Vec<String> {
            [&["sessions", "--all"][..], &["search", "question", "--all"]]
                .into_iter()
                .filter_map(|args| {
                    let output = sessions.run_beside_another_refresh(args);
                    (!output.contains(record)).then(|| format!("{}:\n{output}", args[0]))
                })
                .collect()
        };

        let uncached = sessions.references_of(&sessions.write_new_sessions(8));
        let names_eight = record_naming_changed_session_and(&reference, &uncached);
        let wrong_with_eight = outputs_without(&names_eight);
        sessions.write_new_sessions(1);
        let counts_nine =
            changed_session_record(&reference).replace(" : ", " omitted-sessions=9 : ");
        let wrong_with_nine = outputs_without(&counts_nine);

        assert!(
            wrong_with_eight.is_empty(),
            "each command names the eight sessions it leaves out:\n{names_eight}{}",
            wrong_with_eight.join("\n")
        );
        assert!(
            wrong_with_nine.is_empty(),
            "each command names the changed session and counts the nine it leaves out:\n{counts_nine}{}",
            wrong_with_nine.join("\n")
        );
    }

    /// A page of one session shows `OTHER_SESSION`, the newest by its cached
    /// copy, and leaves out `CHANGED_SESSION` and the new sessions.
    #[test]
    fn page_names_eight_left_out_stale_sessions_and_counts_nine() {
        let sessions = CachedClaudeSessions::new();
        let reference = sessions.changed_reference();
        sessions.append_to_the_changed_session();
        let first_page = ["sessions", "--all", "--limit", "1"];
        let uncached = sessions.references_of(&sessions.write_new_sessions(7));

        let eight_left_out = sessions.run_beside_another_refresh(&first_page);
        sessions.write_new_sessions(1);
        let nine_left_out = sessions.run_beside_another_refresh(&first_page);

        let named = record_naming_changed_session_and(&reference, &uncached);
        assert!(
            eight_left_out.contains(&named),
            "the record names the eight sessions the page leaves out:\n{named}{eight_left_out}"
        );
        assert!(
            nine_left_out.contains(&record_counting(9)),
            "the record counts the nine sessions the page leaves out:\n{nine_left_out}"
        );
    }

    /// Nine targets are more than a record names when the output leaves them
    /// out.
    #[test]
    fn read_names_nine_stale_targets() {
        let sessions = CachedClaudeSessions::new();
        let session_ids: Vec<String> = [CHANGED_SESSION, OTHER_SESSION]
            .map(String::from)
            .into_iter()
            .chain(sessions.write_new_sessions(7))
            .collect();
        sessions.cache_sessions();
        for session_id in &session_ids {
            sessions.append_question_to(&sessions.transcript_of(session_id));
        }
        let references = sessions.references_of(&session_ids);
        let args: Vec<&str> = std::iter::once("read")
            .chain(references.iter().map(String::as_str))
            .collect();

        let output = sessions.run_beside_another_refresh(&args);

        let record = changed_session_record(&references.join(","));
        assert!(
            output.contains(&record),
            "the record names every target:\n{record}{output}"
        );
    }

    /// A `stale-cache` record naming sixty refs is longer than the budget.
    #[test]
    fn sessions_and_search_beside_refresh_of_sixty_uncached_sessions_fit_budget() {
        let sessions = CachedClaudeSessions::new();
        sessions.write_new_sessions(60);

        let mut wrong = Vec::new();
        for args in [
            &["sessions", "--all", "--budget", "1000"][..],
            &["search", "question", "--all", "--budget", "1000"],
            &[
                "search", "question", "--all", "--mode", "hybrid", "--budget", "1000",
            ],
        ] {
            let output = sessions.run_beside_another_refresh(args);
            if !output.ends_with(&record_counting(60)) {
                wrong.push(format!("{args:?}:\n{output}"));
            }
        }
        assert!(
            wrong.is_empty(),
            "each output is complete and ends with the count:\n{}",
            wrong.join("\n")
        );
    }

    const BUDGET_TOO_SMALL_RECORD: &str = "protocol agent-error kind=budget-too-small detail=output%20budget%20cannot%20fit%20a%20complete%20compact%20record%3B%20increase%20--budget\n";

    impl CachedClaudeSessions {
        /// The commands among `sessions`, `search`, `within`, `read` and
        /// `outline` that, at `budget` and beside another refresh, don't fail
        /// with the `stale-cache` record of the changed session at
        /// `reference`, then the `budget-too-small` error; each with what it
        /// printed.
        fn commands_not_failing_after_stale_record(
            &self,
            reference: &str,
            budget: usize,
        ) -> Vec<String> {
            let expected = format!(
                "{}{BUDGET_TOO_SMALL_RECORD}",
                changed_session_record(reference)
            );
            let budget = budget.to_string();
            let commands: [Vec<&str>; 5] = [
                vec!["sessions", "--all"],
                vec!["search", "question", "--all"],
                vec!["within", reference, "question"],
                vec!["read", reference],
                vec!["outline", reference],
            ];
            commands
                .iter()
                .filter_map(|command_args| {
                    let args: Vec<&str> = command_args
                        .iter()
                        .copied()
                        .chain(["--budget", &budget])
                        .collect();
                    match self.execute_beside_another_refresh(&args) {
                        Ok(output) => Some(format!("{} succeeded:\n{output}", command_args[0])),
                        Err(error) => {
                            let failure = crate::error_report(error);
                            (failure != expected)
                                .then(|| format!("{} failed with:\n{failure}", command_args[0]))
                        }
                    }
                })
                .collect()
        }
    }

    /// The budget is 40 characters longer than the `stale-cache` record: too
    /// short for the record beside any command's header and first records.
    #[test]
    fn every_command_at_budget_below_its_shortest_output_prints_its_warning_before_error() {
        let sessions = CachedClaudeSessions::new();
        let reference = sessions.changed_reference();
        sessions.append_to_the_changed_session();
        let record = changed_session_record(&reference);

        let wrong = sessions
            .commands_not_failing_after_stale_record(&reference, record.chars().count() + 40);

        assert!(
            wrong.is_empty(),
            "each command prints the warning record, then the error:\n{}",
            wrong.join("\n")
        );
    }

    #[test]
    fn every_command_at_budget_shorter_than_its_warning_prints_warning_whole_before_error() {
        let sessions = CachedClaudeSessions::new();
        let reference = sessions.changed_reference();
        sessions.append_to_the_changed_session();
        let record = changed_session_record(&reference);

        let wrong = sessions
            .commands_not_failing_after_stale_record(&reference, record.chars().count() / 2);

        assert!(
            wrong.is_empty(),
            "each command prints the whole warning record, then the error:\n{}",
            wrong.join("\n")
        );
    }

    /// The budget is shorter than any command's header.
    #[test]
    fn budget_failure_prints_warnings_of_one_kind_for_two_sessions_as_one_count_record() {
        let sessions = CachedClaudeSessions::new();
        for transcript in [sessions.changed.clone(), sessions.other()] {
            let mut text = std::fs::read_to_string(&transcript).unwrap();
            text.push_str("{malformed\n");
            std::fs::write(&transcript, text).unwrap();
        }
        let environment =
            test_environment(vec![sessions.storage], Some(sessions.cache_base.path()));

        let failure = AgentService::new(environment)
            .execute(agent_command(&["sessions", "--all", "--budget", "60"]))
            .unwrap_or_else(crate::error_report);

        assert_eq!(
            failure,
            format!(
                "protocol agent-warning kind=malformed-transcript count=2\n{BUDGET_TOO_SMALL_RECORD}"
            )
        );
    }

    /// The output of the agent command `args` over `storages` and the
    /// session cache under `cache_base`, in `working_directory`, while
    /// another rearview owns the cache's refresh; or its error report.
    fn output_beside_another_refresh(
        storages: Vec<&'static dyn history::provider::SessionStorage>,
        cache_base: &Path,
        working_directory: &Path,
        args: &[&str],
    ) -> String {
        let other_refresh = history::sqlite_cache::refresh::RefreshOwnership::acquire_for_test(
            &history::CacheBase::new(
                cache_base,
                history::ProcessIdentity::default(),
                Arc::new(TestClock::default()),
                SystemCanonicaliser::shared(),
            ),
        )
        .unwrap();
        let environment = test_environment(storages, Some(cache_base))
            .with_working_directory(working_directory.to_path_buf());
        let output = AgentService::new(environment).execute(agent_command(args));
        drop(other_refresh);
        output.unwrap_or_else(crate::error_report)
    }

    /// `seconds` since the epoch as a `--after` or `--before` value in this
    /// machine's zone.
    fn window_bound(seconds: i64) -> String {
        chrono::DateTime::from_timestamp(seconds, 0)
            .unwrap()
            .with_timezone(&Zone::System)
            .format("%Y-%m-%dT%H:%M:%S")
            .to_string()
    }

    /// Every provider's fixture session, each in a home of its own, cached
    /// by one load. Set up once: each case reads the same transcripts over a
    /// copy of that cache.
    struct EveryProvidersCachedSession {
        _homes: Vec<tempfile::TempDir>,
        storages: Vec<&'static dyn history::provider::SessionStorage>,
        /// In the fixtures' order.
        rows: Vec<history::Conversation>,
        cache: crate::tui::claude_home::TemplateCache,
    }

    impl EveryProvidersCachedSession {
        fn new() -> Self {
            let fixtures = history::provider::contract_tests::registered_fixtures();
            let homes: Vec<tempfile::TempDir> = fixtures
                .iter()
                .map(|_| tempfile::tempdir().unwrap())
                .collect();
            let locators: Vec<PathBuf> = fixtures
                .iter()
                .zip(&homes)
                .map(|(fixture, home)| fixture.write_session(home.path(), fixture.ids().session))
                .collect();
            let storages: Vec<&'static dyn history::provider::SessionStorage> = fixtures
                .iter()
                .zip(&homes)
                .map(|(fixture, home)| {
                    history::provider::contract_tests::kept_storage_under(*fixture, home.path())
                })
                .collect();
            let cache = crate::tui::claude_home::admitted_cache();
            let environment = test_environment(storages.clone(), Some(cache.path()));
            let operation = waiting_operation(&environment);
            let mut loaded = history::load_history(
                &environment,
                false,
                None,
                None,
                history::provider::LeftOutRows::NONE,
                &operation,
            )
            .unwrap()
            .conversations;
            operation.finish().fail_if_low_space().unwrap();
            let rows = locators
                .iter()
                .map(|locator| {
                    let position = loaded
                        .iter()
                        .position(|row| &row.path == locator)
                        .unwrap_or_else(|| panic!("the load lists {}", locator.display()));
                    loaded.remove(position)
                })
                .collect();
            Self {
                _homes: homes,
                storages,
                rows,
                cache: crate::tui::claude_home::TemplateCache::new(cache),
            }
        }

        /// A copy of the cache in which `row`'s copy was read at an older
        /// fingerprint than its transcript's.
        fn cache_with_stale_copy_of(
            &self,
            row: &history::Conversation,
        ) -> crate::cache_file::test_support::TemporaryCacheDirectory {
            let copy = self.cache.copy();
            let changed = rusqlite::Connection::open(
                copy.path().join(history::sqlite_cache::DATABASE_FILE_NAME),
            )
            .unwrap()
            .execute(
                "UPDATE sessions SET mtime_secs = mtime_secs - 60 WHERE session_id = ?1",
                [&row.session_id],
            )
            .unwrap();
            assert_eq!(
                changed, 1,
                "the cache holds exactly one copy of {:?}",
                row.path
            );
            copy
        }
    }

    /// Every provider's stale copy is named, with the end of its messages,
    /// by `sessions` whose window spans that end, in either scope.
    #[test]
    fn sessions_names_every_providers_stale_copy_for_window_spanning_its_end_in_either_scope() {
        let sessions = EveryProvidersCachedSession::new();
        let mut wrong = Vec::new();
        for row in &sessions.rows {
            let cache = sessions.cache_with_stale_copy_of(row);
            let cached_until = row.message_times.last_second().unwrap();
            let window = [
                "--after".to_owned(),
                window_bound(cached_until - 30),
                "--before".to_owned(),
                window_bound(cached_until + 30),
            ];
            let working_directory = row.cwd.clone().unwrap_or_else(|| {
                history::provider::contract_tests::registered_fixtures()
                    .into_iter()
                    .find(|fixture| fixture.provider().source() == row.source)
                    .unwrap()
                    .session_working_dir()
            });
            for scope in ["--all", "--local"] {
                let args: Vec<&str> = ["sessions", scope]
                    .into_iter()
                    .chain(window.iter().map(String::as_str))
                    .collect();
                let output = output_beside_another_refresh(
                    sessions.storages.clone(),
                    cache.path(),
                    &working_directory,
                    &args,
                );
                let reference = output
                    .lines()
                    .find(|line| line.starts_with("session "))
                    .and_then(|line| agent::test_support::atom(line, "ref"));
                let record = reference.map(|reference| {
                    format!(
                        "protocol agent-warning stale-cache refs={reference} cached-until={} : \
                         {STALE_CACHE_SENTENCE}\n",
                        chrono::DateTime::from_timestamp(cached_until, 0)
                            .unwrap()
                            .with_timezone(&Zone::System)
                            .to_rfc3339_opts(chrono::SecondsFormat::Secs, false)
                    )
                });
                if !record.is_some_and(|record| output.contains(&record)) {
                    wrong.push(format!(
                        "{:?} with {scope}: the session is listed and named in the record:\n\
                         {output}",
                        row.source
                    ));
                }
            }
        }
        assert!(wrong.is_empty(), "{}", wrong.join("\n"));
    }

    const NO_VISIBLE_MESSAGES: &str = "skipped transcript has no visible messages";
    const NO_CONVERSATION_METADATA: &str =
        "skipped transcript has no searchable conversation metadata";
    const INVALID_UTF8_FAILURE: &str =
        "io failed to read transcript: stream did not contain valid UTF-8";

    /// A transcript whose session doesn't get a row in the list.
    #[derive(Debug)]
    enum UnlistedTranscript {
        /// The fixture session's first `kept_lines` lines, then `records`.
        Lines {
            kept_lines: usize,
            records: &'static [&'static str],
        },
        /// A line that is not valid UTF-8.
        InvalidUtf8,
        /// The fixture session's rows deleted from its database by
        /// `statements`, each given the session ID.
        DeletedRows(&'static [&'static str]),
    }

    impl UnlistedTranscript {
        const ZERO_BYTES: Self = Self::Lines {
            kept_lines: 0,
            records: &[],
        };

        /// Rewrites the fixture session at `locator` into this transcript.
        fn write_over(&self, locator: &Path) {
            match self {
                Self::Lines {
                    kept_lines,
                    records,
                } => {
                    let fixture = std::fs::read_to_string(locator).unwrap();
                    let text: String = fixture
                        .lines()
                        .take(*kept_lines)
                        .chain(records.iter().copied())
                        .map(|line| format!("{line}\n"))
                        .collect();
                    std::fs::write(locator, text).unwrap();
                }
                Self::InvalidUtf8 => {
                    std::fs::write(locator, b"{\"type\":\"user\",\"text\":\"\xff\xfe\"}\n").unwrap()
                }
                Self::DeletedRows(statements) => {
                    let session_id = locator.file_stem().unwrap().to_string_lossy();
                    let database = rusqlite::Connection::open(locator.parent().unwrap()).unwrap();
                    for statement in *statements {
                        database.execute(statement, [&*session_id]).unwrap();
                    }
                }
            }
        }
    }

    /// A session without a row, and the status and reason the owner's load
    /// records for it.
    struct UnlistedCase {
        source: history::Source,
        transcript: UnlistedTranscript,
        status: &'static str,
        reason: String,
    }

    /// One case per kind of session without a row each provider's fixture
    /// can produce. OpenCode doesn't have a case of a session that fails to
    /// read, because its database drops a malformed row.
    fn every_providers_unlisted_cases() -> Vec<UnlistedCase> {
        use UnlistedTranscript::{DeletedRows, InvalidUtf8, Lines};
        use history::Source::{Claude, Codex, Grok, Kimi, Omp, OpenCode, Pi};
        let empty = |source, transcript, reason: &str| UnlistedCase {
            source,
            transcript,
            status: "empty",
            reason: reason.to_owned(),
        };
        let foreign = |source: history::Source| {
            empty(
                source,
                UnlistedTranscript::ZERO_BYTES,
                &format!(
                    "malformed-transcript not a {} transcript",
                    source.provider().labels().display
                ),
            )
        };
        let unreadable = |source| UnlistedCase {
            source,
            transcript: InvalidUtf8,
            status: "unreadable",
            reason: INVALID_UTF8_FAILURE.to_owned(),
        };
        let kept_lines_and_record = |kept_lines: usize, record: &'static &'static str| Lines {
            kept_lines,
            records: std::slice::from_ref(record),
        };
        vec![
            foreign(Claude),
            empty(
                Claude,
                kept_lines_and_record(
                    0,
                    &r#"{"type":"user","timestamp":"2026-07-26T06:31:00.000Z","message":{"role":"user","content":""}}"#,
                ),
                NO_VISIBLE_MESSAGES,
            ),
            empty(
                Claude,
                kept_lines_and_record(
                    0,
                    &r#"{"type":"user","timestamp":"2026-07-26T06:31:00.000Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"tool output"}]}}"#,
                ),
                NO_CONVERSATION_METADATA,
            ),
            unreadable(Claude),
            foreign(Codex),
            empty(
                Codex,
                Lines {
                    kept_lines: 1,
                    records: &[],
                },
                NO_VISIBLE_MESSAGES,
            ),
            empty(
                Codex,
                kept_lines_and_record(
                    1,
                    &r#"{"timestamp":"2026-08-01T10:00:08.000Z","type":"response_item","payload":{"type":"custom_tool_call_output","id":"ctco_1","call_id":"call_1","output":[{"type":"input_text","text":"tool output"}]}}"#,
                ),
                NO_CONVERSATION_METADATA,
            ),
            unreadable(Codex),
            foreign(Kimi),
            empty(
                Kimi,
                Lines {
                    kept_lines: 1,
                    records: &[],
                },
                NO_VISIBLE_MESSAGES,
            ),
            empty(
                Kimi,
                kept_lines_and_record(
                    1,
                    &r#"{"type":"context.append_loop_event","event":{"type":"tool.result","parentUuid":"6f000000-0000-4000-8000-00000000000d","toolCallId":"Bash_0","result":{"output":"tool output"}},"time":1786010406000}"#,
                ),
                NO_CONVERSATION_METADATA,
            ),
            unreadable(Kimi),
            empty(Grok, UnlistedTranscript::ZERO_BYTES, NO_VISIBLE_MESSAGES),
            empty(
                Grok,
                kept_lines_and_record(
                    0,
                    &r#"{"method":"session/update","params":{"_meta":{"agentTimestampMs":1302,"promptId":"p"},"sessionId":"s","update":{"rawOutput":{"FileContent":{"content":"tool output"},"type":"ReadFile"},"sessionUpdate":"tool_call_update","status":"completed","toolCallId":"read-1"}},"timestamp":1}"#,
                ),
                NO_CONVERSATION_METADATA,
            ),
            unreadable(Grok),
            foreign(Pi),
            empty(
                Pi,
                Lines {
                    kept_lines: 1,
                    records: &[],
                },
                NO_VISIBLE_MESSAGES,
            ),
            empty(
                Pi,
                kept_lines_and_record(
                    1,
                    &r#"{"type":"message","timestamp":"2024-01-01T00:01:00.000Z","message":{"role":"toolResult","toolCallId":"call1","toolName":"read","content":[{"type":"text","text":"tool output"}]}}"#,
                ),
                NO_CONVERSATION_METADATA,
            ),
            unreadable(Pi),
            foreign(Omp),
            empty(
                Omp,
                Lines {
                    kept_lines: 2,
                    records: &[],
                },
                NO_VISIBLE_MESSAGES,
            ),
            empty(
                Omp,
                kept_lines_and_record(
                    2,
                    &r#"{"type":"message","id":"t1","parentId":null,"timestamp":"2026-01-02T03:04:17.000Z","message":{"role":"toolResult","toolCallId":"call_1","content":[]}}"#,
                ),
                NO_CONVERSATION_METADATA,
            ),
            unreadable(Omp),
            empty(
                OpenCode,
                DeletedRows(&[
                    "DELETE FROM part WHERE session_id = ?1",
                    "DELETE FROM message WHERE session_id = ?1",
                ]),
                NO_VISIBLE_MESSAGES,
            ),
            empty(
                OpenCode,
                DeletedRows(&["DELETE FROM part WHERE session_id = ?1 \
                     AND json_extract(data, '$.type') = 'text'"]),
                NO_CONVERSATION_METADATA,
            ),
        ]
    }

    /// Each session row of the cache under `cache_base`: its status and the
    /// reason recorded for it.
    fn recorded_statuses(cache_base: &Path) -> Vec<(String, Option<String>)> {
        let connection =
            rusqlite::Connection::open(cache_base.join(history::sqlite_cache::DATABASE_FILE_NAME))
                .unwrap();
        let mut statement = connection
            .prepare("SELECT status, entries_error FROM sessions ORDER BY id")
            .unwrap();
        statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .map(|row| row.unwrap())
            .collect()
    }

    /// One load of `storage`'s sessions as the refresh owner, writing the
    /// cache under `cache_base`.
    fn load_as_owner(storage: &'static dyn history::provider::SessionStorage, cache_base: &Path) {
        let environment = test_environment(vec![storage], Some(cache_base));
        let operation = waiting_operation(&environment);
        history::load_history(
            &environment,
            false,
            None,
            None,
            history::provider::LeftOutRows::NONE,
            &operation,
        )
        .unwrap();
        operation.finish().fail_if_low_space().unwrap();
    }

    /// The owner's load records, for each kind of session without a row
    /// each provider's fixture can produce, the warning agent commands print
    /// for it.
    #[test]
    fn owners_load_records_each_providers_reason_for_session_without_row() {
        let wrong: Vec<String> = std::thread::scope(|scope| {
            let cases: Vec<_> = every_providers_unlisted_cases()
                .into_iter()
                .map(|case| scope.spawn(move || recorded_reason_differs(&case)))
                .collect();
            cases
                .into_iter()
                .filter_map(|case| case.join().unwrap())
                .collect()
        });
        assert!(wrong.is_empty(), "{}", wrong.join("\n"));
    }

    /// The record the owner's load makes of `case`'s session, when it
    /// differs from the case's status and reason.
    fn recorded_reason_differs(case: &UnlistedCase) -> Option<String> {
        let fixture = history::provider::contract_tests::registered_fixtures()
            .into_iter()
            .find(|fixture| fixture.provider().source() == case.source)
            .unwrap();
        let home = tempfile::tempdir().unwrap();
        let locator = fixture.write_session(home.path(), fixture.ids().session);
        case.transcript.write_over(&locator);
        let cache = crate::tui::claude_home::admitted_cache();

        load_as_owner(
            history::provider::contract_tests::kept_storage_under(fixture, home.path()),
            cache.path(),
        );

        let recorded = recorded_statuses(cache.path());
        let expected = [(case.status.to_owned(), Some(case.reason.clone()))];
        (recorded != expected).then(|| {
            format!(
                "{:?}, {:?}: expected {expected:?}; recorded {recorded:?}",
                case.source, case.transcript
            )
        })
    }

    /// `agent read` of a Codex session on an empty cache whose write fails:
    /// the session's key needs the directory it recorded, and its key
    /// discovery's parse gives both the key and the transcript the command
    /// reads, so the transcript is read once.
    #[test]
    fn read_of_codex_session_on_empty_cache_whose_write_fails_reads_key_discoverys_parse() {
        use history::sqlite_cache::admitted_reads;
        let fixture = history::provider::contract_tests::registered_fixtures()
            .into_iter()
            .find(|fixture| fixture.provider().source() == history::Source::Codex)
            .unwrap();
        let home = tempfile::tempdir().unwrap();
        let locator = fixture.write_session(home.path(), fixture.ids().session);
        assert!(
            !agent::refs::is_keyed_by_path(fixture.provider(), &locator),
            "the key needs the session's record"
        );
        let storage = history::provider::contract_tests::kept_storage_under(fixture, home.path());
        let cache = crate::tui::claude_home::admitted_cache();
        let reference = locator.to_string_lossy().into_owned();
        let failing = fail_the_first_data_commit(cache.path(), || {});
        let before = admitted_reads::count(&locator);

        let output = AgentService::new(test_environment(vec![storage], Some(cache.path())))
            .execute(agent_command(&["read", &reference]))
            .unwrap_or_else(crate::error_report);

        drop(failing);
        assert!(
            !output.contains("agent-error") && output.contains("\nm1 "),
            "the parse is read:\n{output}"
        );
        assert_eq!(admitted_reads::count(&locator) - before, 1);
    }

    /// The reference `agent sessions --all` prints for the one session
    /// `storage` lists. The command writes that session's copy to the
    /// session cache under `cache`.
    fn cache_session_and_reference_it(
        storage: &'static dyn history::provider::SessionStorage,
        cache: &Path,
    ) -> String {
        let sessions = AgentService::new(test_environment(vec![storage], Some(cache)))
            .execute(agent_command(&["sessions", "--all"]))
            .unwrap_or_else(crate::error_report);
        sessions
            .split_whitespace()
            .find_map(|atom| atom.strip_prefix("ref="))
            .expect("agent sessions lists the session")
            .to_owned()
    }

    /// On a warm cache, `read`, `outline` and `within` of a session whose
    /// copy is current list its agent's sessions once, to resolve the
    /// reference, and read the copy as current, for every agent.
    #[test]
    fn targeted_commands_of_current_copy_list_its_agents_sessions_once() {
        let mut wrong = Vec::new();
        for fixture in history::provider::contract_tests::registered_fixtures() {
            let source = fixture.provider().source();
            let home = tempfile::tempdir().unwrap();
            fixture.write_every_record_session(home.path(), &fixture.every_record_session());
            let storage =
                history::provider::contract_tests::kept_storage_under(fixture, home.path());
            let cache = crate::tui::claude_home::admitted_cache();
            let reference = cache_session_and_reference_it(storage, cache.path());
            let discovery_record = format!("perf discovery source={} ", source.label());

            for args in [
                &["read", &reference][..],
                &["outline", &reference],
                &["within", &reference, "the"],
            ] {
                let service =
                    AgentService::new(test_environment(vec![storage], Some(cache.path())));
                let output = service
                    .execute(agent_command(args))
                    .unwrap_or_else(crate::error_report);
                let records = service.perf.records(None);
                let discovery = records
                    .lines()
                    .find(|line| line.starts_with(&discovery_record));
                if output.contains("agent-error")
                    || output.contains("stale-cache")
                    || !discovery.is_some_and(|record| record.contains(" roots=1 "))
                {
                    wrong.push(format!(
                        "{source:?} {}: the command succeeds without a stale-cache warning \
                         and lists the root once, {discovery:?}:\n{output}",
                        args[0]
                    ));
                }
            }
        }
        assert!(wrong.is_empty(), "{}", wrong.join("\n\n"));
    }

    /// `read` of a session that gained a turn since its copy was written
    /// parses it in one refresh round and prints the new reply, for every
    /// agent. The next read doesn't refresh the session.
    #[test]
    fn read_of_session_changed_since_its_copy_prints_new_reply_and_writes_copy() {
        const NEW_REPLY: &str = "Quokka harbour reply";
        let mut wrong = Vec::new();
        for fixture in history::provider::contract_tests::registered_fixtures() {
            let source = fixture.provider().source();
            let home = tempfile::tempdir().unwrap();
            let locator = fixture.write_session(home.path(), fixture.ids().session);
            let storage =
                history::provider::contract_tests::kept_storage_under(fixture, home.path());
            let cache = crate::tui::claude_home::admitted_cache();
            let reference = cache_session_and_reference_it(storage, cache.path());
            fixture.append_turn(&locator, NEW_REPLY);
            let read = || {
                let service =
                    AgentService::new(test_environment(vec![storage], Some(cache.path())));
                let output = service
                    .execute(agent_command(&["read", &reference]))
                    .unwrap_or_else(crate::error_report);
                (output, refresh_rounds(&service))
            };

            let (changed, changed_rounds) = read();
            let (current, current_rounds) = read();

            for (output, rounds, expected_rounds) in [
                (
                    changed,
                    changed_rounds,
                    &["perf refresh-round sessions=1"][..],
                ),
                (current, current_rounds, &[][..]),
            ] {
                if !output.contains(NEW_REPLY)
                    || output.contains("stale-cache")
                    || output.contains("agent-error")
                    || rounds != expected_rounds
                {
                    wrong.push(format!(
                        "{source:?}: the new reply prints without a warning, \
                         refresh rounds {expected_rounds:?}, got {rounds:?}:\n{output}"
                    ));
                }
            }
        }
        assert!(wrong.is_empty(), "{}", wrong.join("\n\n"));
    }

    /// When the write to an empty cache fails, `agent within` titles the hits
    /// from the read's row, the title kept outside the transcript included.
    #[test]
    fn within_of_session_whose_write_fails_prints_what_its_copy_prints() {
        let mut wrong = Vec::new();
        for fixture in history::provider::contract_tests::registered_fixtures() {
            let home = tempfile::tempdir().unwrap();
            fixture.write_every_record_session(home.path(), &fixture.every_record_session());
            let storage =
                history::provider::contract_tests::kept_storage_under(fixture, home.path());
            let run = |cache: &Path, args: &[&str]| {
                AgentService::new(test_environment(vec![storage], Some(cache)))
                    .execute(agent_command(args))
                    .unwrap_or_else(crate::error_report)
            };
            let cached = crate::tui::claude_home::admitted_cache();
            let sessions = run(cached.path(), &["sessions", "--all"]);
            let reference = sessions
                .split_whitespace()
                .find_map(|atom| atom.strip_prefix("ref="))
                .expect("agent sessions lists the session")
                .to_owned();
            let within = ["within", &reference, "the"];
            let uncached = crate::tui::claude_home::admitted_cache();
            let failing = fail_the_first_data_commit(uncached.path(), || {});

            let from_parse = run(uncached.path(), &within);
            drop(failing);
            let from_copy = run(cached.path(), &within);

            let source = fixture.provider().source();
            if from_parse != from_copy {
                wrong.push(format!(
                    "{source:?}: the parse prints what the copy prints; the parse:\n{from_parse}\n\
                     the copy:\n{from_copy}"
                ));
            }
            if !from_copy.contains("\ntitle ") {
                wrong.push(format!(
                    "{source:?}: the query finds a titled hit:\n{from_copy}"
                ));
            }
        }
        assert!(wrong.is_empty(), "{}", wrong.join("\n\n"));
    }

    /// `agent read` of each provider's session without a row reads the
    /// transcript once on an empty cache, and prints what it printed then
    /// when read again from the cache.
    ///
    /// A session whose path keys it prints the entries the refresh parsed,
    /// or fails with the warning the refresh built, a file the agent's
    /// format rejects included. Read again, a session the cache records as
    /// empty is read once more because the cache does not store its entries;
    /// an unreadable transcript is not read again.
    ///
    /// Key discovery reads a session keyed by the session ID and directory it
    /// recorded, and doesn't find a record or a key: the read reports it not
    /// found, and doesn't read it again from the cache.
    #[test]
    fn read_of_session_without_row_parses_it_once_then_again_only_from_an_empty_copy() {
        use history::sqlite_cache::admitted_reads;
        let mut wrong = Vec::new();
        let mut path_keyed_cases = 0;
        for case in every_providers_unlisted_cases() {
            let fixture = history::provider::contract_tests::registered_fixtures()
                .into_iter()
                .find(|fixture| fixture.provider().source() == case.source)
                .unwrap();
            let home = tempfile::tempdir().unwrap();
            let locator = fixture.write_session(home.path(), fixture.ids().session);
            case.transcript.write_over(&locator);
            let is_keyed_by_path = agent::refs::is_keyed_by_path(fixture.provider(), &locator);
            path_keyed_cases += usize::from(is_keyed_by_path);
            let storage =
                history::provider::contract_tests::kept_storage_under(fixture, home.path());
            let cache = crate::tui::claude_home::admitted_cache();
            let reference = locator.to_string_lossy().into_owned();
            let read = || {
                let before = admitted_reads::count(&locator);
                let output = AgentService::new(test_environment(vec![storage], Some(cache.path())))
                    .execute(agent_command(&["read", &reference]))
                    .unwrap_or_else(crate::error_report);
                (admitted_reads::count(&locator) - before, output)
            };

            let (cold_reads, cold) = read();
            let (warm_reads, warm) = read();

            let is_skipped = case.reason.starts_with("skipped ");
            let is_empty = case.status == "empty";
            let label = format!("{:?}, {:?}", case.source, case.transcript);
            let warm_parses = usize::from(is_keyed_by_path && is_empty);
            if (cold_reads, warm_reads) != (1, warm_parses) {
                wrong.push(format!(
                    "{label}: read {cold_reads} times, then {warm_reads} times"
                ));
            }
            if cold != warm {
                wrong.push(format!("{label}: prints\n{cold}\nthen\n{warm}"));
            }
            let prints_its_outcome = if is_keyed_by_path {
                cold.contains("agent-error") != is_skipped
            } else {
                cold.contains("agent-error kind=not-found")
            };
            if !prints_its_outcome {
                wrong.push(format!("{label}: prints\n{cold}"));
            }
        }
        assert!(path_keyed_cases > 0, "a provider keys its sessions by path");
        assert!(wrong.is_empty(), "{}", wrong.join("\n\n"));
    }

    /// A transcript another program holds open without sharing fails the
    /// owner's read with an error the next read can clear: the load doesn't
    /// record a row, and the load after the lock is released records the
    /// session.
    #[cfg(windows)]
    #[test]
    fn transcript_locked_during_owners_load_gets_row_only_from_load_after_release() {
        use std::os::windows::fs::OpenOptionsExt;
        let fixture = history::provider::contract_tests::registered_fixtures()
            .into_iter()
            .find(|fixture| fixture.provider().source() == history::Source::Claude)
            .unwrap();
        let home = tempfile::tempdir().unwrap();
        let locator = fixture.write_session(home.path(), fixture.ids().session);
        let storage = history::provider::contract_tests::kept_storage_under(fixture, home.path());
        let cache = crate::tui::claude_home::admitted_cache();
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&locator)
            .unwrap();

        load_as_owner(storage, cache.path());
        let while_locked = recorded_statuses(cache.path());
        drop(lock);
        load_as_owner(storage, cache.path());
        let after_release = recorded_statuses(cache.path());

        assert!(
            while_locked.is_empty(),
            "the locked transcript doesn't get a row: {while_locked:?}"
        );
        assert_eq!(
            after_release,
            [("listed".to_owned(), None)],
            "the load after the release records the session"
        );
    }

    /// An uncached session is named without `cached-until` when its path
    /// keys it, and left out without an error otherwise.
    #[test]
    fn sessions_names_every_providers_uncached_session_its_path_keys() {
        let mut wrong = Vec::new();
        for fixture in history::provider::contract_tests::registered_fixtures() {
            let home = tempfile::tempdir().unwrap();
            let locator = fixture.write_session(home.path(), fixture.ids().session);
            let cache = crate::tui::claude_home::admitted_cache();
            let output = output_beside_another_refresh(
                vec![history::provider::contract_tests::kept_storage_under(
                    fixture,
                    home.path(),
                )],
                cache.path(),
                home.path(),
                &["sessions", "--all"],
            );
            let stub = fixture
                .discover_under(home.path())
                .into_iter()
                .find(|stub| stub.locator == locator)
                .unwrap();
            let expected = agent::refs::session_key(
                fixture.provider(),
                &stub,
                None,
                &crate::history::path::CanonicalDirectories::of_system(),
                agent::refs::AgentConversationKey::address_for_test(
                    fixture.provider().source(),
                    &stub.locator,
                ),
            )
            .map(|key| {
                let reference =
                    agent::refs::resolved_conversation_for_key(std::slice::from_ref(&key), &key)
                        .reference
                        .canonical();
                format!(
                    "protocol agent-warning stale-cache refs={reference} : {STALE_CACHE_SENTENCE}\n"
                )
            });
            let is_right = match &expected {
                Some(record) => output.contains(record),
                None => {
                    !output.contains("stale-cache") && output.starts_with("protocol agent-sessions")
                }
            };
            if !is_right {
                wrong.push(format!(
                    "{}: expected {}; got:\n{output}",
                    fixture.provider().labels().name,
                    expected.as_deref().unwrap_or("no record and no error\n")
                ));
            }
        }
        assert!(wrong.is_empty(), "{}", wrong.join("\n"));
    }

    /// No key names this Pi session: it is uncached and its path doesn't
    /// contain a session ID.
    #[test]
    fn uncached_pi_session_without_id_in_path_is_left_out_without_error() {
        let fixture = history::provider::contract_tests::registered_fixtures()
            .into_iter()
            .find(|fixture| fixture.provider().source() == history::Source::Pi)
            .unwrap();
        let home = tempfile::tempdir().unwrap();
        let written = fixture.write_session(home.path(), fixture.ids().session);
        let without_id = written.with_file_name("notes.jsonl");
        std::fs::rename(&written, &without_id).unwrap();
        let cache = crate::tui::claude_home::admitted_cache();

        let output = output_beside_another_refresh(
            vec![history::provider::contract_tests::kept_storage_under(
                fixture,
                home.path(),
            )],
            cache.path(),
            home.path(),
            &["sessions", "--all"],
        );

        assert!(
            output.starts_with("protocol agent-sessions") && !output.contains("stale-cache"),
            "the session without an ID is left out without an error:\n{output}"
        );
    }

    /// A session that started a sub-agent since it was cached is read from
    /// its copy as it is beside another rearview's refresh: `read` doesn't
    /// wait for ownership, and `search` names it in the `stale-cache`
    /// record instead of an `io` warning.
    #[test]
    fn session_whose_sub_agents_changed_is_read_from_its_copy_beside_another_refresh() {
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

    /// The result of `search question --all` beside another rearview's
    /// refresh, `change` running on the cache's directory as the ranking opens
    /// its first reader.
    fn search_changing_cache_as_ranking_opens(
        sessions: &CachedClaudeSessions,
        change: fn(&Path),
    ) -> Result<String> {
        use crate::history::sqlite_cache::connection::test_hooks::{
            ReaderOpening, run_as_a_reader_opens,
        };
        let base = sessions.cache_base.path().to_path_buf();
        // The command's load opens its readers on other threads; the ranking
        // opens its first reader on the command's thread.
        let changed = run_as_a_reader_opens(
            sessions.cache_base.path(),
            ReaderOpening::OnThisThread(1),
            move || change(&base),
        );

        let output = sessions.execute_beside_another_refresh(&["search", "question", "--all"]);

        assert!(changed.has_run(), "the cache changes as the ranking reads");
        output
    }

    /// Checks that `CHANGED_SESSION`, changed by `change` as `search` starts
    /// its ranking beside another rearview's refresh, leaves the results and
    /// the `stale-cache` record names it.
    fn assert_session_changed_as_search_ranks_is_named_in_stale_cache_record(change: fn(&Path)) {
        let sessions = CachedClaudeSessions::new();
        let reference = sessions.changed_reference();

        let output = search_changing_cache_as_ranking_opens(&sessions, change)
            .unwrap_or_else(crate::error_report);

        assert!(
            !output.contains(&format!("uuid={CHANGED_SESSION}")),
            "the changed session doesn't have a hit:\n{output}"
        );
        assert!(
            output.contains(&format!("uuid={OTHER_SESSION}")),
            "the other session's hit is shown:\n{output}"
        );
        assert!(
            output.contains(&format!(
                "protocol agent-warning stale-cache refs={reference} : {STALE_CACHE_SENTENCE}\n"
            )),
            "the stale-cache record names the changed session:\n{output}"
        );
    }

    #[test]
    fn session_removed_as_search_ranks_is_named_in_stale_cache_record() {
        assert_session_changed_as_search_ranks_is_named_in_stale_cache_record(|base| {
            history::sqlite_cache::tests::remove_sessions_where(
                base,
                "session_id",
                CHANGED_SESSION,
            );
        });
    }

    #[test]
    fn session_being_written_as_search_ranks_is_named_in_stale_cache_record() {
        assert_session_changed_as_search_ranks_is_named_in_stale_cache_record(|base| {
            history::sqlite_cache::tests::mark_sessions_writing_where(
                base,
                "session_id",
                CHANGED_SESSION,
            );
        });
    }

    /// When the read of every session fails as `search` ranks, the command
    /// fails with one error and doesn't warn about any session: the failure is
    /// the cache's.
    #[test]
    fn search_whose_every_read_fails_fails_without_session_warnings() {
        let sessions = CachedClaudeSessions::new();

        let output = search_changing_cache_as_ranking_opens(
            &sessions,
            history::sqlite_cache::tests::hide_roots_table,
        );

        let Err(error) = output else {
            panic!("the search fails: {output:?}");
        };
        let report = crate::error_report(error);
        assert!(
            report.lines().count() == 1 && !report.contains("agent-warning"),
            "the command reports one error without any session warning:\n{report}"
        );
    }

    /// A query of separators ranks every session without reading a
    /// transcript: a ranked session doesn't have any hit messages, and its
    /// hit read doesn't open a reader.
    #[test]
    fn separators_only_query_doesnt_read_a_transcript() {
        use crate::history::sqlite_cache::connection::test_hooks::{
            ReaderOpening, run_as_a_reader_opens,
        };
        use crate::search::test_fixtures::cached_claude_sessions;
        let sessions = cached_claude_sessions(&[
            ("first", october_messages(&["zebrafish - one"])),
            ("second", october_messages(&["zebrafish / two"])),
        ]);
        let (keys, _) = keyed_inputs(&sessions.rows);
        let ranking = lexically_rank_scoped(
            &sessions.rows,
            "- /",
            ScopedSessions {
                indices: &[0, 1],
                scope: agent::search::AgentSearchScope::Global,
            },
            SearchMode::Lexical,
            &TimeFilter::default(),
            &TestClock::default(),
        )
        .unwrap();
        let reader =
            run_as_a_reader_opens(sessions.cache_path(), ReaderOpening::OnThisThread(1), || {});

        let reads = ranking
            .ranked
            .iter()
            .map(|&index| {
                let hits = ranking
                    .hit_messages
                    .of(index)
                    .expect("the session is ranked");
                assert!(hits.ordinals.is_empty(), "no message holds a hit: {hits:?}");
                read_hit_messages(
                    &TestClock::default(),
                    &history::WaitStatus::default(),
                    &ranking,
                    &sessions.rows[index],
                    hits,
                    &keys[index],
                )
                .unwrap()
                .expect("the session is in the cache")
            })
            .collect::<Vec<_>>();

        assert_eq!(ranking.ranked.len(), 2, "every session ranks");
        assert!(
            reads.iter().all(|read| read.transcript.messages.is_empty()),
            "the hit reads don't read a message"
        );
        assert!(!reader.has_run(), "the hit reads don't open a reader");
    }

    /// The read-ahead limit counts a hit read as the share of the transcript
    /// its hit messages hold, not the whole file.
    #[test]
    fn hit_read_counts_share_of_transcript_its_hit_messages_hold() {
        use crate::search::test_fixtures::cached_claude_sessions;
        let sessions = cached_claude_sessions(&[(
            "session",
            october_messages(&["zebrafish", "plain", "plain", "plain"]),
        )]);
        let (keys, _) = keyed_inputs(&sessions.rows);
        let ranking = lexically_rank_scoped(
            &sessions.rows,
            "zebrafish",
            ScopedSessions {
                indices: &[0],
                scope: agent::search::AgentSearchScope::Global,
            },
            SearchMode::Lexical,
            &TimeFilter::default(),
            &TestClock::default(),
        )
        .unwrap();
        let bytes = keys[0].transcript_bytes.expect("the key holds the size");

        assert_eq!(
            hit_read_bytes(&ranking, &sessions.rows[0], 0, &keys[0]),
            Some(bytes / 4),
            "one of four messages holds a hit"
        );
    }

    use crate::search::test_fixtures::october_messages;

    /// Each of `rows`' keys, and its search input.
    fn keyed_inputs(
        rows: &[history::Conversation],
    ) -> (
        Vec<agent::refs::AgentConversationKey>,
        Vec<agent::search::AgentConversationInput<'_>>,
    ) {
        let keys: Vec<_> = rows
            .iter()
            .map(|row| {
                agent::refs::AgentConversationKey::from_conversation(
                    row,
                    &crate::history::path::CanonicalDirectories::of_system(),
                )
                .unwrap()
            })
            .collect();
        let inputs = rows
            .iter()
            .zip(&keys)
            .enumerate()
            .map(
                |(index, (row, key))| agent::search::AgentConversationInput {
                    conversation: row,
                    resolved: agent::refs::resolved_conversation_for_key(&keys, key),
                    original_index: index,
                },
            )
            .collect();
        (keys, inputs)
    }

    /// A semantic hit on the dialogue of messages `focus` of the session at
    /// input `index`.
    fn semantic_dialogue_hit(
        index: usize,
        focus: agent::refs::MessageRange,
    ) -> crate::semantic::types::SemanticHit {
        crate::semantic::types::SemanticHit::new(
            crate::semantic::types::SemanticScoreBreakdown::new(0.9, 0.0),
            crate::semantic::types::SemanticExplanation {
                quality: crate::semantic::types::SemanticQuality::Good,
                quality_label: "good",
                matched_terms: Vec::new(),
                evidence_preview: "zebrafish".to_owned(),
                rationale_kind: crate::semantic::types::SemanticRationaleKind::SemanticOnly,
                chunk: crate::semantic::types::SemanticChunkIdentity {
                    conversation_index: index,
                    source: crate::semantic::types::SemanticChunkSource::VisibleDialogue,
                    session: "session".to_owned(),
                    chunk_index: 0,
                    message_range: focus,
                },
            },
        )
    }

    fn semantic_request() -> agent::search::AgentSearchRequest {
        agent::search::AgentSearchRequest {
            query: "zebrafish".to_owned(),
            top: 10,
            cli_mode: Some(SearchMode::Semantic),
            config_mode: None,
            tui_semantic_search: None,
            flat: true,
            hits_per_conversation: DEFAULT_HITS_PER_CONVERSATION,
            retrieval_hits_per_conversation: None,
            all_hits: false,
            budget: None,
            session_offset: 0,
            window: TimeFilter::default(),
        }
    }

    /// The agent route's text of each semantic candidate read from `rows`,
    /// and the route texts the read computed from the copies' texts.
    fn routes_read(
        rows: &[history::Conversation],
    ) -> (Vec<Vec<String>>, Vec<history::SemanticRoute>) {
        let (_, inputs) = keyed_inputs(rows);
        let read = semantic_candidates(&inputs).unwrap();
        let routes = read
            .candidates
            .iter()
            .filter(|candidate| {
                candidate.source == crate::semantic::types::SemanticChunkSource::AgentRoute
            })
            .map(|candidate| candidate.conversation.semantic_turns.clone())
            .collect();
        (routes, read.computed_routes)
    }

    /// A semantic search stores the route text it computed from a copy's
    /// texts, and later searches read it while the copy, the row's title and
    /// summary and the route algorithm's version stay; a change to any of
    /// them computes it again.
    #[test]
    fn semantic_route_text_is_read_as_stored_until_title_summary_version_or_copy_changes() {
        use crate::search::test_fixtures::cached_claude_sessions;
        let sessions = cached_claude_sessions(&[(
            "routed",
            october_messages(&["zebrafish keeper", "zebrafish tanks"]),
        )]);
        let service = AgentService::new(test_environment(Vec::new(), Some(sessions.cache_path())));
        let (computed, routes) = routes_read(&sessions.rows);
        assert_eq!(routes.len(), 1, "the first read computes the route text");
        assert!(
            computed[0][0].contains("zebrafish"),
            "the route reads the session's text: {computed:?}"
        );
        service.store_semantic_routes(&routes).unwrap();
        assert!(
            service
                .perf
                .records(None)
                .contains("perf semantic-routes computed=1 stored=1\n"),
            "the route text is stored"
        );
        // A stored text other than the computed one shows which is read.
        let cache =
            rusqlite::Connection::open(sessions.cache_path().join("history.sqlite3")).unwrap();
        cache
            .execute("UPDATE semantic_routes SET text = 'stored route'", [])
            .unwrap();

        assert_eq!(
            routes_read(&sessions.rows),
            (vec![vec!["stored route".to_owned()]], Vec::new()),
            "a later read reads the stored route text"
        );
        let mut renamed = sessions.rows.clone();
        renamed[0].custom_title = Some("Renamed keeper".to_owned());
        let (route, routes) = routes_read(&renamed);
        assert!(
            routes.len() == 1 && route[0][0].contains("renamed"),
            "a renamed row computes its route text again: {route:?}"
        );
        let mut summarised = sessions.rows.clone();
        summarised[0].summary = Some("Aquarium chores".to_owned());
        let (route, routes) = routes_read(&summarised);
        assert!(
            routes.len() == 1 && route[0][0].contains("aquarium"),
            "a row with another summary computes its route text again: {route:?}"
        );
        cache
            .execute("UPDATE semantic_routes SET version = version - 1", [])
            .unwrap();
        let (route, routes) = routes_read(&sessions.rows);
        assert!(
            routes.len() == 1 && route[0][0].contains("zebrafish"),
            "a route text an older route algorithm stored is computed again: {route:?}"
        );
        sessions.rewrite(
            0,
            &october_messages(&["zebrafish keeper", "zebrafish tanks", "zebrafish food"]),
        );
        let (route, routes) = routes_read(&sessions.rows);
        assert!(
            routes.len() == 1 && route[0][0].contains("food"),
            "another copy computes its route text again: {route:?}"
        );
    }

    /// While another rearview owns the refresh, a computed route text is not
    /// stored, and `--perf-stats` reports why.
    #[test]
    fn semantic_route_text_isnt_stored_while_another_rearview_owns_refresh() {
        use crate::search::test_fixtures::cached_claude_sessions;
        let sessions = cached_claude_sessions(&[("routed", october_messages(&["zebrafish"]))]);
        let service = AgentService::new(test_environment(Vec::new(), Some(sessions.cache_path())));
        let (_, routes) = routes_read(&sessions.rows);
        let other_refresh = history::sqlite_cache::refresh::RefreshOwnership::acquire_for_test(
            &history::CacheBase::new(
                sessions.cache_path(),
                history::ProcessIdentity::default(),
                Arc::new(TestClock::default()),
                SystemCanonicaliser::shared(),
            ),
        )
        .unwrap();

        service.store_semantic_routes(&routes).unwrap();

        drop(other_refresh);
        assert!(
            service
                .perf
                .records(None)
                .contains("perf semantic-routes computed=1 stored=0 skipped=another-rearview\n"),
            "the route text isn't stored: {}",
            service.perf.records(None)
        );
        assert_eq!(
            routes_read(&sessions.rows).1.len(),
            1,
            "the next read computes the route text again"
        );
    }

    /// A route write on a drive low on space fails with the low-space error
    /// and sets the cache's low-space latch. The low-space deletion then empties
    /// the cache database.
    #[test]
    fn semantic_route_write_finding_drive_low_on_space_fails_and_deletes_cache() {
        use crate::search::test_fixtures::cached_claude_sessions;
        let sessions = cached_claude_sessions(&[("routed", october_messages(&["zebrafish"]))]);
        let (_, routes) = routes_read(&sessions.rows);
        let service = AgentService::new(
            test_environment(Vec::new(), Some(sessions.cache_path())).with_disk_space(Arc::new(
                history::sqlite_cache::DriveRunningOut::after(0, || {}),
            )),
        );

        let stored = service.store_semantic_routes(&routes);

        assert!(
            matches!(&stored, Err(AppError::SessionCacheLowSpace { .. })),
            "the write fails with the low-space error: {stored:?}"
        );
        assert_eq!(
            history::sqlite_cache::low_space::run_deletion(service.environment.cache_base()),
            Some(crate::error::CacheDeletion::Deleted)
        );
        assert!(
            history::sqlite_cache::is_emptied(sessions.cache_path()),
            "the cache database's contents are deleted"
        );
    }

    /// A route write that fails for another reason doesn't stop the search;
    /// `--perf-stats` reports the cause.
    #[test]
    fn failed_semantic_route_write_reports_its_cause() {
        use crate::search::test_fixtures::cached_claude_sessions;
        let sessions = cached_claude_sessions(&[("routed", october_messages(&["zebrafish"]))]);
        let (_, routes) = routes_read(&sessions.rows);
        rusqlite::Connection::open(sessions.cache_path().join("history.sqlite3"))
            .unwrap()
            .execute_batch(
                "DROP TABLE semantic_routes; CREATE TABLE semantic_routes (session INTEGER \
                 PRIMARY KEY, version INTEGER, generation INTEGER, custom_title TEXT, \
                 summary TEXT, text TEXT CHECK (text = 'refused'))",
            )
            .unwrap();
        let service = AgentService::new(test_environment(Vec::new(), Some(sessions.cache_path())));

        service.store_semantic_routes(&routes).unwrap();

        let records = service.perf.records(None);
        assert!(
            records.contains(
                "perf semantic-routes computed=1 stored=0 skipped=write-failed \
                 cause=Session%20cache%20database%20error:%20CHECK%20constraint%20failed:\
                 %20text%20%3D%20%27refused%27\n"
            ),
            "the record names the write's cause, encoded as one field: {records}"
        );
    }

    /// The semantic stage reads a session another rearview wrote again since
    /// the load from the copy the cache holds, renumbered and timed as that
    /// copy holds it, and leaves out a session it removed, naming it. A hit
    /// on a message the new copy added gets that message's anchors.
    #[test]
    fn semantic_candidates_read_copy_cache_holds_and_leave_out_removed_session() {
        use crate::search::test_fixtures::{cached_claude_sessions, october};
        let sessions = cached_claude_sessions(&[
            ("kept", october_messages(&["zebrafish one"])),
            ("replaced", october_messages(&["zebrafish two"])),
            ("removed", october_messages(&["zebrafish three"])),
        ]);
        sessions.rewrite(
            1,
            &october_messages(&["inserted", "zebrafish two", "zebrafish appended"]),
        );
        sessions.remove(2);
        let (keys, inputs) = keyed_inputs(&sessions.rows);
        let held_generation = sessions.rows[1]
            .cached_session
            .as_ref()
            .unwrap()
            .read_copy(|held| Ok(held.generation()))
            .unwrap();

        let SemanticCandidates {
            candidates, reads, ..
        } = semantic_candidates(&inputs).unwrap();

        let visible = |index| {
            candidates
                .iter()
                .find(|candidate| {
                    candidate.index == index
                        && candidate.source
                            == crate::semantic::types::SemanticChunkSource::VisibleDialogue
                })
                .map(|candidate| {
                    (
                        candidate.conversation.semantic_turns.clone(),
                        candidate.conversation.semantic_turn_ranges.clone(),
                    )
                })
        };
        assert_eq!(
            visible(1),
            Some((
                vec![
                    "inserted".to_owned(),
                    "zebrafish two".to_owned(),
                    "zebrafish appended".to_owned()
                ],
                (1..=3).map(agent::refs::MessageRange::single).collect()
            )),
            "the replaced session's turns are the held copy's"
        );
        assert!(visible(0).is_some(), "the kept session is a candidate");
        assert!(
            visible(2).is_none(),
            "the removed session isn't a candidate"
        );
        assert_eq!(
            reads.no_longer_cached,
            [keys[2].clone()],
            "the removed session is named"
        );
        assert_eq!(
            reads.copies.get(&inputs[1].resolved.reference.canonical()),
            Some(&held_generation),
            "the replaced session's hits number the held copy"
        );
        let replaced = &reads.replaced_rows[&1];
        let third_message = october(10, 14).timestamp();
        assert_eq!(
            (replaced.message_count, replaced.message_times.span(3)),
            (
                3,
                Some(agent::time_span::TimeSpan::from_seconds(
                    third_message,
                    third_message
                ))
            ),
            "the replaced session's row is counted and timed as the held copy"
        );
        assert!(
            !reads.replaced_rows.contains_key(&0),
            "the kept session's own row stays: {:?}",
            reads.replaced_rows.keys()
        );

        let appended = semantic_dialogue_hit(1, agent::refs::MessageRange::single(3));
        let after_the_second_message = agent::search::AgentSearchRequest {
            window: TimeFilter {
                after: Some(
                    (october(10, 13) + chrono::Duration::minutes(30)).with_timezone(&Zone::System),
                ),
                before: None,
            },
            ..semantic_request()
        };
        let service = AgentService::new(test_environment(Vec::new(), Some(sessions.cache_path())));
        let (output, _) = service.fuse_semantic_hits(
            SemanticHalf {
                ranked: RankedHits {
                    conversations: vec![appended.clone()],
                    chunks: vec![appended],
                },
                reads,
            },
            &HashMap::new(),
            &inputs,
            &keys,
            |ranked, inputs| {
                agent::search::run_global_semantic_search(&after_the_second_message, inputs, ranked)
            },
        );
        let hits = output.every_hit().collect::<Vec<_>>();
        assert!(
            hits.len() == 1 && hits[0].focus_range.start == 3 && !hits[0].anchors.is_empty(),
            "the hit on the appended message is inside the window and gets its anchors: {hits:?}"
        );
    }

    /// The semantic stage, alone or as hybrid search's semantic half, reads
    /// the other sessions and leaves out a session another rearview is
    /// writing; the `stale-cache` record names it.
    #[test]
    fn semantic_candidates_leave_out_session_being_written_and_name_it() {
        use crate::search::test_fixtures::cached_claude_sessions;
        let sessions = cached_claude_sessions(&[
            ("kept", october_messages(&["zebrafish one"])),
            ("written", october_messages(&["zebrafish two"])),
        ]);
        sessions.mark_writing(1);
        let (keys, inputs) = keyed_inputs(&sessions.rows);
        let reference = inputs[1].resolved.reference.canonical();

        let SemanticCandidates {
            candidates, reads, ..
        } = semantic_candidates(&inputs).expect("the semantic stage reads the other session");
        let service = AgentService::new(test_environment(Vec::new(), Some(sessions.cache_path())));
        let (output, _) = service.fuse_semantic_hits(
            SemanticHalf {
                ranked: RankedHits {
                    conversations: Vec::new(),
                    chunks: Vec::new(),
                },
                reads,
            },
            &HashMap::new(),
            &inputs,
            &keys,
            |ranked, inputs| {
                agent::search::run_global_semantic_search(&semantic_request(), inputs, ranked)
            },
        );

        assert!(
            !candidates.is_empty() && candidates.iter().all(|candidate| candidate.index == 0),
            "the other session alone is a candidate"
        );
        assert_eq!(
            warning_records(&service.with_stale_cache_record(Vec::new(), &output)),
            [format!(
                "protocol agent-warning stale-cache refs={reference} : {STALE_CACHE_SENTENCE}\n"
            )],
            "the stale-cache record names the session being written"
        );
    }

    /// When the read of every session fails, the semantic stage fails with
    /// one error instead of a warning per session.
    #[test]
    fn semantic_candidates_whose_every_read_fails_fail_without_session_warnings() {
        use crate::search::test_fixtures::cached_claude_sessions;
        let sessions = cached_claude_sessions(&[
            ("first", october_messages(&["zebrafish one"])),
            ("second", october_messages(&["zebrafish two"])),
        ]);
        history::sqlite_cache::tests::hide_roots_table(sessions.cache_path());
        let (_, inputs) = keyed_inputs(&sessions.rows);

        let read = semantic_candidates(&inputs);

        if let Ok(SemanticCandidates { reads, .. }) = read {
            panic!(
                "the semantic stage fails instead of warning: {:?}",
                warning_records(&reads.failed)
            );
        }
    }

    /// A semantic hit gets the anchors of its focus message in the copy the
    /// semantic stage read. Its session's semantic hits leave the output,
    /// and the stale-cache record names it, when another rearview renumbers
    /// the session before the anchors are read, or the lexical hits read
    /// another copy of it.
    #[test]
    fn semantic_hits_of_session_written_again_since_semantic_stage_leave_output() {
        use crate::search::test_fixtures::cached_claude_sessions;
        let hit = semantic_dialogue_hit(0, agent::refs::MessageRange::single(2));
        let request = semantic_request();
        for (case, renumbers, lexical_reads_another_copy) in [
            ("unchanged", false, false),
            ("renumbered before the anchor read", true, false),
            ("read by the lexical hits at another copy", false, true),
        ] {
            let sessions = cached_claude_sessions(&[(
                "session",
                october_messages(&["plain", "zebrafish here"]),
            )]);
            let (keys, inputs) = keyed_inputs(&sessions.rows);
            let reference = inputs[0].resolved.reference.canonical();
            let generation = sessions.rows[0]
                .cached_session
                .as_ref()
                .unwrap()
                .generation();
            let semantic = SemanticHalf {
                ranked: RankedHits {
                    conversations: vec![hit.clone()],
                    chunks: vec![hit.clone()],
                },
                reads: SemanticReads {
                    copies: HashMap::from([(reference.clone(), generation)]),
                    ..SemanticReads::default()
                },
            };
            let lexical_copies = if lexical_reads_another_copy {
                HashMap::from([(reference.clone(), generation + 1)])
            } else {
                HashMap::new()
            };
            if renumbers {
                sessions.rewrite(
                    0,
                    &october_messages(&["inserted", "plain", "zebrafish here"]),
                );
            }
            let service =
                AgentService::new(test_environment(Vec::new(), Some(sessions.cache_path())));

            let (output, failed_reads) = service.fuse_semantic_hits(
                semantic,
                &lexical_copies,
                &inputs,
                &keys,
                |ranked, inputs| {
                    agent::search::run_global_semantic_search(&request, inputs, ranked)
                },
            );
            assert!(
                failed_reads.is_empty(),
                "{case}: every anchor read succeeds: {failed_reads:?}"
            );
            let records = warning_records(&service.with_stale_cache_record(Vec::new(), &output));

            let changed = renumbers || lexical_reads_another_copy;
            let hits = output.every_hit().collect::<Vec<_>>();
            if changed {
                assert!(
                    hits.is_empty(),
                    "{case}: the semantic hit leaves the output: {hits:?}"
                );
                assert_eq!(
                    records,
                    [format!(
                        "protocol agent-warning stale-cache refs={reference} : {STALE_CACHE_SENTENCE}\n"
                    )],
                    "{case}: the stale-cache record names the session"
                );
            } else {
                assert!(
                    hits.len() == 1 && !hits[0].anchors.is_empty(),
                    "{case}: the hit gets its focus message's anchors: {hits:?}"
                );
                assert!(
                    records.is_empty(),
                    "{case}: the command doesn't warn: {records:?}"
                );
            }
        }
    }

    /// A semantic hit whose anchor read fails keeps its place without
    /// anchors, and the command warns about its session.
    #[test]
    fn semantic_hit_whose_anchor_read_fails_stays_and_warns_about_its_session() {
        use crate::search::test_fixtures::cached_claude_sessions;
        let hit = semantic_dialogue_hit(0, agent::refs::MessageRange::single(2));
        let sessions =
            cached_claude_sessions(&[("session", october_messages(&["plain", "zebrafish here"]))]);
        let (keys, inputs) = keyed_inputs(&sessions.rows);
        let reference = inputs[0].resolved.reference.canonical();
        let generation = sessions.rows[0]
            .cached_session
            .as_ref()
            .unwrap()
            .generation();
        rusqlite::Connection::open(sessions.cache_path().join("history.sqlite3"))
            .unwrap()
            .execute_batch(
                "ALTER TABLE parts RENAME TO parts_kept; \
                 CREATE TABLE parts (id INTEGER PRIMARY KEY)",
            )
            .unwrap();
        let service = AgentService::new(test_environment(Vec::new(), Some(sessions.cache_path())));

        let (output, failed_reads) = service.fuse_semantic_hits(
            SemanticHalf {
                ranked: RankedHits {
                    conversations: vec![hit.clone()],
                    chunks: vec![hit],
                },
                reads: SemanticReads {
                    copies: HashMap::from([(reference.clone(), generation)]),
                    ..SemanticReads::default()
                },
            },
            &HashMap::new(),
            &inputs,
            &keys,
            |ranked, inputs| {
                agent::search::run_global_semantic_search(&semantic_request(), inputs, ranked)
            },
        );

        let hits = output.every_hit().collect::<Vec<_>>();
        assert!(
            hits.len() == 1 && hits[0].anchors.is_empty(),
            "the hit stays, without anchors: {hits:?}"
        );
        let records = warning_records(&failed_reads);
        assert!(
            records.len() == 1 && records[0].contains(&format!("ref={reference} ")),
            "the command warns about the session: {records:?}"
        );
    }

    /// Beside another rearview's refresh, a session the cache recorded as
    /// empty or unreadable gets the warning the refresh owner printed for
    /// it, and an uncached session is named in the `stale-cache` record.
    /// Neither is parsed. The owner's warning is the one its load recorded.
    #[test]
    fn sessions_and_search_beside_another_refresh_print_owners_warnings_and_name_uncached_session()
    {
        let user_line = |content: serde_json::Value| {
            format!(
                "{}\n",
                serde_json::json!({
                    "type": "user", "timestamp": "2026-07-26T06:31:00.000Z",
                    "message": {"role": "user", "content": content}
                })
            )
            .into_bytes()
        };
        let unlisted_sessions: [(&str, Vec<u8>, (&str, &str)); 4] = [
            (
                "empty, zero bytes",
                Vec::new(),
                ("malformed-transcript", "not%20a%20Claude%20transcript"),
            ),
            (
                "empty, without a visible message",
                user_line(serde_json::json!("")),
                ("skipped", "transcript%20has%20no%20visible%20messages"),
            ),
            (
                "empty, its tool output not listed",
                user_line(serde_json::json!([
                    {"type": "tool_result", "tool_use_id": "toolu_1", "content": "tool output"}
                ])),
                (
                    "skipped",
                    "transcript%20has%20no%20searchable%20conversation%20metadata",
                ),
            ),
            (
                "unreadable",
                b"{\"type\":\"user\",\"text\":\"\xff\xfe\"}\n".to_vec(),
                (
                    "io",
                    "failed%20to%20read%20transcript:%20stream%20did%20not%20contain%20valid%20UTF-8",
                ),
            ),
        ];
        let warnings_of = |output: &str| -> Vec<String> {
            output
                .lines()
                .filter(|line| {
                    line.starts_with("protocol agent-warning")
                        && !line.starts_with("protocol agent-warning stale-cache")
                })
                .map(|line| format!("{line}\n"))
                .collect()
        };
        let mut wrong = Vec::new();
        for (label, transcript, (kind, detail)) in unlisted_sessions {
            let claude = history::provider::claude::tests::ConfigDir::new();
            let directory = tempfile::tempdir().unwrap();
            let listed = claude_session_in(&claude, directory.path(), CHANGED_SESSION, 30);
            let folder = listed.parent().unwrap();
            std::fs::write(
                folder.join("0f000000-0000-4000-8000-000000000002.jsonl"),
                transcript,
            )
            .unwrap();
            let cache_base = tempfile::tempdir().unwrap();
            let storage: &'static dyn history::provider::SessionStorage =
                Box::leak(Box::new(history::provider::RootedStorage {
                    inner: history::Source::Claude.provider().storage(),
                    root: history::provider::SessionRoot::new(folder.parent().unwrap())
                        .in_agent_tree(),
                }));
            let owners =
                AgentService::new(test_environment(vec![storage], Some(cache_base.path())))
                    .execute(agent_command(&["sessions", "--all"]))
                    .unwrap();
            let owners_warnings = warnings_of(&owners);
            let is_expected = |warning: &String| {
                warning.starts_with(&format!("protocol agent-warning kind={kind} ref="))
                    && warning.ends_with(&format!(" detail={detail}\n"))
            };
            if owners_warnings.len() != 1 || !is_expected(&owners_warnings[0]) {
                wrong.push(format!(
                    "{label}: the owner prints exactly one {kind} warning with detail={detail}:\n\
                     {owners}"
                ));
                continue;
            }
            let uncached = claude_session_in(
                &claude,
                directory.path(),
                "0f000000-0000-4000-8000-000000000005",
                33,
            );
            wrong.extend(
                beside_another_refresh_warnings_differ(
                    storage,
                    cache_base.path(),
                    &uncached,
                    &owners_warnings,
                    warnings_of,
                )
                .into_iter()
                .map(|failure| format!("{label}: {failure}")),
            );
        }
        assert!(wrong.is_empty(), "{}", wrong.join("\n"));
    }

    /// The differences of `sessions` and `search` over `storage` and the
    /// cache under `cache_base`, beside another rearview's refresh, from the
    /// expected output: `owners_warnings` as `warnings_of` finds them, the
    /// session at `uncached` named in the `stale-cache` record without
    /// `cached-until`, and no transcript parsed.
    fn beside_another_refresh_warnings_differ(
        storage: &'static dyn history::provider::SessionStorage,
        cache_base: &Path,
        uncached: &Path,
        owners_warnings: &[String],
        warnings_of: impl Fn(&str) -> Vec<String>,
    ) -> Vec<String> {
        let mut wrong = Vec::new();
        for args in [
            &["sessions", "--all"][..],
            &["search", "question", "--all"][..],
        ] {
            let other_refresh = history::sqlite_cache::refresh::RefreshOwnership::acquire_for_test(
                &history::CacheBase::new(
                    cache_base,
                    history::ProcessIdentity::default(),
                    Arc::new(TestClock::default()),
                    SystemCanonicaliser::shared(),
                ),
            )
            .unwrap();
            let service = AgentService::new(test_environment(vec![storage], Some(cache_base)));
            let output = service
                .execute(agent_command(args))
                .unwrap_or_else(crate::error_report);
            drop(other_refresh);
            let command = args[0];
            if warnings_of(&output) != owners_warnings {
                wrong.push(format!(
                    "{command}: the warnings match the owner's:\n{}\nbeside the refresh:\n{output}",
                    owners_warnings.concat()
                ));
            }
            let uncached_record = format!(
                "protocol agent-warning stale-cache refs={} : {STALE_CACHE_SENTENCE}\n",
                agent::refs::session_key(
                    history::Source::Claude.provider(),
                    &history::provider::SessionStub {
                        locator: uncached.to_path_buf(),
                        subagents: Vec::new(),
                        cache_key: String::new(),
                        fingerprint: history::provider::Fingerprint {
                            size: 0,
                            modified: None,
                        },
                    },
                    None,
                    &crate::history::path::CanonicalDirectories::of_system(),
                    agent::refs::AgentConversationKey::address_for_test(
                        history::Source::Claude,
                        uncached,
                    ),
                )
                .map(|key| {
                    agent::refs::resolved_conversation_for_key(std::slice::from_ref(&key), &key)
                        .reference
                        .canonical()
                })
                .unwrap()
            );
            if !output.contains(&uncached_record) {
                wrong.push(format!(
                    "{command}: the record names the uncached session without cached-until:\n{output}"
                ));
            }
            let parsed: usize = service
                .perf
                .load_meter()
                .agents()
                .iter()
                .map(|agent| agent.refresh.transcripts)
                .sum();
            if parsed != 0 {
                wrong.push(format!(
                    "{command}: no transcript is parsed (parsed {parsed})"
                ));
            }
        }
        wrong
    }

    #[test]
    fn unreadable_directory_is_named_in_io_warning_and_its_cached_sessions_stay_listed() {
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
                .any(|line| line.starts_with("protocol agent-warning kind=io ")
                    && line.contains(&*folder_name)),
            "an io warning names the unreadable directory:\n{output}"
        );
        assert!(
            output.contains("\nsessions matched=2 "),
            "the cached sessions under the unreadable directory are listed:\n{output}"
        );
    }

    impl CachedClaudeSessions {
        /// An environment over these sessions whose directory listing
        /// refuses two Claude project folders, and the two folders' names
        /// as an `io` warning prints them. The first folder contains these
        /// sessions. The second is the project folder of a session recorded
        /// in `other_directory`.
        fn environment_refusing_two_folders(
            &self,
            other_directory: &Path,
        ) -> (history::Environment, [String; 2]) {
            let other = claude_session_in(
                &self.claude,
                other_directory,
                "0f000000-0000-4000-8000-000000000003",
                32,
            );
            let folders = [self.changed.parent().unwrap(), other.parent().unwrap()];
            let lister = Arc::new(history::provider::walk::test_support::ScriptedLister::default());
            for folder in folders {
                lister.refuse(folder);
            }
            let environment = history::Environment::new(
                vec![self.storage],
                self.cache_base.path().to_path_buf(),
                history::ProcessIdentity::default(),
                Arc::new(TestClock::default()),
                lister,
            );
            let folder_names = folders.map(|folder| {
                agent::protocol::escape_atom(&folder.file_name().unwrap().to_string_lossy())
            });
            (environment, folder_names)
        }
    }

    #[test]
    fn listing_prints_io_warning_for_each_unreadable_directory() {
        let sessions = CachedClaudeSessions::new();
        let other_directory = tempfile::tempdir().unwrap();
        let (environment, folder_names) =
            sessions.environment_refusing_two_folders(other_directory.path());

        let output = AgentService::new(environment)
            .execute(agent_command(&["sessions", "--all"]))
            .unwrap_or_else(crate::error_report);

        let io_records: Vec<&str> = output
            .lines()
            .filter(|line| line.starts_with("protocol agent-warning kind=io "))
            .collect();
        assert!(
            io_records.len() == 2
                && folder_names
                    .iter()
                    .all(|name| io_records.iter().any(|record| record.contains(name))),
            "each unreadable directory has an io record of its own:\n{output}"
        );
        assert!(
            output
                .lines()
                .next()
                .unwrap()
                .ends_with(" warnings=2 warning-records=2"),
            "the header counts both warnings:\n{output}"
        );
    }

    #[test]
    fn ref_not_found_prints_io_warning_for_each_unreadable_directory_before_error() {
        let sessions = CachedClaudeSessions::new();
        let other_directory = tempfile::tempdir().unwrap();
        let (environment, folder_names) =
            sessions.environment_refusing_two_folders(other_directory.path());

        let failure = AgentService::new(environment)
            .execute(agent_command(&["read", "ch_000000000000"]))
            .unwrap_or_else(crate::error_report);

        let lines: Vec<&str> = failure.lines().collect();
        assert!(
            lines.len() == 3
                && lines[..2]
                    .iter()
                    .all(|line| line.starts_with("protocol agent-warning kind=io "))
                && folder_names
                    .iter()
                    .all(|name| lines[..2].iter().any(|line| line.contains(name)))
                && lines[2].starts_with("protocol agent-error kind=not-found "),
            "an io record for each unreadable directory, then the error:\n{failure}"
        );
    }

    #[test]
    fn search_read_outline_and_within_name_unreadable_directory_once_and_read_its_cached_session() {
        let sessions = CachedClaudeSessions::new();
        let reference = sessions.changed_reference();
        let folder = sessions.changed.parent().unwrap();
        let folder_name =
            agent::protocol::escape_atom(&folder.file_name().unwrap().to_string_lossy());
        let lister = Arc::new(history::provider::walk::test_support::ScriptedLister::default());
        lister.refuse(folder);
        let commands: [(&[&str], &str); 4] = [
            (&["search", "answer", "--all"], "\nconversation rank="),
            (&["read", &reference], "an answer"),
            (&["outline", &reference], "a question"),
            (&["within", &reference, "answer"], "an answer"),
        ];

        let mut wrong = Vec::new();
        for (args, expected) in commands {
            let environment = history::Environment::new(
                vec![sessions.storage],
                sessions.cache_base.path().to_path_buf(),
                history::ProcessIdentity::default(),
                Arc::new(TestClock::default()),
                lister.clone(),
            );
            let output = AgentService::new(environment)
                .execute(agent_command(args))
                .unwrap_or_else(crate::error_report);
            let directory_warnings = output
                .lines()
                .filter(|line| {
                    line.starts_with("protocol agent-warning kind=io ")
                        && line.contains(&folder_name)
                })
                .count();
            if directory_warnings != 1 {
                wrong.push(format!(
                    "{}: exactly one io warning names the unreadable directory (found {directory_warnings}):\n{output}",
                    args[0]
                ));
            }
            if !output.contains(expected) {
                wrong.push(format!(
                    "{}: the cached session under the unreadable directory is read:\n{output}",
                    args[0]
                ));
            }
        }
        assert!(wrong.is_empty(), "{}", wrong.join("\n\n"));
    }

    /// Beside another rearview's refresh, search ranks a changed session on
    /// its cached copy: text only the transcript holds doesn't find a session,
    /// and the `stale-cache` record names the session.
    #[test]
    fn search_beside_another_refresh_ranks_changed_session_on_its_cached_copy() {
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
    fn read_beside_another_refresh_reads_stale_copy_holding_mixed_tool_result() {
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

    /// A cache write that finds less than 1 GB free on the cache's drive
    /// stops the command with an `agent-error` record naming the free space,
    /// and the cache database is deleted.
    #[test]
    fn command_whose_cache_write_finds_less_than_gigabyte_free_reports_agent_error() {
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
            "protocol agent-error kind=io detail=rearview:%20not%20enough%20disk%20space%20for%20the%20session%20cache%20%280.9%20GB%20free%2C%20needs%20at%20least%201%20GB%29.%20The%20session%20cache%20was%20deleted.%20Free%20some%20space%20and%20run%20rearview%20again.\n"
        );
        assert!(
            history::sqlite_cache::is_emptied(sessions.cache_base.path()),
            "the cache database's contents are deleted"
        );
    }

    /// A command whose cache write finds the drive low on space records
    /// the cache database's deletion as its `cache-deletion` stage.
    #[test]
    fn command_whose_cache_write_finds_drive_low_on_space_records_cache_deletion_stage() {
        let sessions = CachedClaudeSessions::new();
        sessions.append_to_the_changed_session();
        let service = AgentService::new(
            test_environment(vec![sessions.storage], Some(sessions.cache_base.path()))
                .with_free_space(950_000_000),
        );

        service
            .execute(agent_command(&["sessions", "--all"]))
            .unwrap_err();

        let records = service.perf.records(None);
        assert!(
            records.contains("perf stage=cache-deletion ms="),
            "the deletion's stage is recorded:\n{records}"
        );
    }

    /// The sentence a `stale-cache` record ends with after the command's own
    /// cache writes stopped.
    const WRITES_STOPPED_SENTENCE: &str = "rearview could not update these sessions in the session cache; repeat the command to include their newest messages";

    /// A command whose cache writes stop doesn't parse again: it reads the
    /// sessions it has not refreshed from their copies, and names them as
    /// sessions it could not update.
    #[test]
    fn command_whose_cache_writes_stop_reads_rest_from_their_copies_and_names_them() {
        let sessions = crate::tui::claude_home::ChangedSessionsInTwoRoots::new();
        let watch = sessions.watch.clone();
        let _failing = fail_the_first_data_commit(sessions.cache.path(), move || watch.mark());

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

    /// The references a `stale-cache` record in `output` names, with the
    /// sentence it ends with; empty without a record.
    fn stale_cache_references(output: &str) -> (Vec<String>, String) {
        let Some(record) = output
            .lines()
            .find(|line| line.starts_with("protocol agent-warning stale-cache refs="))
        else {
            return (Vec::new(), String::new());
        };
        let references = agent::test_support::atom(record, "refs")
            .unwrap()
            .split(',')
            .map(str::to_owned)
            .collect();
        let sentence = record.split_once(" : ").unwrap().1.to_owned();
        (references, sentence)
    }

    /// Cached sessions start a sub-agent after they were cached, and the
    /// refresh of `read`, `outline` and `within` fails at its first write.
    /// Each command reads the copies and names every session it could not
    /// update: for `read` of two sessions, the session whose write failed
    /// and the session whose write was withheld after it.
    #[test]
    fn targeted_commands_whose_refresh_writes_stop_name_copies_lacking_new_sub_agent() {
        let sessions = CachedClaudeSessions::new();
        let selection =
            sessions.select_beside_another_refresh(&every_session_in(TimeFilter::default()));
        let changed = reference_of(&selection, CHANGED_SESSION);
        let other = reference_of(&selection, OTHER_SESSION);
        sessions.start_a_sub_agent_of_the_changed_session();
        sessions.start_a_sub_agent_of(&sessions.other());
        let commands: [(&[&str], &[&String]); 3] = [
            (&["read", &changed, &other], &[&changed, &other]),
            (&["outline", &changed], &[&changed]),
            (&["within", &changed, "question"], &[&changed]),
        ];

        let mut wrong = Vec::new();
        for (args, read) in commands {
            let failing = fail_the_first_data_commit(sessions.cache_base.path(), || {});
            let output = AgentService::new(test_environment(
                vec![sessions.storage],
                Some(sessions.cache_base.path()),
            ))
            .execute(agent_command(args))
            .unwrap_or_else(crate::error_report);
            drop(failing);
            let (mut named, sentence) = stale_cache_references(&output);
            named.sort();
            let mut expected: Vec<String> =
                read.iter().map(|reference| (*reference).clone()).collect();
            expected.sort();
            if named != expected || sentence != WRITES_STOPPED_SENTENCE {
                wrong.push(format!(
                    "{}: the record names {expected:?} as sessions it could not update:\n{output}",
                    args[0]
                ));
            }
            if !output.contains("question") || output.contains("agent-error") {
                wrong.push(format!("{}: the copy is read:\n{output}", args[0]));
            }
        }
        assert!(wrong.is_empty(), "{}", wrong.join("\n\n"));
    }

    /// `read` and `outline` of an uncached session, whose refresh fails at
    /// its first write, read the session the refresh parsed: it is parsed
    /// once, and a cached session read beside it is not parsed.
    #[test]
    fn targeted_commands_of_uncached_session_whose_write_fails_read_its_one_parse() {
        let home = crate::tui::claude_home::ClaudeHome::new();
        let cache = crate::tui::claude_home::admitted_cache();
        let environment = || {
            home.environment_with_cache(
                cache.path(),
                history::ProcessIdentity::default(),
                Arc::new(TestClock::default()),
            )
        };
        let cached = home.write("cached", 5, &["a cached question"]);
        home.rows(&environment());
        let uncached = home.write("never", 6, &["an unseen question"]);
        let (cached, uncached) = (
            cached.to_string_lossy().into_owned(),
            uncached.to_string_lossy().into_owned(),
        );
        let commands: [&[&str]; 2] = [&["read", &cached, &uncached], &["outline", &uncached]];

        let reads_of = |path: &str| history::sqlite_cache::admitted_reads::count(Path::new(path));
        let mut wrong = Vec::new();
        for args in commands {
            let failing = fail_the_first_data_commit(cache.path(), || {});
            let before = (reads_of(&uncached), reads_of(&cached));
            let output = AgentService::new(environment())
                .execute(agent_command(args))
                .unwrap_or_else(crate::error_report);
            drop(failing);
            let reads = (reads_of(&uncached) - before.0, reads_of(&cached) - before.1);
            if reads != (1, 0) {
                wrong.push(format!(
                    "{}: reads the uncached and the cached session {reads:?} times",
                    args[0]
                ));
            }
            if !output.contains("an unseen question")
                || output.contains("agent-error")
                || output.contains("stale-cache")
            {
                wrong.push(format!("{}: the parse is read:\n{output}", args[0]));
            }
        }
        assert!(wrong.is_empty(), "{}", wrong.join("\n\n"));
    }

    /// Beside another rearview's refresh, `read` of a cached and an uncached
    /// session waits for refresh ownership, then refreshes the uncached
    /// session alone, reading its transcript once, and reads the cached
    /// session from its copy.
    #[test]
    fn read_beside_another_refresh_waits_then_parses_only_target_without_copy() {
        use history::sqlite_cache::admitted_reads;
        let sessions = SessionsBesideAnotherRefresh::new();
        let cached = sessions.home.write("cached", 5, &["a cached question"]);
        sessions.home.rows(&sessions.environment);
        let uncached = sessions.home.write("never", 6, &["an unseen question"]);
        let before = (
            admitted_reads::count(&cached),
            admitted_reads::count(&uncached),
        );

        let (output, refresh_rounds) = sessions.read_past_the_wait(&[&cached, &uncached]);

        assert!(
            output.contains("a cached question")
                && output.contains("an unseen question")
                && !output.contains("agent-error"),
            "both sessions print without an agent-error:\n{output}"
        );
        assert_eq!(
            (
                admitted_reads::count(&cached) - before.0,
                admitted_reads::count(&uncached) - before.1
            ),
            (0, 1),
            "the cached transcript isn't read and the uncached one is read once"
        );
        assert_eq!(
            refresh_rounds,
            ["perf refresh-round sessions=1"],
            "one refresh round refreshes the uncached session"
        );
    }

    /// Beside another rearview's refresh, `read` of a session whose copy is
    /// older than its transcript and of an uncached session waits for
    /// refresh ownership, then refreshes both: the older copy's newest
    /// message prints, and no `stale-cache` record names it.
    #[test]
    fn read_beside_another_refresh_refreshes_target_with_older_copy_after_the_wait() {
        let sessions = SessionsBesideAnotherRefresh::new();
        let changed = sessions.home.write("changed", 5, &["a cached question"]);
        sessions.home.rows(&sessions.environment);
        sessions.home.append(&changed, 7, "a newer question");
        let uncached = sessions.home.write("never", 6, &["an unseen question"]);

        let (output, refresh_rounds) = sessions.read_past_the_wait(&[&changed, &uncached]);

        assert!(
            output.contains("a newer question")
                && output.contains("an unseen question")
                && !output.contains("stale-cache")
                && !output.contains("agent-error"),
            "both sessions print their newest messages without a warning or an error:\n{output}"
        );
        assert_eq!(
            refresh_rounds,
            ["perf refresh-round sessions=2"],
            "one refresh round refreshes both sessions"
        );
    }

    /// The longest a test waits for a command on another thread to reach a
    /// wait before it fails: a watchdog for a hang.
    const COMMAND_WAIT_BEGINS_WITHIN: std::time::Duration = std::time::Duration::from_secs(60);

    /// Claude Code sessions whose session cache another rearview's refresh
    /// owns until a command reading them waits past its patience.
    struct SessionsBesideAnotherRefresh {
        home: crate::tui::claude_home::ClaudeHome,
        cache: crate::cache_file::test_support::TemporaryCacheDirectory,
        clock: Arc<TestClock>,
        environment: history::Environment,
    }

    impl SessionsBesideAnotherRefresh {
        fn new() -> Self {
            let home = crate::tui::claude_home::ClaudeHome::new();
            let cache = crate::tui::claude_home::admitted_cache();
            let clock = Arc::new(TestClock::default());
            let environment = home.environment_with_cache(
                cache.path(),
                history::ProcessIdentity::default(),
                clock.clone(),
            );
            Self {
                home,
                cache,
                clock,
                environment,
            }
        }

        /// The output and `refresh-round` records of `read` of `sessions`
        /// while another rearview's refresh owns the cache, released once
        /// the command waits past its patience.
        fn read_past_the_wait(&self, sessions: &[&Path]) -> (String, Vec<String>) {
            let references: Vec<String> = sessions
                .iter()
                .map(|path| path.to_string_lossy().into_owned())
                .collect();
            let mut args = vec!["read"];
            args.extend(references.iter().map(String::as_str));
            let other_refresh = history::sqlite_cache::refresh::RefreshOwnership::acquire_for_test(
                &crate::tui::claude_home::another_rearview(
                    self.cache.path(),
                    Arc::new(TestClock::default()),
                ),
            )
            .unwrap();
            let started = self.clock.now();
            std::thread::scope(|scope| {
                let command = scope.spawn(|| {
                    let service = AgentService::new(self.environment.clone());
                    let output = service
                        .execute(agent_command(&args))
                        .unwrap_or_else(crate::error_report);
                    (output, refresh_rounds(&service))
                });
                // Test time moves only while the command waits. Time past
                // twice its patience is its wait after reading the cache as
                // it is, because its first wait for ownership ends at its
                // patience.
                let deadline = std::time::Instant::now() + COMMAND_WAIT_BEGINS_WITHIN;
                while self.clock.elapsed_since(started) < REFRESH_OWNERSHIP_PATIENCE * 2 {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "the command waits past its patience"
                    );
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                drop(other_refresh);
                command.join().unwrap()
            })
        }
    }

    /// `read` of an empty session the cache holds and of an uncached
    /// session, whose write fails: the empty session, whose entries the
    /// cache does not store, is not parsed once the command's writes stop,
    /// and is reported as not in the session cache.
    #[test]
    fn read_of_empty_copy_after_the_commands_writes_stop_reports_it_unparsed() {
        use history::sqlite_cache::admitted_reads;
        let home = crate::tui::claude_home::ClaudeHome::new();
        let cache = crate::tui::claude_home::admitted_cache();
        let environment = home.environment_with_cache(
            cache.path(),
            history::ProcessIdentity::default(),
            Arc::new(TestClock::default()),
        );
        let empty = home.write("empty", 5, &[""]);
        home.rows(&environment);
        let uncached = home.write("never", 6, &["an unseen question"]);
        let references = [&uncached, &empty].map(|path| path.to_string_lossy().into_owned());
        let before = admitted_reads::count(&empty);
        let failing = fail_the_first_data_commit(cache.path(), || {});

        let output = AgentService::new(environment)
            .execute(agent_command(&["read", &references[0], &references[1]]))
            .unwrap_or_else(crate::error_report);

        drop(failing);
        assert_eq!(
            admitted_reads::count(&empty) - before,
            0,
            "the empty session isn't read"
        );
        assert!(
            output.contains("the%20session%20is%20not%20in%20the%20session%20cache"),
            "the empty session is reported as not in the session cache:\n{output}"
        );
    }

    /// The `refresh-round` records of `service`'s command.
    fn refresh_rounds(service: &AgentService) -> Vec<String> {
        service
            .perf
            .records(None)
            .lines()
            .filter(|line| line.starts_with("perf refresh-round "))
            .map(str::to_owned)
            .collect()
    }

    /// `read` of two ranges of one session on an empty cache refreshes the
    /// session once, in one round of one session, parsing the transcript
    /// once. The next read doesn't refresh the session or parse it.
    #[test]
    fn read_of_two_ranges_of_one_session_refreshes_it_once() {
        use history::sqlite_cache::admitted_reads;
        let home = crate::tui::claude_home::ClaudeHome::new();
        let cache = crate::tui::claude_home::admitted_cache();
        let environment = home.environment_with_cache(
            cache.path(),
            history::ProcessIdentity::default(),
            Arc::new(TestClock::default()),
        );
        let transcript = home.write("ranges", 5, &["first", "second", "third"]);
        let reference = transcript.to_string_lossy().into_owned();
        let (first, rest) = (format!("{reference}:m1"), format!("{reference}:m2..m3"));
        let read = || {
            let before = admitted_reads::count(&transcript);
            let service = AgentService::new(environment.clone());
            let output = service
                .execute(agent_command(&["read", &first, &rest]))
                .unwrap_or_else(crate::error_report);
            assert!(
                output.contains("first") && output.contains("third"),
                "both ranges print:\n{output}"
            );
            (
                admitted_reads::count(&transcript) - before,
                refresh_rounds(&service),
            )
        };

        let cold = read();
        let warm = read();

        assert_eq!(
            cold,
            (1, vec!["perf refresh-round sessions=1".to_owned()]),
            "on an empty cache: one parse and one refresh round of one session"
        );
        assert_eq!(
            warm,
            (0, Vec::<String>::new()),
            "from the current copy: no parse and no refresh round"
        );
    }

    /// A target this command published a copy of reads the copy at its
    /// generation as current, and a copy before it, as a cache rebuilt since
    /// holds, as older, as the printout reads it.
    #[test]
    fn target_whose_copy_is_before_its_published_generation_reads_as_older() {
        use history::sqlite_cache::Publication;
        let home = crate::tui::claude_home::ClaudeHome::new();
        let cache = crate::tui::claude_home::admitted_cache();
        let environment = home.environment_with_cache(
            cache.path(),
            history::ProcessIdentity::default(),
            Arc::new(TestClock::default()),
        );
        let transcript = home.write("published", 5, &["a question"]);
        let row = home
            .rows(&environment)
            .into_iter()
            .find(|row| row.path == transcript)
            .unwrap();
        let generation = history::sqlite_cache::read_copy_waiting(
            environment.cache_base(),
            row.cache_address.as_ref().unwrap(),
            |held| Ok(held.generation()),
        )
        .expect("the load cached the session");
        let key = agent::refs::AgentConversationKey::from_conversation(
            &row,
            &crate::history::path::CanonicalDirectories::of_system(),
        )
        .unwrap();
        let is_older = |published: i64| {
            let service = AgentService::new(environment.clone());
            let operation =
                command_operation(service.environment.cache_base(), service.perf.load_meter());
            let mut keyed = KeyedReads::default();
            keyed.sessions.insert(
                key.path.clone(),
                TargetRead::Listed {
                    address: key.address.clone(),
                    subagents: key.subagents.clone(),
                    publication: Publication::Published {
                        generation: published,
                    },
                    title_and_summary: history::provider::TitleAndSummary::default(),
                    unwritten: history::provider::UnwrittenParse::Nothing,
                },
            );
            let checked = service
                .refresh_and_check_targets(&operation, &[&key], &mut keyed)
                .unwrap();
            checked.stale_copies.contains_key(&key.path)
        };

        assert!(
            !is_older(generation),
            "the copy at its published generation reads as current"
        );
        assert!(
            is_older(generation + 1),
            "a copy before its published generation reads as older"
        );
    }

    /// `read` of a Codex session on an empty cache reads the session its
    /// key discovery cached, without a refresh round.
    #[test]
    fn read_of_session_key_discovery_cached_skips_the_refresh_round() {
        let fixture = history::provider::contract_tests::registered_fixtures()
            .into_iter()
            .find(|fixture| fixture.provider().source() == history::Source::Codex)
            .unwrap();
        let home = tempfile::tempdir().unwrap();
        let locator = fixture.write_session(home.path(), fixture.ids().session);
        let storage = history::provider::contract_tests::kept_storage_under(fixture, home.path());
        let cache = crate::tui::claude_home::admitted_cache();
        let service = AgentService::new(test_environment(vec![storage], Some(cache.path())));

        let output = service
            .execute(agent_command(&["read", &locator.to_string_lossy()]))
            .unwrap_or_else(crate::error_report);

        assert!(
            !output.contains("agent-error"),
            "the read succeeds:\n{output}"
        );
        assert_eq!(refresh_rounds(&service), Vec::<String>::new());
    }

    /// `read` of one uncached session more than a write group holds,
    /// whose first group's commit fails: the session left for the next
    /// group is reported as not in the session cache, without a parse,
    /// while each session of the first group is parsed once.
    #[test]
    fn read_whose_first_write_group_fails_reports_target_of_later_group_unparsed() {
        use history::sqlite_cache::admitted_reads;
        let home = crate::tui::claude_home::ClaudeHome::new();
        let cache = crate::tui::claude_home::admitted_cache();
        let sessions: Vec<PathBuf> = (0..=history::provider::READ_BATCH_LENGTH)
            .map(|index| {
                home.write(
                    &format!("never-{index}"),
                    5 + index as u32,
                    &["an unseen question"],
                )
            })
            .collect();
        let references: Vec<String> = sessions
            .iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect();
        let mut args = vec!["read"];
        args.extend(references.iter().map(String::as_str));
        let failing = fail_the_first_data_commit(cache.path(), || {});

        let output = AgentService::new(home.environment_with_cache(
            cache.path(),
            history::ProcessIdentity::default(),
            Arc::new(TestClock::default()),
        ))
        .execute(agent_command(&args))
        .unwrap_or_else(crate::error_report);

        drop(failing);
        let mut reads: Vec<usize> = sessions
            .iter()
            .map(|path| admitted_reads::count(path))
            .collect();
        reads.sort();
        let mut expected = vec![1; history::provider::READ_BATCH_LENGTH];
        expected.insert(0, 0);
        assert_eq!(
            reads, expected,
            "each session of the first group is read once and the later session isn't read"
        );
        assert!(
            output.contains("the%20session%20is%20not%20in%20the%20session%20cache"),
            "the later session is reported as not in the session cache:\n{output}"
        );
    }

    /// Beside another rearview's refresh, `read`, `outline` and `within` of a
    /// session that started a sub-agent since it was cached, whose copy
    /// another rearview then refreshed, read that copy as current: no
    /// `stale-cache` record names it. `read` and `outline` given the key of
    /// the session's row from before the sub-agent read the newer copy as
    /// current too.
    #[test]
    fn targeted_commands_read_copy_another_rearview_published_after_row_as_current() {
        let sessions = CachedClaudeSessions::new();
        let selection =
            sessions.select_beside_another_refresh(&every_session_in(TimeFilter::default()));
        let reference = reference_of(&selection, CHANGED_SESSION);
        let row = selection
            .conversations
            .iter()
            .find(|row| row.session_id == CHANGED_SESSION)
            .unwrap();
        let row_key = agent::refs::AgentConversationKey::from_conversation(
            row,
            &crate::history::path::CanonicalDirectories::of_system(),
        )
        .unwrap();
        sessions.start_a_sub_agent_of_the_changed_session();
        load_through(
            sessions.storage,
            sessions.cache_base.path(),
            history::provider::LeftOutRows::NONE,
        );

        let mut outputs: Vec<(String, String)> = [
            vec!["read", &reference],
            vec!["outline", &reference],
            vec!["within", &reference, "question"],
        ]
        .into_iter()
        .map(|args| {
            (
                args[0].to_owned(),
                sessions.run_beside_another_refresh(&args),
            )
        })
        .collect();
        let other_refresh = history::sqlite_cache::refresh::RefreshOwnership::acquire_for_test(
            &history::CacheBase::new(
                sessions.cache_base.path(),
                history::ProcessIdentity::default(),
                Arc::new(TestClock::default()),
                SystemCanonicaliser::shared(),
            ),
        )
        .unwrap();
        let service = || {
            AgentService::new(test_environment(
                vec![sessions.storage],
                Some(sessions.cache_base.path()),
            ))
        };
        let keys = std::slice::from_ref(&row_key);
        outputs.push((
            "read by the row's key".to_owned(),
            service()
                .run_read(&read_args(reference.clone()), Some(keys))
                .map_or_else(crate::error_report, |output| output.records),
        ));
        outputs.push((
            "outline by the row's key".to_owned(),
            service()
                .run_outline(
                    &AgentOutlineArgs {
                        conversation: reference.clone(),
                        perf_stats: false,
                        output: output_flags(),
                    },
                    Some(keys),
                )
                .map_or_else(crate::error_report, |output| output.records),
        ));
        drop(other_refresh);

        let wrong: Vec<String> = outputs
            .iter()
            .filter(|(_, output)| {
                output.contains("stale-cache")
                    || output.contains("agent-error")
                    || !output.contains("question")
            })
            .map(|(command, output)| {
                format!("{command} reads the newer copy as current:\n{output}")
            })
            .collect();
        assert!(wrong.is_empty(), "{}", wrong.join("\n\n"));
    }

    /// A cache write that finds the drive low on space, at the first, the
    /// middle or the last write, stops the command before its next parse.
    #[test]
    fn command_whose_cache_write_finds_drive_low_on_space_doesnt_parse_after_it() {
        use history::sqlite_cache::DriveRunningOut;
        let template = crate::tui::claude_home::ChangedSessionsTemplate::new();
        let checks = {
            let sessions = template.case();
            let drive = Arc::new(DriveRunningOut::after(usize::MAX, || {}));
            AgentService::new(sessions.environment().with_disk_space(drive.clone()))
                .execute(agent_command(&["sessions", "--all"]))
                .unwrap();
            drive.answers()
        };
        assert!(checks > 0, "the command checks free space");

        for roomy_checks in DriveRunningOut::check_points(checks) {
            let sessions = template.case();
            let watch = sessions.watch.clone();
            let drive = DriveRunningOut::after(roomy_checks, move || watch.mark());

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

    #[test]
    fn command_reports_agent_error_while_drive_doesnt_have_room_for_deleted_cache() {
        let sessions = crate::tui::claude_home::CacheOutgrowingDrive::new();
        let first = AgentService::new(sessions.environment())
            .execute(agent_command(&["sessions", "--all"]));
        assert!(
            matches!(&first, Err(AppError::AgentProtocol(record))
                if record.contains("The%20session%20cache%20was%20deleted.")),
            "the first command runs out of space and deletes the cache: {first:?}"
        );
        sessions.home.watch().mark();

        let outcome = AgentService::new(sessions.environment())
            .execute(agent_command(&["sessions", "--all"]));

        let Err(AppError::AgentProtocol(record)) = outcome else {
            panic!("the command fails with an agent-error record: {outcome:?}");
        };
        assert_eq!(
            record,
            "protocol agent-error kind=io detail=rearview:%20not%20enough%20disk%20space%20to%20rebuild%20the%20session%20cache%20%281.0%20GB%20free%2C%20needs%20at%20least%201.1%20GB%29.%20Free%20at%20least%200.1%20GB%20and%20run%20rearview%20again.\n"
        );
        assert_eq!(
            sessions.home.watch().parses_after_mark(),
            0,
            "the command doesn't parse any session"
        );
    }

    /// On a first fill of two agents' sessions, a write of the first agent's
    /// load finds the drive low on space, and the command returns early at
    /// the second agent's. Its record reports the deletion's outcome:
    /// deferred while another rearview holds the database open.
    #[test]
    fn command_returning_early_on_drive_low_on_space_reports_deletion_another_rearview_defers() {
        use crate::tui::claude_home::{held_open_by_another_rearview, two_agents_first_fill};
        let (environment, cache, _homes) = two_agents_first_fill();
        let environment = environment
            .with_disk_space(Arc::new(history::sqlite_cache::DriveRunningOut::after(
                0,
                || {},
            )))
            .with_low_space_wait(std::time::Duration::from_millis(300));
        let another_rearview = held_open_by_another_rearview(cache.path());

        let outcome = AgentService::new(environment).execute(agent_command(&["sessions", "--all"]));

        let Err(AppError::AgentProtocol(record)) = &outcome else {
            panic!("the command fails with its record: {outcome:?}");
        };
        assert!(
            record.contains(
                "The%20session%20cache%20was%20not%20deleted:%20another%20rearview%20has%20it%20open."
            ),
            "the record reports the deferred deletion: {record}"
        );
        drop(another_rearview);
    }

    /// A `within`, `read` or `outline` of an unknown session ID fails with
    /// the low-space record, not the reference's, once the first fill found
    /// the drive low on space, and deletes the cache database.
    #[test]
    fn reference_command_whose_session_isnt_found_after_fill_finds_drive_low_on_space_deletes_cache()
     {
        use crate::tui::claude_home::two_agents_first_fill;
        let unknown = "00000000-0000-4000-8000-000000000000";
        for arguments in [
            vec!["within", unknown, "question"],
            vec!["read", unknown],
            vec!["outline", unknown],
        ] {
            let (environment, cache, _homes) = two_agents_first_fill();
            let environment = environment.with_disk_space(Arc::new(
                history::sqlite_cache::DriveRunningOut::after(0, || {}),
            ));

            let outcome = AgentService::new(environment).execute(agent_command(&arguments));

            let Err(AppError::AgentProtocol(record)) = &outcome else {
                panic!("{arguments:?} fails with its record: {outcome:?}");
            };
            assert!(
                record.starts_with(
                    "protocol agent-error kind=io detail=rearview:%20not%20enough%20disk%20space"
                ),
                "{arguments:?} reports the low-space message: {record}"
            );
            assert!(
                history::sqlite_cache::is_emptied(cache.path()),
                "{arguments:?} deletes the cache database"
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
    fn targeted_commands_wait_through_rebuild_and_publish_it() {
        let sessions = CachedClaudeSessions::new();
        let reference = sessions.changed_reference();
        for command in [
            vec!["read", reference.as_str()],
            vec!["outline", reference.as_str()],
            vec!["within", reference.as_str(), "question"],
        ] {
            let rebuild = RebuildAtFirstWriter::on(sessions.cache_base.path());
            let clock = Arc::new(TestClock::default());
            let environment = history::environment::test_support::environment_on_clock(
                vec![sessions.storage],
                Some(sessions.cache_base.path()),
                clock.clone(),
            );
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
            let deadline = std::time::Instant::now() + COMMAND_WAIT_BEGINS_WITHIN;
            while wait.current() != Some(history::OtherRearview::Rebuilding) {
                assert!(
                    std::time::Instant::now() < deadline,
                    "{command:?} waits for the rebuild"
                );
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            // Only the command's polls of 200 ms move test time: 400 ms on,
            // it has tried again since it published its wait.
            let published = clock.now();
            while clock.elapsed_since(published) < std::time::Duration::from_millis(400) {
                assert!(
                    std::time::Instant::now() < deadline,
                    "{command:?} tries again during the rebuild"
                );
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
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

        let (keys, warnings) = loaded_agent_keys(
            &[],
            &failures,
            &[],
            None,
            &crate::history::path::CanonicalDirectories::of_system(),
        );

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

    /// `read`, `outline` and `within` beside an agent whose sessions cannot
    /// be listed read the other agent's session and print one warning
    /// naming the agent that failed.
    #[test]
    fn targeted_commands_warn_of_agent_whose_sessions_cannot_be_listed() {
        let home = HomeWithUnlistableCodex::new();

        let mut wrong = Vec::new();
        for args in [
            vec!["read", &home.claude_reference],
            vec!["outline", &home.claude_reference],
            vec!["within", &home.claude_reference, "question"],
        ] {
            let output = home.run(&args);
            if home.listing_warnings(&output) != 1 || output.contains("agent-error") {
                wrong.push(format!("{}:\n{output}", args[0]));
            }
        }
        assert!(wrong.is_empty(), "{}", wrong.join("\n\n"));
    }

    /// `read`, `outline` and `within` of a session of the agent whose
    /// sessions cannot be listed print the listing failure with the error.
    #[test]
    fn targeted_commands_print_listing_failure_with_error_for_reference_it_explains() {
        let home = HomeWithUnlistableCodex::new();

        let mut wrong = Vec::new();
        for args in [
            vec!["read", &home.codex_reference],
            vec!["outline", &home.codex_reference],
            vec!["within", &home.codex_reference, "question"],
        ] {
            let output = home.run(&args);
            if home.listing_warnings(&output) != 1 || !output.contains("agent-error") {
                wrong.push(format!("{}:\n{output}", args[0]));
            }
        }
        assert!(wrong.is_empty(), "{}", wrong.join("\n\n"));
    }

    /// `read` of a Codex session whose key needs its record, when the
    /// session cache fails to give the records, names the cache in the
    /// warning it prints with the error, not the listing.
    #[test]
    fn read_whose_key_records_the_cache_fails_to_give_warns_of_the_cache() {
        use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
        let fixture = history::provider::contract_tests::registered_fixtures()
            .into_iter()
            .find(|fixture| fixture.provider().source() == history::Source::Codex)
            .unwrap();
        let home = tempfile::tempdir().unwrap();
        let locator = fixture.write_session(home.path(), fixture.ids().session);
        let storage = history::provider::contract_tests::kept_storage_under(fixture, home.path());
        let cache = crate::tui::claude_home::admitted_cache();
        load_as_owner(storage, cache.path());
        let _refused = history::sqlite_cache::connection::test_hooks::on_every_connection(
            cache.path(),
            |connection| {
                connection
                    .authorizer(Some(|context: AuthContext<'_>| match context.action {
                        AuthAction::Read {
                            table_name: "sessions",
                            column_name: "cwd",
                        } => Authorization::Deny,
                        _ => Authorization::Allow,
                    }))
                    .unwrap();
            },
        );

        let output = AgentService::new(test_environment(vec![storage], Some(cache.path())))
            .execute(agent_command(&["read", &locator.to_string_lossy()]))
            .unwrap_or_else(crate::error_report);

        assert!(
            output.contains(
                "failed%20to%20read%20or%20write%20the%20session%20cache%20for%20Codex%20sessions"
            ) && !output.contains("failed%20to%20list")
                && output.contains("agent-error"),
            "the error's warning names the session cache, not the listing:\n{output}"
        );
    }

    /// A Claude and a Codex session under one home, with an environment
    /// whose directory listing refuses Codex's sessions root, so listing
    /// Codex's sessions fails.
    struct HomeWithUnlistableCodex {
        _home: tempfile::TempDir,
        cache: crate::cache_file::test_support::TemporaryCacheDirectory,
        storages: Vec<&'static dyn history::provider::SessionStorage>,
        lister: Arc<history::provider::walk::test_support::ScriptedLister>,
        claude_reference: String,
        codex_reference: String,
    }

    impl HomeWithUnlistableCodex {
        fn new() -> Self {
            let home = tempfile::tempdir().unwrap();
            let mut storages = Vec::new();
            let mut references = Vec::new();
            for source in [history::Source::Claude, history::Source::Codex] {
                let fixture = history::provider::contract_tests::registered_fixtures()
                    .into_iter()
                    .find(|fixture| fixture.provider().source() == source)
                    .unwrap();
                let locator = fixture.write_session(home.path(), fixture.ids().session);
                references.push(locator.to_string_lossy().into_owned());
                storages.push(history::provider::contract_tests::kept_storage_under(
                    fixture,
                    home.path(),
                ));
            }
            let lister = Arc::new(history::provider::walk::test_support::ScriptedLister::default());
            let cache = crate::tui::claude_home::admitted_cache();
            let environment = Self::environment_of(&storages, &cache, &lister);
            for root in storages[1]
                .roots_in(&environment.root_environment())
                .unwrap()
            {
                lister.refuse(&root.path);
            }
            let codex_reference = references.pop().unwrap();
            let claude_reference = references.pop().unwrap();
            Self {
                _home: home,
                cache,
                storages,
                lister,
                claude_reference,
                codex_reference,
            }
        }

        fn environment_of(
            storages: &[&'static dyn history::provider::SessionStorage],
            cache: &crate::cache_file::test_support::TemporaryCacheDirectory,
            lister: &Arc<history::provider::walk::test_support::ScriptedLister>,
        ) -> history::Environment {
            history::Environment::new(
                storages.to_vec(),
                cache.path().to_path_buf(),
                history::ProcessIdentity::default(),
                Arc::new(TestClock::default()),
                lister.clone(),
            )
        }

        /// The output of the agent command `args`, or its error report.
        fn run(&self, args: &[&str]) -> String {
            AgentService::new(Self::environment_of(
                &self.storages,
                &self.cache,
                &self.lister,
            ))
            .execute(agent_command(args))
            .unwrap_or_else(crate::error_report)
        }

        /// The number of `output`'s warnings that Codex's sessions could not
        /// be listed.
        fn listing_warnings(&self, output: &str) -> usize {
            output
                .lines()
                .filter(|line| line.contains("failed%20to%20list%20Codex%20sessions"))
                .count()
        }
    }

    /// Key discovery over a root without sessions in the session cache loads
    /// the root as the refresh owner, and keys its session from what that
    /// load recorded.
    #[test]
    fn session_in_root_without_cache_is_keyed_from_its_owners_load() {
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
                        &history::provider::SessionRoot::new(directory.path()),
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
                ScopedSessions {
                    indices: &[0, 1],
                    scope: agent::search::AgentSearchScope::Global,
                },
                mode,
                &TimeFilter::default(),
                &clock,
            )
            .unwrap()
            .ranked
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
            lexically_rank_scoped(
                &conversations,
                "flaky test",
                ScopedSessions {
                    indices: &[0],
                    scope: agent::search::AgentSearchScope::Global,
                },
                mode,
                window,
                &clock,
            )
            .unwrap()
            .ranked
        };

        assert_eq!(rank(SearchMode::Exact, &TimeFilter::default()), [0]);
        assert_eq!(rank(SearchMode::Lexical, &window_on_october_10), [0]);
        assert_eq!(
            rank(SearchMode::Exact, &window_on_october_10),
            Vec::<usize>::new()
        );
    }

    /// Exact mode ranks as lexical mode ranks the query, keeping the
    /// sessions holding the phrase: an unfinished quote, a quoted word
    /// beside a bare one and a phrase alone each find the sessions holding
    /// `alpha beta`, best first, at the end of a text too.
    #[test]
    fn exact_mode_ranks_as_lexical_mode_keeping_sessions_holding_phrase() {
        use search::test_fixtures::{cached_claude_sessions, claude_user_record, october};
        let sessions = cached_claude_sessions(&[
            (
                "ends",
                vec![claude_user_record("alpha beta", october(10, 12))],
            ),
            (
                "apart",
                vec![claude_user_record("beta then alpha", october(10, 13))],
            ),
            (
                "inside",
                vec![claude_user_record(
                    "alpha beta gamma alpha",
                    october(10, 14),
                )],
            ),
        ]);
        let conversations = &sessions.rows;
        let holding_phrase = [0, 2];
        let clock = TestClock::default();
        let rank = |query, mode| {
            lexically_rank_scoped(
                conversations,
                query,
                ScopedSessions {
                    indices: &[0, 1, 2],
                    scope: agent::search::AgentSearchScope::Global,
                },
                mode,
                &TimeFilter::default(),
                &clock,
            )
            .unwrap()
            .ranked
        };
        for query in ["alpha \"beta", "\"alpha\" beta", "\"alpha beta\""] {
            let expected = rank(query, SearchMode::Lexical)
                .into_iter()
                .filter(|index| holding_phrase.contains(index))
                .collect::<Vec<_>>();
            assert_eq!(
                expected.len(),
                holding_phrase.len(),
                "{query:?}: lexical mode ranks every session holding the phrase"
            );
            assert_eq!(
                rank(query, SearchMode::Exact),
                expected,
                "{query:?}: exact mode ranks the sessions holding the phrase as lexical mode does"
            );
        }
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
        let mut conversations = [history::Source::Claude
            .provider()
            .format()
            .parse_conversation(&path, history::parser::RowDating::new(None, &clock), None)
            .unwrap()
            .expect("the session is listed")];
        history::environment::test_support::cache_rows(&mut conversations);
        let keys = [agent::refs::AgentConversationKey::from_conversation(
            &conversations[0],
            &crate::history::path::CanonicalDirectories::of_system(),
        )
        .unwrap()];

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
            let ranked = lexically_rank_scoped(
                &conversations,
                query,
                ScopedSessions {
                    indices: &[0],
                    scope: agent::search::AgentSearchScope::Global,
                },
                mode,
                &request.window,
                &clock,
            )
            .unwrap()
            .ranked;
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
    fn agent_route_uses_compact_route_text() {
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

        let route = agent_route_of(&conversation, conversation.semantic_route_text()).unwrap();

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
            .unwrap()
            .records;

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
        let unlisted_warnings = [&unlisted_written_before, &unlisted_written_after]
            .into_iter()
            .map(|key| {
                let warning = AgentWarning::skipped(None, "transcript has no visible messages");
                (key.path.clone(), warning)
            })
            .collect();
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
            &unlisted_warnings,
        );

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
            .unwrap()
            .records;

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
            .unwrap()
            .records;

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
