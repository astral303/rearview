//! The rule that decides which sessions are listed: by the TUI's list, the
//! printouts and the agent commands.

use super::{Conversation, MessageTimes};
use crate::time_filter::TimeFilter;
use chrono::{DateTime, Local};

/// A session's origin, which decides whether the window applies to it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionOrigin {
    /// Named by its session ID.
    FoundById,
    /// Already listed: a listed session stays listed as it changes.
    AlreadyListed,
    /// Found by discovery.
    Discovered,
}

/// The times a session is judged by.
#[derive(Clone, Copy, Debug)]
pub enum SessionTimes<'a> {
    /// Its row's messages.
    Messages(&'a MessageTimes),
    /// Without a row: its transcript's last write, `None` when unknown.
    LastWrite(Option<DateTime<Local>>),
}

/// True when a session of `origin` with `times` is listed under `window`. A
/// session found by ID or already listed is listed whatever its times; a
/// discovered one needs a message inside `window`. A discovered session
/// without a row is listed unless its transcript was last written before
/// `window` opened.
pub fn is_listed(origin: SessionOrigin, times: SessionTimes<'_>, window: &TimeFilter) -> bool {
    match origin {
        SessionOrigin::FoundById | SessionOrigin::AlreadyListed => true,
        SessionOrigin::Discovered => match times {
            SessionTimes::Messages(times) => times.has_message_within(window),
            SessionTimes::LastWrite(last_write) => {
                last_write.is_none_or(|last_write| window.may_hold_a_message_inside(last_write))
            }
        },
    }
}

/// Keep the rows of `rows`, each found by discovery, that `window` lists.
pub fn retain_listed(rows: &mut Vec<Conversation>, window: &TimeFilter) {
    rows.retain(|row| {
        is_listed(
            SessionOrigin::Discovered,
            SessionTimes::Messages(&row.message_times),
            window,
        )
    });
}
