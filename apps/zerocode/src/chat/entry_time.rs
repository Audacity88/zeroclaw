//! Wall-clock times on transcript message headers.

use std::fmt::Display;

use chrono::{DateTime, Local, NaiveDate, TimeZone};

/// Parse a daemon `created_at` (RFC 3339) into local time. A value that does
/// not parse yields no time, so the header simply shows none.
pub(super) fn parse_created_at(value: &str) -> Option<DateTime<Local>> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|at| at.with_timezone(&Local))
}

/// Compact header label: `HH:MM` when `at` falls on `today`, otherwise
/// `YYYY-MM-DD HH:MM`. Both are numeric, so no locale text is involved.
pub(super) fn header_label<Tz>(at: &DateTime<Tz>, today: NaiveDate) -> String
where
    Tz: TimeZone,
    Tz::Offset: Display,
{
    if at.date_naive() == today {
        at.format("%H:%M").to_string()
    } else {
        at.format("%Y-%m-%d %H:%M").to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::FixedOffset;

    fn at(rfc3339: &str) -> DateTime<FixedOffset> {
        DateTime::parse_from_rfc3339(rfc3339).unwrap()
    }

    #[test]
    fn header_label_is_clock_time_today_and_dated_otherwise() {
        let stamp = at("2026-10-09T01:46:07+08:00");
        let today = NaiveDate::from_ymd_opt(2026, 10, 9).unwrap();
        let tomorrow = NaiveDate::from_ymd_opt(2026, 10, 10).unwrap();
        assert_eq!(header_label(&stamp, today), "01:46");
        assert_eq!(header_label(&stamp, tomorrow), "2026-10-09 01:46");
    }

    #[test]
    fn parse_created_at_accepts_rfc3339_and_rejects_garbage() {
        let parsed = parse_created_at("2026-10-08T17:46:07.123456+00:00").unwrap();
        assert_eq!(
            parsed.with_timezone(&chrono::Utc).to_rfc3339(),
            "2026-10-08T17:46:07.123456+00:00"
        );
        assert!(parse_created_at("yesterday-ish").is_none());
        assert!(parse_created_at("").is_none());
    }
}
