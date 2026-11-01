mod common;

use std::path::Path;

#[test]
fn debug_search_reports_each_stage_and_evidence_row_count() {
    let config = tempfile::tempdir().expect("config");
    let home = tempfile::tempdir().expect("home");
    let project = config.path().join("projects").join("-tmp-debug-search");
    std::fs::create_dir_all(&project).expect("create project");
    // Evidence covers a match the row's preview hides. A needle in the opening
    // question is hidden: the default preview holds the last three messages.
    write_session(&project, "first", "needle question", "answer");
    write_session(&project, "second", "needle question", "answer");
    write_session(&project, "visible", "question", "needle answer");

    let output = common::isolated_rearview(config.path())
        .env("CLAUDE_CONFIG_DIR", config.path())
        // With the home directory here, the user's config.toml is not read.
        .env("HOME", home.path())
        .env("USERPROFILE", home.path())
        .args(["--debug-search", "needle"])
        .output()
        .expect("run rearview");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "the search succeeds: {stderr}");
    let records = stderr
        .lines()
        .filter(|line| line.starts_with("perf "))
        .map(mask_milliseconds)
        .collect::<Vec<_>>();
    assert_eq!(
        records,
        [
            "perf stage=search-data ms=<n>",
            "perf stage=search ms=<n>",
            "perf stage=evidence ms=<n>",
            "perf evidence rows=2",
            "perf total ms=<n>",
        ],
        "one record per stage in the order they ran, then evidence for the two \
         sessions whose preview hides the needle, then the total:\n{stderr}"
    );
}

/// `record` with a whole number of milliseconds after `ms=` replaced by
/// `<n>`. Any other value after `ms=` stays, so the comparison shows it.
fn mask_milliseconds(record: &str) -> String {
    match record.split_once(" ms=") {
        Some((head, milliseconds)) if milliseconds.parse::<u128>().is_ok() => {
            format!("{head} ms=<n>")
        }
        _ => record.to_string(),
    }
}

/// A session of two exchanges: `opening` asked first, `last_answer` replied
/// last.
fn write_session(project: &Path, name: &str, opening: &str, last_answer: &str) {
    let user = |second: u32, text: &str| {
        serde_json::json!({
            "type": "user",
            "timestamp": format!("2026-07-20T00:00:{second:02}Z"),
            "cwd": "/tmp/debug-search",
            "message": {"role": "user", "content": text}
        })
    };
    let assistant = |second: u32, text: &str| {
        serde_json::json!({
            "type": "assistant",
            "timestamp": format!("2026-07-20T00:00:{second:02}Z"),
            "message": {"role": "assistant", "content": [{"type": "text", "text": text}]}
        })
    };
    let records = [
        user(0, opening),
        assistant(1, "answer"),
        user(2, "later question"),
        assistant(3, last_answer),
    ];
    let transcript = records
        .iter()
        .map(|record| format!("{record}\n"))
        .collect::<String>();
    std::fs::write(project.join(format!("{name}.jsonl")), transcript).expect("write transcript");
}
