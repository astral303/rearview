//! `rearview` searches, reads and continues local coding-agent conversations,
//! and is primarily used as a command-line binary.
//!
//! See [GitHub](https://github.com/astral303/rearview) for more information.
//!
//! A limited public library interface is available. It has no semantic
//! version guarantees and may break or change at any time.

// Denied in `history` only; see `clippy.toml`.
#![allow(clippy::disallowed_methods)]

mod agent;
mod cache_file;
mod cli;
mod clock;
mod config;
mod debug;
mod debug_log;
mod display;
mod dropped_records_cli;
mod error;
mod history;
mod log_entry;
mod markdown;
#[cfg(test)]
mod output_contract;
mod pager;
mod parallel;
mod search;
mod semantic;
mod semantic_cli;
mod sessions_dir_cli;
mod syntax;
#[doc(hidden)]
pub mod test_support;
mod text_match;
mod time_filter;
mod tool_format;
mod tui;
mod update;

/// The package name from Cargo.toml. It is also the binary name, the config,
/// cache, and state directory name, and the prefix of release artifacts.
pub(crate) const APP_NAME: &str = env!("CARGO_PKG_NAME");

use clap::Parser;
use cli::{Args, Commands, DeleteEmptyArgs};
use clock::{Clock, SystemClock};
use error::{AppError, Result};
use history::provider::{Deleted, Stored};
use search::mode::{SearchModeResolution, TuiSearchMode};
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use std::process::{Command, ExitCode};
use tui::ListSearchMode;

/// Run `rearview` with the process's arguments on the system clock, print any
/// error to stderr, and return the exit status.
pub fn run() -> ExitCode {
    let Err(error) = try_run(Arc::new(SystemClock::new())) else {
        return ExitCode::SUCCESS;
    };
    if matches!(error, AppError::SelectionCancelled) {
        return ExitCode::SUCCESS;
    }
    eprint!("{}", error_report(error));
    ExitCode::FAILURE
}

fn error_report(error: AppError) -> String {
    match error {
        AppError::AgentProtocol(output) => output,
        AppError::Agent(error) => agent::diagnostic::format_error(&error),
        error => format!("Error: {error}\n"),
    }
}

/// Helper function to resolve a boolean setting by merging CLI flags and config values.
///
/// Priority: enable_flag > disable_flag > config_value > default_value
fn resolve_bool_setting(
    enable_flag: bool,
    disable_flag: bool,
    config_value: Option<bool>,
    default_value: bool,
) -> bool {
    if enable_flag {
        true
    } else if disable_flag {
        false
    } else {
        config_value.unwrap_or(default_value)
    }
}

fn run_delete_empty_command(
    args: DeleteEmptyArgs,
    environment: &history::Environment,
) -> Result<()> {
    let scope = if args.local {
        history::DeleteEmptyScope::Local
    } else {
        history::DeleteEmptyScope::All
    };
    let summary = history::delete_empty_sessions(environment, scope, args.yes)?;

    if summary.candidates.is_empty() && summary.refused.is_empty() {
        println!("No empty sessions found.");
        println!("{}", delete_empty_summary_line(args.yes, &[]));
        return Ok(());
    }

    if args.yes {
        println!("Deleted {} empty session(s):", summary.deleted);
    } else {
        println!(
            "Found {} empty session(s). Re-run with --yes to delete:",
            summary.candidates.len()
        );
    }

    for session in &summary.candidates {
        let preview = session
            .preview
            .as_deref()
            .map(truncate_delete_empty_preview)
            .unwrap_or_else(|| "(no preview)".to_string());
        println!(
            "{}\t{}\t{}\tusers={}\t{}",
            session.session_id,
            session.source.display_label(),
            session.project_name,
            session.user_messages,
            preview
        );
        println!("  {}", session.path.display());
    }

    if !summary.refused.is_empty() {
        println!(
            "Refused to delete {} empty session(s):",
            summary.refused.len()
        );
    }
    for refused in &summary.refused {
        println!(
            "{}\t{}\t{}",
            refused.session.session_id,
            refused.session.source.display_label(),
            refused.reason
        );
        println!("  {}", refused.session.path.display());
    }

    println!(
        "{}",
        delete_empty_summary_line(args.yes, &summary.candidates)
    );

    Ok(())
}

/// A bare total would leave a user with Codex history unable to tell whose
/// sessions are about to go.
fn delete_empty_summary_line(delete: bool, candidates: &[history::EmptySession]) -> String {
    let verb = if delete {
        "deleted"
    } else {
        "would be deleted"
    };
    if candidates.is_empty() {
        return format!("Summary: 0 empty session(s) {verb}.");
    }

    let mut per_agent = Vec::new();
    for provider in history::provider::providers() {
        let count = candidates
            .iter()
            .filter(|session| session.source == provider.source())
            .count();
        if count > 0 {
            per_agent.push(format!("{} {count}", provider.labels().display));
        }
    }
    format!(
        "Summary: {} empty session(s) {verb} ({}).",
        candidates.len(),
        per_agent.join(", ")
    )
}

/// Delete the session `session_id` names, whichever agent recorded it.
///
/// Each provider removes what the session owns beyond the transcript: Claude
/// every copy a cross-project fork left, Codex the thread's earlier rollouts
/// and its sub-agent threads, Kimi the whole session directory.
fn delete_session_by_id(session_id: &str) -> Result<()> {
    let found = history::provider::find_sessions_by_id(session_id);
    let (source, path) = match found.as_slice() {
        [] => return Err(AppError::SessionNotFound(session_id.to_owned())),
        [only] => only.clone(),
        several => return Err(ambiguous_session_id(session_id, several)),
    };
    let deleted = history::provider::delete_session(source, &path)?;
    eprintln!("{}", deletion_report(source, session_id, deleted));
    Ok(())
}

/// `Deleted Codex session <id> (2 session files, 14 sub-agent sessions)`, with
/// the parenthesis only when the delete removed more than one session file or
/// copy, or any sub-agent session.
fn deletion_report(source: history::Source, session_id: &str, deleted: Deleted) -> String {
    let mut beyond_the_session = Vec::new();
    match deleted.stored {
        Stored::SessionFiles(files) if files > 1 => {
            beyond_the_session.push(format!("{files} session files"));
        }
        Stored::Copies(copies) if copies > 1 => {
            beyond_the_session.push(format!("{copies} stored copies"));
        }
        Stored::SessionFiles(_) | Stored::Copies(_) => {}
    }
    match deleted.subagent_sessions {
        0 => {}
        1 => beyond_the_session.push("1 sub-agent session".to_owned()),
        sessions => beyond_the_session.push(format!("{sessions} sub-agent sessions")),
    }
    let mut report = format!("Deleted {} session {session_id}", source.display_label());
    if !beyond_the_session.is_empty() {
        report.push_str(&format!(" ({})", beyond_the_session.join(", ")));
    }
    report
}

/// Name every session `session_id` matched, so the user can tell them apart
/// and delete the one they meant from the list.
fn ambiguous_session_id(session_id: &str, found: &[(history::Source, PathBuf)]) -> AppError {
    let mut message = format!("{} sessions have the ID {session_id}:", found.len());
    for (_, path) in found {
        message.push_str(&format!("\n  {}", path.display()));
    }
    message.push_str("\nDelete the one you want from the list instead.");
    AppError::AmbiguousSessionId(message)
}

fn truncate_delete_empty_preview(preview: &str) -> String {
    const MAX_CHARS: usize = 120;
    let mut chars = preview.chars();
    let truncated = chars.by_ref().take(MAX_CHARS).collect::<String>();
    if chars.next().is_some() {
        format!("{}...", truncated)
    } else {
        truncated
    }
}

fn try_run(clock: Arc<dyn Clock>) -> Result<()> {
    let args = Args::parse();
    config::migrate_pre_rename_config()?;
    let environment = history::Environment::of_user(Arc::clone(&clock));

    // Handle subcommands
    if let Some(command) = args.command {
        return match command {
            Commands::Agent { command } => {
                agent::service::execute(*command, environment).map(|output| {
                    print!("{output}");
                })
            }
            Commands::DeleteEmpty { yes, local, all } => {
                run_delete_empty_command(DeleteEmptyArgs { yes, local, all }, &environment)
            }
            Commands::Update => update::run(),
        };
    }

    // Detect terminal theme before entering raw mode / alternate screen,
    // as terminal_light queries the terminal for background color
    tui::theme::detect_theme();

    let config = config::load_config()?;

    // Merge CLI arguments with config file settings. CLI takes precedence.
    let display_config = config.display.unwrap_or_default();

    // Extract resume config
    let resume_config = config.resume.unwrap_or_default();
    let default_args = resume_config.default_args.as_deref().unwrap_or(&[]);

    let search_config = config.search.unwrap_or_default();
    let tui_config = config.tui.unwrap_or_default();
    let search_mode = search::mode::resolve_tui_search_mode(SearchModeResolution {
        cli_mode: None,
        config_mode: search_config.mode,
        tui_semantic_search: tui_config.semantic_search,
    });
    let exclude_projects = tui_config.exclude_projects;

    // Disable colors globally when --no-color is passed
    if args.no_color {
        colored::control::set_override(false);
    }

    // Resolve keybindings
    let keys = config::KeyBindings::from_config(config.keys);

    // Use positive names internally for clarity
    let show_tools = resolve_bool_setting(
        args.show_tools,
        args.no_tools,
        display_config.no_tools.map(|b| !b),
        false, // Default: hide tools
    );
    // Map CLI flag to ToolDisplayMode
    // --show-tools → Full, --no-tools → Hidden, default → Hidden summary
    let tool_display = if args.show_tools {
        tui::ToolDisplayMode::Full
    } else if args.no_tools {
        tui::ToolDisplayMode::Hidden
    } else {
        match display_config.no_tools {
            Some(true) => tui::ToolDisplayMode::Hidden,
            Some(false) => tui::ToolDisplayMode::Full,
            None => tui::ToolDisplayMode::Hidden,
        }
    };
    let show_last = resolve_bool_setting(args.last, args.first, display_config.last, true);
    let show_thinking = resolve_bool_setting(
        args.show_thinking,
        args.hide_thinking,
        display_config.show_thinking,
        false,
    );
    let plain_mode = resolve_bool_setting(args.plain, false, display_config.plain, false);
    let use_pager = resolve_bool_setting(
        args.pager,
        args.no_pager,
        display_config.pager,
        std::io::stdout().is_terminal(),
    );

    // Handle --delete flag: delete a session by its ID and exit
    if let Some(ref session_id) = args.delete {
        return delete_session_by_id(session_id);
    }

    // Resolved before the search paths below so every one of them honours the
    // window rather than silently ignoring it.
    let time_filter = args.time.resolve(clock.as_ref())?;
    let load_all_conversations =
        || history::load_all_conversations(&environment, show_last, args.debug);

    // Handle --debug-search flag: debug search result scoring
    if let Some(ref query) = args.debug_search {
        let mut conversations = load_all_conversations()?;
        conversations
            .retain(|conversation| conversation.message_times.has_message_within(&time_filter));
        conversations.sort_by_key(|conversation| std::cmp::Reverse(conversation.timestamp));

        let searchable = search::precompute_search_text(&conversations);
        let now = clock.wall().with_timezone(&chrono::Local);

        let workspace = if args.local {
            Some(history::Workspace::current()?)
        } else {
            None
        };

        let debug_search =
            search::try_debug_search(&conversations, &searchable, query, now, |index| {
                workspace
                    .as_ref()
                    .is_none_or(|workspace| workspace.contains(&conversations[index]))
            })?;
        let results = debug_search.results;

        eprintln!("intent: {:?}", debug_search.parsed.unquoted());
        if debug_search.parsed.literals().is_empty() {
            eprintln!("literals: none");
        } else {
            eprintln!("literals:");
            for literal in debug_search.parsed.literals() {
                eprintln!("  {:?} ({:?})", literal.text(), literal.case_mode());
            }
        }
        eprintln!();

        for (rank, (idx, debug)) in results.iter().take(30).enumerate() {
            let conv = &conversations[*idx];
            let age = now.signed_duration_since(conv.timestamp);
            let project = conv.project_name.as_deref().unwrap_or("(none)");
            let age_str = format_debug_age(age);
            let session = conv
                .path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("?");
            eprintln!(
                "#{:2} score={:.2} freshness={:.2} | {} | {} | {} ago",
                rank + 1,
                debug.total,
                debug.freshness,
                project,
                session,
                age_str
            );

            for field in &debug.fields {
                if field.tf_score > 0.0 || field.adjacency_score > 0.0 {
                    eprintln!(
                        "     {}: tf={:.2} adj={:.2} (w={:.1})",
                        field.name, field.tf_score, field.adjacency_score, field.weight
                    );
                    for (word, tf, ln_score) in &field.word_details {
                        if *tf > 0 {
                            eprintln!("       \"{}\" tf={} ln={:.2}", word, tf, ln_score);
                        }
                    }
                }
            }
            eprintln!();
        }

        return Ok(());
    }

    if let Some(ref query) = args.debug_semantic_search {
        let mut conversations = load_all_conversations()?;
        conversations
            .retain(|conversation| conversation.message_times.has_message_within(&time_filter));
        conversations.sort_by_key(|conversation| std::cmp::Reverse(conversation.timestamp));
        return semantic_cli::debug_search(query, &conversations, args.local, clock.as_ref());
    }

    if args.generate_semantic_cache {
        // Cache generation covers the whole corpus by definition; a partial
        // cache would be silently wrong on later unfiltered searches.
        if time_filter.is_active() {
            return Err(AppError::ConfigError(
                "--generate-semantic-cache cannot be combined with a time filter".to_string(),
            ));
        }
        let mut conversations = load_all_conversations()?;
        conversations.sort_by_key(|conversation| std::cmp::Reverse(conversation.timestamp));
        return semantic_cli::generate_cache(&conversations, args.local, clock);
    }

    if args.clear_semantic_cache {
        return semantic_cli::clear_cache();
    }

    if args.report_dropped_records {
        let mut conversations = load_all_conversations()?;
        conversations
            .retain(|conversation| conversation.message_times.has_message_within(&time_filter));
        dropped_records_cli::print_report(&conversations);
        return Ok(());
    }

    // Handle --semantic-search flag
    if let Some(ref query) = args.semantic_search {
        let mut conversations = load_all_conversations()?;
        conversations
            .retain(|conversation| conversation.message_times.has_message_within(&time_filter));
        conversations.sort_by_key(|conversation| std::cmp::Reverse(conversation.timestamp));
        return semantic_cli::run(query, &conversations, args.semantic_top, args.local, clock);
    }

    // Handle --render flag: render a JSONL file in ledger format and exit
    if let Some(ref render_path) = args.render {
        let display_options = display::DisplayOptions {
            no_tools: !show_tools,
            show_thinking,
            debug_level: args.debug,
            use_pager,
            no_color: args.no_color,
        };
        return display::render_to_terminal(render_path, &display_options, clock.as_ref());
    }

    // Handle direct file input mode
    if let Some(ref input_file) = args.input_file {
        if !input_file.exists() {
            return Err(AppError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("File not found: {}", input_file.display()),
            )));
        }
        if !input_file.is_file() {
            return Err(AppError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("Not a file: {}", input_file.display()),
            )));
        }
        let source = history::format::parse_transcript(input_file)?
            .map(|projection| projection.source)
            .ok_or_else(|| AppError::UnrecognizedTranscript(input_file.display().to_string()))?;
        tui::run_single_file(
            input_file.clone(),
            source,
            tool_display,
            show_thinking,
            keys,
            environment,
        )?;
        return Ok(());
    }

    let use_local = args.local;

    let current_dir = std::env::current_dir().ok();

    if args.show_dir.is_some() || args.show_all_dirs {
        let Some(ref directory) = current_dir else {
            return Err(AppError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "Failed to get current directory",
            )));
        };
        let environment = history::provider::RootEnvironment::of_process();
        match args.show_dir {
            Some(agent) => println!(
                "{}",
                sessions_dir_cli::agent_sessions_dir(agent.0.provider(), &environment, directory)?
                    .display()
            ),
            None => sessions_dir_cli::print_all_sessions_dirs(
                &environment,
                directory,
                &mut std::io::stdout(),
                &mut std::io::stderr(),
            )?,
        }
        return Ok(());
    }

    let scope = tui::ListScope {
        // --local starts with workspace filter on; default is global (filter off)
        workspace_filter: use_local,
        workspace: current_dir.map(history::Workspace::at),
        exclude_projects,
    };

    // Always use streaming global loader for all conversations
    let load_meter = std::sync::Arc::new(history::load_meter::LoadMeter::default());
    let rx = history::load_all_conversations_streaming(
        environment.clone(),
        show_last,
        args.debug,
        time_filter,
        std::sync::Arc::clone(&load_meter),
    );

    // An empty result is far more often the time filter than an empty history,
    // so say which when a filter is in play.
    let describe_empty = |error| match error {
        AppError::NoHistoryFound(scope) if time_filter.is_active() => {
            AppError::NoHistoryFound(format!("{scope} within the requested time range"))
        }
        other => other,
    };

    let (conversations, selected_path) = match tui::run_with_loader(
        rx,
        load_meter,
        history::RefreshOptions {
            show_last,
            debug_level: args.debug,
            time: time_filter,
        },
        tool_display,
        show_thinking,
        keys,
        scope,
        tui::TuiSearchOptions {
            default_mode: tui_search_mode(search_mode),
        },
        history::active_load_filters(&[&time_filter]),
        environment.clone(),
    )
    .map_err(describe_empty)?
    {
        (tui::Action::Select(path), convs) => (convs, path),
        (tui::Action::Resume(path), convs) => {
            let conv = listed_conversation(&convs, &path)?;
            resume_with_agent(
                conv.source,
                &path,
                conv.project_path.as_ref(),
                default_args,
                false,
            )?;
            return Ok(());
        }
        (tui::Action::ForkResume(path), convs) => {
            let conv = listed_conversation(&convs, &path)?;
            resume_with_agent(
                conv.source,
                &path,
                conv.project_path.as_ref(),
                default_args,
                true,
            )?;
            return Ok(());
        }
        (tui::Action::Quit, _) => return Err(AppError::SelectionCancelled),
        (tui::Action::Delete(_) | tui::Action::Refresh, _) => {
            unreachable!("the TUI handles delete and refresh itself")
        }
    };

    if args.show_path {
        println!("{}", selected_path.display());
        return Ok(());
    }

    if args.show_id {
        println!(
            "{}",
            listed_conversation(&conversations, &selected_path)?.session_id
        );
        return Ok(());
    }

    if args.resume {
        debug::debug(
            args.debug,
            &format!("Selected path: {}", selected_path.display()),
        );
        let conv = listed_conversation(&conversations, &selected_path)?;
        debug::debug(
            args.debug,
            &format!("project_path: {:?}", conv.project_path),
        );
        if let Some(p) = &conv.project_path {
            debug::debug(args.debug, &format!("project_path exists: {}", p.exists()));
        }
        resume_with_agent(
            conv.source,
            &selected_path,
            conv.project_path.as_ref(),
            default_args,
            args.fork_session,
        )?;
        return Ok(());
    }

    // Log parse errors to debug log if debug mode is enabled
    if args.debug.is_some()
        && let Some(conv) = conversations.iter().find(|c| c.path == selected_path)
    {
        if let Err(e) = debug_log::log_parse_errors(conv, clock.as_ref()) {
            debug::warn(
                args.debug,
                &format!("Failed to write parse errors to log: {}", e),
            );
        } else if !conv.parse_errors.is_empty()
            && let Some(log_path) = debug_log::debug_log_path()
        {
            debug::info(
                args.debug,
                &format!(
                    "Logged {} parse error(s) to {}",
                    conv.parse_errors.len(),
                    log_path.display()
                ),
            );
        }
    }

    // Display the selected conversation
    let display_options = display::DisplayOptions {
        no_tools: !show_tools,
        show_thinking,
        debug_level: args.debug,
        use_pager,
        no_color: args.no_color,
    };

    let session =
        display::read_session_to_print(environment.cache_base(), &conversations, &selected_path)?;
    if plain_mode {
        display::display_conversation_plain(
            &selected_path,
            session,
            &display_options,
            clock.as_ref(),
        )?;
    } else {
        display::display_conversation(&selected_path, session, &display_options, clock.as_ref())?;
    }

    Ok(())
}

fn tui_search_mode(mode: TuiSearchMode) -> ListSearchMode {
    match mode {
        TuiSearchMode::Lexical => ListSearchMode::Lexical,
        TuiSearchMode::Semantic => ListSearchMode::Semantic,
    }
}

/// The row the list selected at `path`. An error when the list holds no such
/// row, rather than a guess at which agent recorded the file.
fn listed_conversation<'a>(
    conversations: &'a [history::Conversation],
    path: &Path,
) -> Result<&'a history::Conversation> {
    conversations
        .iter()
        .find(|conversation| conversation.path == path)
        .ok_or_else(|| AppError::SessionNotFound(path.display().to_string()))
}

fn resume_with_agent(
    source: history::Source,
    selected_path: &Path,
    project_path: Option<&PathBuf>,
    default_args: &[String],
    fork_session: bool,
) -> Result<()> {
    let launch = history::provider::SessionLaunch {
        path: selected_path,
        project_path: project_path.map(PathBuf::as_path),
        configured_args: default_args,
    };
    let command = history::provider::launch_command(source, &launch, fork_session)?;
    run_agent_command(command).map_err(|detail| AppError::AgentLaunch {
        agent: source.display_label(),
        detail,
    })
}

fn format_debug_age(age: chrono::Duration) -> String {
    let hours = age.num_hours();
    if hours < 24 {
        format!("{}h", hours)
    } else {
        format!("{}d", hours / 24)
    }
}

/// Replace this process with `command`; the error when it cannot start.
#[cfg(unix)]
fn run_agent_command(mut command: Command) -> std::result::Result<(), String> {
    use std::os::unix::process::CommandExt;

    Err(command.exec().to_string())
}

/// Run `command` to completion; the error when it cannot start or exits
/// with a failure status.
#[cfg(not(unix))]
fn run_agent_command(mut command: Command) -> std::result::Result<(), String> {
    let status = command.status().map_err(|error| error.to_string())?;
    if !status.success() {
        return Err(format!(
            "{} exited with {status}",
            command.get_program().to_string_lossy()
        ));
    }
    Ok(())
}

#[cfg(test)]
mod agent_command_tests {
    use super::*;
    use crate::agent::refs::AgentConversationKey;
    use crate::agent::service::{
        agent_semantic_candidates, agent_semantic_candidates_matching_tools,
        resolve_agent_conversation_arg, resolve_agent_read_args, run_agent_outline, run_agent_read,
    };
    use crate::agent::test_support::{
        assistant_jsonl_line as assistant, day_record, record_time, user_jsonl_line as user,
    };
    use crate::search::mode::SearchMode;
    use cli::{AgentOutlineArgs, AgentOutputFlags, AgentReadArgs};

    fn key(project: &str, filename: &str) -> AgentConversationKey {
        AgentConversationKey::new(
            project,
            filename,
            PathBuf::from(format!("/{project}/{filename}")),
        )
    }

    fn default_output() -> AgentOutputFlags {
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

    fn read_args(refs: Vec<String>, focus: Option<String>) -> AgentReadArgs {
        AgentReadArgs {
            refs,
            anchor: None,
            focus,
            tail: None,
            lines: None,
            match_query: None,
            context: 3,
            perf_stats: false,
            output: default_output(),
        }
    }

    /// A Pi id that two logs state must name both, so the user can tell which
    /// session they meant before anything is removed.
    #[test]
    fn ambiguous_id_names_every_session_that_carries_it() {
        let found = vec![
            (
                history::Source::Pi,
                PathBuf::from("/sessions/project/session.jsonl"),
            ),
            (
                history::Source::Pi,
                PathBuf::from("/sessions/project/branch.jsonl"),
            ),
        ];

        let error = ambiguous_session_id("custom_v1_id", &found).to_string();

        assert!(
            error.contains("2 sessions have the ID custom_v1_id"),
            "{error}"
        );
        assert!(error.contains("session.jsonl"), "{error}");
        assert!(error.contains("branch.jsonl"), "{error}");
        assert!(error.contains("from the list"), "{error}");
    }

    #[test]
    fn delete_report_counts_what_was_deleted_beyond_session() {
        let report = |stored, subagent_sessions| {
            deletion_report(
                history::Source::Codex,
                "thread",
                Deleted {
                    stored,
                    subagent_sessions,
                },
            )
        };

        assert_eq!(
            report(Stored::SessionFiles(1), 0),
            "Deleted Codex session thread"
        );
        assert_eq!(
            report(Stored::SessionFiles(2), 0),
            "Deleted Codex session thread (2 session files)"
        );
        assert_eq!(
            report(Stored::SessionFiles(1), 1),
            "Deleted Codex session thread (1 sub-agent session)"
        );
        assert_eq!(
            report(Stored::SessionFiles(2), 14),
            "Deleted Codex session thread (2 session files, 14 sub-agent sessions)"
        );
        assert_eq!(
            deletion_report(
                history::Source::OpenCode,
                "ses_migrated",
                Deleted {
                    stored: Stored::Copies(2),
                    subagent_sessions: 0,
                },
            ),
            "Deleted OpenCode session ses_migrated (2 stored copies)"
        );
    }

    fn empty_session(source: history::Source) -> history::EmptySession {
        history::EmptySession {
            source,
            path: PathBuf::from("session.jsonl"),
            session_id: "session".to_owned(),
            project_name: "project".to_owned(),
            timestamp: chrono::Local::now(),
            preview: None,
            user_messages: 1,
        }
    }

    #[test]
    fn summary_counts_each_agent_that_has_empty_session() {
        let candidates = [
            empty_session(history::Source::Claude),
            empty_session(history::Source::Codex),
            empty_session(history::Source::Claude),
        ];

        assert_eq!(
            delete_empty_summary_line(false, &candidates),
            "Summary: 3 empty session(s) would be deleted (Claude 2, Codex 1)."
        );
        assert_eq!(
            delete_empty_summary_line(true, &candidates),
            "Summary: 3 empty session(s) deleted (Claude 2, Codex 1)."
        );
    }

    #[test]
    fn summary_names_no_agent_when_nothing_is_empty() {
        assert_eq!(
            delete_empty_summary_line(false, &[]),
            "Summary: 0 empty session(s) would be deleted."
        );
    }

    #[test]
    fn read_validation_rejects_focus_outside_range() {
        let keys = vec![key(
            "project-a",
            "12345678-1234-4234-9234-123456789abc.jsonl",
        )];
        let conversation = keys[0].conversation_ref().canonical();
        let args = read_args(
            vec![format!("{conversation}:m2..m4")],
            Some("m5".to_string()),
        );
        let err = resolve_agent_read_args(&args, &keys).unwrap_err();
        assert!(err.to_string().contains("outside"));
    }

    fn write_jsonl(dir: &tempfile::TempDir, filename: &str, lines: &[String]) -> PathBuf {
        let path = dir.path().join(filename);
        std::fs::write(&path, lines.join("\n")).unwrap();
        path
    }

    #[test]
    fn command_refs_accept_session_id() {
        let keys = vec![key(
            "project-a",
            "12345678-1234-4234-9234-123456789abc.jsonl",
        )];
        let uuid = "12345678-1234-4234-9234-123456789abc";

        let read = read_args(vec![format!("{uuid}:m1..m2")], None);
        let (resolved, _) = resolve_agent_read_args(&read, &keys).unwrap();
        assert_eq!(resolved[0].1.key, keys[0]);

        let focus = read_args(
            vec![format!("{}:m1..m2", keys[0].conversation_ref().canonical())],
            Some(format!("{uuid}:m1")),
        );
        assert!(resolve_agent_read_args(&focus, &keys).is_ok());

        assert_eq!(
            resolve_agent_conversation_arg(uuid, &keys).unwrap().key,
            keys[0]
        );
    }

    #[test]
    fn read_command_loads_transcript_and_formats_protocol() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_jsonl(
            &dir,
            "12345678-1234-4234-9234-123456789abc.jsonl",
            &[user("question"), assistant("answer")],
        );
        let keys = vec![AgentConversationKey::new(
            "project-a",
            "12345678-1234-4234-9234-123456789abc.jsonl",
            path,
        )];
        let conversation = keys[0].conversation_ref().canonical();
        let args = read_args(vec![format!("{conversation}:m1..m2")], None);

        let output = run_agent_read(&args, Some(&keys)).unwrap();

        assert!(output.starts_with("protocol agent-read"));
        assert!(output.contains("conversation project=pr_"));
        assert!(output.contains("uuid=12345678-1234-4234-9234-123456789abc ref=ch_"));
        assert!(record_of(&output, "m1").starts_with("m1 user "), "{output}");
        assert!(output.contains("| question\n"));
        assert!(is_agent_record(record_of(&output, "m2")), "{output}");
        assert!(output.contains("| answer\n"));
        assert!(!output.contains("not implemented"));
    }

    /// The record of `output` that `address` opens: `m7`, `m7.2` or `m7..m9`.
    fn record_of<'a>(output: &'a str, address: &str) -> &'a str {
        output
            .lines()
            .find(|line| line.split_whitespace().next() == Some(address))
            .unwrap_or_else(|| panic!("no record of {address}: {output}"))
    }

    /// True when the field after `record`'s address, if any, is a time or an
    /// atom, not a word: the agent's own message.
    fn is_agent_record(record: &str) -> bool {
        record.split_whitespace().nth(1).is_none_or(|field| {
            field.starts_with(|first: char| first.is_ascii_digit()) || field.contains('=')
        })
    }

    #[test]
    fn read_slice_requires_one_single_message_ref() {
        let keys = vec![key(
            "project-a",
            "12345678-1234-4234-9234-123456789abc.jsonl",
        )];
        let conversation = keys[0].conversation_ref().canonical();
        let mut args = read_args(vec![format!("{conversation}:m1..m2")], None);
        args.match_query = Some("needle".to_string());

        let error = resolve_agent_read_args(&args, &keys).unwrap_err();

        assert!(error.to_string().contains("single-message ref"));
    }

    #[test]
    fn read_command_slices_large_message_around_matches() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_jsonl(
            &dir,
            "12345678-1234-4234-9234-123456789abc.jsonl",
            &[user("one\ntwo\nhistorical correction\nfour\nfive")],
        );
        let keys = vec![AgentConversationKey::new(
            "project-a",
            "12345678-1234-4234-9234-123456789abc.jsonl",
            path,
        )];
        let conversation = keys[0].conversation_ref().canonical();
        let mut args = read_args(vec![format!("{conversation}:m1")], None);
        args.match_query = Some("historical correction".to_string());
        args.context = 1;

        let output = run_agent_read(&args, Some(&keys)).unwrap();

        assert!(output.contains("slice=match=historical%20correction context=1 hits=1"));
        assert!(output.contains("|  2: two\n| >3: historical correction\n|  4: four\n"));
        assert!(!output.contains("1: one"));
        assert!(!output.contains("5: five"));
    }

    #[test]
    fn outline_command_loads_transcript_and_formats_protocol() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_jsonl(
            &dir,
            "12345678-1234-4234-9234-123456789abc.jsonl",
            &[user("question"), assistant("answer")],
        );
        let keys = vec![AgentConversationKey::new(
            "project-a",
            "12345678-1234-4234-9234-123456789abc.jsonl",
            path,
        )];
        let args = AgentOutlineArgs {
            conversation: keys[0].conversation_ref().canonical(),
            perf_stats: false,
            output: default_output(),
        };

        let output = run_agent_outline(&args, Some(&keys)).unwrap();

        assert!(output.starts_with("protocol agent-outline"));
        assert!(output.contains("conversation project=pr_"));
        assert!(output.contains("uuid=12345678-1234-4234-9234-123456789abc ref=ch_"));
        let at = |utc| record_time(utc, utc);
        assert_eq!(
            record_of(&output, "m1"),
            format!("m1 user{} chars=8 | question", at("2024-01-01T00:00:00Z")),
            "{output}"
        );
        assert_eq!(
            record_of(&output, "m2"),
            format!("m2{} chars=6 | answer", at("2024-01-01T00:00:01Z")),
            "{output}"
        );
    }

    #[test]
    fn search_output_emits_read_ref_with_focus_recipe() {
        let output = agent::search::AgentSearchOutput {
            protocol: agent::search::AgentProtocolKind::Search,
            target: None,
            query: "cache warming".to_string(),
            mode: SearchMode::Lexical,
            hits: vec![agent::search::AgentOutputHit {
                conversation_ref: "ch_1234abcd5678".to_string(),
                project_id: "pr_test".to_string(),
                conversation_uuid: "12345678-1234-4234-9234-123456789abc".to_string(),
                session: "12345678-1234-4234-9234-123456789abc.jsonl".to_string(),
                anchors: vec!["ma_0000000000000000".to_string()],
                title: "cache session".to_string(),
                score: 12.5,
                evidence_score: 12.5,
                semantic_score_breakdown: None,
                source: agent::search::AgentHitKind::Lexical,
                evidence_source: agent::retrieval::AgentHitSource::Dialogue,
                render_options: agent::retrieval::AgentHitRenderOptions::default(),
                preview: "cache warming answer".to_string(),
                focus_range: agent::refs::MessageRange::single(2),
                times: None,
                read_range: agent::refs::MessageRange { start: 1, end: 3 },
            }],
            groups: vec![],
            flat: true,
            budget: None,
            coverage: None,
        };

        let rendered = agent::search::format_agent_output(&output);

        assert!(rendered.starts_with(&format!(
            "protocol agent-search mode=lexical cut=none chars=none policy=per-hit hits=1{}\n",
            agent::time_span::tz_atom(agent::time_span::output_offset(
                None,
                clock::TestClock::start_wall()
            ))
        )));
        assert!(rendered.contains(
            "hit project=pr_test uuid=12345678-1234-4234-9234-123456789abc ref=ch_1234abcd5678"
        ));
        assert!(rendered.contains("read ref=ch_1234abcd5678:m1..m3 focus=m2..m2 tools=false tool-results=false thinking=false subagents=false\n"));
    }

    fn read_args_from_line(read_line: &str) -> AgentReadArgs {
        let mut read_ref = None;
        let mut focus = None;
        let mut tools = false;
        let mut tool_results = false;
        let mut thinking = false;
        let mut subagents = false;
        for field in read_line.split_whitespace().skip(1) {
            if let Some(value) = field.strip_prefix("ref=") {
                read_ref = Some(value.to_string());
            } else if let Some(value) = field.strip_prefix("focus=") {
                focus = Some(value.to_string());
            } else if field == "tools=true" {
                tools = true;
            } else if field == "tool-results=true" {
                tool_results = true;
            } else if field == "thinking=true" {
                thinking = true;
            } else if field == "subagents=true" {
                subagents = true;
            }
        }
        AgentReadArgs {
            refs: vec![read_ref.expect("read ref field")],
            anchor: None,
            focus,
            tail: None,
            lines: None,
            match_query: None,
            context: 3,
            perf_stats: false,
            output: AgentOutputFlags {
                budget: Some(6000),
                no_budget: false,
                tools,
                tool_results,
                thinking,
                subagents,
                time: cli::MessageTimeArgs::default(),
            },
        }
    }

    #[test]
    fn within_read_ref_can_drive_focused_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_jsonl(
            &dir,
            "12345678-1234-4234-9234-123456789abc.jsonl",
            &[
                user("question"),
                assistant("cache warming answer"),
                user("follow up"),
            ],
        );
        let (keys, resolved) = resolved_test_conversation(path.clone());
        let conversation = stubbed_conversation(path, 3);
        let transcript = load_transcript(&resolved.key.path);
        let within_args = cli::AgentWithinArgs {
            conversation: resolved.reference.canonical(),
            query: "cache warming".to_string(),
            top: Some(1),
            budget: Some(6000),
            no_budget: false,
            perf_stats: false,
            search_mode: cli::AgentSearchModeArgs::explicit(SearchMode::Lexical),
            time: cli::MessageTimeArgs::default(),
        };
        let within_request = agent::search::AgentWithinRequest {
            query: within_args.query.clone(),
            top: within_args.top.unwrap(),
            cli_mode: within_args.mode_override(),
            config_mode: None,
            tui_semantic_search: None,
            budget: None,
            window: time_filter::TimeFilter::default(),
        };
        let within = agent::search::format_agent_output(&agent::search::run_within_search(
            &within_request,
            &conversation,
            &resolved,
            &transcript,
            &[],
        ));
        let read_line = within
            .lines()
            .find(|line| line.starts_with("read ref="))
            .expect("within output should include a read ref");
        let read_args = read_args_from_line(read_line);

        let output = run_agent_read(&read_args, Some(&keys)).unwrap();

        assert!(output.starts_with("protocol agent-read"));
        assert!(is_agent_record(record_of(&output, "m2")), "{output}");
        assert!(output.contains("| cache warming answer\n"));
    }

    fn tool_result_user(tool_output: &str) -> String {
        serde_json::json!({
            "type": "user",
            "timestamp": "2024-01-01T00:00:02Z",
            "message": {
                "role": "user",
                "content": [{"type": "tool_result", "tool_use_id": "toolu_1", "content": tool_output}]
            }
        })
        .to_string()
    }

    fn resolved_test_conversation(
        path: PathBuf,
    ) -> (Vec<AgentConversationKey>, agent::refs::ResolvedConversation) {
        let key = AgentConversationKey::new(
            "project-a",
            "12345678-1234-4234-9234-123456789abc.jsonl",
            path,
        );
        let resolved = agent::refs::ResolvedConversation {
            key: key.clone(),
            reference: key.conversation_ref(),
        };
        (vec![key], resolved)
    }

    fn stubbed_conversation(path: PathBuf, message_count: usize) -> history::Conversation {
        history::Conversation {
            source: history::Source::Claude,
            subagents: Vec::new(),
            session_id: path
                .file_stem()
                .and_then(|name| name.to_str())
                .unwrap_or_default()
                .to_owned(),
            path,
            index: 0,
            timestamp: chrono::Local::now(),
            preview: "session".to_string(),
            preview_first: "session".to_string(),
            preview_last: "session".to_string(),
            full_text: "session".to_string(),
            agent_search_text: String::new(),
            semantic_route_text: String::new(),
            semantic_turns: vec!["session".to_string()],
            semantic_turn_ranges: vec![agent::refs::MessageRange::single(1)],
            search_text_lower: "session".to_string(),
            project_name: Some("project-a".to_string()),
            project_path: None,
            cwd: None,
            message_count,
            message_times: history::MessageTimes::default(),
            message_text_starts: history::MessageTextStarts::default(),
            assistant_messages: 1,
            parse_errors: Vec::new(),
            summary: None,
            custom_title: Some("session".to_string()),
            model: None,
            total_tokens: 0,
            duration_minutes: None,
            fingerprint: None,
            has_transient_read_error: false,
            is_missing_earlier_turns: false,
            cached_session: None,
            thinking_and_tool_call_text: String::new(),
        }
    }

    fn parsed_conversation(path: &Path) -> history::Conversation {
        history::format::claude::conversation_from_reader(
            path,
            std::io::Cursor::new(std::fs::read_to_string(path).unwrap()),
            history::parser::RowDating::new(None, &clock::TestClock::default()),
            None,
        )
        .unwrap()
        .unwrap()
    }

    fn load_transcript(path: &Path) -> agent::transcript::AgentTranscript {
        agent::transcript::AgentTranscript::load(path).unwrap()
    }

    fn semantic_hit_for_test(
        source: crate::semantic::types::SemanticChunkSource,
        message_range: agent::refs::MessageRange,
        evidence_preview: &str,
    ) -> crate::semantic::types::SemanticHit {
        crate::semantic::types::SemanticHit::new(
            crate::semantic::types::SemanticScoreBreakdown::new(0.9, 0.0),
            crate::semantic::types::SemanticExplanation {
                quality: crate::semantic::types::SemanticQuality::Good,
                quality_label: "good",
                matched_terms: vec![],
                evidence_preview: evidence_preview.to_string(),
                rationale_kind: crate::semantic::types::SemanticRationaleKind::SemanticOnly,
                chunk: crate::semantic::types::SemanticChunkIdentity {
                    conversation_index: 0,
                    source,
                    session: "session".to_string(),
                    chunk_index: 0,
                    message_range,
                },
            },
        )
    }

    fn progress_text_message(text: &str) -> String {
        serde_json::json!({
            "type": "progress",
            "data": {
                "type": "agent_progress",
                "agentId": "agent-abcdef",
                "message": {
                    "type": "assistant",
                    "message": {
                        "role": "assistant",
                        "content": [{"type": "text", "text": text}]
                    }
                }
            }
        })
        .to_string()
    }

    #[test]
    fn tool_result_read_recipe_replays_without_manual_flags() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_jsonl(
            &dir,
            "12345678-1234-4234-9234-123456789abc.jsonl",
            &[
                user("question"),
                assistant("I'll inspect it"),
                tool_result_user("hidden_exact_tool_needle"),
                assistant("done"),
            ],
        );
        let (keys, resolved) = resolved_test_conversation(path.clone());
        let conversation = stubbed_conversation(path, 4);
        let transcript = load_transcript(&resolved.key.path);
        let within_request = agent::search::AgentWithinRequest {
            query: "\"hidden_exact_tool_needle\"".to_string(),
            top: 1,
            cli_mode: None,
            config_mode: None,
            tui_semantic_search: None,
            budget: None,
            window: time_filter::TimeFilter::default(),
        };
        let within = agent::search::format_agent_output(&agent::search::run_within_search(
            &within_request,
            &conversation,
            &resolved,
            &transcript,
            &[],
        ));

        assert!(within.contains("hit project=pr_"));
        assert!(within.contains(" ref="));
        assert!(within.contains("source=tool"));
        let read_line = within
            .lines()
            .find(|line| line.starts_with("read ref="))
            .expect("within output should include a read ref");
        assert!(read_line.contains("tool-results=true"));
        let output = run_agent_read(&read_args_from_line(read_line), Some(&keys)).unwrap();

        assert!(output.contains("hidden_exact_tool_needle"));
    }

    #[test]
    fn semantic_read_ref_uses_canonical_assistant_ordinal() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_jsonl(
            &dir,
            "12345678-1234-4234-9234-123456789abc.jsonl",
            &[
                user("first visible"),
                serde_json::json!({
                    "type": "assistant",
                    "timestamp": "2024-01-01T00:00:01Z",
                    "message": {"role": "assistant", "content": [{"type": "tool_use", "id": "toolu_1", "name": "Bash", "input": {"command": "pwd"}}]}
                })
                .to_string(),
                tool_result_user("tool output only"),
                progress_text_message("subagent hidden text"),
                assistant("final assistant text"),
            ],
        );
        let (keys, resolved) = resolved_test_conversation(path.clone());
        let conversation = parsed_conversation(&path);
        let transcript = load_transcript(&resolved.key.path);
        let semantic_range = *conversation
            .semantic_turn_ranges
            .iter()
            .find(|range| **range == agent::refs::MessageRange::single(5))
            .expect("assistant text should use canonical m5");
        let semantic_hit = semantic_hit_for_test(
            crate::semantic::types::SemanticChunkSource::VisibleDialogue,
            semantic_range,
            "final assistant text",
        );
        let within_request = agent::search::AgentWithinRequest {
            query: "final assistant".to_string(),
            top: 1,
            cli_mode: Some(SearchMode::Semantic),
            config_mode: None,
            tui_semantic_search: None,
            budget: None,
            window: time_filter::TimeFilter::default(),
        };
        let within = agent::search::format_agent_output(&agent::search::run_within_search(
            &within_request,
            &conversation,
            &resolved,
            &transcript,
            &[semantic_hit],
        ));

        assert!(within.contains("focus=m5..m5"));
        let read_line = within
            .lines()
            .find(|line| line.starts_with("read ref="))
            .expect("within output should include a read ref");
        let output = run_agent_read(&read_args_from_line(read_line), Some(&keys)).unwrap();

        assert!(is_agent_record(record_of(&output, "m5")), "{output}");
        assert!(output.contains("final assistant text"));
    }

    #[test]
    fn semantic_read_ref_skips_assistant_image_only_ordinal() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_jsonl(
            &dir,
            "12345678-1234-4234-9234-123456789abc.jsonl",
            &[
                user("first visible"),
                serde_json::json!({
                    "type": "assistant",
                    "timestamp": "2024-01-01T00:00:01Z",
                    "message": {"role": "assistant", "content": [{"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "abc"}}]}
                })
                .to_string(),
                assistant("final assistant text"),
            ],
        );
        let (keys, resolved) = resolved_test_conversation(path.clone());
        let conversation = parsed_conversation(&path);
        let transcript = load_transcript(&resolved.key.path);
        let semantic_range = *conversation
            .semantic_turn_ranges
            .iter()
            .find(|range| **range == agent::refs::MessageRange::single(2))
            .expect("assistant text should use canonical m2");
        let semantic_hit = semantic_hit_for_test(
            crate::semantic::types::SemanticChunkSource::VisibleDialogue,
            semantic_range,
            "final assistant text",
        );
        let within_request = agent::search::AgentWithinRequest {
            query: "final assistant".to_string(),
            top: 1,
            cli_mode: Some(SearchMode::Semantic),
            config_mode: None,
            tui_semantic_search: None,
            budget: None,
            window: time_filter::TimeFilter::default(),
        };
        let within = agent::search::format_agent_output(&agent::search::run_within_search(
            &within_request,
            &conversation,
            &resolved,
            &transcript,
            &[semantic_hit],
        ));

        assert!(within.contains("focus=m2..m2"));
        let read_line = within
            .lines()
            .find(|line| line.starts_with("read ref="))
            .expect("within output should include a read ref");
        let output = run_agent_read(&read_args_from_line(read_line), Some(&keys)).unwrap();

        assert!(is_agent_record(record_of(&output, "m2")), "{output}");
        assert!(output.contains("final assistant text"));
    }

    #[test]
    fn global_search_finds_progress_only_subagent_evidence() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_jsonl(
            &dir,
            "12345678-1234-4234-9234-123456789abc.jsonl",
            &[
                user("question"),
                progress_text_message("subagent_unique_needle"),
                assistant("done"),
            ],
        );
        let (keys, _) = resolved_test_conversation(path.clone());
        let conversation = parsed_conversation(&path);
        assert!(!conversation.full_text.contains("subagent_unique_needle"));
        assert!(
            conversation
                .agent_search_text
                .contains("subagent_unique_needle")
        );
        let conversations = vec![conversation];
        let texts = search::precompute_agent_search_text(
            &conversations,
            &time_filter::TimeFilter::default(),
        );
        let ranked = search::agent_search(
            &conversations,
            &texts,
            "\"subagent_unique_needle\"",
            chrono::Local::now(),
        );
        let request = agent::search::AgentSearchRequest {
            query: "\"subagent_unique_needle\"".to_string(),
            top: 1,
            cli_mode: None,
            config_mode: None,
            tui_semantic_search: None,
            flat: false,
            hits_per_conversation: 2,
            retrieval_hits_per_conversation: None,
            all_hits: false,
            budget: None,
            session_offset: 0,
            window: time_filter::TimeFilter::default(),
        };
        let output = agent::search::run_global_lexical_search(
            &request,
            &conversations,
            &keys,
            &ranked,
            |key| agent::transcript::AgentTranscript::load(&key.path),
        )
        .unwrap();
        let rendered = agent::search::format_agent_output(&output);

        assert!(rendered.contains("protocol agent-search"));
        assert!(rendered.contains("conversation rank=1"));
        assert!(rendered.contains("subagents=true"));
        let read_line = rendered
            .lines()
            .find(|line| line.starts_with("read ref="))
            .expect("search output should include a read ref");
        let read_output = run_agent_read(&read_args_from_line(read_line), Some(&keys)).unwrap();

        assert!(read_output.contains("subagent_unique_needle"));
    }

    #[test]
    fn search_and_read_keep_every_record_of_one_claude_reply() {
        let reply_record = |content: serde_json::Value| {
            serde_json::json!({
                "type": "assistant",
                "timestamp": "2024-01-01T00:00:01Z",
                "message": {"id": "msg_1", "role": "assistant", "content": [content]}
            })
            .to_string()
        };
        let dir = tempfile::tempdir().unwrap();
        let path = write_jsonl(
            &dir,
            "12345678-1234-4234-9234-123456789abc.jsonl",
            &[
                user("question"),
                reply_record(
                    serde_json::json!({"type": "thinking", "thinking": "weigh_options_needle", "signature": "sig"}),
                ),
                reply_record(
                    serde_json::json!({"type": "text", "text": "check_cache_first_needle"}),
                ),
                reply_record(
                    serde_json::json!({"type": "tool_use", "id": "toolu_1", "name": "Bash", "input": {"command": "ls"}}),
                ),
                tool_result_user("ok"),
                assistant("done"),
            ],
        );
        let (keys, _) = resolved_test_conversation(path.clone());
        let conversations = vec![parsed_conversation(&path)];
        let texts = search::precompute_agent_search_text(
            &conversations,
            &time_filter::TimeFilter::default(),
        );
        let query = "\"check_cache_first_needle\"";
        let ranked = search::agent_search(&conversations, &texts, query, chrono::Local::now());
        let request = agent::search::AgentSearchRequest {
            query: query.to_string(),
            top: 1,
            cli_mode: None,
            config_mode: None,
            tui_semantic_search: None,
            flat: false,
            hits_per_conversation: 2,
            retrieval_hits_per_conversation: None,
            all_hits: false,
            budget: None,
            session_offset: 0,
            window: time_filter::TimeFilter::default(),
        };
        let output = agent::search::run_global_lexical_search(
            &request,
            &conversations,
            &keys,
            &ranked,
            |key| agent::transcript::AgentTranscript::load(&key.path),
        )
        .unwrap();
        let rendered = agent::search::format_agent_output(&output);
        assert!(rendered.contains("focus=m2..m2"), "{rendered}");

        let whole_reply = format!(
            "read ref={}:m2 tools=true thinking=true",
            keys[0].conversation_ref().canonical()
        );
        let read_output = run_agent_read(&read_args_from_line(&whole_reply), Some(&keys)).unwrap();

        for part in [
            "weigh_options_needle",
            "check_cache_first_needle",
            "Bash: ls",
        ] {
            assert!(
                read_output.contains(part),
                "missing {part:?}: {read_output}"
            );
        }
    }

    /// A Claude Code session of six messages. The reply m2 joins records
    /// written minutes apart: its `Bash` call was written after m3, the
    /// result of its `Read` call. m5 is a sub-agent's reply, and m6 a prompt
    /// recorded without a time.
    fn session_with_times(dir: &tempfile::TempDir) -> PathBuf {
        let record = |kind: &str, timestamp: Option<&str>, message: serde_json::Value| {
            let mut record = serde_json::json!({"type": kind, "message": message});
            if let Some(timestamp) = timestamp {
                record["timestamp"] = serde_json::json!(timestamp);
            }
            record.to_string()
        };
        let reply = |timestamp, block: serde_json::Value| {
            record(
                "assistant",
                Some(timestamp),
                serde_json::json!({"id": "msg_1", "role": "assistant", "content": [block]}),
            )
        };
        let result = |timestamp, call: &str, output: &str| {
            record(
                "user",
                Some(timestamp),
                serde_json::json!({"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": call, "content": output}
                ]}),
            )
        };
        let subagent_reply = serde_json::json!({
            "type": "progress",
            "timestamp": "2026-10-14T15:42:00Z",
            "data": {
                "type": "agent_progress",
                "agentId": "agent-abcdef",
                "message": {"type": "assistant", "message": {"role": "assistant", "content": [
                    {"type": "text", "text": "The sub-agent found stale_cache_needle"}
                ]}}
            }
        });
        write_jsonl(
            dir,
            "12345678-1234-4234-9234-123456789abc.jsonl",
            &[
                record(
                    "user",
                    Some("2026-10-14T15:38:00Z"),
                    serde_json::json!({"role": "user", "content": "Check the cache"}),
                ),
                reply(
                    "2026-10-14T15:38:10Z",
                    serde_json::json!({"type": "text", "text": "I'll read the cache config first"}),
                ),
                reply(
                    "2026-10-14T15:38:12Z",
                    serde_json::json!({"type": "tool_use", "id": "toolu_read", "name": "Read", "input": {"file_path": "cache.toml"}}),
                ),
                result("2026-10-14T15:38:13Z", "toolu_read", "cache_size = 64"),
                reply(
                    "2026-10-14T15:41:30Z",
                    serde_json::json!({"type": "tool_use", "id": "toolu_warm", "name": "Bash", "input": {"command": "pwsh -File warm-cache.ps1"}}),
                ),
                result(
                    "2026-10-14T15:41:31Z",
                    "toolu_warm",
                    "active_cache_needle warmed",
                ),
                subagent_reply.to_string(),
                record(
                    "user",
                    None,
                    serde_json::json!({"role": "user", "content": "untimed_prompt_needle"}),
                ),
            ],
        )
    }

    /// The `at=` and `to=` atoms of a record line; empty when it has none.
    fn times_in(line: &str) -> &str {
        let Some(start) = line.find(" at=") else {
            return "";
        };
        let times = &line[start..];
        times.find(" | ").map_or(times, |end| &times[..end])
    }

    fn at(timestamp: &str) -> String {
        format!(" at={}", agent::test_support::local_time(timestamp))
    }

    fn at_to(first: &str, last: &str) -> String {
        format!("{} to={}", at(first), agent::test_support::local_time(last))
    }

    fn line_starting<'a>(output: &'a str, prefix: &str) -> &'a str {
        output
            .lines()
            .find(|line| line.starts_with(prefix))
            .unwrap_or_else(|| panic!("no line starts with {prefix:?}: {output}"))
    }

    #[test]
    fn read_and_outline_show_when_each_message_was_written() {
        let dir = tempfile::tempdir().unwrap();
        let (keys, resolved) = resolved_test_conversation(session_with_times(&dir));
        let every_part = || AgentOutputFlags {
            budget: None,
            no_budget: true,
            tools: true,
            tool_results: true,
            thinking: true,
            subagents: true,
            time: cli::MessageTimeArgs::default(),
        };
        let read = run_agent_read(
            &AgentReadArgs {
                output: every_part(),
                ..read_args(vec![resolved.reference.canonical()], None)
            },
            Some(&keys),
        )
        .unwrap();
        let outline = run_agent_outline(
            &AgentOutlineArgs {
                conversation: resolved.reference.canonical(),
                perf_stats: false,
                output: every_part(),
            },
            Some(&keys),
        )
        .unwrap();

        let at = |utc| record_time(utc, utc);
        let expected = [
            at("2026-10-14T15:38:00Z"),
            record_time("2026-10-14T15:38:10Z", "2026-10-14T15:41:30Z"),
            at("2026-10-14T15:38:13Z"),
            at("2026-10-14T15:41:31Z"),
            at("2026-10-14T15:42:00Z"),
            String::new(),
        ];
        for (index, time) in expected.iter().enumerate() {
            let address = format!("m{}", index + 1);
            assert_eq!(
                positional_time(record_of(&read, &address)),
                *time,
                "read {address}"
            );
            assert_eq!(
                positional_time(record_of(&outline, &address)),
                *time,
                "outline {address}"
            );
        }
        let day = format!("\n{}\n", day_record("2026-10-14T15:38:00Z"));
        assert!(read.contains(&day), "{read}");
        assert!(outline.contains(&day), "{outline}");
    }

    /// The time in position on a read or outline record, after its address
    /// and any word, with its leading space; empty without a time.
    fn positional_time(record: &str) -> String {
        record
            .split_whitespace()
            .skip(1)
            .find(|field| field.starts_with(|first: char| first.is_ascii_digit()))
            .map(|time| format!(" {time}"))
            .unwrap_or_default()
    }

    /// m2's `Bash` call is hidden without `--tools`, and its time still ends
    /// m2's times.
    #[test]
    fn message_shows_time_of_part_flags_hide() {
        let dir = tempfile::tempdir().unwrap();
        let (keys, resolved) = resolved_test_conversation(session_with_times(&dir));

        let read = run_agent_read(
            &read_args(vec![format!("{}:m2", resolved.reference.canonical())], None),
            Some(&keys),
        )
        .unwrap();

        assert!(!read.contains("tool Bash"), "{read}");
        assert_eq!(
            positional_time(record_of(&read, "m2")),
            record_time("2026-10-14T15:38:10Z", "2026-10-14T15:41:30Z")
        );
    }

    /// m2 shows its text and hides its two calls; m3 and m4 hold a result
    /// alone, and m5 is a sub-agent's reply.
    #[test]
    fn read_reports_what_its_flags_hide() {
        let dir = tempfile::tempdir().unwrap();
        let (keys, resolved) = resolved_test_conversation(session_with_times(&dir));

        let read = run_agent_read(
            &read_args(vec![resolved.reference.canonical()], None),
            Some(&keys),
        )
        .unwrap();

        assert!(
            line_starting(&read, "protocol agent-read ")
                .ends_with(" hidden-tools=2 hidden-tool-results=2 hidden-subagents=1"),
            "{read}"
        );
        assert_eq!(
            record_of(&read, "m2"),
            format!(
                "m2{} hidden-tools=Read,Bash",
                record_time("2026-10-14T15:38:10Z", "2026-10-14T15:41:30Z")
            ),
            "{read}"
        );
        assert_eq!(
            record_of(&read, "m3..m4"),
            format!(
                "m3..m4 hidden{} tool-results=2 calls=m2.1..2",
                record_time("2026-10-14T15:38:13Z", "2026-10-14T15:41:31Z")
            )
        );
        assert_eq!(
            record_of(&read, "m5"),
            format!(
                "m5 s1 hidden{} subagents=1",
                record_time("2026-10-14T15:42:00Z", "2026-10-14T15:42:00Z")
            )
        );
    }

    #[test]
    fn search_and_within_hits_show_when_their_message_was_written() {
        let dir = tempfile::tempdir().unwrap();
        let path = session_with_times(&dir);
        let (keys, resolved) = resolved_test_conversation(path.clone());
        let conversations = vec![parsed_conversation(&path)];
        let transcript = load_transcript(&path);
        let within = |query: &str,
                      mode: SearchMode,
                      semantic_hits: &[crate::semantic::types::SemanticHit]| {
            let request = agent::search::AgentWithinRequest {
                query: query.to_string(),
                top: 1,
                cli_mode: Some(mode),
                config_mode: None,
                tui_semantic_search: None,
                budget: None,
                window: time_filter::TimeFilter::default(),
            };
            agent::search::format_agent_output(&agent::search::run_within_search(
                &request,
                &conversations[0],
                &resolved,
                &transcript,
                semantic_hits,
            ))
        };
        let search = |query: &str, mode: SearchMode| {
            let request = agent::search::AgentSearchRequest {
                query: query.to_string(),
                top: 1,
                cli_mode: Some(mode),
                config_mode: None,
                tui_semantic_search: None,
                flat: false,
                hits_per_conversation: 1,
                retrieval_hits_per_conversation: None,
                all_hits: false,
                budget: None,
                session_offset: 0,
                window: time_filter::TimeFilter::default(),
            };
            agent::search::format_agent_output(
                &agent::search::run_global_lexical_search(
                    &request,
                    &conversations,
                    &keys,
                    &[0],
                    |key| agent::transcript::AgentTranscript::load(&key.path),
                )
                .unwrap(),
            )
        };
        let semantic_hit = semantic_hit_for_test(
            crate::semantic::types::SemanticChunkSource::VisibleDialogue,
            agent::refs::MessageRange { start: 1, end: 2 },
            "Check the cache",
        );
        let semantic_reply_hit = semantic_hit_for_test(
            crate::semantic::types::SemanticChunkSource::VisibleDialogue,
            agent::refs::MessageRange::single(2),
            "I'll read the cache config first",
        );
        let reply_times = at_to("2026-10-14T15:38:10Z", "2026-10-14T15:41:30Z");
        let subagent_times = at("2026-10-14T15:42:00Z");
        let hybrid = within("cache config", SearchMode::Hybrid, &[semantic_reply_hit]);
        assert!(
            line_starting(&hybrid, "hit ").contains(" source=hybrid "),
            "{hybrid}"
        );

        let cases = [
            (
                "lexical within",
                within("cache config", SearchMode::Lexical, &[]),
                reply_times.clone(),
            ),
            (
                "exact within",
                within("\"stale_cache_needle\"", SearchMode::Exact, &[]),
                subagent_times.clone(),
            ),
            (
                "semantic within",
                within("cache", SearchMode::Semantic, &[semantic_hit]),
                at_to("2026-10-14T15:38:00Z", "2026-10-14T15:41:30Z"),
            ),
            ("hybrid within", hybrid, reply_times.clone()),
            (
                "within without a time",
                within("untimed_prompt_needle", SearchMode::Lexical, &[]),
                String::new(),
            ),
            (
                "lexical search",
                search("cache config", SearchMode::Lexical),
                reply_times,
            ),
            (
                "exact search",
                search("\"stale_cache_needle\"", SearchMode::Exact),
                subagent_times,
            ),
        ];
        for (case, output, times) in cases {
            assert_eq!(times_in(line_starting(&output, "hit ")), times, "{case}");
        }
    }

    /// `--after` and `--before` at the local times of the UTC times `after`
    /// and `before`, to the second.
    fn window_args(after: &str, before: &str) -> cli::MessageTimeArgs {
        let local = |utc: &str| {
            agent::test_support::local_time(utc)[.."2026-10-14T15:38:00".len()]
                .parse::<time_filter::TimePoint>()
                .unwrap()
        };
        cli::MessageTimeArgs {
            since: None,
            after: Some(local(after)),
            before: Some(local(before)),
        }
    }

    /// The `mN` of each message record of a read or an outline.
    fn ordinals_shown(output: &str) -> Vec<&str> {
        output
            .lines()
            .filter_map(|line| line.split_whitespace().next())
            .filter(|atom| {
                atom.strip_prefix('m').is_some_and(|digits| {
                    !digits.is_empty() && digits.chars().all(|digit| digit.is_ascii_digit())
                })
            })
            .collect()
    }

    /// True when the header line of `output` carries `atom`.
    fn header_has(output: &str, atom: &str) -> bool {
        output
            .lines()
            .next()
            .is_some_and(|header| header.split_whitespace().any(|field| field == atom))
    }

    /// `session_with_times`, read and outlined with every part shown inside
    /// `window`.
    struct WindowedSession {
        _dir: tempfile::TempDir,
        keys: Vec<AgentConversationKey>,
        reference: String,
    }

    impl WindowedSession {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let (keys, resolved) = resolved_test_conversation(session_with_times(&dir));
            Self {
                _dir: dir,
                keys,
                reference: resolved.reference.canonical(),
            }
        }

        fn every_part_within(window: cli::MessageTimeArgs) -> AgentOutputFlags {
            AgentOutputFlags {
                budget: None,
                no_budget: true,
                tools: true,
                tool_results: true,
                thinking: true,
                subagents: true,
                time: window,
            }
        }

        fn read(&self, range: &str, window: cli::MessageTimeArgs) -> String {
            let args = AgentReadArgs {
                output: Self::every_part_within(window),
                ..read_args(vec![format!("{}{range}", self.reference)], None)
            };
            run_agent_read(&args, Some(&self.keys)).unwrap()
        }

        fn read_tail(&self, tail: usize, window: cli::MessageTimeArgs) -> String {
            let args = AgentReadArgs {
                tail: std::num::NonZeroUsize::new(tail),
                output: Self::every_part_within(window),
                ..read_args(vec![self.reference.clone()], None)
            };
            run_agent_read(&args, Some(&self.keys)).unwrap()
        }

        fn outline(&self, window: cli::MessageTimeArgs) -> String {
            let args = AgentOutlineArgs {
                conversation: self.reference.clone(),
                perf_stats: false,
                output: Self::every_part_within(window),
            };
            run_agent_outline(&args, Some(&self.keys)).unwrap()
        }
    }

    /// m2 runs from 15:38:10 to 15:41:30, across both bounds.
    fn window_across_m2() -> cli::MessageTimeArgs {
        window_args("2026-10-14T15:39:00Z", "2026-10-14T15:40:00Z")
    }

    /// m4 at 15:41:31 and m5 at 15:42:00 fall on the bounds.
    fn window_bounded_by_m4_and_m5() -> cli::MessageTimeArgs {
        window_args("2026-10-14T15:41:31Z", "2026-10-14T15:42:00Z")
    }

    /// After m4 and before m5: messages on either side, none inside.
    fn window_between_m4_and_m5() -> cli::MessageTimeArgs {
        window_args("2026-10-14T15:41:32Z", "2026-10-14T15:41:59Z")
    }

    /// m1 to m5. m6 does not have a time, so it falls outside.
    fn window_around_every_timed_message() -> cli::MessageTimeArgs {
        window_args("2026-10-14T15:00:00Z", "2026-10-14T16:00:00Z")
    }

    #[test]
    fn read_holds_only_messages_inside_window() {
        let session = WindowedSession::new();

        for (case, window, expected) in [
            ("across m2", window_across_m2(), vec!["m2"]),
            (
                "on the bounds",
                window_bounded_by_m4_and_m5(),
                vec!["m4", "m5"],
            ),
            ("idle", window_between_m4_and_m5(), vec![]),
            (
                "every timed message",
                window_around_every_timed_message(),
                vec!["m1", "m2", "m3", "m4", "m5"],
            ),
        ] {
            let read = session.read("", window);

            assert_eq!(ordinals_shown(&read), expected, "{case}");
            let in_window = format!("messages-in-window={}", expected.len());
            assert!(header_has(&read, &in_window), "{case}: {read}");
        }
    }

    #[test]
    fn read_with_tail_holds_last_messages_inside_window() {
        let session = WindowedSession::new();

        let unbounded = session.read_tail(2, cli::MessageTimeArgs::default());
        let windowed = session.read_tail(2, window_around_every_timed_message());

        assert_eq!(ordinals_shown(&unbounded), ["m5", "m6"]);
        assert_eq!(ordinals_shown(&windowed), ["m4", "m5"]);
        assert!(header_has(&windowed, "messages-in-window=5"), "{windowed}");
    }

    #[test]
    fn read_with_range_and_window_holds_messages_in_both() {
        let session = WindowedSession::new();

        let both = session.read(":m3..m4", window_bounded_by_m4_and_m5());
        let range_alone = session.read(":m5..m6", cli::MessageTimeArgs::default());

        assert_eq!(ordinals_shown(&both), ["m4"]);
        assert!(header_has(&both, "messages-in-window=1"), "{both}");
        assert_eq!(ordinals_shown(&range_alone), ["m5", "m6"]);
        assert!(!range_alone.contains("messages-in-window"), "{range_alone}");
    }

    #[test]
    fn outline_spans_messages_inside_window() {
        let session = WindowedSession::new();

        for (case, window, expected) in [
            ("across m2", window_across_m2(), vec!["m2"]),
            ("idle", window_between_m4_and_m5(), vec![]),
        ] {
            let outline = session.outline(window);

            assert_eq!(ordinals_shown(&outline), expected, "{case}");
            assert!(header_has(&outline, "messages=6"), "{case}: {outline}");
            let in_window = format!("messages-in-window={}", expected.len());
            assert!(header_has(&outline, &in_window), "{case}: {outline}");
        }
        let unbounded = session.outline(cli::MessageTimeArgs::default());
        assert_eq!(ordinals_shown(&unbounded).len(), 6);
        assert!(!unbounded.contains("messages-in-window"), "{unbounded}");
    }

    /// The focus of each hit of a search or within output.
    fn hit_focuses(output: &str) -> Vec<&str> {
        output
            .lines()
            .filter(|line| line.starts_with("hit "))
            .filter_map(|line| {
                line.split_whitespace()
                    .find_map(|atom| atom.strip_prefix("focus="))
            })
            .collect()
    }

    /// The query `cache` matches m1 to m5; only m4 falls inside the window,
    /// from m4's time to the second before m5's.
    #[test]
    fn search_and_within_hits_come_only_from_messages_inside_window() {
        let dir = tempfile::tempdir().unwrap();
        let path = session_with_times(&dir);
        let (keys, resolved) = resolved_test_conversation(path.clone());
        let conversations = vec![parsed_conversation(&path)];
        let transcript = load_transcript(&path);
        let window = window_args("2026-10-14T15:41:31Z", "2026-10-14T15:41:59Z")
            .resolve(&clock::TestClock::default())
            .unwrap();
        let inputs = [agent::search::AgentConversationInput {
            conversation: &conversations[0],
            resolved: resolved.clone(),
            original_index: 0,
        }];
        let passage = |start, end| {
            semantic_hit_for_test(
                crate::semantic::types::SemanticChunkSource::VisibleDialogue,
                agent::refs::MessageRange { start, end },
                "the cache",
            )
        };
        let passages = [passage(1, 5), passage(1, 2)];
        let ranked = crate::semantic::rank::RankedHits {
            conversations: passages[..1].to_vec(),
            chunks: passages.to_vec(),
        };
        let within = |query: &str, mode: SearchMode| {
            let request = agent::search::AgentWithinRequest {
                query: query.to_string(),
                top: 10,
                cli_mode: Some(mode),
                config_mode: None,
                tui_semantic_search: None,
                budget: None,
                window,
            };
            agent::search::format_agent_output(&agent::search::run_within_search(
                &request,
                &conversations[0],
                &resolved,
                &transcript,
                &passages,
            ))
        };
        let search_request = |mode| agent::search::AgentSearchRequest {
            query: "cache".to_string(),
            top: 10,
            cli_mode: Some(mode),
            config_mode: None,
            tui_semantic_search: None,
            flat: true,
            hits_per_conversation: 10,
            retrieval_hits_per_conversation: None,
            all_hits: true,
            budget: None,
            session_offset: 0,
            window,
        };
        let lexical_search = |mode| {
            agent::search::run_global_lexical_search(
                &search_request(mode),
                &conversations,
                &keys,
                &[0],
                |key| agent::transcript::AgentTranscript::load(&key.path),
            )
            .unwrap()
        };
        let semantic_search = agent::search::run_global_semantic_search(
            &search_request(SearchMode::Semantic),
            &inputs,
            &ranked,
        );
        let hybrid_search = agent::search::run_global_hybrid_search(
            &search_request(SearchMode::Hybrid),
            lexical_search(SearchMode::Lexical),
            &ranked,
            &inputs,
        );

        let cases = [
            ("lexical within", within("cache", SearchMode::Lexical)),
            ("exact within", within("cache", SearchMode::Exact)),
            ("semantic within", within("cache", SearchMode::Semantic)),
            ("hybrid within", within("cache", SearchMode::Hybrid)),
            (
                "lexical search",
                agent::search::format_agent_output(&lexical_search(SearchMode::Lexical)),
            ),
            (
                "exact search",
                agent::search::format_agent_output(&lexical_search(SearchMode::Exact)),
            ),
            (
                "semantic search",
                agent::search::format_agent_output(&semantic_search),
            ),
            (
                "hybrid search",
                agent::search::format_agent_output(&hybrid_search),
            ),
        ];
        for (case, output) in cases {
            let focuses = hit_focuses(&output);
            assert!(!focuses.is_empty(), "{case}: {output}");
            assert!(
                focuses.iter().all(|focus| *focus == "m4..m4"),
                "{case}: {output}"
            );
        }
    }

    #[test]
    fn agent_semantic_candidates_preserve_tool_results_and_select_matching_segments() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_jsonl(
            &dir,
            "12345678-1234-4234-9234-123456789abc.jsonl",
            &[
                user("question"),
                serde_json::json!({
                    "type": "assistant",
                    "timestamp": "2024-01-01T00:00:01Z",
                    "message": {"role": "assistant", "content": [{"type": "tool_use", "id": "toolu_1", "name": "Read", "input": {"file_path": "/tmp/example"}}]}
                })
                .to_string(),
                tool_result_user(&format!(
                    "HEAD{}distinctive_calibration{}TAIL",
                    "x".repeat(5_000),
                    "y".repeat(5_000)
                )),
                assistant("done"),
            ],
        );
        let (keys, resolved) = resolved_test_conversation(path.clone());
        let conversation = parsed_conversation(&path);
        let input = agent::search::AgentConversationInput {
            conversation: &conversation,
            resolved,
            original_index: 0,
        };

        let matching_tools_input = agent::search::AgentConversationInput {
            conversation: &conversation,
            resolved: input.resolved.clone(),
            original_index: 0,
        };
        let unmatched_tools_input = agent::search::AgentConversationInput {
            conversation: &conversation,
            resolved: input.resolved.clone(),
            original_index: 0,
        };
        let tool_call_input = agent::search::AgentConversationInput {
            conversation: &conversation,
            resolved: input.resolved.clone(),
            original_index: 0,
        };
        let candidates = agent_semantic_candidates(&[input]).0;
        let matching_candidates = agent_semantic_candidates_matching_tools(
            &[matching_tools_input],
            "distinctive calibration",
        )
        .0;
        let unmatched_candidates =
            agent_semantic_candidates_matching_tools(&[unmatched_tools_input], "absent needle").0;
        let tool_call_candidates =
            agent_semantic_candidates_matching_tools(&[tool_call_input], "Read example").0;
        let tool = candidates
            .iter()
            .find(|candidate| {
                candidate.source == crate::semantic::types::SemanticChunkSource::AgentTool
            })
            .expect("tool result candidate");

        let matching_tool = matching_candidates
            .iter()
            .find(|candidate| {
                candidate.source == crate::semantic::types::SemanticChunkSource::AgentTool
            })
            .expect("matching tool result candidate");
        let tool_call = tool_call_candidates
            .iter()
            .find(|candidate| {
                candidate.source == crate::semantic::types::SemanticChunkSource::AgentTool
            })
            .expect("matching tool call candidate");

        assert_eq!(keys.len(), 1);
        assert!(tool.conversation.semantic_turns.len() > 1);
        assert!(
            tool.conversation
                .semantic_turns
                .iter()
                .all(|turn| turn.len() <= crate::semantic::types::DEFAULT_CHUNK_TARGET_CHARS)
        );
        let all_tool_text = tool.conversation.semantic_turns.join("");
        assert!(all_tool_text.contains("Read: /tmp/example"));
        assert!(all_tool_text.contains("HEAD"));
        assert!(all_tool_text.contains("TAIL"));
        assert!(all_tool_text.contains("distinctive_calibration"));
        assert_eq!(matching_tool.conversation.semantic_turns.len(), 1);
        assert!(matching_tool.conversation.semantic_turns[0].contains("distinctive_calibration"));
        assert_eq!(
            tool_call.conversation.semantic_turns,
            ["Read: /tmp/example"]
        );
        assert!(
            matching_tool.conversation.semantic_turns[0].len()
                <= crate::semantic::types::DEFAULT_CHUNK_TARGET_CHARS
        );

        let config = crate::semantic::types::ChunkConfig::default();
        let all_chunks = crate::semantic::chunk::build_chunks_with_sources(
            [(0, tool.source, tool.conversation.as_ref())],
            config,
        );
        let matching_chunks = crate::semantic::chunk::build_chunks_with_sources(
            [(0, matching_tool.source, matching_tool.conversation.as_ref())],
            config,
        );
        assert!(
            matching_chunks
                .iter()
                .all(|matching| all_chunks.iter().any(|all| all.text == matching.text))
        );
        assert!(unmatched_candidates.iter().all(|candidate| !matches!(
            candidate.source,
            crate::semantic::types::SemanticChunkSource::AgentTool
                | crate::semantic::types::SemanticChunkSource::AgentSubagentTool
        )));
    }

    #[test]
    fn agent_semantic_tool_matching_sanitizes_text_and_handles_unicode_and_short_terms() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_jsonl(
            &dir,
            "12345678-1234-4234-9234-123456789abc.jsonl",
            &[
                user("question"),
                tool_result_user("prefix \u{1b}[31mÜber UI\u{1b}[0m menuBar suffix"),
                assistant("done"),
            ],
        );
        let (_, resolved) = resolved_test_conversation(path.clone());
        let conversation = parsed_conversation(&path);
        let input = agent::search::AgentConversationInput {
            conversation: &conversation,
            resolved: resolved.clone(),
            original_index: 0,
        };
        let camel_case_input = agent::search::AgentConversationInput {
            conversation: &conversation,
            resolved: resolved.clone(),
            original_index: 0,
        };
        let exact_identifier_input = agent::search::AgentConversationInput {
            conversation: &conversation,
            resolved: resolved.clone(),
            original_index: 0,
        };
        let stop_words_input = agent::search::AgentConversationInput {
            conversation: &conversation,
            resolved,
            original_index: 0,
        };

        let candidates = agent_semantic_candidates_matching_tools(&[input], "über ui").0;
        let camel_case_candidates =
            agent_semantic_candidates_matching_tools(&[camel_case_input], "bar").0;
        let exact_identifier_candidates =
            agent_semantic_candidates_matching_tools(&[exact_identifier_input], "menuBar").0;
        let stop_word_candidates =
            agent_semantic_candidates_matching_tools(&[stop_words_input], "how to the an or if").0;
        let tool = candidates
            .iter()
            .find(|candidate| {
                candidate.source == crate::semantic::types::SemanticChunkSource::AgentTool
            })
            .expect("matching Unicode tool result candidate");

        assert_eq!(
            tool.conversation.semantic_turns,
            ["prefix Über UI menuBar suffix"]
        );
        assert!(camel_case_candidates.iter().any(|candidate| {
            candidate.source == crate::semantic::types::SemanticChunkSource::AgentTool
        }));
        assert!(exact_identifier_candidates.iter().any(|candidate| {
            candidate.source == crate::semantic::types::SemanticChunkSource::AgentTool
        }));
        assert!(stop_word_candidates.iter().all(|candidate| !matches!(
            candidate.source,
            crate::semantic::types::SemanticChunkSource::AgentTool
                | crate::semantic::types::SemanticChunkSource::AgentSubagentTool
        )));
    }

    #[test]
    fn agent_semantic_candidates_include_progress_only_subagent_text() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_jsonl(
            &dir,
            "12345678-1234-4234-9234-123456789abc.jsonl",
            &[
                user("question"),
                r#"{"type":"progress","data":{"type":"agent_progress","agentId":"agent-abcdef","message":{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"progress_only_semantic_needle"}]}}}}"#.to_string(),
                assistant("done"),
            ],
        );
        let key = AgentConversationKey::new(
            "project-a",
            "12345678-1234-4234-9234-123456789abc.jsonl",
            path.clone(),
        );
        let resolved = agent::refs::ResolvedConversation {
            key: key.clone(),
            reference: key.conversation_ref(),
        };
        let conversation = history::format::claude::conversation_from_reader(
            &path,
            std::io::Cursor::new(std::fs::read_to_string(&path).unwrap()),
            history::parser::RowDating::new(None, &clock::TestClock::default()),
            None,
        )
        .unwrap()
        .unwrap();
        assert!(
            !conversation
                .semantic_turns
                .join(" ")
                .contains("progress_only_semantic_needle")
        );
        let input = agent::search::AgentConversationInput {
            conversation: &conversation,
            resolved,
            original_index: 0,
        };

        let candidates = agent_semantic_candidates(&[input]).0;
        let candidate = candidates[1].conversation.as_ref();

        assert_eq!(candidates.len(), 2);
        assert_eq!(
            candidates[0].source,
            crate::semantic::types::SemanticChunkSource::VisibleDialogue
        );
        assert_eq!(
            candidates[1].source,
            crate::semantic::types::SemanticChunkSource::AgentSubagentDialogue
        );
        assert!(
            candidate
                .semantic_turns
                .join(" ")
                .contains("progress_only_semantic_needle")
        );
        assert_eq!(
            candidate.semantic_turn_ranges.last().copied(),
            Some(agent::refs::MessageRange::single(2))
        );
        assert!(
            !conversation
                .semantic_turns
                .join(" ")
                .contains("progress_only_semantic_needle")
        );
    }

    #[test]
    fn agent_semantic_candidates_require_canonical_transcript_projection() {
        let key = AgentConversationKey::new(
            "project-a",
            "missing.jsonl",
            std::path::PathBuf::from("/missing/session.jsonl"),
        );
        let resolved = agent::refs::ResolvedConversation {
            key: key.clone(),
            reference: key.conversation_ref(),
        };
        let conversation = history::Conversation {
            source: history::Source::Claude,
            subagents: Vec::new(),
            session_id: key.session_id.clone(),
            path: key.path.clone(),
            index: 0,
            timestamp: chrono::Local::now(),
            preview: "visible semantic".to_string(),
            preview_first: "visible semantic".to_string(),
            preview_last: "visible semantic".to_string(),
            full_text: "visible semantic".to_string(),
            agent_search_text: String::new(),
            semantic_route_text: String::new(),
            semantic_turns: vec!["visible semantic".to_string()],
            semantic_turn_ranges: vec![agent::refs::MessageRange::single(1)],
            search_text_lower: "visible semantic".to_string(),
            project_name: Some("project-a".to_string()),
            project_path: None,
            cwd: None,
            message_count: 1,
            message_times: history::MessageTimes::default(),
            message_text_starts: history::MessageTextStarts::default(),
            assistant_messages: 1,
            parse_errors: Vec::new(),
            summary: None,
            custom_title: Some("visible semantic".to_string()),
            model: None,
            total_tokens: 0,
            duration_minutes: None,
            fingerprint: None,
            has_transient_read_error: false,
            is_missing_earlier_turns: false,
            cached_session: None,
            thinking_and_tool_call_text: String::new(),
        };
        let input = agent::search::AgentConversationInput {
            conversation: &conversation,
            resolved,
            original_index: 0,
        };

        let (candidates, warnings) = agent_semantic_candidates(&[input]);

        assert!(candidates.is_empty());
        assert_eq!(warnings.len(), 1);
        assert_eq!(
            warnings[0].reference,
            Some(key.conversation_ref().canonical())
        );
    }

    #[test]
    fn agent_semantic_candidates_recover_malformed_transcripts() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_jsonl(
            &dir,
            "12345678-1234-4234-9234-123456789abc.jsonl",
            &[
                user("visible semantic"),
                r#"{"type":"progress","data":{"type":"agent_progress","agentId":"agent-abcdef","message":{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"progress_only_semantic_needle"}]}}}}"#.to_string(),
                "not json".to_string(),
                assistant("done"),
            ],
        );
        let key = AgentConversationKey::new(
            "project-a",
            "12345678-1234-4234-9234-123456789abc.jsonl",
            path.clone(),
        );
        let resolved = agent::refs::ResolvedConversation {
            key: key.clone(),
            reference: key.conversation_ref(),
        };
        let conversation = history::format::claude::conversation_from_reader(
            &path,
            std::io::Cursor::new([
                user("visible semantic"),
                r#"{"type":"progress","data":{"type":"agent_progress","agentId":"agent-abcdef","message":{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"progress_only_semantic_needle"}]}}}}"#.to_string(),
                "not json".to_string(),
                assistant("done"),
            ].join("\n")),
            history::parser::RowDating::new(None, &clock::TestClock::default()),
            None,
        )
        .unwrap()
        .unwrap();
        assert!(
            conversation
                .agent_search_text
                .contains("progress_only_semantic_needle")
        );
        let input = agent::search::AgentConversationInput {
            conversation: &conversation,
            resolved,
            original_index: 0,
        };

        let (candidates, warnings) = agent_semantic_candidates(&[input]);

        assert_eq!(candidates.len(), 2);
        assert_eq!(warnings.len(), 1);
        assert_eq!(
            warnings[0].kind,
            agent::diagnostic::AgentWarningKind::MalformedTranscript
        );
        assert_eq!(
            candidates[0].source,
            crate::semantic::types::SemanticChunkSource::VisibleDialogue
        );
        assert_eq!(
            candidates[1].source,
            crate::semantic::types::SemanticChunkSource::AgentSubagentDialogue
        );
        assert!(
            candidates[1]
                .conversation
                .semantic_turns
                .join(" ")
                .contains("progress_only_semantic_needle")
        );
    }

    #[test]
    fn semantic_progress_hit_read_recipe_enables_subagents() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_jsonl(
            &dir,
            "12345678-1234-4234-9234-123456789abc.jsonl",
            &[
                user("question"),
                progress_text_message("progress_only_semantic_needle"),
                assistant("done"),
            ],
        );
        let (keys, resolved) = resolved_test_conversation(path.clone());
        let conversation = parsed_conversation(&path);
        let transcript = load_transcript(&resolved.key.path);
        let semantic_hit = semantic_hit_for_test(
            crate::semantic::types::SemanticChunkSource::AgentSubagentDialogue,
            agent::refs::MessageRange::single(2),
            "progress_only_semantic_needle",
        );
        let within_request = agent::search::AgentWithinRequest {
            query: "progress semantic".to_string(),
            top: 1,
            cli_mode: Some(SearchMode::Semantic),
            config_mode: None,
            tui_semantic_search: None,
            budget: None,
            window: time_filter::TimeFilter::default(),
        };
        let rendered = agent::search::format_agent_output(&agent::search::run_within_search(
            &within_request,
            &conversation,
            &resolved,
            &transcript,
            &[semantic_hit],
        ));

        assert!(rendered.contains("focus=m2..m2"));
        assert!(rendered.contains("tools=false tool-results=false thinking=false subagents=true"));
        let read_line = rendered
            .lines()
            .find(|line| line.starts_with("read ref="))
            .expect("within output should include a read ref");
        let read_output = run_agent_read(&read_args_from_line(read_line), Some(&keys)).unwrap();
        assert!(read_output.contains("progress_only_semantic_needle"));
    }

    #[test]
    fn read_command_rejects_out_of_range_loaded_messages() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_jsonl(
            &dir,
            "12345678-1234-4234-9234-123456789abc.jsonl",
            &[user("question")],
        );
        let keys = vec![AgentConversationKey::new(
            "project-a",
            "12345678-1234-4234-9234-123456789abc.jsonl",
            path,
        )];
        let conversation = keys[0].conversation_ref().canonical();
        let args = read_args(vec![format!("{conversation}:m1..m2")], None);

        let err = run_agent_read(&args, Some(&keys)).unwrap_err();

        assert!(err.to_string().contains("exceeds transcript length"));
    }
}
