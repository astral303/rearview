//! A session's search texts in `agent read`'s message order, and where each
//! message's text starts in them.

use super::Conversation;
use crate::agent::refs::MessageRange;
use crate::time_filter::TimeFilter;
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::ops::Range;

/// Written between two parts, between two messages, between the messages
/// and the text outside every message, and between a session's search texts:
/// wherever quoted text must not match across a join. Quoted text does not
/// match across it because it is not whitespace; lexical search reads it as
/// a word boundary.
pub(crate) const PADDED_MESSAGE_SEPARATOR: &str = " \u{1e} ";

/// The character inside [`PADDED_MESSAGE_SEPARATOR`]. Previews show it as a
/// space.
pub const MESSAGE_SEPARATOR: char = PADDED_MESSAGE_SEPARATOR.as_bytes()[1] as char;

/// The start of each message's text in one of a session's search texts.
///
/// The text opens with the text outside every message, such as the title and
/// the summary; each message's text follows in message order, after a
/// separator; text past `last_message_end` is outside every message too. A
/// windowed search does not match a text longer than 4 GiB: it is stored
/// without starts, because each offset is 4 bytes.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct MessageStarts {
    /// `starts[0]` is where m1's text starts, its separator included. A
    /// message past the last start has empty text.
    starts: Vec<u32>,
    last_message_end: u32,
}

impl MessageStarts {
    /// Trailing starts at `last_message_end` are dropped.
    fn of_offsets(mut starts: Vec<usize>, last_message_end: usize) -> Self {
        while starts.last() == Some(&last_message_end) {
            starts.pop();
        }
        let Ok(last_message_end) = u32::try_from(last_message_end) else {
            return Self::default();
        };
        Self {
            // No start exceeds `last_message_end`, and `last_message_end`
            // fits in a u32.
            starts: starts.into_iter().map(|start| start as u32).collect(),
            last_message_end,
        }
    }

    /// Borrowed when the text of `ranges` is one slice.
    pub fn text_of<'t>(&self, text: &'t str, ranges: &[MessageRange]) -> Cow<'t, str> {
        let mut slices = ranges
            .iter()
            .filter_map(|range| {
                let slice = text.get(self.byte_range(*range));
                debug_assert!(slice.is_some(), "message starts outside {text:?}");
                slice
            })
            .filter(|slice| !slice.is_empty());
        let Some(first) = slices.next() else {
            return Cow::Borrowed("");
        };
        let Some(second) = slices.next() else {
            return Cow::Borrowed(first);
        };
        let mut joined = format!("{first}{second}");
        slices.for_each(|slice| joined.push_str(slice));
        Cow::Owned(joined)
    }

    fn byte_range(&self, range: MessageRange) -> Range<usize> {
        self.start_of(range.start)..self.start_of(range.end + 1)
    }

    /// Past the last start, the end of the last message's text.
    fn start_of(&self, ordinal: usize) -> usize {
        let start = ordinal
            .checked_sub(1)
            .and_then(|index| self.starts.get(index))
            .unwrap_or(&self.last_message_end);
        *start as usize
    }
}

/// The [`MessageStarts`] of each of a session's three search texts.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct MessageTextStarts {
    pub full_text: MessageStarts,
    pub agent_search_text: MessageStarts,
    pub thinking_and_tool_call_text: MessageStarts,
}

/// A session's search texts holding only some of its messages, such as those
/// inside a time window. The title, the summary and other text outside every
/// message are left out.
pub struct WindowTexts<'a> {
    pub full_text: Cow<'a, str>,
    pub agent_search_text: Cow<'a, str>,
    pub thinking_and_tool_call_text: Cow<'a, str>,
}

impl<'a> WindowTexts<'a> {
    pub fn into_array(self) -> [Cow<'a, str>; 3] {
        [
            self.full_text,
            self.agent_search_text,
            self.thinking_and_tool_call_text,
        ]
    }
}

impl Conversation {
    pub fn texts_within(&self, window: &TimeFilter) -> WindowTexts<'_> {
        self.texts_of(&self.message_times.ranges_within(window))
    }

    pub fn texts_of(&self, ranges: &[MessageRange]) -> WindowTexts<'_> {
        let starts = &self.message_text_starts;
        WindowTexts {
            full_text: starts.full_text.text_of(&self.full_text, ranges),
            agent_search_text: starts
                .agent_search_text
                .text_of(&self.agent_search_text, ranges),
            thinking_and_tool_call_text: starts
                .thinking_and_tool_call_text
                .text_of(&self.thinking_and_tool_call_text, ranges),
        }
    }
}

/// Each padded separator becomes one space.
pub fn without_message_separators(text: &str) -> Cow<'_, str> {
    if text.contains(PADDED_MESSAGE_SEPARATOR) {
        Cow::Owned(text.replace(PADDED_MESSAGE_SEPARATOR, " "))
    } else {
        Cow::Borrowed(text)
    }
}

/// Append `piece` to `text`, after a separator when `text` is not empty. An
/// empty `piece` adds nothing, separator included.
pub(crate) fn push_separated(text: &mut String, piece: &str) {
    if piece.is_empty() {
        return;
    }
    if !text.is_empty() {
        text.push_str(PADDED_MESSAGE_SEPARATOR);
    }
    text.push_str(piece);
}

/// Collects one search text part by part: the parts outside every message,
/// and each message's parts.
#[derive(Default)]
pub(crate) struct MessageTextBuilder {
    outside_messages: Vec<String>,
    /// `messages[0]` holds m1's parts, one entry each.
    messages: Vec<Vec<String>>,
}

impl MessageTextBuilder {
    /// Add one part's `text` to message `ordinal`, or outside every message
    /// for `None`. An empty `text` adds nothing.
    pub(crate) fn push(&mut self, ordinal: Option<usize>, text: String) {
        if text.is_empty() {
            return;
        }
        let Some(index) = ordinal.and_then(|ordinal| ordinal.checked_sub(1)) else {
            self.outside_messages.push(text);
            return;
        };
        if self.messages.len() <= index {
            self.messages.resize_with(index + 1, Vec::new);
        }
        self.messages[index].push(text);
    }

    pub(crate) fn push_parts(&mut self, ordinal: Option<usize>, parts: Vec<String>) {
        for part in parts {
            self.push(ordinal, part);
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.outside_messages.is_empty() && self.messages.iter().all(Vec::is_empty)
    }

    /// `leading` and the parts outside every message, then the parts of each
    /// of `message_count` messages. Each part is normalised on its own and
    /// written after a separator, so quoted text matches within one part and
    /// each start is an offset into the returned text.
    pub(crate) fn finish<'a>(
        self,
        leading: impl IntoIterator<Item = &'a str>,
        message_count: usize,
        normalize: impl Fn(&str) -> String,
    ) -> (String, MessageStarts) {
        debug_assert!(
            self.messages.len() <= message_count,
            "text pushed for a message past message {message_count}"
        );
        let leading = leading.into_iter().map(str::to_owned);
        let mut text = separated_parts(leading.chain(self.outside_messages), &normalize);
        let mut starts = Vec::with_capacity(message_count);
        let mut messages = self.messages.into_iter();
        for _ in 0..message_count {
            starts.push(text.len());
            let message = separated_parts(messages.next().unwrap_or_default(), &normalize);
            push_separated(&mut text, &message);
        }
        let starts = MessageStarts::of_offsets(starts, text.len());
        (text, starts)
    }

    /// `leading`, then all the builder's text, without starts. A sub-agent
    /// transcript's builder holds its metadata rows and nested sub-agents:
    /// records the parent session does not number as messages.
    pub(crate) fn into_text<'a>(
        self,
        leading: impl IntoIterator<Item = &'a str>,
        normalize: impl Fn(&str) -> String,
    ) -> String {
        let message_count = self.messages.len();
        self.finish(leading, message_count, normalize).0
    }
}

fn separated_parts(
    parts: impl IntoIterator<Item = String>,
    normalize: impl Fn(&str) -> String,
) -> String {
    let mut text = String::new();
    for part in parts {
        push_separated(&mut text, &normalize(&part));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(text: &str) -> String {
        text.to_owned()
    }

    /// The title and an outside part, m1's parts "alpha" and "beta", m2
    /// without text, m3 "gamma".
    fn title_outside_part_and_three_messages() -> (String, MessageStarts) {
        let mut builder = MessageTextBuilder::default();
        builder.push(Some(1), "alpha".to_owned());
        builder.push(None, "outside".to_owned());
        builder.push(Some(3), "gamma".to_owned());
        builder.push(Some(1), "beta".to_owned());
        builder.finish(["title"], 3, identity)
    }

    fn range(start: usize, end: usize) -> MessageRange {
        MessageRange { start, end }
    }

    #[test]
    fn messages_parts_sit_together_after_text_outside_every_message() {
        let (text, _) = title_outside_part_and_three_messages();

        assert_eq!(
            text,
            "title \u{1e} outside \u{1e} alpha \u{1e} beta \u{1e} gamma"
        );
    }

    #[test]
    fn each_messages_slice_holds_its_own_text() {
        let (text, starts) = title_outside_part_and_three_messages();

        assert_eq!(
            starts.text_of(&text, &[range(1, 1)]),
            " \u{1e} alpha \u{1e} beta"
        );
        assert_eq!(starts.text_of(&text, &[range(2, 2)]), "");
        assert_eq!(starts.text_of(&text, &[range(3, 3)]), " \u{1e} gamma");
        assert_eq!(
            starts.text_of(&text, &[range(1, 3)]),
            " \u{1e} alpha \u{1e} beta \u{1e} gamma"
        );
        assert_eq!(starts.text_of(&text, &[range(4, 4)]), "");
    }

    #[test]
    fn separate_ranges_keep_separator_between_their_text() {
        let (text, starts) = title_outside_part_and_three_messages();

        assert_eq!(
            starts.text_of(&text, &[range(1, 1), range(3, 3)]),
            " \u{1e} alpha \u{1e} beta \u{1e} gamma"
        );
    }

    #[test]
    fn text_after_last_message_is_outside_every_message() {
        let (mut text, starts) = title_outside_part_and_three_messages();
        push_separated(&mut text, "nested");

        assert_eq!(starts.text_of(&text, &[range(3, 3)]), " \u{1e} gamma");
    }

    #[test]
    fn without_message_separators_writes_one_space_for_each_separator() {
        let (text, _) = title_outside_part_and_three_messages();

        assert_eq!(
            without_message_separators(&text),
            "title outside alpha beta gamma"
        );
    }

    #[test]
    fn each_part_is_normalised_on_its_own() {
        let mut builder = MessageTextBuilder::default();
        builder.push_parts(Some(1), vec!["  Alpha  ".to_owned(), " Gamma".to_owned()]);
        builder.push(Some(2), "BETA ".to_owned());

        let (text, starts) = builder.finish([], 2, |text| text.trim().to_lowercase());

        assert_eq!(text, "alpha \u{1e} gamma \u{1e} beta");
        assert_eq!(starts.text_of(&text, &[range(1, 1)]), "alpha \u{1e} gamma");
        assert_eq!(starts.text_of(&text, &[range(2, 2)]), " \u{1e} beta");
    }

    #[test]
    fn text_without_message_text_is_stored_without_starts() {
        let (text, starts) = MessageTextBuilder::default().finish(["title"], 4, identity);

        assert_eq!(text, "title");
        assert_eq!(starts.starts, Vec::<u32>::new());
        assert_eq!(starts.text_of(&text, &[range(1, 4)]), "");
    }
}
