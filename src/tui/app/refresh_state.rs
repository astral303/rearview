//! The list's refreshes on the TUI's background worker. A list `Ctrl+R`
//! hands the worker the list's known sessions; a session refresh hands it
//! the sessions that asked for one, to read again and cache. Each outcome
//! is applied through [`App::apply_finished_refresh`].

use super::session_store::RowsAwaitingRefresh;
use super::{App, AppMode, DialogMode};
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
}

impl ListEdit {
    fn counts(&self) -> AppliedChanges {
        AppliedChanges {
            new: self.inserted.len(),
            changed: self.replaced.len() + self.retitled.len(),
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
    /// session that is gone or empty removes it. A failed read, or a session
    /// missing from the cache, keeps the row: the read reveals nothing about
    /// the session.
    fn apply_reread(&mut self, index: usize, read: SessionRead) {
        match read {
            SessionRead::Listed(row) => {
                self.replaced.insert(index, *row);
            }
            SessionRead::Missing | SessionRead::Empty => {
                self.removed.insert(index);
            }
            SessionRead::Failed(_) | SessionRead::NotCached => {}
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
    let targets: Vec<RefreshTarget> = rows.iter().map(|row| RefreshTarget::of(row)).collect();
    let read = read_by_source(&targets, |source, locators| {
        Ok::<_, std::convert::Infallible>(reread_sessions(environment, source, locators, show_last))
    });
    match read {
        Ok(outcomes) => outcomes,
        Err(never) => match never {},
    }
}

/// A session a refresh reads again: its agent, its file, and the row the
/// list shows for it; `None` for a session found by ID that the list does
/// not hold.
#[derive(Clone)]
struct RefreshTarget {
    source: Source,
    path: PathBuf,
    listed: Option<Conversation>,
}

impl RefreshTarget {
    fn of(row: &Conversation) -> Self {
        Self {
            source: row.source,
            path: row.path.clone(),
            listed: Some(row.clone()),
        }
    }
}

/// `targets`' sessions read with `read`, called once per agent with the
/// files of that agent's targets, one outcome per target in order. A listed
/// row keeps its index and the project it is filed under.
fn read_by_source<E>(
    targets: &[RefreshTarget],
    read: impl Fn(Source, &[&Path]) -> std::result::Result<Vec<SessionOutcome>, E>,
) -> std::result::Result<Vec<SessionOutcome>, E> {
    let mut by_source: Vec<(Source, Vec<usize>)> = Vec::new();
    for (index, target) in targets.iter().enumerate() {
        match by_source
            .iter_mut()
            .find(|(source, _)| *source == target.source)
        {
            Some((_, indices)) => indices.push(index),
            None => by_source.push((target.source, vec![index])),
        }
    }
    let mut outcomes: Vec<Option<SessionOutcome>> = targets.iter().map(|_| None).collect();
    for (source, indices) in by_source {
        let locators: Vec<&Path> = indices.iter().map(|&i| targets[i].path.as_path()).collect();
        for (&index, outcome) in indices.iter().zip(read(source, &locators)?) {
            outcomes[index] = Some(match &targets[index].listed {
                Some(row) => outcome.in_place_of(row),
                None => outcome,
            });
        }
    }
    Ok(outcomes
        .into_iter()
        .map(|outcome| outcome.expect("one outcome per target"))
        .collect())
}

/// A session refresh: sessions to read again and cache on the background
/// worker. A session opened out of date, the open session on a viewer
/// `Ctrl+R`, a session found by ID that the cache lacks, and a session a
/// refresh read without caching it each ask for one. While another refresh
/// owns the cache, the request waits until that refresh completes.
pub struct SessionRefresh {
    environment: Environment,
    targets: Vec<RefreshTarget>,
    show_last: bool,
}

impl SessionRefresh {
    /// Read each session again under `operation`'s refresh ownership and
    /// cache it ([`cache_sessions`]). Run on the background worker: it waits
    /// for ownership as a load does.
    pub fn refresh(self, operation: &OperationScope) -> Result<RefreshOutcome> {
        Ok(RefreshOutcome::Cached(read_by_source(
            &self.targets,
            |source, locators| {
                cache_sessions(
                    &self.environment,
                    source,
                    locators,
                    self.show_last,
                    operation,
                )
            },
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
            self.store.request_list_refresh();
        }
    }

    /// Begin the requested list refresh, returning what the list holds for
    /// it to compare discovery against. `None`, and nothing begins, while
    /// none is requested, the initial load runs, another refresh runs or
    /// waits to be applied, a rename or delete waits for confirmation, or
    /// for a file opened directly.
    pub fn begin_requested_refresh(&mut self) -> Option<KnownSessions> {
        if self.single_file_mode
            || self.is_loading()
            || self.is_confirming_a_list_change()
            || !self.store.begin_list_refresh()
        {
            return None;
        }
        Some(KnownSessions {
            listed: self
                .conversations
                .iter()
                .map(|conversation| (conversation.path.clone(), FoundSession::of(conversation)))
                .collect(),
            skipped: self.skipped_sessions.clone(),
        })
    }

    /// Request a list refresh and begin it at once; `None` when it cannot
    /// begin (see [`Self::begin_requested_refresh`]).
    #[cfg(test)]
    pub fn begin_refresh(&mut self) -> Option<KnownSessions> {
        if self.is_refreshing() {
            return None;
        }
        self.request_list_refresh();
        self.begin_requested_refresh()
    }

    /// Begin a session refresh of every session that asked for one,
    /// returning them for the background worker. `None`, and nothing begins,
    /// while none asked, the initial load or another refresh runs, a rename
    /// or delete waits for confirmation, or for a file opened directly.
    pub fn begin_session_refresh(&mut self) -> Option<SessionRefresh> {
        if self.single_file_mode || self.is_loading() || self.is_confirming_a_list_change() {
            return None;
        }
        let paths = self.store.begin_session_refresh()?;
        let targets: Vec<RefreshTarget> = paths
            .iter()
            .filter_map(
                |path| match self.conversations.iter().find(|row| &row.path == path) {
                    Some(row) => Some(RefreshTarget::of(row)),
                    None => self
                        .store
                        .looked_up_source(path)
                        .map(|source| RefreshTarget {
                            source,
                            path: path.clone(),
                            listed: None,
                        }),
                },
            )
            .collect();
        Some(SessionRefresh {
            environment: self.environment.clone(),
            targets,
            show_last: self.show_last,
        })
    }

    /// True while a rename's name box or a delete's confirmation is open:
    /// no refresh begins under them, so none changes the row they act on.
    fn is_confirming_a_list_change(&self) -> bool {
        matches!(
            self.dialog_mode,
            DialogMode::Rename { .. } | DialogMode::ConfirmDelete
        )
    }

    /// Take a list or session refresh's outcome. The open session follows a
    /// read of it at once (see [`Self::follow_open_session`]); the list
    /// applies the outcome once it is on screen with nothing over it.
    pub fn finish_refresh(&mut self, outcome: Result<RefreshOutcome>) {
        self.store.finish(outcome);
        self.follow_open_session();
        self.apply_finished_refresh();
    }

    /// Take the changes the launch read under its own refresh ownership,
    /// behind another rearview's refresh, applied as a list refresh's.
    pub fn add_launch_changes(&mut self, changes: SessionChanges) {
        self.store
            .add_launch_outcome(Ok(RefreshOutcome::Changes(changes)));
        self.follow_open_session();
        self.apply_finished_refresh();
    }

    /// The launch's cache writes stopped part way: ask for a session refresh
    /// of every row it parsed rather than restored.
    pub fn refresh_rows_the_launch_parsed(&mut self) {
        let parsed: Vec<PathBuf> = self
            .conversations
            .iter()
            .filter(|row| row.cached_session.is_none())
            .map(|row| row.path.clone())
            .collect();
        for path in parsed {
            self.store.request_session_refresh(&path);
        }
    }

    /// Apply every finished refresh, oldest first, once the list can be
    /// edited. A refresh that finishes in the viewer, under a dialog or
    /// before a requested open waits, so no row moves under an open session
    /// or a confirmation.
    pub fn apply_finished_refresh(&mut self) {
        if !self.is_list_editable() {
            return;
        }
        for outcome in self.store.take_finished() {
            match outcome {
                Ok(RefreshOutcome::Changes(changes)) => {
                    if let Some(summary) = self.apply_session_changes(changes).summary() {
                        self.set_status(summary);
                    }
                }
                Ok(RefreshOutcome::Cached(outcomes)) => self.apply_cached_reads(outcomes),
                Err(error) => self.set_status(format!("Refresh failed: {error}")),
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

    /// Apply `outcomes`, the sessions a session refresh read: replace a
    /// listed row, remove a session that is gone or empty, keep a row whose
    /// read failed, and add the row of a session found by ID. The TUI acts
    /// on each session's next step.
    fn apply_cached_reads(&mut self, outcomes: Vec<SessionOutcome>) {
        let index_of: HashMap<PathBuf, usize> = self
            .conversations
            .iter()
            .enumerate()
            .map(|(index, row)| (row.path.clone(), index))
            .collect();
        let mut edit = ListEdit::default();
        for outcome in outcomes {
            let path = outcome.locator().to_path_buf();
            let is_looked_up = self.store.looked_up_source(&path).is_some();
            self.store.forget_lookup(&path);
            let read = self.act_on_outcome(outcome);
            match (index_of.get(&path), read) {
                (Some(&index), read) => edit.apply_reread(index, read),
                (None, SessionRead::Listed(row)) if is_looked_up => edit.inserted.push(*row),
                (None, _) if is_looked_up => self.report_lookup_unresolved(&path),
                (None, _) => {}
            }
        }
        if !edit.is_empty() {
            self.edit_list(edit);
        }
    }

    /// True from the start of a refresh until the list applies its outcome.
    pub fn is_refreshing(&self) -> bool {
        self.store.is_refreshing()
    }

    /// Refresh the list with `changes`, keeping the selection on its session
    /// and the search data of every row that did not change.
    pub fn apply_session_changes(&mut self, changes: SessionChanges) -> AppliedChanges {
        let index_of: HashMap<PathBuf, usize> = self
            .conversations
            .iter()
            .enumerate()
            .map(|(index, conversation)| (conversation.path.clone(), index))
            .collect();
        let mut edit = ListEdit::default();
        for path in changes.removed {
            if let Some(&index) = index_of.get(&path) {
                edit.removed.insert(index);
            }
        }
        for UpdatedSession {
            row,
            replaces_listed_row,
            next,
        } in changes.updated
        {
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
            if index_of.contains_key(&kept.path) {
                self.act_on_next_step(&kept.path, kept.next);
            }
        }
        for (source, titles) in &changes.external_titles {
            for (index, conversation) in self.conversations.iter_mut().enumerate() {
                if conversation.source == *source
                    && !edit.replaced.contains_key(&index)
                    && !edit.removed.contains(&index)
                    && let Some(title) = titles.get(&conversation.session_id)
                    && apply_external_title(conversation, title)
                {
                    edit.retitled.insert(index);
                }
            }
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
                self.store.request_session_refresh(path);
            }
            NextStep::CacheInBackground | NextStep::Settled => {}
        }
    }

    /// Ask for a session refresh of the rows at `paths` once `awaited`
    /// completes.
    pub(super) fn await_refresh(
        &mut self,
        paths: impl IntoIterator<Item = PathBuf>,
        awaited: AwaitedRefresh,
    ) {
        let next_check = self.clock().now() + AWAITED_REFRESH_CHECK_INTERVAL;
        let awaiting = self
            .store
            .awaiting
            .get_or_insert_with(|| RowsAwaitingRefresh {
                paths: HashSet::new(),
                awaited,
                next_check,
            });
        awaiting.paths.extend(paths);
        awaiting.awaited = awaited;
    }

    /// The other rearview's work the list waits for: the wait the
    /// running load or background job publishes, or the refresh the
    /// sessions in `RowsAwaitingRefresh` wait for; `None` while the list
    /// waits for no other rearview.
    pub fn another_rearview_activity(&self) -> Option<OtherRearview> {
        self.cache_wait
            .or_else(|| match self.store.awaiting.as_ref()?.awaited {
                AwaitedRefresh::AnotherRearview(activity) => Some(activity),
                AwaitedRefresh::ThisProcess => None,
            })
    }

    /// True while the list waits for another rearview's hold on the session
    /// cache.
    #[cfg(test)]
    pub fn is_waiting_for_another_rearview(&self) -> bool {
        self.another_rearview_activity().is_some()
    }

    /// True while sessions wait for a session refresh to begin.
    #[cfg(test)]
    pub fn is_session_refresh_requested(&self) -> bool {
        self.store.is_session_refresh_requested()
    }

    /// True while rows wait for a cache refresh to complete, whichever
    /// process runs it.
    #[cfg(test)]
    pub fn is_awaiting_refresh(&self) -> bool {
        self.store.awaiting.is_some()
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
        if let Some(awaiting) = &mut self.store.awaiting {
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
    /// rows wait for has completed. Once it has, ask for a session refresh
    /// of the waiting rows. A file opened directly reads its row again in
    /// place: no background worker serves it.
    pub fn check_awaited_refresh(&mut self) {
        let now = self.clock().now();
        let cache = self.interactive_cache();
        let Some(awaiting) = &mut self.store.awaiting else {
            return;
        };
        if now < awaiting.next_check {
            return;
        }
        awaiting.next_check = now + AWAITED_REFRESH_CHECK_INTERVAL;
        if let Some(awaited) = cache.awaited_refresh() {
            awaiting.awaited = awaited;
            return;
        }
        let paths = std::mem::take(&mut awaiting.paths);
        self.store.awaiting = None;
        if self.single_file_mode {
            let indices: Vec<usize> = self
                .conversations
                .iter()
                .enumerate()
                .filter(|(_, row)| paths.contains(&row.path))
                .map(|(index, _)| index)
                .collect();
            self.read_opened_file_again(&indices);
            return;
        }
        for path in &paths {
            self.store.request_session_refresh(path);
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
