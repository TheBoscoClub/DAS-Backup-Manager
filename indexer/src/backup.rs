use crate::btrbk_conf::{self, DeclaredPair};
use crate::config::{Config, Source, Target, TargetRole};
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
/// **Incremental**: `btrbk snapshot` then `btrbk resume` — two steps, so a run
/// can take one of them alone ([`BtrbkSteps`]). Neither is given
/// `--preserve`: btrbk applies the retention policy after the send.
///
/// **Full**: `btrbk run` — snapshot, send and retention cleanup as one btrbk
/// run (deletes old snapshots/backups outside retention windows). The
/// complete backup lifecycle with housekeeping.
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

/// Which btrbk steps a run performs. Snapshot and Send are the GUI's two
/// ticks; neither is not a choice — it is refused (bd c4x, as 7tx refuses
/// an empty selection), so it has no variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BtrbkSteps {
    /// Full: one `btrbk run`. Incremental: `btrbk snapshot`, then `btrbk resume`.
    #[default]
    SnapshotAndSend,
    /// `btrbk snapshot` only, in either mode.
    SnapshotOnly,
    /// `btrbk resume` only, in either mode: send what exists.
    SendOnly,
}

impl BtrbkSteps {
    pub fn from_ticks(snapshot: bool, send: bool) -> Result<Self, String> {
        match (snapshot, send) {
            (true, true) => Ok(Self::SnapshotAndSend),
            (true, false) => Ok(Self::SnapshotOnly),
            (false, true) => Ok(Self::SendOnly),
            (false, false) => Err("Nothing to do: neither Snapshot nor Send is selected — \
                                   refused, never read as both"
                .to_string()),
        }
    }
    pub fn snapshots(self) -> bool {
        !matches!(self, Self::SendOnly)
    }
    pub fn sends(self) -> bool {
        !matches!(self, Self::SnapshotOnly)
    }
}

/// The keys of the steps dictionary the GUI sends with `BackupRun`. Every
/// one must be present: a missing key is refused, never defaulted, so a GUI
/// and a helper from different builds fail loudly.
pub const RUN_STEP_KEYS: [&str; 5] = ["snapshot", "send", "boot_archive", "index", "email"];

/// What a GUI run was asked to do, read from the steps dictionary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunSteps {
    pub btrbk: BtrbkSteps,
    pub boot_archive: bool,
    pub index: bool,
    pub email: bool,
}

impl RunSteps {
    /// `entries` are the dictionary's (key, value) pairs; a value that is not
    /// a boolean arrives as `None`.
    pub fn from_entries<I: IntoIterator<Item = (String, Option<bool>)>>(
        entries: I,
    ) -> Result<Self, String> {
        let mut seen = std::collections::HashMap::new();
        for (key, value) in entries {
            if !RUN_STEP_KEYS.contains(&key.as_str()) {
                return Err(format!("unknown step '{key}' — refused"));
            }
            let Some(value) = value else {
                return Err(format!("step '{key}' is not a boolean — refused"));
            };
            seen.insert(key, value);
        }
        let get = |key: &str| {
            seen.get(key)
                .copied()
                .ok_or_else(|| format!("step '{key}' not given — refused, never defaulted"))
        };
        Ok(Self {
            btrbk: BtrbkSteps::from_ticks(get("snapshot")?, get("send")?)?,
            boot_archive: get("boot_archive")?,
            index: get("index")?,
            email: get("email")?,
        })
    }

    /// Put these steps into `options`.
    pub fn apply(self, options: &mut BackupOptions) {
        options.steps = self.btrbk;
        options.boot_archive = self.boot_archive;
        options.index_after = self.index;
        options.email_report = self.email;
    }
}

/// Options controlling what a backup run does.
#[derive(Debug, Default)]
pub struct BackupOptions {
    /// Incremental or full. None = use schedule default.
    pub mode: Option<BackupMode>,
    /// Source labels to back up. `None` = not specified: every configured
    /// source. `Some(labels)` = exactly these, and btrbk is told to touch
    /// exactly their subvolumes and no others. **`Some` of an empty list is a
    /// selection of nothing, and the run is refused** ([`empty_selection`]) —
    /// never read as "all": a GUI with every box unticked asks for nothing.
    pub sources: Option<Vec<String>>,
    /// Target labels to send to. `None` = not specified: every target that is
    /// mounted. `Some(labels)` = exactly these, and btrbk is told to write to
    /// exactly these and is not told about the rest. `Some` of an empty list
    /// is refused, as for `sources`.
    pub targets: Option<Vec<String>>,
    /// Preview only — don't actually run btrbk.
    pub dry_run: bool,
    /// Which btrbk steps run (Snapshot, Send or both). Default: both.
    pub steps: BtrbkSteps,
    /// Archive boot subvolumes after backup.
    pub boot_archive: bool,
    /// Run the content indexer after backup completes.
    pub index_after: bool,
    /// Mail the report as well, when [email] is enabled. The report itself is
    /// always written to [general].last_report (backup-run.sh writes
    /// $LAST_REPORT before any send).
    pub email_report: bool,
    /// The subvolume sync that started this run (`sync_before_backup`). A
    /// failed one fails the run — in its result, its `backup_runs` row and
    /// its report — and its section is carried into the report.
    pub subvolume_sync: Option<SyncSection>,
}

/// What a boot-step failure is prefixed with in a run's `errors`.
pub const BOOT_ERROR_PREFIX: &str = "Boot subvolumes: ";

/// What the boot step did, counted the way `backup-run.sh` counts it: one
/// `updated` per boot subvolume created or replaced, one `skipped` per
/// subvolume left alone because it exists (incremental run) and per mirror
/// target. A warning is an absence (nothing to act on); a failure is something
/// that was meant to work and did not — it fails the run (bd woq, dtm).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BootOutcome {
    pub updated: usize,
    pub skipped: usize,
    pub warnings: Vec<String>,
    pub failures: Vec<String>,
}

impl BootOutcome {
    /// `FAIL` beats `WARN` beats `OK`.
    pub fn status(&self) -> &'static str {
        if !self.failures.is_empty() {
            "FAIL"
        } else if !self.warnings.is_empty() {
            "WARN"
        } else {
            "OK"
        }
    }

    /// `backup-run.sh` records `boot_subvols` with these words.
    pub fn detail(&self) -> String {
        match self.status() {
            "FAIL" => format!("{} updated, {} failed", self.updated, self.failures.len()),
            "WARN" => format!(
                "{} updated, {} skipped, {} warnings",
                self.updated,
                self.skipped,
                self.warnings.len()
            ),
            _ => format!("{} updated, {} skipped", self.updated, self.skipped),
        }
    }

    fn warn(&mut self, progress: &dyn ProgressCallback, msg: String) {
        progress.on_log(LogLevel::Warning, &msg);
        self.warnings.push(msg);
    }

    fn fail(&mut self, progress: &dyn ProgressCallback, msg: String) {
        progress.on_log(LogLevel::Error, &msg);
        self.failures.push(msg);
    }
}

/// How many boot warning/failure messages the job-end text lists before
/// summarising the rest as `and K more`.
pub const BOOT_MESSAGE_LINES: usize = 5;

/// The boot step of a run: not asked for, switched off in `config.toml`, or
/// run with this outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BootStep {
    NotSelected,
    DisabledInConfig,
    Ran(BootOutcome),
}

impl BootStep {
    /// Whether any boot subvolume was created or replaced.
    pub fn archived(&self) -> bool {
        matches!(self, Self::Ran(o) if o.updated > 0)
    }

    pub fn has_warnings(&self) -> bool {
        matches!(self, Self::Ran(o) if !o.warnings.is_empty())
    }

    /// Whether the step ran and anything that was meant to work did not.
    pub fn failed(&self) -> bool {
        matches!(self, Self::Ran(o) if !o.failures.is_empty())
    }

    /// The exit status of `backup boot-archive`, the doctor's rule: `None` clean, 3
    /// when it began and something failed — the step, or the unmount after it
    /// (`released` false) — and never 1, which is "could not start" and decided
    /// before the step runs.
    pub fn exit_code(&self, released: bool) -> Option<i32> {
        (self.failed() || !released).then_some(3)
    }

    /// Whether the step leaves nothing a summary must mention: it was not
    /// asked for, is off in the config, or ran clean and replaced nothing.
    pub fn has_nothing_to_report(&self) -> bool {
        match self {
            Self::NotSelected | Self::DisabledInConfig => true,
            Self::Ran(o) => o.status() == "OK" && o.updated == 0,
        }
    }

    /// The warning and failure messages of a boot step that ran, as lines for
    /// the GUI's job-end text: failures first, so a cap can never hide one,
    /// at most [`BOOT_MESSAGE_LINES`] of them, then `and K more`. Empty when
    /// there is nothing to say.
    pub fn message_lines(&self) -> String {
        let Self::Ran(o) = self else {
            return String::new();
        };
        let all: Vec<String> = o
            .failures
            .iter()
            .map(|m| format!("boot FAIL: {m}"))
            .chain(o.warnings.iter().map(|m| format!("boot WARN: {m}")))
            .collect();
        let mut lines: Vec<String> = all.iter().take(BOOT_MESSAGE_LINES).cloned().collect();
        if all.len() > BOOT_MESSAGE_LINES {
            lines.push(format!("and {} more", all.len() - BOOT_MESSAGE_LINES));
        }
        lines.join("\n")
    }

    /// The report's `Boot subvolumes` cell.
    pub fn row(&self) -> String {
        match self {
            Self::NotSelected => "N/A  (not selected)".into(),
            Self::DisabledInConfig => "OK  (disabled in config)".into(),
            Self::Ran(o) => format!("{}  ({})", o.status(), o.detail()),
        }
    }
}

/// Result of a completed backup run.
#[derive(Debug)]
pub struct BackupResult {
    pub success: bool,
    pub mode: BackupMode,
    /// Snapshots btrbk created. `None` = unknown: the step that counts them
    /// was asked for and failed, so nothing was measured — never `Some(0)`,
    /// which is a measurement ("there was nothing to do"). Recorded as NULL
    /// in `backup_runs` (schema 4) and shown as "unknown".
    pub snapshots_created: Option<usize>,
    /// Snapshots sent to a target. `None` = unknown, as `snapshots_created`.
    pub snapshots_sent: Option<usize>,
    pub snapshots_cleaned: usize,
    pub bytes_sent: u64,
    pub boot: BootStep,
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
    /// Bytes in use across the mounted targets, read after a sync
    /// ([`synced_target_usage`]): what `bytes_sent` is the difference of.
    usage: &'a UsageOf<'a>,
    /// Index the snapshots now on the mounted targets; whether any target was
    /// indexed ([`index_mounted_targets`]).
    index: &'a IndexTargets<'a>,
}

/// The step behind [`StepEnv::index`].
type IndexTargets<'a> = dyn Fn(&Config, &dyn ProgressCallback) -> bool + 'a;

/// The reading behind [`StepEnv::usage`].
type UsageOf<'a> = dyn Fn(&Config, &dyn ProgressCallback) -> u64 + 'a;

/// The check behind [`StepEnv::verify`]: the targets (all configured ones),
/// the labels about to be written, and where to say what it found.
type VerifyTargets<'a> =
    dyn Fn(&[Target], &[String], &dyn ProgressCallback) -> Result<(), String> + 'a;

impl StepEnv<'static> {
    const HOST: StepEnv<'static> = StepEnv {
        runner: &SystemRunner,
        verify: &mount::verify_write_targets,
        is_mountpoint: &health::is_mountpoint,
        usage: &synced_target_usage,
        index: &index_mounted_targets,
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
    sources: Option<&[String]>,
    targets: Option<&[String]>,
    progress: &dyn ProgressCallback,
) -> Result<Vec<String>, String> {
    // `None` is "every source"; a list that was given and is empty is "no
    // source" — refused here so that no caller can turn an empty list into
    // "everything" (bd DAS-Backup-Manager-7tx).
    if sources.is_some_and(<[String]>::is_empty) {
        return Err(nothing_selected("source", "target"));
    }
    if let Some(label) = sources
        .unwrap_or_default()
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
    let wants_source = |label: &String| sources.is_none_or(|s| s.contains(label));
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

/// The sources a step names: the ones given, or — `None`, "not specified" —
/// every configured source. Never read an empty list as "all": that is what
/// `Some(&[])` is refused for in [`btrbk_filters`].
fn named_sources<'a>(config: &'a Config, sources: Option<&'a [String]>) -> Vec<&'a Source> {
    match sources {
        None => config.sources.iter().collect(),
        Some(labels) => labels
            .iter()
            .filter_map(|label| config.sources.iter().find(|s| &s.label == label))
            .collect(),
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Create btrbk snapshots for specified sources.
///
/// Only the subvolumes of the sources named are snapshotted (`None`: every
/// source; `Some` of an empty list: refused) — by subvolume, so two sources on
/// one volume are told apart. No target is selected here, so btrbk still reads
/// every one the file declares.
pub fn create_snapshots(
    config: &Config,
    sources: Option<&[String]>,
    progress: &dyn ProgressCallback,
) -> Result<usize, Box<dyn std::error::Error>> {
    create_snapshots_with(config, sources, None, progress, &SystemRunner)
}

/// [`create_snapshots`] with every `btrbk` run by `runner`, and — when the run
/// has chosen its targets (`targets`) — confined to the subvolumes that go to
/// them, so an unselected target is not read, let alone written.
fn create_snapshots_with(
    config: &Config,
    sources: Option<&[String]>,
    targets: Option<&[String]>,
    progress: &dyn ProgressCallback,
    runner: &dyn CommandRunner,
) -> Result<usize, Box<dyn std::error::Error>> {
    let named = named_sources(config, sources);
    progress.on_stage("Creating snapshots", named.len() as u64);

    let filters = btrbk_filters(config, sources, targets, progress)?;
    for src in &named {
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

    for (i, src) in named.iter().enumerate() {
        progress.on_progress(i as u64 + 1, named.len() as u64, &src.label);
    }

    progress.on_log(LogLevel::Info, &format!("Snapshots created: {count}"));
    Ok(count)
}

/// Send snapshots to specified targets via btrbk.
///
/// btrbk is told to send exactly the subvolumes of the sources named (`None`:
/// every source) to exactly the targets named (empty: every one that is
/// mounted) — a target that is not named is not read, so one that is absent
/// does not fail the step; one that IS named and cannot be read still does.
///
/// When `preserve` is true, passes `--preserve` to btrbk so retention cleanup
/// is skipped (no run mode asks for it: `run_pipeline` and the CLI/helper pass
/// false).  When false, btrbk enforces retention policy
/// after sending (deletes old snapshots/backups outside the retention window).
///
/// Returns (snapshots_sent, bytes_sent).
pub fn send_snapshots(
    config: &Config,
    sources: Option<&[String]>,
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
    sources: Option<&[String]>,
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

    let mut bytes_sent: u64 = 0;
    let mut stdout_lines = Vec::new();

    let status = stream_command(&mut cmd, env.runner, progress, |line| {
        stdout_lines.push(line.to_string());
        let trimmed = line.trim_start();
        // btrbk marks sends with >>> (incremental) or *** (full). The count
        // comes from the listing below; only a size btrbk prints is read here.
        if trimmed.starts_with(">>>") || trimmed.starts_with("***") {
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
    let snapshots_sent = match btrbk_raw_listing(config, &filters, env.runner) {
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
    sources: Option<&[String]>,
    targets: &[String],
    progress: &dyn ProgressCallback,
) -> Result<(usize, usize, usize, u64), Box<dyn std::error::Error>> {
    run_full_pipeline_with(config, sources, targets, progress, &StepEnv::HOST)
}

/// [`run_full_pipeline`] in `env`: like [`send_snapshots_with`], it verifies
/// the targets before btrbk runs and refuses on one that fails.
fn run_full_pipeline_with(
    config: &Config,
    sources: Option<&[String]>,
    targets: &[String],
    progress: &dyn ProgressCallback,
    env: &StepEnv,
) -> Result<(usize, usize, usize, u64), Box<dyn std::error::Error>> {
    progress.on_stage("Full backup (snapshot + send + cleanup)", 1);

    let targets = step_targets(config, targets)?;
    (env.verify)(&config.targets, &targets, progress)?;
    let filters = btrbk_filters(config, sources, Some(&targets), progress)?;
    for src in named_sources(config, sources) {
        progress.on_log(
            LogLevel::Info,
            &format!("Source '{}' at {}", src.label, src.volume),
        );
    }
    log_target_scope(config, &targets, progress);

    let mut cmd = Command::new("btrbk");
    cmd.arg("-c").arg(&config.general.btrbk_conf);
    cmd.arg("run").args(&filters);

    let mut bytes_sent: u64 = 0;
    let mut stdout_lines = Vec::new();

    let status = stream_command(&mut cmd, env.runner, progress, |line| {
        stdout_lines.push(line.to_string());
        // The counts come from the listing below; only a size btrbk prints on
        // a send line is read here.
        let trimmed = line.trim_start();
        if trimmed.starts_with(">>>") || trimmed.starts_with("***") {
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

    let (snapshots_created, snapshots_sent) = match btrbk_raw_listing(config, &filters, env.runner)
    {
        Some(raw) => (parse_raw_snapshot_count(&raw), parse_raw_send_count(&raw)),
        None => {
            progress.on_log(
                LogLevel::Warning,
                "btrbk --format=raw list latest failed — counts fall back to output markers",
            );
            (
                parse_btrbk_snapshot_count(&full_output),
                parse_btrbk_send_count(&full_output),
            )
        }
    };
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
    // `get` is None for a `)` before the last `(`: no parenthetical, no size.
    let paren_content = match (line.rfind('('), line.rfind(')')) {
        (Some(open), Some(close)) => match line.get(open + 1..close) {
            Some(inside) => inside,
            None => return 0,
        },
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

/// Bytes in use across the mounted targets, after forcing their pending
/// transactions out ([`sync_targets`]) so the reading is not stale.
fn synced_target_usage(config: &Config, progress: &dyn ProgressCallback) -> u64 {
    sync_targets(config);
    measure_target_usage(config, progress)
}

/// Bytes in use on a filesystem from its `statvfs` block counts: the blocks it
/// has less the blocks left for unprivileged users, times the block size.
fn used_bytes(blocks: u64, available_blocks: u64, block_size: u64) -> u64 {
    (blocks * block_size).saturating_sub(available_blocks * block_size)
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
            let used = used_bytes(stat.f_blocks, stat.f_bavail, stat.f_frsize);
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

/// One boot subvolume of the boot step: what `[boot].subvolumes` names, the
/// snapshot name `btrbk.conf` gives it (`None` = absent there, never guessed)
/// and the target subdirectories its snapshots land in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootPlanItem {
    pub subvol: String,
    pub snapshot_name: Option<String>,
    pub subdirs: Vec<String>,
}

/// Whether `ts` is btrbk's `timestamp_format long` suffix: 8 digits, `T`,
/// 4 digits, optionally `_` and a collision number. ASCII only.
fn is_btrbk_timestamp(ts: &str) -> bool {
    let (stamp, collision) = match ts.split_once('_') {
        Some((s, n)) => (s, Some(n)),
        None => (ts, None),
    };
    let b = stamp.as_bytes();
    b.len() == 13
        && b[..8].iter().all(u8::is_ascii_digit)
        && b[8] == b'T'
        && b[9..].iter().all(u8::is_ascii_digit)
        && collision.is_none_or(|n| !n.is_empty() && n.bytes().all(|c| c.is_ascii_digit()))
}

/// `btrfs subvolume list <mount>`. Could not run, or a nonzero exit, is an
/// error carrying stderr: an unreadable target is never an empty listing
/// (fail-silent rule 6).
fn subvolume_listing(runner: &dyn CommandRunner, mount: &str) -> Result<String, String> {
    let output = runner
        .output(Command::new("btrfs").args(["subvolume", "list", mount]))
        .map_err(|e| format!("btrfs could not be run: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "btrfs subvolume list {mount}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// The snapshot-match rule, pinned by `tests/fixtures/boot-subvol-listing.txt`
/// (which the bash suite reads too). A path matches when it is
/// `<subdir>/<snap_name>.<TS>` with `TS` a btrbk timestamp, so `root-root.*`,
/// `root-.latest`, `root-.<TS>.new` and `nvmeX/…` do not. The newest match has
/// the greatest `TS` (bytewise), ties broken by the greater full path: the
/// path alone would pick by subdirectory name.
fn latest_matching_snapshot<'a>(
    listing: &'a str,
    subdirs: &[String],
    snap_name: &str,
) -> Option<&'a str> {
    let prefixes: Vec<String> = subdirs
        .iter()
        .map(|d| format!("{}/{snap_name}.", d.trim_matches('/')))
        .collect();
    listing
        .lines()
        .filter_map(|line| line.split_whitespace().last())
        .filter_map(|path| {
            prefixes
                .iter()
                .find_map(|p| path.strip_prefix(p.as_str()))
                .filter(|ts| is_btrbk_timestamp(ts))
                .map(|ts| (ts, path))
        })
        .max()
        .map(|(_, path)| path)
}

/// The boot subvolumes `[boot].subvolumes` names, each with the snapshot name
/// `btrbk.conf` gives it and the target subdirectories its snapshots land in.
/// What both `archive_boot` and `backup-run.sh` (through `backup boot-plan`)
/// act on, so the names are derived in one place (bd dtm).
pub fn boot_plan(config: &Config) -> Result<Vec<BootPlanItem>, String> {
    let conf = Path::new(&config.general.btrbk_conf);
    let names = crate::forget::live_subvol_snapshot_names(conf).map_err(|e| {
        format!(
            "Cannot read {} ({e}) — no boot subvolume is touched rather than a snapshot name guessed",
            conf.display()
        )
    })?;
    Ok(config
        .boot
        .subvolumes
        .iter()
        .map(|subvol| BootPlanItem {
            subvol: subvol.clone(),
            snapshot_name: names.get(subvol.as_str()).cloned(),
            subdirs: subdirs_for_subvol(config, subvol)
                .into_iter()
                .map(|d| d.trim_matches('/').to_string())
                .collect(),
        })
        .collect())
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

/// [`btrfs_ok`] for the boot step: a `btrfs` that cannot be run at all is a
/// recorded failure (`None`), never a quiet "no".
fn btrfs_step(
    runner: &dyn CommandRunner,
    args: &[&str],
    out: &mut BootOutcome,
    progress: &dyn ProgressCallback,
) -> Option<bool> {
    match btrfs_ok(runner, args) {
        Ok(ok) => Some(ok),
        Err(e) => {
            out.fail(progress, format!("btrfs could not be run: {e}"));
            None
        }
    }
}

/// Whether `path` exists, for a decision to create, archive, delete or build.
/// A stat that fails is not "absent": that reading sends a delete path down
/// the wrong branch (a snapshot nested in a stale `.new`, then live replaced
/// by it). It is a recorded failure (`None`) and the caller stops.
fn exists_or_fail(
    path: &str,
    out: &mut BootOutcome,
    progress: &dyn ProgressCallback,
) -> Option<bool> {
    match Path::new(path).try_exists() {
        Ok(exists) => Some(exists),
        Err(e) => {
            out.fail(
                progress,
                format!("Cannot tell whether {path} exists ({e}) — leaving it untouched"),
            );
            None
        }
    }
}

/// Create or replace the boot subvolumes on backup targets from the newest
/// received snapshot, as `update_boot_subvolumes()` in `scripts/backup-run.sh`
/// does, and with the same classification (`.claude/rules/backup.md`
/// §Boot Subvolume Archival). This entry point (`backup boot-archive`, the
/// GUI's Boot Archive) replaces: archive the live one, then swap in the new.
///
/// **The ordering is the safety property**: locate the replacement and build
/// it alongside the live subvolume BEFORE removing the live one, so no failure
/// path can leave `@` absent. Until 0.7.20.0 the order was archive -> delete ->
/// look up -> recreate, which destroyed the live subvolume whenever the lookup
/// failed — and for `@` the lookup could never succeed, so every Rust-path run
/// on a primary target left the mount without a bootable `@` (bd
/// DAS-Backup-Manager-5ig).
///
/// Every target it will write under is verified first, and it refuses — nothing
/// written — on one whose mount point is a bare directory or holds another
/// filesystem (bd DAS-Backup-Manager-7tx, 9on): [`mount::verify_write_targets`]
/// reads each `target.mount` as the root filesystem's own directory if nothing
/// is mounted there, and the snapshots, deletions and renames below would land
/// on it. A target whose mount point does not exist is not written and is left
/// alone, as `backup-run.sh` leaves an absent target.
pub fn archive_boot(config: &Config, progress: &dyn ProgressCallback) -> BootStep {
    archive_boot_with(config, None, true, progress, &StepEnv::HOST)
}

/// [`archive_boot`] in `env`, for the targets `selected` (`None`: every one).
/// `replace` is a full run: an existing boot subvolume is archived and
/// replaced; without it an existing one is skipped and only a missing one is
/// created. Never returns [`BootStep::NotSelected`].
fn archive_boot_with(
    config: &Config,
    selected: Option<&[String]>,
    replace: bool,
    progress: &dyn ProgressCallback,
    env: &StepEnv,
) -> BootStep {
    if !config.boot.enabled {
        progress.on_log(
            LogLevel::Info,
            "Boot subvolumes: disabled in config ([boot] enabled = false)",
        );
        return BootStep::DisabledInConfig;
    }
    let mut out = BootOutcome::default();
    if config.targets.is_empty() {
        out.warn(
            progress,
            "No backup targets configured — no boot subvolume updated".into(),
        );
        return BootStep::Ran(out);
    }

    // Whether each selected target's mount point exists, mirrors included. A
    // stat that fails is not "absent" (`.claude/rules/backup.md` §Boot Subvolume
    // Archival: mount state that cannot be told fails the whole step): it is a
    // recorded failure and no btrfs call is made.
    let is_selected = |t: &Target| selected.is_none_or(|labels| labels.contains(&t.label));
    let mut mount_exists: std::collections::HashMap<&str, bool> = std::collections::HashMap::new();
    for t in config.targets.iter().filter(|t| is_selected(t)) {
        match Path::new(&t.mount).try_exists() {
            Ok(present) => {
                mount_exists.insert(t.label.as_str(), present);
            }
            Err(e) => {
                out.fail(
                    progress,
                    format!(
                        "Boot subvolumes not updated: cannot tell whether {} is mounted ({e})",
                        t.mount
                    ),
                );
                return BootStep::Ran(out);
            }
        }
    }
    // The targets written under: selected, not a mirror (those carry their own
    // OS and are skipped below), and with a mount point that exists. One that
    // exists must be the filesystem it should be.
    let writes: Vec<String> = config
        .targets
        .iter()
        .filter(|t| {
            t.role != TargetRole::Mirror && mount_exists.get(t.label.as_str()) == Some(&true)
        })
        .map(|t| t.label.clone())
        .collect();
    if let Err(e) = (env.verify)(&config.targets, &writes, progress) {
        out.fail(progress, format!("Boot subvolumes not updated: {e}"));
        return BootStep::Ran(out);
    }

    // Snapshot names come from the file btrbk itself reads. If it cannot be
    // read we do nothing at all rather than fall back to a guess: a wrong name
    // here is what used to cost the live subvolume.
    let plan = match boot_plan(config) {
        Ok(plan) => plan,
        Err(e) => {
            out.fail(progress, e);
            return BootStep::Ran(out);
        }
    };

    let ts = format_timestamp();
    let targets: Vec<&Target> = config.targets.iter().filter(|t| is_selected(t)).collect();
    progress.on_stage("Boot subvolumes", targets.len() as u64);
    for (i, target) in targets.iter().enumerate() {
        progress.on_progress(
            i as u64,
            targets.len() as u64,
            &format!("Boot subvolumes on {}", target.label),
        );
        // Mirror targets carry a genuinely independent OS install in their own
        // @/@home — never archive-then-replace it with a host snapshot. Mirrors
        // still receive ordinary btrbk send/receive; only this step skips
        // them. Wording matches update_boot_subvolumes() in
        // scripts/backup-run.sh (bd DAS-Backup-Manager-am1).
        if target.role == TargetRole::Mirror {
            // Counted as skipped only when mounted, as the script counts it.
            if mount_exists.get(target.label.as_str()) == Some(&true) {
                progress.on_log(
                    LogLevel::Info,
                    &format!("[{}] Skipping mirror target (independent OS)", target.mount),
                );
                out.skipped += 1;
            } else {
                progress.on_log(
                    LogLevel::Info,
                    &format!(
                        "[{}] Not mounted — mirror target left alone (independent OS)",
                        target.mount
                    ),
                );
            }
            continue;
        }
        // Only a target verified above is written: one that is selected but
        // has no mount point was not verified, and is left alone rather than
        // trusted to fail later on a `btrfs` call (review M6).
        if !writes.contains(&target.label) {
            progress.on_log(
                LogLevel::Info,
                &format!(
                    "[{}] Not mounted — boot subvolumes not updated on '{}'",
                    target.mount, target.label
                ),
            );
            continue;
        }
        // An unreadable target is never "no snapshots" (fail-silent rule 6).
        let listing = match subvolume_listing(env.runner, &target.mount) {
            Ok(listing) => listing,
            Err(e) => {
                out.fail(
                    progress,
                    format!(
                        "[{}] Could not list subvolumes ({e}) — refusing to read an unreadable \
                         target as 'no snapshots'",
                        target.label
                    ),
                );
                continue;
            }
        };
        for item in &plan {
            let run = BootRun {
                runner: env.runner,
                btrbk_conf: &config.general.btrbk_conf,
                replace,
                ts: &ts,
            };
            update_boot_subvol(&run, target, item, &listing, &mut out, progress);
        }
    }
    progress.on_progress(
        targets.len() as u64,
        targets.len() as u64,
        "Boot subvolumes done",
    );
    BootStep::Ran(out)
}

/// What one boot-step pass shares across its targets and subvolumes.
struct BootRun<'a> {
    runner: &'a dyn CommandRunner,
    btrbk_conf: &'a str,
    /// A full run: archive and replace an existing boot subvolume.
    replace: bool,
    ts: &'a str,
}

/// One boot subvolume on one (mounted, verified, listed) target. Classifies
/// as `.claude/rules/backup.md` §Boot Subvolume Archival says; every failure
/// stops this subvolume and leaves the live one as it was (until the delete,
/// which is only reached once its replacement exists beside it).
fn update_boot_subvol(
    run: &BootRun,
    target: &Target,
    item: &BootPlanItem,
    listing: &str,
    out: &mut BootOutcome,
    progress: &dyn ProgressCallback,
) {
    let (label, tgt_mount, subvol) = (&target.label, &target.mount, &item.subvol);
    let Some(snap_name) = &item.snapshot_name else {
        out.warn(
            progress,
            format!(
                "[{label}] {subvol} has no snapshot_name in {} — leaving it untouched",
                run.btrbk_conf
            ),
        );
        return;
    };
    if item.subdirs.is_empty() {
        out.warn(
            progress,
            format!(
                "[{label}] No source declares target_subdirs for {subvol} — leaving it untouched"
            ),
        );
        return;
    }
    let Some(latest) = latest_matching_snapshot(listing, &item.subdirs, snap_name) else {
        out.warn(
            progress,
            format!("[{label}] No btrbk snapshot named '{snap_name}' — leaving {subvol} untouched"),
        );
        return;
    };
    let latest_path = format!("{tgt_mount}/{latest}");
    let live = format!("{tgt_mount}/{subvol}");
    let Some(live_exists) = exists_or_fail(&live, out, progress) else {
        return;
    };

    if live_exists && !run.replace {
        progress.on_log(
            LogLevel::Info,
            &format!("[{label}] {subvol} exists, skipping (a full run replaces it)"),
        );
        out.skipped += 1;
        return;
    }

    // Absent, in either mode: create it from the newest snapshot. Nothing is
    // there to lose.
    if !live_exists {
        match btrfs_step(
            run.runner,
            &["subvolume", "snapshot", &latest_path, &live],
            out,
            progress,
        ) {
            None => {}
            Some(true) => {
                progress.on_log(LogLevel::Info, &format!("Created {live} from {latest}"));
                out.updated += 1;
            }
            Some(false) => out.fail(progress, format!("Failed to create {live} from {latest}")),
        }
        return;
    }

    // Full run, live subvolume present: archive -> clear stale staging ->
    // build staging -> delete live -> rename.
    let staging = format!("{live}.new");
    let archive_name = format!("{subvol}.archive.{}", run.ts);
    let archive_path = format!("{tgt_mount}/{archive_name}");

    // Every existence check comes before the first mutation.
    let Some(staging_exists) = exists_or_fail(&staging, out, progress) else {
        return;
    };

    // Step 1: archive the outgoing subvolume read-only.
    match btrfs_step(
        run.runner,
        &["subvolume", "snapshot", "-r", &live, &archive_path],
        out,
        progress,
    ) {
        None => return,
        Some(false) => {
            out.fail(
                progress,
                format!("Failed to archive {live} -> {archive_path}"),
            );
            return;
        }
        Some(true) => progress.on_log(
            LogLevel::Info,
            &format!("Archived {live} -> {archive_path}"),
        ),
    }

    // Step 2: clear any staging subvolume left by an interrupted run.
    if staging_exists {
        match btrfs_step(
            run.runner,
            &["subvolume", "delete", &staging],
            out,
            progress,
        ) {
            None => return,
            Some(false) => {
                out.fail(
                    progress,
                    format!("Stale {staging} could not be removed — leaving {subvol} untouched"),
                );
                return;
            }
            Some(true) => {}
        }
    }

    // Step 3: build the replacement ALONGSIDE the live subvolume.
    match btrfs_step(
        run.runner,
        &["subvolume", "snapshot", &latest_path, &staging],
        out,
        progress,
    ) {
        None => return,
        Some(false) => {
            out.fail(
                progress,
                format!("Failed to create {staging} from {latest} — leaving {subvol} untouched"),
            );
            return;
        }
        Some(true) => {}
    }

    // Step 4: only now remove the live subvolume.
    match btrfs_step(run.runner, &["subvolume", "delete", &live], out, progress) {
        None => return,
        Some(false) => {
            out.fail(
                progress,
                format!("Failed to delete {live} — discarding {staging}"),
            );
            if !matches!(
                btrfs_ok(run.runner, &["subvolume", "delete", &staging]),
                Ok(true)
            ) {
                progress.on_log(
                    LogLevel::Warning,
                    &format!("[{label}] {staging} could not be discarded — remove it by hand"),
                );
            }
            return;
        }
        Some(true) => {}
    }

    // Step 5: swap the replacement into place.
    if let Err(e) = std::fs::rename(&staging, &live) {
        out.fail(
            progress,
            format!(
                "Renamed nothing: {staging} -> {live} failed ({e}). \
                 The archive {archive_name} on {tgt_mount} holds the previous contents."
            ),
        );
        return;
    }
    progress.on_log(
        LogLevel::Info,
        &format!("Recreated {live} from {latest} (archived old {subvol} -> {archive_name})"),
    );
    out.updated += 1;
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

/// The first step of `btrdasd backup snapshot` and `send` (CLI and helper), as
/// [`sync_before_backup`] is of `run`: `btrbk.conf` is brought into line with
/// `config.toml` and the sources, and the config sync wrote is returned. A step
/// that selects everything passes btrbk no filter at all, so it trusts
/// `btrbk.conf` — a target removed from `config.toml` but still in a stale
/// `btrbk.conf` would otherwise be written without ever being verified (review
/// M5). Unlike `run`, a failed sync stops the step: a `run` goes on because the
/// subvolumes already configured must still be backed up, but a step that
/// cannot tell whether `btrbk.conf` is current has no such obligation.
pub fn sync_for_manual_step(
    config_path: &Path,
    before: &Config,
    progress: &dyn ProgressCallback,
) -> Result<Config, String> {
    sync_for_manual_step_with(
        config_path,
        before,
        &crate::caldate::today(),
        &SystemRunner,
        &health::is_mountpoint,
        progress,
    )
}

/// [`sync_for_manual_step`] with the runner, the mount-point test and the date
/// given.
fn sync_for_manual_step_with(
    config_path: &Path,
    before: &Config,
    today: &str,
    runner: &dyn CommandRunner,
    is_mountpoint: &dyn Fn(&Path) -> bool,
    progress: &dyn ProgressCallback,
) -> Result<Config, String> {
    let (config, section) = sync_before_backup(
        config_path,
        before,
        false,
        today,
        runner,
        is_mountpoint,
        progress,
    );
    if section.failed {
        return Err(
            "The subvolume sync failed (its report is in the log above), so btrbk.conf may not \
             match config.toml — nothing was run"
                .to_string(),
        );
    }
    Ok(config)
}

/// Whether the run emails its report: the caller ticked Email and `[email]`
/// is enabled. The report is written either way.
fn emails_report(options: &BackupOptions, config: &Config) -> bool {
    options.email_report && config.email.enabled
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

/// Why a selection that was made and is empty cannot be run, or `None` when
/// nothing is wrong with it. `None` for a list that was never given
/// (`BackupOptions` with no `sources`): that one means "all", an empty one
/// that was given means "nothing" — an operator who unticked every box asked
/// for no backup, not for every backup. Callers whose input cannot say "not
/// specified" (the D-Bus helper's `as` arguments) hand over `Some(list)`, so
/// an empty list is refused there.
pub fn empty_selection(options: &BackupOptions) -> Option<String> {
    options
        .sources
        .as_deref()
        .and_then(refuse_empty_sources)
        .or_else(|| options.targets.as_deref().and_then(refuse_empty_targets))
}

/// Why a selection naming a label the configuration does not have cannot be
/// run, or `None` when every label named is configured. Checked against the
/// configuration the job was handed, before the first lock and mount, so a
/// stale label list (a GUI that has not reloaded, a typo) is refused with
/// nothing started — the same class as [`empty_selection`], and the same
/// outcome (`CouldNotStart`: exit 1, nothing recorded). An unknown *source*
/// was already refused by `btrbk_filters`, but only after the locks, the
/// sync and the mounts; an unknown *target* in a list with some known ones
/// was dropped without a word.
pub fn unknown_label(config: &Config, options: &BackupOptions) -> Option<String> {
    let refuse = |what: &str, label: &str, known: Vec<&str>| {
        format!(
            "The {what} '{label}' is not in the configuration ({}) — nothing was started",
            known.join(", ")
        )
    };
    if let Some(label) = options
        .sources
        .iter()
        .flatten()
        .find(|l| !config.sources.iter().any(|s| &s.label == *l))
    {
        return Some(refuse(
            "source",
            label,
            config.sources.iter().map(|s| s.label.as_str()).collect(),
        ));
    }
    let label = options
        .targets
        .iter()
        .flatten()
        .find(|l| !config.targets.iter().any(|t| &t.label == *l))?;
    Some(refuse(
        "target",
        label,
        config.targets.iter().map(|t| t.label.as_str()).collect(),
    ))
}

/// [`empty_selection`] for one list of source labels that was given: the
/// refusal if it is empty. [`empty_selection`] calls it for the lists the
/// helper's `BackupRun` passes (`Some(list)`: the GUI cannot say "not specified").
fn refuse_empty_sources(sources: &[String]) -> Option<String> {
    sources
        .is_empty()
        .then(|| nothing_selected("source", "target"))
}

/// [`refuse_empty_sources`] for target labels (the D-Bus `BackupRun` `targets` argument).
fn refuse_empty_targets(targets: &[String]) -> Option<String> {
    targets
        .is_empty()
        .then(|| nothing_selected("target", "source"))
}

fn nothing_selected(what: &str, other: &str) -> String {
    format!(
        "No {what} selected — nothing was started. An empty selection is not \"every {what}\"; \
         tick at least one {what} (and one {other}) and run again."
    )
}

/// The source labels a run backs up: the ones named or — when none were
/// specified — every source with at least one subvolume that is not
/// manual-only. A selection made and empty is refused, never widened.
fn effective_sources(config: &Config, options: &BackupOptions) -> Result<Vec<String>, String> {
    if let Some(why) = empty_selection(options).or_else(|| unknown_label(config, options)) {
        return Err(why);
    }
    match &options.sources {
        None => {
            let automatic: Vec<String> = config
                .sources
                .iter()
                .filter(|src| {
                    // Include source if at least one non-manual_only subvolume exists.
                    src.subvolumes.iter().any(|sv| !sv.manual_only)
                })
                .map(|src| src.label.clone())
                .collect();
            if automatic.is_empty() {
                // "Not specified" resolving to nothing is not "everything":
                // btrbk would be handed the manual-only subvolumes too.
                return Err("No source to back up: every source has only manual-only \
                            subvolumes, and none was named — nothing was started"
                    .into());
            }
            Ok(automatic)
        }
        Some(named) => Ok(named.clone()),
    }
}

/// The target labels a run writes to.
///
/// When targets are explicitly specified (D-Bus helper pre-mounts them),
/// trust the caller — don't re-check mount status.  Only auto-detect
/// mounted targets when the caller never specified any (standalone CLI).
/// A selection made and empty, or one naming a target the configuration does
/// not have (even beside known ones), is refused: neither is "every mounted
/// target", and no label is dropped silently.
fn effective_targets(config: &Config, options: &BackupOptions) -> Result<Vec<String>, String> {
    if let Some(why) = empty_selection(options).or_else(|| unknown_label(config, options)) {
        return Err(why);
    }
    // Caller specified targets — every one is in the configuration (checked
    // above, so none is dropped) but its mount status is not re-checked: the
    // caller already ensured mount via MountGuard.
    Ok(match &options.targets {
        Some(named) => named.clone(),
        None => mounted_target_labels(config),
    })
}

/// How the error of a failed btrbk step begins (see [`Pipeline::failed`]). The
/// report reads them back to say whether btrbk failed: the text after them is
/// btrbk's, or this module's own refusal ("none of the selected sources sends
/// to a selected target"), which need not mention btrbk at all.
const BTRBK_STEP_FAILURES: [&str; 3] = [
    "Snapshot step failed",
    "Send step failed",
    "Full backup pipeline failed",
];

/// Whether `error` is the error of a failed btrbk step.
pub fn is_btrbk_step_failure(error: &str) -> bool {
    BTRBK_STEP_FAILURES.iter().any(|p| error.starts_with(p))
}

/// What the btrbk steps counted, and which of them failed.
#[derive(Debug, PartialEq, Eq)]
struct Pipeline {
    /// `None` once a step that counts them was run and failed; a step that was
    /// not asked for leaves `Some(0)`.
    created: Option<usize>,
    sent: Option<usize>,
    cleaned: usize,
    bytes: u64,
    /// One per failed step. A failed step does not stop the next one.
    errors: Vec<String>,
}

impl Default for Pipeline {
    /// Nothing run yet: no step has failed, so nothing is unknown.
    fn default() -> Self {
        Self {
            created: Some(0),
            sent: Some(0),
            cleaned: 0,
            bytes: 0,
            errors: Vec::new(),
        }
    }
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
    let sources = Some(sources);
    let snapshots = |done: &mut Pipeline| match create_snapshots_with(
        config,
        sources,
        Some(targets),
        progress,
        env.runner,
    ) {
        Ok(n) => done.created = Some(n),
        Err(e) => {
            done.created = None;
            done.failed(progress, format!("{}: {e}", BTRBK_STEP_FAILURES[0]));
        }
    };
    let send = |done: &mut Pipeline| {
        // No --preserve: btrbk enforces retention, in both modes.
        match send_snapshots_with(config, sources, targets, false, progress, env) {
            Ok((sent, bytes)) => {
                done.sent = Some(sent);
                done.bytes = bytes;
            }
            Err(e) => {
                done.sent = None;
                done.failed(progress, format!("{}: {e}", BTRBK_STEP_FAILURES[1]));
            }
        }
    };
    match (mode, options.steps) {
        (_, BtrbkSteps::SnapshotOnly) => snapshots(&mut done),
        (_, BtrbkSteps::SendOnly) => send(&mut done),
        (BackupMode::Full, BtrbkSteps::SnapshotAndSend) => {
            match run_full_pipeline_with(config, sources, targets, progress, env) {
                Ok((created, sent, cleaned, bytes)) => {
                    done.created = Some(created);
                    done.sent = Some(sent);
                    done.cleaned = cleaned;
                    done.bytes = bytes;
                }
                Err(e) => {
                    // One btrbk run did all of it: what it created and sent is not known.
                    (done.created, done.sent) = (None, None);
                    done.failed(progress, format!("{}: {e}", BTRBK_STEP_FAILURES[2]));
                }
            }
        }
        (BackupMode::Incremental, BtrbkSteps::SnapshotAndSend) => {
            snapshots(&mut done);
            send(&mut done);
        }
    }
    done
}

/// Indexes one mounted tree into a database: [`indexer::walk`].
type WalkTree<'a> =
    dyn Fn(&Path, &Database) -> Result<indexer::WalkResult, Box<dyn std::error::Error>> + 'a;

/// Walk every mounted target and index the snapshots that are new, so the
/// content index follows the backup. Whether at least one target was indexed.
/// Failures are non-fatal and logged (a target whose walk fails, a database
/// that cannot be opened): the backup itself is safe.
///
/// `mount_of` finds where a target is mounted, `walk` indexes one tree;
/// [`index_mounted_targets`] hands them the real ones.
fn index_targets_with(
    config: &Config,
    progress: &dyn ProgressCallback,
    mount_of: &dyn Fn(&Target) -> Option<String>,
    walk: &WalkTree<'_>,
) -> bool {
    let db = match Database::open(&config.general.db_path) {
        Ok(db) => db,
        Err(e) => {
            progress.on_log(
                LogLevel::Warning,
                &format!("Cannot open index DB for post-backup indexing (non-fatal): {e}"),
            );
            return false;
        }
    };
    let mut targets_indexed = 0usize;
    for target in &config.targets {
        let Some(path) = mount_of(target) else {
            continue;
        };
        progress.on_log(
            LogLevel::Info,
            &format!("Indexing target '{}' at {path}", target.label),
        );
        match walk(Path::new(&path), &db) {
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
                    &format!("Indexing target '{}' failed (non-fatal): {e}", target.label),
                );
            }
        }
    }
    targets_indexed > 0
}

/// [`index_targets_with`] on this machine: the targets' real mounts, the real
/// walk. What [`StepEnv::HOST`] runs.
fn index_mounted_targets(config: &Config, progress: &dyn ProgressCallback) -> bool {
    index_targets_with(
        config,
        progress,
        &|target| health::find_any_mount(&target.mount, &target.serial, &target.role),
        &indexer::walk,
    )
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
    let mut indexed = false;

    // ---------- Resolve effective sources and targets ----------

    let effective_sources = effective_sources(config, options)?;
    let effective_targets = effective_targets(config, options)?;

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
    let total_steps = options.steps.snapshots() as u64
        + options.steps.sends() as u64
        + options.boot_archive as u64
        + options.index_after as u64
        + 1; // the report is always written
    progress.on_stage("Backup", total_steps);

    let mode = options.mode.unwrap_or(BackupMode::Incremental);

    // ---------- Dry-run path ----------

    if options.dry_run {
        if options.steps.snapshots() {
            progress.on_log(
                LogLevel::Info,
                &format!(
                    "DRY RUN ({mode}): would create snapshots for {:?}",
                    effective_sources
                ),
            );
        }
        if options.steps.sends() {
            progress.on_log(
                LogLevel::Info,
                &format!(
                    "DRY RUN ({mode}): would send to targets {:?}",
                    effective_targets
                ),
            );
        }
        if options.boot_archive {
            let what = if !config.boot.enabled {
                "boot subvolumes: disabled in config"
            } else if mode == BackupMode::Full {
                "would archive and replace boot subvolumes"
            } else {
                "would create any missing boot subvolume (never replace one)"
            };
            progress.on_log(LogLevel::Info, &format!("DRY RUN ({mode}): {what}"));
        }
        if options.index_after {
            progress.on_log(
                LogLevel::Info,
                &format!("DRY RUN ({mode}): would index the targets afterwards"),
            );
        }
        progress.on_log(
            LogLevel::Info,
            &format!(
                "DRY RUN ({mode}): {}",
                if options.email_report {
                    "would email the report"
                } else {
                    "would save the report without emailing it"
                }
            ),
        );

        let success = errors.is_empty();
        let result = BackupResult {
            success,
            mode,
            snapshots_created: Some(0),
            snapshots_sent: Some(0),
            snapshots_cleaned: 0,
            bytes_sent: 0,
            boot: BootStep::NotSelected,
            indexed: false,
            report_sent: false,
            errors,
            duration_secs: start.elapsed().as_secs(),
        };
        return Ok(result);
    }

    // ---------- Live pipeline ----------
    //
    // Incremental: `btrbk snapshot` + `btrbk resume` (no --preserve)
    //   Two steps, so snapshot-only and send-only are possible.  btrbk
    //   enforces the retention policy in both modes (`run_pipeline`).
    //
    // Full: `btrbk run` (one run: snapshot + send + retention cleanup)
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
    let usage_before = (env.usage)(config, progress);
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
    if bytes_sent == 0 && (snapshots_sent.unwrap_or(0) > 0 || snapshots_created.unwrap_or(0) > 0) {
        // `usage` forces BTRFS to commit pending transactions first, so statvfs
        // reflects the data that was just received. Without that, BTRFS defers
        // metadata updates and statvfs returns stale values, making the delta
        // zero.
        let usage_after = (env.usage)(config, progress);
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

    // Step (c): boot subvolumes — create a missing one on every run, archive
    // and replace on a full run, as backup-run.sh's update_boot_subvolumes does
    // (bd woq, dtm). A failure fails the run; an absence only warns.
    let boot = if options.boot_archive {
        let step = archive_boot_with(
            config,
            Some(&effective_targets),
            mode == BackupMode::Full,
            progress,
            env,
        );
        if let BootStep::Ran(o) = &step {
            errors.extend(o.failures.iter().map(|f| format!("{BOOT_ERROR_PREFIX}{f}")));
        }
        step
    } else {
        BootStep::NotSelected
    };

    // Step (d): Index — walk each target's mount path to pick up new snapshots.
    if options.index_after {
        indexed = (env.index)(config, progress);
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
        boot,
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
        && result.snapshots_created == Some(0)
        && result.snapshots_sent == Some(0)
        && result.snapshots_cleaned == 0
        && result.boot.has_nothing_to_report()
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
    // A count that could not be measured says so; it is never printed as 0.
    let created = result.snapshots_created.map_or_else(
        || "snapshots created: unknown".to_string(),
        |n| format!("{n} snapshots created"),
    );
    let sent = result
        .snapshots_sent
        .map_or_else(|| "sent: unknown".to_string(), |n| format!("{n} sent"));
    let mut summary = format!(
        "Backup {status} ({mode}): {created}, {sent}{cleaned}, boot subvolumes: {}",
        result.boot.row(),
    );
    if !result.success {
        summary.push_str(&format!(" — {}", result.errors.join("; ")));
    }
    summary
}

/// What became of a run's report: whether it was written, and whether it was
/// mailed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportDelivery {
    /// `Err(why)` when a report was asked for and could not be written;
    /// `Ok` when it was written, or none was asked for.
    pub saved: Result<(), String>,
    /// Whether the email went out.
    pub emailed: bool,
}

impl ReportDelivery {
    /// Why the report is lost, if it is: not written and not mailed either —
    /// the one case in which nothing of it exists. A report that was written
    /// but not mailed (or mailed but not written) is not lost.
    pub fn lost(&self) -> Option<&str> {
        match &self.saved {
            Err(why) if !self.emailed => Some(why),
            _ => None,
        }
    }
}

/// Write the run report to `[general].last_report` — always — and email it
/// when the caller ticked Email and `[email]` is enabled — the report is
/// written whether or not it is mailed, as `backup-run.sh` writes
/// `$LAST_REPORT` before any send. `data` is what was captured while the
/// targets were mounted. Email failure alone is non-fatal — the backup data
/// is safe and the report is on disk — and is logged, not added to the run's
/// errors; a report that exists nowhere is not (see [`ReportDelivery::lost`]).
pub fn deliver_report(
    config: &Config,
    options: &BackupOptions,
    result: &BackupResult,
    data: &crate::report::ReportData,
    progress: &dyn ProgressCallback,
) -> ReportDelivery {
    let report_text =
        crate::report::format_report_from(result, options.subvolume_sync.as_ref(), data);
    // The directory may not exist yet (backup-run.sh: `mkdir -p`). A plain
    // write, not `fsutil::write_atomic`: that makes a new file, which does
    // not keep an existing report's mode and owner.
    let report_path = Path::new(&config.general.last_report);
    let saved = report_path
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .map_or(Ok(()), std::fs::create_dir_all)
        .and_then(|()| std::fs::write(report_path, &report_text))
        .map_err(|e| {
            let why = format!(
                "Failed to save report to {}: {e}",
                config.general.last_report
            );
            progress.on_log(LogLevel::Warning, &why);
            why
        });
    if !emails_report(options, config) {
        return ReportDelivery {
            saved,
            emailed: false,
        };
    }
    let emailed = match crate::report::send_email_report(&report_text, config) {
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
    };
    ReportDelivery { saved, emailed }
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
    /// Write and email the report; what became of it.
    fn report(
        &self,
        config: &Config,
        options: &BackupOptions,
        result: &BackupResult,
        data: &crate::report::ReportData,
        progress: &dyn ProgressCallback,
    ) -> ReportDelivery;
    /// Add the run to `backup_runs`.
    fn record(&self, config: &Config, result: &BackupResult) -> Result<(), String>;
}

/// How a backup job ended.
#[derive(Debug)]
pub enum BackupJobOutcome {
    /// Another backup holds the singleton lock — declined, not queued.
    Declined,
    /// The job never began: the selection was refused, or the locks could not
    /// be taken. Nothing was mounted, run or recorded — the cases in which
    /// `backup-run.sh` exits 1.
    CouldNotStart(String),
    /// The job began and stopped on the state of a target or a source — no
    /// target would mount, a target or a source volume was not what it must
    /// be, btrbk could not be started — and ran nothing, or nothing that
    /// counts. Recorded as a failed run unless it was a dry run, as the
    /// script's abort path does (bd `2my`), so it does not vanish from the
    /// history. Exits 3.
    Aborted(String),
    /// The job ran. Recorded in `backup_runs` (unless a dry run); its
    /// `success` says whether everything worked.
    Ran(BackupResult),
}

impl BackupJobOutcome {
    pub fn success(&self) -> bool {
        matches!(self, Self::Ran(r) if r.success)
    }

    /// The exit status of `btrdasd backup run`, which is the rule of
    /// `btrdasd doctor` and of `backup-run.sh` (bd `d1r`, `vzsu`): **0** it
    /// ran and nothing failed (a warning is still 0) or another backup was
    /// running and this one declined; **3** it began and something failed or
    /// it aborted on a target's or a source's state — whatever caused it will
    /// usually still be there on the next start; **1** it could not start.
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::Declined => 0,
            Self::CouldNotStart(_) => 1,
            Self::Aborted(_) => 3,
            Self::Ran(result) if result.success => 0,
            Self::Ran(_) => 3,
        }
    }

    /// Whether the job succeeded, and the line that says how it ended — what
    /// the GUI shows when the job finishes.
    pub fn finish_line(&self, dry_run: bool) -> (bool, String) {
        match self {
            Self::Declined => (false, "A backup is already running — declined".to_string()),
            Self::CouldNotStart(why) | Self::Aborted(why) => (false, why.clone()),
            Self::Ran(result) => {
                let mut line = backup_summary(result, dry_run);
                // A dry run changes nothing, so its boot step has nothing to add.
                let messages = if dry_run {
                    String::new()
                } else {
                    result.boot.message_lines()
                };
                if !messages.is_empty() {
                    line.push('\n');
                    line.push_str(&messages);
                }
                (result.success, line)
            }
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

/// The job began and stopped on the state of a target or a source: the
/// outcome, after recording it as a failed run — counts unknown, the reason as
/// its error — unless it was a dry run, as `backup-run.sh` records an abort
/// (bd `2my`). A record that cannot be written is added to the reason.
fn abort_job(
    host: &dyn BackupJobHost,
    config: &Config,
    options: &BackupOptions,
    started: std::time::Instant,
    why: String,
    progress: &dyn ProgressCallback,
) -> BackupJobOutcome {
    if options.dry_run {
        return BackupJobOutcome::Aborted(why);
    }
    let result = BackupResult {
        success: false,
        mode: options.mode.unwrap_or(BackupMode::Incremental),
        snapshots_created: None,
        snapshots_sent: None,
        snapshots_cleaned: 0,
        bytes_sent: 0,
        boot: BootStep::NotSelected,
        indexed: false,
        report_sent: false,
        errors: vec![why.clone()],
        duration_secs: started.elapsed().as_secs(),
    };
    match host.record(config, &result) {
        Ok(()) => BackupJobOutcome::Aborted(why),
        Err(e) => {
            let msg = format!("{why}; history not recorded: {e}");
            progress.on_log(LogLevel::Error, &format!("history not recorded: {e}"));
            BackupJobOutcome::Aborted(msg)
        }
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
    // Before the first lock and the first mount: a selection of nothing is
    // refused with nothing touched (bd DAS-Backup-Manager-7tx).
    if let Some(why) = empty_selection(&options).or_else(|| unknown_label(&config, &options)) {
        return BackupJobOutcome::CouldNotStart(why);
    }
    let started = std::time::Instant::now();
    let locks = match host.acquire_locks(progress) {
        Ok(Some(locks)) => locks,
        Ok(None) => return BackupJobOutcome::Declined,
        Err(e) => {
            return BackupJobOutcome::CouldNotStart(format!("Could not acquire backup locks: {e}"));
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
            return abort_job(
                host,
                &config,
                &options,
                started,
                with_still_mounted(format!("Mount failed: {e}"), &still),
                progress,
            );
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
            return abort_job(
                host,
                &config,
                &options,
                started,
                with_still_mounted(format!("Backup failed: {e}"), &still_mounted),
                progress,
            );
        }
    };
    if let Some(e) = mount::still_mounted_error(&still_mounted) {
        progress.on_log(LogLevel::Error, &e);
        result.errors.push(e);
        result.success = false;
    }
    if let Some(captured) = &captured {
        let delivery = host.report(&config, &options, &result, captured, progress);
        result.report_sent = delivery.emailed;
        // With email off — or failing — a report that was not written exists
        // nowhere: the run fails, and the row below says why, as in
        // `backup-run.sh` (bd `d1r` N4). An email that fails beside a report
        // that was written stays a warning: the report is on disk.
        if let Some(why) = delivery.lost() {
            let msg = format!("report: {why}");
            progress.on_log(LogLevel::Error, &msg);
            result.errors.push(msg);
            result.success = false;
        }
        // A run that cannot be recorded vanishes from the history while the
        // job reports success: it fails the job, in its errors and its status
        // (bd DAS-Backup-Manager-no4).
        if let Err(e) = host.record(&config, &result) {
            let msg = format!("history not recorded: {e}");
            progress.on_log(LogLevel::Error, &msg);
            result.errors.push(msg);
            result.success = false;
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
    ) -> ReportDelivery {
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
            result.snapshots_created,
            Some(0),
            "dry_run must create 0 snapshots"
        );
        assert_eq!(
            result.snapshots_sent,
            Some(0),
            "dry_run must send 0 snapshots"
        );
        assert_eq!(result.bytes_sent, 0);
        assert_eq!(result.boot, BootStep::NotSelected);

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
        assert!(
            opts.sources.is_none(),
            "not specified is not an empty selection"
        );
        assert!(opts.targets.is_none());
        assert!(!opts.dry_run);
        assert_eq!(opts.steps, BtrbkSteps::SnapshotAndSend);
        assert!(!opts.boot_archive);
        assert!(!opts.index_after);
        assert!(!opts.email_report);
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
            Some(labels(sources).as_slice()),
            targets.as_deref(),
            &TestProgress::new(),
        )
    }

    /// [`filters_for`] with the sources not specified (`None`: every one).
    fn filters_for_every_source(
        config: &Config,
        targets: Option<&[&str]>,
    ) -> Result<Vec<String>, String> {
        let targets = targets.map(labels);
        btrbk_filters(config, None, targets.as_deref(), &TestProgress::new())
    }

    #[test]
    fn a_selection_of_everything_is_no_filter_at_all() {
        let config = steps_config();
        let every = ["primary-22tb", "recovery"];
        assert_eq!(filters_for_every_source(&config, Some(&every)), Ok(vec![]));
        assert_eq!(filters_for_every_source(&config, None), Ok(vec![]));
        let all = ["nvme-root", "nvme-vm", "hdd"];
        assert_eq!(filters_for(&config, &all, Some(&every)), Ok(vec![]));
        assert_eq!(filters_for(&config, &all, None), Ok(vec![]));
    }

    #[test]
    fn an_unticked_target_is_in_no_filter() {
        let config = steps_config();
        let filters = filters_for_every_source(&config, Some(&["primary-22tb"])).unwrap();
        assert_eq!(filters.join(" "), primary_filters());
        assert!(!filters.iter().any(|f| f.contains(RECOVERY)), "{filters:?}");
        // And the other way round: only the recovery target's directories.
        let filters = filters_for_every_source(&config, Some(&["recovery"])).unwrap();
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
            Some(labels(&["nvme-vm", "hdd"]).as_slice()),
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

    // -- what the send and the full run read from btrbk's own output --

    /// btrbk's lines for two sends with sizes, one without, and noise.
    const SEND_LINES: &str = ">>> /t/a.1 (incremental, 1.0 MiB)\n\
         *** /t/b.1 (full, 2.0 MiB)\n\
         >>> /t/c.1\n\
         not a send line (3.0 MiB)\n";

    #[test]
    fn the_sizes_btrbk_prints_on_its_send_lines_are_added_up_by_a_send_and_a_full_run() {
        let f = primary_filters();
        let send_only = BackupOptions {
            steps: BtrbkSteps::SendOnly,
            ..Default::default()
        };
        let (done, _, _) = pipeline(
            send_only,
            BackupMode::Incremental,
            vec![
                (btrbk(&format!("resume {f}")), 0, SEND_LINES.into()),
                listing_of(String::new()),
            ],
        );
        assert_eq!(
            done.bytes,
            3 * 1_048_576,
            "1 MiB + 2 MiB, the other lines no size"
        );
        let (done, _, _) = pipeline(
            BackupOptions::default(),
            BackupMode::Full,
            vec![
                (btrbk(&format!("run {f}")), 0, SEND_LINES.into()),
                listing_of(String::new()),
            ],
        );
        assert_eq!(done.bytes, 3 * 1_048_576);
    }

    /// The progress lines btrbk (or `pv`) prints, one per unit, a zero rate and
    /// a line that is no rate at all.
    const RATE_LINES: &str =
        "  10.0 KiB/s\n  10.0 MiB/s\n  1.0 GiB/s\n  0.0 MiB/s\n  no rate here\n";

    #[test]
    fn a_send_and_a_full_run_report_each_rate_btrbk_prints_and_never_a_zero_one() {
        let f = primary_filters();
        let want = [10 * 1_024, 10 * 1_048_576, 1_073_741_824];
        let send_only = BackupOptions {
            steps: BtrbkSteps::SendOnly,
            ..Default::default()
        };
        let (_, _, progress) = pipeline(
            send_only,
            BackupMode::Incremental,
            vec![
                (btrbk(&format!("resume {f}")), 0, RATE_LINES.into()),
                listing_of(String::new()),
            ],
        );
        assert_eq!(*progress.throughput.lock().unwrap(), want);
        let (_, _, progress) = pipeline(
            BackupOptions::default(),
            BackupMode::Full,
            vec![
                (btrbk(&format!("run {f}")), 0, RATE_LINES.into()),
                listing_of(String::new()),
            ],
        );
        assert_eq!(*progress.throughput.lock().unwrap(), want);
    }

    #[test]
    fn a_rate_is_read_in_every_unit_btrbk_prints_it_in() {
        for (line, bytes) in [
            ("22.5 MiB/s", 23_592_960),
            ("22.5 MB/s", 23_592_960),
            ("2 GiB/s", 2_147_483_648),
            ("2 GB/s", 2_147_483_648),
            ("3 KiB/s", 3_072),
            ("3 KB/s", 3_072),
            ("5 B/s", 5),
            ("22.5MiB/s", 23_592_960),
            ("nothing", 0),
        ] {
            assert_eq!(parse_throughput_line(line), bytes, "{line:?}");
        }
    }

    #[test]
    fn a_size_is_read_from_the_last_parenthetical_in_every_unit_btrbk_may_use() {
        for (line, bytes) in [
            (">>> /t/a (incremental, 1.5 TiB)", 1_649_267_441_664),
            (">>> /t/a (incremental, 1 TB)", 1_099_511_627_776),
            (">>> /t/a (full, 2 GiB)", 2_147_483_648),
            (">>> /t/a (full, 2 MB)", 2_097_152),
            (">>> /t/a (full, 3 KiB)", 3_072),
            (">>> /t/a (full, 7 B)", 7),
            (">>> /t/a (full, 7 parsecs)", 0),
            (">>> /t/a (no size here)", 0),
            (">>> /t/a with no parenthetical", 0),
            // A `)` before the last `(` is no parenthetical.
            (">>> /t/a ) then (", 0),
        ] {
            assert_eq!(parse_btrbk_size_field(line), bytes, "{line:?}");
        }
    }

    #[test]
    fn used_bytes_is_the_blocks_in_use_times_the_block_size() {
        assert_eq!(used_bytes(1_000, 400, 4_096), 600 * 4_096);
        assert_eq!(used_bytes(1_000, 1_000, 4_096), 0);
        // A reading with more available than total is zero, never a wrapped number.
        assert_eq!(used_bytes(10, 20, 4_096), 0);
    }

    // -- the stage count a run announces --

    #[test]
    fn a_run_announces_one_stage_step_for_each_thing_it_will_do() {
        let announced = |options: BackupOptions| {
            let runner = Scripted::from_owned(vec![]);
            let progress = TestProgress::new();
            let options = BackupOptions {
                dry_run: true,
                targets: Some(labels(&["primary-22tb"])),
                ..options
            };
            run_backup_with(&live_config(), &options, &progress, &env(&runner)).unwrap();
            let stages = progress.stages.lock().unwrap();
            stages
                .iter()
                .find(|(name, _)| name == "Backup")
                .map(|(_, total)| *total)
                .expect("the run announces its stage")
        };
        let with = |f: &dyn Fn(&mut BackupOptions)| {
            let mut options = BackupOptions::default();
            f(&mut options);
            options
        };
        assert_eq!(announced(with(&|_| {})), 3, "snapshot + send + report");
        assert_eq!(announced(with(&|o| o.steps = BtrbkSteps::SendOnly)), 2);
        assert_eq!(announced(with(&|o| o.steps = BtrbkSteps::SnapshotOnly)), 2);
        assert_eq!(announced(with(&|o| o.boot_archive = true)), 4);
        assert_eq!(announced(with(&|o| o.index_after = true)), 4);
        assert_eq!(
            announced(with(&|o| o.email_report = true)),
            3,
            "the report is written whether or not it is emailed"
        );
        assert_eq!(
            announced(with(&|o| {
                o.boot_archive = true;
                o.index_after = true;
                o.email_report = true;
            })),
            5
        );
    }

    #[test]
    fn btrbk_steps_from_the_two_ticks_and_nothing_ticked_is_refused() {
        assert_eq!(
            BtrbkSteps::from_ticks(true, true),
            Ok(BtrbkSteps::SnapshotAndSend)
        );
        assert_eq!(
            BtrbkSteps::from_ticks(true, false),
            Ok(BtrbkSteps::SnapshotOnly)
        );
        assert_eq!(
            BtrbkSteps::from_ticks(false, true),
            Ok(BtrbkSteps::SendOnly)
        );
        let why = BtrbkSteps::from_ticks(false, false).unwrap_err();
        assert!(why.contains("neither Snapshot nor Send"), "{why}");
        assert!(BtrbkSteps::SnapshotOnly.snapshots() && !BtrbkSteps::SnapshotOnly.sends());
        assert!(!BtrbkSteps::SendOnly.snapshots() && BtrbkSteps::SendOnly.sends());
        assert!(BtrbkSteps::SnapshotAndSend.snapshots() && BtrbkSteps::SnapshotAndSend.sends());
    }

    fn entries(pairs: &[(&str, Option<bool>)]) -> Vec<(String, Option<bool>)> {
        pairs.iter().map(|(k, v)| ((*k).to_string(), *v)).collect()
    }

    #[test]
    fn run_steps_reads_every_key_and_each_one_reaches_its_own_field() {
        let all = [
            ("snapshot", true),
            ("send", true),
            ("boot_archive", true),
            ("index", true),
            ("email", true),
        ];
        // Flip one key at a time: a swapped key (index <-> email) fails here.
        for flipped in ["boot_archive", "index", "email"] {
            let pairs: Vec<_> = all
                .iter()
                .map(|(k, v)| (*k, Some(if *k == flipped { !v } else { *v })))
                .collect();
            let s = RunSteps::from_entries(entries(&pairs)).unwrap();
            assert_eq!(s.boot_archive, flipped != "boot_archive", "{flipped}");
            assert_eq!(s.index, flipped != "index", "{flipped}");
            assert_eq!(s.email, flipped != "email", "{flipped}");
            assert_eq!(s.btrbk, BtrbkSteps::SnapshotAndSend);
        }
    }

    #[test]
    fn run_steps_refuses_a_missing_unknown_or_non_boolean_key_and_never_defaults() {
        let full = [
            ("snapshot", Some(true)),
            ("send", Some(true)),
            ("boot_archive", Some(true)),
            ("index", Some(true)),
            ("email", Some(true)),
        ];
        for missing in RUN_STEP_KEYS {
            let pairs: Vec<_> = full
                .iter()
                .copied()
                .filter(|(k, _)| *k != missing)
                .collect();
            let why = RunSteps::from_entries(entries(&pairs)).unwrap_err();
            assert!(why.contains(missing) && why.contains("not given"), "{why}");
        }
        let mut extra = full.to_vec();
        extra.push(("preserve", Some(true)));
        assert!(
            RunSteps::from_entries(entries(&extra))
                .unwrap_err()
                .contains("unknown step 'preserve'")
        );
        let mut not_bool = full.to_vec();
        not_bool[3] = ("index", None);
        assert!(
            RunSteps::from_entries(entries(&not_bool))
                .unwrap_err()
                .contains("'index' is not a boolean")
        );
        assert!(
            RunSteps::from_entries(Vec::new()).is_err(),
            "an empty dictionary is refused"
        );
        let mut neither = full.to_vec();
        neither[0] = ("snapshot", Some(false));
        neither[1] = ("send", Some(false));
        assert!(
            RunSteps::from_entries(entries(&neither))
                .unwrap_err()
                .contains("neither Snapshot nor Send")
        );
    }

    #[test]
    fn run_steps_apply_puts_each_step_into_its_own_option() {
        let mut options = BackupOptions::default();
        RunSteps {
            btrbk: BtrbkSteps::SendOnly,
            boot_archive: true,
            index: false,
            email: true,
        }
        .apply(&mut options);
        assert_eq!(options.steps, BtrbkSteps::SendOnly);
        assert!(options.boot_archive && !options.index_after && options.email_report);
    }

    #[test]
    fn every_mode_and_step_choice_makes_exactly_its_btrbk_calls() {
        use BackupMode::{Full, Incremental};
        use BtrbkSteps::{SendOnly, SnapshotAndSend, SnapshotOnly};
        let f = primary_filters();
        let run = btrbk(&format!("run {f}"));
        let snap = btrbk(&format!("snapshot {f}"));
        let resume = btrbk(&format!("resume {f}"));
        let cases = [
            (Full, SnapshotAndSend, vec![run.clone()], (1, 1)),
            (Full, SnapshotOnly, vec![snap.clone()], (1, 0)),
            (Full, SendOnly, vec![resume.clone()], (0, 1)),
            (
                Incremental,
                SnapshotAndSend,
                vec![snap.clone(), resume.clone()],
                (1, 1),
            ),
            (Incremental, SnapshotOnly, vec![snap.clone()], (1, 0)),
            (Incremental, SendOnly, vec![resume.clone()], (0, 1)),
        ];
        for (mode, steps, want, (created, sent)) in cases {
            let (done, calls, _) = pipeline(
                BackupOptions {
                    steps,
                    ..Default::default()
                },
                mode,
                vec![
                    (run.clone(), 0, String::new()),
                    (snap.clone(), 0, String::new()),
                    (resume.clone(), 0, String::new()),
                    listing_of(raw_row("/s/a.1", "/t1/a.1")),
                ],
            );
            // The listings btrbk is asked for between steps are not steps.
            let btrbk_calls: Vec<_> = calls
                .iter()
                .filter(|c| !c.contains(" list "))
                .cloned()
                .collect();
            assert_eq!(btrbk_calls, want, "{mode} {steps:?}: {calls:?}");
            assert_eq!(
                (done.created, done.sent),
                (Some(created), Some(sent)),
                "{mode} {steps:?}"
            );
        }
    }

    #[test]
    fn a_dry_run_names_only_the_btrbk_steps_it_would_take() {
        let log_of = |steps: BtrbkSteps| {
            let runner = Scripted::from_owned(vec![]);
            let progress = TestProgress::new();
            let options = BackupOptions {
                dry_run: true,
                steps,
                targets: Some(labels(&["primary-22tb"])),
                ..Default::default()
            };
            run_backup_with(&live_config(), &options, &progress, &env(&runner)).unwrap();
            let logs = progress.logs.lock().unwrap();
            logs.iter()
                .map(|(_, m)| m.clone())
                .collect::<Vec<_>>()
                .join("\n")
        };
        let send = log_of(BtrbkSteps::SendOnly);
        assert!(send.contains("would send to targets"), "{send}");
        assert!(!send.contains("would create snapshots"), "{send}");
        let snap = log_of(BtrbkSteps::SnapshotOnly);
        assert!(snap.contains("would create snapshots"), "{snap}");
        assert!(!snap.contains("would send to targets"), "{snap}");
        let both = log_of(BtrbkSteps::SnapshotAndSend);
        assert!(both.contains("would create snapshots") && both.contains("would send to targets"));
        assert!(
            both.contains("would save the report without emailing it"),
            "{both}"
        );
    }

    // -- the index step --

    fn walked() -> Result<indexer::WalkResult, Box<dyn std::error::Error>> {
        Ok(indexer::WalkResult {
            snapshots_discovered: 2,
            snapshots_indexed: 2,
            snapshots_skipped: 0,
            presence_recorded: 0,
            results: Vec::new(),
        })
    }

    fn indexing_config(dir: &Path) -> Config {
        let mut config = live_config();
        config.general.db_path = dir.join("index.db").to_string_lossy().into_owned();
        config
    }

    #[test]
    fn the_index_step_walks_each_mounted_target_and_says_whether_any_was_indexed() {
        let dir = tempfile::tempdir().unwrap();
        let config = indexing_config(dir.path());
        let walked_paths = std::sync::Mutex::new(Vec::<String>::new());
        let progress = TestProgress::new();
        // Only the first target is mounted.
        let indexed = index_targets_with(
            &config,
            &progress,
            &|t| (t.label == "primary-22tb").then(|| "/mnt/primary".to_string()),
            &|path, _| {
                walked_paths
                    .lock()
                    .unwrap()
                    .push(path.display().to_string());
                walked()
            },
        );
        assert!(indexed);
        assert_eq!(*walked_paths.lock().unwrap(), ["/mnt/primary"]);
        assert!(logged(
            &progress,
            LogLevel::Info,
            "Indexed 'primary-22tb': 2 new snapshots (0 files)"
        ));

        // Nothing mounted: nothing walked, nothing indexed.
        walked_paths.lock().unwrap().clear();
        let indexed = index_targets_with(&config, &TestProgress::new(), &|_| None, &|path, _| {
            walked_paths
                .lock()
                .unwrap()
                .push(path.display().to_string());
            walked()
        });
        assert!(!indexed);
        assert!(walked_paths.lock().unwrap().is_empty());
    }

    #[test]
    fn a_target_whose_walk_fails_is_a_warning_and_indexes_nothing_but_the_others_still_count() {
        let dir = tempfile::tempdir().unwrap();
        let config = indexing_config(dir.path());
        let progress = TestProgress::new();
        let indexed = index_targets_with(
            &config,
            &progress,
            &|t| Some(format!("/mnt/{}", t.label)),
            &|path, _| {
                if path.ends_with("recovery") {
                    walked()
                } else {
                    Err("walk failed".into())
                }
            },
        );
        assert!(indexed, "one target was indexed");
        assert!(logged(
            &progress,
            LogLevel::Warning,
            "Indexing target 'primary-22tb' failed (non-fatal): walk failed"
        ));

        let progress = TestProgress::new();
        let indexed = index_targets_with(
            &config,
            &progress,
            &|t| Some(format!("/mnt/{}", t.label)),
            &|_, _| Err("walk failed".into()),
        );
        assert!(!indexed, "every walk failed");
    }

    #[test]
    fn an_index_that_cannot_be_opened_is_a_warning_and_nothing_is_walked() {
        let mut config = live_config();
        config.general.db_path = "/nonexistent-dir/index.db".into();
        let progress = TestProgress::new();
        let walks = std::cell::Cell::new(0);
        let indexed = index_targets_with(
            &config,
            &progress,
            &|_| Some("/mnt/x".to_string()),
            &|_, _| {
                walks.set(walks.get() + 1);
                walked()
            },
        );
        assert!(!indexed);
        assert_eq!(walks.get(), 0);
        assert!(
            progress
                .logs
                .lock()
                .unwrap()
                .iter()
                .any(|(l, m)| *l == LogLevel::Warning
                    && m.starts_with("Cannot open index DB for post-backup indexing (non-fatal)")),
            "{:?}",
            progress.logs.lock().unwrap()
        );
    }

    #[test]
    fn a_run_indexes_only_when_asked_and_reports_what_the_step_found() {
        let f = "/proc/self/root /proc/self/home";
        let at = |args: &str| btrbk_at("/nonexistent/btrbk.conf", args);
        let script = || {
            vec![
                (at(&format!("snapshot {f}")), 0, String::new()),
                (at(&format!("resume {f}")), 0, String::new()),
                (
                    at(&format!("--format=raw list latest {f}")),
                    0,
                    String::new(),
                ),
            ]
        };
        let run = |index_after: bool, steps_find: bool| {
            let runner = Scripted::from_owned(script());
            let asked = std::cell::Cell::new(0);
            let index = |_: &Config, _: &dyn ProgressCallback| {
                asked.set(asked.get() + 1);
                steps_find
            };
            let nvme_mounted = |p: &Path| p == Path::new("/.btrfs-nvme");
            let host = StepEnv {
                is_mountpoint: &nvme_mounted,
                index: &index,
                ..env(&runner)
            };
            let options = BackupOptions {
                index_after,
                ..live_options()
            };
            let result =
                run_backup_with(&live_config(), &options, &TestProgress::new(), &host).unwrap();
            (result.indexed, asked.get())
        };
        assert_eq!(run(true, true), (true, 1));
        assert_eq!(run(true, false), (false, 1));
        assert_eq!(run(false, true), (false, 0), "not asked for: not run");
    }

    #[test]
    fn a_source_with_nothing_declared_is_not_said_to_send_nowhere() {
        // `hdd` has only a retired subvolume: it has no block in btrbk.conf, so
        // there is nothing to say about it. `nvme-vm` is declared, sends to the
        // primary target only, and is said.
        let mut config = steps_config();
        config.sources[2].subvolumes[0].retired = Some("2026-09-01".into());
        let progress = TestProgress::new();
        let filters = btrbk_filters(
            &config,
            Some(labels(&["nvme-root", "nvme-vm", "hdd"]).as_slice()),
            Some(&labels(&["recovery"])),
            &progress,
        )
        .unwrap();
        assert_eq!(
            filters,
            [
                format!("{RECOVERY}/nvme-root/root"),
                format!("{RECOVERY}/nvme-root/home")
            ]
        );
        let warnings: Vec<String> = progress
            .logs
            .lock()
            .unwrap()
            .iter()
            .filter(|(l, _)| *l == LogLevel::Warning)
            .map(|(_, m)| m.clone())
            .collect();
        assert_eq!(
            warnings,
            ["Source 'nvme-vm' sends to none of the selected targets — nothing is done for it"]
        );
    }

    #[test]
    fn each_source_snapshotted_is_logged_by_its_own_label_and_volume() {
        let config = steps_config();
        let runner = Scripted::from_owned(vec![
            (
                btrbk(&format!("snapshot {PRIMARY}/hdd/data")),
                0,
                String::new(),
            ),
            (
                btrbk(&format!("--format=raw list latest {PRIMARY}/hdd/data")),
                0,
                String::new(),
            ),
        ]);
        let progress = TestProgress::new();
        create_snapshots_with(
            &config,
            Some(labels(&["hdd"]).as_slice()),
            Some(&labels(&["primary-22tb"])),
            &progress,
            &runner,
        )
        .unwrap();
        let said: Vec<String> = progress
            .logs
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, m)| m.starts_with("Snapshotting source"))
            .map(|(_, m)| m.clone())
            .collect();
        assert_eq!(said, ["Snapshotting source 'hdd' at /.btrfs-hdd"]);
    }

    #[test]
    fn nothing_selected_is_an_error_and_never_an_empty_filter_list() {
        let config = steps_config();
        // btrbk reads an empty filter list as everything.
        for result in [
            filters_for(&config, &["nvme-vm"], Some(&["recovery"])),
            filters_for_every_source(&config, Some(&[])),
        ] {
            let err = result.unwrap_err();
            assert!(err.contains("there is nothing to run"), "{err}");
        }
        // A source list that was given and is empty is not "every source".
        let none: &[String] = &[];
        let err = btrbk_filters(&config, Some(none), None, &TestProgress::new()).unwrap_err();
        assert!(err.contains("No source selected"), "{err}");
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
                    Some(std::slice::from_ref(&source.label)),
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
            Some(labels(&["nvme-root", "nvme-vm", "hdd"]).as_slice()),
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
            Some(labels(&["nvme-root"]).as_slice()),
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
            None,
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
            Some(labels(&["typo"]).as_slice()),
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
        let err = create_snapshots_with(&steps_config(), None, None, &progress, &runner)
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
        let count = create_snapshots_with(&steps_config(), None, None, &progress, &runner).unwrap();
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
        assert!(create_snapshots_with(&config, None, None, &progress, &Unspawnable).is_err());
        assert!(
            send_snapshots_with(
                &config,
                None,
                &primary,
                false,
                &progress,
                &env(&Unspawnable)
            )
            .is_err()
        );
        assert!(
            run_full_pipeline_with(&config, None, &primary, &progress, &env(&Unspawnable)).is_err()
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
                None,
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
            None,
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
            Some(labels(&["hdd", "nvme-root"]).as_slice()),
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
            None,
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
            None,
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
            None,
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
            None,
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
            None,
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
            None,
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
            Some(labels(&["nvme-root", "nvme-vm", "hdd"]).as_slice()),
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
            Some(labels(&["hdd"]).as_slice()),
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
            None,
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
            None,
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
            &labels(&["nvme-root", "nvme-vm", "hdd"]),
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
                created: Some(2),
                sent: Some(2),
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
                steps: BtrbkSteps::SendOnly,
                ..Default::default()
            },
            BackupMode::Incremental,
            script(),
        );
        assert_eq!(calls[0], btrbk(&format!("resume {f}")));
        assert_eq!(calls.len(), 2, "{calls:?}");
        assert_eq!((done.created, done.sent), (Some(0), Some(1)));

        let (done, calls, _) = pipeline(
            BackupOptions {
                steps: BtrbkSteps::SnapshotOnly,
                ..Default::default()
            },
            BackupMode::Incremental,
            script(),
        );
        assert_eq!(calls[0], btrbk(&format!("snapshot {f}")));
        assert_eq!(calls.len(), 2, "{calls:?}");
        assert_eq!((done.created, done.sent), (Some(1), Some(0)));
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
                created: Some(1),
                sent: Some(1),
                cleaned: 1,
                bytes: 0,
                errors: vec![]
            }
        );
        let (done, calls, _) = pipeline(
            BackupOptions {
                steps: BtrbkSteps::SnapshotOnly,
                ..Default::default()
            },
            BackupMode::Full,
            script(),
        );
        assert_eq!(calls[0], btrbk(&format!("snapshot {f}")));
        assert_eq!(
            (done.created, done.sent, done.cleaned),
            (Some(1), Some(0), 0)
        );
        let (done, calls, _) = pipeline(
            BackupOptions {
                steps: BtrbkSteps::SendOnly,
                ..Default::default()
            },
            BackupMode::Full,
            script(),
        );
        assert_eq!(calls[0], btrbk(&format!("resume {f}")));
        assert_eq!(
            (done.created, done.sent, done.cleaned),
            (Some(0), Some(1), 0)
        );
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
        assert_eq!(
            (done.created, done.sent),
            (None, Some(1)),
            "the snapshot step failed, so its count is unknown, not 0; the send's is real"
        );
        assert!(logged(&progress, LogLevel::Error, &done.errors[0]));

        let (done, _, _) = pipeline(
            BackupOptions::default(),
            BackupMode::Incremental,
            vec![(btrbk(&format!("snapshot {f}")), 0, String::new())],
        );
        assert_eq!(
            (done.created, done.sent),
            (Some(0), None),
            "the send step failed: its count is unknown, not 0 ({:?})",
            done.errors
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
        assert_eq!(
            (done.created, done.sent),
            (None, None),
            "one btrbk run did both; nothing it did is known"
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
            None,
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
            None,
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
            None,
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
        send_snapshots_with(&config, None, &primary, false, &TestProgress::new(), &env).unwrap();
        run_full_pipeline_with(&config, None, &primary, &TestProgress::new(), &env).unwrap();
        let both = labels(&["primary-22tb", "recovery"]);
        assert_eq!(
            *asked.lock().unwrap(),
            [(both.clone(), primary.clone()), (both, primary)],
            "all the targets are passed, and the labels that will be written"
        );
        assert_eq!(runner.calls().len(), 4, "{:?}", runner.calls());
    }

    /// bd DAS-Backup-Manager-7tx (review I2): a verifier that fails only for
    /// the SECOND target. Verifying just the first target of two selected
    /// would pass every other test here; this one pins that every target
    /// written is verified, in send and in the full run.
    #[test]
    fn send_full_and_run_refuse_when_only_the_second_selected_target_fails_verification() {
        let verify =
            |_: &[Target], write: &[String], _: &dyn ProgressCallback| -> Result<(), String> {
                if write.iter().any(|l| l == "recovery") {
                    Err("recovery is not mounted".to_string())
                } else {
                    Ok(())
                }
            };
        let config = steps_config();
        let both = labels(&["primary-22tb", "recovery"]);
        let runner = Scripted::from_owned(vec![]);
        let env = env_for(&runner, &verify);
        let err = send_snapshots_with(&config, None, &both, false, &TestProgress::new(), &env)
            .unwrap_err()
            .to_string();
        assert_eq!(err, "recovery is not mounted");
        let err = run_full_pipeline_with(&config, None, &both, &TestProgress::new(), &env)
            .unwrap_err()
            .to_string();
        assert_eq!(err, "recovery is not mounted");
        assert!(runner.calls().is_empty(), "{:?}", runner.calls());

        // `run` verifies every selected target too (a dry run reaches the check).
        let options = BackupOptions {
            dry_run: true,
            targets: Some(both.clone()),
            ..Default::default()
        };
        let err = run_backup_with(
            &live_config(),
            &options,
            &TestProgress::new(),
            &env_for(&runner, &verify),
        )
        .unwrap_err()
        .to_string();
        assert_eq!(err, "recovery is not mounted");
        assert!(runner.calls().is_empty(), "{:?}", runner.calls());

        // The control: the same verifier lets a selection of the first target
        // alone through to btrbk.
        let filters = primary_filters();
        let runner = Scripted::from_owned(vec![
            (btrbk(&format!("resume {filters}")), 0, String::new()),
            (btrbk(&format!("run {filters}")), 0, String::new()),
        ]);
        let env = env_for(&runner, &verify);
        let primary = labels(&["primary-22tb"]);
        send_snapshots_with(&config, None, &primary, false, &TestProgress::new(), &env).unwrap();
        run_full_pipeline_with(&config, None, &primary, &TestProgress::new(), &env).unwrap();
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
        let err = send_snapshots_with(&config, None, &every, false, &TestProgress::new(), &env)
            .unwrap_err()
            .to_string();
        assert_eq!(err, "recovery is on the wrong disk");
        let err = run_full_pipeline_with(&config, None, &every, &TestProgress::new(), &env)
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
        // And the usage it reads is the real one: the root filesystem of the
        // host has bytes in use, no targets have none.
        let mut config = make_test_config();
        config.targets[0].mount = "/".into();
        assert!((StepEnv::HOST.usage)(&config, &TestProgress::new()) > 0);
        config.targets.clear();
        assert_eq!((StepEnv::HOST.usage)(&config, &TestProgress::new()), 0);
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
            targets: Some(labels(&["primary-22tb"])),
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
        let step = archive_boot_with(
            &config,
            None,
            true,
            &TestProgress::new(),
            &env_verifying(&runner),
        );
        let BootStep::Ran(o) = &step else {
            panic!("{step:?}")
        };
        assert_eq!(o.status(), "FAIL", "{step:?}");
        let err = o.failures.join("\n");
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
            true,
            &TestProgress::new(),
            &env_for(&runner, &verify),
        );
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

    /// Review M6: the loop walks the targets that were verified, not every
    /// selected one. A selected target whose mount point does not exist was
    /// never verified, so btrfs must not be asked anything about it.
    #[test]
    fn boot_archive_walks_only_the_verified_targets_not_every_selected_one() {
        let primary = tempfile::tempdir().unwrap();
        let (mut config, _conf) = archive_fixture(primary.path());
        config.targets.push(another_target(
            &config,
            "absent",
            Path::new("/nonexistent/das/absent"),
            TargetRole::Primary,
        ));
        let runner = Scripted::from_owned(vec![]);
        let progress = TestProgress::new();
        archive_boot_with(&config, None, true, &progress, &env(&runner));
        let calls = runner.calls();
        let on = |mount: &str| calls.iter().filter(|c| c.contains(mount)).count();
        assert!(
            on(&primary.path().to_string_lossy()) > 0,
            "the verified target is the one walked: {calls:?}"
        );
        assert_eq!(on("/nonexistent/das/absent"), 0, "{calls:?}");
        let logs = progress.logs.lock().unwrap();
        assert!(
            logs.iter().any(|(level, msg)| *level == LogLevel::Info
                && msg.contains("/nonexistent/das/absent")
                && msg.contains("Not mounted")),
            "{logs:?}"
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
        archive_boot_with(
            &config,
            Some(&only_second),
            true,
            &TestProgress::new(),
            &env,
        );
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
            true,
            &TestProgress::new(),
            &env_for(&runner, &verify),
        );
        assert_eq!(runner.calls().len(), 2, "{:?}", runner.calls());
    }

    /// `env` for a closure `verify` that is a variable of the test.
    fn env_for<'a>(runner: &'a dyn CommandRunner, verify: &'a VerifyTargets<'a>) -> StepEnv<'a> {
        StepEnv {
            runner,
            verify,
            is_mountpoint: &|_| true,
            usage: &|_, _| 0,
            index: &|_, _| false,
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
        let step = archive_boot_with(&config, None, true, &progress, &env(&runner));
        assert!(
            matches!(&step, BootStep::Ran(o) if o.updated == 1 && o.status() == "OK"),
            "{step:?}"
        );
        assert!(step.archived());

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
        let archive_name = Path::new(archive).file_name().unwrap().to_string_lossy();
        assert!(logged(
            &progress,
            LogLevel::Info,
            &format!(
                "Recreated {m}/@ from nvme/root-.20261005T0100 (archived old @ -> {archive_name})"
            )
        ));
        // One target: started at 0 of the 1 it names, done at 1.
        assert_eq!(
            *progress.steps.lock().unwrap(),
            [
                (0, 1, "Boot subvolumes on primary-22tb".to_string()),
                (1, 1, "Boot subvolumes done".to_string())
            ]
        );
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
                _ => {
                    script.push((format!("btrfs subvolume delete {m}/@"), 1, String::new()));
                    // The replacement's discard works.
                    script.push((
                        format!("btrfs subvolume delete {m}/@.new"),
                        0,
                        String::new(),
                    ));
                }
            }
            let mut runner = Scripted::from_owned(script).snapshotting();
            if failing == "archive" {
                runner = runner.failing_snapshots_of(&format!("{m}/@ "));
            }
            let progress = TestProgress::new();
            let step = archive_boot_with(&config, None, true, &progress, &env(&runner));
            assert!(
                matches!(&step, BootStep::Ran(o) if o.status() == "FAIL" && o.updated == 0),
                "{failing}: {step:?}"
            );
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
                assert!(
                    !progress
                        .logs
                        .lock()
                        .unwrap()
                        .iter()
                        .any(|(_, msg)| msg.contains("could not be discarded")),
                    "a discard that worked is not reported as failed: {:?}",
                    progress.logs.lock().unwrap()
                );
            }
        }
    }

    #[test]
    fn a_replacement_that_cannot_be_discarded_after_a_failed_delete_is_warned_about() {
        let dir = tempfile::tempdir().unwrap();
        let m = dir.path().display().to_string();
        std::fs::create_dir(dir.path().join("@")).unwrap();
        let (config, _conf) = archive_fixture(dir.path());
        let runner = Scripted::from_owned(vec![
            (
                format!("btrfs subvolume list {m}"),
                0,
                "ID 257 gen 9 top level 5 path nvme/root-.20261005T0100\n".to_string(),
            ),
            (format!("btrfs subvolume delete {m}/@"), 1, String::new()),
            (
                format!("btrfs subvolume delete {m}/@.new"),
                1,
                String::new(),
            ),
        ])
        .snapshotting();
        let progress = TestProgress::new();
        let step = archive_boot_with(&config, None, true, &progress, &env(&runner));
        assert!(
            matches!(&step, BootStep::Ran(o) if o.status() == "FAIL"),
            "{step:?}"
        );
        assert!(
            progress
                .logs
                .lock()
                .unwrap()
                .iter()
                .any(|(_, msg)| msg.contains("could not be discarded")),
            "{:?}",
            progress.logs.lock().unwrap()
        );
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
        let step = archive_boot_with(&config, None, true, &progress, &env(&runner));
        assert!(
            matches!(&step, BootStep::Ran(o) if o.status() == "FAIL"),
            "{step:?}"
        );
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
        let step = archive_boot_with(&config, None, true, &progress, &env(&runner));
        assert!(
            matches!(&step, BootStep::Ran(o) if o.status() == "FAIL" && o.updated == 0),
            "{step:?}"
        );
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

    #[test]
    fn boot_outcome_status_and_detail_are_the_scripts_words() {
        let mut o = BootOutcome {
            updated: 1,
            skipped: 2,
            ..Default::default()
        };
        assert_eq!(
            (o.status(), o.detail()),
            ("OK", "1 updated, 2 skipped".to_string())
        );
        o.warnings.push("w".into());
        assert_eq!(
            (o.status(), o.detail()),
            ("WARN", "1 updated, 2 skipped, 1 warnings".to_string())
        );
        o.failures.push("f".into());
        assert_eq!(
            (o.status(), o.detail()),
            ("FAIL", "1 updated, 1 failed".to_string())
        );
    }

    #[test]
    fn an_incremental_run_creates_a_missing_boot_subvolume_and_never_replaces_one() {
        let dir = tempfile::tempdir().unwrap();
        let m = dir.path().display().to_string();
        let (config, _conf) = archive_fixture(dir.path());
        let list = (
            format!("btrfs subvolume list {m}"),
            0,
            "ID 257 gen 9 top level 5 path nvme/root-.20261005T0100\n".to_string(),
        );
        // Absent: created from the newest snapshot.
        let runner = Scripted::from_owned(vec![list.clone()]).snapshotting();
        let step = archive_boot_with(&config, None, false, &TestProgress::new(), &env(&runner));
        assert_eq!(
            runner.calls(),
            [
                format!("btrfs subvolume list {m}"),
                format!("btrfs subvolume snapshot {m}/nvme/root-.20261005T0100 {m}/@")
            ]
        );
        assert!(
            matches!(&step, BootStep::Ran(o) if o.updated == 1 && o.failures.is_empty()),
            "{step:?}"
        );
        // Present: skipped, nothing archived, nothing deleted.
        std::fs::create_dir_all(dir.path().join("@")).unwrap();
        let runner = Scripted::from_owned(vec![list]).snapshotting();
        let step = archive_boot_with(&config, None, false, &TestProgress::new(), &env(&runner));
        assert_eq!(runner.calls(), [format!("btrfs subvolume list {m}")]);
        assert!(
            matches!(&step, BootStep::Ran(o) if o.skipped == 1 && o.updated == 0),
            "{step:?}"
        );
    }

    #[test]
    fn a_disabled_boot_step_runs_nothing_and_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let (mut config, _conf) = archive_fixture(dir.path());
        config.boot.enabled = false;
        let runner = Scripted::from_owned(vec![]);
        assert_eq!(
            archive_boot_with(&config, None, true, &TestProgress::new(), &env(&runner)),
            BootStep::DisabledInConfig
        );
        assert!(runner.calls().is_empty());
        assert_eq!(BootStep::DisabledInConfig.row(), "OK  (disabled in config)");
        assert_eq!(BootStep::NotSelected.row(), "N/A  (not selected)");
    }

    /// btrbk always succeeds with no output (the boot step is under test, not
    /// the snapshot step); every other command goes to the script.
    /// With `.1` set, every `btrfs` other than the listing cannot be spawned.
    struct QuietBtrbk<'a>(&'a Scripted, bool);

    impl CommandRunner for QuietBtrbk<'_> {
        fn output(&self, cmd: &mut Command) -> std::io::Result<std::process::Output> {
            if cmd.get_program() == "btrbk" {
                return Command::new("true").output();
            }
            if self.1
                && cmd.get_program() == "btrfs"
                && cmd.get_args().next().is_some_and(|a| {
                    a != "subvolume" || cmd.get_args().nth(1).is_some_and(|b| b != "list")
                })
            {
                return Err(std::io::Error::from(std::io::ErrorKind::NotFound));
            }
            self.0.output(cmd)
        }
        fn stream(
            &self,
            cmd: &mut Command,
            _: &mut dyn FnMut(&str),
        ) -> std::io::Result<std::process::Output> {
            self.output(cmd)
        }
    }

    /// The classification table of `.claude/rules/backup.md` §Boot Subvolume
    /// Archival, driven through `run_backup_with`: for every FAIL the run
    /// fails, for every WARN (an absence) it does not, and the counts are the
    /// script's. One row per situation; do not shorten.
    #[test]
    fn every_failure_of_the_boot_step_fails_the_run_and_every_absence_only_warns() {
        const SNAP: &str = "nvme/root-.20261005T0100";
        // (case, mode, status, updated, skipped)
        let cases: [(&str, BackupMode, &str, usize, usize); 15] = [
            ("conf-unreadable", BackupMode::Full, "FAIL", 0, 0),
            ("listing-unreadable", BackupMode::Full, "FAIL", 0, 0),
            ("archive-fails", BackupMode::Full, "FAIL", 0, 0),
            ("stale-staging-stuck", BackupMode::Full, "FAIL", 0, 0),
            ("staging-fails", BackupMode::Full, "FAIL", 0, 0),
            ("delete-fails", BackupMode::Full, "FAIL", 0, 0),
            ("rename-fails", BackupMode::Full, "FAIL", 0, 0),
            ("no-snapshot-name", BackupMode::Full, "WARN", 0, 0),
            ("no-target-subdirs", BackupMode::Full, "WARN", 0, 0),
            ("no-snapshot-of-series", BackupMode::Full, "WARN", 0, 0),
            // Creating an absent one fails, in either mode.
            ("absent-create-fails-full", BackupMode::Full, "FAIL", 0, 0),
            (
                "absent-create-fails-incremental",
                BackupMode::Incremental,
                "FAIL",
                0,
                0,
            ),
            // `btrfs` cannot be run at all (after the listing was read).
            ("btrfs-unspawnable", BackupMode::Full, "FAIL", 0, 0),
            // An incremental run leaves an existing boot subvolume alone.
            ("incremental-exists", BackupMode::Incremental, "OK", 0, 1),
            // The control: nothing wrong, so nothing is flagged.
            ("replaced-cleanly", BackupMode::Full, "OK", 1, 0),
        ];
        for (case, mode, status, updated, skipped) in cases {
            let dir = tempfile::tempdir().unwrap();
            let m = dir.path().display().to_string();
            let live = dir.path().join("@");
            std::fs::create_dir(&live).unwrap();
            std::fs::write(live.join("marker"), b"live").unwrap();
            let (mut config, mut conf) = archive_fixture(dir.path());
            let mut script = vec![(
                format!("btrfs subvolume list {m}"),
                0,
                format!("ID 257 gen 9 top level 5 path {SNAP}\n"),
            )];
            let stage = format!("btrfs subvolume snapshot {m}/{SNAP} {m}/@.new");
            let mut failing = None;
            match case {
                "conf-unreadable" => {
                    config.general.btrbk_conf = "/nonexistent/btrbk.conf".into();
                }
                "listing-unreadable" => script[0].1 = 1,
                "archive-fails" => failing = Some(format!("{m}/@ ")),
                "stale-staging-stuck" => {
                    std::fs::create_dir(dir.path().join("@.new")).unwrap();
                    script.push((
                        format!("btrfs subvolume delete {m}/@.new"),
                        1,
                        String::new(),
                    ));
                }
                "staging-fails" => script.push((stage, 1, String::new())),
                "delete-fails" => {
                    script.push((format!("btrfs subvolume delete {m}/@"), 1, String::new()));
                }
                // The scripted delete removes nothing, so the live one is
                // still there and the rename onto it fails.
                "rename-fails" => {
                    script.push((format!("btrfs subvolume delete {m}/@"), 0, String::new()));
                }
                "no-snapshot-name" => conf = write_btrbk_conf("@home", "home"),
                "no-target-subdirs" => config.sources[0].target_subdirs.clear(),
                "no-snapshot-of-series" => {
                    script[0].2 = "ID 9 gen 1 top level 5 path other/x\n".into()
                }
                "absent-create-fails-full" | "absent-create-fails-incremental" => {
                    std::fs::remove_dir_all(&live).unwrap();
                    script.push((
                        format!("btrfs subvolume snapshot {m}/{SNAP} {m}/@"),
                        1,
                        String::new(),
                    ));
                }
                "replaced-cleanly" | "incremental-exists" | "btrfs-unspawnable" => {}
                other => panic!("{other}"),
            }
            if case == "no-snapshot-name" {
                config.general.btrbk_conf = conf.path().to_string_lossy().into_owned();
            }
            let mut scripted = Scripted::from_owned(script).snapshotting();
            if let Some(f) = &failing {
                scripted = scripted.failing_snapshots_of(f);
            }
            if case == "replaced-cleanly" {
                scripted = scripted.deleting_too(vec![format!("{m}/@")]);
            }
            let runner = QuietBtrbk(&scripted, case == "btrfs-unspawnable");
            let host = env(&runner);
            let options = BackupOptions {
                mode: Some(mode),
                steps: BtrbkSteps::SnapshotOnly,
                boot_archive: true,
                targets: Some(labels(&["primary-22tb"])),
                ..Default::default()
            };
            let mut result =
                run_backup_with(&config, &options, &TestProgress::new(), &host).unwrap();
            // The target-directory step asks the host whether the target is a
            // mount point, and a tempdir is not one: that complaint is about the
            // fixture, not the boot step, so it is set aside.
            result
                .errors
                .retain(|e| !e.starts_with("Target directories missing"));
            result.success = result.errors.is_empty();
            let BootStep::Ran(o) = &result.boot else {
                panic!("{case}: {:?}", result.boot)
            };
            assert_eq!(
                (o.status(), o.updated, o.skipped),
                (status, updated, skipped),
                "{case}: {o:?}"
            );
            let boot_errors = result
                .errors
                .iter()
                .filter(|e| e.starts_with(BOOT_ERROR_PREFIX))
                .count();
            assert_eq!(boot_errors, o.failures.len(), "{case}: {:?}", result.errors);
            // The run fails exactly for a FAIL, and for nothing else here.
            assert_eq!(
                result.success,
                status != "FAIL",
                "{case}: {:?}",
                result.errors
            );
            if status != "FAIL" {
                assert!(result.errors.is_empty(), "{case}: {:?}", result.errors);
            }
            let absent = case.starts_with("absent-");
            // Whatever failed, the live subvolume (or its replacement, on a
            // clean run) is still there: nothing leaves `@` absent. (Where it
            // was absent to begin with, a failed create leaves it absent.)
            assert_eq!(
                live.is_dir(),
                !absent,
                "{case}: the live subvolume survives"
            );
            if case != "replaced-cleanly" && !absent {
                assert!(live.join("marker").exists(), "{case}: and it is untouched");
            }
            if case == "incremental-exists" {
                assert!(
                    !scripted
                        .calls()
                        .iter()
                        .any(|c| c.contains("archive") || c.starts_with("btrfs subvolume delete")),
                    "nothing archived or deleted: {:?}",
                    scripted.calls()
                );
            }
        }
    }

    /// A stat that fails is a FAIL and nothing is mutated — never "absent".
    /// Live: `@` is a symlink to itself, so its stat fails with ELOOP — an
    /// error root cannot bypass (a mode-000 directory would pass as root, as
    /// CI's container runs).
    #[test]
    fn a_live_subvolume_that_cannot_be_statted_fails_and_nothing_runs_after_it() {
        let dir = tempfile::tempdir().unwrap();
        let m = dir.path().display().to_string();
        std::os::unix::fs::symlink("@", dir.path().join("@")).unwrap();
        let (config, _conf) = archive_fixture(dir.path());
        let runner = Scripted::from_owned(vec![(
            format!("btrfs subvolume list {m}"),
            0,
            "ID 257 gen 9 top level 5 path nvme/root-.20261005T0100\n".into(),
        )])
        .snapshotting();
        let step = archive_boot_with(&config, None, true, &TestProgress::new(), &env(&runner));
        assert!(
            matches!(&step, BootStep::Ran(o) if o.status() == "FAIL" && o.updated == 0
                && o.failures[0].contains("Cannot tell whether") && o.failures[0].contains(&format!("{m}/@"))),
            "{step:?}"
        );
        assert_eq!(runner.calls(), [format!("btrfs subvolume list {m}")]);
    }

    /// Staging: a subvolume name whose `.new` is one byte too long for a file
    /// name, so only the staging stat fails. Nothing is archived, built or
    /// deleted.
    #[test]
    fn a_staging_path_that_cannot_be_statted_fails_before_building_or_deleting() {
        let dir = tempfile::tempdir().unwrap();
        let m = dir.path().display().to_string();
        let name = "v".repeat(252);
        std::fs::create_dir(dir.path().join(&name)).unwrap();
        let conf = write_btrbk_conf(&name, "root-");
        let (mut config, _unused) = archive_fixture(dir.path());
        config.general.btrbk_conf = conf.path().to_string_lossy().into_owned();
        config.boot.subvolumes = vec![name.clone()];
        config.sources[0].subvolumes = vec![SubvolConfig {
            name: name.clone(),
            ..Default::default()
        }];
        let runner = Scripted::from_owned(vec![(
            format!("btrfs subvolume list {m}"),
            0,
            "ID 257 gen 9 top level 5 path nvme/root-.20261005T0100\n".into(),
        )])
        .snapshotting();
        let step = archive_boot_with(&config, None, true, &TestProgress::new(), &env(&runner));
        assert!(
            matches!(&step, BootStep::Ran(o) if o.status() == "FAIL" && o.updated == 0
                && o.failures[0].contains("Cannot tell whether")),
            "{step:?}"
        );
        let calls = runner.calls();
        assert!(
            !calls
                .iter()
                .any(|c| c.contains(".new") || c.contains("delete")),
            "nothing built or deleted: {calls:?}"
        );
        assert_eq!(calls.len(), 1, "only the listing ran: {calls:?}");
        assert!(dir.path().join(&name).is_dir());
    }

    #[test]
    fn the_dry_run_says_what_the_boot_step_would_do() {
        let dir = tempfile::tempdir().unwrap();
        let (mut config, _conf) = archive_fixture(dir.path());
        let say = |config: &Config, mode| {
            let runner = Scripted::from_owned(vec![]);
            let host = env(&runner);
            let options = BackupOptions {
                mode: Some(mode),
                dry_run: true,
                boot_archive: true,
                targets: Some(labels(&["primary-22tb"])),
                ..Default::default()
            };
            let progress = TestProgress::new();
            let result = run_backup_with(config, &options, &progress, &host).unwrap();
            assert!(runner.calls().is_empty(), "{:?}", runner.calls());
            // (Only the fixture's "not a mount point" complaint, as above.)
            assert!(
                result
                    .errors
                    .iter()
                    .all(|e| e.starts_with("Target directories missing")),
                "{:?}",
                result.errors
            );
            progress
                .logs
                .lock()
                .unwrap()
                .iter()
                .map(|(_, m)| m.clone())
                .collect::<Vec<_>>()
        };
        let has = |logs: &[String], text: &str| logs.iter().any(|l| l.contains(text));
        let full = say(&config, BackupMode::Full);
        assert!(
            has(&full, "would archive and replace boot subvolumes"),
            "{full:?}"
        );
        let inc = say(&config, BackupMode::Incremental);
        assert!(
            has(
                &inc,
                "would create any missing boot subvolume (never replace one)"
            ),
            "{inc:?}"
        );
        assert!(!has(&inc, "would archive and replace"), "{inc:?}");
        config.boot.enabled = false;
        let off = say(&config, BackupMode::Full);
        assert!(has(&off, "boot subvolumes: disabled in config"), "{off:?}");
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
            targets: Some(labels(&["primary-22tb"])),
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
        assert_eq!(
            (result.snapshots_created, result.snapshots_sent),
            (Some(2), Some(2))
        );
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

    /// A live run's result, recorded and read back from the history the way
    /// the GUI reads it.
    fn recorded(result: &BackupResult) -> crate::report::BackupRun {
        let db = Database::open(":memory:").unwrap();
        crate::report::record_backup_run(&db, result).unwrap();
        crate::report::get_backup_history(&db, 1).unwrap().remove(0)
    }

    // -- bytes_sent: the growth of the targets, measured around the btrbk steps --
    //
    // btrbk reports no transfer size, so the run reads the targets' usage
    // before its steps and, when something was created or sent and the steps
    // themselves reported no size, again after: the difference is what was
    // sent. `readings` are what the usage reads in turn.

    /// A live run of `options` with btrbk answering `script`, the usage
    /// reading `readings` in turn (the last one repeats). Returns the result
    /// and how many readings were taken.
    fn live_run_measuring(
        options: BackupOptions,
        script: Vec<(String, i32, String)>,
        readings: &[u64],
    ) -> (BackupResult, usize) {
        let runner = Scripted::from_owned(script);
        let taken = std::cell::Cell::new(0usize);
        let usage = |_: &Config, _: &dyn ProgressCallback| {
            let n = taken.get();
            taken.set(n + 1);
            readings[n.min(readings.len() - 1)]
        };
        let nvme_mounted = |p: &Path| p == Path::new("/.btrfs-nvme");
        let host = StepEnv {
            is_mountpoint: &nvme_mounted,
            usage: &usage,
            ..env(&runner)
        };
        let options = BackupOptions {
            targets: Some(labels(&["primary-22tb"])),
            ..options
        };
        let result =
            run_backup_with(&live_config(), &options, &TestProgress::new(), &host).unwrap();
        (result, taken.get())
    }

    /// The scripted btrbk of a live run: `snapshot`, `resume` (printing
    /// `resume_out`, exiting `resume_status`) and the listing of `rows`.
    fn btrbk_script(
        resume_status: i32,
        resume_out: &str,
        rows: &str,
    ) -> Vec<(String, i32, String)> {
        let f = "/proc/self/root /proc/self/home";
        let at = |args: &str| btrbk_at("/nonexistent/btrbk.conf", args);
        vec![
            (at(&format!("snapshot {f}")), 0, String::new()),
            (at(&format!("resume {f}")), resume_status, resume_out.into()),
            (
                at(&format!("--format=raw list latest {f}")),
                0,
                rows.to_string(),
            ),
        ]
    }

    #[test]
    fn bytes_sent_is_the_growth_of_the_targets_when_something_was_created_and_sent() {
        let row = raw_row("/s/a.1", "/t/a.1");
        let (result, taken) = live_run_measuring(
            BackupOptions::default(),
            btrbk_script(0, ">>> /t/a.1\n", &row),
            &[1000, 5000],
        );
        assert!(result.success, "{:?}", result.errors);
        assert_eq!(
            (result.snapshots_created, result.snapshots_sent),
            (Some(1), Some(1))
        );
        assert_eq!(result.bytes_sent, 4000);
        assert_eq!(taken, 2, "before the steps and after them");
    }

    #[test]
    fn bytes_sent_is_measured_for_a_send_alone_and_for_a_snapshot_alone() {
        let row = raw_row("/s/a.1", "/t/a.1");
        let send_only = BackupOptions {
            steps: BtrbkSteps::SendOnly,
            ..Default::default()
        };
        let (result, taken) = live_run_measuring(
            send_only,
            btrbk_script(0, ">>> /t/a.1\n", &row),
            &[1000, 5000],
        );
        assert_eq!(
            (result.snapshots_created, result.snapshots_sent),
            (Some(0), Some(1))
        );
        assert_eq!((result.bytes_sent, taken), (4000, 2));

        let snapshot_only = BackupOptions {
            steps: BtrbkSteps::SnapshotOnly,
            ..Default::default()
        };
        let (result, taken) =
            live_run_measuring(snapshot_only, btrbk_script(0, "", &row), &[1000, 5000]);
        assert_eq!(
            (result.snapshots_created, result.snapshots_sent),
            (Some(1), Some(0))
        );
        assert_eq!((result.bytes_sent, taken), (4000, 2));
    }

    #[test]
    fn a_step_that_failed_still_leaves_what_the_other_one_sent_to_be_measured() {
        // The send fails (btrbk exit 10): its count is unknown, the snapshot
        // step created one, so the targets are read again.
        let row = raw_row("/s/a.1", "/t/a.1");
        let (result, taken) = live_run_measuring(
            BackupOptions::default(),
            btrbk_script(10, "", &row),
            &[1000, 5000],
        );
        assert!(!result.success);
        assert_eq!(
            (result.snapshots_created, result.snapshots_sent),
            (Some(1), None)
        );
        assert_eq!((result.bytes_sent, taken), (4000, 2));
    }

    #[test]
    fn nothing_created_or_sent_reads_the_targets_once_and_sends_zero_bytes() {
        let (result, taken) = live_run_measuring(
            BackupOptions {
                steps: BtrbkSteps::SendOnly,
                ..Default::default()
            },
            btrbk_script(0, "", ""),
            &[1000, 5000],
        );
        assert!(result.success, "{:?}", result.errors);
        assert_eq!(
            (result.snapshots_created, result.snapshots_sent),
            (Some(0), Some(0))
        );
        assert_eq!((result.bytes_sent, taken), (0, 1), "no second reading");
    }

    #[test]
    fn a_size_btrbk_reported_is_kept_and_the_targets_are_not_read_again() {
        let row = raw_row("/s/a.1", "/t/a.1");
        let (result, taken) = live_run_measuring(
            BackupOptions {
                steps: BtrbkSteps::SendOnly,
                ..Default::default()
            },
            btrbk_script(0, ">>> /t/a.1 (incremental, 2.0 MiB)\n", &row),
            &[1000, 5000],
        );
        assert_eq!(result.bytes_sent, 2 * 1_048_576);
        assert_eq!(taken, 1, "the step's own figure is the answer");
    }

    #[test]
    fn a_target_that_shrank_during_the_run_sent_zero_bytes_not_a_wrapped_number() {
        let row = raw_row("/s/a.1", "/t/a.1");
        let (result, taken) = live_run_measuring(
            BackupOptions::default(),
            btrbk_script(0, ">>> /t/a.1\n", &row),
            &[5000, 1000],
        );
        assert_eq!((result.bytes_sent, taken), (0, 2));
    }

    #[test]
    fn a_failed_btrbk_step_leaves_its_count_unknown_and_the_history_says_null_not_zero() {
        // The snapshot step fails (exit 10); the send, which does not depend
        // on it, runs and counts one.
        let f = "/proc/self/root /proc/self/home";
        let runner = Scripted::from_owned(vec![
            (
                btrbk_at("/nonexistent/btrbk.conf", &format!("snapshot {f}")),
                10,
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
                raw_row("/s/a.1", "/t/a.1"),
            ),
        ]);
        let nvme_mounted = |p: &Path| p == Path::new("/.btrfs-nvme");
        let host = StepEnv {
            is_mountpoint: &nvme_mounted,
            ..env(&runner)
        };
        let result =
            run_backup_with(&live_config(), &live_options(), &TestProgress::new(), &host).unwrap();
        assert!(!result.success);
        assert_eq!(
            (result.snapshots_created, result.snapshots_sent),
            (None, Some(1))
        );
        let row = recorded(&result);
        assert_eq!((row.snapshots_created, row.snapshots_sent), (None, Some(1)));
        let summary = backup_summary(&result, false);
        assert!(
            summary.contains("snapshots created: unknown, 1 sent"),
            "{summary}"
        );
    }

    #[test]
    fn a_run_that_counted_zero_records_zero_not_unknown() {
        // The counter-case: a measured zero is a count, and stays one.
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
                String::new(),
            ),
            (
                btrbk_at(
                    "/nonexistent/btrbk.conf",
                    &format!("--format=raw list latest {f}"),
                ),
                0,
                String::new(),
            ),
        ]);
        let nvme_mounted = |p: &Path| p == Path::new("/.btrfs-nvme");
        let host = StepEnv {
            is_mountpoint: &nvme_mounted,
            ..env(&runner)
        };
        let result =
            run_backup_with(&live_config(), &live_options(), &TestProgress::new(), &host).unwrap();
        assert!(result.success, "{:?}", result.errors);
        assert_eq!(
            (result.snapshots_created, result.snapshots_sent),
            (Some(0), Some(0))
        );
        let row = recorded(&result);
        assert_eq!(
            (row.snapshots_created, row.snapshots_sent),
            (Some(0), Some(0))
        );
        assert_eq!(
            backup_summary(&result, false),
            "Backup (incremental): nothing to do — all snapshots up to date"
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
            sources: Some(labels(&["manual-src"])),
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
        assert_eq!(effective_sources(&config, &options).unwrap(), ["nvme-root"]);
    }

    #[test]
    fn a_source_named_explicitly_is_backed_up_even_if_manual_only() {
        let config = make_test_config();
        let options = BackupOptions {
            sources: Some(labels(&["manual-src", "nvme-root"])),
            ..Default::default()
        };
        assert_eq!(
            effective_sources(&config, &options).unwrap(),
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
        assert_eq!(
            effective_targets(&config, &BackupOptions::default()).unwrap(),
            ["primary-22tb"],
            "/proc is mounted, the other mount point does not exist"
        );
    }

    #[test]
    fn targets_named_are_trusted_without_a_mount_check_and_an_unknown_label_is_refused() {
        let mut config = make_test_config();
        let mut absent = config.targets[0].clone();
        absent.label = "absent".into();
        absent.mount = "/nonexistent/das/absent".into();
        config.targets.push(absent);
        let options = BackupOptions {
            targets: Some(labels(&["absent"])),
            ..Default::default()
        };
        // Named, so a ticked target is passed on whether or not it is
        // mounted: the verification before btrbk is what refuses it.
        assert_eq!(effective_targets(&config, &options).unwrap(), ["absent"]);
        // An unknown label beside it is refused, not dropped (review M2):
        // the run would otherwise go on to the known one without a word.
        let options = BackupOptions {
            targets: Some(labels(&["absent", "no-such-target"])),
            ..Default::default()
        };
        let why = effective_targets(&config, &options).unwrap_err();
        assert!(why.contains("target 'no-such-target'"), "{why}");
        assert!(
            why.contains("absent") && why.contains("primary-22tb"),
            "{why}"
        );
    }

    #[test]
    fn an_unknown_label_in_a_partial_list_is_found_in_either_list() {
        let config = make_test_config();
        let known = BackupOptions {
            sources: Some(labels(&["nvme-root"])),
            targets: Some(labels(&["primary-22tb"])),
            ..Default::default()
        };
        assert_eq!(unknown_label(&config, &known), None);
        assert_eq!(unknown_label(&config, &BackupOptions::default()), None);
        let bad_target = BackupOptions {
            sources: Some(labels(&["nvme-root"])),
            targets: Some(labels(&["primary-22tb", "bogus"])),
            ..Default::default()
        };
        let why = unknown_label(&config, &bad_target).unwrap();
        assert!(
            why.starts_with("The target 'bogus' is not in the configuration"),
            "{why}"
        );
        let bad_source = BackupOptions {
            sources: Some(labels(&["nvme-root", "bogus"])),
            targets: Some(labels(&["primary-22tb"])),
            ..Default::default()
        };
        let why = unknown_label(&config, &bad_source).unwrap();
        assert!(
            why.starts_with("The source 'bogus' is not in the configuration"),
            "{why}"
        );
        assert!(effective_sources(&config, &bad_source).is_err());
    }

    // -- an empty selection is a refusal, never "all" (bd DAS-Backup-Manager-7tx) --

    #[test]
    fn targets_named_that_match_nothing_are_refused_not_widened_to_those_mounted() {
        let config = make_test_config();
        let options = BackupOptions {
            targets: Some(labels(&["stale-label"])),
            ..Default::default()
        };
        let why = effective_targets(&config, &options).unwrap_err();
        assert!(
            why.contains("stale-label") && why.contains("primary-22tb"),
            "{why}"
        );
    }

    #[test]
    fn every_target_unticked_is_a_refusal_not_every_mounted_target() {
        let config = make_test_config();
        let options = BackupOptions {
            targets: Some(Vec::new()),
            ..Default::default()
        };
        let why = effective_targets(&config, &options).unwrap_err();
        assert!(why.contains("No target selected"), "{why}");
        // The control: left unspecified, the same config selects what is mounted.
        assert_eq!(
            effective_targets(&config, &BackupOptions::default()).unwrap(),
            ["primary-22tb"]
        );
    }

    #[test]
    fn every_source_unticked_is_a_refusal_not_every_source() {
        let config = make_test_config();
        let options = BackupOptions {
            sources: Some(Vec::new()),
            ..Default::default()
        };
        let why = effective_sources(&config, &options).unwrap_err();
        assert!(why.contains("No source selected"), "{why}");
        assert_eq!(
            effective_sources(&config, &BackupOptions::default()).unwrap(),
            ["nvme-root"]
        );
    }

    #[test]
    fn the_empty_selection_check_names_the_list_that_is_empty_and_passes_the_rest() {
        let empty_sources = BackupOptions {
            sources: Some(Vec::new()),
            targets: Some(labels(&["primary-22tb"])),
            ..Default::default()
        };
        assert!(
            empty_selection(&empty_sources)
                .unwrap()
                .starts_with("No source selected")
        );
        let empty_targets = BackupOptions {
            sources: Some(labels(&["nvme-root"])),
            targets: Some(Vec::new()),
            ..Default::default()
        };
        assert!(
            empty_selection(&empty_targets)
                .unwrap()
                .starts_with("No target selected")
        );
        let both = BackupOptions {
            sources: Some(labels(&["nvme-root"])),
            targets: Some(labels(&["primary-22tb"])),
            ..Default::default()
        };
        assert_eq!(empty_selection(&both), None);
        assert_eq!(empty_selection(&BackupOptions::default()), None);
    }

    #[test]
    fn the_d_bus_lists_are_checked_one_by_one() {
        assert!(refuse_empty_sources(&[]).unwrap().contains("No source"));
        assert!(refuse_empty_targets(&[]).unwrap().contains("No target"));
        assert_eq!(refuse_empty_sources(&labels(&["nvme-root"])), None);
        assert_eq!(refuse_empty_targets(&labels(&["primary-22tb"])), None);
    }

    #[test]
    fn a_live_run_with_every_target_unticked_is_refused_and_btrbk_is_never_run() {
        let config = live_config();
        let runner = Scripted::from_owned(vec![]);
        let options = BackupOptions {
            targets: Some(Vec::new()),
            ..live_options()
        };
        let err = run_backup_with(&config, &options, &TestProgress::new(), &env(&runner))
            .unwrap_err()
            .to_string();
        assert!(err.contains("No target selected"), "{err}");
        assert!(
            runner.calls().is_empty(),
            "nothing may run: {:?}",
            runner.calls()
        );
    }

    #[test]
    fn a_live_run_with_every_source_unticked_is_refused_and_btrbk_is_never_run() {
        let config = live_config();
        let runner = Scripted::from_owned(vec![]);
        let options = BackupOptions {
            sources: Some(Vec::new()),
            ..live_options()
        };
        let err = run_backup_with(&config, &options, &TestProgress::new(), &env(&runner))
            .unwrap_err()
            .to_string();
        assert!(err.contains("No source selected"), "{err}");
        assert!(
            runner.calls().is_empty(),
            "nothing may run: {:?}",
            runner.calls()
        );
    }

    /// bd DAS-Backup-Manager-7tx (review I1): "no source named" that resolves to
    /// nothing — every source has only manual-only subvolumes — used to be an
    /// empty list, which `btrbk_filters` read as "every declared pair": btrbk
    /// was handed the manual-only subvolumes.
    #[test]
    fn a_run_naming_no_source_where_every_source_is_manual_only_is_refused_and_btrbk_never_runs() {
        let mut config = live_config();
        for source in &mut config.sources {
            for sv in &mut source.subvolumes {
                sv.manual_only = true;
            }
        }
        // The control: with one automatic subvolume the same run selects it.
        assert!(effective_sources(&live_config(), &live_options()).is_ok());
        let why = effective_sources(&config, &live_options()).unwrap_err();
        assert!(why.contains("manual-only"), "{why}");

        let runner = Scripted::from_owned(vec![]);
        let err = run_backup_with(
            &config,
            &live_options(),
            &TestProgress::new(),
            &env(&runner),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("manual-only"), "{err}");
        assert!(
            runner.calls().is_empty(),
            "btrbk must not be handed the manual-only subvolumes: {:?}",
            runner.calls()
        );
    }

    /// The same defect one level down: whoever hands a step a list that was
    /// given and is empty gets a refusal from the step itself, so "everything"
    /// is only ever `None`.
    #[test]
    fn a_step_given_an_empty_source_list_refuses_where_none_means_every_source() {
        let config = steps_config();
        let primary = labels(&["primary-22tb"]);
        let progress = TestProgress::new();
        let none: &[String] = &[];
        for (name, err) in [
            (
                "snapshot",
                create_snapshots_with(&config, Some(none), None, &progress, &Unspawnable)
                    .unwrap_err()
                    .to_string(),
            ),
            (
                "send",
                send_snapshots_with(
                    &config,
                    Some(none),
                    &primary,
                    false,
                    &progress,
                    &env(&Unspawnable),
                )
                .unwrap_err()
                .to_string(),
            ),
            (
                "full",
                run_full_pipeline_with(
                    &config,
                    Some(none),
                    &primary,
                    &progress,
                    &env(&Unspawnable),
                )
                .unwrap_err()
                .to_string(),
            ),
        ] {
            assert!(err.contains("No source selected"), "{name}: {err}");
        }
        // The control: "not specified" is every source, and gets as far as btrbk.
        let runner = Scripted::from_owned(vec![]);
        assert!(create_snapshots_with(&config, None, None, &progress, &runner).is_err());
        assert!(!runner.calls().is_empty(), "None must reach btrbk");
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

    const SHARED_LISTING: &str = include_str!("../../tests/fixtures/boot-subvol-listing.txt");

    #[test]
    fn the_snapshot_match_rule_is_the_one_the_script_uses() {
        let cases: [(&[&str], &str, Option<&str>); 7] = [
            (&["nvme"], "root-", Some("nvme/root-.20261005T0100_1")),
            (&["/nvme/"], "root-", Some("nvme/root-.20261005T0100_1")),
            (&["nvme"], "home", Some("nvme/home.20261005T0100")),
            (&["nvme", "ssd"], "home", Some("ssd/home.20261007T0100")),
            (&["nvme", "ssd"], "var", Some("nvme/var.20261009T0100")),
            (&["nvme"], "a.b", Some("nvme/a.b.20261003T0100")),
            (&["nvme"], "log", None),
        ];
        for (subdirs, name, want) in cases {
            let subdirs: Vec<String> = subdirs.iter().map(|s| s.to_string()).collect();
            assert_eq!(
                latest_matching_snapshot(SHARED_LISTING, &subdirs, name),
                want,
                "{subdirs:?} {name}"
            );
        }
    }

    #[test]
    fn a_btrbk_timestamp_is_digits_t_digits_and_an_optional_numeric_collision() {
        for good in ["20261005T0100", "20261005T0100_1", "20261005T0100_12"] {
            assert!(is_btrbk_timestamp(good), "{good}");
        }
        for bad in [
            "",
            "20261005T010",
            "20261005X0100",
            "2026100aT0100",
            "20261005T01a0",
            "20261005T0100_",
            "20261005T0100_x",
            "20261005T0100_1x",
            "20261005T0100_1_2",
        ] {
            assert!(!is_btrbk_timestamp(bad), "{bad:?}");
        }
    }

    #[test]
    fn a_boot_step_reports_archived_and_failed_only_when_it_ran_and_did() {
        let mut ran = BootOutcome::default();
        assert!(!BootStep::NotSelected.archived() && !BootStep::NotSelected.failed());
        assert!(!BootStep::DisabledInConfig.archived() && !BootStep::DisabledInConfig.failed());
        assert!(!BootStep::Ran(ran.clone()).archived() && !BootStep::Ran(ran.clone()).failed());
        ran.updated = 1;
        assert!(BootStep::Ran(ran.clone()).archived() && !BootStep::Ran(ran.clone()).failed());
        ran.failures.push("x".into());
        assert!(BootStep::Ran(ran).failed());
    }

    #[test]
    fn a_listing_that_cannot_be_read_is_an_error_not_an_empty_listing() {
        let failed =
            Scripted::from_owned(vec![("btrfs subvolume list /m".into(), 1, String::new())])
                .with_stderr("btrfs subvolume list /m", "ERROR: can't access '/m'\n");
        let why = subvolume_listing(&failed, "/m").unwrap_err();
        assert!(why.contains("can't access"), "{why}");
        let ok = Scripted::from_owned(vec![(
            "btrfs subvolume list /m".into(),
            0,
            "ID 1 path x\n".into(),
        )]);
        assert_eq!(subvolume_listing(&ok, "/m").unwrap(), "ID 1 path x\n");
    }

    #[test]
    fn the_boot_plan_names_come_from_btrbk_conf_and_an_unreadable_one_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let (mut config, _conf) = archive_fixture(dir.path());
        config.boot.subvolumes = vec!["@".into(), "@home".into()];
        let plan = boot_plan(&config).unwrap();
        assert_eq!(
            plan[0],
            BootPlanItem {
                subvol: "@".into(),
                snapshot_name: Some("root-".into()),
                subdirs: vec!["nvme".into()]
            }
        );
        assert_eq!(plan[1].subvol, "@home");
        assert_eq!(
            plan[1].snapshot_name, None,
            "absent from btrbk.conf: never guessed"
        );
        config.general.btrbk_conf = "/nonexistent-c4x/btrbk.conf".into();
        assert!(
            boot_plan(&config)
                .unwrap_err()
                .contains("/nonexistent-c4x/btrbk.conf")
        );
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
        let runner = Scripted::from_owned(vec![]);
        let result = archive_boot_with(&config, None, true, &progress, &env(&runner));
        assert!(
            matches!(&result, BootStep::Ran(o) if o.status() == "FAIL" && o.updated == 0),
            "nothing may be archived when names cannot be resolved: {result:?}"
        );
        assert!(runner.calls().is_empty(), "{:?}", runner.calls());
        let logs = progress.logs.lock().unwrap();
        assert!(
            logs.iter().any(|(_, m)| m
                .contains("no boot subvolume is touched rather than a snapshot name guessed")),
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
        let runner = Scripted::from_owned(vec![(
            format!("btrfs subvolume list {}", target_dir.path().display()),
            0,
            String::new(),
        )]);
        let step = archive_boot_with(&config, None, true, &progress, &env(&runner));
        assert!(
            matches!(&step, BootStep::Ran(o) if o.status() == "WARN" && o.updated == 0),
            "{step:?}"
        );
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
            true,
            &progress,
            &env(&Scripted::from_owned(vec![])),
        );
        assert!(
            matches!(&result, BootStep::Ran(o) if o.skipped == 1),
            "the mirror is skipped, once: {result:?}"
        );

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

    /// A primary and a mirror target on the given mount paths, with a
    /// readable btrbk.conf (kept alive by the returned handle).
    fn primary_and_mirror(primary: &str, mirror: &str) -> (Config, tempfile::NamedTempFile) {
        let mut config = make_test_config();
        let conf = write_btrbk_conf("@", "root-");
        config.general.btrbk_conf = conf.path().to_string_lossy().to_string();
        config.boot.subvolumes = vec!["@".to_string()];
        config.sources[0].subvolumes = vec![SubvolConfig {
            name: "@".into(),
            manual_only: false,
            snapshot_name: None,
            ..Default::default()
        }];
        config.sources[0].target_subdirs = vec!["nvme".into()];
        config.targets[0].mount = primary.to_string();
        config.targets[0].label = "primary-22tb".into();
        config.targets.push(Target {
            label: "system-recovery-A-2tb".into(),
            serial: "MIRRORSERIAL".into(),
            serials: vec!["MIRRORSERIAL".into()],
            mount_uuid: None,
            mount: mirror.to_string(),
            role: TargetRole::Mirror,
            retention: Retention {
                weekly: 4,
                monthly: 2,
                daily: 7,
                yearly: 0,
            },
            display_name: "Recovery A (independent OS)".into(),
        });
        (config, conf)
    }

    #[test]
    fn a_mount_point_that_cannot_be_statted_fails_the_whole_step() {
        // bd 4za8: `exists()` read a failed stat as "not mounted", so the step
        // reported OK with nothing done. The mount path lies under a regular
        // file (ENOTDIR), which root cannot bypass, unlike a mode-000 directory.
        let primary_dir = tempfile::tempdir().unwrap();
        let file = primary_dir.path().join("file");
        std::fs::write(&file, b"x").unwrap();
        let unreadable = file.join("mnt");
        assert!(
            Path::new(&unreadable).try_exists().is_err(),
            "fixture: the stat must fail"
        );
        let (config, _conf) = primary_and_mirror(
            &primary_dir.path().to_string_lossy(),
            &unreadable.to_string_lossy(),
        );
        let progress = TestProgress::new();
        let runner = Scripted::from_owned(vec![]);
        let step = archive_boot_with(&config, None, true, &progress, &env(&runner));
        assert!(
            matches!(&step, BootStep::Ran(o) if o.status() == "FAIL"
                && o.failures.iter().any(|f| f.contains("cannot tell whether")
                    && f.contains(&*unreadable.to_string_lossy()))),
            "{step:?}"
        );
        assert!(runner.calls().is_empty(), "{:?}", runner.calls());
        assert!(step.failed());
    }

    #[test]
    fn an_unmounted_mirror_is_not_counted_as_skipped_and_a_mounted_one_is() {
        let primary_dir = tempfile::tempdir().unwrap();
        let mirror_dir = tempfile::tempdir().unwrap();
        let absent = mirror_dir.path().join("absent");

        let (config, _conf) = primary_and_mirror(
            &primary_dir.path().to_string_lossy(),
            &absent.to_string_lossy(),
        );
        let step = archive_boot_with(
            &config,
            None,
            true,
            &TestProgress::new(),
            &env(&Scripted::from_owned(vec![])),
        );
        assert!(
            matches!(&step, BootStep::Ran(o) if o.skipped == 0),
            "an unmounted mirror is not a skip: {step:?}"
        );

        let (config, _conf) = primary_and_mirror(
            &primary_dir.path().to_string_lossy(),
            &mirror_dir.path().to_string_lossy(),
        );
        let step = archive_boot_with(
            &config,
            None,
            true,
            &TestProgress::new(),
            &env(&Scripted::from_owned(vec![])),
        );
        assert!(
            matches!(&step, BootStep::Ran(o) if o.skipped == 1),
            "a mounted mirror is skipped, once: {step:?}"
        );
    }

    #[test]
    fn boot_archive_exits_3_when_the_step_or_the_release_failed() {
        let ok = BootStep::Ran(BootOutcome::default());
        let mut failed_outcome = BootOutcome::default();
        failed_outcome.fail(&TestProgress::new(), "x".into());
        let failed = BootStep::Ran(failed_outcome);
        assert_eq!(ok.exit_code(true), None);
        assert_eq!(
            failed.exit_code(true),
            Some(3),
            "step failed, mounts released"
        );
        assert_eq!(ok.exit_code(false), Some(3), "step fine, unmount failed");
        assert_eq!(failed.exit_code(false), Some(3), "both");
        assert_eq!(BootStep::DisabledInConfig.exit_code(true), None);
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

    /// Review M5: `backup snapshot` / `send` sync first, as `run` does, and
    /// work from the config sync wrote — not the one they loaded.
    #[test]
    fn a_manual_step_syncs_first_and_runs_on_the_config_sync_wrote() {
        let dir = tempfile::tempdir().unwrap();
        let path = sync_fixture(dir.path());
        let progress = TestProgress::new();
        let config = sync_for_manual_step_with(
            &path,
            &make_test_config(),
            "2026-10-02",
            &listing(&["@srv", "@new"]),
            &|_| true,
            &progress,
        )
        .unwrap();
        assert!(
            config
                .sources
                .iter()
                .flat_map(|s| &s.subvolumes)
                .any(|e| e.name == "@new"),
            "the step must use the config sync wrote"
        );
    }

    #[test]
    fn a_manual_step_whose_sync_failed_is_refused_and_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let path = sync_fixture(dir.path());
        let progress = TestProgress::new();
        // The volume is not mounted: sync reads nothing and fails.
        let err = sync_for_manual_step_with(
            &path,
            &make_test_config(),
            "2026-10-02",
            &listing(&["@srv"]),
            &|_| false,
            &progress,
        )
        .unwrap_err();
        assert!(err.contains("nothing was run"), "{err}");
        // The reason is in the log, at error level, for the operator.
        let logs = progress.logs.lock().unwrap();
        assert!(
            logs.iter()
                .any(|(level, msg)| *level == LogLevel::Error && msg.contains("VOLUMES NOT READ")),
            "{logs:?}"
        );
    }

    /// The binding to the host's runner, reached without touching the host: a
    /// config path that does not exist fails sync at its first step (loading
    /// the config), before any command or mount test, so the binding must
    /// refuse. Its whole-body mutant (`Ok(Default::default())`) fails here.
    #[test]
    fn the_host_binding_refuses_when_sync_cannot_even_load_the_config() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("no-such-dir").join("config.toml");
        let progress = TestProgress::new();
        let err = sync_for_manual_step(&missing, &make_test_config(), &progress).unwrap_err();
        assert!(err.contains("nothing was run"), "{err}");
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
                email_report: send,
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
        /// What `report` says became of the report.
        report_saved: Result<(), String>,
        report_emailed: bool,
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
                report_saved: Ok(()),
                report_emailed: true,
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
            boot: r.boot.clone(),
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
                snapshots_created: Some(2),
                snapshots_sent: Some(2),
                snapshots_cleaned: 0,
                bytes_sent: 10,
                boot: BootStep::NotSelected,
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
        ) -> ReportDelivery {
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
            ReportDelivery {
                saved: self.report_saved.clone(),
                emailed: self.report_emailed,
            }
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
    fn a_job_with_nothing_selected_is_refused_before_any_lock_mount_or_record() {
        for (sources, targets, says) in [
            (None, Some(Vec::new()), "No target selected"),
            (Some(Vec::new()), None, "No source selected"),
            (Some(Vec::new()), Some(Vec::new()), "No source selected"),
        ] {
            let host = FakeHost::default();
            let options = BackupOptions {
                sources,
                targets,
                ..Default::default()
            };
            let outcome = run_backup_job(&host, make_test_config(), options, &TestProgress::new());
            let (ok, line) = outcome.finish_line(false);
            assert!(!ok, "{outcome:?}");
            assert!(line.contains(says), "{line}");
            assert_eq!(outcome.exit_code(), 1, "it never began: {outcome:?}");
            assert!(
                host.steps().is_empty(),
                "the host was touched: {:?}",
                host.steps()
            );
        }
    }

    /// Review M3: a label list the configuration does not know is refused
    /// before the first lock, sync or mount, as an empty one is: it never
    /// began, so exit 1 and nothing recorded (the script's rule for a run that
    /// "could not start"), not an abort with a failed history row.
    #[test]
    fn a_job_naming_a_label_the_configuration_lacks_is_refused_before_any_lock_mount_or_record() {
        for (sources, targets, says) in [
            (
                None,
                Some(labels(&["primary-22tb", "bogus"])),
                "target 'bogus'",
            ),
            (
                Some(labels(&["nvme-root", "bogus"])),
                None,
                "source 'bogus'",
            ),
            (None, Some(labels(&["bogus"])), "target 'bogus'"),
        ] {
            let host = FakeHost::default();
            let options = BackupOptions {
                sources,
                targets,
                ..Default::default()
            };
            let outcome = run_backup_job(&host, make_test_config(), options, &TestProgress::new());
            let (ok, line) = outcome.finish_line(false);
            assert!(!ok, "{outcome:?}");
            assert!(line.contains(says), "{line}");
            assert_eq!(outcome.exit_code(), 1, "it never began: {outcome:?}");
            assert!(
                host.steps().is_empty(),
                "the host was touched: {:?}",
                host.steps()
            );
        }
    }

    #[test]
    fn a_job_with_a_selection_made_proceeds_exactly_as_one_without() {
        let host = FakeHost::default();
        let options = BackupOptions {
            sources: Some(labels(&["nvme-root"])),
            targets: Some(labels(&["primary-22tb"])),
            ..Default::default()
        };
        let outcome = run_backup_job(&host, make_test_config(), options, &TestProgress::new());
        assert!(outcome.success(), "{outcome:?}");
        assert!(host.steps().contains(&"locks".to_string()));
        assert!(host.steps().contains(&"record".to_string()));
    }

    // -- the exit rule of `backup run`: 0 / 3 / 1 (bd DAS-Backup-Manager-vzsu) --

    #[test]
    fn the_exit_status_follows_the_doctors_rule_for_every_way_a_job_ends() {
        let ran = |success| BackupJobOutcome::Ran(result_with(success, 1, 1, 0));
        // 0: ran clean, or another backup was running and this one declined.
        assert_eq!(ran(true).exit_code(), 0);
        assert_eq!(BackupJobOutcome::Declined.exit_code(), 0);
        // 3: began and something failed, or aborted on a target's or a source's state.
        assert_eq!(ran(false).exit_code(), 3);
        assert_eq!(BackupJobOutcome::Aborted("x".into()).exit_code(), 3);
        // 1: could not start.
        assert_eq!(BackupJobOutcome::CouldNotStart("x".into()).exit_code(), 1);
    }

    #[test]
    fn an_absent_ticked_target_that_stops_the_run_exits_3_not_1() {
        // The refusal run_backup gives for a ticked target that is not what it
        // must be ("Refusing to run btrbk ...") used to exit 1 here.
        let host = FakeHost {
            run: Err("Refusing to run btrbk on a bare directory".into()),
            ..Default::default()
        };
        let (outcome, _) = job(&host, false);
        assert_eq!(outcome.exit_code(), 3, "{outcome:?}");
        assert!(host.steps().contains(&"locks".to_string()));
    }

    #[test]
    fn an_abort_is_recorded_as_a_failed_run_with_unknown_counts() {
        let host = FakeHost {
            run: Err("Refusing to run btrbk".into()),
            ..Default::default()
        };
        let options = BackupOptions {
            mode: Some(BackupMode::Full),
            ..Default::default()
        };
        let outcome = run_backup_job(&host, make_test_config(), options, &TestProgress::new());
        assert_eq!(outcome.exit_code(), 3);
        let recorded = host.recorded.lock().unwrap();
        assert_eq!(recorded.len(), 1);
        let row = &recorded[0];
        assert!(!row.success);
        assert_eq!(row.mode, BackupMode::Full);
        assert_eq!((row.snapshots_created, row.snapshots_sent), (None, None));
        assert_eq!(row.errors, ["Backup failed: Refusing to run btrbk"]);
    }

    #[test]
    fn an_aborted_dry_run_records_nothing() {
        let host = FakeHost {
            run: Err("Refusing to run btrbk".into()),
            ..Default::default()
        };
        let (outcome, _) = job(&host, true);
        assert_eq!(outcome.exit_code(), 3);
        assert!(host.recorded.lock().unwrap().is_empty());
        assert!(!host.steps().contains(&"record".to_string()));
    }

    #[test]
    fn an_abort_whose_row_cannot_be_written_says_so_and_still_exits_3() {
        let host = FakeHost {
            run: Err("Refusing to run btrbk".into()),
            record_fails: true,
            ..Default::default()
        };
        let (outcome, progress) = job(&host, false);
        assert_eq!(outcome.exit_code(), 3);
        let (ok, line) = outcome.finish_line(false);
        assert!(!ok);
        assert_eq!(
            line,
            "Backup failed: Refusing to run btrbk; history not recorded: disk full"
        );
        assert!(
            progress
                .logs
                .lock()
                .unwrap()
                .iter()
                .any(|(l, m)| *l == LogLevel::Error && m == "history not recorded: disk full")
        );
    }

    #[test]
    fn a_job_that_could_not_start_records_nothing_and_exits_1() {
        let host = FakeHost {
            locks: Err("permission denied".into()),
            ..Default::default()
        };
        let (outcome, _) = job(&host, false);
        assert_eq!(outcome.exit_code(), 1);
        assert_eq!(host.steps(), vec!["locks"]);
        assert!(host.recorded.lock().unwrap().is_empty());
    }

    #[test]
    fn a_failed_run_that_did_begin_exits_3_and_a_clean_one_0() {
        let host = FakeHost::default();
        let (clean, _) = job(&host, false);
        assert_eq!(clean.exit_code(), 0, "{clean:?}");
        let host = FakeHost {
            sync_failed: true,
            ..Default::default()
        };
        let (failed, _) = job(&host, false);
        assert_eq!(failed.exit_code(), 3, "{failed:?}");
    }

    // -- a report that exists nowhere fails the run, and the row says why --

    #[test]
    fn a_report_that_is_neither_saved_nor_mailed_fails_the_run_before_it_is_recorded() {
        let host = FakeHost {
            report_saved: Err(
                "Failed to save report to /x/last-report.txt: Not a directory".into(),
            ),
            report_emailed: false,
            ..Default::default()
        };
        let (outcome, _) = job(&host, false);
        assert!(!outcome.success(), "{outcome:?}");
        assert_eq!(outcome.exit_code(), 3);
        let recorded = host.recorded.lock().unwrap();
        assert_eq!(recorded.len(), 1);
        assert!(!recorded[0].success, "the row is written after the report");
        assert_eq!(
            recorded[0].errors,
            ["report: Failed to save report to /x/last-report.txt: Not a directory"]
        );
        let steps = host.steps();
        let at = |s: &str| steps.iter().position(|x| x == s).unwrap();
        assert!(at("report") < at("record"), "{steps:?}");
    }

    #[test]
    fn a_report_that_exists_somewhere_does_not_fail_the_run() {
        // Mailed but not saved: the report exists.
        let host = FakeHost {
            report_saved: Err("disk full".into()),
            report_emailed: true,
            ..Default::default()
        };
        let (outcome, _) = job(&host, false);
        assert!(outcome.success(), "{outcome:?}");
        // Saved but not mailed: the report exists, the email is non-fatal.
        let host = FakeHost {
            report_saved: Ok(()),
            report_emailed: false,
            ..Default::default()
        };
        let (outcome, _) = job(&host, false);
        assert!(outcome.success(), "{outcome:?}");
        let BackupJobOutcome::Ran(result) = &outcome else {
            panic!("{outcome:?}")
        };
        assert!(!result.report_sent);
    }

    #[test]
    fn a_report_is_lost_only_when_it_was_neither_written_nor_mailed() {
        let lost = |saved: Result<(), String>, emailed| {
            ReportDelivery { saved, emailed }.lost().map(str::to_string)
        };
        assert_eq!(lost(Err("e".into()), false), Some("e".to_string()));
        assert_eq!(lost(Err("e".into()), true), None);
        assert_eq!(lost(Ok(()), false), None);
        assert_eq!(lost(Ok(()), true), None);
    }

    #[test]
    fn a_lock_error_does_not_run() {
        let host = FakeHost {
            locks: Err("permission denied".into()),
            ..Default::default()
        };
        let (outcome, _) = job(&host, false);
        let BackupJobOutcome::CouldNotStart(why) = &outcome else {
            panic!("{outcome:?}")
        };
        assert_eq!(why, "Could not acquire backup locks: permission denied");
        assert_eq!(outcome.exit_code(), 1);
        assert!(!host.steps().contains(&"record".to_string()));
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
        let BackupJobOutcome::Aborted(why) = &outcome else {
            panic!("{outcome:?}")
        };
        assert_eq!(
            why,
            "Mount failed: No DAS drives found; still mounted: /.btrfs-nvme"
        );
        assert_eq!(outcome.finish_line(false), (false, why.clone()));
        assert_eq!(outcome.exit_code(), 3);
        assert!(host.steps().contains(&"release sources".to_string()));
        // Began, and stopped on a target's state: recorded as a failed run.
        let recorded = host.recorded.lock().unwrap();
        assert_eq!(recorded.len(), 1);
        assert!(!recorded[0].success);
        assert_eq!(recorded[0].errors, std::slice::from_ref(why));
    }

    #[test]
    fn a_backup_that_cannot_start_is_not_run_and_releases_everything() {
        let host = FakeHost {
            run: Err("Refusing to run btrbk".into()),
            ..Default::default()
        };
        let (outcome, _) = job(&host, false);
        let BackupJobOutcome::Aborted(why) = &outcome else {
            panic!("{outcome:?}")
        };
        assert_eq!(why, "Backup failed: Refusing to run btrbk");
        assert_eq!(outcome.exit_code(), 3);
        let steps = host.steps();
        assert!(steps.contains(&"release targets".to_string()), "{steps:?}");
        assert!(steps.contains(&"release sources".to_string()), "{steps:?}");
        let recorded = host.recorded.lock().unwrap();
        assert_eq!(recorded.len(), 1, "an abort is a failed row: {steps:?}");
        assert!(!recorded[0].success);
    }

    #[test]
    fn a_record_that_fails_fails_the_run_and_says_so() {
        let host = FakeHost {
            record_fails: true,
            ..Default::default()
        };
        let (outcome, progress) = job(&host, false);
        assert!(
            !outcome.success(),
            "a run missing from the history is not a success: {outcome:?}"
        );
        let (ok, line) = outcome.finish_line(false);
        assert!(!ok);
        assert!(line.contains("history not recorded: disk full"), "{line}");
        let BackupJobOutcome::Ran(result) = &outcome else {
            panic!("{outcome:?}")
        };
        assert_eq!(result.errors, ["history not recorded: disk full"]);
        assert!(
            progress
                .logs
                .lock()
                .unwrap()
                .iter()
                .any(|(l, m)| *l == LogLevel::Error && m == "history not recorded: disk full")
        );
    }

    #[test]
    fn a_record_that_works_leaves_the_run_a_success() {
        // The counter-case to the one above.
        let host = FakeHost::default();
        let (outcome, _) = job(&host, false);
        assert!(outcome.success(), "{outcome:?}");
        let BackupJobOutcome::Ran(result) = &outcome else {
            panic!("{outcome:?}")
        };
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        assert_eq!(host.recorded.lock().unwrap().len(), 1);
    }

    fn result_with(success: bool, created: usize, sent: usize, cleaned: usize) -> BackupResult {
        BackupResult {
            success,
            mode: BackupMode::Full,
            snapshots_created: Some(created),
            snapshots_sent: Some(sent),
            snapshots_cleaned: cleaned,
            bytes_sent: 0,
            boot: BootStep::Ran(BootOutcome {
                updated: 1,
                ..Default::default()
            }),
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

    fn quiet_boot(mut r: BackupResult) -> BackupResult {
        r.boot = BootStep::Ran(BootOutcome::default());
        r
    }

    fn with_boot(boot: BootStep) -> BackupResult {
        let mut r = result_with(true, 0, 0, 0);
        r.boot = boot;
        r
    }

    #[test]
    fn a_boot_step_that_did_something_is_never_nothing_to_do() {
        let nothing = "Backup (full): nothing to do — all snapshots up to date";
        for boot in [BootStep::NotSelected, BootStep::DisabledInConfig] {
            assert_eq!(backup_summary(&with_boot(boot), false), nothing);
        }
        assert_eq!(
            backup_summary(&with_boot(BootStep::Ran(BootOutcome::default())), false),
            nothing,
            "ran OK, 0 updated"
        );
        assert_eq!(
            backup_summary(
                &with_boot(BootStep::Ran(BootOutcome {
                    updated: 1,
                    ..Default::default()
                })),
                false
            ),
            "Backup succeeded (full): 0 snapshots created, 0 sent, boot subvolumes: OK  (1 updated, 0 skipped)"
        );
        assert_eq!(
            backup_summary(
                &with_boot(BootStep::Ran(BootOutcome {
                    warnings: vec!["w".into()],
                    ..Default::default()
                })),
                false
            ),
            "Backup succeeded (full): 0 snapshots created, 0 sent, boot subvolumes: WARN  (0 updated, 0 skipped, 1 warnings)"
        );
        assert_eq!(
            backup_summary(
                &with_boot(BootStep::Ran(BootOutcome {
                    failures: vec!["f".into()],
                    ..Default::default()
                })),
                false
            ),
            "Backup succeeded (full): 0 snapshots created, 0 sent, boot subvolumes: FAIL  (0 updated, 1 failed)"
        );
    }

    #[test]
    fn the_job_end_text_carries_boot_warnings_and_failures_failures_first() {
        let ran = |o: BootOutcome| {
            BackupJobOutcome::Ran(with_boot(BootStep::Ran(o)))
                .finish_line(false)
                .1
        };
        assert_eq!(
            ran(BootOutcome {
                warnings: vec!["w1".into()],
                ..Default::default()
            }),
            "Backup succeeded (full): 0 snapshots created, 0 sent, boot subvolumes: WARN  (0 updated, 0 skipped, 1 warnings)\nboot WARN: w1"
        );
        let many = ran(BootOutcome {
            warnings: (0..6).map(|i| format!("w{i}")).collect(),
            failures: vec!["f0".into()],
            ..Default::default()
        });
        let lines: Vec<&str> = many.lines().skip(1).collect();
        assert_eq!(
            lines,
            [
                "boot FAIL: f0",
                "boot WARN: w0",
                "boot WARN: w1",
                "boot WARN: w2",
                "boot WARN: w3",
                "and 2 more"
            ]
        );
        // A clean or absent boot step adds nothing, and neither does a dry run.
        assert!(!ran(BootOutcome::default()).contains('\n'));
        assert!(
            !BackupJobOutcome::Ran(with_boot(BootStep::Ran(BootOutcome {
                warnings: vec!["w".into()],
                ..Default::default()
            })))
            .finish_line(true)
            .1
            .contains('\n')
        );
    }

    #[test]
    fn a_summary_never_prints_an_unknown_count_as_a_number() {
        let mut result = result_with(false, 0, 0, 0);
        result.snapshots_created = None;
        result.snapshots_sent = None;
        assert_eq!(
            backup_summary(&result, false),
            "Backup completed with errors (full): snapshots created: unknown, sent: unknown, \
             boot subvolumes: OK  (1 updated, 0 skipped) — a; b"
        );
        // A run whose counts are unknown is never "nothing to do", even clean
        // and even when the other count is a measured zero.
        result.success = true;
        result.errors.clear();
        for (created, sent) in [(None, None), (None, Some(0)), (Some(0), None)] {
            result.snapshots_created = created;
            result.snapshots_sent = sent;
            let line = backup_summary(&result, false);
            assert!(
                line.contains("unknown") && !line.contains("nothing to do"),
                "{created:?} {sent:?}: {line}"
            );
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
            backup_summary(&quiet_boot(result_with(true, 0, 0, 0)), false),
            "Backup (full): nothing to do — all snapshots up to date"
        );
        assert_eq!(
            backup_summary(&result_with(true, 2, 0, 0), false),
            "Backup succeeded (full): 2 snapshots created, 0 sent, boot subvolumes: OK  (1 updated, 0 skipped)"
        );
        assert_eq!(
            backup_summary(&result_with(true, 0, 3, 0), false),
            "Backup succeeded (full): 0 snapshots created, 3 sent, boot subvolumes: OK  (1 updated, 0 skipped)"
        );
        assert_eq!(
            backup_summary(&result_with(true, 0, 0, 4), false),
            "Backup succeeded (full): 0 snapshots created, 0 sent, 4 cleaned up, boot subvolumes: OK  (1 updated, 0 skipped)"
        );
        assert_eq!(
            backup_summary(&result_with(false, 0, 0, 0), false),
            "Backup completed with errors (full): 0 snapshots created, 0 sent, boot subvolumes: OK  (1 updated, 0 skipped) — a; b"
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
        // Email not ticked: written, not mailed, even with [email] enabled.
        config.email.enabled = true;
        let not_asked = BackupOptions::default();
        assert!(
            !host
                .report(&config, &not_asked, &result, &data, &progress)
                .emailed
        );
        assert!(report.exists(), "email not ticked: still written");
        std::fs::remove_file(&report).unwrap();

        // Asked for, email disabled: written (as backup-run.sh does), not mailed.
        config.email.enabled = false;
        let ask = BackupOptions {
            email_report: true,
            ..Default::default()
        };
        assert!(
            !host
                .report(&config, &ask, &result, &data, &progress)
                .emailed
        );
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
        assert!(
            !host
                .report(&config, &ask, &result, &data, &progress)
                .emailed
        );
        assert!(tried_to_mail(&progress), "email enabled: the send is tried");
        let text = std::fs::read_to_string(&report).unwrap();
        assert!(text.contains("a"), "{text}");
    }

    #[test]
    fn the_report_is_written_whether_or_not_it_is_emailed() {
        for email_report in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let mut config = make_test_config();
            config.general.last_report = dir.path().join("last-report.txt").display().to_string();
            config.email.enabled = false; // nothing is mailed in a test
            let options = BackupOptions {
                email_report,
                ..Default::default()
            };
            let data = crate::report::ReportData {
                capacity_and_smart: String::new(),
                latest_snapshots: String::new(),
            };
            let delivery = deliver_report(
                &config,
                &options,
                &result_with(true, 1, 1, 0),
                &data,
                &TestProgress::new(),
            );
            assert_eq!(delivery.saved, Ok(()), "email_report={email_report}");
            assert!(
                Path::new(&config.general.last_report).is_file(),
                "email_report={email_report}: written"
            );
            assert!(!delivery.emailed);
        }
    }

    #[test]
    fn an_unticked_email_is_not_mailed_even_with_email_enabled() {
        let mut config = make_test_config();
        config.email.enabled = true;
        let options = BackupOptions {
            email_report: false,
            ..Default::default()
        };
        assert!(!emails_report(&options, &config));
        let options = BackupOptions {
            email_report: true,
            ..Default::default()
        };
        assert!(emails_report(&options, &config));
    }

    #[test]
    fn a_report_that_cannot_be_written_is_lost_when_nothing_mails_it_and_says_where() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = make_test_config();
        // A file where the report's directory should be.
        std::fs::write(dir.path().join("blocker"), b"x").unwrap();
        let path = dir.path().join("blocker/last-report.txt");
        config.general.last_report = path.to_string_lossy().into_owned();
        config.email.enabled = false;
        let options = BackupOptions {
            email_report: true,
            ..Default::default()
        };
        let data = crate::report::ReportData {
            capacity_and_smart: String::new(),
            latest_snapshots: String::new(),
        };
        let progress = TestProgress::new();
        let delivery = deliver_report(
            &config,
            &options,
            &result_with(true, 1, 1, 0),
            &data,
            &progress,
        );
        assert!(!delivery.emailed);
        let why = delivery.lost().expect("email off and unwritable: lost");
        assert!(
            why.starts_with(&format!("Failed to save report to {}", path.display())),
            "{why}"
        );
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
            email_report: true,
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
