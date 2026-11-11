use crate::history::Conversation;
use crate::history::message_texts::push_separated;
use crate::search::byte_search::{AsciiCaselessFinder, NonAsciiCapitals};
use crate::time_filter::TimeFilter;
use chrono::{DateTime, Local};
use memchr::memmem;
use rayon::prelude::*;
use std::borrow::Borrow;
use std::ops::ControlFlow;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaseMode {
    Sensitive,
    Insensitive,
}

/// Quoted query text. It ignores case unless it contains an uppercase
/// letter. Each run of whitespace in it matches any run of whitespace in the
/// text, so the cached text, which collapses whitespace, and an opened
/// transcript give the same answer.
#[derive(Debug, Clone)]
pub struct Literal {
    text: String,
    case_mode: CaseMode,
    /// `text` folded for `case_mode`: the characters a match compares.
    folded: Vec<char>,
    /// For each prefix of `folded`, the length of its longest proper prefix
    /// that is also its suffix: where a partial match resumes after a
    /// mismatch.
    resume_at: Vec<usize>,
    /// `folded` split at its whitespace: each must appear in a text that
    /// holds a match.
    words: Vec<String>,
    search: LiteralSearch,
}

/// Every other field follows from these two.
impl PartialEq for Literal {
    fn eq(&self, other: &Self) -> bool {
        self.text == other.text && self.case_mode == other.case_mode
    }
}

impl Eq for Literal {}

/// The literal's search for its matches in a text.
#[derive(Debug, Clone)]
enum LiteralSearch {
    /// The literal starts and ends with a word: find its first word, then
    /// check the rest from there.
    FirstWord(WordFinders),
    /// Check that the text holds each word, when the literal has finders for
    /// them, then fold the text one character at a time.
    EachChar(Option<WordFinders>),
}

/// A finder for each of the literal's words.
#[derive(Debug, Clone)]
enum WordFinders {
    /// A case-sensitive literal's words, as written.
    Exact {
        finders: Box<[memmem::Finder<'static>]>,
    },
    /// A case-insensitive ASCII literal's words, lowercase, ignoring ASCII
    /// case.
    IgnoringAsciiCase {
        finders: Box<[AsciiCaselessFinder]>,
        capitals: NonAsciiCapitals,
    },
}

impl WordFinders {
    /// `folded` is the literal folded for `case_mode`, and `words` its words.
    fn new(case_mode: CaseMode, folded: &str, words: &[String]) -> Option<Self> {
        match case_mode {
            CaseMode::Sensitive => Some(Self::Exact {
                finders: words
                    .iter()
                    .map(|word| memmem::Finder::new(word.as_bytes()).into_owned())
                    .collect(),
            }),
            CaseMode::Insensitive if folded.is_ascii() => Some(Self::IgnoringAsciiCase {
                finders: words
                    .iter()
                    .map(|word| AsciiCaselessFinder::new(word.as_bytes()))
                    .collect(),
                capitals: NonAsciiCapitals::matchable_by(folded.as_bytes()),
            }),
            CaseMode::Insensitive => None,
        }
    }

    /// The byte where word `index` next occurs in `text` from byte `from`.
    fn find(&self, index: usize, text: &str, from: usize) -> Option<usize> {
        match self {
            Self::Exact { finders, .. } => finders[index]
                .find(&text.as_bytes()[from..])
                .map(|offset| from + offset),
            Self::IgnoringAsciiCase { finders, .. } => finders[index].find(text.as_bytes(), from),
        }
    }

    /// The byte where `text` first holds a character whose lowercase holds a
    /// letter of a case-insensitive literal: from there its ASCII bytes alone
    /// cannot settle a match.
    fn first_non_ascii_capital(&self, text: &str) -> Option<usize> {
        match self {
            Self::Exact { .. } => None,
            Self::IgnoringAsciiCase { capitals, .. } => capitals.first_in(text),
        }
    }

    fn holds_word(&self, index: usize, text: &str) -> bool {
        self.find(index, text, 0).is_some()
    }

    /// True when `candidate`, bytes of a text, matches `word`.
    fn matches(&self, candidate: &[u8], word: &str) -> bool {
        match self {
            Self::Exact { .. } => candidate == word.as_bytes(),
            Self::IgnoringAsciiCase { .. } => candidate.eq_ignore_ascii_case(word.as_bytes()),
        }
    }
}

impl Literal {
    pub fn new(text: String) -> Self {
        let case_mode = if text.chars().any(char::is_uppercase) {
            CaseMode::Sensitive
        } else {
            CaseMode::Insensitive
        };
        Self::with_case_mode(text, case_mode)
    }

    /// A literal that ignores case even when it contains an uppercase letter,
    /// as the viewer's search does.
    pub fn ignoring_case(text: String) -> Self {
        Self::with_case_mode(text, CaseMode::Insensitive)
    }

    fn with_case_mode(text: String, case_mode: CaseMode) -> Self {
        let text = collapse_whitespace(&text);
        let mut folded = Vec::with_capacity(text.len());
        for ch in text.chars() {
            match case_mode {
                CaseMode::Sensitive => folded.push(ch),
                CaseMode::Insensitive => folded.extend(ch.to_lowercase()),
            }
        }
        let resume_at = resume_positions(&folded);
        let folded_text = folded.iter().collect::<String>();
        let words: Vec<String> = folded_text
            .split(' ')
            .filter(|word| !word.is_empty())
            .map(str::to_owned)
            .collect();
        let finders = WordFinders::new(case_mode, &folded_text, &words);
        let search = match finders {
            Some(finders)
                if !words.is_empty() && !text.starts_with(' ') && !text.ends_with(' ') =>
            {
                LiteralSearch::FirstWord(finders)
            }
            finders => LiteralSearch::EachChar(finders),
        };
        Self {
            text,
            case_mode,
            folded,
            resume_at,
            words,
            search,
        }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn case_mode(&self) -> CaseMode {
        self.case_mode
    }

    pub fn matches(&self, text: &str) -> bool {
        let mut found = false;
        self.scan(text, |_, _| {
            found = true;
            ControlFlow::Break(())
        });
        found
    }

    pub fn match_ranges(&self, text: &str) -> Vec<(usize, usize)> {
        let mut ranges = Vec::new();
        self.scan(text, |start, end| {
            ranges.push((start, end));
            ControlFlow::Continue(())
        });
        ranges
    }

    /// Calls `on_match` with the byte range of each match in `text`, left to
    /// right and not overlapping, until it breaks.
    ///
    /// A literal that starts and ends with a word, case-sensitive or ASCII,
    /// is found by searching for its first word and checking the rest from
    /// there. Reaching a character whose lowercase holds a letter of a
    /// case-insensitive literal, that search folds the rest of `text` one
    /// character at a time from the end of its last match. Any other literal
    /// first checks that `text` holds each of its words, when it can, then
    /// folds `text` one character at a time.
    fn scan(&self, text: &str, mut on_match: impl FnMut(usize, usize) -> ControlFlow<()>) {
        if self.folded.is_empty() {
            return;
        }
        let finders = match &self.search {
            LiteralSearch::FirstWord(finders) => finders,
            LiteralSearch::EachChar(finders) => {
                if finders
                    .as_ref()
                    .is_none_or(|finders| self.may_occur_in(text, finders))
                {
                    self.scan_each_char(text, 0, on_match);
                }
                return;
            }
        };
        let fold_from = finders.first_non_ascii_capital(text);
        let mut from = 0;
        let mut last_match_end = 0;
        loop {
            let start = finders.find(0, text, from);
            if let Some(fold_from) = fold_from
                && start.is_none_or(|start| start > fold_from)
            {
                self.scan_each_char(text, last_match_end, on_match);
                return;
            }
            let Some(start) = start else { return };
            match self.match_end(text, start, finders) {
                Some(end) => {
                    if on_match(start, end).is_break() {
                        return;
                    }
                    from = end;
                    last_match_end = end;
                }
                None => from = start + 1,
            }
        }
    }

    /// The end of the match starting with the literal's first word at
    /// `start`: each later word follows a run of whitespace. `None` when the
    /// text there departs from the literal.
    fn match_end(&self, text: &str, start: usize, finders: &WordFinders) -> Option<usize> {
        let mut end = start + self.words[0].len();
        for word in &self.words[1..] {
            let word_start = text[end..]
                .char_indices()
                .find(|(_, ch)| !ch.is_whitespace())
                .map_or(text.len(), |(offset, _)| end + offset);
            if word_start == end {
                return None;
            }
            let word_end = word_start + word.len();
            if !finders.matches(text.as_bytes().get(word_start..word_end)?, word) {
                return None;
            }
            end = word_end;
        }
        Some(end)
    }

    /// False when `text` lacks one of the literal's words and can't hold a
    /// match. A case-insensitive literal may match any text holding a
    /// character whose lowercase holds one of its letters.
    fn may_occur_in(&self, text: &str, finders: &WordFinders) -> bool {
        finders.first_non_ascii_capital(text).is_some()
            || (0..self.words.len()).all(|index| finders.holds_word(index, text))
    }

    /// [`Self::scan`] by folding `text` from byte `from` one character at a
    /// time: each run of whitespace as one space, and each character
    /// lowercased unless the literal is case-sensitive. `from` must be 0 or
    /// the end of a match: the matcher starts with no partial match.
    fn scan_each_char(
        &self,
        text: &str,
        from: usize,
        mut on_match: impl FnMut(usize, usize) -> ControlFlow<()>,
    ) {
        let mut matcher = FoldedMatcher::new(self);
        let mut feed = |ch: char, start: usize, end: usize| match matcher.feed(ch, start, end) {
            Some((match_start, match_end)) => on_match(match_start, match_end),
            None => ControlFlow::Continue(()),
        };
        let mut previous_was_whitespace = false;
        for (offset, ch) in text[from..].char_indices() {
            let start = from + offset;
            let end = start + ch.len_utf8();
            let flow = if ch.is_whitespace() {
                let flow = if previous_was_whitespace {
                    ControlFlow::Continue(())
                } else {
                    feed(' ', start, end)
                };
                previous_was_whitespace = true;
                flow
            } else {
                previous_was_whitespace = false;
                match self.case_mode {
                    CaseMode::Sensitive => feed(ch, start, end),
                    CaseMode::Insensitive => ch
                        .to_lowercase()
                        .try_for_each(|folded| feed(folded, start, end)),
                }
            };
            if flow.is_break() {
                return;
            }
        }
    }
}

/// A Knuth–Morris–Pratt match of a literal's folded characters against a
/// text fed one folded character at a time.
struct FoldedMatcher<'a> {
    folded: &'a [char],
    resume_at: &'a [usize],
    matched: usize,
    /// The byte where each of the last `folded.len()` fed characters starts,
    /// as a ring: `next_slot` holds the oldest and receives the next.
    starts: Vec<usize>,
    next_slot: usize,
}

impl<'a> FoldedMatcher<'a> {
    fn new(literal: &'a Literal) -> Self {
        Self {
            folded: &literal.folded,
            resume_at: &literal.resume_at,
            matched: 0,
            starts: vec![0; literal.folded.len()],
            next_slot: 0,
        }
    }

    /// The byte range of the match `ch` completes, if it completes one.
    /// `start..end` is the text character `ch` was folded from.
    fn feed(&mut self, ch: char, start: usize, end: usize) -> Option<(usize, usize)> {
        let length = self.folded.len();
        self.starts[self.next_slot] = start;
        self.next_slot = if self.next_slot + 1 == length {
            0
        } else {
            self.next_slot + 1
        };
        while self.matched > 0 && self.folded[self.matched] != ch {
            self.matched = self.resume_at[self.matched - 1];
        }
        if self.folded[self.matched] != ch {
            return None;
        }
        self.matched += 1;
        if self.matched < length {
            return None;
        }
        self.matched = 0;
        Some((self.starts[self.next_slot], end))
    }
}

fn resume_positions(folded: &[char]) -> Vec<usize> {
    let mut resume_at = vec![0; folded.len()];
    let mut length = 0;
    for index in 1..folded.len() {
        while length > 0 && folded[index] != folded[length] {
            length = resume_at[length - 1];
        }
        if folded[index] == folded[length] {
            length += 1;
        }
        resume_at[index] = length;
    }
    resume_at
}

/// `text` with each run of whitespace as one space.
fn collapse_whitespace(text: &str) -> String {
    let mut collapsed = String::with_capacity(text.len());
    let mut previous_was_whitespace = false;
    for ch in text.chars() {
        if ch.is_whitespace() {
            if !previous_was_whitespace {
                collapsed.push(' ');
            }
            previous_was_whitespace = true;
        } else {
            collapsed.push(ch);
            previous_was_whitespace = false;
        }
    }
    collapsed
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiteralCorpusEntry {
    pub index: usize,
    pub text: String,
}

pub fn build_literal_corpus<C: Borrow<Conversation> + Sync>(
    conversations: &[C],
) -> Vec<LiteralCorpusEntry> {
    build_literal_corpus_with(conversations, false)
}

/// With `window` active, each session's entry holds only the text of its
/// messages inside the window.
pub fn build_agent_literal_corpus<C: Borrow<Conversation> + Sync>(
    conversations: &[C],
    window: &TimeFilter,
) -> Vec<LiteralCorpusEntry> {
    if !window.is_active() {
        return build_literal_corpus_with(conversations, true);
    }
    conversations
        .par_iter()
        .enumerate()
        .map(|(index, conversation)| {
            let mut text = String::new();
            for search_text in conversation.borrow().texts_within(window).into_array() {
                push_separated(&mut text, &search_text);
            }
            LiteralCorpusEntry { index, text }
        })
        .collect()
}

fn build_literal_corpus_with<C: Borrow<Conversation> + Sync>(
    conversations: &[C],
    include_agent_text: bool,
) -> Vec<LiteralCorpusEntry> {
    conversations
        .par_iter()
        .enumerate()
        .map(|(index, conversation)| LiteralCorpusEntry {
            index,
            text: literal_text(conversation.borrow(), include_agent_text),
        })
        .collect()
}

pub fn matches_all_literals(text: &str, literals: &[Literal]) -> bool {
    literals.iter().all(|literal| literal.matches(text))
}

pub fn match_literal_ranges(text: &str, literals: &[Literal]) -> Vec<(usize, usize)> {
    literals
        .iter()
        .flat_map(|literal| literal.match_ranges(text))
        .collect()
}

/// The indices of the entries in scope that match every literal, newest
/// first by `time`.
pub fn exact_fallback(
    corpus: &[LiteralCorpusEntry],
    literals: &[Literal],
    scope: impl Fn(usize) -> bool + Sync,
    time: impl Fn(usize) -> DateTime<Local> + Sync,
) -> Vec<usize> {
    if literals.is_empty() {
        return Vec::new();
    }

    let mut matches = corpus
        .par_iter()
        .filter(|entry| scope(entry.index) && matches_all_literals(&entry.text, literals))
        .map(|entry| (entry.index, time(entry.index)))
        .collect::<Vec<_>>();

    matches.sort_unstable_by_key(|(_, timestamp)| std::cmp::Reverse(*timestamp));
    matches.into_iter().map(|(index, _)| index).collect()
}

/// A session's texts joined with separators, so quoted text does not match
/// across two of them.
fn literal_text(conversation: &Conversation, include_agent_text: bool) -> String {
    let mut text = String::new();
    for piece in conversation
        .title_and_summary_texts()
        .chain([conversation.full_text.as_str()])
    {
        push_separated(&mut text, piece);
    }
    if include_agent_text {
        push_separated(&mut text, &conversation.agent_search_text);
        push_separated(&mut text, &conversation.thinking_and_tool_call_text);
    }
    push_separated(
        &mut text,
        conversation.project_name.as_deref().unwrap_or_default(),
    );
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::test_fixtures::one_message_conversation;
    use chrono::{Duration, Local};

    fn make_conv_full(
        text: &str,
        project: Option<&str>,
        title: Option<&str>,
        summary: Option<&str>,
        timestamp: chrono::DateTime<Local>,
    ) -> Conversation {
        one_message_conversation(text, timestamp, summary, title, project)
    }

    /// The ranges folding `text` one character at a time finds, which every
    /// faster search must reproduce.
    fn ranges_folding_each_char(literal: &Literal, text: &str) -> Vec<(usize, usize)> {
        let mut ranges = Vec::new();
        literal.scan_each_char(text, 0, |start, end| {
            ranges.push((start, end));
            ControlFlow::Continue(())
        });
        ranges
    }

    #[test]
    fn every_search_finds_ranges_folding_each_character_finds() {
        let literals = [
            "isolated",
            "--isolated",
            "cache rebuild",
            "a a",
            "k",
            "i",
            " leading space",
            "trailing space ",
            "Mixed Case",
            "café",
            "rust_backtrace",
            "abck",
            "k k",
            "Retry Backoff",
            "A A",
            "Ab Ab C",
            "SQLITE_PROTOCOL",
            "日本 語",
            "abi",
            "kit",
        ]
        .map(|literal| Literal::new(literal.to_owned()));
        let ignoring_case = ["\u{212A}it", "Mixed Case", "SQLITE_PROTOCOL"]
            .map(|literal| Literal::ignoring_case(literal.to_owned()));
        let texts = [
            "",
            "pass --isolated to panache, not --Isolated twice: --ISOLATED",
            "cache rebuild, Cache\tRebuild, cache\n\n rebuild, CACHE\u{a0}REBUILD, cacherebuild",
            "a a a a",
            "a\u{3000}a",
            "KELVIN \u{212A} and k and K",
            "\u{130}stanbul and istanbul",
            "text with a leading space and a trailing space ",
            "Mixed Case and mixed case",
            "CAFÉ and café",
            "RUST_BACKTRACE=1",
            "mycache rebuilds",
            "abc\u{212A}",
            "k k then \u{212A} k k",
            "\u{212A} k k",
            "isolated",
            "xisolatedx éisolatedé isolated中",
            "SQLITE_PROTOCOL",
            "sqlite_protocol, then SQLITE_PROTOCOL and SQLITE_PROTOCOLS",
            "Retry  Backoff, retry backoff, Retry\n\tBackoff, RetryBackoff",
            "A A A a",
            "Ab Ab Ab C",
            "日本 語 日本\u{3000}語 日本語",
            "abİ abi ABI",
            "kit \u{212A}it KIT",
        ];
        for literal in literals.iter().chain(&ignoring_case) {
            for text in texts {
                assert_eq!(
                    literal.match_ranges(text),
                    ranges_folding_each_char(literal, text),
                    "{:?} in {text:?}",
                    literal.text()
                );
                assert_eq!(
                    literal.matches(text),
                    !ranges_folding_each_char(literal, text).is_empty(),
                    "{:?} in {text:?}",
                    literal.text()
                );
            }
        }
    }

    #[test]
    fn literal_uses_smart_case() {
        assert!(Literal::new("exact_phrase".to_string()).matches("EXACT_PHRASE"));
        assert!(!Literal::new("Exact_Phrase".to_string()).matches("exact_phrase"));
        assert!(Literal::new("Exact_Phrase".to_string()).matches("Exact_Phrase"));
    }

    #[test]
    fn literal_ranges_use_smart_case() {
        let insensitive = Literal::new("exact_phrase".to_string());
        let sensitive = Literal::new("Exact_Phrase".to_string());

        assert_eq!(insensitive.match_ranges("EXACT_PHRASE"), vec![(0, 12)]);
        assert_eq!(sensitive.match_ranges("exact_phrase"), Vec::new());
        assert_eq!(sensitive.match_ranges("Exact_Phrase"), vec![(0, 12)]);
    }

    #[test]
    fn insensitive_literal_ranges_are_original_text_boundaries() {
        let text = "pre İSTANBUL post";
        let literal = Literal::new("i\u{307}stanbul".to_string());
        let ranges = literal.match_ranges(text);

        assert_eq!(ranges, vec![(4, 13)]);
        assert_eq!(&text[ranges[0].0..ranges[0].1], "İSTANBUL");
    }

    #[test]
    fn insensitive_literal_match_uses_smart_case() {
        let literal = Literal::new("i\u{307}stanbul".to_string());

        assert!(literal.matches("pre İSTANBUL post"));
        assert!(!literal.matches("pre constantinople post"));
    }

    #[test]
    fn whitespace_run_matches_spaces_tabs_and_line_breaks() {
        let insensitive = Literal::new("retry  backoff".to_string());
        let sensitive = Literal::new("Retry Backoff".to_string());

        for text in ["retry backoff", "retry\tbackoff", "retry\n  \r\nbackoff"] {
            assert!(insensitive.matches(text), "{text:?}");
        }
        assert!(sensitive.matches("see Retry\n\tBackoff here"));
        assert_eq!(
            insensitive.match_ranges("x RETRY\n\nbackoff y"),
            vec![(2, 16)]
        );
    }

    #[test]
    fn whitespace_run_doesnt_match_text_without_whitespace() {
        let literal = Literal::new("retry backoff".to_string());

        assert!(!literal.matches("retrybackoff"));
        assert!(literal.match_ranges("retry-backoff").is_empty());
    }

    #[test]
    fn matching_resumes_inside_partial_match() {
        let literal = Literal::new("abab c".to_string());

        assert_eq!(literal.match_ranges("ababab   c"), vec![(2, 10)]);
    }

    #[test]
    fn corpus_preserves_punctuation_and_metadata() {
        let now = Local::now();
        let conversations = vec![make_conv_full(
            "body_with_under_score and punctuation: yes",
            Some("project_name/raw-value"),
            Some("Title: Raw_Value"),
            Some("Summary.with punctuation"),
            now,
        )];

        let corpus = build_literal_corpus(&conversations);

        assert!(corpus[0].text.contains("body_with_under_score"));
        assert!(corpus[0].text.contains("punctuation: yes"));
        assert!(corpus[0].text.contains("project_name/raw-value"));
        assert!(corpus[0].text.contains("Title: Raw_Value"));
        assert!(corpus[0].text.contains("Summary.with punctuation"));
    }

    #[test]
    fn corpus_does_not_duplicate_title_and_summary() {
        let now = Local::now();
        let conversations = vec![make_conv_full(
            "body sentinel",
            None,
            Some("TitleA"),
            Some("SummaryB"),
            now,
        )];

        let corpus = build_literal_corpus(&conversations);

        assert!(!corpus[0].text.contains("SummaryB TitleA"));
    }

    #[test]
    fn exact_fallback_returns_scoped_matches_newest_first() {
        let now = Local::now();
        let conversations = vec![
            make_conv_full("needle phrase", None, None, None, now - Duration::days(1)),
            make_conv_full("needle phrase", None, None, None, now),
            make_conv_full("needle phrase", None, None, None, now - Duration::hours(1)),
        ];
        let corpus = build_literal_corpus(&conversations);
        let literal = Literal::new("needle phrase".to_string());

        let results = exact_fallback(
            &corpus,
            &[literal],
            |index| index != 1,
            |index| conversations[index].timestamp,
        );

        assert_eq!(results, vec![2, 0]);
    }

    #[test]
    fn exact_fallback_requires_all_literals() {
        let now = Local::now();
        let conversations = vec![
            make_conv_full("alpha beta", None, None, None, now),
            make_conv_full("alpha gamma", None, None, None, now),
        ];
        let corpus = build_literal_corpus(&conversations);
        let literals = vec![
            Literal::new("alpha".to_string()),
            Literal::new("beta".to_string()),
        ];

        let results = exact_fallback(
            &corpus,
            &literals,
            |_| true,
            |index| conversations[index].timestamp,
        );

        assert_eq!(results, vec![0]);
    }

    fn conversation_with_agent_texts(
        full_text: &str,
        agent_search_text: &str,
        thinking_and_tool_call_text: &str,
    ) -> Conversation {
        let mut conversation = make_conv_full(full_text, None, None, None, Local::now());
        conversation.agent_search_text = agent_search_text.to_owned();
        conversation.thinking_and_tool_call_text = thinking_and_tool_call_text.to_owned();
        conversation
    }

    #[test]
    fn agent_literal_spanning_two_cached_texts_doesnt_match() {
        let conversation = conversation_with_agent_texts("ends with alpha", "beta starts", "");

        assert!(
            build_agent_literal_corpus(&[conversation], &TimeFilter::default())
                .iter()
                .all(|entry| !Literal::new("alpha beta".to_owned()).matches(&entry.text))
        );
    }

    #[test]
    fn literal_doesnt_match_across_separator_between_two_messages() {
        for (end, start, phrase) in [
            ("ends with alpha", "beta starts", "alpha beta"),
            ("ends with Alpha", "Beta starts", "Alpha Beta"),
        ] {
            let literal = Literal::new(phrase.to_owned());
            let mut separated = end.to_owned();
            push_separated(&mut separated, start);

            assert!(literal.matches(&format!("{end} {start}")), "{phrase}");
            assert!(!literal.matches(&separated), "{phrase}");
        }
    }
}
