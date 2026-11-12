//! UTC timestamps in ISO 8601, without a date library.
//!
//! The standard library gives the time as seconds since 1970-01-01 (the Unix epoch). Turning a day
//! count into a calendar date is done with Howard Hinnant's `civil_from_days` algorithm: it shifts
//! the year to start on 1 March (so the leap day is the last day of the year), splits time into
//! 400-year eras of exactly 146,097 days, and works out the year, month and day with integer
//! arithmetic only. It is exact for every date, with no loops and no tables.

use std::time::{SystemTime, UNIX_EPOCH};

/// The calendar date (year, month 1-12, day 1-31) of a day number counted from 1970-01-01.
pub fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468; // days since 0000-03-01, the start of the shifted calendar
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097; // 400-year era (floor division)
    let doe = z - era * 146_097; // day of the era, 0..=146096
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365; // year of the era, 0..=399
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // day of the (March-based) year, 0..=365
    let mp = (5 * doy + 2) / 153; // month, March = 0
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + i64::from(month <= 2); // January and February belong to the next year
    (year, month, day)
}

/// A point in time (milliseconds since the Unix epoch) as `YYYY-MM-DDTHH:MM:SS.mmm+00:00`.
pub fn iso_from_millis(millis: i64) -> String {
    let seconds = millis.div_euclid(1_000);
    let (year, month, day) = civil_from_days(seconds.div_euclid(86_400));
    let in_day = seconds.rem_euclid(86_400);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}+00:00",
        in_day / 3_600,
        in_day % 3_600 / 60,
        in_day % 60,
        millis.rem_euclid(1_000)
    )
}

/// Milliseconds since the Unix epoch, now.
pub fn now_millis() -> i64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(elapsed) => elapsed.as_millis() as i64,
        Err(_) => 0, // a clock set before 1970: not worth stopping a machine for
    }
}

/// The current UTC time in ISO 8601, for message envelopes and log lines.
pub fn now_iso() -> String {
    iso_from_millis(now_millis())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
        assert_eq!(civil_from_days(11_016), (2000, 2, 29)); // a leap day in a year divisible by 400
        assert_eq!(civil_from_days(20_605), (2026, 6, 1));
    }

    #[test]
    fn every_day_follows_the_previous_one() {
        // Walk 1600-01-01 to 2400-12-31 day by day: each date is the day after the previous one.
        let (mut y, mut m, mut d) = civil_from_days(-135_140);
        assert_eq!((y, m, d), (1600, 1, 1));
        for days in -135_139..=157_419 {
            let next = civil_from_days(days);
            let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
            let month_length = [
                31,
                if leap { 29 } else { 28 },
                31,
                30,
                31,
                30,
                31,
                31,
                30,
                31,
                30,
                31,
            ][m as usize - 1];
            let expected = if d < month_length {
                (y, m, d + 1)
            } else if m < 12 {
                (y, m + 1, 1)
            } else {
                (y + 1, 1, 1)
            };
            assert_eq!(next, expected, "day {days}");
            (y, m, d) = next;
        }
        assert_eq!((y, m, d), (2400, 12, 31));
    }

    #[test]
    fn iso_format() {
        assert_eq!(iso_from_millis(0), "1970-01-01T00:00:00.000+00:00");
        assert_eq!(
            iso_from_millis(1_780_301_700_123),
            "2026-06-01T08:15:00.123+00:00"
        );
    }
}
