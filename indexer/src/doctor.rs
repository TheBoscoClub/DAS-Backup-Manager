//! Subvolume drift detector (`btrdasd doctor --check-drift`, bd DAS-Backup-Manager-01u).
//!
//! Finds two classes of drift between `config.toml` and what actually exists on
//! each source filesystem:
//!
//! - **Missing** — a subvolume exists on disk but no live `[[source]].subvolume`
//!   entry references it (an entry marked `retired` does not count: it is not
//!   backed up, so a subvolume whose only entry is retired is missing). This is the dangerous case: the 2026-05-17 audit found ~30
//!   subvolumes (all of `ClaudeCodeProjects/*`, audiobook app state, `@docker`,
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
        // A retired entry is not backed up. If its subvolume is back on disk it
        // must read as missing, because sync will revive it rather than ignore it.
        for sv in source.subvolumes.iter().filter(|e| e.retired.is_none()) {
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
    /// Volumes this check mounted and could not unmount again (bd
    /// DAS-Backup-Manager-5oc).
    pub left_mounted: Vec<String>,
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
        self.has_drift() || !self.volumes_failed.is_empty() || !self.left_mounted.is_empty()
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
fn perform_drift_check(config: &Config, progress: &dyn ProgressCallback) -> DriftReport {
    let mut guard = mount::ensure_sources_mounted(config, progress);
    let listings =
        crate::adopt::list_volumes(config, &crate::fsutil::SystemRunner, &health::is_mountpoint);
    let mut report = drift_from_listings(config, &listings);
    report.left_mounted = guard.unmount(progress);
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
                m.name,
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
        let on_disk = s(&["@", "@home", "ClaudeCodeProjects/new-project"]);
        let configured = s(&["@", "@home"]);
        assert_eq!(
            compute_missing(&on_disk, &configured, &[]),
            s(&["ClaudeCodeProjects/new-project"])
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
                    manual_only: false,
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
                name: "bosco-media/video".into(),
            }],
            stale: Vec::new(),
            left_mounted: Vec::new(),
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
            text.contains("  bosco-media/video  (volume /.btrfs-hdd, source(s): hdd-media)\n"),
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
                name: "ClaudeCodeProjects/new-project".into(),
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
            name: "ClaudeCodeProjects/deleted-project".into(),
        });
        let text = format_report(&report);
        assert!(
            text.contains("STALE CONFIG ENTRIES (configured, not retired, not found on disk)"),
            "{text}"
        );
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
}
