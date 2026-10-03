use crate::config::{Config, TargetRole};
use crate::db::Database;
use crate::health;
use crate::indexer;
use crate::maintenance::{HoldsMaintenance, MaintenanceHeld};
use crate::mount;
use crate::progress::{LogLevel, ProgressCallback};
use crate::scrub;
use std::io::BufRead;
use std::path::Path;
use std::process::{Command, Stdio};
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
    /// Source labels to back up. Empty = all configured sources.
    pub sources: Vec<String>,
    /// Target labels to send to. Empty = all available targets.
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

/// Ensure source top-level volumes are mounted (subvolid=5).
///
/// btrbk needs the raw BTRFS volume mounted to see subvolumes.  The backup
/// shell script (`backup-run.sh`) does this, but the Rust CLI/GUI code path
/// calls btrbk directly.  This function mounts any unmounted source volumes.
fn ensure_sources_mounted(
    config: &Config,
    progress: &dyn ProgressCallback,
) -> Result<(), Box<dyn std::error::Error>> {
    use std::collections::HashSet;
    let mut seen = HashSet::new();
    for src in &config.sources {
        if !seen.insert((&src.volume, &src.device)) {
            continue;
        }
        let mount_path = std::path::Path::new(&src.volume);
        if !mount_path.exists() {
            std::fs::create_dir_all(mount_path)?;
        }
        // Check if already mounted
        let check = Command::new("mountpoint")
            .arg("-q")
            .arg(&src.volume)
            .status();
        if check.map(|s| s.success()).unwrap_or(false) {
            continue;
        }
        progress.on_log(
            LogLevel::Info,
            &format!("Mounting source volume {} from {}", src.volume, src.device),
        );
        let status = Command::new("mount")
            .arg("-o")
            .arg("subvolid=5")
            .arg(&src.device)
            .arg(&src.volume)
            .status()?;
        if !status.success() {
            return Err(format!(
                "Failed to mount source volume {} from {}",
                src.volume, src.device
            )
            .into());
        }
    }
    Ok(())
}

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
fn btrbk_raw_listing(config: &Config) -> Option<String> {
    let output = Command::new("btrbk")
        .args([
            "-c",
            &config.general.btrbk_conf,
            "--format=raw",
            "list",
            "latest",
        ])
        .output()
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

/// Run a command and return (stdout, stderr, success).
/// Logs stderr lines at Warning level via progress.
fn run_command(
    cmd: &mut Command,
    progress: &dyn ProgressCallback,
) -> Result<(String, bool), Box<dyn std::error::Error>> {
    let output = cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).output()?;

    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

    for line in stderr.lines() {
        if !line.trim().is_empty() {
            progress.on_log(LogLevel::Warning, &format!("btrbk stderr: {line}"));
        }
    }

    Ok((stdout, output.status.success()))
}

/// Stream a command line by line, applying a callback to each stdout line.
/// Stderr is collected and logged at Warning level. Returns success status.
fn stream_command<F>(
    cmd: &mut Command,
    progress: &dyn ProgressCallback,
    mut line_cb: F,
) -> Result<bool, Box<dyn std::error::Error>>
where
    F: FnMut(&str),
{
    // Log the command being executed for diagnostics.
    progress.on_log(LogLevel::Info, &format!("stream_command: {:?}", cmd));

    let mut child = cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;

    // stderr MUST be drained concurrently with stdout, not after the child
    // exits. A pipe buffer is ~64 KiB on Linux; once it fills, the child blocks
    // in write(2), therefore stops producing stdout, therefore the stdout loop
    // below blocks forever on a line that never comes and `wait()` is never
    // reached. `run_command` avoids this by using `Command::output()`, which
    // drains both streams on separate threads internally; this is the same
    // thing, done by hand because we stream stdout line by line
    // (bd DAS-Backup-Manager-az3).
    let stderr = child.stderr.take().expect("stderr must be piped");
    let stderr_thread = std::thread::spawn(move || {
        std::io::BufReader::new(stderr)
            .lines()
            .map_while(Result::ok)
            .collect::<Vec<String>>()
    });

    // Read stdout line by line while the process runs.
    let stdout = child.stdout.take().expect("stdout must be piped");
    let reader = std::io::BufReader::new(stdout);
    let mut line_count = 0usize;
    for line in reader.lines() {
        let line = line?;
        line_count += 1;
        line_cb(&line);
    }

    let status = child.wait()?;
    progress.on_log(
        LogLevel::Info,
        &format!(
            "stream_command: exit={}, stdout_lines={}",
            status.code().unwrap_or(-1),
            line_count
        ),
    );

    // The reader thread ends at stderr EOF, which the exit above guarantees.
    for line in stderr_thread.join().unwrap_or_default() {
        if !line.trim().is_empty() {
            progress.on_log(LogLevel::Warning, &format!("btrbk stderr: {line}"));
        }
    }

    Ok(status.success())
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Create btrbk snapshots for specified sources.
pub fn create_snapshots(
    config: &Config,
    sources: &[String],
    progress: &dyn ProgressCallback,
) -> Result<usize, Box<dyn std::error::Error>> {
    progress.on_stage("Creating snapshots", sources.len() as u64);

    let mut cmd = Command::new("btrbk");
    cmd.arg("-c").arg(&config.general.btrbk_conf);

    // btrbk syntax: `btrbk -c <conf> snapshot [<volume-path>...]`
    // The "snapshot" subcommand must appear exactly once, followed by volume
    // paths as optional filter arguments.
    cmd.arg("snapshot");

    if !sources.is_empty() {
        // Collect unique volume paths — multiple sources can share a volume
        // (e.g. hdd-projects and hdd-audiobooks both use /.btrfs-hdd).
        let mut seen_volumes = std::collections::HashSet::new();
        for label in sources {
            if let Some(src) = config.sources.iter().find(|s| &s.label == label) {
                if seen_volumes.insert(src.volume.clone()) {
                    progress.on_log(
                        LogLevel::Info,
                        &format!("Snapshotting source '{}' at {}", label, src.volume),
                    );
                    cmd.arg(&src.volume);
                } else {
                    progress.on_log(
                        LogLevel::Info,
                        &format!(
                            "Source '{}' shares volume {} (already included)",
                            label, src.volume
                        ),
                    );
                }
            } else {
                progress.on_log(
                    LogLevel::Warning,
                    &format!("Source label '{label}' not found in config — skipping"),
                );
            }
        }
    }

    let (stdout, success) = run_command(&mut cmd, progress)?;

    if !success {
        // Must be an Err, not a Warning. Every caller turns Err into an entry
        // in `errors`, and run_backup derives `success = errors.is_empty()`,
        // so a Warning here left the run reporting SUCCESS to the CLI exit
        // code, the GUI JobFinished flag, the DB history row and the email
        // subject alike. bd DAS-Backup-Manager-nsp (finding #1/#2) — the same
        // shape as bd oi0, where the outcome was decided by whether anything
        // pushed an error and nothing ever did.
        let msg = "btrbk snapshot command exited with non-zero status. No snapshots can be assumed created.";
        progress.on_log(LogLevel::Error, msg);
        return Err(msg.into());
    }

    // Prefer the machine-readable listing; fall back to the marker parse only
    // if btrbk cannot be queried (bd DAS-Backup-Manager-06p).
    let count = match btrbk_raw_listing(config) {
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
    progress.on_stage("Sending snapshots", 1);

    let mut cmd = Command::new("btrbk");
    if preserve {
        cmd.arg("--preserve");
    }
    cmd.arg("-c").arg(&config.general.btrbk_conf);

    // Use `resume` to handle interrupted transfers gracefully.
    cmd.arg("resume");

    // Add source volume path filters if requested (deduplicate shared volumes).
    if !sources.is_empty() {
        let mut seen_volumes = std::collections::HashSet::new();
        for label in sources {
            if let Some(src) = config.sources.iter().find(|s| &s.label == label)
                && seen_volumes.insert(src.volume.clone())
            {
                cmd.arg(&src.volume);
            }
        }
    }

    // Note: target mount paths (e.g. /mnt/backup-22tb) are NOT passed as
    // btrbk filter arguments.  btrbk expects exact matches to the configured
    // target *directories* (e.g. /mnt/backup-22tb/nvme), not the top-level
    // mount point.  Source volume paths already limit which data is processed,
    // and btrbk automatically skips targets whose paths don't exist.
    //
    // Log which targets are expected so the user knows the scope.
    for label in targets {
        if let Some(tgt) = config.targets.iter().find(|t| &t.label == label) {
            if let Some(actual) = health::find_any_mount(&tgt.mount, &tgt.serial, &tgt.role) {
                progress.on_log(
                    LogLevel::Info,
                    &format!("Target '{label}' mounted at {actual} — will receive"),
                );
            } else {
                progress.on_log(
                    LogLevel::Warning,
                    &format!(
                        "Target '{label}' at {} is not mounted — btrbk will skip",
                        tgt.mount
                    ),
                );
            }
        }
    }

    let mut snapshots_sent: usize = 0;
    let mut bytes_sent: u64 = 0;
    let mut stdout_lines = Vec::new();

    let success = stream_command(&mut cmd, progress, |line| {
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

    if !success {
        // Must be an Err, not a Warning. Every caller turns Err into an entry
        // in `errors`, and run_backup derives `success = errors.is_empty()`,
        // so a Warning here left the run reporting SUCCESS to the CLI exit
        // code, the GUI JobFinished flag, the DB history row and the email
        // subject alike. bd DAS-Backup-Manager-nsp (finding #1/#2) — the same
        // shape as bd oi0, where the outcome was decided by whether anything
        // pushed an error and nothing ever did.
        let msg = "btrbk resume command exited with non-zero status. The send may be incomplete.";
        progress.on_log(LogLevel::Error, msg);
        return Err(msg.into());
    }

    // Count from the machine-readable listing, not the human output.
    let full_output = stdout_lines.join("\n");
    snapshots_sent = match btrbk_raw_listing(config) {
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
/// Full backup mode — equivalent to what the nightly bash script does.
///
/// Returns (snapshots_created, snapshots_sent, snapshots_cleaned, bytes_sent).
pub fn run_full_pipeline(
    config: &Config,
    sources: &[String],
    targets: &[String],
    progress: &dyn ProgressCallback,
) -> Result<(usize, usize, usize, u64), Box<dyn std::error::Error>> {
    progress.on_stage("Full backup (snapshot + send + cleanup)", 1);

    let mut cmd = Command::new("btrbk");
    cmd.arg("-c").arg(&config.general.btrbk_conf);
    cmd.arg("run");

    // Add source volume path filters (deduplicate shared volumes).
    if !sources.is_empty() {
        let mut seen_volumes = std::collections::HashSet::new();
        for label in sources {
            if let Some(src) = config.sources.iter().find(|s| &s.label == label)
                && seen_volumes.insert(src.volume.clone())
            {
                progress.on_log(
                    LogLevel::Info,
                    &format!("Source '{}' at {}", label, src.volume),
                );
                cmd.arg(&src.volume);
            }
        }
    }

    // Log target mount status.
    for label in targets {
        if let Some(tgt) = config.targets.iter().find(|t| &t.label == label) {
            if let Some(actual) = health::find_any_mount(&tgt.mount, &tgt.serial, &tgt.role) {
                progress.on_log(
                    LogLevel::Info,
                    &format!("Target '{label}' mounted at {actual} — will receive"),
                );
            } else {
                progress.on_log(
                    LogLevel::Warning,
                    &format!(
                        "Target '{label}' at {} is not mounted — btrbk will skip",
                        tgt.mount
                    ),
                );
            }
        }
    }

    let mut snapshots_created: usize = 0;
    let mut snapshots_sent: usize = 0;
    let mut bytes_sent: u64 = 0;
    let mut stdout_lines = Vec::new();

    let success = stream_command(&mut cmd, progress, |line| {
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

    if !success {
        // Must be an Err, not a Warning. Every caller turns Err into an entry
        // in `errors`, and run_backup derives `success = errors.is_empty()`,
        // so a Warning here left the run reporting SUCCESS to the CLI exit
        // code, the GUI JobFinished flag, the DB history row and the email
        // subject alike. bd DAS-Backup-Manager-nsp (finding #1/#2) — the same
        // shape as bd oi0, where the outcome was decided by whether anything
        // pushed an error and nothing ever did.
        let msg = "btrbk run command exited with non-zero status. The backup may be incomplete.";
        progress.on_log(LogLevel::Error, msg);
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

    match btrbk_raw_listing(config) {
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
    target_mount: &str,
    subdirs: &[String],
    snap_name: &str,
) -> Option<String> {
    let output = Command::new("btrfs")
        .args(["subvolume", "list", target_mount])
        .output()
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

/// Run a `btrfs` subcommand, returning whether it succeeded.
fn btrfs_ok(args: &[&str]) -> std::io::Result<bool> {
    Ok(Command::new("btrfs")
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .status()?
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
pub fn archive_boot(
    config: &Config,
    progress: &dyn ProgressCallback,
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

        for target in &config.targets {
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
            let Some(latest) = find_latest_btrbk_snapshot(tgt_mount, &subdirs, snap_name) else {
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
                if !btrfs_ok(&["subvolume", "snapshot", "-r", &subvol_path, &archive_path])? {
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
                && !btrfs_ok(&["subvolume", "delete", &staging])?
            {
                progress.on_log(
                    LogLevel::Warning,
                    &format!("Stale {staging} could not be removed — leaving {subvol} untouched"),
                );
                continue;
            }

            // Step 4: build the replacement ALONGSIDE the live subvolume.
            if !btrfs_ok(&["subvolume", "snapshot", &latest_path, &staging])? {
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
                && !btrfs_ok(&["subvolume", "delete", &subvol_path])?
            {
                progress.on_log(
                    LogLevel::Warning,
                    &format!("Failed to delete {subvol_path} — discarding {staging}"),
                );
                let _ = btrfs_ok(&["subvolume", "delete", &staging]);
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

/// Run a backup with the given options. Calls btrbk under the hood.
/// The caller must ensure this runs with appropriate privileges (root).
pub fn run_backup(
    config: &Config,
    options: &BackupOptions,
    progress: &dyn ProgressCallback,
) -> Result<BackupResult, Box<dyn std::error::Error>> {
    let start = std::time::Instant::now();

    let mut errors: Vec<String> = Vec::new();
    if options.subvolume_sync.as_ref().is_some_and(|s| s.failed) {
        errors.push("Subvolume sync failed — see SUBVOLUME SYNC in the report".into());
    }
    let mut snapshots_created: usize = 0;
    let mut snapshots_sent: usize = 0;
    let mut bytes_sent: u64 = 0;
    let mut boot_archived = false;
    let mut indexed = false;

    // ---------- Resolve effective sources ----------

    // Exclude manual_only subvolumes unless explicitly requested.
    let effective_sources: Vec<String> = if options.sources.is_empty() {
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
    };

    // ---------- Resolve effective targets ----------
    //
    // When targets are explicitly specified (D-Bus helper pre-mounts them),
    // trust the caller — don't re-check mount status.  Only auto-detect
    // mounted targets when the caller leaves the list empty (standalone CLI).

    let effective_targets: Vec<String> = if options.targets.is_empty() {
        config
            .targets
            .iter()
            .filter(|tgt| health::find_any_mount(&tgt.mount, &tgt.serial, &tgt.role).is_some())
            .map(|tgt| tgt.label.clone())
            .collect()
    } else {
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
            config
                .targets
                .iter()
                .filter(|tgt| health::find_any_mount(&tgt.mount, &tgt.serial, &tgt.role).is_some())
                .map(|tgt| tgt.label.clone())
                .collect()
        } else {
            matched
        }
    };

    // Require at least one target (unless dry-run).
    if effective_targets.is_empty() && !options.dry_run {
        return Err("No backup targets are mounted. Connect the DAS enclosure and mount targets before running.".into());
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
    mount::verify_write_targets(&config.targets, &effective_targets, progress)?;

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

    // Mount source top-level volumes (subvolid=5) so btrbk can see subvolumes.
    ensure_sources_mounted(config, progress)?;

    // Measure target disk usage before btrbk runs so we can calculate
    // bytes_sent as the delta (btrbk doesn't report transfer sizes).
    // Sync first so both before/after measurements use committed metadata.
    sync_targets(config);
    let usage_before = measure_target_usage(config, progress);
    progress.on_log(
        LogLevel::Info,
        &format!("Target usage before: {} bytes", usage_before),
    );

    let mut snapshots_cleaned: usize = 0;

    match mode {
        BackupMode::Full => {
            if options.snapshot_only {
                // Full + snapshot-only: just create snapshots (same as incremental).
                match create_snapshots(config, &effective_sources, progress) {
                    Ok(n) => snapshots_created = n,
                    Err(e) => {
                        let msg = format!("Snapshot step failed: {e}");
                        progress.on_log(LogLevel::Error, &msg);
                        errors.push(msg);
                    }
                }
            } else if options.send_only {
                // Full + send-only: send with retention cleanup (no --preserve).
                match send_snapshots(
                    config,
                    &effective_sources,
                    &effective_targets,
                    false, // no preserve → btrbk enforces retention
                    progress,
                ) {
                    Ok((sent, bytes)) => {
                        snapshots_sent = sent;
                        bytes_sent = bytes;
                    }
                    Err(e) => {
                        let msg = format!("Send step failed: {e}");
                        progress.on_log(LogLevel::Error, &msg);
                        errors.push(msg);
                    }
                }
            } else {
                // Full: btrbk run does snapshot + send + cleanup atomically.
                match run_full_pipeline(config, &effective_sources, &effective_targets, progress) {
                    Ok((snaps, sent, cleaned, bytes)) => {
                        snapshots_created = snaps;
                        snapshots_sent = sent;
                        snapshots_cleaned = cleaned;
                        bytes_sent = bytes;
                    }
                    Err(e) => {
                        let msg = format!("Full backup pipeline failed: {e}");
                        progress.on_log(LogLevel::Error, &msg);
                        errors.push(msg);
                    }
                }
            }
        }
        BackupMode::Incremental => {
            // Step (a): Snapshots
            if !options.send_only {
                match create_snapshots(config, &effective_sources, progress) {
                    Ok(n) => snapshots_created = n,
                    Err(e) => {
                        let msg = format!("Snapshot step failed: {e}");
                        progress.on_log(LogLevel::Error, &msg);
                        errors.push(msg);
                    }
                }
            }
            // Step (b): Send with retention cleanup (same as full mode).
            // Both incremental and full modes enforce retention policy to
            // prevent targets from filling up.
            if !options.snapshot_only {
                match send_snapshots(
                    config,
                    &effective_sources,
                    &effective_targets,
                    false, // enforce retention cleanup on every backup
                    progress,
                ) {
                    Ok((sent, bytes)) => {
                        snapshots_sent = sent;
                        bytes_sent = bytes;
                    }
                    Err(e) => {
                        let msg = format!("Send step failed: {e}");
                        progress.on_log(LogLevel::Error, &msg);
                        errors.push(msg);
                    }
                }
            }
        }
    }

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
        match archive_boot(config, progress) {
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

        let result = run_backup(&config, &options, &progress)
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

        let result = run_backup(&config, &options, &progress);
        assert!(result.is_err(), "must fail when no targets are mounted");
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.to_lowercase().contains("no backup targets"),
            "error message must mention targets, got: '{err_msg}'"
        );
    }

    // -----------------------------------------------------------------
    // Source filtering: manual_only excluded by default
    // -----------------------------------------------------------------

    #[test]
    fn test_source_filtering_excludes_manual_only() {
        let config = make_test_config();

        // When sources is empty, effective_sources should exclude "manual-src"
        // because all its subvolumes are manual_only = true.
        let effective: Vec<String> = if config.sources.is_empty() {
            vec![]
        } else {
            config
                .sources
                .iter()
                .filter(|src| src.subvolumes.iter().any(|sv| !sv.manual_only))
                .map(|src| src.label.clone())
                .collect()
        };

        assert!(
            effective.contains(&"nvme-root".to_string()),
            "nvme-root (has non-manual subvols) must be included"
        );
        assert!(
            !effective.contains(&"manual-src".to_string()),
            "manual-src (all subvols are manual_only) must be excluded"
        );
    }

    #[test]
    fn test_source_filtering_explicit_override() {
        // When sources is explicitly set, manual_only restriction is bypassed.
        let explicit_sources = vec!["manual-src".to_string()];
        // Simulate what run_backup does when options.sources is non-empty.
        let effective = explicit_sources.clone();

        assert!(
            effective.contains(&"manual-src".to_string()),
            "explicitly requested manual-src must be included"
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
            let _ = tx.send(stream_command(&mut cmd, &progress, |_| {}).is_ok());
        });
        match rx.recv_timeout(std::time::Duration::from_secs(20)) {
            Ok(ok) => assert!(ok, "stream_command reported failure"),
            Err(_) => panic!("stream_command deadlocked on a large stderr write"),
        }
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
        let result = archive_boot(&config, &progress).expect("must not error");
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
        archive_boot(&config, &progress).expect("must not error");
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
        let result = archive_boot(&config, &progress);
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
        let result = run_backup(&config, &options, &progress).unwrap();
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
        let result = run_backup(&config, &options, &progress).unwrap();
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
        let result = run_backup(&config, &options, &TestProgress::new()).unwrap();
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
        let result = host.run(&make_test_config(), &options, &progress).unwrap();
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
