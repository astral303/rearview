use super::{Action, App, DialogMode, ForkRead};
use crate::history::Source;
use crate::tui::actions_menu::{self, ActionAvailability, MenuAvailability, SessionAction};
use crossterm::event::{KeyCode, KeyModifiers};
use ratatui::layout::Rect;
use std::sync::mpsc;

/// This rearview's work on the list that rename and delete wait for.
#[derive(Clone, Copy)]
enum ListWork {
    Loading,
    Refreshing,
    RefreshingSession,
}

impl ListWork {
    /// The reason the actions menu shows beside Delete and Rename.
    fn menu_reason(self) -> &'static str {
        match self {
            Self::Loading => "list loading",
            Self::Refreshing => "list refreshing",
            Self::RefreshingSession => "refreshing",
        }
    }

    /// The status the rename key shows.
    fn rename_status(self) -> &'static str {
        match self {
            Self::Loading => "Rename is unavailable while the list loads",
            Self::Refreshing => "Rename is unavailable while the list refreshes",
            Self::RefreshingSession => "Rename waits for this session's refresh",
        }
    }
}

impl App {
    /// True when `action` is available on the selected or open session.
    pub fn is_action_available(&self, action: SessionAction) -> bool {
        self.action_availability(action) == ActionAvailability::Available
    }

    /// The availability of `action` as the open menu read it on opening, or
    /// read now while no menu is open.
    pub fn action_availability(&self, action: SessionAction) -> ActionAvailability {
        match &self.dialog_mode {
            DialogMode::ActionsMenu { availability, .. } => availability.of(action),
            _ => self.read_availability(action),
        }
    }

    /// The availability of `action` on the selected or open session, without
    /// waiting for a `fork` declaration that has yet to be read. A file opened
    /// directly offers Rename alone, and only when its agent stores it:
    /// renaming writes to the file, and a copy elsewhere must stay unchanged.
    /// Rename and delete wait for this rearview's work on the list (see
    /// [`Self::rename_and_delete_wait`]).
    fn read_availability(&self, action: SessionAction) -> ActionAvailability {
        if matches!(action, SessionAction::Delete | SessionAction::Rename)
            && let Some(work) = self.rename_and_delete_wait()
        {
            return ActionAvailability::Waiting(work.menu_reason());
        }
        let Some(source) = self.get_selected_source() else {
            return ActionAvailability::Unavailable;
        };
        if self.single_file_mode
            && (action != SessionAction::Rename || self.store.refuses_session_refreshes())
        {
            return ActionAvailability::Unavailable;
        }
        let provider = source.provider();
        let is_capable = match action {
            SessionAction::Resume => provider.resume().is_capable(),
            SessionAction::Fork => match (self.fork_if_known)(source) {
                Some(is_capable) => is_capable,
                None => return ActionAvailability::Loading,
            },
            SessionAction::Delete => provider.delete().is_capable(),
            SessionAction::Rename => provider.rename().is_capable(),
        };
        if is_capable {
            ActionAvailability::Available
        } else {
            ActionAvailability::Unavailable
        }
    }

    /// This rearview's work on the list that rename and delete of the
    /// selected or open session wait for: the launch, a list refresh, or the
    /// running refresh of that session, while it reads or writes the cache.
    /// `None` while that work waits for another rearview. A finished
    /// refresh is applied before they act.
    fn rename_and_delete_wait(&self) -> Option<ListWork> {
        if self.cache_wait.is_some() {
            return None;
        }
        if self.is_loading() {
            return Some(ListWork::Loading);
        }
        if self.store.is_list_refresh_running() {
            return Some(ListWork::Refreshing);
        }
        let path = self.get_selected_path()?;
        self.store
            .is_in_running_session_refresh(&path)
            .then_some(ListWork::RefreshingSession)
    }

    /// True while the actions menu waits for a `fork` declaration read on a
    /// background thread.
    pub fn is_reading_fork(&self) -> bool {
        self.fork_read.is_some()
    }

    /// Show the `fork` declaration read on a background thread in the open
    /// menu, once it arrives. A read that ended without an answer greys Fork
    /// out.
    pub fn receive_fork_read(&mut self) {
        let Some(read) = self.fork_read.as_ref() else {
            return;
        };
        let is_capable = match read.is_capable.try_recv() {
            Ok(is_capable) => is_capable,
            Err(mpsc::TryRecvError::Empty) => return,
            Err(mpsc::TryRecvError::Disconnected) => false,
        };
        let source = read.source;
        self.fork_read = None;
        if self.get_selected_source() != Some(source) {
            return;
        }
        if let DialogMode::ActionsMenu {
            selected,
            availability,
        } = &mut self.dialog_mode
        {
            availability.set(
                SessionAction::Fork,
                if is_capable {
                    ActionAvailability::Available
                } else {
                    ActionAvailability::Unavailable
                },
            );
            if selected.is_none() {
                *selected = availability.first_available();
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
            } else if let Some(work) = self.rename_and_delete_wait() {
                self.set_status(work.rename_status().to_owned());
            }
            return Some(None);
        }
        None
    }

    /// Open the menu with its first available action selected. Draws, keys
    /// and clicks read the availability it reads here. A `fork` declaration
    /// that has yet to be read is read on a background thread, so the menu
    /// opens without waiting.
    pub(super) fn open_actions_menu(&mut self) {
        let availability = MenuAvailability::read(|action| self.read_availability(action));
        if availability.of(SessionAction::Fork) == ActionAvailability::Loading
            && let Some(source) = self.get_selected_source()
        {
            self.start_fork_read(source);
        }
        self.dialog_mode = DialogMode::ActionsMenu {
            selected: availability.first_available(),
            availability,
        };
    }

    /// Read `source`'s `fork` declaration on a background thread, unless a
    /// read of it is running already.
    fn start_fork_read(&mut self, source: Source) {
        if self
            .fork_read
            .as_ref()
            .is_some_and(|read| read.source == source)
        {
            return;
        }
        let (send, is_capable) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = send.send(source.provider().fork().is_capable());
        });
        self.fork_read = Some(ForkRead { source, is_capable });
    }

    pub(super) fn handle_actions_menu_key(
        &mut self,
        code: KeyCode,
        modifiers: KeyModifiers,
    ) -> Option<Action> {
        let DialogMode::ActionsMenu { selected, .. } = self.dialog_mode else {
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
                if let DialogMode::ActionsMenu { selected, .. } = &mut self.dialog_mode {
                    *selected = Some(*action);
                }
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
