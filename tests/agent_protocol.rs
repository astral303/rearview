use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_claude-history"))
}

fn run(config: &Path, args: &[&str]) -> Output {
    Command::new(binary())
        .env("CLAUDE_CONFIG_DIR", config)
        .args(args)
        .output()
        .expect("run claude-history")
}

fn json_lines(output: &[u8]) -> Vec<Value> {
    String::from_utf8(output.to_vec())
        .expect("UTF-8 output")
        .lines()
        .map(|line| serde_json::from_str(line).expect("valid JSONL record"))
        .collect()
}

fn write_search_transcript(config: &Path) {
    let project = config.join("projects").join("-tmp-agent-protocol-tests");
    std::fs::create_dir_all(&project).expect("create project");
    let path = project.join("12345678-1234-4234-9234-123456789abc.jsonl");
    let lines = (1..=12)
        .map(|ordinal| {
            serde_json::json!({
                "type": if ordinal % 2 == 0 { "assistant" } else { "user" },
                "timestamp": format!("2026-07-20T00:00:{ordinal:02}Z"),
                "cwd": "/tmp/agent-protocol-tests",
                "message": {
                    "role": if ordinal % 2 == 0 { "assistant" } else { "user" },
                    "content": format!("unicode needle 界 evidence number {ordinal}")
                }
            })
            .to_string()
        })
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(path, lines + "\n").expect("write transcript");
}

#[test]
fn capabilities_match_compact_and_jsonl_golden_fixtures() {
    let config = tempfile::tempdir().expect("config");
    let compact = run(config.path(), &["agent", "capabilities"]);
    assert!(compact.status.success());
    assert_eq!(
        String::from_utf8(compact.stdout).unwrap(),
        include_str!("fixtures/agent/capabilities.compact")
    );

    let jsonl = run(
        config.path(),
        &["agent", "capabilities", "--format", "jsonl"],
    );
    assert!(jsonl.status.success());
    assert_eq!(
        String::from_utf8(jsonl.stdout.clone()).unwrap(),
        include_str!("fixtures/agent/capabilities.jsonl")
    );
    let records = json_lines(&jsonl.stdout);
    assert_eq!(records[0]["type"], "capabilities");
    assert_eq!(records[0]["protocol"]["compatibility"], "same-major");
}

#[test]
fn capabilities_commands_and_cli_help_remain_in_sync() {
    let config = tempfile::tempdir().expect("config");
    let capabilities = run(
        config.path(),
        &["agent", "capabilities", "--format", "jsonl"],
    );
    let records = json_lines(&capabilities.stdout);
    let commands = records[0]["commands"].as_array().unwrap();
    let help = run(config.path(), &["agent", "--help"]);
    let help = String::from_utf8(help.stdout).unwrap();
    for command in commands {
        let name = command["command"].as_str().unwrap();
        assert!(help.contains(name), "agent help lacks {name}");
    }
    for command in ["search", "within", "outline", "read"] {
        let help = run(config.path(), &["agent", command, "--help"]);
        let help = String::from_utf8(help.stdout).unwrap();
        assert!(help.contains("--format <FORMAT>"));
    }
}

#[test]
fn jsonl_search_cursor_continues_and_rejects_stale_input() {
    let config = tempfile::tempdir().expect("config");
    write_search_transcript(config.path());
    let first = run(
        config.path(),
        &[
            "agent",
            "search",
            "--lexical",
            "--flat",
            "--top",
            "8",
            "--budget",
            "1300",
            "--format",
            "jsonl",
            "unicode needle",
        ],
    );
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert!(String::from_utf8_lossy(&first.stdout).chars().count() <= 1300);
    let first_records = json_lines(&first.stdout);
    assert_eq!(first_records[0]["type"], "header");
    assert_eq!(first_records[0]["cut"], "tail");
    let cursor = first_records
        .iter()
        .find(|record| record["type"] == "continuation")
        .and_then(|record| record["cursor"].as_str())
        .expect("continuation cursor");

    let second = run(
        config.path(),
        &[
            "agent",
            "search",
            "--lexical",
            "--flat",
            "--top",
            "8",
            "--budget",
            "1300",
            "--format",
            "jsonl",
            "--cursor",
            cursor,
            "unicode needle",
        ],
    );
    assert!(
        second.status.success(),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    let second_records = json_lines(&second.stdout);
    assert!(second_records[0]["position"].as_u64().unwrap() > 0);

    let stale = run(
        config.path(),
        &[
            "agent",
            "search",
            "--lexical",
            "--flat",
            "--top",
            "8",
            "--budget",
            "1300",
            "--format",
            "jsonl",
            "--cursor",
            cursor,
            "different query",
        ],
    );
    assert!(!stale.status.success());
    let errors = json_lines(&stale.stderr);
    assert_eq!(errors[0]["type"], "error");
    assert_eq!(errors[0]["kind"], "stale-cursor");
}
