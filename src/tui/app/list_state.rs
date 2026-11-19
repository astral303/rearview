use super::{App, AppMode, DialogMode};
use crate::clock::Moment;
use crate::error::AppError;
use crate::history::provider::Deleted;
use crate::history::{Conversation, ExcludedProjects, Workspace};
use crate::tui::list_change::{DeleteStages, ListChange, ListChangeOutcome};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

const LIST_PAGE_ROWS: isize = 10;

/// A delete at least this long records its stages in the debug log.
const SLOW_DELETE_THRESHOLD: Duration = Duration::from_secs(1);

/// The session the list's selection stays on while the rows change.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum SelectionAnchor {
    /// Kept until the user's next key, click on a row or wheel turn on the
    /// loaded list.
    Kept(PathBuf),
    /// Selected by the user while the list loads, until the first results
    /// after the load.
    SelectedWhileLoading(PathBuf),
}

/// The session the selection stays on after a change, while its row is
/// shown, and the row it falls back to otherwise.
pub(super) struct SelectionTarget {
    session: Option<PathBuf>,
    row: usize,
}

impl SelectionTarget {
    /// Row `row`, without a session to stay on.
    pub(super) fn row(row: usize) -> Self {
        Self { session: None, row }
    }
}

/// The sessions whose confirmed delete is queued or running, and since when
/// a delete has been in progress without a break.
#[derive(Default)]
pub(super) struct DeletingSessions {
    paths: HashSet<PathBuf>,
    since: Option<Moment>,
}

impl DeletingSessions {
    fn add(&mut self, path: PathBuf, now: Moment) {
        self.since.get_or_insert(now);
        self.paths.insert(path);
    }

    fn remove(&mut self, path: &Path) {
        self.paths.remove(path);
        if self.paths.is_empty() {
            self.since = None;
        }
    }

    fn contains(&self, path: &Path) -> bool {
        self.paths.contains(path)
    }

    /// The count of sessions being deleted, and since when one has been;
    /// `None` while none is.
    pub(super) fn in_progress(&self) -> Option<(usize, Moment)> {
        Some((self.paths.len(), self.since?))
    }
}

impl SelectionAnchor {
    fn path(&self) -> &Path {
        match self {
            Self::Kept(path) | Self::SelectedWhileLoading(path) => path,
        }
    }
}

impl App {
    pub(super) fn filter_indices<I>(&self, indices: I) -> Vec<usize>
    where
        I: IntoIterator<Item = usize>,
    {
        filter_conversation_indices(
            indices,
            &self.conversations,
            &self.excluded_projects,
            self.workspace_filter,
            self.workspace.as_ref(),
        )
    }

    pub(super) fn apply_filtered(&mut self, filtered: Vec<usize>) {
        self.filtered = filtered;
        self.place_selection(self.anchored_target());
        self.settle_selection_made_while_loading();
    }

    /// The one write of the selection outside tests. Select `target`'s
    /// session while its row is selectable; otherwise the selectable row
    /// nearest that row, or nearest `target`'s fallback row when the
    /// session is not shown. The selection is `None` only while no shown
    /// row is selectable.
    pub(super) fn place_selection(&mut self, target: SelectionTarget) {
        let row = target
            .session
            .as_deref()
            .and_then(|session| self.shown_row_of(session))
            .unwrap_or(target.row);
        self.selected = self.selectable_row_near(row);
    }

    /// Place the selection again after rows became selectable, stopped being
    /// selectable or were added below the shown rows: it stays on its
    /// session while that session can be selected; with nothing selected,
    /// the first selectable row is selected.
    pub(super) fn settle_selection(&mut self) {
        self.place_selection(self.selection_target(|_| true));
    }

    /// The session the selection is anchored to, else the first row.
    pub(super) fn anchored_target(&self) -> SelectionTarget {
        SelectionTarget {
            session: self
                .selection_anchor
                .as_ref()
                .map(|anchor| anchor.path().to_path_buf()),
            row: 0,
        }
    }

    /// The shown row of the session at `path`.
    fn shown_row_of(&self, path: &Path) -> Option<usize> {
        self.filtered
            .iter()
            .position(|&index| self.conversations[index].path == path)
    }

    /// True when shown row `row` can be selected: its session is not being
    /// deleted.
    pub(super) fn is_selectable(&self, row: usize) -> bool {
        self.filtered
            .get(row)
            .is_some_and(|&index| !self.deleting.contains(&self.conversations[index].path))
    }

    /// `row` when it is selectable, else the next selectable row after it,
    /// else the nearest one before it; `None` when no shown row is
    /// selectable.
    pub(super) fn selectable_row_near(&self, row: usize) -> Option<usize> {
        (row..self.filtered.len())
            .find(|&row| self.is_selectable(row))
            .or_else(|| {
                (0..row.min(self.filtered.len()))
                    .rev()
                    .find(|&row| self.is_selectable(row))
            })
    }

    /// Drop the anchor of a selection made while the list loaded when the
    /// first results after the load don't show its session; keep it when
    /// they do. Once the first row is selected, a later search, such as the
    /// rerun after the sessions' texts are read, must not move the selection
    /// back to that session without any input.
    fn settle_selection_made_while_loading(&mut self) {
        if self.is_loading() {
            return;
        }
        if let Some(SelectionAnchor::SelectedWhileLoading(path)) = &self.selection_anchor {
            let is_shown = self.get_selected_path().as_ref() == Some(path);
            self.selection_anchor = is_shown.then(|| SelectionAnchor::Kept(path.clone()));
        }
    }

    /// A key, a click on a row or a wheel turn arrived: clear the selection
    /// anchor on a loaded list. While the list loads, the anchor stays until
    /// the load's end.
    pub(super) fn clear_selection_anchor_on_loaded_list(&mut self) {
        if !self.is_loading() {
            self.selection_anchor = None;
        }
    }

    /// Select `row` for a key or wheel turn. A key or wheel turn that leaves
    /// the selection on its row doesn't choose that row.
    pub(super) fn select_row(&mut self, row: usize) {
        let leaves_selection_on_its_row = self.selected == Some(row);
        if !leaves_selection_on_its_row {
            self.choose_row(row);
        }
    }

    /// Select the row a click landed on. A click on the selected row chooses
    /// it too: the user pointed at that session.
    pub(super) fn click_row(&mut self, row: usize) {
        self.choose_row(row);
    }

    /// Select `row` as the user's choice. While the list loads, anchor the
    /// selection to the row's session, so the load's end keeps it selected.
    fn choose_row(&mut self, row: usize) {
        self.place_selection(SelectionTarget::row(row));
        if self.is_loading() {
            self.selection_anchor = self
                .get_selected_path()
                .map(SelectionAnchor::SelectedWhileLoading);
        }
    }

    pub(super) fn select_prev(&mut self) {
        self.move_selection_by(-1);
    }

    pub(super) fn select_next(&mut self) {
        self.move_selection_by(1);
    }

    pub(super) fn select_first(&mut self) {
        if let Some(row) = self.selectable_row_near(0) {
            self.select_row(row);
        }
    }

    pub(super) fn select_last(&mut self) {
        if let Some(row) = (0..self.filtered.len())
            .rev()
            .find(|&row| self.is_selectable(row))
        {
            self.select_row(row);
        }
    }

    pub(super) fn select_page_up(&mut self) {
        self.move_selection_by(-LIST_PAGE_ROWS);
    }

    pub(super) fn select_page_down(&mut self) {
        self.move_selection_by(LIST_PAGE_ROWS);
    }

    pub(super) fn select_half_page_down(&mut self, viewport_height: usize) {
        self.move_selection_by((viewport_height / 2) as isize);
    }

    /// Move the selection `delta` rows, stopping at the first and last row.
    /// A row that can't be selected is passed over: the selection lands on
    /// the next selectable row in the direction of the move, or, with none
    /// there, on the selectable row nearest the target on the way back.
    pub(super) fn move_selection_by(&mut self, delta: isize) {
        let Some(selected) = self.selected else {
            return;
        };

        let max = self.filtered.len().saturating_sub(1);
        let landing = if delta >= 0 {
            let target = selected.saturating_add(delta as usize).min(max);
            (target..=max)
                .find(|&row| self.is_selectable(row))
                .or_else(|| {
                    (selected..target)
                        .rev()
                        .find(|&row| self.is_selectable(row))
                })
        } else {
            let target = selected.saturating_sub((-delta) as usize);
            (0..=target)
                .rev()
                .find(|&row| self.is_selectable(row))
                .or_else(|| (target + 1..=selected).find(|&row| self.is_selectable(row)))
        };
        if let Some(row) = landing {
            self.select_row(row);
        }
    }

    pub(super) fn get_selected_path(&self) -> Option<PathBuf> {
        self.selected
            .and_then(|sel| self.filtered.get(sel))
            .map(|&idx| self.conversations[idx].path.clone())
    }

    /// The listed row of the session at `path`.
    pub fn listed_row(&self, path: &Path) -> Option<&Conversation> {
        self.conversations.iter().find(|row| row.path == path)
    }

    pub(crate) fn has_multiple_sources(&self) -> bool {
        let sources = self
            .conversations
            .iter()
            .map(|conversation| conversation.source)
            .collect::<HashSet<_>>();
        sources.len() > 1
    }

    /// Ask the background worker to delete the listed session at `path`, and
    /// show its row as deleting until the delete ends: greyed, and passed
    /// over by the selection. A viewer of the session closes, so the list
    /// shows the row.
    pub(crate) fn request_delete(&mut self, path: &Path) {
        let Some(source) = self.listed_row(path).map(|row| row.source) else {
            let _ = crate::debug_log::log_debug(
                &format!("No listed session to delete at {}", path.display()),
                self.clock(),
            );
            return;
        };
        let now = self.clock().now();
        self.list_changes.push_back(ListChange::Delete {
            source,
            path: path.to_path_buf(),
            confirmed_at: now,
        });
        self.deleting.add(path.to_path_buf(), now);
        if self.is_viewing(path) {
            self.exit_view_mode();
        }
        self.settle_selection();
    }

    /// True when the viewer shows the session at `path`.
    fn is_viewing(&self, path: &Path) -> bool {
        matches!(&self.app_mode, AppMode::View(state) if state.conversation_path == path)
    }

    /// True while the listed session at `path` is being deleted.
    pub fn is_deleting(&self, path: &Path) -> bool {
        self.deleting.contains(path)
    }

    /// Hand the background worker the oldest confirmed rename or delete;
    /// `None` while none waits.
    pub(crate) fn begin_list_change(&mut self) -> Option<ListChange> {
        self.list_changes.pop_front()
    }

    /// Show the result of a rename or delete.
    pub(crate) fn finish_list_change(&mut self, outcome: ListChangeOutcome) {
        match outcome {
            ListChangeOutcome::Renamed {
                path,
                title,
                result,
            } => self.finish_rename(&path, &title, result),
            ListChangeOutcome::Deleted {
                path,
                result,
                stages,
            } => {
                let update_started_at = self.clock().now();
                self.finish_delete(&path, result);
                let list_update = self.clock().elapsed_since(update_started_at);
                self.log_slow_delete(&path, stages, list_update);
            }
        }
    }

    /// Remove the deleted session at `path` from the list and leave its
    /// viewer, or report why it could not be deleted and show its row as
    /// before.
    fn finish_delete(&mut self, path: &Path, result: crate::error::Result<Deleted>) {
        self.deleting.remove(path);
        self.settle_selection();
        match result {
            Ok(_) => {
                self.remove_from_list(path);
                if self.is_viewing(path) {
                    self.exit_view_mode();
                }
            }
            Err(error) => {
                let _ = crate::debug_log::log_debug(
                    &format!("Failed to delete session {}: {}", path.display(), error),
                    self.clock(),
                );
                self.report_delete_failure(&error);
            }
        }
    }

    /// Record in the debug log the time each stage of a delete of the session
    /// at `path` took, when the delete took [`SLOW_DELETE_THRESHOLD`] or
    /// longer from the user's `y`: `stages` in the queue and on the
    /// background worker, then `list_update`.
    fn log_slow_delete(&self, path: &Path, stages: DeleteStages, list_update: Duration) {
        let total =
            stages.queue + stages.ownership + stages.write_batch + stages.provider + list_update;
        if total < SLOW_DELETE_THRESHOLD {
            return;
        }
        let _ = crate::debug_log::log_debug(
            &format!(
                "Slow delete of {}: {} ms: queue {} ms, refresh ownership {} ms, \
                 write batch {} ms, provider {} ms, list update {} ms",
                path.display(),
                total.as_millis(),
                stages.queue.as_millis(),
                stages.ownership.as_millis(),
                stages.write_batch.as_millis(),
                stages.provider.as_millis(),
                list_update.as_millis(),
            ),
            self.clock(),
        );
    }

    pub(crate) fn report_delete_failure(&mut self, error: &AppError) {
        self.set_status(format!("Failed to delete: {error}"));
    }

    /// Remove the row of the session at `path`, keeping the selection on its
    /// session (see [`Self::place_selection`]).
    fn remove_from_list(&mut self, path: &Path) {
        let Some(conv_idx) = self.conversations.iter().position(|row| row.path == path) else {
            return;
        };
        let selection = self.selection_target(|index| index != conv_idx);

        self.conversations.remove(conv_idx);

        self.searchable.retain_mut(|s| {
            if s.index == conv_idx {
                false
            } else {
                if s.index > conv_idx {
                    s.index -= 1;
                }
                true
            }
        });

        self.filtered.retain(|&idx| idx != conv_idx);
        for idx in &mut self.filtered {
            if *idx > conv_idx {
                *idx -= 1;
            }
        }

        self.place_selection(selection);

        self.refresh_search_data();
        self.close_dialog_of_unlisted_session();
    }

    /// The selected session before the shown rows change, and the row that
    /// takes its place once it is gone: the count of shown rows above it
    /// whose conversation index `stays` holds for. Row 0 when nothing is
    /// selected.
    pub(super) fn selection_target(&self, stays: impl Fn(usize) -> bool) -> SelectionTarget {
        let (Some(selected), Some(session)) = (self.selected, self.get_selected_path()) else {
            return SelectionTarget::row(0);
        };
        SelectionTarget {
            session: Some(session),
            row: self.filtered[..selected]
                .iter()
                .filter(|&&index| stays(index))
                .count(),
        }
    }

    /// Anchor the selection to the selected session, so the results of the
    /// query rerun after a change keep it there.
    pub(super) fn anchor_selection_to_its_session(&mut self) {
        self.selection_anchor = self.get_selected_path().map(SelectionAnchor::Kept);
    }

    /// Close the actions menu, the delete prompt or the name box once the
    /// session it was opened for is no longer listed.
    pub(super) fn close_dialog_of_unlisted_session(&mut self) {
        let is_unlisted = self
            .dialog_mode
            .session()
            .is_some_and(|session| self.listed_row(session).is_none());
        if is_unlisted {
            self.dialog_mode = DialogMode::None;
        }
    }
}

pub(super) fn filter_conversation_indices<I>(
    indices: I,
    conversations: &[Conversation],
    excluded_projects: &ExcludedProjects,
    workspace_filter: bool,
    workspace: Option<&Workspace>,
) -> Vec<usize>
where
    I: IntoIterator<Item = usize>,
{
    let workspace = workspace.filter(|_| workspace_filter);
    indices
        .into_iter()
        .filter(|&idx| !excluded_projects.excludes(&conversations[idx]))
        .filter(|&idx| workspace.is_none_or(|workspace| workspace.contains(&conversations[idx])))
        .collect()
}
