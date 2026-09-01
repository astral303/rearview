//! Label and style helpers shared across viewer entry/summary rendering.

use std::borrow::Cow;

use crate::log_entry;

/// The label on everything the user authored: their messages, and the
/// commands they ran through the agent.
pub(super) const USER_LABEL: &str = "You";

/// Create a label for subagent entries from a parent_tool_use_id.
pub(super) fn subagent_label(parent_tool_use_id: &str) -> String {
    format!("↳{}", log_entry::short_parent_id(parent_tool_use_id))
}

/// Resolve the assistant-side label for the current entry.
pub(super) fn assistant_label<'a>(parent_id: Option<&str>, agent: Option<&'a str>) -> Cow<'a, str> {
    match parent_id {
        Some(p) => Cow::Owned(subagent_label(p)),
        None => Cow::Borrowed(agent.unwrap_or("Claude")),
    }
}
