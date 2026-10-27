use super::session_store::SessionCopy;
use super::view_state::DeferredCopy;
use super::{Action, App, AppMode, DialogMode};
use crate::error::Result;
use crate::history::DisplayEntries;
use crate::tui::list_change::ListChange;
use crossterm::event::{KeyCode, KeyModifiers};
use std::path::Path;

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
                let path = self.get_selected_path()?;
                self.apply_finished_refreshes_before_editing(&path)?;
                Some(Action::Delete(path))
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
        let Some(idx) = self
            .get_selected_path()
            .and_then(|path| self.apply_finished_refreshes_before_editing(&path))
        else {
            self.dialog_mode = DialogMode::None;
            return;
        };
        let row = &self.conversations[idx];
        self.list_change = Some(ListChange::Rename {
            source: row.source,
            path: row.path.clone(),
            title,
            renamed_at: self.clock().wall(),
        });
    }

    /// Show the session at `path` renamed to `title`, or report why the
    /// rename failed and keep the name box open.
    pub(super) fn finish_rename(&mut self, path: &Path, title: &str, result: Result<()>) {
        match result {
            Ok(()) => {
                if let Some(idx) = self.conversations.iter().position(|row| row.path == path) {
                    self.show_renamed_session(idx, title);
                }
            }
            Err(e) => {
                self.status_message =
                    Some((format!("Failed to rename: {}", e), self.clock().now()));
            }
        }
    }

    /// Show row `idx` renamed to `title`, keeping it selected. The row shows
    /// `title` at once and a session refresh reads the session again.
    pub(super) fn show_renamed_session(&mut self, idx: usize, title: &str) {
        let path = self.conversations[idx].path.clone();
        self.conversations[idx].custom_title =
            Some(title.to_owned()).filter(|title| !title.is_empty());
        self.store.request_session_refresh(&path);
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

    /// Export the open session as the export menu's `option`. JSONL copies
    /// the transcript file at once: a refresh does not change the file. The
    /// other formats render the session's current copy; while its refresh
    /// is pending, or its copy is out of date or missing, they wait until
    /// the session's refresh is applied, so the export holds the newest
    /// messages.
    pub(super) fn perform_export(&mut self, option: usize, to_clipboard: bool) {
        if crate::tui::export::ExportFormat::from_index(option)
            .is_some_and(crate::tui::export::ExportFormat::copies_the_transcript)
        {
            self.export_now(option, to_clipboard, None);
            return;
        }
        let entries = if self.defers_copy() {
            None
        } else {
            self.current_export_entries()
        };
        match entries {
            Some(entries) => self.export_now(option, to_clipboard, Some(entries)),
            None => {
                self.deferred_copy = Some(DeferredCopy::Export {
                    option,
                    to_clipboard,
                });
            }
        }
    }

    /// The entries an export of the open session writes: the file opened
    /// directly outside every provider's session roots as the background
    /// worker last parsed it, or the session's copy in the cache when it is
    /// current. `None` for an out-of-date or missing copy, whose next step
    /// is recorded.
    pub(super) fn current_export_entries(&mut self) -> Option<DisplayEntries> {
        if let Some(entries) = &self.opened_file_entries {
            return Some(entries.clone());
        }
        let AppMode::View(state) = &self.app_mode else {
            return None;
        };
        let path = state.conversation_path.clone();
        let read = crate::history::display_log_entries_interactively(
            &self.interactive_cache(),
            state.conversation_source?,
            &path,
            &state.subagents,
        );
        match self.take_interactive(&path, read) {
            SessionCopy::Current(copy) => Some(copy.into_value()),
            SessionCopy::Older(_) | SessionCopy::Missing => None,
        }
    }

    /// Export the open session as the export menu's `option`, rendering
    /// `entries`, the session's; `None` for JSONL, which copies the
    /// transcript file.
    pub(super) fn export_now(
        &mut self,
        option: usize,
        to_clipboard: bool,
        entries: Option<DisplayEntries>,
    ) {
        let (path, options) = match &self.app_mode {
            AppMode::View(state) => (
                state.conversation_path.clone(),
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

        let read_entries =
            || entries.ok_or_else(|| std::io::Error::other("the session's entries were not read"));
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
        if let Some(status) = status {
            self.set_status(status);
        }
    }
}
