//! Tool-specific formatting for nicer display of tool calls.
//!
//! Instead of showing raw JSON, this module formats each tool's input
//! in a human-readable way that highlights the most relevant information.
//! The layout is chosen by the call's canonical [`Tool`]; the header always
//! opens with the provider's own tool name.

use crate::duration::format_duration;
use crate::log_entry::{DELIVERED_REPORT_KEY, Tool, replacement_diff};
use serde_json::Value;
use std::time::Duration;

/// Formatted tool call representation
pub struct FormattedToolCall {
    /// The header up to its value: `Read: `, `Task (Explore): `, or the bare
    /// `name:` of a call formatted from its raw input. Empty when the value
    /// is the whole header, as for a Codex launch without a task name.
    pub prefix: String,
    /// The rest of the header: a path, a pattern, a command's first line.
    pub value: String,
    /// True when `value` is the first line of the call's text and `body` its
    /// later lines, as for a shell command. Otherwise `value` names the call
    /// (a path, a pattern) and `body` stands apart from it (a diff, a prompt).
    pub value_is_first_line: bool,
    /// Optional continuation lines (e.g., prompt text, diff lines)
    pub body: Option<ToolBody>,
    /// A shell call's commands after the first, each after `prefix`; `value`
    /// and `body` show the first. Empty for any other call.
    pub later_commands: Vec<FormattedCommand>,
}

/// A shell command's first row and its later rows, as
/// [`FormattedToolCall::value`] and [`FormattedToolCall::body`] show the
/// first command.
pub struct FormattedCommand {
    pub value: String,
    pub body: Option<ToolBody>,
}

impl FormattedToolCall {
    /// The header as one line: `prefix` then `value`.
    pub fn header(&self) -> String {
        format!("{}{}", self.prefix, self.value)
    }

    /// The call as exports write it: the header, its body on the lines
    /// after it, then each later command after `prefix`, followed by its
    /// body.
    pub fn text(&self) -> String {
        let mut text = self.header();
        push_body_lines(&mut text, self.body.as_ref());
        for command in &self.later_commands {
            text.push('\n');
            text.push_str(&self.prefix);
            text.push_str(&command.value);
            push_body_lines(&mut text, command.body.as_ref());
        }
        text
    }

    /// A header whose `value` names the call.
    fn named(prefix: String, value: String, body: Option<ToolBody>) -> Self {
        Self {
            prefix,
            value,
            value_is_first_line: false,
            body,
            later_commands: Vec::new(),
        }
    }
}

fn push_body_lines(text: &mut String, body: Option<&ToolBody>) {
    if let Some(body) = body {
        text.push('\n');
        text.push_str(&body.text);
    }
}

/// `max_width` for a caller that wraps at render: a shell command's lines
/// stay whole.
pub const NO_WRAP: usize = 0;

/// The continuation lines of a formatted tool call.
pub struct ToolBody {
    pub text: String,
    pub kind: ToolBodyKind,
}

impl ToolBody {
    fn plain(text: String) -> Self {
        Self {
            text,
            kind: ToolBodyKind::Plain,
        }
    }

    fn diff(text: String) -> Self {
        Self {
            text,
            kind: ToolBodyKind::Diff,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolBodyKind {
    Plain,
    /// Lines carry git's bare `+` / `-` signs. Only these bodies are coloured,
    /// so a markdown bullet inside a plain body never reads as a removal.
    Diff,
}

/// The side of a diff a body line belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiffSide {
    Added,
    Removed,
}

impl ToolBodyKind {
    /// `None` for context lines and for every line of a plain body.
    pub fn diff_side(self, line: &str) -> Option<DiffSide> {
        match self {
            Self::Plain => None,
            Self::Diff if line.starts_with('+') => Some(DiffSide::Added),
            Self::Diff if line.starts_with('-') => Some(DiffSide::Removed),
            Self::Diff => None,
        }
    }
}

/// A call's header without its body: `prefix` then `value`, as
/// [`format_tool_call`] opens it.
pub struct ToolCallHeader {
    /// As [`FormattedToolCall::prefix`].
    pub prefix: String,
    /// The rest of the header, such as a path or a pattern, or a shell call's
    /// first command, whole.
    pub value: String,
    /// True when `value` is a shell command. [`format_tool_call`] shows its
    /// first line in the header and its later lines as the body.
    pub value_is_command: bool,
    /// A shell call's commands after `value`, each whole; empty for any
    /// other call.
    pub later_commands: Vec<String>,
}

impl ToolCallHeader {
    fn named(prefix: String, value: String) -> Self {
        Self {
            prefix,
            value,
            value_is_command: false,
            later_commands: Vec::new(),
        }
    }

    fn command(prefix: String, input: &Value) -> Self {
        let mut commands = shell_commands(input).unwrap_or_default().into_iter();
        Self {
            prefix,
            value: commands.next().unwrap_or_default().to_owned(),
            value_is_command: true,
            later_commands: commands.map(str::to_owned).collect(),
        }
    }

    /// A shell call's commands in order, `value` first; empty for any other
    /// call.
    pub fn commands(&self) -> impl Iterator<Item = &str> {
        self.value_is_command
            .then_some(self.value.as_str())
            .into_iter()
            .chain(self.later_commands.iter().map(String::as_str))
    }

    /// The bare `name:` of a call shown as its raw input.
    fn raw_input(name: &str) -> Self {
        Self::named(format!("{name}:"), String::new())
    }
}

/// Format a tool call for display
///
/// `max_width` wraps a shell command's lines; [`NO_WRAP`] leaves them whole.
pub fn format_tool_call(
    name: &str,
    tool: Tool,
    input: &Value,
    max_width: usize,
) -> FormattedToolCall {
    let header = tool_call_header(name, tool, input);
    if header.value_is_command {
        return format_shell(header, max_width);
    }
    FormattedToolCall::named(
        header.prefix,
        header.value,
        tool_call_body(tool, input).and_then(ToolCallBody::shown),
    )
}

/// The header of a call, built without its body.
pub fn tool_call_header(name: &str, tool: Tool, input: &Value) -> ToolCallHeader {
    if lacks_header_field(tool, input) {
        return ToolCallHeader::raw_input(name);
    }
    let field = |key: &str| string_field(input, key).unwrap_or("").to_owned();
    match tool {
        Tool::Shell => ToolCallHeader::command(name_prefix(name), input),
        Tool::Script => ToolCallHeader::named(name_prefix(name), script_header(input)),
        // The user ran it, so there is no tool to name: the header completes
        // the sentence its author label opens, `You ran …`.
        Tool::UserShell => ToolCallHeader::command("ran ".to_owned(), input),
        Tool::Read => read_header(name, input),
        Tool::Edit | Tool::Write | Tool::FileReference => {
            ToolCallHeader::named(name_prefix(name), field("file_path"))
        }
        Tool::Grep => grep_header(name, input),
        Tool::Glob => glob_header(name, input),
        Tool::DirectoryListing => ToolCallHeader::named(
            name_prefix(name),
            string_field(input, "path").unwrap_or(".").to_owned(),
        ),
        Tool::Skill => ToolCallHeader::named(name_prefix(name), field("skill")),
        Tool::Agent => agent_header(name, input),
        Tool::AgentMessage => ToolCallHeader::named(name_prefix(name), field("recipient")),
        Tool::ReceivedMessage => {
            ToolCallHeader::named(name_prefix(name), received_message_header(input))
        }
        Tool::AgentReport if is_delivered_report(input) => {
            ToolCallHeader::named(name_prefix(name), "report delivered".to_owned())
        }
        Tool::AgentReport => ToolCallHeader::named(format!("{name}:"), String::new()),
        Tool::WebFetch => ToolCallHeader::named(name_prefix(name), field("url")),
        Tool::WebSearch => web_search_header(name, input),
        Tool::Wait => match wait_header(input) {
            Some(header) => ToolCallHeader::named(name_prefix(name), header),
            None => ToolCallHeader::raw_input(name),
        },
        Tool::UserQuestion => ToolCallHeader::named(
            name_prefix(name),
            question_count(asked_questions(input).map_or(0, |questions| questions.len())),
        ),
        Tool::TaskList if task_list_steps(input).is_some() => task_list_header(name, input),
        // `ResultReceipt` reaches no header; the raw input keeps the match
        // exhaustive.
        Tool::TaskList | Tool::ResultReceipt | Tool::Other => ToolCallHeader::raw_input(name),
    }
}

/// The body of a call whose header names it, as [`tool_call_body`] finds it.
pub enum ToolCallBody<'a> {
    /// A diff, a prompt, a message, or a launch's settings as `key: value`
    /// lines; the viewer shows it as this text.
    Text(ToolBody),
    /// The input of a call [`tool_call_header`] has no layout for, or that
    /// lacks its header field. The viewer shows it as pretty JSON.
    RawInput(&'a Value),
}

impl ToolCallBody<'_> {
    /// The body as the viewer shows it.
    fn shown(self) -> Option<ToolBody> {
        match self {
            Self::Text(body) => Some(body),
            Self::RawInput(input) => raw_input_body(input),
        }
    }

    /// The body as search reads it: text as the viewer shows it, and input
    /// the viewer shows as JSON as [`unindented_json_with_decoded_strings`]
    /// writes it.
    pub fn searched(self) -> String {
        match self {
            Self::Text(body) => body.text,
            Self::RawInput(input) => unindented_json_with_decoded_strings(input),
        }
    }
}

/// The body of a call whose header names it: a diff, a prompt, a message,
/// or the input of a call [`tool_call_header`] has no layout for. A shell
/// command's later lines are not a body here: its header holds them.
pub fn tool_call_body(tool: Tool, input: &Value) -> Option<ToolCallBody<'_>> {
    if lacks_header_field(tool, input) {
        return Some(ToolCallBody::RawInput(input));
    }
    let plain_field =
        |key: &str| string_field(input, key).map(|text| ToolBody::plain(text.to_owned()));
    let text = match tool {
        Tool::Script => plain_field("source"),
        Tool::Edit => edit_body(input),
        Tool::Skill => plain_field("args"),
        Tool::Agent => agent_body(input),
        Tool::AgentMessage | Tool::ReceivedMessage => plain_field("message"),
        Tool::AgentReport if is_delivered_report(input) => None,
        Tool::AgentReport => plain_field("message"),
        Tool::WebFetch => plain_field("prompt"),
        Tool::Wait if wait_header(input).is_some() => typed_lines_body(input),
        Tool::TaskList => match task_list_steps(input) {
            Some(steps) => task_list_body(input, &steps),
            None => return Some(ToolCallBody::RawInput(input)),
        },
        Tool::UserQuestion => {
            asked_questions(input).map(|questions| ToolBody::plain(questions_text(&questions)))
        }
        Tool::Wait | Tool::ResultReceipt | Tool::Other => {
            return Some(ToolCallBody::RawInput(input));
        }
        Tool::Shell
        | Tool::UserShell
        | Tool::Read
        | Tool::Write
        | Tool::Grep
        | Tool::Glob
        | Tool::DirectoryListing
        | Tool::FileReference
        | Tool::WebSearch => None,
    };
    text.map(ToolCallBody::Text)
}

fn string_field<'a>(input: &'a Value, key: &str) -> Option<&'a str> {
    input.get(key).and_then(Value::as_str)
}

/// The commands a shell call runs, in order: its `commands`, a list of
/// strings, when the provider's reader wrote several, or else its
/// `command`. `None` without either, or when `commands` is empty or lists
/// anything but strings.
fn shell_commands(input: &Value) -> Option<Vec<&str>> {
    let Some(commands) = input.get("commands") else {
        return string_field(input, "command").map(|command| vec![command]);
    };
    commands
        .as_array()?
        .iter()
        .map(Value::as_str)
        .collect::<Option<Vec<_>>>()
        .filter(|commands| !commands.is_empty())
}

/// The calls `input` counts as in a run's summary row: one per command of a
/// shell call, one for any other call.
pub(crate) fn counted_calls(tool: Tool, input: &Value) -> usize {
    match tool {
        Tool::Shell | Tool::UserShell => shell_commands(input).map_or(1, |commands| commands.len()),
        _ => 1,
    }
}

/// Whether `input` lacks the key the layout for `tool` builds its header
/// from. A provider's mapping can leave a canonical call without it (an
/// OpenCode injected read whose narration did not parse carries the narration
/// under `input` instead of `file_path`); the name plus the input then shows
/// more than an empty header.
fn lacks_header_field(tool: Tool, input: &Value) -> bool {
    match tool {
        Tool::Shell | Tool::UserShell => shell_commands(input).is_none(),
        Tool::UserQuestion => asked_questions(input).is_none(),
        _ => header_field(tool, input).is_some_and(|key| string_field(input, key).is_none()),
    }
}

fn header_field(tool: Tool, input: &Value) -> Option<&'static str> {
    match tool {
        Tool::Script => Some("source"),
        Tool::Read | Tool::Edit | Tool::Write | Tool::FileReference => Some("file_path"),
        Tool::Grep | Tool::Glob => Some("pattern"),
        Tool::Skill => Some("skill"),
        // A launch without a task name opens its header with the tool's name.
        Tool::Agent if launch_settings(input).is_some() => None,
        Tool::Agent => Some("description"),
        Tool::AgentMessage => Some("recipient"),
        Tool::ReceivedMessage => Some("sender"),
        Tool::AgentReport if is_delivered_report(input) => None,
        Tool::AgentReport => Some("message"),
        Tool::WebFetch => Some("url"),
        Tool::WebSearch => Some("query"),
        // `lacks_header_field` checks a shell call's commands and a list of
        // questions.
        Tool::Shell | Tool::UserShell | Tool::UserQuestion => None,
        Tool::DirectoryListing
        | Tool::Wait
        | Tool::TaskList
        | Tool::ResultReceipt
        | Tool::Other => None,
    }
}

/// One question of a [`Tool::UserQuestion`] call: its title and the answers
/// it offers, if any.
pub(crate) struct AskedQuestion<'a> {
    pub(crate) title: &'a str,
    pub(crate) options: Vec<&'a str>,
}

/// The questions of a [`Tool::UserQuestion`] call; `None` unless `questions`
/// is a non-empty list of objects, each with a string `title`.
pub(crate) fn asked_questions(input: &Value) -> Option<Vec<AskedQuestion<'_>>> {
    let questions = input.get("questions")?.as_array()?;
    if questions.is_empty() {
        return None;
    }
    questions
        .iter()
        .map(|question| {
            Some(AskedQuestion {
                title: string_field(question, "title")?,
                options: question
                    .get("options")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .collect(),
            })
        })
        .collect()
}

fn question_count(count: usize) -> String {
    match count {
        1 => "1 question".to_owned(),
        count => format!("{count} questions"),
    }
}

/// Each title, numbered when there are several, then its options as bullets;
/// a blank line between questions. A numbered title's later lines hang
/// under its text, not under its number.
fn questions_text(questions: &[AskedQuestion<'_>]) -> String {
    let numbered = questions.len() > 1;
    questions
        .iter()
        .enumerate()
        .map(|(index, question)| {
            let number = if numbered {
                format!("{}. ", index + 1)
            } else {
                String::new()
            };
            let hanging_indent = " ".repeat(number.len());
            let mut title_lines = question.title.lines();
            let first_line = title_lines.next().unwrap_or_default();
            let mut lines = vec![format!("{number}{first_line}")];
            lines.extend(title_lines.map(|line| format!("{hanging_indent}{line}")));
            lines.extend(
                question
                    .options
                    .iter()
                    .map(|option| format!("  • {option}")),
            );
            lines.join("\n")
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn name_prefix(name: &str) -> String {
    format!("{name}: ")
}

/// The prompt of a call that launches a sub-agent; the viewer shows it as the
/// call's body. `None` for any other call.
pub(crate) fn launch_prompt(tool: Tool, input: &Value) -> Option<&str> {
    match tool {
        Tool::Agent => string_field(input, "prompt"),
        _ => None,
    }
}

fn agent_header(name: &str, input: &Value) -> ToolCallHeader {
    let description = string_field(input, "description");
    if let Some(launch) = launch_settings(input) {
        return match description {
            Some(task) => ToolCallHeader::named(name_prefix(name), launch_header(task, launch)),
            // The whole header is the value: `agent read` and search replace
            // an empty value with the call's input keys.
            None => ToolCallHeader::named(String::new(), launch_header(name, launch)),
        };
    }
    let prefix = match string_field(input, "subagent_type") {
        Some(subagent_type) => format!("{name} ({subagent_type}): "),
        None => name_prefix(name),
    };
    ToolCallHeader::named(prefix, description.unwrap_or("").to_owned())
}

fn agent_body(input: &Value) -> Option<ToolBody> {
    let prompt = launch_prompt(Tool::Agent, input);
    match launch_settings(input) {
        Some(launch) => launch_body(launch, prompt),
        None => prompt.map(|prompt| ToolBody::plain(prompt.to_owned())),
    }
}

/// A Codex `spawn_agent` call's settings, gathered under `launch`.
fn launch_settings(input: &Value) -> Option<&serde_json::Map<String, Value>> {
    input.get("launch").and_then(Value::as_object)
}

/// The launch settings a `spawn_agent` header carries; the body lists the rest.
const LAUNCH_HEADER_KEYS: [&str; 4] = ["agent_type", "model", "reasoning_effort", "fork_turns"];

/// `lead (agent_type) · model (effort) · fork last 3 turns`, unlabelled:
/// model names and efforts read as themselves. `lead` is the task name, or
/// the tool's name for a launch without a task name.
fn launch_header(lead: &str, launch: &serde_json::Map<String, Value>) -> String {
    let setting = |key: &str| launch.get(key).and_then(Value::as_str);
    let mut header = lead.to_owned();
    if let Some(agent_type) = setting("agent_type") {
        header.push_str(&format!(" ({agent_type})"));
    }
    match (setting("model"), setting("reasoning_effort")) {
        (Some(model), Some(effort)) => header.push_str(&format!(" · {model} ({effort})")),
        (Some(model), None) => header.push_str(&format!(" · {model}")),
        (None, Some(effort)) => header.push_str(&format!(" · {effort}")),
        (None, None) => {}
    }
    if let Some(fork) = fork_part(setting("fork_turns")) {
        header.push_str(&format!(" · {fork}"));
    }
    header
}

/// Codex's `fork_turns`: `none`, `all` (also when missing or empty), or a
/// count of the most recent turns.
fn fork_part(fork_turns: Option<&str>) -> Option<String> {
    match fork_turns.map(str::trim).unwrap_or_default() {
        "none" => None,
        "" | "all" => Some("fork".to_owned()),
        "1" => Some("fork last 1 turn".to_owned()),
        turns if turns.chars().all(|c| c.is_ascii_digit()) => {
            Some(format!("fork last {turns} turns"))
        }
        other => Some(format!("fork {other}")),
    }
}

/// Every launch setting the header does not carry, as `key: value`, then a
/// readable prompt.
fn launch_body(launch: &serde_json::Map<String, Value>, prompt: Option<&str>) -> Option<ToolBody> {
    let mut lines: Vec<String> = launch
        .iter()
        .filter(|(key, _)| !LAUNCH_HEADER_KEYS.contains(&key.as_str()))
        .map(|(key, value)| match value {
            Value::String(text) => format!("{key}: {text}"),
            other => format!("{key}: {other}"),
        })
        .collect();
    lines.extend(prompt.map(str::to_owned));
    (!lines.is_empty()).then(|| ToolBody::plain(lines.join("\n")))
}

fn is_delivered_report(input: &Value) -> bool {
    input.get(DELIVERED_REPORT_KEY).and_then(Value::as_bool) == Some(true)
}

/// The header value of a wait for a set time, `1m 30s`, or of a wait on the
/// process `session_id` names, with its typed text when that is one line and
/// its count of lines otherwise. `None` for a wait on anything else, such as
/// an agent.
fn wait_header(input: &Value) -> Option<String> {
    if let Some(duration) = wait_duration(input, "duration_ms") {
        return Some(format_duration(duration));
    }
    let session_id = string_field(input, "session_id")?;
    let mut header = match typed_lines(input).as_deref() {
        Some([line]) => format!("send to session {session_id}: {line}"),
        Some(lines) => format!("send to session {session_id} · {} lines", lines.len()),
        None => format!("poll session {session_id}"),
    };
    if let Some(wait) = wait_duration(input, "wait_ms") {
        header.push_str(&format!(" · up to {}", format_duration(wait)));
    }
    Some(header)
}

/// Typed text of two or more lines, one row per line; one line shows in
/// the header.
fn typed_lines_body(input: &Value) -> Option<ToolBody> {
    typed_lines(input)
        .filter(|lines| lines.len() > 1)
        .map(|lines| ToolBody::plain(lines.join("\n")))
}

fn wait_duration(input: &Value, key: &str) -> Option<Duration> {
    input
        .get(key)
        .and_then(Value::as_u64)
        .map(Duration::from_millis)
}

/// The lines of a wait's typed `input`; `None` for a wait that doesn't type.
fn typed_lines(input: &Value) -> Option<Vec<String>> {
    string_field(input, "input").map(shown_typed_lines)
}

/// Text typed into a process, split at each line break: a line feed, or a
/// carriage return with or without one, shown as `⏎` at its line's end. An
/// ASCII control character shows in caret notation, as `^C`; any other
/// control character as its escape, as `\u{9b}`.
fn shown_typed_lines(typed: &str) -> Vec<String> {
    let mut lines = vec![String::new()];
    let mut characters = typed.chars().peekable();
    while let Some(character) = characters.next() {
        let line = lines.last_mut().expect("the text starts a line");
        match character {
            '\r' | '\n' => {
                if character == '\r' {
                    characters.next_if_eq(&'\n');
                }
                line.push('⏎');
                if characters.peek().is_some() {
                    lines.push(String::new());
                }
            }
            '\u{7f}' => line.push_str("^?"),
            control if control.is_ascii_control() => {
                line.push('^');
                line.push(char::from(control as u8 + 0x40));
            }
            control if control.is_control() => line.extend(control.escape_unicode()),
            other => line.push(other),
        }
    }
    lines
}

/// `new task from /root`, or `from /root` without a kind, then
/// ` · text encrypted by Codex` when the text is unreadable.
fn received_message_header(input: &Value) -> String {
    let sender = string_field(input, "sender").unwrap_or("");
    let mut header = match string_field(input, "kind") {
        Some(kind) => format!("{kind} from {sender}"),
        None => format!("from {sender}"),
    };
    if let Some(agent) = string_field(input, "encrypted_by") {
        header.push_str(&format!(" · text encrypted by {agent}"));
    }
    header
}

/// The tools a script calls, or `script` when it doesn't call a tool, then
/// its settings: `update_plan, exec_command · yields after 1m · output ≤
/// 14,000 tokens`.
fn script_header(input: &Value) -> String {
    let calls = called_tools(input);
    let mut header = if calls.is_empty() {
        "script".to_owned()
    } else {
        calls.join(", ")
    };
    if let Some(yield_after_ms) = input.get("yield_after_ms").and_then(Value::as_u64) {
        let duration = format_duration(Duration::from_millis(yield_after_ms));
        header.push_str(&format!(" · yields after {duration}"));
    }
    if let Some(tokens) = input.get("max_output_tokens").and_then(Value::as_u64) {
        let tokens = with_thousands_separators(tokens);
        header.push_str(&format!(" · output ≤ {tokens} tokens"));
    }
    header
}

fn called_tools(input: &Value) -> Vec<&str> {
    input
        .get("calls")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect()
}

struct TaskStep<'a> {
    mark: char,
    text: &'a str,
}

/// The steps of a task list, each marked by its status; `None` unless
/// `steps` lists a text and a known status for each.
fn task_list_steps(input: &Value) -> Option<Vec<TaskStep<'_>>> {
    input
        .get("steps")?
        .as_array()?
        .iter()
        .map(|step| {
            let mark = match string_field(step, "status")? {
                "completed" => '✓',
                "in_progress" => '▸',
                "pending" => '·',
                _ => return None,
            };
            Some(TaskStep {
                mark,
                text: string_field(step, "text")?,
            })
        })
        .collect()
}

/// The tools of the script that updated the task list; for a direct call,
/// without `calls`, the tool's name alone, without the `name: ` prefix.
fn task_list_header(name: &str, input: &Value) -> ToolCallHeader {
    let calls = called_tools(input);
    if calls.is_empty() {
        ToolCallHeader::named(String::new(), name.to_owned())
    } else {
        ToolCallHeader::named(name_prefix(name), calls.join(", "))
    }
}

/// The explanation, then each step after its mark.
fn task_list_body(input: &Value, steps: &[TaskStep<'_>]) -> Option<ToolBody> {
    let explanation = string_field(input, "explanation").filter(|text| !text.trim().is_empty());
    let lines: Vec<String> = explanation
        .map(str::to_owned)
        .into_iter()
        .chain(
            steps
                .iter()
                .map(|step| format!("{} {}", step.mark, step.text)),
        )
        .collect();
    (!lines.is_empty()).then(|| ToolBody::plain(lines.join("\n")))
}

/// `14000` as `14,000`.
fn with_thousands_separators(count: u64) -> String {
    let digits = count.to_string();
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    grouped
}

/// The first command's first line is the header's value and its later lines
/// the body; each later command follows in the same form. With a width,
/// every line wraps at the width less the prefix.
fn format_shell(header: ToolCallHeader, max_width: usize) -> FormattedToolCall {
    let available_width = max_width.saturating_sub(header.prefix.chars().count());
    let first = format_command(&header.value, available_width);
    FormattedToolCall {
        prefix: header.prefix,
        value: first.value,
        value_is_first_line: true,
        body: first.body,
        later_commands: header
            .later_commands
            .iter()
            .map(|command| format_command(command, available_width))
            .collect(),
    }
}

fn format_command(command: &str, available_width: usize) -> FormattedCommand {
    let mut rows = command
        .lines()
        .flat_map(|line| wrap_shell_line(line, available_width));
    let value = rows.next().unwrap_or_default();
    let later_rows: Vec<String> = rows.collect();
    FormattedCommand {
        value,
        body: (!later_rows.is_empty()).then(|| ToolBody::plain(later_rows.join("\n"))),
    }
}

fn wrap_shell_line(line: &str, width: usize) -> Vec<String> {
    if width == 0 || line.chars().count() <= width {
        return vec![line.to_owned()];
    }
    textwrap::wrap(line, width)
        .into_iter()
        .map(|row| row.into_owned())
        .collect()
}

fn read_header(name: &str, input: &Value) -> ToolCallHeader {
    let file_path = string_field(input, "file_path").unwrap_or("");
    let offset = input.get("offset").and_then(Value::as_u64);
    let limit = input.get("limit").and_then(Value::as_u64);

    let value = match (offset, limit) {
        (Some(offset), Some(limit)) => {
            format!("{file_path}:{offset}-{}", offset + limit)
        }
        (Some(offset), None) => format!("{file_path}:{offset}"),
        _ => file_path.to_owned(),
    };

    ToolCallHeader::named(name_prefix(name), value)
}

fn grep_header(name: &str, input: &Value) -> ToolCallHeader {
    let pattern = string_field(input, "pattern").unwrap_or("");
    let path = string_field(input, "path");
    let glob = string_field(input, "glob");

    let location = match (path, glob) {
        (Some(path), Some(glob)) => format!("{path}/{glob}"),
        (Some(path), None) => path.to_string(),
        (None, Some(glob)) => glob.to_string(),
        (None, None) => ".".to_string(),
    };

    ToolCallHeader::named(name_prefix(name), format!("\"{pattern}\" in {location}"))
}

fn glob_header(name: &str, input: &Value) -> ToolCallHeader {
    let pattern = string_field(input, "pattern").unwrap_or("");

    let value = match string_field(input, "path") {
        Some(path) => format!("{pattern} in {path}"),
        None => pattern.to_owned(),
    };

    ToolCallHeader::named(name_prefix(name), value)
}

fn edit_body(input: &Value) -> Option<ToolBody> {
    match (
        string_field(input, "old_string"),
        string_field(input, "new_string"),
    ) {
        (Some(old), Some(new)) => Some(ToolBody::diff(replacement_diff(old, new))),
        _ => string_field(input, "patch").map(|patch| ToolBody::diff(patch.to_owned())),
    }
}

fn web_search_header(name: &str, input: &Value) -> ToolCallHeader {
    match string_field(input, "query").filter(|query| !query.is_empty()) {
        Some(query) => ToolCallHeader::named(name_prefix(name), format!("\"{query}\"")),
        None => ToolCallHeader::named(name.to_owned(), String::new()),
    }
}

fn raw_input_body(input: &Value) -> Option<ToolBody> {
    serde_json::to_string_pretty(input)
        .ok()
        .map(ToolBody::plain)
}

/// `input` in the lines [`raw_input_body`] lays it out in, unindented, each
/// key and string written between its quotes as its characters: a line break
/// as a line break, and `\"` as `"`. A query then matches the word after a
/// line break as a word's start.
/// Indentation would make the list's context line differ between a cached
/// session, whose texts keep runs of whitespace, and a parsed one, whose
/// texts collapse them.
fn unindented_json_with_decoded_strings(input: &Value) -> String {
    let mut json = String::new();
    push_unindented_json(input, &mut json);
    json
}

fn push_unindented_json(value: &Value, json: &mut String) {
    match value {
        Value::String(text) => push_quoted(text, json),
        Value::Array(items) if !items.is_empty() => {
            json.push('[');
            for (index, item) in items.iter().enumerate() {
                json.push_str(separator_before_item(index));
                push_unindented_json(item, json);
            }
            json.push_str("\n]");
        }
        Value::Object(fields) if !fields.is_empty() => {
            json.push('{');
            for (index, (key, field)) in fields.iter().enumerate() {
                json.push_str(separator_before_item(index));
                push_quoted(key, json);
                json.push_str(": ");
                push_unindented_json(field, json);
            }
            json.push_str("\n}");
        }
        scalar_or_empty => json.push_str(&scalar_or_empty.to_string()),
    }
}

/// The line break before item `index` of an array or object, with the comma
/// that ends the item before it.
fn separator_before_item(index: usize) -> &'static str {
    if index == 0 { "\n" } else { ",\n" }
}

fn push_quoted(text: &str, json: &mut String) {
    json.push('"');
    json.push_str(text);
    json.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn body_text(call: &FormattedToolCall) -> Option<&str> {
        call.body.as_ref().map(|body| body.text.as_str())
    }

    fn body_kind(call: &FormattedToolCall) -> Option<ToolBodyKind> {
        call.body.as_ref().map(|body| body.kind)
    }

    #[test]
    fn call_without_its_header_field_shows_its_name_and_input() {
        let input = json!({"input": "{\"filePath\":\"C:\\src\\lib.rs\"}"});
        let result = format_tool_call("read", Tool::Read, &input, 80);
        assert_eq!(result.header(), "read:");
        assert_eq!(
            body_text(&result),
            Some(serde_json::to_string_pretty(&input).unwrap().as_str())
        );
        assert_eq!(body_kind(&result), Some(ToolBodyKind::Plain));
    }

    #[test]
    fn every_layout_falls_back_without_its_header_field() {
        for tool in [
            Tool::Shell,
            Tool::Script,
            Tool::Read,
            Tool::Edit,
            Tool::Write,
            Tool::Grep,
            Tool::Glob,
            Tool::Skill,
            Tool::Agent,
            Tool::AgentMessage,
            Tool::ReceivedMessage,
            Tool::AgentReport,
            Tool::WebFetch,
            Tool::WebSearch,
        ] {
            let result = format_tool_call("name", tool, &json!({"other": 1}), 80);
            assert_eq!(result.header(), "name:", "{tool:?}");
        }
    }

    #[test]
    fn test_format_task() {
        let input = json!({
            "subagent_type": "Explore",
            "description": "Find the bug",
            "prompt": "Look for issues in the code"
        });
        let result = format_tool_call("Task", Tool::Agent, &input, 80);
        assert_eq!(result.header(), "Task (Explore): Find the bug");
        assert_eq!(body_text(&result), Some("Look for issues in the code"));
        assert_eq!(body_kind(&result), Some(ToolBodyKind::Plain));
    }

    #[test]
    fn agent_without_subagent_type_omits_parentheses() {
        let input = json!({
            "description": "classifier_state_machine",
            "prompt": "Classify every state."
        });
        let result = format_tool_call("spawn_agent", Tool::Agent, &input, 80);
        assert_eq!(result.header(), "spawn_agent: classifier_state_machine");
    }

    fn spawn_agent_header(launch: Value) -> String {
        let input = json!({"description": "scout", "launch": launch});
        format_tool_call("spawn_agent", Tool::Agent, &input, 80).header()
    }

    #[test]
    fn spawn_agent_header_carries_its_type_model_effort_and_forked_turns() {
        assert_eq!(
            spawn_agent_header(json!({
                "agent_type": "suite_runner",
                "model": "gpt-5.6-sol",
                "reasoning_effort": "high",
                "fork_turns": "3",
            })),
            "spawn_agent: scout (suite_runner) · gpt-5.6-sol (high) · fork last 3 turns"
        );
        assert_eq!(
            spawn_agent_header(json!({"model": "gpt-5.6-sol", "fork_turns": "none"})),
            "spawn_agent: scout · gpt-5.6-sol"
        );
        assert_eq!(
            spawn_agent_header(json!({"reasoning_effort": "xhigh", "fork_turns": "1"})),
            "spawn_agent: scout · xhigh · fork last 1 turn"
        );
    }

    #[test]
    fn spawn_agent_call_with_all_or_no_forked_turns_reads_fork() {
        assert_eq!(
            spawn_agent_header(json!({"fork_turns": "all"})),
            "spawn_agent: scout · fork"
        );
        assert_eq!(spawn_agent_header(json!({})), "spawn_agent: scout · fork");
    }

    #[test]
    fn unrecognized_fork_value_shows_after_fork() {
        assert_eq!(
            spawn_agent_header(json!({"fork_turns": "recent"})),
            "spawn_agent: scout · fork recent"
        );
    }

    #[test]
    fn spawn_agent_calls_readable_prompt_follows_its_other_launch_settings() {
        let input = json!({
            "description": "scout",
            "prompt": "List the modules.",
            "launch": {"fork_turns": "none", "service_tier": "flex"},
        });
        let result = format_tool_call("spawn_agent", Tool::Agent, &input, 80);
        assert_eq!(
            body_text(&result),
            Some("service_tier: flex\nList the modules.")
        );
    }

    #[test]
    fn spawn_agent_calls_other_launch_settings_list_under_its_header() {
        let input = json!({
            "description": "scout",
            "launch": {"fork_turns": "none", "service_tier": "flex", "depth": 2},
        });
        let result = format_tool_call("spawn_agent", Tool::Agent, &input, 80);
        assert_eq!(result.header(), "spawn_agent: scout");
        assert_eq!(body_text(&result), Some("depth: 2\nservice_tier: flex"));
    }

    const LAUNCH_PROMPT: &str = "Find where the session cache decides a copy is stale.";

    /// `launch` carries a `fork_turns`, as the Codex reader writes for every
    /// launch without a task name.
    fn launch_without_task_name(launch: Value) -> FormattedToolCall {
        let input = json!({"prompt": LAUNCH_PROMPT, "launch": launch});
        format_tool_call("spawn_agent", Tool::Agent, &input, 80)
    }

    #[test]
    fn spawn_agent_header_without_task_name_carries_its_type_model_effort_and_fork() {
        let headers = [
            (
                json!({
                    "agent_type": "explorer",
                    "model": "gpt-5.6-sol",
                    "reasoning_effort": "high",
                    "fork_turns": "all",
                }),
                "spawn_agent (explorer) · gpt-5.6-sol (high) · fork",
            ),
            (
                json!({"agent_type": "explorer", "fork_turns": "none"}),
                "spawn_agent (explorer)",
            ),
            (
                json!({"model": "gpt-5.6-sol", "fork_turns": "none"}),
                "spawn_agent · gpt-5.6-sol",
            ),
            (
                json!({"reasoning_effort": "high", "fork_turns": "none"}),
                "spawn_agent · high",
            ),
            (json!({"fork_turns": "all"}), "spawn_agent · fork"),
            (json!({"fork_turns": "none"}), "spawn_agent"),
        ];
        for (launch, header) in headers {
            let call = launch_without_task_name(launch.clone());
            assert_eq!(call.header(), header, "{launch} reads as `{header}`");
            assert_eq!(
                body_text(&call),
                Some(LAUNCH_PROMPT),
                "the body of {launch} is its prompt alone"
            );
        }
    }

    #[test]
    fn spawn_agent_without_task_name_shows_its_items_in_place_of_a_prompt() {
        let input = json!({"launch": {
            "agent_type": "explorer",
            "fork_turns": "all",
            "items": [{"type": "text", "text": "Read the cache."}],
        }});
        let call = format_tool_call("spawn_agent", Tool::Agent, &input, 80);
        assert_eq!(call.header(), "spawn_agent (explorer) · fork");
        assert_eq!(
            body_text(&call),
            Some(r#"items: [{"text":"Read the cache.","type":"text"}]"#)
        );
    }

    #[test]
    fn spawn_agent_with_fork_context_not_boolean_shows_it_in_body() {
        let call = launch_without_task_name(json!({"fork_context": "yes", "fork_turns": "none"}));
        assert_eq!(call.header(), "spawn_agent");
        assert_eq!(
            body_text(&call),
            Some(format!("fork_context: yes\n{LAUNCH_PROMPT}").as_str())
        );
    }

    #[test]
    fn skill_names_skill_and_carries_its_arguments() {
        let input = json!({"skill": "write-commit-messages", "args": "amend the last commit"});
        let result = format_tool_call("Skill", Tool::Skill, &input, 80);
        assert_eq!(result.header(), "Skill: write-commit-messages");
        assert_eq!(body_text(&result), Some("amend the last commit"));

        let result = format_tool_call("skill", Tool::Skill, &json!({"skill": "pdf-tools"}), 80);
        assert_eq!(result.header(), "skill: pdf-tools");
        assert!(result.body.is_none());
    }

    #[test]
    fn agent_message_names_recipient_and_carries_message() {
        let input = json!({"recipient": "worker-1", "message": "How far along are you?"});
        let result = format_tool_call("SendMessage", Tool::AgentMessage, &input, 80);
        assert_eq!(result.header(), "SendMessage: worker-1");
        assert_eq!(body_text(&result), Some("How far along are you?"));
    }

    #[test]
    fn received_message_names_its_kind_and_sender_and_carries_readable_text() {
        let input = json!({"kind": "new task", "sender": "/root", "message": "Map the callers."});
        let result = format_tool_call("inter_agent_message", Tool::ReceivedMessage, &input, 80);
        assert_eq!(result.header(), "inter_agent_message: new task from /root");
        assert_eq!(body_text(&result), Some("Map the callers."));
    }

    #[test]
    fn encrypted_received_message_is_one_header_line() {
        let input = json!({"kind": "message", "sender": "/root/scout", "encrypted_by": "Codex"});
        let result = format_tool_call("inter_agent_message", Tool::ReceivedMessage, &input, 80);
        assert_eq!(
            result.header(),
            "inter_agent_message: message from /root/scout · text encrypted by Codex"
        );
        assert_eq!(body_text(&result), None);
    }

    #[test]
    fn received_message_without_kind_names_its_sender_alone() {
        let input = json!({"sender": "/root", "message": "Map the callers."});
        let result = format_tool_call("inter_agent_message", Tool::ReceivedMessage, &input, 80);
        assert_eq!(result.header(), "inter_agent_message: from /root");
    }

    fn header_of_script(input: Value) -> String {
        format_tool_call("exec", Tool::Script, &input, 80).header()
    }

    #[test]
    fn script_header_names_its_tools_then_each_setting_it_ran_under() {
        let source = "text(1);";
        assert_eq!(
            header_of_script(json!({"source": source, "calls": ["update_plan", "exec_command"]})),
            "exec: update_plan, exec_command"
        );
        assert_eq!(
            header_of_script(json!({"source": source, "calls": [], "yield_after_ms": 90_000})),
            "exec: script · yields after 1m 30s"
        );
        assert_eq!(
            header_of_script(
                json!({"source": source, "calls": ["mcp__ide__find"], "max_output_tokens": 1_800})
            ),
            "exec: mcp__ide__find · output ≤ 1,800 tokens"
        );
        assert_eq!(
            header_of_script(json!({
                "source": source,
                "calls": ["update_plan"],
                "yield_after_ms": 60_000,
                "max_output_tokens": 14_000,
            })),
            "exec: update_plan · yields after 1m · output ≤ 14,000 tokens"
        );
    }

    #[test]
    fn script_body_is_its_source_with_its_line_breaks() {
        let source = "const r = await tools.exec_command({ cmd });\ntext(r.output);";
        let result = format_tool_call(
            "exec",
            Tool::Script,
            &json!({"source": source, "calls": ["exec_command"]}),
            80,
        );
        assert_eq!(body_text(&result), Some(source));
        assert_eq!(body_kind(&result), Some(ToolBodyKind::Plain));
    }

    #[test]
    fn token_counts_group_their_digits_in_threes() {
        for (count, grouped) in [
            (0, "0"),
            (999, "999"),
            (1_000, "1,000"),
            (14_000, "14,000"),
            (1_234_567, "1,234,567"),
        ] {
            assert_eq!(with_thousands_separators(count), grouped);
        }
    }

    #[test]
    fn wait_for_set_time_shows_its_duration_without_body() {
        let result = format_tool_call("sleep", Tool::Wait, &json!({"duration_ms": 90_000}), 80);
        assert_eq!(result.header(), "sleep: 1m 30s");
        assert_eq!(body_text(&result), None);
    }

    #[test]
    fn wait_on_process_polls_or_sends_its_input_for_up_to_its_wait() {
        for (input, header) in [
            (
                json!({"session_id": "24279", "wait_ms": 50_000}),
                "exec: poll session 24279 · up to 50s",
            ),
            (json!({"session_id": "24279"}), "exec: poll session 24279"),
            (
                json!({"session_id": "72867", "input": "\u{3}", "wait_ms": 10_000}),
                "exec: send to session 72867: ^C · up to 10s",
            ),
            (
                json!({"session_id": "5", "input": "y\n"}),
                "exec: send to session 5: y⏎",
            ),
        ] {
            let result = format_tool_call("exec", Tool::Wait, &input, 80);
            assert_eq!(result.header(), header);
            assert_eq!(body_text(&result), None, "{header} is one line");
        }
    }

    #[test]
    fn typed_text_of_several_lines_shows_one_row_per_line_under_its_count() {
        let input = json!({
            "session_id": "5",
            "input": "import json\ndata = json.load(open(\"a.json\"))\r\nprint(len(data))\rexit()\n",
            "wait_ms": 10_000,
        });
        for name in ["exec", "write_stdin"] {
            let result = format_tool_call(name, Tool::Wait, &input, 80);
            assert_eq!(
                result.header(),
                format!("{name}: send to session 5 · 4 lines · up to 10s")
            );
            assert_eq!(
                body_text(&result),
                Some(
                    "import json⏎\ndata = json.load(open(\"a.json\"))⏎\nprint(len(data))⏎\nexit()⏎"
                )
            );
            assert_eq!(body_kind(&result), Some(ToolBodyKind::Plain));
        }
        let last_line_unended = json!({"session_id": "5", "input": "a\nb"});
        let result = format_tool_call("exec", Tool::Wait, &last_line_unended, 80);
        assert_eq!(result.header(), "exec: send to session 5 · 2 lines");
        assert_eq!(body_text(&result), Some("a⏎\nb"));
    }

    #[test]
    fn task_list_shows_its_explanation_then_each_step_marked_by_its_status() {
        let input = json!({
            "explanation": "Two read-only agents run in parallel.",
            "steps": [
                {"text": "Read every source", "status": "completed"},
                {"text": "Trace the transport", "status": "in_progress"},
                {"text": "Write the report", "status": "pending"},
            ],
            "calls": ["update_plan"],
        });
        let result = format_tool_call("exec", Tool::TaskList, &input, 80);
        assert_eq!(result.header(), "exec: update_plan");
        assert_eq!(
            body_text(&result),
            Some(
                "Two read-only agents run in parallel.\n✓ Read every source\n▸ Trace the transport\n· Write the report"
            )
        );
    }

    #[test]
    fn task_list_without_calls_is_named_by_its_tool_alone() {
        let input = json!({"steps": [{"text": "Ship", "status": "pending"}]});
        let result = format_tool_call("update_plan", Tool::TaskList, &input, 80);
        assert_eq!(result.prefix, "");
        assert_eq!(result.value, "update_plan");
        assert_eq!(body_text(&result), Some("· Ship"));

        let empty = format_tool_call("update_plan", Tool::TaskList, &json!({"steps": []}), 80);
        assert_eq!(empty.header(), "update_plan");
        assert_eq!(body_text(&empty), None);
    }

    #[test]
    fn task_list_without_steps_of_known_status_shows_its_name_and_input() {
        for input in [
            json!({"todos": [{"content": "Ship", "status": "pending"}]}),
            json!({"steps": [{"text": "Ship", "status": "blocked"}]}),
            json!({"steps": [{"status": "pending"}]}),
        ] {
            let result = format_tool_call("TodoWrite", Tool::TaskList, &input, 80);
            assert_eq!(
                result.header(),
                "TodoWrite:",
                "the header names the tool alone for {input}"
            );
            assert_eq!(
                body_text(&result),
                Some(serde_json::to_string_pretty(&input).unwrap().as_str()),
                "the body shows the raw input {input}"
            );
        }
    }

    #[test]
    fn typed_text_ends_each_line_with_return_and_shows_other_control_characters_as_caret_or_escape()
    {
        for (typed, lines) in [
            ("/exit\n", &["/exit⏎"][..]),
            ("y\r\nn\r", &["y⏎", "n⏎"]),
            ("\u{1b}[A\t\u{7f}", &["^[[A^I^?"]),
            ("\u{9b}2J\u{85}", &["\\u{9b}2J\\u{85}"]),
            ("ö", &["ö"]),
        ] {
            assert_eq!(
                shown_typed_lines(typed),
                lines,
                "{typed:?} shows as {lines:?}"
            );
        }
    }

    #[test]
    fn wait_on_something_else_shows_its_name_and_input() {
        let input = json!({"agent_ids": ["scout"]});
        let result = format_tool_call("wait_agent", Tool::Wait, &input, 80);
        assert_eq!(result.header(), "wait_agent:");
        assert_eq!(
            body_text(&result),
            Some(serde_json::to_string_pretty(&input).unwrap().as_str())
        );
    }

    #[test]
    fn question_call_counts_its_questions_and_lists_each_title_and_option() {
        let input = json!({"questions": [
            {"title": "Is the firmware session still running?", "options": ["Yes", "No"]},
        ]});
        let result = format_tool_call("request_user_input_async", Tool::UserQuestion, &input, 80);
        assert_eq!(result.header(), "request_user_input_async: 1 question");
        assert_eq!(
            body_text(&result),
            Some("Is the firmware session still running?\n  • Yes\n  • No")
        );
    }

    #[test]
    fn several_questions_are_numbered_and_one_without_options_shows_its_title_alone() {
        let input = json!({"questions": [
            {"title": "Which profile?", "options": ["warm idle", "cold start"]},
            {"title": "Keep recording\nonce warm?"},
        ]});
        let result = format_tool_call("request_user_input_async", Tool::UserQuestion, &input, 80);
        assert_eq!(result.header(), "request_user_input_async: 2 questions");
        assert_eq!(
            body_text(&result),
            Some(
                "1. Which profile?\n  • warm idle\n  • cold start\n\n2. Keep recording\n   once warm?"
            )
        );
    }

    #[test]
    fn question_call_without_titled_questions_shows_its_name_and_input() {
        for input in [
            json!({"questions": []}),
            json!({"questions": [{"options": ["Yes"]}]}),
            json!({"arguments": "not json"}),
        ] {
            let result =
                format_tool_call("request_user_input_async", Tool::UserQuestion, &input, 80);
            assert_eq!(result.header(), "request_user_input_async:", "{input}");
            assert_eq!(
                body_text(&result),
                Some(serde_json::to_string_pretty(&input).unwrap().as_str()),
                "{input}"
            );
        }
    }

    #[test]
    fn agent_report_carries_report_as_text() {
        let report = "Review done.\n\n1. **Fix.** The title \"names\" the bug.";
        let result = format_tool_call(
            "SubagentHandback",
            Tool::AgentReport,
            &json!({ "message": report }),
            80,
        );
        assert_eq!(result.header(), "SubagentHandback:");
        assert_eq!(body_text(&result), Some(report));
    }

    #[test]
    fn delivered_agent_report_is_one_header_line() {
        let result = format_tool_call(
            "SubagentHandback",
            Tool::AgentReport,
            &json!({ DELIVERED_REPORT_KEY: true }),
            80,
        );
        assert_eq!(result.header(), "SubagentHandback: report delivered");
        assert_eq!(body_text(&result), None);
    }

    #[test]
    fn test_format_bash() {
        let input = json!({
            "command": "git status",
            "description": "Check repo status"
        });
        let result = format_tool_call("Bash", Tool::Shell, &input, 80);
        assert_eq!(result.header(), "Bash: git status");
        assert!(result.body.is_none());
    }

    #[test]
    fn header_opens_with_provider_name() {
        let input = json!({"command": "git status"});
        let result = format_tool_call("PowerShell", Tool::Shell, &input, 80);
        assert_eq!(result.header(), "PowerShell: git status");
        let result = format_tool_call("exec_command", Tool::Shell, &input, 80);
        assert_eq!(result.header(), "exec_command: git status");
    }

    #[test]
    fn test_format_bash_wrapping() {
        let long_command = "cargo build --release --features 'feature1 feature2 feature3' --target x86_64-unknown-linux-gnu";
        let input = json!({
            "command": long_command
        });
        // With width 40, command should wrap (available width is 40 - 6 = 34 for command text)
        let result = format_tool_call("Bash", Tool::Shell, &input, 40);
        assert!(result.header().starts_with("Bash: cargo"));
        assert!(
            result.body.is_some(),
            "Long command should have body for continuation"
        );
    }

    #[test]
    fn test_format_bash_no_wrap_when_fits() {
        let input = json!({
            "command": "ls -la"
        });
        let result = format_tool_call("Bash", Tool::Shell, &input, 80);
        assert_eq!(result.header(), "Bash: ls -la");
        assert!(result.body.is_none());
    }

    #[test]
    fn test_format_bash_multiline_command_has_body() {
        let input = json!({
            "command": "one\ntwo"
        });
        let result = format_tool_call("Bash", Tool::Shell, &input, 80);
        assert_eq!(result.header(), "Bash: one");
        assert_eq!(body_text(&result), Some("two"));
    }

    #[test]
    fn test_format_bash_wraps_long_lines_in_multiline_command() {
        let input = json!({
            "command": "alpha beta gamma\nnext"
        });
        let result = format_tool_call("Bash", Tool::Shell, &input, 12);
        assert_eq!(result.header(), "Bash: alpha");
        assert_eq!(body_text(&result), Some("beta\ngamma\nnext"));
    }

    #[test]
    fn no_wrap_keeps_shell_commands_lines_whole() {
        let long_line = "cargo ".repeat(40);
        let input = json!({"command": format!("{long_line}\nnext")});
        let result = format_tool_call("Bash", Tool::Shell, &input, NO_WRAP);
        assert_eq!(result.prefix, "Bash: ");
        assert_eq!(result.value, long_line);
        assert!(result.value_is_first_line);
        assert_eq!(body_text(&result), Some("next"));
    }

    #[test]
    fn web_search_with_empty_query_shows_tool_name_alone() {
        let result = format_tool_call("web_search", Tool::WebSearch, &json!({"query": ""}), 80);
        assert_eq!(result.header(), "web_search");

        let result = format_tool_call(
            "web_search",
            Tool::WebSearch,
            &json!({"query": "rust regex"}),
            80,
        );
        assert_eq!(result.header(), r#"web_search: "rust regex""#);
    }

    #[test]
    fn header_splits_into_its_prefix_and_value() {
        let input = json!({"file_path": "/src/main.rs", "offset": 100, "limit": 50});
        let result = format_tool_call("Read", Tool::Read, &input, 80);
        assert_eq!(result.prefix, "Read: ");
        assert_eq!(result.value, "/src/main.rs:100-150");
        assert!(!result.value_is_first_line);

        let input = json!({"subagent_type": "Explore", "description": "Find the bug"});
        let result = format_tool_call("Task", Tool::Agent, &input, 80);
        assert_eq!(result.prefix, "Task (Explore): ");
        assert_eq!(result.value, "Find the bug");

        let result = format_tool_call("ExitPlanMode", Tool::Other, &json!({}), 80);
        assert_eq!(result.prefix, "ExitPlanMode:");
        assert_eq!(result.value, "");
    }

    #[test]
    fn glob_write_reference_web_fetch_and_user_shell_calls_show_their_header_and_body() {
        let call = |name, tool, input: Value| {
            let formatted = format_tool_call(name, tool, &input, NO_WRAP);
            (formatted.header(), body_text(&formatted).map(str::to_owned))
        };

        assert_eq!(
            call(
                "Glob",
                Tool::Glob,
                json!({"pattern": "*.rs", "path": "src"})
            ),
            ("Glob: *.rs in src".to_owned(), None)
        );
        assert_eq!(
            call("Glob", Tool::Glob, json!({"pattern": "*.rs"})),
            ("Glob: *.rs".to_owned(), None)
        );
        assert_eq!(
            call(
                "Write",
                Tool::Write,
                json!({"file_path": "src/lib.rs", "content": "x"})
            ),
            ("Write: src/lib.rs".to_owned(), None)
        );
        assert_eq!(
            call(
                "Reference",
                Tool::FileReference,
                json!({"file_path": "src/lib.rs"})
            ),
            ("Reference: src/lib.rs".to_owned(), None)
        );
        assert_eq!(
            call(
                "WebFetch",
                Tool::WebFetch,
                json!({"url": "https://example.com", "prompt": "Summarise it"})
            ),
            (
                "WebFetch: https://example.com".to_owned(),
                Some("Summarise it".to_owned())
            )
        );
        assert_eq!(
            call(
                "bash",
                Tool::UserShell,
                json!({"command": "cd src\ncargo test"})
            ),
            ("ran cd src".to_owned(), Some("cargo test".to_owned()))
        );
    }

    #[test]
    fn shell_calls_header_alone_holds_its_whole_command() {
        let input = json!({"command": "cd src\ncargo test", "description": "Test"});

        let header = tool_call_header("Bash", Tool::Shell, &input);

        assert_eq!(header.prefix, "Bash: ");
        assert_eq!(header.value, "cd src\ncargo test");
        assert!(header.value_is_command);
    }

    fn several_commands() -> Value {
        json!({"commands": ["git status --short", "cd src\ncargo test", "rg -n TODO src"]})
    }

    #[test]
    fn shell_call_of_several_commands_header_contains_each_whole_in_order() {
        let header = tool_call_header("exec", Tool::Shell, &several_commands());

        assert_eq!(header.prefix, "exec: ");
        assert_eq!(
            header.commands().collect::<Vec<_>>(),
            ["git status --short", "cd src\ncargo test", "rg -n TODO src"]
        );
    }

    #[test]
    fn shell_call_of_several_commands_formats_each_after_prefix_with_its_own_later_rows() {
        let result = format_tool_call("exec", Tool::Shell, &several_commands(), 20);

        assert_eq!(result.header(), "exec: git status");
        assert_eq!(body_text(&result), Some("--short"));
        let later: Vec<_> = result
            .later_commands
            .iter()
            .map(|command| {
                (
                    command.value.as_str(),
                    command.body.as_ref().map(|body| body.text.as_str()),
                )
            })
            .collect();
        assert_eq!(
            later,
            [("cd src", Some("cargo test")), ("rg -n TODO src", None)]
        );
        assert_eq!(
            result.text(),
            "exec: git status\n--short\nexec: cd src\ncargo test\nexec: rg -n TODO src",
            "the export text shows each command after the prefix, its later rows under it"
        );
    }

    #[test]
    fn shell_call_with_commands_not_a_list_of_strings_shows_its_name_and_input() {
        for input in [
            json!({"commands": []}),
            json!({"commands": ["ls", 1]}),
            json!({"commands": "ls", "command": "ls"}),
        ] {
            let result = format_tool_call("exec", Tool::Shell, &input, NO_WRAP);
            assert_eq!(result.header(), "exec:", "{input} shows as its input");
            assert!(result.later_commands.is_empty());
        }
    }

    #[test]
    fn summary_counts_shell_call_once_per_command_and_other_calls_once() {
        assert_eq!(counted_calls(Tool::Shell, &several_commands()), 3);
        assert_eq!(counted_calls(Tool::Shell, &json!({"command": "ls"})), 1);
        assert_eq!(
            counted_calls(
                Tool::Read,
                &json!({"file_path": "a.rs", "commands": ["a", "b"]})
            ),
            1
        );
    }

    #[test]
    fn test_format_read_with_range() {
        let input = json!({
            "file_path": "/src/main.rs",
            "offset": 100,
            "limit": 50
        });
        let result = format_tool_call("Read", Tool::Read, &input, 80);
        assert_eq!(result.header(), "Read: /src/main.rs:100-150");
    }

    #[test]
    fn test_format_grep() {
        let input = json!({
            "pattern": "fn main",
            "path": "src",
            "glob": "*.rs"
        });
        let result = format_tool_call("Grep", Tool::Grep, &input, 80);
        assert_eq!(result.header(), "Grep: \"fn main\" in src/*.rs");
    }

    #[test]
    fn directory_listing_shows_its_path_or_working_directory() {
        let listing = format_tool_call(
            "list_dir",
            Tool::DirectoryListing,
            &json!({"path": "/repo/src", "depth": 2}),
            80,
        );
        assert_eq!(listing.header(), "list_dir: /repo/src");
        assert_eq!(body_text(&listing), None);

        let working_directory = format_tool_call("ls", Tool::DirectoryListing, &json!({}), 80);
        assert_eq!(working_directory.header(), "ls: .");
    }

    #[test]
    fn test_format_edit() {
        let input = json!({
            "file_path": "/src/lib.rs",
            "old_string": "old code",
            "new_string": "new code"
        });
        let result = format_tool_call("Edit", Tool::Edit, &input, 80);
        assert_eq!(result.header(), "Edit: /src/lib.rs");
        assert_eq!(body_text(&result), Some("-old code\n+new code"));
        assert_eq!(body_kind(&result), Some(ToolBodyKind::Diff));
    }

    #[test]
    fn edit_with_patch_keeps_it_as_diff_body() {
        let input = json!({
            "file_path": "src/lib.rs",
            "patch": "@@ -1,2 +1,2 @@\n-old\n+new"
        });
        let result = format_tool_call("apply_patch", Tool::Edit, &input, 80);
        assert_eq!(result.header(), "apply_patch: src/lib.rs");
        assert_eq!(body_text(&result), Some("@@ -1,2 +1,2 @@\n-old\n+new"));
        assert_eq!(body_kind(&result), Some(ToolBodyKind::Diff));
    }

    #[test]
    fn unmapped_tool_falls_back_to_its_name_and_input() {
        let input = json!({"plan": "- step one"});
        let result = format_tool_call("ExitPlanMode", Tool::Other, &input, 80);
        assert_eq!(result.header(), "ExitPlanMode:");
        assert_eq!(body_kind(&result), Some(ToolBodyKind::Plain));
        assert!(
            body_text(&result)
                .unwrap()
                .contains("\"plan\": \"- step one\"")
        );
    }

    #[test]
    fn search_reads_input_shown_as_json_without_escapes_as_viewers_lines_unindented() {
        let input = json!({
            "todos": [{"content": "ship it", "done": false, "note": null, "priority": 2}],
            "merge": true,
            "labels": [],
            "settings": {},
        });
        let viewers_lines_unindented = raw_input_body(&input)
            .unwrap()
            .text
            .lines()
            .map(str::trim_start)
            .collect::<Vec<_>>()
            .join("\n");

        assert_eq!(
            ToolCallBody::RawInput(&input).searched(),
            viewers_lines_unindented
        );
    }

    #[test]
    fn search_reads_each_string_escape_of_input_shown_as_json_as_its_character() {
        let input = json!({"plan": "Read \"the\" grammar\nRewrite C:\\src\tnow\u{1b}"});

        assert_eq!(
            ToolCallBody::RawInput(&input).searched(),
            "{\n\"plan\": \"Read \"the\" grammar\nRewrite C:\\src\tnow\u{1b}\"\n}"
        );
    }

    #[test]
    fn only_diff_bodies_have_signed_lines() {
        assert_eq!(ToolBodyKind::Plain.diff_side("- a markdown bullet"), None);
        assert_eq!(ToolBodyKind::Plain.diff_side("+1 for that"), None);
        assert_eq!(
            ToolBodyKind::Diff.diff_side("+added"),
            Some(DiffSide::Added)
        );
        assert_eq!(
            ToolBodyKind::Diff.diff_side("-removed"),
            Some(DiffSide::Removed)
        );
        assert_eq!(ToolBodyKind::Diff.diff_side(" context"), None);
        assert_eq!(ToolBodyKind::Diff.diff_side("@@ -1 +1 @@"), None);
    }
}
