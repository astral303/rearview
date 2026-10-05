//! The `Ctrl+X` actions menu: the actions on the selected or open session,
//! and the menu's screen area, shared by drawing and click handling.

use ratatui::layout::Rect;

/// An action the menu offers, in the order it lists them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionAction {
    Resume,
    Fork,
    Delete,
    Rename,
}

impl SessionAction {
    pub const ALL: [SessionAction; 4] = [
        SessionAction::Resume,
        SessionAction::Fork,
        SessionAction::Delete,
        SessionAction::Rename,
    ];

    /// The label the menu shows. The ellipsis marks the two actions that ask
    /// for more before they act: Delete's confirmation and Rename's name box.
    pub fn label(self) -> &'static str {
        match self {
            SessionAction::Resume => "Resume",
            SessionAction::Fork => "Fork",
            SessionAction::Delete => "Delete…",
            SessionAction::Rename => "Rename…",
        }
    }

    /// The letter that picks the action, with or without `Ctrl`.
    pub fn key(self) -> char {
        match self {
            SessionAction::Resume => 'r',
            SessionAction::Fork => 'f',
            SessionAction::Delete => 'd',
            SessionAction::Rename => 'm',
        }
    }

    /// The key letter's index in the label. The menu draws that letter in the
    /// accent colour, as the status bar marks a key inside its word.
    pub fn key_position(self) -> usize {
        match self {
            SessionAction::Rename => 4,
            _ => 0,
        }
    }

    pub fn for_key(key: char) -> Option<SessionAction> {
        let key = key.to_ascii_lowercase();
        Self::ALL.into_iter().find(|action| action.key() == key)
    }
}

const WIDTH: u16 = 32;
/// The actions, a blank line and the `[Esc] Cancel` line, inside the border.
const HEIGHT: u16 = SessionAction::ALL.len() as u16 + 4;

/// The menu's rectangle, centred in `frame`.
pub fn area(frame: Rect) -> Rect {
    let width = WIDTH.min(frame.width);
    let height = HEIGHT.min(frame.height);
    Rect {
        x: frame.x + frame.width.saturating_sub(width) / 2,
        y: frame.y + frame.height.saturating_sub(height) / 2,
        width,
        height,
    }
}

/// The action on the row at `column`, `row`, or `None` outside the action
/// rows. On a terminal too short for the whole menu, the bottom border covers
/// the rows of the actions that do not fit.
pub fn action_at(frame: Rect, column: u16, row: u16) -> Option<SessionAction> {
    let menu = area(frame);
    let inside_columns = column > menu.x && column < menu.x + menu.width.saturating_sub(1);
    let first_row = menu.y + 1;
    let bottom_border = (menu.y + menu.height).saturating_sub(1);
    if !inside_columns || row < first_row || row >= bottom_border {
        return None;
    }
    SessionAction::ALL
        .get(usize::from(row - first_row))
        .copied()
}

/// True when `column`, `row` falls inside the menu's rectangle.
pub fn contains(frame: Rect, column: u16, row: u16) -> bool {
    let menu = area(frame);
    column >= menu.x && column < menu.x + menu.width && row >= menu.y && row < menu.y + menu.height
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_action_marks_its_key_letter_in_its_label() {
        for action in SessionAction::ALL {
            let marked = action.label().chars().nth(action.key_position()).unwrap();
            assert_eq!(marked.to_ascii_lowercase(), action.key(), "{action:?}");
        }
    }

    #[test]
    fn a_click_maps_to_the_action_on_its_row() {
        let frame = Rect::new(0, 0, 100, 40);
        let menu = area(frame);

        assert_eq!(
            action_at(frame, menu.x + 3, menu.y + 1),
            Some(SessionAction::Resume)
        );
        assert_eq!(
            action_at(frame, menu.x + 3, menu.y + 4),
            Some(SessionAction::Rename)
        );
        assert_eq!(action_at(frame, menu.x + 3, menu.y), None);
        assert_eq!(action_at(frame, menu.x + 3, menu.y + 5), None);
        assert_eq!(action_at(frame, menu.x, menu.y + 1), None);
    }

    #[test]
    fn a_click_on_a_cut_off_menus_bottom_border_runs_nothing() {
        let frame = Rect::new(0, 0, 100, 5);
        let menu = area(frame);

        assert_eq!(
            action_at(frame, menu.x + 3, menu.y + 3),
            Some(SessionAction::Delete)
        );
        assert_eq!(action_at(frame, menu.x + 3, menu.y + 4), None);
        assert_eq!(action_at(frame, menu.x + 3, menu.y + 5), None);
    }
}
