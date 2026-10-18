//! `context.append_message` records by sender. Kimi Code names each message's
//! sender in `message.origin.kind` and shows `user` messages as the user's
//! prompt; the reader converts each other kind to a labelled row, a call or
//! nothing, as Kimi shows it.

use serde_json::{Map, Value, json};

use crate::agent::transcript::tool_result_text;
use crate::history::TaskReport;
use crate::history::format::dropped::DroppedRecords;
use crate::history::task_notification::{ReportKind, ReportSender};
use crate::log_entry::{ContentBlock, HOOK_LABEL, LogEntry, Tool, UserContent, UserMessage};

const ORIGIN_KIND: &str = "origin.kind";
const SCHEDULED_LABEL: &str = "Scheduled";

/// A user message's conversion, by sender.
pub(super) enum Appended {
    /// An entry to show as it is.
    Entry(Box<LogEntry>),
    /// A user-run `!` command. Its output arrives in the next `shell_command`
    /// record.
    ShellCommand(String),
    /// The output of the `!` command before it.
    ShellOutput(String),
}

/// `message`, converted by its `origin.kind`. `None` when Kimi shows nothing
/// for the kind, and for an assistant message other than a hook's output.
pub(super) fn appended_message(
    message: &Map<String, Value>,
    timestamp: Option<String>,
    dropped: &mut DroppedRecords,
) -> Option<Appended> {
    let origin = Origin(message.get("origin").and_then(Value::as_object));
    let kind = origin.text("kind");
    let content = message
        .get("content")
        .and_then(Value::as_array)
        .map_or(&[][..], Vec::as_slice);
    // Kimi records a blocking hook's output as an assistant message; loop
    // events carry the text of every other assistant message.
    match message.get("role").and_then(Value::as_str) {
        Some("assistant") if kind == Some("hook_result") => {
            return Some(Appended::Entry(Box::new(hook_result(
                content, origin, timestamp,
            ))));
        }
        Some("user") => {}
        _ => return None,
    }
    let entry = match kind {
        None | Some("user") => prompt(content, origin, timestamp),
        // Kimi shows none of these: injected context, sub-agent prompts, goal
        // continuations and empty retry messages.
        Some("injection" | "system_trigger" | "retry") => {
            dropped.skipped(ORIGIN_KIND, kind);
            None
        }
        Some("background_task" | "task") => {
            task_notice(content, timestamp.clone()).or_else(|| prompt(content, origin, timestamp))
        }
        Some("hook_result") => Some(hook_result(content, origin, timestamp)),
        Some("skill_activation") if origin.text("trigger") == Some("user-slash") => {
            let command = slash_command(origin.text("skillName")?, origin.text("skillArgs"));
            user_text_entry(vec![command], timestamp)
        }
        // Skipped when the model or another skill loaded it: its `Skill` call
        // shows the load.
        Some("skill_activation") => {
            dropped.skipped(ORIGIN_KIND, kind);
            None
        }
        Some("plugin_command") => {
            let name = format!(
                "{}:{}",
                origin.text("pluginId")?,
                origin.text("commandName")?
            );
            user_text_entry(
                vec![slash_command(&name, origin.text("commandArgs"))],
                timestamp,
            )
        }
        Some("cron_job") => Some(labelled_row(
            SCHEDULED_LABEL,
            reminder_prompt(&joined_text(content)).to_owned(),
            timestamp,
        )),
        Some("cron_missed") => Some(labelled_row(
            SCHEDULED_LABEL,
            missed_reminders(origin, &joined_text(content)),
            timestamp,
        )),
        Some("shell_command") => return Some(shell_command(&joined_text(content), origin)),
        Some(_) => {
            dropped.unrecognised(ORIGIN_KIND, kind);
            prompt(content, origin, timestamp)
        }
    };
    entry.map(|entry| Appended::Entry(Box::new(entry)))
}

/// A message's `origin` object, absent in a record without one.
#[derive(Clone, Copy)]
struct Origin<'a>(Option<&'a Map<String, Value>>);

impl<'a> Origin<'a> {
    fn get(self, field: &str) -> Option<&'a Value> {
        self.0?.get(field)
    }

    fn text(self, field: &str) -> Option<&'a str> {
        self.get(field).and_then(Value::as_str)
    }
}

/// The user's prompt: its text parts, without the bundled skill files at its
/// start or injected `<system-reminder>` blocks.
fn prompt(content: &[Value], origin: Origin, timestamp: Option<String>) -> Option<LogEntry> {
    let bundled_skills = origin
        .get("skillActivations")
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    let texts = content
        .iter()
        .skip(bundled_skills)
        .filter_map(part_text)
        .filter(|text| !is_injected_context(text))
        .map(str::to_owned)
        .collect();
    user_text_entry(texts, timestamp)
}

/// True when a user-role content block is injected context, not the user's
/// text. Kimi wraps plan-mode instructions, task reminders and other injected
/// fragments in a `<system-reminder>` element.
fn is_injected_context(text: &str) -> bool {
    let text = text.trim();
    text.starts_with("<system-reminder>") && text.ends_with("</system-reminder>")
}

/// A background task's `<notification>` as a `Task` row holding its status
/// line, or its `Title:` text when the notice holds no status line. `None`
/// when the notice holds neither.
fn task_notice(content: &[Value], timestamp: Option<String>) -> Option<LogEntry> {
    let notice = joined_text(content);
    let summary = notice_status_line(&notice).or_else(|| notice_title(&notice))?;
    let report = TaskReport {
        kind: ReportKind::Task,
        summary: summary.to_owned(),
        usage: None,
        body: None,
        sender: ReportSender::default(),
    };
    Some(user_entry(
        UserContent::String(report.notification_text()),
        timestamp,
    ))
}

const NOTICE_TITLE_PREFIX: &str = "Title: ";

/// The first line of a notice's body, past its `Title:`, `Severity:` and
/// `Wall time:` lines.
fn notice_status_line(notice: &str) -> Option<&str> {
    const HEADER_PREFIXES: [&str; 3] = [NOTICE_TITLE_PREFIX, "Severity: ", "Wall time: "];
    notice_body_lines(notice).find(|line| {
        !line.is_empty()
            && !HEADER_PREFIXES
                .iter()
                .any(|prefix| line.starts_with(prefix))
    })
}

fn notice_title(notice: &str) -> Option<&str> {
    notice_body_lines(notice)
        .find_map(|line| line.strip_prefix(NOTICE_TITLE_PREFIX))
        .map(str::trim)
        .filter(|title| !title.is_empty())
}

/// A notice's lines inside its `<notification>` element, up to the first tag:
/// the output file, an output preview, or the notice's close.
fn notice_body_lines(notice: &str) -> impl Iterator<Item = &str> {
    notice
        .lines()
        .skip_while(|line| !line.trim_start().starts_with("<notification"))
        .skip(1)
        .map(str::trim)
        .take_while(|line| !line.starts_with('<'))
}

/// A hook's output as a `Hook` row: `<event>: <output>` for each
/// `<hook_result>` block, with `blocked` after the event when the hook
/// blocked.
fn hook_result(content: &[Value], origin: Origin, timestamp: Option<String>) -> LogEntry {
    let text = joined_text(content);
    let blocked = origin.get("blocked").and_then(Value::as_bool) == Some(true);
    let results = hook_result_blocks(&text)
        .unwrap_or_else(|| vec![(origin.text("event").unwrap_or("hook"), text.trim())]);
    let lines = results
        .into_iter()
        .map(|(event, output)| {
            let event = if blocked {
                format!("{event} blocked")
            } else {
                event.to_owned()
            };
            if output.is_empty() {
                event
            } else {
                format!("{event}: {output}")
            }
        })
        .collect::<Vec<_>>();
    labelled_row(HOOK_LABEL, lines.join("\n\n"), timestamp)
}

/// Each `<hook_result hook_event="…">` block's event and output. `None` when
/// `text` holds no block, or text outside one: Kimi then shows the whole text
/// under the record's own event.
fn hook_result_blocks(text: &str) -> Option<Vec<(&str, &str)>> {
    const OPEN: &str = "<hook_result hook_event=\"";
    const CLOSE: &str = "</hook_result>";
    let mut blocks = Vec::new();
    let mut rest = text.trim();
    while !rest.is_empty() {
        let (event, after_event) = rest.strip_prefix(OPEN)?.split_once("\">")?;
        let (output, after_block) = after_event.split_once(CLOSE)?;
        blocks.push((event, output.trim()));
        rest = after_block.trim_start();
    }
    (!blocks.is_empty()).then_some(blocks)
}

/// `/<name>`, then its arguments when there are any.
fn slash_command(name: &str, arguments: Option<&str>) -> String {
    match arguments
        .map(str::trim)
        .filter(|arguments| !arguments.is_empty())
    {
        Some(arguments) => format!("/{name} {arguments}"),
        None => format!("/{name}"),
    }
}

/// A fired reminder's prompt: the text inside its `<prompt>` element, or the
/// reminder without its `<cron-fire>` envelope.
fn reminder_prompt(reminder: &str) -> &str {
    if let Some((_, after_open)) = reminder.split_once("<prompt>\n")
        && let Some((prompt, _)) = after_open.rsplit_once("\n</prompt>")
    {
        return prompt;
    }
    without_cron_envelope(reminder)
}

/// The missed reminders' count, then Kimi's notice listing them.
fn missed_reminders(origin: Origin, notice: &str) -> String {
    let count = origin.get("count").and_then(Value::as_u64).unwrap_or(0);
    let mut text = match count {
        1 => "Missed 1 scheduled reminder".to_owned(),
        count => format!("Missed {count} scheduled reminders"),
    };
    let notice = without_cron_envelope(notice).trim();
    if !notice.is_empty() {
        text.push('\n');
        text.push_str(notice);
    }
    text
}

fn without_cron_envelope(text: &str) -> &str {
    let Some((first_line, rest)) = text.split_once('\n') else {
        return text;
    };
    match rest.rsplit_once('\n') {
        Some((inside, "</cron-fire>")) if first_line.starts_with("<cron-fire ") => inside,
        _ => text,
    }
}

/// A `!` command's input record as its command, or its output record as the
/// command's output.
fn shell_command(text: &str, origin: Origin) -> Appended {
    if origin.text("phase") == Some("input") {
        let command = tagged_text(text, "bash-input").unwrap_or_else(|| text.to_owned());
        return Appended::ShellCommand(command.trim().to_owned());
    }
    let output = ["bash-stdout", "bash-stderr"]
        .into_iter()
        .filter_map(|tag| tagged_text(text, tag))
        .map(|stream| stream.trim().to_owned())
        .filter(|stream| !stream.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    Appended::ShellOutput(output)
}

/// A user-run shell command's call, by its ID.
pub(super) struct ShellCall {
    id: String,
}

impl ShellCall {
    /// Each `!` command's input line gives its call an ID unique in the wire.
    pub(super) fn at_line(line_number: usize) -> Self {
        Self {
            id: format!("shell_{line_number}"),
        }
    }

    /// An entry holding this call, without a result yet.
    pub(super) fn entry(&self, command: &str, timestamp: Option<String>) -> LogEntry {
        user_entry(
            UserContent::Blocks(vec![ContentBlock::ToolUse {
                id: self.id.clone(),
                name: "shell".to_owned(),
                tool: Tool::UserShell,
                input: json!({ "command": command }),
            }]),
            timestamp,
        )
    }

    /// Adds `output` to `entry`, the entry holding this call, as its result.
    pub(super) fn add_result(&self, entry: &mut LogEntry, output: String) {
        let LogEntry::User {
            message:
                UserMessage {
                    content: UserContent::Blocks(blocks),
                    ..
                },
            ..
        } = entry
        else {
            return;
        };
        let text = tool_result_text(&Value::String(output)).unwrap_or_default();
        blocks.push(ContentBlock::ToolResult {
            tool_use_id: self.id.clone(),
            content: Some(json!(text)),
            standalone_tool_name: None,
        });
    }
}

/// The text between `<tag>` and `</tag>`, with Kimi's XML escapes undone.
fn tagged_text(text: &str, tag: &str) -> Option<String> {
    let (_, after_open) = text.split_once(&format!("<{tag}>"))?;
    let (inside, _) = after_open.split_once(&format!("</{tag}>"))?;
    Some(
        inside
            .replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&quot;", "\"")
            .replace("&amp;", "&"),
    )
}

fn part_text(part: &Value) -> Option<&str> {
    part.get("text")
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
}

fn joined_text(content: &[Value]) -> String {
    content
        .iter()
        .filter_map(part_text)
        .collect::<Vec<_>>()
        .join("\n")
}

fn labelled_row(label: &str, text: String, timestamp: Option<String>) -> LogEntry {
    LogEntry::PiMetadata {
        label: label.to_owned(),
        text,
        timestamp,
        searchable: true,
        usage: None,
    }
}

fn user_text_entry(texts: Vec<String>, timestamp: Option<String>) -> Option<LogEntry> {
    if texts.is_empty() {
        return None;
    }
    Some(user_entry(
        UserContent::Blocks(
            texts
                .into_iter()
                .map(|text| ContentBlock::Text { text })
                .collect(),
        ),
        timestamp,
    ))
}

fn user_entry(content: UserContent, timestamp: Option<String>) -> LogEntry {
    LogEntry::User {
        message: UserMessage {
            role: "user".to_owned(),
            content,
        },
        timestamp,
        uuid: None,
        cwd: None,
        parent_tool_use_id: None,
        source_tool_use_id: None,
        usage: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::format::dropped::DropOutcome;
    use crate::history::user_task_report;

    /// A `context.append_message` line: `role`, one text part per item of
    /// `texts`, and `origin` when it is not null.
    fn appended(role: &str, texts: &[&str], origin: Value) -> Value {
        let content = texts
            .iter()
            .map(|text| json!({"type": "text", "text": text}))
            .collect::<Vec<_>>();
        let mut message = json!({"role": role, "content": content, "toolCalls": []});
        if !origin.is_null() {
            message["origin"] = origin;
        }
        json!({"type": "context.append_message", "message": message, "time": 1786010401000_i64})
    }

    fn read_wire(lines: &[Value]) -> (Vec<LogEntry>, DroppedRecords) {
        let mut wire = json!({"type": "metadata", "protocol_version": "1.5"}).to_string();
        for line in lines {
            wire.push('\n');
            wire.push_str(&line.to_string());
        }
        let parsed = super::super::parse_reader(
            std::io::Cursor::new(wire.into_bytes()),
            DroppedRecords::counting(),
        )
        .unwrap()
        .expect("a wire");
        (
            parsed.entries.into_iter().map(|(_, entry)| entry).collect(),
            parsed.dropped,
        )
    }

    fn entries_of(lines: &[Value]) -> Vec<LogEntry> {
        read_wire(lines).0
    }

    fn user_texts(entry: &LogEntry) -> Vec<&str> {
        match entry {
            LogEntry::User {
                message:
                    UserMessage {
                        content: UserContent::Blocks(blocks),
                        ..
                    },
                ..
            } => blocks
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect(),
            other => panic!("not a user entry with blocks: {other:?}"),
        }
    }

    fn labelled(entry: &LogEntry) -> (&str, &str) {
        match entry {
            LogEntry::PiMetadata { label, text, .. } => (label.as_str(), text.as_str()),
            other => panic!("not a labelled row: {other:?}"),
        }
    }

    #[test]
    fn a_task_notice_shows_its_status_line_as_a_task_row() {
        let completed = concat!(
            "<notification id=\"task:bash-1:completed\" category=\"task\" type=\"task.completed\" source_kind=\"background_task\" source_id=\"bash-1\">\n",
            "Title: Background process completed\n",
            "Severity: info\n",
            "Headless trace completed.\n",
            "<output-file path=\"C:/sessions/tasks/bash-1/output.log\" bytes=\"1982\">\n",
            "Read the output file to retrieve the result: C:/sessions/tasks/bash-1/output.log\n",
            "</output-file>\n",
            "</notification>",
        );
        let timed_out = concat!(
            "<notification id=\"task:bash-2:timed_out\" category=\"task\" type=\"task.timed_out\" source_kind=\"background_task\" source_id=\"bash-2\">\n",
            "Title: Background process timed_out\n",
            "Severity: warning\n",
            "Wall time: 600.000 seconds\n",
            "Bash: du -sh . timed out.\n",
            "</notification>",
        );

        let entries = entries_of(&[
            appended(
                "user",
                &[completed],
                json!({"kind": "background_task", "taskId": "bash-1", "status": "completed"}),
            ),
            appended(
                "user",
                &[timed_out],
                json!({"kind": "task", "taskId": "bash-2", "status": "timed_out"}),
            ),
        ]);

        let summaries = entries
            .iter()
            .map(|entry| match entry {
                LogEntry::User { message, .. } => {
                    user_task_report(&message.content)
                        .expect("a task report")
                        .summary
                }
                other => panic!("not a user entry: {other:?}"),
            })
            .collect::<Vec<_>>();
        assert_eq!(
            summaries,
            ["Headless trace completed.", "Bash: du -sh . timed out."]
        );
    }

    #[test]
    fn a_task_notice_without_a_status_line_shows_its_title_as_a_task_row() {
        let untitled_body = concat!(
            "<notification id=\"task:bash-3:lost\" category=\"task\" type=\"task.lost\" source_kind=\"background_task\" source_id=\"bash-3\">\n",
            "Title: Background process lost\n",
            "Severity: warning\n",
            "</notification>",
        );

        let entries = entries_of(&[appended(
            "user",
            &[untitled_body],
            json!({"kind": "task", "taskId": "bash-3", "status": "lost"}),
        )]);

        let LogEntry::User { message, .. } = &entries[0] else {
            panic!("not a user entry: {:?}", entries[0]);
        };
        assert_eq!(
            user_task_report(&message.content)
                .expect("a task report")
                .summary,
            "Background process lost"
        );
    }

    #[test]
    fn a_hook_result_shows_as_a_hook_row_and_a_blocking_one_as_blocked() {
        let entries = entries_of(&[
            appended(
                "user",
                &[
                    "<hook_result hook_event=\"UserPromptSubmit\">\nPrompt quality: C.\n</hook_result>",
                ],
                json!({"kind": "hook_result", "event": "UserPromptSubmit"}),
            ),
            appended(
                "assistant",
                &["<hook_result hook_event=\"PreToolUse\">\nrm is not allowed.\n</hook_result>"],
                json!({"kind": "hook_result", "event": "PreToolUse", "blocked": true}),
            ),
        ]);

        let rows = entries.iter().map(labelled).collect::<Vec<_>>();
        assert_eq!(
            rows,
            [
                (HOOK_LABEL, "UserPromptSubmit: Prompt quality: C."),
                (HOOK_LABEL, "PreToolUse blocked: rm is not allowed."),
            ]
        );
    }

    #[test]
    fn a_retry_doesnt_show() {
        let entries = entries_of(&[
            appended("user", &[], json!({"kind": "retry", "trigger": "error"})),
            appended("user", &["the prompt"], json!({"kind": "user"})),
        ]);

        assert_eq!(entries.len(), 1, "{entries:?}");
        assert_eq!(user_texts(&entries[0]), ["the prompt"]);
    }

    #[test]
    fn a_shell_command_and_its_output_show_as_one_call_and_its_result() {
        let entries = entries_of(&[
            appended(
                "user",
                &["<bash-input>\ngit log -1 &amp;&amp; echo &quot;done&quot;\n</bash-input>"],
                json!({"kind": "shell_command", "phase": "input"}),
            ),
            appended(
                "user",
                &[
                    "<bash-stdout>abc123 First commit\ndone\n</bash-stdout><bash-stderr>warning: &lt;none&gt;</bash-stderr>",
                ],
                json!({"kind": "shell_command", "phase": "output"}),
            ),
        ]);

        assert_eq!(entries.len(), 1, "{entries:?}");
        let LogEntry::User { message, .. } = &entries[0] else {
            panic!("not a user entry: {:?}", entries[0]);
        };
        let UserContent::Blocks(blocks) = &message.content else {
            panic!("no blocks");
        };
        match blocks.as_slice() {
            [
                ContentBlock::ToolUse {
                    id,
                    tool: Tool::UserShell,
                    input,
                    ..
                },
                ContentBlock::ToolResult {
                    tool_use_id,
                    content: Some(result),
                    ..
                },
            ] => {
                assert_eq!(input["command"], "git log -1 && echo \"done\"");
                assert_eq!(tool_use_id, id);
                assert_eq!(
                    result.as_str(),
                    Some("abc123 First commit\ndone\nwarning: <none>")
                );
            }
            other => panic!("not a call and its result: {other:?}"),
        }
    }

    #[test]
    fn a_scheduled_reminder_shows_its_prompt_and_a_missed_one_its_count() {
        let entries = entries_of(&[
            appended(
                "user",
                &[
                    "<cron-fire jobId=\"job-1\" cron=\"0 9 * * *\" recurring=\"true\" coalescedCount=\"1\" stale=\"false\">\n<prompt>\nCheck the build.\n</prompt>\n</cron-fire>",
                ],
                json!({"kind": "cron_job", "jobId": "job-1", "cron": "0 9 * * *", "recurring": true}),
            ),
            appended(
                "user",
                &["<cron-fire missed=\"2\">\nCheck the build.\nWater the plants.\n</cron-fire>"],
                json!({"kind": "cron_missed", "count": 2}),
            ),
        ]);

        let rows = entries.iter().map(labelled).collect::<Vec<_>>();
        assert_eq!(
            rows,
            [
                (SCHEDULED_LABEL, "Check the build."),
                (
                    SCHEDULED_LABEL,
                    "Missed 2 scheduled reminders\nCheck the build.\nWater the plants."
                ),
            ]
        );
    }

    #[test]
    fn a_plugin_command_shows_its_slash_command() {
        let entries = entries_of(&[appended(
            "user",
            &["PLUGIN_COMMAND_PROMPT"],
            json!({"kind": "plugin_command", "pluginId": "review", "commandName": "pr", "commandArgs": "42", "trigger": "user-slash"}),
        )]);

        assert_eq!(user_texts(&entries[0]), ["/review:pr 42"]);
    }

    #[test]
    fn a_prompt_with_bundled_skills_shows_without_the_skill_files() {
        let entries = entries_of(&[appended(
            "user",
            &[
                "SKILL_FILE_TEXT one",
                "SKILL_FILE_TEXT two",
                "fix the parser",
            ],
            json!({
                "kind": "user",
                "skillActivations": [
                    {"activationId": "a1", "skillName": "rust"},
                    {"activationId": "a2", "skillName": "tests"}
                ]
            }),
        )]);

        assert_eq!(user_texts(&entries[0]), ["fix the parser"]);
    }

    #[test]
    fn skipped_and_unknown_senders_are_counted_and_an_unknown_one_shows_its_text() {
        let (entries, dropped) = read_wire(&[
            appended("user", &["kept text"], json!({"kind": "teleport"})),
            appended(
                "user",
                &["prompt"],
                json!({"kind": "system_trigger", "name": "subagent"}),
            ),
            appended(
                "user",
                &["reminder"],
                json!({"kind": "injection", "variant": "todo"}),
            ),
            appended(
                "user",
                &["skill file"],
                json!({"kind": "skill_activation", "skillName": "docs", "trigger": "model-tool"}),
            ),
            appended("user", &[], json!({"kind": "retry"})),
        ]);

        assert_eq!(user_texts(&entries[0]), ["kept text"]);
        let version = Some("wire protocol 1.5");
        assert_eq!(
            dropped.counts().collect::<Vec<_>>(),
            [
                (version, "origin.kind=injection", DropOutcome::Skipped, 1),
                (version, "origin.kind=retry", DropOutcome::Skipped, 1),
                (
                    version,
                    "origin.kind=skill_activation",
                    DropOutcome::Skipped,
                    1
                ),
                (
                    version,
                    "origin.kind=system_trigger",
                    DropOutcome::Skipped,
                    1
                ),
                (
                    version,
                    "origin.kind=teleport",
                    DropOutcome::Unrecognised,
                    1
                ),
            ]
        );
    }

    #[test]
    fn injected_context_is_recognised_by_its_reminder_wrapper() {
        assert!(is_injected_context(
            "<system-reminder>\nPlan mode is active.\n</system-reminder>"
        ));
        for typed_by_a_user in [
            "what does <system-reminder> mean?",
            "plain question",
            "<b>bold</b>",
        ] {
            assert!(!is_injected_context(typed_by_a_user), "{typed_by_a_user}");
        }
    }
}
