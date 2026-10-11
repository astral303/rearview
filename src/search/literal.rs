use crate::history::Conversation;
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Literal {
    text: String,
    case_mode: CaseMode,
    /// `text` folded for `case_mode`: the characters a match compares.
    folded: Vec<char>,
    /// For each prefix of `folded`, the length of its longest proper prefix
    /// that is also its suffix: where a partial match resumes after a
    /// mismatch.
    resume_at: Vec<usize>,
}

impl Literal {
    pub fn new(text: String) -> Self {
        let case_mode = if text.chars().any(char::is_uppercase) {
            CaseMode::Sensitive
        } else {
            CaseMode::Insensitive
        };
        let text = collapse_whitespace(&text);
        let mut folded = Vec::with_capacity(text.len());
        for ch in text.chars() {
            match case_mode {
                CaseMode::Sensitive => folded.push(ch),
                CaseMode::Insensitive => folded.extend(ch.to_lowercase()),
            }
        }
        let resume_at = resume_positions(&folded);
        Self {
            text,
            case_mode,
            folded,
            resume_at,
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
    fn scan(&self, text: &str, mut on_match: impl FnMut(usize, usize) -> ControlFlow<()>) {
        if self.folded.is_empty() {
            return;
        }
        if self.case_mode == CaseMode::Sensitive && !self.text.contains(char::is_whitespace) {
            for (start, end) in find_substring_ranges(text, &self.text) {
                if on_match(start, end).is_break() {
                    return;
                }
            }
            return;
        }

        let mut matcher = FoldedMatcher::new(self);
        let mut feed = |ch: char, start: usize, end: usize| match matcher.feed(ch, start, end) {
            Some((match_start, match_end)) => on_match(match_start, match_end),
            None => ControlFlow::Continue(()),
        };
        let mut previous_was_whitespace = false;
        for (start, ch) in text.char_indices() {
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
    fed: usize,
    /// The byte where each of the last `folded.len()` fed characters starts,
    /// by position modulo `folded.len()`.
    starts: Vec<usize>,
}

impl<'a> FoldedMatcher<'a> {
    fn new(literal: &'a Literal) -> Self {
        Self {
            folded: &literal.folded,
            resume_at: &literal.resume_at,
            matched: 0,
            fed: 0,
            starts: vec![0; literal.folded.len()],
        }
    }

    /// The byte range of the match `ch` completes, if it completes one.
    /// `start..end` is the text character `ch` was folded from.
    fn feed(&mut self, ch: char, start: usize, end: usize) -> Option<(usize, usize)> {
        let length = self.folded.len();
        self.starts[self.fed % length] = start;
        self.fed += 1;
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
        Some((self.starts[self.fed % length], end))
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

pub fn build_agent_literal_corpus<C: Borrow<Conversation> + Sync>(
    conversations: &[C],
) -> Vec<LiteralCorpusEntry> {
    build_literal_corpus_with(conversations, true)
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

pub fn conversation_matches_all_literals(
    conversation: &Conversation,
    literals: &[Literal],
) -> bool {
    matches_all_literals(&literal_text(conversation, false), literals)
}

/// [`conversation_matches_all_literals`] over the text `agent search` reads:
/// sub-agent text, thinking and tool calls included. Each literal must match
/// within one of those texts.
pub fn agent_conversation_matches_all_literals(
    conversation: &Conversation,
    literals: &[Literal],
) -> bool {
    let fields = [
        conversation.full_text.as_str(),
        conversation.agent_search_text.as_str(),
        conversation.thinking_and_tool_call_text.as_str(),
        conversation.project_name.as_deref().unwrap_or_default(),
    ];
    literals
        .iter()
        .all(|literal| fields.iter().any(|field| literal.matches(field)))
}

pub fn match_literal_ranges(text: &str, literals: &[Literal]) -> Vec<(usize, usize)> {
    literals
        .iter()
        .flat_map(|literal| literal.match_ranges(text))
        .collect()
}

pub fn exact_fallback<C: Borrow<Conversation> + Sync>(
    conversations: &[C],
    corpus: &[LiteralCorpusEntry],
    literals: &[Literal],
    scope: impl Fn(usize) -> bool + Sync,
) -> Vec<usize> {
    if literals.is_empty() {
        return Vec::new();
    }

    let mut matches = corpus
        .par_iter()
        .filter(|entry| scope(entry.index) && matches_all_literals(&entry.text, literals))
        .map(|entry| (entry.index, conversations[entry.index].borrow().timestamp))
        .collect::<Vec<_>>();

    matches.sort_unstable_by_key(|(_, timestamp)| std::cmp::Reverse(*timestamp));
    matches.into_iter().map(|(index, _)| index).collect()
}

fn find_substring_ranges(text: &str, needle: &str) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let mut start = 0;
    while let Some(pos) = text[start..].find(needle) {
        let range_start = start + pos;
        let range_end = range_start + needle.len();
        ranges.push((range_start, range_end));
        start = range_end;
    }
    ranges
}

fn literal_text(conversation: &Conversation, include_agent_text: bool) -> String {
    let mut text = String::new();
    push_part(&mut text, Some(&conversation.full_text));
    if include_agent_text {
        push_part(&mut text, Some(&conversation.agent_search_text));
        push_part(&mut text, Some(&conversation.thinking_and_tool_call_text));
    }
    push_part(&mut text, conversation.project_name.as_deref());
    text
}

fn push_part(text: &mut String, part: Option<&str>) {
    let Some(part) = part else {
        return;
    };
    if part.is_empty() {
        return;
    }
    if !text.is_empty() {
        text.push(' ');
    }
    text.push_str(part);
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
    fn a_whitespace_run_matches_spaces_tabs_and_line_breaks() {
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
    fn a_whitespace_run_doesnt_match_text_without_whitespace() {
        let literal = Literal::new("retry backoff".to_string());

        assert!(!literal.matches("retrybackoff"));
        assert!(literal.match_ranges("retry-backoff").is_empty());
    }

    #[test]
    fn matching_resumes_inside_a_partial_match() {
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

        let results = exact_fallback(&conversations, &corpus, &[literal], |index| index != 1);

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

        let results = exact_fallback(&conversations, &corpus, &literals, |_| true);

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
    fn an_agent_literal_matches_in_any_cached_text() {
        let conversation =
            conversation_with_agent_texts("main reply", "sub-agent reply", "thought it through");

        for phrase in ["main reply", "sub-agent reply", "thought it through"] {
            assert!(
                agent_conversation_matches_all_literals(
                    &conversation,
                    &[Literal::new(phrase.to_owned())]
                ),
                "{phrase}"
            );
        }
    }

    #[test]
    fn an_agent_literal_spanning_two_cached_texts_doesnt_match() {
        let conversation = conversation_with_agent_texts("ends with alpha", "beta starts", "");

        assert!(!agent_conversation_matches_all_literals(
            &conversation,
            &[Literal::new("alpha beta".to_owned())]
        ));
    }
}
