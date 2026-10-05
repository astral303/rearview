use super::{Action, App, DialogMode};
use crate::tui::actions_menu::{self, SessionAction};
use crossterm::event::{KeyCode, KeyModifiers};
use ratatui::layout::Rect;

impl App {
    /// True when `action` can act on the selected or open session. A file
    /// opened directly offers Rename alone, and only when its agent stores
    /// it: renaming writes to the file, and a copy elsewhere must stay
    /// unchanged.
    pub fn is_action_available(&self, action: SessionAction) -> bool {
        if self.single_file_mode {
            return action == SessionAction::Rename
                && self.is_opened_file_stored
                && self.get_selected_conversation_index().is_some();
        }
        match action {
            SessionAction::Rename => self.get_selected_conversation_index().is_some(),
            SessionAction::Resume | SessionAction::Fork | SessionAction::Delete => {
                self.get_selected_path().is_some()
            }
        }
    }

    pub fn is_actions_menu_open(&self) -> bool {
        matches!(self.dialog_mode, DialogMode::ActionsMenu { .. })
    }

    /// The keys that act on the session in the list and the viewer alike:
    /// `Ctrl+X` opens the actions menu and the rename key renames. The outer
    /// `None` means `code` is neither.
    pub(super) fn handle_session_key(
        &mut self,
        code: KeyCode,
        modifiers: KeyModifiers,
    ) -> Option<Option<Action>> {
        if modifiers.contains(KeyModifiers::CONTROL) && code == KeyCode::Char('x') {
            self.open_actions_menu();
            return Some(None);
        }
        if self.keys.rename.matches(code, modifiers) {
            if self.is_action_available(SessionAction::Rename) {
                self.start_rename();
            }
            return Some(None);
        }
        None
    }

    /// Open the menu with its first available action selected.
    pub(super) fn open_actions_menu(&mut self) {
        let selected = SessionAction::ALL
            .into_iter()
            .find(|action| self.is_action_available(*action));
        self.dialog_mode = DialogMode::ActionsMenu { selected };
    }

    pub(super) fn handle_actions_menu_key(
        &mut self,
        code: KeyCode,
        modifiers: KeyModifiers,
    ) -> Option<Action> {
        let DialogMode::ActionsMenu { selected } = self.dialog_mode else {
            return None;
        };
        match code {
            KeyCode::Esc => {
                self.dialog_mode = DialogMode::None;
                None
            }
            KeyCode::Up | KeyCode::Char('k') if !modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(from) = selected {
                    self.move_actions_selection(from, -1);
                }
                None
            }
            KeyCode::Down | KeyCode::Char('j') if !modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(from) = selected {
                    self.move_actions_selection(from, 1);
                }
                None
            }
            KeyCode::Enter => selected.and_then(|action| self.run_session_action(action)),
            KeyCode::Char(key) => SessionAction::for_key(key)
                .filter(|action| self.is_action_available(*action))
                .and_then(|action| self.run_session_action(action)),
            _ => None,
        }
    }

    /// A click on an action runs it; a click outside the menu closes it.
    pub fn handle_actions_menu_click(
        &mut self,
        column: u16,
        row: u16,
        frame_area: Rect,
    ) -> Option<Action> {
        if !self.is_actions_menu_open() {
            return None;
        }
        match actions_menu::action_at(frame_area, column, row) {
            Some(action) if self.is_action_available(action) => self.run_session_action(action),
            Some(_) => None,
            None => {
                if !actions_menu::contains(frame_area, column, row) {
                    self.dialog_mode = DialogMode::None;
                }
                None
            }
        }
    }

    /// Step to the next available action in `direction`, staying put at
    /// either end.
    fn move_actions_selection(&mut self, from: SessionAction, direction: isize) {
        let actions = SessionAction::ALL;
        let Some(start) = actions.iter().position(|action| *action == from) else {
            return;
        };
        let mut index = start as isize;
        loop {
            index += direction;
            let Some(action) = usize::try_from(index)
                .ok()
                .and_then(|index| actions.get(index))
            else {
                return;
            };
            if self.is_action_available(*action) {
                self.dialog_mode = DialogMode::ActionsMenu {
                    selected: Some(*action),
                };
                return;
            }
        }
    }

    fn run_session_action(&mut self, action: SessionAction) -> Option<Action> {
        self.dialog_mode = DialogMode::None;
        match action {
            SessionAction::Resume => self.get_selected_path().map(Action::Resume),
            SessionAction::Fork => self.get_selected_path().map(Action::ForkResume),
            SessionAction::Delete => {
                self.dialog_mode = DialogMode::ConfirmDelete;
                None
            }
            SessionAction::Rename => {
                self.start_rename();
                None
            }
        }
    }
}
