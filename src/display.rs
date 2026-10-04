use crate::cli::DebugLevel;
use crate::debug;
use crate::debug_log;
use crate::error::Result;
use crate::history::{
    Conversation, DisplayEntries, MalformedLine, display_log_entries, sniffed_display_log_entries,
};
use crate::pager;
use crate::tui::viewer::{
    ParsedConversation, RenderedLine, parse_unattributed_conversation_file, parsed_conversation,
    render_parsed_conversation,
};
use crate::tui::{ExportOptions, RenderOptions, ToolDisplayMode, plain_text};
use colored::{Colorize, CustomColor};
use crossterm::terminal;
use std::io::{self, Write};
use std::path::Path;

/// Configuration options for displaying conversations
#[derive(Debug, Clone, Default)]
pub struct DisplayOptions {
    /// Hide tool calls and results
    pub no_tools: bool,
    /// Show thinking/reasoning blocks
    pub show_thinking: bool,
    /// Debug level for error logging
    pub debug_level: Option<DebugLevel>,
    /// Use a pager for output (less/more)
    pub use_pager: bool,
    /// Disable colored output
    pub no_color: bool,
}

const NAME_WIDTH: usize = 9;
const SEPARATOR_WIDTH: usize = 3; // Display width of " │ "

/// The ledger's content width on this terminal: its width, defaulting to 80
/// when unavailable, less the name column and separator.
fn terminal_content_width() -> usize {
    let terminal_width = terminal::size().map(|(w, _)| w as usize).unwrap_or(80);
    terminal_width.saturating_sub(NAME_WIDTH + SEPARATOR_WIDTH)
}

pub(crate) enum DisplayFormat {
    Ledger { content_width: usize },
    Plain,
}

/// The session the terminal printout shows for `selected_path`, read through
/// its row's agent with the row's sub-agent transcripts. A path no row holds
/// is read by whichever agent's format recognizes it.
pub fn read_session_to_print(
    conversations: &[Conversation],
    selected_path: &Path,
) -> Result<DisplayEntries> {
    match conversations.iter().find(|row| row.path == selected_path) {
        Some(row) => display_log_entries(row.source, &row.path, &row.subagents),
        None => sniffed_display_log_entries(selected_path),
    }
}

/// Print `session`, read from `file_path`, as the viewer's ledger.
pub fn display_conversation(
    file_path: &Path,
    session: DisplayEntries,
    options: &DisplayOptions,
) -> Result<()> {
    let content_width = terminal_content_width();
    print_session(
        file_path,
        session,
        options,
        DisplayFormat::Ledger { content_width },
    )
}

/// Print `session`, read from `file_path`, as the Plain export's text.
pub fn display_conversation_plain(
    file_path: &Path,
    session: DisplayEntries,
    options: &DisplayOptions,
) -> Result<()> {
    print_session(file_path, session, options, DisplayFormat::Plain)
}

fn print_session(
    file_path: &Path,
    session: DisplayEntries,
    options: &DisplayOptions,
    format: DisplayFormat,
) -> Result<()> {
    report_malformed_lines(file_path, &session.malformed_lines, options.debug_level);
    let printed = Printout::of(session, options, format);
    write_through_pager(options.use_pager, |writer| {
        printed.write(writer, options.no_color);
    });
    Ok(())
}

/// The terminal printout: the viewer's ledger rows, or the Plain export's text.
enum Printout {
    Ledger(Vec<RenderedLine>),
    Plain(String),
}

impl Printout {
    fn of(session: DisplayEntries, options: &DisplayOptions, format: DisplayFormat) -> Self {
        match format {
            DisplayFormat::Ledger { content_width } => Self::Ledger(rendered_ledger_lines(
                &parsed_conversation(session),
                options,
                content_width,
            )),
            DisplayFormat::Plain => Self::Plain(plain_text(
                session,
                ExportOptions {
                    show_tools: !options.no_tools,
                    show_thinking: options.show_thinking,
                },
            )),
        }
    }

    fn write(&self, writer: &mut dyn Write, no_color: bool) {
        match self {
            Self::Ledger(lines) => write_rendered_lines(lines, no_color, writer),
            Self::Plain(text) => {
                let _ = writer.write_all(text.as_bytes());
            }
        }
    }
}

/// The terminal printout of `session` in `format`, uncolored and without a
/// pager.
#[cfg(test)]
pub(crate) fn printout(
    session: DisplayEntries,
    options: &DisplayOptions,
    format: DisplayFormat,
) -> String {
    let mut printed = Vec::new();
    Printout::of(session, options, format).write(&mut printed, true);
    String::from_utf8_lossy(&printed).into_owned()
}

/// Report each line of `file_path` that did not parse, on stderr and in the
/// debug log, when `--debug` is set.
fn report_malformed_lines(
    file_path: &Path,
    malformed_lines: &[MalformedLine],
    debug_level: Option<DebugLevel>,
) {
    for line in malformed_lines {
        debug::error(
            debug_level,
            &format!(
                "Failed to parse line {}: {}",
                line.line_number, line.error_message
            ),
        );
        if debug_level.is_some() {
            let _ = debug_log::log_display_error(
                file_path,
                line.line_number,
                &line.error_message,
                &line.line_content,
            );
        }
    }
}

/// The viewer's ledger rows for `conversation`, as `--render` and the
/// terminal printout print them: tools whole or summarized, and every task
/// report and sub-agent reply whole, since nothing here can expand one.
fn rendered_ledger_lines(
    conversation: &ParsedConversation,
    options: &DisplayOptions,
    content_width: usize,
) -> Vec<RenderedLine> {
    let render_options = RenderOptions {
        tool_display: if options.no_tools {
            ToolDisplayMode::Hidden
        } else {
            ToolDisplayMode::Full
        },
        show_thinking: options.show_thinking,
        show_timing: false, // Non-TUI render doesn't support timing toggle
        content_width,
        expanded_tool_outputs: std::collections::BTreeSet::new(),
        can_expand: false,
    };
    render_parsed_conversation(conversation, &render_options).lines
}

/// Render a conversation in TUI ledger format to terminal (for debugging)
pub fn render_to_terminal(file_path: &Path, options: &DisplayOptions) -> Result<()> {
    let conversation = parse_unattributed_conversation_file(file_path)?;
    let lines = rendered_ledger_lines(&conversation, options, terminal_content_width());
    write_through_pager(options.use_pager, |writer| {
        write_rendered_lines(&lines, options.no_color, writer);
    });
    Ok(())
}

/// Run `write` against a pager's input when `use_pager` is set and the pager
/// starts, and against stdout otherwise; then wait for the pager to exit.
fn write_through_pager(use_pager: bool, write: impl FnOnce(&mut dyn Write)) {
    let mut pager_child = if use_pager {
        pager::spawn_pager().ok()
    } else {
        None
    };
    let mut stdout_handle = io::stdout().lock();
    let writer: &mut dyn Write = if let Some(ref mut child) = pager_child {
        child.stdin.as_mut().unwrap()
    } else {
        &mut stdout_handle
    };
    write(writer);

    drop(stdout_handle);
    if let Some(mut child) = pager_child {
        let _ = child.wait();
    }
}

/// Write the viewer's ledger rows as terminal text, colored unless
/// `no_color`. Stops when the output closes (the pager quit).
fn write_rendered_lines(lines: &[RenderedLine], no_color: bool, writer: &mut dyn Write) {
    'outer: for line in lines {
        for (text, style) in &line.spans {
            let output: Box<dyn std::fmt::Display> = if no_color {
                Box::new(text.as_str())
            } else {
                let mut styled = text.as_str().normal();

                if let Some((r, g, b)) = style.fg {
                    styled = styled.custom_color(CustomColor { r, g, b });
                }
                if style.bold {
                    styled = styled.bold();
                }
                if style.dimmed {
                    styled = styled.dimmed();
                }
                if style.italic {
                    styled = styled.italic();
                }

                Box::new(styled)
            };

            if write!(writer, "{}", output).is_err() {
                break 'outer;
            }
        }
        if writeln!(writer).is_err() {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `--render` has no expand gesture, so a task report prints whole with
    /// tools hidden and with tools shown alike.
    #[test]
    fn render_prints_a_task_report_whole_under_the_task_label() {
        use crate::history::task_notification::test_support::*;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("report.jsonl");
        let notification = serde_json::json!({
            "type": "user",
            "timestamp": "2024-01-01T00:00:02Z",
            "message": {"role": "user", "content": AGENT_REPORT}
        })
        .to_string();
        std::fs::write(&path, format!("{notification}\n")).unwrap();
        let conversation = parse_unattributed_conversation_file(&path).unwrap();

        for no_tools in [true, false] {
            let options = DisplayOptions {
                no_tools,
                ..DisplayOptions::default()
            };
            let text = rendered_ledger_lines(&conversation, &options, 80)
                .iter()
                .map(|line| {
                    line.spans
                        .iter()
                        .map(|(text, _)| text.as_str())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n");

            assert!(
                text.starts_with(&format!(
                    "     Task │ {AGENT_SUMMARY}\n          │ {AGENT_USAGE_LINE}"
                )),
                "no_tools={no_tools}:\n{text}"
            );
            assert!(
                text.contains(AGENT_REPORT_LAST_LINE),
                "no_tools={no_tools}:\n{text}"
            );
            assert!(!text.contains("more lines"), "no_tools={no_tools}:\n{text}");
            assert!(!text.contains("task-id"), "no_tools={no_tools}:\n{text}");
        }
    }

    #[test]
    fn the_terminal_printout_shows_a_background_launch_as_running_in_the_background() {
        use crate::history::subagent_launch::{
            BACKGROUND_LAUNCH_RESULT, test_support::write_launch_session,
        };
        let project = tempfile::tempdir().unwrap();
        let (transcript, _) = write_launch_session(project.path());
        let session = crate::history::sniffed_display_log_entries(&transcript).unwrap();

        let printed = printout(session, &DisplayOptions::default(), DisplayFormat::Plain);

        assert!(printed.contains(BACKGROUND_LAUNCH_RESULT), "{printed}");
        assert!(!printed.contains("Async agent launched"), "{printed}");
    }

    /// A path no list row holds is read by detecting its format, so a Pi
    /// session prints as Pi, not as an empty Claude transcript.
    #[test]
    fn the_terminal_printout_reads_a_path_no_row_holds_by_its_format() {
        let path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/pi/v3-branched.jsonl");

        let session = read_session_to_print(&[], &path).unwrap();
        let printed = printout(session, &DisplayOptions::default(), DisplayFormat::Plain);

        assert!(printed.contains("active root question"), "{printed}");
        assert!(printed.contains("root answer"), "{printed}");
    }

    #[test]
    fn the_terminal_printout_shows_a_skill_load_once() {
        use crate::history::skill_text::test_support::{SKILL_CALL_LOAD, SLASH_COMMAND_LOAD};

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        std::fs::write(
            &path,
            [&SKILL_CALL_LOAD[..], &SLASH_COMMAND_LOAD]
                .concat()
                .join("\n"),
        )
        .unwrap();
        let session = crate::history::sniffed_display_log_entries(&path).unwrap();

        let text = printout(session, &DisplayOptions::default(), DisplayFormat::Plain);

        assert!(
            text.contains("You: /frontend-design:frontend-design"),
            "{text}"
        );
        assert!(
            text.contains("Tool: Skill: write-commit-messages"),
            "{text}"
        );
        assert!(!text.contains("*Skill:"), "{text}");
    }

    #[test]
    fn the_terminal_printout_shows_a_handed_back_report_as_a_task_row() {
        use crate::history::subagent_report::test_support::{
            DELIVERED_NOTE, DESCRIPTION, FRAME_OPENING, REPORT_FIRST_LINE, write_handback_session,
        };
        let project = tempfile::tempdir().unwrap();
        let (transcript, subagents) = write_handback_session(project.path(), true);
        let session =
            display_log_entries(crate::history::Source::Claude, &transcript, &subagents).unwrap();
        let with_thinking = DisplayOptions {
            show_thinking: true,
            ..DisplayOptions::default()
        };

        let printed = printout(session, &with_thinking, DisplayFormat::Plain);

        assert!(
            printed.contains(&format!(
                "{}: Agent \"{DESCRIPTION}\" handed back its report",
                crate::history::TASK_LABEL
            )),
            "{printed}"
        );
        assert_eq!(printed.matches(REPORT_FIRST_LINE).count(), 1, "{printed}");
        assert!(
            printed.contains("SubagentHandback: report delivered"),
            "{printed}"
        );
        assert!(!printed.contains(FRAME_OPENING), "{printed}");
        assert!(!printed.contains(DELIVERED_NOTE), "{printed}");
    }
}
