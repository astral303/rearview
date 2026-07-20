use crate::agent::protocol::MessageLineRange;
use crate::search::mode::SearchMode;
use clap::{Args as ClapArgs, Parser, Subcommand};
use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;

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
        command: AgentCommand,
    },
    /// Delete transcript files with no Claude messages
    DeleteEmpty {
        /// Delete matching transcripts instead of printing a dry run
        #[arg(long)]
        yes: bool,
        /// Only inspect the current workspace
        #[arg(long, group = "delete_empty_scope")]
        local: bool,
        /// Inspect all workspaces
        #[arg(long, group = "delete_empty_scope")]
        all: bool,
    },
    /// Update claude-history to the latest version
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
    /// Search across all conversations
    Search(AgentSearchArgs),
    /// Search within one conversation
    Within(AgentWithinArgs),
    /// Read transcript message ranges
    Read(AgentReadArgs),
    /// Outline a conversation transcript
    Outline(AgentOutlineArgs),
}

#[derive(Debug, ClapArgs)]
#[command(group = clap::ArgGroup::new("agent_search_mode").args(["lexical", "semantic", "exact", "hybrid"]).multiple(false))]
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
    /// Search only the current workspace
    #[arg(long, group = "agent_search_scope")]
    pub local: bool,
    /// Search all workspaces
    #[arg(long, group = "agent_search_scope")]
    pub all: bool,
    /// Use lexical search for this invocation
    #[arg(long, group = "agent_search_mode")]
    pub lexical: bool,
    /// Use semantic search for this invocation
    #[arg(long, group = "agent_search_mode")]
    pub semantic: bool,
    /// Use exact search for this invocation
    #[arg(long, group = "agent_search_mode")]
    pub exact: bool,
    /// Combine lexical and semantic message ranking
    #[arg(long, group = "agent_search_mode")]
    pub hybrid: bool,
}

#[derive(Debug, ClapArgs)]
#[command(group = clap::ArgGroup::new("agent_within_mode").args(["lexical", "semantic", "exact", "hybrid"]).multiple(false))]
pub struct AgentWithinArgs {
    /// Conversation reference
    #[arg(value_parser = non_empty_string)]
    pub conversation: String,
    /// Search query
    #[arg(value_parser = non_empty_string)]
    pub query: String,
    /// Maximum number of results
    #[arg(long, value_parser = non_zero_usize)]
    pub top: Option<usize>,
    /// Reject the search if the transcript content revision differs
    #[arg(long, value_parser = non_empty_string)]
    pub revision: Option<String>,
    /// Output budget in Unicode characters
    #[arg(long, value_parser = non_zero_usize, conflicts_with = "no_budget")]
    pub budget: Option<usize>,
    /// Disable output budgeting
    #[arg(long)]
    pub no_budget: bool,
    /// Use lexical search for this invocation
    #[arg(long, group = "agent_within_mode")]
    pub lexical: bool,
    /// Use semantic search for this invocation
    #[arg(long, group = "agent_within_mode")]
    pub semantic: bool,
    /// Use exact search for this invocation
    #[arg(long, group = "agent_within_mode")]
    pub exact: bool,
    /// Use hybrid search for this invocation
    #[arg(long, group = "agent_within_mode")]
    pub hybrid: bool,
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
}

#[derive(Debug, ClapArgs)]
pub struct AgentReadArgs {
    /// Conversation or message range refs to read
    #[arg(required = true, value_parser = non_empty_string)]
    pub refs: Vec<String>,
    /// Durable message anchor to read within one conversation
    #[arg(long, value_parser = non_empty_string)]
    pub anchor: Option<String>,
    /// Reject the read if the transcript content revision differs
    #[arg(long, value_parser = non_empty_string)]
    pub revision: Option<String>,
    /// Message or range to prioritize when budgeted output is truncated
    #[arg(long, value_parser = non_empty_string)]
    pub focus: Option<String>,
    /// Inclusive content line range within one message
    #[arg(long, value_name = "START..END", conflicts_with = "match_query")]
    pub lines: Option<MessageLineRange>,
    /// Find text within one message and return matching lines with context
    #[arg(long = "match", value_parser = non_empty_string, conflicts_with = "lines")]
    pub match_query: Option<String>,
    /// Content lines to include before and after each match
    #[arg(long, default_value_t = 3, requires = "match_query")]
    pub context: usize,
    #[command(flatten)]
    pub output: AgentOutputFlags,
}

#[derive(Debug, ClapArgs)]
pub struct AgentOutlineArgs {
    /// Conversation reference to outline
    #[arg(value_parser = non_empty_string)]
    pub conversation: String,
    #[command(flatten)]
    pub output: AgentOutputFlags,
}

impl AgentSearchArgs {
    pub fn mode_override(&self) -> Option<SearchMode> {
        agent_mode_override(self.lexical, self.semantic, self.exact, self.hybrid)
    }
}

impl AgentWithinArgs {
    pub fn mode_override(&self) -> Option<SearchMode> {
        agent_mode_override(self.lexical, self.semantic, self.exact, self.hybrid)
    }
}

fn agent_mode_override(
    lexical: bool,
    semantic: bool,
    exact: bool,
    hybrid: bool,
) -> Option<SearchMode> {
    if lexical {
        Some(SearchMode::Lexical)
    } else if semantic {
        Some(SearchMode::Semantic)
    } else if exact {
        Some(SearchMode::Exact)
    } else if hybrid {
        Some(SearchMode::Hybrid)
    } else {
        None
    }
}

#[derive(Parser, Debug)]
#[command(name = "claude-history")]
#[command(version)]
#[command(about = "View Claude conversation history")]
#[command(args_conflicts_with_subcommands = true)]
pub struct Args {
    #[command(subcommand)]
    pub command: Option<Commands>,

    /// Show tool calls in the conversation output
    #[arg(long, short = 't', group = "tools_display")]
    pub show_tools: bool,

    /// Hide tool calls from the conversation output
    #[arg(long, group = "tools_display")]
    pub no_tools: bool,

    /// Show the conversation directory and exit
    #[arg(
        long,
        short = 'd',
        help = "Print the conversation directory path and exit"
    )]
    pub show_dir: bool,

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

    /// Resume the selected conversation in the Claude CLI
    #[arg(
        long,
        short = 'c',
        help = "Resume the selected conversation in Claude Code"
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

    /// Show only conversations from the current workspace
    #[arg(
        long,
        short = 'L',
        help = "Show only conversations from the current workspace directory"
    )]
    pub local: bool,

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
        help = "Delete a session by its UUID and exit",
        conflicts_with_all = ["global", "show_dir", "resume", "show_path", "show_id", "plain", "render", "input_file"]
    )]
    pub delete: Option<String>,

    /// Debug search scoring for a query
    #[arg(
        long = "debug-search",
        value_name = "QUERY",
        help = "Debug search result scoring for a query",
        conflicts_with_all = ["show_dir", "resume", "show_path", "show_id", "plain", "render", "delete", "input_file", "semantic_search"]
    )]
    pub debug_search: Option<String>,

    /// Debug semantic search matching for a query
    #[arg(
        long = "debug-semantic-search",
        value_name = "QUERY",
        value_parser = non_empty_string,
        hide = true,
        conflicts_with_all = ["show_dir", "resume", "show_path", "show_id", "plain", "render", "delete", "input_file", "debug_search", "semantic_search", "generate_semantic_cache", "clear_semantic_cache"]
    )]
    pub debug_semantic_search: Option<String>,

    /// Run a local semantic search over conversations
    #[arg(
        long = "semantic-search",
        value_name = "QUERY",
        help = "Run a local semantic search over conversations",
        value_parser = non_empty_string,
        hide = true,
        conflicts_with_all = ["show_dir", "resume", "show_path", "show_id", "plain", "render", "delete", "input_file", "debug_search", "clear_semantic_cache"]
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
    #[arg(
        long = "generate-semantic-cache",
        hide = true,
        conflicts_with_all = ["show_dir", "resume", "show_path", "show_id", "plain", "render", "delete", "input_file", "debug_search", "semantic_search", "clear_semantic_cache"]
    )]
    pub generate_semantic_cache: bool,

    /// Clear semantic embedding and model cache files and exit
    #[arg(
        long = "clear-semantic-cache",
        hide = true,
        conflicts_with_all = ["show_dir", "resume", "show_path", "show_id", "plain", "render", "delete", "input_file", "debug_search", "semantic_search", "generate_semantic_cache", "debug_semantic_search"]
    )]
    pub clear_semantic_cache: bool,

    /// Input JSONL file to view directly (skips conversation selection)
    #[arg(
        value_name = "FILE",
        help = "JSONL conversation file to view directly",
        conflicts_with_all = ["global", "local", "show_dir", "resume", "show_path", "show_id", "plain", "render", "delete"]
    )]
    pub input_file: Option<PathBuf>,
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
    let parsed = value
        .parse::<usize>()
        .map_err(|_| format!("invalid positive integer: {value}"))?;
    if parsed == 0 {
        Err("value must be greater than zero".to_string())
    } else {
        Ok(parsed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn delete_empty_defaults_to_dry_run_all_workspaces() {
        let args = Args::try_parse_from(["claude-history", "delete-empty"]).unwrap();

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
        let args =
            Args::try_parse_from(["claude-history", "delete-empty", "--yes", "--local"]).unwrap();

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
    fn agent_search_captures_query_top_scope_and_mode_override() {
        let args = Args::try_parse_from([
            "claude-history",
            "agent",
            "search",
            "cache warming",
            "--hybrid",
            "--top",
            "7",
            "--local",
        ])
        .unwrap();

        match args.command.unwrap() {
            Commands::Agent {
                command: AgentCommand::Search(search),
            } => {
                assert_eq!(search.query, "cache warming");
                assert_eq!(search.top, Some(7));
                assert!(!search.flat);
                assert_eq!(search.hits_per_conv, None);
                assert!(!search.all_hits);
                assert!(search.local);
                assert!(!search.all);
                assert_eq!(search.mode_override(), Some(SearchMode::Hybrid));
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn agent_search_defaults_to_global_scope_top_ten_and_config_mode() {
        let args = Args::try_parse_from(["claude-history", "agent", "search", "cache"]).unwrap();

        match args.command.unwrap() {
            Commands::Agent {
                command: AgentCommand::Search(search),
            } => {
                assert_eq!(search.top, None);
                assert!(!search.flat);
                assert_eq!(search.hits_per_conv, None);
                assert!(!search.all_hits);
                assert!(!search.local);
                assert!(!search.all);
                assert_eq!(search.mode_override(), None);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn agent_search_captures_browsing_flags() {
        let args = Args::try_parse_from([
            "claude-history",
            "agent",
            "search",
            "cache",
            "--flat",
            "--hits-per-conv",
            "5",
            "--all-hits",
        ])
        .unwrap();

        match args.command.unwrap() {
            Commands::Agent {
                command: AgentCommand::Search(search),
            } => {
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
    }

    #[test]
    fn agent_help_documents_anchor_and_revision_guards() {
        let mut command = Args::command();
        let agent = command.find_subcommand_mut("agent").unwrap();
        let read_help = agent
            .find_subcommand_mut("read")
            .unwrap()
            .render_long_help()
            .to_string();
        assert!(read_help.contains("--anchor"));
        assert!(read_help.contains("Durable message anchor"));
        assert!(read_help.contains("--revision"));

        let within_help = agent
            .find_subcommand_mut("within")
            .unwrap()
            .render_long_help()
            .to_string();
        assert!(within_help.contains("--revision"));
        assert!(within_help.contains("transcript content revision"));
    }

    #[test]
    fn agent_within_rejects_search_browsing_flags() {
        let err = Args::try_parse_from([
            "claude-history",
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
            "claude-history",
            "agent",
            "within",
            "ch_abc123",
            "cache",
            "--semantic",
            "--top",
            "4",
        ])
        .unwrap();

        match args.command.unwrap() {
            Commands::Agent {
                command: AgentCommand::Within(within),
            } => {
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
            "claude-history",
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

        match args.command.unwrap() {
            Commands::Agent {
                command: AgentCommand::Read(read),
            } => {
                assert_eq!(read.refs, vec!["ch_abc123:m1..m3", "ch_def456:m4"]);
                assert_eq!(read.anchor, None);
                assert_eq!(read.revision, None);
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
    fn agent_read_and_within_capture_revision_guards_and_anchor() {
        let read = Args::try_parse_from([
            "claude-history",
            "agent",
            "read",
            "ch_abc123",
            "--anchor",
            "ma_1234567890abcdef",
            "--revision",
            "rv_1234567890abcdef",
        ])
        .unwrap();
        match read.command.unwrap() {
            Commands::Agent {
                command: AgentCommand::Read(read),
            } => {
                assert_eq!(read.anchor.as_deref(), Some("ma_1234567890abcdef"));
                assert_eq!(read.revision.as_deref(), Some("rv_1234567890abcdef"));
            }
            other => panic!("unexpected command: {other:?}"),
        }

        let within = Args::try_parse_from([
            "claude-history",
            "agent",
            "within",
            "ch_abc123",
            "needle",
            "--revision",
            "rv_1234567890abcdef",
        ])
        .unwrap();
        match within.command.unwrap() {
            Commands::Agent {
                command: AgentCommand::Within(within),
            } => assert_eq!(within.revision.as_deref(), Some("rv_1234567890abcdef")),
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn agent_read_defaults_to_budgeted_minimal_output() {
        let args = Args::try_parse_from(["claude-history", "agent", "read", "ch_abc123"]).unwrap();

        match args.command.unwrap() {
            Commands::Agent {
                command: AgentCommand::Read(read),
            } => {
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
            "claude-history",
            "agent",
            "read",
            "ch_abc123:m7",
            "--lines",
            "40..120",
        ])
        .unwrap();
        match lines.command.unwrap() {
            Commands::Agent {
                command: AgentCommand::Read(read),
            } => assert_eq!(
                read.lines,
                Some(MessageLineRange {
                    start: 40,
                    end: 120
                })
            ),
            other => panic!("unexpected command: {other:?}"),
        }

        let matching = Args::try_parse_from([
            "claude-history",
            "agent",
            "read",
            "ch_abc123:m7",
            "--match",
            "historical correction",
            "--context",
            "12",
        ])
        .unwrap();
        match matching.command.unwrap() {
            Commands::Agent {
                command: AgentCommand::Read(read),
            } => {
                assert_eq!(read.match_query.as_deref(), Some("historical correction"));
                assert_eq!(read.context, 12);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn agent_read_slice_options_validate_arguments() {
        let conflict = Args::try_parse_from([
            "claude-history",
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
            "claude-history",
            "agent",
            "read",
            "ch_abc123:m7",
            "--context",
            "2",
        ])
        .expect_err("context should require a match query");
        assert!(missing_match.to_string().contains("required"));

        let invalid_lines = Args::try_parse_from([
            "claude-history",
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
    fn agent_outline_captures_ref_and_display_options() {
        let args = Args::try_parse_from([
            "claude-history",
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

        match args.command.unwrap() {
            Commands::Agent {
                command: AgentCommand::Outline(outline),
            } => {
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
    fn agent_mode_flags_conflict() {
        let err = Args::try_parse_from([
            "claude-history",
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
            "claude-history",
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
            "claude-history",
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
        let err = Args::try_parse_from([
            "claude-history",
            "agent",
            "outline",
            "ch_abc123",
            "--budget",
            "0",
        ])
        .expect_err("zero budget should fail");

        assert!(err.to_string().contains("value must be greater than zero"));
    }

    #[test]
    fn agent_query_must_not_be_empty() {
        let err = Args::try_parse_from(["claude-history", "agent", "search", "   "])
            .expect_err("empty query should fail");

        assert!(err.to_string().contains("value must not be empty"));
    }

    #[test]
    fn semantic_query_must_not_be_empty() {
        let err = Args::try_parse_from(["claude-history", "--semantic-search", "   "])
            .expect_err("empty query should fail");

        assert!(err.to_string().contains("value must not be empty"));
    }

    #[test]
    fn semantic_top_must_be_positive() {
        let err = Args::try_parse_from([
            "claude-history",
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
        let args =
            Args::try_parse_from(["claude-history", "--debug-semantic-search", "cache"]).unwrap();

        assert_eq!(args.debug_semantic_search.as_deref(), Some("cache"));
    }

    #[test]
    fn generate_semantic_cache_flag_is_parseable() {
        let args = Args::try_parse_from(["claude-history", "--generate-semantic-cache"]).unwrap();

        assert!(args.generate_semantic_cache);
    }

    #[test]
    fn clear_semantic_cache_flag_is_parseable() {
        let args = Args::try_parse_from(["claude-history", "--clear-semantic-cache"]).unwrap();

        assert!(args.clear_semantic_cache);
    }

    #[test]
    fn semantic_flag_is_removed() {
        let err = Args::try_parse_from(["claude-history", "--semantic"])
            .expect_err("removed flag should fail");

        assert!(err.to_string().contains("unexpected argument '--semantic'"));
    }
}
