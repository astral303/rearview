use super::{Action, App, AppMode, DialogMode};
use crate::history::provider::Operation;
use crossterm::event::{KeyCode, KeyModifiers};

/// An export's course when the session is missing from the cache.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum CopyMissing {
    /// Wait for the session's refresh, then export.
    WaitForTheRefresh,
    /// Report the failure: the export already waited for the refresh.
    Report,
}

const EXPORT_OPTIONS: [&str; 4] = [
    "Ledger (formatted)",
    "Plain text",
    "Markdown",
    "JSONL (raw)",
];

impl App {
    pub(super) fn handle_confirm_key(&mut self, code: KeyCode) -> Option<Action> {
        match code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                self.dialog_mode = DialogMode::None;
                self.get_selected_path().map(Action::Delete)
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                self.dialog_mode = DialogMode::None;
                None
            }
            _ => None,
        }
    }

    pub(super) fn handle_menu_key(&mut self, code: KeyCode) -> Option<Action> {
        let (selected, is_yank) = match &mut self.dialog_mode {
            DialogMode::ExportMenu { selected } => (selected, false),
            DialogMode::YankMenu { selected } => (selected, true),
            _ => return None,
        };

        match code {
            KeyCode::Up | KeyCode::Char('k') => {
                *selected = selected.saturating_sub(1);
                None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                *selected = (*selected + 1).min(EXPORT_OPTIONS.len() - 1);
                None
            }
            KeyCode::Char('1') => {
                self.perform_export(0, is_yank);
                self.dialog_mode = DialogMode::None;
                None
            }
            KeyCode::Char('2') => {
                self.perform_export(1, is_yank);
                self.dialog_mode = DialogMode::None;
                None
            }
            KeyCode::Char('3') => {
                self.perform_export(2, is_yank);
                self.dialog_mode = DialogMode::None;
                None
            }
            KeyCode::Char('4') => {
                self.perform_export(3, is_yank);
                self.dialog_mode = DialogMode::None;
                None
            }
            KeyCode::Enter => {
                let sel = *selected;
                self.perform_export(sel, is_yank);
                self.dialog_mode = DialogMode::None;
                None
            }
            KeyCode::Esc => {
                self.dialog_mode = DialogMode::None;
                None
            }
            _ => None,
        }
    }

    pub(super) fn handle_help_key(
        &mut self,
        code: KeyCode,
        viewport_height: usize,
    ) -> Option<Action> {
        let DialogMode::Help { scroll } = &mut self.dialog_mode else {
            return None;
        };

        match code {
            KeyCode::Char('?') | KeyCode::Char('q') | KeyCode::Esc => {
                self.dialog_mode = DialogMode::None;
                None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                *scroll = scroll.saturating_add(1);
                None
            }
            KeyCode::Up | KeyCode::Char('k') => {
                *scroll = scroll.saturating_sub(1);
                None
            }
            KeyCode::PageDown | KeyCode::Char('d') => {
                *scroll = scroll.saturating_add(viewport_height.max(1));
                None
            }
            KeyCode::PageUp | KeyCode::Char('u') => {
                *scroll = scroll.saturating_sub(viewport_height.max(1));
                None
            }
            KeyCode::Home | KeyCode::Char('g') => {
                *scroll = 0;
                None
            }
            _ => None,
        }
    }

    pub(super) fn start_rename(&mut self) {
        let Some(idx) = self.get_selected_conversation_index() else {
            return;
        };
        let input = self.conversations[idx]
            .custom_title
            .clone()
            .unwrap_or_default();
        let cursor = input.chars().count();
        self.dialog_mode = DialogMode::Rename { input, cursor };
    }

    pub(super) fn handle_rename_key(
        &mut self,
        code: KeyCode,
        modifiers: KeyModifiers,
    ) -> Option<Action> {
        match code {
            KeyCode::Esc => {
                self.dialog_mode = DialogMode::None;
            }
            KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => {
                self.dialog_mode = DialogMode::None;
            }
            KeyCode::Enter => self.submit_rename(),
            KeyCode::Left => {
                if let DialogMode::Rename { cursor, .. } = &mut self.dialog_mode {
                    *cursor = cursor.saturating_sub(1);
                }
            }
            KeyCode::Right => {
                if let DialogMode::Rename { input, cursor } = &mut self.dialog_mode {
                    *cursor = (*cursor + 1).min(input.chars().count());
                }
            }
            KeyCode::Char('u') if modifiers.contains(KeyModifiers::CONTROL) => {
                if let DialogMode::Rename { input, cursor } = &mut self.dialog_mode {
                    input.clear();
                    *cursor = 0;
                }
            }
            KeyCode::Home | KeyCode::Char('a') if modifiers.contains(KeyModifiers::CONTROL) => {
                if let DialogMode::Rename { cursor, .. } = &mut self.dialog_mode {
                    *cursor = 0;
                }
            }
            KeyCode::End | KeyCode::Char('e') if modifiers.contains(KeyModifiers::CONTROL) => {
                if let DialogMode::Rename { input, cursor } = &mut self.dialog_mode {
                    *cursor = input.chars().count();
                }
            }
            KeyCode::Backspace => {
                if let DialogMode::Rename { input, cursor } = &mut self.dialog_mode
                    && *cursor > 0
                    && let Some((byte_pos, _)) = input.char_indices().nth(*cursor - 1)
                {
                    input.remove(byte_pos);
                    *cursor -= 1;
                }
            }
            KeyCode::Delete => {
                if let DialogMode::Rename { input, cursor } = &mut self.dialog_mode
                    && *cursor < input.chars().count()
                    && let Some((byte_pos, _)) = input.char_indices().nth(*cursor)
                {
                    input.remove(byte_pos);
                }
            }
            KeyCode::Char(ch) if modifiers.is_empty() || modifiers == KeyModifiers::SHIFT => {
                if let DialogMode::Rename { input, cursor } = &mut self.dialog_mode {
                    let byte_pos = input
                        .char_indices()
                        .nth(*cursor)
                        .map(|(i, _)| i)
                        .unwrap_or(input.len());
                    input.insert(byte_pos, ch);
                    *cursor += 1;
                }
            }
            _ => {}
        }
        None
    }

    pub(super) fn submit_rename(&mut self) {
        let title = match &self.dialog_mode {
            DialogMode::Rename { input, .. } => input.trim().to_string(),
            _ => return,
        };
        let Some(idx) = self.get_selected_conversation_index() else {
            self.dialog_mode = DialogMode::None;
            return;
        };
        let row = &self.conversations[idx];
        let provider = row.source.provider();
        let renamed = provider
            .rename()
            .or_refuse(provider.labels().display, Operation::Rename)
            .and_then(|renamer| renamer.rename_session(&row.path, &title, self.clock().wall()));
        match renamed {
            Ok(()) => self.show_renamed_session(idx, &title),
            Err(e) => {
                self.status_message =
                    Some((format!("Failed to rename: {}", e), self.clock().now()));
            }
        }
    }

    /// Show row `idx` renamed to `title`, keeping it selected. The row shows
    /// `title` at once and a session refresh reads the session again. A file
    /// opened directly is read again now, and a failed read keeps `title` on
    /// the row as it is: the rename already saved it.
    pub(super) fn show_renamed_session(&mut self, idx: usize, title: &str) {
        use crate::history::provider::SessionRead;
        let path = self.conversations[idx].path.clone();
        let titled = Some(title.to_owned()).filter(|title| !title.is_empty());
        if !self.single_file_mode {
            self.conversations[idx].custom_title = titled;
            self.store.request_session_refresh(&path);
        } else {
            let read = self
                .read_rows_again(&[idx])
                .pop()
                .expect("one read per row");
            match read {
                SessionRead::Listed(conv) => self.conversations[idx] = *conv,
                SessionRead::Failed(_) | SessionRead::NotCached => {
                    self.conversations[idx].custom_title = titled;
                }
                SessionRead::Missing | SessionRead::Empty => {
                    self.status_message = Some((
                        "Failed to rename: conversation became empty".to_string(),
                        self.clock().now(),
                    ));
                    return;
                }
            }
        }
        self.dialog_mode = DialogMode::None;
        self.status_message = Some(("Session renamed".to_string(), self.clock().now()));
        self.refresh_search_data();
        // The search results arrive later; they keep the renamed row selected.
        self.selection_anchor = Some(path.clone());
        self.update_filter();
        if let Some(new_selected) = self
            .filtered
            .iter()
            .position(|&i| self.conversations[i].path == path)
        {
            self.selected = Some(new_selected);
        }
    }

    /// Export the open session as the export menu's `option`; while its
    /// refresh is pending, once it lands, so the export holds the newest
    /// messages.
    pub(super) fn perform_export(&mut self, option: usize, to_clipboard: bool) {
        if self.defers_copy() {
            self.deferred_copy = Some(super::view_state::DeferredCopy::Export {
                option,
                to_clipboard,
            });
            return;
        }
        self.export_now(option, to_clipboard, CopyMissing::WaitForTheRefresh);
    }

    /// Export the open session as the export menu's `option` from the cache.
    /// When the session is missing from the cache, `missing` decides whether
    /// the export waits for the session's refresh or reports the failure.
    pub(super) fn export_now(&mut self, option: usize, to_clipboard: bool, missing: CopyMissing) {
        let (source, path, subagents, options) = match &self.app_mode {
            AppMode::View(state) => (
                state.conversation_source,
                state.conversation_path.clone(),
                state.subagents.clone(),
                crate::tui::export::ExportOptions {
                    show_tools: state.tool_display.is_visible(),
                    show_thinking: state.show_thinking,
                },
            ),
            _ => return,
        };

        let format = match crate::tui::export::ExportFormat::from_index(option) {
            Some(f) => f,
            None => return,
        };

        let mut next = crate::history::NextStep::Settled;
        let mut is_copy_missing = false;
        let read_entries = || {
            let read = crate::history::display_log_entries_interactively(
                &self.interactive_cache(),
                source,
                &path,
                &subagents,
            );
            next = read.next;
            read.value.ok_or_else(|| {
                is_copy_missing = true;
                std::io::Error::other("the session is not in the session cache yet")
            })
        };
        let status = if to_clipboard {
            match crate::tui::export::export_content(&path, format, options, read_entries) {
                Ok(content) => {
                    self.copy_to_clipboard("Conversation", &content);
                    None
                }
                Err(error) => Some(format!("Failed to read: {error}")),
            }
        } else {
            let exported_at = self.clock().local_wall();
            Some(
                crate::tui::export::export_to_file(
                    &path,
                    format,
                    options,
                    exported_at,
                    read_entries,
                )
                .message,
            )
        };
        self.act_on_next_step(&path, next);
        if is_copy_missing && missing == CopyMissing::WaitForTheRefresh {
            self.deferred_copy = Some(super::view_state::DeferredCopy::Export {
                option,
                to_clipboard,
            });
            return;
        }
        if let Some(status) = status {
            self.set_status(status);
        }
    }
}
