//! Codex's tool calls mapped onto the canonical [`Tool`] set.
//!
//! `exec` runs a JavaScript script in which tool calls read `tools.<name>(…)`.
//! [`script_scan`] finds those calls outside comments and literals; the
//! source is not parsed further. A layout reads literal values out of a
//! call's argument text by key, whether the argument is JSON
//! (`{"command":"…"}`) or a bare object literal (`{ cmd: "…" }`), and reads
//! a literal, a `String.raw` template included, only when it is the
//! property's whole value.
//! - A script whose two or more calls are all shell commands shows as one
//!   shell call of every command.
//! - The first call names the layout of a shell command, an edit, a web
//!   search or fetch, an agent launch or message, or an image read.
//! - A poll, typed input, a plan update and a patch in a variable need the
//!   script's one call, at depth 0, and arguments known before it runs.
//!
//! Any other script is a [`Tool::Script`]: its source and the tools it
//! calls.

mod script_scan;

use crate::log_entry::{ContentBlock, Tool};
use regex::Regex;
use script_scan::{ScriptCall, ScriptScan};
use serde_json::{Map, Value, json};
use std::sync::LazyLock;

pub(super) fn tool_use_blocks(call_id: &str, name: &str, input: &Value) -> Vec<ContentBlock> {
    canonical_calls(name, input)
        .into_iter()
        .enumerate()
        .map(|(index, call)| ContentBlock::ToolUse {
            id: if index == 0 {
                call_id.to_owned()
            } else {
                format!("{call_id}#{}", index + 1)
            },
            name: name.to_owned(),
            tool: call.tool,
            input: call.input,
        })
        .collect()
}

/// The call that puts questions to the user and returns before they answer.
const QUESTION_CALL: &str = "request_user_input_async";

struct CanonicalCall {
    tool: Tool,
    input: Value,
}

fn canonical_calls(name: &str, input: &Value) -> Vec<CanonicalCall> {
    let input = &without_encrypted_payloads(input);
    let calls = match name {
        "exec" => input.as_str().map(script_calls),
        "apply_patch" => patch_text(input).map(patch_edits),
        "run" | "web__run" => Some(web_calls(input)),
        "sleep" => sleep_call(input).map(|call| vec![call]),
        "write_stdin" => stdin_call(input)
            .or_else(|| function_call(name, input))
            .map(|call| vec![call]),
        "update_plan" => plan_update(input)
            .or_else(|| function_call(name, input))
            .map(|call| vec![call]),
        // Codex's arguments are the canonical input; a call they don't fit
        // shows its name and input.
        QUESTION_CALL => Some(vec![CanonicalCall {
            tool: Tool::UserQuestion,
            input: input.clone(),
        }]),
        _ => function_call(name, input).map(|call| vec![call]),
    };
    calls.filter(|calls| !calls.is_empty()).unwrap_or_else(|| {
        vec![CanonicalCall {
            tool: Tool::Other,
            input: input.clone(),
        }]
    })
}

struct ToolMapping {
    tool: Tool,
    header_keys: &'static [(&'static str, &'static str)],
}

fn tool_mapping(name: &str) -> ToolMapping {
    let (tool, header_keys): (Tool, &'static [(&'static str, &'static str)]) = match name {
        "shell_command" => (Tool::Shell, &[("command", "command")]),
        "exec_command" => (Tool::Shell, &[("cmd", "command")]),
        "write_stdin" | "wait" | "wait_agent" => (Tool::Wait, &[]),
        "spawn_agent" => (
            Tool::Agent,
            &[("task_name", "description"), ("message", "prompt")],
        ),
        "send_message" | "followup_task" => (
            Tool::AgentMessage,
            &[("target", "recipient"), ("message", "message")],
        ),
        "send_input" => (
            Tool::AgentMessage,
            &[("id", "recipient"), ("message", "message")],
        ),
        "update_plan" => (Tool::TaskList, &[]),
        "view_image" => (Tool::Read, &[("path", "file_path")]),
        _ => (Tool::Other, &[]),
    };
    ToolMapping { tool, header_keys }
}

fn function_call(name: &str, input: &Value) -> Option<CanonicalCall> {
    let mapping = tool_mapping(name);
    if mapping.tool == Tool::Other {
        return None;
    }
    let mut input = input.clone();
    // Every `spawn_agent` call is an agent launch, including a launch without
    // a task name whose prompt is `items` and not `message`.
    let is_launch = name == "spawn_agent" && input.is_object();
    if let Some(object) = input.as_object_mut() {
        for (codex_key, canonical_key) in mapping.header_keys {
            if let Some(value) = object.remove(*codex_key) {
                object.insert((*canonical_key).to_owned(), value);
            }
        }
        if is_launch {
            gather_launch_settings(object);
        }
    }
    (is_launch || has_header_string(mapping.header_keys, &input)).then_some(CanonicalCall {
        tool: mapping.tool,
        input,
    })
}

/// A `write_stdin` call as a wait on the process its `session_id` names,
/// after typing its `chars`, for up to its `yield_time_ms`. `None` unless
/// the process's `session_id` is a number or text, `chars` is text, and
/// `yield_time_ms` a whole number; null or missing `chars` and
/// `yield_time_ms` are left out.
fn stdin_call(arguments: &Value) -> Option<CanonicalCall> {
    let session_id = match arguments.get("session_id")? {
        Value::Number(number) => number.to_string(),
        Value::String(text) => text.clone(),
        _ => return None,
    };
    let chars = match arguments.get("chars") {
        None | Some(Value::Null) => "",
        Some(chars) => chars.as_str()?,
    };
    let wait_ms = match arguments.get("yield_time_ms") {
        None | Some(Value::Null) => None,
        Some(wait_ms) => Some(wait_ms.as_u64()?),
    };
    Some(process_wait(session_id, chars, wait_ms))
}

/// A wait on the process `session_id` names, for up to `wait_ms`, after
/// typing `chars`. Without `chars`, the wait polls the process for output.
fn process_wait(session_id: String, chars: &str, wait_ms: Option<u64>) -> CanonicalCall {
    let mut input = json!({ "session_id": session_id });
    if !chars.is_empty() {
        input["input"] = Value::from(chars);
    }
    if let Some(wait_ms) = wait_ms {
        input["wait_ms"] = Value::from(wait_ms);
    }
    CanonicalCall {
        tool: Tool::Wait,
        input,
    }
}

/// The statuses Codex accepts for a plan's step.
const PLAN_STATUSES: [&str; 3] = ["pending", "in_progress", "completed"];

/// An `update_plan` call as its explanation and steps. `None` unless `plan`
/// is a list, each step has a text `step` and one of [`PLAN_STATUSES`], and
/// `explanation` is text, null or missing.
fn plan_update(arguments: &Value) -> Option<CanonicalCall> {
    let steps = arguments
        .get("plan")?
        .as_array()?
        .iter()
        .map(|step| plan_step(step.get("step")?.as_str()?, step.get("status")?.as_str()?))
        .collect::<Option<Vec<_>>>()?;
    let explanation = match arguments.get("explanation") {
        None | Some(Value::Null) => None,
        Some(explanation) => Some(explanation.as_str()?),
    };
    Some(task_list(steps, explanation))
}

fn plan_step(text: &str, status: &str) -> Option<Value> {
    PLAN_STATUSES
        .contains(&status)
        .then(|| json!({ "text": text, "status": status }))
}

fn task_list(steps: Vec<Value>, explanation: Option<&str>) -> CanonicalCall {
    let mut input = json!({ "steps": steps });
    if let Some(explanation) = explanation {
        input["explanation"] = Value::from(explanation);
    }
    CanonicalCall {
        tool: Tool::TaskList,
        input,
    }
}

/// A `sleep` call as a wait for its `duration_ms`; `None` unless that is a
/// non-negative integer.
fn sleep_call(input: &Value) -> Option<CanonicalCall> {
    input
        .get("duration_ms")
        .is_some_and(Value::is_u64)
        .then(|| CanonicalCall {
            tool: Tool::Wait,
            input: input.clone(),
        })
}

/// Move every `spawn_agent` argument but the task name and the message under
/// `launch`, where the agent header reads the type, model, effort and
/// `fork_turns`.
fn gather_launch_settings(object: &mut Map<String, Value>) {
    let has_task_name = object.contains_key("description");
    let settings: Vec<String> = object
        .keys()
        .filter(|key| !matches!(key.as_str(), "description" | "prompt"))
        .cloned()
        .collect();
    let mut launch: Map<String, Value> = settings
        .iter()
        .filter_map(|key| object.remove_entry(key))
        .collect();
    if !has_task_name {
        write_fork_context_as_fork_turns(&mut launch);
    }
    object.insert("launch".to_owned(), Value::Object(launch));
}

/// Replace the boolean `fork_context` of a launch without a task name with
/// the `fork_turns` of a launch with a task name: `true` forks every turn,
/// `false` or missing none. Keep a `fork_context` that is not a boolean for
/// the body to show: Codex rejects that call, and the value shows why.
fn write_fork_context_as_fork_turns(launch: &mut Map<String, Value>) {
    let fork_context = launch.get("fork_context").and_then(Value::as_bool);
    if fork_context.is_some() {
        launch.remove("fork_context");
    }
    let fork_turns = if fork_context == Some(true) {
        "all"
    } else {
        "none"
    };
    launch.insert("fork_turns".to_owned(), Value::from(fork_turns));
}

/// The arguments without their encrypted values. Codex encrypts the message
/// one agent sends another, and the recipient receives it encrypted too, so
/// no local copy is readable. Only top-level values are checked.
fn without_encrypted_payloads(input: &Value) -> Value {
    let mut input = input.clone();
    if let Some(object) = input.as_object_mut() {
        object.retain(|_, value| !value.as_str().is_some_and(is_encrypted));
    }
    input
}

/// Codex stores an encrypted message as a Fernet token, whose version byte
/// and leading timestamp bytes always encode as `gAAAAA`.
fn is_encrypted(text: &str) -> bool {
    text.starts_with("gAAAAA")
}

fn has_header_string(header_keys: &[(&str, &str)], input: &Value) -> bool {
    header_keys.is_empty()
        || header_keys
            .iter()
            .any(|(_, canonical_key)| input.get(canonical_key).is_some_and(Value::is_string))
}

fn script_calls(input: &str) -> Vec<CanonicalCall> {
    let script = ExecScript::parse(input);
    let scan = script_scan::scan(script.source);
    layout(&scan)
        .filter(|layout| !layout.is_empty())
        .unwrap_or_else(|| vec![script.into_call(&scan.calls)])
}

/// The calls a script's layout shows; `None` for a script without one. A
/// script of several shell commands shows them all; otherwise the first
/// call names the layout. A wait on a process, a plan update and a patch in
/// a variable get their layouts only as the script's one call, at depth 0,
/// in a script that doesn't name `tools` anywhere else.
fn layout(scan: &ScriptScan<'_>) -> Option<Vec<CanonicalCall>> {
    if let Some(commands) = several_shell_commands(scan) {
        return Some(vec![commands]);
    }
    let call = scan.calls.first()?;
    let is_only_call = scan.calls.len() == 1 && call.depth == 0 && !scan.names_tools_otherwise;
    match call.name {
        "apply_patch" => script_patch(call.rest_of_script)
            .or_else(|| {
                if is_only_call {
                    patch_in_variable(scan, call)
                } else {
                    None
                }
            })
            .map(|patch| patch_edits(&patch)),
        "run" | "web__run" => script_web_call(call.rest_of_script).map(|call| vec![call]),
        "write_stdin" if is_only_call => {
            script_stdin_call(call.rest_of_script).map(|call| vec![call])
        }
        "update_plan" if is_only_call => {
            script_plan_update(call.rest_of_script).map(|call| vec![call])
        }
        name => script_function_call(name, call.rest_of_script).map(|call| vec![call]),
    }
}

/// One shell call containing the command of each shell call in the source,
/// in source order. `None` unless the script has two or more calls, each a
/// shell command whose object literal argument sets the command to a
/// literal that isn't blank, and doesn't name `tools` anywhere else.
fn several_shell_commands(scan: &ScriptScan<'_>) -> Option<CanonicalCall> {
    if scan.calls.len() < 2 || scan.names_tools_otherwise {
        return None;
    }
    let commands = scan
        .calls
        .iter()
        .map(|call| {
            let arguments = leading_object(call.rest_of_script)?;
            property_string(arguments, shell_command_key(call.name)?)
                .filter(|command| !command.trim().is_empty())
        })
        .collect::<Option<Vec<_>>>()?;
    Some(CanonicalCall {
        tool: Tool::Shell,
        input: json!({ "commands": commands }),
    })
}

/// The key of the command a shell call's arguments name; `None` for a call
/// that isn't a shell command.
fn shell_command_key(name: &str) -> Option<&'static str> {
    let mapping = tool_mapping(name);
    if mapping.tool != Tool::Shell {
        return None;
    }
    mapping
        .header_keys
        .iter()
        .find(|(_, canonical_key)| *canonical_key == "command")
        .map(|(codex_key, _)| *codex_key)
}

static PLAN_ARRAY_START: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?:^|[^\w"'`])["']?plan["']?\s*:\s*\["#).unwrap());

/// The plan update a script's `update_plan` call makes, read from its object
/// literal as [`plan_update`] reads a direct call's arguments; its input
/// names `update_plan` as the tool the script called. `None` unless `plan`
/// is an array literal of object literals, each with `step` and `status`
/// string literals, and `explanation` is a string literal or missing.
fn script_plan_update(rest_of_script: &str) -> Option<CanonicalCall> {
    let arguments = leading_object(rest_of_script)?;
    let array_start = PLAN_ARRAY_START.find(arguments)?.end() - 1;
    let array = bracketed(&arguments[array_start..], '[', ']')?;
    if !ends_value(&arguments[array_start + array.len()..]) {
        return None;
    }
    let known_text = |object: &str, key: &str| match string_argument(object, key) {
        Argument::Known(text) => Some(text),
        Argument::Missing | Argument::Unknown => None,
    };
    let steps = object_literals(array)?
        .into_iter()
        .map(|step| plan_step(&known_text(step, "step")?, &known_text(step, "status")?))
        .collect::<Option<Vec<_>>>()?;
    let explanation = string_argument(arguments, "explanation")
        .map(Some)
        .or_missing(|| None)?;
    let mut plan_update = task_list(steps, explanation.as_deref());
    plan_update.input["calls"] = json!(["update_plan"]);
    Some(plan_update)
}

/// The object literals `array`, an array literal, lists in order; `None`
/// when it lists anything else.
fn object_literals(array: &str) -> Option<Vec<&str>> {
    let mut rest = &array[1..array.len() - 1];
    let mut objects = Vec::new();
    loop {
        rest = rest.trim_start();
        if rest.is_empty() {
            return Some(objects);
        }
        let object = bracketed(rest, '{', '}')?;
        objects.push(object);
        rest = rest[object.len()..].trim_start();
        rest = rest.strip_prefix(',').unwrap_or(rest);
    }
}

/// The wait a script's `write_stdin` call makes, read from its object
/// literal as [`stdin_call`] reads a direct call's arguments. `None` unless
/// `session_id` is a number literal, and `chars` and `yield_time_ms` are
/// literals or missing.
fn script_stdin_call(rest_of_script: &str) -> Option<CanonicalCall> {
    let arguments = leading_object(rest_of_script)?;
    let Argument::Known(session_id) = number_argument(arguments, "session_id") else {
        return None;
    };
    let chars = string_argument(arguments, "chars").or_missing(String::new)?;
    let wait_ms = number_argument(arguments, "yield_time_ms")
        .map(Some)
        .or_missing(|| None)?;
    Some(process_wait(session_id.to_string(), &chars, wait_ms))
}

/// Codex's `exec` input split as Codex splits it: an optional control
/// comment on the first line, `// @exec:` then a JSON object of settings,
/// and the source after it.
struct ExecScript<'a> {
    source: &'a str,
    yield_time_ms: Option<u64>,
    max_output_tokens: Option<u64>,
}

const CONTROL_COMMENT_PREFIX: &str = "// @exec:";
/// The largest integer JavaScript holds exactly. Codex rejects a setting
/// above it.
const MAX_SAFE_INTEGER: u64 = (1 << 53) - 1;

impl<'a> ExecScript<'a> {
    /// A control comment Codex rejects stays in the source: Codex refused
    /// that script, and its result shows why.
    fn parse(input: &'a str) -> Self {
        Self::with_control_comment(input).unwrap_or(Self {
            source: input,
            yield_time_ms: None,
            max_output_tokens: None,
        })
    }

    /// `None` unless the first line is a control comment Codex accepts: a
    /// JSON object of `yield_time_ms` and `max_output_tokens`, each a safe
    /// integer or null, followed by source.
    fn with_control_comment(input: &'a str) -> Option<Self> {
        let (first_line, source) = input.split_once('\n')?;
        let settings = first_line
            .trim_start()
            .strip_prefix(CONTROL_COMMENT_PREFIX)?;
        if source.trim().is_empty() {
            return None;
        }
        let settings: Map<String, Value> = serde_json::from_str(settings.trim()).ok()?;
        let mut script = Self {
            source,
            yield_time_ms: None,
            max_output_tokens: None,
        };
        for (key, value) in settings {
            let value = match value {
                Value::Null => None,
                number => Some(number.as_u64().filter(|value| *value <= MAX_SAFE_INTEGER)?),
            };
            match key.as_str() {
                "yield_time_ms" => script.yield_time_ms = value,
                "max_output_tokens" => script.max_output_tokens = value,
                _ => return None,
            }
        }
        Some(script)
    }

    /// The script as its source, the tools `calls` name in order of first
    /// use, and the control comment's settings.
    fn into_call(self, calls: &[ScriptCall<'_>]) -> CanonicalCall {
        let mut called: Vec<&str> = Vec::new();
        for call in calls {
            if !called.contains(&call.name) {
                called.push(call.name);
            }
        }
        let mut input = json!({ "source": self.source, "calls": called });
        if let Some(yield_time_ms) = self.yield_time_ms {
            input["yield_after_ms"] = Value::from(yield_time_ms);
        }
        if let Some(max_output_tokens) = self.max_output_tokens {
            input["max_output_tokens"] = Value::from(max_output_tokens);
        }
        CanonicalCall {
            tool: Tool::Script,
            input,
        }
    }
}

/// A call whose header reads strings out of its argument; `None` for a tool
/// without such a header, such as a wait.
fn script_function_call(name: &str, rest_of_script: &str) -> Option<CanonicalCall> {
    let mapping = tool_mapping(name);
    if mapping.tool == Tool::Other || mapping.header_keys.is_empty() {
        return None;
    }
    let input: Map<String, Value> = mapping
        .header_keys
        .iter()
        .filter_map(|(codex_key, canonical_key)| {
            property_string(rest_of_script, codex_key)
                .map(|value| ((*canonical_key).to_owned(), Value::String(value)))
        })
        .collect();
    (!input.is_empty()).then_some(CanonicalCall {
        tool: mapping.tool,
        input: Value::Object(input),
    })
}

fn patch_text(input: &Value) -> Option<&str> {
    input
        .as_str()
        .or_else(|| input.get("input").and_then(Value::as_str))
}

fn script_patch(rest_of_script: &str) -> Option<String> {
    leading_string_literal(rest_of_script).or_else(|| property_string(rest_of_script, "input"))
}

static VARIABLE_ARGUMENT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*(\w+)\s*\)").unwrap());

static INITIALISER: LazyLock<Regex> = LazyLock::new(|| {
    let string_literal = string_literal_pattern();
    Regex::new(&format!(r"(?s)^\s*=\s*({string_literal})")).unwrap()
});

/// The patch of `apply_patch(patch)`, from `const`, `let` or `var patch =`
/// and a string literal before the call. `None` unless the script declares
/// the name once, at depth 0, and doesn't assign it again, and the literal
/// is the whole initialiser, its text known before the script runs.
fn patch_in_variable(scan: &ScriptScan<'_>, call: &ScriptCall<'_>) -> Option<String> {
    let variable = VARIABLE_ARGUMENT
        .captures(call.rest_of_script)?
        .get(1)?
        .as_str();
    let mut declarations = scan
        .declarations
        .iter()
        .filter(|declaration| declaration.name == variable);
    let declaration = declarations.next()?;
    if declarations.next().is_some()
        || declaration.depth != 0
        || declaration.offset > call.offset
        || scan.assigned.contains(&variable)
    {
        return None;
    }
    let literal = INITIALISER.captures(declaration.rest_of_script)?.get(1)?;
    if !ends_initialiser(&declaration.rest_of_script[literal.end()..]) {
        return None;
    }
    static_text(literal.as_str())
}

/// True when the text after a declaration's literal ends its initialiser:
/// after whitespace and comments, the script ends, `;` or `,` follows, or a
/// line break and then a name, which starts the next statement.
fn ends_initialiser(after_literal: &str) -> bool {
    let rest = after_whitespace_and_comments(after_literal);
    let crosses_line = after_literal[..after_literal.len() - rest.len()].contains('\n');
    rest.is_empty()
        || rest.starts_with([';', ','])
        || (crosses_line
            && rest
                .starts_with(|first: char| first.is_alphabetic() || first == '_' || first == '$'))
}

const FILE_SECTION_MARKERS: [&str; 3] =
    ["*** Update File: ", "*** Add File: ", "*** Delete File: "];
const PATCH_END: &str = "*** End Patch";

fn patch_edits(patch: &str) -> Vec<CanonicalCall> {
    let mut edits = Vec::new();
    let mut section: Option<FileSection<'_>> = None;
    for line in patch.lines() {
        if line == PATCH_END {
            break;
        }
        if let Some(path) = file_section_path(line) {
            edits.extend(section.take().map(FileSection::into_edit));
            section = Some(FileSection {
                marker: line,
                path,
                hunk: Vec::new(),
            });
        } else if let Some(section) = &mut section {
            section.hunk.push(line);
        }
    }
    edits.extend(section.map(FileSection::into_edit));
    edits
}

struct FileSection<'a> {
    marker: &'a str,
    path: &'a str,
    hunk: Vec<&'a str>,
}

impl FileSection<'_> {
    fn into_edit(self) -> CanonicalCall {
        let body = if self.hunk.is_empty() {
            self.marker.to_owned()
        } else {
            self.hunk.join("\n")
        };
        CanonicalCall {
            tool: Tool::Edit,
            input: json!({ "file_path": self.path, "patch": body }),
        }
    }
}

fn file_section_path(line: &str) -> Option<&str> {
    FILE_SECTION_MARKERS
        .iter()
        .find_map(|marker| line.strip_prefix(marker))
}

fn web_search(query: &str) -> CanonicalCall {
    CanonicalCall {
        tool: Tool::WebSearch,
        input: json!({ "query": query }),
    }
}

fn web_fetch(url: &str) -> CanonicalCall {
    CanonicalCall {
        tool: Tool::WebFetch,
        input: json!({ "url": url }),
    }
}

/// The call for a `web_search_call` item: a web search, an opened page, or a
/// search within a page.
pub(super) fn web_search_call_block(id: &str, action: Option<&Map<String, Value>>) -> ContentBlock {
    let text = |key: &str| {
        action
            .and_then(|action| action.get(key))
            .and_then(Value::as_str)
    };
    let call = match text("type") {
        Some("open_page") => web_fetch(text("url").unwrap_or_default()),
        Some("find_in_page") => web_search(&page_search_query(text("pattern"), text("url"))),
        _ => web_search(&searched_query(action)),
    };
    ContentBlock::ToolUse {
        id: id.to_owned(),
        name: "web_search".to_owned(),
        tool: call.tool,
        input: call.input,
    }
}

/// A search action's `query`, or its `queries` joined with `, ` when `query`
/// is empty, as Codex shows them.
fn searched_query(action: Option<&Map<String, Value>>) -> String {
    let field = |key: &str| action.and_then(|action| action.get(key));
    match field("query").and_then(Value::as_str) {
        Some(query) if !query.is_empty() => query.to_owned(),
        _ => field("queries")
            .and_then(Value::as_array)
            .map(|queries| {
                queries
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default(),
    }
}

/// A search within a page in Codex's words: `'<pattern>' in <url>`, or the
/// pattern or URL alone when the other is missing.
fn page_search_query(pattern: Option<&str>, url: Option<&str>) -> String {
    let pattern = pattern.filter(|pattern| !pattern.is_empty());
    let url = url.filter(|url| !url.is_empty());
    match (pattern, url) {
        (Some(pattern), Some(url)) => format!("'{pattern}' in {url}"),
        (Some(pattern), None) => format!("'{pattern}'"),
        (None, Some(url)) => url.to_owned(),
        (None, None) => String::new(),
    }
}

fn web_calls(input: &Value) -> Vec<CanonicalCall> {
    let searches = list_strings(input, "search_query", "q").map(web_search);
    let fetches = list_strings(input, "open", "ref_id").map(web_fetch);
    searches.chain(fetches).collect()
}

fn list_strings<'a>(input: &'a Value, list: &str, key: &'a str) -> impl Iterator<Item = &'a str> {
    input
        .get(list)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(move |item| item.get(key).and_then(Value::as_str))
}

fn script_web_call(rest_of_script: &str) -> Option<CanonicalCall> {
    if has_property(rest_of_script, "search_query")
        && let Some(query) = property_string(rest_of_script, "q")
    {
        return Some(web_search(&query));
    }
    if has_property(rest_of_script, "open")
        && let Some(url) = property_string(rest_of_script, "ref_id")
    {
        return Some(web_fetch(&url));
    }
    None
}

/// JavaScript keeps the text of a template with this tag as written,
/// backslashes included.
const RAW_TAG: &str = "String.raw";

/// The pattern of a quoted string, or of a template literal with or without
/// [`RAW_TAG`].
fn string_literal_pattern() -> String {
    let raw_tag = regex::escape(RAW_TAG);
    format!(r#""(?:[^"\\]|\\.)*"|'(?:[^'\\]|\\.)*'|(?:{raw_tag}\s*)?`(?:[^`\\]|\\.)*`"#)
}

static PROPERTY: LazyLock<Regex> = LazyLock::new(|| {
    let string_literal = string_literal_pattern();
    Regex::new(&format!(
        r#"(?s)(?:^|[^\w"'`])["']?(\w+)["']?\s*:\s*(?:({string_literal})|(\d+)\b)?"#
    ))
    .unwrap()
});

static LEADING_STRING_LITERAL: LazyLock<Regex> = LazyLock::new(|| {
    let string_literal = string_literal_pattern();
    Regex::new(&format!(r"(?s)^\s*({string_literal})")).unwrap()
});

struct Property<'a> {
    key: &'a str,
    /// The value when one literal is the whole of it: `"y"`, not
    /// `"y" + "\n"`.
    literal: Option<Literal<'a>>,
}

enum Literal<'a> {
    String(&'a str),
    /// A whole number written in decimal digits.
    Number(&'a str),
}

fn properties(object_text: &str) -> impl Iterator<Item = Property<'_>> {
    PROPERTY.captures_iter(object_text).map(|captures| {
        let literal = captures
            .get(2)
            .map(|literal| Literal::String(literal.as_str()))
            .or_else(|| {
                captures
                    .get(3)
                    .map(|literal| Literal::Number(literal.as_str()))
            })
            .filter(|_| ends_value(&object_text[captures.get_match().end()..]));
        Property {
            key: captures.get(1).map_or("", |key| key.as_str()),
            literal,
        }
    })
}

/// True when the text after a value's literal ends the value: `,`, `}` or
/// `]` follows, after any whitespace and comments.
fn ends_value(after_literal: &str) -> bool {
    after_whitespace_and_comments(after_literal).starts_with([',', '}', ']'])
}

fn after_whitespace_and_comments(text: &str) -> &str {
    let mut rest = text.trim_start();
    loop {
        if let Some(comment) = rest.strip_prefix("//") {
            rest = comment.find('\n').map_or("", |end| &comment[end..]);
        } else if let Some(comment) = rest.strip_prefix("/*") {
            rest = comment.find("*/").map_or("", |end| &comment[end + 2..]);
        } else {
            return rest;
        }
        rest = rest.trim_start();
    }
}

/// The first property `key` names in `object_text`.
fn property<'a>(object_text: &'a str, key: &str) -> Option<Property<'a>> {
    properties(object_text).find(|property| property.key == key)
}

/// A layout's argument: missing, a literal with its value known before the
/// script runs, or anything else.
enum Argument<T> {
    Missing,
    Known(T),
    Unknown,
}

impl<T> Argument<T> {
    fn map<U>(self, change: impl FnOnce(T) -> U) -> Argument<U> {
        match self {
            Self::Missing => Argument::Missing,
            Self::Known(value) => Argument::Known(change(value)),
            Self::Unknown => Argument::Unknown,
        }
    }

    /// The known value, or `missing` for a missing argument; `None` for an
    /// unknown one, which the layout doesn't show.
    fn or_missing(self, missing: impl FnOnce() -> T) -> Option<T> {
        match self {
            Self::Missing => Some(missing()),
            Self::Known(value) => Some(value),
            Self::Unknown => None,
        }
    }
}

fn string_argument(object_text: &str, key: &str) -> Argument<String> {
    match property(object_text, key) {
        None => Argument::Missing,
        Some(Property {
            literal: Some(Literal::String(literal)),
            ..
        }) => static_text(literal).map_or(Argument::Unknown, Argument::Known),
        Some(_) => Argument::Unknown,
    }
}

fn number_argument(object_text: &str, key: &str) -> Argument<u64> {
    match property(object_text, key) {
        None => Argument::Missing,
        Some(Property {
            literal: Some(Literal::Number(digits)),
            ..
        }) => digits.parse().map_or(Argument::Unknown, Argument::Known),
        Some(_) => Argument::Unknown,
    }
}

/// The text of a string literal; `None` for a template literal with a
/// substitution, whose text is known only when the script runs.
fn static_text(literal: &str) -> Option<String> {
    let is_template = literal.ends_with('`');
    let has_substitution = is_template && literal.contains("${");
    (!has_substitution).then(|| literal_text(literal))
}

/// The object literal `text` opens with, braces included; `None` when `text`
/// opens with anything else, or the object doesn't close.
fn leading_object(text: &str) -> Option<&str> {
    bracketed(text.trim_start(), '{', '}')
}

/// `text` from its first character, `open`, to the `close` that balances
/// it. Brackets inside string literals don't count.
fn bracketed(text: &str, open: char, close: char) -> Option<&str> {
    if !text.starts_with(open) {
        return None;
    }
    let mut depth = 0_usize;
    let mut open_quote: Option<char> = None;
    let mut is_escaped = false;
    for (index, character) in text.char_indices() {
        if let Some(quote) = open_quote {
            if is_escaped {
                is_escaped = false;
            } else if character == '\\' {
                is_escaped = true;
            } else if character == quote {
                open_quote = None;
            }
            continue;
        }
        if matches!(character, '"' | '\'' | '`') {
            open_quote = Some(character);
        } else if character == open {
            depth += 1;
        } else if character == close {
            depth -= 1;
            if depth == 0 {
                return Some(&text[..index + close.len_utf8()]);
            }
        }
    }
    None
}

fn has_property(object_text: &str, key: &str) -> bool {
    property(object_text, key).is_some()
}

/// The first `key`'s string literal, as written: a template literal keeps
/// its substitutions.
fn property_string(object_text: &str, key: &str) -> Option<String> {
    match property(object_text, key)?.literal? {
        Literal::String(literal) => Some(literal_text(literal)),
        Literal::Number(_) => None,
    }
}

/// The string literal a call's argument list, `text`, opens with, when it is
/// the whole first argument.
fn leading_string_literal(text: &str) -> Option<String> {
    let literal = LEADING_STRING_LITERAL.captures(text)?.get(1)?;
    after_whitespace_and_comments(&text[literal.end()..])
        .starts_with([')', ','])
        .then(|| literal_text(literal.as_str()))
}

/// The text of a string literal: a [`RAW_TAG`] template's text as written,
/// any other literal unescaped. A template literal keeps its substitutions
/// as written.
fn literal_text(literal: &str) -> String {
    let Some(raw_template) = literal.strip_prefix(RAW_TAG) else {
        return unescape_literal(literal);
    };
    let template = raw_template.trim_start();
    template[1..template.len() - 1].to_owned()
}

fn unescape_literal(literal: &str) -> String {
    let quote = &literal[..1];
    let raw = &literal[1..literal.len() - 1];
    // A template literal escapes its delimiters as `\`` and `\$`, which
    // `unescaper` would keep as written.
    let raw = if quote == "`" {
        raw.replace("\\`", "`").replace("\\$", "$")
    } else {
        raw.to_owned()
    };
    unescaper::unescape_lossy(&raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn calls(name: &str, input: Value) -> Vec<(Tool, Value, String)> {
        tool_use_blocks("call_1", name, &input)
            .into_iter()
            .map(|block| match block {
                ContentBlock::ToolUse {
                    id, tool, input, ..
                } => (tool, input, id),
                other => panic!("not a tool use: {other:?}"),
            })
            .collect()
    }

    fn single(name: &str, input: Value) -> (Tool, Value) {
        let mut calls = calls(name, input);
        assert_eq!(calls.len(), 1, "{name} became {calls:?}");
        let (tool, input, id) = calls.remove(0);
        assert_eq!(id, "call_1");
        (tool, input)
    }

    #[test]
    fn every_codex_function_call_lands_in_its_bucket() {
        let expected = [
            ("shell_command", json!({"command": "ls"}), Tool::Shell),
            ("exec_command", json!({"cmd": "ls"}), Tool::Shell),
            (
                "write_stdin",
                json!({"session_id": 1, "chars": ""}),
                Tool::Wait,
            ),
            ("wait", json!({"cell_id": "2"}), Tool::Wait),
            ("wait_agent", json!({"agent_ids": ["scout"]}), Tool::Wait),
            ("update_plan", json!({"plan": []}), Tool::TaskList),
            (
                "spawn_agent",
                json!({"task_name": "scout", "message": "Look around."}),
                Tool::Agent,
            ),
            (
                "send_message",
                json!({"target": "scout", "message": "status?"}),
                Tool::AgentMessage,
            ),
            (
                "followup_task",
                json!({"target": "scout", "message": "one more"}),
                Tool::AgentMessage,
            ),
            (
                "send_input",
                json!({"id": "019a0000-0000-7000-8000-000000000001", "message": "go on"}),
                Tool::AgentMessage,
            ),
            ("view_image", json!({"path": "shot.png"}), Tool::Read),
            ("list_agents", json!({}), Tool::Other),
            ("interrupt_agent", json!({"target": "scout"}), Tool::Other),
            ("request_user_input", json!({"questions": []}), Tool::Other),
            ("sleep", json!({"duration_ms": 60000}), Tool::Wait),
            (
                "request_user_input_async",
                json!({"questions": [{"title": "Ship it?", "options": ["Yes", "No"]}]}),
                Tool::UserQuestion,
            ),
            ("sleep", json!({"duration_ms": "1m"}), Tool::Other),
            ("sleep", json!({"duration_ms": 1.5}), Tool::Other),
            ("sleep", json!({"duration_ms": -1}), Tool::Other),
            ("ide__find_references", json!({"file": "a.rs"}), Tool::Other),
        ];
        for (name, input, tool) in expected {
            assert_eq!(single(name, input).0, tool, "{name}");
        }
    }

    #[test]
    fn codex_keys_are_renamed_to_canonical_ones_and_rest_kept() {
        assert_eq!(
            single(
                "exec_command",
                json!({"cmd": "git fetch", "workdir": "/repo"})
            )
            .1,
            json!({"command": "git fetch", "workdir": "/repo"})
        );
        assert_eq!(
            single(
                "spawn_agent",
                json!({"task_name": "scout", "message": "Look around.", "model": "gpt-5"})
            )
            .1,
            json!({"description": "scout", "prompt": "Look around.", "launch": {"model": "gpt-5"}})
        );
        assert_eq!(
            single(
                "send_message",
                json!({"target": "scout", "message": "status?"})
            )
            .1,
            json!({"recipient": "scout", "message": "status?"})
        );
        assert_eq!(
            single(
                "send_input",
                json!({"id": "019a0000-0000-7000-8000-000000000001", "message": "go on", "interrupt": false})
            )
            .1,
            json!({"recipient": "019a0000-0000-7000-8000-000000000001", "message": "go on", "interrupt": false})
        );
        assert_eq!(
            single("view_image", json!({"path": "shot.png", "detail": "high"})).1,
            json!({"file_path": "shot.png", "detail": "high"})
        );
    }

    #[test]
    fn write_stdin_waits_on_the_process_its_session_id_names_after_typing_its_chars() {
        assert_eq!(
            single(
                "write_stdin",
                json!({"session_id": 24279, "chars": "", "yield_time_ms": 50000, "max_output_tokens": 330})
            ),
            (Tool::Wait, json!({"session_id": "24279", "wait_ms": 50000}))
        );
        assert_eq!(
            single(
                "write_stdin",
                json!({"session_id": "72867", "chars": "\u{3}"})
            ),
            (Tool::Wait, json!({"session_id": "72867", "input": "\u{3}"}))
        );
        assert_eq!(
            single("write_stdin", json!({"session_id": 1})),
            (Tool::Wait, json!({"session_id": "1"}))
        );
    }

    #[test]
    fn write_stdin_without_session_id_or_with_chars_not_text_or_wait_not_whole_number_keeps_its_arguments()
     {
        for arguments in [
            json!({"chars": "y\n"}),
            json!({"session_id": 1, "chars": ["y"]}),
            json!({"session_id": 1, "yield_time_ms": "1s"}),
            json!({"session_id": 1, "yield_time_ms": 1.5}),
        ] {
            assert_eq!(
                single("write_stdin", arguments.clone()),
                (Tool::Wait, arguments)
            );
        }
    }

    #[test]
    fn spawn_agent_calls_launch_settings_gather_under_launch_and_its_encrypted_message_is_dropped()
    {
        let (tool, input) = single(
            "spawn_agent",
            json!({
                "task_name": "scout",
                "message": "gAAAAABqty3Zrroq3Hbl9fZU8LOihuAGjWTbFQX5",
                "agent_type": "suite_runner",
                "fork_turns": "all",
                "model": "gpt-5.6-sol",
                "reasoning_effort": "high",
                "service_tier": "flex",
            }),
        );
        assert_eq!(tool, Tool::Agent);
        assert_eq!(
            input,
            json!({
                "description": "scout",
                "launch": {
                    "agent_type": "suite_runner",
                    "fork_turns": "all",
                    "model": "gpt-5.6-sol",
                    "reasoning_effort": "high",
                    "service_tier": "flex",
                },
            })
        );
    }

    #[test]
    fn spawn_agent_call_without_task_name_or_message_is_agent_launch() {
        let items = json!([{"type": "text", "text": "Map the cache."}]);
        let (tool, input) = single(
            "spawn_agent",
            json!({"items": items, "agent_type": "explorer", "fork_context": true}),
        );
        assert_eq!(tool, Tool::Agent);
        assert_eq!(
            input,
            json!({"launch": {"items": items, "agent_type": "explorer", "fork_turns": "all"}})
        );
    }

    fn launch_settings_of(arguments: Value) -> Value {
        let (_, input) = single("spawn_agent", arguments);
        input["launch"].clone()
    }

    #[test]
    fn spawn_agent_without_task_name_and_with_fork_context_true_forks_every_turn() {
        assert_eq!(
            launch_settings_of(json!({"message": "Map the cache.", "fork_context": true})),
            json!({"fork_turns": "all"})
        );
    }

    #[test]
    fn spawn_agent_with_fork_context_false_or_missing_doesnt_fork() {
        for arguments in [
            json!({"message": "Map the cache.", "fork_context": false}),
            json!({"message": "Map the cache."}),
        ] {
            assert_eq!(
                launch_settings_of(arguments.clone()),
                json!({"fork_turns": "none"}),
                "{arguments} forks none of the turns"
            );
        }
    }

    #[test]
    fn spawn_agent_with_fork_context_not_boolean_keeps_it_and_doesnt_fork() {
        assert_eq!(
            launch_settings_of(json!({"message": "Map the cache.", "fork_context": "yes"})),
            json!({"fork_context": "yes", "fork_turns": "none"})
        );
    }

    #[test]
    fn spawn_agent_with_task_name_and_without_fork_turns_doesnt_gain_one() {
        assert_eq!(
            launch_settings_of(json!({"task_name": "scout", "message": "Map the cache."})),
            json!({}),
            "the launch keeps Codex's default of forking every turn"
        );
    }

    #[test]
    fn agent_messages_encrypted_message_is_dropped_and_its_recipient_kept() {
        for name in ["send_message", "followup_task"] {
            let (tool, input) = single(
                name,
                json!({"target": "scout", "message": "gAAAAABqtxzhqwvYEo_lDMZ8Ic5dMFpE"}),
            );
            assert_eq!(tool, Tool::AgentMessage, "{name}");
            assert_eq!(input, json!({"recipient": "scout"}), "{name}");
        }
    }

    #[test]
    fn agent_messages_readable_message_is_kept() {
        let (_, input) = single(
            "send_message",
            json!({"target": "scout", "message": "How far along are you?"}),
        );
        assert_eq!(
            input,
            json!({"recipient": "scout", "message": "How far along are you?"})
        );
    }

    #[test]
    fn call_without_strings_its_header_shows_stays_other() {
        let (tool, input) = single("shell_command", json!({"arguments": "not json"}));
        assert_eq!(tool, Tool::Other);
        assert_eq!(input, json!({"arguments": "not json"}));
    }

    #[test]
    fn apply_patch_edits_one_file_per_section() {
        let patch = "*** Begin Patch\n*** Update File: src/lib.rs\n*** Move to: src/lib2.rs\n@@ fn main\n-old\n+new\n*** Add File: NEW.md\n+# New\n*** Delete File: old.txt\n*** End Patch";

        let edits = calls("apply_patch", json!(patch));

        assert_eq!(
            edits.iter().map(|(tool, _, _)| *tool).collect::<Vec<_>>(),
            vec![Tool::Edit; 3]
        );
        assert_eq!(
            edits
                .iter()
                .map(|(_, _, id)| id.as_str())
                .collect::<Vec<_>>(),
            ["call_1", "call_1#2", "call_1#3"]
        );
        assert_eq!(
            edits[0].1,
            json!({"file_path": "src/lib.rs", "patch": "*** Move to: src/lib2.rs\n@@ fn main\n-old\n+new"})
        );
        assert_eq!(
            edits[1].1,
            json!({"file_path": "NEW.md", "patch": "+# New"})
        );
        assert_eq!(
            edits[2].1,
            json!({"file_path": "old.txt", "patch": "*** Delete File: old.txt"})
        );
    }

    #[test]
    fn apply_patch_as_function_call_reads_its_input_field() {
        let (tool, input) = single(
            "apply_patch",
            json!({"input": "*** Begin Patch\n*** Add File: a.txt\n+a\n*** End Patch"}),
        );
        assert_eq!(tool, Tool::Edit);
        assert_eq!(input, json!({"file_path": "a.txt", "patch": "+a"}));
    }

    #[test]
    fn patch_with_no_file_section_stays_other() {
        let (tool, input) = single("apply_patch", json!("*** Begin Patch\n*** End Patch"));
        assert_eq!(tool, Tool::Other);
        assert_eq!(input, json!("*** Begin Patch\n*** End Patch"));
    }

    #[test]
    fn web_run_searches_per_query_and_fetches_per_opened_reference() {
        let web = calls(
            "run",
            json!({
                "search_query": [{"q": "rust regex"}, {"q": "serde"}],
                "open": [{"ref_id": "https://docs.rs"}]
            }),
        );

        assert_eq!(
            web.iter()
                .map(|(tool, input, id)| (*tool, input.clone(), id.as_str()))
                .collect::<Vec<_>>(),
            vec![
                (Tool::WebSearch, json!({"query": "rust regex"}), "call_1"),
                (Tool::WebSearch, json!({"query": "serde"}), "call_1#2"),
                (
                    Tool::WebFetch,
                    json!({"url": "https://docs.rs"}),
                    "call_1#3"
                ),
            ]
        );
        let find = json!({"find": [{"ref_id": "turn0search0", "pattern": "install"}]});
        assert_eq!(single("run", find.clone()), (Tool::Other, find));
    }

    #[test]
    fn exec_script_is_named_by_its_first_call() {
        let json_quoted =
            json!(r#"await tools.shell_command({"command":"git status","workdir":"/repo"})"#);
        assert_eq!(
            single("exec", json_quoted),
            (Tool::Shell, json!({"command": "git status"}))
        );

        let object_literal = json!(
            "const result = await tools.exec_command({\n  cmd: \"printf 'a b'\",\n  yield_time_ms: 250,\n});\ntext(result.output);"
        );
        assert_eq!(
            single("exec", object_literal),
            (Tool::Shell, json!({"command": "printf 'a b'"}))
        );
    }

    #[test]
    fn exec_script_of_several_literal_shell_commands_is_one_call_of_every_command() {
        for (source, commands) in [
            (
                "const [s, l] = await Promise.all([\n  tools.exec_command({ cmd: \"git status --short\" }),\n  tools.exec_command({ cmd: 'git log -5 --oneline' }),\n]);",
                &["git status --short", "git log -5 --oneline"][..],
            ),
            (
                "await tools.exec_command({ cmd: \"first\" });\nawait tools.shell_command({ command: \"second\" });\nawait tools.exec_command({\"cmd\":\"third\",\"workdir\":\"/repo\"});",
                &["first", "second", "third"],
            ),
            (
                "await Promise.allSettled([tools.exec_command({cmd:String.raw`rg -n \"\\bTODO\\b\" src`}), tools.exec_command({cmd:`cargo test\n--lib`})]);",
                &[r#"rg -n "\bTODO\b" src"#, "cargo test\n--lib"],
            ),
            (
                "for (const dir of dirs) { await tools.exec_command({ cmd: \"ls\" }); }\nawait tools.exec_command({ cmd: \"ls\" });",
                &["ls", "ls"],
            ),
        ] {
            assert_eq!(
                single("exec", json!(source)),
                (Tool::Shell, json!({ "commands": commands })),
                "the script shows each command in source order"
            );
        }
    }

    #[test]
    fn exec_script_with_call_not_literal_shell_command_or_with_blank_command_is_named_by_its_first_call()
     {
        for source in [
            "await tools.exec_command({ cmd: \"first\" });\nawait tools.exec_command({ cmd: later });",
            "await tools.exec_command({ cmd: \"first\" });\nawait tools.exec_command({ cmd: `rg ${pattern}` + \" src\" });",
            "await tools.exec_command({ cmd: \"first\" });\nawait tools.view_image({ path: \"shot.png\" });",
            "await tools.exec_command({ cmd: \"first\" });\nawait tools.exec_command({ cmd: \"second\" });\nawait tools[name]({});",
            "await tools.exec_command({ cmd: \"first\" });\nawait tools.exec_command(options);\nawait tools.exec_command({ cmd: \"third\" });",
            "await tools.exec_command({ cmd: \"first\" });\nawait tools.exec_command({ cmd: \"\" });",
            "await tools.exec_command({ cmd: \"first\" });\nawait tools.exec_command({ cmd: \" \\n \" });",
        ] {
            assert_eq!(
                single("exec", json!(source)),
                (Tool::Shell, json!({"command": "first"})),
                "the script shows its first command alone"
            );
        }
        let blank_first =
            "await tools.exec_command({ cmd: \"\" });\nawait tools.exec_command({ cmd: \"ls\" });";
        assert_eq!(
            single("exec", json!(blank_first)),
            (Tool::Shell, json!({"command": ""})),
            "the script with a blank first command shows that command alone"
        );
    }

    /// A script shown as its tools and source, without control comment
    /// settings.
    fn script(source: &str, calls: &[&str]) -> (Tool, Value) {
        (Tool::Script, json!({"source": source, "calls": calls}))
    }

    #[test]
    fn exec_script_without_header_from_its_first_call_is_its_tools_and_source() {
        for (source, calls) in [
            ("echo TOOL_INPUT_VISIBLE", &[][..]),
            (
                "const now = await tools.clock__curr_time({});",
                &["clock__curr_time"],
            ),
            (
                "await tools.exec_command({ cmd: command })",
                &["exec_command"],
            ),
            ("await tools.exec_command(", &["exec_command"]),
            (
                "await tools.wait_agent({ agent_ids: [\"scout\"] })",
                &["wait_agent"],
            ),
        ] {
            assert_eq!(single("exec", json!(source)), script(source, calls));
        }
    }

    #[test]
    fn exec_script_names_its_tools_once_each_in_order_of_first_use() {
        let source = "const r = await Promise.allSettled([tools.update_plan({ plan: [] }), tools.exec_command({ cmd }), tools.update_plan({ plan: [] })]);\nawait tools.mcp__ide__find({ symbol: \"a\" });";
        assert_eq!(
            single("exec", json!(source)),
            script(source, &["update_plan", "exec_command", "mcp__ide__find"])
        );
    }

    #[test]
    fn exec_script_names_its_calls_outside_comments_and_literals() {
        for (source, calls) in [
            (
                "// await tools.exec_command({ cmd: \"git push\" })\ntext(\"not yet\");",
                &[][..],
            ),
            (
                "/* tools.update_plan({ plan: [] }) */ text('tools.exec_command({ cmd: \"ls\" })');",
                &[],
            ),
            (
                "text(`${await tools.mcp__ide__find({ symbol: \"a\" })} and tools.exec_command(`);",
                &["mcp__ide__find"],
            ),
            (
                "const r = await tools[\"mcp__ide__find\"]({ symbol: \"a\" });",
                &["mcp__ide__find"],
            ),
        ] {
            assert_eq!(
                single("exec", json!(source)),
                script(source, calls),
                "the header names {calls:?} alone"
            );
        }
    }

    #[test]
    fn tool_name_inside_another_word_isnt_a_call() {
        let source = "const py = \"for a, b in itertools.zip_longest(x, y): pass\";\nawait tools.exec_command({ cmd: py });";
        assert_eq!(
            single("exec", json!(source)),
            script(source, &["exec_command"])
        );
    }

    #[test]
    fn control_comment_leaves_source_and_its_settings_are_read() {
        let source = "text(load(\"skill\").length);";
        let with_comment = format!(
            "  // @exec: {{\"yield_time_ms\": 60000, \"max_output_tokens\": 14000}}\n{source}"
        );
        assert_eq!(
            single("exec", json!(with_comment)),
            (
                Tool::Script,
                json!({"source": source, "calls": [], "yield_after_ms": 60000, "max_output_tokens": 14000})
            )
        );
        let null_setting = format!("// @exec: {{\"max_output_tokens\": null}}\n{source}");
        assert_eq!(single("exec", json!(null_setting)), script(source, &[]));
        let largest_safe_integer =
            format!("// @exec: {{\"max_output_tokens\": 9007199254740991}}\n{source}");
        assert_eq!(
            single("exec", json!(largest_safe_integer)),
            (
                Tool::Script,
                json!({"source": source, "calls": [], "max_output_tokens": 9_007_199_254_740_991_u64})
            )
        );
    }

    #[test]
    fn control_comment_codex_rejects_stays_in_source() {
        for input in [
            "// @exec: {\"timeout_ms\": 1000}\ntext(1);",
            "// @exec: {\"yield_time_ms\": \"1s\"}\ntext(1);",
            "// @exec: {\"yield_time_ms\": -1}\ntext(1);",
            "// @exec: {\"max_output_tokens\": 9007199254740992}\ntext(1);",
            "// @exec: [1000]\ntext(1);",
            "// @exec: {yield_time_ms: 1000}\ntext(1);",
            "// @exec: {\"yield_time_ms\": 1000}\n  \n",
            "// @exec: {\"yield_time_ms\": 1000}",
        ] {
            assert_eq!(
                single("exec", json!(input)),
                script(input, &[]),
                "the control comment Codex rejects stays in the source"
            );
        }
    }

    #[test]
    fn update_plan_reads_its_explanation_and_each_steps_text_and_status() {
        assert_eq!(
            single(
                "update_plan",
                json!({"explanation": "Ship it", "plan": [
                    {"step": "Read", "status": "completed"},
                    {"step": "Write", "status": "in_progress"},
                    {"step": "Ship", "status": "pending"},
                ]})
            ),
            (
                Tool::TaskList,
                json!({"explanation": "Ship it", "steps": [
                    {"text": "Read", "status": "completed"},
                    {"text": "Write", "status": "in_progress"},
                    {"text": "Ship", "status": "pending"},
                ]})
            )
        );
    }

    #[test]
    fn update_plan_with_step_codex_rejects_keeps_its_arguments() {
        for arguments in [
            json!({"plan": [{"step": "Read", "status": "done"}]}),
            json!({"plan": [{"status": "pending"}]}),
        ] {
            assert_eq!(
                single("update_plan", arguments.clone()),
                (Tool::TaskList, arguments)
            );
        }
    }

    #[test]
    fn update_plan_with_plan_not_list_or_explanation_not_text_keeps_its_arguments() {
        for arguments in [
            json!({"plan": "Read, then ship"}),
            json!({"explanation": 3, "plan": []}),
        ] {
            assert_eq!(
                single("update_plan", arguments.clone()),
                (Tool::TaskList, arguments)
            );
        }
    }

    #[test]
    fn exec_update_plan_as_its_one_call_is_plan_update_naming_its_tool() {
        let plan = "// @exec: {\"max_output_tokens\": 3600}\ntext(await tools.update_plan({ explanation: \"Ship it\", plan: [{ step: \"Read, then [write]\", status: \"completed\" }, {status:'pending', step:'Ship'},] }));";
        assert_eq!(
            single("exec", json!(plan)),
            (
                Tool::TaskList,
                json!({
                    "explanation": "Ship it",
                    "steps": [
                        {"text": "Read, then [write]", "status": "completed"},
                        {"text": "Ship", "status": "pending"},
                    ],
                    "calls": ["update_plan"],
                })
            )
        );
        let json_quoted =
            r#"await tools.update_plan({"plan":[{"step":"Read","status":"in_progress"}]})"#;
        assert_eq!(
            single("exec", json!(json_quoted)),
            (
                Tool::TaskList,
                json!({"steps": [{"text": "Read", "status": "in_progress"}], "calls": ["update_plan"]})
            )
        );
    }

    #[test]
    fn exec_update_plan_with_step_not_literal_or_beside_other_calls_is_its_tools_and_source() {
        for (source, calls) in [
            (
                "const plan = [{ step: \"a\", status: \"pending\" }];\nawait tools.update_plan({ plan });",
                &["update_plan"][..],
            ),
            (
                "await tools.update_plan({ plan: steps.map(step => ({ step, status: \"pending\" })) });",
                &["update_plan"],
            ),
            (
                "await tools.update_plan({ plan: [{ step: name, status: \"pending\" }] });",
                &["update_plan"],
            ),
            (
                "await tools.update_plan({ plan: [{ step: \"a\", status: \"done\" }] });",
                &["update_plan"],
            ),
            (
                "await tools.update_plan({ explanation: why, plan: [{ step: \"a\", status: \"pending\" }] });",
                &["update_plan"],
            ),
            (
                "await Promise.all([tools.update_plan({ plan: [{ step: \"a\", status: \"pending\" }] }), tools.exec_command({ cmd: \"ls\" })]);",
                &["update_plan", "exec_command"],
            ),
        ] {
            assert_eq!(single("exec", json!(source)), script(source, calls));
        }
    }

    #[test]
    fn exec_update_plan_with_text_known_only_at_run_time_is_its_tools_and_source() {
        for source in [
            "await tools.update_plan({ plan: [{ step: \"Run \" + n, status: \"pending\" }] });",
            "await tools.update_plan({ plan: [{ step: `Run ${n}`, status: \"pending\" }] });",
            "await tools.update_plan({ explanation: `${done} done`, plan: [] });",
            "await tools.update_plan({ plan: [{ step: \"a\", status: \"pending\" }].concat(rest) });",
            "for (const n of runs) await tools.update_plan({ plan: [{ step: \"a\", status: \"pending\" }] });",
        ] {
            assert_eq!(
                single("exec", json!(source)),
                script(source, &["update_plan"]),
                "the script shows its source, not a plan read from part of it"
            );
        }
    }

    #[test]
    fn exec_update_plan_only_in_comment_or_string_is_script_without_tools() {
        for source in [
            "// await tools.update_plan({ plan: [{ step: \"a\", status: \"pending\" }] })\ntext(\"not yet\");",
            "text(\"await tools.update_plan({ plan: [] })\");",
        ] {
            assert_eq!(
                single("exec", json!(source)),
                script(source, &[]),
                "the script doesn't call a tool"
            );
        }
    }

    #[test]
    fn exec_write_stdin_as_its_one_call_is_wait_on_its_process() {
        let poll = "// @exec: {\"yield_time_ms\":60000,\"max_output_tokens\":480}\ntext(await tools.write_stdin({session_id:24279,chars:\"\",yield_time_ms:50000,max_output_tokens:330}))";
        assert_eq!(
            single("exec", json!(poll)),
            (Tool::Wait, json!({"session_id": "24279", "wait_ms": 50000}))
        );
        let typed = "await tools.write_stdin({ session_id: 8852, chars: \"/exit\\n\" });";
        assert_eq!(
            single("exec", json!(typed)),
            (
                Tool::Wait,
                json!({"session_id": "8852", "input": "/exit\n"})
            )
        );
        let json_quoted = r#"await tools.write_stdin({"session_id":3,"yield_time_ms":1000})"#;
        assert_eq!(
            single("exec", json!(json_quoted)),
            (Tool::Wait, json!({"session_id": "3", "wait_ms": 1000}))
        );
    }

    #[test]
    fn exec_write_stdin_with_argument_not_literal_or_beside_other_calls_is_its_tools_and_source() {
        for (source, calls) in [
            (
                "await tools.write_stdin({ session_id: shell.session_id, chars: \"\" });",
                &["write_stdin"][..],
            ),
            (
                "await tools.write_stdin({ session_id: 7, chars: keys });",
                &["write_stdin"],
            ),
            (
                "await tools.write_stdin({ session_id: 7, yield_time_ms: wait });",
                &["write_stdin"],
            ),
            ("await tools.write_stdin(arguments);", &["write_stdin"]),
            (
                "await Promise.all(ids.map(id => tools.write_stdin({ session_id: 7 })).concat([tools.write_stdin({ session_id: 8 })]));",
                &["write_stdin"],
            ),
            (
                "const s = await tools.exec_command({ cmd: server });\nawait tools.write_stdin({ session_id: 7 });",
                &["exec_command", "write_stdin"],
            ),
        ] {
            assert_eq!(single("exec", json!(source)), script(source, calls));
        }
    }

    #[test]
    fn exec_write_stdin_with_literal_inside_expression_is_its_tools_and_source() {
        for source in [
            "await tools.write_stdin({ session_id: 7, yield_time_ms: 30 * 1000 });",
            "await tools.write_stdin({ session_id: 7, chars: \"y\" + \"\\n\" });",
            "await tools.write_stdin({ session_id: 7.5 });",
            "await tools.write_stdin({ session_id: 7, chars: `${cmd}\\n` });",
        ] {
            assert_eq!(
                single("exec", json!(source)),
                script(source, &["write_stdin"]),
                "the script shows its source, not a wait on the literal alone"
            );
        }
    }

    #[test]
    fn exec_write_stdin_in_loop_branch_or_beside_other_use_of_tools_is_its_tools_and_source() {
        for source in [
            "for (const key of keys) { await tools.write_stdin({ session_id: 7, chars: \"y\" }); }",
            "for (let i = 0; i < 3; i++) await tools.write_stdin({ session_id: 7 });",
            "if (stuck)\n  await tools.write_stdin({ session_id: 7, chars: \"\\u0003\" });",
            "stuck && await tools.write_stdin({ session_id: 7 });",
            "[1, 2].forEach(async () => { await tools.write_stdin({ session_id: 7 }); });",
            "await tools.write_stdin({ session_id: 7 });\nawait tools[name]({});",
        ] {
            assert_eq!(
                single("exec", json!(source)),
                script(source, &["write_stdin"]),
                "a script that may call `write_stdin` other than once shows its source"
            );
        }
        let after_braceless_branch = "if (!ready) text(\"waiting\")\nawait tools.write_stdin({ session_id: 7, yield_time_ms: 1000 });";
        assert_eq!(
            single("exec", json!(after_braceless_branch)),
            (Tool::Wait, json!({"session_id": "7", "wait_ms": 1000}))
        );
    }

    #[test]
    fn exec_reads_agent_and_image_calls() {
        let spawn = json!(
            "await tools.spawn_agent({ task_name: 'scout', message: `Do this:\n- a\n- \\`b\\`` })"
        );
        assert_eq!(
            single("exec", spawn),
            (
                Tool::Agent,
                json!({"description": "scout", "prompt": "Do this:\n- a\n- `b`"})
            )
        );
        let message = json!(r#"await tools.send_message({"target":"scout","message":"done?"})"#);
        assert_eq!(
            single("exec", message),
            (
                Tool::AgentMessage,
                json!({"recipient": "scout", "message": "done?"})
            )
        );
        let image = json!("await tools.view_image({ path: \"shot.png\" })");
        assert_eq!(
            single("exec", image),
            (Tool::Read, json!({"file_path": "shot.png"}))
        );
    }

    #[test]
    fn exec_patches_split_into_edits_like_direct_ones() {
        let template = json!(
            "await tools.apply_patch(`*** Begin Patch\n*** Update File: a.rs\n@@\n-x\n+y\n*** End Patch`)"
        );
        assert_eq!(
            single("exec", template),
            (
                Tool::Edit,
                json!({"file_path": "a.rs", "patch": "@@\n-x\n+y"})
            )
        );
        let keyed = json!(
            r#"await tools.apply_patch({"input":"*** Begin Patch\n*** Add File: b.rs\n+b\n*** Delete File: c.rs\n*** End Patch"})"#
        );
        assert_eq!(
            calls("exec", keyed)
                .into_iter()
                .map(|(tool, input, _)| (tool, input))
                .collect::<Vec<_>>(),
            vec![
                (Tool::Edit, json!({"file_path": "b.rs", "patch": "+b"})),
                (
                    Tool::Edit,
                    json!({"file_path": "c.rs", "patch": "*** Delete File: c.rs"})
                ),
            ]
        );
    }

    #[test]
    fn exec_patch_in_variable_splits_into_edits_like_a_literal_one() {
        let source = "// @exec: {\"max_output_tokens\": 600}\nconst patch = \"*** Begin Patch\\n*** Update File: a.md\\n@@\\n-x\\n+y\\n*** Delete File: b.md\\n*** End Patch\";\ntext(await tools.apply_patch(patch));";
        assert_eq!(
            calls("exec", json!(source))
                .into_iter()
                .map(|(tool, input, _)| (tool, input))
                .collect::<Vec<_>>(),
            vec![
                (
                    Tool::Edit,
                    json!({"file_path": "a.md", "patch": "@@\n-x\n+y"})
                ),
                (
                    Tool::Edit,
                    json!({"file_path": "b.md", "patch": "*** Delete File: b.md"})
                ),
            ]
        );
        for source in [
            "let p = `*** Begin Patch\n*** Add File: c.md\n+c\n*** End Patch`;\nawait tools.apply_patch( p );",
            "var p = '*** Begin Patch\\n*** Add File: c.md\\n+c\\n*** End Patch' // the patch\ntext(await tools.apply_patch(p));",
            "const p = \"*** Begin Patch\\n*** Add File: c.md\\n+c\\n*** End Patch\", q = 1;\nawait tools.apply_patch(p);",
        ] {
            assert_eq!(
                single("exec", json!(source)),
                (Tool::Edit, json!({"file_path": "c.md", "patch": "+c"})),
                "the literal is the whole initialiser of the patch"
            );
        }
    }

    #[test]
    fn exec_patch_in_variable_changed_or_declared_twice_is_its_tools_and_source() {
        for source in [
            "let patch = \"*** Begin Patch\\n*** Add File: a.md\\n+a\\n*** End Patch\";\npatch = patch.replace(\"a.md\", \"b.md\");\nawait tools.apply_patch(patch);",
            "let patch = \"*** Begin Patch\\n*** Add File: a.md\\n+a\\n*** End Patch\";\npatch += \"\\n\";\nawait tools.apply_patch(patch);",
            "var p = 'old';\nvar p = '*** Begin Patch\\n*** Add File: c.md\\n+c\\n*** End Patch';\nawait tools.apply_patch(p);",
            "const patch = \"*** Begin Patch\\n*** Add File: a.md\\n+a\\n*** End Patch\";\nfunction other() { const patch = \"*** Begin Patch\\n*** Add File: b.md\\n+b\\n*** End Patch\"; }\nawait tools.apply_patch(patch);",
            "if (ready) { const patch = \"*** Begin Patch\\n*** Add File: a.md\\n+a\\n*** End Patch\"; }\nawait tools.apply_patch(patch);",
            "const patch = \"*** Begin Patch\\n*** Add File: a.md\\n+a\\n*** End Patch\" + \"*** Begin Patch\\n*** Add File: b.md\\n+b\\n*** End Patch\";\nawait tools.apply_patch(patch);",
            "const patch = \"*** Begin Patch\\n*** Add File: a.md\\n+a\\n*** End Patch\"\n  + extra;\nawait tools.apply_patch(patch);",
            "await tools.apply_patch(patch);\nvar patch = \"*** Begin Patch\\n*** Add File: a.md\\n+a\\n*** End Patch\";",
        ] {
            assert_eq!(
                single("exec", json!(source)),
                script(source, &["apply_patch"]),
                "the script shows its source, not a patch it may not apply"
            );
        }
    }

    #[test]
    fn exec_patch_literal_inside_expression_is_its_tools_and_source() {
        let source = "await tools.apply_patch(\"*** Begin Patch\\n*** Add File: a.md\\n+a\\n*** End Patch\" + rest);";
        assert_eq!(
            single("exec", json!(source)),
            script(source, &["apply_patch"])
        );
    }

    #[test]
    fn exec_patch_not_literal_or_beside_other_calls_is_its_tools_and_source() {
        for (source, calls) in [
            (
                "const patch = `*** Begin Patch\n*** Add File: ${name}\n+c\n*** End Patch`;\nawait tools.apply_patch(patch);",
                &["apply_patch"][..],
            ),
            ("await tools.apply_patch(patch);", &["apply_patch"]),
            (
                "const patches = [\"*** Begin Patch\\n*** Add File: c.md\\n+c\\n*** End Patch\"];\nawait tools.apply_patch(patches[0]);",
                &["apply_patch"],
            ),
            (
                "const patch = \"no file section\";\nawait tools.apply_patch(patch);",
                &["apply_patch"],
            ),
            (
                "const patch = \"*** Begin Patch\\n*** Add File: c.md\\n+c\\n*** End Patch\";\nawait tools.apply_patch(patch);\nawait tools.exec_command({ cmd: \"git diff\" });",
                &["apply_patch", "exec_command"],
            ),
        ] {
            assert_eq!(single("exec", json!(source)), script(source, calls));
        }
    }

    fn rows(source: &str) -> Vec<(Tool, Value)> {
        calls("exec", json!(source))
            .into_iter()
            .map(|(tool, input, _)| (tool, input))
            .collect()
    }

    #[test]
    fn exec_shell_command_in_string_raw_shows_as_its_plain_literal_twin() {
        for (raw, plain, command) in [
            (
                "const r = await tools.exec_command({cmd:String.raw`git log -1 --format=%s`,workdir:\"/repo\",yield_time_ms:10000}); text(r.output);",
                "const r = await tools.exec_command({cmd:`git log -1 --format=%s`,workdir:\"/repo\",yield_time_ms:10000}); text(r.output);",
                "git log -1 --format=%s",
            ),
            (
                "await tools.shell_command({ command: String.raw`\n$ErrorActionPreference = 'Stop'\nGet-ChildItem`, workdir: \"/repo\" });",
                "await tools.shell_command({ command: \"\\n$ErrorActionPreference = 'Stop'\\nGet-ChildItem\", workdir: \"/repo\" });",
                "\n$ErrorActionPreference = 'Stop'\nGet-ChildItem",
            ),
            (
                "await tools.exec_command({ cmd: String.raw `git status` });",
                "await tools.exec_command({ cmd: `git status` });",
                "git status",
            ),
            (
                "await tools.exec_command({ cmd: String.raw\n  `git status` });",
                "await tools.exec_command({ cmd:\n  `git status` });",
                "git status",
            ),
        ] {
            let row = (Tool::Shell, json!({ "command": command }));
            assert_eq!(
                single("exec", json!(raw)),
                row,
                "the `String.raw` script shows its command as written"
            );
            assert_eq!(
                single("exec", json!(plain)),
                row,
                "the plain-literal twin shows the same command"
            );
        }
    }

    #[test]
    fn exec_patch_in_string_raw_variable_splits_into_edits_like_its_plain_literal_twin() {
        let expected = vec![
            (
                Tool::Edit,
                json!({"file_path": "a.md", "patch": "@@\n-x\n+y"}),
            ),
            (Tool::Edit, json!({"file_path": "b.md", "patch": "+b"})),
        ];
        let raw = "const patch = String.raw`*** Begin Patch\n*** Update File: a.md\n@@\n-x\n+y\n*** Add File: b.md\n+b\n*** End Patch`;\ntext(await tools.apply_patch(patch));";
        let plain = "const patch = \"*** Begin Patch\\n*** Update File: a.md\\n@@\\n-x\\n+y\\n*** Add File: b.md\\n+b\\n*** End Patch\";\ntext(await tools.apply_patch(patch));";
        assert_eq!(
            rows(raw),
            expected,
            "the `String.raw` script shows one edit per file"
        );
        assert_eq!(
            rows(plain),
            expected,
            "the plain-literal twin shows the same edits"
        );
    }

    #[test]
    fn string_raw_template_keeps_backslashes_as_written() {
        let raw = r#"await tools.exec_command({ cmd: String.raw`rg "\bfoo\s+bar"` });"#;
        assert_eq!(
            single("exec", json!(raw)),
            (Tool::Shell, json!({"command": r#"rg "\bfoo\s+bar""#}))
        );
        let plain = r#"await tools.exec_command({ cmd: `rg "\bfoo\s+bar"` });"#;
        assert_eq!(
            single("exec", json!(plain)),
            (Tool::Shell, json!({"command": "rg \"\u{8}foo\\s+bar\""})),
            "the plain template unescapes `\\b` and keeps `\\s`"
        );
        let escaped_backtick = r"await tools.exec_command({ cmd: String.raw`echo \`date\`` });";
        assert_eq!(
            single("exec", json!(escaped_backtick)),
            (Tool::Shell, json!({"command": r"echo \`date\`"})),
            "an escaped backtick keeps its backslash"
        );
    }

    #[test]
    fn string_raw_template_with_substitution_shows_its_text_where_plain_template_does() {
        for (plain, raw, expected) in [
            (
                "await tools.exec_command({ cmd: `rg ${pattern} docs` });",
                "await tools.exec_command({ cmd: String.raw`rg ${pattern} docs` });",
                (Tool::Shell, json!({"command": "rg ${pattern} docs"})),
            ),
            (
                "await tools.apply_patch(`*** Begin Patch\n*** Add File: ${name}\n+c\n*** End Patch`);",
                "await tools.apply_patch(String.raw`*** Begin Patch\n*** Add File: ${name}\n+c\n*** End Patch`);",
                (Tool::Edit, json!({"file_path": "${name}", "patch": "+c"})),
            ),
        ] {
            assert_eq!(
                rows(plain),
                std::slice::from_ref(&expected),
                "the plain template shows its text, the substitution as written"
            );
            assert_eq!(
                rows(raw),
                [expected],
                "the `String.raw` twin shows the same text"
            );
        }
    }

    #[test]
    fn string_raw_template_with_substitution_is_tools_and_source_where_plain_template_is() {
        for (plain, raw, calls) in [
            (
                "const patch = `*** Begin Patch\n*** Add File: ${name}\n+c\n*** End Patch`;\nawait tools.apply_patch(patch);",
                "const patch = String.raw`*** Begin Patch\n*** Add File: ${name}\n+c\n*** End Patch`;\nawait tools.apply_patch(patch);",
                &["apply_patch"][..],
            ),
            (
                "await tools.write_stdin({ session_id: 7, chars: `${keys}` });",
                "await tools.write_stdin({ session_id: 7, chars: String.raw`${keys}` });",
                &["write_stdin"],
            ),
            (
                "await tools.update_plan({ plan: [{ step: `Run ${n}`, status: \"pending\" }] });",
                "await tools.update_plan({ plan: [{ step: String.raw`Run ${n}`, status: \"pending\" }] });",
                &["update_plan"],
            ),
        ] {
            assert_eq!(
                single("exec", json!(plain)),
                script(plain, calls),
                "the plain template's script shows its tools and source"
            );
            assert_eq!(
                single("exec", json!(raw)),
                script(raw, calls),
                "the `String.raw` twin's script shows its tools and source"
            );
        }
    }

    #[test]
    fn exec_string_raw_inside_expression_or_other_tag_is_its_tools_and_source() {
        for (source, calls) in [
            (
                "await tools.exec_command({ cmd: String.raw`rg -n ` + pattern });",
                &["exec_command"][..],
            ),
            (
                "await tools.exec_command({ cmd: String.raw` git status `.trim() });",
                &["exec_command"],
            ),
            (
                "await tools.exec_command({ cmd: dedent`git status` });",
                &["exec_command"],
            ),
            (
                "const raw = String.raw;\nawait tools.exec_command({ cmd: raw`git status` });",
                &["exec_command"],
            ),
            (
                "const patch = String.raw`*** Begin Patch\n*** Add File: c.md\n+c\n*** End Patch` + extra;\nawait tools.apply_patch(patch);",
                &["apply_patch"],
            ),
            (
                "const patch = String.raw`*** Begin Patch\n*** Add File: c.md\n+c\n*** End Patch`.trim();\nawait tools.apply_patch(patch);",
                &["apply_patch"],
            ),
            (
                "const patch = dedent`*** Begin Patch\n*** Add File: c.md\n+c\n*** End Patch`;\nawait tools.apply_patch(patch);",
                &["apply_patch"],
            ),
        ] {
            assert_eq!(
                single("exec", json!(source)),
                script(source, calls),
                "the script shows its source, not a value it may not use"
            );
        }
    }

    #[test]
    fn exec_web_runs_read_their_first_query_or_reference() {
        let search = json!(
            "const result = await tools.web__run({\n  search_query: [{ q: \"standalone web search\" }],\n});"
        );
        assert_eq!(
            single("exec", search),
            (Tool::WebSearch, json!({"query": "standalone web search"}))
        );
        let open =
            json!(r#"await tools.web__run({"open":[{"ref_id":"https://example.com/docs"}]})"#);
        assert_eq!(
            single("exec", open),
            (Tool::WebFetch, json!({"url": "https://example.com/docs"}))
        );
        let find = "await tools.web__run({ find: [{ ref_id: \"turn0search0\", pattern: \"x\" }] })";
        assert_eq!(single("exec", json!(find)), script(find, &["web__run"]));
    }
}
