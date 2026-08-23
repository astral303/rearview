//! Codex's tool calls mapped onto the canonical [`Tool`] set.
//!
//! `exec` runs a JavaScript script in which tool calls read `tools.<name>(…)`.
//! The script is not parsed: its first such call names the tool, and the
//! strings the header needs are read out of the argument text by key, whether
//! the argument is JSON (`{"command":"…"}`) or a bare object literal
//! (`{ cmd: "…" }`).

use crate::log_entry::{ContentBlock, Tool};
use regex::Regex;
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

struct CanonicalCall {
    tool: Tool,
    input: Value,
}

fn canonical_calls(name: &str, input: &Value) -> Vec<CanonicalCall> {
    let calls = match name {
        "exec" => input.as_str().and_then(script_calls),
        "apply_patch" => patch_text(input).map(patch_edits),
        "run" | "web__run" => Some(web_calls(input)),
        _ => function_call(name, input).map(|call| vec![call]),
    };
    calls.filter(|calls| !calls.is_empty()).unwrap_or_else(|| {
        vec![CanonicalCall {
            tool: Tool::Other,
            input: input.clone(),
        }]
    })
}

fn tool_named(name: &str) -> Tool {
    match name {
        "shell_command" | "exec_command" => Tool::Shell,
        // `write_stdin` also feeds `chars` to the process's stdin when it is
        // non-empty; counted as a wait, that text is not shown.
        "write_stdin" | "wait" | "wait_agent" => Tool::Wait,
        "spawn_agent" => Tool::Agent,
        "send_message" | "followup_task" => Tool::AgentMessage,
        "update_plan" => Tool::TaskList,
        "view_image" => Tool::Read,
        _ => Tool::Other,
    }
}

fn header_keys(name: &str) -> &'static [(&'static str, &'static str)] {
    match name {
        "shell_command" => &[("command", "command")],
        "exec_command" => &[("cmd", "command")],
        "spawn_agent" => &[("task_name", "description"), ("message", "prompt")],
        "send_message" | "followup_task" => &[("target", "recipient"), ("message", "message")],
        "view_image" => &[("path", "file_path")],
        _ => &[],
    }
}

fn function_call(name: &str, input: &Value) -> Option<CanonicalCall> {
    let tool = tool_named(name);
    if tool == Tool::Other {
        return None;
    }
    let mut input = input.clone();
    if let Some(object) = input.as_object_mut() {
        for (codex_key, canonical_key) in header_keys(name) {
            if let Some(value) = object.remove(*codex_key) {
                object.insert((*canonical_key).to_owned(), value);
            }
        }
    }
    has_header_string(name, &input).then_some(CanonicalCall { tool, input })
}

fn has_header_string(name: &str, input: &Value) -> bool {
    let keys = header_keys(name);
    keys.is_empty()
        || keys
            .iter()
            .any(|(_, canonical_key)| input.get(canonical_key).is_some_and(Value::is_string))
}

fn script_calls(script: &str) -> Option<Vec<CanonicalCall>> {
    let call = first_script_call(script)?;
    match call.name {
        "apply_patch" => script_patch(call.argument).map(|patch| patch_edits(&patch)),
        "run" | "web__run" => script_web_call(call.argument).map(|call| vec![call]),
        name => script_function_call(name, call.argument, script).map(|call| vec![call]),
    }
}

fn script_function_call(name: &str, argument: &str, script: &str) -> Option<CanonicalCall> {
    let tool = tool_named(name);
    if tool == Tool::Other {
        return None;
    }
    let keys = header_keys(name);
    if keys.is_empty() {
        return Some(CanonicalCall {
            tool,
            input: Value::String(script.to_owned()),
        });
    }
    let input: Map<String, Value> = keys
        .iter()
        .filter_map(|(codex_key, canonical_key)| {
            property_string(argument, codex_key)
                .map(|value| ((*canonical_key).to_owned(), Value::String(value)))
        })
        .collect();
    (!input.is_empty()).then_some(CanonicalCall {
        tool,
        input: Value::Object(input),
    })
}

fn patch_text(input: &Value) -> Option<&str> {
    input
        .as_str()
        .or_else(|| input.get("input").and_then(Value::as_str))
}

fn script_patch(argument: &str) -> Option<String> {
    leading_string_literal(argument).or_else(|| property_string(argument, "input"))
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

fn web_calls(input: &Value) -> Vec<CanonicalCall> {
    let searches = list_strings(input, "search_query", "q").map(|query| CanonicalCall {
        tool: Tool::WebSearch,
        input: json!({ "query": query }),
    });
    let fetches = list_strings(input, "open", "ref_id").map(|url| CanonicalCall {
        tool: Tool::WebFetch,
        input: json!({ "url": url }),
    });
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

fn script_web_call(argument: &str) -> Option<CanonicalCall> {
    if has_property(argument, "search_query")
        && let Some(query) = property_string(argument, "q")
    {
        return Some(CanonicalCall {
            tool: Tool::WebSearch,
            input: json!({ "query": query }),
        });
    }
    if has_property(argument, "open")
        && let Some(url) = property_string(argument, "ref_id")
    {
        return Some(CanonicalCall {
            tool: Tool::WebFetch,
            input: json!({ "url": url }),
        });
    }
    None
}

struct ScriptCall<'a> {
    name: &'a str,
    /// The text after the call's opening parenthesis, to the end of the script.
    argument: &'a str,
}

static FIRST_CALL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"tools\.(\w+)\s*\(").unwrap());

fn first_script_call(script: &str) -> Option<ScriptCall<'_>> {
    let call = FIRST_CALL.captures(script)?;
    Some(ScriptCall {
        name: call.get(1)?.as_str(),
        argument: &script[call.get(0)?.end()..],
    })
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
            ("view_image", json!({"path": "shot.png"}), Tool::Read),
            ("list_agents", json!({}), Tool::Other),
            ("interrupt_agent", json!({"target": "scout"}), Tool::Other),
            ("request_user_input", json!({"questions": []}), Tool::Other),
            ("ide__find_references", json!({"file": "a.rs"}), Tool::Other),
        ];
        for (name, input, tool) in expected {
            assert_eq!(single(name, input).0, tool, "{name}");
        }
    }

    #[test]
    fn codex_keys_are_renamed_to_the_canonical_ones_and_the_rest_kept() {
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
            json!({"description": "scout", "prompt": "Look around.", "model": "gpt-5"})
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
            single("view_image", json!({"path": "shot.png", "detail": "high"})).1,
            json!({"file_path": "shot.png", "detail": "high"})
        );
        assert_eq!(
            single("write_stdin", json!({"session_id": 1, "chars": "y\n"})).1,
            json!({"session_id": 1, "chars": "y\n"})
        );
    }

    #[test]
    fn a_call_without_the_strings_its_header_shows_stays_other() {
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
    fn apply_patch_as_a_function_call_reads_its_input_field() {
        let (tool, input) = single(
            "apply_patch",
            json!({"input": "*** Begin Patch\n*** Add File: a.txt\n+a\n*** End Patch"}),
        );
        assert_eq!(tool, Tool::Edit);
        assert_eq!(input, json!({"file_path": "a.txt", "patch": "+a"}));
    }

    #[test]
    fn a_patch_with_no_file_section_stays_other() {
        let (tool, input) = single("apply_patch", json!("*** Begin Patch\n*** End Patch"));
        assert_eq!(tool, Tool::Other);
        assert_eq!(input, json!("*** Begin Patch\n*** End Patch"));
    }

    #[test]
    fn a_web_run_searches_per_query_and_fetches_per_opened_reference() {
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
    fn an_exec_script_is_named_by_its_first_call() {
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

    #[test]
    fn an_exec_script_with_no_recognizable_call_stays_other() {
        for script in [
            "echo TOOL_INPUT_VISIBLE",
            "const now = await tools.clock__curr_time({});",
            "await tools.exec_command({ cmd: command })",
            "await tools.exec_command(",
        ] {
            let (tool, input) = single("exec", json!(script));
            assert_eq!(tool, Tool::Other, "{script}");
            assert_eq!(input, json!(script));
        }
    }

    #[test]
    fn exec_waits_and_plan_updates_keep_the_script_as_input() {
        let plan = "await tools.update_plan({ plan: [{ step: \"a\", status: \"in_progress\" }] });";
        assert_eq!(single("exec", json!(plan)), (Tool::TaskList, json!(plan)));
        let stdin = "await tools.write_stdin({ session_id: shell.session_id, chars: \"\" });";
        assert_eq!(single("exec", json!(stdin)), (Tool::Wait, json!(stdin)));
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
        assert_eq!(single("exec", json!(find)), (Tool::Other, json!(find)));
    }
}
