//! Codex's tool calls mapped onto the canonical [`Tool`] set.
//!
//! `exec` runs a JavaScript script in which tool calls read `tools.<name>(…)`.
//! [`script_scan`] finds those calls outside comments and literals; the
//! source is not parsed further. A layout reads literal values out of a
//! call's argument text by key, whether the argument is JSON
//! (`{"command":"…"}`) or a bare object literal (`{ cmd: "…" }`). The first
//! call names the layout of a shell command, an edit, a web search or fetch,
//! an agent launch or message, or an image read. Any other script is a
//! [`Tool::Script`]: its source and the tools it calls.

mod script_scan;

use crate::log_entry::{ContentBlock, Tool};
use regex::Regex;
use script_scan::ScriptCall;
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
        // `write_stdin` also feeds `chars` to the process's stdin when it is
        // non-empty; counted as a wait, that text is not shown.
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
    let calls = script_scan::scan(script.source).calls;
    calls
        .first()
        .and_then(first_call_layout)
        .filter(|layout| !layout.is_empty())
        .unwrap_or_else(|| vec![script.into_call(&calls)])
}

/// The calls the layout of `call`, a script's first call, shows; `None` when
/// that call has no layout.
fn first_call_layout(call: &ScriptCall<'_>) -> Option<Vec<CanonicalCall>> {
    match call.name {
        "apply_patch" => script_patch(call.rest_of_script).map(|patch| patch_edits(&patch)),
        "run" | "web__run" => script_web_call(call.rest_of_script).map(|call| vec![call]),
        name => script_function_call(name, call.rest_of_script).map(|call| vec![call]),
    }
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

const STRING_LITERAL: &str = r#""(?:[^"\\]|\\.)*"|'(?:[^'\\]|\\.)*'|`(?:[^`\\]|\\.)*`"#;

static PROPERTY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r#"(?s)(?:^|[^\w"'`])["']?(\w+)["']?\s*:\s*({STRING_LITERAL})?"#
    ))
    .unwrap()
});

static LEADING_STRING_LITERAL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(&format!(r"(?s)^\s*({STRING_LITERAL})")).unwrap());

struct Property<'a> {
    key: &'a str,
    string_literal: Option<&'a str>,
}

fn properties(object_text: &str) -> impl Iterator<Item = Property<'_>> {
    PROPERTY
        .captures_iter(object_text)
        .map(|captures| Property {
            key: captures.get(1).map_or("", |key| key.as_str()),
            string_literal: captures.get(2).map(|literal| literal.as_str()),
        })
}

fn has_property(object_text: &str, key: &str) -> bool {
    properties(object_text).any(|property| property.key == key)
}

fn property_string(object_text: &str, key: &str) -> Option<String> {
    properties(object_text)
        .filter(|property| property.key == key)
        .find_map(|property| property.string_literal)
        .map(unescape_literal)
}

fn leading_string_literal(text: &str) -> Option<String> {
    let literal = LEADING_STRING_LITERAL.captures(text)?.get(1)?;
    Some(unescape_literal(literal.as_str()))
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
        assert_eq!(
            single("write_stdin", json!({"session_id": 1, "chars": "y\n"})).1,
            json!({"session_id": 1, "chars": "y\n"})
        );
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

        let two_kinds = json!(
            "await tools.exec_command({ cmd: \"first\" });\nawait tools.shell_command({ command: \"second\" });"
        );
        assert_eq!(
            single("exec", two_kinds),
            (Tool::Shell, json!({"command": "first"}))
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
    fn exec_waits_and_plan_updates_are_their_tools_and_source() {
        let plan = "await tools.update_plan({ plan: [{ step: \"a\", status: \"in_progress\" }] });";
        assert_eq!(single("exec", json!(plan)), script(plan, &["update_plan"]));
        let stdin = "await tools.write_stdin({ session_id: shell.session_id, chars: \"\" });";
        assert_eq!(single("exec", json!(stdin)), script(stdin, &["write_stdin"]));
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
