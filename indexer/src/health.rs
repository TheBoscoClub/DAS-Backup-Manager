use crate::config::{Config, Target, TargetRole};
use crate::scrub;
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
    pub scrub: ScrubHealth,
}

// ---------------------------------------------------------------------------
// Scrub health (bd DAS-Backup-Manager-5kb)
// ---------------------------------------------------------------------------

/// Overall scrub-health bucket for one DAS filesystem.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrubHealthStatus {
    /// Not a configured `[scrub].targets` entry, or `[scrub].enabled = false`.
    NotApplicable,
    /// The engine's state file has no entry for this filesystem's UUID —
    /// scrub is enabled for this target but has never completed a pass.
    NeverScrubbed,
    /// The filesystem UUID could not be resolved (drive missing, config gap).
    Unresolved,
    Ok,
    Warn,
    Fail,
}

/// Scrub health of one DAS filesystem, derived from the scrub engine's
/// persisted state (`/var/lib/das-backup/scrub-state.json`) and the
/// `[scrub]` config thresholds (`warn_age_days` / `fail_age_days`).
///
/// **Deliberately sourced only from the engine's own state file — never
/// from the raw per-device `/var/lib/btrfs/scrub.status.<fsuuid>` record.**
/// Two reasons, both load-bearing:
///
/// 1. The state file (mode `0644`) is the contract the `scrub` module docs
///    name for this integration ("written... so health checks and the GUI
///    can read it without any DAS filesystem mounted"). The raw record
///    (mode `0600`, root-only) is reserved for `btrdasd scrub status`'s
///    manual/diagnostic display, which already implements that fallback —
///    see `build_scrub_target_view` in `main.rs`.
/// 2. Automated Healthy/Warning/Critical determination must never trust an
///    ad hoc, out-of-band scrub record blindly. That is exactly the bug
///    this feature exists to fix: `system-recovery-A`'s aborted scrub
///    looked healthy for 64 days because a raw record was read at face
///    value. Only records the engine itself produced — with its
///    aborted/error cross-checks already applied via
///    [`crate::scrub::ScrubFsResult::ok`] — feed the automated rollup.
///
/// A consequence, verified live 2026-08-01: on a host where the scrub
/// engine has not yet completed a single pass (`scrub-state.json` does not
/// exist), every configured scrub target reports [`ScrubHealthStatus::NeverScrubbed`]
/// here — even for a filesystem that has a genuine, root-only, *finished*
/// raw btrfs record from an earlier manual `btrfs scrub start` run. That is
/// correct: the engine has no tracked history for it yet, and "never
/// scrubbed" is the honest answer for the source of truth this struct
/// reports. `btrdasd scrub status` (root) remains available to show the
/// raw-record history for manual triage.
///
/// This also keeps the CLI (`btrdasd health`, any user) and the GUI (via
/// the root `btrdasd-helper` D-Bus daemon) in agreement regardless of which
/// one runs privileged — there is no root-vs-unprivileged divergence to
/// reconcile here, unlike SMART data.
#[derive(Debug, Clone)]
pub struct ScrubHealth {
    pub status: ScrubHealthStatus,
    pub fsuuid: Option<String>,
    /// Epoch of the last scrub that finished clean (zero errors). Carried
    /// forward across a later failed attempt by the engine itself — never
    /// reset by an aborted/errored record.
    pub last_success_epoch: Option<i64>,
    pub age_days: Option<i64>,
    /// `finished`, `canceled`, `aborted`, or `error` — the *latest* attempt's
    /// outcome, which may be worse than `last_success_epoch` suggests.
    pub last_outcome: Option<String>,
    /// Whether the latest attempt passed (`false` = immediate fail).
    pub last_ok: Option<bool>,
    pub error_total: Option<u64>,
    pub resolve_error: Option<String>,
}

impl ScrubHealth {
    pub fn not_applicable() -> Self {
        Self {
            status: ScrubHealthStatus::NotApplicable,
            fsuuid: None,
            last_success_epoch: None,
            age_days: None,
            last_outcome: None,
            last_ok: None,
            error_total: None,
            resolve_error: None,
        }
    }

    pub fn never_scrubbed() -> Self {
        Self {
            status: ScrubHealthStatus::NeverScrubbed,
            ..Self::not_applicable()
        }
    }

    pub fn unresolved(err: String) -> Self {
        Self {
            status: ScrubHealthStatus::Unresolved,
            resolve_error: Some(err),
            ..Self::not_applicable()
        }
    }
}

/// Days elapsed from `then_epoch` to `now_epoch`, floored, clamped at 0 so a
/// slightly-in-the-future record (clock skew) never reads as a negative age.
fn age_days_between(now_epoch: i64, then_epoch: i64) -> i64 {
    (now_epoch - then_epoch).max(0) / 86_400
}

/// Derive scrub health for one filesystem from its engine state entry (or
/// the absence of one). Pure function — `now_epoch` and the thresholds are
/// parameters (not read internally) so fixture tests can pin "now" and walk
/// the warn/fail day boundaries exactly.
///
/// Threshold semantics (per the `[scrub]` config doc comments): WARN when
/// age of the last successful scrub is *strictly greater than*
/// `warn_age_days`; FAIL when *strictly greater than* `fail_age_days`. So
/// `age == warn_age_days` is still OK, and `age == fail_age_days` is still
/// only WARN.
pub fn scrub_health_for(
    fs: Option<&scrub::FsState>,
    now_epoch: i64,
    warn_age_days: u32,
    fail_age_days: u32,
) -> ScrubHealth {
    let Some(fs) = fs else {
        return ScrubHealth::never_scrubbed();
    };
    let attempt = &fs.last_attempt;

    if !attempt.ok {
        // Immediate fail: the latest attempt is aborted/canceled/errored, or
        // carries nonzero error counters (`ok` already folds both cases —
        // see `ScrubFsResult::ok`). This applies regardless of how recent or
        // how clean `last_success_epoch` is: a good scrub from weeks ago
        // must never mask a bad one today. `last_success_epoch` is still
        // reported below (carried forward by the engine, never reset by
        // this failed attempt) so callers can show "last known good" even
        // while flagging FAIL.
        return ScrubHealth {
            status: ScrubHealthStatus::Fail,
            fsuuid: None,
            last_success_epoch: fs.last_success_epoch,
            age_days: fs
                .last_success_epoch
                .map(|t| age_days_between(now_epoch, t)),
            last_outcome: Some(attempt.outcome.clone()),
            last_ok: Some(false),
            error_total: Some(attempt.error_total),
            resolve_error: None,
        };
    }

    // Latest attempt passed clean. Age off `last_success_epoch` — set by the
    // engine whenever `ok`, and the field the config thresholds are
    // documented against — never off `attempt.finished_epoch` directly.
    let Some(success_epoch) = fs.last_success_epoch else {
        // Defensive: `ok == true` should always carry a `last_success_epoch`
        // (the engine sets both together in `merge_pass_into_state`), but a
        // health check must never trust that invariant blindly — fall back
        // to "never scrubbed" rather than reporting a clean pass with no age.
        return ScrubHealth::never_scrubbed();
    };
    let age = age_days_between(now_epoch, success_epoch);
    let status = if age > i64::from(fail_age_days) {
        ScrubHealthStatus::Fail
    } else if age > i64::from(warn_age_days) {
        ScrubHealthStatus::Warn
    } else {
        ScrubHealthStatus::Ok
    };
    ScrubHealth {
        status,
        fsuuid: None,
        last_success_epoch: Some(success_epoch),
        age_days: Some(age),
        last_outcome: Some(attempt.outcome.clone()),
        last_ok: Some(true),
        error_total: Some(attempt.error_total),
        resolve_error: None,
    }
}

/// Compute a target's scrub health, folding in config applicability
/// (`[scrub].enabled`, whether this target's label is in `[scrub].targets`)
/// and UUID resolution ahead of [`scrub_health_for`].
fn target_scrub_health(
    config: &Config,
    target: &Target,
    state: Option<&scrub::ScrubState>,
    now_epoch: i64,
) -> ScrubHealth {
    if !config.scrub.enabled || !config.scrub.targets.iter().any(|l| l == &target.label) {
        return ScrubHealth::not_applicable();
    }
    match scrub::resolve_target_fsuuid(config, &target.label) {
        Ok(fsuuid) => {
            let fs = state.and_then(|s| s.filesystems.get(&fsuuid));
            let mut health = scrub_health_for(
                fs,
                now_epoch,
                config.scrub.warn_age_days,
                config.scrub.fail_age_days,
            );
            health.fsuuid = Some(fsuuid);
            health
        }
        Err(e) => ScrubHealth::unresolved(e),
    }
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

/// Find where a device partition is currently mounted, regardless of path.
///
/// Resolves the target's serial to a block device, determines its partition
/// based on role, then scans `/proc/mounts` for that device.  Returns the
/// actual mount path (which may be `/run/media/…` from udisks2, or the
/// configured `/mnt/backup-…`, or wherever the kernel says it is).
pub fn find_mount_for_device(serial: &str, role: &TargetRole) -> Option<String> {
    let dev = device_from_serial(serial)?;
    let part = crate::mount::partition_device(&dev, role);
    // Canonicalize the partition device so we match /proc/mounts entries that
    // use the real path (e.g. /dev/sdk1) rather than a symlink.
    let canonical_part = std::fs::canonicalize(&part)
        .ok()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or(part);

    let mounts = fs::read_to_string("/proc/mounts").ok()?;
    for line in mounts.lines() {
        let mut cols = line.splitn(3, ' ');
        if let (Some(device), Some(mountpoint)) = (cols.next(), cols.next())
            && device == canonical_part
        {
            return Some(mountpoint.to_string());
        }
    }
    None
}

/// Find the effective mount path for a target, checking both the configured
/// path and any alternate mount (e.g. udisks2 at `/run/media/…`).
///
/// Returns `Some(path)` where the target is actually mounted, or `None`.
pub fn find_any_mount(configured_path: &str, serial: &str, role: &TargetRole) -> Option<String> {
    // 1. Prefer the configured path if it's an active mount point.
    let cfg_path = Path::new(configured_path);
    if cfg_path.exists() && is_mountpoint(cfg_path) {
        return Some(configured_path.to_string());
    }
    // 2. Fall back to scanning /proc/mounts by device.
    find_mount_for_device(serial, role)
}

/// Measure `(total_bytes, used_bytes)` for a mounted target.
///
/// `btrfs filesystem usage --raw` first (it accounts for RAID profiles, which
/// statvfs does not), then `statvfs(2)`. Returns `None` when neither can
/// answer — never `(0, 0)`, which is a real reading a caller cannot tell from a
/// missing one (bd DAS-Backup-Manager-8wx).
pub fn measure_target_usage(mount: &str) -> Option<(u64, u64)> {
    let btrfs_output = std::process::Command::new("btrfs")
        .args(["filesystem", "usage", "--raw", mount])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok());

    match btrfs_output {
        Some(output) => parse_btrfs_usage(&output).or_else(|| disk_space_statvfs(mount)),
        None => disk_space_statvfs(mount),
    }
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
/// - Critical: any target with SMART "FAILED", usage > 95 %, or scrub FAIL
///   (aborted/errored latest attempt, or age past `fail_age_days`)
/// - Warning: any target with usage > 85 %, SMART unavailable, unmounted,
///   or scrub WARN/never-scrubbed/unresolved
/// - Healthy: everything else
fn determine_status(targets: &[TargetHealth], warnings: &[String]) -> HealthStatus {
    for t in targets {
        if t.smart_status.as_deref() == Some("FAILED") {
            return HealthStatus::Critical;
        }
        if t.total_bytes > 0 && t.usage_percent() > 95.0 {
            return HealthStatus::Critical;
        }
        if t.scrub.status == ScrubHealthStatus::Fail {
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
    let mut effective_mounts: Vec<String> = Vec::new();

    // Load the scrub engine's persisted state once, up front, for all
    // targets — see `ScrubHealth` doc comment for why this (and only this)
    // is the source for scrub health. A missing file is normal (no pass has
    // run yet) and yields the default empty state, not an error; only a
    // present-but-unreadable/corrupt file produces a warning here, and only
    // once for the whole report rather than once per target.
    let scrub_state = if config.scrub.enabled {
        match scrub::load_state() {
            Ok(s) => Some(s),
            Err(e) => {
                warnings.push(format!("Scrub state unreadable: {e}"));
                None
            }
        }
    } else {
        None
    };
    let now_epoch = std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    for target in &config.targets {
        // 1. Check mount status — detect both configured path and udisks2 mounts
        let actual_mount = find_any_mount(&target.mount, &target.serial, &target.role);
        let mounted = actual_mount.is_some();
        let effective_path = actual_mount.as_deref().unwrap_or(&target.mount).to_string();
        effective_mounts.push(effective_path.clone());

        if !mounted {
            warnings.push(format!(
                "Target '{}' (mount: {}) is not mounted",
                target.label, target.mount
            ));
        }

        // 2. Get disk space (using effective mount path)
        let usage = if mounted {
            measure_target_usage(&effective_path)
        } else {
            None
        };
        // A mounted target whose capacity could not be read is NOT a target at
        // 0 % — but that is exactly how it used to be reported, because both
        // fallbacks ended in `.unwrap_or((0, 0))`. `determine_status` skips the
        // >95 %-full escalation when `total_bytes == 0`, so the one target whose
        // usage nobody could measure was also the one target that could never
        // raise the disk-full alarm, and the GUI drew it as 0 % used
        // (bd DAS-Backup-Manager-8wx).
        if mounted && usage.is_none() {
            warnings.push(format!(
                "Target '{}': mounted at '{}' but its capacity could not be measured                  (`btrfs filesystem usage` and statvfs(2) both failed) — the disk-full                  check is DISABLED for this target and its usage is reported as unknown",
                target.label, effective_path
            ));
        }
        let (total_bytes, used_bytes) = usage.unwrap_or((0, 0));

        // 3. Get snapshot count (using effective mount path)
        let snapshot_count = if mounted {
            count_snapshots(&effective_path)
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

        // 5b. Scrub health — independent of mount state, since the engine's
        // state file (unlike disk usage / SMART) is readable whether or not
        // the DAS filesystem is currently mounted.
        let scrub_health = target_scrub_health(config, target, scrub_state.as_ref(), now_epoch);
        match scrub_health.status {
            ScrubHealthStatus::NeverScrubbed => warnings.push(format!(
                "Target '{}': never scrubbed — no successful scrub recorded yet",
                target.label
            )),
            ScrubHealthStatus::Warn => warnings.push(format!(
                "Target '{}': last successful scrub is {} day(s) old (warn threshold {}d)",
                target.label,
                scrub_health.age_days.unwrap_or(0),
                config.scrub.warn_age_days
            )),
            ScrubHealthStatus::Fail => warnings.push(format!(
                "Target '{}': scrub FAILED — last attempt outcome '{}'{}",
                target.label,
                scrub_health.last_outcome.as_deref().unwrap_or("unknown"),
                match scrub_health.age_days {
                    Some(age) => format!(
                        ", last known-good scrub {age} day(s) old (fail threshold {}d)",
                        config.scrub.fail_age_days
                    ),
                    None => " (never a successful scrub)".to_string(),
                }
            )),
            ScrubHealthStatus::Unresolved => warnings.push(format!(
                "Target '{}': cannot resolve filesystem UUID for scrub health: {}",
                target.label,
                scrub_health
                    .resolve_error
                    .as_deref()
                    .unwrap_or("unknown error")
            )),
            ScrubHealthStatus::Ok | ScrubHealthStatus::NotApplicable => {}
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
            scrub: scrub_health,
        });
    }

    // 6. Parse growth log — map mount paths to target labels
    //    Include both configured and effective mount paths for matching.
    let mut mount_to_label: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    for (target, eff_mount) in config.targets.iter().zip(effective_mounts.iter()) {
        mount_to_label.insert(target.mount.clone(), target.label.clone());
        if eff_mount != &target.mount {
            mount_to_label.insert(eff_mount.clone(), target.label.clone());
        }
    }
    let growth_points = fs::read_to_string(&config.general.growth_log)
        .map(|content| {
            let mut pts = parse_growth_log(&content);
            for pt in &mut pts {
                if let Some(label) = mount_to_label.get(&pt.target_label) {
                    pt.target_label = label.clone();
                }
            }
            pts
        })
        .unwrap_or_default();

    // 7. Determine overall status
    let status = determine_status(&target_healths, &warnings);

    // 8. Last backup time (use effective mount paths)
    let last_backup = latest_snapshot_time(&target_healths, &effective_mounts);

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
            scrub: ScrubHealth::not_applicable(),
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
            scrub: ScrubHealth::not_applicable(),
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
                    1709000000 system-recovery-B-2tb 1073741824\n";
        let points = parse_growth_log(log);
        assert_eq!(points.len(), 3);
        assert_eq!(points[0].timestamp, 1709000000);
        assert_eq!(points[0].target_label, "primary-22tb");
        assert_eq!(points[0].used_bytes, 5368709120);
        assert_eq!(points[2].target_label, "system-recovery-B-2tb");
    }

    #[test]
    fn parse_growth_log_iso_timestamps() {
        let log = "2026-02-20T07:39:42 /mnt/backup-22tb 1861347422208\n\
                   2026-02-20T07:39:42 /mnt/backup-system-recovery-B 871137460224\n";
        let points = parse_growth_log(log);
        assert_eq!(points.len(), 2);
        assert!(points[0].timestamp > 1_700_000_000);
        assert_eq!(points[0].target_label, "/mnt/backup-22tb");
        assert_eq!(points[0].used_bytes, 1861347422208);
        assert_eq!(points[1].target_label, "/mnt/backup-system-recovery-B");
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

    /// A path that cannot be measured must report `None`, not `Some((0, 0))`.
    /// The fabricated zero is what let an unmeasurable mounted target sail past
    /// `determine_status`'s `total_bytes > 0` guard and render as 0 % used.
    #[test]
    fn measure_target_usage_returns_none_when_nothing_can_measure_it() {
        // Neither `btrfs filesystem usage` nor statvfs(2) can answer for a path
        // that does not exist.
        let missing = "/nonexistent-das-backup-manager-8wx/target";
        assert_eq!(
            measure_target_usage(missing),
            None,
            "an unmeasurable path must be None, never a fabricated (0, 0)"
        );

        // Positive control: a path that DOES exist still yields a real reading,
        // so the function cannot be passing by returning None for everything.
        let (total, used) = measure_target_usage("/").expect("/ must be measurable via statvfs");
        assert!(total > 0, "positive control: / reported total_bytes == 0");
        assert!(used <= total, "used {used} exceeds total {total}");
    }

    /// The consequence the None above exists to prevent: a target reporting
    /// `total_bytes == 0` is skipped by the disk-full escalation, so without an
    /// accompanying warning an unmeasurable target reads as perfectly Healthy.
    #[test]
    fn unmeasurable_target_is_not_silently_healthy() {
        let mut t = base_target_health(ScrubHealth::not_applicable());
        t.mounted = true;
        t.total_bytes = 0;
        t.used_bytes = 0;
        assert_eq!(
            determine_status(std::slice::from_ref(&t), &[]),
            HealthStatus::Healthy,
            "documents the gap: total_bytes == 0 cannot trip the >95% check"
        );
        let warnings =
            vec!["Target 't1': mounted but its capacity could not be measured".to_string()];
        assert_eq!(
            determine_status(&[t], &warnings),
            HealthStatus::Warning,
            "the warning is the only thing that keeps an unmeasurable target visible"
        );
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
                scrub: ScrubHealth::not_applicable(),
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
                scrub: ScrubHealth::not_applicable(),
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
            scrub: ScrubHealth::not_applicable(),
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
            scrub: ScrubHealth::not_applicable(),
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
            scrub: ScrubHealth::not_applicable(),
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

    // --- scrub health (bd DAS-Backup-Manager-5kb) ---

    const NOW: i64 = 1_800_000_000; // fixed "now" so age-day tests are exact

    /// Build a clean, `finished`, zero-error `FsState` whose last success was
    /// `age_days` ago (relative to [`NOW`]).
    fn fs_state_clean(age_days: i64) -> scrub::FsState {
        let success_epoch = NOW - age_days * 86_400;
        scrub::FsState {
            target_label: "t1".into(),
            mountpoint: "/mnt/t1".into(),
            last_success_epoch: Some(success_epoch),
            last_attempt: scrub::AttemptState {
                outcome: "finished".into(),
                ok: true,
                started_epoch: success_epoch - 6000,
                finished_epoch: success_epoch,
                duration_secs: 6000,
                bytes_scrubbed: 1_000_000_000,
                error_total: 0,
                counters: scrub::ScrubCounters::default(),
                messages: Vec::new(),
            },
        }
    }

    /// Build an `FsState` whose *latest* attempt is bad (`ok: false`), with
    /// an optional carried-forward `last_success_epoch` from an earlier
    /// clean pass (`prior_success_age_days` ago), mirroring what
    /// `merge_pass_into_state` produces for a failed pass following a good
    /// one.
    fn fs_state_bad_latest(
        outcome: &str,
        error_total: u64,
        prior_success_age_days: Option<i64>,
    ) -> scrub::FsState {
        let latest_epoch = NOW - 86_400; // latest (bad) attempt: yesterday
        scrub::FsState {
            target_label: "t1".into(),
            mountpoint: "/mnt/t1".into(),
            last_success_epoch: prior_success_age_days.map(|d| NOW - d * 86_400),
            last_attempt: scrub::AttemptState {
                outcome: outcome.into(),
                ok: false,
                started_epoch: latest_epoch - 3000,
                finished_epoch: latest_epoch,
                duration_secs: 3000,
                bytes_scrubbed: 500_000_000,
                error_total,
                counters: scrub::ScrubCounters::default(),
                messages: Vec::new(),
            },
        }
    }

    #[test]
    fn scrub_health_never_scrubbed_when_no_state_entry() {
        let h = scrub_health_for(None, NOW, 45, 75);
        assert_eq!(h.status, ScrubHealthStatus::NeverScrubbed);
        assert_eq!(h.age_days, None);
        assert_eq!(h.last_success_epoch, None);
    }

    #[test]
    fn scrub_health_warn_age_boundary_44_still_ok() {
        let fs = fs_state_clean(44);
        let h = scrub_health_for(Some(&fs), NOW, 45, 75);
        assert_eq!(h.status, ScrubHealthStatus::Ok);
        assert_eq!(h.age_days, Some(44));
    }

    #[test]
    fn scrub_health_warn_age_boundary_45_still_ok() {
        // Requirement text: "WARN when age ... > warn_age_days" — age equal
        // to the threshold is not yet a warning.
        let fs = fs_state_clean(45);
        let h = scrub_health_for(Some(&fs), NOW, 45, 75);
        assert_eq!(h.status, ScrubHealthStatus::Ok);
        assert_eq!(h.age_days, Some(45));
    }

    #[test]
    fn scrub_health_warn_age_boundary_46_crosses_to_warn() {
        let fs = fs_state_clean(46);
        let h = scrub_health_for(Some(&fs), NOW, 45, 75);
        assert_eq!(h.status, ScrubHealthStatus::Warn);
        assert_eq!(h.age_days, Some(46));
    }

    #[test]
    fn scrub_health_fail_age_boundary_74_still_warn() {
        let fs = fs_state_clean(74);
        let h = scrub_health_for(Some(&fs), NOW, 45, 75);
        assert_eq!(h.status, ScrubHealthStatus::Warn);
        assert_eq!(h.age_days, Some(74));
    }

    #[test]
    fn scrub_health_fail_age_boundary_75_still_warn() {
        let fs = fs_state_clean(75);
        let h = scrub_health_for(Some(&fs), NOW, 45, 75);
        assert_eq!(h.status, ScrubHealthStatus::Warn);
        assert_eq!(h.age_days, Some(75));
    }

    #[test]
    fn scrub_health_fail_age_boundary_76_crosses_to_fail() {
        let fs = fs_state_clean(76);
        let h = scrub_health_for(Some(&fs), NOW, 45, 75);
        assert_eq!(h.status, ScrubHealthStatus::Fail);
        assert_eq!(h.age_days, Some(76));
    }

    #[test]
    fn scrub_health_aborted_is_immediate_fail_regardless_of_age() {
        // Motivating case: recovery-A sat 64 days with an aborted super=3
        // scrub that looked fine. Even a *recent* aborted attempt (well
        // inside the warn window) must fail immediately.
        let fs = fs_state_bad_latest("aborted", 0, Some(5));
        let h = scrub_health_for(Some(&fs), NOW, 45, 75);
        assert_eq!(h.status, ScrubHealthStatus::Fail);
        assert_eq!(h.last_outcome.as_deref(), Some("aborted"));
        assert_eq!(h.last_ok, Some(false));
    }

    #[test]
    fn scrub_health_nonzero_errors_is_immediate_fail_regardless_of_age() {
        // A "finished" outcome with nonzero error counters is still an
        // immediate fail — outcome alone is not sufficient, per the
        // requirement ("Any nonzero error counter ... = IMMEDIATE FAIL").
        let fs = fs_state_bad_latest("finished", 3, Some(2));
        let h = scrub_health_for(Some(&fs), NOW, 45, 75);
        assert_eq!(h.status, ScrubHealthStatus::Fail);
        assert_eq!(h.error_total, Some(3));
    }

    #[test]
    fn scrub_health_aborted_does_not_reset_age() {
        // A finished record 10 days old, followed by an aborted attempt
        // yesterday: the reported age must still reflect the *finished*
        // record (10 days), never reset to the aborted attempt's date —
        // this is `last_success_epoch`'s carried-forward contract.
        let fs = fs_state_bad_latest("aborted", 0, Some(10));
        let h = scrub_health_for(Some(&fs), NOW, 45, 75);
        assert_eq!(h.status, ScrubHealthStatus::Fail);
        assert_eq!(h.age_days, Some(10));
        assert_eq!(h.last_success_epoch, Some(NOW - 10 * 86_400));
    }

    #[test]
    fn scrub_health_bad_latest_with_no_prior_success_reports_no_age() {
        let fs = fs_state_bad_latest("error", 0, None);
        let h = scrub_health_for(Some(&fs), NOW, 45, 75);
        assert_eq!(h.status, ScrubHealthStatus::Fail);
        assert_eq!(h.age_days, None);
        assert_eq!(h.last_success_epoch, None);
    }

    #[test]
    fn scrub_health_ok_clean_records_last_ok_true() {
        let fs = fs_state_clean(1);
        let h = scrub_health_for(Some(&fs), NOW, 45, 75);
        assert_eq!(h.last_ok, Some(true));
        assert_eq!(h.last_outcome.as_deref(), Some("finished"));
        assert_eq!(h.error_total, Some(0));
    }

    #[test]
    fn scrub_health_not_applicable_constructor() {
        let h = ScrubHealth::not_applicable();
        assert_eq!(h.status, ScrubHealthStatus::NotApplicable);
        assert_eq!(h.fsuuid, None);
    }

    #[test]
    fn scrub_health_unresolved_constructor_carries_error() {
        let h = ScrubHealth::unresolved("no serial".to_string());
        assert_eq!(h.status, ScrubHealthStatus::Unresolved);
        assert_eq!(h.resolve_error.as_deref(), Some("no serial"));
    }

    fn base_target_health(scrub: ScrubHealth) -> TargetHealth {
        TargetHealth {
            label: "t1".into(),
            serial: "S1".into(),
            mounted: true,
            total_bytes: 1_000_000_000,
            used_bytes: 100_000_000, // 10% — fine on its own
            snapshot_count: 10,
            smart_status: Some("PASSED".into()),
            temperature_c: None,
            power_on_hours: None,
            errors: None,
            scrub,
        }
    }

    #[test]
    fn determine_status_scrub_fail_is_critical() {
        let fs = fs_state_clean(76);
        let scrub = scrub_health_for(Some(&fs), NOW, 45, 75);
        assert_eq!(scrub.status, ScrubHealthStatus::Fail);
        let targets = vec![base_target_health(scrub)];
        let warnings = vec!["Target 't1': scrub FAILED".to_string()];
        assert_eq!(
            determine_status(&targets, &warnings),
            HealthStatus::Critical
        );
    }

    #[test]
    fn determine_status_scrub_warn_is_warning_not_critical() {
        let fs = fs_state_clean(46);
        let scrub = scrub_health_for(Some(&fs), NOW, 45, 75);
        assert_eq!(scrub.status, ScrubHealthStatus::Warn);
        let targets = vec![base_target_health(scrub)];
        let warnings = vec!["Target 't1': last successful scrub is 46 day(s) old".to_string()];
        assert_eq!(determine_status(&targets, &warnings), HealthStatus::Warning);
    }

    #[test]
    fn determine_status_scrub_not_applicable_stays_healthy() {
        let targets = vec![base_target_health(ScrubHealth::not_applicable())];
        let warnings: Vec<String> = vec![];
        assert_eq!(determine_status(&targets, &warnings), HealthStatus::Healthy);
    }

    /// Minimal target with an explicit `mount_uuid`, mirroring the
    /// `test_target` helper in `main.rs`'s scrub CLI tests — `resolve_target_fsuuid`
    /// prefers `mount_uuid` and never has to touch a real device for it.
    fn scrub_test_target(label: &str, mount_uuid: &str) -> Target {
        Target {
            label: label.to_string(),
            serial: String::new(),
            serials: Vec::new(),
            mount_uuid: Some(mount_uuid.to_string()),
            mount: format!("/mnt/{label}"),
            role: crate::config::TargetRole::Primary,
            retention: crate::config::Retention::default(),
            display_name: label.to_string(),
        }
    }

    #[test]
    fn target_scrub_health_not_applicable_when_scrub_disabled() {
        let mut config = Config::default();
        config.scrub.enabled = false;
        let target = scrub_test_target("primary-22tb", "11111111-1111-1111-1111-111111111111");
        let h = target_scrub_health(&config, &target, None, NOW);
        assert_eq!(h.status, ScrubHealthStatus::NotApplicable);
    }

    #[test]
    fn target_scrub_health_not_applicable_when_label_not_a_scrub_target() {
        let mut config = Config::default();
        config.scrub.enabled = true;
        config.scrub.targets = vec!["primary-22tb".into()];
        let target = scrub_test_target("some-other-target", "11111111-1111-1111-1111-111111111111");
        let h = target_scrub_health(&config, &target, None, NOW);
        assert_eq!(h.status, ScrubHealthStatus::NotApplicable);
    }

    #[test]
    fn target_scrub_health_never_scrubbed_when_state_empty() {
        let mut config = Config::default();
        config.scrub.enabled = true;
        config.scrub.targets = vec!["primary-22tb".into()];
        let target = scrub_test_target("primary-22tb", "11111111-1111-1111-1111-111111111111");
        config.targets = vec![target.clone()];
        let state = scrub::ScrubState::default();
        let h = target_scrub_health(&config, &target, Some(&state), NOW);
        assert_eq!(h.status, ScrubHealthStatus::NeverScrubbed);
    }

    #[test]
    fn target_scrub_health_ok_when_state_has_recent_clean_entry() {
        let mut config = Config::default();
        config.scrub.enabled = true;
        config.scrub.targets = vec!["primary-22tb".into()];
        config.scrub.warn_age_days = 45;
        config.scrub.fail_age_days = 75;
        let fsuuid = "11111111-1111-1111-1111-111111111111".to_string();
        let target = scrub_test_target("primary-22tb", &fsuuid);
        config.targets = vec![target.clone()];
        let mut state = scrub::ScrubState::default();
        state.filesystems.insert(fsuuid.clone(), fs_state_clean(5));
        let h = target_scrub_health(&config, &target, Some(&state), NOW);
        assert_eq!(h.status, ScrubHealthStatus::Ok);
        assert_eq!(h.fsuuid.as_deref(), Some(fsuuid.as_str()));
        assert_eq!(h.age_days, Some(5));
    }
}
