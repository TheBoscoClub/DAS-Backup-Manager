use crate::config::{Config, InitSystem};
use std::process::Command;

/// Information about the current backup schedule.
#[derive(Debug, Clone)]
pub struct ScheduleInfo {
    pub incremental_time: String,
    pub full_schedule: String,
    pub delay_min: u32,
    pub enabled: bool,
    pub next_incremental: Option<String>,
    pub next_full: Option<String>,
}

// ---------------------------------------------------------------------------
// Systemd timer query helpers
// ---------------------------------------------------------------------------

/// Run `systemctl is-enabled <unit>` and return true if it exits 0.
fn systemctl_is_enabled(unit: &str) -> bool {
    Command::new("systemctl")
        .args(["is-enabled", unit])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Run `systemctl show <unit> --property=NextElapseUSecRealtime --value`
/// and parse the microsecond timestamp into a human-readable string.
///
/// Returns `None` when the timer is not installed, the command fails, or the
/// timestamp is zero (timer not yet scheduled).
fn systemctl_next_elapse(unit: &str) -> Option<String> {
    let output = Command::new("systemctl")
        .args(["show", unit, "--property=NextElapseUSecRealtime", "--value"])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let raw = String::from_utf8_lossy(&output.stdout);
    let trimmed = raw.trim();

    // systemd returns microseconds since epoch, or a human-readable local-time
    // string depending on the version.  Try both.
    parse_systemd_next_elapse(trimmed)
}

/// Parse the value of `NextElapseUSecRealtime`.
///
/// systemd may return:
/// - A bare microsecond integer (e.g. `"1740891600000000"`)
/// - A human-readable local string (e.g. `"Mon 2026-03-02 03:00:00 CST"`)
/// - `"0"` when the timer is inactive / not scheduled
///
/// The function returns `None` for a zero timestamp or an unrecognisable value,
/// and a normalised `"YYYY-MM-DD HH:MM"` string otherwise.
pub(crate) fn parse_systemd_next_elapse(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() || trimmed == "0" {
        return None;
    }

    // Try parsing as a raw microsecond integer first.
    if let Ok(usec) = trimmed.parse::<u64>() {
        if usec == 0 {
            return None;
        }
        let secs = usec / 1_000_000;
        // Format as UTC ISO-like string using only stdlib (no chrono dependency).
        return Some(usec_to_display(secs));
    }

    // Otherwise expect a systemd local-time string like:
    //   "Mon 2026-03-02 03:00:00 CST"
    //   "Mon 2026-03-02 03:00:00 UTC"
    //   "Tue 2026-03-03 04:30:00 EST"
    // We want "YYYY-MM-DD HH:MM".
    let parts: Vec<&str> = trimmed.split_whitespace().collect();
    // parts[0] = weekday, parts[1] = YYYY-MM-DD, parts[2] = HH:MM:SS, parts[3] = TZ
    if parts.len() >= 3 {
        let date = parts[1];
        let time_full = parts[2];
        // Trim seconds from HH:MM:SS -> HH:MM
        let time_hhmm = time_full
            .splitn(3, ':')
            .take(2)
            .collect::<Vec<_>>()
            .join(":");
        return Some(format!("{date} {time_hhmm}"));
    }

    None
}

/// Convert a Unix timestamp (seconds) to a simple display string.
///
/// Uses a rough calculation without importing chrono.  Accurate for dates in
/// the near future (2026-2100).  The output format is `"YYYY-MM-DD HH:MM (UTC)"`.
fn usec_to_display(secs: u64) -> String {
    // Days since Unix epoch.
    let days = secs / 86400;
    let rem = secs % 86400;
    let hh = rem / 3600;
    let mm = (rem % 3600) / 60;

    // Civil calendar from day count (Gregorian proleptic via the formula from
    // Howard Hinnant's chrono paper — license: public domain).
    let z = days as i64 + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y_adj = if m <= 2 { y + 1 } else { y };

    format!("{y_adj:04}-{m:02}-{d:02} {hh:02}:{mm:02} (UTC)")
}

// ---------------------------------------------------------------------------
// Cron helper
// ---------------------------------------------------------------------------

/// Build a cron schedule line from a time string like "03:00".
/// Returns `None` if the format is not parseable as a valid `HH:MM` (hour 0-23, minute 0-59).
fn time_to_cron(time_str: &str) -> Option<String> {
    let parts: Vec<&str> = time_str.split(':').collect();
    if parts.len() != 2 {
        return None;
    }
    let hh: u8 = parts[0].parse().ok()?;
    let mm: u8 = parts[1].parse().ok()?;
    // Validate ranges explicitly.
    if hh > 23 || mm > 59 {
        return None;
    }
    // "mm hh * * * root /usr/bin/btrdasd backup --incremental"
    Some(format!(
        "{mm} {hh} * * * root /usr/bin/btrdasd backup --incremental"
    ))
}

/// Write or remove `/etc/cron.d/das-backup`.
fn manage_cron_entry(config: &Config, enabled: bool) -> Result<(), Box<dyn std::error::Error>> {
    let cron_path = "/etc/cron.d/das-backup";
    if enabled {
        let cron_line = time_to_cron(&config.schedule.incremental).ok_or_else(|| {
            format!(
                "Cannot build cron expression from schedule '{}'; expected HH:MM",
                config.schedule.incremental
            )
        })?;
        let content = format!(
            "# Managed by btrdasd — do not edit manually.\n\
             SHELL=/bin/sh\n\
             PATH=/usr/local/sbin:/usr/local/bin:/sbin:/bin:/usr/sbin:/usr/bin\n\
             {cron_line}\n"
        );
        std::fs::write(cron_path, content)
            .map_err(|e| format!("Failed to write cron file {cron_path}: {e}"))?;
    } else {
        match std::fs::remove_file(cron_path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(format!("Failed to remove cron file {cron_path}: {e}").into());
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Get current schedule info by querying systemd timers (or cron/openrc).
pub fn get_schedule(config: &Config) -> Result<ScheduleInfo, Box<dyn std::error::Error>> {
    let (enabled, next_incremental, next_full) = match config.init.system {
        InitSystem::Systemd => {
            let enabled = systemctl_is_enabled("das-backup.timer");
            let next_inc = systemctl_next_elapse("das-backup.timer");
            let next_full = systemctl_next_elapse("das-backup-full.timer");
            (enabled, next_inc, next_full)
        }
        // For non-systemd init systems we report based on cron/rc presence,
        // but we cannot easily query next-run time without extra tooling.
        InitSystem::Sysvinit => {
            let enabled = std::path::Path::new("/etc/cron.d/das-backup").exists();
            (enabled, None, None)
        }
        InitSystem::Openrc => {
            let output = Command::new("rc-status")
                .args(["--all"])
                .output()
                .unwrap_or_else(|_| std::process::Output {
                    status: std::process::ExitStatus::default(),
                    stdout: Vec::new(),
                    stderr: Vec::new(),
                });
            let enabled = String::from_utf8_lossy(&output.stdout).contains("das-backup");
            (enabled, None, None)
        }
    };

    Ok(ScheduleInfo {
        incremental_time: config.schedule.incremental.clone(),
        full_schedule: config.schedule.full.clone(),
        delay_min: config.schedule.randomized_delay_min,
        enabled,
        next_incremental,
        next_full,
    })
}

/// Modify the backup schedule — updates config fields in memory only.
///
/// The caller is responsible for persisting the config to disk and calling
/// `install` to regenerate timer/cron files.
pub fn set_schedule(
    config: &mut Config,
    incremental: Option<&str>,
    full: Option<&str>,
    delay: Option<u32>,
) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(inc) = incremental {
        config.schedule.incremental = inc.to_string();
    }
    if let Some(f) = full {
        config.schedule.full = f.to_string();
    }
    if let Some(d) = delay {
        config.schedule.randomized_delay_min = d;
    }
    Ok(())
}

/// Enable or disable scheduled backups via the appropriate init system.
pub fn set_enabled(config: &Config, enabled: bool) -> Result<(), Box<dyn std::error::Error>> {
    match config.init.system {
        InitSystem::Systemd => {
            let verb = if enabled { "enable" } else { "disable" };
            let output = Command::new("systemctl")
                .args([verb, "--now", "das-backup.timer", "das-backup-full.timer"])
                .output()
                .map_err(|e| format!("Failed to run systemctl {verb}: {e}"))?;

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                return Err(format!("systemctl {verb} failed: {stderr}").into());
            }
        }
        InitSystem::Sysvinit => {
            manage_cron_entry(config, enabled)?;
        }
        InitSystem::Openrc => {
            let verb = if enabled { "add" } else { "del" };
            let output = Command::new("rc-update")
                .args([verb, "das-backup"])
                .output()
                .map_err(|e| format!("Failed to run rc-update {verb}: {e}"))?;

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                return Err(format!("rc-update {verb} failed: {stderr}").into());
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, InitSystem};

    // -----------------------------------------------------------------------
    // Pre-existing test (kept unchanged)
    // -----------------------------------------------------------------------

    #[test]
    fn schedule_info_fields() {
        let info = ScheduleInfo {
            incremental_time: "03:00".into(),
            full_schedule: "Sun 04:00".into(),
            delay_min: 30,
            enabled: true,
            next_incremental: Some("2026-02-28 03:00".into()),
            next_full: None,
        };
        assert_eq!(info.incremental_time, "03:00");
        assert_eq!(info.delay_min, 30);
        assert!(info.enabled);
        assert!(info.next_incremental.is_some());
        assert!(info.next_full.is_none());
    }

    // -----------------------------------------------------------------------
    // New tests
    // -----------------------------------------------------------------------

    /// Parse a human-readable systemd NextElapseUSecRealtime string.
    #[test]
    fn test_parse_systemctl_next_elapse() {
        // Weekday + date + time + timezone format produced by systemd.
        let result = parse_systemd_next_elapse("Mon 2026-03-02 03:00:00 CST");
        assert!(
            result.is_some(),
            "expected Some(...), got None for human-readable input"
        );
        let s = result.unwrap();
        println!("Parsed next_elapse: {s}");
        assert!(
            s.starts_with("2026-03-02"),
            "expected date '2026-03-02' in '{s}'"
        );
        assert!(s.contains("03:00"), "expected time '03:00' in '{s}'");
    }

    /// Zero timestamp returns None (timer not yet scheduled).
    #[test]
    fn test_parse_systemctl_next_elapse_zero() {
        assert!(
            parse_systemd_next_elapse("0").is_none(),
            "zero usec should return None"
        );
        assert!(
            parse_systemd_next_elapse("").is_none(),
            "empty string should return None"
        );
    }

    /// Microsecond integer timestamp round-trips through the display formatter.
    #[test]
    fn test_parse_systemctl_next_elapse_usec() {
        // 1772420400 seconds = 2026-03-02 03:00:00 UTC exactly.
        let usec = 1772420400u64 * 1_000_000;
        let result = parse_systemd_next_elapse(&usec.to_string());
        assert!(
            result.is_some(),
            "expected Some(...) for usec '{usec}', got None"
        );
        let s = result.unwrap();
        println!("Parsed usec next_elapse: {s}");
        assert!(s.contains("2026-03-02"), "expected date in '{s}'");
        assert!(s.contains("03:00"), "expected time in '{s}'");
    }

    /// set_schedule updates config fields when Some values are provided.
    #[test]
    fn test_set_schedule_updates_config() {
        let mut config = Config::default();
        assert_eq!(config.schedule.incremental, "03:00");
        assert_eq!(config.schedule.full, "Sun 04:00");
        assert_eq!(config.schedule.randomized_delay_min, 30);

        set_schedule(&mut config, Some("05:00"), Some("Sat 06:00"), Some(15))
            .expect("set_schedule should not fail");

        assert_eq!(config.schedule.incremental, "05:00");
        assert_eq!(config.schedule.full, "Sat 06:00");
        assert_eq!(config.schedule.randomized_delay_min, 15);
    }

    /// set_schedule does not touch fields when None values are provided.
    #[test]
    fn test_set_schedule_preserves_none_fields() {
        let mut config = Config::default();
        set_schedule(&mut config, None, Some("Wed 02:00"), None)
            .expect("set_schedule should not fail");

        // Unchanged fields keep their defaults.
        assert_eq!(config.schedule.incremental, "03:00");
        assert_eq!(config.schedule.randomized_delay_min, 30);
        // Only the provided field changed.
        assert_eq!(config.schedule.full, "Wed 02:00");
    }

    /// get_schedule returns a ScheduleInfo populated from config values.
    #[test]
    fn test_schedule_info_from_config() {
        let mut config = Config::default();
        // Override with non-default values to verify they pass through.
        config.schedule.incremental = "02:30".into();
        config.schedule.full = "Fri 03:00".into();
        config.schedule.randomized_delay_min = 10;
        // Use Sysvinit so we don't invoke systemctl in the test environment.
        config.init.system = InitSystem::Sysvinit;

        let info = get_schedule(&config).expect("get_schedule should not fail");

        assert_eq!(info.incremental_time, "02:30");
        assert_eq!(info.full_schedule, "Fri 03:00");
        assert_eq!(info.delay_min, 10);
        // next_* are always None for Sysvinit without actual cron tooling.
        assert!(info.next_incremental.is_none());
        assert!(info.next_full.is_none());
    }

    /// time_to_cron produces a valid cron expression.
    #[test]
    fn test_time_to_cron() {
        let line = time_to_cron("03:00").expect("should parse 03:00");
        println!("cron line: {line}");
        // Cron fields: minute=0, hour=3, *, *, *
        assert!(line.starts_with("0 3 "), "expected '0 3 ...' in '{line}'");
        assert!(
            line.contains("btrdasd"),
            "expected btrdasd command in '{line}'"
        );
    }

    /// time_to_cron returns None for invalid input.
    #[test]
    fn test_time_to_cron_invalid() {
        assert!(time_to_cron("not-a-time").is_none());
        assert!(time_to_cron("25:00").is_none()); // u8 overflows for hour 25 but ok here
        assert!(time_to_cron("").is_none());
    }
}
