//! Clock helpers whose output is byte-identical to JavaScript.
//!
//! Every timestamp the TS server writes (SQLite TEXT columns, `server-runtime.json`, wire
//! `DateTimeUtc` values) comes from `Date.prototype.toISOString` (directly or through Effect's
//! `DateTime.formatIso`): `YYYY-MM-DDTHH:mm:ss.sssZ`, always milliseconds, always `Z`. Years
//! outside 0..=9999 use the expanded `+YYYYYY` / `-YYYYYY` form. [`iso_from_millis`] reproduces
//! that exactly; [`parse_iso_millis`] reads it (and other RFC 3339 forms) back.

use std::time::{SystemTime, UNIX_EPOCH};

/// The largest magnitude a JS `Date` accepts (±8.64e15 ms, i.e. ±100,000,000 days).
pub const JS_DATE_MAX_MILLIS: i64 = 8_640_000_000_000_000;

/// Milliseconds since the Unix epoch, like `Date.now()`.
pub fn now_millis() -> i64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(elapsed) => elapsed.as_millis() as i64,
        Err(before) => -(before.duration().as_millis() as i64),
    }
}

/// `new Date().toISOString()`.
pub fn now_iso() -> String {
    iso_from_millis(now_millis())
}

/// `new Date(millis).toISOString()`.
///
/// Panics like JS throws (`RangeError: Invalid time value`) when `millis` is outside
/// ±[`JS_DATE_MAX_MILLIS`]; use [`try_iso_from_millis`] to get `None` instead.
pub fn iso_from_millis(millis: i64) -> String {
    try_iso_from_millis(millis).expect("time value outside the range of a JS Date")
}

/// `new Date(millis).toISOString()`, or `None` where JS would throw.
pub fn try_iso_from_millis(millis: i64) -> Option<String> {
    if !(-JS_DATE_MAX_MILLIS..=JS_DATE_MAX_MILLIS).contains(&millis) {
        return None;
    }
    let days = millis.div_euclid(86_400_000);
    let ms_of_day = millis.rem_euclid(86_400_000);
    let (year, month, day) = civil_from_days(days);
    let hour = ms_of_day / 3_600_000;
    let minute = (ms_of_day / 60_000) % 60;
    let second = (ms_of_day / 1000) % 60;
    let milli = ms_of_day % 1000;
    let year_text = if (0..=9999).contains(&year) {
        format!("{year:04}")
    } else if year < 0 {
        format!("-{:06}", -year)
    } else {
        format!("+{year:06}")
    };
    Some(format!("{year_text}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{milli:03}Z"))
}

/// Parse an ISO 8601 / RFC 3339 instant (`…Z` or with an offset, any sub-second precision) into
/// epoch milliseconds, truncating below the millisecond like `Date.parse`. Also accepts the
/// expanded `±YYYYYY` years that [`iso_from_millis`] emits.
pub fn parse_iso_millis(input: &str) -> Option<i64> {
    let trimmed = input.trim();
    if let Some(millis) = parse_canonical(trimmed) {
        return Some(millis);
    }
    let timestamp: jiff::Timestamp = trimmed.parse().ok()?;
    let millis = timestamp.as_millisecond();
    (-JS_DATE_MAX_MILLIS..=JS_DATE_MAX_MILLIS).contains(&millis).then_some(millis)
}

/// Normalize any accepted instant to the canonical JS form (`2026-10-01T12:00:00.000Z`).
pub fn normalize_iso(input: &str) -> Option<String> {
    parse_iso_millis(input).and_then(try_iso_from_millis)
}

fn parse_canonical(input: &str) -> Option<i64> {
    // `YYYY-MM-DDTHH:mm:ss.sssZ` or `±YYYYYY-MM-DDTHH:mm:ss.sssZ`, exactly what toISOString
    // emits. Parsed by hand because jiff's range stops short of 9999-12-31T23:59:59.999Z.
    let bytes = input.as_bytes();
    let (sign, year_len, offset): (i64, usize, usize) = match (bytes.len(), bytes.first()) {
        (24, Some(b'0'..=b'9')) => (1, 4, 0),
        (27, Some(b'+')) => (1, 6, 1),
        (27, Some(b'-')) => (-1, 6, 1),
        _ => return None,
    };
    // Index of the 4th year digit's successor: everything after the year has a fixed layout.
    let base = offset + year_len - 4;
    let separator = |index: usize, expected: u8| bytes.get(base + index) == Some(&expected);
    let separators_ok = separator(4, b'-')
        && separator(7, b'-')
        && separator(10, b'T')
        && separator(13, b':')
        && separator(16, b':')
        && separator(19, b'.')
        && separator(23, b'Z');
    if !separators_ok {
        return None;
    }
    let num = |from: usize, len: usize| -> Option<i64> {
        let text = input.get(from..from + len)?;
        text.bytes().all(|b| b.is_ascii_digit()).then(|| text.parse().ok()).flatten()
    };
    let year = sign * num(offset, year_len)?;
    let month = num(base + 5, 2)?;
    let day = num(base + 8, 2)?;
    let hour = num(base + 11, 2)?;
    let minute = num(base + 14, 2)?;
    let second = num(base + 17, 2)?;
    let milli = num(base + 20, 3)?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    let days = days_from_civil(year, month as u32, day as u32);
    let millis = days * 86_400_000 + hour * 3_600_000 + minute * 60_000 + second * 1000 + milli;
    (-JS_DATE_MAX_MILLIS..=JS_DATE_MAX_MILLIS).contains(&millis).then_some(millis)
}

/// Howard Hinnant's `civil_from_days`: proleptic Gregorian date for a day count since 1970-01-01.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let m = month as i64;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + day as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Produced by `new Date(ms).toISOString()` on Node 26.
    const JS_FIXTURES: &[(i64, &str)] = &[
        (0, "1970-01-01T00:00:00.000Z"),
        (1, "1970-01-01T00:00:00.001Z"),
        (999, "1970-01-01T00:00:00.999Z"),
        (1000, "1970-01-01T00:00:01.000Z"),
        (-1, "1969-12-31T23:59:59.999Z"),
        (-1000, "1969-12-31T23:59:59.000Z"),
        (-62_135_596_800_000, "0001-01-01T00:00:00.000Z"),
        (-62_167_219_200_000, "0000-01-01T00:00:00.000Z"),
        (-62_167_219_200_001, "-000001-12-31T23:59:59.999Z"),
        (253_402_300_799_999, "9999-12-31T23:59:59.999Z"),
        (253_402_300_800_000, "+010000-01-01T00:00:00.000Z"),
        (8_640_000_000_000_000, "+275760-09-13T00:00:00.000Z"),
        (-8_640_000_000_000_000, "-271821-04-20T00:00:00.000Z"),
        (1_790_000_000_123, "2026-09-21T14:13:20.123Z"),
        (951_782_400_000, "2000-02-29T00:00:00.000Z"),
        (1_709_210_096_789, "2024-02-29T12:34:56.789Z"),
        (4_102_444_800_000, "2100-01-01T00:00:00.000Z"),
        (-2_208_988_800_000, "1900-01-01T00:00:00.000Z"),
    ];

    #[test]
    fn matches_js_to_iso_string_byte_for_byte() {
        for (millis, expected) in JS_FIXTURES {
            assert_eq!(iso_from_millis(*millis), *expected, "millis {millis}");
        }
    }

    #[test]
    fn round_trips_through_parse() {
        for (millis, text) in JS_FIXTURES {
            assert_eq!(parse_iso_millis(text), Some(*millis), "text {text}");
        }
    }

    #[test]
    fn out_of_range_is_rejected_like_js() {
        assert_eq!(try_iso_from_millis(JS_DATE_MAX_MILLIS + 1), None);
        assert_eq!(try_iso_from_millis(-JS_DATE_MAX_MILLIS - 1), None);
    }

    #[test]
    fn parses_offsets_and_extra_precision() {
        assert_eq!(normalize_iso("2026-10-01T14:00:00.123456+02:00").as_deref(), Some("2026-10-01T12:00:00.123Z"));
        assert_eq!(normalize_iso("2026-10-01T12:00:00Z").as_deref(), Some("2026-10-01T12:00:00.000Z"));
        assert_eq!(parse_iso_millis("not a date"), None);
    }

    #[test]
    fn now_has_canonical_shape() {
        let now = now_iso();
        assert_eq!(now.len(), 24);
        assert!(now.ends_with('Z'));
        assert_eq!(&now[19..20], ".");
    }
}
