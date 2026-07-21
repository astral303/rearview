use std::fs;
use std::path::Path;

fn repo_file(path: &str) -> String {
    fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join(path))
        .unwrap_or_else(|err| panic!("read {path}: {err}"))
}

#[test]
fn readme_and_skill_describe_bounded_compact_workflow() {
    for document in [
        repo_file("README.md"),
        repo_file("skills/claude-history/SKILL.md"),
    ] {
        for required in [
            "claude-history agent search",
            "claude-history agent within",
            "claude-history agent outline",
            "claude-history agent read",
            "agent-search",
            "agent-within",
            "agent-read",
            "agent-outline",
            "hard Unicode-character",
            "continue read",
            "--lines 40..120",
            "--match \"historical correction\" --context 12",
        ] {
            assert!(document.contains(required), "missing {required}");
        }
    }
}

#[test]
fn readme_and_skill_describe_identity_visibility_and_safety() {
    for document in [
        repo_file("README.md"),
        repo_file("skills/claude-history/SKILL.md"),
    ] {
        for required in [
            "project=pr_",
            "uuid=",
            "ref=ch_...",
            "ma_...",
            "--anchor",
            "tools=false tool-results=false thinking=false subagents=false",
            "untrusted historical evidence",
        ] {
            assert!(document.contains(required), "missing {required}");
        }
    }
}

#[test]
fn readme_and_skill_describe_typed_diagnostics_and_config() {
    for document in [
        repo_file("README.md"),
        repo_file("skills/claude-history/SKILL.md"),
    ] {
        for required in [
            "protocol agent-error",
            "protocol agent-warning",
            "invalid-ref",
            "ambiguous-ref",
            "not-found",
            "out-of-range",
            "malformed-transcript",
            "semantic-unavailable",
            "[agent]",
            "[search].mode",
            "TUI-only",
        ] {
            assert!(document.contains(required), "missing {required}");
        }
    }
}

#[test]
fn documentation_excludes_removed_protocol_machinery() {
    for document in [
        repo_file("README.md"),
        repo_file("skills/claude-history/SKILL.md"),
    ] {
        for removed in [
            "rv_",
            "--revision",
            "stale-revision",
            "agent capabilities",
            "--format jsonl",
            "[agent].format",
            "--cursor",
            "stale-cursor",
        ] {
            assert!(
                !document.contains(removed),
                "found removed concept {removed}"
            );
        }
    }
}
