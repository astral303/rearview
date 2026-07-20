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

fn project(config: &Path) -> PathBuf {
    let project = config.join("projects").join("-tmp-agent-phase3-tests");
    std::fs::create_dir_all(&project).expect("create project");
    project
}

fn write_transcript(path: &Path, needle: &str) {
    let user = serde_json::json!({
        "type": "user",
        "timestamp": "2026-07-20T00:00:00Z",
        "cwd": "/tmp/agent-phase3-tests",
        "message": {"role": "user", "content": needle}
    });
    let assistant = serde_json::json!({
        "type": "assistant",
        "timestamp": "2026-07-20T00:00:01Z",
        "message": {"role": "assistant", "content": [{"type": "text", "text": "answer"}]}
    });
    std::fs::write(path, format!("{user}\n{assistant}\n")).expect("write transcript");
}

fn first_ref(output: &[u8]) -> String {
    String::from_utf8_lossy(output)
        .split_whitespace()
        .find_map(|field| field.strip_prefix("ref="))
        .expect("search ref")
        .trim_end_matches(|character: char| !character.is_ascii_hexdigit())
        .to_string()
}

#[test]
fn malformed_and_missing_refs_have_structured_stderr_and_nonzero_exit() {
    let config = tempfile::tempdir().expect("config");
    project(config.path());

    let invalid = run(config.path(), &["agent", "read", "not-a-ref"]);
    assert!(!invalid.status.success());
    assert!(invalid.stdout.is_empty());
    assert!(
        String::from_utf8_lossy(&invalid.stderr)
            .starts_with("protocol agent-error v=1 kind=invalid-ref ref=not-a-ref")
    );

    let missing = run(config.path(), &["agent", "read", "ch_12345678"]);
    assert!(!missing.status.success());
    assert!(
        String::from_utf8_lossy(&missing.stderr)
            .starts_with("protocol agent-error v=1 kind=not-found ref=ch_12345678")
    );
}

#[test]
fn target_transcript_and_range_failures_have_precise_kinds() {
    let config = tempfile::tempdir().expect("config");
    let transcript = project(config.path()).join("12345678-1234-4234-9234-123456789abc.jsonl");
    write_transcript(&transcript, "phase three needle");

    let search = run(
        config.path(),
        &["agent", "search", "--lexical", "phase three needle"],
    );
    assert!(
        search.status.success(),
        "{}",
        String::from_utf8_lossy(&search.stderr)
    );
    let reference = first_ref(&search.stdout);

    let range = run(
        config.path(),
        &["agent", "read", &format!("{reference}:m99")],
    );
    assert!(!range.status.success());
    assert!(String::from_utf8_lossy(&range.stderr).starts_with(&format!(
        "protocol agent-error v=1 kind=out-of-range ref={reference}"
    )));

    std::fs::write(&transcript, "{malformed\n").expect("malform transcript");
    let malformed = run(config.path(), &["agent", "read", &reference]);
    assert!(!malformed.status.success());
    assert!(
        String::from_utf8_lossy(&malformed.stderr).starts_with(&format!(
            "protocol agent-error v=1 kind=malformed-transcript ref={reference}"
        ))
    );
}

#[test]
fn search_reports_partial_warnings_and_preserves_compact_success_output() {
    let config = tempfile::tempdir().expect("config");
    let project = project(config.path());
    write_transcript(
        &project.join("12345678-1234-4234-9234-123456789abc.jsonl"),
        "warning contract needle",
    );
    std::fs::write(
        project.join("87654321-1234-4234-9234-123456789abc.jsonl"),
        "{malformed\n",
    )
    .expect("write malformed transcript");

    let output = run(
        config.path(),
        &["agent", "search", "--lexical", "warning contract needle"],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.starts_with("protocol agent-search v=3 mode=lexical"));
    assert!(stdout.contains("protocol agent-warning v=1 kind=malformed-transcript ref=ch_"));
    assert!(stdout.contains("read ref=ch_"));
}

#[test]
fn agent_filesystem_failures_use_io_envelope() {
    let config = tempfile::tempdir().expect("config");
    std::fs::write(config.path().join("projects"), "not a directory").expect("write projects file");

    let output = run(config.path(), &["agent", "search", "--lexical", "needle"]);
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).starts_with("protocol agent-error v=1 kind=io")
    );
}
