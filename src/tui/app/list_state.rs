use super::App;
use crate::history::{Conversation, ExcludedProjects, Workspace};
use std::collections::HashSet;
use std::path::PathBuf;

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
        self.select_anchor_or_first();
    }

    /// Select the session the selection is anchored to, when the list shows
    /// it, else the first row.
    pub(super) fn select_anchor_or_first(&mut self) {
        let anchored = self.selection_anchor.as_ref().and_then(|anchor| {
            self.filtered
                .iter()
                .position(|&index| &self.conversations[index].path == anchor)
        });
        self.selected = anchored.or_else(|| (!self.filtered.is_empty()).then_some(0));
    }

    pub(super) fn select_prev(&mut self) {
        if let Some(selected) = self.selected
            && selected > 0
        {
            self.selected = Some(selected - 1);
        }
    }

    pub(super) fn select_next(&mut self) {
        if let Some(selected) = self.selected
            && selected + 1 < self.filtered.len()
        {
            self.selected = Some(selected + 1);
        }
    }

    pub(super) fn select_first(&mut self) {
        if !self.filtered.is_empty() {
            self.selected = Some(0);
        }
    }

    pub(super) fn select_last(&mut self) {
        if !self.filtered.is_empty() {
            self.selected = Some(self.filtered.len() - 1);
        }
    }

    pub(super) fn select_page_up(&mut self) {
        if let Some(selected) = self.selected {
            self.selected = Some(selected.saturating_sub(10));
        }
    }

    pub(super) fn select_page_down(&mut self) {
        if let Some(selected) = self.selected {
            let new_selected = (selected + 10).min(self.filtered.len().saturating_sub(1));
            self.selected = Some(new_selected);
        }
    }

    pub(super) fn select_half_page_down(&mut self, viewport_height: usize) {
        if let Some(selected) = self.selected {
            let half_page = viewport_height / 2;
            let new_selected = (selected + half_page).min(self.filtered.len().saturating_sub(1));
            self.selected = Some(new_selected);
        }
    }

    pub(super) fn scroll_list(&mut self, delta: isize) {
        self.selection_anchor = None;
        let Some(selected) = self.selected else {
            return;
        };

        let max = self.filtered.len().saturating_sub(1);
        let new_selected = if delta >= 0 {
            selected.saturating_add(delta as usize).min(max)
        } else {
            selected.saturating_sub((-delta) as usize)
        };
        self.selected = Some(new_selected);
    }

    pub(super) fn get_selected_path(&self) -> Option<PathBuf> {
        self.selected
            .and_then(|sel| self.filtered.get(sel))
            .map(|&idx| self.conversations[idx].path.clone())
    }

    pub(crate) fn get_selected_source(&self) -> Option<crate::history::Source> {
        self.get_selected_conversation_index()
            .map(|index| self.conversations[index].source)
    }

    pub(crate) fn has_multiple_sources(&self) -> bool {
        let sources = self
            .conversations
            .iter()
            .map(|conversation| conversation.source)
            .collect::<HashSet<_>>();
        sources.len() > 1
    }

    pub(super) fn get_selected_conversation_index(&self) -> Option<usize> {
        self.selected
            .and_then(|sel| self.filtered.get(sel))
            .copied()
    }

    pub(crate) fn remove_selected_from_list(&mut self) {
        let Some(selected) = self.selected else {
            return;
        };
        let Some(&conv_idx) = self.filtered.get(selected) else {
            return;
        };

        let removed = self.conversations.remove(conv_idx);
        self.note_changed_during_refresh(&removed.path);

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

        if self.filtered.is_empty() {
            self.selected = None;
        } else if selected >= self.filtered.len() {
            self.selected = Some(self.filtered.len() - 1);
        }

        self.refresh_search_data();
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
