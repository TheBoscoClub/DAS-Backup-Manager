//! Subvolume drift detector (`btrdasd doctor --check-drift`, bd DAS-Backup-Manager-01u).
//!
//! Finds two classes of drift between `config.toml` and what actually exists on
//! each source filesystem:
//!
//! - **Missing** — a subvolume exists on disk but no `[[source]].subvolume` entry
//!   references it. This is the dangerous case: the 2026-05-17 audit found ~30
//!   subvolumes (all of `ClaudeCodeProjects/*`, audiobook app state, `@docker`,
//!   `@hibp`, Steam libraries, ISOs) silently never backed up, because daily
//!   backups of the *configured* subvolumes kept succeeding and masked the gap.
//! - **Stale** — a `[[source]].subvolume` entry references a name that no longer
//!   exists on disk (renamed or deleted project, never cleaned out of config).
//!
//! # Design: pure core, thin orchestration shell
//!
//! Every decision that can be made from data alone — parsing `btrfs subvolume
//! list` output, applying exclusions, computing the missing/stale sets,
//! categorizing a missing subvolume as irreplaceable vs. rebuildable, matching a
//! glob pattern — is a pure function over `Vec<String>`/structs, fixture-tested
//! below with no root privileges and no real btrfs filesystem. Only
//! [`run_drift_check`] and its helpers touch locks, mounts, or spawn `btrfs`.
//!
//! # Why a real mountpoint check gates every `btrfs subvolume list` call
//!
//! `btrfs subvolume list <path>` does not fail on a path that merely *isn't a
//! btrfs mountpoint* — the DAS source volumes (`/.btrfs-nvme`, `/.btrfs-hdd`,
//! `/.btrfs-ssd`) are plain empty directories on the host's root filesystem when
//! unmounted, so an unmounted-and-unguarded call silently lists the *root
//! filesystem's* subvolumes instead and returns a normal, successful-looking
//! result — this was reproduced live while investigating this feature (`sudo
//! btrfs subvolume list /.btrfs-hdd` on an unmounted `/.btrfs-hdd` returned the
//! NVMe root filesystem's `@`/`@home`/`@root`/`@log`/... subvolumes). That is the
//! same "bare mountpoint falls through to the parent filesystem" trap documented
//! for backup targets in `.claude/rules/backup.md` (bd DAS-Backup-Manager-9on),
//! applied to sources instead of targets. [`list_mounted_subvolumes`] therefore
//! requires [`health::is_mountpoint`] to confirm the path is a real mountpoint
//! before ever trusting `btrfs subvolume list`'s output for it — a silent
//! wrong-filesystem read here would corrupt both the missing and stale sets.

use std::path::Path;
use std::process::Command;

use crate::config::Config;
use crate::health;
use crate::mount;
use crate::progress::{LogLevel, ProgressCallback};
use crate::scrub;

/// Non-blocking singleton lock — a second concurrent doctor run skips rather
/// than queues (this is a fast, cheap, read-mostly check; queuing behind
/// another instance would gain nothing).
pub const DOCTOR_LOCK_PATH: &str = "/run/das-doctor.lock";

/// Subvolume-name substrings (case-insensitive) that mark a missing subvolume
/// as "probably rebuildable" rather than "likely irreplaceable". Deliberately a
/// fixed, non-configurable list — `[doctor].exclude` is for suppressing drift
/// reports entirely, this list only changes how a still-reported item is
/// categorized.
const REBUILDABLE_PATTERNS: &[&str] = &[
    "cache",
    "tmp",
    "steam",
    "docker",
    "build",
    "target",
    "node_modules",
    ".cargo",
    "iso",
];

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
/// **This does not cover every backup code path.** The manual CLI
/// subcommands `btrdasd backup run` / `snapshot` / `send` / `boot-archive`
/// (`backup.rs`, invoked directly by an operator rather than through the
/// scheduled systemd timer) take no lock at all — `backup.rs` has no
/// reference to `MAINTENANCE_LOCK_PATH` or `scrub::FileLock` anywhere. A
/// `btrdasd doctor` run and a concurrent manual `btrdasd backup` invocation
/// can therefore both mount/unmount the same source volumes at once today;
/// the maintenance lock only serializes doctor/scrub against the *scheduled*
/// backup path. This is a real, currently-uncovered gap, not a design
/// decision — closing it (having the manual backup subcommands take the same
/// lock) is tracked separately (see bd DAS-Backup-Manager, manual-backup lock
/// gap) rather than folded into this feature.
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
    maintenance: scrub::FileLock,
    #[allow(dead_code)]
    singleton: scrub::FileLock,
}

/// Outcome of trying to acquire both locks without blocking.
enum LockAttempt {
    Acquired(DoctorLocks),
    /// Another `btrdasd doctor` invocation holds the singleton lock.
    SingletonBusy,
    /// A backup or scrub pass holds the shared maintenance lock.
    MaintenanceBusy,
}

fn try_acquire_locks_at(
    singleton_path: &Path,
    maintenance_path: &Path,
) -> Result<LockAttempt, DoctorError> {
    let Some(singleton) = scrub::FileLock::try_acquire(singleton_path)? else {
        return Ok(LockAttempt::SingletonBusy);
    };
    let Some(maintenance) = scrub::FileLock::try_acquire(maintenance_path)? else {
        return Ok(LockAttempt::MaintenanceBusy);
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
// Pure core — glob matching, exclusion, categorization, drift computation
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

/// Whether `path` matches one of the built-in exclusions: inside a
/// `.snapshots/` or `.btrbk-snapshots/` directory (Snapper-managed or btrbk's
/// own — nested at any depth, e.g. `Audiobooks/.btrbk-snapshots/...`), or is
/// exactly `@tmp` / `@var-tmp`.
pub fn is_builtin_excluded(path: &str) -> bool {
    let in_snapshot_dir = path
        .split('/')
        .any(|component| component == ".snapshots" || component == ".btrbk-snapshots");
    in_snapshot_dir || path == "@tmp" || path == "@var-tmp"
}

/// Whether `path` matches any user-supplied `[doctor].exclude` glob pattern.
pub fn is_user_excluded(path: &str, patterns: &[String]) -> bool {
    patterns.iter().any(|pat| glob_match(pat, path))
}

/// Category assigned to a missing (on-disk, not-in-config) subvolume.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriftCategory {
    /// No name match against [`REBUILDABLE_PATTERNS`] — treated as data that
    /// cannot be regenerated (code, databases, configs, media).
    Irreplaceable,
    /// Name contains a rebuildable-pattern substring (cache, build output,
    /// package manager state, ISOs) — still reported, but flagged as lower
    /// priority to add to config.
    Rebuildable,
}

/// Categorize a subvolume name by case-insensitive substring match against
/// [`REBUILDABLE_PATTERNS`].
pub fn categorize(name: &str) -> DriftCategory {
    let lower = name.to_lowercase();
    if REBUILDABLE_PATTERNS.iter().any(|pat| lower.contains(pat)) {
        DriftCategory::Rebuildable
    } else {
        DriftCategory::Irreplaceable
    }
}

/// Parse `btrfs subvolume list <mount>` stdout into subvolume paths (the last
/// whitespace-delimited field of each line — the same parsing convention
/// already used by `backup::find_latest_btrbk_snapshot`).
pub fn parse_subvolume_list_output(stdout: &str) -> Vec<String> {
    stdout
        .lines()
        .filter_map(|line| line.split_whitespace().last())
        .map(|s| s.to_string())
        .collect()
}

/// Result of comparing an on-disk subvolume list against a configured list.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct DriftDiff {
    /// On disk, not in config — `(name, category)`, sorted by name.
    pub missing: Vec<(String, DriftCategory)>,
    /// In config, not on disk — sorted by name.
    pub stale: Vec<String>,
}

/// Compute missing/stale drift between an on-disk subvolume list and a
/// configured subvolume list. `user_exclude` patterns (plus the built-in
/// exclusions) are applied only to the *missing* side — an excluded on-disk
/// path is simply invisible to this check, never reported either way. The
/// *stale* side is never filtered: a configured entry that happens to match an
/// exclude pattern is still a legitimate stale-config finding if it's absent
/// on disk.
pub fn compute_missing(
    on_disk: &[String],
    configured: &[String],
    user_exclude: &[String],
) -> Vec<(String, DriftCategory)> {
    let mut missing: Vec<(String, DriftCategory)> = on_disk
        .iter()
        .filter(|p| !is_builtin_excluded(p) && !is_user_excluded(p, user_exclude))
        .filter(|p| !configured.contains(p))
        .map(|p| (p.clone(), categorize(p)))
        .collect();
    missing.sort_by(|a, b| a.0.cmp(&b.0));
    missing.dedup_by(|a, b| a.0 == b.0);
    missing
}

/// Compute stale config entries: names in `configured` with no exact match in
/// `on_disk`.
pub fn compute_stale(on_disk: &[String], configured: &[String]) -> Vec<String> {
    let mut stale: Vec<String> = configured
        .iter()
        .filter(|c| !on_disk.contains(c))
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
        for sv in &source.subvolumes {
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
    pub category: DriftCategory,
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
#[derive(Debug, Default)]
pub struct DriftReport {
    /// Volumes successfully mounted and listed.
    pub volumes_checked: usize,
    /// Volumes that could not be examined — `(volume, reason)`. A non-empty
    /// list here does not by itself fail the check (see [`DriftReport::ran`])
    /// unless *every* configured volume failed.
    pub volumes_failed: Vec<(String, String)>,
    pub missing: Vec<MissingSubvolume>,
    pub stale: Vec<StaleSubvolume>,
}

impl DriftReport {
    /// Whether any drift (missing or stale) was found.
    pub fn has_drift(&self) -> bool {
        !self.missing.is_empty() || !self.stale.is_empty()
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
        self.has_drift() || !self.volumes_failed.is_empty()
    }
}

/// Outcome of a full `run_drift_check` invocation.
pub enum DoctorOutcome {
    /// A lock was held by another process — this invocation deferred without
    /// examining anything. Always maps to exit code 0 (see `main.rs`).
    Deferred { reason: String },
    /// The check ran (successfully or not) — see [`DriftReport::ran`].
    Ran(DriftReport),
}

/// Run `btrfs subvolume list` against `mount_path`, requiring it to already be
/// a real, live mountpoint (see the module doc comment for why this guard is
/// safety-critical, not optional).
fn list_mounted_subvolumes(mount_path: &str) -> Result<Vec<String>, String> {
    if !health::is_mountpoint(Path::new(mount_path)) {
        return Err(format!(
            "{mount_path} is not mounted — refusing to run 'btrfs subvolume list' \
             against it (an unmounted bare directory would silently list whatever \
             filesystem it falls through to, not this source)"
        ));
    }
    let output = Command::new("btrfs")
        .args(["subvolume", "list", mount_path])
        .output()
        .map_err(|e| format!("failed to execute btrfs: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "'btrfs subvolume list {mount_path}' failed: {}",
            stderr.trim()
        ));
    }
    Ok(parse_subvolume_list_output(&String::from_utf8_lossy(
        &output.stdout,
    )))
}

/// Mount sources, list + compare every configured volume, unmount, and build
/// the report. Never returns `Err` — per-volume failures are recorded in
/// [`DriftReport::volumes_failed`] instead, matching the scrub engine's
/// "some targets fail, the pass still ran" philosophy (`exit_code_for_pass`
/// in `main.rs`): a check that examined 3 of 4 volumes still ran.
fn perform_drift_check(config: &Config, progress: &dyn ProgressCallback) -> DriftReport {
    let mut guard = mount::ensure_sources_mounted(config, progress);

    let groups = group_sources_by_volume(config);
    let mut report = DriftReport::default();
    // volume -> on-disk list, so the per-source stale check below doesn't
    // re-run `btrfs subvolume list` for volumes multiple sources share.
    let mut on_disk_by_volume: Vec<(String, Vec<String>)> = Vec::new();

    for group in &groups {
        match list_mounted_subvolumes(&group.volume) {
            Ok(on_disk) => {
                report.volumes_checked += 1;
                let missing = compute_missing(&on_disk, &group.configured, &config.doctor.exclude);
                for (name, category) in missing {
                    report.missing.push(MissingSubvolume {
                        volume: group.volume.clone(),
                        source_labels: group.source_labels.clone(),
                        name,
                        category,
                    });
                }
                on_disk_by_volume.push((group.volume.clone(), on_disk));
            }
            Err(detail) => {
                report.volumes_failed.push((group.volume.clone(), detail));
            }
        }
    }

    // Stale check is per-source (each config entry belongs to exactly one
    // source), using the already-fetched on-disk list for that source's
    // volume — skipped entirely for a source whose volume failed above.
    for source in &config.sources {
        let Some((_, on_disk)) = on_disk_by_volume.iter().find(|(v, _)| *v == source.volume) else {
            continue;
        };
        let configured: Vec<String> = source.subvolumes.iter().map(|sv| sv.name.clone()).collect();
        for name in compute_stale(on_disk, &configured) {
            report.stale.push(StaleSubvolume {
                volume: source.volume.clone(),
                source_label: source.label.clone(),
                name,
            });
        }
    }

    guard.unmount(progress);
    report
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
        LockAttempt::MaintenanceBusy => {
            let reason = "maintenance lock held (backup/scrub in progress?) — skipping drift check"
                .to_string();
            progress.on_log(LogLevel::Info, &reason);
            Ok(DoctorOutcome::Deferred { reason })
        }
        LockAttempt::Acquired(_locks) => {
            progress.on_stage("Checking for subvolume drift", 1);
            let report = perform_drift_check(config, progress);
            progress.on_complete(
                !report.has_drift(),
                &format!(
                    "{} volume(s) checked, {} missing, {} stale",
                    report.volumes_checked,
                    report.missing.len(),
                    report.stale.len()
                ),
            );
            Ok(DoctorOutcome::Ran(report))
            // `_locks` drops here, releasing both file locks.
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
/// Deliberately does not try to pick a single `[[source]]` block to suggest
/// adding a missing subvolume to when multiple sources share its volume — the
/// four `/.btrfs-hdd` sources in the production config (`hdd-projects`,
/// `hdd-media`, `hdd-system`, `hdd-audiobooks`) split that one filesystem by
/// *purpose*, and only a human knows whether a newly-found subvolume is a new
/// project, new media, or something else. The snippet instead lists every
/// source label sharing the volume so the operator can pick.
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

    if !report.volumes_failed.is_empty() {
        r.push_str(&format!("\nVOLUMES NOT CHECKED\n{thin}\n"));
        for (volume, detail) in &report.volumes_failed {
            r.push_str(&format!("  {volume}: {detail}\n"));
        }
    }

    if !report.missing.is_empty() {
        let irreplaceable: Vec<&MissingSubvolume> = report
            .missing
            .iter()
            .filter(|m| m.category == DriftCategory::Irreplaceable)
            .collect();
        let rebuildable: Vec<&MissingSubvolume> = report
            .missing
            .iter()
            .filter(|m| m.category == DriftCategory::Rebuildable)
            .collect();

        if !irreplaceable.is_empty() {
            r.push_str(&format!(
                "\nMISSING — LIKELY IRREPLACEABLE (add to config)\n{thin}\n"
            ));
            for m in &irreplaceable {
                r.push_str(&format!(
                    "  {}  (volume {}, source(s): {})\n",
                    m.name,
                    m.volume,
                    m.source_labels.join(", ")
                ));
            }
        }
        if !rebuildable.is_empty() {
            r.push_str(&format!(
                "\nMISSING — PROBABLY REBUILDABLE (review before adding)\n{thin}\n"
            ));
            for m in &rebuildable {
                r.push_str(&format!(
                    "  {}  (volume {}, source(s): {})\n",
                    m.name,
                    m.volume,
                    m.source_labels.join(", ")
                ));
            }
        }

        if !irreplaceable.is_empty() {
            r.push_str(&format!(
                "\nSUGGESTED config.toml ADDITIONS (irreplaceable set — pick the \
                 appropriate [[source]] block per volume)\n{thin}\n"
            ));
            for m in &irreplaceable {
                r.push_str(&format!(
                    "  # volume {} — add under one of: {}\n",
                    m.volume,
                    m.source_labels.join(", ")
                ));
                r.push_str("  [[source.subvolumes]]\n");
                r.push_str(&format!("  name = \"{}\"\n", m.name));
                r.push_str("  manual_only = false\n\n");
            }
        }
    }

    if !report.stale.is_empty() {
        r.push_str(&format!(
            "\nSTALE CONFIG ENTRIES (configured, not found on disk)\n{thin}\n"
        ));
        for s in &report.stale {
            r.push_str(&format!(
                "  {}  (source: {}, volume: {})\n",
                s.name, s.source_label, s.volume
            ));
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

    // -- is_builtin_excluded ------------------------------------------------

    #[test]
    fn builtin_excludes_snapshots_dir() {
        assert!(is_builtin_excluded(".snapshots/4460/snapshot"));
        assert!(is_builtin_excluded("@root/.snapshots/3811/snapshot"));
        assert!(is_builtin_excluded(".snapshots"));
    }

    #[test]
    fn builtin_excludes_btrbk_snapshots_dir_nested() {
        assert!(is_builtin_excluded(".btrbk-snapshots/root.20260801T0325"));
        assert!(is_builtin_excluded(
            "Audiobooks/.btrbk-snapshots/foo.20260101T0000"
        ));
        assert!(is_builtin_excluded(".btrbk-snapshots"));
    }

    #[test]
    fn builtin_excludes_tmp_subvols() {
        assert!(is_builtin_excluded("@tmp"));
        assert!(is_builtin_excluded("@var-tmp"));
        // Substring, not exact match, must NOT be excluded — only the exact
        // top-level names are built-in exclusions.
        assert!(!is_builtin_excluded("@tmpfiles-extra"));
    }

    #[test]
    fn builtin_does_not_exclude_ordinary_paths() {
        assert!(!is_builtin_excluded(
            "ClaudeCodeProjects/DAS-Backup-Manager"
        ));
        assert!(!is_builtin_excluded("@docker"));
    }

    // -- is_user_excluded ----------------------------------------------------

    #[test]
    fn user_exclude_patterns() {
        let patterns = vec!["*.cache".to_string(), "Downloads/*".to_string()];
        assert!(is_user_excluded("build.cache", &patterns));
        assert!(is_user_excluded("Downloads/movie.iso", &patterns));
        assert!(!is_user_excluded("ClaudeCodeProjects/foo", &patterns));
    }

    #[test]
    fn user_exclude_empty_excludes_nothing() {
        assert!(!is_user_excluded("anything", &[]));
    }

    // -- categorize -----------------------------------------------------------

    #[test]
    fn categorize_rebuildable_patterns() {
        assert_eq!(categorize("@docker"), DriftCategory::Rebuildable);
        assert_eq!(categorize("SteamLibrary"), DriftCategory::Rebuildable);
        assert_eq!(categorize("build-cache"), DriftCategory::Rebuildable);
        assert_eq!(categorize("ISOs"), DriftCategory::Rebuildable);
        assert_eq!(categorize("node_modules"), DriftCategory::Rebuildable);
        assert_eq!(categorize(".cargo"), DriftCategory::Rebuildable);
        assert_eq!(categorize("target"), DriftCategory::Rebuildable);
    }

    #[test]
    fn categorize_case_insensitive() {
        assert_eq!(categorize("STEAM-library"), DriftCategory::Rebuildable);
        assert_eq!(categorize("Build-Output"), DriftCategory::Rebuildable);
    }

    #[test]
    fn categorize_irreplaceable_by_default() {
        assert_eq!(
            categorize("ClaudeCodeProjects/DAS-Backup-Manager"),
            DriftCategory::Irreplaceable
        );
        assert_eq!(categorize("@audiobooks-db"), DriftCategory::Irreplaceable);
        assert_eq!(categorize("bosco-media"), DriftCategory::Irreplaceable);
    }

    // -- parse_subvolume_list_output ------------------------------------------

    #[test]
    fn parse_subvolume_list_basic() {
        let stdout = "ID 257 gen 1342848 top level 5 path @home\n\
                       ID 829 gen 1342847 top level 5 path @\n";
        let paths = parse_subvolume_list_output(stdout);
        assert_eq!(paths, vec!["@home", "@"]);
    }

    #[test]
    fn parse_subvolume_list_nested_paths() {
        let stdout = "ID 100 gen 1 top level 5 path ClaudeCodeProjects\n\
                       ID 101 gen 1 top level 100 path ClaudeCodeProjects/foo\n";
        let paths = parse_subvolume_list_output(stdout);
        assert_eq!(paths, vec!["ClaudeCodeProjects", "ClaudeCodeProjects/foo"]);
    }

    #[test]
    fn parse_subvolume_list_empty() {
        assert!(parse_subvolume_list_output("").is_empty());
    }

    // -- compute_missing / compute_stale -------------------------------------

    #[test]
    fn compute_missing_finds_new_subvolumes() {
        let on_disk = vec![
            "@".to_string(),
            "@home".to_string(),
            "ClaudeCodeProjects/new-project".to_string(),
        ];
        let configured = vec!["@".to_string(), "@home".to_string()];
        let missing = compute_missing(&on_disk, &configured, &[]);
        assert_eq!(missing.len(), 1);
        assert_eq!(missing[0].0, "ClaudeCodeProjects/new-project");
        assert_eq!(missing[0].1, DriftCategory::Irreplaceable);
    }

    #[test]
    fn compute_missing_excludes_builtins() {
        let on_disk = vec![
            "@".to_string(),
            ".snapshots/1/snapshot".to_string(),
            ".btrbk-snapshots/foo.20260101".to_string(),
            "@tmp".to_string(),
            "@var-tmp".to_string(),
        ];
        let configured = vec!["@".to_string()];
        let missing = compute_missing(&on_disk, &configured, &[]);
        assert!(missing.is_empty(), "expected no drift, got {missing:?}");
    }

    #[test]
    fn compute_missing_excludes_user_patterns() {
        let on_disk = vec!["@".to_string(), "scratch.cache".to_string()];
        let configured = vec!["@".to_string()];
        let missing = compute_missing(&on_disk, &configured, &["*.cache".to_string()]);
        assert!(missing.is_empty());
    }

    #[test]
    fn compute_missing_categorizes_rebuildable_separately() {
        let on_disk = vec![
            "@".to_string(),
            "ClaudeCodeProjects/foo".to_string(),
            "SteamLibrary".to_string(),
        ];
        let configured = vec!["@".to_string()];
        let missing = compute_missing(&on_disk, &configured, &[]);
        assert_eq!(missing.len(), 2);
        let steam = missing.iter().find(|(n, _)| n == "SteamLibrary").unwrap();
        assert_eq!(steam.1, DriftCategory::Rebuildable);
        let proj = missing
            .iter()
            .find(|(n, _)| n == "ClaudeCodeProjects/foo")
            .unwrap();
        assert_eq!(proj.1, DriftCategory::Irreplaceable);
    }

    #[test]
    fn compute_missing_sorted_and_deduped() {
        let on_disk = vec!["zeta".to_string(), "alpha".to_string(), "alpha".to_string()];
        let missing = compute_missing(&on_disk, &[], &[]);
        assert_eq!(
            missing.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(),
            vec!["alpha", "zeta"]
        );
    }

    #[test]
    fn compute_stale_finds_removed_subvolumes() {
        let on_disk = vec!["@".to_string(), "@home".to_string()];
        let configured = vec![
            "@".to_string(),
            "@home".to_string(),
            "@deleted-project".to_string(),
        ];
        let stale = compute_stale(&on_disk, &configured);
        assert_eq!(stale, vec!["@deleted-project"]);
    }

    #[test]
    fn compute_stale_empty_when_all_present() {
        let on_disk = vec!["@".to_string(), "@home".to_string()];
        let configured = vec!["@".to_string(), "@home".to_string()];
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
                    manual_only: false,
                    snapshot_name: None,
                })
                .collect(),
            device: "/dev/fake".into(),
            snapshot_dir: ".btrbk-snapshots".into(),
            target_subdirs: vec![],
            target_labels: vec![],
        }
    }

    #[test]
    fn group_sources_by_volume_merges_shared_volume() {
        let mut cfg = Config::default();
        cfg.sources.push(source(
            "hdd-projects",
            "/.btrfs-hdd",
            &["ClaudeCodeProjects"],
        ));
        cfg.sources.push(source(
            "hdd-media",
            "/.btrfs-hdd",
            &["bosco-media", "SteamLibrary"],
        ));
        cfg.sources
            .push(source("nvme", "/.btrfs-nvme", &["@", "@home"]));

        let groups = group_sources_by_volume(&cfg);
        assert_eq!(groups.len(), 2);
        let hdd = groups.iter().find(|g| g.volume == "/.btrfs-hdd").unwrap();
        assert_eq!(hdd.source_labels, vec!["hdd-projects", "hdd-media"]);
        assert_eq!(
            hdd.configured,
            vec!["ClaudeCodeProjects", "bosco-media", "SteamLibrary"]
        );
        let nvme = groups.iter().find(|g| g.volume == "/.btrfs-nvme").unwrap();
        assert_eq!(nvme.source_labels, vec!["nvme"]);
        assert_eq!(nvme.configured, vec!["@", "@home"]);
    }

    #[test]
    fn group_sources_by_volume_empty_config() {
        let cfg = Config::default();
        assert!(group_sources_by_volume(&cfg).is_empty());
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
            category: DriftCategory::Irreplaceable,
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
    }

    #[test]
    fn format_report_drift_contains_failure_word() {
        let mut report = DriftReport {
            volumes_checked: 1,
            ..Default::default()
        };
        report.missing.push(MissingSubvolume {
            volume: "/.btrfs-hdd".into(),
            source_labels: vec!["hdd-projects".into(), "hdd-media".into()],
            name: "ClaudeCodeProjects/new-project".into(),
            category: DriftCategory::Irreplaceable,
        });
        let text = format_report(&report);
        assert!(text.contains("FAILURE"));
        assert!(text.contains("DRIFT DETECTED"));
        assert!(text.contains("ClaudeCodeProjects/new-project"));
        assert!(text.contains("hdd-projects, hdd-media"));
        assert!(text.contains("[[source.subvolumes]]"));
        assert!(text.contains("name = \"ClaudeCodeProjects/new-project\""));
    }

    #[test]
    fn format_report_could_not_run_contains_failure_word() {
        let mut report = DriftReport::default();
        report
            .volumes_failed
            .push(("/.btrfs-hdd".into(), "not mounted".into()));
        let text = format_report(&report);
        assert!(text.contains("COULD NOT RUN — FAILURE"));
        assert!(!report.ran());
    }

    #[test]
    fn format_report_separates_rebuildable_from_irreplaceable() {
        let mut report = DriftReport {
            volumes_checked: 1,
            ..Default::default()
        };
        report.missing.push(MissingSubvolume {
            volume: "/.btrfs-hdd".into(),
            source_labels: vec!["hdd-media".into()],
            name: "SteamLibrary2".into(),
            category: DriftCategory::Rebuildable,
        });
        let text = format_report(&report);
        assert!(text.contains("PROBABLY REBUILDABLE"));
        // Rebuildable items must not get a suggested-diff snippet.
        assert!(!text.contains("name = \"SteamLibrary2\""));
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
            name: "ClaudeCodeProjects/deleted-project".into(),
        });
        let text = format_report(&report);
        assert!(text.contains("STALE CONFIG ENTRIES"));
        assert!(text.contains("ClaudeCodeProjects/deleted-project"));
        assert!(text.contains("hdd-projects"));
    }

    // -- lock plumbing (no root/btrfs needed — just file locking on tmpfiles) --

    #[test]
    fn try_acquire_locks_at_acquires_when_free() {
        let dir = tempfile::tempdir().unwrap();
        let singleton = dir.path().join("doctor.lock");
        let maintenance = dir.path().join("maintenance.lock");
        let result = try_acquire_locks_at(&singleton, &maintenance).unwrap();
        assert!(matches!(result, LockAttempt::Acquired(_)));
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
        let held = scrub::FileLock::try_acquire(&maintenance).unwrap().unwrap();
        let result = try_acquire_locks_at(&singleton, &maintenance).unwrap();
        assert!(matches!(result, LockAttempt::MaintenanceBusy));
        drop(held);
    }

    // -- list_mounted_subvolumes guard (no real btrfs needed) ------------------

    #[test]
    fn list_mounted_subvolumes_rejects_non_mountpoint() {
        let dir = tempfile::tempdir().unwrap();
        // `dir` is a real directory but not a mountpoint of anything.
        let result = list_mounted_subvolumes(dir.path().to_str().unwrap());
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not mounted"));
    }
}
