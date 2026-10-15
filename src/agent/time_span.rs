//! The `at=` and `to=` atoms of `agent` output: when the parts of a message,
//! of an outline segment's messages or of a hit's messages were written.

use crate::agent::transcript::AgentMessage;
use chrono::{DateTime, FixedOffset, Local, SecondsFormat, TimeZone};
use std::fmt::Display;

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
}
