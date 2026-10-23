// SPDX-License-Identifier: AGPL-3.0-only

//! UTC days and the timestamp convention.
//!
//! Every timestamp in the store is an `i64` count of milliseconds since the
//! Unix epoch, UTC. A [`Day`] is a UTC calendar day, written `YYYY-MM-DD`.

use std::fmt;
use std::str::FromStr;

/// Milliseconds in a UTC day (UTC has no leap seconds in Unix time).
pub const MS_PER_DAY: i64 = 86_400_000;

/// A UTC calendar day, counted from 1970-01-01.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Day(i64);

impl Day {
    /// The day that contains the instant `ms` (unix milliseconds, UTC).
    #[must_use]
    pub const fn containing(ms: i64) -> Day {
        Day(ms.div_euclid(MS_PER_DAY))
    }

    /// The day `days` days after 1970-01-01.
    #[must_use]
    pub const fn from_epoch_days(days: i64) -> Day {
        Day(days)
    }

    /// Days since 1970-01-01.
    #[must_use]
    pub const fn epoch_days(self) -> i64 {
        self.0
    }

    /// The first millisecond of the day.
    #[must_use]
    pub const fn start_ms(self) -> i64 {
        self.0 * MS_PER_DAY
    }

    /// This day moved by `days` (negative moves back).
    #[must_use]
    pub const fn plus(self, days: i64) -> Day {
        Day(self.0 + days)
    }
}

impl fmt::Display for Day {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (year, month, day) = civil_from_days(self.0);
        write!(f, "{year:04}-{month:02}-{day:02}")
    }
}

/// The text was not a real calendar date written `YYYY-MM-DD`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidDay(pub String);

impl fmt::Display for InvalidDay {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?} is not a date written YYYY-MM-DD", self.0)
    }
}

impl std::error::Error for InvalidDay {}

impl FromStr for Day {
    type Err = InvalidDay;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let invalid = || InvalidDay(text.to_owned());
        let bytes = text.as_bytes();
        let shape_ok = bytes.len() == 10
            && bytes[4] == b'-'
            && bytes[7] == b'-'
            && bytes
                .iter()
                .enumerate()
                .all(|(i, b)| i == 4 || i == 7 || b.is_ascii_digit());
        if !shape_ok {
            return Err(invalid());
        }
        let field =
            |range: std::ops::Range<usize>| text[range].parse::<u32>().map_err(|_| invalid());
        let (year, month, day) = (field(0..4)?, field(5..7)?, field(8..10)?);
        if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
            return Err(invalid());
        }
        let parsed = Day(days_from_civil(i64::from(year), month, day));
        // Day 31 of a 30-day month rolls into the next month; reject it.
        if civil_from_days(parsed.0) == (i64::from(year), month, day) {
            Ok(parsed)
        } else {
            Err(invalid())
        }
    }
}

/// `ms` as an RFC 3339 UTC timestamp with millisecond precision.
#[must_use]
pub fn rfc3339(ms: i64) -> String {
    let day = Day::containing(ms);
    let of_day = ms - day.start_ms();
    let (hours, rest) = (of_day / 3_600_000, of_day % 3_600_000);
    let (minutes, rest) = (rest / 60_000, rest % 60_000);
    let (seconds, millis) = (rest / 1000, rest % 1000);
    format!("{day}T{hours:02}:{minutes:02}:{seconds:02}.{millis:03}Z")
}

/// Days from 1970-01-01 to a proleptic Gregorian date (Howard Hinnant's
/// `days_from_civil`).
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let shifted_month = i64::from((month + 9) % 12);
    let day_of_year = (153 * shifted_month + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// The inverse of [`days_from_civil`].
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let days = days + 719_468;
    let era = days.div_euclid(146_097);
    let day_of_era = days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    // Both are bounded (1..=31, 1..=12) by construction.
    (
        year,
        u32::try_from(month).unwrap_or_default(),
        u32::try_from(day).unwrap_or_default(),
    )
}

#[cfg(test)]
mod tests {
    use super::{Day, MS_PER_DAY, rfc3339};

    #[test]
    fn epoch_and_known_dates() {
        assert_eq!("1970-01-01".parse::<Day>().unwrap().epoch_days(), 0);
        assert_eq!("2000-03-01".parse::<Day>().unwrap().epoch_days(), 11_017);
        assert_eq!("2026-10-23".parse::<Day>().unwrap().epoch_days(), 20_749);
        assert_eq!(Day::from_epoch_days(20_749).to_string(), "2026-10-23");
    }

    #[test]
    fn every_day_round_trips() {
        // Every day from 1900 to 2200, then a sweep of 0001-01-01 to 9999-12-31.
        let dense = -25_567..=84_000;
        let sparse = (-719_162..=2_932_896).step_by(13);
        for days in dense.chain(sparse) {
            let day = Day::from_epoch_days(days);
            assert_eq!(day.to_string().parse::<Day>(), Ok(day), "{day}");
        }
    }

    #[test]
    fn rejects_impossible_and_malformed_dates() {
        for text in [
            "2026-02-29",
            "2026-04-31",
            "2026-13-01",
            "2026-00-10",
            "2026-1-01",
            "26-01-01",
            "2026/01/01",
            "2026-01-01T00:00:00Z",
            "",
        ] {
            assert!(text.parse::<Day>().is_err(), "{text} parsed");
        }
        assert!("2024-02-29".parse::<Day>().is_ok());
    }

    #[test]
    fn day_boundaries() {
        assert_eq!(Day::containing(MS_PER_DAY - 1), Day::from_epoch_days(0));
        assert_eq!(Day::containing(MS_PER_DAY), Day::from_epoch_days(1));
        assert_eq!(Day::containing(-1), Day::from_epoch_days(-1));
    }

    #[test]
    fn formats_rfc3339() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(rfc3339(1_792_713_600_123), "2026-10-23T00:00:00.123Z");
        assert_eq!(rfc3339(MS_PER_DAY - 1), "1970-01-01T23:59:59.999Z");
    }
}
