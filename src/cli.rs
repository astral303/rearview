use crate::agent::protocol::MessageLineRange;
use crate::clock::Clock;
use crate::history::Source;
use crate::history::provider::providers;
use crate::search::mode::SearchMode;
use crate::time_filter::{TimeFilter, TimeFilterError, TimePoint};
use clap::builder::{PossibleValue, TypedValueParser};
use clap::error::ErrorKind;
use clap::{Arg, ArgGroup, Args as ClapArgs, Command, Parser, Subcommand};
use std::ffi::OsStr;
use std::fmt;
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::LazyLock;

/// Log level for debug output filtering
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum DebugLevel {
    Debug,
    Info,
    Warn,
    Error,
}

impl FromStr for DebugLevel {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "debug" => Ok(DebugLevel::Debug),
            "info" => Ok(DebugLevel::Info),
            "warn" | "warning" => Ok(DebugLevel::Warn),
            "error" => Ok(DebugLevel::Error),
            _ => Err(format!(
                "invalid log level '{}', expected: debug, info, warn, error",
                s
            )),
        }
    }
}

impl fmt::Display for DebugLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DebugLevel::Debug => write!(f, "debug"),
            DebugLevel::Info => write!(f, "info"),
            DebugLevel::Warn => write!(f, "warn"),
            DebugLevel::Error => write!(f, "error"),
        }
    }
}

#[derive(Debug, Subcommand)]
pub enum Commands {
    /// Run agent-oriented search and transcript commands
    Agent {
        #[command(subcommand)]
        command: Box<AgentCommand>,
    },
    /// Delete sessions the agent never answered
    DeleteEmpty {
        /// Delete matching sessions instead of printing a dry run
        #[arg(long)]
        yes: bool,
        /// Only inspect sessions recorded in the current repository, or,
        /// outside one, the current directory
        #[arg(long, group = "delete_empty_scope")]
        local: bool,
        /// Inspect all workspaces
        #[arg(long, group = "delete_empty_scope")]
        all: bool,
    },
    /// Update rearview to the latest version
    Update,
}

#[derive(Debug, ClapArgs)]
pub struct DeleteEmptyArgs {
    pub yes: bool,
    pub local: bool,
    pub all: bool,
}

#[derive(Debug, Subcommand)]
pub enum AgentCommand {
    /// List sessions newest first, or those with a message inside a time window
    Sessions(AgentSessionsArgs),
    /// Search across all conversations
    Search(AgentSearchArgs),
    /// Search within one conversation
    Within(AgentWithinArgs),
    /// Read transcript message ranges
    Read(AgentReadArgs),
    /// Outline a conversation transcript
    Outline(AgentOutlineArgs),
}

impl AgentCommand {
    /// The subcommand's name, as typed.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Sessions(_) => "sessions",
            Self::Search(_) => "search",
            Self::Within(_) => "within",
            Self::Read(_) => "read",
            Self::Outline(_) => "outline",
        }
    }

    pub fn reports_perf_stats(&self) -> bool {
        match self {
            Self::Sessions(args) => args.perf_stats,
            Self::Search(args) => args.perf_stats,
            Self::Within(args) => args.perf_stats,
            Self::Read(args) => args.perf_stats,
            Self::Outline(args) => args.perf_stats,
        }
    }
}

#[derive(Debug, ClapArgs, Default)]
#[group(id = "agent_search_mode", multiple = false)]
pub struct AgentSearchModeArgs {
    /// Search mode for this invocation
    #[arg(long, value_enum)]
    pub mode: Option<SearchMode>,
    /// Alias for --mode lexical
    #[arg(long)]
    lexical: bool,
    /// Alias for --mode semantic
    #[arg(long)]
    semantic: bool,
    /// Alias for --mode exact. Match the whole query as one phrase
    #[arg(long)]
    exact: bool,
    /// Alias for --mode hybrid. Combine lexical and semantic message ranking
    #[arg(long)]
    hybrid: bool,
}

impl AgentSearchModeArgs {
    #[cfg(test)]
    pub fn explicit(mode: SearchMode) -> Self {
        Self {
            mode: Some(mode),
            ..Self::default()
        }
    }

    pub fn override_value(&self) -> Option<SearchMode> {
        self.mode.or_else(|| {
            [
                (self.lexical, SearchMode::Lexical),
                (self.semantic, SearchMode::Semantic),
                (self.exact, SearchMode::Exact),
                (self.hybrid, SearchMode::Hybrid),
            ]
            .into_iter()
            .find_map(|(selected, mode)| selected.then_some(mode))
        })
    }
}

#[derive(Debug, ClapArgs)]
pub struct AgentSearchArgs {
    /// Search query
    #[arg(value_parser = non_empty_string)]
    pub query: String,
    /// Maximum conversations, or message hits with --flat
    #[arg(long, value_parser = non_zero_usize)]
    pub top: Option<usize>,
    /// Output budget in Unicode characters
    #[arg(long, value_parser = non_zero_usize, conflicts_with = "no_budget")]
    pub budget: Option<usize>,
    /// Disable output budgeting
    #[arg(long)]
    pub no_budget: bool,
    /// Rank message hits across conversations; --top counts hits
    #[arg(long)]
    pub flat: bool,
    /// Maximum evidence hits to show per conversation in grouped output
    #[arg(long, value_parser = non_zero_usize)]
    pub hits_per_conv: Option<usize>,
    /// Disable duplicate suppression inside grouped conversation results
    #[arg(long)]
    pub all_hits: bool,
    /// Skip this many matching sessions, in rank order, before searching; a
    /// `continue search` record names the next value. With --mode hybrid, an
    /// offset runs a lexical search
    #[arg(long, default_value_t = 0)]
    pub offset: usize,
    #[command(flatten)]
    pub scope: AgentScopeArgs,
    /// Report time per stage, what was read, and peak memory on stderr
    #[arg(long)]
    pub perf_stats: bool,
    #[command(flatten)]
    pub search_mode: AgentSearchModeArgs,
    #[command(flatten)]
    pub time: MessageTimeArgs,
}

#[derive(Debug, ClapArgs)]
pub struct AgentSessionsArgs {
    /// Maximum sessions to list; 20 by default
    #[arg(long, short = 'n', value_parser = non_zero_usize)]
    pub limit: Option<usize>,
    /// Output budget in Unicode characters
    #[arg(long, value_parser = non_zero_usize, conflicts_with = "no_budget")]
    pub budget: Option<usize>,
    /// Disable output budgeting
    #[arg(long)]
    pub no_budget: bool,
    /// Skip this many sessions, newest first, before listing; a `continue
    /// sessions` record names the next value
    #[arg(long, default_value_t = 0)]
    pub offset: usize,
    #[command(flatten)]
    pub scope: AgentScopeArgs,
    /// Report time per stage, what was read, and peak memory on stderr
    #[arg(long)]
    pub perf_stats: bool,
    #[command(flatten)]
    pub time: TimeRangeArgs,
}

#[derive(Debug, ClapArgs)]
pub struct AgentScopeArgs {
    /// Only sessions recorded in the current repository, or, outside one, the
    /// current directory
    #[arg(long, group = "agent_scope")]
    pub local: bool,
    /// Sessions from all workspaces, overriding a configured local scope
    #[arg(long, group = "agent_scope")]
    pub all: bool,
    /// Include the session this command was launched from
    #[arg(long)]
    pub include_current_session: bool,
}

#[derive(Debug, ClapArgs)]
pub struct AgentWithinArgs {
    /// Conversation: a ch_ handle, a session ID, or a transcript path
    #[arg(value_parser = non_empty_string)]
    pub conversation: String,
    /// Search query
    #[arg(value_parser = non_empty_string)]
    pub query: String,
    /// Maximum number of results
    #[arg(long, value_parser = non_zero_usize)]
    pub top: Option<usize>,
    /// Output budget in Unicode characters
    #[arg(long, value_parser = non_zero_usize, conflicts_with = "no_budget")]
    pub budget: Option<usize>,
    /// Disable output budgeting
    #[arg(long)]
    pub no_budget: bool,
    /// Report time per stage, what was read, and peak memory on stderr
    #[arg(long)]
    pub perf_stats: bool,
    #[command(flatten)]
    pub search_mode: AgentSearchModeArgs,
    #[command(flatten)]
    pub time: MessageTimeArgs,
}

#[derive(Debug, ClapArgs)]
pub struct AgentOutputFlags {
    /// Output budget in Unicode characters
    #[arg(long, value_parser = non_zero_usize, conflicts_with = "no_budget")]
    pub budget: Option<usize>,
    /// Disable output budgeting
    #[arg(long)]
    pub no_budget: bool,
    /// Include tool calls
    #[arg(long)]
    pub tools: bool,
    /// Include tool results
    #[arg(long)]
    pub tool_results: bool,
    /// Include thinking blocks
    #[arg(long)]
    pub thinking: bool,
    /// Include subagent internals
    #[arg(long)]
    pub subagents: bool,
    #[command(flatten)]
    pub time: MessageTimeArgs,
}

#[derive(Debug, ClapArgs)]
pub struct AgentReadArgs {
    /// Conversations to read, each a ch_ handle, a session ID, or a transcript
    /// path, optionally followed by :mN, :mN..mM, :mN.K for one call, or :sN
    /// for one sub-agent
    #[arg(required = true, value_parser = non_empty_string)]
    pub refs: Vec<String>,
    /// Durable message anchor to read within one conversation
    #[arg(long, value_parser = non_empty_string)]
    pub anchor: Option<String>,
    /// Message or range to prioritize when budgeted output is truncated
    #[arg(long, value_parser = non_empty_string)]
    pub focus: Option<String>,
    /// Read the last N messages of each ref or range, not counting hidden messages
    #[arg(
        long,
        value_name = "N",
        value_parser = non_zero_count,
        conflicts_with_all = ["anchor", "focus", "lines", "match_query"]
    )]
    pub tail: Option<NonZeroUsize>,
    /// Inclusive content line range within one message
    #[arg(long, value_name = "START..END", conflicts_with = "match_query")]
    pub lines: Option<MessageLineRange>,
    /// Find text within one message and return matching lines with context
    #[arg(long = "match", value_parser = non_empty_string, conflicts_with = "lines")]
    pub match_query: Option<String>,
    /// Content lines to include before and after each match
    #[arg(long, default_value_t = 3, requires = "match_query")]
    pub context: usize,
    /// Report time per stage, what was read, and peak memory on stderr
    #[arg(long)]
    pub perf_stats: bool,
    #[command(flatten)]
    pub output: AgentOutputFlags,
}

#[derive(Debug, ClapArgs)]
pub struct AgentOutlineArgs {
    /// Conversation to outline: a ch_ handle, a session ID, or a transcript
    /// path, optionally followed by :mN, :mN..mM, or :sN for one sub-agent
    #[arg(value_parser = non_empty_string)]
    pub conversation: String,
    /// Report time per stage, what was read, and peak memory on stderr
    #[arg(long)]
    pub perf_stats: bool,
    #[command(flatten)]
    pub output: AgentOutputFlags,
}

// The session list's time window: a session is listed when one of its
// messages falls inside it.
//
// Kept as `//` rather than `///`: clap promotes a flattened struct's doc
// comment to the containing command's long help.
#[derive(Debug, Default, ClapArgs)]
pub struct TimeRangeArgs {
    /// Only sessions with a message this recent, as a duration or date (2d, 1d6h, 6mo, 2026-07-20)
    #[arg(long, value_name = "WHEN", group = "time_lower_bound")]
    pub since: Option<TimePoint>,
    /// Alias for --since
    #[arg(long, value_name = "WHEN", group = "time_lower_bound")]
    pub after: Option<TimePoint>,
    /// Only sessions with a message older than this duration or date
    #[arg(long, value_name = "WHEN")]
    pub before: Option<TimePoint>,
}

impl TimeRangeArgs {
    /// Resolve the flags into a filter, relative to `clock`'s current instant.
    pub fn resolve(&self, clock: &dyn Clock) -> std::result::Result<TimeFilter, TimeFilterError> {
        TimeFilter::resolve(
            self.since.as_ref(),
            self.after.as_ref(),
            self.before.as_ref(),
            clock,
        )
    }
}

// An agent command's time window: it searches, reads or outlines only the
// messages inside it.
#[derive(Debug, Default, ClapArgs)]
pub struct MessageTimeArgs {
    /// Only messages this recent, as a duration or date (2d, 1d6h, 6mo, 2026-07-20)
    #[arg(long, value_name = "WHEN", group = "time_lower_bound")]
    pub since: Option<TimePoint>,
    /// Alias for --since
    #[arg(long, value_name = "WHEN", group = "time_lower_bound")]
    pub after: Option<TimePoint>,
    /// Only messages older than this duration or date
    #[arg(long, value_name = "WHEN")]
    pub before: Option<TimePoint>,
}

impl MessageTimeArgs {
    /// Resolve the flags into a filter, relative to `clock`'s current instant.
    pub fn resolve(&self, clock: &dyn Clock) -> std::result::Result<TimeFilter, TimeFilterError> {
        TimeFilter::resolve(
            self.since.as_ref(),
            self.after.as_ref(),
            self.before.as_ref(),
            clock,
        )
    }
}

impl AgentSearchArgs {
    pub fn mode_override(&self) -> Option<SearchMode> {
        self.search_mode.override_value()
    }
}

impl AgentWithinArgs {
    pub fn mode_override(&self) -> Option<SearchMode> {
        self.search_mode.override_value()
    }
}

#[derive(Parser, Debug)]
#[command(name = crate::APP_NAME)]
#[command(version)]
#[command(about = "View Claude conversation history")]
#[command(args_conflicts_with_subcommands = true)]
#[command(group(
    ArgGroup::new("one_shot_action")
        .args([
            "show_dir",
            "show_all_dirs",
            "delete",
            "debug_search",
            "debug_semantic_search",
            "semantic_search",
            "generate_semantic_cache",
            "clear_semantic_cache",
            "report_dropped_records",
            "render",
            "input_file",
        ])
        .multiple(false)
))]
#[command(group(
    ArgGroup::new("selection_conflicting_action")
        .args([
            "delete",
            "debug_search",
            "debug_semantic_search",
            "semantic_search",
            "generate_semantic_cache",
            "clear_semantic_cache",
            "report_dropped_records",
            "input_file",
        ])
        .multiple(false)
        .conflicts_with_all(["resume", "show_path", "show_id", "plain"])
))]
pub struct Args {
    #[command(subcommand)]
    pub command: Option<Commands>,

    /// Show tool calls in the conversation output
    #[arg(long, short = 't', group = "tools_display")]
    pub show_tools: bool,

    /// Hide tool calls from the conversation output
    #[arg(long, group = "tools_display")]
    pub no_tools: bool,

    /// Print where the agent stores sessions for the current directory or its
    /// repository, and exit
    #[arg(
        long,
        short = 'd',
        value_name = "AGENT",
        value_parser = SessionsDirAgentParser,
        hide_possible_values = true,
        help = show_dir_help()
    )]
    pub show_dir: Option<SessionsDirAgent>,

    /// Print "agent: directory" for each agent storing sessions for the
    /// current directory or its repository, and exit
    #[arg(long)]
    pub show_all_dirs: bool,

    /// Show the last messages in the TUI preview (default)
    #[arg(long, short = 'l', group = "preview_content")]
    pub last: bool,

    /// Show the first messages in the TUI preview
    #[arg(long, group = "preview_content")]
    pub first: bool,

    /// Show thinking blocks and subagent internals in the conversation output
    #[arg(long, group = "thinking_display")]
    pub show_thinking: bool,

    /// Hide thinking blocks and subagent internals from the conversation output
    #[arg(long, group = "thinking_display")]
    pub hide_thinking: bool,

    /// Resume the selected session in the agent that recorded it
    #[arg(
        long,
        short = 'c',
        help = "Resume the selected session in the agent that recorded it"
    )]
    pub resume: bool,

    /// Fork the session when resuming (creates a new session branching from the original)
    #[arg(long, help = "Fork the session when resuming", requires = "resume")]
    pub fork_session: bool,

    /// Print the selected conversation's file path and exit
    #[arg(long, short = 'p', help = "Print the selected conversation file path")]
    pub show_path: bool,

    /// Print the selected conversation's session ID and exit
    #[arg(long, short = 'i', help = "Print the selected conversation session ID")]
    pub show_id: bool,

    /// Output in plain text format without ledger formatting (for piping to other tools)
    #[arg(long, help = "Output plain text without ledger formatting")]
    pub plain: bool,

    /// Show debug output for conversation loading
    #[arg(
        long,
        value_name = "LEVEL",
        default_missing_value = "debug",
        num_args = 0..=1,
        help = "Print debug information (optionally filter by level: debug, info, warn, error)"
    )]
    pub debug: Option<DebugLevel>,

    /// Deprecated: global is now the default behavior
    #[arg(long, short = 'g', hide = true)]
    pub global: bool,

    /// Show only sessions recorded in the current repository, or, outside
    /// one, the current directory
    #[arg(long, short = 'L')]
    pub local: bool,

    #[command(flatten)]
    pub time: TimeRangeArgs,

    /// Display output through a pager (less)
    #[arg(long, group = "pager_display")]
    pub pager: bool,

    /// Disable pager output
    #[arg(long, group = "pager_display")]
    pub no_pager: bool,

    /// Render a JSONL file in ledger format and exit (for debugging)
    #[arg(
        long,
        value_name = "FILE",
        help = "Render a JSONL file in ledger format and exit"
    )]
    pub render: Option<PathBuf>,

    /// Disable colored output (for --render)
    #[arg(long, help = "Disable colored output")]
    pub no_color: bool,

    /// Delete a session by its ID
    #[arg(
        long,
        value_name = "SESSION_ID",
        help = "Delete a session by its ID and exit",
        conflicts_with = "global"
    )]
    pub delete: Option<String>,

    /// Debug search scoring for a query
    #[arg(
        long = "debug-search",
        value_name = "QUERY",
        help = "Debug search result scoring for a query"
    )]
    pub debug_search: Option<String>,

    /// Debug semantic search matching for a query
    #[arg(
        long = "debug-semantic-search",
        value_name = "QUERY",
        value_parser = non_empty_string,
        hide = true
    )]
    pub debug_semantic_search: Option<String>,

    /// Run a local semantic search over conversations
    #[arg(
        long = "semantic-search",
        value_name = "QUERY",
        help = "Run a local semantic search over conversations",
        value_parser = non_empty_string,
        hide = true
    )]
    pub semantic_search: Option<String>,

    /// Number of semantic search results to show
    #[arg(
        long = "semantic-top",
        default_value_t = 20,
        value_parser = non_zero_usize,
        hide = true,
        requires = "semantic_search"
    )]
    pub semantic_top: usize,

    /// Generate the semantic embedding cache and exit
    #[arg(long = "generate-semantic-cache", hide = true)]
    pub generate_semantic_cache: bool,

    /// Clear the semantic embedding cache, keeping the model, and exit
    #[arg(long = "clear-semantic-cache", hide = true)]
    pub clear_semantic_cache: bool,

    /// Print, per agent and agent version, the record kinds each reader drops
    /// (skipped, unrecognised or failed to parse), and exit. For a maintainer
    /// reviewing an agent's new releases.
    #[arg(long = "report-dropped-records", hide = true)]
    pub report_dropped_records: bool,

    /// Input JSONL file to view directly (skips conversation selection)
    #[arg(
        value_name = "FILE",
        help = "JSONL conversation file to view directly",
        conflicts_with_all = ["global", "local"]
    )]
    pub input_file: Option<PathBuf>,
}

/// An agent `--show-dir` accepts: one declaring `workspace_sessions_dir`
/// Capable, named as its provider names itself.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SessionsDirAgent(pub Source);

/// Every [`SessionsDirAgent`], in registration order.
static SESSIONS_DIR_AGENTS: LazyLock<Vec<SessionsDirAgent>> = LazyLock::new(|| {
    providers()
        .iter()
        .filter(|provider| provider.workspace_sessions_dir().is_capable())
        .map(|provider| SessionsDirAgent(provider.source()))
        .collect()
});

/// The [`SessionsDirAgent`] names, as `--show-dir`'s help and errors list
/// them.
fn sessions_dir_agent_names() -> String {
    SESSIONS_DIR_AGENTS
        .iter()
        .map(|agent| agent.0.label())
        .collect::<Vec<_>>()
        .join(", ")
}

fn show_dir_help() -> String {
    format!(
        "Print where the agent stores sessions for the current directory or its repository, \
         and exit. Only supported for: {}",
        sessions_dir_agent_names()
    )
}

/// Parses `--show-dir`'s value into a [`SessionsDirAgent`]. A value naming
/// no such agent fails with the agents `--show-dir` supports.
#[derive(Clone)]
struct SessionsDirAgentParser;

impl TypedValueParser for SessionsDirAgentParser {
    type Value = SessionsDirAgent;

    fn parse_ref(
        &self,
        cmd: &Command,
        _arg: Option<&Arg>,
        value: &OsStr,
    ) -> std::result::Result<SessionsDirAgent, clap::Error> {
        let name = value.to_string_lossy();
        if let Some(agent) = SESSIONS_DIR_AGENTS
            .iter()
            .find(|agent| agent.0.label() == name)
        {
            return Ok(*agent);
        }
        let refusal = match providers()
            .iter()
            .find(|provider| provider.labels().name == name)
        {
            Some(provider) => format!(
                "{} does not store sessions per directory",
                provider.labels().display
            ),
            None => format!("unknown agent '{name}'"),
        };
        let message = format!(
            "{refusal}. --show-dir is only supported for: {}",
            sessions_dir_agent_names()
        );
        Err(clap::Error::raw(ErrorKind::InvalidValue, message).format(&mut cmd.clone()))
    }

    /// The supported agents, for shell completion.
    fn possible_values(&self) -> Option<Box<dyn Iterator<Item = PossibleValue> + '_>> {
        Some(Box::new(
            SESSIONS_DIR_AGENTS
                .iter()
                .map(|agent| PossibleValue::new(agent.0.label())),
        ))
    }
}

fn non_empty_string(value: &str) -> std::result::Result<String, String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        Err("value must not be empty".to_string())
    } else {
        Ok(trimmed.to_string())
    }
}

fn non_zero_usize(value: &str) -> std::result::Result<usize, String> {
    non_zero_count(value).map(NonZeroUsize::get)
}

fn non_zero_count(value: &str) -> std::result::Result<NonZeroUsize, String> {
    let parsed = value
        .parse::<usize>()
        .map_err(|_| format!("invalid positive integer: {value}"))?;
    NonZeroUsize::new(parsed).ok_or_else(|| "value must be greater than zero".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::TestClock;
    use clap::CommandFactory;

    /// The agent subcommand `args` parsed to.
    fn agent_command(args: Args) -> AgentCommand {
        match args.command.unwrap() {
            Commands::Agent { command } => *command,
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn delete_empty_defaults_to_dry_run_all_workspaces() {
        let args = Args::try_parse_from(["rearview", "delete-empty"]).unwrap();

        match args.command.unwrap() {
            Commands::DeleteEmpty { yes, local, all } => {
                assert!(!yes);
                assert!(!local);
                assert!(!all);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn delete_empty_captures_delete_and_local_flags() {
        let args = Args::try_parse_from(["rearview", "delete-empty", "--yes", "--local"]).unwrap();

        match args.command.unwrap() {
            Commands::DeleteEmpty { yes, local, all } => {
                assert!(yes);
                assert!(local);
                assert!(!all);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn agent_search_captures_time_range_flags() {
        let args = Args::try_parse_from([
            "rearview",
            "agent",
            "search",
            "cache warming",
            "--since",
            "2d",
            "--before",
            "2026-07-20",
        ])
        .unwrap();

        match agent_command(args) {
            AgentCommand::Search(search) => {
                assert!(search.time.since.is_some());
                assert!(search.time.after.is_none());
                assert!(search.time.before.is_some());
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn agent_within_read_and_outline_accept_time_window() {
        let window = [
            "--after",
            "2026-10-14T11:36",
            "--before",
            "2026-10-14T11:46",
        ];
        let parse = |command: &[&str]| {
            let mut args = vec!["rearview", "agent"];
            args.extend_from_slice(command);
            args.extend_from_slice(&window);
            agent_command(Args::try_parse_from(args).unwrap())
        };

        let times = [
            match parse(&["within", "ch_abc123", "cache"]) {
                AgentCommand::Within(within) => within.time,
                other => panic!("unexpected command: {other:?}"),
            },
            match parse(&["read", "ch_abc123"]) {
                AgentCommand::Read(read) => read.output.time,
                other => panic!("unexpected command: {other:?}"),
            },
            match parse(&["outline", "ch_abc123"]) {
                AgentCommand::Outline(outline) => outline.output.time,
                other => panic!("unexpected command: {other:?}"),
            },
        ];

        for time in times {
            let filter = time.resolve(&TestClock::default()).unwrap();
            assert!(
                filter.after.is_some() && filter.before.is_some(),
                "{time:?}"
            );
        }
    }

    #[test]
    fn agent_sessions_accepts_window_page_and_scope() {
        let args = Args::try_parse_from([
            "rearview",
            "agent",
            "sessions",
            "--after",
            "2026-10-14T11:38",
            "--before",
            "2026-10-14T11:45",
            "--limit",
            "5",
            "--offset",
            "10",
            "--local",
        ])
        .unwrap();

        match agent_command(args) {
            AgentCommand::Sessions(sessions) => {
                let window = sessions.time.resolve(&TestClock::default()).unwrap();
                assert!(window.after.is_some() && window.before.is_some());
                assert_eq!(sessions.limit, Some(5));
                assert_eq!(sessions.offset, 10);
                assert!(sessions.scope.local);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn agent_sessions_accepts_n_as_its_limit() {
        let args = Args::try_parse_from(["rearview", "agent", "sessions", "-n", "3"]).unwrap();

        match agent_command(args) {
            AgentCommand::Sessions(sessions) => assert_eq!(sessions.limit, Some(3)),
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn agent_sessions_refuses_local_with_all_and_since_with_after() {
        assert!(
            Args::try_parse_from(["rearview", "agent", "sessions", "--local", "--all"]).is_err()
        );
        assert!(
            Args::try_parse_from([
                "rearview", "agent", "sessions", "--since", "2d", "--after", "1d"
            ])
            .is_err()
        );
    }

    #[test]
    fn top_level_time_range_flags_are_accepted() {
        let args = Args::try_parse_from(["rearview", "--since", "1w"]).unwrap();
        let clock = TestClock::default();
        let week_ago = clock.wall() - chrono::Duration::weeks(1);

        assert!(args.time.since.is_some());
        let filter = args.time.resolve(&clock).unwrap();
        assert_eq!(filter.after, Some(week_ago.with_timezone(&clock.zone())));
        assert_eq!(filter.before, None);
    }

    #[test]
    fn absent_time_range_flags_resolve_to_inactive_filter() {
        let args = Args::try_parse_from(["rearview"]).unwrap();
        assert_eq!(
            args.time.resolve(&TestClock::default()).unwrap(),
            TimeFilter::default()
        );
    }

    #[test]
    fn since_and_after_are_mutually_exclusive() {
        assert!(Args::try_parse_from(["rearview", "--since", "2d", "--after", "3d"]).is_err());
        assert!(
            Args::try_parse_from([
                "rearview", "agent", "search", "needle", "--since", "2d", "--after", "3d",
            ])
            .is_err()
        );
    }

    #[test]
    fn since_and_before_can_be_combined() {
        // Absolute on both ends so the assertion does not depend on today's
        // date the way a relative lower bound would.
        let args = Args::try_parse_from([
            "rearview",
            "--since",
            "2026-01-01",
            "--before",
            "2026-07-20",
        ])
        .unwrap();

        let filter = args.time.resolve(&TestClock::default()).unwrap();
        assert!(filter.after.is_some());
        assert!(filter.before.is_some());
    }

    #[test]
    fn invalid_time_value_is_rejected_at_parse_time() {
        let error = Args::try_parse_from(["rearview", "--since", "30x"])
            .expect_err("30x is not a valid span");

        // Proves the bare FromStr impl's Display reaches the user rather than a
        // generic clap "invalid value" message.
        assert!(
            error.to_string().contains("unknown unit 'x'"),
            "unhelpful error: {error}"
        );
    }

    #[test]
    fn inverted_time_range_is_rejected_at_resolve_time() {
        // clap accepts each value on its own; only resolving the pair catches
        // the contradiction.
        let args = Args::try_parse_from([
            "rearview",
            "--after",
            "2026-07-20",
            "--before",
            "2026-01-01",
        ])
        .unwrap();

        assert!(args.time.resolve(&TestClock::default()).is_err());
    }

    #[test]
    fn agent_search_captures_query_top_scope_and_mode_override() {
        let args = Args::try_parse_from([
            "rearview",
            "agent",
            "search",
            "cache warming",
            "--hybrid",
            "--top",
            "7",
            "--local",
        ])
        .unwrap();

        match agent_command(args) {
            AgentCommand::Search(search) => {
                assert_eq!(search.query, "cache warming");
                assert_eq!(search.top, Some(7));
                assert!(!search.flat);
                assert_eq!(search.hits_per_conv, None);
                assert!(!search.all_hits);
                assert!(search.scope.local);
                assert!(!search.scope.all);
                assert_eq!(search.mode_override(), Some(SearchMode::Hybrid));
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn agent_search_defaults_to_global_scope_top_ten_and_config_mode() {
        let args = Args::try_parse_from(["rearview", "agent", "search", "cache"]).unwrap();

        match agent_command(args) {
            AgentCommand::Search(search) => {
                assert_eq!(search.top, None);
                assert!(!search.flat);
                assert_eq!(search.hits_per_conv, None);
                assert!(!search.all_hits);
                assert!(!search.scope.local);
                assert!(!search.scope.all);
                assert_eq!(search.mode_override(), None);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn agent_search_captures_browsing_flags() {
        let args = Args::try_parse_from([
            "rearview",
            "agent",
            "search",
            "cache",
            "--flat",
            "--hits-per-conv",
            "5",
            "--all-hits",
        ])
        .unwrap();

        match agent_command(args) {
            AgentCommand::Search(search) => {
                assert!(search.flat);
                assert_eq!(search.hits_per_conv, Some(5));
                assert!(search.all_hits);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn agent_search_help_explains_grouped_and_flat_top_semantics() {
        let mut command = Args::command();
        let search = command
            .find_subcommand_mut("agent")
            .unwrap()
            .find_subcommand_mut("search")
            .unwrap();
        let help = search.render_long_help().to_string();

        assert!(help.contains("Maximum conversations, or message hits with --flat"));
        assert!(help.contains("Rank message hits across conversations; --top counts hits"));
        assert!(help.contains("Combine lexical and semantic message ranking"));
        assert!(help.contains("Match the whole query as one phrase"));
        assert!(help.contains("spacing and line breaks need not match"));
    }

    #[test]
    fn agent_help_documents_anchor_reads() {
        let mut command = Args::command();
        let agent = command.find_subcommand_mut("agent").unwrap();
        let read_help = agent
            .find_subcommand_mut("read")
            .unwrap()
            .render_long_help()
            .to_string();
        assert!(read_help.contains("--anchor"));
        assert!(read_help.contains("Durable message anchor"));
    }

    #[test]
    fn agent_within_rejects_search_browsing_flags() {
        let err = Args::try_parse_from([
            "rearview",
            "agent",
            "within",
            "ch_abc123",
            "cache",
            "--flat",
        ])
        .expect_err("search-only flag should fail for within");

        assert!(err.to_string().contains("unexpected argument '--flat'"));
    }

    #[test]
    fn agent_within_captures_ref_query_top_and_mode_override() {
        let args = Args::try_parse_from([
            "rearview",
            "agent",
            "within",
            "ch_abc123",
            "cache",
            "--semantic",
            "--top",
            "4",
        ])
        .unwrap();

        match agent_command(args) {
            AgentCommand::Within(within) => {
                assert_eq!(within.conversation, "ch_abc123");
                assert_eq!(within.query, "cache");
                assert_eq!(within.top, Some(4));
                assert_eq!(within.mode_override(), Some(SearchMode::Semantic));
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn agent_read_captures_refs_and_display_options() {
        let args = Args::try_parse_from([
            "rearview",
            "agent",
            "read",
            "ch_abc123:m1..m3",
            "ch_def456:m4",
            "--focus",
            "m2",
            "--budget",
            "1234",
            "--tools",
            "--tool-results",
            "--thinking",
            "--subagents",
        ])
        .unwrap();

        match agent_command(args) {
            AgentCommand::Read(read) => {
                assert_eq!(read.refs, vec!["ch_abc123:m1..m3", "ch_def456:m4"]);
                assert_eq!(read.anchor, None);
                assert_eq!(read.focus.as_deref(), Some("m2"));
                assert_eq!(read.lines, None);
                assert_eq!(read.match_query, None);
                assert_eq!(read.context, 3);
                assert_eq!(read.output.budget, Some(1234));
                assert!(!read.output.no_budget);
                assert!(read.output.tools);
                assert!(read.output.tool_results);
                assert!(read.output.thinking);
                assert!(read.output.subagents);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn agent_read_defaults_to_budgeted_minimal_output() {
        let args = Args::try_parse_from(["rearview", "agent", "read", "ch_abc123"]).unwrap();

        match agent_command(args) {
            AgentCommand::Read(read) => {
                assert_eq!(read.output.budget, None);
                assert!(!read.output.no_budget);
                assert!(!read.output.tools);
                assert!(!read.output.tool_results);
                assert!(!read.output.thinking);
                assert!(!read.output.subagents);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn agent_read_captures_message_slice_options() {
        let lines = Args::try_parse_from([
            "rearview",
            "agent",
            "read",
            "ch_abc123:m7",
            "--lines",
            "40..120",
        ])
        .unwrap();
        match agent_command(lines) {
            AgentCommand::Read(read) => assert_eq!(
                read.lines,
                Some(MessageLineRange {
                    start: 40,
                    end: 120
                })
            ),
            other => panic!("unexpected command: {other:?}"),
        }

        let matching = Args::try_parse_from([
            "rearview",
            "agent",
            "read",
            "ch_abc123:m7",
            "--match",
            "historical correction",
            "--context",
            "12",
        ])
        .unwrap();
        match agent_command(matching) {
            AgentCommand::Read(read) => {
                assert_eq!(read.match_query.as_deref(), Some("historical correction"));
                assert_eq!(read.context, 12);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn agent_read_slice_options_validate_arguments() {
        let conflict = Args::try_parse_from([
            "rearview",
            "agent",
            "read",
            "ch_abc123:m7",
            "--lines",
            "1..2",
            "--match",
            "needle",
        ])
        .expect_err("line and match slices should conflict");
        assert!(conflict.to_string().contains("cannot be used with"));

        let missing_match = Args::try_parse_from([
            "rearview",
            "agent",
            "read",
            "ch_abc123:m7",
            "--context",
            "2",
        ])
        .expect_err("context should require a match query");
        assert!(missing_match.to_string().contains("required"));

        let invalid_lines = Args::try_parse_from([
            "rearview",
            "agent",
            "read",
            "ch_abc123:m7",
            "--lines",
            "4..2",
        ])
        .expect_err("reversed line range should fail");
        assert!(invalid_lines.to_string().contains("line range"));
    }

    #[test]
    fn agent_read_tail_accepts_nonzero_count_and_conflicts_with_anchor_focus_lines_and_match() {
        let parse_read = |extra: &[&str]| {
            Args::try_parse_from(
                ["rearview", "agent", "read", "ch_abc123"]
                    .iter()
                    .chain(extra)
                    .copied(),
            )
        };

        match agent_command(parse_read(&["--tail", "20"]).unwrap()) {
            AgentCommand::Read(read) => assert_eq!(read.tail.map(NonZeroUsize::get), Some(20)),
            other => panic!("unexpected command: {other:?}"),
        }
        assert!(parse_read(&["--tail", "0"]).is_err());
        for other in [
            &["--anchor", "ma_1234"][..],
            &["--focus", "m2"],
            &["--lines", "1..2"],
            &["--match", "needle"],
        ] {
            let error = parse_read(&[&["--tail", "5"][..], other].concat())
                .expect_err("--tail conflicts with --anchor, --focus, --lines and --match");
            assert!(error.to_string().contains("cannot be used with"), "{error}");
        }
    }

    #[test]
    fn agent_outline_captures_ref_and_display_options() {
        let args = Args::try_parse_from([
            "rearview",
            "agent",
            "outline",
            "ch_abc123",
            "--no-budget",
            "--tools",
            "--tool-results",
            "--thinking",
            "--subagents",
        ])
        .unwrap();

        match agent_command(args) {
            AgentCommand::Outline(outline) => {
                assert_eq!(outline.conversation, "ch_abc123");
                assert_eq!(outline.output.budget, None);
                assert!(outline.output.no_budget);
                assert!(outline.output.tools);
                assert!(outline.output.tool_results);
                assert!(outline.output.thinking);
                assert!(outline.output.subagents);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn agent_mode_value_and_legacy_aliases_share_one_override() {
        let canonical =
            Args::try_parse_from(["rearview", "agent", "search", "cache", "--mode", "hybrid"])
                .unwrap();
        let legacy = Args::try_parse_from([
            "rearview",
            "agent",
            "within",
            "ch_abc123",
            "cache",
            "--hybrid",
        ])
        .unwrap();

        match agent_command(canonical) {
            AgentCommand::Search(search) => {
                assert_eq!(search.mode_override(), Some(SearchMode::Hybrid))
            }
            other => panic!("unexpected command: {other:?}"),
        }
        match agent_command(legacy) {
            AgentCommand::Within(within) => {
                assert_eq!(within.mode_override(), Some(SearchMode::Hybrid))
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn agent_mode_value_conflicts_with_legacy_alias() {
        let err = Args::try_parse_from([
            "rearview", "agent", "search", "cache", "--mode", "semantic", "--hybrid",
        ])
        .expect_err("canonical and legacy modes should conflict");

        assert!(err.to_string().contains("cannot be used with"));
    }

    #[test]
    fn top_level_one_shot_actions_share_one_conflict_group() {
        let cases = [
            vec!["rearview", "--show-dir", "claude", "--delete", "session"],
            vec!["rearview", "--show-dir", "claude", "--show-all-dirs"],
            vec![
                "rearview",
                "--debug-search",
                "cache",
                "--render",
                "session.jsonl",
            ],
            vec![
                "rearview",
                "--generate-semantic-cache",
                "--clear-semantic-cache",
            ],
        ];

        for args in cases {
            let err = Args::try_parse_from(args).expect_err("one-shot actions should conflict");
            assert!(err.to_string().contains("cannot be used with"));
        }
    }

    #[test]
    fn show_dir_takes_agent_name() {
        let args = Args::try_parse_from(["rearview", "-d", "claude"]).unwrap();

        assert_eq!(args.show_dir, Some(SessionsDirAgent(Source::Claude)));
    }

    #[test]
    fn show_dir_without_agent_fails() {
        let error = Args::try_parse_from(["rearview", "--show-dir"]).unwrap_err();

        assert_eq!(error.kind(), clap::error::ErrorKind::InvalidValue);
    }

    /// The first line of `--show-dir <value>`'s error, which must keep clap's
    /// usage line, `--help` hint and exit code.
    fn show_dir_error(value: &str) -> String {
        let error = Args::try_parse_from(["rearview", "--show-dir", value]).unwrap_err();
        assert_eq!(error.kind(), clap::error::ErrorKind::InvalidValue);
        assert_eq!(error.exit_code(), 2);
        let message = error.to_string();
        assert!(message.contains("\nUsage: "), "{message}");
        assert!(
            message.contains("For more information, try '--help'."),
            "{message}"
        );
        message.lines().next().unwrap().to_owned()
    }

    #[test]
    fn show_dir_refuses_agent_without_sessions_per_directory_by_name() {
        assert_eq!(
            show_dir_error("codex"),
            "error: Codex does not store sessions per directory. --show-dir is only \
             supported for: claude, kimi, grok, pi, omp"
        );
        assert_eq!(
            show_dir_error("opencode"),
            "error: OpenCode does not store sessions per directory. --show-dir is only \
             supported for: claude, kimi, grok, pi, omp"
        );
    }

    #[test]
    fn show_dir_refuses_unknown_agent() {
        assert_eq!(
            show_dir_error("cursor"),
            "error: unknown agent 'cursor'. --show-dir is only supported for: claude, kimi, \
             grok, pi, omp"
        );
    }

    #[test]
    fn show_dir_help_lists_supported_agents() {
        let help = Args::command().render_help().to_string();

        assert!(
            help.contains(
                "Print where the agent stores sessions for the current directory or its \
                 repository, and exit. Only supported for: claude, kimi, grok, pi, omp"
            ),
            "{help}"
        );
        assert!(!help.contains("[possible values"), "{help}");
    }

    #[test]
    fn display_one_shots_keep_compatible_selection_modifiers() {
        assert!(Args::try_parse_from(["rearview", "--show-dir", "claude", "--plain"]).is_ok());
        assert!(
            Args::try_parse_from(["rearview", "--render", "session.jsonl", "--resume",]).is_ok()
        );
    }

    #[test]
    fn agent_mode_flags_conflict() {
        let err = Args::try_parse_from([
            "rearview",
            "agent",
            "search",
            "cache",
            "--semantic",
            "--hybrid",
        ])
        .expect_err("conflicting mode flags should fail");

        assert!(err.to_string().contains("cannot be used with"));
    }

    #[test]
    fn agent_budget_and_no_budget_conflict() {
        let err = Args::try_parse_from([
            "rearview",
            "agent",
            "read",
            "ch_abc123",
            "--budget",
            "10",
            "--no-budget",
        ])
        .expect_err("budget and no-budget should conflict");

        assert!(err.to_string().contains("cannot be used with"));
    }

    #[test]
    fn agent_top_must_be_positive() {
        let err = Args::try_parse_from([
            "rearview",
            "agent",
            "within",
            "ch_abc123",
            "cache",
            "--top",
            "0",
        ])
        .expect_err("zero top should fail");

        assert!(err.to_string().contains("value must be greater than zero"));
    }

    #[test]
    fn agent_budget_must_be_positive() {
        let err =
            Args::try_parse_from(["rearview", "agent", "outline", "ch_abc123", "--budget", "0"])
                .expect_err("zero budget should fail");

        assert!(err.to_string().contains("value must be greater than zero"));
    }

    #[test]
    fn agent_query_must_not_be_empty() {
        let err = Args::try_parse_from(["rearview", "agent", "search", "   "])
            .expect_err("empty query should fail");

        assert!(err.to_string().contains("value must not be empty"));
    }

    #[test]
    fn semantic_query_must_not_be_empty() {
        let err = Args::try_parse_from(["rearview", "--semantic-search", "   "])
            .expect_err("empty query should fail");

        assert!(err.to_string().contains("value must not be empty"));
    }

    #[test]
    fn semantic_top_must_be_positive() {
        let err = Args::try_parse_from([
            "rearview",
            "--semantic-search",
            "cache",
            "--semantic-top",
            "0",
        ])
        .expect_err("zero top should fail");

        assert!(err.to_string().contains("value must be greater than zero"));
    }

    #[test]
    fn agent_budget_help_uses_character_units() {
        let mut command = Args::command();
        let help = command
            .find_subcommand_mut("agent")
            .unwrap()
            .find_subcommand_mut("read")
            .unwrap()
            .render_long_help()
            .to_string();
        assert!(help.contains("Output budget in Unicode characters"));
        assert!(!help.contains("approximate tokens"));
    }

    /// The code blocks of `markdown`, which is where the README shows a
    /// command rather than merely mentioning one.
    #[cfg(test)]
    fn code_examples(markdown: &str) -> String {
        use pulldown_cmark::{Event, Parser as Markdown, Tag, TagEnd};

        let mut examples = String::new();
        let mut inside = false;
        for event in Markdown::new(markdown) {
            match event {
                Event::Start(Tag::CodeBlock(_)) => inside = true,
                Event::End(TagEnd::CodeBlock) => inside = false,
                Event::Text(text) if inside => examples.push_str(&text),
                _ => {}
            }
        }
        examples
    }

    /// Every subcommand a user can reach has to be shown being run. Which
    /// section shows it is the README's business — it was one CLI reference
    /// and is now a section per task — so this asks the whole file, and asks
    /// for a runnable example rather than a passing mention.
    #[test]
    fn readme_shows_every_visible_subcommand_being_run() {
        let examples = code_examples(include_str!("../README.md"));

        for subcommand in Args::command()
            .get_subcommands()
            .filter(|subcommand| !subcommand.is_hide_set())
        {
            let invocation = format!("rearview {}", subcommand.get_name());
            assert!(
                examples.contains(&invocation),
                "no README example runs `{invocation}`"
            );
        }
    }

    #[test]
    fn prose_around_example_is_not_mistaken_for_one() {
        let markdown = concat!(
            "Run `rearview update` to update.\n\n",
            "```sh\nrearview agent search x\n```\n\n",
            "- In a list:\n\n  ```sh\n  rearview delete-empty\n  ```\n"
        );

        let examples = code_examples(markdown);

        assert!(examples.contains("rearview agent search x"), "{examples:?}");
        assert!(
            examples.contains("rearview delete-empty"),
            "a block indented inside a list is still an example: {examples:?}"
        );
        assert!(
            !examples.contains("rearview update"),
            "a command named in prose is not an example of running it"
        );
    }

    #[test]
    fn semantic_help_text_is_polished_when_available() {
        let help = Args::command().render_long_help().to_string();

        assert!(!help.contains("proof of concept"));
        assert!(!help.contains("POC"));
    }

    #[test]
    fn default_help_hides_semantic_flags() {
        let help = Args::command().render_long_help().to_string();

        assert!(!help.contains("--semantic-search"));
        assert!(!help.contains("--semantic-top"));
        assert!(!help.contains("--generate-semantic-cache"));
        assert!(!help.contains("--clear-semantic-cache"));
        assert!(!help.contains("--debug-semantic-search"));
    }

    #[test]
    fn debug_semantic_search_flag_is_parseable() {
        let args = Args::try_parse_from(["rearview", "--debug-semantic-search", "cache"]).unwrap();

        assert_eq!(args.debug_semantic_search.as_deref(), Some("cache"));
    }

    #[test]
    fn generate_semantic_cache_flag_is_parseable() {
        let args = Args::try_parse_from(["rearview", "--generate-semantic-cache"]).unwrap();

        assert!(args.generate_semantic_cache);
    }

    #[test]
    fn clear_semantic_cache_flag_is_parseable() {
        let args = Args::try_parse_from(["rearview", "--clear-semantic-cache"]).unwrap();

        assert!(args.clear_semantic_cache);
    }

    #[test]
    fn report_dropped_records_is_parseable_and_kept_out_of_help() {
        let args = Args::try_parse_from(["rearview", "--report-dropped-records"]).unwrap();
        let help = Args::command().render_long_help().to_string();

        assert!(args.report_dropped_records);
        assert!(!help.contains("--report-dropped-records"));
    }

    #[test]
    fn report_dropped_records_runs_alone() {
        assert!(
            Args::try_parse_from(["rearview", "--report-dropped-records", "--show-id"]).is_err()
        );
    }

    #[test]
    fn semantic_flag_is_removed() {
        let err =
            Args::try_parse_from(["rearview", "--semantic"]).expect_err("removed flag should fail");

        assert!(err.to_string().contains("unexpected argument '--semantic'"));
    }
}
