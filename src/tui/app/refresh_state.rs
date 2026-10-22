//! The list's refreshes on the TUI's background worker. A list `Ctrl+R`
//! hands the worker the list's known sessions; a session refresh hands it
//! the sessions that asked for one, to read again and cache. Each outcome
//! is applied through [`App::apply_finished_refresh`].

use super::{App, AppMode, DialogMode, RefreshState};
use crate::clock::{Clock, Moment};
use crate::error::Result;
use crate::history::provider::{
    SessionOutcome, SessionRead, apply_external_title, cache_sessions, reread_sessions,
};
use crate::history::{
    AwaitedRefresh, Conversation, Environment, FoundSession, KnownSessions, NextStep,
    OperationScope, OtherRearview, RefreshOutcome, SessionChanges, SkippedSessions, Source,
    UpdatedSession,
};
use crate::search;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// The interval between checks for the completion of the cache refresh that
/// rows wait for.
const AWAITED_REFRESH_CHECK_INTERVAL: Duration = Duration::from_secs(1);

/// Rows read outside the session cache database while a refresh owned it,
/// by path, the refresh they wait for, and when to check that refresh next.
pub(super) struct RowsAwaitingRefresh {
    paths: HashSet<PathBuf>,
    awaited: AwaitedRefresh,
    /// On the app's clock.
    next_check: Moment,
    /// True once a check found the refresh complete while the list could
    /// not be edited: the rows wait for the list, not for the refresh.
    is_refresh_complete: bool,
}

impl RowsAwaitingRefresh {
    /// The remaining time on `clock`, the app's, until the next check.
    pub(super) fn until_next_check(&self, clock: &dyn Clock) -> Duration {
        self.next_check.saturating_duration_since(clock.now())
    }
}

/// How many rows applying a refresh's changes added, changed and removed.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct AppliedChanges {
    pub new: usize,
    pub changed: usize,
    pub removed: usize,
}

impl AppliedChanges {
    /// `3 new · 1 changed · 1 removed`, without the counts that are zero;
    /// `None` when nothing changed.
    pub fn summary(&self) -> Option<String> {
        let parts = [
            (self.new, "new"),
            (self.changed, "changed"),
            (self.removed, "removed"),
        ]
        .into_iter()
        .filter(|(count, _)| *count > 0)
        .map(|(count, label)| format!("{count} {label}"))
        .collect::<Vec<_>>();
        (!parts.is_empty()).then(|| parts.join(" · "))
    }
}

/// Rows to drop, replace and add, by index into the list.
#[derive(Default)]
struct ListEdit {
    removed: HashSet<usize>,
    replaced: HashMap<usize, Conversation>,
    inserted: Vec<Conversation>,
    /// Rows whose title changed in place, so their search data is rebuilt.
    retitled: HashSet<usize>,
    /// Replaced rows read again after the user changed them during the
    /// refresh, whose fingerprint is the one the row already had. They
    /// show the user's own change, so the summary does not count them.
    reread_unchanged: HashSet<usize>,
}

impl ListEdit {
    fn counts(&self) -> AppliedChanges {
        AppliedChanges {
            new: self.inserted.len(),
            changed: self.replaced.len() - self.reread_unchanged.len() + self.retitled.len(),
            removed: self.removed.len(),
        }
    }

    fn is_empty(&self) -> bool {
        self.removed.is_empty()
            && self.replaced.is_empty()
            && self.inserted.is_empty()
            && self.retitled.is_empty()
    }

    /// Record `read`, row `index` read again: a listed row replaces it, and a
    /// session that is gone or empty removes it. A failed read keeps the row,
    /// because it shows nothing about the session.
    fn apply_reread(&mut self, index: usize, read: SessionRead) {
        match read {
            SessionRead::Listed(row) => {
                self.replaced.insert(index, *row);
            }
            SessionRead::Missing | SessionRead::Empty => {
                self.removed.insert(index);
            }
            SessionRead::Failed(_) => {}
        }
    }
}

/// `rows`' sessions read again from disk as the list builds their rows, one
/// outcome each in order, sub-agent transcripts included: the file each row
/// names, as its agent's discovery lists it or, for a session opened by its
/// ID that discovery leaves out, as the agent finds it again; not another
/// copy stored under the same session id. Each agent's discovery runs once
/// for all of its rows. A row keeps its index and the project it is filed
/// under, and previews its last messages when `show_last` is true. Read as
/// the drawing thread reads ([`reread_sessions`]) in `environment`.
pub(super) fn read_listed_sessions(
    environment: &Environment,
    rows: &[&Conversation],
    show_last: bool,
) -> Vec<SessionOutcome> {
    let read = read_rows_by_source(rows, |source, locators| {
        Ok::<_, std::convert::Infallible>(reread_sessions(environment, source, locators, show_last))
    });
    match read {
        Ok(outcomes) => outcomes,
        Err(never) => match never {},
    }
}

/// [`read_listed_sessions`] on the background worker ([`cache_sessions`])
/// under `operation`: each session the database does not hold is cached.
pub(super) fn cache_listed_sessions(
    environment: &Environment,
    rows: &[&Conversation],
    show_last: bool,
    operation: &OperationScope,
) -> Result<Vec<SessionOutcome>> {
    read_rows_by_source(rows, |source, locators| {
        cache_sessions(environment, source, locators, show_last, operation)
    })
}

/// `rows`' sessions read with `read`, called once per agent with the files
/// of that agent's rows, one outcome per row in order. A listed row keeps
/// its row's index and the project it is filed under.
fn read_rows_by_source<E>(
    rows: &[&Conversation],
    read: impl Fn(Source, &[&Path]) -> std::result::Result<Vec<SessionOutcome>, E>,
) -> std::result::Result<Vec<SessionOutcome>, E> {
    let mut by_source: Vec<(Source, Vec<usize>)> = Vec::new();
    for (index, row) in rows.iter().enumerate() {
        match by_source
            .iter_mut()
            .find(|(source, _)| *source == row.source)
        {
            Some((_, indices)) => indices.push(index),
            None => by_source.push((row.source, vec![index])),
        }
    }
    let mut outcomes: Vec<Option<SessionOutcome>> = rows.iter().map(|_| None).collect();
    for (source, indices) in by_source {
        let locators: Vec<&Path> = indices.iter().map(|&i| rows[i].path.as_path()).collect();
        for (&index, outcome) in indices.iter().zip(read(source, &locators)?) {
            outcomes[index] = Some(outcome.in_place_of(rows[index]));
        }
    }
    Ok(outcomes
        .into_iter()
        .map(|outcome| outcome.expect("one outcome per row"))
        .collect())
}

/// A session refresh: the rows read outside the session cache database,
/// handed to the background worker to read again and cache.
pub struct SessionRefresh {
    environment: Environment,
    rows: Vec<Conversation>,
    show_last: bool,
}

impl SessionRefresh {
    /// Read each row again under `operation`'s refresh ownership and cache
    /// it. Run on the background worker: it waits for ownership as a load
    /// does.
    pub fn refresh(self, operation: &OperationScope) -> Result<RefreshOutcome> {
        let rows: Vec<&Conversation> = self.rows.iter().collect();
        Ok(RefreshOutcome::Cached(cache_listed_sessions(
            &self.environment,
            &rows,
            self.show_last,
            operation,
        )?))
    }
}

impl App {
    /// Record the sessions the load found for a refresh to skip.
    pub fn add_skipped_sessions(&mut self, sessions: SkippedSessions) {
        self.skipped_sessions.extend(sessions);
    }

    /// Request a list refresh, as `Ctrl+R` does. One request waits at most:
    /// the background worker runs it once no other job runs.
    pub fn request_list_refresh(&mut self) {
        if !self.single_file_mode {
            self.is_list_refresh_requested = true;
        }
    }

    /// [`Self::begin_refresh`] for a requested list refresh; `None` while
    /// none is requested, or it cannot begin yet.
    pub fn begin_requested_refresh(&mut self) -> Option<KnownSessions> {
        if !self.is_list_refresh_requested {
            return None;
        }
        let known = self.begin_refresh()?;
        self.is_list_refresh_requested = false;
        Some(known)
    }

    /// Start a refresh, returning what the list holds for it to compare
    /// discovery against. `None`, and nothing starts, while the initial load
    /// or another refresh runs, or for a file opened directly.
    pub fn begin_refresh(&mut self) -> Option<KnownSessions> {
        if self.single_file_mode || self.is_loading() || self.is_refreshing() {
            return None;
        }
        self.refresh = RefreshState::Running;
        Some(KnownSessions {
            listed: self
                .conversations
                .iter()
                .map(|conversation| (conversation.path.clone(), FoundSession::of(conversation)))
                .collect(),
            skipped: self.skipped_sessions.clone(),
        })
    }

    /// Start a session refresh of the rows read outside the database,
    /// returning them for the background worker. `None`, and nothing starts,
    /// while no listed row waits, the initial load or another refresh runs,
    /// or for a file opened directly.
    pub fn begin_session_refresh(&mut self) -> Option<SessionRefresh> {
        if self.sessions_to_refresh.is_empty()
            || self.single_file_mode
            || self.is_loading()
            || self.is_refreshing()
        {
            return None;
        }
        let paths = std::mem::take(&mut self.sessions_to_refresh);
        let rows: Vec<Conversation> = self
            .conversations
            .iter()
            .filter(|row| paths.contains(&row.path))
            .cloned()
            .collect();
        if rows.is_empty() {
            return None;
        }
        self.refresh = RefreshState::Running;
        Some(SessionRefresh {
            environment: self.environment.clone(),
            rows,
            show_last: self.show_last,
        })
    }

    /// Take a refresh's outcome, applying it at once when the list is on
    /// screen with nothing over it.
    pub fn finish_refresh(&mut self, outcome: Result<RefreshOutcome>) {
        self.refresh = RefreshState::Finished(Box::new(outcome));
        self.apply_finished_refresh();
    }

    /// Apply a finished refresh once the list can be edited. A refresh that
    /// finishes in the viewer, under a dialog or before a requested open
    /// waits, so no row moves under an open session or a confirmation.
    pub fn apply_finished_refresh(&mut self) {
        if !self.is_list_editable() || !matches!(self.refresh, RefreshState::Finished(_)) {
            return;
        }
        let RefreshState::Finished(outcome) =
            std::mem::replace(&mut self.refresh, RefreshState::Idle)
        else {
            return;
        };
        match *outcome {
            Ok(RefreshOutcome::Changes(changes)) => {
                if let Some(summary) = self.apply_session_changes(changes).summary() {
                    self.set_status(summary);
                }
            }
            Ok(RefreshOutcome::Cached(outcomes)) => self.apply_cached_reads(outcomes),
            Err(error) => {
                self.changed_during_refresh.clear();
                self.set_status(format!("Refresh failed: {error}"));
            }
        }
    }

    /// True when the list is on screen with nothing over it: no dialog, and
    /// no requested open. Only then may a refresh move, add or remove rows.
    fn is_list_editable(&self) -> bool {
        matches!(self.app_mode, AppMode::List)
            && self.dialog_mode == DialogMode::None
            && self.pending_open.is_none()
    }

    /// Apply `outcomes`, the rows a session refresh cached, as a
    /// completion check applies its rereads: replace a listed row, remove a
    /// session that is gone or empty, and keep a row whose read failed. A
    /// row the user renamed, deleted or opened meanwhile is read again
    /// instead. The TUI acts on each row's next step.
    fn apply_cached_reads(&mut self, outcomes: Vec<SessionOutcome>) {
        let changed_by_user = std::mem::take(&mut self.changed_during_refresh);
        let index_of: HashMap<PathBuf, usize> = self
            .conversations
            .iter()
            .enumerate()
            .map(|(index, row)| (row.path.clone(), index))
            .collect();
        let mut edit = ListEdit::default();
        for outcome in outcomes {
            if changed_by_user.contains(outcome.locator()) {
                continue;
            }
            let Some(&index) = index_of.get(outcome.locator()) else {
                continue;
            };
            edit.apply_reread(index, self.act_on_outcome(outcome));
        }
        let reread: Vec<usize> = changed_by_user
            .iter()
            .filter_map(|path| index_of.get(path).copied())
            .collect();
        for (&index, read) in reread.iter().zip(self.read_rows_again(&reread)) {
            edit.apply_reread(index, read);
        }
        if !edit.is_empty() {
            self.edit_list(edit);
        }
    }

    /// True from the start of a refresh until its outcome is applied.
    pub fn is_refreshing(&self) -> bool {
        !matches!(self.refresh, RefreshState::Idle)
    }

    /// Record that the user renamed, deleted or opened the session at `path`,
    /// so a refresh running now reads it again when it is applied.
    pub(super) fn note_changed_during_refresh(&mut self, path: &Path) {
        if self.is_refreshing() {
            self.changed_during_refresh.insert(path.to_path_buf());
        }
    }

    /// Refresh the list with `changes`, keeping the selection on its session
    /// and the search data of every row that did not change.
    ///
    /// The refresh read sessions before the user renamed, deleted or opened
    /// some of them. Skip its changes for those sessions and read each one
    /// that is still listed again, so a rename keeps its title and shows
    /// messages written meanwhile.
    pub fn apply_session_changes(&mut self, changes: SessionChanges) -> AppliedChanges {
        let changed_by_user = std::mem::take(&mut self.changed_during_refresh);
        let index_of: HashMap<PathBuf, usize> = self
            .conversations
            .iter()
            .enumerate()
            .map(|(index, conversation)| (conversation.path.clone(), index))
            .collect();
        let mut edit = ListEdit::default();
        for path in changes.removed {
            if let Some(&index) = index_of.get(&path)
                && !changed_by_user.contains(&path)
            {
                edit.removed.insert(index);
            }
        }
        for UpdatedSession {
            row,
            replaces_listed_row,
            next,
        } in changes.updated
        {
            if changed_by_user.contains(&row.path) {
                continue;
            }
            match (index_of.get(&row.path).copied(), replaces_listed_row) {
                (Some(index), true) if !edit.removed.contains(&index) => {
                    self.act_on_next_step(&row.path, next);
                    edit.replaced.insert(index, row);
                }
                (None, false) => {
                    self.act_on_next_step(&row.path, next);
                    edit.inserted.push(row);
                }
                _ => {}
            }
        }
        for kept in changes.kept {
            if index_of.contains_key(&kept.path) && !changed_by_user.contains(&kept.path) {
                self.act_on_next_step(&kept.path, kept.next);
            }
        }
        for (source, titles) in &changes.external_titles {
            for (index, conversation) in self.conversations.iter_mut().enumerate() {
                if conversation.source == *source
                    && !changed_by_user.contains(&conversation.path)
                    && !edit.replaced.contains_key(&index)
                    && !edit.removed.contains(&index)
                    && let Some(title) = titles.get(&conversation.session_id)
                    && apply_external_title(conversation, title)
                {
                    edit.retitled.insert(index);
                }
            }
        }
        let reread: Vec<usize> = changed_by_user
            .iter()
            .filter_map(|path| index_of.get(path).copied())
            .collect();
        let listed_fingerprints: Vec<_> = reread
            .iter()
            .map(|&index| self.conversations[index].fingerprint)
            .collect();
        let reads = self.read_rows_again(&reread);
        for ((index, listed_fingerprint), read) in
            reread.into_iter().zip(listed_fingerprints).zip(reads)
        {
            if let SessionRead::Listed(read) = &read
                && read.fingerprint.is_some()
                && read.fingerprint == listed_fingerprint
            {
                edit.reread_unchanged.insert(index);
            }
            edit.apply_reread(index, read);
        }

        self.active_filters.truncate(self.launch_filter_count);
        self.active_filters.extend(changes.ignored);
        self.skipped_sessions = changes.skipped;

        let applied = edit.counts();
        if !edit.is_empty() {
            self.edit_list(edit);
        }
        for conversation in &self.conversations {
            self.skipped_sessions.unlisted.remove(&conversation.path);
        }
        applied
    }

    /// The rows at `indices` read again as the list builds their rows, one
    /// read each in order, after [`Self::act_on_outcome`] for each.
    pub(super) fn read_rows_again(&mut self, indices: &[usize]) -> Vec<SessionRead> {
        let rows: Vec<&Conversation> = indices
            .iter()
            .map(|&index| &self.conversations[index])
            .collect();
        read_listed_sessions(&self.environment, &rows, self.show_last)
            .into_iter()
            .map(|outcome| self.act_on_outcome(outcome))
            .collect()
    }

    /// [`Self::act_on_next_step`] for `outcome`'s session, and return its
    /// read.
    pub(super) fn act_on_outcome(&mut self, outcome: SessionOutcome) -> SessionRead {
        self.act_on_next_step(outcome.locator(), outcome.next());
        outcome.into_read()
    }

    /// Act on `next`, the next step for the session at `path`: read its row
    /// again once the refresh it names completes, or request a session
    /// refresh of it. The row of a file opened directly only waits for a
    /// running refresh, because no background worker serves it.
    pub(super) fn act_on_next_step(&mut self, path: &Path, next: NextStep) {
        match next {
            NextStep::AwaitRefresh(awaited) => {
                self.await_refresh([path.to_path_buf()], awaited);
            }
            NextStep::CacheInBackground if !self.single_file_mode => {
                self.sessions_to_refresh.insert(path.to_path_buf());
            }
            NextStep::CacheInBackground | NextStep::Settled => {}
        }
    }

    /// Read the rows at `paths` again once `awaited` completes.
    pub(super) fn await_refresh(
        &mut self,
        paths: impl IntoIterator<Item = PathBuf>,
        awaited: AwaitedRefresh,
    ) {
        let next_check = self.clock().now() + AWAITED_REFRESH_CHECK_INTERVAL;
        let awaiting = self
            .rows_awaiting_refresh
            .get_or_insert_with(|| RowsAwaitingRefresh {
                paths: HashSet::new(),
                awaited,
                next_check,
                is_refresh_complete: false,
            });
        awaiting.paths.extend(paths);
        awaiting.awaited = awaited;
        awaiting.is_refresh_complete = false;
    }

    /// The other rearview's work the list waits for: the wait the
    /// running load or background job publishes, or the refresh the
    /// sessions in `RowsAwaitingRefresh` wait for; `None` while the list
    /// waits for no other rearview.
    pub fn another_rearview_activity(&self) -> Option<OtherRearview> {
        self.cache_wait.or_else(|| {
            let awaiting = self.rows_awaiting_refresh.as_ref()?;
            match awaiting.awaited {
                AwaitedRefresh::AnotherRearview(activity) if !awaiting.is_refresh_complete => {
                    Some(activity)
                }
                AwaitedRefresh::AnotherRearview(_) | AwaitedRefresh::ThisProcess => None,
            }
        })
    }

    /// True while the list waits for another rearview's hold on the session
    /// cache.
    #[cfg(test)]
    pub fn is_waiting_for_another_rearview(&self) -> bool {
        self.another_rearview_activity().is_some()
    }

    /// True while rows wait for a session refresh to start.
    #[cfg(test)]
    pub fn is_session_refresh_requested(&self) -> bool {
        !self.sessions_to_refresh.is_empty()
    }
    /// True while rows wait for a cache refresh to complete, whichever
    /// process runs it.
    #[cfg(test)]
    pub fn is_awaiting_refresh(&self) -> bool {
        self.rows_awaiting_refresh.is_some()
    }

    /// Wait for `awaited`, as a row read during it does.
    #[cfg(test)]
    pub fn await_refresh_for_test(&mut self, awaited: AwaitedRefresh) {
        self.await_refresh(std::iter::empty(), awaited);
    }

    /// Make the next check for the awaited refresh's completion due now.
    #[cfg(test)]
    pub fn expire_awaited_refresh_check_for_test(&mut self) {
        let now = self.clock().now();
        if let Some(awaiting) = &mut self.rows_awaiting_refresh {
            awaiting.next_check = now;
        }
    }

    /// Record what the running load or background job waits for; `None`
    /// while it waits for no other rearview, or none runs.
    pub fn set_cache_wait(&mut self, wait: Option<OtherRearview>) {
        self.cache_wait = wait;
    }

    /// Check, without waiting and at most once per
    /// [`AWAITED_REFRESH_CHECK_INTERVAL`], whether the cache refresh the
    /// rows wait for has completed. Once it has and the list can be edited,
    /// read the waiting rows again as a refresh applies them: replace a
    /// listed row, remove a session that is gone or empty, and keep a row
    /// whose read failed. A row read again while a newer refresh runs keeps
    /// waiting. A file opened directly replaces its row in place, without
    /// waiting for the list.
    pub fn check_awaited_refresh(&mut self) {
        let is_list_editable = self.is_list_editable();
        let now = self.clock().now();
        let cache = self.interactive_cache();
        let Some(awaiting) = &mut self.rows_awaiting_refresh else {
            return;
        };
        if now < awaiting.next_check {
            return;
        }
        awaiting.next_check = now + AWAITED_REFRESH_CHECK_INTERVAL;
        if !awaiting.is_refresh_complete {
            if let Some(awaited) = cache.awaited_refresh() {
                awaiting.awaited = awaited;
                return;
            }
            awaiting.is_refresh_complete = true;
        }
        if !self.single_file_mode && !is_list_editable {
            return;
        }
        let paths = std::mem::take(&mut awaiting.paths);
        self.rows_awaiting_refresh = None;
        let indices: Vec<usize> = self
            .conversations
            .iter()
            .enumerate()
            .filter(|(_, row)| paths.contains(&row.path))
            .map(|(index, _)| index)
            .collect();
        if self.single_file_mode {
            self.read_opened_file_again(&indices);
            return;
        }
        let mut edit = ListEdit::default();
        for (index, read) in indices.iter().zip(self.read_rows_again(&indices)) {
            edit.apply_reread(*index, read);
        }
        if !edit.is_empty() {
            self.edit_list(edit);
        }
    }

    /// Replace the rows at `indices` of a file opened directly with the file
    /// read again by the format it was opened with: no agent's discovery
    /// lists it. A read that fails or finds nothing keeps the row.
    fn read_opened_file_again(&mut self, indices: &[usize]) {
        for &index in indices {
            let row = &self.conversations[index];
            if let Ok(Some(read)) = super::read_single_file(
                &row.path,
                row.source,
                self.is_opened_file_stored,
                self.clock(),
            ) {
                self.conversations[index] = read;
            }
        }
    }

    /// Replace row `index` with `row`, read again, or remove it when `row` is
    /// `None`, with the same edit a refresh applies.
    pub(super) fn update_listed_row(&mut self, index: usize, row: Option<Conversation>) {
        let mut edit = ListEdit::default();
        match row {
            Some(row) => {
                edit.replaced.insert(index, row);
            }
            None => {
                edit.removed.insert(index);
            }
        }
        self.edit_list(edit);
    }

    /// Apply `edit` in one pass: the list stays newest first, the selection
    /// stays on its session, and only added, replaced and retitled rows get
    /// new search data. Unchanged rows keep their search data and their
    /// shared row in the workers' snapshot.
    fn edit_list(&mut self, mut edit: ListEdit) {
        let selected_path = self.get_selected_path();
        let old_rows = std::mem::take(&mut self.conversations);
        let old_count = old_rows.len();
        // Reuse search data only while it matches the rows one to one.
        let old_snapshot = Some(self.conversations_snapshot.clone())
            .filter(|snapshot| snapshot.len() == old_count);
        let mut old_searchable = Some(std::mem::take(&mut self.searchable))
            .filter(|searchable| searchable.len() == old_count)
            .map(|searchable| searchable.into_iter().map(Some).collect::<Vec<_>>());

        // Each row with its old index, if it had one, and whether its search
        // data still holds.
        let mut rows = Vec::with_capacity(old_count + edit.inserted.len());
        for (index, row) in old_rows.into_iter().enumerate() {
            if edit.removed.contains(&index) {
                continue;
            }
            match edit.replaced.remove(&index) {
                Some(replacement) => rows.push((replacement, Some(index), false)),
                None => rows.push((row, Some(index), !edit.retitled.contains(&index))),
            }
        }
        rows.extend(edit.inserted.drain(..).map(|row| (row, None, false)));
        rows.sort_by_key(|(row, _, _)| std::cmp::Reverse(row.timestamp));

        let mut new_index_of_old = vec![None; old_count];
        let mut snapshot = Vec::with_capacity(rows.len());
        let mut searchable = Vec::with_capacity(rows.len());
        for (index, (row, old, unchanged)) in rows.iter_mut().enumerate() {
            row.index = index;
            if let Some(old) = *old {
                new_index_of_old[old] = Some(index);
            }
            let reused = old.filter(|_| *unchanged).and_then(|old| {
                let shared = old_snapshot.as_ref()?.get(old)?.clone();
                let entry = old_searchable.as_mut()?.get_mut(old)?.take()?;
                Some((shared, entry))
            });
            match reused {
                Some((shared, mut entry)) => {
                    entry.index = index;
                    snapshot.push(shared);
                    searchable.push(entry);
                }
                None => {
                    snapshot.push(Arc::new(row.clone()));
                    searchable.push(search::searchable_conversation(row, index));
                }
            }
        }
        self.conversations = rows.into_iter().map(|(row, _, _)| row).collect();
        self.searchable = searchable;
        self.replace_conversations_snapshot(Arc::new(snapshot));
        self.send_search_data();
        self.invalidate_search_generation();

        // Rows on screen until the rerun query answers, by their new index.
        self.filtered = self
            .filtered
            .iter()
            .filter_map(|&old| new_index_of_old.get(old).copied().flatten())
            .collect();
        self.selection_anchor = selected_path;
        self.select_anchor_or_first();
        self.rerun_query();
    }
}
