//! Time and time zones (docs: reference/types, "Dates and times").
//!
//! - A `DateTime` is an instant, kept in UTC: `now()`, a record's `created_at`, the time of an
//!   event. Two of them compare without ambiguity, across projects and with other systems.
//! - A `Date` is a day of the calendar, in no zone: a due date, a birthday.
//! - The project's zone (`timezone` in quadra.toml, default UTC) is used only where one turns
//!   into the other: `today()`, `dt.date`, `dt.hour`, `dt.weekday`, `datetime(date, time)`, and
//!   adding days (`+ 1 day` moves one day of the calendar there; `+ 24 hours` 24 exact hours).
//! - Another zone is said explicitly: `today(tz)`, `dt.in(tz).date`, `datetime(date, time, tz)`.
//!
//! A local time that does not exist (the hour skipped when clocks move forward) moves forward
//! by the gap; one that happens twice (clocks moving back) takes the first.

use chrono::{Duration, LocalResult, NaiveDateTime, Offset, TimeZone};
use chrono_tz::Tz;

/// The zone of a name (`Europe/Rome`); an empty name is UTC.
pub fn zone(name: &str) -> Result<Tz, String> {
    if name.is_empty() {
        return Ok(Tz::UTC);
    }
    name.parse::<Tz>()
        .map_err(|_| format!("`{name}` is not a time zone (an IANA name such as `Europe/Rome`, `America/New_York` or `UTC`)"))
}

/// The wall-clock time of an instant in a zone.
pub fn local(utc: NaiveDateTime, tz: Tz) -> NaiveDateTime {
    tz.from_utc_datetime(&utc).naive_local()
}

/// The instant of a wall-clock time in a zone. A time skipped by a change of the clocks moves
/// forward by the gap; a time that happens twice is the first of the two.
pub fn instant(local: NaiveDateTime, tz: Tz) -> NaiveDateTime {
    match tz.from_local_datetime(&local) {
        LocalResult::Single(t) => t.naive_utc(),
        LocalResult::Ambiguous(first, _) => first.naive_utc(),
        LocalResult::None => {
            // in the gap: read with the offset before it, the time lands after the gap by as much
            // as it was into it (02:30 when 02:00 jumps to 03:00 is 03:30)
            let mut probe = local;
            for _ in 0..(24 * 4) {
                probe -= Duration::minutes(15);
                if let LocalResult::Single(t) | LocalResult::Ambiguous(t, _) = tz.from_local_datetime(&probe) {
                    let before = t.offset().fix().local_minus_utc() as i64;
                    return local - Duration::seconds(before);
                }
            }
            local
        }
    }
}

/// The offset of a zone from UTC at an instant, in seconds (`+3600` for Rome in winter).
pub fn offset(utc: NaiveDateTime, tz: Tz) -> i64 {
    tz.from_utc_datetime(&utc).offset().fix().local_minus_utc() as i64
}

/// `n` days of the calendar after an instant, in a zone: the same wall-clock time, even across
/// a change of the clocks.
pub fn add_days(utc: NaiveDateTime, days: i64, tz: Tz) -> NaiveDateTime {
    instant(local(utc, tz) + Duration::days(days), tz)
}

/// An instant with its zone, written as ISO 8601 with the offset (`Z` for UTC).
pub fn iso(utc: NaiveDateTime) -> String {
    format!("{}Z", utc.format("%Y-%m-%dT%H:%M:%S"))
}

/// An instant from text: ISO 8601 with an offset or `Z` (`2026-11-24T10:00:00+01:00`), or a
/// time without one, read in `tz` (the project's).
pub fn parse_instant(text: &str, tz: Tz) -> Option<NaiveDateTime> {
    let t = text.trim();
    if let Ok(d) = chrono::DateTime::parse_from_rfc3339(t) {
        return Some(d.naive_utc());
    }
    for f in ["%Y-%m-%dT%H:%M%:z", "%Y-%m-%d %H:%M:%S%:z", "%Y-%m-%d %H:%M%:z"] {
        if let Ok(d) = chrono::DateTime::parse_from_str(t, f) {
            return Some(d.naive_utc());
        }
    }
    if let Some(z) = t.strip_suffix('Z').or_else(|| t.strip_suffix('z')) {
        for f in ["%Y-%m-%dT%H:%M", "%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%d %H:%M:%S"] {
            if let Ok(d) = NaiveDateTime::parse_from_str(z, f) {
                return Some(d);
            }
        }
    }
    for f in ["%Y-%m-%dT%H:%M", "%Y-%m-%dT%H:%M:%S", "%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%d %H:%M", "%Y-%m-%d %H:%M:%S"] {
        if let Ok(d) = NaiveDateTime::parse_from_str(t, f) {
            return Some(instant(d, tz));
        }
    }
    None
}

/// A time of day, `9:00` or `09:30:15`.
pub fn time_of_day(text: &str) -> Option<chrono::NaiveTime> {
    let t = text.trim();
    chrono::NaiveTime::parse_from_str(t, "%H:%M:%S")
        .or_else(|_| chrono::NaiveTime::parse_from_str(t, "%H:%M"))
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn at(y: i32, m: u32, d: u32, h: u32, min: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(y, m, d).unwrap().and_hms_opt(h, min, 0).unwrap()
    }

    #[test]
    fn rome_in_winter_and_summer() {
        let rome = zone("Europe/Rome").unwrap();
        assert_eq!(local(at(2026, 1, 10, 8, 0), rome), at(2026, 1, 10, 9, 0));
        assert_eq!(local(at(2026, 7, 10, 7, 0), rome), at(2026, 7, 10, 9, 0));
        assert_eq!(instant(at(2026, 7, 10, 9, 0), rome), at(2026, 7, 10, 7, 0));
        assert_eq!(offset(at(2026, 1, 10, 8, 0), rome), 3600);
        assert_eq!(offset(at(2026, 7, 10, 8, 0), rome), 7200);
        assert!(zone("Mars/Olympus").is_err());
        assert_eq!(zone("").unwrap(), Tz::UTC);
    }

    #[test]
    fn the_skipped_hour_moves_forward_and_the_repeated_one_takes_the_first() {
        let rome = zone("Europe/Rome").unwrap();
        // 29 March 2026: 02:00 → 03:00; 02:30 does not exist, it is 03:30 (01:30 UTC)
        assert_eq!(instant(at(2026, 3, 29, 2, 30), rome), at(2026, 3, 29, 1, 30));
        // 25 October 2026: 03:00 → 02:00; 02:30 happens twice, the first is 00:30 UTC
        assert_eq!(instant(at(2026, 10, 25, 2, 30), rome), at(2026, 10, 25, 0, 30));
    }

    #[test]
    fn a_day_of_the_calendar_and_24_hours_differ_across_the_change() {
        let rome = zone("Europe/Rome").unwrap();
        // 9:00 in Rome on 24 October (07:00 UTC, summer time)
        let nine = at(2026, 10, 24, 7, 0);
        let next = add_days(nine, 1, rome);
        assert_eq!(local(next, rome), at(2026, 10, 25, 9, 0), "still 9:00");
        assert_eq!(next, at(2026, 10, 25, 8, 0), "25 hours later in UTC");
        assert_eq!(local(nine + Duration::hours(24), rome), at(2026, 10, 25, 8, 0), "24 hours: 8:00");
    }

    #[test]
    fn instants_from_text() {
        let rome = zone("Europe/Rome").unwrap();
        let utc9 = at(2026, 11, 24, 9, 0);
        assert_eq!(parse_instant("2026-11-24T10:00:00+01:00", rome), Some(utc9));
        assert_eq!(parse_instant("2026-11-24T09:00:00Z", rome), Some(utc9));
        assert_eq!(parse_instant("2026-11-24T09:00Z", rome), Some(utc9));
        // without an offset: the project's zone
        assert_eq!(parse_instant("2026-11-24T10:00", rome), Some(utc9));
        assert_eq!(parse_instant("2026-11-24 10:00:00", Tz::UTC), Some(at(2026, 11, 24, 10, 0)));
        assert_eq!(iso(utc9), "2026-11-24T09:00:00Z");
        assert_eq!(time_of_day("9:00"), chrono::NaiveTime::from_hms_opt(9, 0, 0));
        assert_eq!(parse_instant("tomorrow", rome), None);
    }
}
