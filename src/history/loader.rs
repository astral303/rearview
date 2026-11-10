//! Loading every provider's conversations, at once or streamed to the TUI.

use super::load_meter::{AgentMeter, LoadMeter};
use super::provider::{Deleted, DiscoveredSession, FoundSession, LeftOutRows, SkippedSessions};
use super::{
    Conversation, Environment, FilterTerm, LoadProgress, LoaderMessage, Source, Workspace,
};
use crate::cli::DebugLevel;
use crate::clock::Moment;
use crate::debug;
use crate::error::{AppError, Result};
use crate::time_filter::TimeFilter;
use chrono::{DateTime, Local};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::Duration;

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
    /// The sessions deleted, or the ones a dry run would delete. Refused
    /// deletes are in `refused`.
    pub candidates: Vec<EmptySession>,
    pub deleted: usize,
    pub refused: Vec<RefusedDelete>,
}

#[derive(Debug, Clone)]
pub struct RefusedDelete {
    pub session: EmptySession,
    pub reason: String,
}

/// Each provider's load outcome. If no provider found a history to read,
/// the failures are fatal.
struct ProviderHistory {
    /// One entry per provider that failed, in registration order. Kept per
    /// provider because each is reported under its own name.
    failures: Vec<(Source, AppError)>,
    /// At least one provider has a session root on disk that loaded cleanly.
    usable: bool,
    /// Each provider that loaded, in registration order, with the sessions
    /// its discovery found.
    discovered: Vec<(Source, Vec<DiscoveredSession>)>,
}

impl ProviderHistory {
    /// Load every provider in registration order. Each provider's sessions reach
    /// `report` as one `Batch` the moment that provider completes, after a
    /// `Progress` for every session, so a caller can show the load as it
    /// happens rather than after the slowest provider. An `Ignored` precedes
    /// the batch for each reason the provider ignored sessions for, and a
    /// `SkippedSessions` for the sessions that hold no conversation and the
    /// unreadable ones. `meter` records what each provider's load read. A
    /// changed session whose cached row `left_out` holds keeps that row unread.
    ///
    /// The session cache database's first build is admitted or refused
    /// before any provider loads. A refused build loads without a cache: it
    /// writes no database, so every session is parsed at every launch.
    fn load(
        environment: &Environment,
        show_last: bool,
        debug_level: Option<DebugLevel>,
        meter: Option<&LoadMeter>,
        left_out: LeftOutRows,
        report: &mut dyn FnMut(LoaderMessage),
    ) -> Self {
        let mut history = Self {
            failures: Vec::new(),
            usable: false,
            discovered: Vec::new(),
        };
        if let Err(error) = super::sqlite_cache::admit_first_build(environment, debug_level, meter)
        {
            debug::warn(debug_level, &error.to_string());
        }
        for &storage in environment.storages() {
            let root_on_disk = storage
                .roots()
                .is_ok_and(|roots| roots.iter().any(|root| root.path.exists()));
            let source = storage.source();
            let loaded = super::provider::load_sessions(
                environment,
                storage,
                show_last,
                debug_level,
                AgentMeter::of(meter, source),
                left_out,
                &mut |done, total| {
                    report(LoaderMessage::Progress(LoadProgress {
                        source,
                        done,
                        total,
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
                    if !loaded.skipped.is_empty() {
                        report(LoaderMessage::SkippedSessions(loaded.skipped));
                    }
                    if !loaded.conversations.is_empty() {
                        report(LoaderMessage::Batch(loaded.conversations));
                    }
                    history.discovered.push((source, loaded.discovered));
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

    /// The error to report when no provider had a history to read: the first
    /// failure, which names a cause, rather than reporting that no history
    /// was found.
    fn into_fatal_error(mut self) -> AppError {
        if self.failures.is_empty() {
            return AppError::NoHistoryFound(super::provider::display_names_in_prose());
        }
        self.failures.remove(0).1
    }

    fn failure_reports(&self) -> impl Iterator<Item = String> + '_ {
        self.failures.iter().map(|(source, error)| {
            format!("Failed to load {} history: {error}", source.display_label())
        })
    }
}

/// The list's term for a provider whose session list is present but could
/// not be read, `OpenCode │ session database locked: sessions not loaded`,
/// so the list shows why it holds none of that provider's sessions. `None`
/// for any other failure, which `--debug` alone reports.
pub(super) fn sessions_not_loaded_term(source: Source, error: &AppError) -> Option<FilterTerm> {
    match error {
        AppError::SessionListUnreadable { reason, .. } => Some(FilterTerm::new(
            source.display_label(),
            format!("{reason}: sessions not loaded"),
        )),
        _ => None,
    }
}

/// The list's term for each agent with loaded sessions missing earlier turns,
/// `Codex │ 2 sessions missing earlier turns`, in registration order.
pub fn missing_earlier_turns_terms(conversations: &[Conversation]) -> Vec<FilterTerm> {
    super::provider::providers()
        .iter()
        .map(|provider| provider.source())
        .filter_map(|source| {
            let count = conversations
                .iter()
                .filter(|row| row.source == source && row.is_missing_earlier_turns)
                .count();
            let sessions = if count == 1 { "session" } else { "sessions" };
            (count > 0).then(|| {
                FilterTerm::new(
                    source.display_label(),
                    format!("{count} {sessions} missing earlier turns"),
                )
            })
        })
        .collect()
}

/// Every agent's conversations, and what the load found but ignores.
pub struct LoadedHistory {
    pub conversations: Vec<Conversation>,
    /// One term per reason an agent's sessions were ignored for, named for
    /// the user.
    pub ignored: Vec<FilterTerm>,
    /// Each agent that loaded, in registration order, with every session its
    /// discovery found and what the load recorded of it.
    pub discovered: Vec<(Source, Vec<DiscoveredSession>)>,
    /// Each agent whose sessions could not be listed, and why.
    pub failures: Vec<(Source, AppError)>,
}

/// The conversations of [`load_history`], for callers with no use for the
/// ignored terms.
pub fn load_all_conversations(
    environment: &Environment,
    show_last: bool,
    debug_level: Option<DebugLevel>,
) -> Result<Vec<Conversation>> {
    Ok(load_history(environment, show_last, debug_level, None, LeftOutRows::NONE)?.conversations)
}

/// Every agent's conversations from every project, and what the load found
/// but ignores. `meter` records what each agent's load read. A changed
/// session whose cached row `left_out` holds keeps that row unread. A row
/// without any time is dated `environment`'s current time.
pub fn load_history(
    environment: &Environment,
    show_last: bool,
    debug_level: Option<DebugLevel>,
    meter: Option<&LoadMeter>,
    left_out: LeftOutRows,
) -> Result<LoadedHistory> {
    let mut conversations = Vec::new();
    let mut ignored = Vec::new();
    let history = ProviderHistory::load(
        environment,
        show_last,
        debug_level,
        meter,
        left_out,
        &mut |message| match message {
            LoaderMessage::Batch(batch) => conversations.extend(batch),
            LoaderMessage::Ignored(term) => ignored.push(term),
            _ => {}
        },
    );
    if !history.usable {
        return Err(history.into_fatal_error());
    }
    for report in history.failure_reports() {
        debug::warn(debug_level, &report);
    }
    finalize_conversations(&mut conversations);
    debug::info(
        debug_level,
        &format!("Total global conversations loaded: {}", conversations.len()),
    );
    Ok(LoadedHistory {
        conversations,
        ignored,
        discovered: history.discovered,
        failures: history.failures,
    })
}

fn finalize_conversations(conversations: &mut Vec<Conversation>) {
    deduplicate_conversations(conversations);
    conversations.sort_by_key(|conversation| std::cmp::Reverse(conversation.timestamp));
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
pub(crate) struct SeenPaths(HashSet<PathBuf>);

impl SeenPaths {
    fn retain_unseen(&mut self, conversations: &mut Vec<Conversation>) {
        conversations.retain(|conversation| self.insert(&conversation.path));
    }

    /// The conversations whose file was not seen earlier, then the rest.
    fn partition_unseen(
        &mut self,
        conversations: Vec<Conversation>,
    ) -> (Vec<Conversation>, Vec<Conversation>) {
        conversations
            .into_iter()
            .partition(|conversation| self.insert(&conversation.path))
    }

    /// True when no earlier path named the file at `path`.
    pub(crate) fn insert(&mut self, path: &Path) -> bool {
        self.0
            .insert(path.canonicalize().unwrap_or_else(|_| path.to_path_buf()))
    }
}

/// One progress report per interval, plus the ones that announce a total and
/// complete it: the status line shows where a source started and that it
/// finished, however fast it loaded.
struct ProgressThrottle {
    interval: Duration,
    last_sent: Option<Moment>,
}

impl ProgressThrottle {
    fn new(interval: Duration) -> Self {
        Self {
            interval,
            last_sent: None,
        }
    }

    fn admit(&mut self, done: usize, total: usize, now: Moment) -> bool {
        let due = match self.last_sent {
            None => true,
            Some(last) => {
                done == 0 || done == total || now.saturating_duration_since(last) >= self.interval
            }
        };
        if due {
            self.last_sent = Some(now);
        }
        due
    }
}

/// Start loading all conversations in the background
/// Returns a receiver that will receive LoaderMessage updates. `meter`
/// records what the load read, and whether it waits for another rearview's
/// cache refresh.
pub fn load_all_conversations_streaming(
    environment: Environment,
    show_last: bool,
    debug_level: Option<DebugLevel>,
    time: TimeFilter,
    meter: Arc<LoadMeter>,
) -> Receiver<LoaderMessage> {
    let (tx, rx) = mpsc::channel();

    thread::spawn(move || {
        load_all_streaming_inner(tx, &environment, show_last, debug_level, time, &meter);
    });

    rx
}

fn load_all_streaming_inner(
    tx: Sender<LoaderMessage>,
    environment: &Environment,
    show_last: bool,
    debug_level: Option<DebugLevel>,
    time: TimeFilter,
    meter: &LoadMeter,
) {
    let clock = environment.clock().as_ref();
    let mut seen = SeenPaths::default();
    let mut throttle = ProgressThrottle::new(PROGRESS_INTERVAL);
    let history = ProviderHistory::load(
        environment,
        show_last,
        debug_level,
        Some(meter),
        LeftOutRows::NONE,
        &mut |message| {
            let message = match message {
                LoaderMessage::Batch(conversations) => {
                    let (unseen, mut unlisted) = seen.partition_unseen(conversations);
                    let (listed, outside_time): (Vec<_>, Vec<_>) =
                        unseen.into_iter().partition(|conversation| {
                            conversation.message_times.has_message_within(&time)
                        });
                    unlisted.extend(outside_time);
                    if !unlisted.is_empty() {
                        let _ = tx.send(LoaderMessage::SkippedSessions(SkippedSessions {
                            unlisted: unlisted
                                .iter()
                                .map(|row| (row.path.clone(), FoundSession::of(row)))
                                .collect(),
                            unreadable: HashMap::new(),
                        }));
                    }
                    if listed.is_empty() {
                        return;
                    }
                    LoaderMessage::Batch(listed)
                }
                LoaderMessage::Progress(progress) => {
                    if !throttle.admit(progress.done, progress.total, clock.now()) {
                        return;
                    }
                    LoaderMessage::Progress(progress)
                }
                message => message,
            };
            let _ = tx.send(message);
        },
    );

    if !history.usable {
        let _ = tx.send(LoaderMessage::Fatal(history.into_fatal_error()));
        return;
    }
    for report in history.failure_reports() {
        debug::warn(debug_level, &report);
        let _ = tx.send(LoaderMessage::ProviderError);
    }
    let _ = tx.send(LoaderMessage::Done);
}

/// Every session that was started and never answered, newest first.
///
/// Loads the whole corpus. Emptiness is a property of the parsed session, and
/// one rule read there covers every agent.
pub fn find_empty_sessions(
    environment: &Environment,
    scope: DeleteEmptyScope,
) -> Result<Vec<EmptySession>> {
    let workspace = match scope {
        DeleteEmptyScope::All => None,
        DeleteEmptyScope::Local => Some(Workspace::current()?),
    };

    let mut empty = load_all_conversations(environment, false, None)?
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
/// Each goes through the agent that recorded it, which also deletes what the
/// agent stores beside the session, such as a Codex thread's earlier rollouts.
pub fn delete_empty_sessions(
    environment: &Environment,
    scope: DeleteEmptyScope,
    delete: bool,
) -> Result<DeleteEmptySummary> {
    let candidates = find_empty_sessions(environment, scope)?;
    if !delete {
        return Ok(DeleteEmptySummary {
            candidates,
            ..DeleteEmptySummary::default()
        });
    }
    delete_each(candidates, |session| {
        super::provider::delete_session(session.source, &session.path)
    })
}

/// Record a refused delete with its reason and carry on; stop at any other
/// failure.
fn delete_each(
    candidates: Vec<EmptySession>,
    mut delete_session: impl FnMut(&EmptySession) -> Result<Deleted>,
) -> Result<DeleteEmptySummary> {
    let mut summary = DeleteEmptySummary::default();
    for session in candidates {
        match delete_session(&session) {
            Ok(_) => {
                summary.deleted += 1;
                summary.candidates.push(session);
            }
            Err(AppError::DeleteRefused(reason)) => {
                summary.refused.push(RefusedDelete { session, reason });
            }
            Err(error) => return Err(error),
        }
    }
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::{Clock, TestClock};
    use crate::error::SessionDatabaseFailure;

    fn conversation_at(name: &str) -> Conversation {
        Conversation {
            source: Source::Pi,
            path: PathBuf::from(name),
            ..crate::search::test_fixtures::one_message_conversation(
                "",
                TestClock::default().wall().with_timezone(&chrono::Local),
                None,
                None,
                None,
            )
        }
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
    /// list shows, under its own name; any other failure stays on `--debug`
    /// alone.
    #[test]
    fn provider_whose_session_list_is_unreadable_reports_term() {
        let unreadable = AppError::SessionListUnreadable {
            reason: SessionDatabaseFailure::Locked,
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
        assert_eq!(
            sessions_not_loaded_term(Source::OpenCode, &unreadable),
            Some(FilterTerm::new(
                "OpenCode",
                "session database locked: sessions not loaded"
            ))
        );
        assert_eq!(sessions_not_loaded_term(Source::Codex, &other), None);
    }

    #[test]
    fn progress_goes_out_first_last_and_once_per_interval() {
        let clock = TestClock::default();
        let mut throttle = ProgressThrottle::new(PROGRESS_INTERVAL);
        let mut admit = |done, total| throttle.admit(done, total, clock.now());

        assert!(admit(0, 10), "a new total");
        assert!(!admit(1, 10));
        clock.advance(PROGRESS_INTERVAL);
        assert!(admit(2, 10), "the interval has passed");
        assert!(!admit(3, 10));
        assert!(admit(10, 10), "the last report");
        assert!(admit(0, 5), "the next source's total");
    }

    /// Providers stream one batch each, so a session two of them reach must be
    /// dropped from the later batch, not only within one.
    #[test]
    fn path_seen_in_earlier_batch_is_dropped_from_later_one() {
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

    fn empty_session(session_id: &str) -> EmptySession {
        EmptySession {
            source: Source::Codex,
            path: PathBuf::from(format!("{session_id}.jsonl")),
            session_id: session_id.to_owned(),
            project_name: "project".to_owned(),
            timestamp: Local::now(),
            preview: None,
            user_messages: 1,
        }
    }

    fn session_ids(sessions: &[EmptySession]) -> Vec<&str> {
        sessions
            .iter()
            .map(|session| session.session_id.as_str())
            .collect()
    }

    #[test]
    fn delete_empty_records_refused_delete_with_its_reason_and_carries_on() {
        let reason = "The fork fork-id shows turns from this session. Delete the fork first.";
        let candidates = vec![empty_session("refused"), empty_session("deletable")];

        let summary = delete_each(candidates, |session| match session.session_id.as_str() {
            "refused" => Err(AppError::DeleteRefused(reason.to_owned())),
            _ => Ok(Deleted::just_the_session()),
        })
        .unwrap();

        assert_eq!(summary.deleted, 1);
        assert_eq!(session_ids(&summary.candidates), ["deletable"]);
        let [refused] = summary.refused.as_slice() else {
            panic!("one refused delete: {:?}", summary.refused);
        };
        assert_eq!(refused.session.session_id, "refused");
        assert_eq!(refused.reason, reason);
    }

    #[test]
    fn delete_empty_stops_at_failure_other_than_refused_delete() {
        let candidates = vec![empty_session("failing"), empty_session("deletable")];
        let mut attempted = Vec::new();

        let failure = delete_each(candidates, |session| {
            attempted.push(session.session_id.clone());
            Err(AppError::Io(std::io::Error::other("disk full")))
        })
        .unwrap_err();

        assert!(matches!(failure, AppError::Io(_)), "{failure:?}");
        assert_eq!(attempted, ["failing"]);
    }
}
