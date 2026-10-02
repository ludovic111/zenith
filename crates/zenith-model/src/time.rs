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

/// The web sidebar's age of a thread: "now", "5m", "3h", "2d" (`formatRelativeTime`, without
/// "ago"; no weeks).
pub fn sidebar_age(from_ms: i64, now_ms: i64) -> String {
    let seconds = (now_ms - from_ms).max(0) / 1000;
    match seconds {
        s if s < 60 => "now".into(),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86_400 => format!("{}h", s / 3600),
        s => format!("{}d", s / 86_400),
    }
}

/// How long a thread has been working, in the sidebar: "42s", "7m", "1h 5m"
/// (`formatWorkingDurationLabel`).
pub fn working_label(ms: i64) -> String {
    let seconds = ms.max(0) / 1000;
    if seconds < 60 {
        return format!("{seconds}s");
    }
    let minutes = seconds / 60;
    if minutes < 60 {
        return format!("{minutes}m");
    }
    format!("{}h {}m", minutes / 60, minutes % 60)
}

/// A duration as the web's timeline writes it ("Worked for 7m 7s"): "250ms", "4.2s", "42s",
/// then hours, minutes and seconds without the zero parts (`formatDuration`).
pub fn format_duration(ms: i64) -> String {
    if ms < 0 {
        return "0ms".into();
    }
    if ms < 1_000 {
        return format!("{}ms", ms.max(1));
    }
    if ms < 10_000 {
        let tenths = (ms as f64 / 100.).round() / 10.;
        return if tenths >= 10. { "10s".into() } else { format!("{tenths:.1}s") };
    }
    if ms < 60_000 {
        return format!("{}s", (ms as f64 / 1_000.).round() as i64);
    }
    let total = (ms as f64 / 1_000.).round() as i64;
    let (hours, minutes, seconds) = (total / 3_600, (total % 3_600) / 60, total % 60);
    [(hours, "h"), (minutes, "m"), (seconds, "s")]
        .iter()
        .filter(|(n, _)| *n > 0)
        .map(|(n, unit)| format!("{n}{unit}"))
        .collect::<Vec<_>>()
        .join(" ")
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
        assert_eq!(sidebar_age(t, t + 9 * 86_400_000), "9d");
        assert_eq!(working_label(42_000), "42s");
        assert_eq!(working_label(31 * 60_000 + 5_000), "31m");
        assert_eq!(working_label(65 * 60_000), "1h 5m");
        assert_eq!(format_duration(250), "250ms");
        assert_eq!(format_duration(4_249), "4.2s");
        assert_eq!(format_duration(9_960), "10s");
        assert_eq!(format_duration(42_400), "42s");
        assert_eq!(format_duration(427_000), "7m 7s");
        assert_eq!(format_duration(3_600_000), "1h");
        assert_eq!(format_duration(3_660_000 + 4_000), "1h 1m 4s");
        assert_eq!(duration(125_000), "2m 05s");
        assert_eq!(duration(4_320_000), "1h 12m");
        assert_eq!(millis("nope"), None);
    }
}
