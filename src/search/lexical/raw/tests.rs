use super::*;
use crate::history::parser::normalize_whitespace;
use crate::search::lexical::{count_adjacent_pairs, count_prefix_matches, normalize_for_search};

/// The counts matching the normalised text finds, which the raw matcher must
/// reproduce: whitespace collapsed, then lowercased with separators as
/// spaces.
fn normalised_counts(
    text: &str,
    words: &[&str],
    pairs: &[String],
    literals: &[Literal],
) -> (Vec<bool>, Vec<usize>, usize, Vec<bool>, bool) {
    let collapsed = normalize_whitespace(text);
    let normalised = normalize_for_search(&collapsed);
    (
        words.iter().map(|word| normalised.contains(word)).collect(),
        words
            .iter()
            .map(|word| count_prefix_matches(&normalised, word, PREFIX_CAP))
            .collect(),
        count_adjacent_pairs(&normalised, pairs, ADJACENCY_CAP),
        literals
            .iter()
            .map(|literal| literal.matches(&collapsed))
            .collect(),
        !collapsed.is_empty(),
    )
}

fn raw_counts(
    text: &str,
    words: &[&str],
    pairs: &[String],
    literals: &[Literal],
) -> (Vec<bool>, Vec<usize>, usize, Vec<bool>, bool) {
    let query = RawQuery::new(words, pairs, literals);
    let mut matcher = query.matcher();
    let mut counts = matcher.counts();
    matcher.count(text, &mut counts);
    (
        counts.contains,
        counts.prefixes,
        counts.adjacency,
        counts.literals,
        counts.has_text,
    )
}

const TEXTS: &[&str] = &[
    "",
    "   ",
    "Hello World, hello world: HELLO\tWORLD",
    "hello_world-foo/bar and foo - bar, foo-bar, foo  bar, foo_ bar",
    "  leading and trailing words  ",
    "tabs\tand\nnewlines\nmixed\n\nlines",
    "\u{130}stanbul istanbul ISTANBUL \u{130}word",
    "KELVIN \u{212A}elvin k \u{212A} kk \u{212A}word",
    "ß STRASSE straße Straße",
    "ΣΑΣ σας final sigma ΟΔΟΣ οδος",
    "SIGTERM\u{FF0C}属于这个 SIGTERM 属于",
    "中文\u{3000}测试\u{3001}标点\u{3002}中文测试",
    "foo\u{a0}bar foo\u{2003}\u{a0} bar foo\u{a0}\n bar",
    "aaa aa aaaa abab ababab a a a a",
    "éword Éword word 1word _word",
    "first item ends with alpha\nbeta starts the second",
    "x\n\ny",
    "word word word word word word word word word word word word a a a a a",
    "word",
    "WORD",
    "hello world",
    "swordfish sWord wordy keyword w wo wor",
    "éword wordé 中word word中 ÉWORD—WORD",
    "hello\u{2014}world hello\u{a0}WORLD hello_World",
    "The theme: then THE thesis; other bathe tHe eee e ee eE Ee",
    "cache  \t\n refresh, cache_refresh cache-refresh cache / refresh cacherefresh Cache\u{2003}REFRESH",
    "naïve café Naïve CAFÉ cafés xcafé",
];

const QUERIES: &[&str] = &[
    "word",
    "hello world",
    "foo bar",
    "istanbul",
    "i",
    "k",
    "kelvin",
    "elvin",
    "strasse",
    "straße",
    "σας",
    "οδος",
    "a",
    "aa",
    "abab",
    "a a",
    "alpha beta",
    "中文",
    "测试 标点",
    "sigterm 属于",
    "leading and trailing",
    "mixed lines",
    "b",
    "x y",
    "the",
    "t",
    "e",
    "ee",
    "the theme",
    "cache refresh",
    "café",
    "naïve café",
];

const LITERALS: &[&str] = &[
    "Hello World",
    "hello world",
    "alpha beta",
    "a b",
    "İstanbul",
    "foo bar",
    "x y",
    "ab",
    "trailing words",
    " leading",
    "kk",
];

#[test]
fn raw_counts_equal_counts_over_normalised_text() {
    let literals: Vec<Literal> = LITERALS
        .iter()
        .map(|literal| Literal::new((*literal).to_owned()))
        .collect();
    for query in QUERIES {
        let normalised = normalize_for_search(query);
        let words: Vec<&str> = normalised.split_whitespace().collect();
        let pairs: Vec<String> = words
            .windows(2)
            .map(|pair| format!("{} {}", pair[0], pair[1]))
            .collect();
        for text in TEXTS {
            assert_eq!(
                raw_counts(text, &words, &pairs, &literals),
                normalised_counts(text, &words, &pairs, &literals),
                "the raw counts equal the normalised counts: {query:?} in {text:?}"
            );
        }
    }
}

#[test]
fn normalising_keeps_whether_each_character_is_alphanumeric_outside_dotted_capital_i() {
    let changed = (0..=u32::from(char::MAX))
        .filter_map(char::from_u32)
        .filter(|ch| {
            let normalised = if is_word_separator(*ch) {
                ' '
            } else {
                ch.to_lowercase().last().unwrap_or(*ch)
            };
            normalised.is_alphanumeric() != ch.is_alphanumeric()
        })
        .collect::<Vec<_>>();

    assert_eq!(
        changed,
        ['\u{130}'],
        "only the dotted capital I normalises to a character of another class"
    );
}

#[test]
fn counts_stop_at_their_caps_across_texts() {
    let words = ["word"];
    let pairs = ["word word".to_owned()];
    let query = RawQuery::new(&words, &pairs, &[]);
    let mut matcher = query.matcher();
    let mut counts = matcher.counts();
    let text = "word word word word";
    for _ in 0..4 {
        matcher.count(text, &mut counts);
    }
    assert_eq!(
        (counts.prefixes[0], counts.adjacency),
        (PREFIX_CAP, ADJACENCY_CAP),
        "the counts summed over texts stop at their caps"
    );
    assert!(
        matcher.is_complete(&counts),
        "the counts are complete at their caps"
    );
}
