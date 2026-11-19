//! Durations as tool headers print them.

use std::time::Duration;

/// `49s`, `1m`, `1m 30s`, `1h 4m`: hours and minutes from an hour on,
/// minutes and seconds below it, and a unit of zero left out. Below one
/// second, the seconds with their decimals: `0.25s`.
pub(crate) fn format_duration(duration: Duration) -> String {
    let seconds = duration.as_secs();
    if seconds == 0 {
        return format!("{}s", duration.as_millis() as f64 / 1_000.0);
    }
    let (hours, minutes, seconds) = (seconds / 3_600, seconds / 60 % 60, seconds % 60);
    let units = if hours > 0 {
        [(hours, "h"), (minutes, "m")]
    } else {
        [(minutes, "m"), (seconds, "s")]
    };
    units
        .iter()
        .filter(|(count, _)| *count > 0)
        .map(|(count, unit)| format!("{count}{unit}"))
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn formatted(milliseconds: u64) -> String {
        format_duration(Duration::from_millis(milliseconds))
    }

    #[test]
    fn duration_reads_in_its_two_largest_units_without_zero_ones() {
        assert_eq!(formatted(49_000), "49s");
        assert_eq!(formatted(60_000), "1m");
        assert_eq!(formatted(90_000), "1m 30s");
        assert_eq!(formatted(3_600_000), "1h");
        assert_eq!(formatted(3_840_000 + 59_000), "1h 4m");
    }

    #[test]
    fn duration_below_one_second_reads_in_decimal_seconds() {
        assert_eq!(formatted(0), "0s");
        assert_eq!(formatted(250), "0.25s");
        assert_eq!(formatted(1), "0.001s");
    }
}
