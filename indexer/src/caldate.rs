//! Whole-day calendar arithmetic on `YYYY-MM-DD` strings, UTC.
//!
//! Retirement and expiry only ever ask "how many days apart are these two
//! dates", so this is the proleptic Gregorian day count and nothing more.

use std::time::{SystemTime, UNIX_EPOCH};

fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ => {
            if (year % 4 == 0 && year % 100 != 0) || year % 400 == 0 {
                29
            } else {
                28
            }
        }
    }
}

/// Days since 1970-01-01 for a `YYYY-MM-DD` date, or `None` if the text is
/// not exactly that shape or is not a real calendar date.
pub fn day_number(date: &str) -> Option<i64> {
    let bytes = date.as_bytes();
    if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return None;
    }
    // Defensive: after the length check above, bytes 4 and 7 are ASCII `-`, so
    // these ranges always fall on character boundaries. `get` rather than
    // indexing keeps a future change to that check from turning into a panic.
    let digits = |s: Option<&str>| -> Option<i64> {
        let s = s?;
        s.bytes()
            .all(|b| b.is_ascii_digit())
            .then(|| s.parse().ok())?
    };
    let (year, month, day) = (
        digits(date.get(0..4))?,
        digits(date.get(5..7))?,
        digits(date.get(8..10))?,
    );
    if !(1..=12).contains(&month) || day < 1 || day > days_in_month(year, month) {
        return None;
    }
    // Days-from-civil (H. Hinnant): count in 400-year eras with the year
    // starting in March, so the leap day is the last day of the year.
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let year_of_era = y - era * 400;
    let month_from_march = if month > 2 { month - 3 } else { month + 9 };
    let day_of_year = (153 * month_from_march + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    Some(era * 146_097 + day_of_era - 719_468)
}

/// The `YYYY-MM-DD` date `day` days after 1970-01-01.
pub fn date_of(day: i64) -> String {
    let z = day + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_from_march = (5 * day_of_year + 2) / 153;
    let d = day_of_year - (153 * month_from_march + 2) / 5 + 1;
    let m = if month_from_march < 10 {
        month_from_march + 3
    } else {
        month_from_march - 9
    };
    let y = year_of_era + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Today's date in UTC. Callers that act on it check `untrusted_clock`.
pub fn today() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        // A clock before 1970 is a broken clock. Day 0 is 1970-01-01, which
        // `untrusted_clock` refuses, so nothing is retired or expired by it.
        .unwrap_or(0);
    date_of(secs.div_euclid(86_400))
}

/// The earliest date this code accepts from the system clock. Nothing was
/// retired before this feature existed, so an earlier reading is a clock
/// that was never set, or was reset — a dead RTC battery reads 1970.
pub const EARLIEST_TRUSTED_DATE: &str = "2026-01-01";

/// Why `today` cannot be trusted to date a retirement or an expiry, or
/// `None` if it can. A wrong clock must cost a late deletion, never an early
/// one: sync refuses to stamp a retirement with it and expiry refuses to
/// delete by it. An unreadable `today` is not reported here — no comparison
/// against it ever finds a series expired.
pub fn untrusted_clock(today: &str) -> Option<String> {
    let floor = day_number(EARLIEST_TRUSTED_DATE)?;
    (day_number(today)? < floor).then(|| {
        format!(
            "the system clock reads {today}, before {EARLIEST_TRUSTED_DATE} — it cannot be trusted to date a retirement or an expiry"
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn day_number_counts_from_the_unix_epoch() {
        assert_eq!(day_number("1970-01-01"), Some(0));
        assert_eq!(day_number("1970-01-02"), Some(1));
        assert_eq!(day_number("1969-12-31"), Some(-1));
        // 2026-10-01 is 20727 days after the epoch (`date -ud 2026-10-01 +%s` / 86400).
        assert_eq!(day_number("2026-10-01"), Some(20727));
    }

    #[test]
    fn day_number_knows_leap_years() {
        assert_eq!(
            day_number("2024-03-01").unwrap() - day_number("2024-02-28").unwrap(),
            2
        );
        assert_eq!(
            day_number("2026-03-01").unwrap() - day_number("2026-02-28").unwrap(),
            1
        );
        // Century rule: 2100 is not a leap year, 2000 was.
        assert_eq!(day_number("2100-02-29"), None);
        assert!(day_number("2000-02-29").is_some());
    }

    #[test]
    fn day_number_rejects_anything_that_is_not_a_real_date() {
        for bad in [
            "",
            "2026-10",
            "2026-13-01",
            "2026-00-10",
            "2026-04-31",
            "2026-10-00",
            "26-10-01",
            "2026/10/01",
            "2026-10-01T00:00",
            " 2026-10-01",
            "2026-1-1",
            "abcd-ef-gh",
        ] {
            assert_eq!(day_number(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn date_of_is_the_inverse_of_day_number() {
        for date in [
            "1970-01-01",
            "1999-12-31",
            "2000-02-29",
            "2026-10-01",
            "2027-01-01",
        ] {
            assert_eq!(date_of(day_number(date).unwrap()), date);
        }
        assert_eq!(date_of(20727 + 7), "2026-10-08");
    }

    #[test]
    fn today_is_a_valid_date_not_before_this_code_was_written() {
        let today = today();
        assert!(
            day_number(&today).unwrap() >= day_number("2026-10-01").unwrap(),
            "{today}"
        );
    }

    #[test]
    fn day_number_matches_known_calendar_days() {
        for (date, day) in [
            ("0000-03-01", -719_468),
            ("0001-01-01", -719_162),
            ("1900-01-01", -25_567),
            ("1900-03-01", -25_508),
            ("1970-03-01", 59),
            ("1972-03-01", 790),
            ("2000-01-01", 10_957),
            ("2000-02-29", 11_016),
            ("2000-03-01", 11_017),
            ("2024-12-31", 20_088),
            ("2100-03-01", 47_541),
            ("9999-12-31", 2_932_896),
        ] {
            assert_eq!(day_number(date), Some(day), "{date}");
            assert_eq!(date_of(day), date, "{day}");
        }
    }

    #[test]
    fn month_lengths_follow_the_calendar() {
        let lengths = |year: i64| {
            [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
                .iter()
                .enumerate()
                .map(|(i, &n)| (i as i64 + 1, if i == 1 && is_leap(year) { 29 } else { n }))
                .collect::<Vec<_>>()
        };
        fn is_leap(y: i64) -> bool {
            matches!(y, 1904 | 1972 | 2000 | 2024 | 2400)
        }
        for year in [1900, 1903, 1904, 1971, 1972, 2000, 2024, 2026, 2100, 2400] {
            for (month, days) in lengths(year) {
                let first = |m: i64, d: i64| day_number(&format!("{year:04}-{m:02}-{d:02}"));
                assert!(first(month, days).is_some(), "{year}-{month}-{days}");
                assert_eq!(first(month, days + 1), None, "{year}-{month}-{}", days + 1);
                assert_eq!(first(month, 0), None);
                if month < 12 {
                    assert_eq!(
                        first(month + 1, 1).unwrap() - first(month, 1).unwrap(),
                        days
                    );
                }
            }
        }
    }

    #[test]
    fn every_day_round_trips_across_many_eras() {
        // Continuous, so every month, year, century and 400-year boundary
        // between year 0 and year 9999 is crossed, forwards and backwards.
        let mut previous = date_of(-719_468);
        for day in -719_467..=2_932_896 {
            let date = date_of(day);
            assert_eq!(day_number(&date), Some(day), "{date}");
            assert!(date > previous, "{previous} then {date}");
            previous = date;
        }
    }

    #[test]
    fn day_number_rejects_signs_spaces_and_multibyte_text() {
        for bad in [
            "+026-10-01",
            "-026-10-01",
            "2026-+1-01",
            "2026--1-01",
            "2026-10-+1",
            "2026-10- 1",
            "2026-10-1 ",
            "2026-10/01",
            "2026/10-01",
            "2026-10x01",
            "20é6-10-01",
            "2026-1é-01",
            "é026-10-01",
            "2026-10-0é",
            "２０２６-10-01",
        ] {
            assert_eq!(day_number(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn a_clock_before_2026_is_not_trusted_and_names_the_date() {
        for early in ["1970-01-01", "2025-12-31"] {
            let why = untrusted_clock(early).expect(early);
            assert!(why.contains(early) && why.contains("2026-01-01"), "{why}");
        }
        assert_eq!(untrusted_clock("2026-01-01"), None);
        assert_eq!(untrusted_clock("2026-10-02"), None);
        // An unreadable date is refused where it is used (it never compares
        // as expired), so it is not this check's to report.
        assert_eq!(untrusted_clock("garbage"), None);
    }
}
