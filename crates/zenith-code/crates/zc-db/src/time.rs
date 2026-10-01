//! Timestamps as the TS server stores them: `Date.prototype.toISOString()`,
//! `YYYY-MM-DDTHH:mm:ss.sssZ`. They are compared as text in SQL (`expires_at > ?`), so the
//! format must be exact.

use jiff::{tz::TimeZone, Timestamp};

/// `toISOString()` of `timestamp` (millisecond precision, truncated, `Z`).
pub fn format_iso(timestamp: Timestamp) -> String {
    let millis = timestamp.as_millisecond();
    let truncated = Timestamp::from_millisecond(millis).unwrap_or(timestamp);
    let civil = truncated.to_zoned(TimeZone::UTC).datetime();
    let year = civil.year();
    let year = if (0..=9999).contains(&year) {
        format!("{year:04}")
    } else if year < 0 {
        format!("-{:06}", -(year as i32))
    } else {
        format!("+{year:06}")
    };
    format!(
        "{year}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        civil.month(),
        civil.day(),
        civil.hour(),
        civil.minute(),
        civil.second(),
        civil.subsec_nanosecond() / 1_000_000
    )
}

/// The current time, formatted with [`format_iso`].
pub fn now_iso() -> String {
    format_iso(Timestamp::now())
}

/// Parses a stored timestamp (`Schema.DateTimeUtcFromString`). Accepts any RFC 3339 instant;
/// the TS server only ever writes [`format_iso`] output.
pub fn parse_iso(value: &str) -> Option<Timestamp> {
    value.parse::<Timestamp>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_like_to_iso_string() {
        let ts: Timestamp = "2026-10-01T12:00:00.123456Z".parse().unwrap();
        assert_eq!(format_iso(ts), "2026-10-01T12:00:00.123Z");
        let ts: Timestamp = "2026-01-02T03:04:05Z".parse().unwrap();
        assert_eq!(format_iso(ts), "2026-01-02T03:04:05.000Z");
        assert_eq!(parse_iso("2026-01-02T03:04:05.000Z"), Some(ts));
        assert_eq!(parse_iso("nope"), None);
    }
}
