//! Wire dates (`2026-10-01T12:00:00.000Z`) as milliseconds, and short human durations.

use zc_contracts::DateTimeUtc;

/// Milliseconds since the epoch, or `None` for a malformed date.
pub fn millis(iso: &str) -> Option<i64> {
    DateTimeUtc::parse(iso).ok().map(DateTimeUtc::as_millis)
}

/// Now, in milliseconds since the epoch.
pub fn now_millis() -> i64 {
    DateTimeUtc::now().as_millis()
}

/// "now", "5m", "3h", "2d", "4w" (the sidebar's ages).
pub fn short_age(from_ms: i64, now_ms: i64) -> String {
    let seconds = (now_ms - from_ms).max(0) / 1000;
    match seconds {
        s if s < 60 => "now".into(),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86_400 => format!("{}h", s / 3600),
        s if s < 7 * 86_400 => format!("{}d", s / 86_400),
        s => format!("{}w", s / (7 * 86_400)),
    }
}

/// "4s", "2m 05s", "1h 12m" (how long something ran).
pub fn duration(ms: i64) -> String {
    let seconds = ms.max(0) / 1000;
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3600 {
        format!("{}m {:02}s", seconds / 60, seconds % 60)
    } else {
        format!("{}h {:02}m", seconds / 3600, (seconds % 3600) / 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ages_and_durations() {
        let t = millis("2026-10-01T12:00:00.000Z").unwrap();
        assert_eq!(short_age(t, t + 30_000), "now");
        assert_eq!(short_age(t, t + 5 * 60_000), "5m");
        assert_eq!(short_age(t, t + 3 * 3_600_000), "3h");
        assert_eq!(short_age(t, t + 2 * 86_400_000), "2d");
        assert_eq!(duration(4_200), "4s");
        assert_eq!(duration(125_000), "2m 05s");
        assert_eq!(duration(4_320_000), "1h 12m");
        assert_eq!(millis("nope"), None);
    }
}
