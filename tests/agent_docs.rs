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

    assert!(shell_blocks.contains("claude-history agent search --hybrid"));
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
        "read ref=... focus=...",
        "uuid=",
        "ref=ch_...",
        "qualified `--focus`",
        "--hits-per-conv 2",
        "skills/claude-history-search",
    ] {
        assert!(agent_section.contains(required), "missing {required}");
    }
}

#[test]
fn companion_skill_starts_with_search_and_preserves_focus() {
    let skill = repo_file("skills/claude-history-search/SKILL.md");
    let first_command = skill
        .lines()
        .find(|line| line.contains("claude-history agent"))
        .expect("skill has an agent command");

    assert!(first_command.contains("claude-history agent search --hybrid"));
    assert!(skill.contains("focus="));
    assert!(skill.contains("--focus"));
    assert!(skill.contains("Use `uuid=` when reporting"));
    assert!(skill.contains("Do not use UUIDs as command refs"));
    assert!(skill.contains("one `agent read` command per emitted `read` line"));
    assert!(skill.contains("Do not read a full transcript by default"));
}

#[test]
fn companion_skill_recommends_lexical_or_exact_for_identifiers() {
    let skill = repo_file("skills/claude-history-search/SKILL.md");

    assert!(skill.contains("api_key"));
    assert!(skill.contains("--lexical"));
    assert!(skill.contains("--exact"));
}
