//! Label and style helpers shared across viewer entry/summary rendering.

use std::borrow::Cow;

use crate::claude;

/// Create a label for subagent entries from a parent_tool_use_id.
pub(super) fn subagent_label(parent_tool_use_id: &str) -> String {
    format!("↳{}", claude::short_parent_id(parent_tool_use_id))
}

/// Resolve the assistant-side label for the current entry.
pub(super) fn assistant_label<'a>(parent_id: Option<&str>, agent: Option<&'a str>) -> Cow<'a, str> {
    match parent_id {
        Some(p) => Cow::Owned(subagent_label(p)),
        None => Cow::Borrowed(agent.unwrap_or("Claude")),
    }
}
