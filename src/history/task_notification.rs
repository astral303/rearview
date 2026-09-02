//! The report of a finished background task. Claude Code records it as a user
//! message wrapping a `<task-notification>`: an agent's report with its usage
//! figures, or a background command's one-line summary.

use crate::log_entry::{ContentBlock, UserContent};

/// The ledger label on a task report, for an agent and a background command
/// alike: the summary names which kind it is.
pub const TASK_LABEL: &str = "Task";

const OPEN_TAG: &str = "<task-notification>";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskReport {
    /// The `<summary>` verbatim: `Agent "…" finished`, `Background command
    /// "…" completed (exit code 0)`.
    pub summary: String,
    /// The `<usage>` figures, which an agent's report carries and a
    /// background command's does not.
    pub usage: Option<TaskUsage>,
    /// The `<result>`: the agent's report as Markdown.
    pub body: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TaskUsage {
    pub tokens: u64,
    pub tool_calls: u64,
    pub duration_ms: u64,
}

impl TaskReport {
    /// The summary, the usage line, then the body: the text `--render`,
    /// export and a sub-agent's dimmed report print.
    pub fn display_text(&self) -> String {
        let mut text = self.summary.clone();
        if let Some(usage) = &self.usage {
            text.push('\n');
            text.push_str(&usage.line());
        }
        if let Some(body) = &self.body {
            text.push_str("\n\n");
            text.push_str(body);
        }
        text
    }

    /// The summary and the body, without the usage line: the text search
    /// indexes and the agent CLI reads.
    pub fn search_text(&self) -> String {
        match &self.body {
            Some(body) => format!("{}\n\n{body}", self.summary),
            None => self.summary.clone(),
        }
    }
}

impl TaskUsage {
    /// `107k tokens · 32 tool calls · 3m 5s`, in the units the session
    /// header uses.
    pub fn line(&self) -> String {
        let tool_calls = match self.tool_calls {
            1 => "1 tool call".to_owned(),
            count => format!("{count} tool calls"),
        };
        format!(
            "{} tokens · {tool_calls} · {}",
            crate::tui::format_tokens(self.tokens),
            format_duration(self.duration_ms)
        )
    }
}

/// `1h 4m`, `3m 5s` or `12s`: the two largest units that are not both zero.
fn format_duration(duration_ms: u64) -> String {
    let seconds = duration_ms / 1_000;
    let (hours, minutes, seconds) = (seconds / 3_600, seconds / 60 % 60, seconds % 60);
    if hours > 0 {
        format!("{hours}h {minutes}m")
    } else if minutes > 0 {
        format!("{minutes}m {seconds}s")
    } else {
        format!("{seconds}s")
    }
}

/// The task report of a user message, from the first text block that holds
/// one.
pub fn user_task_report(content: &UserContent) -> Option<TaskReport> {
    match content {
        UserContent::String(text) => parse_task_report(text),
        UserContent::Blocks(blocks) => blocks.iter().find_map(|block| match block {
            ContentBlock::Text { text } => parse_task_report(text),
            _ => None,
        }),
    }
}

/// The report a `<task-notification>` wraps, or `None` when `text` does not
/// open with one or names no summary. A field whose closing tag is missing,
/// as in a transcript written mid-line, is treated as absent; every field
/// other than the summary, the result and the usage is ignored.
pub fn parse_task_report(text: &str) -> Option<TaskReport> {
    let fields = text.trim().strip_prefix(OPEN_TAG)?;
    let summary = tagged_text(fields, "summary")?.trim().to_owned();
    let body = result_text(fields)
        .map(str::trim)
        .filter(|body| !body.is_empty())
        .map(str::to_owned);
    let usage = tagged_text(fields, "usage").and_then(|usage| {
        Some(TaskUsage {
            tokens: tagged_number(usage, "subagent_tokens")?,
            tool_calls: tagged_number(usage, "tool_uses")?,
            duration_ms: tagged_number(usage, "duration_ms")?,
        })
    });
    Some(TaskReport {
        summary,
        usage,
        body,
    })
}

/// The text between `<tag>` and the first `</tag>` after it.
fn tagged_text<'a>(text: &'a str, tag: &str) -> Option<&'a str> {
    let (rest, close) = after_open_tag(text, tag)?;
    Some(&rest[..rest.find(&close)?])
}

/// The text between `<result>` and the last `</result>`: a body may quote
/// its own closing tag, and nothing follows the result but the usage and the
/// notification's end.
fn result_text(text: &str) -> Option<&str> {
    let (rest, close) = after_open_tag(text, "result")?;
    Some(&rest[..rest.rfind(&close)?])
}

fn after_open_tag<'a>(text: &'a str, tag: &str) -> Option<(&'a str, String)> {
    let open = format!("<{tag}>");
    let start = text.find(&open)? + open.len();
    Some((&text[start..], format!("</{tag}>")))
}

fn tagged_number(text: &str, tag: &str) -> Option<u64> {
    tagged_text(text, tag)?.trim().parse().ok()
}

/// Two notifications trimmed from the corpus, for the tests of every surface
/// a task report reaches.
#[cfg(test)]
pub(crate) mod test_support {
    /// An agent's report: eight lines, so the truncated body omits its end.
    pub(crate) const AGENT_REPORT: &str = "<task-notification>\n<task-id>adf46fa9b7c515f5b</task-id>\n<tool-use-id>toolu_01K7Pa6D8S5xq2t3NKwsRX9q</tool-use-id>\n<output-file>C:\\Temp\\tasks\\adf46fa9b7c515f5b.output</output-file>\n<status>completed</status>\n<summary>Agent \"Verify session-action claims\" finished</summary>\n<note>A task-notification fires each time this agent stops with no live background children of its own. The user can send it another message and resume it, so the same task-id may notify more than once.</note>\n<result>Verified against source. Verdicts:\n\n1. **VERIFIED** — Resume passes the recorded cwd as `current_dir`.\n2. **VERIFIED** — The list opens in lexical search.\n3. **PARTLY** — `t` cycles the three tool modes.\n4. **REFUTED** — Nothing reads the output file.\n\nNothing else to report.</result>\n<usage><subagent_tokens>107559</subagent_tokens><tool_uses>32</tool_uses><duration_ms>185278</duration_ms></usage>\n</task-notification>";

    pub(crate) const AGENT_SUMMARY: &str = "Agent \"Verify session-action claims\" finished";
    pub(crate) const AGENT_USAGE_LINE: &str = "107k tokens · 32 tool calls · 3m 5s";
    /// The body's last line, past the truncated body's four.
    pub(crate) const AGENT_REPORT_LAST_LINE: &str = "Nothing else to report.";

    pub(crate) const BACKGROUND_COMMAND: &str = "<task-notification>\n<task-id>bi4ksxex0</task-id>\n<tool-use-id>toolu_016sRjyKoA2xzXyQquth9v1K</tool-use-id>\n<output-file>C:\\Temp\\tasks\\bi4ksxex0.output</output-file>\n<status>completed</status>\n<summary>Background command \"Run full check suite\" completed (exit code 0)</summary>\n</task-notification>";

    pub(crate) const BACKGROUND_COMMAND_SUMMARY: &str =
        "Background command \"Run full check suite\" completed (exit code 0)";
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;

    fn agent_usage() -> TaskUsage {
        TaskUsage {
            tokens: 107_559,
            tool_calls: 32,
            duration_ms: 185_278,
        }
    }

    #[test]
    fn an_agent_report_yields_its_summary_usage_and_body() {
        let report = parse_task_report(AGENT_REPORT).unwrap();

        assert_eq!(report.summary, AGENT_SUMMARY);
        assert_eq!(report.usage, Some(agent_usage()));
        let body = report.body.as_deref().unwrap();
        assert!(body.starts_with("Verified against source. Verdicts:\n\n1. **VERIFIED**"));
        assert!(body.ends_with(AGENT_REPORT_LAST_LINE));
    }

    #[test]
    fn a_background_command_yields_its_summary_alone() {
        let report = parse_task_report(BACKGROUND_COMMAND).unwrap();

        assert_eq!(
            report,
            TaskReport {
                summary: BACKGROUND_COMMAND_SUMMARY.to_owned(),
                usage: None,
                body: None,
            }
        );
    }

    /// The corpus holds notifications without a tool-use id, without a note,
    /// without usage figures, and with a worktree; each reads the same.
    #[test]
    fn every_field_combination_the_corpus_holds_reads_the_same_fields() {
        let without_tool_use_id = AGENT_REPORT.replace(
            "<tool-use-id>toolu_01K7Pa6D8S5xq2t3NKwsRX9q</tool-use-id>\n",
            "",
        );
        let without_note = AGENT_REPORT.replace(
            "<note>A task-notification fires each time this agent stops with no live background children of its own. The user can send it another message and resume it, so the same task-id may notify more than once.</note>\n",
            "",
        );
        let with_worktree = AGENT_REPORT.replace(
            "</usage>\n",
            "</usage>\n<worktree>C:\\work\\lane-1</worktree>\n",
        );
        let expected = parse_task_report(AGENT_REPORT).unwrap();
        for text in [without_tool_use_id, without_note, with_worktree] {
            assert_eq!(parse_task_report(&text).unwrap(), expected, "{text}");
        }

        let without_usage = AGENT_REPORT.replace(
            "<usage><subagent_tokens>107559</subagent_tokens><tool_uses>32</tool_uses><duration_ms>185278</duration_ms></usage>\n",
            "",
        );
        let report = parse_task_report(&without_usage).unwrap();
        assert_eq!(report.usage, None);
        assert_eq!(report.body, expected.body);
    }

    #[test]
    fn a_notification_ending_before_its_closing_tags_yields_the_fields_that_closed() {
        let ending_inside_usage = &AGENT_REPORT
            [..AGENT_REPORT.find("<usage>").unwrap() + "<usage><subagent_tokens>1075".len()];

        let report = parse_task_report(ending_inside_usage).unwrap();

        assert_eq!(report.summary, AGENT_SUMMARY);
        assert!(report.body.is_some());
        assert_eq!(report.usage, None);

        let ending_inside_result = &AGENT_REPORT[..AGENT_REPORT.find("Verdicts").unwrap()];
        let report = parse_task_report(ending_inside_result).unwrap();
        assert_eq!(report.body, None);
    }

    #[test]
    fn a_message_that_mentions_the_notification_is_not_a_report() {
        assert_eq!(
            parse_task_report("Why does rearview print <task-notification> under You?"),
            None
        );
        assert_eq!(
            parse_task_report("<task-notification>\n<task-id>x</task-id>\n</task-notification>"),
            None,
            "a notification without a summary has no row to render"
        );
    }

    /// The result runs to its last closing tag; the summary stops at its
    /// first, so a body quoting `</summary>` does not extend the summary.
    #[test]
    fn a_body_quoting_the_closing_tags_reads_whole_and_leaves_the_summary_intact() {
        let text = AGENT_REPORT.replace(
            "Verdicts:",
            "Verdicts (the notification closes the summary with `</summary>` and the body with `</result>`):",
        );

        let report = parse_task_report(&text).unwrap();

        assert_eq!(report.summary, AGENT_SUMMARY);
        let body = report.body.as_deref().unwrap();
        assert!(body.contains("`</summary>`"), "{body}");
        assert!(body.contains("`</result>`"), "{body}");
        assert!(body.ends_with(AGENT_REPORT_LAST_LINE), "{body}");
        assert_eq!(report.usage, Some(agent_usage()));
    }

    #[test]
    fn the_usage_line_uses_the_header_units() {
        assert_eq!(agent_usage().line(), "107k tokens · 32 tool calls · 3m 5s");
        assert_eq!(
            TaskUsage {
                tokens: 950,
                tool_calls: 1,
                duration_ms: 12_400
            }
            .line(),
            "950 tokens · 1 tool call · 12s"
        );
        assert_eq!(format_duration(3_840_000), "1h 4m");
        assert_eq!(format_duration(0), "0s");
    }

    #[test]
    fn display_text_carries_the_usage_line_and_search_text_does_not() {
        let report = parse_task_report(AGENT_REPORT).unwrap();

        let display = report.display_text();
        assert!(
            display.starts_with(&format!("{AGENT_SUMMARY}\n{AGENT_USAGE_LINE}\n\nVerified")),
            "{display}"
        );
        assert_eq!(
            report.search_text(),
            format!("{}\n\n{}", report.summary, report.body.as_deref().unwrap())
        );
        let command = parse_task_report(BACKGROUND_COMMAND).unwrap();
        assert_eq!(command.display_text(), command.summary);
        assert_eq!(command.search_text(), command.summary);
    }

    #[test]
    fn a_user_message_holds_a_report_in_either_content_shape() {
        let as_string = UserContent::String(BACKGROUND_COMMAND.to_owned());
        let as_blocks = UserContent::Blocks(vec![ContentBlock::Text {
            text: BACKGROUND_COMMAND.to_owned(),
        }]);
        let plain = UserContent::String("hello".to_owned());

        assert!(user_task_report(&as_string).is_some());
        assert!(user_task_report(&as_blocks).is_some());
        assert!(user_task_report(&plain).is_none());
    }
}
