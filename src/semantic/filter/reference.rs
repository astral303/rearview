//! The tests compare [`super::filter_turn`] with this copy of it as written
//! before its fast paths. The helpers the fast paths left unchanged are the
//! production ones.

use super::{
    SemanticTurnRole, contains_any, is_artifact_char, slash_command_args, split_semantic_blocks,
    strip_tag_pair,
};

pub(super) fn filter_turn(role: SemanticTurnRole, turn: &str) -> Option<String> {
    let turn = slash_command_args(turn).unwrap_or(turn);
    let without_fences = strip_markdown_code_fences(turn);
    let without_tags = strip_structural_tag_spans(&without_fences);
    let normalized = normalize_whitespace(&without_tags);
    let kept = split_semantic_blocks(&normalized)
        .into_iter()
        .map(str::trim)
        .filter(|block| !block.is_empty())
        .filter(|block| !is_low_value_turn(block))
        .filter(|block| !is_status_artifact(role, block))
        .filter(|block| !is_artifact_block(block))
        .collect::<Vec<_>>()
        .join(" ");
    let normalized = normalize_whitespace(&kept);
    (!normalized.is_empty()).then_some(normalized)
}

fn strip_markdown_code_fences(text: &str) -> String {
    let mut output = String::new();
    let mut rest = text;

    while let Some(start) = rest.find("```") {
        output.push_str(&rest[..start]);
        let after_open = &rest[start + 3..];
        let Some(end) = after_open.find("```") else {
            rest = "";
            break;
        };
        rest = &after_open[end + 3..];
    }

    output.push_str(rest);
    output
}

fn strip_structural_tag_spans(text: &str) -> String {
    let mut stripped = text.to_string();
    for (open, close) in [
        ("<system-reminder>", "</system-reminder>"),
        ("<local-command-caveat>", "</local-command-caveat>"),
        ("<local-command-stdout>", "</local-command-stdout>"),
        ("<local-command-stderr>", "</local-command-stderr>"),
        ("<command-message>", "</command-message>"),
        ("<command-name>", "</command-name>"),
    ] {
        stripped = strip_tag_pair(&stripped, open, close);
    }
    stripped
}

fn is_low_value_turn(turn: &str) -> bool {
    let trimmed = turn.trim();
    trimmed.is_empty()
        || trimmed.starts_with('/')
        || contains_any(
            trimmed,
            &[
                "<system-reminder>",
                "</system-reminder>",
                "<local-command-caveat>",
                "</local-command-caveat>",
                "<local-command-stdout>",
                "</local-command-stdout>",
                "<local-command-stderr>",
                "</local-command-stderr>",
                "<command-message>",
                "</command-message>",
                "<command-name>",
                "</command-name>",
            ],
        )
}

fn is_status_artifact(role: SemanticTurnRole, block: &str) -> bool {
    if role != SemanticTurnRole::Assistant {
        return false;
    }
    let lower = block.to_lowercase();
    let validation_terms = [
        "validation passed",
        "all checks pass",
        "working tree clean",
        "committed",
        "commit:",
        "validation:",
    ];
    let has_validation = validation_terms.iter().any(|term| lower.contains(term));
    let command_terms = ["cargo test", "just check", "git status", "git diff"];
    let command_hits = command_terms
        .iter()
        .filter(|term| lower.contains(**term))
        .count();
    has_validation && command_hits > 0
}

fn is_artifact_block(block: &str) -> bool {
    let chars = block.chars().filter(|c| !c.is_whitespace()).count();
    if chars == 0 {
        return true;
    }

    let box_chars = block.chars().filter(|c| is_artifact_char(*c)).count();
    if box_chars * 4 >= chars || (box_chars >= 3 && chars <= 24) {
        return true;
    }

    let punctuation = block
        .chars()
        .filter(|c| matches!(c, '|' | '-' | '+' | '=' | '_' | '`'))
        .count();
    let alphanumeric = block.chars().filter(|c| c.is_alphanumeric()).count();
    if punctuation * 2 > alphanumeric && block.contains('|') {
        return true;
    }

    let code_markers = [
        "::{",
        "=>",
        "</",
        "<div",
        "function ",
        "const ",
        "let ",
        "fn ",
    ];
    contains_any(block, &code_markers) && punctuation > alphanumeric / 4
}

fn normalize_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<&str>>().join(" ")
}
