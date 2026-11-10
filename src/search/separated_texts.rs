//! Several texts read as the one text `push_separated` would join them into,
//! without copying them: the evidence and the context line read a row's
//! title and summary ahead of its other texts this way.

use crate::history::Conversation;
use crate::history::message_texts::PADDED_MESSAGE_SEPARATOR;
use std::borrow::Cow;
use std::ops::Range;

/// The texts, each after [`PADDED_MESSAGE_SEPARATOR`] but the first, as one
/// text. An empty text adds nothing, separator included. Offsets are byte
/// offsets into the joined text.
#[derive(Clone, Debug)]
pub(crate) struct SeparatedTexts<'a> {
    /// Each text and each separator, with its offset, in order.
    segments: Vec<(usize, &'a str)>,
    len: usize,
}

impl<'a> SeparatedTexts<'a> {
    pub(crate) fn new(texts: impl IntoIterator<Item = &'a str>) -> Self {
        let mut segments = Vec::new();
        let mut len = 0;
        for text in texts.into_iter().filter(|text| !text.is_empty()) {
            if len > 0 {
                segments.push((len, PADDED_MESSAGE_SEPARATOR));
                len += PADDED_MESSAGE_SEPARATOR.len();
            }
            segments.push((len, text));
            len += text.len();
        }
        Self { segments, len }
    }

    /// `conversation`'s title and summary, then its `full_text`.
    pub(crate) fn of_row(conversation: &'a Conversation) -> Self {
        Self::new(
            conversation
                .leading_texts()
                .chain([conversation.full_text.as_str()]),
        )
    }

    pub(crate) fn len(&self) -> usize {
        self.len
    }

    /// Each text, without the separators, with its offset.
    pub(crate) fn texts(&self) -> impl Iterator<Item = (usize, &'a str)> + '_ {
        self.segments
            .iter()
            .step_by(2)
            .map(|&(offset, text)| (offset, text))
    }

    /// The joined text's `range`, borrowed when it lies inside one text or
    /// separator. Both ends of `range` are character boundaries.
    pub(crate) fn slice(&self, range: Range<usize>) -> Cow<'a, str> {
        let mut pieces = self.segments.iter().filter_map(|&(offset, text)| {
            let start = range.start.max(offset);
            let end = range.end.min(offset + text.len());
            (start < end).then(|| &text[start - offset..end - offset])
        });
        let Some(first) = pieces.next() else {
            return Cow::Borrowed("");
        };
        let Some(second) = pieces.next() else {
            return Cow::Borrowed(first);
        };
        let mut joined = format!("{first}{second}");
        pieces.for_each(|piece| joined.push_str(piece));
        Cow::Owned(joined)
    }

    /// The start of the character `chars` characters before the one ending
    /// at `end`, as `text[..end].char_indices().rev().nth(chars)` finds it;
    /// 0 when there are fewer.
    pub(crate) fn start_before(&self, end: usize, chars: usize) -> usize {
        self.segments
            .iter()
            .rev()
            .filter(|(offset, _)| *offset < end)
            .flat_map(|&(offset, text)| {
                let before = &text[..(end - offset).min(text.len())];
                before
                    .char_indices()
                    .rev()
                    .map(move |(index, _)| offset + index)
            })
            .nth(chars)
            .unwrap_or(0)
    }

    /// The start of the character `chars` characters after the one starting
    /// at `start`, as `text[start..].char_indices().nth(chars)` finds it;
    /// the joined text's length when there are fewer.
    pub(crate) fn start_after(&self, start: usize, chars: usize) -> usize {
        self.segments
            .iter()
            .filter(|(offset, text)| offset + text.len() > start)
            .flat_map(|&(offset, text)| {
                let from = start.saturating_sub(offset);
                text[from..]
                    .char_indices()
                    .map(move |(index, _)| offset + from + index)
            })
            .nth(chars)
            .unwrap_or(self.len)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::message_texts::push_separated;

    const TEXTS: [&str; 4] = ["Ünïcode title", "", "a summary", "body \u{1e} text"];

    fn joined() -> String {
        let mut text = String::new();
        for piece in TEXTS {
            push_separated(&mut text, piece);
        }
        text
    }

    #[test]
    fn texts_sit_at_their_offsets_in_the_joined_text() {
        let joined = joined();
        let texts = SeparatedTexts::new(TEXTS);

        assert_eq!(texts.len(), joined.len());
        for (offset, text) in texts.texts() {
            assert_eq!(&joined[offset..offset + text.len()], text);
        }
        assert_eq!(
            texts.texts().map(|(_, text)| text).collect::<Vec<_>>(),
            ["Ünïcode title", "a summary", "body \u{1e} text"],
            "an empty text adds nothing"
        );
    }

    #[test]
    fn each_slice_and_character_walk_is_the_joined_texts() {
        let joined = joined();
        let texts = SeparatedTexts::new(TEXTS);
        let boundaries = (0..=joined.len())
            .filter(|index| joined.is_char_boundary(*index))
            .collect::<Vec<_>>();
        for &start in &boundaries {
            for &end in boundaries.iter().filter(|end| **end >= start) {
                assert_eq!(
                    texts.slice(start..end),
                    &joined[start..end],
                    "{start}..{end}"
                );
            }
            for chars in 0..joined.len() {
                assert_eq!(
                    texts.start_before(start, chars),
                    joined[..start]
                        .char_indices()
                        .rev()
                        .nth(chars)
                        .map_or(0, |(index, _)| index),
                    "{chars} characters before {start}"
                );
                assert_eq!(
                    texts.start_after(start, chars),
                    joined[start..]
                        .char_indices()
                        .nth(chars)
                        .map_or(joined.len(), |(index, _)| start + index),
                    "{chars} characters after {start}"
                );
            }
        }
    }

    #[test]
    fn slice_is_borrowed_only_inside_one_text() {
        let texts = SeparatedTexts::new(TEXTS);

        assert!(matches!(texts.slice(0..3), Cow::Borrowed("Ün")));
        assert!(matches!(texts.slice(10..20), Cow::Owned(_)));
    }
}
