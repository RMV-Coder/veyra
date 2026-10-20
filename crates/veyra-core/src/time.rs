//! Time formatting shared by `log`, `op log`, `checkpoint list` and friends.

/// Current time as seconds since the Unix epoch.
pub fn now_seconds() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or_default()
}

/// Git's default date format: `Tue Oct 20 10:15:00 2026 +0800`, in the signature's own offset.
pub fn format_git(seconds: i64, offset_seconds: i32) -> String {
    let Ok(ts) = jiff::Timestamp::from_second(seconds) else {
        return seconds.to_string();
    };
    let Ok(offset) = jiff::tz::Offset::from_seconds(offset_seconds) else {
        return seconds.to_string();
    };
    ts.to_zoned(offset.to_time_zone()).strftime("%a %b %-d %H:%M:%S %Y %z").to_string()
}

/// Local time, e.g. `2026-10-20 10:15`.
pub fn format_local(seconds: i64) -> String {
    let Ok(ts) = jiff::Timestamp::from_second(seconds) else {
        return seconds.to_string();
    };
    ts.to_zoned(jiff::tz::TimeZone::system()).strftime("%Y-%m-%d %H:%M").to_string()
}

/// Human relative time: `just now`, `5 minutes ago`, `3 days ago`.
pub fn format_relative(seconds: i64, now: i64) -> String {
    let delta = now - seconds;
    if delta < 0 {
        return "in the future".into();
    }
    let (n, unit) = match delta {
        0..=44 => return "just now".into(),
        45..=2_699 => ((delta + 30) / 60, "minute"),
        2_700..=79_199 => ((delta + 1800) / 3600, "hour"),
        79_200..=2_246_399 => ((delta + 43_200) / 86_400, "day"),
        2_246_400..=27_647_999 => ((delta + 1_296_000) / 2_592_000, "month"),
        _ => ((delta + 15_768_000) / 31_536_000, "year"),
    };
    let n = n.max(1);
    format!("{n} {unit}{} ago", if n == 1 { "" } else { "s" })
}

/// Parse durations like `30m`, `12h`, `14d`, `2w` into seconds.
pub fn parse_duration(s: &str) -> Option<i64> {
    let s = s.trim();
    let (num, unit) = s.split_at(s.find(|c: char| !c.is_ascii_digit())?);
    let n: i64 = num.parse().ok()?;
    let mult = match unit.trim() {
        "s" | "sec" | "secs" => 1,
        "m" | "min" | "mins" => 60,
        "h" | "hr" | "hrs" | "hour" | "hours" => 3600,
        "d" | "day" | "days" => 86_400,
        "w" | "week" | "weeks" => 604_800,
        _ => return None,
    };
    Some(n * mult)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_times() {
        assert_eq!(format_relative(1000, 1010), "just now");
        assert_eq!(format_relative(0, 120), "2 minutes ago");
        assert_eq!(format_relative(0, 3 * 86_400), "3 days ago");
        assert_eq!(format_relative(0, 3600), "1 hour ago");
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration("10m"), Some(600));
        assert_eq!(parse_duration("14d"), Some(14 * 86_400));
        assert_eq!(parse_duration("2w"), Some(2 * 604_800));
        assert_eq!(parse_duration("abc"), None);
        assert_eq!(parse_duration("5x"), None);
    }

    #[test]
    fn git_format_uses_offset() {
        assert_eq!(format_git(0, 0), "Thu Jan 1 00:00:00 1970 +0000");
        assert_eq!(format_git(0, 8 * 3600), "Thu Jan 1 08:00:00 1970 +0800");
    }
}
