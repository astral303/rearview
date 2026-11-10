//! The one source of time for rearview: every read of the current time and
//! every measured interval goes through a [`Clock`]. The entry point
//! constructs one [`SystemClock`] and passes it down; a test passes its own
//! [`TestClock`], so tests run on test time and in parallel.
//!
//! These waits stay outside the clock and run on real time in tests too:
//!
//! - the terminal's event poll; the clock computes only its timeout
//! - SQLite busy timeouts
//! - the updater's connect timeout and spinner, timed by curl and indicatif

use chrono::{DateTime, Local, NaiveDate, Utc};
use std::ops::Add;
use std::time::{Duration, Instant};

pub(crate) trait Clock: Send + Sync {
    /// The current instant in UTC. Convert it to local time where it is
    /// shown or compared with local times.
    fn wall(&self) -> DateTime<Utc>;

    /// The local date of [`Self::wall`]. Dates on screen are shown relative
    /// to it.
    fn today(&self) -> NaiveDate {
        self.wall().with_timezone(&Local).date_naive()
    }

    /// The current moment on this clock's monotonic timeline, for intervals
    /// and deadlines. Unlike [`Self::wall`], it never moves back.
    fn now(&self) -> Moment;

    /// Wait `duration` of this clock's time.
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "only tests sleep; a production wait sleeps here, not in std::thread"
        )
    )]
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

    fn now(&self) -> Moment {
        Moment(self.constructed.elapsed())
    }

    fn sleep(&self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

/// Test time: it starts at [`TestClock::start_wall`] and moves only when a
/// test sleeps on it, advances it or sets its wall time, so a wait finishes
/// at once and in order.
#[cfg(test)]
pub(crate) struct TestClock {
    time: std::sync::Mutex<TestTime>,
}

#[cfg(test)]
struct TestTime {
    since_start: Duration,
    wall: DateTime<Utc>,
}

#[cfg(test)]
impl TestClock {
    pub(crate) fn start_wall() -> DateTime<Utc> {
        use chrono::TimeZone;
        Utc.with_ymd_and_hms(2026, 7, 1, 12, 0, 0).unwrap()
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
        }
    }
}

#[cfg(test)]
impl Clock for TestClock {
    fn wall(&self) -> DateTime<Utc> {
        self.time.lock().unwrap().wall
    }

    fn now(&self) -> Moment {
        Moment(self.time.lock().unwrap().since_start)
    }

    fn sleep(&self, duration: Duration) {
        self.advance(duration);
    }
}
