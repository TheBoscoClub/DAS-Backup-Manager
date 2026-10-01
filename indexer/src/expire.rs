//! Expiry of the backups a retired subvolume left behind.
//!
//! Rule (spec §9): every snapshot of a retired series stays on a target until
//! the retirement date plus that target's longest retention window, then all
//! of them are deleted from that target together. btrbk cannot do this:
//! `btrbk prune` skips deletion when the source is not accessible.

use crate::caldate::{date_of, day_number};
use crate::config::Retention;

/// The longest of a target's retention tiers, in whole days: a daily tier
/// counts 1 day each, weekly 7, monthly 31, yearly 366. Rounded up on
/// purpose — the cost of rounding is keeping a backup a little longer.
/// `None` when no tier is set: there is then no window to measure against,
/// and nothing may be expired on that target.
pub fn longest_window_days(r: &Retention) -> Option<u32> {
    let longest = [
        r.daily,
        r.weekly.saturating_mul(7),
        r.monthly.saturating_mul(31),
        r.yearly.saturating_mul(366),
    ]
    .into_iter()
    .max()
    .unwrap_or(0);
    (longest > 0).then_some(longest)
}

/// The first day on which a series retired on `retired` may be deleted.
pub fn expiry_date(retired: &str, window_days: u32) -> Option<String> {
    Some(date_of(day_number(retired)? + i64::from(window_days) + 1))
}

/// Whether a series retired on `retired` is past its window on `today`.
/// `None` if either date cannot be read — and an unreadable date must never
/// be the reason a backup is deleted.
pub fn is_expired(retired: &str, window_days: u32, today: &str) -> Option<bool> {
    Some(day_number(today)? > day_number(retired)? + i64::from(window_days))
}

fn is_btrbk_timestamp(s: &str) -> bool {
    // btrbk: YYYYMMDD, optionally Thhmm or Thhmmss, optionally _N.
    let (stamp, counter) = match s.split_once('_') {
        Some((stamp, n)) => (stamp, Some(n)),
        None => (s, None),
    };
    if counter.is_some_and(|n| n.is_empty() || !n.bytes().all(|b| b.is_ascii_digit())) {
        return false;
    }
    let all_digits = |t: &str| !t.is_empty() && t.bytes().all(|b| b.is_ascii_digit());
    match stamp.split_once('T') {
        Some((date, time)) => {
            date.len() == 8
                && all_digits(date)
                && (time.len() == 4 || time.len() == 6)
                && all_digits(time)
        }
        None => stamp.len() == 8 && all_digits(stamp),
    }
}

/// The entries of a directory that are snapshots of exactly this series:
/// `<snapshot_name>.<btrbk timestamp>`. Sorted. A longer name that merely
/// starts the same (`home-video` for `home`) is a different series.
pub fn series_snapshots(entries: &[String], snapshot_name: &str) -> Vec<String> {
    if snapshot_name.is_empty() {
        return Vec::new();
    }
    let mut found: Vec<String> = entries
        .iter()
        .filter(|entry| {
            entry
                .strip_prefix(snapshot_name)
                .and_then(|rest| rest.strip_prefix('.'))
                .is_some_and(is_btrbk_timestamp)
        })
        .cloned()
        .collect();
    found.sort();
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Retention;

    fn r(daily: u32, weekly: u32, monthly: u32, yearly: u32) -> Retention {
        Retention {
            daily,
            weekly,
            monthly,
            yearly,
        }
    }

    #[test]
    fn longest_window_is_the_longest_tier_in_whole_days() {
        assert_eq!(longest_window_days(&r(7, 0, 0, 0)), Some(7));
        assert_eq!(longest_window_days(&r(7, 4, 0, 0)), Some(28));
        assert_eq!(longest_window_days(&r(7, 4, 12, 0)), Some(372));
        assert_eq!(longest_window_days(&r(7, 4, 12, 1)), Some(372));
        assert_eq!(longest_window_days(&r(0, 0, 0, 1)), Some(366));
        assert_eq!(longest_window_days(&r(30, 1, 0, 0)), Some(30));
        // No retention configured is not "expire at once": there is no window.
        assert_eq!(longest_window_days(&r(0, 0, 0, 0)), None);
    }

    #[test]
    fn a_series_expires_only_after_the_whole_window_has_passed() {
        // Retired on the 1st with a 7-day window: kept through the 8th.
        assert_eq!(is_expired("2026-10-01", 7, "2026-10-01"), Some(false));
        assert_eq!(is_expired("2026-10-01", 7, "2026-10-07"), Some(false));
        assert_eq!(is_expired("2026-10-01", 7, "2026-10-08"), Some(false));
        assert_eq!(is_expired("2026-10-01", 7, "2026-10-09"), Some(true));
        // A clock set back before the retirement date expires nothing.
        assert_eq!(is_expired("2026-10-01", 7, "2026-09-01"), Some(false));
        assert_eq!(expiry_date("2026-10-01", 7).as_deref(), Some("2026-10-09"));
    }

    #[test]
    fn an_unreadable_date_never_expires_anything() {
        assert_eq!(is_expired("not-a-date", 7, "2026-10-09"), None);
        assert_eq!(is_expired("2026-10-01", 7, "garbage"), None);
        assert_eq!(expiry_date("", 7), None);
    }

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn series_snapshots_does_not_match_a_longer_name() {
        let dir = names(&[
            "home.20261001T1421",
            "home.20260930T0323",
            "home-video.20261001T1421",
            "home.20261001T1421_1",
            "home.20261001",
            "home.20261001T142100",
            "home",
            "home.tmp",
            "home.2026",
            "home.20261001T1421.partial",
            "xhome.20261001T1421",
        ]);
        assert_eq!(
            series_snapshots(&dir, "home"),
            [
                "home.20260930T0323",
                "home.20261001",
                "home.20261001T1421",
                "home.20261001T142100",
                "home.20261001T1421_1",
            ]
        );
        assert_eq!(
            series_snapshots(&dir, "home-video"),
            ["home-video.20261001T1421"]
        );
        assert!(series_snapshots(&dir, "").is_empty());
    }

    #[test]
    fn series_snapshots_treats_the_name_literally() {
        // '.' and '*' in a snapshot name are characters, not patterns.
        let dir = names(&["a.b.20261001T1421", "aXb.20261001T1421", "a*.20261001T1421"]);
        assert_eq!(series_snapshots(&dir, "a.b"), ["a.b.20261001T1421"]);
        assert_eq!(series_snapshots(&dir, "a*"), ["a*.20261001T1421"]);
    }

    #[test]
    fn each_tier_is_converted_on_its_own() {
        assert_eq!(longest_window_days(&r(0, 1, 0, 0)), Some(7));
        assert_eq!(longest_window_days(&r(0, 0, 1, 0)), Some(31));
        assert_eq!(longest_window_days(&r(0, 0, 0, 2)), Some(732));
        assert_eq!(longest_window_days(&r(400, 0, 0, 1)), Some(400));
        // Overflow saturates upward: a longer window only keeps a backup longer.
        assert_eq!(longest_window_days(&r(0, u32::MAX, 0, 0)), Some(u32::MAX));
    }

    #[test]
    fn windows_cross_month_and_year_boundaries_and_may_be_zero() {
        assert_eq!(expiry_date("2026-12-31", 0).as_deref(), Some("2027-01-01"));
        assert_eq!(expiry_date("2026-02-20", 10).as_deref(), Some("2026-03-03"));
        assert_eq!(is_expired("2026-12-31", 0, "2026-12-31"), Some(false));
        assert_eq!(is_expired("2026-12-31", 0, "2027-01-01"), Some(true));
        assert_eq!(is_expired("2026-02-20", 10, "2026-03-02"), Some(false));
        assert_eq!(is_expired("2026-02-20", 10, "2026-03-03"), Some(true));
        // The day expiry_date names is the first on which is_expired holds.
        assert_eq!(
            is_expired("2026-10-01", 372, &expiry_date("2026-10-01", 372).unwrap()),
            Some(true)
        );
    }

    fn matches(entry: &str) -> bool {
        !series_snapshots(&names(&[entry]), "s").is_empty()
    }

    #[test]
    fn timestamp_shapes_are_matched_exactly() {
        for ok in [
            "s.20261001",
            "s.20261001T1421",
            "s.20261001T142100",
            "s.20261001_1",
            "s.20261001T1421_12",
            "s.20261001T142100_3",
        ] {
            assert!(matches(ok), "{ok} should match");
        }
        for bad in [
            "s.2026100",            // date one digit short
            "s.202610011",          // date one digit long
            "s.2026100a",           // non-digit in date
            "s.20261001T",          // empty time
            "s.20261001T142",       // time 3 digits
            "s.20261001T14210",     // time 5 digits
            "s.20261001T1421000",   // time 7 digits
            "s.20261001T14a1",      // non-digit in time
            "s.20261001Tabcd",      // time all letters
            "s.20261001_",          // empty counter
            "s.20261001_a",         // non-digit counter
            "s.20261001_1_2",       // second underscore
            "s.20261001T1421T1421", // second T
            "s.T1421",              // no date
            "s.",                   // nothing after the dot
            "s.20261001 ",          // trailing space
        ] {
            assert!(!matches(bad), "{bad} should not match");
        }
    }

    #[test]
    fn a_timezone_suffix_is_not_matched_so_such_a_series_is_kept() {
        // btrbk's long-iso format can append an offset. Not matching means
        // "keep", the cautious direction; this pins that behaviour.
        assert!(!matches("s.20261001T142100+0200"));
        assert!(!matches("s.20261001T142100-0500"));
        assert!(!matches("s.20261001T1421+0200"));
    }

    #[test]
    fn series_snapshots_returns_sorted_and_only_the_matching_entries() {
        let dir = names(&["s.20261002", "s.20261001", "t.20261001", "s.20261001T1421"]);
        assert_eq!(
            series_snapshots(&dir, "s"),
            ["s.20261001", "s.20261001T1421", "s.20261002"]
        );
        assert!(series_snapshots(&[], "s").is_empty());
    }
}
