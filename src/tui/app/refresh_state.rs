//! The `Ctrl+R` refresh: the list's known sessions are sent to a refresh
//! thread, and its changes are applied through
//! [`App::apply_session_changes`].

use super::{App, AppMode, DialogMode, RefreshState};
use crate::error::Result;
use crate::history::provider::{SessionRead, apply_external_title, reread_sessions};
use crate::history::{
    Conversation, FoundSession, KnownSessions, SessionChanges, SkippedSessions, UpdatedSession,
};
use crate::search;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

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
}

/// `rows`' sessions read again from disk as the list builds their rows, one
/// result each in order, sub-agent transcripts included: the file each row
/// names, as its agent's discovery lists it, not another copy stored under
/// the same session id. Each agent's discovery runs once for all of its
/// rows. A row keeps its index and the project it is filed under, and
/// previews its last messages when `show_last` is true. `None` when the
/// agent no longer finds the file.
pub(super) fn read_listed_sessions(
    rows: &[&Conversation],
    show_last: bool,
) -> Vec<Option<SessionRead>> {
    let mut by_source: Vec<(crate::history::Source, Vec<usize>)> = Vec::new();
    for (index, row) in rows.iter().enumerate() {
        match by_source
            .iter_mut()
            .find(|(source, _)| *source == row.source)
        {
            Some((_, indices)) => indices.push(index),
            None => by_source.push((row.source, vec![index])),
        }
    }
    let mut reads: Vec<Option<SessionRead>> = rows.iter().map(|_| None).collect();
    for (source, indices) in by_source {
        let locators: Vec<&Path> = indices.iter().map(|&i| rows[i].path.as_path()).collect();
        for (&index, mut read) in indices
            .iter()
            .zip(reread_sessions(source, &locators, show_last))
        {
            if let Some(SessionRead::Listed(session)) = &mut read {
                let row = rows[index];
                session.index = row.index;
                session.project_name.clone_from(&row.project_name);
                session.project_path.clone_from(&row.project_path);
            }
            reads[index] = read;
        }
    }
    reads
}

impl App {
    /// Record the sessions the load found for a refresh to skip.
    pub fn add_skipped_sessions(&mut self, sessions: SkippedSessions) {
        self.skipped_sessions.extend(sessions);
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

    /// Take a refresh's outcome, applying it at once when the list is on
    /// screen with nothing over it.
    pub fn finish_refresh(&mut self, outcome: Result<SessionChanges>) {
        self.refresh = RefreshState::Finished(outcome);
        self.apply_finished_refresh();
    }

    /// Apply a finished refresh once the list is on screen with nothing over
    /// it: one that finishes in the viewer or under a dialog waits, so no row
    /// moves under an open session or a confirmation.
    pub fn apply_finished_refresh(&mut self) {
        if !matches!(self.app_mode, AppMode::List)
            || self.dialog_mode != DialogMode::None
            || self.pending_open.is_some()
            || !matches!(self.refresh, RefreshState::Finished(_))
        {
            return;
        }
        let RefreshState::Finished(outcome) =
            std::mem::replace(&mut self.refresh, RefreshState::Idle)
        else {
            return;
        };
        match outcome {
            Ok(changes) => {
                if let Some(summary) = self.apply_session_changes(changes).summary() {
                    self.set_status(summary);
                }
            }
            Err(error) => {
                self.changed_during_refresh.clear();
                self.set_status(format!("Refresh failed: {error}"));
            }
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
        } in changes.updated
        {
            if changed_by_user.contains(&row.path) {
                continue;
            }
            match (index_of.get(&row.path).copied(), replaces_listed_row) {
                (Some(index), true) if !edit.removed.contains(&index) => {
                    edit.replaced.insert(index, row);
                }
                (None, false) => edit.inserted.push(row),
                _ => {}
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
        let rows: Vec<&Conversation> = reread
            .iter()
            .map(|&index| &self.conversations[index])
            .collect();
        let reads = (self.session_reader)(&rows, self.show_last);
        for ((index, listed), read) in reread.into_iter().zip(rows).zip(reads) {
            match read {
                Some(SessionRead::Listed(read)) => {
                    if read.fingerprint.is_some() && read.fingerprint == listed.fingerprint {
                        edit.reread_unchanged.insert(index);
                    }
                    edit.replaced.insert(index, *read);
                }
                None | Some(SessionRead::Empty) => {
                    edit.removed.insert(index);
                }
                // Keep the row as the user's change left it; a failed read
                // says nothing about the session.
                Some(SessionRead::Failed(_)) => {}
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
