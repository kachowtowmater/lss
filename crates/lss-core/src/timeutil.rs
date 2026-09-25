//! Timestamp helpers. Everything internal is unix seconds (i64).

use chrono::{DateTime, Local, TimeZone, Utc};

/// RFC 3339, with or without fractional seconds (`2026-09-19T11:36:29.229739584Z`).
pub fn parse_rfc3339(s: &str) -> Option<i64> {
    DateTime::parse_from_rfc3339(s.trim()).ok().map(|d| d.timestamp())
}

/// ISO-8601 as journald (`-o short-iso`) and the gate print it: the offset may be `-07:00`
/// or `-0700`.
pub fn parse_iso_loose(s: &str) -> Option<i64> {
    let s = s.trim();
    parse_rfc3339(s).or_else(|| {
        DateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%z").ok().map(|d| d.timestamp())
    })
}

/// `1h02m`, `3m05s`, `42s`, `2d03h`.
pub fn fmt_duration(secs: i64) -> String {
    let s = secs.max(0);
    if s >= 86_400 {
        format!("{}d{:02}h", s / 86_400, (s % 86_400) / 3600)
    } else if s >= 3600 {
        format!("{}h{:02}m", s / 3600, (s % 3600) / 60)
    } else if s >= 60 {
        format!("{}m{:02}s", s / 60, s % 60)
    } else {
        format!("{s}s")
    }
}

/// Local wall-clock rendering of a unix timestamp, e.g. `09-19 04:41`.
pub fn fmt_local(ts: i64, fmt: &str) -> String {
    match Local.timestamp_opt(ts, 0).single() {
        Some(d) => d.format(fmt).to_string(),
        None => "-".into(),
    }
}

/// UTC rendering of a unix timestamp - for text that must read the same on every box, whatever
/// its time zone (card #261: the probe times quoted in a C1 alert).
pub fn fmt_utc(ts: i64, fmt: &str) -> String {
    match Utc.timestamp_opt(ts, 0).single() {
        Some(d) => d.format(fmt).to_string(),
        None => "-".into(),
    }
}

/// Unix timestamp of the most recent local midnight at or before `now`.
pub fn local_midnight(now: i64) -> i64 {
    let Some(d) = Local.timestamp_opt(now, 0).single() else { return now - now.rem_euclid(86_400) };
    let midnight = d.date_naive().and_hms_opt(0, 0, 0).expect("00:00:00 is valid");
    Local
        .from_local_datetime(&midnight)
        .earliest()
        .map(|m| m.timestamp())
        .unwrap_or(now - now.rem_euclid(86_400))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_docker_and_journal_stamps() {
        assert_eq!(parse_rfc3339("2026-09-19T11:36:29.229739584Z"), Some(1_789_817_789));
        assert_eq!(parse_iso_loose("2026-09-18T21:22:57-07:00"), Some(1_789_791_777));
        assert_eq!(parse_iso_loose("2026-09-18T21:22:57-0700"), Some(1_789_791_777));
        assert_eq!(parse_iso_loose("2026-09-19T11:41:48+0000"), Some(1_789_818_108));
        assert_eq!(parse_iso_loose("garbage"), None);
    }

    #[test]
    fn durations() {
        assert_eq!(fmt_duration(42), "42s");
        assert_eq!(fmt_duration(185), "3m05s");
        assert_eq!(fmt_duration(3720), "1h02m");
        assert_eq!(fmt_duration(183_600), "2d03h");
        assert_eq!(fmt_duration(-5), "0s");
    }
}
