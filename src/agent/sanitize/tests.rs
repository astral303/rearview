use super::*;
use crate::history::provider::contract_tests::every_record_sessions;
use serde_json::Value;

#[test]
fn strips_ansi_and_terminal_control_sequences() {
    let input = "safe\u{1b}[31mred\u{1b}[0m\u{1b}]0;title\u{7}text\u{1b}Ppayload\u{1b}\\done";

    assert_eq!(sanitize_agent_text(input), "saferedtextdone");
}

#[test]
fn preserves_unicode_line_breaks_and_tabs() {
    assert_eq!(sanitize_agent_text("α\tβ\nγ\u{0}δ\u{7f}"), "α\tβ\nγδ");
}

/// Pieces of text and of terminal codes, each terminated and unterminated
/// where it can be.
const FRAGMENTS: &[&str] = &[
    "word",
    "é中😀",
    " ",
    "\r\n",
    "\n",
    "\t",
    "\u{a0}",
    "\u{1b}[31m",
    "\u{1b}[1;2",
    "\u{1b}]0;title\u{7}",
    "\u{1b}]8;;url\u{1b}\\",
    "\u{1b}]0;unterminated",
    "\u{1b}Pdcs\u{1b}\\",
    "\u{1b}^pm\u{7}",
    "\u{1b}_apc\u{1b}\\",
    "\u{1b}_é",
    "\u{1b}",
    "\u{1b}c",
    "\u{1b}\u{1b}",
    "\u{0}\u{7}\u{8}\u{7f}",
    "\u{85}\u{9b}",
    "[0m",
    "\\",
];

/// Every sequence of up to three fragments.
fn generated_inputs() -> Vec<String> {
    let mut inputs = vec![String::new()];
    let mut previous = vec![String::new()];
    for _ in 0..3 {
        let next = previous
            .iter()
            .flat_map(|prefix| {
                FRAGMENTS
                    .iter()
                    .map(move |fragment| format!("{prefix}{fragment}"))
            })
            .collect::<Vec<_>>();
        inputs.extend(next.iter().cloned());
        previous = next;
    }
    inputs
}

/// Every string an entry of each provider's every-record session holds.
fn fixture_texts() -> Vec<String> {
    fn collect(value: &Value, texts: &mut Vec<String>) {
        match value {
            Value::String(text) => texts.push(text.clone()),
            Value::Array(items) => items.iter().for_each(|item| collect(item, texts)),
            Value::Object(object) => object.values().for_each(|item| collect(item, texts)),
            _ => {}
        }
    }
    let mut texts = Vec::new();
    for session in every_record_sessions() {
        let row = &session.row;
        let entries = crate::history::normalized_session(
            crate::history::FOR_FIXTURES,
            row.source,
            &row.path,
            &row.subagents,
        )
        .unwrap();
        for (_, entry) in &entries.entries {
            collect(&serde_json::to_value(entry).unwrap(), &mut texts);
        }
    }
    texts
}

#[test]
fn split_text_is_sanitised_and_joins_back_to_the_input() {
    for input in generated_inputs().iter().chain(&fixture_texts()) {
        let split = split_terminal_codes(input);
        assert_eq!(
            split.text,
            sanitize_agent_text(input),
            "the split text is the sanitised text of {input:?}"
        );
        let joined = match &split.codes {
            Some(codes) => join_terminal_codes(&split.text, codes).unwrap(),
            None => split.text.clone().into_owned(),
        };
        assert_eq!(&joined, input, "the split text and codes join back");
    }
}

#[test]
fn text_without_codes_splits_into_itself_without_codes() {
    let split = split_terminal_codes("plain é\ttext\n");
    assert!(
        matches!(split.text, Cow::Borrowed("plain é\ttext\n")),
        "the text is borrowed as it is"
    );
    assert!(split.codes.is_none(), "a text without codes stores none");
}

#[test]
fn adjacent_codes_join_back_at_one_offset_in_order() {
    let input = "a\u{1b}[1m\u{1b}[31m\u{7}b\u{1b}]0;unterminated";
    let split = split_terminal_codes(input);
    let codes = split.codes.expect("the text holds codes");

    assert_eq!(split.text, "ab");
    assert_eq!(
        codes,
        [
            &[1, 10][..],
            b"\x1b[1m\x1b[31m\x07",
            &[1, 16],
            b"\x1b]0;unterminated"
        ]
        .concat(),
        "each run is stored once, after the text before it"
    );
    assert_eq!(join_terminal_codes(&split.text, &codes).unwrap(), input);
}

#[test]
fn codes_past_their_text_are_malformed() {
    let split = split_terminal_codes("abc\u{1b}[0m");
    let codes = split.codes.unwrap();

    assert!(
        join_terminal_codes("ab", &codes).is_err(),
        "codes after the end of a shorter text are malformed"
    );
    assert!(
        join_terminal_codes("abc", &codes[..codes.len() - 1]).is_err(),
        "a truncated run is malformed"
    );
}
