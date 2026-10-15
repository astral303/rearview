//! The `at=` and `to=` atoms of `agent` output: when the parts of a message,
//! of an outline segment's messages or of a hit's messages were written.

use crate::agent::transcript::AgentMessage;
use chrono::{DateTime, FixedOffset, Local, SecondsFormat, TimeZone};
use std::fmt::Display;
use std::ops::Range;

/// The first and last time on the parts of one or more messages. Times come
/// from parts, not from a message's first record: a Claude Code reply joins
/// records written minutes apart.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TimeSpan {
    first: DateTime<FixedOffset>,
    last: DateTime<FixedOffset>,
}

impl TimeSpan {
    /// The span of the RFC 3339 times on `messages`' parts; `None` when no
    /// part carries one.
    pub fn of_messages<'a>(messages: impl IntoIterator<Item = &'a AgentMessage>) -> Option<Self> {
        messages
            .into_iter()
            .flat_map(|message| &message.parts)
            .filter_map(|part| part.source().timestamp.as_deref())
            .filter_map(|timestamp| DateTime::parse_from_rfc3339(timestamp).ok())
            .map(|time| Self {
                first: time,
                last: time,
            })
            .reduce(Self::union)
    }

    pub fn of_message(message: &AgentMessage) -> Option<Self> {
        Self::of_messages([message])
    }

    pub fn union(self, other: Self) -> Self {
        Self {
            first: self.first.min(other.first),
            last: self.last.max(other.last),
        }
    }

    /// ` at=` the first time and, when the last falls in a later second,
    /// ` to=` the last; each in `zone`, RFC 3339 to the second.
    fn atoms_in<Tz: TimeZone>(self, zone: &Tz) -> String
    where
        Tz::Offset: Display,
    {
        let at = self
            .first
            .with_timezone(zone)
            .to_rfc3339_opts(SecondsFormat::Secs, false);
        let to = self
            .last
            .with_timezone(zone)
            .to_rfc3339_opts(SecondsFormat::Secs, false);
        if at == to {
            format!(" at={at}")
        } else {
            format!(" at={at} to={to}")
        }
    }
}

/// `span`'s atoms in local time; empty without a span.
pub fn time_atoms(span: Option<TimeSpan>) -> String {
    span.map(|span| span.atoms_in(&Local)).unwrap_or_default()
}

/// The span of any range of items, found in constant time. Level `k` holds
/// the span of each range of `2^k` items; two overlapping ranges of one level
/// cover any range up to twice their width.
pub struct RangeSpans {
    levels: Vec<Vec<Option<TimeSpan>>>,
}

impl RangeSpans {
    pub fn new(spans: Vec<Option<TimeSpan>>) -> Self {
        let mut levels = vec![spans];
        let mut width = 1;
        while width * 2 <= levels[0].len() {
            let narrower = levels.last().expect("the first level exists");
            let next = (0..narrower.len() - width)
                .map(|start| union(narrower[start], narrower[start + width]))
                .collect();
            levels.push(next);
            width *= 2;
        }
        Self { levels }
    }

    /// The span of the items in `range`; `None` when none of them has one.
    pub fn of(&self, range: Range<usize>) -> Option<TimeSpan> {
        if range.is_empty() {
            return None;
        }
        let level = range.len().ilog2() as usize;
        let spans = &self.levels[level];
        union(spans[range.start], spans[range.end - (1 << level)])
    }
}

fn union(first: Option<TimeSpan>, second: Option<TimeSpan>) -> Option<TimeSpan> {
    match (first, second) {
        (Some(first), Some(second)) => Some(first.union(second)),
        (first, second) => first.or(second),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::test_support::{text_message, text_message_at};
    use crate::agent::transcript::AgentMessageRole::{Assistant, User};

    fn eastern() -> FixedOffset {
        FixedOffset::west_opt(4 * 3600).unwrap()
    }

    #[test]
    fn one_time_shows_as_at_alone_in_the_zone_with_its_offset() {
        let message = text_message_at(1, User, "question", "2026-10-14T15:41:28.512Z");

        let span = TimeSpan::of_message(&message).unwrap();

        assert_eq!(span.atoms_in(&eastern()), " at=2026-10-14T11:41:28-04:00");
    }

    #[test]
    fn parts_written_seconds_apart_show_their_first_and_last_time() {
        let mut message = text_message_at(1, Assistant, "plan", "2026-10-14T15:41:30Z");
        message
            .parts
            .extend(text_message_at(1, Assistant, "write", "2026-10-14T15:41:28Z").parts);

        let span = TimeSpan::of_message(&message).unwrap();

        assert_eq!(
            span.atoms_in(&eastern()),
            " at=2026-10-14T11:41:28-04:00 to=2026-10-14T11:41:30-04:00"
        );
    }

    #[test]
    fn parts_written_in_the_same_second_show_as_at_alone() {
        let messages = [
            text_message_at(1, User, "question", "2026-10-14T15:41:28.100Z"),
            text_message_at(2, Assistant, "answer", "2026-10-14T15:41:28.900Z"),
        ];

        let span = TimeSpan::of_messages(&messages).unwrap();

        assert_eq!(span.atoms_in(&eastern()), " at=2026-10-14T11:41:28-04:00");
    }

    #[test]
    fn parts_without_an_rfc_3339_time_have_no_span() {
        let messages = [
            text_message(1, User, "question"),
            text_message_at(2, Assistant, "answer", "yesterday"),
        ];

        assert_eq!(TimeSpan::of_messages(&messages), None);
        assert_eq!(time_atoms(None), "");
    }

    #[test]
    fn range_spans_match_the_span_of_each_range_of_items() {
        let minutes = [
            None,
            Some(35),
            Some(31),
            Some(40),
            None,
            Some(33),
            Some(39),
            Some(30),
            Some(36),
            None,
            Some(38),
        ];
        let spans: Vec<Option<TimeSpan>> = minutes
            .iter()
            .map(|minute| {
                minute.and_then(|minute| {
                    let time = format!("2026-10-14T15:{minute}:00Z");
                    TimeSpan::of_message(&text_message_at(1, User, "text", &time))
                })
            })
            .collect();

        let range_spans = RangeSpans::new(spans.clone());

        for start in 0..spans.len() {
            for end in start..=spans.len() {
                let expected = spans[start..end]
                    .iter()
                    .flatten()
                    .copied()
                    .reduce(TimeSpan::union);
                assert_eq!(range_spans.of(start..end), expected, "{start}..{end}");
            }
        }
    }
}
