//! Each sub-agent's label, and the sub-agent each `spawn_agent` call started,
//! collected as the viewer loads a conversation.

use std::collections::HashMap;

use serde_json::Value;
use unicode_width::UnicodeWidthStr;

use crate::log_entry::{ContentBlock, LogEntry, SubagentIdentity, Tool, UserContent, UserMessage};

use super::NAME_WIDTH;
use super::ledger::fitted_name;
use super::style::subagent_label;
use super::tools::tool_result_display_text;

#[derive(Debug, Default)]
pub(super) struct SubagentRoster {
    /// The label of each sub-agent whose provider recorded a nickname, by the
    /// sub-agent's key (its `parent_tool_use_id`).
    labels: HashMap<String, String>,
    /// Each sub-agent's identity by the task path its `spawn_agent` call named.
    by_agent_path: HashMap<String, SubagentIdentity>,
    /// The agent type each agent call asked for, by call id; `None` for a
    /// call that names none.
    agent_calls: HashMap<String, Option<String>>,
}

impl SubagentRoster {
    pub(super) fn record_identity(&mut self, key: &str, identity: SubagentIdentity) {
        if let Some(nickname) = &identity.nickname {
            self.labels
                .entry(key.to_owned())
                .or_insert_with(|| nickname_label(nickname));
        }
        if let Some(agent_path) = &identity.agent_path {
            self.by_agent_path
                .entry(agent_path.clone())
                .or_insert(identity);
        }
    }

    /// Record the agent calls `entry` makes, each with the agent type its
    /// launch settings name.
    pub(super) fn record_agent_calls(&mut self, entry: &LogEntry) {
        let blocks: &[ContentBlock] = match entry {
            LogEntry::Assistant { message, .. } => &message.content,
            LogEntry::User {
                message:
                    UserMessage {
                        content: UserContent::Blocks(blocks),
                        ..
                    },
                ..
            } => blocks,
            _ => return,
        };
        for block in blocks {
            if let ContentBlock::ToolUse {
                id,
                tool: Tool::Agent,
                input,
                ..
            } = block
            {
                let agent_type = input
                    .get("launch")
                    .and_then(|launch| launch.get("agent_type"))
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                self.agent_calls.insert(id.clone(), agent_type);
            }
        }
    }

    /// The label a sub-agent's rows carry: its nickname when its provider
    /// recorded one, else the first characters of its key.
    pub(super) fn label(&self, key: &str) -> String {
        self.labels
            .get(key)
            .cloned()
            .unwrap_or_else(|| subagent_label(key))
    }

    /// The text a tool result shows. A result answering an agent call with
    /// nothing but the task path it started reads as the sub-agent it names:
    /// `Lorentz² (suite_runner) · /root/scout`.
    pub(super) fn result_text(&self, tool_use_id: &str, content: Option<&Value>) -> String {
        self.started_subagent(tool_use_id, content)
            .unwrap_or_else(|| tool_result_display_text(content))
    }

    fn started_subagent(&self, tool_use_id: &str, content: Option<&Value>) -> Option<String> {
        let call_agent_type = self.agent_calls.get(tool_use_id)?;
        let agent_path = lone_task_name(content?)?;
        let Some((identity, nickname)) = self
            .by_agent_path
            .get(&agent_path)
            .and_then(|identity| Some((identity, identity.nickname.as_deref()?)))
        else {
            return Some(agent_path);
        };
        let mut text = compact_ordinal(nickname);
        if let Some(agent_type) = identity.role.as_ref().or(call_agent_type.as_ref()) {
            text.push_str(&format!(" ({agent_type})"));
        }
        text.push_str(&format!(" · {agent_path}"));
        Some(text)
    }
}

/// The task path of a `spawn_agent` result that carries nothing else:
/// `{"task_name": "/root/scout"}`, as a JSON string or an object.
fn lone_task_name(content: &Value) -> Option<String> {
    let parsed;
    let object = match content {
        Value::String(text) => {
            parsed = serde_json::from_str::<Value>(text).ok()?;
            parsed.as_object()?
        }
        Value::Object(object) => object,
        _ => return None,
    };
    match object.iter().collect::<Vec<_>>().as_slice() {
        [(key, Value::String(path))] if key.as_str() == "task_name" => Some(path.clone()),
        _ => None,
    }
}

const SUPERSCRIPT_DIGITS: [char; 10] = ['⁰', '¹', '²', '³', '⁴', '⁵', '⁶', '⁷', '⁸', '⁹'];

/// A nickname with Codex's reuse ordinal (`Lorentz the 2nd`) written as
/// superscript digits (`Lorentz²`). A superscript cannot belong to the name
/// itself: Codex allows only ASCII letters, digits, spaces, `-` and `_`.
fn compact_ordinal(nickname: &str) -> String {
    let (name, ordinal) = split_ordinal(nickname);
    format!("{name}{ordinal}")
}

/// The name and its ordinal as superscript digits, or the whole nickname and
/// no ordinal when it ends in none.
fn split_ordinal(nickname: &str) -> (&str, String) {
    let ordinal = nickname.rsplit_once(" the ").and_then(|(name, rest)| {
        let digits = ["st", "nd", "rd", "th"]
            .iter()
            .find_map(|suffix| rest.strip_suffix(suffix))?;
        (!digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit())).then(|| {
            let superscript = digits
                .chars()
                .map(|digit| SUPERSCRIPT_DIGITS[digit as usize - '0' as usize])
                .collect::<String>();
            (name, superscript)
        })
    });
    ordinal.unwrap_or((nickname, String::new()))
}

/// `↳` and the nickname inside the name column. A name too wide is truncated
/// before its ordinal, so `Chandrasekhar` and `Chandrasekhar the 2nd` stay
/// apart as `↳Chandra…` and `↳Chandr…²`.
fn nickname_label(nickname: &str) -> String {
    let (name, ordinal) = split_ordinal(nickname);
    let name_width = NAME_WIDTH.saturating_sub(1 + ordinal.width());
    format!("↳{}{ordinal}", fitted_name(name, name_width))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_nickname_that_fits_labels_whole() {
        assert_eq!(nickname_label("Lorentz"), "↳Lorentz");
        assert_eq!(nickname_label("Lorentz the 2nd"), "↳Lorentz²");
    }

    #[test]
    fn a_long_nickname_is_truncated_before_its_ordinal() {
        assert_eq!(nickname_label("Chandrasekhar"), "↳Chandra…");
        assert_eq!(nickname_label("Chandrasekhar the 2nd"), "↳Chandr…²");
        assert_eq!(nickname_label("Lorentz the 12th"), "↳Loren…¹²");
    }

    #[test]
    fn the_inside_a_name_is_not_an_ordinal() {
        assert_eq!(
            split_ordinal("Ivan the Terrible"),
            ("Ivan the Terrible", String::new())
        );
        assert_eq!(
            split_ordinal("Ivan the Terrible the 3rd"),
            ("Ivan the Terrible", "³".to_owned())
        );
    }
}
