//! The JS date pieces the usage code leans on: `Date.parse`, `toISOString`, and the
//! `Intl.DateTimeFormat("en-CA", {timeZone})` day of an instant.

use jiff::tz::TimeZone;
use jiff::Timestamp;

const MS_PER_DAY: i64 = 86_400_000;
/// ±8.64e15 ms: the range of a JS `Date`.
const MAX_TIME: f64 = 8.64e15;

/// Days since 1970-01-01 of a proleptic Gregorian date (month 1..=12; the day may overflow
/// the month, as JS `MakeDay` lets it).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let yoe = year - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468 + (day - 1)
}

/// `TimeClip`: `NaN` outside the JS range, integral otherwise.
fn time_clip(value: f64) -> f64 {
    if !value.is_finite() || value.abs() > MAX_TIME {
        f64::NAN
    } else {
        value.trunc() + 0.0
    }
}

struct Cursor<'a> {
    b: &'a [u8],
    i: usize,
}

impl Cursor<'_> {
    fn digits(&mut self, count: usize) -> Option<i64> {
        let slice = self.b.get(self.i..self.i + count)?;
        if !slice.iter().all(u8::is_ascii_digit) {
            return None;
        }
        self.i += count;
        Some(slice.iter().fold(0i64, |acc, digit| acc * 10 + i64::from(digit - b'0')))
    }

    fn eat(&mut self, byte: u8) -> bool {
        if self.b.get(self.i) == Some(&byte) {
            self.i += 1;
            true
        } else {
            false
        }
    }

    fn done(&self) -> bool {
        self.i == self.b.len()
    }
}

/// `Date.parse(text)` for the ECMAScript date-time string format (`YYYY[-MM[-DD]]`,
/// `±YYYYYY`, `THH:mm[:ss[.sss…]]`, `Z` or `±HH:mm`), plus the space separator V8 also
/// accepts. Date-only forms are UTC; a date-time without an offset is local time. `None`
/// is `NaN`.
pub fn date_parse(text: &str) -> Option<f64> {
    let mut cursor = Cursor { b: text.as_bytes(), i: 0 };
    let year = match cursor.b.first()? {
        b'+' | b'-' => {
            let negative = cursor.b[0] == b'-';
            cursor.i = 1;
            let year = cursor.digits(6)?;
            if negative && year == 0 {
                return None;
            }
            if negative {
                -year
            } else {
                year
            }
        }
        _ => cursor.digits(4)?,
    };
    let mut month = 1;
    let mut day = 1;
    if cursor.eat(b'-') {
        month = cursor.digits(2)?;
        if cursor.eat(b'-') {
            day = cursor.digits(2)?;
        }
    }
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let date_days = days_from_civil(year, month, day);
    if cursor.done() {
        return Some(time_clip((date_days * MS_PER_DAY) as f64)).filter(|value| !value.is_nan());
    }
    if !(cursor.eat(b'T') || cursor.eat(b' ')) {
        return None;
    }
    let hour = cursor.digits(2)?;
    if !cursor.eat(b':') {
        return None;
    }
    let minute = cursor.digits(2)?;
    let mut second = 0;
    let mut millis = 0;
    if cursor.eat(b':') {
        second = cursor.digits(2)?;
        if cursor.eat(b'.') {
            let start = cursor.i;
            while cursor.b.get(cursor.i).is_some_and(u8::is_ascii_digit) {
                cursor.i += 1;
            }
            let fraction = &cursor.b[start..cursor.i];
            if fraction.is_empty() {
                return None;
            }
            for (index, digit) in fraction.iter().take(3).enumerate() {
                millis += i64::from(digit - b'0') * [100, 10, 1][index];
            }
        }
    }
    if hour > 24 || minute > 59 || second > 59 || (hour == 24 && (minute > 0 || second > 0 || millis > 0)) {
        return None;
    }
    let local_ms = date_days * MS_PER_DAY + ((hour * 60 + minute) * 60 + second) * 1000 + millis;
    let offset_ms = if cursor.eat(b'Z') {
        Some(0)
    } else if matches!(cursor.b.get(cursor.i), Some(b'+' | b'-')) {
        let sign = if cursor.b[cursor.i] == b'-' { -1 } else { 1 };
        cursor.i += 1;
        let offset_hours = cursor.digits(2)?;
        cursor.eat(b':');
        let offset_minutes = cursor.digits(2)?;
        if offset_hours > 23 || offset_minutes > 59 {
            return None;
        }
        Some(sign * (offset_hours * 60 + offset_minutes) * 60_000)
    } else {
        None
    };
    if !cursor.done() {
        return None;
    }
    let utc_ms = match offset_ms {
        Some(offset) => local_ms - offset,
        None => local_to_utc(local_ms)?,
    };
    Some(time_clip(utc_ms as f64)).filter(|value| !value.is_nan())
}

/// A local wall-clock time (as if it were UTC milliseconds) to an instant, in the system
/// time zone, resolving gaps and folds like `Date` (`compatible`).
fn local_to_utc(local_ms: i64) -> Option<i64> {
    let civil = Timestamp::from_millisecond(local_ms).ok()?.to_zoned(TimeZone::UTC).datetime();
    let zoned = civil.to_zoned(TimeZone::system()).ok()?;
    Some(zoned.timestamp().as_millisecond())
}

/// `new Date(millis).toISOString()` (millis truncated like `TimeClip`); `None` where JS throws.
pub fn iso_from_millis(millis: f64) -> Option<String> {
    let clipped = time_clip(millis);
    if clipped.is_nan() {
        return None;
    }
    #[allow(clippy::cast_possible_truncation)]
    zc_core::time::try_iso_from_millis(clipped as i64)
}

/// The reporting time zone of `Intl.DateTimeFormat`: an IANA name (case-insensitive), or a
/// `±HH:mm` offset; an unknown zone degrades to UTC like the TS formatter's fallback.
pub fn resolve_time_zone(name: &str) -> TimeZone {
    if let Some(zone) = parse_offset_zone(name) {
        return zone;
    }
    TimeZone::get(name).unwrap_or(TimeZone::UTC)
}

fn parse_offset_zone(name: &str) -> Option<TimeZone> {
    let bytes = name.as_bytes();
    let sign = match bytes.first()? {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    let rest = &name[1..];
    let (hours, minutes) = match rest.len() {
        2 => (rest.parse::<i32>().ok()?, 0),
        4 => (rest[..2].parse::<i32>().ok()?, rest[2..].parse::<i32>().ok()?),
        5 if rest.as_bytes()[2] == b':' => (rest[..2].parse::<i32>().ok()?, rest[3..].parse::<i32>().ok()?),
        _ => return None,
    };
    if hours > 23 || minutes > 59 || !rest.bytes().all(|b| b.is_ascii_digit() || b == b':') {
        return None;
    }
    let offset = jiff::tz::Offset::from_seconds(sign * (hours * 3600 + minutes * 60)).ok()?;
    Some(TimeZone::fixed(offset))
}

/// `YYYY-MM-DD` of an instant in `zone` (`Intl.DateTimeFormat("en-CA", …)`).
pub fn format_day(zone: &TimeZone, timestamp_ms: f64) -> String {
    let clipped = time_clip(timestamp_ms);
    if clipped.is_nan() {
        // `format(new Date(NaN))` throws in JS; the parsers never produce such records.
        return String::new();
    }
    #[allow(clippy::cast_possible_truncation)]
    let Ok(timestamp) = Timestamp::from_millisecond(clipped as i64) else {
        return String::new();
    };
    let date = timestamp.to_zoned(zone.clone()).date();
    let year = date.year();
    if (0..=9999).contains(&year) {
        format!("{year:04}-{:02}-{:02}", date.month(), date.day())
    } else {
        format!("{year}-{:02}-{:02}", date.month(), date.day())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn date_parse_iso_forms() {
        assert_eq!(date_parse("2026-08-07T04:05:13.944Z"), Some(1_786_075_513_944.0));
        assert_eq!(date_parse("2026-08-01T10:00:00Z"), Some(1_785_578_400_000.0));
        assert_eq!(date_parse("2026-08-01T10:00Z"), Some(1_785_578_400_000.0));
        assert_eq!(date_parse("2026-08-01T12:00:00+02:00"), Some(1_785_578_400_000.0));
        assert_eq!(date_parse("2026-08-01"), Some(1_785_542_400_000.0));
        assert_eq!(date_parse("2026-08-01T10:00:00.123456Z"), Some(1_785_578_400_123.0));
        assert_eq!(date_parse("not a date"), None);
        assert_eq!(date_parse("2026-13-01"), None);
        assert_eq!(date_parse("2026-08-01T10:00:00Zjunk"), None);
    }

    #[test]
    fn days_in_zones() {
        let millis = date_parse("2026-08-07T04:05:13.944Z").unwrap();
        assert_eq!(format_day(&resolve_time_zone("UTC"), millis), "2026-08-07");
        assert_eq!(format_day(&resolve_time_zone("America/Los_Angeles"), millis), "2026-08-06");
        assert_eq!(format_day(&resolve_time_zone("america/los_angeles"), millis), "2026-08-06");
        assert_eq!(format_day(&resolve_time_zone("Not/AZone"), millis), "2026-08-07");
        assert_eq!(format_day(&resolve_time_zone("-05:00"), millis), "2026-08-06");
        assert_eq!(iso_from_millis(1_785_578_400_123.9).as_deref(), Some("2026-08-01T10:00:00.123Z"));
    }
}
