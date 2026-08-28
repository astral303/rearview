use crate::agent::sanitize::sanitize_agent_text;
use crate::tui::{parse_command_name, parse_command_name_and_args};

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

    // Skill invocation expanded prompts - show description instead of full prompt
    if trimmed.starts_with("Base directory for this skill:") {
        let description = trimmed
            .lines()
            .skip(1)
            .find(|l| !l.trim().is_empty())
            .unwrap_or("invoked");
        return Some(format!("*Skill: {}*", description));
    }

    Some(text.to_string())
}

/// The output of a slash command (`/compact`, `/add-dir`, …) that Claude Code
/// recorded wrapped in `<local-command-stdout>`, trimmed and with the terminal
/// styling it wrote around that output removed. `None` when `text` is not
/// such a wrapper; empty when the command printed nothing.
pub(crate) fn local_command_stdout(text: &str) -> Option<String> {
    let inner = text
        .strip_prefix("<local-command-stdout>")?
        .strip_suffix("</local-command-stdout>")?;
    Some(sanitize_agent_text(inner).trim().to_string())
}
