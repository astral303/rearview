//! Query words counted in a stored text as they occur in the text after
//! normalisation: each run of whitespace one space, lowercased, and each word
//! separator a space. The stored text, which the session cache stores without
//! terminal codes, is never normalised.
//!
//! An ASCII word is found by a byte search that ignores ASCII case. Text
//! holding a character lowercasing to ASCII takes the exact path, one
//! character at a time through the normalisation, as do words outside ASCII.

use super::hit_messages::{MessageTerms, RetrievalTerms};
use crate::search::byte_search::{AsciiCaselessFinder, DOTTED_CAPITAL_I, KELVIN_SIGN};
use crate::search::literal::Literal;
use crate::text_match::{contains_cjk, is_word_separator};

/// The most prefix matches a word counts in a session's body.
pub(super) const PREFIX_CAP: usize = 10;
/// The most adjacent pairs a session's body counts.
pub(super) const ADJACENCY_CAP: usize = 3;

/// What a session's body holds of the query, summed over its texts.
pub(super) struct Counts {
    pub(super) contains: Vec<bool>,
    pub(super) prefixes: Vec<usize>,
    pub(super) adjacency: usize,
    pub(super) literals: Vec<bool>,
    pub(super) has_text: bool,
}

/// The query's words, adjacent pairs and literals, with their byte searches
/// built once for every session a search scores.
pub(super) struct RawQuery<'q> {
    words: Vec<Pattern>,
    pairs: Vec<Pattern>,
    literals: &'q [Literal],
}

impl<'q> RawQuery<'q> {
    /// `words` are normalised query words; each of `pairs` is two of them
    /// joined by a space.
    pub(super) fn new(words: &[&str], pairs: &[String], literals: &'q [Literal]) -> Self {
        Self {
            words: words.iter().map(|word| Pattern::word(word)).collect(),
            pairs: pairs.iter().map(|pair| Pattern::pair(pair)).collect(),
            literals,
        }
    }

    pub(super) fn literals(&self) -> &'q [Literal] {
        self.literals
    }

    /// The terms a message must hold to be a hit message under `terms`.
    pub(super) fn required_terms(&self, terms: &RetrievalTerms) -> MessageTerms {
        match terms {
            RetrievalTerms::WordsAndLiterals => MessageTerms {
                words: MessageTerms::first(self.words.len()),
                literals: MessageTerms::first(self.literals.len()),
            },
            RetrievalTerms::Phrase(phrase) => MessageTerms {
                words: 0,
                literals: MessageTerms::bits(
                    self.literals.iter().map(|literal| phrase.contains(literal)),
                ),
            },
        }
    }

    /// A matcher for one session's texts.
    pub(super) fn matcher(&self) -> RawMatcher<'_> {
        RawMatcher {
            query: self,
            word_states: self.words.iter().map(PatternState::new).collect(),
            pair_states: self.pairs.iter().map(PatternState::new).collect(),
        }
    }
}

/// A query matched against one session's texts, with the state the exact
/// path keeps while it scans one text.
pub(super) struct RawMatcher<'a> {
    query: &'a RawQuery<'a>,
    word_states: Vec<PatternState>,
    pair_states: Vec<PatternState>,
}

impl RawMatcher<'_> {
    pub(super) fn counts(&self) -> Counts {
        Counts {
            contains: vec![false; self.query.words.len()],
            prefixes: vec![0; self.query.words.len()],
            adjacency: 0,
            literals: vec![false; self.query.literals.len()],
            has_text: false,
        }
    }

    /// The terms `text`, whose counts are `counts`, holds as `agent search`'s
    /// retrieval matches a message: a word starting a word of the text, or a
    /// word holding CJK anywhere in it, and a literal inside it. Unlike
    /// `counts`, a literal starting or ending with whitespace also matches
    /// the whitespace at the text's edges, as it matches a message's part.
    pub(super) fn message_terms(&self, text: &str, counts: &Counts) -> MessageTerms {
        let words = self
            .query
            .words
            .iter()
            .zip(counts.prefixes.iter().zip(&counts.contains))
            .map(|(word, (&prefixes, &contains))| prefixes > 0 || (word.holds_cjk && contains));
        let literals =
            self.query
                .literals
                .iter()
                .zip(&counts.literals)
                .map(|(literal, &in_trimmed_text)| {
                    in_trimmed_text || (has_edge_whitespace(literal) && literal.matches(text))
                });
        MessageTerms {
            words: MessageTerms::bits(words),
            literals: MessageTerms::bits(literals),
        }
    }

    /// True once more text cannot change `counts`.
    pub(super) fn is_complete(&self, counts: &Counts) -> bool {
        counts.has_text
            && counts.contains.iter().all(|found| *found)
            && counts.prefixes.iter().all(|count| *count == PREFIX_CAP)
            && (self.query.pairs.is_empty() || counts.adjacency == ADJACENCY_CAP)
            && counts.literals.iter().all(|found| *found)
    }

    /// Adds what `text` holds of the query to `counts`.
    pub(super) fn count(&mut self, text: &str, counts: &mut Counts) {
        let words_need_exact_path = holds_character_lowercasing_to_ascii(text);
        let mut needs_exact_path = false;
        for (index, word) in self.query.words.iter().enumerate() {
            let state = &mut self.word_states[index];
            state.active = false;
            if counts.contains[index] && counts.prefixes[index] == PREFIX_CAP {
                continue;
            }
            match &word.ascii {
                Some(AsciiForm::Word(finder)) if !words_need_exact_path => {
                    count_word(text, finder, index, counts);
                }
                _ if word.may_occur_in(text) => {
                    state.active = true;
                    needs_exact_path = true;
                }
                _ => {}
            }
        }
        for (index, pair) in self.query.pairs.iter().enumerate() {
            let state = &mut self.pair_states[index];
            state.active = false;
            if counts.adjacency == ADJACENCY_CAP {
                continue;
            }
            match &pair.ascii {
                Some(AsciiForm::Pair { first, second }) if !words_need_exact_path => {
                    counts.adjacency = count_pair(text, first, second, counts.adjacency);
                }
                _ if pair.may_occur_in(text) => {
                    state.active = true;
                    needs_exact_path = true;
                }
                _ => {}
            }
        }
        for (index, literal) in self.query.literals.iter().enumerate() {
            if !counts.literals[index] {
                counts.literals[index] = literal.matches(text.trim());
            }
        }
        counts.has_text |= !text.trim().is_empty();
        if needs_exact_path {
            self.count_exactly(text, counts);
        }
    }

    /// [`Self::count`] for the active patterns, by normalising `text` one
    /// character at a time.
    fn count_exactly(&mut self, text: &str, counts: &mut Counts) {
        for state in self.word_states.iter_mut().chain(&mut self.pair_states) {
            state.restart();
        }
        let words = &self.query.words;
        let pairs = &self.query.pairs;
        let word_states = &mut self.word_states;
        let pair_states = &mut self.pair_states;
        let mut previous: Option<char> = None;
        let mut feed_normalised = |ch: char, counts: &mut Counts| {
            let starts_word = previous.is_none_or(|previous| !previous.is_alphanumeric());
            previous = Some(ch);
            for (index, state) in word_states.iter_mut().enumerate() {
                if !state.active {
                    continue;
                }
                if let Some(match_starts_word) = state.feed(&words[index], ch, starts_word) {
                    counts.contains[index] = true;
                    if match_starts_word && counts.prefixes[index] < PREFIX_CAP {
                        counts.prefixes[index] += 1;
                    }
                }
            }
            for (index, state) in pair_states.iter_mut().enumerate() {
                if state.active
                    && state.feed(&pairs[index], ch, starts_word) == Some(true)
                    && counts.adjacency < ADJACENCY_CAP
                {
                    counts.adjacency += 1;
                }
            }
        };
        for_each_collapsed_char(text, |ch| {
            // The collapsed text's only whitespace is a space.
            if ch.is_ascii() {
                let is_separator = matches!(ch, ' ' | '_' | '-' | '/');
                feed_normalised(
                    if is_separator {
                        ' '
                    } else {
                        ch.to_ascii_lowercase()
                    },
                    counts,
                );
            } else if is_word_separator(ch) {
                feed_normalised(' ', counts);
            } else {
                for lower in ch.to_lowercase() {
                    feed_normalised(lower, counts);
                }
            }
        });
    }
}

/// A query word or adjacent pair, normalised.
struct Pattern {
    chars: Vec<char>,
    /// For each prefix of `chars`, the length of its longest proper prefix
    /// that is also its suffix: where a partial match resumes.
    resume_at: Vec<usize>,
    ascii: Option<AsciiForm>,
    holds_cjk: bool,
}

enum AsciiForm {
    Word(AsciiCaselessFinder),
    Pair {
        first: AsciiCaselessFinder,
        second: Box<[u8]>,
    },
}

impl Pattern {
    fn word(word: &str) -> Self {
        let ascii = word
            .is_ascii()
            .then(|| AsciiForm::Word(AsciiCaselessFinder::new(word.as_bytes())));
        Self::with_ascii_form(word, ascii)
    }

    fn pair(pair: &str) -> Self {
        let ascii = pair
            .is_ascii()
            .then(|| pair.split_once(' '))
            .flatten()
            .filter(|(first, second)| !first.is_empty() && !second.is_empty())
            .map(|(first, second)| AsciiForm::Pair {
                first: AsciiCaselessFinder::new(first.as_bytes()),
                second: second.as_bytes().into(),
            });
        Self::with_ascii_form(pair, ascii)
    }

    /// False when `text` cannot hold the pattern once normalised: it lacks a
    /// byte every match holds.
    fn may_occur_in(&self, text: &str) -> bool {
        match &self.ascii {
            Some(AsciiForm::Word(finder) | AsciiForm::Pair { first: finder, .. }) => {
                finder.anchor_may_occur_in(text)
            }
            None => true,
        }
    }

    fn with_ascii_form(text: &str, ascii: Option<AsciiForm>) -> Self {
        let chars: Vec<char> = text.chars().collect();
        let mut resume_at = vec![0; chars.len()];
        let mut length = 0;
        for index in 1..chars.len() {
            while length > 0 && chars[index] != chars[length] {
                length = resume_at[length - 1];
            }
            if chars[index] == chars[length] {
                length += 1;
            }
            resume_at[index] = length;
        }
        Self {
            chars,
            resume_at,
            ascii,
            holds_cjk: contains_cjk(text),
        }
    }
}

/// A Knuth–Morris–Pratt match of one pattern against normalised characters,
/// starting again after each match, as a search from the end of the last
/// match does.
struct PatternState {
    active: bool,
    matched: usize,
    /// Whether each of the last `chars.len()` characters fed starts a word,
    /// as a ring: `next_slot` holds the oldest and takes the next.
    word_starts: Vec<bool>,
    next_slot: usize,
}

impl PatternState {
    fn new(pattern: &Pattern) -> Self {
        Self {
            active: false,
            matched: 0,
            word_starts: vec![false; pattern.chars.len()],
            next_slot: 0,
        }
    }

    fn restart(&mut self) {
        self.matched = 0;
        self.next_slot = 0;
    }

    /// `Some` when `ch` completes a match, holding whether the match starts a
    /// word.
    fn feed(&mut self, pattern: &Pattern, ch: char, starts_word: bool) -> Option<bool> {
        let length = pattern.chars.len();
        if length == 0 {
            return None;
        }
        self.word_starts[self.next_slot] = starts_word;
        self.next_slot = if self.next_slot + 1 == length {
            0
        } else {
            self.next_slot + 1
        };
        while self.matched > 0 && pattern.chars[self.matched] != ch {
            self.matched = pattern.resume_at[self.matched - 1];
        }
        if pattern.chars[self.matched] != ch {
            return None;
        }
        self.matched += 1;
        if self.matched < length {
            return None;
        }
        self.matched = 0;
        Some(self.word_starts[self.next_slot])
    }
}

/// True when `literal` starts or ends with whitespace: only then can it
/// match a text and not the text without the whitespace at its edges.
fn has_edge_whitespace(literal: &Literal) -> bool {
    literal.text().starts_with(' ') || literal.text().ends_with(' ')
}

/// True when `text` holds a character outside ASCII that lowercases to
/// ASCII: the byte search cannot see its words.
fn holds_character_lowercasing_to_ascii(text: &str) -> bool {
    let bytes = text.as_bytes();
    memchr::memchr2_iter(0xC4, 0xE2, bytes).any(|index| {
        bytes[index..].starts_with(DOTTED_CAPITAL_I.as_bytes())
            || bytes[index..].starts_with(KELVIN_SIGN.as_bytes())
    })
}

/// Calls `visit` with each character of `text` as whitespace normalisation
/// leaves it: each run of whitespace one space, without leading or trailing
/// whitespace.
fn for_each_collapsed_char(text: &str, mut visit: impl FnMut(char)) {
    let mut pending_space = false;
    let mut has_visited = false;
    for ch in text.chars() {
        if ch.is_whitespace() {
            pending_space = true;
            continue;
        }
        if pending_space && has_visited {
            visit(' ');
        }
        pending_space = false;
        has_visited = true;
        visit(ch);
    }
}

/// Adds the occurrences of the lowercase ASCII word `finder` finds in `text`
/// to the word's counts at `index`, from the left and not overlapping.
fn count_word(text: &str, finder: &AsciiCaselessFinder, index: usize, counts: &mut Counts) {
    let bytes = text.as_bytes();
    let mut from = 0;
    while let Some(start) = finder.find(bytes, from) {
        counts.contains[index] = true;
        if starts_word(text, start) {
            counts.prefixes[index] += 1;
            if counts.prefixes[index] == PREFIX_CAP {
                return;
            }
        }
        from = start + finder.needle().len();
    }
}

/// `adjacency` plus the occurrences in `text` of `first`, one space after
/// normalisation, then `second`, that start a word, up to the cap.
fn count_pair(
    text: &str,
    first: &AsciiCaselessFinder,
    second: &[u8],
    mut adjacency: usize,
) -> usize {
    let bytes = text.as_bytes();
    let mut from = 0;
    while let Some(start) = first.find(bytes, from) {
        let second_start = separator_end(text, start + first.needle().len());
        let end = second_start.map(|second_start| second_start + second.len());
        match (second_start, end) {
            (Some(second_start), Some(end))
                if bytes
                    .get(second_start..end)
                    .is_some_and(|candidate| candidate.eq_ignore_ascii_case(second)) =>
            {
                if starts_word(text, start) {
                    adjacency += 1;
                    if adjacency == ADJACENCY_CAP {
                        return adjacency;
                    }
                }
                from = end;
            }
            _ => from = start + 1,
        }
    }
    adjacency
}

/// The end of what normalises to one space at byte `at`: a run of
/// whitespace, or one other word separator. `None` when `at` holds neither.
fn separator_end(text: &str, at: usize) -> Option<usize> {
    let rest = &text[at..];
    let first = rest.chars().next()?;
    if first.is_whitespace() {
        let run = rest
            .char_indices()
            .find(|(_, ch)| !ch.is_whitespace())
            .map_or(rest.len(), |(offset, _)| offset);
        Some(at + run)
    } else if is_word_separator(first) {
        Some(at + first.len_utf8())
    } else {
        None
    }
}

/// True when the character before byte `start` of `text` is not
/// alphanumeric, or `start` begins the text. Its normalised form is
/// alphanumeric when it is, outside LATIN CAPITAL LETTER I WITH DOT ABOVE,
/// whose text takes the exact path.
fn starts_word(text: &str, start: usize) -> bool {
    text[..start]
        .chars()
        .next_back()
        .is_none_or(|previous| !previous.is_alphanumeric())
}

#[cfg(test)]
mod tests;
