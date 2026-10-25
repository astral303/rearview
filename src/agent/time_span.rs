//! When the parts of a message, of an outline segment's messages or of a
//! hit's messages were written, as `agent` output shows it: local times
//! without an offset, named once in the header's `tz=`. Read and outline
//! records carry the time of day in position, below a `day` record naming the
//! date; hits, segments and sessions carry `at=` and `to=` in full, with the
//! offset of a time in another offset than the header's.

use crate::agent::transcript::AgentMessage;
use chrono::{DateTime, FixedOffset, Local, NaiveDate, NaiveDateTime, Offset, Utc};
use std::ops::Range;

/// The first and last time on the parts of one or more messages, in whole
/// seconds since the Unix epoch: output shows times to the second, and a time
/// window compares them to the second. Times come from parts, not from a
/// message's first record: a Claude Code reply joins records written minutes
/// apart.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TimeSpan {
    first: i64,
    last: i64,
}

impl TimeSpan {
    /// The second the RFC 3339 time `timestamp` falls in; `None` when it does
    /// not parse.
    pub fn of_timestamp(timestamp: &str) -> Option<Self> {
        let second = DateTime::parse_from_rfc3339(timestamp).ok()?.timestamp();
        Some(Self::from_seconds(second, second))
    }

    /// The span of the RFC 3339 times on `messages`' parts; `None` when no
    /// part carries one.
    pub fn of_messages<'a>(messages: impl IntoIterator<Item = &'a AgentMessage>) -> Option<Self> {
        messages
            .into_iter()
            .flat_map(|message| &message.parts)
            .filter_map(|part| part.source().timestamp.as_deref())
            .filter_map(Self::of_timestamp)
            .reduce(Self::union)
    }

    pub fn of_message(message: &AgentMessage) -> Option<Self> {
        Self::of_messages([message])
    }

    /// The span from second `first` to second `last`, each counted from the
    /// Unix epoch.
    pub(crate) fn from_seconds(first: i64, last: i64) -> Self {
        Self { first, last }
    }

    pub(crate) fn first_second(self) -> i64 {
        self.first
    }

    pub(crate) fn last_second(self) -> i64 {
        self.last
    }

    pub fn union(self, other: Self) -> Self {
        Self {
            first: self.first.min(other.first),
            last: self.last.max(other.last),
        }
    }

    /// True when the span shares a second with the seconds from `from` to
    /// `to`, both included; a `None` bound is open.
    pub fn overlaps(self, from: Option<i64>, to: Option<i64>) -> bool {
        from.is_none_or(|from| self.last >= from) && to.is_none_or(|to| self.first <= to)
    }

    fn first_local(self) -> LocalTime {
        LocalTime::of_second(self.first)
    }

    fn last_local(self) -> LocalTime {
        LocalTime::of_second(self.last)
    }
}

/// ` at=` the first time of `span` and, when the last falls in a later second,
/// ` to=` the last: local, to the second. `header_offset` is the offset the
/// header's `tz=` names; a time in another offset carries its own:
/// `at=2026-02-03T10:00:00-05:00`. Empty without a span.
pub fn time_atoms(span: Option<TimeSpan>, header_offset: FixedOffset) -> String {
    let Some(span) = span else {
        return String::new();
    };
    let at = span.first_local().date_time_below(header_offset);
    if span.first == span.last {
        format!(" at={at}")
    } else {
        format!(
            " at={at} to={}",
            span.last_local().date_time_below(header_offset)
        )
    }
}

/// The offset of an output's times: that of `first`, the first time it
/// shows, or the current local offset when it shows none.
pub fn output_offset(first: Option<TimeSpan>) -> FixedOffset {
    first.map_or_else(
        || Local::now().offset().fix(),
        |span| span.first_local().offset,
    )
}

/// ` tz=-04:00`: the header atom naming the offset of an output's times.
pub fn tz_atom(offset: FixedOffset) -> String {
    format!(" tz={offset}")
}

/// One second as local output shows it: the wall-clock time and the UTC
/// offset it is in.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LocalTime {
    time: NaiveDateTime,
    offset: FixedOffset,
}

impl LocalTime {
    #[cfg(test)]
    pub fn new(time: NaiveDateTime, offset: FixedOffset) -> Self {
        Self { time, offset }
    }

    fn of_second(second: i64) -> Self {
        let time = DateTime::<Utc>::from_timestamp(second, 0)
            .expect("a span's seconds come from parsed times")
            .with_timezone(&Local);
        Self {
            time: time.naive_local(),
            offset: time.offset().fix(),
        }
    }

    fn day(self) -> Day {
        Day {
            date: self.time.date(),
            offset: self.offset,
        }
    }

    fn date_time(self) -> String {
        self.time.format("%Y-%m-%dT%H:%M:%S").to_string()
    }

    /// [`Self::date_time`] below a header naming `header_offset`, with this
    /// time's offset when it differs.
    fn date_time_below(self, header_offset: FixedOffset) -> String {
        if self.offset == header_offset {
            self.date_time()
        } else {
            format!("{}{}", self.date_time(), self.offset)
        }
    }

    fn clock(self) -> String {
        self.time.format("%H:%M:%S").to_string()
    }
}

/// A local date and the offset of the times on it: what a `day` record
/// states for the positional times below it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Day {
    date: NaiveDate,
    offset: FixedOffset,
}

impl Day {
    pub fn offset(self) -> FixedOffset {
        self.offset
    }
}

/// The time of a read or outline record, in position after its address.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RecordTime {
    first: LocalTime,
    last: LocalTime,
}

impl RecordTime {
    pub fn of(span: TimeSpan) -> Self {
        Self::new(span.first_local(), span.last_local())
    }

    pub fn new(first: LocalTime, last: LocalTime) -> Self {
        Self { first, last }
    }

    /// The day of the record's first time: its `day` record states it.
    pub fn day(self) -> Day {
        self.first.day()
    }

    /// ` 14:30:12`, or ` 14:30:12..14:31:05` for a span. The end of a span on
    /// a later date is in full, ` 23:59:50..2026-10-15T00:00:12`, and so is
    /// an end in another offset, with that offset:
    /// ` 01:50:00..2026-11-01T01:10:00-05:00`.
    pub fn field(self) -> String {
        let (first, last) = (self.first, self.last);
        let clock = first.clock();
        if last == first {
            format!(" {clock}")
        } else if last.offset == first.offset && last.time.date() == first.time.date() {
            format!(" {clock}..{}", last.clock())
        } else {
            format!(" {clock}..{}", last.date_time_below(first.offset))
        }
    }
}

/// Places the `day` records of a read or an outline: one before the first
/// timed record after the header or a `conversation` record, and one wherever
/// the next timed record falls on another date or in another offset.
#[derive(Clone, Copy, Debug)]
pub struct DayRecords {
    day: Option<Day>,
    /// The offset in effect: the header's, or the last `day` record's.
    offset: Option<FixedOffset>,
    /// The header's offset: the one it was given, or else the first `day`
    /// record's.
    header_offset: Option<FixedOffset>,
}

impl DayRecords {
    /// Below a header whose `tz=` names `offset`. Without one, the first
    /// `day` record's offset is the header's.
    pub fn below_header(offset: Option<FixedOffset>) -> Self {
        Self {
            day: None,
            offset,
            header_offset: offset,
        }
    }

    /// Already on `day`, as inside one entry of a read measured alone.
    pub fn on(day: Option<Day>) -> Self {
        let offset = day.map(Day::offset);
        Self {
            day,
            offset,
            header_offset: offset,
        }
    }

    /// The offset the header's `tz=` names, once a `day` record is placed
    /// below a header given none.
    pub fn header_offset(self) -> Option<FixedOffset> {
        self.header_offset
    }

    /// After a `conversation` record, the next timed record states its day.
    pub fn restart(&mut self) {
        self.day = None;
    }

    /// The `day` record to print before a record on `day`, when it needs one:
    /// `day 2026-10-14`, with ` tz=` when its offset differs from the one in
    /// effect.
    pub fn record_before(&mut self, day: Day) -> Option<String> {
        if self.day == Some(day) {
            return None;
        }
        let tz = match self.offset {
            Some(offset) if offset != day.offset => tz_atom(day.offset),
            _ => String::new(),
        };
        self.day = Some(day);
        self.offset = Some(day.offset);
        self.header_offset.get_or_insert(day.offset);
        Some(format!("day {}{tz}\n", day.date.format("%Y-%m-%d")))
    }
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

/// The span covering both, or the one present.
pub fn union(first: Option<TimeSpan>, second: Option<TimeSpan>) -> Option<TimeSpan> {
    match (first, second) {
        (Some(first), Some(second)) => Some(first.union(second)),
        (first, second) => first.or(second),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::test_support::{local_time, text_message, text_message_at};
    use crate::agent::transcript::AgentMessageRole::{Assistant, User};

    /// `time_atoms` of `span` below a header in the offset of its first time.
    fn time_atoms_in_own_offset(span: Option<TimeSpan>) -> String {
        time_atoms(span, output_offset(span))
    }

    #[test]
    fn one_time_shows_as_at_alone_in_local_time_without_its_offset() {
        let message = text_message_at(1, User, "question", "2026-10-14T15:41:28.512Z");

        let span = TimeSpan::of_message(&message);

        assert_eq!(
            time_atoms_in_own_offset(span),
            format!(" at={}", local_time("2026-10-14T15:41:28Z"))
        );
    }

    #[test]
    fn full_time_in_another_offset_than_headers_carries_its_own() {
        let span = TimeSpan::of_timestamp("2026-10-14T15:41:28Z");
        let own = output_offset(span);
        let other = FixedOffset::east_opt(own.local_minus_utc() + 3600).unwrap();

        assert_eq!(
            time_atoms(span, other),
            format!(" at={}{own}", local_time("2026-10-14T15:41:28Z"))
        );
    }

    /// January and July: in a zone with daylight saving time, one of them is
    /// in it.
    #[test]
    fn span_ending_in_another_offset_shows_that_offset_on_its_end() {
        let (winter, summer) = ("2026-01-14T15:00:00Z", "2026-07-14T15:00:00Z");
        let span = TimeSpan::of_timestamp(winter)
            .unwrap()
            .union(TimeSpan::of_timestamp(summer).unwrap());
        let (winter_offset, summer_offset) = (
            output_offset(TimeSpan::of_timestamp(winter)),
            output_offset(TimeSpan::of_timestamp(summer)),
        );
        let end_offset = if summer_offset == winter_offset {
            String::new()
        } else {
            summer_offset.to_string()
        };

        assert_eq!(
            time_atoms(Some(span), winter_offset),
            format!(
                " at={} to={}{end_offset}",
                local_time(winter),
                local_time(summer)
            )
        );
    }

    #[test]
    fn parts_written_seconds_apart_show_their_first_and_last_time() {
        let mut message = text_message_at(1, Assistant, "plan", "2026-10-14T15:41:30Z");
        message
            .parts
            .extend(text_message_at(1, Assistant, "write", "2026-10-14T15:41:28Z").parts);

        let span = TimeSpan::of_message(&message);

        assert_eq!(
            time_atoms_in_own_offset(span),
            format!(
                " at={} to={}",
                local_time("2026-10-14T15:41:28Z"),
                local_time("2026-10-14T15:41:30Z")
            )
        );
    }

    #[test]
    fn parts_written_in_same_second_show_as_at_alone() {
        let messages = [
            text_message_at(1, User, "question", "2026-10-14T15:41:28.100Z"),
            text_message_at(2, Assistant, "answer", "2026-10-14T15:41:28.900Z"),
        ];

        let span = TimeSpan::of_messages(&messages);

        assert_eq!(
            time_atoms_in_own_offset(span),
            format!(" at={}", local_time("2026-10-14T15:41:28Z"))
        );
    }

    fn eastern(hours_west: i32) -> FixedOffset {
        FixedOffset::west_opt(hours_west * 3600).unwrap()
    }

    /// `time` (`2026-10-14 23:59:50`) in the offset `hours_west` hours west of
    /// UTC.
    fn at(time: &str, hours_west: i32) -> LocalTime {
        let naive = NaiveDateTime::parse_from_str(time, "%Y-%m-%d %H:%M:%S").unwrap();
        LocalTime::new(naive, eastern(hours_west))
    }

    fn record_time(first: &str, last: &str) -> RecordTime {
        RecordTime::new(at(first, 4), at(last, 4))
    }

    #[test]
    fn record_time_shows_time_of_day_and_spans_end_in_full_on_later_date() {
        assert_eq!(
            record_time("2026-10-14 14:30:12", "2026-10-14 14:30:12").field(),
            " 14:30:12"
        );
        assert_eq!(
            record_time("2026-10-14 14:30:12", "2026-10-14 14:31:05").field(),
            " 14:30:12..14:31:05"
        );
        assert_eq!(
            record_time("2026-10-14 23:59:50", "2026-10-15 00:00:12").field(),
            " 23:59:50..2026-10-15T00:00:12"
        );
    }

    /// Clocks fall back from 02:00 at -04:00 to 01:00 at -05:00: the span's
    /// end reads earlier than its start without its offset.
    #[test]
    fn spans_end_in_another_offset_shows_in_full_with_its_offset() {
        let across_fall_back =
            RecordTime::new(at("2026-11-01 01:50:00", 4), at("2026-11-01 01:10:00", 5));

        assert_eq!(
            across_fall_back.field(),
            " 01:50:00..2026-11-01T01:10:00-05:00"
        );
    }

    fn day(time: &str, hours_west: i32) -> Day {
        RecordTime::new(at(time, hours_west), at(time, hours_west)).day()
    }

    #[test]
    fn first_timed_record_states_its_day_without_offset_when_header_names_it() {
        let mut days = DayRecords::below_header(Some(eastern(4)));

        assert_eq!(
            days.record_before(day("2026-10-14 11:41:25", 4)).as_deref(),
            Some("day 2026-10-14\n")
        );
        assert_eq!(days.record_before(day("2026-10-14 11:42:09", 4)), None);
    }

    #[test]
    fn record_on_another_date_states_its_day() {
        let mut days = DayRecords::below_header(None);
        days.record_before(day("2026-10-14 23:59:50", 4));

        assert_eq!(
            days.record_before(day("2026-10-15 00:00:12", 4)).as_deref(),
            Some("day 2026-10-15\n")
        );
        assert_eq!(days.header_offset(), Some(eastern(4)));
    }

    #[test]
    fn record_in_another_offset_states_its_day_with_new_offset() {
        let mut days = DayRecords::below_header(Some(eastern(4)));
        days.record_before(day("2026-11-01 01:59:59", 4));

        assert_eq!(
            days.record_before(day("2026-11-01 01:00:00", 5)).as_deref(),
            Some("day 2026-11-01 tz=-05:00\n")
        );
        assert_eq!(
            days.record_before(day("2026-11-02 09:00:00", 5)).as_deref(),
            Some("day 2026-11-02\n")
        );
    }

    #[test]
    fn conversation_record_restarts_day_and_keeps_offset() {
        let mut days = DayRecords::below_header(Some(eastern(4)));
        days.record_before(day("2026-10-14 11:41:25", 4));

        days.restart();

        assert_eq!(
            days.record_before(day("2026-10-14 11:41:26", 4)).as_deref(),
            Some("day 2026-10-14\n")
        );
    }

    #[test]
    fn header_without_offset_names_first_day_records_offset() {
        let mut days = DayRecords::below_header(None);

        assert_eq!(
            days.record_before(day("2026-11-02 09:00:00", 5)).as_deref(),
            Some("day 2026-11-02\n")
        );
        assert_eq!(days.header_offset(), Some(eastern(5)));
    }

    #[test]
    fn header_without_offset_keeps_first_day_records_offset_after_change() {
        let mut days = DayRecords::below_header(None);
        days.record_before(day("2026-11-01 01:59:59", 4));

        assert_eq!(
            days.record_before(day("2026-11-01 01:00:00", 5)).as_deref(),
            Some("day 2026-11-01 tz=-05:00\n")
        );
        assert_eq!(days.header_offset(), Some(eastern(4)));
    }

    #[test]
    fn parts_without_rfc_3339_time_have_no_span() {
        let messages = [
            text_message(1, User, "question"),
            text_message_at(2, Assistant, "answer", "yesterday"),
        ];

        assert_eq!(TimeSpan::of_messages(&messages), None);
        assert_eq!(time_atoms_in_own_offset(None), "");
    }

    #[test]
    fn range_spans_match_span_of_each_range_of_items() {
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
