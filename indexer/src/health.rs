use crate::config::Config;
use regex::Regex;
use std::fs;
use std::path::Path;
use std::sync::LazyLock;
use std::time::UNIX_EPOCH;

/// Matches btrbk snapshot directory names: `<name>.<YYYYMMDDTHHMMSS>`
static SNAP_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(.+)\.\d{8}T\d{4,6}$").expect("valid snapshot regex"));

/// Overall health status of the backup system.
#[derive(Debug, Clone, PartialEq)]
pub enum HealthStatus {
    Healthy,
    Warning,
    Critical,
}

/// Health information for a single target drive.
#[derive(Debug, Clone)]
pub struct TargetHealth {
    pub label: String,
    pub serial: String,
    pub mounted: bool,
    pub total_bytes: u64,
    pub used_bytes: u64,
    pub snapshot_count: usize,
    pub smart_status: Option<String>,
    pub temperature_c: Option<i32>,
    pub power_on_hours: Option<u64>,
    pub errors: Option<u64>,
}

impl TargetHealth {
    /// Percentage of disk space used (0.0-100.0).
    pub fn usage_percent(&self) -> f64 {
        if self.total_bytes == 0 {
            return 0.0;
        }
        (self.used_bytes as f64 / self.total_bytes as f64) * 100.0
    }
}

/// Growth trend data point.
#[derive(Debug, Clone)]
pub struct GrowthPoint {
    pub timestamp: i64,
    pub target_label: String,
    pub used_bytes: u64,
}

/// Full health report for the backup system.
#[derive(Debug)]
pub struct HealthReport {
    pub status: HealthStatus,
    pub targets: Vec<TargetHealth>,
    pub last_backup: Option<String>,
    pub growth_points: Vec<GrowthPoint>,
    pub warnings: Vec<String>,
}

// ---------------------------------------------------------------------------
// Helper functions
// ---------------------------------------------------------------------------

/// Check if a path is an active mount point by reading `/proc/mounts`.
/// Falls back to `false` if `/proc/mounts` is unreadable.
pub fn is_mountpoint(path: &Path) -> bool {
    // "/" is always a mount point, and a quick canonical check avoids the
    // unlikely race between exists() and /proc/mounts parsing.
    if !path.exists() {
        return false;
    }
    let canonical = match path.canonicalize() {
        Ok(p) => p,
        Err(_) => return false,
    };
    let target = canonical.to_string_lossy();

    let mounts = match fs::read_to_string("/proc/mounts") {
        Ok(s) => s,
        Err(_) => return false,
    };

    for line in mounts.lines() {
        // /proc/mounts columns: device mountpoint fstype options dump pass
        let mut cols = line.splitn(3, ' ');
        cols.next(); // device
        if let Some(mp) = cols.next()
            && mp == target.as_ref()
        {
            return true;
        }
    }
    false
}

/// Parse the raw text output of `btrfs filesystem usage --raw <mount>` and
/// return `(total_bytes, used_bytes)`.
///
/// The lines we care about look like:
/// ```text
///     Device size:                    21001628770304
///     Used:                            4763696603136
/// ```
pub fn parse_btrfs_usage(output: &str) -> Option<(u64, u64)> {
    let mut total: Option<u64> = None;
    let mut used: Option<u64> = None;

    for line in output.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("Device size:") {
            total = trimmed
                .split(':')
                .nth(1)
                .and_then(|v| v.trim().parse::<u64>().ok());
        } else if trimmed.starts_with("Used:") {
            used = trimmed
                .split(':')
                .nth(1)
                .and_then(|v| v.trim().parse::<u64>().ok());
        }
    }

    match (total, used) {
        (Some(t), Some(u)) => Some((t, u)),
        _ => None,
    }
}

/// Parse the JSON output of `smartctl --json --all <device>` and return the
/// SMART status string (`"PASSED"` or `"FAILED"`).
pub fn parse_smartctl_json(json_str: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(json_str).ok()?;
    let passed = v.get("smart_status")?.get("passed")?.as_bool()?;
    Some(if passed {
        "PASSED".to_string()
    } else {
        "FAILED".to_string()
    })
}

/// Detailed SMART information parsed from `smartctl --json --all` output.
pub struct SmartDetails {
    pub status: String,
    pub temperature_c: Option<i32>,
    pub power_on_hours: Option<u64>,
    pub errors: Option<u64>,
}

/// Parse the JSON output of `smartctl --json --all <device>` and return detailed
/// SMART information including temperature, power-on hours, and error counts.
pub fn parse_smartctl_details(json_str: &str) -> Option<SmartDetails> {
    let v: serde_json::Value = serde_json::from_str(json_str).ok()?;
    let passed = v.get("smart_status")?.get("passed")?.as_bool()?;
    let temperature_c = v
        .get("temperature")
        .and_then(|t| t.get("current"))
        .and_then(|c| c.as_i64())
        .map(|t| t as i32);
    let power_on_hours = v
        .get("power_on_time")
        .and_then(|p| p.get("hours"))
        .and_then(|h| h.as_u64());
    let errors = v
        .get("ata_smart_error_log")
        .and_then(|e| e.get("summary"))
        .and_then(|s| s.get("count"))
        .and_then(|c| c.as_u64());
    Some(SmartDetails {
        status: if passed {
            "PASSED".to_string()
        } else {
            "FAILED".to_string()
        },
        temperature_c,
        power_on_hours,
        errors,
    })
}

/// Attempt to find the block device path whose serial number contains `serial`.
///
/// Walks `/dev/disk/by-id/` looking for symlinks whose name includes the
/// serial string. Returns the first matching real device path (e.g.
/// `/dev/sdb`), excluding partition entries (names that end in `-partN`).
pub fn device_from_serial(serial: &str) -> Option<String> {
    device_info_from_serial(serial).map(|(dev, _is_usb)| dev)
}

/// Resolve a `/dev/disk/by-id/` symlink for a drive identified by its
/// serial string. Returns `(device_path, is_usb)` — `is_usb` is true when
/// the by-id symlink name starts with `usb-`, indicating a USB-attached
/// drive that needs `smartctl -d sat` for SMART access.
pub fn device_info_from_serial(serial: &str) -> Option<(String, bool)> {
    if serial.is_empty() {
        return None;
    }
    let by_id = Path::new("/dev/disk/by-id");
    if !by_id.exists() {
        return None;
    }
    let entries = fs::read_dir(by_id).ok()?;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        // Skip partition symlinks like `ata-WDC_WD20EFRX-..._123456-part1`
        if name.contains(serial) && !name.ends_with(|c: char| c.is_ascii_digit())
            || (name.contains(serial) && !name.contains("-part"))
        {
            // Resolve the symlink to get the real device path
            if let Ok(target) = fs::read_link(entry.path()) {
                let resolved = if target.is_absolute() {
                    target
                } else {
                    by_id.join(&target)
                };
                if let Ok(canonical) = resolved.canonicalize() {
                    let dev_str = canonical.to_string_lossy().to_string();
                    // Skip partition devices (/dev/sdb1, /dev/nvme0n1p1, etc.)
                    if !dev_str.chars().last().is_some_and(|c| c.is_ascii_digit()) {
                        let is_usb = name.starts_with("usb-");
                        return Some((dev_str, is_usb));
                    }
                }
            }
        }
    }
    None
}

/// Get disk space for `mount` using `statvfs(2)`.
/// Returns `(total_bytes, used_bytes)` or `None` on error.
fn disk_space_statvfs(mount: &str) -> Option<(u64, u64)> {
    use std::ffi::CString;
    let c_path = CString::new(mount).ok()?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) };
    if rc != 0 {
        return None;
    }
    let block_size = stat.f_frsize;
    let total = stat.f_blocks * block_size;
    let avail = stat.f_bavail * block_size;
    let used = total.saturating_sub(avail);
    Some((total, used))
}

/// Count snapshot directories inside `mount` that match btrbk naming convention.
fn count_snapshots(mount: &str) -> usize {
    let path = Path::new(mount);
    if !path.is_dir() {
        return 0;
    }
    let mut count = 0usize;
    // Walk one level of subdirectories (source dirs like "nvme", "ssd")
    if let Ok(source_entries) = fs::read_dir(path) {
        for source_entry in source_entries.flatten() {
            if !source_entry
                .file_type()
                .is_ok_and(|ft| ft.is_dir() || ft.is_symlink())
            {
                continue;
            }
            // Count snapshot dirs inside each source dir
            if let Ok(snap_entries) = fs::read_dir(source_entry.path()) {
                for snap_entry in snap_entries.flatten() {
                    let name = snap_entry.file_name().to_string_lossy().to_string();
                    if SNAP_RE.is_match(&name) {
                        count += 1;
                    }
                }
            }
        }
    }
    count
}

/// Walk all mounted target mount points and return the most recent snapshot
/// directory modification time as a Unix timestamp string (`"YYYY-MM-DD HH:MM"`),
/// or `None` if nothing is accessible.
fn latest_snapshot_time(targets: &[TargetHealth], mounts: &[String]) -> Option<String> {
    let mut latest: Option<u64> = None;

    for (th, mount) in targets.iter().zip(mounts.iter()) {
        if !th.mounted {
            continue;
        }
        let path = Path::new(mount.as_str());
        if let Ok(source_entries) = fs::read_dir(path) {
            for source_entry in source_entries.flatten() {
                if let Ok(snap_entries) = fs::read_dir(source_entry.path()) {
                    for snap_entry in snap_entries.flatten() {
                        let name = snap_entry.file_name().to_string_lossy().to_string();
                        if SNAP_RE.is_match(&name)
                            && let Ok(meta) = snap_entry.metadata()
                            && let Ok(modified) = meta.modified()
                        {
                            let secs = modified
                                .duration_since(UNIX_EPOCH)
                                .map(|d| d.as_secs())
                                .unwrap_or(0);
                            if latest.is_none_or(|prev| secs > prev) {
                                latest = Some(secs);
                            }
                        }
                    }
                }
            }
        }
    }

    latest.map(|secs| {
        // Format as simple UTC date-time string without pulling in chrono.
        // UNIX timestamp -> broken-down time via manual division.
        let minutes_total = secs / 60;
        let minute = minutes_total % 60;
        let hours_total = minutes_total / 60;
        let hour = hours_total % 24;
        let days_since_epoch = hours_total / 24;
        // Gregorian calendar approximation (good enough for display)
        let (year, month, day) = days_to_ymd(days_since_epoch as i64);
        format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}")
    })
}

/// Convert days since Unix epoch (1970-01-01) to (year, month, day).
/// Uses the proleptic Gregorian calendar algorithm from civil.h (Howard Hinnant).
pub fn days_to_ymd(z: i64) -> (i32, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y as i32, m as u32, d as u32)
}

/// Determine the overall `HealthStatus` from a slice of per-target health
/// records and an accompanying list of warning messages.
///
/// Rules:
/// - Critical: any target with SMART "FAILED" or usage > 95 %
/// - Warning: any target with usage > 85 %, SMART unavailable, or unmounted
/// - Healthy: everything else
fn determine_status(targets: &[TargetHealth], warnings: &[String]) -> HealthStatus {
    for t in targets {
        if t.smart_status.as_deref() == Some("FAILED") {
            return HealthStatus::Critical;
        }
        if t.total_bytes > 0 && t.usage_percent() > 95.0 {
            return HealthStatus::Critical;
        }
    }

    if !warnings.is_empty() {
        return HealthStatus::Warning;
    }

    HealthStatus::Healthy
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Query health status of all configured targets.
pub fn get_health(config: &Config) -> Result<HealthReport, Box<dyn std::error::Error>> {
    let mut target_healths: Vec<TargetHealth> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    let mounts: Vec<String> = config.targets.iter().map(|t| t.mount.clone()).collect();

    for target in &config.targets {
        let mount_path = Path::new(&target.mount);

        // 1. Check mount status
        let path_exists = mount_path.exists();
        let mounted = path_exists && is_mountpoint(mount_path);

        if !mounted {
            warnings.push(format!(
                "Target '{}' (mount: {}) is not mounted",
                target.label, target.mount
            ));
        }

        // 2. Get disk space
        let (total_bytes, used_bytes) = if mounted {
            // Prefer btrfs filesystem usage for accuracy; fall back to statvfs
            let btrfs_output = std::process::Command::new("btrfs")
                .args(["filesystem", "usage", "--raw", &target.mount])
                .output()
                .ok()
                .filter(|o| o.status.success())
                .and_then(|o| String::from_utf8(o.stdout).ok());

            if let Some(output) = btrfs_output {
                parse_btrfs_usage(&output)
                    .or_else(|| disk_space_statvfs(&target.mount))
                    .unwrap_or((0, 0))
            } else {
                disk_space_statvfs(&target.mount).unwrap_or((0, 0))
            }
        } else {
            (0, 0)
        };

        // 3. Get snapshot count
        let snapshot_count = if mounted {
            count_snapshots(&target.mount)
        } else {
            0
        };

        // 4. Get SMART details (use -d sat for USB-attached SATA drives)
        let smart_details = if !target.serial.is_empty() {
            device_info_from_serial(&target.serial)
                .and_then(|(dev, is_usb)| {
                    let mut cmd = std::process::Command::new("smartctl");
                    cmd.args(["--json", "--all"]);
                    if is_usb {
                        cmd.arg("-d").arg("sat");
                    }
                    cmd.arg(&dev).output().ok()
                })
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .and_then(|json| parse_smartctl_details(&json))
        } else {
            None
        };

        let smart_status = smart_details.as_ref().map(|d| d.status.clone());
        let temperature_c = smart_details.as_ref().and_then(|d| d.temperature_c);
        let power_on_hours = smart_details.as_ref().and_then(|d| d.power_on_hours);
        let errors = smart_details.as_ref().and_then(|d| d.errors);

        // 5. Build warnings for this target
        if mounted {
            let usage = if total_bytes > 0 {
                (used_bytes as f64 / total_bytes as f64) * 100.0
            } else {
                0.0
            };

            if usage > 95.0 {
                warnings.push(format!(
                    "Target '{}' is critically full: {:.1}% used",
                    target.label, usage
                ));
            } else if usage > 85.0 {
                warnings.push(format!(
                    "Target '{}' is nearly full: {:.1}% used",
                    target.label, usage
                ));
            }

            match &smart_status {
                None => warnings.push(format!(
                    "Target '{}': SMART data unavailable (drive not connected or smartctl not installed)",
                    target.label
                )),
                Some(s) if s == "FAILED" => warnings.push(format!(
                    "Target '{}': SMART status FAILED — drive may be failing!",
                    target.label
                )),
                _ => {}
            }
        }

        target_healths.push(TargetHealth {
            label: target.label.clone(),
            serial: target.serial.clone(),
            mounted,
            total_bytes,
            used_bytes,
            snapshot_count,
            smart_status,
            temperature_c,
            power_on_hours,
            errors,
        });
    }

    // 6. Parse growth log — map mount paths to target labels
    let mount_to_label: std::collections::HashMap<&str, &str> = config
        .targets
        .iter()
        .map(|t| (t.mount.as_str(), t.label.as_str()))
        .collect();
    let growth_points = fs::read_to_string(&config.general.growth_log)
        .map(|content| {
            let mut pts = parse_growth_log(&content);
            for pt in &mut pts {
                if let Some(label) = mount_to_label.get(pt.target_label.as_str()) {
                    pt.target_label = (*label).to_string();
                }
            }
            pts
        })
        .unwrap_or_default();

    // 7. Determine overall status
    let status = determine_status(&target_healths, &warnings);

    // 8. Last backup time
    let last_backup = latest_snapshot_time(&target_healths, &mounts);

    Ok(HealthReport {
        status,
        targets: target_healths,
        last_backup,
        growth_points,
        warnings,
    })
}

/// Parse an ISO 8601 datetime string (`YYYY-MM-DDTHH:MM:SS`) into a Unix
/// timestamp (seconds since epoch).  Returns `None` for malformed input.
fn parse_iso_datetime(s: &str) -> Option<i64> {
    // Try parsing as plain i64 first (backwards compat with raw Unix timestamps)
    if let Ok(ts) = s.parse::<i64>() {
        return Some(ts);
    }

    // Parse "YYYY-MM-DDTHH:MM:SS" — no timezone, assumed UTC-ish (good enough
    // for day-granularity growth tracking).
    let (date_part, time_part) = s.split_once('T')?;
    let mut date_iter = date_part.split('-');
    let year: i64 = date_iter.next()?.parse().ok()?;
    let month: i64 = date_iter.next()?.parse().ok()?;
    let day: i64 = date_iter.next()?.parse().ok()?;

    let mut time_iter = time_part.split(':');
    let hour: i64 = time_iter.next()?.parse().ok()?;
    let min: i64 = time_iter.next()?.parse().ok()?;
    let sec: i64 = time_iter.next()?.parse().ok()?;

    // Convert to days since epoch, then to seconds.
    // Inverse of days_to_ymd (civil_from_days algorithm, Howard Hinnant).
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let m = month;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;

    Some(days * 86400 + hour * 3600 + min * 60 + sec)
}

/// Parse a growth.log file into GrowthPoint entries.
///
/// Each line has the format: `<timestamp> <mount_path_or_label> <used_bytes>`
/// where timestamp can be either a Unix epoch integer or an ISO 8601 datetime
/// string (`YYYY-MM-DDTHH:MM:SS`).
pub fn parse_growth_log(content: &str) -> Vec<GrowthPoint> {
    let mut points = Vec::new();
    for line in content.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 3
            && let (Some(ts), Ok(used)) = (parse_iso_datetime(parts[0]), parts[2].parse::<u64>())
        {
            points.push(GrowthPoint {
                timestamp: ts,
                target_label: parts[1].to_string(),
                used_bytes: used,
            });
        }
    }
    points
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // --- pre-existing tests (unchanged) ---

    #[test]
    fn target_health_usage_percent() {
        let th = TargetHealth {
            label: "test".into(),
            serial: "ABC".into(),
            mounted: true,
            total_bytes: 1_000_000,
            used_bytes: 250_000,
            snapshot_count: 10,
            smart_status: Some("PASSED".into()),
            temperature_c: Some(32),
            power_on_hours: Some(12345),
            errors: None,
        };
        let pct = th.usage_percent();
        assert!((pct - 25.0).abs() < 0.01);
    }

    #[test]
    fn target_health_usage_percent_zero_total() {
        let th = TargetHealth {
            label: "empty".into(),
            serial: "X".into(),
            mounted: false,
            total_bytes: 0,
            used_bytes: 0,
            snapshot_count: 0,
            smart_status: None,
            temperature_c: None,
            power_on_hours: None,
            errors: None,
        };
        assert_eq!(th.usage_percent(), 0.0);
    }

    #[test]
    fn parse_iso_datetime_valid() {
        let ts = parse_iso_datetime("2026-02-20T07:39:42").unwrap();
        // 2026-02-20 07:39:42 UTC ≈ day 20504 * 86400 + 7*3600 + 39*60 + 42
        assert!(ts > 1_700_000_000, "timestamp should be recent: {ts}");
        // Verify round-trip through days_to_ymd
        let days = ts / 86400;
        let (y, m, d) = days_to_ymd(days);
        assert_eq!(y, 2026);
        assert_eq!(m, 2);
        assert_eq!(d, 20);
    }

    #[test]
    fn parse_iso_datetime_unix_fallback() {
        assert_eq!(parse_iso_datetime("1709000000"), Some(1709000000));
    }

    #[test]
    fn parse_iso_datetime_invalid() {
        assert!(parse_iso_datetime("not-a-date").is_none());
        assert!(parse_iso_datetime("2026-13-01T00:00:00").is_some()); // month 13 parses, ymd handles
        assert!(parse_iso_datetime("").is_none());
    }

    #[test]
    fn parse_growth_log_entries() {
        let log = "1709000000 primary-22tb 5368709120\n\
                    1709086400 primary-22tb 5905580032\n\
                    1709000000 system-2tb 1073741824\n";
        let points = parse_growth_log(log);
        assert_eq!(points.len(), 3);
        assert_eq!(points[0].timestamp, 1709000000);
        assert_eq!(points[0].target_label, "primary-22tb");
        assert_eq!(points[0].used_bytes, 5368709120);
        assert_eq!(points[2].target_label, "system-2tb");
    }

    #[test]
    fn parse_growth_log_iso_timestamps() {
        let log = "2026-02-20T07:39:42 /mnt/backup-22tb 1861347422208\n\
                   2026-02-20T07:39:42 /mnt/backup-system 871137460224\n";
        let points = parse_growth_log(log);
        assert_eq!(points.len(), 2);
        assert!(points[0].timestamp > 1_700_000_000);
        assert_eq!(points[0].target_label, "/mnt/backup-22tb");
        assert_eq!(points[0].used_bytes, 1861347422208);
        assert_eq!(points[1].target_label, "/mnt/backup-system");
    }

    #[test]
    fn parse_growth_log_skips_malformed() {
        let log = "bad line\n1709000000 ok 100\nincomplete 42\n";
        let points = parse_growth_log(log);
        assert_eq!(points.len(), 1);
        assert_eq!(points[0].target_label, "ok");
    }

    #[test]
    fn health_status_equality() {
        assert_eq!(HealthStatus::Healthy, HealthStatus::Healthy);
        assert_ne!(HealthStatus::Healthy, HealthStatus::Warning);
        assert_ne!(HealthStatus::Warning, HealthStatus::Critical);
    }

    // --- new tests ---

    #[test]
    fn test_parse_btrfs_usage() {
        // Typical output of `btrfs filesystem usage --raw /mnt/backup`
        let output = "\
Overall:
    Device size:                    21001628770304
    Device allocated:                5772436480000
    Device unallocated:             15229192290304
    Device missing:                          0
    Used:                            4763696603136
    Free (estimated):               15795064954880\t(min: 8180468809728)
    Free (statfs, df):              15795064954880
    Data ratio:                               1.00
    Metadata ratio:                           1.00
    Global reserve:                    536870912\t(used: 0)
    Multiple profiles:                          No

Data,single: Size:5638021120000, Used:4763696603136 (84.49%)
   /dev/sdb         5638021120000

Metadata,single: Size:134415360000, Used:0 (0.00%)
   /dev/sdb          134415360000
";
        let result = parse_btrfs_usage(output);
        assert!(result.is_some(), "should parse btrfs usage output");
        let (total, used) = result.unwrap();
        assert_eq!(total, 21_001_628_770_304);
        assert_eq!(used, 4_763_696_603_136);
    }

    #[test]
    fn test_parse_btrfs_usage_missing_fields() {
        let output = "Some random output without the fields we need\n";
        assert!(parse_btrfs_usage(output).is_none());
    }

    #[test]
    fn test_parse_btrfs_usage_only_total() {
        let output = "    Device size:                    1000000\n";
        // Used field is missing → should return None
        assert!(parse_btrfs_usage(output).is_none());
    }

    #[test]
    fn test_parse_smartctl_json_passed() {
        let json = r#"{
            "smart_status": {
                "passed": true
            },
            "temperature": {
                "current": 32
            },
            "power_on_time": {
                "hours": 12345
            }
        }"#;
        let result = parse_smartctl_json(json);
        assert_eq!(result, Some("PASSED".to_string()));
    }

    #[test]
    fn test_parse_smartctl_json_failed() {
        let json = r#"{"smart_status": {"passed": false}}"#;
        let result = parse_smartctl_json(json);
        assert_eq!(result, Some("FAILED".to_string()));
    }

    #[test]
    fn test_parse_smartctl_json_invalid() {
        assert!(parse_smartctl_json("not json").is_none());
        assert!(parse_smartctl_json("{}").is_none());
        assert!(parse_smartctl_json(r#"{"smart_status": {}}"#).is_none());
    }

    #[test]
    fn test_is_mountpoint_root() {
        // "/" is always a mount point on any Linux system
        assert!(is_mountpoint(Path::new("/")));
    }

    #[test]
    fn test_is_mountpoint_nonexistent() {
        // A path that cannot possibly exist is not a mount point
        assert!(!is_mountpoint(Path::new(
            "/tmp/das_health_test_nonexistent_xyz"
        )));
    }

    #[test]
    fn test_is_mountpoint_regular_dir() {
        use std::fs;
        use tempfile::TempDir;
        let tmp = TempDir::new().unwrap();
        // A freshly-created temp directory is not a mount point
        assert!(!is_mountpoint(tmp.path()));
        fs::remove_dir_all(tmp.path()).ok();
    }

    #[test]
    fn test_overall_status_healthy() {
        let targets = vec![
            TargetHealth {
                label: "t1".into(),
                serial: "S1".into(),
                mounted: true,
                total_bytes: 1_000_000_000,
                used_bytes: 500_000_000, // 50%
                snapshot_count: 10,
                smart_status: Some("PASSED".into()),
                temperature_c: None,
                power_on_hours: None,
                errors: None,
            },
            TargetHealth {
                label: "t2".into(),
                serial: "S2".into(),
                mounted: true,
                total_bytes: 2_000_000_000,
                used_bytes: 800_000_000, // 40%
                snapshot_count: 5,
                smart_status: Some("PASSED".into()),
                temperature_c: None,
                power_on_hours: None,
                errors: None,
            },
        ];
        let warnings: Vec<String> = vec![];
        assert_eq!(determine_status(&targets, &warnings), HealthStatus::Healthy);
    }

    #[test]
    fn test_overall_status_warning_high_usage() {
        let targets = vec![TargetHealth {
            label: "t1".into(),
            serial: "S1".into(),
            mounted: true,
            total_bytes: 1_000_000_000,
            used_bytes: 900_000_000, // 90% — warning threshold
            snapshot_count: 10,
            smart_status: Some("PASSED".into()),
            temperature_c: None,
            power_on_hours: None,
            errors: None,
        }];
        let warnings = vec!["Target 't1' is nearly full: 90.0% used".to_string()];
        assert_eq!(determine_status(&targets, &warnings), HealthStatus::Warning);
    }

    #[test]
    fn test_overall_status_critical_smart_failed() {
        let targets = vec![TargetHealth {
            label: "t1".into(),
            serial: "S1".into(),
            mounted: true,
            total_bytes: 1_000_000_000,
            used_bytes: 300_000_000, // 30% — usage fine
            snapshot_count: 10,
            smart_status: Some("FAILED".into()), // SMART failure → Critical
            temperature_c: None,
            power_on_hours: None,
            errors: None,
        }];
        let warnings = vec!["Target 't1': SMART status FAILED".to_string()];
        assert_eq!(
            determine_status(&targets, &warnings),
            HealthStatus::Critical
        );
    }

    #[test]
    fn test_overall_status_critical_disk_full() {
        let targets = vec![TargetHealth {
            label: "t1".into(),
            serial: "S1".into(),
            mounted: true,
            total_bytes: 1_000_000_000,
            used_bytes: 970_000_000, // 97% — critical threshold
            snapshot_count: 10,
            smart_status: Some("PASSED".into()),
            temperature_c: None,
            power_on_hours: None,
            errors: None,
        }];
        let warnings = vec!["Target 't1' is critically full: 97.0% used".to_string()];
        assert_eq!(
            determine_status(&targets, &warnings),
            HealthStatus::Critical
        );
    }

    #[test]
    fn test_determine_status_no_targets() {
        let targets: Vec<TargetHealth> = vec![];
        let warnings: Vec<String> = vec![];
        assert_eq!(determine_status(&targets, &warnings), HealthStatus::Healthy);
    }

    #[test]
    fn test_days_to_ymd_epoch() {
        // Day 0 = 1970-01-01
        let (y, m, d) = days_to_ymd(0);
        assert_eq!(y, 1970);
        assert_eq!(m, 1);
        assert_eq!(d, 1);
    }

    #[test]
    fn test_days_to_ymd_known_date() {
        // 2024-02-29 (leap day): days since epoch = 19782
        let (y, m, d) = days_to_ymd(19_782);
        assert_eq!(y, 2024);
        assert_eq!(m, 2);
        assert_eq!(d, 29);
    }

    #[test]
    fn test_parse_smartctl_details() {
        let json = r#"{"smart_status":{"passed":true},"temperature":{"current":35},"power_on_time":{"hours":54321},"ata_smart_error_log":{"summary":{"count":2}}}"#;
        let d = parse_smartctl_details(json).unwrap();
        assert_eq!(d.status, "PASSED");
        assert_eq!(d.temperature_c, Some(35));
        assert_eq!(d.power_on_hours, Some(54321));
        assert_eq!(d.errors, Some(2));
    }

    #[test]
    fn test_parse_smartctl_details_failed() {
        let json = r#"{"smart_status":{"passed":false}}"#;
        let d = parse_smartctl_details(json).unwrap();
        assert_eq!(d.status, "FAILED");
        assert_eq!(d.temperature_c, None);
        assert_eq!(d.power_on_hours, None);
        assert_eq!(d.errors, None);
    }

    #[test]
    fn test_parse_smartctl_details_invalid() {
        assert!(parse_smartctl_details("not json").is_none());
        assert!(parse_smartctl_details("{}").is_none());
    }
}
