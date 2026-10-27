//! The one source of time for rearview: every read of the current time and
//! every measured interval goes through a [`Clock`], and every local time is
//! shown and read in its [`Zone`]. The entry point constructs one
//! [`SystemClock`] and passes it down; a test passes its own [`TestClock`],
//! so tests run on test time, in a zone of their choice, and in parallel.
//!
//! These waits stay outside the clock and run on real time in tests too:
//!
//! - the terminal's event poll; the clock computes only its timeout
//! - SQLite busy timeouts
//! - the updater's connect timeout and spinner, timed by curl and indicatif

use chrono::{
    DateTime, FixedOffset, Local, MappedLocalTime, NaiveDate, NaiveDateTime, NaiveTime, Offset,
    TimeZone, Utc,
};
use std::fmt;
use std::ops::Add;
use std::time::{Duration, Instant};

pub(crate) trait Clock: Send + Sync {
    /// The current instant in UTC. Convert it to [`Self::zone`] where it is
    /// shown or compared with local times.
    fn wall(&self) -> DateTime<Utc>;

    /// The time zone local times are shown and read in.
    fn zone(&self) -> Zone;

    /// [`Self::wall`] in [`Self::zone`].
    fn local_wall(&self) -> DateTime<Zone> {
        self.wall().with_timezone(&self.zone())
    }

    /// The local date of [`Self::wall`]. Dates on screen are shown relative
    /// to it.
    fn today(&self) -> NaiveDate {
        self.local_wall().date_naive()
    }

    /// The current moment on this clock's monotonic timeline, for intervals
    /// and deadlines. Unlike [`Self::wall`], it never moves back.
    fn now(&self) -> Moment;

    /// Wait `duration` of this clock's time.
    fn sleep(&self, duration: Duration);

    /// The time since `start`; zero when `start` is later.
    fn elapsed_since(&self, start: Moment) -> Duration {
        self.now().saturating_duration_since(start)
    }
}

/// A point on one clock's monotonic timeline. Compare only moments of one
/// clock.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct Moment(Duration);

impl Moment {
    /// The time from `earlier` to this moment; zero when `earlier` is later.
    pub(crate) fn saturating_duration_since(self, earlier: Self) -> Duration {
        self.0.saturating_sub(earlier.0)
    }

    /// The time from `earlier` to this moment; `None` when `earlier` is
    /// later.
    pub(crate) fn checked_duration_since(self, earlier: Self) -> Option<Duration> {
        self.0.checked_sub(earlier.0)
    }
}

impl Add<Duration> for Moment {
    type Output = Self;

    fn add(self, duration: Duration) -> Self {
        Self(self.0 + duration)
    }
}

/// A time zone local times are shown and read in.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Zone {
    /// The operating system's local time zone.
    System,
    /// One offset all year.
    #[cfg(test)]
    Fixed(FixedOffset),
    /// US Eastern Time: UTC-4 from 02:00 on the second Sunday in March to
    /// 02:00 on the first Sunday in November, UTC-5 otherwise.
    #[cfg(test)]
    UsEastern,
}

impl Zone {
    fn offset_at(self, utc: &NaiveDateTime) -> FixedOffset {
        match self {
            Self::System => Local.offset_from_utc_datetime(utc),
            #[cfg(test)]
            Self::Fixed(offset) => offset,
            #[cfg(test)]
            Self::UsEastern => us_eastern::offset_at(utc),
        }
    }

    fn offsets_at_local(self, local: &NaiveDateTime) -> MappedLocalTime<FixedOffset> {
        match self {
            Self::System => Local.offset_from_local_datetime(local),
            #[cfg(test)]
            Self::Fixed(offset) => MappedLocalTime::Single(offset),
            #[cfg(test)]
            Self::UsEastern => us_eastern::offsets_at_local(local),
        }
    }

    fn with_offset(self, offset: FixedOffset) -> ZoneOffset {
        ZoneOffset { zone: self, offset }
    }
}

/// A [`Zone`]'s offset at one moment, with the zone: a time moved by a
/// duration takes the zone's offset at its new moment.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ZoneOffset {
    zone: Zone,
    offset: FixedOffset,
}

impl Offset for ZoneOffset {
    fn fix(&self) -> FixedOffset {
        self.offset
    }
}

impl fmt::Display for ZoneOffset {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.offset.fmt(formatter)
    }
}

impl TimeZone for Zone {
    type Offset = ZoneOffset;

    fn from_offset(offset: &ZoneOffset) -> Self {
        offset.zone
    }

    fn offset_from_local_date(&self, local: &NaiveDate) -> MappedLocalTime<ZoneOffset> {
        self.offset_from_local_datetime(&local.and_time(NaiveTime::MIN))
    }

    fn offset_from_local_datetime(&self, local: &NaiveDateTime) -> MappedLocalTime<ZoneOffset> {
        self.offsets_at_local(local)
            .map(|offset| self.with_offset(offset))
    }

    fn offset_from_utc_date(&self, utc: &NaiveDate) -> ZoneOffset {
        self.offset_from_utc_datetime(&utc.and_time(NaiveTime::MIN))
    }

    fn offset_from_utc_datetime(&self, utc: &NaiveDateTime) -> ZoneOffset {
        self.with_offset(self.offset_at(utc))
    }
}

#[cfg(test)]
mod us_eastern {
    use chrono::{Datelike, FixedOffset, MappedLocalTime, NaiveDate, NaiveDateTime, Weekday};

    const HOUR: i32 = 3600;

    fn standard() -> FixedOffset {
        FixedOffset::west_opt(5 * HOUR).unwrap()
    }

    fn daylight() -> FixedOffset {
        FixedOffset::west_opt(4 * HOUR).unwrap()
    }

    /// The UTC times daylight saving time starts and ends in `year`: 02:00
    /// standard time on the second Sunday in March, 02:00 daylight time on
    /// the first Sunday in November.
    fn daylight_saving_in(year: i32) -> std::ops::Range<NaiveDateTime> {
        let sunday = |month, nth, utc_hour| {
            NaiveDate::from_weekday_of_month_opt(year, month, Weekday::Sun, nth)
                .and_then(|day| day.and_hms_opt(utc_hour, 0, 0))
                .unwrap()
        };
        sunday(3, 2, 7)..sunday(11, 1, 6)
    }

    pub(super) fn offset_at(utc: &NaiveDateTime) -> FixedOffset {
        if daylight_saving_in(utc.year()).contains(utc) {
            daylight()
        } else {
            standard()
        }
    }

    /// The offsets `local` can be in: each one whose UTC time for `local`
    /// the zone is in then. None in the hour skipped in March; both, the
    /// earlier instant's first, in the hour repeated in November.
    pub(super) fn offsets_at_local(local: &NaiveDateTime) -> MappedLocalTime<FixedOffset> {
        let fitting: Vec<FixedOffset> = [daylight(), standard()]
            .into_iter()
            .filter(|&offset| offset_at(&(*local - offset)) == offset)
            .collect();
        match fitting[..] {
            [offset] => MappedLocalTime::Single(offset),
            [earlier, later] => MappedLocalTime::Ambiguous(earlier, later),
            _ => MappedLocalTime::None,
        }
    }
}

/// The operating system's clocks.
pub(crate) struct SystemClock {
    /// The origin of this clock's moments.
    constructed: Instant,
}

impl SystemClock {
    pub(crate) fn new() -> Self {
        Self {
            constructed: Instant::now(),
        }
    }
}

impl Clock for SystemClock {
    fn wall(&self) -> DateTime<Utc> {
        Utc::now()
    }

    fn zone(&self) -> Zone {
        Zone::System
    }

    fn now(&self) -> Moment {
        Moment(self.constructed.elapsed())
    }

    fn sleep(&self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

/// Test time: it starts at [`TestClock::start_wall`] and moves only when a
/// test sleeps on it, advances it or sets its wall time, so a wait finishes
/// at once and in order. Its zone is the system's unless a test gives one.
#[cfg(test)]
pub(crate) struct TestClock {
    time: std::sync::Mutex<TestTime>,
    zone: Zone,
}

#[cfg(test)]
struct TestTime {
    since_start: Duration,
    wall: DateTime<Utc>,
}

#[cfg(test)]
impl TestClock {
    pub(crate) fn start_wall() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 7, 1, 12, 0, 0).unwrap()
    }

    /// A clock at [`Self::start_wall`] whose local times are in `zone`.
    pub(crate) fn in_zone(zone: Zone) -> Self {
        Self {
            zone,
            ..Self::default()
        }
    }

    /// Move monotonic and wall time forward by `duration`, as a sleep does.
    pub(crate) fn advance(&self, duration: Duration) {
        let mut time = self.time.lock().unwrap();
        time.since_start += duration;
        time.wall += chrono::TimeDelta::from_std(duration).unwrap();
    }

    /// Set the wall time to `wall` without moving monotonic time, as a
    /// system clock adjustment does.
    pub(crate) fn set_wall(&self, wall: DateTime<Utc>) {
        self.time.lock().unwrap().wall = wall;
    }
}

#[cfg(test)]
impl Default for TestClock {
    fn default() -> Self {
        Self {
            time: std::sync::Mutex::new(TestTime {
                since_start: Duration::ZERO,
                wall: Self::start_wall(),
            }),
            zone: Zone::System,
        }
    }
}

#[cfg(test)]
impl Clock for TestClock {
    fn wall(&self) -> DateTime<Utc> {
        self.time.lock().unwrap().wall
    }

    fn zone(&self) -> Zone {
        self.zone
    }

    fn now(&self) -> Moment {
        Moment(self.time.lock().unwrap().since_start)
    }

    fn sleep(&self, duration: Duration) {
        self.advance(duration);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utc(rfc_3339: &str) -> NaiveDateTime {
        DateTime::parse_from_rfc3339(rfc_3339).unwrap().naive_utc()
    }

    fn local(time: &str) -> NaiveDateTime {
        NaiveDateTime::parse_from_str(time, "%Y-%m-%d %H:%M:%S").unwrap()
    }

    fn hours_east(hours: i32) -> FixedOffset {
        FixedOffset::east_opt(hours * 3600).unwrap()
    }

    fn us_eastern_offset_at(utc_time: &str) -> FixedOffset {
        Zone::UsEastern
            .offset_from_utc_datetime(&utc(utc_time))
            .fix()
    }

    #[test]
    fn us_eastern_time_is_five_hours_behind_utc_in_winter_and_four_in_summer() {
        assert_eq!(us_eastern_offset_at("2026-01-15T12:00:00Z"), hours_east(-5));
        assert_eq!(us_eastern_offset_at("2026-07-15T12:00:00Z"), hours_east(-4));
    }

    /// In 2026 clocks go forward on 8 March and back on 1 November, each
    /// time at 02:00 local time.
    #[test]
    fn us_eastern_time_changes_offset_at_two_in_morning_on_each_transition_day() {
        assert_eq!(us_eastern_offset_at("2026-03-08T06:59:59Z"), hours_east(-5));
        assert_eq!(us_eastern_offset_at("2026-03-08T07:00:00Z"), hours_east(-4));
        assert_eq!(us_eastern_offset_at("2026-11-01T05:59:59Z"), hours_east(-4));
        assert_eq!(us_eastern_offset_at("2026-11-01T06:00:00Z"), hours_east(-5));
    }

    /// A local time in the skipped hour has no offset; one in the repeated
    /// hour has both, the earlier instant's first.
    #[test]
    fn us_eastern_time_skips_hour_in_march_and_repeats_one_in_november() {
        let offsets = |time| {
            Zone::UsEastern
                .offset_from_local_datetime(&local(time))
                .map(|offset| offset.fix())
        };

        assert_eq!(
            offsets("2026-03-08 01:59:59"),
            MappedLocalTime::Single(hours_east(-5))
        );
        assert_eq!(offsets("2026-03-08 02:30:00"), MappedLocalTime::None);
        assert_eq!(
            offsets("2026-03-08 03:00:00"),
            MappedLocalTime::Single(hours_east(-4))
        );
        assert_eq!(
            offsets("2026-11-01 00:59:59"),
            MappedLocalTime::Single(hours_east(-4))
        );
        assert_eq!(
            offsets("2026-11-01 01:30:00"),
            MappedLocalTime::Ambiguous(hours_east(-4), hours_east(-5))
        );
        assert_eq!(
            offsets("2026-11-01 02:00:00"),
            MappedLocalTime::Single(hours_east(-5))
        );
    }

    #[test]
    fn us_eastern_time_moved_across_transition_takes_offset_after_it() {
        let before = Zone::UsEastern.from_utc_datetime(&utc("2026-11-01T05:30:00Z"));

        let after = before + chrono::TimeDelta::hours(1);

        assert_eq!(after.to_rfc3339(), "2026-11-01T01:30:00-05:00");
    }

    #[test]
    fn test_clocks_local_time_and_date_are_in_its_zone() {
        let clock = TestClock::in_zone(Zone::Fixed(hours_east(9)));
        clock.set_wall(utc("2026-07-01T20:00:00Z").and_utc());

        assert_eq!(clock.local_wall().to_rfc3339(), "2026-07-02T05:00:00+09:00");
        assert_eq!(clock.today(), NaiveDate::from_ymd_opt(2026, 7, 2).unwrap());
    }
}
