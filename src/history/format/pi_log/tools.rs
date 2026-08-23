//! Pi's and OMP's tool calls mapped onto the canonical [`Tool`] set.
//!
//! The two agents share the log format but not the tool vocabulary, so the
//! session's source chooses the mapping. Every OMP call carries `i`, a
//! one-line description of the call, which passes through like any other key.

use crate::history::Source;
use crate::log_entry::{ContentBlock, Tool};
use regex::Regex;
use serde_json::{Map, Value};
use std::sync::LazyLock;

/// An OMP `edit` of several files becomes several blocks; the first keeps
/// the call's id, so its result still pairs with it, and the rest are
/// numbered after it.
pub(super) fn tool_use_blocks(
    call_id: &str,
    name: &str,
    input: Value,
    source: Source,
) -> Vec<ContentBlock> {
    let calls = match source {
        Source::Pi => vec![pi_call(name, input)],
        Source::Omp => omp_calls(name, input),
        _ => vec![CanonicalCall {
            tool: Tool::Other,
            input,
        }],
    };
    calls
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

fn pi_call(name: &str, mut input: Value) -> CanonicalCall {
    let tool = pi_tool(name);
    if let Some(arguments) = input.as_object_mut() {
        canonicalize_pi_arguments(name, tool, arguments);
    }
    CanonicalCall { tool, input }
}

fn pi_tool(name: &str) -> Tool {
    match name {
        "bash" | "nu" => Tool::Shell,
        "read" => Tool::Read,
        "edit" | "patch" | "target_edit" | "quick_edit" => Tool::Edit,
        "write" => Tool::Write,
        "grep" => Tool::Grep,
        "find" => Tool::Glob,
        // Of the `task_*` extension, these four write task state; the others
        // (`task_evidence`, `task_resume`, `task_list`, `task_checkpoint`,
        // `task_focus`) read or annotate it.
        "task_plan" | "task_update" | "task_decompose" | "task_complete" => Tool::TaskList,
        _ => Tool::Other,
    }
}

fn canonicalize_pi_arguments(name: &str, tool: Tool, arguments: &mut Map<String, Value>) {
    if addresses_one_file(tool) {
        rename_key(arguments, "path", "file_path");
    }
    if sends_replacement_pairs(name) {
        replace_pairs_with_patch(arguments);
    }
}

/// `grep` sends `path` as the directory it searches, which the canonical
/// input calls `path` as well.
fn addresses_one_file(tool: Tool) -> bool {
    tool.is_file_tool() && !tool.is_search_tool()
}

/// `edit` lists `{oldText, newText}` pairs and `patch` `{old_str, new_str}`
/// pairs under `edits`; `target_edit` and `quick_edit` send operations of
/// their own that are kept as they are.
fn sends_replacement_pairs(name: &str) -> bool {
    matches!(name, "edit" | "patch")
}

fn rename_key(arguments: &mut Map<String, Value>, from: &str, to: &str) {
    if let Some(value) = arguments.remove(from) {
        arguments.insert(to.to_owned(), value);
    }
}

/// One `patch` body for every pair, in order, hunks separated by `@@`. An
/// `edits` that lists no pair (a bare string, an empty list) is kept as it is.
fn replace_pairs_with_patch(arguments: &mut Map<String, Value>) {
    let Some(edits) = arguments.get("edits") else {
        return;
    };
    let hunks = replacement_pairs(edits)
        .into_iter()
        .map(|(old, new)| replacement_diff(old, new))
        .collect::<Vec<_>>();
    if hunks.is_empty() {
        return;
    }
    arguments.remove("edits");
    arguments.insert("patch".to_owned(), Value::String(hunks.join("\n@@\n")));
}

fn replacement_pairs(edits: &Value) -> Vec<(&str, &str)> {
    edits
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|edit| {
            let old = string_field(edit, "oldText").or_else(|| string_field(edit, "old_str"))?;
            let new = string_field(edit, "newText").or_else(|| string_field(edit, "new_str"))?;
            Some((old, new))
        })
        .collect()
}

/// The replaced lines then the replacement, each signed as git prints a hunk.
/// A deliberate twin of `tool_format::replacement_diff`: the history layer
/// builds canonical input and does not depend on the renderer.
fn replacement_diff(old: &str, new: &str) -> String {
    old.lines()
        .map(|line| format!("-{line}"))
        .chain(new.lines().map(|line| format!("+{line}")))
        .collect::<Vec<_>>()
        .join("\n")
}

fn string_field<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

fn omp_calls(name: &str, mut input: Value) -> Vec<CanonicalCall> {
    let tool = omp_tool(name);
    if tool == Tool::Edit {
        return hashline_edits(input);
    }
    if let Some(arguments) = input.as_object_mut() {
        canonicalize_omp_arguments(tool, arguments);
    }
    vec![CanonicalCall { tool, input }]
}

/// OMP's `glob` sends the pattern it matches as `path`, with no search root.
fn canonicalize_omp_arguments(tool: Tool, arguments: &mut Map<String, Value>) {
    if addresses_one_file(tool) {
        rename_key(arguments, "path", "file_path");
    }
    if tool == Tool::Glob {
        rename_key(arguments, "path", "pattern");
    }
}

fn omp_tool(name: &str) -> Tool {
    match name {
        "bash" => Tool::Shell,
        "read" => Tool::Read,
        "edit" => Tool::Edit,
        "write" => Tool::Write,
        "glob" => Tool::Glob,
        "todo" => Tool::TaskList,
        _ => Tool::Other,
    }
}

/// One edit per `[PATH#TAG]` section of a hashline edit's `input`, each
/// carrying the section's lines verbatim as its `patch` beside the call's
/// other keys. An `input` with no section stays `Other`.
///
/// The operation lines are not parsed: the vocabulary the documentation
/// describes (`PUT`, `CUT`, `REM`, `MV`) and the one sessions record
/// (`INS`, `SWAP`, `DEL`) differ, and both read as a body.
fn hashline_edits(input: Value) -> Vec<CanonicalCall> {
    let edits = match input.as_object() {
        Some(arguments) => section_edits(arguments),
        None => Vec::new(),
    };
    if edits.is_empty() {
        return vec![CanonicalCall {
            tool: Tool::Other,
            input,
        }];
    }
    edits
}

fn section_edits(arguments: &Map<String, Value>) -> Vec<CanonicalCall> {
    let hashline = arguments.get("input").and_then(Value::as_str).unwrap_or("");
    hashline_sections(hashline)
        .iter()
        .map(|section| {
            let mut edit = arguments.clone();
            edit.remove("input");
            edit.insert(
                "file_path".to_owned(),
                Value::String(section.path.to_owned()),
            );
            edit.insert("patch".to_owned(), Value::String(section.lines.join("\n")));
            CanonicalCall {
                tool: Tool::Edit,
                input: Value::Object(edit),
            }
        })
        .collect()
}

struct HashlineSection<'a> {
    path: &'a str,
    lines: Vec<&'a str>,
}

static SECTION_HEADER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\[(.+)#[0-9A-F]{4}\]$").unwrap());

fn hashline_sections(input: &str) -> Vec<HashlineSection<'_>> {
    let mut sections: Vec<HashlineSection<'_>> = Vec::new();
    for line in input.lines() {
        if let Some(path) = section_path(line) {
            sections.push(HashlineSection {
                path,
                lines: Vec::new(),
            });
        } else if let Some(section) = sections.last_mut() {
            section.lines.push(line);
        }
    }
    sections
}

fn section_path(line: &str) -> Option<&str> {
    SECTION_HEADER
        .captures(line)?
        .get(1)
        .map(|path| path.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn calls(source: Source, name: &str, input: Value) -> Vec<(Tool, Value, String)> {
        tool_use_blocks("call_1", name, input, source)
            .into_iter()
            .map(|block| match block {
                ContentBlock::ToolUse {
                    id,
                    name: kept,
                    tool,
                    input,
                } => {
                    assert_eq!(kept, name);
                    (tool, input, id)
                }
                other => panic!("not a tool use: {other:?}"),
            })
            .collect()
    }

    fn single(source: Source, name: &str, input: Value) -> (Tool, Value) {
        let mut calls = calls(source, name, input);
        assert_eq!(calls.len(), 1, "{name} became {calls:?}");
        let (tool, input, id) = calls.remove(0);
        assert_eq!(id, "call_1");
        (tool, input)
    }

    fn pi(name: &str, input: Value) -> (Tool, Value) {
        single(Source::Pi, name, input)
    }

    #[test]
    fn every_pi_tool_name_lands_in_its_bucket() {
        let expected = [
            ("bash", Tool::Shell),
            ("nu", Tool::Shell),
            ("read", Tool::Read),
            ("edit", Tool::Edit),
            ("patch", Tool::Edit),
            ("target_edit", Tool::Edit),
            ("quick_edit", Tool::Edit),
            ("write", Tool::Write),
            ("grep", Tool::Grep),
            ("find", Tool::Glob),
            ("task_plan", Tool::TaskList),
            ("task_update", Tool::TaskList),
            ("task_decompose", Tool::TaskList),
            ("task_complete", Tool::TaskList),
            ("task_evidence", Tool::Other),
            ("task_resume", Tool::Other),
            ("task_list", Tool::Other),
            ("task_checkpoint", Tool::Other),
            ("task_focus", Tool::Other),
        ];
        for (name, tool) in expected {
            assert_eq!(pi(name, json!({})).0, tool, "{name}");
        }
    }

    #[test]
    fn pi_file_tools_address_the_file_as_file_path_and_keep_the_rest() {
        assert_eq!(
            pi(
                "read",
                json!({"path": "src/lib.rs", "offset": 10, "limit": 5})
            )
            .1,
            json!({"file_path": "src/lib.rs", "offset": 10, "limit": 5})
        );
        assert_eq!(
            pi("write", json!({"path": "NEW.md", "content": "# New"})).1,
            json!({"file_path": "NEW.md", "content": "# New"})
        );
        assert_eq!(
            pi("grep", json!({"pattern": "fn main", "path": "src"})).1,
            json!({"pattern": "fn main", "path": "src"})
        );
        assert_eq!(
            pi(
                "bash",
                json!({"command": "ls", "_piToolGuardRoot": "/repo"})
            )
            .1,
            json!({"command": "ls", "_piToolGuardRoot": "/repo"})
        );
        assert_eq!(pi("read", json!({})), (Tool::Read, json!({})));
    }

    #[test]
    fn a_pi_edit_lists_its_replacements_as_one_patch() {
        let (tool, input) = pi(
            "edit",
            json!({
                "path": "src/lib.rs",
                "edits": [
                    {"oldText": "a\nb", "newText": "c"},
                    {"oldText": "", "newText": "d"}
                ]
            }),
        );
        assert_eq!(tool, Tool::Edit);
        assert_eq!(
            input,
            json!({"file_path": "src/lib.rs", "patch": "-a\n-b\n+c\n@@\n+d"})
        );

        assert_eq!(
            pi(
                "patch",
                json!({"path": "a.rs", "edits": [{"old_str": "x", "new_str": "y"}]})
            )
            .1,
            json!({"file_path": "a.rs", "patch": "-x\n+y"})
        );
    }

    #[test]
    fn a_pi_edit_without_replacement_pairs_keeps_its_edits() {
        assert_eq!(
            pi("edit", json!({"path": "a.rs", "edits": "raw edit text"})).1,
            json!({"file_path": "a.rs", "edits": "raw edit text"})
        );
        assert_eq!(
            pi("edit", json!({"path": "a.rs", "edits": []})).1,
            json!({"file_path": "a.rs", "edits": []})
        );
    }

    fn omp(name: &str, input: Value) -> (Tool, Value) {
        single(Source::Omp, name, input)
    }

    const HASHLINE_EDIT: &str =
        "[src/lib.rs#A1B2]\nINS.POST 3:\n+added\n[README.md#C3D4]\nDEL 1\nDEL.BLK 4";

    #[test]
    fn every_omp_tool_name_lands_in_its_bucket() {
        let expected = [
            ("bash", Tool::Shell),
            ("read", Tool::Read),
            ("write", Tool::Write),
            ("glob", Tool::Glob),
            ("todo", Tool::TaskList),
            ("ask", Tool::Other),
        ];
        for (name, tool) in expected {
            assert_eq!(omp(name, json!({"i": "why"})).0, tool, "{name}");
        }
        assert_eq!(
            omp("edit", json!({"input": "[src/lib.rs#A1B2]\nDEL 1"})).0,
            Tool::Edit
        );
    }

    #[test]
    fn omp_file_tools_address_the_file_as_file_path_and_keep_i() {
        assert_eq!(
            omp("read", json!({"path": "src/lib.rs", "i": "look"})).1,
            json!({"file_path": "src/lib.rs", "i": "look"})
        );
        assert_eq!(
            omp(
                "write",
                json!({"path": "NEW.md", "content": "# New", "i": "add"})
            )
            .1,
            json!({"file_path": "NEW.md", "content": "# New", "i": "add"})
        );
        assert_eq!(
            omp("bash", json!({"command": "ls", "i": "list"})).1,
            json!({"command": "ls", "i": "list"})
        );
    }

    #[test]
    fn an_omp_glob_sends_its_pattern_as_path() {
        assert_eq!(
            omp("glob", json!({"path": "src/**/*.rs", "i": "list"})).1,
            json!({"pattern": "src/**/*.rs", "i": "list"})
        );
    }

    #[test]
    fn an_omp_edit_is_one_edit_per_hashline_section() {
        let edits = calls(
            Source::Omp,
            "edit",
            json!({"input": HASHLINE_EDIT, "i": "update regex"}),
        );

        assert_eq!(
            edits,
            vec![
                (
                    Tool::Edit,
                    json!({"file_path": "src/lib.rs", "patch": "INS.POST 3:\n+added", "i": "update regex"}),
                    "call_1".to_owned(),
                ),
                (
                    Tool::Edit,
                    json!({"file_path": "README.md", "patch": "DEL 1\nDEL.BLK 4", "i": "update regex"}),
                    "call_1#2".to_owned(),
                ),
            ]
        );
    }

    #[test]
    fn an_omp_edit_without_a_section_header_stays_other() {
        for input in [
            json!({"input": "+no header", "i": "why"}),
            json!({"input": "[src/lib.rs#a1b2]\n+lowercase tag", "i": "why"}),
            json!({"input": "[src/lib.rs#A1B]\n+short tag", "i": "why"}),
            json!({"i": "no input"}),
            json!("not an object"),
        ] {
            assert_eq!(omp("edit", input.clone()), (Tool::Other, input));
        }
    }

    #[test]
    fn pi_operation_edits_keep_their_operations() {
        assert_eq!(
            pi(
                "target_edit",
                json!({"path": "a.rs", "ops": [{"op": "replace"}]})
            )
            .1,
            json!({"file_path": "a.rs", "ops": [{"op": "replace"}]})
        );
        assert_eq!(
            pi(
                "quick_edit",
                json!({"path": "a.rs", "edits": [{"oldText": "x", "newText": "y"}]})
            )
            .1,
            json!({"file_path": "a.rs", "edits": [{"oldText": "x", "newText": "y"}]})
        );
    }
}
