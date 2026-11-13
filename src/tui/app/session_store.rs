//! The TUI's session store: each listed session's pending refresh, by
//! identity, and each finished refresh's outcome until the list applies it.
//! The rows stay on `App`. `App` changes them only through `App::edit_list`,
//! once the list is on screen with nothing over it.

use crate::clock::{Clock, Moment};
use crate::error::{AppError, Result};
use crate::history::fingerprint::CachedFingerprint;
use crate::history::listing::SessionOrigin;
use crate::history::provider::walk;
use crate::history::provider::{SessionOutcome, SessionRead};
use crate::history::sqlite_cache::{AdoptedCopy, WriteFailure};
use crate::history::{
    AwaitedRefresh, CachedRead, CurrentCopy, InteractiveRead, NextStep, OtherRearview,
    RefreshOutcome, SessionChanges, Source,
};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The interval between checks for the completion of the cache refresh that
/// rows wait for.
pub(super) const AWAITED_REFRESH_CHECK_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Default)]
pub(super) struct SessionStore {
    /// True from a list `Ctrl+R` until its refresh begins.
    is_list_refresh_requested: bool,
    /// True while a list refresh runs.
    is_list_refresh_running: bool,
    /// Sessions waiting for a session refresh to begin, by path.
    requested: HashSet<PathBuf>,
    /// Sessions the running session refresh reads.
    refreshing: HashSet<PathBuf>,
    /// Sessions the list does not hold whose refresh may add their rows, by
    /// path, until the list applies that refresh.
    unlisted: HashMap<PathBuf, UnlistedSession>,
    /// Rows waiting for a refresh that another job or rearview runs.
    pub(super) awaiting: Option<RowsAwaitingRefresh>,
    /// Finished refreshes, oldest first, until the list applies them.
    finished: VecDeque<FinishedRefresh>,
    /// True for a file opened directly until the worker finds it inside a
    /// provider's session roots: no provider reads a file outside them.
    refuses_session_refreshes: bool,
    /// Sessions whose last refresh failed, by path, in path order. Each is
    /// refreshed again only once its transcripts change, when the user acts
    /// on it, or when another rearview's refresh it waited for ends. The
    /// records last until the TUI exits.
    failed_refreshes: BTreeMap<PathBuf, FailedRefresh>,
    /// When to check the transcripts of `failed_refreshes` next, on the
    /// app's clock.
    next_failed_refresh_check: Option<Moment>,
    /// The last session the latest check of `failed_refreshes` checked; the
    /// next check starts after it.
    last_checked_failed_refresh: Option<PathBuf>,
}

/// The interval between checks of the transcripts of sessions whose refresh
/// failed.
const FAILED_REFRESH_CHECK_INTERVAL: Duration = Duration::from_secs(1);

/// The limit on the transcripts one check of failed refreshes stats,
/// sub-agent transcripts included: more would delay the next frame on a
/// large set or a slow drive. A check stats at least one session's
/// transcripts, however many it has.
pub(super) const FAILED_REFRESH_TRANSCRIPTS_PER_CHECK: usize = 16;

/// A session whose refresh failed, waiting for its transcripts to change:
/// their stamp at the failure.
pub(super) struct FailedRefresh {
    /// The transcripts' stamp at the failure; `None` when unknown.
    revision: Option<CachedFingerprint>,
    /// The sub-agent transcripts `revision` spans.
    subagents: Vec<PathBuf>,
}

impl FailedRefresh {
    /// The transcripts of the session at `path` named by `failure`, at the
    /// failure's revision or, without one, as they stamp now.
    fn of(path: &Path, failure: &WriteFailure) -> Self {
        let subagents = failure.subagents.clone();
        match failure.revision {
            Some(revision) => Self {
                revision: Some(revision),
                subagents,
            },
            None => Self::now(path, subagents),
        }
    }

    /// The transcripts of the session at `path`, with the sub-agent
    /// transcripts `subagents`, as they stamp now.
    pub(super) fn now(path: &Path, subagents: Vec<PathBuf>) -> Self {
        Self {
            revision: walk::session_stamp(path, &subagents),
            subagents,
        }
    }

    /// True when the transcripts of the session at `path` no longer match
    /// `revision`. False while its transcript doesn't have file metadata to
    /// read, as for a locator inside a database file: such a session is
    /// refreshed again only when the user acts on it or another rearview's
    /// refresh it waited for ends.
    fn has_changed(&self, path: &Path) -> bool {
        walk::session_stamp(path, &self.subagents).is_some_and(|stamp| Some(stamp) != self.revision)
    }

    /// The transcripts a check of this session stats: its own and each
    /// sub-agent transcript.
    fn transcript_count(&self) -> usize {
        1 + self.subagents.len()
    }
}

/// A list or session refresh that failed as a whole: its error, and each
/// session a session refresh was to read, its transcripts stamped on the
/// worker before the refresh waited for ownership.
pub struct RefreshFailure {
    pub(crate) error: AppError,
    sessions: Vec<(PathBuf, FailedRefresh)>,
}

impl RefreshFailure {
    pub(super) fn of_session_refresh(
        error: AppError,
        sessions: Vec<(PathBuf, FailedRefresh)>,
    ) -> Self {
        Self { error, sessions }
    }
}

impl From<AppError> for RefreshFailure {
    fn from(error: AppError) -> Self {
        Self {
            error,
            sessions: Vec::new(),
        }
    }
}

/// A finished refresh, each session's next step recorded, until the list
/// applies it.
pub(super) enum FinishedRefresh {
    /// A session refresh's reads.
    Reads(Vec<FinishedRead>),
    /// A list refresh's or the launch's changes on disk.
    Changes(Box<SessionChanges>),
    Failed(AppError),
}

/// One session a session refresh read: the path it answers for, the read's
/// result, and the failure of its write when it failed.
pub(super) struct FinishedRead {
    pub(super) path: PathBuf,
    pub(super) read: SessionRead,
    pub(super) write_failure: Option<WriteFailure>,
}

impl FinishedRefresh {
    /// True when this refresh read the session at `path`.
    fn has_read_of(&self, path: &Path) -> bool {
        match self {
            Self::Reads(reads) => reads.iter().any(|read| read.path == path),
            Self::Changes(changes) => {
                changes
                    .updated
                    .iter()
                    .any(|updated| updated.row.path == path)
                    || changes.kept.iter().any(|kept| kept.path == path)
                    || changes.removed.iter().any(|removed| removed == path)
            }
            Self::Failed(_) => false,
        }
    }
}

/// The cause of an interactive read of a session. It decides whether a
/// session whose refresh failed is refreshed again while its transcripts
/// are unchanged.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ReadCause {
    /// The user opened or exported the session: the read is the user's
    /// refresh, and its next step runs as recorded.
    UserAction,
    /// The viewer follows a finished refresh: a session whose refresh
    /// failed waits for its transcripts to change.
    FinishedRefresh,
}

/// An interactive read's copy of a session, its next step recorded.
pub(super) enum SessionCopy<T> {
    Current(CurrentCopy<T>),
    Older(T),
    Missing,
}

impl<T> SessionCopy<T> {
    /// The copy, current or out of date; `None` when missing. A copy written
    /// after the session's row comes with its [`AdoptedCopy`].
    pub(super) fn current_or_older_with_adopted(self) -> (Option<T>, Option<AdoptedCopy>) {
        match self {
            Self::Current(copy) => {
                let (copy, adopted) = copy.into_parts();
                (Some(copy), adopted)
            }
            Self::Older(copy) => (Some(copy), None),
            Self::Missing => (None, None),
        }
    }
}

/// A session the list does not hold, waiting for its refresh.
#[derive(Clone, Copy, Debug)]
pub(super) struct UnlistedSession {
    pub(super) source: Source,
    /// True for a session found by ID: a read without a row reports the ID
    /// unresolved. False for one found by discovery.
    pub(super) is_found_by_id: bool,
}

impl UnlistedSession {
    pub(super) fn origin(self) -> SessionOrigin {
        if self.is_found_by_id {
            SessionOrigin::FoundById
        } else {
            SessionOrigin::Discovered
        }
    }
}

/// Sessions read from an out-of-date or missing copy while a refresh owned
/// the cache, by path; the refresh they wait for; and when to check that
/// refresh next. Once it completes, each gets a session refresh.
pub(super) struct RowsAwaitingRefresh {
    pub(super) paths: HashSet<PathBuf>,
    pub(super) awaited: AwaitedRefresh,
    /// On the app's clock.
    pub(super) next_check: Moment,
}

impl RowsAwaitingRefresh {
    /// The remaining time on `clock`, the app's, until the next check.
    pub(super) fn until_next_check(&self, clock: &dyn Clock) -> Duration {
        self.next_check.saturating_duration_since(clock.now())
    }
}

impl SessionStore {
    /// The store of a file opened directly. It doesn't request a session
    /// refresh until [`Self::allow_session_refreshes`].
    pub(super) fn for_a_file_opened_directly() -> Self {
        Self {
            refuses_session_refreshes: true,
            ..Self::default()
        }
    }

    pub(super) fn request_list_refresh(&mut self) {
        self.is_list_refresh_requested = true;
    }

    /// Begin the requested list refresh, once no refresh runs and every
    /// finished one is applied: the list it compares discovery against must
    /// be the list as the user sees it. False when none can begin.
    pub(super) fn begin_list_refresh(&mut self) -> bool {
        if !self.is_list_refresh_requested || self.is_running() || !self.finished.is_empty() {
            return false;
        }
        self.is_list_refresh_requested = false;
        self.is_list_refresh_running = true;
        true
    }

    /// Ask for a session refresh of the session at `path`, as the user or a
    /// change of the session asks: whether or not its last refresh failed.
    pub(super) fn request_session_refresh(&mut self, path: &Path) {
        self.failed_refreshes.remove(path);
        self.requested.insert(path.to_path_buf());
    }

    /// Ask for a session refresh of the session at `path`, as an automatic
    /// request asks: unless its last refresh failed and its transcripts
    /// haven't changed since.
    pub(super) fn request_session_refresh_unless_failed(&mut self, path: &Path) {
        let has_unchanged_failure = self
            .failed_refreshes
            .get(path)
            .is_some_and(|failed| !failed.has_changed(path));
        if !has_unchanged_failure {
            self.request_session_refresh(path);
        }
    }

    /// Record `next`, the next step for the session at `path`, at `now` on
    /// the app's clock: read it again once the refresh it names completes,
    /// request a session refresh of it, or record its failed refresh.
    pub(super) fn record_next_step(&mut self, path: &Path, next: &NextStep, now: Moment) {
        match next {
            NextStep::AwaitRefresh(awaited) => {
                self.await_refresh([path.to_path_buf()], *awaited, now);
            }
            NextStep::CacheInBackground if !self.refuses_session_refreshes => {
                self.request_session_refresh_unless_failed(path);
            }
            NextStep::RefreshOnChange(failure) if !self.refuses_session_refreshes => {
                self.failed_refreshes
                    .insert(path.to_path_buf(), FailedRefresh::of(path, failure));
            }
            NextStep::Settled => {
                self.failed_refreshes.remove(path);
            }
            NextStep::CacheInBackground | NextStep::RefreshOnChange(_) => {}
        }
    }

    /// Ask for a session refresh of each session whose refresh failed and
    /// whose transcripts changed since, checking at most once per
    /// [`FAILED_REFRESH_CHECK_INTERVAL`] of the app's clock, `now` on it.
    pub(super) fn request_changed_failed_refreshes(&mut self, now: Moment) {
        if self.failed_refreshes.is_empty()
            || self
                .next_failed_refresh_check
                .is_some_and(|next| now < next)
        {
            return;
        }
        self.next_failed_refresh_check = Some(now + FAILED_REFRESH_CHECK_INTERVAL);
        let checked = self.failed_refreshes_to_check();
        self.last_checked_failed_refresh = checked.last().cloned();
        for path in checked {
            if self.failed_refreshes[&path].has_changed(&path) {
                self.request_session_refresh(&path);
            }
        }
    }

    /// The sessions the next check of failed refreshes stats, in path order
    /// from the one after the last session checked, wrapping to the first:
    /// at least one, and as many more as fit within
    /// [`FAILED_REFRESH_TRANSCRIPTS_PER_CHECK`] transcripts.
    fn failed_refreshes_to_check(&self) -> Vec<PathBuf> {
        let last = self.last_checked_failed_refresh.as_ref();
        let is_up_to_last_checked = |path: &&PathBuf| last.is_some_and(|last| *path <= last);
        let after_last = self
            .failed_refreshes
            .keys()
            .skip_while(is_up_to_last_checked);
        let from_start = self
            .failed_refreshes
            .keys()
            .take_while(is_up_to_last_checked);
        let mut checked = Vec::new();
        let mut transcripts = 0;
        for path in after_last.chain(from_start) {
            transcripts += self.failed_refreshes[path].transcript_count();
            if !checked.is_empty() && transcripts > FAILED_REFRESH_TRANSCRIPTS_PER_CHECK {
                break;
            }
            checked.push(path.clone());
        }
        checked
    }

    /// The time from `now` until the next check of failed refreshes is due;
    /// `None` while no refresh failed.
    pub(super) fn until_failed_refresh_check(&self, now: Moment) -> Option<Duration> {
        if self.failed_refreshes.is_empty() {
            return None;
        }
        Some(
            self.next_failed_refresh_check
                .map_or(Duration::ZERO, |next| next.saturating_duration_since(now)),
        )
    }

    /// True while the session at `path` waits for its transcripts to change
    /// after a failed refresh.
    pub(super) fn has_failed_refresh(&self, path: &Path) -> bool {
        self.failed_refreshes.contains_key(path)
    }

    /// Record `outcome`'s next step at `now`, and return its read.
    pub(super) fn take_read(&mut self, outcome: SessionOutcome, now: Moment) -> SessionRead {
        self.take_finished_read(outcome, now).read
    }

    /// Record `outcome`'s next step at `now`, and return its read with the
    /// failure of its write.
    fn take_finished_read(&mut self, outcome: SessionOutcome, now: Moment) -> FinishedRead {
        let path = outcome.locator().to_path_buf();
        let (read, next) = outcome.into_parts();
        self.record_next_step(&path, &next, now);
        FinishedRead {
            path,
            read,
            write_failure: next.into_write_failure(),
        }
    }

    /// Record the next step of `read`, an interactive read of the session at
    /// `path` for `cause`, at `now`, and return its copy. A user's action
    /// forgets the session's failed refresh first: an out-of-date or missing
    /// copy then requests a refresh, at once or once the refresh it awaits
    /// completes.
    pub(super) fn take_interactive<T>(
        &mut self,
        path: &Path,
        read: InteractiveRead<T>,
        cause: ReadCause,
        now: Moment,
    ) -> SessionCopy<T> {
        if cause == ReadCause::UserAction {
            self.failed_refreshes.remove(path);
        }
        match read.into_read() {
            CachedRead::Current(copy) => SessionCopy::Current(copy),
            CachedRead::Older { copy, next } => {
                self.record_next_step(path, &next, now);
                SessionCopy::Older(copy)
            }
            CachedRead::Missing { next } => {
                self.record_next_step(path, &next, now);
                SessionCopy::Missing
            }
        }
    }

    /// Ask for a session refresh of the rows at `paths` once `awaited`
    /// completes, checking first at [`AWAITED_REFRESH_CHECK_INTERVAL`] after
    /// `now`.
    pub(super) fn await_refresh(
        &mut self,
        paths: impl IntoIterator<Item = PathBuf>,
        awaited: AwaitedRefresh,
        now: Moment,
    ) {
        let awaiting = self.awaiting.get_or_insert_with(|| RowsAwaitingRefresh {
            paths: HashSet::new(),
            awaited,
            next_check: now + AWAITED_REFRESH_CHECK_INTERVAL,
        });
        awaiting.paths.extend(paths);
        awaiting.awaited = awaited;
    }

    /// The worker found the file opened directly inside a provider's session
    /// roots: its next steps may request session refreshes.
    pub(super) fn allow_session_refreshes(&mut self) {
        self.refuses_session_refreshes = false;
    }

    /// True for a file opened directly until the worker finds it inside a
    /// provider's session roots.
    pub(super) fn refuses_session_refreshes(&self) -> bool {
        self.refuses_session_refreshes
    }

    /// Record the session `source` stores at `path`, found by ID, whose row
    /// the list does not hold, until the list applies its refresh.
    pub(super) fn track_lookup(&mut self, source: Source, path: &Path) {
        self.unlisted.insert(
            path.to_path_buf(),
            UnlistedSession {
                source,
                is_found_by_id: true,
            },
        );
    }

    /// Record the session `source` stores at `path`, found by discovery,
    /// whose row the list does not hold, until the list applies its refresh.
    /// A session found by ID stays recorded as found by ID.
    pub(super) fn track_new_session(&mut self, source: Source, path: &Path) {
        self.unlisted
            .entry(path.to_path_buf())
            .or_insert(UnlistedSession {
                source,
                is_found_by_id: false,
            });
    }

    /// Keep tracking `session`, the unlisted session at `path`, until the
    /// list applies a later refresh of it.
    pub(super) fn keep_tracking(&mut self, path: PathBuf, session: UnlistedSession) {
        self.unlisted.insert(path, session);
    }

    /// The agent of the unlisted session at `path` waiting for its refresh;
    /// `None` for any other session.
    pub(super) fn unlisted_source(&self, path: &Path) -> Option<Source> {
        self.unlisted.get(path).map(|session| session.source)
    }

    /// Stop tracking the unlisted session at `path`, whose refresh the list
    /// applies now, and return it; `None` for any other session.
    pub(super) fn take_unlisted(&mut self, path: &Path) -> Option<UnlistedSession> {
        self.unlisted.remove(path)
    }

    /// Begin a session refresh of every requested session, once no refresh
    /// runs; `None` when none can begin.
    pub(super) fn begin_session_refresh(&mut self) -> Option<HashSet<PathBuf>> {
        if self.requested.is_empty() || self.is_running() {
            return None;
        }
        self.refreshing = std::mem::take(&mut self.requested);
        Some(self.refreshing.clone())
    }

    /// The running refresh finished with `outcome`: record each session's
    /// next step at `now`, on the app's clock, and keep the rest for the list
    /// to apply. A session refresh that failed as a whole records each of its
    /// sessions as a failed refresh, at the stamps its worker took.
    pub(super) fn finish(
        &mut self,
        outcome: std::result::Result<RefreshOutcome, RefreshFailure>,
        now: Moment,
    ) {
        self.is_list_refresh_running = false;
        self.refreshing.clear();
        let outcome = outcome.map_err(|failure| {
            self.failed_refreshes.extend(failure.sessions);
            failure.error
        });
        self.add_launch_outcome(outcome, now);
    }

    /// The launch read `outcome` under its own refresh ownership: record and
    /// keep it as [`Self::finish`] does.
    pub(super) fn add_launch_outcome(&mut self, outcome: Result<RefreshOutcome>, now: Moment) {
        let finished = match outcome {
            Ok(RefreshOutcome::Cached(outcomes)) => FinishedRefresh::Reads(
                outcomes
                    .into_iter()
                    .map(|outcome| self.take_finished_read(outcome, now))
                    .collect(),
            ),
            Ok(RefreshOutcome::Changes(changes)) => {
                self.record_next_steps_of(&changes, now);
                FinishedRefresh::Changes(changes)
            }
            Err(error) => FinishedRefresh::Failed(error),
        };
        self.finished.push_back(finished);
    }

    /// Record the next step of each session `changes` read, at `now`, and
    /// track each new session read from an out-of-date copy as found by
    /// discovery.
    fn record_next_steps_of(&mut self, changes: &SessionChanges, now: Moment) {
        for updated in &changes.updated {
            if !updated.replaces_listed_row && updated.next != NextStep::Settled {
                self.track_new_session(updated.row.source, &updated.row.path);
            }
            self.record_next_step(&updated.row.path, &updated.next, now);
        }
        for kept in &changes.kept {
            self.record_next_step(&kept.path, &kept.next, now);
        }
        for pending in &changes.pending {
            self.track_new_session(pending.source, &pending.path);
            self.record_next_step(&pending.path, &pending.next, now);
        }
    }

    /// The newest finished refresh the list has yet to apply.
    pub(super) fn newest_finished(&self) -> Option<&FinishedRefresh> {
        self.finished.back()
    }

    /// Take the oldest finished refresh for the list to apply. The newer
    /// ones stay in the store until the list takes them.
    pub(super) fn take_oldest_finished(&mut self) -> Option<FinishedRefresh> {
        self.finished.pop_front()
    }

    /// True while a list or session refresh runs.
    pub(super) fn is_running(&self) -> bool {
        self.is_list_refresh_running || !self.refreshing.is_empty()
    }

    /// True while a list refresh runs.
    pub(super) fn is_list_refresh_running(&self) -> bool {
        self.is_list_refresh_running
    }

    /// True from the start of a refresh until the list applies its outcome.
    pub(super) fn is_refreshing(&self) -> bool {
        self.is_running() || !self.finished.is_empty()
    }

    /// True while the session at `path` is in the running session refresh.
    pub(super) fn is_in_running_session_refresh(&self, path: &Path) -> bool {
        self.refreshing.contains(path)
    }

    /// True while the session at `path` is in the running session refresh,
    /// or in a finished outcome the list has yet to apply.
    pub(super) fn is_session_refreshing(&self, path: &Path) -> bool {
        self.refreshing.contains(path)
            || self
                .finished
                .iter()
                .any(|finished| finished.has_read_of(path))
    }

    /// True while a refresh of the session at `path` waits to begin, runs,
    /// or awaits another refresh.
    pub(super) fn is_refresh_pending(&self, path: &Path) -> bool {
        self.requested.contains(path)
            || self.refreshing.contains(path)
            || self
                .awaiting
                .as_ref()
                .is_some_and(|awaiting| awaiting.paths.contains(path))
    }

    /// The other rearview's activity the row at `path` awaits; `None` while
    /// it doesn't await one.
    pub(super) fn another_rearview_awaited_by(&self, path: &Path) -> Option<OtherRearview> {
        let awaiting = self
            .awaiting
            .as_ref()
            .filter(|awaiting| awaiting.paths.contains(path))?;
        match awaiting.awaited {
            AwaitedRefresh::AnotherRearview(activity) => Some(activity),
            AwaitedRefresh::ThisProcess => None,
        }
    }

    /// True while a refresh of the session at `path` is pending, or finished
    /// without the list applying it.
    pub(super) fn has_unapplied_refresh(&self, path: &Path) -> bool {
        self.is_refresh_pending(path) || self.is_session_refreshing(path)
    }

    /// True while a session refresh waits to begin.
    #[cfg(test)]
    pub(super) fn is_session_refresh_requested(&self) -> bool {
        !self.requested.is_empty()
    }
}
