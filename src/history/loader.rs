//! Conversation loading and project discovery.
//!
//! This module handles loading conversations from Claude project directories,
//! both synchronously and via streaming for the TUI.

use super::cache;
use super::parser::process_conversation_file;
use super::path::{
    decode_project_dir_name, decode_project_dir_name_to_path, format_short_name_from_path,
};
use super::{
    Conversation, FilterTerm, LoadProgress, LoadUnit, LoaderMessage, Project, Source, Workspace,
};
use crate::cli::DebugLevel;
use crate::debug;
use crate::error::{AppError, Result};
use crate::time_filter::TimeFilter;
use chrono::{DateTime, Local};
use rayon::prelude::*;
use std::collections::{HashMap, HashSet};
use std::fs::read_dir;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

/// How often the streaming loader passes progress on to the TUI. Every report
/// redraws the status line, and a fast provider reports hundreds of times a
/// second.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(250);

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum DeleteEmptyScope {
    All,
    Local,
}

/// A session that was started and never answered.
#[derive(Debug, Clone)]
pub struct EmptySession {
    pub source: Source,
    pub path: PathBuf,
    pub session_id: String,
    pub project_name: String,
    pub timestamp: DateTime<Local>,
    pub preview: Option<String>,
    pub user_messages: usize,
}

impl From<Conversation> for EmptySession {
    fn from(conversation: Conversation) -> Self {
        Self {
            source: conversation.source,
            path: conversation.path,
            session_id: conversation.session_id,
            project_name: conversation
                .project_name
                .unwrap_or_else(|| "(none)".to_owned()),
            timestamp: conversation.timestamp,
            preview: (!conversation.preview.trim().is_empty()).then_some(conversation.preview),
            user_messages: conversation.message_count - conversation.assistant_messages,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct DeleteEmptySummary {
    pub candidates: Vec<EmptySession>,
    pub deleted: usize,
}

/// What loading the providers that keep sessions under roots found out.
///
/// Claude is loaded separately and treated as the primary history: it streams per
/// project, and its projects directory is what the callers below fall back on.
/// Whether these providers found anything decides whether a missing Claude
/// directory is fatal or merely uninteresting.
struct AuxiliaryHistory {
    /// One entry per provider that failed, in registration order. Kept per
    /// provider because each is reported under its own name.
    failures: Vec<(Source, AppError)>,
    /// At least one provider has a session root on disk that loaded cleanly.
    usable: bool,
}

impl AuxiliaryHistory {
    /// Load every provider in registration order. Each provider's sessions reach
    /// `report` as one `Batch` the moment that provider completes, after a
    /// `Progress` for every session, so a caller can show the load as it
    /// happens rather than after the slowest provider. An `Ignored` precedes
    /// the batch for each reason the provider ignored sessions for.
    fn load(
        show_last: bool,
        debug_level: Option<DebugLevel>,
        report: &mut dyn FnMut(LoaderMessage),
    ) -> Self {
        let mut history = Self {
            failures: Vec::new(),
            usable: false,
        };
        for provider in super::provider::providers() {
            let Some(storage) = provider.storage() else {
                continue;
            };
            let root_on_disk = storage
                .roots()
                .is_ok_and(|roots| roots.iter().any(|root| root.path.exists()));
            let source = provider.source();
            let loaded = super::provider::load_sessions(
                storage,
                show_last,
                debug_level,
                &mut |done, total| {
                    report(LoaderMessage::Progress(LoadProgress {
                        source,
                        done,
                        total,
                        unit: LoadUnit::Sessions,
                    }));
                },
            );
            match loaded {
                Ok(loaded) => {
                    history.usable |= root_on_disk;
                    for term in loaded.ignored {
                        debug::warn(debug_level, &term.to_string());
                        report(LoaderMessage::Ignored(term));
                    }
                    if !loaded.conversations.is_empty() {
                        report(LoaderMessage::Batch(loaded.conversations));
                    }
                }
                Err(error) => {
                    if let Some(term) = sessions_not_loaded_term(source, &error) {
                        report(LoaderMessage::Ignored(term));
                    }
                    history.failures.push((source, error));
                }
            }
        }
        history
    }

    /// The failure to report when nothing loaded at all. A real cause beats the
    /// generic "projects directory not found" the caller would otherwise raise.
    fn take_first_failure(&mut self) -> Option<AppError> {
        if self.failures.is_empty() {
            return None;
        }
        Some(self.failures.remove(0).1)
    }

    fn failure_reports(&self) -> impl Iterator<Item = String> + '_ {
        self.failures.iter().map(|(source, error)| {
            format!("Failed to load {} history: {error}", source.display_label())
        })
    }
}

/// The list's term for a provider whose session list is present but could
/// not be read, `Codex │ session database locked: sessions not loaded`, so
/// the list shows why it holds none of that provider's sessions. `None` for
/// any other failure, which `--debug` alone reports.
fn sessions_not_loaded_term(source: Source, error: &AppError) -> Option<FilterTerm> {
    match error {
        AppError::SessionListUnreadable { reason, .. } => Some(FilterTerm::new(
            source.display_label(),
            format!("{reason}: sessions not loaded"),
        )),
        _ => None,
    }
}

/// Every agent's conversations, and what the load found but ignores.
pub struct LoadedHistory {
    pub conversations: Vec<Conversation>,
    /// One term per reason an agent's sessions were ignored for, named for
    /// the user.
    pub ignored: Vec<FilterTerm>,
}

/// The conversations of [`load_history`], for callers with no use for the
/// ignored terms.
pub fn load_all_conversations(
    show_last: bool,
    debug_level: Option<DebugLevel>,
) -> Result<Vec<Conversation>> {
    Ok(load_history(show_last, debug_level)?.conversations)
}

/// Every agent's conversations from every project, and what the load found
/// but ignores.
pub fn load_history(show_last: bool, debug_level: Option<DebugLevel>) -> Result<LoadedHistory> {
    let mut auxiliary_conversations = Vec::new();
    let mut ignored = Vec::new();
    let mut auxiliary =
        AuxiliaryHistory::load(show_last, debug_level, &mut |message| match message {
            LoaderMessage::Batch(conversations) => auxiliary_conversations.extend(conversations),
            LoaderMessage::Ignored(term) => ignored.push(term),
            _ => {}
        });
    let conversations = claude_and_auxiliary_conversations(
        &mut auxiliary,
        auxiliary_conversations,
        show_last,
        debug_level,
    )?;
    Ok(LoadedHistory {
        conversations,
        ignored,
    })
}

/// Claude's conversations from every project joined to the auxiliary ones,
/// or the auxiliary ones alone when Claude's projects directory is absent and
/// another agent's history loaded.
fn claude_and_auxiliary_conversations(
    auxiliary: &mut AuxiliaryHistory,
    mut auxiliary_conversations: Vec<Conversation>,
    show_last: bool,
    debug_level: Option<DebugLevel>,
) -> Result<Vec<Conversation>> {
    let root = match super::get_claude_projects_root() {
        Ok(root) => root,
        Err(error) => {
            if auxiliary.usable {
                finalize_conversations(&mut auxiliary_conversations);
                return Ok(auxiliary_conversations);
            }
            return Err(auxiliary.take_first_failure().unwrap_or(error));
        }
    };
    if !root.exists() {
        if auxiliary.usable {
            return Ok(auxiliary_conversations);
        }
        if let Some(error) = auxiliary.take_first_failure() {
            return Err(error);
        }
        return Err(AppError::ProjectsDirNotFound(root.display().to_string()));
    }
    for report in auxiliary.failure_reports() {
        debug::warn(debug_level, &report);
    }
    let projects = list_projects(&root)?;

    debug::info(
        debug_level,
        &format!("Loading global history from {} projects", projects.len()),
    );

    // Load conversations from all projects in parallel
    let mut all_conversations: Vec<Conversation> = projects
        .par_iter()
        .flat_map(|project| {
            load_project(&root, project, show_last, debug_level).unwrap_or_else(|e| {
                debug::warn(
                    debug_level,
                    &format!("Failed to load project {}: {}", project.display_name, e),
                );
                Vec::new()
            })
        })
        .collect();

    all_conversations.append(&mut auxiliary_conversations);
    finalize_conversations(&mut all_conversations);

    debug::info(
        debug_level,
        &format!(
            "Total global conversations loaded: {}",
            all_conversations.len()
        ),
    );

    Ok(all_conversations)
}

fn finalize_conversations(conversations: &mut Vec<Conversation>) {
    deduplicate_conversations(conversations);
    conversations.sort_by(|left, right| right.timestamp.cmp(&left.timestamp));
    for (index, conversation) in conversations.iter_mut().enumerate() {
        conversation.index = index;
    }
}

fn deduplicate_conversations(conversations: &mut Vec<Conversation>) {
    SeenPaths::default().retain_unseen(conversations);
}

/// The files already listed, so a session reachable through two roots, or
/// through two providers sharing a redirected directory, appears once: the
/// first to load keeps the row.
#[derive(Default)]
struct SeenPaths(HashSet<PathBuf>);

impl SeenPaths {
    fn retain_unseen(&mut self, conversations: &mut Vec<Conversation>) {
        conversations.retain(|conversation| {
            let path = conversation
                .path
                .canonicalize()
                .unwrap_or_else(|_| conversation.path.clone());
            self.0.insert(path)
        });
    }
}

/// One progress report per interval, plus the ones that announce a total and
/// complete it: the status line shows where a source started and that it
/// finished, however fast it loaded.
struct ProgressThrottle {
    interval: Duration,
    last_sent: Option<Instant>,
}

impl ProgressThrottle {
    fn new(interval: Duration) -> Self {
        Self {
            interval,
            last_sent: None,
        }
    }

    fn admit(&mut self, done: usize, total: usize, now: Instant) -> bool {
        let due = match self.last_sent {
            None => true,
            Some(last) => done == 0 || done == total || now.duration_since(last) >= self.interval,
        };
        if due {
            self.last_sent = Some(now);
        }
        due
    }
}

/// Claude's progress, reported from parallel project loads. Behind a mutex,
/// a count and its report cannot interleave with another project's.
struct ProjectProgress<'a> {
    sink: &'a Sender<LoaderMessage>,
    done: usize,
    total: usize,
    throttle: ProgressThrottle,
}

impl ProjectProgress<'_> {
    fn report(&mut self, now: Instant) {
        if self.throttle.admit(self.done, self.total, now) {
            let _ = self.sink.send(LoaderMessage::Progress(LoadProgress {
                source: Source::Claude,
                done: self.done,
                total: self.total,
                unit: LoadUnit::Projects,
            }));
        }
    }

    fn project_done(&mut self, now: Instant) {
        self.done += 1;
        self.report(now);
    }
}

/// Start loading all conversations in the background
/// Returns a receiver that will receive LoaderMessage updates
pub fn load_all_conversations_streaming(
    show_last: bool,
    debug_level: Option<DebugLevel>,
    time: TimeFilter,
) -> Receiver<LoaderMessage> {
    let (tx, rx) = mpsc::channel();

    thread::spawn(move || {
        load_all_streaming_inner(tx, show_last, debug_level, time);
    });

    rx
}

fn load_all_streaming_inner(
    tx: Sender<LoaderMessage>,
    show_last: bool,
    debug_level: Option<DebugLevel>,
    time: TimeFilter,
) {
    let mut seen = SeenPaths::default();
    let mut throttle = ProgressThrottle::new(PROGRESS_INTERVAL);
    let mut auxiliary = AuxiliaryHistory::load(show_last, debug_level, &mut |message| {
        let message = match message {
            LoaderMessage::Batch(mut conversations) => {
                seen.retain_unseen(&mut conversations);
                if time.is_active() {
                    conversations.retain(|conversation| time.matches(conversation.timestamp));
                }
                if conversations.is_empty() {
                    return;
                }
                LoaderMessage::Batch(conversations)
            }
            LoaderMessage::Progress(progress) => {
                if !throttle.admit(progress.done, progress.total, Instant::now()) {
                    return;
                }
                LoaderMessage::Progress(progress)
            }
            message => message,
        };
        let _ = tx.send(message);
    });

    let root = match super::get_claude_projects_root() {
        Ok(root) => root,
        Err(error) => {
            if auxiliary.usable {
                let _ = tx.send(LoaderMessage::Done);
            } else {
                let _ = tx.send(LoaderMessage::Fatal(
                    auxiliary.take_first_failure().unwrap_or(error),
                ));
            }
            return;
        }
    };

    if !root.exists() {
        if auxiliary.usable {
            let _ = tx.send(LoaderMessage::Done);
        } else {
            let error = auxiliary
                .take_first_failure()
                .unwrap_or_else(|| AppError::ProjectsDirNotFound(root.display().to_string()));
            let _ = tx.send(LoaderMessage::Fatal(error));
        }
        return;
    }

    for report in auxiliary.failure_reports() {
        debug::warn(debug_level, &report);
        let _ = tx.send(LoaderMessage::ProjectError);
    }

    let projects = match list_projects(&root) {
        Ok(p) => p,
        Err(error) => {
            if auxiliary.usable {
                let _ = tx.send(LoaderMessage::Done);
            } else {
                let _ = tx.send(LoaderMessage::Fatal(error));
            }
            return;
        }
    };

    debug::info(
        debug_level,
        &format!("Loading global history from {} projects", projects.len()),
    );

    let mut progress = ProjectProgress {
        sink: &tx,
        done: 0,
        total: projects.len(),
        throttle,
    };
    progress.report(Instant::now());
    let progress = Mutex::new(progress);

    // Process projects in parallel and send batches as they complete
    projects.par_iter().for_each(|project| {
        match load_project(&root, project, show_last, debug_level) {
            Ok(mut conversations) => {
                // Filtered here rather than inside load_conversations, whose
                // per-project cache is rebuilt from the vec it returns —
                // dropping conversations earlier would evict their cache
                // entries and force a re-parse on every later run.
                if time.is_active() {
                    conversations.retain(|conversation| time.matches(conversation.timestamp));
                }
                if !conversations.is_empty() {
                    // Send batch, ignore error if receiver dropped
                    let _ = tx.send(LoaderMessage::Batch(conversations));
                }
            }
            Err(e) => {
                debug::warn(
                    debug_level,
                    &format!("Failed to load project {}: {}", project.display_name, e),
                );
                let _ = tx.send(LoaderMessage::ProjectError);
            }
        }
        progress.lock().unwrap().project_done(Instant::now());
    });

    let _ = tx.send(LoaderMessage::Done);
}

/// One Claude project's conversations, attributed to the project: a
/// transcript's own cwd, or for one recorded without it, the path decoded
/// from the project directory's name.
fn load_project(
    root: &Path,
    project: &Project,
    show_last: bool,
    debug_level: Option<DebugLevel>,
) -> Result<Vec<Conversation>> {
    let project_dir = root.join(&project.name);
    let mut conversations =
        load_conversations(&project_dir, show_last, &project.name, debug_level)?;

    let fallback_path = decode_project_dir_name_to_path(&project.name);
    for conversation in &mut conversations {
        let project_path = conversation
            .cwd
            .clone()
            .unwrap_or_else(|| fallback_path.clone());
        conversation.project_name = Some(format_short_name_from_path(&project_path));
        conversation.project_path = Some(project_path);
    }
    Ok(conversations)
}

/// Find a session JSONL file by UUID across all projects.
/// Returns the path to the `.jsonl` file if found.
pub fn find_jsonl_by_uuid(uuid: &str) -> Result<Option<PathBuf>> {
    let matches = find_all_jsonl_by_uuid(uuid)?;
    Ok(matches.into_iter().next())
}

/// Find all session JSONL files by UUID across all projects.
/// A session may exist in multiple project directories due to cross-project forking.
fn find_all_jsonl_by_uuid(uuid: &str) -> Result<Vec<PathBuf>> {
    let root = super::get_claude_projects_root()?;
    if !root.exists() {
        return Ok(Vec::new());
    }

    let filename = format!("{}.jsonl", uuid);
    let mut matches = Vec::new();

    for entry in read_dir(&root)? {
        let entry = entry?;
        let project_dir = entry.path();
        if !project_dir.is_dir() {
            continue;
        }
        let candidate = project_dir.join(&filename);
        if candidate.exists() {
            matches.push(candidate);
        }
    }

    Ok(matches)
}

/// Delete a session by UUID across all projects.
/// Removes both the .jsonl file and the session subdirectory (tool-results/, subagents/).
/// Returns the number of files deleted.
pub fn delete_session_by_uuid(uuid: &str) -> Result<usize> {
    // Validate format to prevent path traversal
    if uuid.is_empty() || !uuid.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return Err(AppError::SessionNotFound(uuid.to_owned()));
    }

    let matches = find_all_jsonl_by_uuid(uuid)?;
    if matches.is_empty() {
        return Err(AppError::SessionNotFound(uuid.to_owned()));
    }

    let count = matches.len();
    for jsonl_path in &matches {
        std::fs::remove_file(jsonl_path)?;

        // Also remove the session subdirectory if it exists
        if let Some(project_dir) = jsonl_path.parent() {
            let session_dir = project_dir.join(uuid);
            if session_dir.is_dir() {
                std::fs::remove_dir_all(&session_dir)?;
            }
        }
    }

    Ok(count)
}

/// Every session that was started and never answered, newest first.
///
/// Loads the whole corpus. Emptiness is a property of the parsed session, and
/// one rule read there covers every agent.
pub fn find_empty_sessions(scope: DeleteEmptyScope) -> Result<Vec<EmptySession>> {
    let workspace = match scope {
        DeleteEmptyScope::All => None,
        DeleteEmptyScope::Local => Some(Workspace::current()?),
    };

    let mut empty = load_all_conversations(false, None)?
        .into_iter()
        .filter(|conversation| conversation.assistant_messages == 0)
        .filter(|conversation| {
            workspace
                .as_ref()
                .is_none_or(|workspace| workspace.contains(conversation))
        })
        .map(EmptySession::from)
        .collect::<Vec<_>>();

    empty.sort_by(|a, b| b.timestamp.cmp(&a.timestamp).then(a.path.cmp(&b.path)));
    Ok(empty)
}

/// Remove every empty session `scope` covers, or list them when `delete` is
/// false.
///
/// Each goes through the agent that recorded it, so a Codex thread's older
/// rollouts and a Kimi session's directory go with it.
pub fn delete_empty_sessions(scope: DeleteEmptyScope, delete: bool) -> Result<DeleteEmptySummary> {
    let candidates = find_empty_sessions(scope)?;
    let mut deleted = 0;

    if delete {
        for session in &candidates {
            session.source.provider().delete_session(&session.path)?;
            deleted += 1;
        }
    }

    Ok(DeleteEmptySummary {
        candidates,
        deleted,
    })
}

/// List all projects that contain conversation files
pub fn list_projects(root: &Path) -> Result<Vec<Project>> {
    let entries = read_dir(root)?;

    let mut projects: Vec<Project> = entries
        .par_bridge()
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let path = entry.path();

            if !path.is_dir() {
                return None;
            }

            // Check if project has any non-agent .jsonl files
            let has_conversations = read_dir(&path).ok()?.any(|e| {
                e.ok()
                    .map(|e| {
                        let path = e.path();
                        let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
                        path.extension().map(|s| s == "jsonl").unwrap_or(false)
                            && !name.starts_with("agent-")
                    })
                    .unwrap_or(false)
            });

            if !has_conversations {
                return None;
            }

            let name = path.file_name()?.to_string_lossy().to_string();
            // Heuristic decode: convert encoded directory name back to readable path
            // The encoding replaces non-alphanumeric chars (except -) with -
            // So / becomes -, but _ also becomes -, and __ becomes --
            // We convert single dashes to / but preserve double dashes as _
            let display_name = decode_project_dir_name(&name);
            let modified = entry
                .metadata()
                .ok()?
                .modified()
                .ok()
                .unwrap_or(SystemTime::UNIX_EPOCH);

            Some(Project {
                name,
                display_name,
                modified,
            })
        })
        .collect();

    // Sort by recently modified
    projects.sort_by(|a, b| b.modified.cmp(&a.modified));

    Ok(projects)
}

/// Find and process all conversation files in one pass, using per-project cache
pub fn load_conversations(
    projects_dir: &Path,
    show_last: bool,
    project_dir_name: &str,
    debug_level: Option<DebugLevel>,
) -> Result<Vec<Conversation>> {
    // Load existing cache for this project
    let cached_entries = cache::read_project_cache(project_dir_name).unwrap_or_default();

    // Find all JSONL files and capture metadata in one pass
    let mut files_with_meta = Vec::new();
    let mut skipped_agent_files = 0;

    for entry in read_dir(projects_dir)? {
        let entry = entry?;
        let path = entry.path();

        if path.extension().and_then(|s| s.to_str()) == Some("jsonl") {
            if let Some(filename) = path.file_name().and_then(|f| f.to_str())
                && filename.starts_with("agent-")
            {
                skipped_agent_files += 1;
                debug::debug(debug_level, &format!("Skipping agent file: {}", filename));
                continue;
            }

            let metadata = entry.metadata().ok();
            let modified = metadata.as_ref().and_then(|m| m.modified().ok());
            let file_size = metadata.as_ref().map(|m| m.len()).unwrap_or(0);

            files_with_meta.push((path, modified, file_size));
        }
    }

    debug::info(
        debug_level,
        &format!(
            "Found {} conversation files ({} agent files skipped)",
            files_with_meta.len(),
            skipped_agent_files
        ),
    );

    // Sort by modification time (newest first)
    files_with_meta.sort_by_key(|(_, modified, _)| modified.unwrap_or(SystemTime::UNIX_EPOCH));
    files_with_meta.reverse();

    // Partition into cache hits and misses
    let mut dirty = false;
    let mut conversations: Vec<Conversation> = Vec::with_capacity(files_with_meta.len());
    let mut files_to_parse: Vec<(PathBuf, Option<SystemTime>, u64)> = Vec::new();

    for (path, modified, file_size) in &files_with_meta {
        let filename = path
            .file_name()
            .and_then(|f| f.to_str())
            .unwrap_or("unknown");

        if let Some(mtime) = modified
            && let Some(entry) = cached_entries.get(filename)
            && entry.fingerprint().matches(*file_size, *mtime)
        {
            match entry {
                cache::ProjectCacheEntry::Empty(_) => {
                    // Negative cache hit — file was previously parsed and yielded nothing
                    debug::debug(debug_level, &format!("Cache hit (empty) {}", filename));
                }
                cache::ProjectCacheEntry::Listed { conversation, .. } => {
                    let conv =
                        cache::conversation_from_cached(conversation, path.clone(), show_last);
                    debug::debug(
                        debug_level,
                        &format!("Cache hit {}: {}", filename, conv.preview),
                    );
                    conversations.push(conv);
                }
            }
        } else {
            dirty = true;
            files_to_parse.push((path.clone(), *modified, *file_size));
        }
    }

    if !dirty && files_with_meta.len() != cached_entries.len() {
        // Files were deleted — need to rewrite cache to remove stale entries
        dirty = true;
    }

    debug::info(
        debug_level,
        &format!(
            "Cache: {} hits, {} misses",
            conversations.len(),
            files_to_parse.len()
        ),
    );

    // Parse only cache misses in parallel
    // Returns (Option<Conversation>, filename, file_size, mtime) — None for empty/filtered files
    let parse_results: Vec<(Option<Conversation>, String, u64, Option<SystemTime>)> =
        files_to_parse
            .into_par_iter()
            .map(|(path, modified, file_size)| {
                let filename = path
                    .file_name()
                    .and_then(|f| f.to_str())
                    .unwrap_or("unknown")
                    .to_owned();

                match process_conversation_file(path, modified, debug_level) {
                    Ok(Some(mut conversation)) => {
                        conversation.preview = if show_last {
                            conversation.preview_last.clone()
                        } else {
                            conversation.preview_first.clone()
                        };
                        debug::debug(
                            debug_level,
                            &format!("Parsed {}: {}", filename, conversation.preview),
                        );
                        (Some(conversation), filename, file_size, modified)
                    }
                    Ok(None) => (None, filename, file_size, modified),
                    Err(e) => {
                        debug::warn(
                            debug_level,
                            &format!("Error processing {}: {}", filename, e),
                        );
                        (None, filename, file_size, modified)
                    }
                }
            })
            .collect();

    // Separate conversations from empty results (for negative caching)
    for (conv, _, _, _) in &parse_results {
        if let Some(conv) = conv {
            conversations.push(conv.clone());
        }
    }

    // Ensure deterministic ordering after parallel processing
    conversations.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));

    // Inject project info into each conversation
    let fallback_path = projects_dir
        .file_name()
        .map(|n| decode_project_dir_name_to_path(&n.to_string_lossy()))
        .unwrap_or_default();

    for (idx, conv) in conversations.iter_mut().enumerate() {
        conv.index = idx;

        // Prefer the cwd extracted from the JSONL file, fall back to decoded path
        let project_path = conv.cwd.clone().unwrap_or_else(|| fallback_path.clone());
        conv.project_name = Some(format_short_name_from_path(&project_path));
        conv.project_path = Some(project_path);
    }

    // Write updated cache if anything changed
    if dirty {
        let mut new_cache: HashMap<String, cache::ProjectCacheEntry> = HashMap::new();

        // Add existing conversations (both cache hits and fresh parses)
        for conv in &conversations {
            let filename = conv
                .path
                .file_name()
                .and_then(|f| f.to_str())
                .unwrap_or("unknown");

            if let Some((_, modified, file_size)) = files_with_meta
                .iter()
                .find(|(p, _, _)| p.file_name() == conv.path.file_name())
                && let Some(mtime) = modified
            {
                new_cache.insert(
                    filename.to_owned(),
                    cache::ProjectCacheEntry::Listed {
                        fingerprint: cache::CachedFingerprint::of(*file_size, *mtime),
                        conversation: cache::cached_conversation(conv),
                    },
                );
            }
        }

        // Add negative cache entries for files that parsed to nothing
        for (conv, filename, file_size, modified) in &parse_results {
            if conv.is_none()
                && let Some(mtime) = modified
            {
                new_cache.insert(
                    filename.to_owned(),
                    cache::ProjectCacheEntry::Empty(cache::CachedFingerprint::of(
                        *file_size, *mtime,
                    )),
                );
            }
        }

        cache::write_project_cache(project_dir_name, new_cache);
    }

    debug::info(
        debug_level,
        &format!("Total conversations loaded: {}", conversations.len()),
    );

    Ok(conversations)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_transcript(lines: &[&str]) -> tempfile::NamedTempFile {
        let mut file = tempfile::Builder::new()
            .suffix(".jsonl")
            .tempfile()
            .unwrap();
        for line in lines {
            writeln!(file, "{line}").unwrap();
        }
        file
    }

    fn conversation_at(name: &str) -> Conversation {
        cache::conversation_from_cached(
            &cache::CachedConversation::default(),
            PathBuf::from(name),
            false,
        )
    }

    fn file_names(conversations: &[Conversation]) -> Vec<String> {
        conversations
            .iter()
            .map(|conversation| {
                conversation
                    .path
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect()
    }

    /// A provider whose session list could not be read joins the terms the
    /// list shows; any other failure stays on `--debug` alone.
    #[test]
    fn a_provider_whose_session_list_is_unreadable_reports_a_term() {
        let unreadable = AppError::SessionListUnreadable {
            reason: "session database locked",
            detail: "state_5.sqlite: database is locked".to_owned(),
        };
        let other = AppError::ConfigError("no home directory".to_owned());

        assert_eq!(
            sessions_not_loaded_term(Source::Codex, &unreadable),
            Some(FilterTerm::new(
                "Codex",
                "session database locked: sessions not loaded"
            ))
        );
        assert_eq!(sessions_not_loaded_term(Source::Codex, &other), None);
    }

    #[test]
    fn progress_goes_out_first_last_and_once_per_interval() {
        let start = Instant::now();
        let at = |millis| start + Duration::from_millis(millis);
        let mut throttle = ProgressThrottle::new(Duration::from_millis(250));

        assert!(throttle.admit(0, 10, at(0)), "a new total");
        assert!(!throttle.admit(1, 10, at(10)));
        assert!(throttle.admit(2, 10, at(260)), "the interval has passed");
        assert!(!throttle.admit(3, 10, at(270)));
        assert!(throttle.admit(10, 10, at(280)), "the last report");
        assert!(throttle.admit(0, 5, at(281)), "the next source's total");
    }

    /// Providers stream one batch each, so a session two of them reach must be
    /// dropped from the later batch, not only within one.
    #[test]
    fn a_path_seen_in_an_earlier_batch_is_dropped_from_a_later_one() {
        let mut seen = SeenPaths::default();
        let mut first = vec![
            conversation_at("first.jsonl"),
            conversation_at("shared.jsonl"),
        ];
        let mut second = vec![
            conversation_at("shared.jsonl"),
            conversation_at("second.jsonl"),
        ];

        seen.retain_unseen(&mut first);
        seen.retain_unseen(&mut second);

        assert_eq!(file_names(&first), ["first.jsonl", "shared.jsonl"]);
        assert_eq!(file_names(&second), ["second.jsonl"]);
    }

    #[test]
    fn a_session_with_no_reply_counts_no_assistant_messages() {
        let file = write_transcript(&[
            r#"{"type":"user","message":{"role":"user","content":"<command-name>/status</command-name>"}}"#,
        ]);

        let conversation = process_conversation_file(file.path().to_path_buf(), None, None)
            .unwrap()
            .expect("a user message is a conversation");

        assert_eq!(conversation.assistant_messages, 0);
        assert_eq!(conversation.message_count, 1);
    }

    #[test]
    fn a_session_that_was_answered_counts_the_reply() {
        let file = write_transcript(&[
            r#"{"type":"user","message":{"role":"user","content":"hi"}}"#,
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"hello"}]}}"#,
        ]);

        let conversation = process_conversation_file(file.path().to_path_buf(), None, None)
            .unwrap()
            .expect("a conversation");

        assert_eq!(conversation.assistant_messages, 1);
    }

    /// Pinned because it is the one thing the file-by-file scan did that this
    /// rule does not: such a transcript never reaches the corpus.
    #[test]
    fn a_transcript_holding_no_conversation_is_not_a_session_at_all() {
        for lines in [
            vec![r#"{"type":"summary","summary":"Only metadata"}"#],
            vec!["{malformed"],
            vec![],
        ] {
            let file = write_transcript(&lines);

            assert!(
                process_conversation_file(file.path().to_path_buf(), None, None)
                    .unwrap()
                    .is_none(),
                "{lines:?} should hold no conversation"
            );
        }
    }
}
