use crate::agent::sanitize::sanitize_agent_text;
use crate::history::format::trim_blank_lines;
use crate::history::skill_text::{skill_directory, skill_name};
use crate::log_entry::{ContentBlock, UserContent};
use crate::tui::{parse_command_name, parse_command_name_and_args};

/// The text a user message shows: each text block through
/// [`process_command_message`], joined by a blank line. `None` when no block
/// shows anything.
pub(crate) fn user_text(content: &UserContent) -> Option<String> {
    match content {
        UserContent::String(text) => process_command_message(text),
        UserContent::Blocks(blocks) => {
            let texts: Vec<String> = blocks
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::Text { text } => process_command_message(text),
                    _ => None,
                })
                .collect();
            (!texts.is_empty()).then(|| texts.join("\n\n"))
        }
    }
}

/// Process user message text to handle command-related XML tags.
/// Returns None if the message should be skipped entirely (e.g., empty local-command-stdout).
pub(crate) fn process_command_message(text: &str) -> Option<String> {
    let trimmed = text.trim();

    // Check for local-command-caveat - skip these system messages entirely
    if trimmed.starts_with("<local-command-caveat>") && trimmed.ends_with("</local-command-caveat>")
    {
        return None;
    }

    if let Some(output) = local_command_stdout(trimmed) {
        return Some(output).filter(|output| !output.is_empty());
    }

    // Check if this is a command message with <command-name> tag
    if let Some(command_name) = parse_command_name(trimmed) {
        // Skip /clear commands - internal context-clearing, not meaningful to display
        if command_name == "/clear" {
            return None;
        }

        return parse_command_name_and_args(trimmed);
    }

    if let Some(directory) = skill_directory(trimmed) {
        let name = skill_name(directory).unwrap_or("invoked");
        return Some(format!("*Skill: {name}*"));
    }

    Some(text.to_string())
}

/// The output of a slash command (`/compact`, `/add-dir`, …) that Claude Code
/// recorded wrapped in `<local-command-stdout>`, with the terminal styling it
/// wrote around that output removed and the blank lines at either end dropped.
/// `None` when `text` is not such a wrapper; empty when the command printed
/// nothing.
pub(crate) fn local_command_stdout(text: &str) -> Option<String> {
    let inner = text
        .strip_prefix("<local-command-stdout>")?
        .strip_suffix("</local-command-stdout>")?;
    Some(trim_blank_lines(&sanitize_agent_text(inner)).to_string())
}
