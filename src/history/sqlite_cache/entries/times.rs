use chrono::{DateTime, SecondsFormat};
use rusqlite::types::Type;

pub(super) fn millis(timestamp: Option<&str>) -> Option<i64> {
    DateTime::parse_from_rfc3339(timestamp?)
        .ok()
        .map(|time| time.timestamp_millis())
}

pub(super) fn text(millis: i64) -> rusqlite::Result<String> {
    DateTime::from_timestamp_millis(millis)
        .map(|time| time.to_rfc3339_opts(SecondsFormat::Millis, true))
        .ok_or_else(|| {
            rusqlite::Error::FromSqlConversionFailure(
                5,
                Type::Integer,
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "cached timestamp is out of range",
                )
                .into(),
            )
        })
}
