//! The steps `backup-run.sh` does around btrbk that the CLI/GUI run
//! (`backup::run_backup_with`) now does too (bd DAS-Backup-Manager-7qe): the
//! USB link check, the `throughput.jsonl` and `growth.log` lines, and the
//! expiry of retired subvolumes. Everything here is a pure function over a
//! path, a number or a string, so a test needs no host: the run reaches the
//! machine only through `backup::StepEnv`.

use crate::recovery_os::RecordStatus;
use std::path::{Path, PathBuf};

/// Where the kernel lists USB devices.
pub const USB_DEVICES_DIR: &str = "/sys/bus/usb/devices";

/// What a USB-link failure is prefixed with in a run's `errors`.
pub const USB_LINK_ERROR_PREFIX: &str = "USB link: ";

/// What a retired-expiry failure is prefixed with in a run's `errors`.
pub const EXPIRY_ERROR_PREFIX: &str = "Retired expiry: ";

/// What the link check read from one reading of the enclosure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsbLinkCheck {
    pub status: RecordStatus,
    /// The report row's detail, worded as `log_throughput` words it.
    pub detail: String,
    /// The slowest negotiated speed read, in Mbit/s; `None` when none was.
    pub speed: Option<u64>,
}

/// The USB link check of a live run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UsbLinkStep {
    /// A dry run, or no `[das].model_pattern`: no device is singled out, so
    /// the link is unknown and nothing is claimed about it.
    NotChecked,
    Checked(UsbLinkCheck),
}

impl UsbLinkStep {
    /// A checked link with a status and detail, for a test of a report row.
    #[cfg(test)]
    pub(crate) fn checked_for_test(status: RecordStatus, detail: &str) -> Self {
        Self::Checked(UsbLinkCheck {
            status,
            detail: detail.into(),
            speed: None,
        })
    }

    /// Whether the link is below what the enclosure should negotiate.
    pub fn failed(&self) -> bool {
        matches!(self, Self::Checked(c) if c.status == RecordStatus::Fail)
    }

    /// Whether the link went unread.
    pub fn has_warnings(&self) -> bool {
        matches!(self, Self::Checked(c) if c.status == RecordStatus::Warn)
    }

    /// The report's `USB link` cell.
    pub fn row(&self) -> String {
        match self {
            Self::NotChecked => "N/A  (not checked)".into(),
            Self::Checked(c) => format!("{}  ({})", c.status.word(), c.detail),
        }
    }

    /// The `usb_link_mbit_s` of `throughput.jsonl`: the speed, or `unknown`.
    pub fn speed_text(&self) -> String {
        match self {
            Self::Checked(UsbLinkCheck { speed: Some(s), .. }) => s.to_string(),
            _ => "unknown".into(),
        }
    }
}

/// Read every USB device under `root` whose product contains `pattern` and
/// judge the slowest against `expected` Mbit/s.
pub fn check_usb_link(root: &Path, pattern: &str, expected: u32) -> UsbLinkStep {
    if pattern.is_empty() {
        return UsbLinkStep::NotChecked;
    }
    let unread = |detail: String| {
        UsbLinkStep::Checked(UsbLinkCheck {
            status: RecordStatus::Warn,
            detail,
            speed: None,
        })
    };
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(e) => return unread(format!("cannot read {}: {e}", root.display())),
    };
    // Each device is read on its own: one that lacks a file, or whose speed
    // is not a number (`unknown`), is not the enclosure and is passed over.
    let slowest = entries
        .flatten()
        .filter_map(|entry| {
            let dir = entry.path();
            let product = std::fs::read_to_string(dir.join("product")).ok()?;
            if !product.contains(pattern) {
                return None;
            }
            let speed = std::fs::read_to_string(dir.join("speed")).ok()?;
            let speed = speed.trim();
            if speed.is_empty() || !speed.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            speed.parse::<u64>().ok()
        })
        .min();
    let Some(speed) = slowest else {
        return unread(format!("no USB device's product contains '{pattern}'"));
    };
    let (status, detail) = if speed < u64::from(expected) {
        (
            RecordStatus::Fail,
            format!("negotiated {speed} Mbit/s, expected {expected}"),
        )
    } else {
        (RecordStatus::Ok, format!("negotiated {speed} Mbit/s"))
    };
    UsbLinkStep::Checked(UsbLinkCheck {
        status,
        detail,
        speed: Some(speed),
    })
}

/// `throughput.jsonl` sits beside the growth log.
pub fn throughput_log_path(growth_log: &str) -> PathBuf {
    Path::new(growth_log).with_file_name("throughput.jsonl")
}

/// One `throughput.jsonl` line, `None` when the run wrote nothing or took no
/// whole second (no rate to give).
pub fn throughput_line(ts: &str, elapsed_s: u64, bytes: u64, link: &str) -> Option<String> {
    if elapsed_s == 0 || bytes == 0 {
        return None;
    }
    Some(format!(
        "{{\"ts\":\"{ts}\",\"elapsed_s\":{elapsed_s},\"bytes\":{bytes},\"bytes_per_s\":{},\"usb_link_mbit_s\":\"{link}\"}}",
        bytes / elapsed_s
    ))
}

/// `growth.log` lines: `<ts> <mount> <used bytes>` for each target with a
/// reading; a target with none gets no line, never a 0.
pub fn growth_lines(ts: &str, readings: &[(String, Option<u64>)]) -> Vec<String> {
    readings
        .iter()
        .filter_map(|(mount, used)| used.map(|u| format!("{ts} {mount} {u}")))
        .collect()
}

/// `date -Is`: local time with its UTC offset.
pub fn iso_local(secs: i64) -> String {
    let tm = local_tm(secs);
    let off = tm.tm_gmtoff;
    let sign = if off < 0 { '-' } else { '+' };
    format!(
        "{}{sign}{:02}:{:02}",
        growth_stamp(secs),
        off.abs() / 3600,
        off.abs() % 3600 / 60
    )
}

fn local_tm(secs: i64) -> libc::tm {
    let t = secs as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: `t` is a valid time_t and `tm` a properly allocated libc::tm.
    unsafe { libc::localtime_r(&t, &mut tm) };
    tm
}

/// `date '+%Y-%m-%dT%H:%M:%S'`: local time, as `growth.log` lines carry it.
pub fn growth_stamp(secs: i64) -> String {
    let tm = local_tm(secs);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec
    )
}

/// What the expiry seam answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExpireRun {
    /// The command ran: the `RETIRED SUBVOLUMES` section it printed (empty
    /// when nothing is retired) and, when it failed, why.
    Performed {
        section: String,
        failure: Option<String>,
    },
    /// It could not be asked at all.
    Unavailable(String),
}

/// [`crate::expire::expire_retired`] for a run: the section it prints, and
/// when it failed why — the expiry itself, a config that could not be read or
/// saved, or deleted snapshots the index at `db` still lists.
pub fn expire_at(
    config_path: &Path,
    db: &Path,
    dry_run: bool,
    today: &str,
    runner: &dyn crate::fsutil::CommandRunner,
    is_mountpoint: &dyn Fn(&Path) -> bool,
) -> ExpireRun {
    let outcome =
        match crate::expire::expire_retired(config_path, db, dry_run, today, runner, is_mountpoint)
        {
            Ok(outcome) => outcome,
            Err(e) => {
                return ExpireRun::Performed {
                    section: String::new(),
                    failure: Some(e),
                };
            }
        };
    let mut section = crate::expire::format_expire_report(&outcome, dry_run);
    let mut failure = outcome.failed().then(|| "exit code 1".to_string());
    // Snapshots are gone and the index still lists them: the run's history
    // would show backups that no longer exist.
    let deleted = outcome.deleted_paths();
    if !deleted.is_empty() {
        match crate::expire::prune_deleted_from_index(
            db,
            &deleted,
            &outcome.indexed_deleted_paths(),
        ) {
            Ok(Some(shortfall)) => section.push_str(&shortfall),
            Ok(None) => {}
            Err(e) => {
                failure = Some(format!(
                    "deleted snapshots could not be removed from the index: {e}"
                ));
            }
        }
    }
    ExpireRun::Performed { section, failure }
}

/// Why a step that ran is `Skip`, `Ok` or `Fail`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExpiryStatus {
    Ok,
    Skip,
    Fail,
}

impl ExpiryStatus {
    pub fn word(self) -> &'static str {
        match self {
            Self::Ok => "OK",
            Self::Skip => "SKIP",
            Self::Fail => "FAIL",
        }
    }
}

/// The retired-subvolume expiry of a run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExpiryStep {
    /// Not reached: the run was cut short before it.
    NotRun,
    Ran {
        status: ExpiryStatus,
        detail: String,
        /// The `RETIRED SUBVOLUMES` section, empty when nothing is retired.
        section: String,
    },
}

/// Why a run of a selection does not expire.
pub const NARROW_SELECTION: &str = "not performed: a run of a selection (the nightly run or `btrdasd subvol expire` expires retired subvolumes)";

/// Why a run whose sync failed only shows what it would expire.
pub const SYNC_FAILED: &str = "not performed: subvolume sync failed this run; dry run shown";

impl ExpiryStep {
    pub fn failed(&self) -> bool {
        matches!(
            self,
            Self::Ran {
                status: ExpiryStatus::Fail,
                ..
            }
        )
    }

    /// The report's `Retired expiry` cell.
    pub fn row(&self) -> String {
        match self {
            Self::NotRun => "N/A  (n/a)".into(),
            Self::Ran { status, detail, .. } => format!("{}  ({detail})", status.word()),
        }
    }

    /// The step of a run that is not everything it was given.
    pub fn skipped_for_selection() -> Self {
        Self::Ran {
            status: ExpiryStatus::Skip,
            detail: NARROW_SELECTION.into(),
            section: String::new(),
        }
    }

    /// The step after the command answered `run`; `held_back` when only a dry
    /// run was allowed because the sync failed.
    pub fn from_run(run: ExpireRun, held_back: bool) -> Self {
        let (mut section, failure) = match run {
            ExpireRun::Unavailable(why) => {
                return Self::Ran {
                    status: ExpiryStatus::Skip,
                    detail: format!("not performed: {why}"),
                    section: String::new(),
                };
            }
            ExpireRun::Performed { section, failure } => (section, failure),
        };
        if held_back && !section.is_empty() {
            // After the section's header line, before the dry-run lines.
            let (header, rest) = section.split_once('\n').unwrap_or((&section, ""));
            section = format!(
                "{header}\n  EXPIRY NOT PERFORMED: subvolume sync failed this run; nothing was deleted (dry run shown)\n{rest}"
            );
        }
        let (status, detail) = match failure {
            Some(why) => (
                ExpiryStatus::Fail,
                format!("{why} — see RETIRED SUBVOLUMES in the report"),
            ),
            None if held_back => (ExpiryStatus::Skip, SYNC_FAILED.to_string()),
            None => (ExpiryStatus::Ok, "n/a".to_string()),
        };
        Self::Ran {
            status,
            detail,
            section,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A sysfs-shaped directory: one directory per device, with the files the
    /// kernel gives it (`None` leaves a file out).
    fn sysfs(devices: &[(&str, Option<&str>, Option<&str>)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (name, product, speed) in devices {
            let d = dir.path().join(name);
            std::fs::create_dir(&d).unwrap();
            if let Some(p) = product {
                std::fs::write(d.join("product"), format!("{p}\n")).unwrap();
            }
            if let Some(s) = speed {
                std::fs::write(d.join("speed"), format!("{s}\n")).unwrap();
            }
        }
        dir
    }

    fn checked(step: UsbLinkStep) -> UsbLinkCheck {
        match step {
            UsbLinkStep::Checked(c) => c,
            UsbLinkStep::NotChecked => panic!("expected a checked link"),
        }
    }

    #[test]
    fn the_slowest_matching_device_is_the_link_and_below_expected_is_a_fail() {
        let dir = sysfs(&[
            ("1-1", Some("Acme DAS-X"), Some("10000")),
            ("1-2", Some("Acme DAS-X"), Some("5000")),
            ("2-1", Some("Keyboard"), Some("12")),
        ]);
        let step = check_usb_link(dir.path(), "DAS-X", 10000);
        assert!(step.failed());
        let c = checked(step);
        assert_eq!(c.status, RecordStatus::Fail);
        assert_eq!(c.speed, Some(5000));
        assert_eq!(c.detail, "negotiated 5000 Mbit/s, expected 10000");
    }

    #[test]
    fn a_link_at_or_above_expected_is_ok_and_a_foreign_device_is_ignored() {
        let dir = sysfs(&[
            ("1-1", Some("Acme DAS-X"), Some("10000")),
            ("2-1", Some("Keyboard"), Some("12")),
        ]);
        let step = check_usb_link(dir.path(), "DAS-X", 10000);
        assert!(!step.failed());
        let c = checked(step);
        assert_eq!(c.status, RecordStatus::Ok);
        assert_eq!(c.detail, "negotiated 10000 Mbit/s");
    }

    #[test]
    fn a_pattern_no_device_matches_is_a_warn_naming_it() {
        let dir = sysfs(&[("2-1", Some("Keyboard"), Some("12"))]);
        let step = check_usb_link(dir.path(), "DAS-X", 10000);
        assert!(step.has_warnings());
        let c = checked(step);
        assert_eq!(c.status, RecordStatus::Warn);
        assert_eq!(c.detail, "no USB device's product contains 'DAS-X'");
        assert_eq!(c.speed, None);
    }

    #[test]
    fn a_device_missing_a_file_or_with_a_non_numeric_speed_is_not_read() {
        let dir = sysfs(&[
            ("1-1", Some("Acme DAS-X"), None),
            ("1-2", None, Some("5000")),
            ("1-3", Some("Acme DAS-X"), Some("fast")),
            ("1-4", Some("Acme DAS-X"), Some("10000")),
        ]);
        let c = checked(check_usb_link(dir.path(), "DAS-X", 10000));
        assert_eq!(c.status, RecordStatus::Ok);
        assert_eq!(c.speed, Some(10000));
    }

    #[test]
    fn no_pattern_checks_nothing_and_an_unreadable_root_is_a_warn() {
        let dir = sysfs(&[("1-1", Some("Acme DAS-X"), Some("480"))]);
        assert_eq!(
            check_usb_link(dir.path(), "", 10000),
            UsbLinkStep::NotChecked
        );
        let gone = dir.path().join("absent");
        let c = checked(check_usb_link(&gone, "DAS-X", 10000));
        assert_eq!(c.status, RecordStatus::Warn);
        assert_eq!(c.speed, None);
    }

    #[test]
    fn the_usb_link_row_and_speed_text_are_the_scripts() {
        assert_eq!(UsbLinkStep::NotChecked.row(), "N/A  (not checked)");
        assert_eq!(UsbLinkStep::NotChecked.speed_text(), "unknown");
        let dir = sysfs(&[("1-1", Some("Acme DAS-X"), Some("5000"))]);
        let step = check_usb_link(dir.path(), "DAS-X", 10000);
        assert_eq!(step.row(), "FAIL  (negotiated 5000 Mbit/s, expected 10000)");
        assert_eq!(step.speed_text(), "5000");
        let none = check_usb_link(sysfs(&[]).path(), "DAS-X", 10000);
        assert_eq!(none.speed_text(), "unknown");
    }

    #[test]
    fn the_throughput_log_sits_beside_the_growth_log() {
        assert_eq!(
            throughput_log_path("/var/lib/das-backup/growth.log"),
            PathBuf::from("/var/lib/das-backup/throughput.jsonl")
        );
        assert_eq!(
            throughput_log_path("growth.log"),
            PathBuf::from("throughput.jsonl")
        );
    }

    #[test]
    fn a_throughput_line_is_the_scripts_json_with_the_link_speed_a_string() {
        assert_eq!(
            throughput_line("2026-11-10T03:00:00-08:00", 100, 5000, "5000").as_deref(),
            Some(
                r#"{"ts":"2026-11-10T03:00:00-08:00","elapsed_s":100,"bytes":5000,"bytes_per_s":50,"usb_link_mbit_s":"5000"}"#
            )
        );
        let unknown = throughput_line("t", 10, 10, "unknown").unwrap();
        assert!(
            unknown.ends_with(r#""usb_link_mbit_s":"unknown"}"#),
            "{unknown}"
        );
        assert_eq!(throughput_line("t", 0, 10, "1"), None, "no whole second");
        assert_eq!(throughput_line("t", 10, 0, "1"), None, "nothing written");
    }

    #[test]
    fn growth_lines_name_each_read_target_and_never_invent_a_zero() {
        let lines = growth_lines(
            "2026-11-10T03:00:00",
            &[
                ("/mnt/a".into(), Some(123)),
                ("/mnt/b".into(), None),
                ("/mnt/c".into(), Some(0)),
            ],
        );
        assert_eq!(
            lines,
            vec![
                "2026-11-10T03:00:00 /mnt/a 123".to_string(),
                "2026-11-10T03:00:00 /mnt/c 0".to_string()
            ]
        );
    }

    #[test]
    fn the_stamps_have_the_shape_date_gives() {
        let is_digits = |s: &str| s.bytes().all(|b| b.is_ascii_digit());
        let g = growth_stamp(1_700_000_000);
        let (d, t) = g.split_once('T').expect("T");
        assert_eq!(d.len(), 10, "{g}");
        assert!(
            d.split('-').all(is_digits) && t.split(':').all(is_digits),
            "{g}"
        );
        assert_eq!(t.len(), 8, "{g}");
        let iso = iso_local(1_700_000_000);
        assert!(iso.starts_with(&g), "{iso} vs {g}");
        let offset = &iso[g.len()..];
        assert_eq!(offset.len(), 6, "{iso}");
        assert!(
            offset.starts_with(['+', '-']) && &offset[3..4] == ":",
            "{iso}"
        );
    }

    fn performed(section: &str, failure: Option<&str>) -> ExpireRun {
        ExpireRun::Performed {
            section: section.into(),
            failure: failure.map(Into::into),
        }
    }

    #[test]
    fn an_expiry_that_ran_is_ok_and_one_that_failed_says_where_to_look() {
        let ok = ExpiryStep::from_run(performed("RETIRED SUBVOLUMES\n  x\n", None), false);
        assert_eq!(ok.row(), "OK  (n/a)");
        assert!(!ok.failed());
        let bad = ExpiryStep::from_run(performed("", Some("boom")), false);
        assert!(bad.failed());
        assert_eq!(
            bad.row(),
            "FAIL  (boom — see RETIRED SUBVOLUMES in the report)"
        );
        let gone = ExpiryStep::from_run(ExpireRun::Unavailable("no path".into()), false);
        assert_eq!(gone.row(), "SKIP  (not performed: no path)");
        assert!(!gone.failed());
    }

    #[test]
    fn a_held_back_expiry_is_a_skip_whose_section_says_nothing_was_deleted() {
        let run = performed("RETIRED SUBVOLUMES\n  would delete a\n", None);
        let step = ExpiryStep::from_run(run, true);
        let ExpiryStep::Ran {
            status,
            detail,
            section,
        } = &step
        else {
            panic!("{step:?}");
        };
        assert_eq!(*status, ExpiryStatus::Skip);
        assert_eq!(detail, SYNC_FAILED);
        assert_eq!(
            section,
            "RETIRED SUBVOLUMES\n  EXPIRY NOT PERFORMED: subvolume sync failed this run; nothing was deleted (dry run shown)\n  would delete a\n"
        );
        // A failure stays a failure even when held back.
        assert!(ExpiryStep::from_run(performed("", Some("x")), true).failed());
    }

    #[test]
    fn a_selection_run_skips_with_the_scripts_wording() {
        let step = ExpiryStep::skipped_for_selection();
        assert_eq!(step.row(), format!("SKIP  ({NARROW_SELECTION})"));
        assert!(!step.failed());
    }

    #[test]
    fn an_expiry_whose_config_cannot_be_read_is_a_failure_with_the_reason() {
        let dir = tempfile::tempdir().unwrap();
        let run = expire_at(
            &dir.path().join("absent.toml"),
            &dir.path().join("index.db"),
            true,
            "2026-11-10",
            &crate::fsutil::testing::Scripted::from_owned(vec![]),
            &|_| false,
        );
        let ExpireRun::Performed { section, failure } = run else {
            panic!("{run:?}");
        };
        assert_eq!(section, "");
        assert!(
            failure
                .as_deref()
                .is_some_and(|f| f.contains("could not load")),
            "{failure:?}"
        );
        assert!(!dir.path().join("index.db").exists(), "no index was opened");
    }
}
