//! The TUI's session store: each listed session's pending refresh, by
//! identity, and each finished refresh's outcome until the list applies it.
//! The rows stay on `App`. `App` changes them only through `App::edit_list`,
//! once the list is on screen with nothing over it.

use crate::clock::{Clock, Moment};
use crate::error::Result;
use crate::history::{AwaitedRefresh, RefreshOutcome, Source};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::time::Duration;

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
    /// Sessions found by ID that the list does not hold, with their agents,
    /// until a refresh lands their rows.
    looked_up: HashMap<PathBuf, Source>,
    /// Rows waiting for a refresh that another job or rearview runs.
    pub(super) awaiting: Option<RowsAwaitingRefresh>,
    /// Finished refreshes, oldest first, until the list applies them.
    finished: VecDeque<Result<RefreshOutcome>>,
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

    /// Ask for a session refresh of the session at `path`.
    pub(super) fn request_session_refresh(&mut self, path: &Path) {
        self.requested.insert(path.to_path_buf());
    }

    /// Record the session `source` stores at `path`, found by ID, whose row
    /// the list does not hold: the refresh that reads it lands its row.
    pub(super) fn track_lookup(&mut self, source: Source, path: &Path) {
        self.looked_up.insert(path.to_path_buf(), source);
    }

    /// The agent of the session found by ID at `path` whose row has not
    /// landed; `None` for any other session.
    pub(super) fn looked_up_source(&self, path: &Path) -> Option<Source> {
        self.looked_up.get(path).copied()
    }

    /// The read of the session found by ID at `path` landed.
    pub(super) fn forget_lookup(&mut self, path: &Path) {
        self.looked_up.remove(path);
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

    /// The running refresh finished with `outcome`.
    pub(super) fn finish(&mut self, outcome: Result<RefreshOutcome>) {
        self.is_list_refresh_running = false;
        self.refreshing.clear();
        self.finished.push_back(outcome);
    }

    /// The launch read `outcome` under its own refresh ownership.
    pub(super) fn add_launch_outcome(&mut self, outcome: Result<RefreshOutcome>) {
        self.finished.push_back(outcome);
    }

    /// The newest finished outcome the list has yet to apply.
    pub(super) fn newest_finished(&self) -> Option<&Result<RefreshOutcome>> {
        self.finished.back()
    }

    /// Take every finished outcome, oldest first, for the list to apply.
    pub(super) fn take_finished(&mut self) -> Vec<Result<RefreshOutcome>> {
        self.finished.drain(..).collect()
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

    /// True while the session at `path` is in the running session refresh,
    /// or in a finished outcome the list has yet to apply.
    pub(super) fn is_session_refreshing(&self, path: &Path) -> bool {
        self.refreshing.contains(path)
            || self
                .finished
                .iter()
                .any(|outcome| outcome_names(outcome, path))
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

    /// True while a session refresh waits to begin.
    #[cfg(test)]
    pub(super) fn is_session_refresh_requested(&self) -> bool {
        !self.requested.is_empty()
    }
}

/// True when `outcome` holds a read of the session at `path`.
fn outcome_names(outcome: &Result<RefreshOutcome>, path: &Path) -> bool {
    match outcome {
        Ok(RefreshOutcome::Cached(outcomes)) => {
            outcomes.iter().any(|outcome| outcome.locator() == path)
        }
        Ok(RefreshOutcome::Changes(changes)) => {
            changes
                .updated
                .iter()
                .any(|updated| updated.row.path == path)
                || changes.kept.iter().any(|kept| kept.path == path)
                || changes.removed.iter().any(|removed| removed == path)
        }
        Err(_) => false,
    }
}
