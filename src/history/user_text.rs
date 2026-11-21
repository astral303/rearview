//! The text a user message shows. The viewer, exports, `agent read`,
//! `agent outline`, `agent within`, the list's search and `agent search` by
//! words read a user message through this rule. Semantic search reads a user
//! message through its own filter.

use crate::agent::sanitize::sanitize_agent_text;
use crate::history::format::trim_blank_lines;
use crate::history::skill_text::{skill_directory, skill_name};
use crate::log_entry::{ContentBlock, UserContent};
use std::borrow::Cow;

/// The text a user message shows: each text block through
/// [`user_block_text`], joined by a blank line. `None` when no block shows
/// anything.
pub(crate) fn user_text(content: &UserContent) -> Option<String> {
    match content {
        UserContent::String(text) => user_block_text(text).map(Cow::into_owned),
        UserContent::Blocks(blocks) => {
            let texts: Vec<Cow<'_, str>> = blocks
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::Text { text } => user_block_text(text),
                    _ => None,
                })
                .collect();
            (!texts.is_empty()).then(|| texts.join("\n\n"))
        }
    }
}

/// The text a user message's text block shows:
/// - nothing for the caveat Claude Code writes before a slash command's
///   records, and for `/clear`
/// - a slash command as `name args`
/// - a slash command's output or error without its tags and terminal
///   styling, or nothing when it printed nothing
/// - skill text as one `*Skill: name*` line
/// - any other text as written
pub(crate) fn user_block_text(text: &str) -> Option<Cow<'_, str>> {
    let trimmed = text.trim();
    if trimmed.starts_with("<local-command-caveat>") && trimmed.ends_with("</local-command-caveat>")
    {
        return None;
    }
    if let Some(output) = local_command_output(trimmed) {
        return (!output.is_empty()).then_some(Cow::Owned(output));
    }
    if let Some(command) = SlashCommand::of(trimmed) {
        return command.shown_line().map(Cow::Owned);
    }
    if let Some(directory) = skill_directory(trimmed) {
        let name = skill_name(directory).unwrap_or("invoked");
        return Some(Cow::Owned(format!("*Skill: {name}*")));
    }
    Some(Cow::Borrowed(text))
}

/// Claude Code opens a slash command's text with one of these tags. A prompt
/// that quotes a tag after other words is not a slash command.
const COMMAND_TAG_OPENERS: [&str; 2] = ["<command-name>", "<command-message>"];

/// A slash command the user ran, as Claude Code records it: text opening
/// with one of [`COMMAND_TAG_OPENERS`] that names the command in
/// `<command-name>`. The name shows as recorded, with or without a `/`.
pub(crate) struct SlashCommand<'a> {
    pub(crate) name: &'a str,
    /// Empty when the command took no arguments.
    args: &'a str,
}

impl<'a> SlashCommand<'a> {
    pub(crate) fn of(text: &'a str) -> Option<Self> {
        let trimmed = text.trim();
        if !COMMAND_TAG_OPENERS
            .iter()
            .any(|opener| trimmed.starts_with(opener))
        {
            return None;
        }
        Some(Self {
            name: tag_content(trimmed, "<command-name>", "</command-name>")?,
            args: tag_content(trimmed, "<command-args>", "</command-args>").map_or("", str::trim),
        })
    }

    /// The line the command shows, `name args`; `None` for `/clear`.
    pub(crate) fn shown_line(&self) -> Option<String> {
        if self.name == "/clear" {
            return None;
        }
        Some(if self.args.is_empty() {
            self.name.to_owned()
        } else {
            format!("{} {}", self.name, self.args)
        })
    }
}

/// The non-empty text between the first `open` and the first `close` in
/// `text`.
fn tag_content<'a>(text: &'a str, open: &str, close: &str) -> Option<&'a str> {
    let start = text.find(open)? + open.len();
    let end = text.find(close)?;
    (start < end).then(|| &text[start..end])
}

/// The output of a slash command (`/compact`, `/add-dir`, …) that Claude Code
/// recorded in `<local-command-stdout>`, or in `<local-command-stderr>` when
/// the command failed, without its terminal styling or the blank lines at
/// either end. `None` when `text` is not such a wrapper; empty when the
/// command printed nothing.
pub(crate) fn local_command_output(text: &str) -> Option<String> {
    let inner = [
        ("<local-command-stdout>", "</local-command-stdout>"),
        ("<local-command-stderr>", "</local-command-stderr>"),
    ]
    .into_iter()
    .find_map(|(open, close)| text.strip_prefix(open)?.strip_suffix(close))?;
    Some(trim_blank_lines(&sanitize_agent_text(inner)).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shown(text: &str) -> Option<String> {
        user_block_text(text).map(Cow::into_owned)
    }

    #[test]
    fn user_block_text_skips_local_command_caveat() {
        let caveat = "<local-command-caveat>Caveat: The messages below were generated by the user while running local commands. DO NOT respond to these messages or otherwise consider them in your response unless the user explicitly asks you to.</local-command-caveat>";
        assert_eq!(shown(caveat), None);
    }

    #[test]
    fn user_block_text_skips_local_command_caveat_with_whitespace() {
        let caveat = "  <local-command-caveat>Some caveat text</local-command-caveat>  ";
        assert_eq!(shown(caveat), None);
    }

    #[test]
    fn user_block_text_preserves_normal_text() {
        assert_eq!(shown("Hello world"), Some("Hello world".to_string()));
    }

    #[test]
    fn user_block_text_skips_empty_stdout() {
        assert_eq!(shown("<local-command-stdout></local-command-stdout>"), None);
        assert_eq!(
            shown("<local-command-stdout>   </local-command-stdout>"),
            None
        );
    }

    #[test]
    fn user_block_text_extracts_nonempty_stdout() {
        assert_eq!(
            shown("<local-command-stdout>output here</local-command-stdout>"),
            Some("output here".to_string())
        );
    }

    #[test]
    fn user_block_text_extracts_failed_commands_stderr() {
        assert_eq!(
            shown(
                "<local-command-stderr>Error during compaction: API Error: Connection lost mid-response.</local-command-stderr>"
            ),
            Some("Error during compaction: API Error: Connection lost mid-response.".to_string())
        );
    }

    #[test]
    fn user_block_text_removes_terminal_styling_from_stdout() {
        assert_eq!(
            shown(
                "<local-command-stdout>\u{1b}[2mCompacted (ctrl+o to see full summary)\u{1b}[22m</local-command-stdout>"
            ),
            Some("Compacted (ctrl+o to see full summary)".to_string())
        );
        assert_eq!(
            shown(
                "<local-command-stdout>\u{1b}[1mC:\\lane-1\u{1b}[22m is already added as a working directory.</local-command-stdout>"
            ),
            Some("C:\\lane-1 is already added as a working directory.".to_string())
        );
    }

    #[test]
    fn user_block_text_skips_stdout_that_is_only_terminal_styling() {
        assert_eq!(
            shown("<local-command-stdout>\u{1b}[2m\u{1b}[22m</local-command-stdout>"),
            None
        );
    }

    #[test]
    fn user_block_text_skips_clear_command() {
        assert_eq!(shown("<command-name>/clear</command-name>"), None);
        assert_eq!(
            shown(
                "<command-name>/clear</command-name>\n<command-message>clear</command-message>\n<command-args></command-args>"
            ),
            None
        );
    }

    #[test]
    fn user_block_text_extracts_other_command_names() {
        assert_eq!(
            shown("<command-name>/help</command-name>"),
            Some("/help".to_string())
        );
    }

    #[test]
    fn user_block_text_condenses_skill_invocation() {
        let skill_msg = "Base directory for this skill: /Users/raine/.claude/skills/consult\n\nConsult an external LLM with the user's query.\n\n**Arguments:** `how to add more aliases?`";
        assert_eq!(shown(skill_msg), Some("*Skill: consult*".to_string()));
        let windows_skill_msg =
            "Base directory for this skill: C:\\Users\\u\\.claude\\skills\\consult\n\n# Consult";
        assert_eq!(
            shown(windows_skill_msg),
            Some("*Skill: consult*".to_string())
        );
    }

    #[test]
    fn user_block_text_skill_invocation_fallback() {
        let skill_msg = "Base directory for this skill:";
        assert_eq!(shown(skill_msg), Some("*Skill: invoked*".to_string()));
    }

    #[test]
    fn slash_command_shows_its_name_then_its_arguments() {
        assert_eq!(
            shown(
                "<command-message>review</command-message>\n<command-name>/review</command-name>\n<command-args>  the parser  </command-args>"
            ),
            Some("/review the parser".to_owned())
        );
        assert_eq!(
            shown("<command-name>/model</command-name>\n<command-args></command-args>"),
            Some("/model".to_owned()),
            "empty arguments show the name alone"
        );
    }

    #[test]
    fn command_name_without_slash_shows_as_written() {
        assert_eq!(
            shown(
                "<command-message>init is analyzing</command-message>\n<command-name>init</command-name>"
            ),
            Some("init".to_owned())
        );
        assert_eq!(
            shown("<command-name>init</command-name>\n<command-args>the repo</command-args>"),
            Some("init the repo".to_owned())
        );
    }

    #[test]
    fn sentence_quoting_command_tag_shows_as_written() {
        let sentence = "why does <command-name>/model</command-name> show twice in the list?";
        assert_eq!(shown(sentence), Some(sentence.to_owned()));
        let quoted_clear = "is <command-name>/clear</command-name> hidden?";
        assert_eq!(shown(quoted_clear), Some(quoted_clear.to_owned()));
    }

    #[test]
    fn sentence_quoting_caveat_or_output_shows_as_written() {
        for sentence in [
            "the <local-command-caveat> record precedes each command",
            "read `<local-command-stdout>ok</local-command-stdout>` as output",
        ] {
            assert_eq!(shown(sentence), Some(sentence.to_owned()));
        }
    }

    #[test]
    fn user_text_joins_shown_blocks_and_skips_hidden_ones() {
        let content = UserContent::Blocks(vec![
            ContentBlock::Text {
                text: "<local-command-caveat>Caveat</local-command-caveat>".to_owned(),
            },
            ContentBlock::Text {
                text: "first".to_owned(),
            },
            ContentBlock::Text {
                text: "<local-command-stdout>second</local-command-stdout>".to_owned(),
            },
        ]);
        assert_eq!(user_text(&content), Some("first\n\nsecond".to_owned()));
        let hidden = UserContent::String("<command-name>/clear</command-name>".to_owned());
        assert_eq!(user_text(&hidden), None);
    }
}
