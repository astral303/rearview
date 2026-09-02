//! Claude conversation history loading and parsing.
//!
//! This module provides functionality for:
//! - Loading conversations from Claude project directories
//! - Parsing JSONL conversation files
//! - Encoding/decoding project directory paths
//!
//! # Module Structure
//!
//! - `loader` - Loading conversations from directories
//! - `parser` - Parsing individual JSONL files
//! - `path` - Path encoding/decoding utilities

pub mod cache;
mod filter;
pub mod format;
mod loader;
pub mod omp_loader;
pub mod parser;
pub mod path;
pub mod pi_loader;
pub mod provider;
mod rename;
mod workspace;

use crate::error::{AppError, Result};
use chrono::{DateTime, Local};
use std::path::PathBuf;
use std::time::SystemTime;

// Re-export public API
pub use filter::{FilterTerm, HistoryFilter, active_load_filters};
pub use loader::{
    DeleteEmptyScope, EmptySession, LoadedHistory, delete_empty_sessions, delete_session_by_uuid,
    find_jsonl_by_uuid, load_all_conversations, load_all_conversations_streaming, load_history,
};
pub(crate) use parser::{
    extract_skill_preview, is_clear_metadata_message, process_conversation_file,
};
pub use path::{convert_path_to_project_dir_name, format_short_name_from_path, is_same_project};
pub use rename::append_session_rename;
pub use workspace::Workspace;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Source {
    Claude,
    Pi,
    Omp,
    Codex,
    Kimi,
    OpenCode,
}

impl Source {
    pub fn label(self) -> &'static str {
        self.provider().labels().name
    }

    pub fn list_label(self) -> &'static str {
        self.provider().labels().list
    }

    pub fn display_label(self) -> &'static str {
        self.provider().labels().display
    }
}

/// The entries of the session at `path` as `source` records it: normalized,
/// with the sub-agent transcripts at `subagents` — the row's, as discovery
/// named them — spliced in.
///
/// Only `source`'s own format reads the locator — a session the list already
/// attributed to a provider never meets a foreign format, which is what lets a
/// provider's locators be something other than openable files.
pub fn normalized_log_entries(
    source: Source,
    path: &std::path::Path,
    subagents: &[PathBuf],
) -> Result<Vec<(usize, crate::log_entry::LogEntry)>> {
    let projection = match source.provider().format() {
        Some(format) => format::view_projection(format, path, subagents)?,
        None => None,
    };
    if let Some(projection) = projection {
        return Ok(projection.entries);
    }
    raw_log_entries(path)
}

/// [`normalized_log_entries`] for a bare file nothing has attributed —
/// `--render` and direct path arguments. The first registered format that
/// recognizes the file wins; a file no format claims is read as a raw Claude
/// transcript.
pub fn sniffed_log_entries(
    path: &std::path::Path,
) -> Result<Vec<(usize, crate::log_entry::LogEntry)>> {
    if let Some(projection) = format::sniffed_view_projection(path)? {
        return Ok(projection.entries);
    }
    raw_log_entries(path)
}

/// Claude records [`LogEntry`](crate::log_entry::LogEntry) values directly, one
/// per line; only the canonical tool of each tool call is added afterwards.
fn raw_log_entries(path: &std::path::Path) -> Result<Vec<(usize, crate::log_entry::LogEntry)>> {
    let file = std::fs::File::open(path)?;
    let reader = std::io::BufReader::new(file);
    use std::io::BufRead;
    let mut entries = Vec::new();
    for (line_index, line) in reader.lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(mut entry) = serde_json::from_str(&line) {
            provider::assign_canonical_tools(&mut entry);
            entries.push((line_index + 1, entry));
        }
    }
    Ok(entries)
}

/// Represents a JSONL parsing error with context for debugging
#[derive(Clone, Debug)]
pub struct ParseError {
    pub line_number: usize,
    pub line_content: String,
    pub error_message: String,
    /// Lines before the error (up to 2)
    pub context_before: Vec<String>,
    /// Lines after the error (up to 2)
    pub context_after: Vec<String>,
}

#[derive(Clone)]
pub struct Conversation {
    pub source: Source,
    pub session_id: String,
    /// The sub-agent transcripts merged into this row, spliced into its
    /// view. Empty for agents that keep sub-agent turns inside the transcript
    /// itself.
    pub subagents: Vec<PathBuf>,
    pub path: PathBuf,
    pub index: usize,
    pub timestamp: DateTime<Local>,
    pub preview: String,
    /// Preview showing first 3 messages (used when show_last=false)
    pub preview_first: String,
    /// Preview showing last 3 messages (used when show_last=true)
    pub preview_last: String,
    pub full_text: String,
    pub agent_search_text: String,
    pub semantic_route_text: String,
    pub semantic_turns: Vec<String>,
    pub semantic_turn_ranges: Vec<crate::agent::refs::MessageRange>,
    /// Pre-normalized lowercase search text (avoids re-normalizing on every startup)
    pub search_text_lower: String,
    pub project_name: Option<String>,
    pub project_path: Option<PathBuf>,
    /// The working directory extracted from the JSONL file (the actual cwd)
    pub cwd: Option<PathBuf>,
    /// Number of user and assistant messages in the conversation
    pub message_count: usize,
    /// The agent's share of `message_count`. Zero is a session that was
    /// started and never answered, which `delete-empty` collects.
    pub assistant_messages: usize,
    /// Parse errors encountered while processing this conversation file
    pub parse_errors: Vec<ParseError>,
    /// Summary/title of the conversation (from type=summary JSONL entry)
    pub summary: Option<String>,
    /// Custom session title set by user via /rename (from type=custom-title JSONL entry)
    pub custom_title: Option<String>,
    /// Model name from assistant messages (e.g., "claude-opus-4-5-20251101")
    pub model: Option<String>,
    /// Total tokens used in the conversation (input + output + cache)
    pub total_tokens: u64,
    /// Conversation duration in minutes (from first to last message)
    pub duration_minutes: Option<u64>,
}

pub(crate) fn semantic_route_text(full_text: &str, agent_search_text: &str) -> String {
    const ROUTE_EVIDENCE_CHARS: usize = 1_000;
    const ROUTE_EVIDENCE_SEGMENTS: usize = 4;
    const ROUTE_KEYWORDS: usize = 100;
    const ROUTE_KEYWORD_CHARS: usize = 800;

    let mut text = full_text.to_string();
    if !agent_search_text.is_empty() {
        text.push(' ');
        text.push_str(agent_search_text);
    }
    if text.is_empty() {
        return String::new();
    }
    let keywords = semantic_route_keywords(&text, ROUTE_KEYWORDS)
        .chars()
        .take(ROUTE_KEYWORD_CHARS)
        .collect::<String>();
    let excerpt = evenly_spaced_excerpt(&text, ROUTE_EVIDENCE_CHARS, ROUTE_EVIDENCE_SEGMENTS);
    [keywords, excerpt]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

fn semantic_route_keywords(text: &str, limit: usize) -> String {
    const STOP_WORDS: &[&str] = &[
        "about", "after", "also", "been", "before", "being", "could", "does", "from", "have",
        "into", "just", "more", "only", "other", "should", "some", "than", "that", "their",
        "there", "these", "they", "this", "through", "using", "very", "what", "when", "where",
        "which", "while", "with", "would", "your",
    ];

    let normalized = text.to_lowercase();
    let mut words = std::collections::HashMap::<&str, (usize, usize)>::new();
    for (position, word) in normalized
        .split(|character: char| !character.is_alphanumeric())
        .filter(|word| {
            let chars = word.chars().count();
            (4..=40).contains(&chars) && !STOP_WORDS.contains(word)
        })
        .enumerate()
    {
        let entry = words.entry(word).or_insert((0, position));
        entry.0 += 1;
    }
    let mut ranked = words.into_iter().collect::<Vec<_>>();
    ranked.sort_unstable_by(|(left_word, left), (right_word, right)| {
        right
            .0
            .cmp(&left.0)
            .then_with(|| right_word.chars().count().cmp(&left_word.chars().count()))
            .then_with(|| left.1.cmp(&right.1))
    });
    ranked
        .into_iter()
        .take(limit)
        .map(|(word, _)| word)
        .collect::<Vec<_>>()
        .join(" ")
}

fn evenly_spaced_excerpt(text: &str, max_chars: usize, segments: usize) -> String {
    let char_count = text.chars().count();
    if char_count <= max_chars || segments <= 1 {
        return text.chars().take(max_chars).collect();
    }
    let segment_chars = max_chars / segments;
    let last_start = char_count.saturating_sub(segment_chars);
    (0..segments)
        .map(|index| {
            let start = last_start * index / (segments - 1);
            text.chars()
                .skip(start)
                .take(segment_chars)
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n...\n")
}

pub struct Project {
    pub name: String,         // directory name (encoded)
    pub display_name: String, // heuristic decoded path
    pub modified: SystemTime,
}

/// Message sent from background loader to TUI
pub enum LoaderMessage {
    /// A fatal error occurred (e.g., projects root doesn't exist)
    Fatal(AppError),
    /// A non-fatal error occurred (project-level, error already logged)
    ProjectError,
    /// The conversations of one provider, or of one Claude project
    Batch(Vec<Conversation>),
    /// How far the loader is through the source it is on
    Progress(LoadProgress),
    /// A term for sessions one provider found under a root but ignores, so
    /// the list can show why it holds less than the disk does
    Ignored(FilterTerm),
    /// Loading completed
    Done,
}

/// How far the loader is through one source: `done` of `total` units restored
/// or parsed. Sent with `done == 0` as soon as the total is known, when the
/// source completes, and at most a few times a second in between.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LoadProgress {
    pub source: Source,
    pub done: usize,
    pub total: usize,
    pub unit: LoadUnit,
}

/// What a [`LoadProgress`] counts. Providers with session roots count
/// sessions; Claude is loaded one project directory at a time.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LoadUnit {
    Sessions,
    Projects,
}

/// Get the root Claude projects directory (~/.claude/projects)
/// Respects CLAUDE_CONFIG_DIR env variable if set.
pub fn get_claude_projects_root() -> Result<PathBuf> {
    let claude_dir = if let Ok(config_dir) = std::env::var("CLAUDE_CONFIG_DIR") {
        PathBuf::from(config_dir)
    } else {
        let home_dir = home::home_dir().ok_or_else(|| {
            AppError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "Could not determine home directory",
            ))
        })?;
        home_dir.join(".claude")
    };

    Ok(claude_dir.join("projects"))
}

/// Get the Claude projects directory for the current working directory
pub fn get_claude_projects_dir(current_dir: &std::path::Path) -> Result<PathBuf> {
    let converted = convert_path_to_project_dir_name(current_dir);
    Ok(get_claude_projects_root()?.join(converted))
}
