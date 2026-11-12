use crate::config::{Config, Target, TargetRole};
use crate::scrub;
use regex::Regex;
use std::fs;
use std::path::Path;
use std::sync::LazyLock;
use std::time::{Duration, Instant, UNIX_EPOCH};

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
    /// Whether `total_bytes`/`used_bytes` were measured. When not, both are
    /// 0 for the JSON consumers, and a report must say "unknown" rather than
    /// print the zeros as a reading (fail-silent.md "always a defect" #1).
    pub usage_known: bool,
    /// Snapshots counted on the target; `None` when it was not read (not
    /// mounted, unreadable, or held by a job with no kept reading), never 0
    /// (fail-silent.md "always a defect" #1; bd DAS-Backup-Manager-5237.16).
    pub snapshot_count: Option<usize>,
    /// Age of `snapshot_count` when it is a kept reading served because a job
    /// held the targets; `None` when it was read for this report.
    pub snapshot_reading_age_secs: Option<u64>,
    pub smart_status: Option<String>,
    pub temperature_c: Option<i32>,
    pub power_on_hours: Option<u64>,
    pub errors: Option<u64>,
    pub scrub: ScrubHealth,
    /// Every configured drive of the target, in `serials` order: a RAID-1
    /// pair has two, so a missing leg is a member with `present: false`, not
    /// a drive nobody lists (bd DAS-Backup-Manager-uhi). The fields above
    /// carry the first present drive's SMART reading, for the JSON consumers.
    pub members: Vec<MemberHealth>,
}

/// One configured drive of a target and its SMART reading.
#[derive(Debug, Clone, Default)]
pub struct MemberHealth {
    pub serial: String,
    /// A block device with this serial was found.
    pub present: bool,
    pub smart_status: Option<String>,
    pub temperature_c: Option<i32>,
    pub power_on_hours: Option<u64>,
}

/// The members of a target with `serials`, each looked up by `device_of`
/// (`(device, is_usb)`) and read by `smart_of`, beside the SMART details of
/// the first present one (for [`TargetHealth`]'s single-drive fields).
pub fn members_with(
    serials: &[String],
    device_of: &dyn Fn(&str) -> Option<(String, bool)>,
    smart_of: &dyn Fn(&str, bool) -> Option<SmartDetails>,
) -> (Vec<MemberHealth>, Option<SmartDetails>) {
    let mut members = Vec::new();
    let mut first = None;
    for serial in serials.iter().filter(|s| !s.is_empty()) {
        let Some((dev, is_usb)) = device_of(serial) else {
            members.push(MemberHealth {
                serial: serial.clone(),
                ..MemberHealth::default()
            });
            continue;
        };
        let details = smart_of(&dev, is_usb);
        members.push(MemberHealth {
            serial: serial.clone(),
            present: true,
            smart_status: details.as_ref().map(|d| d.status.clone()),
            temperature_c: details.as_ref().and_then(|d| d.temperature_c),
            power_on_hours: details.as_ref().and_then(|d| d.power_on_hours),
        });
        if first.is_none() {
            first = details;
        }
    }
    (members, first)
}

/// The warning for a target some of whose drives are absent while another is
/// present: a RAID-1 running degraded on its surviving leg. `None` when every
/// drive is present, or none is (the not-mounted warning covers that).
pub fn degraded_warning(label: &str, members: &[MemberHealth]) -> Option<String> {
    let absent: Vec<&str> = members
        .iter()
        .filter(|m| !m.present)
        .map(|m| m.serial.as_str())
        .collect();
    let present: Vec<&str> = members
        .iter()
        .filter(|m| m.present)
        .map(|m| m.serial.as_str())
        .collect();
    if absent.is_empty() || present.is_empty() {
        return None;
    }
    Some(format!(
        "Target '{label}': RAID-1 degraded — {} absent, running on {}",
        absent.join(", "),
        present.join(", ")
    ))
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
///    this feature exists to fix: a recovery target's aborted scrub
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
    if !config.scrub.enabled || !config.scrub_target_labels().contains(&target.label) {
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
    /// Age of the oldest kept reading `last_backup` rests on; `None` when
    /// every target that answered was read for this report.
    pub last_backup_reading_age_secs: Option<u64>,
    /// A backup, scrub or other job held the targets, so their snapshot
    /// directories were not walked (bd DAS-Backup-Manager-5237.16).
    pub targets_busy: bool,
    pub growth_points: Vec<GrowthPoint>,
    pub warnings: Vec<String>,
    /// One line per `role = "mirror"` target's OS (bd DAS-Backup-Manager-xd3),
    /// with its enabled timers. A stale, unreadable or never-checked one, and
    /// one whose boot may run btrbk (bd DAS-Backup-Manager-1yg), is also in
    /// `warnings`.
    pub recovery_os: Vec<String>,
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

/// The block device holding the filesystem whose UUID is `uuid`, if this host
/// has one — `blkid -U`, the same question `backup-run.sh` asks. This looks
/// up ONE identity named in the config; it never lists filesystems. For a
/// multi-device BTRFS filesystem any one member answers. `None` when the UUID
/// is empty, nothing has it, or `blkid` cannot answer — the target is then
/// not available, which is the cautious reading.
pub fn device_for_fs_uuid(uuid: &str) -> Option<String> {
    device_for_fs_uuid_with(&crate::fsutil::SystemRunner, uuid)
}

/// [`device_for_fs_uuid`] with `blkid` run by `runner`.
pub fn device_for_fs_uuid_with(
    runner: &dyn crate::fsutil::CommandRunner,
    uuid: &str,
) -> Option<String> {
    if uuid.is_empty() {
        return None;
    }
    let out = runner
        .output(std::process::Command::new("blkid").args(["-U", uuid]))
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let dev = String::from_utf8_lossy(&out.stdout).trim().to_string();
    dev.starts_with("/dev/").then_some(dev)
}

/// The UUID of the BTRFS filesystem on the partition a target of `role` uses
/// on the drive with `serial` — what `btrdasd setup` records as a new
/// target's `mount_uuid`. One lookup by the identities the operator typed;
/// `None` when the drive is absent or that partition is not BTRFS (a
/// recovery drive's partition 1 is its own ESP and is never offered).
pub fn btrfs_uuid_for_serial(serial: &str, role: &TargetRole) -> Option<String> {
    btrfs_uuid_for_serial_with(
        &crate::fsutil::SystemRunner,
        &device_from_serial,
        serial,
        role,
    )
}

/// [`btrfs_uuid_for_serial`] with the drive lookup and `blkid` supplied.
pub fn btrfs_uuid_for_serial_with(
    runner: &dyn crate::fsutil::CommandRunner,
    device_from_serial: &dyn Fn(&str) -> Option<String>,
    serial: &str,
    role: &TargetRole,
) -> Option<String> {
    let dev = device_from_serial(serial)?;
    btrfs_uuid_of_with(runner, &crate::mount::partition_device(&dev, role))
}

/// The filesystem UUID of `device` if it holds BTRFS, per `blkid -o export`.
pub fn btrfs_uuid_of_with(
    runner: &dyn crate::fsutil::CommandRunner,
    device: &str,
) -> Option<String> {
    let out = runner
        .output(std::process::Command::new("blkid").args(["-o", "export", device]))
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let field = |key: &str| {
        text.lines()
            .find_map(|l| l.strip_prefix(key).and_then(|v| v.strip_prefix('=')))
    };
    (field("TYPE") == Some("btrfs"))
        .then(|| field("UUID"))
        .flatten()
        .filter(|u| !u.is_empty())
        .map(str::to_string)
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
/// based on role, then scans `/proc/self/mountinfo` for that device.  Returns the
/// actual mount path (which may be `/run/media/…` from udisks2, or the
/// configured `/mnt/backup-…`, or wherever the kernel says it is).
pub fn find_mount_for_device(serial: &str, role: &TargetRole) -> Option<String> {
    let mounts = fs::read_to_string("/proc/self/mountinfo").ok()?;
    find_mount_for_device_with(&device_from_serial, &mounts, serial, role)
}

/// [`find_mount_for_device`] with the serial lookup and the
/// `/proc/self/mountinfo` text supplied.
pub fn find_mount_for_device_with(
    dev_of_serial: &dyn Fn(&str) -> Option<String>,
    mounts_text: &str,
    serial: &str,
    role: &TargetRole,
) -> Option<String> {
    let dev = dev_of_serial(serial)?;
    let part = crate::mount::partition_device(&dev, role);
    // Canonicalize the partition device so we match mountinfo entries that
    // use the real path (e.g. /dev/sdk1) rather than a symlink.
    let canonical_part = std::fs::canonicalize(&part)
        .ok()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or(part);
    top_level_mount_of(mounts_text, &canonical_part)
}

/// The first mount of `device` in `/proc/self/mountinfo` text that shows the
/// filesystem's TOP LEVEL, never a subvolume of it nor a directory within it.
/// Binding a subvolume mount (a booted recovery drive's `/`, subvolume `@`) at a
/// target path would put a different tree where the target's snapshots are
/// expected. A mount qualifies only when its root (field 4) is `/`:
/// `/proc/mounts` cannot tell a bind of a subdirectory of the top level from
/// the top level itself — both read `subvolid=5` (bd DAS-Backup-Manager-71q1).
fn top_level_mount_of(mountinfo: &str, device: &str) -> Option<String> {
    for line in mountinfo.lines() {
        // `<id> <parent> <maj:min> <root> <mount point> <options> [optional…]
        // - <fstype> <source> <super options>`
        let Some((mine, theirs)) = line.split_once(" - ") else {
            continue;
        };
        let mut left = mine.split(' ');
        let (Some(root), Some(mount_point)) = (left.nth(3), left.next()) else {
            continue;
        };
        if theirs.split(' ').nth(1) == Some(device) && root == "/" {
            return Some(mount_point.to_string());
        }
    }
    None
}

/// `(filesystem UUID, FSROOT)` of the mount at `path`: what is mounted there
/// and which part of it (`/` is the top level; `/@` is a subvolume).
pub fn mount_uuid_fsroot(path: &Path) -> Option<(String, String)> {
    mount_uuid_fsroot_with(&crate::fsutil::SystemRunner, path)
}

/// [`mount_uuid_fsroot`] with `findmnt` run by `runner`. `None` when it fails
/// or does not print exactly two fields (an empty UUID prints one) — the
/// caller reads that as "not verified". Stacked mounts print several lines;
/// the last is the one on top.
pub fn mount_uuid_fsroot_with(
    runner: &dyn crate::fsutil::CommandRunner,
    path: &Path,
) -> Option<(String, String)> {
    let out = runner
        .output(
            std::process::Command::new("findmnt")
                .args(["-no", "UUID,FSROOT"])
                .arg(path),
        )
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let line = text.lines().rev().find(|l| !l.trim().is_empty())?;
    let mut f = line.split_whitespace();
    let (uuid, fsroot) = (f.next()?, f.next()?);
    f.next()
        .is_none()
        .then(|| (uuid.to_string(), fsroot.to_string()))
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
    // 2. Fall back to scanning /proc/self/mountinfo by device.
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

/// What one pass over a target's snapshot directories found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SnapshotReading {
    /// Directories named as btrbk names a snapshot, one level below the
    /// target's top level.
    pub count: usize,
    /// The newest modification time among them, seconds since the epoch.
    pub newest_epoch: Option<u64>,
}

/// Walk `mount` once for both numbers: the snapshot count and the newest
/// snapshot's time (two walks before bd DAS-Backup-Manager-5237.16). `None`
/// when the target's top level cannot be listed — not a count of 0.
pub fn read_snapshots(mount: &str) -> Option<SnapshotReading> {
    let source_entries = fs::read_dir(Path::new(mount)).ok()?;
    let mut reading = SnapshotReading {
        count: 0,
        newest_epoch: None,
    };
    // One level of subdirectories (source dirs like "nvme", "ssd").
    for source_entry in source_entries.flatten() {
        if !source_entry
            .file_type()
            .is_ok_and(|ft| ft.is_dir() || ft.is_symlink())
        {
            continue;
        }
        let Ok(snap_entries) = fs::read_dir(source_entry.path()) else {
            continue;
        };
        for snap_entry in snap_entries.flatten() {
            let name = snap_entry.file_name().to_string_lossy().to_string();
            if !SNAP_RE.is_match(&name) {
                continue;
            }
            reading.count += 1;
            let modified = snap_entry
                .metadata()
                .ok()
                .and_then(|meta| meta.modified().ok())
                .and_then(|m| m.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs());
            if let Some(secs) = modified
                && reading.newest_epoch.is_none_or(|prev| secs > prev)
            {
                reading.newest_epoch = Some(secs);
            }
        }
    }
    Some(reading)
}

/// A target's snapshot numbers as a health report gets them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SnapshotAnswer {
    /// `None`: unknown.
    pub reading: Option<SnapshotReading>,
    /// `Some(age)`: `reading` is a kept one, taken that long ago.
    pub kept_age_secs: Option<u64>,
}

/// The last snapshot reading taken of each target, by label, for a process
/// that answers health queries while jobs run (the GUI's helper). While a job
/// holds the targets a directory read on a filesystem btrbk is receiving
/// into waits for minutes (443 s measured, bd DAS-Backup-Manager-5237.16), so
/// the kept reading is served, with its age, instead of a walk.
#[derive(Debug, Default)]
pub struct KeptSnapshots(
    std::sync::Mutex<std::collections::HashMap<String, (SnapshotReading, u64)>>,
);

impl KeptSnapshots {
    /// The answer for the target `label` mounted at `mount`: when `busy`, the
    /// kept reading (or unknown) and `walk` is never called; otherwise
    /// `walk`'s reading, which is kept when it is one.
    pub fn answer(
        &self,
        label: &str,
        mount: &str,
        busy: bool,
        now_epoch: u64,
        walk: &dyn Fn(&str) -> Option<SnapshotReading>,
    ) -> SnapshotAnswer {
        // A poisoned map still holds whole readings: each is one insert.
        let kept = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if busy {
            return match kept.get(label) {
                Some((reading, taken_epoch)) => SnapshotAnswer {
                    reading: Some(*reading),
                    kept_age_secs: Some(now_epoch.saturating_sub(*taken_epoch)),
                },
                None => SnapshotAnswer {
                    reading: None,
                    kept_age_secs: None,
                },
            };
        }
        // Not held across the walk: another target's answer must not wait on
        // this one's directory reads.
        drop(kept);
        let reading = walk(mount);
        if let Some(reading) = reading {
            self.0
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(label.to_string(), (reading, now_epoch));
        }
        SnapshotAnswer {
            reading,
            kept_age_secs: None,
        }
    }
}

/// Whether some process holds a `flock` on the file with this device and
/// inode, read from the kernel's lock table (`/proc/locks` text) without
/// touching the lock: a probing `flock` would make a backup's own
/// non-blocking attempt find it held and skip the run.
pub fn flock_listed(proc_locks: &str, dev: u64, ino: u64) -> bool {
    // `<n>: FLOCK  ADVISORY  WRITE <pid> <major>:<minor>:<inode> <start> <end>`,
    // major and minor in hex; a waiter's line has `->` after the number.
    proc_locks.lines().any(|line| {
        let mut fields = line.split_whitespace().skip(1);
        if fields.next() != Some("FLOCK") {
            return false;
        }
        let Some(id) = fields.nth(3) else {
            return false;
        };
        let mut parts = id.split(':');
        let (Some(major), Some(minor), Some(inode), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return false;
        };
        u32::from_str_radix(major, 16).ok() == Some(libc::major(dev))
            && u32::from_str_radix(minor, 16).ok() == Some(libc::minor(dev))
            && inode.parse::<u64>().ok() == Some(ino)
    })
}

/// How often the kernel's lock table is read before a lock counts as not
/// listed: the table is handed out a page at a time, and a lock released
/// elsewhere between two pages can make a read skip an entry (seen: a held
/// lock missing from about one read in ten while a test suite took and
/// released locks beside it).
const LOCK_TABLE_READS: usize = 3;

/// Whether the lock file at `path` is held, by either sign: its record names
/// a running holder (every holder this project writes records itself), or the
/// kernel's table lists a flock on it (a holder that recorded nothing). A
/// file that does not exist is not held; anything that cannot be read counts
/// as held, so the caller does not walk a target it could not show to be free.
pub fn lock_file_held(path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let meta = match fs::metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return false,
        Err(_) => return true,
    };
    match crate::maintenance::recorded_holder_runs(path) {
        Ok(false) => {}
        Ok(true) | Err(_) => return true,
    }
    (0..LOCK_TABLE_READS).any(|_| match fs::read_to_string("/proc/locks") {
        Ok(table) => flock_listed(&table, meta.dev(), meta.ino()),
        Err(_) => true,
    })
}

/// Whether a job holds the backup targets: the backup singleton or the
/// maintenance lock is held (backup, scrub, doctor, reconcile, walk, restore).
pub fn targets_held_by_a_job() -> bool {
    any_lock_held(&[
        Path::new(crate::backup::BACKUP_LOCK_PATH),
        Path::new(scrub::MAINTENANCE_LOCK_PATH),
    ])
}

/// Whether any of the lock files at `paths` is held.
fn any_lock_held(paths: &[&Path]) -> bool {
    paths.iter().any(|path| lock_file_held(path))
}

/// `secs` since the epoch as `YYYY-MM-DD HH:MM` (UTC), without chrono.
fn format_epoch_minutes(secs: u64) -> String {
    let minutes_total = secs / 60;
    let minute = minutes_total % 60;
    let hours_total = minutes_total / 60;
    let hour = hours_total % 24;
    let (year, month, day) = days_to_ymd((hours_total / 24) as i64);
    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}")
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

/// How long each step of one health query took, in the order they ran
/// (bd DAS-Backup-Manager-amsg: a reply slower than the GUI's 25 s D-Bus
/// timeout must say which step it waited on).
#[derive(Debug, Default)]
pub struct StepTimes(Vec<(String, Duration)>);

impl StepTimes {
    /// Run `f`, recording its duration under `name`.
    pub fn time<T>(&mut self, name: impl Into<String>, f: impl FnOnce() -> T) -> T {
        let start = Instant::now();
        let out = f();
        self.0.push((name.into(), start.elapsed()));
        out
    }

    pub fn record(&mut self, name: impl Into<String>, took: Duration) {
        self.0.push((name.into(), took));
    }

    /// The `n` slowest steps, slowest first: `name 1.2 s, name 0.3 s`.
    pub fn slowest(&self, n: usize) -> String {
        let mut steps: Vec<&(String, Duration)> = self.0.iter().collect();
        steps.sort_by_key(|step| std::cmp::Reverse(step.1));
        steps
            .iter()
            .take(n)
            .map(|(name, took)| format!("{name} {:.1} s", took.as_secs_f64()))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// The journal line for a health query that took `total` or longer than
/// `threshold`, naming its slowest steps; `None` when it was quick.
pub fn slow_health_warning(
    total: Duration,
    threshold: Duration,
    steps: &StepTimes,
) -> Option<String> {
    // The time no step accounts for: a large one says the wait was outside
    // every timed step (on 2026-11-01, 15.4 of 15.7 s; bd tobl).
    let timed: Duration = steps.0.iter().map(|(_, took)| *took).sum();
    (total >= threshold).then(|| {
        format!(
            "HealthQuery took {:.1} s (warn at {} s); slowest steps: {}; untimed {:.1} s",
            total.as_secs_f64(),
            threshold.as_secs(),
            steps.slowest(5),
            total.saturating_sub(timed).as_secs_f64()
        )
    })
}

/// Query health status of all configured targets.
pub fn get_health(config: &Config) -> Result<HealthReport, Box<dyn std::error::Error>> {
    get_health_timed(config, &mut StepTimes::default())
}

/// [`get_health`], recording each step's duration in `times`. Every mounted
/// target is walked: right for a caller that holds the targets itself (a
/// run's report) or answers once (the CLI).
pub fn get_health_timed(
    config: &Config,
    times: &mut StepTimes,
) -> Result<HealthReport, Box<dyn std::error::Error>> {
    get_health_with(config, times, false, &|_, mount| SnapshotAnswer {
        reading: read_snapshots(mount),
        kept_age_secs: None,
    })
}

/// [`get_health_timed`] with each mounted target's snapshot numbers taken
/// from `snapshots(label, mount)`; `targets_busy` is passed through to the
/// report.
pub fn get_health_with(
    config: &Config,
    times: &mut StepTimes,
    targets_busy: bool,
    snapshots: &dyn Fn(&str, &str) -> SnapshotAnswer,
) -> Result<HealthReport, Box<dyn std::error::Error>> {
    let mut newest_epoch: Option<u64> = None;
    let mut last_backup_reading_age_secs: Option<u64> = None;
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
        match times.time("scrub state", scrub::load_state) {
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
        let label = &target.label;
        let actual_mount = times.time(format!("{label} mount"), || {
            find_any_mount(&target.mount, &target.serial, &target.role)
        });
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
            times.time(format!("{label} usage"), || {
                measure_target_usage(&effective_path)
            })
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
        // One pass gives the count and the newest snapshot's time.
        let answer = if mounted {
            times.time(format!("{label} snapshots"), || {
                snapshots(label, &effective_path)
            })
        } else {
            SnapshotAnswer {
                reading: None,
                kept_age_secs: None,
            }
        };
        let snapshot_count = answer.reading.map(|r| r.count);
        if let Some(secs) = answer.reading.and_then(|r| r.newest_epoch) {
            newest_epoch = newest_epoch.max(Some(secs));
            last_backup_reading_age_secs = last_backup_reading_age_secs.max(answer.kept_age_secs);
        }

        // 4. Get SMART details of every configured drive (use -d sat for
        // USB-attached SATA drives); a RAID-1 pair is two (bd uhi).
        let serials: Vec<String> = if target.serials.is_empty() {
            vec![target.serial.clone()]
        } else {
            target.serials.clone()
        };
        let smart_start = Instant::now();
        let (members, smart_details) =
            members_with(&serials, &device_info_from_serial, &|dev, is_usb| {
                let mut cmd = std::process::Command::new("smartctl");
                cmd.args(["--json", "--all"]);
                if is_usb {
                    cmd.arg("-d").arg("sat");
                }
                cmd.arg(dev)
                    .output()
                    .ok()
                    .and_then(|o| String::from_utf8(o.stdout).ok())
                    .and_then(|json| parse_smartctl_details(&json))
            });
        if !members.is_empty() {
            times.record(format!("{label} smart"), smart_start.elapsed());
        }
        if let Some(w) = degraded_warning(label, &members) {
            warnings.push(w);
        }

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
            usage_known: usage.is_some(),
            snapshot_count,
            snapshot_reading_age_secs: answer.kept_age_secs,
            smart_status,
            temperature_c,
            power_on_hours,
            errors,
            scrub: scrub_health,
            members,
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
    let growth_start = Instant::now();
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
    times.record("growth log", growth_start.elapsed());

    // 6b. Recovery OSes: read live while a drive is mounted, otherwise the
    // record the last backup run kept. The host (uname, pacman) is probed only
    // when there is a reading to compare it with.
    let recovery = times.time("recovery OS", || {
        crate::recovery_os::health_with(
            config,
            &crate::recovery_os::load_state(&crate::recovery_os::state_path()),
            &crate::recovery_os::host_versions,
            &crate::caldate::today(),
            &is_mountpoint,
        )
    });
    warnings.extend(recovery.warnings);

    // 7. Determine overall status
    let status = determine_status(&target_healths, &warnings);

    // 8. Last backup time: the newest snapshot the targets' passes found.
    let last_backup = newest_epoch.map(format_epoch_minutes);

    Ok(HealthReport {
        status,
        targets: target_healths,
        last_backup,
        last_backup_reading_age_secs,
        targets_busy,
        growth_points,
        warnings,
        recovery_os: recovery.lines,
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

    // --- bd DAS-Backup-Manager-5237.16: kept snapshot readings ---

    fn reading(count: usize, newest: u64) -> SnapshotReading {
        SnapshotReading {
            count,
            newest_epoch: Some(newest),
        }
    }

    #[test]
    fn a_busy_target_is_served_its_kept_reading_with_its_age_and_is_not_walked() {
        let kept = KeptSnapshots::default();
        let free = kept.answer("primary", "/mnt/p", false, 1_000, &|_| {
            Some(reading(12, 900))
        });
        assert_eq!(
            free,
            SnapshotAnswer {
                reading: Some(reading(12, 900)),
                kept_age_secs: None
            }
        );
        let walked = std::cell::Cell::new(false);
        let busy = kept.answer("primary", "/mnt/p", true, 1_240, &|_| {
            walked.set(true);
            Some(reading(99, 1_200))
        });
        assert!(!walked.get(), "a target a job holds must not be walked");
        assert_eq!(
            busy,
            SnapshotAnswer {
                reading: Some(reading(12, 900)),
                kept_age_secs: Some(240)
            }
        );
    }

    #[test]
    fn a_busy_target_with_no_kept_reading_is_unknown_and_another_targets_is_not_lent() {
        let kept = KeptSnapshots::default();
        kept.answer("other", "/mnt/o", false, 1_000, &|_| Some(reading(3, 800)));
        let busy = kept.answer("primary", "/mnt/p", true, 1_240, &|_| {
            panic!("walked a target a job holds")
        });
        assert_eq!(
            busy,
            SnapshotAnswer {
                reading: None,
                kept_age_secs: None
            }
        );
    }

    #[test]
    fn a_walk_that_cannot_read_keeps_the_earlier_reading() {
        let kept = KeptSnapshots::default();
        kept.answer("primary", "/mnt/p", false, 1_000, &|_| {
            Some(reading(12, 900))
        });
        let failed = kept.answer("primary", "/mnt/p", false, 1_100, &|_| None);
        assert_eq!(
            failed.reading, None,
            "this answer is unknown, not the old one"
        );
        let busy = kept.answer("primary", "/mnt/p", true, 1_300, &|_| None);
        assert_eq!(busy.reading, Some(reading(12, 900)));
        assert_eq!(busy.kept_age_secs, Some(300));
    }

    fn set_mtime(path: &Path, epoch: u64) {
        let dir = fs::File::open(path).unwrap();
        dir.set_modified(UNIX_EPOCH + Duration::from_secs(epoch))
            .unwrap();
    }

    #[test]
    fn one_pass_counts_the_snapshots_and_finds_the_newest() {
        let dir = tempfile::tempdir().unwrap();
        for (name, mtime) in [
            ("nvme/root.20261101T0300", 1_000_000),
            ("nvme/root.20261102T030001", 3_000_000),
            ("ssd/vm.20261101T0300", 2_000_000),
            ("ssd/not-a-snapshot", 9_000_000),
        ] {
            let path = dir.path().join(name);
            fs::create_dir_all(&path).unwrap();
            set_mtime(&path, mtime);
        }
        // A plain file at the top level is no source directory.
        fs::write(dir.path().join("stray.20261101T0300"), "").unwrap();
        assert_eq!(
            read_snapshots(&dir.path().to_string_lossy()),
            Some(SnapshotReading {
                count: 3,
                newest_epoch: Some(3_000_000)
            })
        );
    }

    #[test]
    fn a_target_that_cannot_be_listed_is_unknown_and_an_empty_one_is_zero() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            read_snapshots(&dir.path().to_string_lossy()),
            Some(SnapshotReading {
                count: 0,
                newest_epoch: None
            })
        );
        assert_eq!(
            read_snapshots(&dir.path().join("gone").to_string_lossy()),
            None
        );
    }

    #[test]
    fn an_epoch_is_printed_to_the_minute_in_utc() {
        assert_eq!(format_epoch_minutes(1_794_478_748), "2026-11-12 10:19");
        assert_eq!(format_epoch_minutes(0), "1970-01-01 00:00");
    }

    const PROC_LOCKS: &str = "\
1: FLOCK  ADVISORY  WRITE 4242 00:1a:1234 0 EOF
2: POSIX  ADVISORY  WRITE 4243 00:1a:77 0 EOF
3: -> FLOCK  ADVISORY  WRITE 4244 00:1a:88 0 EOF
4: FLOCK  ADVISORY  READ 4245 fd:02:99 0 EOF
";

    #[test]
    fn a_flock_is_found_by_device_and_inode_in_the_kernels_table() {
        let dev = |major: u32, minor: u32| libc::makedev(major, minor);
        assert!(flock_listed(PROC_LOCKS, dev(0, 0x1a), 1234));
        assert!(flock_listed(PROC_LOCKS, dev(0xfd, 2), 99), "a shared hold");
        // Same inode number on another filesystem.
        assert!(!flock_listed(PROC_LOCKS, dev(0, 0x1b), 1234));
        // A POSIX record lock is not a flock.
        assert!(!flock_listed(PROC_LOCKS, dev(0, 0x1a), 77));
        // A waiter holds nothing.
        assert!(!flock_listed(PROC_LOCKS, dev(0, 0x1a), 88));
        assert!(!flock_listed("", dev(0, 0x1a), 1234));
    }

    /// A pid no process has: above the kernel's largest.
    const NO_SUCH_PID: u32 = 4_000_000_000;

    #[test]
    fn a_lock_file_whose_record_names_a_running_holder_is_held() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("das.lock");
        assert!(!lock_file_held(&path), "no file, no holder");
        fs::write(&path, "").unwrap();
        assert!(!lock_file_held(&path), "an empty record and no flock");
        fs::write(&path, format!("backup-run.sh pid {}\n", std::process::id())).unwrap();
        assert!(lock_file_held(&path));
        // The record of a holder that died without emptying it.
        fs::write(&path, format!("backup-run.sh pid {NO_SUCH_PID}\n")).unwrap();
        assert!(!lock_file_held(&path));
        // A record that cannot be read counts as held.
        fs::write(&path, [0xff, 0xfe]).unwrap();
        assert!(lock_file_held(&path));
    }

    #[test]
    fn a_flock_with_no_record_is_found_in_the_kernels_table_and_gone_once_unlocked() {
        use std::os::fd::AsRawFd;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("das.lock");
        let file = fs::File::create(&path).unwrap();
        assert_eq!(
            unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        // One reading of the table can skip an entry while other tests take
        // and release locks (LOCK_TABLE_READS); it is never listed falsely.
        assert!((0..50).any(|_| lock_file_held(&path)));
        // Unlocked, not only closed: a flock lives on the open file
        // description, and a child another test forks at this moment holds a
        // copy of the descriptor until its exec.
        assert_eq!(unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) }, 0);
        assert!(!lock_file_held(&path));
    }

    #[test]
    fn the_targets_are_held_when_either_lock_is() {
        let dir = tempfile::tempdir().unwrap();
        let (backup, maintenance) = (dir.path().join("b.lock"), dir.path().join("m.lock"));
        let both = [backup.as_path(), maintenance.as_path()];
        assert!(!any_lock_held(&both), "neither file exists");
        for held in &both {
            fs::write(&backup, "").unwrap();
            fs::write(&maintenance, "").unwrap();
            assert!(!any_lock_held(&both));
            fs::write(
                held,
                format!("btrdasd scrub run pid {}\n", std::process::id()),
            )
            .unwrap();
            assert!(any_lock_held(&both), "one held is enough");
        }
    }

    // --- slow health query reporting (bd DAS-Backup-Manager-amsg) ---

    fn steps(list: &[(&str, u64)]) -> StepTimes {
        let mut t = StepTimes::default();
        for (name, ms) in list {
            t.record(*name, Duration::from_millis(*ms));
        }
        t
    }

    #[test]
    fn slowest_steps_are_named_slowest_first_and_capped() {
        let t = steps(&[("a", 100), ("b", 25_800), ("c", 1_200), ("d", 0)]);
        assert_eq!(t.slowest(2), "b 25.8 s, c 1.2 s");
        assert_eq!(t.slowest(9), "b 25.8 s, c 1.2 s, a 0.1 s, d 0.0 s");
        assert_eq!(StepTimes::default().slowest(5), "");
    }

    #[test]
    fn time_records_the_step_and_returns_its_value() {
        let mut t = StepTimes::default();
        let v = t.time("sleep", || {
            std::thread::sleep(Duration::from_millis(30));
            7
        });
        assert_eq!(v, 7);
        assert_eq!(t.0.len(), 1);
        assert_eq!(t.0[0].0, "sleep");
        assert!(t.0[0].1 >= Duration::from_millis(30), "{:?}", t.0);
    }

    #[test]
    fn smart_is_read_only_for_a_target_with_a_serial() {
        // An unmounted target is never measured; SMART is asked for by serial
        // alone, so the step is timed exactly for the target that has one.
        let dir = tempfile::tempdir().unwrap();
        let toml = format!(
            r#"
[general]
version = "0.4.0"
install_prefix = "/usr/local"
db_path = "/var/lib/das-backup/backup-index.db"
growth_log = "{d}/growth.log"
[init]
system = "systemd"
[schedule]
incremental = "03:00"
full = "Sun 04:00"
randomized_delay_min = 30
[email]
enabled = false
[gui]
enabled = false
[scrub]
enabled = false
[[target]]
label = "noserial"
serial = ""
mount = "{d}/a"
role = "primary"
[target.retention]
daily = 7
[[target]]
label = "withserial"
serial = "NO-SUCH-SERIAL-AMSG"
mount = "{d}/b"
role = "primary"
[target.retention]
daily = 7
"#,
            d = dir.path().display()
        );
        let config = Config::from_toml(&toml).unwrap();
        let mut times = StepTimes::default();
        let report = get_health_timed(&config, &mut times).unwrap();
        let names: Vec<&str> = times.0.iter().map(|(n, _)| n.as_str()).collect();
        assert!(names.contains(&"withserial smart"), "{names:?}");
        assert!(!names.contains(&"noserial smart"), "{names:?}");
        assert!(
            names.contains(&"noserial mount") && names.contains(&"recovery OS"),
            "{names:?}"
        );
        // The newest snapshot comes from each target's one pass, not from a
        // second walk of every target (bd DAS-Backup-Manager-5237.16).
        assert!(!names.contains(&"last backup"), "{names:?}");
        assert!(report.targets.iter().all(|t| t.snapshot_count.is_none()));
        assert_eq!(report.last_backup, None);
        assert!(
            report
                .targets
                .iter()
                .all(|t| !t.mounted && t.smart_status.is_none())
        );
    }

    #[test]
    fn a_slow_query_is_warned_at_the_threshold_and_a_quick_one_is_not() {
        let t = steps(&[("primary usage", 25_800), ("recovery OS", 900)]);
        let ten = Duration::from_secs(10);
        assert_eq!(
            slow_health_warning(Duration::from_millis(27_300), ten, &t).as_deref(),
            Some(
                "HealthQuery took 27.3 s (warn at 10 s); slowest steps: primary usage 25.8 s, recovery OS 0.9 s; untimed 0.6 s"
            )
        );
        // 2026-11-01 09:36, during a DAS scrub: 15.7 s with no step over
        // 0.2 s. The line must say where the time was not (bd tobl).
        let quick = steps(&[("b smart", 200), ("a smart", 100)]);
        assert_eq!(
            slow_health_warning(Duration::from_millis(15_700), ten, &quick).as_deref(),
            Some(
                "HealthQuery took 15.7 s (warn at 10 s); slowest steps: b smart 0.2 s, a smart 0.1 s; untimed 15.4 s"
            )
        );
        assert!(
            slow_health_warning(ten, ten, &t).is_some(),
            "at the threshold is slow"
        );
        assert_eq!(
            slow_health_warning(Duration::from_millis(9_999), ten, &t),
            None
        );
    }

    // --- pre-existing tests (unchanged) ---

    #[test]
    fn target_health_usage_percent() {
        let th = TargetHealth {
            label: "test".into(),
            serial: "ABC".into(),
            mounted: true,
            total_bytes: 1_000_000,
            used_bytes: 250_000,
            usage_known: true,
            snapshot_count: Some(10),
            snapshot_reading_age_secs: None,
            smart_status: Some("PASSED".into()),
            temperature_c: Some(32),
            power_on_hours: Some(12345),
            errors: None,
            scrub: ScrubHealth::not_applicable(),
            members: Vec::new(),
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
            usage_known: true,
            snapshot_count: Some(0),
            snapshot_reading_age_secs: None,
            smart_status: None,
            temperature_c: None,
            power_on_hours: None,
            errors: None,
            scrub: ScrubHealth::not_applicable(),
            members: Vec::new(),
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
                usage_known: true,
                snapshot_count: Some(10),
                snapshot_reading_age_secs: None,
                smart_status: Some("PASSED".into()),
                temperature_c: None,
                power_on_hours: None,
                errors: None,
                scrub: ScrubHealth::not_applicable(),
                members: Vec::new(),
            },
            TargetHealth {
                label: "t2".into(),
                serial: "S2".into(),
                mounted: true,
                total_bytes: 2_000_000_000,
                used_bytes: 800_000_000, // 40%
                usage_known: true,
                snapshot_count: Some(5),
                snapshot_reading_age_secs: None,
                smart_status: Some("PASSED".into()),
                temperature_c: None,
                power_on_hours: None,
                errors: None,
                scrub: ScrubHealth::not_applicable(),
                members: Vec::new(),
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
            usage_known: true,
            snapshot_count: Some(10),
            snapshot_reading_age_secs: None,
            smart_status: Some("PASSED".into()),
            temperature_c: None,
            power_on_hours: None,
            errors: None,
            scrub: ScrubHealth::not_applicable(),
            members: Vec::new(),
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
            usage_known: true,
            snapshot_count: Some(10),
            snapshot_reading_age_secs: None,
            smart_status: Some("FAILED".into()), // SMART failure → Critical
            temperature_c: None,
            power_on_hours: None,
            errors: None,
            scrub: ScrubHealth::not_applicable(),
            members: Vec::new(),
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
            usage_known: true,
            snapshot_count: Some(10),
            snapshot_reading_age_secs: None,
            smart_status: Some("PASSED".into()),
            temperature_c: None,
            power_on_hours: None,
            errors: None,
            scrub: ScrubHealth::not_applicable(),
            members: Vec::new(),
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

    #[test]
    fn members_lists_every_serial_and_marks_the_absent_one() {
        let serials = vec!["SERIAL11".to_string(), "SERIAL12".to_string()];
        let device_of = |s: &str| (s == "SERIAL12").then(|| ("/dev/sdc".to_string(), true));
        let smart_of = |dev: &str, is_usb: bool| {
            assert_eq!((dev, is_usb), ("/dev/sdc", true));
            Some(SmartDetails {
                status: "PASSED".into(),
                temperature_c: Some(30),
                power_on_hours: Some(7),
                errors: Some(0),
            })
        };
        let (members, first) = members_with(&serials, &device_of, &smart_of);
        assert_eq!(members.len(), 2);
        assert_eq!(
            (members[0].serial.as_str(), members[0].present),
            ("SERIAL11", false)
        );
        assert_eq!(members[0].smart_status, None);
        assert_eq!(
            (members[1].serial.as_str(), members[1].present),
            ("SERIAL12", true)
        );
        assert_eq!(members[1].smart_status.as_deref(), Some("PASSED"));
        assert_eq!(members[1].power_on_hours, Some(7));
        // The single-drive fields read the first PRESENT drive, not serials[0].
        assert_eq!(first.map(|d| d.temperature_c), Some(Some(30)));
        assert_eq!(
            degraded_warning("p", &members).as_deref(),
            Some("Target 'p': RAID-1 degraded — SERIAL11 absent, running on SERIAL12")
        );
        // Empty serials are skipped, not listed as absent drives.
        let (none, _) = members_with(&[String::new()], &device_of, &smart_of);
        assert!(none.is_empty());
    }

    #[test]
    fn degraded_warning_needs_one_drive_absent_and_one_present() {
        let m = |serial: &str, present| MemberHealth {
            serial: serial.into(),
            present,
            ..MemberHealth::default()
        };
        assert_eq!(degraded_warning("t", &[m("A", true), m("B", true)]), None);
        assert_eq!(degraded_warning("t", &[m("A", false), m("B", false)]), None);
        assert_eq!(degraded_warning("t", &[m("A", true)]), None);
        assert!(degraded_warning("t", &[m("A", true), m("B", false)]).is_some());
    }

    fn base_target_health(scrub: ScrubHealth) -> TargetHealth {
        TargetHealth {
            label: "t1".into(),
            serial: "S1".into(),
            mounted: true,
            total_bytes: 1_000_000_000,
            used_bytes: 100_000_000, // 10% — fine on its own
            usage_known: true,
            snapshot_count: Some(10),
            snapshot_reading_age_secs: None,
            smart_status: Some("PASSED".into()),
            temperature_c: None,
            power_on_hours: None,
            errors: None,
            scrub,
            members: Vec::new(),
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
            esp_partuuid: None,
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

    /// bd DAS-Backup-Manager-dis3: no `[scrub].targets` means every
    /// configured target is scrubbed, so health judges each one rather than
    /// calling it "not applicable".
    #[test]
    fn target_scrub_health_applies_to_every_target_when_no_scrub_targets_listed() {
        let mut config = Config::default();
        config.scrub.enabled = true;
        assert!(config.scrub.targets.is_empty());
        let target = scrub_test_target("any-name", "11111111-1111-1111-1111-111111111111");
        config.targets = vec![target.clone()];
        let state = scrub::ScrubState::default();
        let h = target_scrub_health(&config, &target, Some(&state), NOW);
        assert_eq!(h.status, ScrubHealthStatus::NeverScrubbed);
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

    #[test]
    fn device_for_fs_uuid_reads_blkid_and_is_absent_on_any_failure() {
        use crate::fsutil::testing::Scripted;
        let runner = Scripted::new(&[
            ("blkid -U u-1", 0, "/dev/sdc1\n"),
            ("blkid -U u-2", 2, ""),
            ("blkid -U u-3", 0, "\n"),
            ("blkid -U u-4", 0, "garbage\n"),
        ]);
        assert_eq!(
            device_for_fs_uuid_with(&runner, "u-1").as_deref(),
            Some("/dev/sdc1")
        );
        assert_eq!(
            device_for_fs_uuid_with(&runner, "u-2"),
            None,
            "no such UUID"
        );
        assert_eq!(
            device_for_fs_uuid_with(&runner, "u-3"),
            None,
            "empty answer"
        );
        assert_eq!(
            device_for_fs_uuid_with(&runner, "u-4"),
            None,
            "not a device"
        );
        assert_eq!(
            device_for_fs_uuid_with(&runner, "u-5"),
            None,
            "unscripted: exit 1"
        );
        assert_eq!(device_for_fs_uuid_with(&runner, ""), None);
        assert_eq!(runner.calls().len(), 5, "an empty UUID runs nothing");
    }

    #[test]
    fn a_bind_never_takes_a_subvolume_mount_of_the_device() {
        // The booted recovery drive: its `/` is subvolume @ (listed first),
        // and the same partition's top level is mounted elsewhere.
        // /proc/self/mountinfo: field 4 is the part of the filesystem mounted.
        let mounts = "\
26 1 0:24 /@ / rw,noatime shared:1 - btrfs /dev/sdk2 rw,subvolid=256,subvol=/@
27 26 0:24 /@home /home rw shared:2 - btrfs /dev/sdk2 rw,subvolid=257,subvol=/@home
28 26 8:17 / /srv rw shared:3 - ext4 /dev/sdb1 rw
29 26 0:24 / /run/media/b/top rw shared:4 - btrfs /dev/sdk2 rw,subvolid=5,subvol=/
";
        assert_eq!(
            top_level_mount_of(mounts, "/dev/sdk2").as_deref(),
            Some("/run/media/b/top")
        );
        // Only subvolume mounts: nothing to bind.
        let only_sub = "26 1 0:24 /@ / rw shared:1 - btrfs /dev/sdk2 rw,subvolid=256,subvol=/@\n";
        assert_eq!(top_level_mount_of(only_sub, "/dev/sdk2"), None);
        // A bind of a subdirectory of the top level (bd 71q1, F3): /proc/mounts
        // shows it subvolid=5, as if it were the top level; mountinfo does not.
        let subdir = "\
30 1 0:24 /.btrbk-snapshots /mnt/x rw - btrfs /dev/sdk2 rw,subvolid=5,subvol=/
31 1 0:24 / /mnt/top rw - btrfs /dev/sdk2 rw,subvolid=5,subvol=/
";
        assert_eq!(
            top_level_mount_of(subdir, "/dev/sdk2").as_deref(),
            Some("/mnt/top")
        );
        // No optional fields before the separator, and a line without one.
        assert_eq!(
            top_level_mount_of(
                "31 1 0:24 / /mnt/top rw - btrfs /dev/sdk2 rw\nbroken\n",
                "/dev/sdk2"
            )
            .as_deref(),
            Some("/mnt/top")
        );
        // A filesystem without subvolumes qualifies; another device does not match.
        assert_eq!(
            top_level_mount_of(mounts, "/dev/sdb1").as_deref(),
            Some("/srv")
        );
        assert_eq!(top_level_mount_of(mounts, "/dev/sdz9"), None);
    }

    /// Real `findmnt` on `/`: the production wrapper must return exactly what
    /// the tool prints (any filesystem type with a UUID, not only btrfs).
    #[test]
    fn mount_uuid_fsroot_matches_findmnt_on_this_machine() {
        let findmnt = |col: &str| {
            std::process::Command::new("findmnt")
                .args(["-no", col, "/"])
                .output()
                .ok()
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        };
        if std::process::Command::new("findmnt")
            .arg("--version")
            .output()
            .is_err()
        {
            eprintln!("SKIP: findmnt not installed");
            return;
        }
        let uuid = findmnt("UUID").unwrap();
        if uuid.is_empty() {
            eprintln!("SKIP: / has no filesystem UUID on this machine");
            return;
        }
        let fsroot = findmnt("FSROOT").unwrap();
        assert_eq!(mount_uuid_fsroot(Path::new("/")), Some((uuid, fsroot)));
    }

    #[test]
    fn find_mount_for_device_with_resolves_serial_partition_and_top_level_mount() {
        // A fresh directory: a pid-named one collides across runs in a PID
        // namespace, where the pid repeats.
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_path_buf();
        // `<dev>1` and `<dev>2` are symlinks to the real partition nodes.
        let (real1, real2) = (dir.join("real1"), dir.join("real2"));
        std::fs::write(&real1, "").unwrap();
        std::fs::write(&real2, "").unwrap();
        let dev = dir.join("disk").to_string_lossy().to_string();
        std::os::unix::fs::symlink(&real1, format!("{dev}1")).unwrap();
        std::os::unix::fs::symlink(&real2, format!("{dev}2")).unwrap();
        let (c1, c2) = (
            real1.canonicalize().unwrap().to_string_lossy().to_string(),
            real2.canonicalize().unwrap().to_string_lossy().to_string(),
        );
        let mounts = format!(
            "40 1 0:41 / /mnt/one rw - btrfs {c1} rw,subvolid=5,subvol=/\n\
             41 1 0:42 /@ /sub rw - btrfs {c2} rw,subvolid=256,subvol=/@\n\
             42 1 0:42 / /mnt/two rw - btrfs {c2} rw,subvolid=5,subvol=/\n"
        );
        let resolver = |s: &str| (s == "SER").then(|| dev.clone());
        let find = |serial: &str, role: &TargetRole| {
            find_mount_for_device_with(&resolver, &mounts, serial, role)
        };
        assert_eq!(
            find("SER", &TargetRole::Primary).as_deref(),
            Some("/mnt/one")
        );
        assert_eq!(
            find("SER", &TargetRole::Mirror).as_deref(),
            Some("/mnt/two")
        );
        assert_eq!(
            find("OTHER", &TargetRole::Primary),
            None,
            "serial not found"
        );
        assert_eq!(
            find_mount_for_device_with(&resolver, "", "SER", &TargetRole::Primary),
            None,
            "not mounted"
        );
    }

    #[test]
    fn mount_uuid_fsroot_reads_two_fields_and_nothing_else() {
        use crate::fsutil::testing::Scripted;
        let runner = Scripted::new(&[
            ("findmnt -no UUID,FSROOT /m/a", 0, "60b0-aa /\n"),
            ("findmnt -no UUID,FSROOT /m/b", 0, "60b0-aa /@\n"),
            ("findmnt -no UUID,FSROOT /m/c", 0, "/\n"),
            ("findmnt -no UUID,FSROOT /m/d", 1, "60b0-aa /\n"),
            ("findmnt -no UUID,FSROOT /m/e", 0, "1111-x /@\n60b0-aa /\n"),
            ("findmnt -no UUID,FSROOT /m/f", 0, ""),
        ]);
        let at = |p: &str| mount_uuid_fsroot_with(&runner, Path::new(p));
        assert_eq!(at("/m/a"), Some(("60b0-aa".into(), "/".into())));
        assert_eq!(at("/m/b"), Some(("60b0-aa".into(), "/@".into())));
        assert_eq!(at("/m/c"), None, "empty UUID prints one field");
        assert_eq!(at("/m/d"), None, "findmnt failed");
        assert_eq!(
            at("/m/e"),
            Some(("60b0-aa".into(), "/".into())),
            "the mount on top"
        );
        assert_eq!(at("/m/f"), None);
        assert_eq!(at("/m/unscripted"), None);
    }

    #[test]
    fn device_for_fs_uuid_finds_the_root_filesystem_on_this_machine() {
        // The one identity every machine has: whatever `/` is. Read-only.
        let Ok(out) = std::process::Command::new("findmnt")
            .args(["-n", "-o", "UUID", "/"])
            .output()
        else {
            return;
        };
        let uuid = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if uuid.is_empty() || !Path::new(&format!("/dev/disk/by-uuid/{uuid}")).exists() {
            return; // a container root (overlay) has no UUID to look up
        }
        let dev = device_for_fs_uuid(&uuid).expect("blkid -U must find the root filesystem");
        assert!(dev.starts_with("/dev/"), "{dev}");
        assert_eq!(
            device_for_fs_uuid("00000000-0000-0000-0000-000000000000"),
            None
        );
    }

    #[test]
    fn btrfs_uuid_of_reads_only_a_btrfs_filesystem() {
        use crate::fsutil::testing::Scripted;
        let runner = Scripted::new(&[
            (
                "blkid -o export /dev/sdc2",
                0,
                "DEVNAME=/dev/sdc2\nUUID=60b0\nUUID_SUB=x\nTYPE=btrfs\n",
            ),
            (
                "blkid -o export /dev/sdc1",
                0,
                "DEVNAME=/dev/sdc1\nUUID=ABCD-0003\nTYPE=vfat\n",
            ),
            ("blkid -o export /dev/sdd1", 0, "TYPE=btrfs\n"),
            ("blkid -o export /dev/sde1", 2, "TYPE=btrfs\nUUID=zz\n"),
            ("blkid -o export /dev/sdf1", 0, "TYPE=btrfs\nUUID=\n"),
        ]);
        assert_eq!(
            btrfs_uuid_of_with(&runner, "/dev/sdc2").as_deref(),
            Some("60b0")
        );
        assert_eq!(
            btrfs_uuid_of_with(&runner, "/dev/sdc1"),
            None,
            "an ESP is never offered"
        );
        assert_eq!(
            btrfs_uuid_of_with(&runner, "/dev/sdd1"),
            None,
            "no UUID line"
        );
        assert_eq!(
            btrfs_uuid_of_with(&runner, "/dev/sde1"),
            None,
            "blkid failed"
        );
        assert_eq!(btrfs_uuid_of_with(&runner, "/dev/sdf1"), None, "empty UUID");
        assert_eq!(btrfs_uuid_of_with(&runner, "/dev/none"), None, "unscripted");
    }

    #[test]
    fn btrfs_uuid_for_serial_reads_the_partition_the_role_uses() {
        use crate::fsutil::testing::Scripted;
        let runner = Scripted::new(&[
            (
                "blkid -o export /dev/sdc1",
                0,
                "UUID=ABCD-0003\nTYPE=vfat\n",
            ),
            ("blkid -o export /dev/sdc2", 0, "UUID=60b0\nTYPE=btrfs\n"),
            ("blkid -o export /dev/sdd1", 0, "UUID=b2db\nTYPE=btrfs\n"),
        ]);
        let lookup = |serial: &str| match serial {
            "REC" => Some("/dev/sdc".to_string()),
            "PRI" => Some("/dev/sdd".to_string()),
            _ => None,
        };
        let uuid = |serial, role| btrfs_uuid_for_serial_with(&runner, &lookup, serial, role);
        assert_eq!(uuid("REC", &TargetRole::Mirror).as_deref(), Some("60b0"));
        assert_eq!(
            uuid("REC", &TargetRole::Primary),
            None,
            "partition 1 is the ESP"
        );
        assert_eq!(uuid("PRI", &TargetRole::Primary).as_deref(), Some("b2db"));
        assert_eq!(uuid("GONE", &TargetRole::Primary), None);
        assert_eq!(runner.calls().len(), 3, "an absent drive runs nothing");
    }

    #[test]
    fn btrfs_uuid_for_serial_is_absent_for_a_drive_that_is_not_here() {
        assert_eq!(
            btrfs_uuid_for_serial("DAS-NO-SUCH-SERIAL-XYZ", &TargetRole::Primary),
            None
        );
        assert_eq!(btrfs_uuid_for_serial("", &TargetRole::Mirror), None);
    }
}
