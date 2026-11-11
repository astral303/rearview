//! The messages of each session `agent search` ranks that hold every term
//! its retrieval requires of a hit, from the ranking scan's counts of each
//! kept text: its transcript stage then reads those messages alone.

use crate::search::literal::Literal;
use std::collections::HashMap;

/// The terms `agent search`'s retrieval requires a message to hold.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RetrievalTerms {
    /// Each query word and each literal: lexical mode.
    WordsAndLiterals,
    /// The literals of exact mode's phrase. The ranking counts the query's
    /// own words and literals and also requires every session to hold these.
    Phrase(Vec<Literal>),
}

/// The query terms a message's texts hold, one bit per term: word `i` in
/// `words`, literal `j` in `literals`. Terms after the 64th are not kept, so
/// a message holding the first 64 counts as holding them all; retrieval
/// checks every term again.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct MessageTerms {
    pub(super) words: u64,
    pub(super) literals: u64,
}

impl MessageTerms {
    /// One bit for each of the first 64 of `found` that is true.
    pub(super) fn bits(found: impl Iterator<Item = bool>) -> u64 {
        found
            .take(64)
            .enumerate()
            .filter(|(_, found)| *found)
            .fold(0, |bits, (index, _)| bits | 1 << index)
    }

    /// The bits of the first `count` terms, at most 64.
    pub(super) fn first(count: usize) -> u64 {
        Self::bits(std::iter::repeat_n(true, count))
    }

    pub(super) fn add(&mut self, other: Self) {
        self.words |= other.words;
        self.literals |= other.literals;
    }

    pub(super) fn holds(self, required: Self) -> bool {
        self.words & required.words == required.words
            && self.literals & required.literals == required.literals
    }
}

/// One session's hit messages.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionHitMessages {
    /// The generation of the copy whose texts were counted.
    pub generation: i64,
    /// The numbers of the messages, in order.
    pub ordinals: Vec<usize>,
}

impl SessionHitMessages {
    /// The messages of `terms` holding `required`, counted in the copy of
    /// `generation`.
    pub(super) fn holding(
        generation: i64,
        terms: &HashMap<usize, MessageTerms>,
        required: MessageTerms,
    ) -> Self {
        let mut ordinals = terms
            .iter()
            .filter(|(_, held)| held.holds(required))
            .map(|(&ordinal, _)| ordinal)
            .collect::<Vec<_>>();
        ordinals.sort_unstable();
        Self {
            generation,
            ordinals,
        }
    }
}

/// The hit messages of each session the ranking scan counted, by the
/// session's index among the ranked conversations. Missing for a session
/// the scan did not count or that is not in the session cache: search then
/// reads its transcript whole.
#[derive(Clone, Debug, Default)]
pub struct HitMessages(pub(super) HashMap<usize, SessionHitMessages>);

impl HitMessages {
    pub fn of(&self, index: usize) -> Option<&SessionHitMessages> {
        self.0.get(&index)
    }

    /// No hit message for each of `rows` the cache holds, by index: a query
    /// without terms.
    pub(super) fn none_of<'r>(
        rows: impl IntoIterator<Item = (usize, &'r crate::history::Conversation)>,
    ) -> Self {
        Self(
            rows.into_iter()
                .filter_map(|(index, row)| {
                    let copy = row.cached_session.as_ref()?;
                    Some((
                        index,
                        SessionHitMessages {
                            generation: copy.generation(),
                            ordinals: Vec::new(),
                        },
                    ))
                })
                .collect(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_holds_terms_only_when_it_holds_every_required_bit() {
        let required = MessageTerms {
            words: MessageTerms::first(2),
            literals: MessageTerms::first(1),
        };
        let held = |words: [bool; 2], literal: bool| MessageTerms {
            words: MessageTerms::bits(words.into_iter()),
            literals: MessageTerms::bits([literal].into_iter()),
        };
        assert!(held([true, true], true).holds(required));
        assert!(!held([true, false], true).holds(required));
        assert!(!held([true, true], false).holds(required));
        assert!(
            held([false, false], true).holds(MessageTerms {
                words: 0,
                ..required
            }),
            "literals alone are required in exact mode"
        );
    }

    #[test]
    fn terms_after_the_64th_are_not_required() {
        assert_eq!(MessageTerms::first(64), u64::MAX);
        assert_eq!(MessageTerms::first(70), u64::MAX);
        assert_eq!(MessageTerms::first(0), 0);
    }
}
