//! The values a row reads from other entries of the conversation: each
//! sub-agent's label, the sub-agent each `spawn_agent` call started, and
//! the calls that message an agent, collected as the viewer loads a
//! conversation.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};

use serde_json::{Map, Value};
use unicode_width::UnicodeWidthStr;

use crate::history::RecordedIdentity;
use crate::log_entry::{ContentBlock, LogEntry, SubagentIdentity, Tool};

use super::NAME_WIDTH;
use super::ledger::fitted_name;
use super::style::subagent_label;
use super::tools::tool_result_display_text;

#[derive(Debug, Default, PartialEq)]
pub(crate) struct ConversationLinks {
    /// The label of each sub-agent whose provider recorded a nickname, by the
    /// sub-agent's key (its `parent_tool_use_id`).
    labels: HashMap<String, String>,
    /// Each sub-agent's identity by the task path its `spawn_agent` call named.
    by_agent_path: HashMap<String, SubagentIdentity>,
    /// The agent type each agent call asked for, by call id; `None` for a
    /// call that names none.
    agent_calls: HashMap<String, Option<String>>,
    /// The ids of the calls that message an agent.
    message_calls: HashSet<String>,
}

impl ConversationLinks {
    /// Links with a label for each sub-agent in `identities`, by its key.
    /// The first identity recorded for a key or a task path stays.
    pub(crate) fn from_identities<'a>(
        identities: impl IntoIterator<Item = (&'a str, &'a SubagentIdentity)>,
    ) -> Self {
        let mut links = Self::default();
        for (key, identity) in identities {
            links.record_identity(key, identity);
        }
        links
    }

    /// Links with each of `recorded`, in record order.
    pub(crate) fn from_recorded(recorded: &[RecordedIdentity]) -> Self {
        Self::from_identities(
            recorded
                .iter()
                .map(|recorded| (recorded.agent_id.as_str(), &recorded.identity)),
        )
    }

    pub(super) fn record_identity(&mut self, key: &str, identity: &SubagentIdentity) {
        if let Some(nickname) = &identity.nickname {
            self.labels
                .entry(key.to_owned())
                .or_insert_with(|| nickname_label(nickname));
        }
        if let Some(agent_path) = &identity.agent_path {
            self.by_agent_path
                .entry(agent_path.clone())
                .or_insert_with(|| identity.clone());
        }
    }

    /// Record the agent calls `entry` makes, each with the agent type its
    /// launch settings name, and the calls that message an agent.
    pub(super) fn record_agent_calls(&mut self, entry: &LogEntry) {
        for block in entry.content_blocks() {
            let ContentBlock::ToolUse {
                id, tool, input, ..
            } = block
            else {
                continue;
            };
            match tool {
                Tool::Agent => {
                    let agent_type = input
                        .get("launch")
                        .and_then(|launch| launch.get("agent_type"))
                        .and_then(Value::as_str)
                        .map(str::to_owned);
                    self.agent_calls.insert(id.clone(), agent_type);
                }
                Tool::AgentMessage => {
                    self.message_calls.insert(id.clone());
                }
                _ => {}
            }
        }
    }

    /// The label a sub-agent's rows carry: its nickname when its provider
    /// recorded one, else the first characters of its key.
    pub(crate) fn label(&self, key: &str) -> String {
        self.labels
            .get(key)
            .cloned()
            .unwrap_or_else(|| subagent_label(key))
    }

    /// The text a tool result shows. A result answering an agent call with
    /// nothing but the task path it started reads as the sub-agent it names:
    /// `Lorentz² (suite_runner) · /root/scout`. A result answering a launch
    /// without a task name reads as the nickname it carries, or the agent
    /// id when the nickname is missing.
    ///
    /// `None` for a result [`Self::hides_result`] hides.
    pub(super) fn result_text(&self, tool_use_id: &str, content: Option<&Value>) -> Option<String> {
        if self.hides_result(tool_use_id, content) {
            return None;
        }
        Some(
            self.started_subagent(tool_use_id, content)
                .unwrap_or_else(|| tool_result_display_text(content)),
        )
    }

    /// True for the empty result of a call that messages an agent: Codex
    /// returns one for every delivered message. The result doesn't show a
    /// row, and its call doesn't take a lane.
    pub(super) fn hides_result(&self, tool_use_id: &str, content: Option<&Value>) -> bool {
        self.message_calls.contains(tool_use_id)
            && tool_result_display_text(content).trim().is_empty()
    }

    fn started_subagent(&self, tool_use_id: &str, content: Option<&Value>) -> Option<String> {
        let call_agent_type = self.agent_calls.get(tool_use_id)?;
        let content = content?;
        if let Some(nickname_or_id) = started_agent_nickname_or_id(content) {
            return Some(nickname_or_id);
        }
        let agent_path = lone_task_name(content)?;
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

/// The values of a [`ConversationLinks`] that differ in newer links of the
/// conversation. It leaves out agent calls: a kept result's agent type
/// changes only when a later call reuses its call id.
pub(super) struct LinkChanges {
    /// True when a sub-agent the shown links label has another label, or
    /// none, in the newer links.
    relabelled: bool,
    /// The sub-agent keys only the newer links label.
    new_labels: HashSet<String>,
    agent_paths: HashSet<String>,
}

impl LinkChanges {
    pub(super) fn between(shown: &ConversationLinks, newer: &ConversationLinks) -> Self {
        Self {
            relabelled: shown
                .labels
                .iter()
                .any(|(key, label)| newer.labels.get(key) != Some(label)),
            new_labels: newer
                .labels
                .keys()
                .filter(|key| !shown.labels.contains_key(*key))
                .cloned()
                .collect(),
            agent_paths: Self::changed_keys(&shown.by_agent_path, &newer.by_agent_path),
        }
    }

    /// The keys of `old` and `new` whose values differ, a key missing from
    /// one included.
    fn changed_keys<V: PartialEq>(
        old: &HashMap<String, V>,
        new: &HashMap<String, V>,
    ) -> HashSet<String> {
        old.keys()
            .chain(new.keys())
            .filter(|key| old.get(*key) != new.get(*key))
            .cloned()
            .collect()
    }

    pub(super) fn relabels_shown_sub_agent(&self) -> bool {
        self.relabelled
    }

    /// True when `entry`'s rows read a new label or a changed task path: the
    /// label of the sub-agent the entry belongs to, or the task path a
    /// result carries.
    pub(super) fn reaches(&self, entry: &LogEntry) -> bool {
        let parent = match entry {
            LogEntry::Assistant {
                parent_tool_use_id, ..
            }
            | LogEntry::User {
                parent_tool_use_id, ..
            } => parent_tool_use_id,
            _ => return false,
        };
        parent
            .as_deref()
            .is_some_and(|key| self.new_labels.contains(key))
            || entry.content_blocks().iter().any(|block| match block {
                ContentBlock::ToolResult { content, .. } => content
                    .as_ref()
                    .and_then(lone_task_name)
                    .is_some_and(|path| self.agent_paths.contains(&path)),
                _ => false,
            })
    }

    pub(super) fn is_empty(&self) -> bool {
        self.new_labels.is_empty() && self.agent_paths.is_empty()
    }
}

/// The task path of a `spawn_agent` result that carries nothing else:
/// `{"task_name": "/root/scout"}`, as a JSON string or an object.
fn lone_task_name(content: &Value) -> Option<String> {
    let object = result_object(content)?;
    match object.iter().collect::<Vec<_>>().as_slice() {
        [(key, Value::String(path))] if key.as_str() == "task_name" => Some(path.clone()),
        _ => None,
    }
}

/// The nickname of the result of a launch without a task name,
/// `{"agent_id": "019f…", "nickname": "Hubble"}`, as a JSON string or an
/// object; its agent id when the nickname is missing or null. `None` for a
/// result that carries anything else.
fn started_agent_nickname_or_id(content: &Value) -> Option<String> {
    let object = result_object(content)?;
    let agent_id = object.get("agent_id")?.as_str()?;
    let nickname = match object.get("nickname") {
        None | Some(Value::Null) => None,
        Some(Value::String(nickname)) => Some(nickname.as_str()),
        Some(_) => return None,
    };
    let keys = 1 + usize::from(object.contains_key("nickname"));
    (object.len() == keys).then(|| nickname.map_or_else(|| agent_id.to_owned(), compact_ordinal))
}

/// A tool result's JSON object: the content itself, or the object a string
/// content contains.
fn result_object(content: &Value) -> Option<Cow<'_, Map<String, Value>>> {
    match content {
        Value::String(text) => match serde_json::from_str::<Value>(text).ok()? {
            Value::Object(object) => Some(Cow::Owned(object)),
            _ => None,
        },
        Value::Object(object) => Some(Cow::Borrowed(object)),
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
    fn nickname_that_fits_labels_whole() {
        assert_eq!(nickname_label("Lorentz"), "↳Lorentz");
        assert_eq!(nickname_label("Lorentz the 2nd"), "↳Lorentz²");
    }

    #[test]
    fn long_nickname_is_truncated_before_its_ordinal() {
        assert_eq!(nickname_label("Chandrasekhar"), "↳Chandra…");
        assert_eq!(nickname_label("Chandrasekhar the 2nd"), "↳Chandr…²");
        assert_eq!(nickname_label("Lorentz the 12th"), "↳Loren…¹²");
    }

    #[test]
    fn article_inside_name_is_not_ordinal() {
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
