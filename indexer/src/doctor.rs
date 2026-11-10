//! Subvolume drift detector (`btrdasd doctor --check-drift`, bd DAS-Backup-Manager-01u).
//!
//! Finds two classes of drift between `config.toml` and what actually exists on
//! each source filesystem:
//!
//! - **Missing** — a subvolume exists on disk but no live `[[source]].subvolume`
//!   entry references it (an entry marked `retired` does not count: it is not
//!   backed up, so a subvolume whose only entry is retired is missing). This is the dangerous case: the 2026-05-17 audit found ~30
//!   subvolumes (all of `Projects/*`, audiobook app state, `@docker`,
//!   `@hibp`, Steam libraries, ISOs) silently never backed up, because daily
//!   backups of the *configured* subvolumes kept succeeding and masked the gap.
//! - **Stale** — a `[[source]].subvolume` entry references a name that no longer
//!   exists on disk (renamed or deleted project, never cleaned out of config).
//!
//! The backup run adopts new subvolumes by itself (`btrdasd subvol sync`), so a
//! subvolume reported **missing** now means that step failed or has not run
//! since the subvolume was created. The check uses the same listing
//! ([`crate::adopt::list_volumes`]) and the same exclusion rules as sync, so
//! the two cannot disagree about what a volume holds or what is left out.
//!
//! # Design: pure core, thin orchestration shell
//!
//! Every decision that can be made from data alone — applying exclusions,
//! computing the missing/stale sets, turning volume listings into a report,
//! matching a glob pattern — is a pure function over `Vec<String>`/structs,
//! fixture-tested below with no root privileges and no real btrfs filesystem.
//! Only [`run_drift_check`] and its helpers touch locks, mounts, or spawn
//! `btrfs`.
//!
//! # Why a real mountpoint check gates every `btrfs subvolume list` call
//!
//! `btrfs subvolume list <path>` does not fail on a path that merely *isn't a
//! btrfs mountpoint* — the DAS source volumes (`/.btrfs-nvme`, `/.btrfs-hdd`,
//! `/.btrfs-ssd`) are plain empty directories on the host's root filesystem when
//! unmounted, so an unmounted-and-unguarded call silently lists the *root
//! filesystem's* subvolumes instead and returns a normal, successful-looking
//! result. That is the same "bare mountpoint falls through to the parent
//! filesystem" trap documented for backup targets in `.claude/rules/backup.md`
//! (bd DAS-Backup-Manager-9on), applied to sources instead of targets.
//! [`crate::adopt::list_volumes`] therefore requires a real mountpoint with the
//! expected UUID before it trusts a listing — a silent wrong-filesystem read
//! here would corrupt both the missing and stale sets.

use std::path::Path;

use crate::adopt::VolumeListing;
use crate::config::Config;
use crate::health;
use crate::maintenance::MaintenanceHeld;
use crate::mount;
use crate::progress::{LogLevel, ProgressCallback};
use crate::scrub;

/// Non-blocking singleton lock — a second concurrent doctor run skips rather
/// than queues (this is a fast, cheap, read-mostly check; queuing behind
/// another instance would gain nothing).
pub const DOCTOR_LOCK_PATH: &str = "/run/das-doctor.lock";

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Errors that abort a drift check outright — never per-volume mount/list
/// failures, which are recorded in the report instead (see
/// [`DriftReport::volumes_failed`]).
#[derive(Debug)]
pub enum DoctorError {
    /// A lock file could not be opened or locked (distinct from the lock
    /// simply being *held*, which is [`LockAttempt::SingletonBusy`] /
    /// [`LockAttempt::MaintenanceBusy`], not an error).
    Lock(scrub::ScrubError),
}

impl std::fmt::Display for DoctorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Lock(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for DoctorError {}

impl From<scrub::ScrubError> for DoctorError {
    fn from(e: scrub::ScrubError) -> Self {
        Self::Lock(e)
    }
}

// ---------------------------------------------------------------------------
// Locking
// ---------------------------------------------------------------------------

/// Both locks a drift check holds, for as long as the check runs.
///
/// Unlike the scrub engine's [`scrub::ScrubLocks`], neither lock here is ever
/// acquired *blocking*: a drift check that finds either lock held simply
/// defers (exit 0) rather than waiting, because it has nothing urgent to say
/// that is worth delaying a real backup or scrub for. Acquisition order
/// (singleton, then maintenance) matches the other two holders of
/// `/run/das-maintenance.lock` — the *scheduled* `scripts/backup-run.sh` path
/// and the scrub engine (`scrub::acquire_locks`) — so those three are
/// deadlock-free as a set even though this side never blocks.
///
/// **Manual backups take the same locks** through `backup::acquire_manual_locks`, so a
/// `btrdasd doctor` run and a manual `btrdasd backup` invocation cannot mount or
/// unmount the same source volumes at once (bd `pe6`).
///
/// **Kill-signal case (documented, not handled by a signal handler).** A
/// `SIGKILL` — e.g. `systemd` escalating past `das-backup-doctor.service`'s
/// `TimeoutStartSec=600` — gives [`mount::MountGuard`]'s `Drop` impl no chance
/// to run, so any source volumes newly mounted by this invocation are *not*
/// unmounted. The two locks are unaffected: `flock(2)` is released by the
/// kernel the instant the holding process's file descriptors close, which a
/// `SIGKILL` guarantees, so neither `/run/das-doctor.lock` nor
/// `/run/das-maintenance.lock` can be left stuck locked by a killed doctor
/// run. The leaked *mount* is the only residue, and it is inert rather than
/// actively harmful: `mount::ensure_sources_mounted` already treats an
/// already-mounted source volume as a no-op (`health::is_mountpoint` check),
/// so the next backup, scrub, or doctor run simply finds the volume mounted
/// and proceeds — the leak persists until a reboot or a manual `umount`, but
/// nothing downstream breaks because of it.
struct DoctorLocks {
    #[allow(dead_code)] // held only for its Drop (lock release) side effect
    maintenance: MaintenanceHeld,
    #[allow(dead_code)]
    singleton: scrub::FileLock,
}

/// Outcome of trying to acquire both locks without blocking.
enum LockAttempt {
    Acquired(DoctorLocks),
    /// Another `btrdasd doctor` invocation holds the singleton lock.
    SingletonBusy,
    /// Another job holds the shared maintenance lock — `holder`, as it
    /// recorded itself ([`crate::maintenance::holder_of`]).
    MaintenanceBusy {
        holder: String,
    },
}

fn try_acquire_locks_at(
    singleton_path: &Path,
    maintenance_path: &Path,
) -> Result<LockAttempt, DoctorError> {
    let Some(singleton) = scrub::FileLock::try_acquire(singleton_path)? else {
        return Ok(LockAttempt::SingletonBusy);
    };
    let Some(maintenance) = MaintenanceHeld::try_acquire_at(maintenance_path, "btrdasd doctor")?
    else {
        return Ok(LockAttempt::MaintenanceBusy {
            holder: crate::maintenance::holder_of(maintenance_path),
        });
    };
    Ok(LockAttempt::Acquired(DoctorLocks {
        maintenance,
        singleton,
    }))
}

fn try_acquire_locks() -> Result<LockAttempt, DoctorError> {
    try_acquire_locks_at(
        Path::new(DOCTOR_LOCK_PATH),
        Path::new(scrub::MAINTENANCE_LOCK_PATH),
    )
}

// ---------------------------------------------------------------------------
// Pure core — glob matching, drift computation
// ---------------------------------------------------------------------------

/// Minimal shell-style glob match (`*` = any run of characters including none,
/// `?` = exactly one character). No character classes, no `**`. Deliberately
/// hand-rolled rather than pulling in a glob crate — `[doctor].exclude`
/// patterns are simple by design (`"*.cache"`, `"Downloads/*"`), and this is
/// the same "don't add a dependency for something ~20 lines can do" call the
/// project already makes for `LazyLock<Regex>` compile-once patterns elsewhere.
pub fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    glob_match_inner(&p, &t)
}

fn glob_match_inner(p: &[char], t: &[char]) -> bool {
    match p.first() {
        None => t.is_empty(),
        Some('*') => {
            // Try consuming zero or more characters of `t` for this '*'.
            glob_match_inner(&p[1..], t) || (!t.is_empty() && glob_match_inner(p, &t[1..]))
        }
        Some('?') => !t.is_empty() && glob_match_inner(&p[1..], &t[1..]),
        Some(c) => !t.is_empty() && t[0] == *c && glob_match_inner(&p[1..], &t[1..]),
    }
}

/// On-disk subvolumes that sync should have adopted: not configured, not in
/// a snapshot tree, not excluded.
pub fn compute_missing(
    on_disk: &[String],
    configured: &[String],
    exclude: &[String],
) -> Vec<String> {
    let mut missing: Vec<String> = on_disk
        .iter()
        .filter(|p| !crate::adopt::in_snapshot_tree(p))
        .filter(|p| crate::adopt::excluding_pattern(p, exclude).is_none())
        .filter(|p| !configured.contains(p))
        .cloned()
        .collect();
    missing.sort();
    missing.dedup();
    missing
}

/// Compute stale config entries: names in `configured` with no exact match in
/// `on_disk`.
pub fn compute_stale(on_disk: &[String], configured: &[String]) -> Vec<String> {
    let mut stale: Vec<String> = configured
        .iter()
        // The top level exists while its volume does: never stale.
        .filter(|c| c.as_str() != crate::adopt::TOP_LEVEL && !on_disk.contains(c))
        .cloned()
        .collect();
    stale.sort();
    stale.dedup();
    stale
}

// ---------------------------------------------------------------------------
// Source/volume grouping (pure)
// ---------------------------------------------------------------------------

/// All sources sharing one physical volume (e.g. `hdd-projects`, `hdd-media`,
/// `hdd-system`, and `hdd-audiobooks` all point at `/.btrfs-hdd`), with the
/// union of their configured subvolume names — the set the "missing" check
/// compares against, since any of those sources backing up a name means it is
/// not a gap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VolumeGroup {
    pub volume: String,
    /// Source labels sharing this volume, in config order.
    pub source_labels: Vec<String>,
    /// Union of every subvolume name configured across those sources, in
    /// first-seen order.
    pub configured: Vec<String>,
}

/// Group `config.sources` by physical volume path, preserving first-seen
/// order for both volumes and the labels/names within each group.
pub fn group_sources_by_volume(config: &Config) -> Vec<VolumeGroup> {
    let mut groups: Vec<VolumeGroup> = Vec::new();
    for source in &config.sources {
        let group = match groups.iter_mut().find(|g| g.volume == source.volume) {
            Some(g) => g,
            None => {
                groups.push(VolumeGroup {
                    volume: source.volume.clone(),
                    source_labels: Vec::new(),
                    configured: Vec::new(),
                });
                groups.last_mut().expect("just pushed")
            }
        };
        group.source_labels.push(source.label.clone());
        // A retired entry is not backed up. If its subvolume is back on disk it
        // must read as missing, because sync will revive it rather than ignore it.
        // A top level in a source that reaches a mirror target is not backed
        // up either: the btrbk.conf renderer refuses it (bd xtnq).
        let top_level_refused = !config.mirrors_reached(source).is_empty();
        for sv in source.subvolumes.iter().filter(|e| {
            e.retired.is_none() && !(e.name == crate::adopt::TOP_LEVEL && top_level_refused)
        }) {
            if !group.configured.contains(&sv.name) {
                group.configured.push(sv.name.clone());
            }
        }
    }
    groups
}

// ---------------------------------------------------------------------------
// Orchestration — locks, mount, list, compare, unmount
// ---------------------------------------------------------------------------

/// A missing (on-disk, not configured) subvolume, attributed to the volume it
/// was found on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingSubvolume {
    pub volume: String,
    /// Source labels sharing this volume — the diff snippet must be added
    /// under one of these `[[source]]` blocks in config.toml, but which one is
    /// an operator judgment call this tool does not attempt to make (see
    /// `format_report`'s doc comment).
    pub source_labels: Vec<String>,
    pub name: String,
}

/// A stale (configured, not on-disk) subvolume, attributed to the specific
/// source whose config entry no longer matches anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaleSubvolume {
    pub volume: String,
    pub source_label: String,
    pub name: String,
}

/// Full result of a drift check that actually ran (as opposed to deferring).
#[derive(Debug, Clone, Default)]
pub struct DriftReport {
    /// Volumes successfully mounted and listed.
    pub volumes_checked: usize,
    /// Volumes that could not be examined — `(volume, reason)`. A non-empty
    /// list here does not by itself fail the check (see [`DriftReport::ran`])
    /// unless *every* configured volume failed.
    pub volumes_failed: Vec<(String, String)>,
    pub missing: Vec<MissingSubvolume>,
    pub stale: Vec<StaleSubvolume>,
    /// Volumes this check mounted and could not unmount again (bd
    /// DAS-Backup-Manager-5oc).
    pub left_mounted: Vec<String>,
    /// Targets the delivery check read (bd DAS-Backup-Manager-sp78.7).
    pub targets_checked: usize,
    /// Targets it could not read — `(label, reason)`: verified, mounted and
    /// listed, or none of its pairs is known to be delivered.
    pub targets_failed: Vec<(String, String)>,
    /// Pairs `btrbk.conf` declares whose target holds no snapshot newer than
    /// `[doctor].max_snapshot_age_days`.
    pub undelivered: Vec<UndeliveredPair>,
}

impl DriftReport {
    /// Whether any drift (missing, stale or undelivered) was found.
    pub fn has_drift(&self) -> bool {
        !self.missing.is_empty() || !self.stale.is_empty() || !self.undelivered.is_empty()
    }

    /// Whether the check examined at least one volume. `false` means "could
    /// not run" — every configured volume failed to mount or list.
    pub fn ran(&self) -> bool {
        self.volumes_checked > 0
    }

    /// The single, canonical "this run needs attention" bit — `true` when
    /// there is drift, OR at least one volume could not be examined even
    /// though others were (a genuine finding: that volume's subvolumes went
    /// unchecked, which is exactly the kind of gap this tool exists to
    /// surface, not silently swallow).
    ///
    /// [`format_report`], the `--email` trigger in `main.rs`, and `main.rs`'s
    /// `exit_code_for_doctor()` all read from this one method rather than
    /// re-deriving "not clean" three separate times — a prior version computed the same
    /// condition inline in each of those three places and let the exit-code
    /// copy drift out of sync with the other two (a volume failure among
    /// otherwise-clean volumes reported `DRIFT DETECTED — FAILURE` and sent a
    /// failure email while still exiting 0). Any future third state must be
    /// added here, not at a call site.
    pub fn not_clean(&self) -> bool {
        self.has_drift() || self.could_not_examine()
    }

    /// Something went unexamined: a source volume or a target that could not
    /// be read, or one the check could not unmount. An operational fault, not
    /// a finding: it outranks drift in the exit code.
    pub fn could_not_examine(&self) -> bool {
        !self.volumes_failed.is_empty()
            || !self.left_mounted.is_empty()
            || !self.targets_failed.is_empty()
    }
}

/// A pair `btrbk.conf` declares that its target holds no recent snapshot of
/// (bd DAS-Backup-Manager-sp78.7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UndeliveredPair {
    pub target: String,
    pub source_label: String,
    pub subvolume: String,
    /// The name btrbk gives the subvolume's snapshots.
    pub snapshot_name: String,
    /// The newest snapshot of the pair the target holds, as `YYYY-MM-DD`;
    /// `None` when it holds none at all.
    pub newest: Option<String>,
    /// How many days old that is, `None` with it.
    pub age_days: Option<i64>,
    /// The limit it was held to (`[doctor].max_snapshot_age_days`).
    pub limit_days: u32,
}

/// What the delivery check found.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct DeliveryCheck {
    pub targets_checked: usize,
    pub targets_failed: Vec<(String, String)>,
    pub undelivered: Vec<UndeliveredPair>,
}

/// The delivery check: every target holds a snapshot of every pair
/// `btrbk.conf` declares for it, newer than `[doctor].max_snapshot_age_days`
/// (bd DAS-Backup-Manager-sp78.7). Pure: `today` is a day number
/// ([`civil_days`]), `verify` proves a target is the real, mounted drive and
/// `list_dir` reads a directory on it.
///
/// A pair's snapshots on a target are the entries `<snapshot name>.<timestamp>`
/// of its directory; the newest of them is its delivery. A target is checked
/// only when a declared pair goes to it, and only once it is verified: a
/// target that cannot be verified, or whose directory cannot be listed
/// (anything but "no such directory", which is a pair never sent), is a
/// failure and none of its pairs is reported fresh or stale.
pub fn delivery_check(
    config: &Config,
    today: i64,
    verify: &dyn Fn(&crate::config::Target) -> Result<(), String>,
    list_dir: &dyn Fn(&Path) -> std::io::Result<Vec<String>>,
) -> DeliveryCheck {
    let limit = i64::from(config.doctor.max_snapshot_age_days);
    let pairs = crate::btrbk_conf::declared_pairs(config);
    let mut targets: Vec<&crate::config::Target> = Vec::new();
    for p in &pairs {
        if !targets.iter().any(|t| t.label == p.target.label) {
            targets.push(p.target);
        }
    }
    let mut out = DeliveryCheck::default();
    for target in targets {
        if let Err(why) = verify(target) {
            out.targets_failed.push((target.label.clone(), why));
            continue;
        }
        let mine: Vec<_> = pairs
            .iter()
            .filter(|p| p.target.label == target.label)
            .collect();
        let mut listed: Vec<(String, Vec<String>)> = Vec::new();
        let mut unreadable = None;
        for pair in &mine {
            let dir = pair.target_dir();
            if listed.iter().any(|(d, _)| *d == dir) {
                continue;
            }
            match list_dir(Path::new(&dir)) {
                Ok(names) => listed.push((dir, names)),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    listed.push((dir, Vec::new()))
                }
                Err(e) => {
                    unreadable = Some(format!("{dir} could not be listed: {e}"));
                    break;
                }
            }
        }
        if let Some(why) = unreadable {
            out.targets_failed.push((target.label.clone(), why));
            continue;
        }
        out.targets_checked += 1;
        for pair in mine {
            let dir = pair.target_dir();
            let names = listed
                .iter()
                .find(|(d, _)| *d == dir)
                .map_or(&[][..], |(_, n)| n.as_slice());
            let newest = names
                .iter()
                .filter_map(|n| snapshot_day(n, &pair.snapshot_name))
                .max();
            let age = newest.map(|d| today - d);
            if age.is_some_and(|a| a <= limit) {
                continue;
            }
            out.undelivered.push(UndeliveredPair {
                target: target.label.clone(),
                source_label: pair.source.label.clone(),
                subvolume: pair.subvolume.name.clone(),
                snapshot_name: pair.snapshot_name.clone(),
                newest: newest.map(day_text),
                age_days: age,
                limit_days: config.doctor.max_snapshot_age_days,
            });
        }
    }
    out
}

/// The day a target directory entry `entry` dates `name`'s snapshot to, as a
/// day number: `<name>.<YYYYMMDD>`, then nothing or a time (`T…`) or a
/// counter (`_N`) — btrbk's `short`, `long` and `long-iso` timestamp formats.
/// Anything else, a date that does not exist included, is not a snapshot of
/// `name`.
fn snapshot_day(entry: &str, name: &str) -> Option<i64> {
    let stamp = entry.strip_prefix(name)?.strip_prefix('.')?;
    let (date, rest) = (stamp.get(..8)?, &stamp[8..]);
    if !date.bytes().all(|b| b.is_ascii_digit())
        || !(rest.is_empty() || rest.starts_with(['T', '_']))
    {
        return None;
    }
    civil_days(
        date[..4].parse().ok()?,
        date[4..6].parse().ok()?,
        date[6..].parse().ok()?,
    )
}

/// `YYYY-MM-DD` of a day number (the inverse of [`civil_days`]).
fn day_text(days: i64) -> String {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Today's date on this host's clock, as a day number. Snapshot names carry
/// local dates, so the day is the local one.
fn local_today() -> i64 {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as libc::time_t);
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: `secs` is a valid time_t and `tm` a properly allocated libc::tm.
    unsafe { libc::localtime_r(&secs, &mut tm) };
    days_from_tm(tm.tm_year.into(), tm.tm_mon.into(), tm.tm_mday.into())
}

/// The day number of a `struct tm`'s date: `tm_year` counts from 1900 and
/// `tm_mon` from 0. A date `tm` cannot hold (never one `localtime_r` gives)
/// is day 0, which every snapshot reads as long past.
fn days_from_tm(tm_year: i64, tm_mon: i64, tm_mday: i64) -> i64 {
    civil_days(tm_year + 1900, tm_mon + 1, tm_mday).unwrap_or(0)
}

/// Days since 1970-01-01 of a proleptic Gregorian date, `None` for a date
/// that does not exist.
pub fn civil_days(y: i64, m: i64, d: i64) -> Option<i64> {
    let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    let in_month = match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return None,
    };
    if d < 1 || d > in_month {
        return None;
    }
    // Days from civil, after Howard Hinnant: years counted from 1 March, in
    // 400-year eras of 146097 days.
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some(era * 146_097 + doe - 719_468)
}

/// Outcome of a full `run_drift_check` invocation.
pub enum DoctorOutcome {
    /// A lock was held by another process — this invocation deferred without
    /// examining anything. Always maps to exit code 0 (see `main.rs`).
    Deferred { reason: String },
    /// The check ran (successfully or not) — see [`DriftReport::ran`].
    Ran(DriftReport),
}

/// Turn volume listings into a report. A volume whose listing failed, or that
/// has no listing at all, is recorded in `volumes_failed` and nothing is
/// computed for it: "could not look" must never read as "nothing missing".
pub fn drift_from_listings(config: &Config, listings: &[VolumeListing]) -> DriftReport {
    let groups = group_sources_by_volume(config);
    let mut report = DriftReport::default();
    // volume -> on-disk list, so the per-source stale check below doesn't
    // need the listing again for volumes multiple sources share.
    let mut on_disk_by_volume: Vec<(&str, &Vec<String>)> = Vec::new();
    let exclude = config.exclude_patterns();

    for group in &groups {
        let listing = listings.iter().find(|l| l.volume == group.volume);
        match listing.map(|l| &l.subvolumes) {
            Some(Ok(on_disk)) => {
                report.volumes_checked += 1;
                for name in compute_missing(on_disk, &group.configured, &exclude) {
                    report.missing.push(MissingSubvolume {
                        volume: group.volume.clone(),
                        source_labels: group.source_labels.clone(),
                        name,
                    });
                }
                on_disk_by_volume.push((group.volume.as_str(), on_disk));
            }
            Some(Err(why)) => {
                report
                    .volumes_failed
                    .push((group.volume.clone(), why.clone()));
            }
            None => {
                report
                    .volumes_failed
                    .push((group.volume.clone(), "volume was not listed".to_string()));
            }
        }
    }

    // Stale check is per-source (each config entry belongs to exactly one
    // source), skipped entirely for a source whose volume failed above. A
    // retired entry is expected to be absent, so it is never stale.
    for source in &config.sources {
        let Some((_, on_disk)) = on_disk_by_volume.iter().find(|(v, _)| *v == source.volume) else {
            continue;
        };
        let configured: Vec<String> = source
            .subvolumes
            .iter()
            .filter(|sv| sv.retired.is_none())
            .map(|sv| sv.name.clone())
            .collect();
        for name in compute_stale(on_disk, &configured) {
            report.stale.push(StaleSubvolume {
                volume: source.volume.clone(),
                source_label: source.label.clone(),
                name,
            });
        }
    }
    report
}

/// Mount sources, list every configured volume, unmount, and build the
/// report. Never returns `Err` — per-volume failures are recorded in
/// [`DriftReport::volumes_failed`] instead, matching the scrub engine's
/// "some targets fail, the pass still ran" philosophy (`exit_code_for_pass`
/// in `main.rs`): a check that examined 3 of 4 volumes still ran.
fn perform_drift_check(
    config: &Config,
    progress: &dyn ProgressCallback,
    held: &MaintenanceHeld,
) -> DriftReport {
    let mut guard = mount::ensure_sources_mounted(config, progress);
    let listings =
        crate::adopt::list_volumes(config, &crate::fsutil::SystemRunner, &health::is_mountpoint);
    let mut report = drift_from_listings(config, &listings);
    report.left_mounted = guard.unmount(progress);
    perform_delivery_check(config, progress, held, &mut report);
    report
}

/// Every target a declared pair goes to, once, as a failure whose reason is
/// `why`: what the delivery check reports when no target could be mounted.
fn unmounted_targets(config: &Config, why: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for pair in crate::btrbk_conf::declared_pairs(config) {
        if !out.iter().any(|(label, _)| *label == pair.target.label) {
            out.push((
                pair.target.label.clone(),
                format!("no target could be mounted: {why}"),
            ));
        }
    }
    out
}

/// The delivery check against the real targets (bd DAS-Backup-Manager-sp78.7):
/// mount them under the maintenance lock the check holds, prove each is the
/// drive the configuration names (`mount::verify_write_targets`, the guard
/// that keeps a bare mount point from being read as an empty drive), list
/// what the declared pairs' directories hold, unmount. Nothing is written to
/// a target. With no declared pair there is nothing to check and no target is
/// mounted; when no target can be mounted, every target a pair goes to is a
/// failure.
fn perform_delivery_check(
    config: &Config,
    progress: &dyn ProgressCallback,
    held: &MaintenanceHeld,
    report: &mut DriftReport,
) {
    let pairs = crate::btrbk_conf::declared_pairs(config);
    if pairs.is_empty() {
        return;
    }
    progress.on_stage(
        "Checking that every target holds a recent snapshot of each pair",
        1,
    );
    let mut guard = match mount::ensure_targets_mounted(config, progress, held) {
        Ok(guard) => guard,
        Err(e) => {
            report.targets_failed = unmounted_targets(config, &e.to_string());
            return;
        }
    };
    let check = delivery_check(
        config,
        local_today(),
        &|t| {
            mount::verify_write_targets(
                std::slice::from_ref(t),
                std::slice::from_ref(&t.label),
                progress,
            )
        },
        &|dir| {
            std::fs::read_dir(dir)?
                .map(|e| e.map(|e| e.file_name().to_string_lossy().into_owned()))
                .collect()
        },
    );
    report.left_mounted.extend(guard.unmount(progress));
    report.targets_checked = check.targets_checked;
    report.targets_failed.extend(check.targets_failed);
    report.undelivered = check.undelivered;
}

/// Run the drift check: acquire locks (non-blocking, deferring rather than
/// waiting if either is held), mount sources, compare, unmount, report.
pub fn run_drift_check(
    config: &Config,
    progress: &dyn ProgressCallback,
) -> Result<DoctorOutcome, DoctorError> {
    match try_acquire_locks()? {
        LockAttempt::SingletonBusy => {
            let reason =
                format!("Another doctor run holds {DOCTOR_LOCK_PATH} — skipping this invocation");
            progress.on_log(LogLevel::Info, &reason);
            Ok(DoctorOutcome::Deferred { reason })
        }
        LockAttempt::MaintenanceBusy { holder } => {
            let reason = format!("DAS maintenance lock held by {holder} — skipping drift check");
            progress.on_log(LogLevel::Info, &reason);
            Ok(DoctorOutcome::Deferred { reason })
        }
        LockAttempt::Acquired(locks) => {
            progress.on_stage("Checking for subvolume drift", 1);
            let report = perform_drift_check(config, progress, &locks.maintenance);
            progress.on_complete(
                !report.has_drift(),
                &format!(
                    "{} volume(s) checked, {} missing, {} stale, {} undelivered",
                    report.volumes_checked,
                    report.missing.len(),
                    report.stale.len(),
                    report.undelivered.len()
                ),
            );
            Ok(DoctorOutcome::Ran(report))
            // `locks` drops here, releasing both file locks.
        }
    }
}

// ---------------------------------------------------------------------------
// Reporting
// ---------------------------------------------------------------------------
/// Render a human-readable report, shared verbatim as both the console output
/// and the email body (mirrors `scrub::format_scrub_report`'s approach — the
/// mailer derives its subject status word from the literal string `FAILURE`
/// appearing in the body, so the header below always contains it whenever the
/// check found drift, a volume failure, or could not run at all).
///
/// A missing subvolume is reported as a failure of the backup run's own
/// adoption step, never as a to-do list: the fix is to find out why sync did
/// not adopt it, not to edit `config.toml` by hand.
pub fn format_report(report: &DriftReport) -> String {
    let sep = "═".repeat(63);
    let thin = "─".repeat(63);
    let mut r = String::with_capacity(2048);

    let overall = if !report.ran() {
        "COULD NOT RUN — FAILURE"
    } else if report.not_clean() {
        "DRIFT DETECTED — FAILURE"
    } else {
        "NO DRIFT — CLEAN"
    };

    r.push_str(&format!(
        "{sep}\n  DAS Subvolume Drift Report\n  Status: {overall}\n{sep}\n\n"
    ));

    r.push_str(&format!("SUMMARY\n{thin}\n"));
    r.push_str(&format!(
        "  Volumes checked       {}\n",
        report.volumes_checked
    ));
    r.push_str(&format!(
        "  Volumes failed        {}\n",
        report.volumes_failed.len()
    ));
    r.push_str(&format!(
        "  Missing subvolumes    {}\n",
        report.missing.len()
    ));
    r.push_str(&format!("  Stale config entries  {}\n", report.stale.len()));
    r.push_str(&format!(
        "  Targets checked       {}\n",
        report.targets_checked
    ));
    r.push_str(&format!(
        "  Targets failed        {}\n",
        report.targets_failed.len()
    ));
    r.push_str(&format!(
        "  Undelivered pairs     {}\n",
        report.undelivered.len()
    ));

    if !report.volumes_failed.is_empty() {
        r.push_str(&format!("\nVOLUMES NOT CHECKED\n{thin}\n"));
        for (volume, detail) in &report.volumes_failed {
            r.push_str(&format!("  {volume}: {detail}\n"));
        }
    }

    if !report.missing.is_empty() {
        r.push_str(&format!(
            "\nNOT BACKED UP — the backup run should have adopted these\n{thin}\n"
        ));
        for m in &report.missing {
            r.push_str(&format!(
                "  {}  (volume {}, source(s): {})\n",
                crate::adopt::display_name(&m.name),
                m.volume,
                m.source_labels.join(", ")
            ));
        }
        r.push_str(
            "\n  The backup run adopts new subvolumes by itself. One appearing here means\n  \
             that step failed or has not run since the subvolume was created. See what\n  \
             it would do with:  sudo btrdasd subvol sync --dry-run\n",
        );
    }

    if !report.stale.is_empty() {
        r.push_str(&format!(
            "\nSTALE CONFIG ENTRIES (configured, not retired, not found on disk)\n{thin}\n"
        ));
        for s in &report.stale {
            r.push_str(&format!(
                "  {}  (source: {}, volume: {})\n",
                s.name, s.source_label, s.volume
            ));
        }
    }

    if !report.targets_failed.is_empty() {
        r.push_str(&format!("\nTARGETS NOT CHECKED\n{thin}\n"));
        for (label, detail) in &report.targets_failed {
            r.push_str(&format!("  {label}: {detail}\n"));
        }
        r.push_str(
            "\n  A target that could not be read is never taken as up to date: none of its\n  \
             pairs was checked.\n",
        );
    }

    if !report.undelivered.is_empty() {
        r.push_str(&format!(
            "\nUNDELIVERED — a target holds no snapshot of a declared pair newer than its limit\n{thin}\n"
        ));
        for u in &report.undelivered {
            let name = crate::adopt::display_name(&u.subvolume);
            match (&u.newest, u.age_days) {
                (Some(newest), Some(age)) => r.push_str(&format!(
                    "  {name}  (source: {}, target: {}): newest snapshot {newest}, {age} days old (limit {})\n",
                    u.source_label, u.target, u.limit_days
                )),
                _ => r.push_str(&format!(
                    "  {name}  (source: {}, target: {}): no snapshot at all (limit {} days)\n",
                    u.source_label, u.target, u.limit_days
                )),
            }
        }
        r.push_str(
            "\n  The backup run sends every declared pair to its target; one listed here\n  \
             was skipped or failed without failing the run. Read the last backup report\n  \
             and the btrbk log for the pair.\n",
        );
    }

    if !report.left_mounted.is_empty() {
        r.push_str(&format!(
            "\nLEFT MOUNTED — the check mounted these and could not unmount them\n{thin}\n"
        ));
        for volume in &report.left_mounted {
            r.push_str(&format!("  {volume}\n"));
        }
    }

    r.push_str(&format!("\n{sep}\n"));
    r
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Source, SubvolConfig};

    fn s(items: &[&str]) -> Vec<String> {
        items.iter().map(|x| (*x).to_string()).collect()
    }

    // -- glob_match --------------------------------------------------------

    #[test]
    fn glob_match_exact() {
        assert!(glob_match("foo", "foo"));
        assert!(!glob_match("foo", "bar"));
    }

    #[test]
    fn glob_match_star_suffix() {
        assert!(glob_match("*.cache", "build.cache"));
        assert!(glob_match("*.cache", ".cache"));
        assert!(!glob_match("*.cache", "cache.txt"));
    }

    #[test]
    fn glob_match_star_prefix() {
        assert!(glob_match("Downloads/*", "Downloads/movie.iso"));
        assert!(!glob_match("Downloads/*", "Uploads/movie.iso"));
        // '*' matches zero characters too
        assert!(glob_match("Downloads/*", "Downloads/"));
    }

    #[test]
    fn glob_match_question_mark() {
        assert!(glob_match("v?.0", "v1.0"));
        assert!(!glob_match("v?.0", "v10.0"));
    }

    #[test]
    fn glob_match_multiple_stars() {
        assert!(glob_match("*foo*bar*", "xxfooyybarzz"));
        assert!(!glob_match("*foo*bar*", "xxfooyy"));
    }

    // -- compute_missing / compute_stale -------------------------------------

    #[test]
    fn compute_missing_uses_the_same_rules_as_sync() {
        let on_disk = s(&[
            "@",
            "@home",
            "@cache/stremio",
            "@steam",
            ".snapshots/1/snapshot",
            "@tmp",
            "new",
        ]);
        let configured = s(&["@", "@home"]);
        let exclude = s(&["@tmp", "@var-tmp", "@cache"]);
        // "@steam" is reported: a name that looks rebuildable is no longer a
        // reason to leave a subvolume out.
        assert_eq!(
            compute_missing(&on_disk, &configured, &exclude),
            s(&["@steam", "new"])
        );
    }

    #[test]
    fn compute_missing_finds_new_subvolumes() {
        let on_disk = s(&["@", "@home", "Projects/new-project"]);
        let configured = s(&["@", "@home"]);
        assert_eq!(
            compute_missing(&on_disk, &configured, &[]),
            s(&["Projects/new-project"])
        );
    }

    #[test]
    fn compute_missing_never_reports_snapshot_trees() {
        let on_disk = s(&[
            "@",
            ".snapshots/1/snapshot",
            "Audiobooks/.btrbk-snapshots/foo.20260101",
            ".btrbk-snapshots",
        ]);
        assert!(compute_missing(&on_disk, &s(&["@"]), &[]).is_empty());
    }

    #[test]
    fn compute_missing_excludes_user_patterns_and_what_is_nested_under_them() {
        let on_disk = s(&["@", "scratch.cache", "scratch.cache/inner"]);
        let missing = compute_missing(&on_disk, &s(&["@"]), &s(&["*.cache"]));
        assert!(missing.is_empty(), "{missing:?}");
    }

    #[test]
    fn compute_missing_sorted_and_deduped() {
        let on_disk = s(&["zeta", "alpha", "alpha"]);
        assert_eq!(compute_missing(&on_disk, &[], &[]), s(&["alpha", "zeta"]));
    }

    #[test]
    fn compute_stale_finds_removed_subvolumes() {
        let on_disk = s(&["@", "@home"]);
        let configured = s(&["@", "@home", "@deleted-project"]);
        assert_eq!(
            compute_stale(&on_disk, &configured),
            s(&["@deleted-project"])
        );
    }

    #[test]
    fn compute_stale_empty_when_all_present() {
        let on_disk = s(&["@", "@home"]);
        let configured = s(&["@", "@home"]);
        assert!(compute_stale(&on_disk, &configured).is_empty());
    }

    // -- group_sources_by_volume ---------------------------------------------

    fn source(label: &str, volume: &str, subvols: &[&str]) -> Source {
        Source {
            label: label.into(),
            volume: volume.into(),
            subvolumes: subvols
                .iter()
                .map(|n| SubvolConfig {
                    name: (*n).into(),
                    snapshot_name: None,
                    ..Default::default()
                })
                .collect(),
            device: "/dev/fake".into(),
            snapshot_dir: ".btrbk-snapshots".into(),
            target_subdirs: vec![],
            target_labels: vec![],
        }
    }

    /// One source on `/.btrfs-hdd` holding `@a` and `@b`.
    fn test_config() -> Config {
        let mut cfg = Config::default();
        cfg.sources
            .push(source("hdd", "/.btrfs-hdd", &["@a", "@b"]));
        cfg
    }

    fn listing(volume: &str, subvolumes: Result<Vec<String>, String>) -> VolumeListing {
        VolumeListing {
            volume: volume.into(),
            subvolumes,
            uuids: std::collections::BTreeMap::new(),
            fs_uuid: None,
        }
    }

    #[test]
    fn group_sources_by_volume_merges_shared_volume() {
        let mut cfg = Config::default();
        cfg.sources
            .push(source("hdd-projects", "/.btrfs-hdd", &["Projects"]));
        cfg.sources.push(source(
            "hdd-media",
            "/.btrfs-hdd",
            &["media", "SteamLibrary"],
        ));
        cfg.sources
            .push(source("nvme", "/.btrfs-nvme", &["@", "@home"]));

        let groups = group_sources_by_volume(&cfg);
        assert_eq!(groups.len(), 2);
        let hdd = groups.iter().find(|g| g.volume == "/.btrfs-hdd").unwrap();
        assert_eq!(hdd.source_labels, vec!["hdd-projects", "hdd-media"]);
        assert_eq!(hdd.configured, vec!["Projects", "media", "SteamLibrary"]);
        let nvme = groups.iter().find(|g| g.volume == "/.btrfs-nvme").unwrap();
        assert_eq!(nvme.source_labels, vec!["nvme"]);
        assert_eq!(nvme.configured, vec!["@", "@home"]);
    }

    #[test]
    fn group_sources_by_volume_empty_config() {
        let cfg = Config::default();
        assert!(group_sources_by_volume(&cfg).is_empty());
    }

    #[test]
    fn a_retired_entry_is_neither_configured_nor_stale() {
        let mut cfg = test_config();
        cfg.sources[0].subvolumes[0].retired = Some("2026-10-01".into());
        let gone = cfg.sources[0].subvolumes[0].name.clone();
        let groups = group_sources_by_volume(&cfg);
        assert!(!groups[0].configured.contains(&gone));
        assert!(groups[0].configured.contains(&"@b".to_string()));

        // Through the same grouped list the drift check uses: the retired,
        // absent entry is not stale; a live, absent one still is.
        let on_disk = s(&["@other"]);
        assert_eq!(compute_stale(&on_disk, &groups[0].configured), s(&["@b"]));
    }

    // -- drift_from_listings ---------------------------------------------------

    #[test]
    fn drift_from_listings_reports_missing_and_stale() {
        let cfg = test_config();
        let listings = [listing(
            "/.btrfs-hdd",
            Ok(s(&["@a", "@new", ".snapshots/1/snapshot"])),
        )];
        let report = drift_from_listings(&cfg, &listings);
        assert_eq!(report.volumes_checked, 1);
        assert!(report.volumes_failed.is_empty());
        assert_eq!(
            report.missing,
            vec![MissingSubvolume {
                volume: "/.btrfs-hdd".into(),
                source_labels: vec!["hdd".into()],
                name: "@new".into(),
            }]
        );
        assert_eq!(
            report.stale,
            vec![StaleSubvolume {
                volume: "/.btrfs-hdd".into(),
                source_label: "hdd".into(),
                name: "@b".into(),
            }]
        );
    }

    #[test]
    fn drift_from_listings_uses_the_configured_excludes() {
        let mut cfg = test_config();
        cfg.subvolumes.exclude = s(&["@cache"]);
        cfg.doctor.exclude = s(&["scratch*"]);
        let listings = [listing(
            "/.btrfs-hdd",
            Ok(s(&["@a", "@b", "@tmp", "@var-tmp", "@cache/x", "scratch1"])),
        )];
        let report = drift_from_listings(&cfg, &listings);
        assert!(report.missing.is_empty(), "{:?}", report.missing);
    }

    #[test]
    fn drift_from_listings_failed_listing_computes_nothing_for_that_volume() {
        let cfg = test_config();
        let listings = [listing(
            "/.btrfs-hdd",
            Err("source 'hdd' names device 'x': bad".into()),
        )];
        let report = drift_from_listings(&cfg, &listings);
        assert_eq!(report.volumes_checked, 0);
        assert_eq!(
            report.volumes_failed,
            vec![(
                "/.btrfs-hdd".to_string(),
                "source 'hdd' names device 'x': bad".to_string()
            )]
        );
        // Unlisted must not read as "everything configured is stale".
        assert!(report.missing.is_empty());
        assert!(report.stale.is_empty());
        assert!(report.not_clean());
        assert!(!report.ran());
    }

    #[test]
    fn drift_from_listings_a_volume_with_no_listing_is_failed() {
        let cfg = test_config();
        let report = drift_from_listings(&cfg, &[]);
        assert_eq!(
            report.volumes_failed,
            vec![(
                "/.btrfs-hdd".to_string(),
                "volume was not listed".to_string()
            )]
        );
        assert!(report.stale.is_empty());
        assert_eq!(report.volumes_checked, 0);
    }

    #[test]
    fn drift_from_listings_one_failed_volume_does_not_hide_another() {
        let mut cfg = test_config();
        cfg.sources.push(source("nvme", "/.btrfs-nvme", &["@"]));
        let listings = [
            listing("/.btrfs-hdd", Err("not mounted".into())),
            listing("/.btrfs-nvme", Ok(s(&["@", "@extra"]))),
        ];
        let report = drift_from_listings(&cfg, &listings);
        assert_eq!(report.volumes_checked, 1);
        assert_eq!(report.volumes_failed.len(), 1);
        assert_eq!(report.missing.len(), 1);
        assert_eq!(report.missing[0].name, "@extra");
    }

    #[test]
    fn a_retired_entry_whose_subvolume_is_back_is_missing_until_sync_revives_it() {
        let mut cfg = test_config();
        cfg.sources[0].subvolumes[0].retired = Some("2026-10-01".into());
        // "@a" is present again; only a retired entry names it.
        let listings = [listing("/.btrfs-hdd", Ok(s(&["@a", "@b"])))];
        let report = drift_from_listings(&cfg, &listings);
        assert_eq!(
            report
                .missing
                .iter()
                .map(|m| m.name.as_str())
                .collect::<Vec<_>>(),
            vec!["@a"]
        );
        assert!(report.stale.is_empty());
    }

    #[test]
    fn a_retired_absent_entry_is_not_stale_but_a_live_absent_one_is() {
        let mut cfg = test_config();
        cfg.sources[0].subvolumes[0].retired = Some("2026-10-01".into());
        // Neither "@a" (retired) nor "@b" (live) is on disk.
        let listings = [listing("/.btrfs-hdd", Ok(s(&["@other"])))];
        let report = drift_from_listings(&cfg, &listings);
        let stale: Vec<&str> = report.stale.iter().map(|x| x.name.as_str()).collect();
        assert_eq!(stale, vec!["@b"]);
    }

    // -- DriftReport ----------------------------------------------------------

    #[test]
    fn drift_report_ran_and_has_drift() {
        let mut report = DriftReport::default();
        assert!(!report.ran());
        assert!(!report.has_drift());

        report.volumes_checked = 1;
        assert!(report.ran());
        assert!(!report.has_drift());

        report.missing.push(MissingSubvolume {
            volume: "/.btrfs-hdd".into(),
            source_labels: vec!["hdd-projects".into()],
            name: "new-project".into(),
        });
        assert!(report.has_drift());
    }

    #[test]
    fn drift_report_not_ran_when_zero_volumes_checked() {
        let mut report = DriftReport::default();
        report
            .volumes_failed
            .push(("/.btrfs-hdd".into(), "not mounted".into()));
        assert!(!report.ran());
    }

    // -- format_report ---------------------------------------------------------

    #[test]
    fn format_report_clean_has_no_failure_word() {
        let report = DriftReport {
            volumes_checked: 2,
            ..Default::default()
        };
        let text = format_report(&report);
        assert!(text.contains("NO DRIFT — CLEAN"));
        assert!(!text.contains("FAILURE"));
        assert!(!text.contains("NOT BACKED UP"));
    }

    #[test]
    fn format_report_names_missing_as_a_sync_failure_and_suggests_no_hand_edit() {
        let report = DriftReport {
            volumes_checked: 1,
            volumes_failed: Vec::new(),
            missing: vec![MissingSubvolume {
                volume: "/.btrfs-hdd".into(),
                source_labels: vec!["hdd-media".into()],
                name: "media/video".into(),
            }],
            stale: Vec::new(),
            left_mounted: Vec::new(),
            ..Default::default()
        };
        let text = format_report(&report);
        assert!(
            text.contains("  Status: DRIFT DETECTED — FAILURE\n"),
            "{text}"
        );
        assert!(
            text.contains("NOT BACKED UP — the backup run should have adopted these\n"),
            "{text}"
        );
        assert!(
            text.contains("  media/video  (volume /.btrfs-hdd, source(s): hdd-media)\n"),
            "{text}"
        );
        assert!(
            text.contains("sudo btrdasd subvol sync --dry-run"),
            "{text}"
        );
        assert!(!text.contains("SUGGESTED config.toml ADDITIONS"), "{text}");
        assert!(!text.contains("REBUILDABLE"), "{text}");
        assert!(!text.contains("[[source.subvolumes]]"), "{text}");
    }

    #[test]
    fn format_report_lists_every_source_label_of_a_shared_volume() {
        let report = DriftReport {
            volumes_checked: 1,
            missing: vec![MissingSubvolume {
                volume: "/.btrfs-hdd".into(),
                source_labels: vec!["hdd-projects".into(), "hdd-media".into()],
                name: "Projects/new-project".into(),
            }],
            ..Default::default()
        };
        let text = format_report(&report);
        assert!(text.contains("hdd-projects, hdd-media"), "{text}");
    }

    #[test]
    fn format_report_could_not_run_contains_failure_word() {
        let mut report = DriftReport::default();
        report
            .volumes_failed
            .push(("/.btrfs-hdd".into(), "not mounted".into()));
        let text = format_report(&report);
        assert!(text.contains("COULD NOT RUN — FAILURE"));
        assert!(text.contains("VOLUMES NOT CHECKED"));
        assert!(!report.ran());
    }

    #[test]
    fn format_report_lists_stale_entries() {
        let mut report = DriftReport {
            volumes_checked: 1,
            ..Default::default()
        };
        report.stale.push(StaleSubvolume {
            volume: "/.btrfs-hdd".into(),
            source_label: "hdd-projects".into(),
            name: "Projects/deleted-project".into(),
        });
        let text = format_report(&report);
        assert!(
            text.contains("STALE CONFIG ENTRIES (configured, not retired, not found on disk)"),
            "{text}"
        );
        assert!(text.contains("Projects/deleted-project"));
        assert!(text.contains("hdd-projects"));
    }

    // -- the delivery check (bd DAS-Backup-Manager-sp78.7) ------------------------

    use crate::config::{Retention, Target, TargetRole};
    use std::cell::RefCell;
    use std::collections::BTreeMap;
    use std::io::ErrorKind;

    fn target(label: &str, role: TargetRole) -> Target {
        Target {
            label: label.into(),
            serial: "SERIAL01".into(),
            serials: vec!["SERIAL01".into()],
            mount_uuid: None,
            mount: format!("/mnt/{label}"),
            role,
            retention: Retention {
                daily: 7,
                ..Default::default()
            },
            display_name: String::new(),
            esp_partuuid: None,
        }
    }

    /// `ssd` (to `target_labels`, subdirectory `nvme`): `@srv` (snapshots
    /// `srv`) and `@data` (`data`); targets `big` and `small`.
    fn delivery_config(target_labels: &[&str]) -> Config {
        let mut ssd = source("ssd", "/ssd", &["@srv", "@data"]);
        ssd.target_subdirs = vec!["nvme".into()];
        ssd.target_labels = target_labels.iter().map(|l| l.to_string()).collect();
        Config {
            sources: vec![ssd],
            targets: vec![
                target("big", TargetRole::Primary),
                target("small", TargetRole::Mirror),
            ],
            ..Config::default()
        }
    }

    const TODAY: (i64, i64, i64) = (2026, 11, 10);

    fn today() -> i64 {
        civil_days(TODAY.0, TODAY.1, TODAY.2).unwrap()
    }

    /// What `list_dir` answers by directory: names, or the error kind.
    type Dirs = BTreeMap<&'static str, Result<Vec<&'static str>, ErrorKind>>;

    /// Run the check; also returns the directories read and targets verified.
    fn check_with(
        cfg: &Config,
        dirs: &Dirs,
        unverified: &[&str],
    ) -> (DeliveryCheck, Vec<String>, Vec<String>) {
        let read = RefCell::new(Vec::new());
        let verified = RefCell::new(Vec::new());
        let out = delivery_check(
            cfg,
            today(),
            &|t: &Target| {
                verified.borrow_mut().push(t.label.clone());
                if unverified.contains(&t.label.as_str()) {
                    Err(format!("{} is not mounted", t.mount))
                } else {
                    Ok(())
                }
            },
            &|p: &Path| {
                read.borrow_mut().push(p.display().to_string());
                match dirs.get(p.to_str().unwrap()) {
                    Some(Ok(names)) => Ok(names.iter().map(|n| n.to_string()).collect()),
                    Some(Err(kind)) => Err(std::io::Error::new(*kind, "scripted")),
                    None => panic!("unscripted directory {}", p.display()),
                }
            },
        );
        (out, read.into_inner(), verified.into_inner())
    }

    fn both_fresh() -> Dirs {
        let names = vec!["srv.20261110", "data.20261109"];
        [
            ("/mnt/big/nvme", Ok(names.clone())),
            ("/mnt/small/nvme", Ok(names)),
        ]
        .into_iter()
        .collect()
    }

    #[test]
    fn civil_days_counts_days_since_the_epoch_and_refuses_a_date_that_is_not_one() {
        assert_eq!(civil_days(1970, 1, 1), Some(0));
        assert_eq!(civil_days(1969, 12, 31), Some(-1));
        assert_eq!(civil_days(2026, 11, 10), Some(20767));
        assert_eq!(civil_days(2000, 3, 1), Some(11017));
        assert_eq!(civil_days(2024, 2, 29), Some(19782));
        assert_eq!(civil_days(2026, 2, 29), None, "2026 is no leap year");
        assert_eq!(civil_days(1900, 2, 29), None, "1900 is no leap year");
        assert_eq!(civil_days(2026, 4, 31), None);
        assert_eq!(civil_days(2026, 13, 1), None);
        assert_eq!(civil_days(2026, 0, 1), None);
        assert_eq!(civil_days(2026, 1, 0), None);
    }

    #[test]
    fn when_no_target_mounts_every_target_a_pair_goes_to_fails_once() {
        let failed = unmounted_targets(&delivery_config(&[]), "no drive");
        assert_eq!(
            failed,
            vec![
                (
                    "big".to_string(),
                    "no target could be mounted: no drive".to_string()
                ),
                (
                    "small".to_string(),
                    "no target could be mounted: no drive".to_string()
                ),
            ]
        );
        let narrowed = unmounted_targets(&delivery_config(&["small"]), "no drive");
        assert_eq!(narrowed.len(), 1, "{narrowed:?}");
        assert_eq!(narrowed[0].0, "small");
        assert!(unmounted_targets(&Config::default(), "x").is_empty());
    }

    #[test]
    fn a_struct_tms_date_is_read_with_its_1900_and_0_based_month_offsets() {
        assert_eq!(days_from_tm(126, 10, 10), today());
        assert_eq!(days_from_tm(70, 0, 1), 0);
        assert_eq!(days_from_tm(124, 1, 29), 19_782);
    }

    #[test]
    fn a_target_holding_a_fresh_snapshot_of_every_pair_is_clean() {
        let (out, read, verified) = check_with(&delivery_config(&[]), &both_fresh(), &[]);
        assert_eq!(
            out,
            DeliveryCheck {
                targets_checked: 2,
                ..Default::default()
            }
        );
        assert_eq!(read, ["/mnt/big/nvme", "/mnt/small/nvme"]);
        assert_eq!(verified, ["big", "small"]);
    }

    #[test]
    fn a_pair_newer_than_the_limit_is_fresh_and_one_day_older_is_not() {
        // The limit is 3 days: 2026-11-07 is 3 days old, 2026-11-06 is 4.
        let mut dirs = both_fresh();
        dirs.insert("/mnt/big/nvme", Ok(vec!["srv.20261107", "data.20261106"]));
        let (out, ..) = check_with(&delivery_config(&["big"]), &dirs, &[]);
        assert_eq!(
            out.undelivered,
            vec![UndeliveredPair {
                target: "big".into(),
                source_label: "ssd".into(),
                subvolume: "@data".into(),
                snapshot_name: "data".into(),
                newest: Some("2026-11-06".into()),
                age_days: Some(4),
                limit_days: 3,
            }]
        );
        assert_eq!(out.targets_checked, 1);
    }

    #[test]
    fn the_limit_is_the_configured_one() {
        let mut cfg = delivery_config(&["big"]);
        cfg.doctor.max_snapshot_age_days = 10;
        let mut dirs = both_fresh();
        dirs.insert("/mnt/big/nvme", Ok(vec!["srv.20261101", "data.20261030"]));
        let (out, ..) = check_with(&cfg, &dirs, &[]);
        assert_eq!(out.undelivered.len(), 1, "{out:?}");
        assert_eq!(
            (
                out.undelivered[0].subvolume.as_str(),
                out.undelivered[0].limit_days
            ),
            ("@data", 10)
        );
    }

    #[test]
    fn a_pair_with_no_snapshot_at_all_is_undelivered_without_a_date() {
        let mut dirs = both_fresh();
        dirs.insert("/mnt/small/nvme", Ok(vec!["srv.20261110"]));
        let (out, ..) = check_with(&delivery_config(&[]), &dirs, &[]);
        assert_eq!(out.undelivered.len(), 1, "{out:?}");
        let u = &out.undelivered[0];
        assert_eq!(
            (
                u.target.as_str(),
                u.subvolume.as_str(),
                &u.newest,
                &u.age_days
            ),
            ("small", "@data", &None, &None)
        );
    }

    /// Only `<snapshot name>.<timestamp>` is a snapshot of the pair; the
    /// newest of several counts.
    #[test]
    fn only_a_name_dot_timestamp_entry_counts_and_the_newest_wins() {
        let mut dirs = both_fresh();
        dirs.insert(
            "/mnt/big/nvme",
            Ok(vec![
                "srv.20261001",
                "srv.20261109_2",
                "srv.20261108T0300",
                "srv.latest",
                "srv.20261345",
                "srv-old.20261110",
                "srv.bak.20261110",
                "mysrv.20261110",
                "srv",
                "data.20261110T031500+0200",
            ]),
        );
        let (out, ..) = check_with(&delivery_config(&["big"]), &dirs, &[]);
        assert_eq!(out.undelivered, Vec::new(), "{out:?}");
        // With the one that counts gone, `srv` is the 2026-10-01 one: stale.
        dirs.insert(
            "/mnt/big/nvme",
            Ok(vec![
                "srv.20261001",
                "srv.latest",
                "srv.20261345",
                "srv-old.20261110",
                "srv.bak.20261110",
                "mysrv.20261110",
                "srv",
                "data.20261110",
            ]),
        );
        let (out, ..) = check_with(&delivery_config(&["big"]), &dirs, &[]);
        assert_eq!(out.undelivered.len(), 1, "{out:?}");
        assert_eq!(out.undelivered[0].newest.as_deref(), Some("2026-10-01"));
        assert_eq!(out.undelivered[0].age_days, Some(40));
    }

    #[test]
    fn a_snapshot_dated_after_today_is_not_stale() {
        let mut dirs = both_fresh();
        dirs.insert("/mnt/big/nvme", Ok(vec!["srv.20261112", "data.20261110"]));
        let (out, ..) = check_with(&delivery_config(&["big"]), &dirs, &[]);
        assert_eq!(out.undelivered, Vec::new());
    }

    /// A target that is verified and has no directory for the source has
    /// been sent nothing: every pair is undelivered. It is not an unreadable
    /// target.
    #[test]
    fn a_missing_directory_on_a_verified_target_is_every_pair_undelivered() {
        let mut dirs = both_fresh();
        dirs.insert("/mnt/small/nvme", Err(ErrorKind::NotFound));
        let (out, ..) = check_with(&delivery_config(&[]), &dirs, &[]);
        assert_eq!(out.targets_checked, 2);
        assert!(out.targets_failed.is_empty(), "{out:?}");
        let names: Vec<_> = out
            .undelivered
            .iter()
            .map(|u| (u.target.as_str(), u.subvolume.as_str()))
            .collect();
        assert_eq!(names, [("small", "@srv"), ("small", "@data")]);
    }

    /// "Could not read" never reads as "fresh": no pair of that target is
    /// reported or counted, the target is a failure, and another target is
    /// still checked.
    #[test]
    fn a_target_that_cannot_be_listed_is_a_failure_and_never_fresh() {
        let mut dirs = both_fresh();
        dirs.insert("/mnt/big/nvme", Err(ErrorKind::PermissionDenied));
        let (out, ..) = check_with(&delivery_config(&[]), &dirs, &[]);
        assert_eq!(out.targets_checked, 1, "{out:?}");
        assert_eq!(out.targets_failed.len(), 1, "{out:?}");
        let (label, why) = &out.targets_failed[0];
        assert_eq!(label, "big");
        assert!(
            why.contains("/mnt/big/nvme") && why.contains("scripted"),
            "{why}"
        );
        assert!(out.undelivered.is_empty(), "{out:?}");
    }

    #[test]
    fn a_target_that_is_not_the_verified_drive_is_never_read() {
        let (out, read, verified) = check_with(&delivery_config(&[]), &both_fresh(), &["small"]);
        assert_eq!(out.targets_checked, 1);
        assert_eq!(
            out.targets_failed,
            vec![("small".to_string(), "/mnt/small is not mounted".to_string())]
        );
        assert_eq!(verified, ["big", "small"]);
        assert_eq!(
            read,
            ["/mnt/big/nvme"],
            "a bare mount point is never listed"
        );
        assert!(out.undelivered.is_empty());
    }

    #[test]
    fn a_target_no_declared_pair_goes_to_is_not_verified_or_read() {
        let (out, read, verified) = check_with(&delivery_config(&["big"]), &both_fresh(), &[]);
        assert_eq!(
            (out.targets_checked, verified, read),
            (
                1,
                vec!["big".to_string()],
                vec!["/mnt/big/nvme".to_string()]
            )
        );
    }

    #[test]
    fn retired_and_after_index_entries_are_not_declared_so_not_required() {
        let mut cfg = delivery_config(&["big"]);
        cfg.sources[0].subvolumes[1].retired = Some("2026-10-01".into());
        cfg.sources[0].subvolumes.push(SubvolConfig {
            name: "@state".into(),
            after_index: true,
            ..Default::default()
        });
        let mut dirs = both_fresh();
        dirs.insert("/mnt/big/nvme", Ok(vec!["srv.20261110"]));
        let (out, ..) = check_with(&cfg, &dirs, &[]);
        assert_eq!(out.undelivered, Vec::new());
        assert_eq!(out.targets_checked, 1);
    }

    #[test]
    fn two_sources_sending_to_one_target_read_each_directory_once() {
        let mut cfg = delivery_config(&["big"]);
        let mut hdd = source("hdd", "/hdd", &["media"]);
        hdd.target_subdirs = vec!["hdd".into()];
        hdd.target_labels = vec!["big".into()];
        cfg.sources.push(hdd);
        let mut dirs = both_fresh();
        dirs.insert("/mnt/big/hdd", Ok(vec!["media.20261110"]));
        let (out, read, verified) = check_with(&cfg, &dirs, &[]);
        assert_eq!(out.targets_checked, 1);
        assert_eq!(verified, ["big"], "a target is verified once");
        assert_eq!(read, ["/mnt/big/nvme", "/mnt/big/hdd"]);
        assert_eq!(out.undelivered, Vec::new());
    }

    // -- the report ------------------------------------------------------------

    fn undelivered_pair(newest: Option<(&str, i64)>) -> UndeliveredPair {
        UndeliveredPair {
            target: "big".into(),
            source_label: "ssd".into(),
            subvolume: "@data".into(),
            snapshot_name: "data".into(),
            newest: newest.map(|(d, _)| d.into()),
            age_days: newest.map(|(_, a)| a),
            limit_days: 3,
        }
    }

    #[test]
    fn an_undelivered_pair_is_drift_and_an_unread_target_is_a_fault_that_is_not_drift() {
        let drift = DriftReport {
            volumes_checked: 1,
            undelivered: vec![undelivered_pair(None)],
            ..Default::default()
        };
        assert!(drift.has_drift() && drift.not_clean());
        assert!(!drift.could_not_examine());
        let fault = DriftReport {
            volumes_checked: 1,
            targets_failed: vec![("big".into(), "not mounted".into())],
            ..Default::default()
        };
        assert!(!fault.has_drift());
        assert!(fault.not_clean() && fault.could_not_examine());
        let clean = DriftReport {
            volumes_checked: 1,
            targets_checked: 2,
            ..Default::default()
        };
        assert!(!clean.not_clean() && !clean.could_not_examine());
    }

    #[test]
    fn the_report_names_each_undelivered_pair_and_each_target_not_read() {
        let report = DriftReport {
            volumes_checked: 1,
            targets_checked: 1,
            targets_failed: vec![("small".into(), "/mnt/small is not mounted".into())],
            undelivered: vec![
                undelivered_pair(Some(("2026-11-06", 4))),
                UndeliveredPair {
                    subvolume: "@x".into(),
                    ..undelivered_pair(None)
                },
            ],
            ..Default::default()
        };
        let text = format_report(&report);
        assert!(
            text.contains("  Status: DRIFT DETECTED — FAILURE\n"),
            "{text}"
        );
        for line in [
            "  Targets checked       1\n",
            "  Targets failed        1\n",
            "  Undelivered pairs     2\n",
            "UNDELIVERED — a target holds no snapshot of a declared pair newer than its limit\n",
            "  @data  (source: ssd, target: big): newest snapshot 2026-11-06, 4 days old (limit 3)\n",
            "  @x  (source: ssd, target: big): no snapshot at all (limit 3 days)\n",
            "TARGETS NOT CHECKED\n",
            "  small: /mnt/small is not mounted\n",
        ] {
            assert!(text.contains(line), "missing {line:?} in\n{text}");
        }
        let clean = format_report(&DriftReport {
            volumes_checked: 1,
            targets_checked: 2,
            ..Default::default()
        });
        assert!(clean.contains("NO DRIFT — CLEAN"), "{clean}");
        assert!(!clean.contains("UNDELIVERED") && !clean.contains("TARGETS NOT CHECKED"));
    }

    // -- lock plumbing (no root/btrfs needed — just file locking on tmpfiles) --

    #[test]
    fn try_acquire_locks_at_acquires_when_free() {
        let dir = tempfile::tempdir().unwrap();
        let singleton = dir.path().join("doctor.lock");
        let maintenance = dir.path().join("maintenance.lock");
        let result = try_acquire_locks_at(&singleton, &maintenance).unwrap();
        assert!(matches!(result, LockAttempt::Acquired(_)));
        assert_eq!(
            std::fs::read_to_string(&maintenance).unwrap(),
            format!("btrdasd doctor pid {}\n", std::process::id()),
            "the drift check is recorded as the maintenance lock's holder"
        );
    }

    #[test]
    fn try_acquire_locks_at_reports_singleton_busy() {
        let dir = tempfile::tempdir().unwrap();
        let singleton = dir.path().join("doctor.lock");
        let maintenance = dir.path().join("maintenance.lock");
        let held = scrub::FileLock::try_acquire(&singleton).unwrap().unwrap();
        let result = try_acquire_locks_at(&singleton, &maintenance).unwrap();
        assert!(matches!(result, LockAttempt::SingletonBusy));
        drop(held);
    }

    #[test]
    fn try_acquire_locks_at_reports_maintenance_busy() {
        let dir = tempfile::tempdir().unwrap();
        let singleton = dir.path().join("doctor.lock");
        let maintenance = dir.path().join("maintenance.lock");
        let held = MaintenanceHeld::try_acquire_at(&maintenance, "btrdasd restore browse")
            .unwrap()
            .unwrap();
        let result = try_acquire_locks_at(&singleton, &maintenance).unwrap();
        let LockAttempt::MaintenanceBusy { holder } = result else {
            panic!("the maintenance lock is held");
        };
        assert_eq!(
            holder,
            format!("btrdasd restore browse pid {}", std::process::id()),
            "the holder is named, never guessed"
        );
        drop(held);
    }

    #[test]
    fn a_volume_left_mounted_makes_the_check_not_clean_and_is_reported() {
        let clean = DriftReport {
            volumes_checked: 2,
            ..Default::default()
        };
        assert!(!clean.not_clean());
        assert!(!format_report(&clean).contains("LEFT MOUNTED"));

        let report = DriftReport {
            volumes_checked: 2,
            left_mounted: vec!["/.btrfs-nvme".into()],
            ..Default::default()
        };
        assert!(report.not_clean());
        let text = format_report(&report);
        assert!(text.contains("Status: DRIFT DETECTED — FAILURE"), "{text}");
        assert!(
            text.contains("LEFT MOUNTED — the check mounted these and could not unmount them"),
            "{text}"
        );
        assert!(text.contains("\n  /.btrfs-nvme\n"), "{text}");
    }

    // -- the volume's top level (bd DAS-Backup-Manager-xtnq) ------------------

    #[test]
    fn an_unconfigured_top_level_is_missing_and_says_what_it_is() {
        use crate::adopt::TOP_LEVEL;
        let cfg = test_config();
        let listings = [listing("/.btrfs-hdd", Ok(s(&[TOP_LEVEL, "@a", "@b"])))];
        let report = drift_from_listings(&cfg, &listings);
        assert_eq!(
            report.missing,
            vec![MissingSubvolume {
                volume: "/.btrfs-hdd".into(),
                source_labels: vec!["hdd".into()],
                name: TOP_LEVEL.into(),
            }]
        );
        assert!(report.not_clean());
        let text = format_report(&report);
        assert!(
            text.contains("  . (top level)  (volume /.btrfs-hdd, source(s): hdd)\n"),
            "{text}"
        );
    }

    #[test]
    fn a_configured_or_deliberately_excluded_top_level_is_clean() {
        use crate::adopt::{TOP_LEVEL, TOP_LEVEL_EXCLUDE};
        let listings = [listing("/.btrfs-hdd", Ok(s(&[TOP_LEVEL, "@a", "@b"])))];
        let mut cfg = test_config();
        cfg.subvolumes.exclude = s(&[TOP_LEVEL_EXCLUDE]);
        assert!(!drift_from_listings(&cfg, &listings).not_clean());

        // A wildcard does not exclude it.
        cfg.subvolumes.exclude = s(&["*"]);
        let report = drift_from_listings(&cfg, &listings);
        assert_eq!(report.missing.len(), 1, "{:?}", report.missing);

        let mut cfg = test_config();
        cfg.sources[0].subvolumes.push(SubvolConfig {
            name: TOP_LEVEL.into(),
            snapshot_name: Some("btrfs-hdd-toplevel".into()),
            ..Default::default()
        });
        assert!(!drift_from_listings(&cfg, &listings).not_clean());
        // Configured, and a listing that lacks it (one list_volume never
        // produces): the top level exists while its volume does, so it is
        // never reported stale.
        let without = [listing("/.btrfs-hdd", Ok(s(&["@a", "@b"])))];
        let report = drift_from_listings(&cfg, &without);
        assert!(report.stale.is_empty(), "{:?}", report.stale);
    }

    /// Review of bc2c8a4: a top-level entry in a source that reaches a mirror
    /// is not backed up — the renderer refuses it — so it is missing.
    #[test]
    fn a_top_level_in_a_mirror_reaching_source_is_missing() {
        use crate::adopt::TOP_LEVEL;
        use crate::config::{Target, TargetRole};
        let listings = [listing("/.btrfs-hdd", Ok(s(&[TOP_LEVEL, "@a", "@b"])))];
        let mut cfg = test_config();
        cfg.sources[0].subvolumes.push(SubvolConfig {
            name: TOP_LEVEL.into(),
            snapshot_name: Some("btrfs-hdd-toplevel".into()),
            ..Default::default()
        });
        let target = |label: &str, role| Target {
            label: label.into(),
            serial: "S".into(),
            serials: vec!["S".into()],
            mount_uuid: None,
            mount: format!("/mnt/{label}"),
            role,
            retention: Default::default(),
            display_name: String::new(),
            esp_partuuid: None,
        };
        cfg.targets = vec![target("big", TargetRole::Primary)];
        assert!(!drift_from_listings(&cfg, &listings).not_clean());
        cfg.targets.push(target("small", TargetRole::Mirror));
        let report = drift_from_listings(&cfg, &listings);
        assert_eq!(
            report
                .missing
                .iter()
                .map(|m| m.name.as_str())
                .collect::<Vec<_>>(),
            [TOP_LEVEL]
        );
        // Scoped to the primary by name: backed up, clean again.
        cfg.sources[0].target_labels = vec!["big".into()];
        assert!(!drift_from_listings(&cfg, &listings).not_clean());
    }
}
