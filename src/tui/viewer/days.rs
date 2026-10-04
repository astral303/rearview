//! Calendar days beside the timing column: the label row where the local day
//! changes, the date of the row at the top of the viewer, and the session's
//! date range in the header.

use chrono::{DateTime, Datelike, Local, NaiveDate, TimeZone};
use unicode_width::UnicodeWidthStr;

use super::{LineStyle, NAME_WIDTH, RenderedLine, TIMESTAMP_WIDTH};
use crate::log_entry::LogEntry;

/// The colour of a day label row, the timestamps' grey.
const LABEL_COLOR: (u8, u8, u8) = (140, 140, 140);

const NO_BREAK_SPACE: &str = "\u{a0}";

/// The calendar day of an RFC 3339 timestamp in `zone`.
fn day_in<Tz: TimeZone>(iso_timestamp: &str, zone: &Tz) -> Option<NaiveDate> {
    DateTime::parse_from_rfc3339(iso_timestamp)
        .ok()
        .map(|time| time.with_timezone(zone).date_naive())
}

/// The local calendar day of an RFC 3339 timestamp.
pub(super) fn local_day(iso_timestamp: &str) -> Option<NaiveDate> {
    day_in(iso_timestamp, &Local)
}

/// `Oct 03`, with the year for a date outside `today`'s year.
pub(crate) fn short_date(date: NaiveDate, today: NaiveDate) -> String {
    with_year(date.format("%b %d").to_string(), date, today)
}

/// `Sat Sep 27`, with the year for a date outside `today`'s year.
fn label_date(date: NaiveDate, today: NaiveDate) -> String {
    with_year(date.format("%a %b %d").to_string(), date, today)
}

fn with_year(text: String, date: NaiveDate, today: NaiveDate) -> String {
    if date.year() == today.year() {
        text
    } else {
        format!("{text} {}", date.year())
    }
}

/// The session's dates for the header: `Sep 26 – Oct 03` when its first and
/// last messages fall on different days, or the last message's day and time,
/// `Oct 03 20:00`, when they share one.
pub(crate) fn session_dates<Tz: TimeZone>(
    first: &DateTime<Tz>,
    last: &DateTime<Tz>,
    today: NaiveDate,
) -> String
where
    Tz::Offset: std::fmt::Display,
{
    let (first_day, last_day) = (first.date_naive(), last.date_naive());
    if first_day == last_day {
        format!("{} {}", short_date(last_day, today), last.format("%H:%M"))
    } else {
        format!(
            "{} – {}",
            short_date(first_day, today),
            short_date(last_day, today)
        )
    }
}

/// The row marking where the local day changes: the date across the timing
/// and name columns, ruled to the separator, `Sun Sep 27 ──────┤`. Its
/// columns split as a message row's do, so connector lanes draw through it.
pub(super) fn day_label_row(date: NaiveDate, today: NaiveDate) -> RenderedLine {
    // The date starts in the timing column's first cell, one left of a
    // time, so a current-year date ends before the first connector lane. The
    // rule runs into the separator's first cell, where `┤` meets the
    // separator of the row below. Join the date's parts with no-break
    // spaces: a lane drawn over a plain space would split a past year's date.
    let ruled_width = TIMESTAMP_WIDTH + NAME_WIDTH + 1;
    let label = format!("{} ", label_date(date, today).replace(' ', NO_BREAK_SPACE));
    let rule = "─".repeat(ruled_width.saturating_sub(label.width()));
    let row: Vec<char> = format!("{label}{rule}┤").chars().collect();
    let style = LineStyle {
        fg: Some(LABEL_COLOR),
        ..LineStyle::default()
    };
    let columns = [
        &row[..TIMESTAMP_WIDTH],
        &row[TIMESTAMP_WIDTH..TIMESTAMP_WIDTH + NAME_WIDTH],
        &row[TIMESTAMP_WIDTH + NAME_WIDTH..],
    ];
    RenderedLine::new(
        columns
            .iter()
            .map(|column| (column.iter().collect(), style.clone()))
            .collect(),
    )
}

/// The day label rows of one render: a label before the first row of each
/// new local day while the timing column is shown, none above the first.
pub(super) struct DayLabels {
    shown: bool,
    today: NaiveDate,
    /// The day of the last entry that carried a timestamp.
    current: Option<NaiveDate>,
    /// The latest label's row, until a row follows it.
    unfollowed: Option<usize>,
}

impl DayLabels {
    pub(super) fn new(shown: bool) -> Self {
        Self {
            shown,
            today: Local::now().date_naive(),
            current: None,
            unfollowed: None,
        }
    }

    /// The day `entry` starts, when it falls on another local day than the
    /// entry before it that carried a timestamp.
    pub(super) fn new_day_at(&mut self, entry: &LogEntry) -> Option<NaiveDate> {
        if !self.shown {
            return None;
        }
        let day = local_day(entry.activity_timestamp()?)?;
        let previous = self.current.replace(day);
        previous
            .is_some_and(|previous| previous != day)
            .then_some(day)
    }

    /// Append the label for `day`. A label no row has followed yet takes the
    /// new day instead: the entries since rendered nothing.
    pub(super) fn mark(&mut self, lines: &mut Vec<RenderedLine>, day: NaiveDate) {
        let row = day_label_row(day, self.today);
        match self.unfollowed {
            Some(index) if index + 1 == lines.len() => lines[index] = row,
            _ => {
                self.unfollowed = Some(lines.len());
                lines.push(row);
            }
        }
    }

    /// Remove a last label no row followed.
    pub(super) fn drop_unfollowed(&self, lines: &mut Vec<RenderedLine>) {
        if let Some(index) = self.unfollowed
            && index + 1 == lines.len()
        {
            lines.truncate(index);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::FixedOffset;

    fn date(year: i32, month: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(year, month, day).unwrap()
    }

    fn row_text(line: &RenderedLine) -> String {
        line.spans.iter().map(|(text, _)| text.as_str()).collect()
    }

    #[test]
    fn a_timestamp_falls_on_its_day_in_the_given_zone() {
        let new_york = FixedOffset::west_opt(4 * 3600).unwrap();
        let tokyo = FixedOffset::east_opt(9 * 3600).unwrap();
        let stamp = "2026-09-27T02:30:00Z";

        assert_eq!(day_in(stamp, &new_york), Some(date(2026, 9, 26)));
        assert_eq!(day_in(stamp, &tokyo), Some(date(2026, 9, 27)));
        assert_eq!(day_in("yesterday", &tokyo), None);
    }

    #[test]
    fn dates_carry_the_year_only_outside_the_current_one() {
        let today = date(2026, 10, 3);

        assert_eq!(short_date(date(2026, 9, 27), today), "Sep 27");
        assert_eq!(short_date(date(2025, 9, 27), today), "Sep 27 2025");
        assert_eq!(label_date(date(2026, 9, 27), today), "Sun Sep 27");
        assert_eq!(label_date(date(2025, 9, 27), today), "Sat Sep 27 2025");
    }

    #[test]
    fn the_header_shows_a_range_across_days_and_a_time_within_one() {
        let zone = FixedOffset::east_opt(2 * 3600).unwrap();
        let at = |stamp: &str| {
            DateTime::parse_from_rfc3339(stamp)
                .unwrap()
                .with_timezone(&zone)
        };
        let today = date(2026, 10, 3);

        assert_eq!(
            session_dates(
                &at("2026-09-26T10:00:00+02:00"),
                &at("2026-10-03T20:00:00+02:00"),
                today
            ),
            "Sep 26 – Oct 03"
        );
        assert_eq!(
            session_dates(
                &at("2026-10-03T08:15:00+02:00"),
                &at("2026-10-03T20:00:00+02:00"),
                today
            ),
            "Oct 03 20:00"
        );
        assert_eq!(
            session_dates(
                &at("2025-12-30T10:00:00+02:00"),
                &at("2026-01-02T09:00:00+02:00"),
                today
            ),
            "Dec 30 2025 – Jan 02"
        );
    }

    #[test]
    fn a_day_label_rules_the_date_to_the_separator_in_the_ledger_columns() {
        let row = day_label_row(date(2026, 9, 27), date(2026, 10, 3));

        assert_eq!(row_text(&row), "Sun\u{a0}Sep\u{a0}27 ──────┤");
        let widths: Vec<usize> = row.spans.iter().map(|(text, _)| text.width()).collect();
        assert_eq!(widths, [TIMESTAMP_WIDTH, NAME_WIDTH, 2]);

        let past_year = day_label_row(date(2025, 9, 27), date(2026, 10, 3));
        assert_eq!(row_text(&past_year), "Sat\u{a0}Sep\u{a0}27\u{a0}2025 ─┤");
    }
}
