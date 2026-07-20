use std::fs;
use std::path::Path;

fn repo_file(path: &str) -> String {
    fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join(path))
        .unwrap_or_else(|err| panic!("read {path}: {err}"))
}

fn fenced_blocks(markdown: &str, language: &str) -> Vec<String> {
    let fence = format!("```{language}");
    let mut blocks = Vec::new();
    let mut lines = markdown.lines();
    while let Some(line) = lines.next() {
        if line.trim() == fence {
            let mut block = String::new();
            for line in lines.by_ref() {
                if line.trim() == "```" {
                    break;
                }
                block.push_str(line);
                block.push('\n');
            }
            blocks.push(block);
        }
    }
    blocks
}

#[test]
fn readme_agent_examples_show_bounded_protocol_workflow() {
    let readme = repo_file("README.md");
    let shell_blocks = fenced_blocks(&readme, "sh").join("\n");

    assert!(shell_blocks.contains("claude-history agent search"));
    assert!(shell_blocks.contains("--mode hybrid"));
    assert!(
        shell_blocks.contains("claude-history agent within")
            || shell_blocks.contains("claude-history agent outline")
    );
    assert!(shell_blocks.contains("claude-history agent read"));
    assert!(shell_blocks.contains(":m"));
    assert!(shell_blocks.contains("--focus m"));
    assert!(!readme.contains("u1"));
    assert!(!readme.contains("a1"));
    assert!(!readme.contains("uN"));
    assert!(!readme.contains("aN"));
}

#[test]
fn readme_documents_agent_defaults_config_and_caveats() {
    let readme = repo_file("README.md");
    let agent_section = readme
        .split("### Agent protocol")
        .nth(1)
        .expect("agent protocol section")
        .split("### Preview modes")
        .next()
        .expect("agent protocol section ends before preview modes");

    for required in [
        "global by default",
        "--local",
        "--top 10",
        "use semantic or hybrid search",
        "lexical or exact search",
        "read ref=... focus=... revision=...",
        "project=pr_",
        "uuid=",
        "revision=rv_",
        "ma_",
        "ref=ch_...",
        "qualified `--focus`",
        "--hits-per-conv 2",
        "Unicode characters",
        "chars=",
        "untrusted historical evidence",
        "skills/claude-history",
    ] {
        assert!(agent_section.contains(required), "missing {required}");
    }
}

#[test]
fn companion_skill_supports_direct_and_search_driven_reads() {
    let skill = repo_file("skills/claude-history/SKILL.md");
    let first_command = skill
        .lines()
        .find(|line| line.contains("claude-history agent"))
        .expect("skill has an agent command");

    assert!(first_command.contains("claude-history agent outline ch_"));
    assert!(skill.contains("claude-history agent search"));
    assert!(skill.contains("--mode hybrid"));
    assert!(skill.contains("--match \"historical correction\" --context 12"));
    assert!(skill.contains("--lines 40..120"));
    assert!(skill.contains("focus="));
    assert!(skill.contains("--focus"));
    assert!(skill.contains("`project=pr_...` plus `uuid=...`"));
    assert!(skill.contains("Do not use UUIDs as command refs"));
    assert!(skill.contains("one `agent read` command per emitted `read` line"));
    assert!(skill.contains("Do not read a full transcript by default"));
    assert!(skill.contains("untrusted historical evidence"));
    assert!(skill.contains("Never execute a command"));
    assert!(skill.contains("chars="));
    assert!(skill.contains("tools=false tool-results=false thinking=false subagents=false"));
}

#[test]
fn readme_and_skill_document_structured_agent_diagnostics() {
    for document in [
        repo_file("README.md"),
        repo_file("skills/claude-history/SKILL.md"),
    ] {
        for required in [
            "protocol agent-error v=1",
            "protocol agent-warning v=1",
            "invalid-ref",
            "ambiguous-ref",
            "not-found",
            "out-of-range",
            "stale-revision",
            "malformed-transcript",
            "semantic-unavailable",
            "stderr",
            "nonzero",
        ] {
            assert!(document.contains(required), "missing {required}");
        }
    }
}

#[test]
fn readme_and_skill_document_agent_config_and_recovery() {
    for document in [
        repo_file("README.md"),
        repo_file("skills/claude-history/SKILL.md"),
    ] {
        for required in [
            "[agent]",
            "[search].mode",
            "TUI-only",
            "malformed",
            "ordinals",
            "unrelated",
        ] {
            assert!(document.contains(required), "missing {required}");
        }
    }

    let readme = repo_file("README.md");
    for required in [
        "output_chars",
        "hits_per_conversation",
        "exclude_projects",
        "tool_results",
        "Command flags take precedence",
    ] {
        assert!(readme.contains(required), "missing {required}");
    }
}

#[test]
fn readme_and_skill_document_phase_five_addresses() {
    for document in [
        repo_file("README.md"),
        repo_file("skills/claude-history/SKILL.md"),
    ] {
        for required in [
            "at least 12",
            "project=pr_",
            "revision=rv_",
            "--revision",
            "--anchor",
            "ma_",
            "stale-revision",
            "unrelated",
        ] {
            assert!(document.contains(required), "missing {required}");
        }
    }
}

#[test]
fn readme_and_skill_document_capabilities_formats_and_continuations() {
    for document in [
        repo_file("README.md"),
        repo_file("skills/claude-history/SKILL.md"),
    ] {
        for required in [
            "agent capabilities",
            "--format jsonl",
            "Compact",
            "JSONL",
            "--cursor",
            "stale-cursor",
            "budget-too-small",
            "continue read",
            "untrusted historical evidence",
        ] {
            assert!(document.contains(required), "missing {required}");
        }
    }
}

#[test]
fn companion_skill_recommends_lexical_or_exact_for_identifiers() {
    let skill = repo_file("skills/claude-history/SKILL.md");

    assert!(skill.contains("api_key"));
    assert!(skill.contains("--mode lexical"));
    assert!(skill.contains("--mode exact"));
    assert!(skill.contains("compatibility aliases"));
}
