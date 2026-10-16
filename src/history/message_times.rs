//! When each of a session's messages was written, as the session list and its
//! cache keep it: the span `agent read` shows as a message's `at=` and `to=`.

use crate::agent::refs::MessageRange;
use crate::agent::time_span::TimeSpan;
use crate::agent::transcript::AgentMessage;
use crate::time_filter::TimeFilter;
use serde::{Deserialize, Serialize};

/// Each message's [`TimeSpan`], in message order: the first is m1's. Kept as
/// seconds after the session's earliest time, each offset 4 bytes. A time
/// more than 136 years after the earliest does not fit, and its message reads
/// as one without a time.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct MessageTimes {
    earliest: i64,
    spans: Vec<Option<SecondsAfterEarliest>>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct SecondsAfterEarliest {
    first: u32,
    last: u32,
}

impl MessageTimes {
    /// `spans[0]` is m1's span, `None` for a message without a time.
    pub fn new(spans: &[Option<TimeSpan>]) -> Self {
        let earliest = spans
            .iter()
            .flatten()
            .map(|span| span.first_second())
            .min()
            .unwrap_or_default();
        let after_earliest = |second: i64| u32::try_from(second - earliest).ok();
        Self {
            earliest,
            spans: spans
                .iter()
                .map(|span| {
                    let span = (*span)?;
                    Some(SecondsAfterEarliest {
                        first: after_earliest(span.first_second())?,
                        last: after_earliest(span.last_second())?,
                    })
                })
                .collect(),
        }
    }

    /// The spans of a transcript's `messages`, numbered from m1 in order.
    pub fn of_messages(messages: &[AgentMessage]) -> Self {
        Self::new(
            &messages
                .iter()
                .map(TimeSpan::of_message)
                .collect::<Vec<_>>(),
        )
    }

    /// Message `ordinal`'s span; `None` when it has no time or the session
    /// has no such message.
    pub fn span(&self, ordinal: usize) -> Option<TimeSpan> {
        let seconds = (*self.spans.get(ordinal.checked_sub(1)?)?)?;
        Some(TimeSpan::from_seconds(
            self.earliest + i64::from(seconds.first),
            self.earliest + i64::from(seconds.last),
        ))
    }

    /// True when a message falls inside `window`, and for a window without
    /// bounds.
    pub fn has_message_within(&self, window: &TimeFilter) -> bool {
        !window.is_active()
            || (1..=self.spans.len()).any(|ordinal| window.contains_message(self.span(ordinal)))
    }

    /// The runs of consecutive messages inside `window`, in message order.
    pub fn ranges_within(&self, window: &TimeFilter) -> Vec<MessageRange> {
        let mut ranges: Vec<MessageRange> = Vec::new();
        for ordinal in 1..=self.spans.len() {
            if !window.contains_message(self.span(ordinal)) {
                continue;
            }
            match ranges.last_mut() {
                Some(range) if range.end + 1 == ordinal => range.end = ordinal,
                _ => ranges.push(MessageRange::single(ordinal)),
            }
        }
        ranges
    }

    /// The last second of the messages inside `window`, no later than the
    /// window's end; `None` when no message falls inside it.
    pub fn last_second_within(&self, window: &TimeFilter) -> Option<i64> {
        let last = (1..=self.spans.len())
            .filter_map(|ordinal| self.span(ordinal))
            .filter(|span| window.contains_message(Some(*span)))
            .map(TimeSpan::last_second)
            .max()?;
        Some(
            window
                .before
                .map_or(last, |before| last.min(before.timestamp())),
        )
    }

    /// `range` from its first to its last message inside `window`; `None`
    /// when none of its messages is.
    pub fn narrowed_to(&self, range: MessageRange, window: &TimeFilter) -> Option<MessageRange> {
        let mut inside = (range.start..=range.end)
            .filter(|&ordinal| window.contains_message(self.span(ordinal)));
        let start = inside.next()?;
        let end = inside.next_back().unwrap_or(start);
        Some(MessageRange { start, end })
    }
}

/// Each message's span, widened record by record as the records are
/// numbered.
#[derive(Default)]
pub(crate) struct MessageTimesBuilder {
    spans: Vec<Option<TimeSpan>>,
}

impl MessageTimesBuilder {
    /// Widen message `ordinal`'s span to hold `timestamp`, the RFC 3339 time
    /// of one of its records.
    pub(crate) fn add(&mut self, ordinal: usize, timestamp: Option<&str>) {
        if self.spans.len() < ordinal {
            self.spans.resize(ordinal, None);
        }
        let Some(time) = timestamp.and_then(TimeSpan::of_timestamp) else {
            return;
        };
        let span = &mut self.spans[ordinal - 1];
        *span = Some(span.map_or(time, |span| span.union(time)));
    }

    pub(crate) fn finish(mut self, message_count: usize) -> MessageTimes {
        self.spans.resize(message_count, None);
        MessageTimes::new(&self.spans)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Local, TimeZone};

    fn second(minute: u32, second: u32) -> i64 {
        chrono::Utc
            .with_ymd_and_hms(2026, 10, 14, 15, minute, second)
            .unwrap()
            .timestamp()
    }

    fn span(first: (u32, u32), last: (u32, u32)) -> Option<TimeSpan> {
        Some(TimeSpan::from_seconds(
            second(first.0, first.1),
            second(last.0, last.1),
        ))
    }

    /// The window from `first` to `last`, minute and second, both included.
    fn window(first: (u32, u32), last: (u32, u32)) -> TimeFilter {
        let local = |(minute, second_of_minute)| {
            Local
                .timestamp_opt(second(minute, second_of_minute), 0)
                .unwrap()
        };
        TimeFilter {
            after: Some(local(first)),
            before: Some(local(last)),
        }
    }

    /// m1 at 15:38:00, m2 from 15:38:10 to 15:41:30, m3 without a time, m4
    /// at 15:42:00.
    fn times() -> MessageTimes {
        MessageTimes::new(&[
            span((38, 0), (38, 0)),
            span((38, 10), (41, 30)),
            None,
            span((42, 0), (42, 0)),
        ])
    }

    #[test]
    fn each_message_keeps_its_span() {
        let times = times();

        assert_eq!(times.span(1), span((38, 0), (38, 0)));
        assert_eq!(times.span(2), span((38, 10), (41, 30)));
        assert_eq!(times.span(3), None);
        assert_eq!(times.span(4), span((42, 0), (42, 0)));
        assert_eq!(times.span(0), None);
        assert_eq!(times.span(5), None);
    }

    #[test]
    fn a_session_idle_through_the_whole_window_doesnt_have_a_message_within_it() {
        let times = times();

        assert!(!times.has_message_within(&window((41, 31), (41, 59))));
        assert!(times.has_message_within(&window((41, 30), (41, 59))));
        assert!(times.has_message_within(&window((39, 0), (40, 0))));
        assert!(times.has_message_within(&TimeFilter::default()));
    }

    #[test]
    fn a_range_narrows_to_its_messages_inside_the_window() {
        let times = times();
        let range = MessageRange { start: 1, end: 4 };

        assert_eq!(
            times.narrowed_to(range, &window((39, 0), (42, 0))),
            Some(MessageRange { start: 2, end: 4 })
        );
        assert_eq!(
            times.narrowed_to(range, &window((38, 0), (38, 5))),
            Some(MessageRange::single(1))
        );
        assert_eq!(times.narrowed_to(range, &window((41, 31), (41, 59))), None);
        assert_eq!(
            times.narrowed_to(range, &TimeFilter::default()),
            Some(range)
        );
    }

    #[test]
    fn a_window_selects_runs_of_consecutive_messages_inside_it() {
        let times = times();

        assert_eq!(
            times.ranges_within(&window((38, 5), (42, 0))),
            [MessageRange::single(2), MessageRange::single(4)]
        );
        assert_eq!(
            times.ranges_within(&window((38, 0), (41, 0))),
            [MessageRange { start: 1, end: 2 }]
        );
        assert_eq!(times.ranges_within(&window((41, 31), (41, 59))), []);
        assert_eq!(
            times.ranges_within(&TimeFilter::default()),
            [MessageRange { start: 1, end: 4 }]
        );
    }

    #[test]
    fn the_last_second_inside_a_window_stops_at_the_windows_end() {
        let times = times();

        assert_eq!(
            times.last_second_within(&window((38, 0), (40, 0))),
            Some(second(40, 0))
        );
        assert_eq!(
            times.last_second_within(&window((38, 0), (42, 30))),
            Some(second(42, 0))
        );
        assert_eq!(times.last_second_within(&window((41, 31), (41, 59))), None);
    }

    #[test]
    fn the_builder_spans_each_message_from_its_records_times() {
        let mut builder = MessageTimesBuilder::default();
        builder.add(1, Some("2026-10-14T15:38:00Z"));
        builder.add(2, Some("2026-10-14T15:41:30.400Z"));
        builder.add(2, Some("2026-10-14T15:38:10Z"));
        builder.add(3, None);
        builder.add(4, Some("not a time"));

        let times = builder.finish(5);

        assert_eq!(times.span(1), span((38, 0), (38, 0)));
        assert_eq!(times.span(2), span((38, 10), (41, 30)));
        assert_eq!(times.span(3), None);
        assert_eq!(times.span(4), None);
        assert_eq!(times.span(5), None);
        assert_eq!(times.spans.len(), 5);
    }

    #[test]
    fn a_message_written_too_long_after_the_earliest_reads_as_one_without_a_time() {
        let earliest = second(38, 0);
        let too_far = earliest + i64::from(u32::MAX) + 1;

        let times = MessageTimes::new(&[
            Some(TimeSpan::from_seconds(earliest, earliest)),
            Some(TimeSpan::from_seconds(too_far, too_far)),
        ]);

        assert_eq!(
            times.span(1),
            Some(TimeSpan::from_seconds(earliest, earliest))
        );
        assert_eq!(times.span(2), None);
    }
}
