//! Skill text: the user message Claude Code records after loading a skill.
//! Its first line is `Base directory for this skill: <dir>`, and the skill's
//! file follows.

use std::collections::HashMap;

use crate::log_entry::{ContentBlock, LogEntry, UserContent};
use crate::tui::parse_command_name;

const SKILL_TEXT_PREFIX: &str = "Base directory for this skill:";

/// One transcript's entries without its repeated skill text: the `Skill`
/// call or slash command before that text already shows the load.
pub(crate) fn without_repeated_skill_text(
    entries: Vec<(usize, LogEntry)>,
) -> Vec<(usize, LogEntry)> {
    let mut repeated_skill_text = RepeatedSkillText::default();
    entries
        .into_iter()
        .filter(|(_, entry)| {
            let is_repeated = repeated_skill_text.is_repeated(entry);
            repeated_skill_text.record(entry);
            !is_repeated
        })
        .collect()
}

/// The directory skill text names, or `None` for any other text.
pub(crate) fn skill_directory(text: &str) -> Option<&str> {
    let first_line = text.trim().lines().next()?;
    first_line.strip_prefix(SKILL_TEXT_PREFIX).map(str::trim)
}

/// The last component of a skill's directory, with either path separator.
pub(crate) fn skill_name(directory: &str) -> Option<&str> {
    directory
        .rsplit(['/', '\\'])
        .find(|component| !component.is_empty())
}

/// The skill directory a user message names when its whole content is skill
/// text.
pub(crate) fn skill_text_directory(content: &UserContent) -> Option<&str> {
    match content {
        UserContent::String(text) => skill_directory(text),
        UserContent::Blocks(blocks) => match blocks.as_slice() {
            [ContentBlock::Text { text }] => skill_directory(text),
            _ => None,
        },
    }
}

/// The slash command a user message ran, as `/name`.
fn slash_command(content: &UserContent) -> Option<&str> {
    match content {
        UserContent::String(text) => parse_command_name(text),
        UserContent::Blocks(blocks) => blocks.iter().find_map(|block| match block {
            ContentBlock::Text { text } => parse_command_name(text),
            _ => None,
        }),
    }
}

/// True when `command` loads the skill named `skill`. A plugin skill's
/// command carries its plugin before a colon (`/plugin:skill`).
fn command_loads_skill(command: &str, skill: &str) -> bool {
    let command = command.trim_start_matches('/');
    command.rsplit(':').next() == Some(skill)
}

/// Recognizes repeated skill text: text answering a `Skill` call
/// (`sourceToolUseID`), or text after the thread's slash command for the
/// same skill.
#[derive(Default)]
struct RepeatedSkillText {
    /// The slash command of each thread's latest user message, by the
    /// thread's `parent_tool_use_id`; `None` for a message that ran none.
    latest_command: HashMap<Option<String>, Option<String>>,
}

impl RepeatedSkillText {
    /// True when `entry` is skill text answering a `Skill` call, or
    /// following the thread's slash command for the same skill.
    fn is_repeated(&self, entry: &LogEntry) -> bool {
        let LogEntry::User {
            message,
            parent_tool_use_id,
            source_tool_use_id,
            ..
        } = entry
        else {
            return false;
        };
        let Some(directory) = skill_text_directory(&message.content) else {
            return false;
        };
        if source_tool_use_id.is_some() {
            return true;
        }
        let latest_command = self
            .latest_command
            .get(parent_tool_use_id)
            .and_then(Option::as_deref);
        latest_command
            .zip(skill_name(directory))
            .is_some_and(|(command, skill)| command_loads_skill(command, skill))
    }

    /// Records `entry` as its thread's latest user message.
    fn record(&mut self, entry: &LogEntry) {
        if let LogEntry::User {
            message,
            parent_tool_use_id,
            ..
        } = entry
        {
            self.latest_command.insert(
                parent_tool_use_id.clone(),
                slash_command(&message.content).map(str::to_owned),
            );
        }
    }
}

/// Skill loads trimmed from real Claude Code transcripts, for the tests of
/// every reader. Each record keeps the fields the reader parses; paths, IDs
/// and skill bodies are neutral values of the same shape. The skill text's
/// timestamp is a millisecond before its call's result, as recorded.
#[cfg(test)]
pub(crate) mod test_support {
    /// A load through a `Skill` call: the call, its result, and the skill
    /// text answering the call.
    pub(crate) const SKILL_CALL_LOAD: [&str; 3] = [
        r#"{"type":"assistant","timestamp":"2026-10-03T12:00:01.000Z","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_01SKILLMAINAAAAAAAAAAAAA","name":"Skill","input":{"skill":"write-commit-messages"}}]}}"#,
        r#"{"type":"user","timestamp":"2026-10-03T12:00:02.001Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_01SKILLMAINAAAAAAAAAAAAA","content":"Launching skill: write-commit-messages"}]}}"#,
        r#"{"type":"user","timestamp":"2026-10-03T12:00:02.000Z","sourceToolUseID":"toolu_01SKILLMAINAAAAAAAAAAAAA","message":{"role":"user","content":[{"type":"text","text":"Base directory for this skill: C:\\Users\\example\\.claude\\skills\\write-commit-messages\n\n# Write Commit Messages\r\n\r\nExplain why the change was needed."}]}}"#,
    ];

    /// The same load inside a sub-agent transcript, after the session's.
    pub(crate) const SUB_AGENT_SKILL_CALL_LOAD: [&str; 3] = [
        r#"{"type":"assistant","timestamp":"2026-10-03T12:00:05.000Z","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_01SKILLSUBAAAAAAAAAAAAAA","name":"Skill","input":{"skill":"write-commit-messages"}}]}}"#,
        r#"{"type":"user","timestamp":"2026-10-03T12:00:06.001Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_01SKILLSUBAAAAAAAAAAAAAA","content":"Launching skill: write-commit-messages"}]}}"#,
        r#"{"type":"user","timestamp":"2026-10-03T12:00:06.000Z","sourceToolUseID":"toolu_01SKILLSUBAAAAAAAAAAAAAA","message":{"role":"user","content":[{"type":"text","text":"Base directory for this skill: C:\\Users\\example\\.claude\\skills\\write-commit-messages\n\n# Write Commit Messages\r\n\r\nExplain why the change was needed."}]}}"#,
    ];

    /// A load through a plugin skill's slash command: the command, then
    /// skill text without `sourceToolUseID`.
    pub(crate) const SLASH_COMMAND_LOAD: [&str; 2] = [
        r#"{"type":"user","timestamp":"2026-10-03T12:00:03.000Z","message":{"role":"user","content":"<command-message>frontend-design:frontend-design</command-message>\n<command-name>/frontend-design:frontend-design</command-name>"}}"#,
        r#"{"type":"user","timestamp":"2026-10-03T12:00:03.000Z","message":{"role":"user","content":[{"type":"text","text":"Base directory for this skill: C:\\Users\\example\\.claude\\plugins\\cache\\claude-plugins-official\\frontend-design\\0123456789ab\\skills\\frontend-design\n\n# Frontend Design\n\nDesign the page."}]}}"#,
    ];
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;

    fn kept_lines(records: &[&str]) -> Vec<usize> {
        let entries = records
            .iter()
            .map(|record| serde_json::from_str(record).unwrap())
            .enumerate()
            .collect();
        without_repeated_skill_text(entries)
            .into_iter()
            .map(|(line, _)| line)
            .collect()
    }

    #[test]
    fn skill_text_answering_a_skill_call_is_dropped() {
        assert_eq!(kept_lines(&SKILL_CALL_LOAD), [0, 1]);
    }

    #[test]
    fn skill_text_after_the_slash_command_for_its_skill_is_dropped() {
        assert_eq!(kept_lines(&SLASH_COMMAND_LOAD), [0]);
    }

    #[test]
    fn skill_text_after_a_slash_command_for_another_skill_is_kept() {
        let [command, skill_text] = SLASH_COMMAND_LOAD;
        let other_command = command.replace("frontend-design:frontend-design", "consult");
        assert_eq!(kept_lines(&[&other_command, skill_text]), [0, 1]);
    }

    #[test]
    fn skill_text_after_neither_a_skill_call_nor_a_slash_command_is_kept() {
        assert_eq!(kept_lines(&[SLASH_COMMAND_LOAD[1]]), [0]);
    }

    #[test]
    fn a_plugin_skills_command_loads_the_skill_after_its_colon() {
        assert!(command_loads_skill(
            "/frontend-design:frontend-design",
            "frontend-design"
        ));
        assert!(command_loads_skill("/rearview", "rearview"));
        assert!(!command_loads_skill("/rearview", "consult"));
    }
}
