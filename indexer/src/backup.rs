use crate::btrbk_conf::{self, DeclaredPair};
use crate::config::{Config, Target, TargetRole};
use crate::db::Database;
use crate::fsutil::{CommandRunner, SystemRunner};
use crate::health;
use crate::indexer;
use crate::maintenance::{HoldsMaintenance, MaintenanceHeld};
use crate::mount;
use crate::progress::{LogLevel, ProgressCallback};
use crate::scrub;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::{Command, ExitStatus};
use std::time::UNIX_EPOCH;

// ---------------------------------------------------------------------------
// Maintenance interlock for manual backups (bd DAS-Backup-Manager-pe6)
// ---------------------------------------------------------------------------

/// Singleton lock shared with the scheduled path.
///
/// Deliberately the SAME file `scripts/backup-run.sh` takes, so a manual
/// `btrdasd backup` and the 03:00 timer contend with each other rather than
/// running two backups over one set of targets.
pub const BACKUP_LOCK_PATH: &str = "/run/das-backup.lock";

/// Both locks a manual backup holds, released on drop — maintenance first
/// (fields drop in declaration order).
pub struct BackupLocks {
    maintenance: MaintenanceHeld,
    _singleton: scrub::FileLock,
}

impl HoldsMaintenance for BackupLocks {
    fn maintenance(&self) -> &MaintenanceHeld {
        &self.maintenance
    }
}

/// Outcome of trying to take the manual-backup locks.
pub enum BackupLockAttempt {
    Acquired(Box<BackupLocks>),
    /// Another backup already holds the singleton — decline rather than queue.
    AlreadyRunning,
}

/// Take the manual-backup locks, mirroring `backup-run.sh` exactly.
///
/// The manual `btrdasd backup` subcommands mounted and unmounted the DAS
/// filesystems with no lock at all, outside the mutual-exclusion scheme that
/// `backup-run.sh`, the scrub engine and `doctor` all participate in. The
/// concrete hazard: `doctor` mounts a source, a concurrent manual backup finds it
/// already mounted and uses it unregistered, then `doctor` finishes first and
/// unmounts it mid-use.
///
/// The two halves behave differently, matching the scheduled path rather than
/// inventing a third convention:
///
/// * **singleton, non-blocking** — a second backup is redundant, not merely
///   late, so it declines instead of queueing (`flock -n` in the bash path).
/// * **maintenance, blocking** — a scrub is a peer operation, so the backup
///   waits for it. Defer, never skip.
///
/// Acquisition order is singleton then maintenance, the same order used
/// everywhere else; that shared order is what keeps the set deadlock-free.
/// `job` (`btrdasd backup send`) is recorded in the maintenance lock file as
/// its holder.
pub fn acquire_manual_locks_at(
    singleton_path: &Path,
    maintenance_path: &Path,
    job: &str,
    progress: &dyn ProgressCallback,
) -> Result<BackupLockAttempt, scrub::ScrubError> {
    let Some(singleton) = scrub::FileLock::try_acquire(singleton_path)? else {
        return Ok(BackupLockAttempt::AlreadyRunning);
    };
    let maintenance = MaintenanceHeld::acquire_blocking_at(maintenance_path, job, progress)?;
    Ok(BackupLockAttempt::Acquired(Box::new(BackupLocks {
        maintenance,
        _singleton: singleton,
    })))
}

/// Take the manual-backup locks at their production paths.
pub fn acquire_manual_locks(
    job: &str,
    progress: &dyn ProgressCallback,
) -> Result<BackupLockAttempt, scrub::ScrubError> {
    acquire_manual_locks_at(
        Path::new(BACKUP_LOCK_PATH),
        Path::new(scrub::MAINTENANCE_LOCK_PATH),
        job,
        progress,
    )
}

/// Whether to run an incremental or full backup.
///
/// **Incremental**: `btrbk snapshot` + `btrbk --preserve resume` — creates
/// snapshots, sends deltas, but skips retention cleanup.  Fast daily use.
///
/// **Full**: `btrbk run` — creates snapshots, sends them, AND enforces
/// retention policy (deletes old snapshots/backups outside retention windows).
/// The complete backup lifecycle with housekeeping.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BackupMode {
    Incremental,
    Full,
}

impl std::fmt::Display for BackupMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BackupMode::Incremental => write!(f, "incremental"),
            BackupMode::Full => write!(f, "full"),
        }
    }
}

/// Options controlling what a backup run does.
#[derive(Debug, Default)]
pub struct BackupOptions {
    /// Incremental or full. None = use schedule default.
    pub mode: Option<BackupMode>,
    /// Source labels to back up. Empty = all configured sources. btrbk is
    /// told to touch exactly these sources' subvolumes and no others.
    pub sources: Vec<String>,
    /// Target labels to send to. Empty = all available targets. btrbk is told
    /// to write to exactly these and is not told about the rest.
    pub targets: Vec<String>,
    /// Preview only — don't actually run btrbk.
    pub dry_run: bool,
    /// Create snapshots but skip send/receive.
    pub snapshot_only: bool,
    /// Send existing snapshots without creating new ones.
    pub send_only: bool,
    /// Archive boot subvolumes after backup.
    pub boot_archive: bool,
    /// Run the content indexer after backup completes.
    pub index_after: bool,
    /// Send an email report after backup.
    pub send_report: bool,
    /// The subvolume sync that started this run (`sync_before_backup`). A
    /// failed one fails the run — in its result, its `backup_runs` row and
    /// its report — and its section is carried into the report.
    pub subvolume_sync: Option<SyncSection>,
}

/// Result of a completed backup run.
#[derive(Debug)]
pub struct BackupResult {
    pub success: bool,
    pub mode: BackupMode,
    pub snapshots_created: usize,
    pub snapshots_sent: usize,
    pub snapshots_cleaned: usize,
    pub bytes_sent: u64,
    pub boot_archived: bool,
    pub indexed: bool,
    pub report_sent: bool,
    pub errors: Vec<String>,
    pub duration_secs: u64,
}

// ---------------------------------------------------------------------------
// Helper functions
// ---------------------------------------------------------------------------

/// Build a timestamp string in YYYYMMDDTHHMMSS format using SystemTime.
/// Uses libc localtime_r to convert to local time without extra dependencies.
fn format_timestamp() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time must be after Unix epoch");
    let secs = now.as_secs() as libc::time_t;

    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: secs is a valid time_t and tm is a properly allocated libc::tm.
    unsafe { libc::localtime_r(&secs, &mut tm) };

    format!(
        "{:04}{:02}{:02}T{:02}{:02}{:02}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec,
    )
}

/// Parse btrbk snapshot output and count lines that indicate a snapshot was created.
/// Count btrbk snapshot lines.  btrbk marks created snapshots with `+++`.
/// Snapshot/send counts taken from `btrbk --format=raw list latest`.
///
/// The marker parsers below read btrbk's HUMAN output, which is presentation
/// rather than interface. btrbk 0.32.7 stopped emitting the `up-to-date` string
/// the bash counters grepped for and silently zeroed `snaps_created`/`snaps_sent`
/// on every run for five weeks (bd DAS-Backup-Manager-oi0). `scripts/backup-run.sh`
/// was migrated to `--format=raw` named fields then; the Rust path was not, and
/// carried the identical defect until 0.7.20.0 (bd DAS-Backup-Manager-06p).
///
/// Deliberately a SEPARATE query rather than a flag on the run itself: the run's
/// stdout is consumed line-by-line for progress reporting, and switching that
/// stream to raw would break the progress callback. This mirrors what
/// `backup-run.sh` does, so the two implementations agree by construction.
fn btrbk_raw_listing(
    config: &Config,
    filters: &[String],
    runner: &dyn CommandRunner,
) -> Option<String> {
    let output = runner
        .output(
            Command::new("btrbk")
                .args([
                    "-c",
                    &config.general.btrbk_conf,
                    "--format=raw",
                    "list",
                    "latest",
                ])
                .args(filters),
        )
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Distinct `snapshot_subvolume='…'` values in a raw listing.
///
/// One source subvolume yields one snapshot replicated to N targets, so this is
/// genuinely a different number from [`parse_raw_send_count`] — the pre-`oi0`
/// code used one as a proxy for the other, which was never right.
fn parse_raw_snapshot_count(raw: &str) -> usize {
    raw_field_values(raw, "snapshot_subvolume")
        .collect::<std::collections::BTreeSet<_>>()
        .len()
}

/// Rows carrying a non-empty `target_subvolume='…'` in a raw listing.
fn parse_raw_send_count(raw: &str) -> usize {
    raw_field_values(raw, "target_subvolume").count()
}

/// Non-empty values of a named `key='value'` field across a raw listing.
fn raw_field_values<'a>(raw: &'a str, key: &'a str) -> impl Iterator<Item = &'a str> + 'a {
    let needle = format!("{key}='");
    raw.lines().filter_map(move |line| {
        let start = line.find(&needle)? + needle.len();
        let rest = &line[start..];
        let end = rest.find('\'')?;
        (end > 0).then(|| &rest[..end])
    })
}

fn parse_btrbk_snapshot_count(output: &str) -> usize {
    output
        .lines()
        .filter(|line| {
            let trimmed = line.trim_start();
            trimmed.starts_with("+++")
        })
        .count()
}

/// Count btrbk send lines.  btrbk marks sends with `>>>` (incremental) or
/// `***` (non-incremental/full).
fn parse_btrbk_send_count(output: &str) -> usize {
    output
        .lines()
        .filter(|line| {
            let trimmed = line.trim_start();
            trimmed.starts_with(">>>") || trimmed.starts_with("***")
        })
        .count()
}

/// Every non-blank line of a btrbk command's stderr, as a warning.
fn log_btrbk_stderr(stderr: &[u8], progress: &dyn ProgressCallback) {
    for line in String::from_utf8_lossy(stderr).lines() {
        if !line.trim().is_empty() {
            progress.on_log(LogLevel::Warning, &format!("btrbk stderr: {line}"));
        }
    }
}

/// Run a command through `runner` and return (stdout, exit status).
/// Logs stderr lines at Warning level via progress.
fn run_command(
    cmd: &mut Command,
    runner: &dyn CommandRunner,
    progress: &dyn ProgressCallback,
) -> Result<(String, ExitStatus), Box<dyn std::error::Error>> {
    let output = runner.output(cmd)?;
    log_btrbk_stderr(&output.stderr, progress);
    Ok((
        String::from_utf8_lossy(&output.stdout).into_owned(),
        output.status,
    ))
}

/// Stream a command through `runner`, applying a callback to each stdout
/// line as it is written. Stderr is collected and logged at Warning level.
/// Returns the exit status.
fn stream_command<F>(
    cmd: &mut Command,
    runner: &dyn CommandRunner,
    progress: &dyn ProgressCallback,
    mut line_cb: F,
) -> Result<ExitStatus, Box<dyn std::error::Error>>
where
    F: FnMut(&str),
{
    // Log the command being executed for diagnostics.
    progress.on_log(LogLevel::Info, &format!("stream_command: {:?}", cmd));

    let mut line_count = 0usize;
    let output = runner.stream(cmd, &mut |line| {
        line_count += 1;
        line_cb(line);
    })?;
    progress.on_log(
        LogLevel::Info,
        &format!(
            "stream_command: exit={}, stdout_lines={}",
            output.status.code().unwrap_or(-1),
            line_count
        ),
    );
    log_btrbk_stderr(&output.stderr, progress);
    Ok(output.status)
}

/// How a command ended, for a message: its exit status, or the signal that
/// killed it.
fn describe_exit(status: ExitStatus) -> String {
    match (status.code(), status.signal()) {
        (Some(code), _) => format!("exit status {code}"),
        (None, Some(signal)) => format!("killed by signal {signal}"),
        (None, None) => "no exit status".to_string(),
    }
}

/// What the steps of a backup need from the host besides their arguments.
///
/// Production only ever uses [`StepEnv::HOST`]. A test hands in a scripted
/// runner and a described verdict instead, so that no test can reach a real
/// `btrbk` or `btrfs`: a guard that is taken out leaves the scripted runner to
/// answer, not the machine.
struct StepEnv<'a> {
    /// Runs every `btrbk` and `btrfs`.
    runner: &'a dyn CommandRunner,
    /// Refuses (`Err`, with every reason) unless each target in the second
    /// argument is a real mount point with the filesystem it must have. Run
    /// before anything is written to a target (`mount::verify_write_targets`:
    /// writing to a bare mount point falls through to the root filesystem,
    /// bd DAS-Backup-Manager-9on).
    verify: &'a VerifyTargets<'a>,
    /// Whether a path is a mount point now (`health::is_mountpoint`).
    is_mountpoint: &'a dyn Fn(&Path) -> bool,
}

/// The check behind [`StepEnv::verify`]: the targets (all configured ones),
/// the labels about to be written, and where to say what it found.
type VerifyTargets<'a> =
    dyn Fn(&[Target], &[String], &dyn ProgressCallback) -> Result<(), String> + 'a;

impl StepEnv<'static> {
    const HOST: StepEnv<'static> = StepEnv {
        runner: &SystemRunner,
        verify: &mount::verify_write_targets,
        is_mountpoint: &health::is_mountpoint,
    };
}

/// Why a backup that was handed no target cannot run.
const NO_TARGETS_MOUNTED: &str =
    "No backup targets are mounted. Connect the DAS enclosure and mount targets before running.";

/// The labels of the configured targets that are mounted now.
fn mounted_target_labels(config: &Config) -> Vec<String> {
    config
        .targets
        .iter()
        .filter(|tgt| health::find_any_mount(&tgt.mount, &tgt.serial, &tgt.role).is_some())
        .map(|tgt| tgt.label.clone())
        .collect()
}

/// The targets a step writes to: `requested`, or — when none are named, as
/// `btrdasd backup send` without `--targets` — every configured target that
/// is mounted. Never "every configured target": an absent one is not part of
/// a run that was not told to use it.
fn step_targets(config: &Config, requested: &[String]) -> Result<Vec<String>, String> {
    if !requested.is_empty() {
        return Ok(requested.to_vec());
    }
    let mounted = mounted_target_labels(config);
    if mounted.is_empty() {
        return Err(NO_TARGETS_MOUNTED.to_string());
    }
    Ok(mounted)
}

/// Say which configured targets a step sends to and which it leaves alone.
fn log_target_scope(config: &Config, targets: &[String], progress: &dyn ProgressCallback) {
    for target in &config.targets {
        let line = if targets.contains(&target.label) {
            format!(
                "Target '{}' at {}: will receive",
                target.label, target.mount
            )
        } else {
            format!(
                "Target '{}' at {}: not part of this run — btrbk is told to leave it alone",
                target.label, target.mount
            )
        };
        progress.on_log(LogLevel::Info, &line);
    }
}

/// btrbk's command-line filter arguments for one step: the sources and targets
/// it may touch, and nothing else.
///
/// `sources` are source labels (empty: every source). `targets` are target
/// labels, or `None` for a step that involves none (`btrdasd backup
/// snapshot`). An empty result means "no filter": the selection is everything
/// `btrbk.conf` declares, so btrbk runs as it always did. A label that is not
/// in the configuration, or a selection that leaves nothing to do, is an `Err`
/// — never an empty filter list, which btrbk reads as everything.
///
/// Why these filters (btrbk 0.32.7, `/usr/bin/btrbk`; the line numbers are its
/// own):
///
/// * Filters are a UNION, applied once to the declarations (6245-6293). One
///   that matches a volume keeps all of it (6249-6251); one that matches a
///   subvolume, or its snapshot path, keeps it with EVERY target (6256-6261,
///   the `next` skips the target loop). So a source filter next to a target
///   filter would re-admit the unticked targets, and a volume path, the old
///   filter, cannot tell two sources on one volume apart.
/// * The one filter that selects a subvolume AND a target is
///   `<target directory>/<snapshot_name>` (6263-6268): the target is kept
///   when a filter matches it or that child, the other targets are aborted
///   `skip_cmdline_filter` (6270), and a subvolume left with no target is
///   aborted too (6274-6277). So there is one filter per (subvolume, target)
///   pair ([`btrbk_conf::declared_pairs`]). An absolute filter must equal the
///   whole path; `*` is its only wildcard (3304-3339, 3342-3387), so one with
///   a `*` is refused rather than widened.
/// * An aborted section is skipped, not failed: only keys starting `abort_`
///   count towards exit 10 (5257-5266), and the 3-argument `ABORTED` the
///   filter uses sets `skip_cmdline_filter` (520-541). The target tree is
///   read through `vinfo_subsection` without its include-aborted flag
///   (3277-3298, 6557-6585), so a filtered-out target is never probed: an
///   unticked ABSENT target cannot abort, where an absent target that is
///   not filtered out aborts there (6564-6566) and ends btrbk with 10.
/// * The snapshot and send loops walk the same non-aborted sections (6871-6872,
///   6966-6967): a subvolume the filter leaves without a target gets NO
///   snapshot either, in `run` and in `snapshot`.
/// * A filter that matches nothing is exit 2 before anything runs (6285-6291),
///   so a filter naming something `btrbk.conf` lacks fails loudly.
///
/// A per-run `btrbk.conf` was the alternative. `render_btrbk_conf` takes its
/// retention baseline from the first primary target and writes the other
/// targets' differences from it (btrbk_conf.rs `primary`), so a config without
/// the primary target renders every other target's retention differently — and
/// `btrbk run` enforces retention. The filters leave the one generated file
/// as it is.
fn btrbk_filters(
    config: &Config,
    sources: &[String],
    targets: Option<&[String]>,
    progress: &dyn ProgressCallback,
) -> Result<Vec<String>, String> {
    if let Some(label) = sources
        .iter()
        .find(|l| !config.sources.iter().any(|s| &s.label == *l))
    {
        return Err(format!("source '{label}' is not in the configuration"));
    }
    if let Some(label) = targets
        .unwrap_or_default()
        .iter()
        .find(|l| !config.targets.iter().any(|t| &t.label == *l))
    {
        return Err(format!("target '{label}' is not in the configuration"));
    }
    let wants_source = |label: &String| sources.is_empty() || sources.contains(label);
    let declared = btrbk_conf::declared_pairs(config);
    let selected: Vec<&DeclaredPair<'_>> = declared
        .iter()
        .filter(|p| {
            wants_source(&p.source.label) && targets.is_none_or(|t| t.contains(&p.target.label))
        })
        .collect();
    for source in config.sources.iter().filter(|s| wants_source(&s.label)) {
        let declared_here = declared.iter().any(|p| p.source.label == source.label);
        if declared_here && !selected.iter().any(|p| p.source.label == source.label) {
            progress.on_log(
                LogLevel::Warning,
                &format!(
                    "Source '{}' sends to none of the selected targets — nothing is done for it",
                    source.label
                ),
            );
        }
    }
    if selected.len() == declared.len() {
        return Ok(Vec::new());
    }
    if selected.is_empty() {
        return Err(if targets.is_some() {
            "none of the selected sources sends to a selected target — there is nothing to run"
        } else {
            "none of the selected sources has a subvolume to snapshot"
        }
        .to_string());
    }
    let mut filters: Vec<String> = Vec::new();
    for pair in selected {
        let filter = match targets {
            Some(_) => format!("{}/{}", pair.target_dir(), pair.snapshot_name),
            None => format!("{}/{}", pair.source.volume, pair.subvolume.name),
        };
        if !filter.starts_with('/') || filter.contains(['*', '\n']) {
            return Err(format!(
                "'{filter}' cannot be given to btrbk as a filter: it must be an absolute path \
                 without '*'"
            ));
        }
        if !filters.contains(&filter) {
            filters.push(filter);
        }
    }
    Ok(filters)
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Create btrbk snapshots for specified sources.
///
/// Only the subvolumes of the sources named are snapshotted (empty: every
/// source) — by subvolume, so two sources on one volume are told apart. No
/// target is selected here, so btrbk still reads every one the file declares.
pub fn create_snapshots(
    config: &Config,
    sources: &[String],
    progress: &dyn ProgressCallback,
) -> Result<usize, Box<dyn std::error::Error>> {
    create_snapshots_with(config, sources, None, progress, &SystemRunner)
}

/// [`create_snapshots`] with every `btrbk` run by `runner`, and — when the run
/// has chosen its targets (`targets`) — confined to the subvolumes that go to
/// them, so an unselected target is not read, let alone written.
fn create_snapshots_with(
    config: &Config,
    sources: &[String],
    targets: Option<&[String]>,
    progress: &dyn ProgressCallback,
    runner: &dyn CommandRunner,
) -> Result<usize, Box<dyn std::error::Error>> {
    progress.on_stage("Creating snapshots", sources.len() as u64);

    let filters = btrbk_filters(config, sources, targets, progress)?;
    for src in sources
        .iter()
        .filter_map(|label| config.sources.iter().find(|s| &s.label == label))
    {
        progress.on_log(
            LogLevel::Info,
            &format!("Snapshotting source '{}' at {}", src.label, src.volume),
        );
    }

    let mut cmd = Command::new("btrbk");
    cmd.arg("-c").arg(&config.general.btrbk_conf);

    // btrbk syntax: `btrbk -c <conf> snapshot [<filter>...]`
    // The "snapshot" subcommand must appear exactly once, followed by the
    // filters (see `btrbk_filters`) that limit it to the selection.
    cmd.arg("snapshot").args(&filters);

    let (stdout, status) = run_command(&mut cmd, runner, progress)?;

    if !status.success() {
        // Must be an Err, not a Warning. Every caller turns Err into an entry
        // in `errors`, and run_backup derives `success = errors.is_empty()`,
        // so a Warning here left the run reporting SUCCESS to the CLI exit
        // code, the GUI JobFinished flag, the DB history row and the email
        // subject alike. bd DAS-Backup-Manager-nsp (finding #1/#2) — the same
        // shape as bd oi0, where the outcome was decided by whether anything
        // pushed an error and nothing ever did.
        let msg = format!(
            "btrbk snapshot command failed ({}). No snapshots can be assumed created.",
            describe_exit(status)
        );
        progress.on_log(LogLevel::Error, &msg);
        return Err(msg.into());
    }

    // Prefer the machine-readable listing; fall back to the marker parse only
    // if btrbk cannot be queried (bd DAS-Backup-Manager-06p).
    let count = match btrbk_raw_listing(config, &filters, runner) {
        Some(raw) => parse_raw_snapshot_count(&raw),
        None => {
            progress.on_log(
                LogLevel::Warning,
                "btrbk --format=raw list latest failed — counts fall back to output markers",
            );
            parse_btrbk_snapshot_count(&stdout)
        }
    };

    for (i, label) in sources.iter().enumerate() {
        progress.on_progress(i as u64 + 1, sources.len() as u64, label);
    }

    progress.on_log(LogLevel::Info, &format!("Snapshots created: {count}"));
    Ok(count)
}

/// Send snapshots to specified targets via btrbk.
///
/// btrbk is told to send exactly the subvolumes of the sources named (empty:
/// every source) to exactly the targets named (empty: every one that is
/// mounted) — a target that is not named is not read, so one that is absent
/// does not fail the step; one that IS named and cannot be read still does.
///
/// When `preserve` is true, passes `--preserve` to btrbk so retention cleanup
/// is skipped (incremental mode).  When false, btrbk enforces retention policy
/// after sending (deletes old snapshots/backups outside the retention window).
///
/// Returns (snapshots_sent, bytes_sent).
pub fn send_snapshots(
    config: &Config,
    sources: &[String],
    targets: &[String],
    preserve: bool,
    progress: &dyn ProgressCallback,
) -> Result<(usize, u64), Box<dyn std::error::Error>> {
    send_snapshots_with(config, sources, targets, preserve, progress, &StepEnv::HOST)
}

/// [`send_snapshots`] in `env`: the targets it will write to are verified
/// first, and it refuses — before any btrbk runs — on one that is not a mount
/// point or not the filesystem it must be (bd DAS-Backup-Manager-7tx).
fn send_snapshots_with(
    config: &Config,
    sources: &[String],
    targets: &[String],
    preserve: bool,
    progress: &dyn ProgressCallback,
    env: &StepEnv,
) -> Result<(usize, u64), Box<dyn std::error::Error>> {
    progress.on_stage("Sending snapshots", 1);

    let targets = step_targets(config, targets)?;
    (env.verify)(&config.targets, &targets, progress)?;
    let filters = btrbk_filters(config, sources, Some(&targets), progress)?;
    log_target_scope(config, &targets, progress);

    let mut cmd = Command::new("btrbk");
    if preserve {
        cmd.arg("--preserve");
    }
    cmd.arg("-c").arg(&config.general.btrbk_conf);

    // Use `resume` to handle interrupted transfers gracefully, limited by the
    // filters to the sources and targets selected. btrbk does NOT skip a
    // target it cannot read: it aborts it and exits 10 (btrbk 0.32.7,
    // `exit_status` 5257-5266, target abort 6564-6566), which is why a target
    // that was not selected must not be handed to it at all.
    cmd.arg("resume").args(&filters);

    let mut snapshots_sent: usize = 0;
    let mut bytes_sent: u64 = 0;
    let mut stdout_lines = Vec::new();

    let status = stream_command(&mut cmd, env.runner, progress, |line| {
        stdout_lines.push(line.to_string());
        let trimmed = line.trim_start();
        // btrbk marks sends with >>> (incremental) or *** (full)
        if trimmed.starts_with(">>>") || trimmed.starts_with("***") {
            snapshots_sent += 1;
            bytes_sent += parse_btrbk_size_field(line);
        }
        // Parse throughput hints from btrbk progress lines.
        let lower = line.to_lowercase();
        if lower.contains("mib/s") || lower.contains("kib/s") || lower.contains("gib/s") {
            let bytes_per_sec = parse_throughput_line(line);
            if bytes_per_sec > 0 {
                progress.on_throughput(bytes_per_sec);
            }
        }
    })?;

    if !status.success() {
        // Must be an Err, not a Warning. Every caller turns Err into an entry
        // in `errors`, and run_backup derives `success = errors.is_empty()`,
        // so a Warning here left the run reporting SUCCESS to the CLI exit
        // code, the GUI JobFinished flag, the DB history row and the email
        // subject alike. bd DAS-Backup-Manager-nsp (finding #1/#2) — the same
        // shape as bd oi0, where the outcome was decided by whether anything
        // pushed an error and nothing ever did.
        let msg = format!(
            "btrbk resume command failed ({}). The send may be incomplete.",
            describe_exit(status)
        );
        progress.on_log(LogLevel::Error, &msg);
        return Err(msg.into());
    }

    // Count from the machine-readable listing, not the human output.
    let full_output = stdout_lines.join("\n");
    snapshots_sent = match btrbk_raw_listing(config, &filters, env.runner) {
        Some(raw) => parse_raw_send_count(&raw),
        None => {
            progress.on_log(
                LogLevel::Warning,
                "btrbk --format=raw list latest failed — counts fall back to output markers",
            );
            parse_btrbk_send_count(&full_output)
        }
    };

    progress.on_log(LogLevel::Info, &format!("Snapshots sent: {snapshots_sent}"));
    Ok((snapshots_sent, bytes_sent))
}

/// Count lines matching btrbk's `---` (deleted) marker in output.
fn parse_btrbk_clean_count(output: &str) -> usize {
    output
        .lines()
        .filter(|line| line.trim_start().starts_with("---"))
        .count()
}

/// Run the full btrbk lifecycle: snapshot + send + retention cleanup.
///
/// Uses `btrbk run` which atomically handles all three steps.  This is the
/// Full backup mode — equivalent to what the nightly bash script does. Like
/// [`send_snapshots`], it is limited to the sources and targets named.
///
/// Returns (snapshots_created, snapshots_sent, snapshots_cleaned, bytes_sent).
pub fn run_full_pipeline(
    config: &Config,
    sources: &[String],
    targets: &[String],
    progress: &dyn ProgressCallback,
) -> Result<(usize, usize, usize, u64), Box<dyn std::error::Error>> {
    run_full_pipeline_with(config, sources, targets, progress, &StepEnv::HOST)
}

/// [`run_full_pipeline`] in `env`: like [`send_snapshots_with`], it verifies
/// the targets before btrbk runs and refuses on one that fails.
fn run_full_pipeline_with(
    config: &Config,
    sources: &[String],
    targets: &[String],
    progress: &dyn ProgressCallback,
    env: &StepEnv,
) -> Result<(usize, usize, usize, u64), Box<dyn std::error::Error>> {
    progress.on_stage("Full backup (snapshot + send + cleanup)", 1);

    let targets = step_targets(config, targets)?;
    (env.verify)(&config.targets, &targets, progress)?;
    let filters = btrbk_filters(config, sources, Some(&targets), progress)?;
    for src in sources
        .iter()
        .filter_map(|label| config.sources.iter().find(|s| &s.label == label))
    {
        progress.on_log(
            LogLevel::Info,
            &format!("Source '{}' at {}", src.label, src.volume),
        );
    }
    log_target_scope(config, &targets, progress);

    let mut cmd = Command::new("btrbk");
    cmd.arg("-c").arg(&config.general.btrbk_conf);
    cmd.arg("run").args(&filters);

    let mut snapshots_created: usize = 0;
    let mut snapshots_sent: usize = 0;
    let mut bytes_sent: u64 = 0;
    let mut stdout_lines = Vec::new();

    let status = stream_command(&mut cmd, env.runner, progress, |line| {
        stdout_lines.push(line.to_string());
        let trimmed = line.trim_start();
        if trimmed.starts_with("+++") {
            snapshots_created += 1;
        } else if trimmed.starts_with(">>>") || trimmed.starts_with("***") {
            snapshots_sent += 1;
            bytes_sent += parse_btrbk_size_field(line);
        }
        let lower = line.to_lowercase();
        if lower.contains("mib/s") || lower.contains("kib/s") || lower.contains("gib/s") {
            let bytes_per_sec = parse_throughput_line(line);
            if bytes_per_sec > 0 {
                progress.on_throughput(bytes_per_sec);
            }
        }
    })?;

    if !status.success() {
        // Must be an Err, not a Warning. Every caller turns Err into an entry
        // in `errors`, and run_backup derives `success = errors.is_empty()`,
        // so a Warning here left the run reporting SUCCESS to the CLI exit
        // code, the GUI JobFinished flag, the DB history row and the email
        // subject alike. bd DAS-Backup-Manager-nsp (finding #1/#2) — the same
        // shape as bd oi0, where the outcome was decided by whether anything
        // pushed an error and nothing ever did.
        let msg = format!(
            "btrbk run command failed ({}). The backup may be incomplete.",
            describe_exit(status)
        );
        progress.on_log(LogLevel::Error, &msg);
        return Err(msg.into());
    }

    // Re-count from accumulated output for accuracy.
    let full_output = stdout_lines.join("\n");

    progress.on_log(
        LogLevel::Debug,
        &format!(
            "run_full_pipeline: captured {} stdout lines",
            stdout_lines.len()
        ),
    );

    match btrbk_raw_listing(config, &filters, env.runner) {
        Some(raw) => {
            snapshots_created = parse_raw_snapshot_count(&raw);
            snapshots_sent = parse_raw_send_count(&raw);
        }
        None => {
            progress.on_log(
                LogLevel::Warning,
                "btrbk --format=raw list latest failed — counts fall back to output markers",
            );
            snapshots_created = parse_btrbk_snapshot_count(&full_output);
            snapshots_sent = parse_btrbk_send_count(&full_output);
        }
    }
    // No raw equivalent exists for deletions, so this one still reads markers.
    // Same caveat as above: if btrbk restyles its `---` lines this silently
    // returns 0. Tracked with the rest of 06p.
    let snapshots_cleaned = parse_btrbk_clean_count(&full_output);

    progress.on_log(
        LogLevel::Info,
        &format!(
            "Full backup: {} created, {} sent, {} cleaned up",
            snapshots_created, snapshots_sent, snapshots_cleaned,
        ),
    );

    Ok((
        snapshots_created,
        snapshots_sent,
        snapshots_cleaned,
        bytes_sent,
    ))
}

/// Parse a throughput value (e.g. "22.3 MiB/s") from a btrbk output line.
/// Returns bytes per second, or 0 if not parseable.
fn parse_throughput_line(line: &str) -> u64 {
    // Walk tokens looking for a number followed by a unit.
    let tokens: Vec<&str> = line.split_whitespace().collect();
    for (i, token) in tokens.iter().enumerate() {
        let unit = match tokens.get(i + 1).copied() {
            Some(u) => u,
            None => {
                // Unit might be glued: "22.3MiB/s"
                if let Some(v) = parse_glued_throughput(token) {
                    return v;
                }
                continue;
            }
        };
        if let Ok(val) = token.parse::<f64>() {
            let multiplier: u64 = match unit.to_uppercase().as_str() {
                "GIB/S" | "GB/S" => 1_073_741_824,
                "MIB/S" | "MB/S" => 1_048_576,
                "KIB/S" | "KB/S" => 1_024,
                "B/S" => 1,
                _ => continue,
            };
            return (val * multiplier as f64) as u64;
        }
    }
    0
}

/// Parse a glued token like "22.3MiB/s" into bytes/sec.
fn parse_glued_throughput(token: &str) -> Option<u64> {
    let upper = token.to_uppercase();
    let (val_str, mult) = if let Some(s) = upper.strip_suffix("GIB/S") {
        (s, 1_073_741_824u64)
    } else if let Some(s) = upper.strip_suffix("GB/S") {
        (s, 1_000_000_000u64)
    } else if let Some(s) = upper.strip_suffix("MIB/S") {
        (s, 1_048_576u64)
    } else if let Some(s) = upper.strip_suffix("MB/S") {
        (s, 1_000_000u64)
    } else if let Some(s) = upper.strip_suffix("KIB/S") {
        (s, 1_024u64)
    } else if let Some(s) = upper.strip_suffix("KB/S") {
        (s, 1_000u64)
    } else {
        let s = upper.strip_suffix("B/S")?;
        (s, 1u64)
    };
    val_str
        .parse::<f64>()
        .ok()
        .map(|v| (v * mult as f64) as u64)
}

// sync_esp() removed 2026-04-12 — see .claude/rules/esp-safety.md.

/// Best-effort parse of a size from a btrbk `>>>` or `***` output line.
///
/// btrbk v0.32 does NOT include size info in these lines (just paths).
/// This parser is kept as a secondary source in case future btrbk versions
/// add parenthetical sizes like `(incremental, 45.3 MiB)`.  The primary
/// bytes_sent measurement uses target disk usage delta instead.
///
/// Returns the size in bytes, or 0 if not parseable.
fn parse_btrbk_size_field(line: &str) -> u64 {
    // Look for a parenthetical at the end containing a size.
    let paren_content = match (line.rfind('('), line.rfind(')')) {
        (Some(open), Some(close)) if close > open => &line[open + 1..close],
        _ => return 0,
    };
    // Split on comma — size is usually the last segment: "incremental, 45.3 MiB"
    for segment in paren_content.rsplit(',') {
        let seg = segment.trim();
        let tokens: Vec<&str> = seg.split_whitespace().collect();
        if tokens.len() == 2
            && let Ok(val) = tokens[0].parse::<f64>()
        {
            let multiplier: u64 = match tokens[1].to_uppercase().as_str() {
                "TIB" | "TB" => 1_099_511_627_776,
                "GIB" | "GB" => 1_073_741_824,
                "MIB" | "MB" => 1_048_576,
                "KIB" | "KB" => 1_024,
                "B" => 1,
                _ => continue,
            };
            return (val * multiplier as f64) as u64;
        }
    }
    0
}

/// Force filesystem sync on all mounted backup targets so `statvfs` returns
/// up-to-date space accounting. BTRFS defers transaction commits, so without
/// an explicit sync after `btrfs receive`, the available-blocks counter can
/// remain stale for several seconds.
fn sync_targets(config: &Config) {
    for tgt in &config.targets {
        if let Some(path) = health::find_any_mount(&tgt.mount, &tgt.serial, &tgt.role) {
            // syncfs(2) syncs the filesystem containing the given fd.
            if let Ok(file) = std::fs::File::open(&path) {
                use std::os::unix::io::AsRawFd;
                unsafe {
                    libc::syncfs(file.as_raw_fd());
                }
            }
        }
    }
}

/// Measure total used bytes across all mounted backup targets.
///
/// Uses `statvfs(2)` to read filesystem usage directly (no child process).
/// Returns the sum of used bytes across all target mount points. Used to
/// calculate bytes_sent as the delta between before/after a backup, since
/// btrbk doesn't report transfer sizes in its output.
fn measure_target_usage(config: &Config, progress: &dyn ProgressCallback) -> u64 {
    config
        .targets
        .iter()
        .filter_map(|tgt| {
            let path = health::find_any_mount(&tgt.mount, &tgt.serial, &tgt.role);
            let path = match path {
                Some(p) => p,
                None => {
                    progress.on_log(
                        LogLevel::Warning,
                        &format!(
                            "measure_target_usage: target '{}' not mounted (configured: {})",
                            tgt.label, tgt.mount
                        ),
                    );
                    return None;
                }
            };
            let c_path = std::ffi::CString::new(path.clone()).ok()?;
            let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
            let rc = unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) };
            if rc != 0 {
                progress.on_log(
                    LogLevel::Warning,
                    &format!(
                        "measure_target_usage: statvfs failed for '{}' at {path}",
                        tgt.label
                    ),
                );
                return None;
            }
            let total = stat.f_blocks * stat.f_frsize;
            let avail = stat.f_bavail * stat.f_frsize;
            let used = total.saturating_sub(avail);
            progress.on_log(
                LogLevel::Debug,
                &format!(
                    "measure_target_usage: '{}' at {path}: used={used}",
                    tgt.label
                ),
            );
            Some(used)
        })
        .sum()
}

/// Find the latest btrbk snapshot matching a given name on a target.
///
/// Looks for subvolumes in the `nvme/` subdirectory matching the pattern
/// `nvme/{name}.YYYYMMDDTHHMM`.  Returns the most recent one (by
/// lexicographic sort of the timestamp suffix).
/// Latest btrbk snapshot named `snap_name` under any of `subdirs` on `target_mount`.
///
/// Both inputs are supplied by the caller from a source of truth: `snap_name`
/// from the live `btrbk.conf`, `subdirs` from the owning [`Source`]. Neither is
/// derived here. The previous version hardcoded `nvme/` and an algorithmic
/// name, and the pair silently stopped matching the moment
/// `resolve_snapshot_names` disambiguated a bare `@` to `root-`
/// (bd DAS-Backup-Manager-5ig).
fn find_latest_btrbk_snapshot(
    runner: &dyn CommandRunner,
    target_mount: &str,
    subdirs: &[String],
    snap_name: &str,
) -> Option<String> {
    let output = runner
        .output(Command::new("btrfs").args(["subvolume", "list", target_mount]))
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    Some(latest_matching_snapshot(&stdout, subdirs, snap_name)?.to_string())
}

/// Pure half of [`find_latest_btrbk_snapshot`], so the matching rule is testable
/// without a btrfs filesystem.
fn latest_matching_snapshot<'a>(
    listing: &'a str,
    subdirs: &[String],
    snap_name: &str,
) -> Option<&'a str> {
    let prefixes: Vec<String> = subdirs
        .iter()
        .map(|d| format!("{}/{snap_name}.", d.trim_matches('/')))
        .collect();
    let mut matches: Vec<&str> = listing
        .lines()
        .filter_map(|line| {
            let path = line.split_whitespace().last()?;
            prefixes.iter().any(|p| path.starts_with(p)).then_some(path)
        })
        .collect();
    matches.sort();
    matches.last().copied()
}

/// Which `target_subdirs` a given boot subvolume's snapshots live under.
fn subdirs_for_subvol(config: &Config, subvol: &str) -> Vec<String> {
    let mut dirs: Vec<String> = config
        .sources
        .iter()
        .filter(|s| s.subvolumes.iter().any(|sv| sv.name == subvol))
        .flat_map(|s| s.target_subdirs.iter().cloned())
        .collect();
    dirs.sort();
    dirs.dedup();
    dirs
}

/// Run a `btrfs` subcommand through `runner`, returning whether it succeeded.
fn btrfs_ok(runner: &dyn CommandRunner, args: &[&str]) -> std::io::Result<bool> {
    Ok(runner
        .output(Command::new("btrfs").args(args))?
        .status
        .success())
}

/// Archive boot subvolumes as read-only snapshots on backup targets, then
/// refresh the live subvolume from the newest received snapshot.
///
/// **The ordering is the safety property**, and it mirrors
/// `update_boot_subvolumes()` in `scripts/backup-run.sh`: locate the
/// replacement and build it alongside the live subvolume BEFORE removing the
/// live one, so no failure path can leave `@` absent. Until 0.7.20.0 the order
/// was archive -> delete -> look up -> recreate, which destroyed the live
/// subvolume whenever the lookup failed — and for `@` the lookup could never
/// succeed, so every Rust-path run on a primary target left the mount without a
/// bootable `@` (bd DAS-Backup-Manager-5ig).
///
/// Every target it will write under is verified first, and it refuses — nothing
/// written — on one whose mount point is a bare directory or holds another
/// filesystem (bd DAS-Backup-Manager-7tx, 9on): [`mount::verify_write_targets`]
/// reads each `target.mount` as the root filesystem's own directory if nothing
/// is mounted there, and the snapshots, deletions and renames below would land
/// on it. A target whose mount point does not exist is not written and is left
/// alone, as `backup-run.sh` leaves an absent target.
pub fn archive_boot(
    config: &Config,
    progress: &dyn ProgressCallback,
) -> Result<bool, Box<dyn std::error::Error>> {
    archive_boot_with(config, None, progress, &StepEnv::HOST)
}

/// [`archive_boot`] in `env`, for the targets `selected` (`None`: every one).
fn archive_boot_with(
    config: &Config,
    selected: Option<&[String]>,
    progress: &dyn ProgressCallback,
    env: &StepEnv,
) -> Result<bool, Box<dyn std::error::Error>> {
    if !config.boot.enabled {
        return Ok(false);
    }

    progress.on_stage(
        "Archiving boot subvolumes",
        config.boot.subvolumes.len() as u64,
    );

    if config.targets.is_empty() {
        progress.on_log(
            LogLevel::Warning,
            "No backup targets configured — skipping boot archive",
        );
        return Ok(false);
    }

    // The targets written under: selected, not a mirror (those carry their own
    // OS and are skipped below), and with a mount point that exists. One that
    // exists must be the filesystem it should be.
    let is_selected = |t: &Target| selected.is_none_or(|labels| labels.contains(&t.label));
    let writes: Vec<String> = config
        .targets
        .iter()
        .filter(|t| is_selected(t) && t.role != TargetRole::Mirror)
        .filter(|t| Path::new(&t.mount).exists())
        .map(|t| t.label.clone())
        .collect();
    (env.verify)(&config.targets, &writes, progress)?;

    // Snapshot names come from the file btrbk itself reads. If it cannot be
    // read we do nothing at all rather than fall back to a guess: a wrong name
    // here is what used to cost the live subvolume.
    let btrbk_conf = std::path::Path::new(&config.general.btrbk_conf);
    let snap_names = match crate::forget::live_subvol_snapshot_names(btrbk_conf) {
        Ok(map) => map,
        Err(e) => {
            progress.on_log(
                LogLevel::Error,
                &format!(
                    "Cannot read {} ({e}) — skipping boot archive rather than guessing snapshot names",
                    btrbk_conf.display()
                ),
            );
            return Ok(false);
        }
    };

    let ts = format_timestamp();
    let mut any_archived = false;

    for (step, subvol) in config.boot.subvolumes.iter().enumerate() {
        progress.on_progress(
            step as u64,
            config.boot.subvolumes.len() as u64,
            &format!("Archiving {subvol}"),
        );

        let archive_name = format!("{subvol}.archive.{ts}");

        let Some(snap_name) = snap_names.get(subvol.as_str()) else {
            progress.on_log(
                LogLevel::Warning,
                &format!(
                    "{subvol} has no snapshot_name in {} — leaving it untouched",
                    btrbk_conf.display()
                ),
            );
            continue;
        };

        let subdirs = subdirs_for_subvol(config, subvol);
        if subdirs.is_empty() {
            progress.on_log(
                LogLevel::Warning,
                &format!("No source declares target_subdirs for {subvol} — leaving it untouched"),
            );
            continue;
        }

        for target in config.targets.iter().filter(|t| is_selected(t)) {
            // Mirror targets carry a genuinely independent OS install in their
            // own @/@home (e.g. das-recovery-bay1) — never archive-then-replace
            // it with a host snapshot. Mirrors still receive ordinary btrbk
            // send/receive via run_backup(); only this boot-subvol step skips
            // them. Wording matches update_boot_subvolumes() in
            // scripts/backup-run.sh so both origins log identically
            // (bd DAS-Backup-Manager-am1).
            if target.role == TargetRole::Mirror {
                progress.on_log(
                    LogLevel::Info,
                    &format!("[{}] Skipping mirror target (independent OS)", target.mount),
                );
                continue;
            }

            let tgt_mount = &target.mount;
            let subvol_path = format!("{tgt_mount}/{subvol}");
            let staging = format!("{subvol_path}.new");

            // Step 1: locate the replacement FIRST. Nothing is destroyed if
            // this fails.
            let Some(latest) =
                find_latest_btrbk_snapshot(env.runner, tgt_mount, &subdirs, snap_name)
            else {
                progress.on_log(
                    LogLevel::Warning,
                    &format!(
                        "[{tgt_mount}] No btrbk snapshot named '{snap_name}' — leaving {subvol} untouched"
                    ),
                );
                continue;
            };
            let latest_path = format!("{tgt_mount}/{latest}");

            // Step 2: archive the outgoing subvolume read-only, if there is one.
            if std::path::Path::new(&subvol_path).exists() {
                let archive_path = format!("{tgt_mount}/{archive_name}");
                if !btrfs_ok(
                    env.runner,
                    &["subvolume", "snapshot", "-r", &subvol_path, &archive_path],
                )? {
                    progress.on_log(
                        LogLevel::Warning,
                        &format!("Failed to archive {subvol_path} -> {archive_path}"),
                    );
                    continue;
                }
                progress.on_log(
                    LogLevel::Info,
                    &format!("Archived {subvol_path} -> {archive_path}"),
                );
                any_archived = true;
            }

            // Step 3: clear any staging subvolume left by an interrupted run.
            if std::path::Path::new(&staging).exists()
                && !btrfs_ok(env.runner, &["subvolume", "delete", &staging])?
            {
                progress.on_log(
                    LogLevel::Warning,
                    &format!("Stale {staging} could not be removed — leaving {subvol} untouched"),
                );
                continue;
            }

            // Step 4: build the replacement ALONGSIDE the live subvolume.
            if !btrfs_ok(
                env.runner,
                &["subvolume", "snapshot", &latest_path, &staging],
            )? {
                progress.on_log(
                    LogLevel::Warning,
                    &format!(
                        "Failed to create {staging} from {latest} — leaving {subvol} untouched"
                    ),
                );
                continue;
            }

            // Step 5: only now remove the live subvolume.
            if std::path::Path::new(&subvol_path).exists()
                && !btrfs_ok(env.runner, &["subvolume", "delete", &subvol_path])?
            {
                progress.on_log(
                    LogLevel::Warning,
                    &format!("Failed to delete {subvol_path} — discarding {staging}"),
                );
                let _ = btrfs_ok(env.runner, &["subvolume", "delete", &staging]);
                continue;
            }

            // Step 6: swap the replacement into place.
            if let Err(e) = std::fs::rename(&staging, &subvol_path) {
                progress.on_log(
                    LogLevel::Error,
                    &format!(
                        "Renamed nothing: {staging} -> {subvol_path} failed ({e}). \
                         The archive {archive_name} on {tgt_mount} holds the previous contents."
                    ),
                );
                continue;
            }
            progress.on_log(
                LogLevel::Info,
                &format!("Created {subvol_path} from {latest}"),
            );
        }

        progress.on_progress(
            step as u64 + 1,
            config.boot.subvolumes.len() as u64,
            &format!("Archived {subvol}"),
        );
    }

    Ok(any_archived)
}

/// What the subvolume sync at the start of a backup run found, as the run
/// report shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncSection {
    /// The "SUBVOLUME SYNC" section of the run report.
    pub report: String,
    /// Whether the run must end as failed on its account.
    pub failed: bool,
}

/// The first step of every backup, whoever starts it: bring `config.toml` and
/// `btrbk.conf` into line with the subvolumes on the (already mounted)
/// sources, then reload the config sync may have rewritten. Every backup job
/// (`run_backup_job`, behind `btrdasd backup run` and the GUI) calls this, so
/// no route into a backup skips sync (spec §5.2). A failed sync does not stop
/// the run — the subvolumes already configured must still be backed up — but
/// it is logged at error level and the run ends failed.
///
/// Nor does a config that cannot be reloaded after sync: the run goes on
/// with `before`, the config it loaded before sync, and the section says so
/// and is failed — as `backup-run.sh` continues with its existing config
/// and records the sync as FAIL. It used to abort the Rust/GUI run before
/// anything was recorded (bd DAS-Backup-Manager-h4t).
pub fn sync_before_backup(
    config_path: &Path,
    before: &Config,
    dry_run: bool,
    today: &str,
    runner: &dyn crate::fsutil::CommandRunner,
    is_mountpoint: &dyn Fn(&Path) -> bool,
    progress: &dyn ProgressCallback,
) -> (Config, SyncSection) {
    let mut section =
        match crate::adopt::sync_subvolumes(config_path, dry_run, today, runner, is_mountpoint) {
            Ok(outcome) => SyncSection {
                report: crate::adopt::format_sync_report(&outcome, dry_run),
                failed: outcome.failed(),
            },
            Err(e) => SyncSection {
                report: format!("SUBVOLUME SYNC\n  SYNC COULD NOT RUN: {e}\n"),
                failed: true,
            },
        };
    let config = match Config::load(config_path) {
        Ok(config) => config,
        Err(e) => {
            section.report.push_str(&format!(
                "  CONFIG NOT RELOADED after sync ({}: {e}) — this run uses the config \
                 loaded before sync\n",
                config_path.display()
            ));
            section.failed = true;
            before.clone()
        }
    };
    let level = if section.failed {
        LogLevel::Error
    } else {
        LogLevel::Info
    };
    for line in section.report.lines() {
        progress.on_log(level, line);
    }
    (config, section)
}

/// Whether the run writes and emails its report: the caller asked for it and
/// `[email]` is enabled.
fn emails_report(options: &BackupOptions, config: &Config) -> bool {
    options.send_report && config.email.enabled
}

/// The volumes of the `sources` selected that are not mounted, as `<volume>
/// (source '<label>')`, one per volume. Read-only: it mounts nothing.
fn unmounted_source_volumes(
    config: &Config,
    sources: &[String],
    is_mountpoint: &dyn Fn(&Path) -> bool,
) -> Vec<String> {
    let mut volumes: Vec<&str> = Vec::new();
    let mut unmounted = Vec::new();
    for source in config.sources.iter().filter(|s| sources.contains(&s.label)) {
        if volumes.contains(&source.volume.as_str()) {
            continue;
        }
        volumes.push(&source.volume);
        if !is_mountpoint(Path::new(&source.volume)) {
            unmounted.push(format!("{} (source '{}')", source.volume, source.label));
        }
    }
    unmounted
}

/// The source labels a run backs up: the ones named or — when none are —
/// every source with at least one subvolume that is not manual-only.
fn effective_sources(config: &Config, options: &BackupOptions) -> Vec<String> {
    if options.sources.is_empty() {
        config
            .sources
            .iter()
            .filter(|src| {
                // Include source if at least one non-manual_only subvolume exists.
                src.subvolumes.iter().any(|sv| !sv.manual_only)
            })
            .map(|src| src.label.clone())
            .collect()
    } else {
        options.sources.clone()
    }
}

/// The target labels a run writes to.
///
/// When targets are explicitly specified (D-Bus helper pre-mounts them),
/// trust the caller — don't re-check mount status.  Only auto-detect
/// mounted targets when the caller leaves the list empty (standalone CLI).
fn effective_targets(
    config: &Config,
    options: &BackupOptions,
    progress: &dyn ProgressCallback,
) -> Vec<String> {
    if options.targets.is_empty() {
        return mounted_target_labels(config);
    }
    // Caller specified targets — validate they exist in config but don't
    // re-check mount status (caller already ensured mount via MountGuard).
    let matched: Vec<String> = options
        .targets
        .iter()
        .filter(|label| {
            config
                .targets
                .iter()
                .any(|t| t.label.as_str() == label.as_str())
        })
        .cloned()
        .collect();

    // If no requested targets matched config (e.g. stale label list),
    // fall back to auto-detecting mounted targets so the backup can
    // still proceed.
    if matched.is_empty() {
        progress.on_log(
            LogLevel::Warning,
            &format!(
                "Requested targets {:?} did not match config {:?} — auto-detecting mounted targets",
                options.targets,
                config.targets.iter().map(|t| &t.label).collect::<Vec<_>>()
            ),
        );
        mounted_target_labels(config)
    } else {
        matched
    }
}

/// What the btrbk steps counted, and which of them failed.
#[derive(Debug, Default, PartialEq, Eq)]
struct Pipeline {
    created: usize,
    sent: usize,
    cleaned: usize,
    bytes: u64,
    /// One per failed step. A failed step does not stop the next one.
    errors: Vec<String>,
}

impl Pipeline {
    /// A step failed: log it and keep it.
    fn failed(&mut self, progress: &dyn ProgressCallback, msg: String) {
        progress.on_log(LogLevel::Error, &msg);
        self.errors.push(msg);
    }
}

/// Run the btrbk steps `mode` and `options` call for, over exactly `sources`
/// and `targets`: every step is handed that selection, so btrbk is never told
/// about a source or a target the run was not asked to touch.
///
/// Incremental: `btrbk snapshot` + `btrbk resume` — creates snapshots and
/// sends deltas. Full: `btrbk run` (atomic snapshot + send + retention
/// cleanup). Both enforce the retention policy, so targets cannot fill up.
fn run_pipeline(
    config: &Config,
    options: &BackupOptions,
    mode: BackupMode,
    sources: &[String],
    targets: &[String],
    progress: &dyn ProgressCallback,
    env: &StepEnv,
) -> Pipeline {
    let mut done = Pipeline::default();
    let snapshots = |done: &mut Pipeline| match create_snapshots_with(
        config,
        sources,
        Some(targets),
        progress,
        env.runner,
    ) {
        Ok(n) => done.created = n,
        Err(e) => done.failed(progress, format!("Snapshot step failed: {e}")),
    };
    let send = |done: &mut Pipeline| {
        // No --preserve: btrbk enforces retention, in both modes.
        match send_snapshots_with(config, sources, targets, false, progress, env) {
            Ok((sent, bytes)) => {
                done.sent = sent;
                done.bytes = bytes;
            }
            Err(e) => done.failed(progress, format!("Send step failed: {e}")),
        }
    };
    match mode {
        BackupMode::Full if options.snapshot_only => snapshots(&mut done),
        BackupMode::Full if options.send_only => send(&mut done),
        BackupMode::Full => match run_full_pipeline_with(config, sources, targets, progress, env) {
            Ok((created, sent, cleaned, bytes)) => {
                done.created = created;
                done.sent = sent;
                done.cleaned = cleaned;
                done.bytes = bytes;
            }
            Err(e) => done.failed(progress, format!("Full backup pipeline failed: {e}")),
        },
        BackupMode::Incremental => {
            if !options.send_only {
                snapshots(&mut done);
            }
            if !options.snapshot_only {
                send(&mut done);
            }
        }
    }
    done
}

/// Run a backup with the given options. Calls btrbk under the hood.
/// The caller must ensure this runs with appropriate privileges (root).
pub fn run_backup(
    config: &Config,
    options: &BackupOptions,
    progress: &dyn ProgressCallback,
) -> Result<BackupResult, Box<dyn std::error::Error>> {
    run_backup_with(config, options, progress, &StepEnv::HOST)
}

/// [`run_backup`] in `env`.
fn run_backup_with(
    config: &Config,
    options: &BackupOptions,
    progress: &dyn ProgressCallback,
    env: &StepEnv,
) -> Result<BackupResult, Box<dyn std::error::Error>> {
    let start = std::time::Instant::now();

    let mut errors: Vec<String> = Vec::new();
    if options.subvolume_sync.as_ref().is_some_and(|s| s.failed) {
        errors.push("Subvolume sync failed — see SUBVOLUME SYNC in the report".into());
    }
    let mut boot_archived = false;
    let mut indexed = false;

    // ---------- Resolve effective sources and targets ----------

    let effective_sources = effective_sources(config, options);
    let effective_targets = effective_targets(config, options, progress);

    // Require at least one target (unless dry-run).
    if effective_targets.is_empty() && !options.dry_run {
        return Err(NO_TARGETS_MOUNTED.into());
    }

    // Verify every target btrbk will write to is backed by the filesystem we
    // expect, BEFORE btrbk is invoked.
    //
    // The explicit-targets branch above deliberately trusts the caller's list
    // rather than re-checking mount status, and the Plasma GUI always supplies
    // that list — so without this, a target whose mount silently failed still
    // reached btrbk, which then wrote to a bare mount point and filled the
    // underlying filesystem (bd DAS-Backup-Manager-9on, made reachable from
    // the GUI by the trust-the-caller shortcut, bd DAS-Backup-Manager-aea).
    //
    // Runs on dry-run too: a dry-run that would have written to the root
    // filesystem is exactly the thing worth being told about.
    //
    // Every step that writes checks again, just before it does (a step can be
    // called on its own, and `send` and `boot-archive` are), so a run does not
    // trust a mount to have stayed as it was through a long snapshot step.
    (env.verify)(&config.targets, &effective_targets, progress)?;

    // Only now — targets mounted and verified, config reloaded after sync —
    // create the directories btrbk receives into, so a source sync just
    // adopted gets its target directory this run (bd DAS-Backup-Manager-arx).
    // On a dry run too, as backup-run.sh's create_target_dirs does. A
    // directory that cannot be made fails the run: btrbk would skip it.
    if let Err(e) = mount::create_target_dirs(config, &effective_targets, progress) {
        progress.on_log(LogLevel::Error, &e);
        errors.push(e);
    }

    // Count enabled pipeline steps for the top-level stage announcement.
    let total_steps = {
        let mut n = 0u64;
        if !options.send_only {
            n += 1;
        } // snapshots
        if !options.snapshot_only {
            n += 1;
        } // send
        if options.boot_archive {
            n += 1;
        }
        if options.index_after {
            n += 1;
        }
        if options.send_report {
            n += 1;
        }
        n.max(1)
    };
    progress.on_stage("Backup", total_steps);

    let mode = options.mode.unwrap_or(BackupMode::Incremental);

    // ---------- Dry-run path ----------

    if options.dry_run {
        progress.on_log(
            LogLevel::Info,
            &format!(
                "DRY RUN ({mode}): would create snapshots for {:?}",
                effective_sources
            ),
        );
        progress.on_log(
            LogLevel::Info,
            &format!(
                "DRY RUN ({mode}): would send to targets {:?}",
                effective_targets
            ),
        );
        if options.boot_archive {
            progress.on_log(
                LogLevel::Info,
                &format!(
                    "DRY RUN ({mode}): would archive boot subvolumes: {:?}",
                    config.boot.subvolumes
                ),
            );
        }

        let success = errors.is_empty();
        let result = BackupResult {
            success,
            mode,
            snapshots_created: 0,
            snapshots_sent: 0,
            snapshots_cleaned: 0,
            bytes_sent: 0,
            boot_archived: false,
            indexed: false,
            report_sent: false,
            errors,
            duration_secs: start.elapsed().as_secs(),
        };
        return Ok(result);
    }

    // ---------- Live pipeline ----------
    //
    // Incremental: `btrbk snapshot` + `btrbk --preserve resume`
    //   Creates snapshots and sends deltas.  --preserve skips retention
    //   cleanup so old snapshots/backups are kept.  Fast daily use.
    //
    // Full: `btrbk run` (atomic snapshot + send + retention cleanup)
    //   The complete backup lifecycle including housekeeping.  Deletes
    //   snapshots and backups outside the configured retention windows.

    // The caller mounted the source volumes (`mount::ensure_sources_mounted`
    // owns whatever it mounted and gives it back), and a run that does not
    // find them refuses rather than mount them itself: this function used to
    // mount any it did not find, outside that guard, and nothing ever
    // unmounted them (bd DAS-Backup-Manager-7tx 3, -8cf).
    let unmounted = unmounted_source_volumes(config, &effective_sources, env.is_mountpoint);
    if !unmounted.is_empty() {
        return Err(format!(
            "Source volume not mounted: {} — refusing to run btrbk on a bare directory",
            unmounted.join("; ")
        )
        .into());
    }

    // Measure target disk usage before btrbk runs so we can calculate
    // bytes_sent as the delta (btrbk doesn't report transfer sizes).
    // Sync first so both before/after measurements use committed metadata.
    sync_targets(config);
    let usage_before = measure_target_usage(config, progress);
    progress.on_log(
        LogLevel::Info,
        &format!("Target usage before: {} bytes", usage_before),
    );

    let done = run_pipeline(
        config,
        options,
        mode,
        &effective_sources,
        &effective_targets,
        progress,
        env,
    );
    errors.extend(done.errors);
    let (snapshots_created, snapshots_sent, snapshots_cleaned) =
        (done.created, done.sent, done.cleaned);
    let mut bytes_sent = done.bytes;

    // Calculate bytes_sent from target disk usage delta. btrbk doesn't report
    // transfer sizes in its output, so we measure before/after. For incremental
    // mode (no cleanup) this is the actual bytes sent. For full mode (with
    // cleanup) it's the net change, which may underestimate if old data was
    // purged. Still better than reporting 0.
    if bytes_sent == 0 && (snapshots_sent > 0 || snapshots_created > 0) {
        // Force BTRFS to commit pending transactions so statvfs reflects the
        // data that was just received. Without this, BTRFS defers metadata
        // updates and statvfs returns stale values, making the delta zero.
        sync_targets(config);
        let usage_after = measure_target_usage(config, progress);
        progress.on_log(
            LogLevel::Info,
            &format!(
                "Target usage after: {} bytes (delta: {})",
                usage_after,
                usage_after.saturating_sub(usage_before)
            ),
        );
        bytes_sent = usage_after.saturating_sub(usage_before);
    }

    // Step (c): Boot archive (both modes)
    if options.boot_archive {
        match archive_boot_with(config, Some(&effective_targets), progress, env) {
            Ok(archived) => boot_archived = archived,
            Err(e) => {
                let msg = format!("Boot archive step failed: {e}");
                progress.on_log(LogLevel::Error, &msg);
                errors.push(msg);
            }
        }
    }

    // Step (d): Index — walk each target's mount path to pick up new snapshots.
    if options.index_after {
        match Database::open(&config.general.db_path) {
            Ok(db) => {
                let mut targets_indexed = 0usize;
                for target in &config.targets {
                    let mount = health::find_any_mount(&target.mount, &target.serial, &target.role);
                    if let Some(path) = mount {
                        progress.on_log(
                            LogLevel::Info,
                            &format!("Indexing target '{}' at {path}", target.label),
                        );
                        match indexer::walk(std::path::Path::new(&path), &db) {
                            Ok(result) => {
                                progress.on_log(
                                    LogLevel::Info,
                                    &format!(
                                        "Indexed '{}': {} new snapshots ({} files)",
                                        target.label,
                                        result.snapshots_indexed,
                                        result.results.iter().map(|r| r.files_total).sum::<usize>(),
                                    ),
                                );
                                targets_indexed += 1;
                            }
                            Err(e) => {
                                progress.on_log(
                                    LogLevel::Warning,
                                    &format!(
                                        "Indexing target '{}' failed (non-fatal): {e}",
                                        target.label
                                    ),
                                );
                            }
                        }
                    }
                }
                if targets_indexed > 0 {
                    indexed = true;
                }
            }
            Err(e) => {
                progress.on_log(
                    LogLevel::Warning,
                    &format!("Cannot open index DB for post-backup indexing (non-fatal): {e}"),
                );
            }
        }
    }

    // The report is written and emailed by `run_backup_job`, after the
    // targets are unmounted, so an unmount failure is in it — the order
    // `backup-run.sh` uses (bd DAS-Backup-Manager-ecg, -5oc).
    let success = errors.is_empty();
    let result = BackupResult {
        success,
        mode,
        snapshots_created,
        snapshots_sent,
        snapshots_cleaned,
        bytes_sent,
        boot_archived,
        indexed,
        report_sent: false,
        errors,
        duration_secs: start.elapsed().as_secs(),
    };

    Ok(result)
}

/// The one-line outcome of a backup run, as the CLI prints it and the GUI
/// shows it when the job ends.
pub fn backup_summary(result: &BackupResult, dry_run: bool) -> String {
    let mode = result.mode;
    if dry_run {
        return if result.success {
            format!("DRY RUN ({mode}) completed — no changes made")
        } else {
            format!(
                "DRY RUN ({mode}) FAILED — no changes made: {}",
                result.errors.join("; ")
            )
        };
    }
    if result.success
        && result.snapshots_created == 0
        && result.snapshots_sent == 0
        && result.snapshots_cleaned == 0
    {
        return format!("Backup ({mode}): nothing to do — all snapshots up to date");
    }
    let cleaned = if result.snapshots_cleaned > 0 {
        format!(", {} cleaned up", result.snapshots_cleaned)
    } else {
        String::new()
    };
    let status = if result.success {
        "succeeded"
    } else {
        "completed with errors"
    };
    let mut summary = format!(
        "Backup {status} ({mode}): {} snapshots created, {} sent{cleaned}, boot archived: {}",
        result.snapshots_created, result.snapshots_sent, result.boot_archived,
    );
    if !result.success {
        summary.push_str(&format!(" — {}", result.errors.join("; ")));
    }
    summary
}

/// Write the run report to `[general].last_report` when the caller asked
/// for a report, and email it when `[email]` is enabled too — the report is
/// written whether or not it is mailed, as `backup-run.sh` writes
/// `$LAST_REPORT` before any send. `data` is what was captured while the
/// targets were mounted. Returns whether the email was sent. Email failure
/// is non-fatal — the backup data is safe — and is logged, not added to the
/// run's errors.
pub fn deliver_report(
    config: &Config,
    options: &BackupOptions,
    result: &BackupResult,
    data: &crate::report::ReportData,
    progress: &dyn ProgressCallback,
) -> bool {
    if !options.send_report {
        return false;
    }
    let report_text =
        crate::report::format_report_from(result, options.subvolume_sync.as_ref(), data);
    // The directory may not exist yet (backup-run.sh: `mkdir -p`). A plain
    // write, not `fsutil::write_atomic`: that makes a new file, which does
    // not keep an existing report's mode and owner.
    let report_path = Path::new(&config.general.last_report);
    if let Err(e) = report_path
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .map_or(Ok(()), std::fs::create_dir_all)
        .and_then(|()| std::fs::write(report_path, &report_text))
    {
        progress.on_log(
            LogLevel::Warning,
            &format!(
                "Failed to save report to {}: {e}",
                config.general.last_report
            ),
        );
    }
    if !emails_report(options, config) {
        return false;
    }
    match crate::report::send_email_report(&report_text, config) {
        Ok(()) => {
            progress.on_log(LogLevel::Info, "Email report sent successfully");
            true
        }
        Err(e) => {
            progress.on_log(
                LogLevel::Warning,
                &format!("Failed to send email report (non-fatal): {e}"),
            );
            false
        }
    }
}

/// Something a backup job mounted and must give back: [`mount::MountGuard`]
/// in production. Returns what is still mounted afterwards.
pub trait Release {
    fn release(&mut self, progress: &dyn ProgressCallback) -> Vec<String>;
}

impl Release for mount::MountGuard {
    fn release(&mut self, progress: &dyn ProgressCallback) -> Vec<String> {
        self.unmount(progress)
    }
}

/// The host-facing steps of a backup job. [`run_backup_job`] decides the
/// order, what a failure means and what is recorded; [`SystemBackupHost`]
/// does each step for real, and tests script them.
pub trait BackupJobHost {
    /// `Ok(None)`: another backup holds the singleton lock. The returned
    /// value holds the locks until it is dropped, and is the proof
    /// `mount_targets` needs.
    fn acquire_locks(
        &self,
        progress: &dyn ProgressCallback,
    ) -> Result<Option<Box<dyn HoldsMaintenance>>, String>;
    fn mount_sources(&self, config: &Config, progress: &dyn ProgressCallback) -> Box<dyn Release>;
    /// The subvolume sync, and the config the rest of the run uses —
    /// `before` when the config cannot be reloaded after sync.
    fn sync(
        &self,
        before: &Config,
        dry_run: bool,
        progress: &dyn ProgressCallback,
    ) -> (Config, SyncSection);
    fn mount_targets(
        &self,
        config: &Config,
        progress: &dyn ProgressCallback,
        held: &MaintenanceHeld,
    ) -> Result<Box<dyn Release>, String>;
    fn run(
        &self,
        config: &Config,
        options: &BackupOptions,
        progress: &dyn ProgressCallback,
    ) -> Result<BackupResult, String>;
    /// The report sections that read the mounted targets. Called while they
    /// are still mounted.
    fn capture_report(&self, config: &Config) -> crate::report::ReportData;
    /// Write and email the report; whether the email went out.
    fn report(
        &self,
        config: &Config,
        options: &BackupOptions,
        result: &BackupResult,
        data: &crate::report::ReportData,
        progress: &dyn ProgressCallback,
    ) -> bool;
    /// Add the run to `backup_runs`.
    fn record(&self, config: &Config, result: &BackupResult) -> Result<(), String>;
}

/// How a backup job ended.
#[derive(Debug)]
pub enum BackupJobOutcome {
    /// Another backup holds the singleton lock — declined, not queued.
    Declined,
    /// The job could not run: locks, mounts, target verification, or btrbk
    /// could not be started. Nothing is recorded — the same cases in which
    /// `backup-run.sh` exits before it records a run.
    NotRun(String),
    /// The job ran. Recorded in `backup_runs` (unless a dry run); its
    /// `success` says whether everything worked.
    Ran(BackupResult),
}

impl BackupJobOutcome {
    pub fn success(&self) -> bool {
        matches!(self, Self::Ran(r) if r.success)
    }

    /// Whether the job succeeded, and the line that says how it ended — what
    /// the GUI shows when the job finishes.
    pub fn finish_line(&self, dry_run: bool) -> (bool, String) {
        match self {
            Self::Declined => (false, "A backup is already running — declined".to_string()),
            Self::NotRun(why) => (false, why.clone()),
            Self::Ran(result) => (result.success, backup_summary(result, dry_run)),
        }
    }
}

/// A failure message with the mount points left mounted appended.
fn with_still_mounted(msg: String, still_mounted: &[String]) -> String {
    match mount::still_mounted_error(still_mounted) {
        Some(e) => format!("{msg}; {e}"),
        None => msg,
    }
}

/// Run one backup job, whoever starts it — `btrdasd backup run` and the GUI
/// (D-Bus helper) both come through here, so they cannot drift apart:
/// locks (decline if a backup is running, wait for a scrub), mount the
/// sources, sync subvolumes, mount the targets, run btrbk, unmount, then
/// report and record.
///
/// A mount point that cannot be released fails the run: its error says
/// `still mounted: <paths>`, and it is in the report and in `backup_runs`
/// (bd DAS-Backup-Manager-5oc). The report is built after the unmount for
/// that reason, as `backup-run.sh` does.
pub fn run_backup_job(
    host: &dyn BackupJobHost,
    config: Config,
    mut options: BackupOptions,
    progress: &dyn ProgressCallback,
) -> BackupJobOutcome {
    let locks = match host.acquire_locks(progress) {
        Ok(Some(locks)) => locks,
        Ok(None) => return BackupJobOutcome::Declined,
        Err(e) => {
            return BackupJobOutcome::NotRun(format!("Could not acquire backup locks: {e}"));
        }
    };
    let mut sources = host.mount_sources(&config, progress);
    // Same rule on every path: a failed sync never stops the run, but the
    // run's result, record and report all say it failed.
    let (config, sync) = host.sync(&config, options.dry_run, progress);
    options.subvolume_sync = Some(sync);
    let mut targets = match host.mount_targets(&config, progress, locks.maintenance()) {
        Ok(targets) => targets,
        Err(e) => {
            let still = sources.release(progress);
            return BackupJobOutcome::NotRun(with_still_mounted(
                format!("Mount failed: {e}"),
                &still,
            ));
        }
    };
    let ran = host.run(&config, &options, progress);
    // Capacity, SMART and the latest snapshots are read now, while the
    // targets are mounted; the report itself is built after the unmount so
    // it can say what was left mounted (backup-run.sh: capture_report_data,
    // then unmount_all).
    let captured = (!options.dry_run && ran.is_ok()).then(|| host.capture_report(&config));
    let mut still_mounted = targets.release(progress);
    still_mounted.extend(sources.release(progress));
    let mut result = match ran {
        Ok(result) => result,
        Err(e) => {
            return BackupJobOutcome::NotRun(with_still_mounted(
                format!("Backup failed: {e}"),
                &still_mounted,
            ));
        }
    };
    if let Some(e) = mount::still_mounted_error(&still_mounted) {
        progress.on_log(LogLevel::Error, &e);
        result.errors.push(e);
        result.success = false;
    }
    if let Some(captured) = &captured {
        result.report_sent = host.report(&config, &options, &result, captured, progress);
        if let Err(e) = host.record(&config, &result) {
            progress.on_log(
                LogLevel::Warning,
                &format!("Failed to record backup history: {e}"),
            );
        }
    }
    BackupJobOutcome::Ran(result)
}

/// [`BackupJobHost`] on this machine.
pub struct SystemBackupHost {
    /// The config file sync rewrites and the run reloads.
    pub config_path: std::path::PathBuf,
    pub singleton_lock: std::path::PathBuf,
    pub maintenance_lock: std::path::PathBuf,
    /// Who runs the job — `btrdasd backup run`, or the GUI's — recorded in
    /// the maintenance lock file while it holds the lock.
    pub job: String,
}

impl SystemBackupHost {
    /// The production host: the given config, the production lock files.
    pub fn new(config_path: &Path, job: &str) -> Self {
        Self {
            config_path: config_path.to_path_buf(),
            singleton_lock: BACKUP_LOCK_PATH.into(),
            maintenance_lock: scrub::MAINTENANCE_LOCK_PATH.into(),
            job: job.to_string(),
        }
    }
}

impl BackupJobHost for SystemBackupHost {
    fn acquire_locks(
        &self,
        progress: &dyn ProgressCallback,
    ) -> Result<Option<Box<dyn HoldsMaintenance>>, String> {
        match acquire_manual_locks_at(
            &self.singleton_lock,
            &self.maintenance_lock,
            &self.job,
            progress,
        ) {
            Ok(BackupLockAttempt::Acquired(locks)) => Ok(Some(locks)),
            Ok(BackupLockAttempt::AlreadyRunning) => Ok(None),
            Err(e) => Err(e.to_string()),
        }
    }

    fn mount_sources(&self, config: &Config, progress: &dyn ProgressCallback) -> Box<dyn Release> {
        Box::new(mount::ensure_sources_mounted(config, progress))
    }

    fn sync(
        &self,
        before: &Config,
        dry_run: bool,
        progress: &dyn ProgressCallback,
    ) -> (Config, SyncSection) {
        sync_before_backup(
            &self.config_path,
            before,
            dry_run,
            &crate::caldate::today(),
            &crate::fsutil::SystemRunner,
            &health::is_mountpoint,
            progress,
        )
    }

    fn mount_targets(
        &self,
        config: &Config,
        progress: &dyn ProgressCallback,
        held: &MaintenanceHeld,
    ) -> Result<Box<dyn Release>, String> {
        mount::ensure_targets_mounted(config, progress, held)
            .map(|guard| Box::new(guard) as Box<dyn Release>)
            .map_err(|e| e.to_string())
    }

    fn run(
        &self,
        config: &Config,
        options: &BackupOptions,
        progress: &dyn ProgressCallback,
    ) -> Result<BackupResult, String> {
        run_backup(config, options, progress).map_err(|e| e.to_string())
    }

    fn capture_report(&self, config: &Config) -> crate::report::ReportData {
        crate::report::capture_report_data(config)
    }

    fn report(
        &self,
        config: &Config,
        options: &BackupOptions,
        result: &BackupResult,
        data: &crate::report::ReportData,
        progress: &dyn ProgressCallback,
    ) -> bool {
        deliver_report(config, options, result, data, progress)
    }

    fn record(&self, config: &Config, result: &BackupResult) -> Result<(), String> {
        let db = Database::open(&config.general.db_path)
            .map_err(|e| format!("cannot open {}: {e}", config.general.db_path))?;
        crate::report::record_backup_run(&db, result)
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // --- manual-backup interlock (bd DAS-Backup-Manager-pe6) ----------------

    #[test]
    fn manual_backup_declines_when_another_backup_holds_the_singleton() {
        // A second backup is redundant, not merely late, so it declines rather
        // than queueing — matching `flock -n` in backup-run.sh.
        let dir = tempfile::tempdir().unwrap();
        let singleton = dir.path().join("backup.lock");
        let maintenance = dir.path().join("maintenance.lock");
        let held = scrub::FileLock::try_acquire(&singleton).unwrap();
        assert!(held.is_some(), "fixture must hold the singleton");

        let progress = crate::progress::NullProgress;
        match acquire_manual_locks_at(&singleton, &maintenance, "btrdasd backup send", &progress)
            .unwrap()
        {
            BackupLockAttempt::AlreadyRunning => {}
            BackupLockAttempt::Acquired(_) => panic!("two backups acquired at once"),
        }
    }

    #[test]
    fn manual_backup_acquires_when_nothing_is_held() {
        let dir = tempfile::tempdir().unwrap();
        let maintenance = dir.path().join("maintenance.lock");
        let progress = crate::progress::NullProgress;
        match acquire_manual_locks_at(
            &dir.path().join("backup.lock"),
            &maintenance,
            "btrdasd backup send",
            &progress,
        )
        .unwrap()
        {
            BackupLockAttempt::Acquired(locks) => {
                assert_eq!(locks.maintenance().path(), maintenance);
                assert_eq!(
                    std::fs::read_to_string(&maintenance).unwrap(),
                    format!("btrdasd backup send pid {}\n", std::process::id()),
                    "the job is recorded as the maintenance lock's holder"
                );
            }
            BackupLockAttempt::AlreadyRunning => panic!("declined with nothing held"),
        }
    }
    use crate::config::{
        Boot, Config, Das, Doctor, Email, General, Gui, Init, InitSystem, Retention, Schedule,
        Scrub, Source, SubvolConfig, Target, TargetRole,
    };
    use crate::progress::TestProgress;

    // Build a minimal Config suitable for unit tests.
    fn make_test_config() -> Config {
        Config {
            restore: crate::config::Restore::default(),
            recovery_os: crate::config::RecoveryOs::default(),
            general: General {
                version: "0.6.0".into(),
                install_prefix: "/usr".into(),
                db_path: "/tmp/test.db".into(),
                log_file: "/tmp/test.log".into(),
                growth_log: "/tmp/growth.log".into(),
                last_report: "/tmp/last-report.txt".into(),
                btrbk_conf: "/nonexistent/btrbk.conf".into(),
            },
            init: Init {
                system: InitSystem::Systemd,
            },
            schedule: Schedule {
                incremental: "03:00".into(),
                full: "Sun 04:00".into(),
                randomized_delay_min: 30,
            },
            das: Das::default(),
            boot: Boot {
                enabled: true,
                subvolumes: vec!["@".into(), "@home".into()],
                archive_retention_days: 365,
            },
            scrub: Scrub::default(),
            doctor: Doctor::default(),
            subvolumes: crate::config::Subvolumes::default(),
            sources: vec![
                Source {
                    label: "nvme-root".into(),
                    volume: "/.btrfs-nvme".into(),
                    subvolumes: vec![
                        SubvolConfig {
                            name: "@".into(),
                            manual_only: false,
                            snapshot_name: None,
                            ..Default::default()
                        },
                        SubvolConfig {
                            name: "@home".into(),
                            manual_only: false,
                            snapshot_name: None,
                            ..Default::default()
                        },
                    ],
                    device: "/dev/nvme0n1p2".into(),
                    snapshot_dir: ".btrbk-snapshots".into(),
                    // The target is /proc; `self` already exists there, so
                    // the target-directory step has nothing to create and a
                    // test never writes under a real mount point.
                    target_subdirs: vec!["self".into()],
                    target_labels: vec![],
                },
                Source {
                    label: "manual-src".into(),
                    volume: "/.btrfs-manual".into(),
                    subvolumes: vec![SubvolConfig {
                        name: "@special".into(),
                        manual_only: true,
                        snapshot_name: None,
                        ..Default::default()
                    }],
                    device: "/dev/sdb".into(),
                    snapshot_dir: ".btrbk-snapshots".into(),
                    // The target is /proc; `self` already exists there, so
                    // the target-directory step has nothing to create and a
                    // test never writes under a real mount point.
                    target_subdirs: vec!["self".into()],
                    target_labels: vec![],
                },
            ],
            targets: vec![Target {
                label: "primary-22tb".into(),
                serial: "TESTSERIAL".into(),
                serials: vec!["TESTSERIAL".into()],
                mount_uuid: None,
                // Use a path that's definitely mounted in any Linux test environment.
                mount: "/proc".into(),
                role: TargetRole::Primary,
                retention: Retention {
                    weekly: 4,
                    monthly: 2,
                    daily: 365,
                    yearly: 4,
                },
                display_name: "Test 22TB".into(),
            }],
            email: Email::default(),
            gui: Gui::default(),
        }
    }

    // -----------------------------------------------------------------
    // parse_btrbk_snapshot_count
    // -----------------------------------------------------------------

    #[test]
    fn test_parse_btrbk_snapshot_count() {
        // btrbk marks created snapshots with +++
        let output = "\
+++ /.btrfs-nvme/.btrbk-snapshots/root.20260228T030012
+++ /.btrfs-nvme/.btrbk-snapshots/home.20260228T030012
>>> /mnt/backup/nvme/root.20260228T030012
=== /.btrfs-nvme/.btrbk-snapshots/root.20260227T030012
--- /.btrfs-nvme/.btrbk-snapshots/root.20260220T030012
";
        let count = parse_btrbk_snapshot_count(output);
        assert_eq!(count, 2, "should count 2 +++ lines, got {count}");
    }

    #[test]
    fn test_parse_btrbk_snapshot_count_empty() {
        assert_eq!(parse_btrbk_snapshot_count(""), 0);
    }

    #[test]
    fn test_parse_btrbk_snapshot_count_no_snapshots() {
        let output = "=== up-to-date\n--- deleted old\n";
        assert_eq!(parse_btrbk_snapshot_count(output), 0);
    }

    #[test]
    fn test_parse_btrbk_send_count() {
        // btrbk marks incremental sends with >>> and full sends with ***
        let output = "\
+++ /.btrfs-nvme/.btrbk-snapshots/root.20260302T0835
>>> /mnt/backup-22tb/nvme/root.20260302T0835
>>> /mnt/backup-system-recovery-B/nvme/root.20260302T0835
*** /mnt/backup-system-recovery-A/nvme/root.20260302T0835
=== /.btrfs-nvme/.btrbk-snapshots/home.20260302T0828
--- /mnt/backup-22tb/nvme/root.20260220T030012
";
        let count = parse_btrbk_send_count(output);
        assert_eq!(count, 3, "should count 2 >>> + 1 ***, got {count}");
    }

    #[test]
    fn test_parse_btrbk_send_count_none() {
        let output = "+++ snapshot\n=== up-to-date\n--- deleted\n";
        assert_eq!(parse_btrbk_send_count(output), 0);
    }

    // -----------------------------------------------------------------
    // parse_btrbk_size_field
    // -----------------------------------------------------------------

    #[test]
    fn test_parse_size_field_incremental() {
        let line = "*** /mnt/backup-22tb/nvme/root.20260302T0835 (incremental, 45.3 MiB)";
        let bytes = parse_btrbk_size_field(line);
        // 45.3 * 1_048_576 = 47_508_377
        assert!(bytes > 47_000_000 && bytes < 48_000_000, "got {bytes}");
    }

    #[test]
    fn test_parse_size_field_full_send() {
        let line = ">>> /mnt/backup-22tb/nvme/root.20260302T0835 (full send, 1.2 GiB)";
        let bytes = parse_btrbk_size_field(line);
        // 1.2 * 1_073_741_824 = 1_288_490_188
        assert!(
            bytes > 1_200_000_000 && bytes < 1_400_000_000,
            "got {bytes}"
        );
    }

    #[test]
    fn test_parse_size_field_no_parens() {
        let line = ">>> /mnt/backup-22tb/nvme/root.20260302T0835";
        assert_eq!(parse_btrbk_size_field(line), 0);
    }

    #[test]
    fn test_parse_size_field_no_size_in_parens() {
        let line = ">>> /mnt/backup-22tb/nvme/root.20260302T0835 (incremental)";
        assert_eq!(parse_btrbk_size_field(line), 0);
    }

    // -----------------------------------------------------------------
    // format_timestamp
    // -----------------------------------------------------------------

    #[test]
    fn test_format_timestamp() {
        let ts = format_timestamp();
        // Must match YYYYMMDDTHHMMSS: 15 chars, digit positions, 'T' at index 8.
        assert_eq!(ts.len(), 15, "timestamp length must be 15, got '{ts}'");
        assert_eq!(&ts[8..9], "T", "char at index 8 must be 'T', got '{ts}'");
        // All other characters must be ASCII digits.
        for (i, ch) in ts.chars().enumerate() {
            if i == 8 {
                continue;
            }
            assert!(
                ch.is_ascii_digit(),
                "char {i} ('{ch}') must be a digit in '{ts}'"
            );
        }
        // Year must be >= 2026 (this test was written in 2026).
        let year: u32 = ts[0..4].parse().expect("year must be numeric");
        assert!(year >= 2026, "year {year} should be >= 2026");
    }

    // -----------------------------------------------------------------
    // Dry-run: no commands spawned
    // -----------------------------------------------------------------

    #[test]
    fn test_dry_run_doesnt_execute() {
        let config = make_test_config();
        let options = BackupOptions {
            dry_run: true,
            ..Default::default()
        };
        let progress = TestProgress::new();

        let result = run_backup_with(
            &config,
            &options,
            &progress,
            &env(&Scripted::from_owned(vec![])),
        )
        .expect("dry_run should succeed even with non-existent btrbk.conf");

        assert!(result.success, "dry_run result must be success");
        assert_eq!(
            result.snapshots_created, 0,
            "dry_run must create 0 snapshots"
        );
        assert_eq!(result.snapshots_sent, 0, "dry_run must send 0 snapshots");
        assert_eq!(result.bytes_sent, 0);
        assert!(!result.boot_archived);

        // Verify at least one DRY RUN log message was emitted.
        let logs = progress.logs.lock().unwrap();
        assert!(
            logs.iter().any(|(_, msg)| msg.contains("DRY RUN")),
            "expected DRY RUN log message, got: {logs:?}"
        );

        // run_backup is a step of a job, not the job: it does not end it.
        // `run_backup_job`'s caller does, once (bd DAS-Backup-Manager-6bp).
        assert!(
            progress.completed.lock().unwrap().is_none(),
            "run_backup must not call on_complete"
        );
    }

    // -----------------------------------------------------------------
    // No targets mounted -> error (non dry-run)
    // -----------------------------------------------------------------

    #[test]
    fn test_run_backup_checks_mounted_targets() {
        let mut config = make_test_config();
        // Override target mount to something that cannot be mounted.
        config.targets[0].mount = "/nonexistent/das/mount".into();

        let options = BackupOptions {
            dry_run: false,
            ..Default::default()
        };
        let progress = TestProgress::new();

        let result = run_backup_with(
            &config,
            &options,
            &progress,
            &env(&Scripted::from_owned(vec![])),
        );
        assert!(result.is_err(), "must fail when no targets are mounted");
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.to_lowercase().contains("no backup targets"),
            "error message must mention targets, got: '{err_msg}'"
        );
    }

    // -----------------------------------------------------------------
    // Existing tests (unchanged)
    // -----------------------------------------------------------------

    #[test]
    fn backup_options_defaults() {
        let opts = BackupOptions::default();
        assert!(opts.mode.is_none());
        assert!(opts.sources.is_empty());
        assert!(opts.targets.is_empty());
        assert!(!opts.dry_run);
        assert!(!opts.snapshot_only);
        assert!(!opts.send_only);
        assert!(!opts.boot_archive);
        assert!(!opts.index_after);
        assert!(!opts.send_report);
    }

    #[test]
    fn backup_mode_equality() {
        assert_eq!(BackupMode::Incremental, BackupMode::Incremental);
        assert_ne!(BackupMode::Incremental, BackupMode::Full);
    }

    // -----------------------------------------------------------------
    // Throughput parsing
    // -----------------------------------------------------------------

    #[test]
    fn test_parse_throughput_mib_s_glued() {
        // "22.3MiB/s" glued token
        let bps = parse_glued_throughput("22.3MiB/s");
        assert!(bps.is_some());
        let bps = bps.unwrap();
        assert!(
            bps > 20_000_000 && bps < 25_000_000,
            "22.3 MiB/s ~ {bps} B/s"
        );
    }

    #[test]
    fn test_parse_throughput_line_spaced() {
        // "send 22.3 MiB/s" with space between value and unit
        let bps = parse_throughput_line("send 22.3 MiB/s");
        assert!(
            bps > 20_000_000 && bps < 25_000_000,
            "22.3 MiB/s ~ {bps} B/s"
        );
    }

    #[test]
    fn test_parse_throughput_line_no_throughput() {
        assert_eq!(parse_throughput_line("Snapshot /.btrfs/root.20260228"), 0);
    }

    // -----------------------------------------------------------------
    // archive_boot: mirror-role targets must never be archived/replaced
    // (bd DAS-Backup-Manager-am1)
    // -----------------------------------------------------------------

    // --- bd DAS-Backup-Manager-az3 --------------------------------------
    // A child that writes more than one pipe buffer (~64 KiB) of stderr used to
    // deadlock stream_command: it blocked in write(2), stopped producing
    // stdout, and the stdout loop waited forever for a line that never came.
    // Run it on a worker thread with a wall-clock bound so a regression FAILS
    // instead of hanging the whole suite.
    #[test]
    fn stream_command_survives_more_stderr_than_a_pipe_buffer() {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let progress = TestProgress::new();
            let mut cmd = Command::new("sh");
            // 200 KiB of stderr, comfortably past the 64 KiB default, plus a
            // little stdout so the reader has something to consume first.
            cmd.arg("-c")
                .arg("echo start; head -c 204800 /dev/zero | tr '\\0' 'x' >&2; echo done");
            let _ = tx.send(stream_command(&mut cmd, &SystemRunner, &progress, |_| {}).is_ok());
        });
        match rx.recv_timeout(std::time::Duration::from_secs(20)) {
            Ok(ok) => assert!(ok, "stream_command reported failure"),
            Err(_) => panic!("stream_command deadlocked on a large stderr write"),
        }
    }

    // --- the btrbk steps through a scripted runner (bd DAS-Backup-Manager-thi) ---
    //
    // `create_snapshots`, `send_snapshots` and `run_full_pipeline` spawned btrbk
    // themselves, so nothing they decide could be tested without running it —
    // which is how 152 of the 243 mutants in this file survived. Each now runs
    // btrbk through a `CommandRunner`, and these tests hand it a scripted one:
    // it answers by the exact argument vector and exits 1 for any command it was
    // not told to expect, so a changed argument vector fails a test.

    use crate::fsutil::testing::Scripted;

    const CONF: &str = "/test/btrbk.conf";
    const PRIMARY: &str = "/mnt/test-primary";
    const RECOVERY: &str = "/mnt/test-recovery";

    /// `btrbk -c <conf> <args>`, as the scripted runner keys it.
    fn btrbk(args: &str) -> String {
        btrbk_at(CONF, args)
    }

    /// The same for a config that names its own btrbk.conf.
    fn btrbk_at(conf: &str, args: &str) -> String {
        format!("btrbk -c {conf} {args}")
    }

    fn labels(l: &[&str]) -> Vec<String> {
        l.iter().map(|s| s.to_string()).collect()
    }

    /// Four subvolumes in three sources, two targets. `nvme-root` and `nvme-vm`
    /// share a volume, so a volume path cannot tell them apart, and `nvme-vm`
    /// sends to the primary target only.
    fn steps_config() -> Config {
        let mut config = make_test_config();
        config.general.btrbk_conf = CONF.into();
        let source = |label: &str, volume: &str, subvolumes: &[&str]| Source {
            label: label.into(),
            volume: volume.into(),
            subvolumes: subvolumes
                .iter()
                .map(|name| SubvolConfig {
                    name: name.to_string(),
                    ..Default::default()
                })
                .collect(),
            device: "/dev/test".into(),
            snapshot_dir: ".btrbk-snapshots".into(),
            target_subdirs: vec![label.into()],
            target_labels: vec![],
        };
        let mut vm = source("nvme-vm", "/.btrfs-nvme", &["@vm"]);
        vm.target_labels = labels(&["primary-22tb"]);
        config.sources = vec![
            source("nvme-root", "/.btrfs-nvme", &["@", "@home"]),
            vm,
            source("hdd", "/.btrfs-hdd", &["@data"]),
        ];
        config.targets[0].mount = PRIMARY.into();
        let mut recovery = config.targets[0].clone();
        recovery.label = "recovery".into();
        recovery.mount = RECOVERY.into();
        config.targets.push(recovery);
        config
    }

    /// Every filter naming the primary target, in the order the file declares
    /// them: all four subvolumes (`@` is snapshotted as `root`).
    fn primary_filters() -> String {
        ["nvme-root/root", "nvme-root/home", "nvme-vm/vm", "hdd/data"]
            .map(|p| format!("{PRIMARY}/{p}"))
            .join(" ")
    }

    /// One row of `btrbk --format=raw list latest`; an empty `target` is a
    /// snapshot that has not been sent anywhere.
    fn raw_row(snapshot: &str, target: &str) -> String {
        format!(
            "source_subvolume='/.btrfs-nvme/@' snapshot_subvolume='{snapshot}' \
             target_subvolume='{target}' target_type='send-receive'\n"
        )
    }

    /// A step environment for the tests: `runner` runs every command, and every
    /// target verifies clean — unless a test hands in its own `verify`.
    fn env(runner: &dyn CommandRunner) -> StepEnv<'_> {
        env_for(runner, &|_, _, _| Ok(()))
    }

    /// A runner that cannot start any program, as when btrbk is not installed.
    struct Unspawnable;

    impl CommandRunner for Unspawnable {
        fn output(&self, _: &mut Command) -> std::io::Result<std::process::Output> {
            Err(std::io::Error::from(std::io::ErrorKind::NotFound))
        }
        fn stream(
            &self,
            _: &mut Command,
            _: &mut dyn FnMut(&str),
        ) -> std::io::Result<std::process::Output> {
            Err(std::io::Error::from(std::io::ErrorKind::NotFound))
        }
    }

    fn logged(progress: &TestProgress, level: LogLevel, text: &str) -> bool {
        progress
            .logs
            .lock()
            .unwrap()
            .iter()
            .any(|(l, m)| *l == level && m == text)
    }

    // -- btrbk_filters: the sources and targets a step may touch --------------

    fn filters_for(
        config: &Config,
        sources: &[&str],
        targets: Option<&[&str]>,
    ) -> Result<Vec<String>, String> {
        let targets = targets.map(labels);
        btrbk_filters(
            config,
            &labels(sources),
            targets.as_deref(),
            &TestProgress::new(),
        )
    }

    #[test]
    fn a_selection_of_everything_is_no_filter_at_all() {
        let config = steps_config();
        let every = ["primary-22tb", "recovery"];
        for sources in [&[][..], &["nvme-root", "nvme-vm", "hdd"][..]] {
            assert_eq!(filters_for(&config, sources, Some(&every)), Ok(vec![]));
            assert_eq!(filters_for(&config, sources, None), Ok(vec![]));
        }
    }

    #[test]
    fn an_unticked_target_is_in_no_filter() {
        let config = steps_config();
        let filters = filters_for(&config, &[], Some(&["primary-22tb"])).unwrap();
        assert_eq!(filters.join(" "), primary_filters());
        assert!(!filters.iter().any(|f| f.contains(RECOVERY)), "{filters:?}");
        // And the other way round: only the recovery target's directories.
        let filters = filters_for(&config, &[], Some(&["recovery"])).unwrap();
        assert!(
            filters.iter().all(|f| f.starts_with(RECOVERY)),
            "{filters:?}"
        );
        // nvme-vm sends to the primary only, so it has no recovery filter.
        assert_eq!(
            filters,
            [
                format!("{RECOVERY}/nvme-root/root"),
                format!("{RECOVERY}/nvme-root/home"),
                format!("{RECOVERY}/hdd/data")
            ]
        );
    }

    /// bd 7tx 1a: the old filter was the volume path, and `nvme-root` and
    /// `nvme-vm` share `/.btrfs-nvme`, so unticking one excluded neither.
    #[test]
    fn a_source_is_selected_by_its_subvolumes_not_by_its_volume() {
        let config = steps_config();
        let both = ["primary-22tb", "recovery"];
        let filters = filters_for(&config, &["nvme-root"], Some(&both)).unwrap();
        assert_eq!(
            filters,
            [
                format!("{PRIMARY}/nvme-root/root"),
                format!("{RECOVERY}/nvme-root/root"),
                format!("{PRIMARY}/nvme-root/home"),
                format!("{RECOVERY}/nvme-root/home"),
            ]
        );
        assert!(
            !filters.iter().any(|f| f.contains("nvme-vm")),
            "the source on the same volume is not selected: {filters:?}"
        );
        // A step with no target is filtered by subvolume path, never by volume.
        assert_eq!(
            filters_for(&config, &["nvme-root"], None),
            Ok(labels(&["/.btrfs-nvme/@", "/.btrfs-nvme/@home"]))
        );
        assert_eq!(
            filters_for(&config, &["nvme-vm", "hdd"], None),
            Ok(labels(&["/.btrfs-nvme/@vm", "/.btrfs-hdd/@data"]))
        );
    }

    #[test]
    fn a_filter_names_the_snapshot_name_btrbk_gives_not_the_subvolume_name() {
        let mut config = steps_config();
        config.sources[0].subvolumes[1].snapshot_name = Some("home-data".into());
        let filters = filters_for(&config, &["nvme-root"], Some(&["primary-22tb"])).unwrap();
        assert_eq!(
            filters,
            [
                format!("{PRIMARY}/nvme-root/root"),
                format!("{PRIMARY}/nvme-root/home-data")
            ]
        );
    }

    #[test]
    fn a_retired_subvolume_is_not_named() {
        let mut config = steps_config();
        config.sources[0].subvolumes[1].retired = Some("2026-10-01".into());
        let filters = filters_for(&config, &["nvme-root"], Some(&["primary-22tb"])).unwrap();
        assert_eq!(filters, [format!("{PRIMARY}/nvme-root/root")]);
    }

    #[test]
    fn a_source_that_sends_to_no_selected_target_is_said_and_left_out() {
        let config = steps_config();
        let progress = TestProgress::new();
        // nvme-vm sends to the primary target only; the recovery one is selected.
        let filters = btrbk_filters(
            &config,
            &labels(&["nvme-vm", "hdd"]),
            Some(&labels(&["recovery"])),
            &progress,
        )
        .unwrap();
        assert_eq!(filters, [format!("{RECOVERY}/hdd/data")]);
        assert!(logged(
            &progress,
            LogLevel::Warning,
            "Source 'nvme-vm' sends to none of the selected targets — nothing is done for it"
        ));
        // A source that does send there is not warned about.
        assert!(
            !progress
                .logs
                .lock()
                .unwrap()
                .iter()
                .any(|(_, m)| m.contains("'hdd'"))
        );
    }

    #[test]
    fn nothing_selected_is_an_error_and_never_an_empty_filter_list() {
        let config = steps_config();
        // btrbk reads an empty filter list as everything.
        for result in [
            filters_for(&config, &["nvme-vm"], Some(&["recovery"])),
            filters_for(&config, &[], Some(&[])),
        ] {
            let err = result.unwrap_err();
            assert!(err.contains("there is nothing to run"), "{err}");
        }
        let mut config = steps_config();
        for sv in &mut config.sources[1].subvolumes {
            sv.retired = Some("2026-10-01".into());
        }
        let err = filters_for(&config, &["nvme-vm"], None).unwrap_err();
        assert!(
            err.contains("none of the selected sources has a subvolume to snapshot"),
            "{err}"
        );
    }

    #[test]
    fn a_label_that_is_not_in_the_configuration_is_refused_not_ignored() {
        let config = steps_config();
        assert_eq!(
            filters_for(&config, &["hdd", "typo"], Some(&["primary-22tb"])),
            Err("source 'typo' is not in the configuration".to_string())
        );
        assert_eq!(
            filters_for(&config, &["hdd"], Some(&["primary-22tb", "typo"])),
            Err("target 'typo' is not in the configuration".to_string())
        );
        assert_eq!(
            filters_for(&config, &["typo"], None),
            Err("source 'typo' is not in the configuration".to_string())
        );
    }

    #[test]
    fn a_path_btrbk_would_read_as_a_wildcard_or_a_relative_path_is_refused() {
        for mount in ["/mnt/back*up", "relative/mount", "/mnt/with\nnewline"] {
            let mut config = steps_config();
            config.targets[0].mount = mount.into();
            let err = filters_for(&config, &["hdd"], Some(&["primary-22tb"])).unwrap_err();
            assert!(
                err.contains("cannot be given to btrbk as a filter"),
                "{err}"
            );
        }
        let mut config = steps_config();
        config.sources[2].volume = "/.btrfs-h*".into();
        let err = filters_for(&config, &["hdd"], None).unwrap_err();
        assert!(
            err.contains("cannot be given to btrbk as a filter"),
            "{err}"
        );
    }

    /// The filters name exactly what the file declares: each one is a
    /// `<target dir>/<snapshot name>` the rendered `btrbk.conf` has, and
    /// selecting one source and one target at a time reaches every pair.
    /// btrbk answers a filter that matches nothing with exit 2.
    #[test]
    fn every_filter_is_a_declaration_of_the_rendered_btrbk_conf() {
        let config = steps_config();
        let declared =
            crate::btrbk_conf::pairs_declared_in(&crate::btrbk_conf::render_btrbk_conf(&config));
        let mut reached = std::collections::BTreeSet::new();
        for source in &config.sources {
            for target in &config.targets {
                let got = btrbk_filters(
                    &config,
                    std::slice::from_ref(&source.label),
                    Some(std::slice::from_ref(&target.label)),
                    &TestProgress::new(),
                );
                let Ok(filters) = got else {
                    // nvme-vm has no recovery pair.
                    assert_eq!(
                        (source.label.as_str(), target.label.as_str()),
                        ("nvme-vm", "recovery")
                    );
                    continue;
                };
                for f in filters {
                    assert!(declared.contains(&f), "{f} is not declared in {declared:?}");
                    reached.insert(f);
                }
            }
        }
        assert_eq!(reached, declared);
    }

    // -- create_snapshots ------------------------------------------------------

    #[test]
    fn snapshots_of_everything_are_run_with_no_filter() {
        let runner = Scripted::from_owned(vec![
            (btrbk("snapshot"), 0, String::new()),
            (
                btrbk("--format=raw list latest"),
                0,
                // Two series, one listed twice: the count is of snapshots.
                raw_row("/s/a.1", "") + &raw_row("/s/b.1", "") + &raw_row("/s/a.1", ""),
            ),
        ]);
        let progress = TestProgress::new();
        let count = create_snapshots_with(
            &steps_config(),
            &labels(&["nvme-root", "nvme-vm", "hdd"]),
            None,
            &progress,
            &runner,
        )
        .unwrap();
        assert_eq!(count, 2);
        assert_eq!(
            runner.calls(),
            [btrbk("snapshot"), btrbk("--format=raw list latest")]
        );
        assert_eq!(
            *progress.stages.lock().unwrap(),
            [("Creating snapshots".to_string(), 3)]
        );
        assert_eq!(
            *progress.steps.lock().unwrap(),
            [
                (1, 3, "nvme-root".to_string()),
                (2, 3, "nvme-vm".to_string()),
                (3, 3, "hdd".to_string())
            ]
        );
        assert!(logged(&progress, LogLevel::Info, "Snapshots created: 2"));
        assert!(logged(
            &progress,
            LogLevel::Info,
            "Snapshotting source 'nvme-vm' at /.btrfs-nvme"
        ));
    }

    #[test]
    fn snapshots_of_one_source_name_its_subvolumes_and_not_the_other_on_its_volume() {
        let filters = "/.btrfs-nvme/@ /.btrfs-nvme/@home";
        let runner = Scripted::from_owned(vec![
            (btrbk(&format!("snapshot {filters}")), 0, String::new()),
            (
                btrbk(&format!("--format=raw list latest {filters}")),
                0,
                raw_row("/s/a.1", ""),
            ),
        ]);
        let count = create_snapshots_with(
            &steps_config(),
            &labels(&["nvme-root"]),
            None,
            &TestProgress::new(),
            &runner,
        )
        .unwrap();
        assert_eq!(count, 1);
        assert_eq!(
            runner.calls(),
            [
                btrbk(&format!("snapshot {filters}")),
                btrbk(&format!("--format=raw list latest {filters}"))
            ],
            "the listing that counts is limited to the same subvolumes"
        );
    }

    #[test]
    fn snapshots_in_a_run_are_confined_to_the_subvolumes_that_go_to_its_targets() {
        let filters = primary_filters();
        let runner = Scripted::from_owned(vec![
            (btrbk(&format!("snapshot {filters}")), 0, String::new()),
            (
                btrbk(&format!("--format=raw list latest {filters}")),
                0,
                raw_row("/s/a.1", ""),
            ),
        ]);
        let primary = labels(&["primary-22tb"]);
        create_snapshots_with(
            &steps_config(),
            &[],
            Some(primary.as_slice()),
            &TestProgress::new(),
            &runner,
        )
        .unwrap();
        let calls = runner.calls();
        assert_eq!(calls[0], btrbk(&format!("snapshot {filters}")));
        assert!(
            !calls.iter().any(|c| c.contains(RECOVERY)),
            "an unticked target is not named to btrbk: {calls:?}"
        );
    }

    #[test]
    fn a_source_that_is_not_in_the_configuration_is_refused_before_btrbk_runs() {
        let runner = Scripted::from_owned(vec![]);
        let err = create_snapshots_with(
            &steps_config(),
            &labels(&["typo"]),
            None,
            &TestProgress::new(),
            &runner,
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("source 'typo' is not in the configuration"),
            "{err}"
        );
        // Not "no volume filter, so every volume": nothing ran at all.
        assert!(runner.calls().is_empty());
    }

    #[test]
    fn a_failed_snapshot_run_is_an_error_and_is_not_counted() {
        let runner = Scripted::from_owned(vec![(btrbk("snapshot"), 10, String::new())])
            .with_stderr(&btrbk("snapshot"), "ERROR: x\n\n  \nWARNING: y\n");
        let progress = TestProgress::new();
        let err = create_snapshots_with(&steps_config(), &[], None, &progress, &runner)
            .unwrap_err()
            .to_string();
        assert_eq!(
            err,
            "btrbk snapshot command failed (exit status 10). No snapshots can be assumed created."
        );
        assert_eq!(
            runner.calls(),
            [btrbk("snapshot")],
            "a failed run is not followed by a listing"
        );
        assert!(logged(&progress, LogLevel::Error, &err));
        // Each non-blank stderr line is a warning; blank ones are not logged.
        assert!(logged(
            &progress,
            LogLevel::Warning,
            "btrbk stderr: ERROR: x"
        ));
        assert!(logged(
            &progress,
            LogLevel::Warning,
            "btrbk stderr: WARNING: y"
        ));
        let warnings = progress
            .logs
            .lock()
            .unwrap()
            .iter()
            .filter(|(l, m)| *l == LogLevel::Warning && m.starts_with("btrbk stderr:"))
            .count();
        assert_eq!(warnings, 2);
    }

    #[test]
    fn snapshot_counts_fall_back_to_the_markers_when_the_listing_cannot_be_read() {
        // The listing is not scripted, so it exits 1.
        let runner = Scripted::from_owned(vec![(
            btrbk("snapshot"),
            0,
            "+++ /s/a.1\n  +++ /s/b.1\n>>> /t/a.1\n".into(),
        )]);
        let progress = TestProgress::new();
        let count = create_snapshots_with(&steps_config(), &[], None, &progress, &runner).unwrap();
        assert_eq!(count, 2, "the two +++ lines");
        assert!(logged(
            &progress,
            LogLevel::Warning,
            "btrbk --format=raw list latest failed — counts fall back to output markers"
        ));
    }

    #[test]
    fn a_btrbk_that_cannot_be_started_is_an_error_in_every_step() {
        let config = steps_config();
        let progress = TestProgress::new();
        let primary = labels(&["primary-22tb"]);
        assert!(create_snapshots_with(&config, &[], None, &progress, &Unspawnable).is_err());
        assert!(
            send_snapshots_with(&config, &[], &primary, false, &progress, &env(&Unspawnable))
                .is_err()
        );
        assert!(
            run_full_pipeline_with(&config, &[], &primary, &progress, &env(&Unspawnable)).is_err()
        );
    }

    #[test]
    fn describe_exit_says_the_status_or_the_signal() {
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(
            describe_exit(ExitStatus::from_raw(10 << 8)),
            "exit status 10"
        );
        assert_eq!(describe_exit(ExitStatus::from_raw(0)), "exit status 0");
        // Killed by SIGKILL: no exit status, a signal.
        assert_eq!(describe_exit(ExitStatus::from_raw(9)), "killed by signal 9");
        // Stopped, not ended: neither.
        assert_eq!(
            describe_exit(ExitStatus::from_raw(0x137f)),
            "no exit status"
        );
    }

    // -- send_snapshots --------------------------------------------------------

    #[test]
    fn send_to_every_target_of_every_source_names_no_filter_and_preserve_only_when_asked() {
        let every = labels(&["primary-22tb", "recovery"]);
        for (preserve, argv) in [
            (true, format!("btrbk --preserve -c {CONF} resume")),
            (false, btrbk("resume")),
        ] {
            let runner = Scripted::from_owned(vec![(argv.clone(), 0, String::new())]);
            send_snapshots_with(
                &steps_config(),
                &[],
                &every,
                preserve,
                &TestProgress::new(),
                &env(&runner),
            )
            .unwrap();
            assert_eq!(
                runner.calls(),
                [argv, btrbk("--format=raw list latest")],
                "preserve={preserve}"
            );
        }
    }

    /// The unticked target (`recovery`) never reaches btrbk. The scripted
    /// runner knows only the filtered command, so sending without the filter
    /// is a failure of this test, not a quiet write.
    #[test]
    fn send_limits_btrbk_to_the_targets_selected() {
        let filters = primary_filters();
        let runner = Scripted::from_owned(vec![(
            btrbk(&format!("resume {filters}")),
            0,
            String::new(),
        )]);
        let progress = TestProgress::new();
        send_snapshots_with(
            &steps_config(),
            &[],
            &labels(&["primary-22tb"]),
            false,
            &progress,
            &env(&runner),
        )
        .unwrap();
        let calls = runner.calls();
        assert_eq!(calls[0], btrbk(&format!("resume {filters}")));
        assert_eq!(
            calls[1],
            btrbk(&format!("--format=raw list latest {filters}"))
        );
        assert!(
            !calls.iter().any(|c| c.contains(RECOVERY)),
            "an unticked target is not named to btrbk: {calls:?}"
        );
        assert!(logged(
            &progress,
            LogLevel::Info,
            &format!("Target 'primary-22tb' at {PRIMARY}: will receive")
        ));
        assert!(logged(
            &progress,
            LogLevel::Info,
            &format!(
                "Target 'recovery' at {RECOVERY}: not part of this run — btrbk is told to \
                 leave it alone"
            )
        ));
    }

    #[test]
    fn send_limits_btrbk_to_the_sources_selected_by_subvolume() {
        let filters =
            format!("{PRIMARY}/nvme-root/root {PRIMARY}/nvme-root/home {PRIMARY}/hdd/data");
        let runner = Scripted::from_owned(vec![(
            btrbk(&format!("resume {filters}")),
            0,
            String::new(),
        )]);
        send_snapshots_with(
            &steps_config(),
            &labels(&["hdd", "nvme-root"]),
            &labels(&["primary-22tb"]),
            false,
            &TestProgress::new(),
            &env(&runner),
        )
        .unwrap();
        assert_eq!(runner.calls()[0], btrbk(&format!("resume {filters}")));
    }

    /// A target that IS selected and cannot be read still fails the step: btrbk
    /// aborts it and exits 10, and the error says so and carries its message.
    #[test]
    fn a_selected_target_btrbk_cannot_read_still_fails_the_step_and_says_how() {
        let filters = primary_filters();
        let argv = btrbk(&format!("resume {filters}"));
        let runner = Scripted::from_owned(vec![(argv.clone(), 10, String::new())]).with_stderr(
            &argv,
            "WARNING: Skipping target \"/mnt/test-primary/hdd\": Failed to fetch subvolume detail\n",
        );
        let progress = TestProgress::new();
        let err = send_snapshots_with(
            &steps_config(),
            &[],
            &labels(&["primary-22tb"]),
            false,
            &progress,
            &env(&runner),
        )
        .unwrap_err()
        .to_string();
        assert_eq!(
            err,
            "btrbk resume command failed (exit status 10). The send may be incomplete."
        );
        assert_eq!(runner.calls(), [argv], "no listing after a failure");
        assert!(logged(&progress, LogLevel::Error, &err));
        assert!(logged(
            &progress,
            LogLevel::Warning,
            "btrbk stderr: WARNING: Skipping target \"/mnt/test-primary/hdd\": Failed to fetch subvolume detail"
        ));
        assert!(logged(
            &progress,
            LogLevel::Info,
            "stream_command: exit=10, stdout_lines=0"
        ));
    }

    #[test]
    fn send_without_targets_goes_to_those_mounted_and_not_to_an_absent_one() {
        // `/proc` is mounted wherever the tests run; the other target's mount
        // point is not.
        let mut config = steps_config();
        config.targets[0].mount = "/proc".into();
        let filters = ["nvme-root/root", "nvme-root/home", "nvme-vm/vm", "hdd/data"]
            .map(|p| format!("/proc/{p}"))
            .join(" ");
        let runner = Scripted::from_owned(vec![(
            btrbk(&format!("resume {filters}")),
            0,
            String::new(),
        )]);
        send_snapshots_with(
            &config,
            &[],
            &[],
            false,
            &TestProgress::new(),
            &env(&runner),
        )
        .unwrap();
        assert_eq!(runner.calls()[0], btrbk(&format!("resume {filters}")));
    }

    #[test]
    fn send_with_no_target_mounted_and_none_named_runs_nothing() {
        let runner = Scripted::from_owned(vec![]);
        let err = send_snapshots_with(
            &steps_config(),
            &[],
            &[],
            false,
            &TestProgress::new(),
            &env(&runner),
        )
        .unwrap_err()
        .to_string();
        assert_eq!(err, NO_TARGETS_MOUNTED);
        assert!(runner.calls().is_empty());
    }

    #[test]
    fn a_target_that_is_not_in_the_configuration_is_refused_before_btrbk_runs() {
        let runner = Scripted::from_owned(vec![]);
        let err = send_snapshots_with(
            &steps_config(),
            &[],
            &labels(&["primary-22tb", "typo"]),
            false,
            &TestProgress::new(),
            &env(&runner),
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("target 'typo' is not in the configuration"),
            "{err}"
        );
        assert!(runner.calls().is_empty(), "{:?}", runner.calls());
    }

    #[test]
    fn send_counts_target_rows_and_adds_up_the_sizes_btrbk_printed() {
        let stdout = ">>> /mnt/backup-22tb/nvme/root.1 (incremental, 45.3 MiB)\n\
                      \x20   22.3 MiB/s\n\
                      *** /mnt/b/nvme/root.1 (1.5 GiB)\n\
                      === /.btrfs-nvme/.btrbk-snapshots/up-to-date\n";
        let runner = Scripted::from_owned(vec![
            (btrbk("resume"), 0, stdout.into()),
            (
                btrbk("--format=raw list latest"),
                0,
                // One snapshot on two targets, one sent nowhere: 2 rows.
                raw_row("/s/a.1", "/t1/a.1")
                    + &raw_row("/s/a.1", "/t2/a.1")
                    + &raw_row("/s/b.1", ""),
            ),
        ]);
        let progress = TestProgress::new();
        let (sent, bytes) = send_snapshots_with(
            &steps_config(),
            &[],
            &labels(&["primary-22tb", "recovery"]),
            false,
            &progress,
            &env(&runner),
        )
        .unwrap();
        assert_eq!(sent, 2, "target rows of the listing, not marker lines");
        // 45.3 MiB + 1.5 GiB, from the two sends' parentheses.
        assert_eq!(bytes, 47_500_492 + 1_610_612_736);
        // 22.3 MiB/s, once.
        assert_eq!(*progress.throughput.lock().unwrap(), [23_383_244]);
        assert!(logged(&progress, LogLevel::Info, "Snapshots sent: 2"));
        assert_eq!(
            *progress.stages.lock().unwrap(),
            [("Sending snapshots".to_string(), 1)]
        );
        assert!(logged(
            &progress,
            LogLevel::Info,
            "stream_command: exit=0, stdout_lines=4"
        ));
    }

    #[test]
    fn send_counts_fall_back_to_the_markers_when_the_listing_cannot_be_read() {
        let runner = Scripted::from_owned(vec![(
            btrbk("resume"),
            0,
            ">>> /t/a.1\n  *** /t/b.1\n+++ /s/c.1\n".into(),
        )]);
        let progress = TestProgress::new();
        let (sent, _) = send_snapshots_with(
            &steps_config(),
            &[],
            &labels(&["primary-22tb", "recovery"]),
            false,
            &progress,
            &env(&runner),
        )
        .unwrap();
        assert_eq!(sent, 2, "the >>> and the *** line");
        assert!(logged(
            &progress,
            LogLevel::Warning,
            "btrbk --format=raw list latest failed — counts fall back to output markers"
        ));
    }

    // -- run_full_pipeline -----------------------------------------------------

    #[test]
    fn the_full_pipeline_runs_btrbk_run_and_counts_the_listing_and_the_deletions() {
        let stdout = "+++ /s/a.1\n>>> /t/a.1\n--- /t/old1\n  --- /t/old2\n=== /t/kept\n";
        let runner = Scripted::from_owned(vec![
            (btrbk("run"), 0, stdout.into()),
            (
                btrbk("--format=raw list latest"),
                0,
                raw_row("/s/a.1", "/t1/a.1")
                    + &raw_row("/s/a.1", "/t2/a.1")
                    + &raw_row("/s/b.1", ""),
            ),
        ]);
        let progress = TestProgress::new();
        let got = run_full_pipeline_with(
            &steps_config(),
            &labels(&["nvme-root", "nvme-vm", "hdd"]),
            &labels(&["primary-22tb", "recovery"]),
            &progress,
            &env(&runner),
        )
        .unwrap();
        // created: distinct snapshots (2); sent: target rows (2);
        // cleaned: the two `---` lines; bytes: no size was printed.
        assert_eq!(got, (2, 2, 2, 0));
        assert_eq!(
            runner.calls(),
            [btrbk("run"), btrbk("--format=raw list latest")]
        );
        assert_eq!(
            *progress.stages.lock().unwrap(),
            [("Full backup (snapshot + send + cleanup)".to_string(), 1)]
        );
        assert!(logged(
            &progress,
            LogLevel::Info,
            "Full backup: 2 created, 2 sent, 2 cleaned up"
        ));
        assert!(logged(
            &progress,
            LogLevel::Info,
            "Source 'hdd' at /.btrfs-hdd"
        ));
    }

    #[test]
    fn the_full_pipeline_limits_btrbk_to_the_sources_and_targets_selected() {
        let filters = format!("{RECOVERY}/hdd/data");
        let runner =
            Scripted::from_owned(vec![(btrbk(&format!("run {filters}")), 0, String::new())]);
        run_full_pipeline_with(
            &steps_config(),
            &labels(&["hdd"]),
            &labels(&["recovery"]),
            &TestProgress::new(),
            &env(&runner),
        )
        .unwrap();
        let calls = runner.calls();
        assert_eq!(calls[0], btrbk(&format!("run {filters}")));
        assert!(!calls.iter().any(|c| c.contains(PRIMARY)), "{calls:?}");
    }

    #[test]
    fn the_full_pipeline_falls_back_to_the_markers_when_the_listing_cannot_be_read() {
        let runner = Scripted::from_owned(vec![(
            btrbk("run"),
            0,
            "+++ /s/a.1\n+++ /s/b.1\n>>> /t/a.1\n--- /t/old\n".into(),
        )]);
        let progress = TestProgress::new();
        let got = run_full_pipeline_with(
            &steps_config(),
            &[],
            &labels(&["primary-22tb", "recovery"]),
            &progress,
            &env(&runner),
        )
        .unwrap();
        assert_eq!(got, (2, 1, 1, 0));
        assert!(logged(
            &progress,
            LogLevel::Warning,
            "btrbk --format=raw list latest failed — counts fall back to output markers"
        ));
    }

    #[test]
    fn a_failed_full_run_is_an_error_and_is_not_counted() {
        let runner = Scripted::from_owned(vec![(btrbk("run"), 10, "+++ /s/a.1\n".into())]);
        let progress = TestProgress::new();
        let err = run_full_pipeline_with(
            &steps_config(),
            &[],
            &labels(&["primary-22tb", "recovery"]),
            &progress,
            &env(&runner),
        )
        .unwrap_err()
        .to_string();
        assert_eq!(
            err,
            "btrbk run command failed (exit status 10). The backup may be incomplete."
        );
        assert_eq!(runner.calls(), [btrbk("run")]);
        assert!(logged(&progress, LogLevel::Error, &err));
    }

    // -- run_pipeline: which steps a run takes, over what -----------------------

    /// What a scripted pipeline did: its calls (filters in `argv` as the
    /// primary-only selection) and what it counted.
    fn pipeline(
        options: BackupOptions,
        mode: BackupMode,
        script: Vec<(String, i32, String)>,
    ) -> (Pipeline, Vec<String>, TestProgress) {
        let runner = Scripted::from_owned(script);
        let progress = TestProgress::new();
        let done = run_pipeline(
            &steps_config(),
            &options,
            mode,
            &[],
            &labels(&["primary-22tb"]),
            &progress,
            &env(&runner),
        );
        (done, runner.calls(), progress)
    }

    fn listing_of(rows: String) -> (String, i32, String) {
        (
            btrbk(&format!("--format=raw list latest {}", primary_filters())),
            0,
            rows,
        )
    }

    #[test]
    fn an_incremental_run_snapshots_and_then_sends_to_the_selected_targets_only() {
        let f = primary_filters();
        let (done, calls, _) = pipeline(
            BackupOptions::default(),
            BackupMode::Incremental,
            vec![
                (btrbk(&format!("snapshot {f}")), 0, String::new()),
                (
                    btrbk(&format!("resume {f}")),
                    0,
                    ">>> /t/a.1 (1.0 KiB)\n".into(),
                ),
                listing_of(raw_row("/s/a.1", "/t1/a.1") + &raw_row("/s/b.1", "/t1/b.1")),
            ],
        );
        assert_eq!(
            calls,
            [
                btrbk(&format!("snapshot {f}")),
                btrbk(&format!("--format=raw list latest {f}")),
                btrbk(&format!("resume {f}")),
                btrbk(&format!("--format=raw list latest {f}")),
            ]
        );
        // The listing serves both steps: 2 snapshots, 2 target rows.
        assert_eq!(
            done,
            Pipeline {
                created: 2,
                sent: 2,
                cleaned: 0,
                bytes: 1024,
                errors: vec![]
            }
        );
        assert!(!calls.iter().any(|c| c.contains(RECOVERY)), "{calls:?}");
    }

    #[test]
    fn an_incremental_run_with_send_only_or_snapshot_only_takes_one_step() {
        let f = primary_filters();
        let script = || {
            vec![
                (btrbk(&format!("snapshot {f}")), 0, String::new()),
                (btrbk(&format!("resume {f}")), 0, String::new()),
                listing_of(raw_row("/s/a.1", "/t1/a.1")),
            ]
        };
        let (done, calls, _) = pipeline(
            BackupOptions {
                send_only: true,
                ..Default::default()
            },
            BackupMode::Incremental,
            script(),
        );
        assert_eq!(calls[0], btrbk(&format!("resume {f}")));
        assert_eq!(calls.len(), 2, "{calls:?}");
        assert_eq!((done.created, done.sent), (0, 1));

        let (done, calls, _) = pipeline(
            BackupOptions {
                snapshot_only: true,
                ..Default::default()
            },
            BackupMode::Incremental,
            script(),
        );
        assert_eq!(calls[0], btrbk(&format!("snapshot {f}")));
        assert_eq!(calls.len(), 2, "{calls:?}");
        assert_eq!((done.created, done.sent), (1, 0));
    }

    #[test]
    fn a_full_run_is_one_btrbk_run_and_its_snapshot_only_and_send_only_forms_are_not() {
        let f = primary_filters();
        let script = || {
            vec![
                (btrbk(&format!("run {f}")), 0, "--- /t/old\n".into()),
                (btrbk(&format!("snapshot {f}")), 0, String::new()),
                (btrbk(&format!("resume {f}")), 0, String::new()),
                listing_of(raw_row("/s/a.1", "/t1/a.1")),
            ]
        };
        let (done, calls, _) = pipeline(BackupOptions::default(), BackupMode::Full, script());
        assert_eq!(calls[0], btrbk(&format!("run {f}")));
        assert_eq!(calls.len(), 2, "{calls:?}");
        assert_eq!(
            done,
            Pipeline {
                created: 1,
                sent: 1,
                cleaned: 1,
                bytes: 0,
                errors: vec![]
            }
        );
        let (done, calls, _) = pipeline(
            BackupOptions {
                snapshot_only: true,
                ..Default::default()
            },
            BackupMode::Full,
            script(),
        );
        assert_eq!(calls[0], btrbk(&format!("snapshot {f}")));
        assert_eq!((done.created, done.sent, done.cleaned), (1, 0, 0));
        let (done, calls, _) = pipeline(
            BackupOptions {
                send_only: true,
                ..Default::default()
            },
            BackupMode::Full,
            script(),
        );
        assert_eq!(calls[0], btrbk(&format!("resume {f}")));
        assert_eq!((done.created, done.sent, done.cleaned), (0, 1, 0));
    }

    #[test]
    fn a_failed_step_is_recorded_and_the_next_one_still_runs() {
        let f = primary_filters();
        let (done, calls, progress) = pipeline(
            BackupOptions::default(),
            BackupMode::Incremental,
            vec![
                (btrbk(&format!("snapshot {f}")), 10, String::new()),
                (btrbk(&format!("resume {f}")), 0, String::new()),
                listing_of(raw_row("/s/a.1", "/t1/a.1")),
            ],
        );
        assert_eq!(
            done.errors,
            [
                "Snapshot step failed: btrbk snapshot command failed (exit status 10). \
              No snapshots can be assumed created."
            ]
        );
        assert_eq!(
            calls.len(),
            3,
            "snapshot, then resume and its listing: {calls:?}"
        );
        assert_eq!((done.created, done.sent), (0, 1));
        assert!(logged(&progress, LogLevel::Error, &done.errors[0]));

        let (done, _, _) = pipeline(
            BackupOptions::default(),
            BackupMode::Incremental,
            vec![(btrbk(&format!("snapshot {f}")), 0, String::new())],
        );
        assert_eq!(done.errors.len(), 1, "{:?}", done.errors);
        assert!(
            done.errors[0]
                .starts_with("Send step failed: btrbk resume command failed (exit status 1)"),
            "{:?}",
            done.errors
        );

        let (done, _, _) = pipeline(
            BackupOptions::default(),
            BackupMode::Full,
            vec![(btrbk(&format!("run {f}")), 2, String::new())],
        );
        assert_eq!(
            done.errors,
            [
                "Full backup pipeline failed: btrbk run command failed (exit status 2). \
              The backup may be incomplete."
            ]
        );
    }

    // -- the guard before every write to a target (bd DAS-Backup-Manager-7tx, 9on) --
    //
    // `btrdasd backup send` and `backup boot-archive`, from the CLI and from the
    // helper, wrote to a target nobody had checked: only `backup run` verified.
    // The check is now inside the steps themselves, so every caller has it.

    /// `runner`, with the real `mount::verify_write_targets` as the check.
    fn env_verifying(runner: &dyn CommandRunner) -> StepEnv<'_> {
        env_for(runner, &mount::verify_write_targets)
    }

    /// The steps' config, its primary target's mount point a plain directory
    /// with nothing mounted on it.
    fn bare_dir_config(dir: &Path) -> Config {
        let mut config = steps_config();
        config.targets[0].mount = dir.to_string_lossy().into_owned();
        config
    }

    fn assert_refused_for_a_bare_dir(err: &str, dir: &Path) {
        assert!(err.contains("Refusing to run btrbk"), "{err}");
        assert!(err.contains("primary-22tb"), "{err}");
        assert!(err.contains("NOT a mount point"), "{err}");
        assert!(err.contains(&dir.to_string_lossy().to_string()), "{err}");
    }

    #[test]
    fn send_refuses_a_bare_mount_point_and_runs_no_btrbk() {
        let dir = tempfile::tempdir().unwrap();
        let runner = Scripted::from_owned(vec![]);
        let err = send_snapshots_with(
            &bare_dir_config(dir.path()),
            &[],
            &labels(&["primary-22tb"]),
            false,
            &TestProgress::new(),
            &env_verifying(&runner),
        )
        .unwrap_err()
        .to_string();
        assert_refused_for_a_bare_dir(&err, dir.path());
        assert!(runner.calls().is_empty(), "{:?}", runner.calls());
    }

    #[test]
    fn a_full_run_refuses_a_bare_mount_point_and_runs_no_btrbk() {
        let dir = tempfile::tempdir().unwrap();
        let runner = Scripted::from_owned(vec![]);
        let err = run_full_pipeline_with(
            &bare_dir_config(dir.path()),
            &[],
            &labels(&["primary-22tb"]),
            &TestProgress::new(),
            &env_verifying(&runner),
        )
        .unwrap_err()
        .to_string();
        assert_refused_for_a_bare_dir(&err, dir.path());
        assert!(runner.calls().is_empty(), "{:?}", runner.calls());
    }

    /// A mount point is not enough: it has to hold the filesystem the config
    /// names. `/proc` is one, and carries no UUID `findmnt` can report.
    #[test]
    fn send_refuses_a_mount_point_that_is_not_the_filesystem_expected() {
        let mut config = steps_config();
        config.targets[0].mount = "/proc".into();
        config.targets[0].mount_uuid = Some("00000000-0000-0000-0000-000000000000".into());
        let runner = Scripted::from_owned(vec![]);
        let err = send_snapshots_with(
            &config,
            &[],
            &labels(&["primary-22tb"]),
            false,
            &TestProgress::new(),
            &env_verifying(&runner),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("Refusing to run btrbk"), "{err}");
        assert!(err.contains("/proc"), "{err}");
        assert!(
            err.contains("could not determine the filesystem UUID")
                || err.contains("has filesystem UUID"),
            "{err}"
        );
        assert!(runner.calls().is_empty(), "{:?}", runner.calls());
    }

    /// What `verify` was asked, as `(every target's label, the labels to be
    /// written)`.
    type Asked = Vec<(Vec<String>, Vec<String>)>;

    #[test]
    fn send_and_the_full_run_verify_exactly_the_targets_they_write_and_then_run_btrbk() {
        let asked = std::sync::Mutex::new(Asked::new());
        let verify = |targets: &[Target],
                      write: &[String],
                      _: &dyn ProgressCallback|
         -> Result<(), String> {
            asked.lock().unwrap().push((
                targets.iter().map(|t| t.label.clone()).collect(),
                write.to_vec(),
            ));
            Ok(())
        };
        let filters = primary_filters();
        let runner = Scripted::from_owned(vec![
            (btrbk(&format!("resume {filters}")), 0, String::new()),
            (btrbk(&format!("run {filters}")), 0, String::new()),
        ]);
        let env = env_for(&runner, &verify);
        let primary = labels(&["primary-22tb"]);
        let config = steps_config();
        send_snapshots_with(&config, &[], &primary, false, &TestProgress::new(), &env).unwrap();
        run_full_pipeline_with(&config, &[], &primary, &TestProgress::new(), &env).unwrap();
        let both = labels(&["primary-22tb", "recovery"]);
        assert_eq!(
            *asked.lock().unwrap(),
            [(both.clone(), primary.clone()), (both, primary)],
            "all the targets are passed, and the labels that will be written"
        );
        assert_eq!(runner.calls().len(), 4, "{:?}", runner.calls());
    }

    #[test]
    fn a_step_whose_verification_fails_runs_nothing_and_says_why() {
        let verify = |_: &[Target], _: &[String], _: &dyn ProgressCallback| -> Result<(), String> {
            Err("recovery is on the wrong disk".to_string())
        };
        let runner = Scripted::from_owned(vec![]);
        let env = env_for(&runner, &verify);
        let config = steps_config();
        let every = labels(&["primary-22tb", "recovery"]);
        let err = send_snapshots_with(&config, &[], &every, false, &TestProgress::new(), &env)
            .unwrap_err()
            .to_string();
        assert_eq!(err, "recovery is on the wrong disk");
        let err = run_full_pipeline_with(&config, &[], &every, &TestProgress::new(), &env)
            .unwrap_err()
            .to_string();
        assert_eq!(err, "recovery is on the wrong disk");
        assert!(runner.calls().is_empty(), "{:?}", runner.calls());
    }

    /// The host's environment is the real thing: the real command runner, and
    /// the real check — which the wiring above rests on.
    #[test]
    fn the_host_environment_runs_real_commands_and_verifies_for_real() {
        let out = StepEnv::HOST
            .runner
            .output(Command::new("sh").args(["-c", "exit 3"]))
            .unwrap();
        assert_eq!(out.status.code(), Some(3));
        let dir = tempfile::tempdir().unwrap();
        let config = bare_dir_config(dir.path());
        let err = (StepEnv::HOST.verify)(
            &config.targets,
            &labels(&["primary-22tb"]),
            &TestProgress::new(),
        )
        .unwrap_err();
        assert_refused_for_a_bare_dir(&err, dir.path());
    }

    #[test]
    fn a_run_verifies_its_targets_before_it_does_anything_else() {
        let asked = std::sync::Mutex::new(Asked::new());
        let verify = |targets: &[Target],
                      write: &[String],
                      _: &dyn ProgressCallback|
         -> Result<(), String> {
            asked.lock().unwrap().push((
                targets.iter().map(|t| t.label.clone()).collect(),
                write.to_vec(),
            ));
            Ok(())
        };
        let runner = Scripted::from_owned(vec![]);
        let options = BackupOptions {
            dry_run: true,
            targets: labels(&["primary-22tb"]),
            ..Default::default()
        };
        let progress = TestProgress::new();
        run_backup_with(
            &make_test_config(),
            &options,
            &progress,
            &env_for(&runner, &verify),
        )
        .unwrap();
        assert_eq!(
            *asked.lock().unwrap(),
            [(labels(&["primary-22tb"]), labels(&["primary-22tb"]))]
        );

        // A target that fails the check ends the run before its first step.
        let refuse = |_: &[Target], _: &[String], _: &dyn ProgressCallback| -> Result<(), String> {
            Err("not the disk it should be".to_string())
        };
        let progress = TestProgress::new();
        let err = run_backup_with(
            &make_test_config(),
            &options,
            &progress,
            &env_for(&runner, &refuse),
        )
        .unwrap_err()
        .to_string();
        assert_eq!(err, "not the disk it should be");
        assert!(progress.stages.lock().unwrap().is_empty());
        assert!(runner.calls().is_empty());
    }

    // -- boot archival: guarded, and its btrfs through the runner ----------------

    /// A config with one live `@` under `mount`, a btrbk.conf naming `root-`,
    /// and a source that sends to `<mount>/nvme`. The conf file lives as long
    /// as the returned guard.
    fn archive_fixture(mount: &Path) -> (Config, tempfile::NamedTempFile) {
        let conf = write_btrbk_conf("@", "root-");
        let mut config = make_test_config();
        config.general.btrbk_conf = conf.path().to_string_lossy().into_owned();
        config.boot.subvolumes = vec!["@".into()];
        config.sources[0].subvolumes = vec![SubvolConfig {
            name: "@".into(),
            ..Default::default()
        }];
        config.sources[0].target_subdirs = vec!["nvme".into()];
        config.targets[0].mount = mount.to_string_lossy().into_owned();
        (config, conf)
    }

    fn another_target(config: &Config, label: &str, mount: &Path, role: TargetRole) -> Target {
        Target {
            label: label.into(),
            mount: mount.to_string_lossy().into_owned(),
            role,
            ..config.targets[0].clone()
        }
    }

    #[test]
    fn boot_archive_refuses_a_bare_mount_point_and_touches_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let live = dir.path().join("@");
        std::fs::create_dir(&live).unwrap();
        let (config, _conf) = archive_fixture(dir.path());
        let runner = Scripted::from_owned(vec![]);
        let err = archive_boot_with(&config, None, &TestProgress::new(), &env_verifying(&runner))
            .unwrap_err()
            .to_string();
        assert!(err.contains("Refusing to run btrbk"), "{err}");
        assert!(err.contains("NOT a mount point"), "{err}");
        assert!(
            err.contains(&dir.path().to_string_lossy().to_string()),
            "{err}"
        );
        assert!(
            runner.calls().is_empty(),
            "no btrfs command may run against it: {:?}",
            runner.calls()
        );
        assert!(live.exists());
    }

    #[test]
    fn boot_archive_verifies_only_the_targets_it_writes_under() {
        let primary = tempfile::tempdir().unwrap();
        let mirror = tempfile::tempdir().unwrap();
        let (mut config, _conf) = archive_fixture(primary.path());
        config.targets.push(another_target(
            &config,
            "recovery-mirror",
            mirror.path(),
            TargetRole::Mirror,
        ));
        config.targets.push(another_target(
            &config,
            "absent",
            Path::new("/nonexistent/das/absent"),
            TargetRole::Primary,
        ));
        let asked = std::sync::Mutex::new(Asked::new());
        let verify = |targets: &[Target],
                      write: &[String],
                      _: &dyn ProgressCallback|
         -> Result<(), String> {
            asked.lock().unwrap().push((
                targets.iter().map(|t| t.label.clone()).collect(),
                write.to_vec(),
            ));
            Ok(())
        };
        let runner = Scripted::from_owned(vec![]);
        archive_boot_with(
            &config,
            None,
            &TestProgress::new(),
            &env_for(&runner, &verify),
        )
        .unwrap();
        // A mirror carries its own OS and is never written; a target whose
        // mount point does not exist is not written either, and is left alone.
        assert_eq!(
            *asked.lock().unwrap(),
            [(
                labels(&["primary-22tb", "recovery-mirror", "absent"]),
                labels(&["primary-22tb"])
            )]
        );
    }

    #[test]
    fn boot_archive_writes_only_under_the_targets_selected() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let (mut config, _conf) = archive_fixture(a.path());
        config.targets.push(another_target(
            &config,
            "second",
            b.path(),
            TargetRole::Primary,
        ));
        let asked = std::sync::Mutex::new(Asked::new());
        let verify =
            |_: &[Target], write: &[String], _: &dyn ProgressCallback| -> Result<(), String> {
                asked.lock().unwrap().push((vec![], write.to_vec()));
                Ok(())
            };
        let runner = Scripted::from_owned(vec![]);
        let env = env_for(&runner, &verify);
        let only_second = labels(&["second"]);
        archive_boot_with(&config, Some(&only_second), &TestProgress::new(), &env).unwrap();
        assert_eq!(*asked.lock().unwrap(), [(vec![], only_second)]);
        assert_eq!(
            runner.calls(),
            [format!("btrfs subvolume list {}", b.path().display())],
            "nothing is read or written under the target left out"
        );
        // Selecting nothing in particular is every target.
        let runner = Scripted::from_owned(vec![]);
        archive_boot_with(
            &config,
            None,
            &TestProgress::new(),
            &env_for(&runner, &verify),
        )
        .unwrap();
        assert_eq!(runner.calls().len(), 2, "{:?}", runner.calls());
    }

    /// `env` for a closure `verify` that is a variable of the test.
    fn env_for<'a>(runner: &'a dyn CommandRunner, verify: &'a VerifyTargets<'a>) -> StepEnv<'a> {
        StepEnv {
            runner,
            verify,
            is_mountpoint: &|_| true,
        }
    }

    /// The whole sequence, in its safe order, against a real directory: the
    /// replacement is found first, the live subvolume is archived, the
    /// replacement is built alongside it, and only then is the live one
    /// removed and the replacement renamed into its place.
    #[test]
    fn boot_archive_replaces_the_live_subvolume_in_its_safe_order() {
        let dir = tempfile::tempdir().unwrap();
        let m = dir.path().display().to_string();
        let live = dir.path().join("@");
        std::fs::create_dir(&live).unwrap();
        std::fs::write(live.join("old"), b"outgoing").unwrap();
        let (config, _conf) = archive_fixture(dir.path());
        let runner = Scripted::from_owned(vec![(
            format!("btrfs subvolume list {m}"),
            0,
            "ID 300 gen 3 top level 5 path nvme/root-.20261004T0100\n\
             ID 257 gen 9 top level 5 path nvme/root-.20261005T0100\n"
                .into(),
        )])
        .snapshotting()
        .deleting_too(vec![format!("{m}/@")]);
        let progress = TestProgress::new();
        let archived = archive_boot_with(&config, None, &progress, &env(&runner)).unwrap();
        assert!(archived);

        let calls = runner.calls();
        assert_eq!(calls.len(), 4, "{calls:?}");
        assert_eq!(calls[0], format!("btrfs subvolume list {m}"));
        let archive_prefix = format!("btrfs subvolume snapshot -r {m}/@ {m}/@.archive.");
        assert!(calls[1].starts_with(&archive_prefix), "{calls:?}");
        assert_eq!(
            calls[2],
            format!("btrfs subvolume snapshot {m}/nvme/root-.20261005T0100 {m}/@.new"),
            "the NEWEST snapshot is built alongside the live subvolume"
        );
        assert_eq!(calls[3], format!("btrfs subvolume delete {m}/@"));

        // The live subvolume is now the replacement, not the outgoing one.
        assert!(live.is_dir());
        assert!(!live.join("old").exists(), "the outgoing contents are gone");
        assert!(!dir.path().join("@.new").exists());
        let archive = calls[1].rsplit(' ').next().unwrap();
        assert!(
            Path::new(archive).is_dir(),
            "the archive was made: {archive}"
        );
        assert!(logged(
            &progress,
            LogLevel::Info,
            &format!("Created {m}/@ from nvme/root-.20261005T0100")
        ));
    }

    /// bd 5ig: no failure path may leave the live subvolume absent. Each step
    /// that can fail is made to, and the live one survives, with what the
    /// run says about it.
    #[test]
    fn boot_archive_never_loses_the_live_subvolume_whichever_step_fails() {
        let list = |m: &str| {
            (
                format!("btrfs subvolume list {m}"),
                0,
                "ID 257 gen 9 top level 5 path nvme/root-.20261005T0100\n".to_string(),
            )
        };
        // (the step made to fail, what must be in the log, calls that must NOT happen)
        type Case = (&'static str, &'static str, &'static [&'static str]);
        let cases: [Case; 3] = [
            // The archive cannot be made: nothing else is done.
            (
                "archive",
                "Failed to archive",
                &["snapshot {m}/nvme", "delete {m}/@"],
            ),
            // The replacement cannot be built: the live one is not touched.
            ("staging", "Failed to create", &["delete {m}/@"]),
            // The live one cannot be removed: the replacement is discarded.
            ("delete", "Failed to delete", &[]),
        ];
        for (failing, said, never) in cases {
            let dir = tempfile::tempdir().unwrap();
            let m = dir.path().display().to_string();
            let live = dir.path().join("@");
            std::fs::create_dir(&live).unwrap();
            std::fs::write(live.join("marker"), b"live").unwrap();
            let (config, _conf) = archive_fixture(dir.path());
            let mut script = vec![list(&m)];
            let stage = format!("btrfs subvolume snapshot {m}/nvme/root-.20261005T0100 {m}/@.new");
            match failing {
                "archive" => {
                    // The archive's name has a timestamp: every snapshot of
                    // the live subvolume fails, the replacement's would not.
                    script.push((stage.clone(), 0, String::new()));
                }
                "staging" => script.push((stage.clone(), 1, String::new())),
                _ => script.push((format!("btrfs subvolume delete {m}/@"), 1, String::new())),
            }
            let mut runner = Scripted::from_owned(script).snapshotting();
            if failing == "archive" {
                runner = runner.failing_snapshots_of(&format!("{m}/@ "));
            }
            let progress = TestProgress::new();
            archive_boot_with(&config, None, &progress, &env(&runner)).unwrap();
            assert!(
                live.join("marker").exists(),
                "{failing}: the live subvolume survives"
            );
            assert!(
                progress
                    .logs
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|(_, msg)| msg.contains(said)),
                "{failing}: {:?}",
                progress.logs.lock().unwrap()
            );
            for call in runner.calls() {
                for pattern in never {
                    assert!(
                        !call.contains(&pattern.replace("{m}", &m)),
                        "{failing}: {call} must not run after the failure"
                    );
                }
            }
            if failing == "delete" {
                assert!(
                    runner
                        .calls()
                        .contains(&format!("btrfs subvolume delete {m}/@.new")),
                    "the replacement is discarded: {:?}",
                    runner.calls()
                );
            }
        }
    }

    #[test]
    fn boot_archive_removes_a_stale_staging_subvolume_first_and_stops_if_it_cannot() {
        let dir = tempfile::tempdir().unwrap();
        let m = dir.path().display().to_string();
        std::fs::create_dir(dir.path().join("@")).unwrap();
        std::fs::create_dir(dir.path().join("@.new")).unwrap();
        let (config, _conf) = archive_fixture(dir.path());
        let list = (
            format!("btrfs subvolume list {m}"),
            0,
            "ID 257 gen 9 top level 5 path nvme/root-.20261005T0100\n".to_string(),
        );
        // It cannot be removed: the live subvolume is left as it is.
        let runner = Scripted::from_owned(vec![
            list.clone(),
            (
                format!("btrfs subvolume delete {m}/@.new"),
                1,
                String::new(),
            ),
        ])
        .snapshotting();
        let progress = TestProgress::new();
        archive_boot_with(&config, None, &progress, &env(&runner)).unwrap();
        assert!(
            runner
                .calls()
                .iter()
                .any(|c| c.as_str() == format!("btrfs subvolume delete {m}/@.new"))
        );
        assert!(
            !runner
                .calls()
                .iter()
                .any(|c| c.contains("nvme/root-") && c.starts_with("btrfs subvolume snapshot ")),
            "no replacement is built over a stale one: {:?}",
            runner.calls()
        );
        assert!(dir.path().join("@").is_dir());
        assert!(
            progress
                .logs
                .lock()
                .unwrap()
                .iter()
                .any(|(_, msg)| msg.contains("Stale") && msg.contains("could not be removed"))
        );
    }

    #[test]
    fn boot_archive_reports_a_replacement_that_cannot_be_moved_into_place() {
        // The scripted delete does nothing, so `@` is still there and the
        // rename of `@.new` onto it fails: said, and the archive holds the old.
        let dir = tempfile::tempdir().unwrap();
        let m = dir.path().display().to_string();
        let live = dir.path().join("@");
        std::fs::create_dir(&live).unwrap();
        std::fs::write(live.join("old"), b"x").unwrap();
        let (config, _conf) = archive_fixture(dir.path());
        let runner = Scripted::from_owned(vec![
            (
                format!("btrfs subvolume list {m}"),
                0,
                "ID 257 gen 9 top level 5 path nvme/root-.20261005T0100\n".into(),
            ),
            (format!("btrfs subvolume delete {m}/@"), 0, String::new()),
        ])
        .snapshotting();
        let progress = TestProgress::new();
        archive_boot_with(&config, None, &progress, &env(&runner)).unwrap();
        assert!(
            progress
                .logs
                .lock()
                .unwrap()
                .iter()
                .any(|(l, msg)| *l == LogLevel::Error && msg.starts_with("Renamed nothing:"))
        );
        assert!(live.join("old").exists());
    }

    // -- a run mounts nothing itself (bd DAS-Backup-Manager-7tx 3, 8cf) ----------
    //
    // `run_backup` used to mount any source volume it did not find mounted, with
    // a raw `mount` after the guard that owns the job's mounts had finished,
    // and nothing ever unmounted it. The caller mounts (and gives back); the
    // run only checks.

    /// The run's config: `/proc` stands in for the one target that is
    /// mounted, a second one is absent, and one of the two sources is
    /// manual-only.
    fn live_config() -> Config {
        let mut config = make_test_config();
        config.targets.push(Target {
            label: "recovery".into(),
            mount: "/nonexistent/das/recovery".into(),
            ..config.targets[0].clone()
        });
        config
    }

    fn live_options() -> BackupOptions {
        BackupOptions {
            mode: Some(BackupMode::Incremental),
            targets: labels(&["primary-22tb"]),
            ..Default::default()
        }
    }

    #[test]
    fn a_live_run_goes_to_the_selected_target_only_and_mounts_nothing_itself() {
        // `manual-src` is left out (manual-only), the absent `recovery` target
        // is not ticked: the run names `/proc/self/root` and `home` to btrbk.
        let f = "/proc/self/root /proc/self/home";
        let runner = Scripted::from_owned(vec![
            (
                btrbk_at("/nonexistent/btrbk.conf", &format!("snapshot {f}")),
                0,
                String::new(),
            ),
            (
                btrbk_at("/nonexistent/btrbk.conf", &format!("resume {f}")),
                0,
                ">>> /t/a.1\n".into(),
            ),
            (
                btrbk_at(
                    "/nonexistent/btrbk.conf",
                    &format!("--format=raw list latest {f}"),
                ),
                0,
                raw_row("/s/a.1", "/t/a.1") + &raw_row("/s/b.1", "/t/b.1"),
            ),
        ]);
        let progress = TestProgress::new();
        let nvme_mounted = |p: &Path| p == Path::new("/.btrfs-nvme");
        let host = StepEnv {
            is_mountpoint: &nvme_mounted,
            ..env(&runner)
        };
        let result = run_backup_with(&live_config(), &live_options(), &progress, &host).unwrap();
        assert!(result.success, "{:?}", result.errors);
        assert_eq!((result.snapshots_created, result.snapshots_sent), (2, 2));
        let calls = runner.calls();
        assert_eq!(calls.len(), 4, "{calls:?}");
        assert!(
            !calls
                .iter()
                .any(|c| c.starts_with("mount ") || c.starts_with("mountpoint ")),
            "the run mounts nothing: {calls:?}"
        );
        assert!(
            !calls.iter().any(|c| c.contains("nonexistent/das/recovery")),
            "the unticked, absent target is not named to btrbk: {calls:?}"
        );
    }

    #[test]
    fn a_live_run_refuses_a_source_volume_that_is_not_mounted_and_runs_nothing() {
        let runner = Scripted::from_owned(vec![]);
        let nothing_mounted = |_: &Path| false;
        let host = StepEnv {
            is_mountpoint: &nothing_mounted,
            ..env(&runner)
        };
        let err = run_backup_with(&live_config(), &live_options(), &TestProgress::new(), &host)
            .unwrap_err()
            .to_string();
        assert_eq!(
            err,
            "Source volume not mounted: /.btrfs-nvme (source 'nvme-root') — refusing to run \
             btrbk on a bare directory"
        );
        assert!(
            runner.calls().is_empty(),
            "neither btrbk nor a mount: {:?}",
            runner.calls()
        );
    }

    #[test]
    fn only_the_volumes_of_the_sources_selected_have_to_be_mounted() {
        let runner = Scripted::from_owned(vec![]);
        // `manual-src` is selected, its volume is not mounted.
        let options = BackupOptions {
            sources: labels(&["manual-src"]),
            ..live_options()
        };
        let nvme_mounted = |p: &Path| p == Path::new("/.btrfs-nvme");
        let host = StepEnv {
            is_mountpoint: &nvme_mounted,
            ..env(&runner)
        };
        let err = run_backup_with(&live_config(), &options, &TestProgress::new(), &host)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("/.btrfs-manual (source 'manual-src')"),
            "{err}"
        );
        assert!(!err.contains("/.btrfs-nvme"), "{err}");
    }

    #[test]
    fn unmounted_volumes_are_listed_once_each_for_the_selected_sources_only() {
        let mut config = make_test_config();
        let mut twin = config.sources[0].clone();
        twin.label = "nvme-twin".into();
        config.sources.push(twin);
        let mounted = |_: &Path| false;
        // Two sources share /.btrfs-nvme: one entry, the first source's name.
        assert_eq!(
            unmounted_source_volumes(&config, &labels(&["nvme-root", "nvme-twin"]), &mounted),
            ["/.btrfs-nvme (source 'nvme-root')"]
        );
        // Only the sources named are looked at.
        assert_eq!(
            unmounted_source_volumes(&config, &labels(&["manual-src"]), &mounted),
            ["/.btrfs-manual (source 'manual-src')"]
        );
        assert!(unmounted_source_volumes(&config, &[], &mounted).is_empty());
        // One that is mounted is not listed.
        assert_eq!(
            unmounted_source_volumes(&config, &labels(&["nvme-root", "manual-src"]), &|p| {
                p == Path::new("/.btrfs-nvme")
            }),
            ["/.btrfs-manual (source 'manual-src')"]
        );
    }

    #[test]
    fn the_host_environment_asks_the_real_mount_table() {
        // `/proc` is a mount point wherever the tests run; a fresh directory is not.
        assert!((StepEnv::HOST.is_mountpoint)(Path::new("/proc")));
        let dir = tempfile::tempdir().unwrap();
        assert!(!(StepEnv::HOST.is_mountpoint)(dir.path()));
    }

    // -- the selection a run starts from ----------------------------------------

    #[test]
    fn a_run_with_no_sources_named_backs_up_those_with_a_subvolume_that_is_not_manual_only() {
        let config = make_test_config();
        let options = BackupOptions::default();
        // `manual-src` has only a manual-only subvolume.
        assert_eq!(effective_sources(&config, &options), ["nvme-root"]);
    }

    #[test]
    fn a_source_named_explicitly_is_backed_up_even_if_manual_only() {
        let config = make_test_config();
        let options = BackupOptions {
            sources: labels(&["manual-src", "nvme-root"]),
            ..Default::default()
        };
        assert_eq!(
            effective_sources(&config, &options),
            ["manual-src", "nvme-root"]
        );
    }

    #[test]
    fn a_run_with_no_targets_named_writes_to_those_mounted() {
        let mut config = make_test_config();
        let mut absent = config.targets[0].clone();
        absent.label = "absent".into();
        absent.mount = "/nonexistent/das/absent".into();
        config.targets.push(absent);
        let progress = TestProgress::new();
        assert_eq!(
            effective_targets(&config, &BackupOptions::default(), &progress),
            ["primary-22tb"],
            "/proc is mounted, the other mount point does not exist"
        );
    }

    #[test]
    fn targets_named_are_trusted_without_a_mount_check_and_unknown_labels_are_dropped() {
        let mut config = make_test_config();
        let mut absent = config.targets[0].clone();
        absent.label = "absent".into();
        absent.mount = "/nonexistent/das/absent".into();
        config.targets.push(absent);
        let options = BackupOptions {
            targets: labels(&["absent", "no-such-target"]),
            ..Default::default()
        };
        // Named, so a ticked target is passed on whether or not it is
        // mounted: the verification before btrbk is what refuses it.
        assert_eq!(
            effective_targets(&config, &options, &TestProgress::new()),
            ["absent"]
        );
    }

    #[test]
    fn targets_named_that_match_nothing_fall_back_to_those_mounted_with_a_warning() {
        let config = make_test_config();
        let options = BackupOptions {
            targets: labels(&["stale-label"]),
            ..Default::default()
        };
        let progress = TestProgress::new();
        assert_eq!(
            effective_targets(&config, &options, &progress),
            ["primary-22tb"]
        );
        assert!(
            progress
                .logs
                .lock()
                .unwrap()
                .iter()
                .any(|(l, m)| *l == LogLevel::Warning
                    && m.contains("did not match config")
                    && m.contains("stale-label"))
        );
    }

    /// A minimal btrbk.conf declaring one subvolume and its snapshot_name.
    fn write_btrbk_conf(subvol: &str, snapshot_name: &str) -> tempfile::NamedTempFile {
        use std::io::Write;
        let mut f = tempfile::NamedTempFile::new().unwrap();
        writeln!(
            f,
            "volume /.btrfs-nvme\n  subvolume  {subvol}\n    snapshot_name  {snapshot_name}\n"
        )
        .unwrap();
        f
    }

    // --- bd DAS-Backup-Manager-5ig ---------------------------------------
    // The finder must resolve the name btrbk ACTUALLY writes. Production
    // btrbk.conf carries `snapshot_name root-` for a bare `@` (disambiguated
    // against `root-root`), so on-disk paths are `nvme/root-.<TS>`. The old
    // hardcoded prefix was `nvme/root.`, which never matched, and the caller
    // deleted the live subvolume before discovering that.
    #[test]
    fn finder_matches_the_real_disambiguated_snapshot_name() {
        let listing = "ID 256 gen 1 top level 5 path nvme/root-.20260827T2015\n\
                       ID 257 gen 1 top level 5 path nvme/home.20260827T2015\n";
        let subdirs = vec!["nvme".to_string()];
        assert_eq!(
            latest_matching_snapshot(listing, &subdirs, "root-"),
            Some("nvme/root-.20260827T2015"),
            "must find the hyphenated series btrbk really writes"
        );
        // Counter-test: the name the old code derived algorithmically finds
        // nothing at all against the same listing.
        assert_eq!(
            latest_matching_snapshot(listing, &subdirs, "root"),
            None,
            "the pre-5ig hardcoded name must be shown NOT to match"
        );
    }

    #[test]
    fn finder_picks_the_newest_of_several() {
        let listing = "path nvme/root-.20260101T0100\npath nvme/root-.20260827T2015\n\
                       path nvme/root-.20260501T0300\n";
        assert_eq!(
            latest_matching_snapshot(listing, &["nvme".to_string()], "root-"),
            Some("nvme/root-.20260827T2015")
        );
    }

    #[test]
    fn finder_is_scoped_to_the_declared_subdir() {
        let listing = "path ssd/root-.20260827T2015\n";
        assert_eq!(
            latest_matching_snapshot(listing, &["nvme".to_string()], "root-"),
            None,
            "a snapshot under another source's subdir must not be adopted"
        );
    }

    #[test]
    fn archive_boot_refuses_to_act_without_a_readable_btrbk_conf() {
        let target_dir = tempfile::tempdir().unwrap();
        let mut config = make_test_config();
        config.general.btrbk_conf = "/nonexistent/btrbk.conf".into();
        config.targets[0].mount = target_dir.path().to_string_lossy().to_string();

        let progress = TestProgress::new();
        let result = archive_boot_with(
            &config,
            None,
            &progress,
            &env(&Scripted::from_owned(vec![])),
        )
        .expect("must not error");
        assert!(
            !result,
            "nothing may be archived when names cannot be resolved"
        );
        let logs = progress.logs.lock().unwrap();
        assert!(
            logs.iter()
                .any(|(_, m)| m.contains("skipping boot archive rather than guessing")),
            "must say why it declined, got: {logs:?}"
        );
    }

    #[test]
    fn archive_boot_leaves_the_subvolume_alone_when_no_snapshot_exists() {
        // The regression that motivated 5ig: with no matching snapshot on the
        // target, the OLD code had already deleted the live subvolume by the
        // time it found out. Nothing may be deleted now.
        let target_dir = tempfile::tempdir().unwrap();
        let live = target_dir.path().join("@");
        std::fs::create_dir(&live).unwrap();

        let conf = write_btrbk_conf("@", "root-");
        let mut config = make_test_config();
        config.general.btrbk_conf = conf.path().to_string_lossy().to_string();
        config.targets[0].mount = target_dir.path().to_string_lossy().to_string();

        let progress = TestProgress::new();
        archive_boot_with(
            &config,
            None,
            &progress,
            &env(&Scripted::from_owned(vec![])),
        )
        .expect("must not error");
        assert!(
            live.exists(),
            "live subvolume must survive a failed snapshot lookup"
        );
    }

    #[test]
    fn test_archive_boot_skips_mirror_targets() {
        // Two targets: a primary (fair game for archive/replace) and a mirror
        // (independent OS — must be skipped entirely by archive_boot). Both
        // mount paths are real tempdirs so `Path::exists()` checks behave
        // like a real (if empty) target; neither has an "@"/"@home" subvolume
        // on disk, so no actual btrfs mutation is attempted against either —
        // this test only asserts *target selection*, not btrfs plumbing.
        let primary_dir = tempfile::tempdir().unwrap();
        let mirror_dir = tempfile::tempdir().unwrap();

        let mut config = make_test_config();
        // archive_boot resolves snapshot names from the live btrbk.conf and
        // refuses to act at all if it cannot read it, so the fixture needs one.
        let conf = write_btrbk_conf("@", "root-");
        config.general.btrbk_conf = conf.path().to_string_lossy().to_string();
        // The source must declare @ and a target subdir, or archive_boot
        // correctly declines before it ever reaches the target loop.
        config.boot.subvolumes = vec!["@".to_string()];
        config.sources[0].subvolumes = vec![SubvolConfig {
            name: "@".into(),
            manual_only: false,
            snapshot_name: None,
            ..Default::default()
        }];
        config.sources[0].target_subdirs = vec!["nvme".into()];
        config.targets[0].mount = primary_dir.path().to_string_lossy().to_string();
        config.targets[0].label = "primary-22tb".into();
        config.targets.push(Target {
            label: "system-recovery-A-2tb".into(),
            serial: "MIRRORSERIAL".into(),
            serials: vec!["MIRRORSERIAL".into()],
            mount_uuid: None,
            mount: mirror_dir.path().to_string_lossy().to_string(),
            role: TargetRole::Mirror,
            retention: Retention {
                weekly: 4,
                monthly: 2,
                daily: 7,
                yearly: 0,
            },
            display_name: "Recovery A (independent OS)".into(),
        });

        let progress = TestProgress::new();
        let result = archive_boot_with(
            &config,
            None,
            &progress,
            &env(&Scripted::from_owned(vec![])),
        );
        assert!(result.is_ok(), "archive_boot must not error: {result:?}");

        let logs = progress.logs.lock().unwrap();

        // The mirror target must be explicitly skipped, with wording mirroring
        // scripts/backup-run.sh's update_boot_subvolumes().
        let mirror_mount = mirror_dir.path().to_string_lossy().to_string();
        assert!(
            logs.iter().any(|(_, msg)| msg.contains(&mirror_mount)
                && msg.contains("Skipping mirror target (independent OS)")),
            "expected a 'Skipping mirror target (independent OS)' log mentioning {mirror_mount}, got: {logs:?}"
        );

        // No archive/create/delete/snapshot log line may ever reference the
        // mirror's mount path — the skip must happen before any btrfs
        // subvolume operation is attempted against it.
        assert!(
            !logs.iter().any(|(_, msg)| {
                (msg.contains("Archived") || msg.contains("Created") || msg.contains("delete"))
                    && msg.contains(&mirror_mount)
            }),
            "no archive/create/delete operation may reference the mirror mount, got: {logs:?}"
        );
    }

    // --- the sync that starts every backup, from the CLI and the GUI alike ---

    /// A config on disk: one source on `/ssd` holding `@srv`, one primary
    /// target, btrbk.conf beside it. Returns the config path.
    fn sync_fixture(dir: &Path) -> std::path::PathBuf {
        let mut config = make_test_config();
        config.sources.truncate(1);
        config.sources[0].volume = "/ssd".into();
        config.sources[0].device = "UUID=abc".into();
        config.sources[0].subvolumes.truncate(1);
        config.sources[0].subvolumes[0].name = "@srv".into();
        config.general.btrbk_conf = dir.join("btrbk.conf").to_string_lossy().into_owned();
        let path = dir.join("config.toml");
        config.save(&path).unwrap();
        path
    }

    fn listing(paths: &[&str]) -> crate::fsutil::testing::Scripted {
        let list: String = paths
            .iter()
            .map(|p| format!("ID 1 gen 1 top level 5 path {p}\n"))
            .collect();
        crate::fsutil::testing::Scripted::from_owned(vec![
            (
                "findmnt -n -o UUID,FSROOT --target /ssd".into(),
                0,
                "abc /\n".into(),
            ),
            ("btrfs subvolume list /ssd".into(), 0, list),
        ])
    }

    #[test]
    fn sync_before_backup_adopts_reloads_and_logs_the_report() {
        let dir = tempfile::tempdir().unwrap();
        let path = sync_fixture(dir.path());
        let progress = TestProgress::new();
        let (config, sync) = sync_before_backup(
            &path,
            &make_test_config(),
            false,
            "2026-10-02",
            &listing(&["@srv", "@new"]),
            &|_| true,
            &progress,
        );
        assert!(!sync.failed, "{}", sync.report);
        assert!(
            sync.report.starts_with("SUBVOLUME SYNC\n"),
            "{}",
            sync.report
        );
        assert!(sync.report.contains("@new"), "{}", sync.report);
        // The returned config is the one sync wrote, not the one before it.
        assert!(
            config
                .sources
                .iter()
                .flat_map(|s| &s.subvolumes)
                .any(|e| e.name == "@new"),
            "the run must use the config sync wrote"
        );
        let logs = progress.logs.lock().unwrap();
        for line in sync.report.lines() {
            assert!(
                logs.iter()
                    .any(|(level, msg)| *level == LogLevel::Info && msg == line),
                "report line {line:?} missing from the progress log: {logs:?}"
            );
        }
    }

    #[test]
    fn sync_before_backup_marks_a_failed_sync_and_logs_it_as_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = sync_fixture(dir.path());
        let progress = TestProgress::new();
        // The volume is not mounted: nothing may be read, the run must fail.
        let (_, sync) = sync_before_backup(
            &path,
            &make_test_config(),
            false,
            "2026-10-02",
            &listing(&["@srv"]),
            &|_| false,
            &progress,
        );
        assert!(sync.failed, "{}", sync.report);
        assert!(sync.report.contains("VOLUMES NOT READ"), "{}", sync.report);
        let logs = progress.logs.lock().unwrap();
        assert!(
            logs.iter()
                .any(|(level, msg)| *level == LogLevel::Error && msg.contains("VOLUMES NOT READ")),
            "{logs:?}"
        );
    }

    /// Before: a config that could not be loaded after sync was an `Err`,
    /// and the run stopped before anything was recorded. Now the run goes on
    /// with the config it had, and the sync section is failed and says why.
    #[test]
    fn a_config_that_cannot_be_reloaded_after_sync_fails_the_section_not_the_run() {
        let dir = tempfile::tempdir().unwrap();
        let progress = TestProgress::new();
        let mut before = make_test_config();
        before.general.version = "loaded-before-sync".into();
        let (config, sync) = sync_before_backup(
            &dir.path().join("absent.toml"),
            &before,
            false,
            "2026-10-02",
            &listing(&["@srv"]),
            &|_| true,
            &progress,
        );
        assert_eq!(config.general.version, "loaded-before-sync");
        assert!(sync.failed);
        assert!(
            sync.report.contains("  CONFIG NOT RELOADED after sync ("),
            "{}",
            sync.report
        );
        assert!(
            sync.report.contains("absent.toml")
                && sync
                    .report
                    .contains("— this run uses the config loaded before sync"),
            "{}",
            sync.report
        );
        // The reason reaches the progress log, as an error.
        let logs = progress.logs.lock().unwrap();
        assert!(
            logs.iter().any(
                |(level, msg)| *level == LogLevel::Error && msg.contains("CONFIG NOT RELOADED")
            ),
            "{logs:?}"
        );
    }

    #[test]
    fn a_reloaded_config_is_the_one_on_disk_not_the_one_from_before() {
        let dir = tempfile::tempdir().unwrap();
        let path = sync_fixture(dir.path());
        let mut before = make_test_config();
        before.general.version = "loaded-before-sync".into();
        let (config, sync) = sync_before_backup(
            &path,
            &before,
            true,
            "2026-10-02",
            &listing(&["@srv"]),
            &|_| true,
            &TestProgress::new(),
        );
        assert!(!sync.failed, "{}", sync.report);
        assert!(
            !sync.report.contains("CONFIG NOT RELOADED"),
            "{}",
            sync.report
        );
        assert_ne!(config.general.version, "loaded-before-sync");
    }

    // --- a failed sync is a failed run, in the record and in the report ---

    fn failed_sync() -> SyncSection {
        SyncSection {
            report: "SUBVOLUME SYNC\n  VOLUMES NOT READ (nothing adopted or retired there):\n    /ssd: not mounted\n".into(),
            failed: true,
        }
    }

    #[test]
    fn a_failed_sync_makes_the_run_fail_and_is_recorded_as_a_failure() {
        let config = make_test_config();
        let options = BackupOptions {
            dry_run: true,
            subvolume_sync: Some(failed_sync()),
            ..Default::default()
        };
        let progress = TestProgress::new();
        let result = run_backup_with(
            &config,
            &options,
            &progress,
            &env(&Scripted::from_owned(vec![])),
        )
        .unwrap();
        assert!(!result.success, "a failed sync must fail the run");
        assert!(
            result
                .errors
                .iter()
                .any(|e| e.contains("Subvolume sync failed")),
            "{:?}",
            result.errors
        );
        assert!(progress.completed.lock().unwrap().is_none());

        let db = Database::open(":memory:").unwrap();
        crate::report::record_backup_run(&db, &result).unwrap();
        let history = crate::report::get_backup_history(&db, 1).unwrap();
        assert!(!history[0].success, "backup_runs.success must be 0");
        assert!(
            history[0]
                .errors
                .iter()
                .any(|e| e.contains("SUBVOLUME SYNC")),
            "{:?}",
            history[0].errors
        );
    }

    /// The run makes the directories btrbk receives into, after verifying the
    /// targets, and a directory it cannot make fails the run (procfs refuses
    /// every mkdir, so nothing is ever written here).
    #[test]
    fn a_target_directory_that_cannot_be_made_fails_the_run() {
        let mut config = make_test_config();
        config.sources[0].target_subdirs = vec!["das-test-cannot-exist".into()];
        let options = BackupOptions {
            dry_run: true,
            ..Default::default()
        };
        let progress = TestProgress::new();
        let result = run_backup_with(
            &config,
            &options,
            &progress,
            &env(&Scripted::from_owned(vec![])),
        )
        .unwrap();
        assert!(!result.success);
        assert!(
            result.errors[0].starts_with("Target directories missing"),
            "{:?}",
            result.errors
        );
        assert!(result.errors[0].contains("das-test-cannot-exist"));
    }

    #[test]
    fn a_clean_sync_leaves_the_run_successful() {
        let config = make_test_config();
        let options = BackupOptions {
            dry_run: true,
            subvolume_sync: Some(SyncSection {
                report: "SUBVOLUME SYNC\n  No new, vanished or returning subvolumes.\n".into(),
                failed: false,
            }),
            ..Default::default()
        };
        let result = run_backup_with(
            &config,
            &options,
            &TestProgress::new(),
            &env(&Scripted::from_owned(vec![])),
        )
        .unwrap();
        assert!(result.success, "{:?}", result.errors);
        assert!(result.errors.is_empty());
    }

    #[test]
    fn the_report_is_emailed_only_when_asked_for_and_email_is_enabled() {
        let mut config = make_test_config();
        for (send, enabled, want) in [
            (true, true, true),
            (true, false, false),
            (false, true, false),
            (false, false, false),
        ] {
            config.email.enabled = enabled;
            let options = BackupOptions {
                send_report: send,
                ..Default::default()
            };
            assert_eq!(emails_report(&options, &config), want, "{send} {enabled}");
        }
    }

    // --- run_backup_job: one job for the CLI and the GUI ------------------

    /// A scripted host. Every step it is asked to do is appended to `steps`.
    struct FakeHost {
        steps: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
        locks: Result<bool, String>,
        sync_failed: bool,
        targets: Result<Vec<String>, String>,
        sources_left: Vec<String>,
        run: Result<(), String>,
        recorded: std::sync::Mutex<Vec<BackupResult>>,
        reported: std::sync::Mutex<Vec<BackupResult>>,
        report_texts: std::sync::Mutex<Vec<String>>,
        record_fails: bool,
    }

    impl Default for FakeHost {
        fn default() -> Self {
            Self {
                steps: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
                locks: Ok(true),
                sync_failed: false,
                targets: Ok(Vec::new()),
                sources_left: Vec::new(),
                run: Ok(()),
                recorded: std::sync::Mutex::new(Vec::new()),
                reported: std::sync::Mutex::new(Vec::new()),
                report_texts: std::sync::Mutex::new(Vec::new()),
                record_fails: false,
            }
        }
    }

    impl FakeHost {
        fn step(&self, s: &str) {
            self.steps.lock().unwrap().push(s.to_string());
        }
        fn steps(&self) -> Vec<String> {
            self.steps.lock().unwrap().clone()
        }
    }

    /// What a fake mount leaves behind when released; logs its release.
    struct FakeMounts {
        name: &'static str,
        left: Vec<String>,
        steps: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    }

    impl Release for FakeMounts {
        fn release(&mut self, _: &dyn ProgressCallback) -> Vec<String> {
            self.steps
                .lock()
                .unwrap()
                .push(format!("release {}", self.name));
            std::mem::take(&mut self.left)
        }
    }

    fn copy_result(r: &BackupResult) -> BackupResult {
        BackupResult {
            success: r.success,
            mode: r.mode,
            snapshots_created: r.snapshots_created,
            snapshots_sent: r.snapshots_sent,
            snapshots_cleaned: r.snapshots_cleaned,
            bytes_sent: r.bytes_sent,
            boot_archived: r.boot_archived,
            indexed: r.indexed,
            report_sent: r.report_sent,
            errors: r.errors.clone(),
            duration_secs: r.duration_secs,
        }
    }

    impl BackupJobHost for FakeHost {
        fn acquire_locks(
            &self,
            _: &dyn ProgressCallback,
        ) -> Result<Option<Box<dyn HoldsMaintenance>>, String> {
            self.step("locks");
            self.locks.clone().map(|got| {
                got.then(|| Box::new(MaintenanceHeld::assumed()) as Box<dyn HoldsMaintenance>)
            })
        }
        fn mount_sources(&self, _: &Config, _: &dyn ProgressCallback) -> Box<dyn Release> {
            self.step("mount sources");
            Box::new(FakeMounts {
                name: "sources",
                left: self.sources_left.clone(),
                steps: self.steps.clone(),
            })
        }
        fn sync(
            &self,
            before: &Config,
            dry_run: bool,
            _: &dyn ProgressCallback,
        ) -> (Config, SyncSection) {
            self.step(&format!(
                "sync dry_run={dry_run} before={}",
                before.general.version
            ));
            let mut config = make_test_config();
            config.general.version = "after-sync".into();
            (
                config,
                SyncSection {
                    report: "SUBVOLUME SYNC\n".into(),
                    failed: self.sync_failed,
                },
            )
        }
        fn mount_targets(
            &self,
            config: &Config,
            _: &dyn ProgressCallback,
            _: &MaintenanceHeld,
        ) -> Result<Box<dyn Release>, String> {
            self.step(&format!("mount targets ({})", config.general.version));
            self.targets.clone().map(|left| {
                Box::new(FakeMounts {
                    name: "targets",
                    left,
                    steps: self.steps.clone(),
                }) as Box<dyn Release>
            })
        }
        fn run(
            &self,
            config: &Config,
            options: &BackupOptions,
            _: &dyn ProgressCallback,
        ) -> Result<BackupResult, String> {
            self.step(&format!(
                "run ({}, sync failed={:?})",
                config.general.version,
                options.subvolume_sync.as_ref().map(|s| s.failed)
            ));
            self.run.clone()?;
            let mut errors = Vec::new();
            if options.subvolume_sync.as_ref().is_some_and(|s| s.failed) {
                errors.push("Subvolume sync failed".to_string());
            }
            Ok(BackupResult {
                success: errors.is_empty(),
                mode: BackupMode::Incremental,
                snapshots_created: 2,
                snapshots_sent: 2,
                snapshots_cleaned: 0,
                bytes_sent: 10,
                boot_archived: false,
                indexed: true,
                report_sent: false,
                errors,
                duration_secs: 1,
            })
        }
        fn capture_report(&self, _: &Config) -> crate::report::ReportData {
            self.step("capture report");
            crate::report::ReportData {
                capacity_and_smart: "\nDISK CAPACITY\n  primary-loop  1 GiB\n".into(),
                latest_snapshots: "\nLATEST SNAPSHOTS\n  root-.20261002\n".into(),
            }
        }
        fn report(
            &self,
            _: &Config,
            options: &BackupOptions,
            result: &BackupResult,
            data: &crate::report::ReportData,
            _: &dyn ProgressCallback,
        ) -> bool {
            self.step("report");
            self.reported.lock().unwrap().push(copy_result(result));
            self.report_texts
                .lock()
                .unwrap()
                .push(crate::report::format_report_from(
                    result,
                    options.subvolume_sync.as_ref(),
                    data,
                ));
            true
        }
        fn record(&self, _: &Config, result: &BackupResult) -> Result<(), String> {
            self.step("record");
            self.recorded.lock().unwrap().push(copy_result(result));
            if self.record_fails {
                Err("disk full".into())
            } else {
                Ok(())
            }
        }
    }

    fn job(host: &FakeHost, dry_run: bool) -> (BackupJobOutcome, TestProgress) {
        let progress = TestProgress::new();
        let options = BackupOptions {
            dry_run,
            ..Default::default()
        };
        let outcome = run_backup_job(host, make_test_config(), options, &progress);
        (outcome, progress)
    }

    #[test]
    fn a_clean_job_runs_every_step_in_order_and_records_success() {
        let host = FakeHost::default();
        let (outcome, progress) = job(&host, false);
        assert!(outcome.success(), "{outcome:?}");
        assert_eq!(
            host.steps(),
            vec![
                "locks",
                "mount sources",
                "sync dry_run=false before=0.6.0",
                "mount targets (after-sync)",
                "run (after-sync, sync failed=Some(false))",
                "capture report",
                "release targets",
                "release sources",
                "report",
                "record",
            ]
        );
        let BackupJobOutcome::Ran(result) = outcome else {
            panic!()
        };
        assert!(result.report_sent, "what report() returned");
        assert!(host.recorded.lock().unwrap()[0].success);
        assert!(
            progress.completed.lock().unwrap().is_none(),
            "the caller ends the job"
        );
    }

    #[test]
    fn a_target_left_mounted_fails_the_job_its_record_and_its_report() {
        let host = FakeHost {
            targets: Ok(vec!["/mnt/backup-22tb".into()]),
            ..Default::default()
        };
        let (outcome, progress) = job(&host, false);
        assert!(!outcome.success());
        let (ok, line) = outcome.finish_line(false);
        assert!(!ok);
        assert!(line.contains("still mounted: /mnt/backup-22tb"), "{line}");
        for (what, results) in [("record", &host.recorded), ("report", &host.reported)] {
            let results = results.lock().unwrap();
            assert!(!results[0].success, "{what}");
            assert!(
                results[0]
                    .errors
                    .contains(&"still mounted: /mnt/backup-22tb".to_string()),
                "{what}: {:?}",
                results[0].errors
            );
        }
        // The report comes after the unmount, so it can say so.
        let steps = host.steps();
        let pos = |s: &str| steps.iter().position(|x| x == s).unwrap();
        assert!(pos("release targets") < pos("report"), "{steps:?}");
        assert!(
            progress
                .logs
                .lock()
                .unwrap()
                .iter()
                .any(|(l, m)| *l == LogLevel::Error && m == "still mounted: /mnt/backup-22tb")
        );
    }

    /// The report is built after the unmount from what was captured BEFORE
    /// it: a cleanly unmounted run's report carries the capacity rows and
    /// the latest snapshots, and a still-mounted failure reaches the same
    /// report (bd DAS-Backup-Manager-h4t review).
    #[test]
    fn the_report_carries_what_was_captured_while_mounted_and_the_unmount_result() {
        let host = FakeHost::default();
        let _ = job(&host, false);
        let steps = host.steps();
        let pos = |s: &str| steps.iter().position(|x| x == s).unwrap();
        assert!(pos("capture report") < pos("release targets"), "{steps:?}");
        assert!(pos("release sources") < pos("report"), "{steps:?}");
        let text = host.report_texts.lock().unwrap()[0].clone();
        assert!(
            text.contains("DISK CAPACITY\n  primary-loop  1 GiB"),
            "{text}"
        );
        assert!(
            text.contains("LATEST SNAPSHOTS\n  root-.20261002"),
            "{text}"
        );
        assert!(text.contains("ALL OPERATIONS SUCCESSFUL"), "{text}");

        let left = FakeHost {
            targets: Ok(vec!["/mnt/backup-22tb".into()]),
            ..Default::default()
        };
        let _ = job(&left, false);
        let text = left.report_texts.lock().unwrap()[0].clone();
        assert!(text.contains("FAILURES DETECTED"), "{text}");
        assert!(
            text.contains("  - still mounted: /mnt/backup-22tb"),
            "{text}"
        );
        assert!(
            text.contains("DISK CAPACITY\n  primary-loop  1 GiB"),
            "{text}"
        );
    }

    #[test]
    fn nothing_is_captured_for_a_dry_run_or_a_run_that_could_not_start() {
        let host = FakeHost::default();
        let _ = job(&host, true);
        assert!(!host.steps().contains(&"capture report".to_string()));
        let failed = FakeHost {
            run: Err("Refusing to run btrbk".into()),
            ..Default::default()
        };
        let _ = job(&failed, false);
        assert!(!failed.steps().contains(&"capture report".to_string()));
    }

    #[test]
    fn a_source_left_mounted_fails_the_job_too() {
        let host = FakeHost {
            sources_left: vec!["/.btrfs-nvme".into()],
            ..Default::default()
        };
        let (outcome, _) = job(&host, false);
        let BackupJobOutcome::Ran(result) = outcome else {
            panic!("{outcome:?}")
        };
        assert!(!result.success);
        assert_eq!(
            result.errors,
            vec!["still mounted: /.btrfs-nvme".to_string()]
        );
    }

    #[test]
    fn a_dry_run_left_mounted_fails_but_records_and_reports_nothing() {
        // The incident: a dry run from the GUI left the 22 TB target mounted
        // and reported success.
        let host = FakeHost {
            targets: Ok(vec!["/mnt/backup-22tb".into()]),
            ..Default::default()
        };
        let (outcome, _) = job(&host, true);
        let (ok, line) = outcome.finish_line(true);
        assert!(!ok);
        assert!(line.starts_with("DRY RUN (incremental) FAILED"), "{line}");
        assert!(line.contains("still mounted: /mnt/backup-22tb"), "{line}");
        assert!(host.recorded.lock().unwrap().is_empty());
        assert!(host.reported.lock().unwrap().is_empty());
        assert!(
            host.steps()
                .contains(&"sync dry_run=true before=0.6.0".to_string())
        );
    }

    #[test]
    fn a_failed_sync_runs_the_backup_and_fails_it() {
        let host = FakeHost {
            sync_failed: true,
            ..Default::default()
        };
        let (outcome, _) = job(&host, false);
        assert!(!outcome.success());
        assert!(
            host.steps()
                .contains(&"run (after-sync, sync failed=Some(true))".to_string())
        );
        assert!(!host.recorded.lock().unwrap()[0].success);
    }

    #[test]
    fn another_backup_running_declines_before_touching_anything() {
        let host = FakeHost {
            locks: Ok(false),
            ..Default::default()
        };
        let (outcome, _) = job(&host, false);
        assert!(matches!(outcome, BackupJobOutcome::Declined));
        assert_eq!(host.steps(), vec!["locks"]);
        assert_eq!(
            outcome.finish_line(false),
            (false, "A backup is already running — declined".to_string())
        );
    }

    #[test]
    fn a_lock_error_does_not_run() {
        let host = FakeHost {
            locks: Err("permission denied".into()),
            ..Default::default()
        };
        let (outcome, _) = job(&host, false);
        let BackupJobOutcome::NotRun(why) = outcome else {
            panic!()
        };
        assert_eq!(why, "Could not acquire backup locks: permission denied");
        assert_eq!(host.steps(), vec!["locks"]);
    }

    #[test]
    fn no_target_mounted_is_not_run_and_the_sources_are_released() {
        let host = FakeHost {
            targets: Err("No DAS drives found".into()),
            sources_left: vec!["/.btrfs-nvme".into()],
            ..Default::default()
        };
        let (outcome, _) = job(&host, false);
        let BackupJobOutcome::NotRun(why) = &outcome else {
            panic!()
        };
        assert_eq!(
            why,
            "Mount failed: No DAS drives found; still mounted: /.btrfs-nvme"
        );
        assert_eq!(outcome.finish_line(false), (false, why.clone()));
        assert!(host.steps().contains(&"release sources".to_string()));
        assert!(!host.steps().contains(&"record".to_string()));
    }

    #[test]
    fn a_backup_that_cannot_start_is_not_run_and_releases_everything() {
        let host = FakeHost {
            run: Err("Refusing to run btrbk".into()),
            ..Default::default()
        };
        let (outcome, _) = job(&host, false);
        let BackupJobOutcome::NotRun(why) = outcome else {
            panic!()
        };
        assert_eq!(why, "Backup failed: Refusing to run btrbk");
        let steps = host.steps();
        assert!(steps.contains(&"release targets".to_string()), "{steps:?}");
        assert!(steps.contains(&"release sources".to_string()), "{steps:?}");
        assert!(!steps.contains(&"record".to_string()), "{steps:?}");
    }

    #[test]
    fn a_record_that_fails_is_a_warning_not_a_failed_run() {
        let host = FakeHost {
            record_fails: true,
            ..Default::default()
        };
        let (outcome, progress) = job(&host, false);
        assert!(outcome.success());
        assert!(
            progress
                .logs
                .lock()
                .unwrap()
                .iter()
                .any(|(l, m)| *l == LogLevel::Warning
                    && m == "Failed to record backup history: disk full")
        );
    }

    fn result_with(success: bool, created: usize, sent: usize, cleaned: usize) -> BackupResult {
        BackupResult {
            success,
            mode: BackupMode::Full,
            snapshots_created: created,
            snapshots_sent: sent,
            snapshots_cleaned: cleaned,
            bytes_sent: 0,
            boot_archived: true,
            indexed: false,
            report_sent: false,
            errors: if success {
                Vec::new()
            } else {
                vec!["a".into(), "b".into()]
            },
            duration_secs: 0,
        }
    }

    #[test]
    fn backup_summary_says_what_happened() {
        assert_eq!(
            backup_summary(&result_with(true, 0, 0, 0), true),
            "DRY RUN (full) completed — no changes made"
        );
        assert_eq!(
            backup_summary(&result_with(false, 0, 0, 0), true),
            "DRY RUN (full) FAILED — no changes made: a; b"
        );
        assert_eq!(
            backup_summary(&result_with(true, 0, 0, 0), false),
            "Backup (full): nothing to do — all snapshots up to date"
        );
        assert_eq!(
            backup_summary(&result_with(true, 2, 0, 0), false),
            "Backup succeeded (full): 2 snapshots created, 0 sent, boot archived: true"
        );
        assert_eq!(
            backup_summary(&result_with(true, 0, 3, 0), false),
            "Backup succeeded (full): 0 snapshots created, 3 sent, boot archived: true"
        );
        assert_eq!(
            backup_summary(&result_with(true, 0, 0, 4), false),
            "Backup succeeded (full): 0 snapshots created, 0 sent, 4 cleaned up, boot archived: true"
        );
        assert_eq!(
            backup_summary(&result_with(false, 0, 0, 0), false),
            "Backup completed with errors (full): 0 snapshots created, 0 sent, boot archived: true — a; b"
        );
    }

    // --- SystemBackupHost: the steps that need no root ----------------------

    fn system_host(dir: &Path) -> SystemBackupHost {
        SystemBackupHost {
            config_path: dir.join("config.toml"),
            singleton_lock: dir.join("backup.lock"),
            maintenance_lock: dir.join("maintenance.lock"),
            job: "btrdasd backup run".into(),
        }
    }

    #[test]
    fn system_host_uses_the_production_locks_by_default() {
        let host = SystemBackupHost::new(
            Path::new("/etc/das-backup/config.toml"),
            "btrdasd-helper BackupRun job",
        );
        assert_eq!(host.config_path, Path::new("/etc/das-backup/config.toml"));
        assert_eq!(host.singleton_lock, Path::new(BACKUP_LOCK_PATH));
        assert_eq!(
            host.maintenance_lock,
            Path::new(scrub::MAINTENANCE_LOCK_PATH)
        );
        assert_eq!(host.job, "btrdasd-helper BackupRun job");
    }

    #[test]
    fn system_host_takes_the_locks_and_declines_while_they_are_held() {
        let dir = tempfile::tempdir().unwrap();
        let host = system_host(dir.path());
        let progress = TestProgress::new();
        let held = host.acquire_locks(&progress).unwrap();
        assert!(held.is_some(), "free locks are taken");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("maintenance.lock")).unwrap(),
            format!("btrdasd backup run pid {}\n", std::process::id()),
            "the job is recorded as the maintenance lock's holder"
        );
        assert_eq!(
            held.as_ref().unwrap().maintenance().path(),
            dir.path().join("maintenance.lock"),
            "the proof mount_targets gets is the maintenance lock this host took"
        );
        assert!(
            host.acquire_locks(&progress).unwrap().is_none(),
            "a second backup declines"
        );
        drop(held);
        assert!(host.acquire_locks(&progress).unwrap().is_some());
        let bad = SystemBackupHost {
            // A directory cannot be opened as a lock file.
            singleton_lock: dir.path().to_path_buf(),
            ..system_host(dir.path())
        };
        assert!(bad.acquire_locks(&progress).is_err());
    }

    #[test]
    fn system_host_records_the_run_in_backup_runs() {
        let dir = tempfile::tempdir().unwrap();
        let host = system_host(dir.path());
        let mut config = make_test_config();
        config.general.db_path = dir.path().join("index.db").to_string_lossy().into_owned();
        host.record(&config, &result_with(false, 1, 1, 0)).unwrap();
        let db = Database::open(&config.general.db_path).unwrap();
        let history = crate::report::get_backup_history(&db, 5).unwrap();
        assert_eq!(history.len(), 1);
        assert!(!history[0].success);

        config.general.db_path = dir
            .path()
            .join("no/such/dir/index.db")
            .to_string_lossy()
            .into_owned();
        assert!(host.record(&config, &result_with(true, 0, 0, 0)).is_err());
    }

    #[test]
    fn system_host_writes_the_report_when_asked_and_mails_it_only_when_enabled() {
        let dir = tempfile::tempdir().unwrap();
        let host = system_host(dir.path());
        let mut config = make_test_config();
        // In a directory that does not exist yet: it is created.
        let report = dir.path().join("reports/last-report.txt");
        config.general.last_report = report.to_string_lossy().into_owned();
        let progress = TestProgress::new();
        let result = result_with(false, 1, 1, 0);

        let data = crate::report::ReportData {
            capacity_and_smart: "\nDISK CAPACITY\n  t  CAPTURED\n".into(),
            latest_snapshots: "\nLATEST SNAPSHOTS\n  CAPTURED-SNAP\n".into(),
        };
        // Not asked for: nothing written.
        config.email.enabled = true;
        let not_asked = BackupOptions::default();
        assert!(!host.report(&config, &not_asked, &result, &data, &progress));
        assert!(!report.exists(), "no report asked for: none written");

        // Asked for, email disabled: written (as backup-run.sh does), not mailed.
        config.email.enabled = false;
        let ask = BackupOptions {
            send_report: true,
            ..Default::default()
        };
        assert!(!host.report(&config, &ask, &result, &data, &progress));
        let text = std::fs::read_to_string(&report).unwrap();
        assert!(
            text.contains("CAPTURED-SNAP") && text.contains("t  CAPTURED"),
            "{text}"
        );
        std::fs::remove_file(&report).unwrap();
        let tried_to_mail = |p: &TestProgress| {
            p.logs
                .lock()
                .unwrap()
                .iter()
                .any(|(_, m)| m.starts_with("Failed to send email report"))
        };
        assert!(
            !tried_to_mail(&progress),
            "email disabled: no send is tried"
        );

        config.email.enabled = true;
        // No recipient, and nothing listens on that port: the email cannot be
        // sent (and nothing ever leaves this machine) — but the report is
        // written first, and the failure is a warning.
        config.email.to = String::new();
        config.email.smtp_port = 1;
        config.email.smtp_host = "127.0.0.1".into();
        assert!(!host.report(&config, &ask, &result, &data, &progress));
        assert!(tried_to_mail(&progress), "email enabled: the send is tried");
        let text = std::fs::read_to_string(&report).unwrap();
        assert!(text.contains("a"), "{text}");
    }

    #[test]
    fn a_report_that_cannot_be_written_is_a_warning_naming_the_path() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = make_test_config();
        // A file where the report's directory should be.
        std::fs::write(dir.path().join("blocker"), b"x").unwrap();
        let path = dir.path().join("blocker/last-report.txt");
        config.general.last_report = path.to_string_lossy().into_owned();
        config.email.enabled = false;
        let options = BackupOptions {
            send_report: true,
            ..Default::default()
        };
        let data = crate::report::ReportData {
            capacity_and_smart: String::new(),
            latest_snapshots: String::new(),
        };
        let progress = TestProgress::new();
        assert!(!deliver_report(
            &config,
            &options,
            &result_with(true, 1, 1, 0),
            &data,
            &progress
        ));
        let logs = progress.logs.lock().unwrap();
        assert!(
            logs.iter().any(|(l, m)| *l == LogLevel::Warning
                && m.starts_with(&format!("Failed to save report to {}", path.display()))),
            "{logs:?}"
        );
    }

    #[test]
    fn an_existing_report_keeps_its_mode() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("last-report.txt");
        std::fs::write(&path, "old").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        let mut config = make_test_config();
        config.general.last_report = path.to_string_lossy().into_owned();
        config.email.enabled = false;
        let options = BackupOptions {
            send_report: true,
            ..Default::default()
        };
        let data = crate::report::ReportData {
            capacity_and_smart: String::new(),
            latest_snapshots: String::new(),
        };
        deliver_report(
            &config,
            &options,
            &result_with(true, 1, 1, 0),
            &data,
            &TestProgress::new(),
        );
        assert_ne!(std::fs::read_to_string(&path).unwrap(), "old");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o640);
    }

    #[test]
    fn system_host_mounts_nothing_for_a_config_without_targets_and_runs_a_dry_run() {
        let dir = tempfile::tempdir().unwrap();
        let host = system_host(dir.path());
        let mut config = make_test_config();
        config.targets.clear();
        let progress = TestProgress::new();
        let mut targets = host
            .mount_targets(&config, &progress, &MaintenanceHeld::assumed())
            .unwrap();
        assert!(targets.release(&progress).is_empty());

        let options = BackupOptions {
            dry_run: true,
            ..Default::default()
        };
        // With no target there is nothing to verify (a legacy target would be
        // checked by its drive's serial, which no test host can supply).
        let result = host.run(&config, &options, &progress).unwrap();
        assert!(result.success);
        let mut nothing = make_test_config();
        nothing.targets[0].mount = "/nonexistent/das/mount".into();
        assert!(
            host.run(&nothing, &BackupOptions::default(), &progress)
                .is_err()
        );
    }

    #[test]
    fn system_host_syncs_the_config_at_its_path() {
        let dir = tempfile::tempdir().unwrap();
        let host = system_host(dir.path());
        let progress = TestProgress::new();
        let mut before = make_test_config();
        before.general.version = "before".into();
        // No config there: the run keeps the config it had, and fails sync.
        let (config, sync) = host.sync(&before, true, &progress);
        assert_eq!(config.general.version, "before");
        assert!(sync.failed);
        assert_eq!(sync_fixture(dir.path()), host.config_path);
        // A dry run of sync against this host's real volumes: whatever it
        // finds, it loads the config back from the file.
        let (config, _) = host.sync(&before, true, &progress);
        assert_ne!(config.general.version, "before");
        assert_eq!(config.sources[0].volume, "/ssd");
    }
}
