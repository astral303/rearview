use crate::search::literal::Literal;
use crate::semantic::types::EmbeddedChunk;
use std::borrow::Cow;

/// The version of the semantic turns the session cache stores: what
/// [`filter_turn`] keeps, and which entries and texts
/// [`crate::history::parser::entry_semantic_turn`] passes it. A cache holding
/// another version is emptied and rebuilt: bump it with any change to
/// either.
pub(crate) const SEMANTIC_TURN_VERSION: i64 = 2;

/// The tag pairs whose spans a turn drops, and whose tags left in a block
/// drop the block.
const STRUCTURAL_TAGS: &[(&str, &str)] = &[
    ("<system-reminder>", "</system-reminder>"),
    ("<local-command-caveat>", "</local-command-caveat>"),
    ("<local-command-stdout>", "</local-command-stdout>"),
    ("<local-command-stderr>", "</local-command-stderr>"),
    ("<command-message>", "</command-message>"),
    ("<command-name>", "</command-name>"),
];

/// An assistant block naming one of these and one of [`COMMAND_TERMS`] is a
/// status report, dropped.
const VALIDATION_TERMS: &[&str] = &[
    "validation passed",
    "all checks pass",
    "working tree clean",
    "committed",
    "commit:",
    "validation:",
];
const COMMAND_TERMS: &[&str] = &["cargo test", "just check", "git status", "git diff"];

/// Box-drawing characters: a turn splits into blocks where they start and
/// end, and a block mostly of them is dropped.
const ARTIFACT_CHARS: &[char] = &[
    '─', '━', '│', '┃', '┌', '┐', '└', '┘', '├', '┤', '┬', '┴', '┼', '╭', '╮', '╰', '╯', '═', '║',
    '╔', '╗', '╚', '╝',
];

/// The punctuation a table or code block is mostly made of.
const ARTIFACT_PUNCTUATION: &[char] = &['|', '-', '+', '=', '_', '`'];

/// A block with these beside much punctuation is code, dropped.
const CODE_MARKERS: &[&str] = &[
    "::{",
    "=>",
    "</",
    "<div",
    "function ",
    "const ",
    "let ",
    "fn ",
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SemanticTurnRole {
    User,
    Assistant,
}

pub fn filter_embedded_chunks_by_literals(
    chunks: Vec<EmbeddedChunk>,
    literal_filters: &[Literal],
) -> Vec<EmbeddedChunk> {
    if literal_filters.is_empty() {
        return chunks;
    }

    chunks
        .into_iter()
        .filter(|chunk| matches_every_literal(&chunk.text, literal_filters))
        .collect()
}

pub fn matches_every_literal(text: &str, literal_filters: &[Literal]) -> bool {
    literal_filters.iter().all(|literal| literal.matches(text))
}

pub fn filter_turn(role: SemanticTurnRole, turn: &str) -> Option<String> {
    let turn = slash_command_args(turn).unwrap_or(turn);
    let without_fences = strip_markdown_code_fences(turn);
    let without_tags = strip_structural_tag_spans(without_fences);
    let normalized = normalize_whitespace(&without_tags);
    let mut kept = String::with_capacity(normalized.len());
    for block in split_semantic_blocks(&normalized)
        .into_iter()
        .map(str::trim)
        .filter(|block| !block.is_empty())
        .filter(|block| !is_low_value_turn(block))
        .filter(|block| !is_status_artifact(role, block))
        .filter(|block| !is_artifact_block(block))
    {
        if !kept.is_empty() {
            kept.push(' ');
        }
        kept.push_str(block);
    }
    let normalized = normalize_whitespace(&kept);
    (!normalized.is_empty()).then_some(normalized)
}

/// `text` without its fenced code blocks; borrowed when it doesn't hold a
/// fence.
fn strip_markdown_code_fences(text: &str) -> Cow<'_, str> {
    if !text.contains("```") {
        return Cow::Borrowed(text);
    }
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
    Cow::Owned(output)
}

fn strip_structural_tag_spans(text: Cow<'_, str>) -> Cow<'_, str> {
    if !text.contains('<') {
        return text;
    }
    let mut stripped = text;
    for &(open, close) in STRUCTURAL_TAGS {
        if stripped.contains(open) {
            stripped = Cow::Owned(strip_tag_pair(&stripped, open, close));
        }
    }
    stripped
}

fn strip_tag_pair(text: &str, open: &str, close: &str) -> String {
    let mut output = String::new();
    let mut rest = text;

    while let Some(start) = rest.find(open) {
        output.push_str(&rest[..start]);
        let after_open = &rest[start + open.len()..];
        let Some(end) = after_open.find(close) else {
            rest = "";
            break;
        };
        rest = &after_open[end + close.len()..];
    }

    output.push_str(rest);
    output
}

fn slash_command_args(turn: &str) -> Option<&str> {
    if !turn.contains("<command-name>") || !turn.contains("</command-name>") {
        return None;
    }
    let args_start = turn.find("<command-args>")? + "<command-args>".len();
    let args_end = turn[args_start..].find("</command-args>")? + args_start;
    let args = turn[args_start..args_end].trim();
    (!args.is_empty()).then_some(args)
}

fn split_semantic_blocks(text: &str) -> Vec<&str> {
    let mut blocks = Vec::new();
    let mut start = 0;
    let mut previous_artifact = false;

    for (index, ch) in text.char_indices() {
        let artifact = is_artifact_char(ch);
        if index > start && artifact != previous_artifact {
            blocks.push(&text[start..index]);
            start = index;
        }
        previous_artifact = artifact;
    }
    if start < text.len() {
        blocks.push(&text[start..]);
    }
    blocks
}

/// Unicode's Box Drawing block. Every [`ARTIFACT_CHARS`] entry lies in it.
const BOX_DRAWING: std::ops::RangeInclusive<char> = '\u{2500}'..='\u{257f}';

fn is_artifact_char(ch: char) -> bool {
    BOX_DRAWING.contains(&ch) && ARTIFACT_CHARS.contains(&ch)
}

fn is_low_value_turn(turn: &str) -> bool {
    let trimmed = turn.trim();
    trimmed.is_empty()
        || trimmed.starts_with('/')
        || trimmed.contains('<')
            && STRUCTURAL_TAGS
                .iter()
                .any(|(open, close)| trimmed.contains(open) || trimmed.contains(close))
}

fn is_status_artifact(role: SemanticTurnRole, block: &str) -> bool {
    if role != SemanticTurnRole::Assistant {
        return false;
    }
    // ASCII text lowercases to the same text either way, the ASCII way faster.
    let lower = if block.is_ascii() {
        block.to_ascii_lowercase()
    } else {
        block.to_lowercase()
    };
    contains_any(&lower, VALIDATION_TERMS) && contains_any(&lower, COMMAND_TERMS)
}

fn is_artifact_block(block: &str) -> bool {
    let (mut chars, mut box_chars, mut punctuation, mut alphanumeric) = (0, 0, 0, 0);
    for c in block.chars() {
        chars += usize::from(!c.is_whitespace());
        box_chars += usize::from(is_artifact_char(c));
        punctuation += usize::from(ARTIFACT_PUNCTUATION.contains(&c));
        alphanumeric += usize::from(c.is_alphanumeric());
    }
    if chars == 0 {
        return true;
    }
    if box_chars * 4 >= chars || (box_chars >= 3 && chars <= 24) {
        return true;
    }
    if punctuation * 2 > alphanumeric && block.contains('|') {
        return true;
    }

    punctuation > alphanumeric / 4 && contains_any(block, CODE_MARKERS)
}

fn contains_any(text: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| text.contains(needle))
}

fn normalize_whitespace(text: &str) -> String {
    let mut normalized = String::with_capacity(text.len());
    for word in text.split_whitespace() {
        if !normalized.is_empty() {
            normalized.push(' ');
        }
        normalized.push_str(word);
    }
    normalized
}

#[cfg(test)]
mod reference;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_listed_artifact_char_lies_in_box_drawing() {
        for ch in ARTIFACT_CHARS {
            assert!(
                BOX_DRAWING.contains(ch),
                "{ch:?} lies in the Box Drawing block, so `is_artifact_char` finds it"
            );
        }
    }

    /// Turns that reach each of `filter_turn`'s fast paths, and the branch
    /// beside it.
    const FAST_PATH_TURNS: &[&str] = &[
        "",
        "   \t\n ",
        "plain text with  double  spaces,\ttabs\nand lines\u{3000}wide",
        "before ```rust\nlet x = 1;\n``` after",
        "before ``` never closed",
        "<p>angle bracket without a structural tag</p> and </ closing",
        "keep <system-reminder>drop</system-reminder> keep",
        "keep <local-command-caveat>drop</local-command-caveat> and <command-name>/x</command-name>",
        "keep <local-command-stdout>drop never closed",
        "<command-message>m</command-message> <command-name>/debate</command-name> <command-args>argued text</command-args>",
        "/slash at the start",
        "Validation passed: cargo test and just check. Committed abc123.",
        "VALIDATION PASSED with GIT STATUS clean",
        "validation passed but no command named",
        "wor\u{212a}ing tree clean after git status",
        "\u{130}nside a committed git diff",
        "Use a preview.\n╭────────╮\n│ Search │\n╰────────╯\nIt summarizes.",
        "| a | b |\n|---|---|\n| 1 | 2 |",
        "fn main() { let x = a::{b}; } => done",
        "prose naming function and const without punctuation",
        "ünïcödé text, ünïcödé ∑ symbols — dashes",
    ];

    /// The texts of every provider's every-record session: each search
    /// text, each piece of one between message separators, and each
    /// semantic turn.
    fn every_record_texts() -> Vec<String> {
        let mut texts = Vec::new();
        for session in crate::history::provider::contract_tests::every_record_sessions() {
            let row = &session.row;
            for text in [
                &row.full_text,
                &row.agent_search_text,
                &row.thinking_and_tool_call_text,
            ] {
                texts.push(text.clone());
                texts.extend(text.split('\u{1e}').map(str::to_owned));
            }
            texts.extend(row.semantic_turns.iter().cloned());
        }
        texts
    }

    #[test]
    fn filter_turn_gives_reference_implementations_output_for_fast_path_turns_and_fixture_texts() {
        let texts = every_record_texts();
        assert!(
            texts.len() > 100,
            "the fixtures hold texts: {}",
            texts.len()
        );
        for text in FAST_PATH_TURNS
            .iter()
            .copied()
            .chain(texts.iter().map(String::as_str))
        {
            for role in [SemanticTurnRole::User, SemanticTurnRole::Assistant] {
                assert_eq!(
                    filter_turn(role, text),
                    reference::filter_turn(role, text),
                    "filter_turn keeps what the reference keeps for {role:?} {text:?}"
                );
            }
        }
    }

    /// Fails when the lists [`filter_turn`] reads, or what it keeps of
    /// [`FAST_PATH_TURNS`] in either role, change without a new
    /// [`SEMANTIC_TURN_VERSION`]. A change to an unlisted rule these turns
    /// don't reach, or to the entries and texts the parse passes
    /// `filter_turn`, does not fail it.
    #[test]
    fn filter_lists_and_outputs_change_only_with_semantic_turn_version() {
        use sha2::{Digest, Sha256};
        let mut digest = Sha256::new();
        let mut add = |text: &str| {
            digest.update(text.len().to_le_bytes());
            digest.update(text.as_bytes());
        };
        for (open, close) in STRUCTURAL_TAGS {
            add(open);
            add(close);
        }
        for term in VALIDATION_TERMS
            .iter()
            .chain(COMMAND_TERMS)
            .chain(CODE_MARKERS)
        {
            add(term);
        }
        for ch in ARTIFACT_CHARS.iter().chain(ARTIFACT_PUNCTUATION) {
            add(ch.encode_utf8(&mut [0; 4]));
        }
        for turn in FAST_PATH_TURNS {
            for role in [SemanticTurnRole::User, SemanticTurnRole::Assistant] {
                match filter_turn(role, turn) {
                    Some(kept) => {
                        add("kept");
                        add(&kept);
                    }
                    None => add("dropped"),
                }
            }
        }

        assert_eq!(
            (SEMANTIC_TURN_VERSION, format!("{:x}", digest.finalize())),
            (
                2,
                "7bb928518625c90ab1162db3df0b08a6cbf9bb7881b2a9ab2c1831bd66b24212".to_owned()
            ),
            "filter_turn's lists and outputs change only with a new SEMANTIC_TURN_VERSION: \
             bump it, then record the new digest"
        );
    }

    #[test]
    fn strips_inline_fenced_blocks_and_preserves_surrounding_discussion() {
        let filtered = filter_turn(
            SemanticTurnRole::Assistant,
            "The idea is a detail pane. ```text ╭─ Search ─╮ │ mockup │ ╰──────────╯ ``` It helps users recognize sessions.",
        )
        .unwrap();

        assert!(filtered.contains("detail pane"));
        assert!(filtered.contains("recognize sessions"));
        assert!(!filtered.contains("mockup"));
        assert!(!filtered.contains("```"));
    }

    #[test]
    fn strips_multiline_fenced_blocks() {
        let filtered = filter_turn(
            SemanticTurnRole::Assistant,
            "Semantic cache stores dialogue.\n```rust\nlet secret = tool_output();\n```\nUse generated embeddings.",
        )
        .unwrap();

        assert_eq!(
            filtered,
            "Semantic cache stores dialogue. Use generated embeddings."
        );
    }

    #[test]
    fn drops_box_drawing_artifacts_but_keeps_text_blocks() {
        let filtered = filter_turn(
            SemanticTurnRole::Assistant,
            "Use a responsive preview pane.\n╭────────╮\n│ Search │\n╰────────╯\nIt should summarize the selected conversation.",
        )
        .unwrap();

        assert!(filtered.contains("responsive preview pane"));
        assert!(filtered.contains("selected conversation"));
        assert!(!filtered.contains("╭"));
        assert!(!filtered.contains("Search │"));
    }

    #[test]
    fn strips_structural_tag_spans_without_dropping_surrounding_text() {
        let filtered = filter_turn(
            SemanticTurnRole::Assistant,
            "Useful explanation <system-reminder>noise</system-reminder> more explanation",
        )
        .unwrap();

        assert_eq!(filtered, "Useful explanation more explanation");
    }

    #[test]
    fn command_args_require_command_wrapper() {
        let filtered = filter_turn(
            SemanticTurnRole::User,
            "Discuss literal <command-args>syntax examples</command-args> in docs",
        )
        .unwrap();

        assert!(filtered.contains("Discuss literal"));
        assert!(filtered.contains("syntax examples"));
    }

    #[test]
    fn drops_command_wrapper_turns_without_args() {
        assert_eq!(
            filter_turn(
                SemanticTurnRole::User,
                "<command-message>debate</command-message> <command-name>/debate</command-name>",
            ),
            None
        );
    }

    #[test]
    fn keeps_command_args_without_wrapper_markup() {
        let filtered = filter_turn(
            SemanticTurnRole::User,
            "<command-message>debate</command-message> <command-name>/debate</command-name> <command-args>find robust semantic filtering</command-args>",
        )
        .unwrap();

        assert_eq!(filtered, "find robust semantic filtering");
    }

    #[test]
    fn drops_assistant_validation_status_artifacts() {
        assert_eq!(
            filter_turn(
                SemanticTurnRole::Assistant,
                "Validation passed: cargo test and just check. Committed abc123.",
            ),
            None
        );
    }

    #[test]
    fn preserves_compact_technical_explanations() {
        let filtered = filter_turn(
            SemanticTurnRole::Assistant,
            "The race condition is in scheduler::rebalance() because the cache signature updates before scope ranking.",
        )
        .unwrap();

        assert!(filtered.contains("race condition"));
        assert!(filtered.contains("cache signature"));
    }
}
