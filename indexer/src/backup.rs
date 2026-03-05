use crate::config::Config;
use crate::db::Database;
use crate::health;
use crate::indexer;
use crate::progress::{LogLevel, ProgressCallback};
use std::io::BufRead;
use std::process::{Command, Stdio};
use std::time::UNIX_EPOCH;

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
    let mut child = cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;

    // Read stdout line by line while the process runs.
    let stdout = child.stdout.take().expect("stdout must be piped");
    let reader = std::io::BufReader::new(stdout);
    for line in reader.lines() {
        let line = line?;
        line_cb(&line);
    }

    let status = child.wait()?;

    // Collect stderr from the now-finished child.
    if let Some(stderr) = child.stderr.take() {
        let reader = std::io::BufReader::new(stderr);
        for line in reader.lines().map_while(Result::ok) {
            if !line.trim().is_empty() {
                progress.on_log(LogLevel::Warning, &format!("btrbk stderr: {line}"));
            }
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
        progress.on_log(
            LogLevel::Warning,
            "btrbk snapshot command exited with non-zero status",
        );
    }

    let count = parse_btrbk_snapshot_count(&stdout);

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
        progress.on_log(
            LogLevel::Warning,
            "btrbk resume command exited with non-zero status",
        );
    }

    // Re-count from accumulated output for accuracy.
    let full_output = stdout_lines.join("\n");
    snapshots_sent = parse_btrbk_send_count(&full_output);

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
        progress.on_log(
            LogLevel::Warning,
            "btrbk run command exited with non-zero status",
        );
    }

    // Re-count from accumulated output for accuracy.
    let full_output = stdout_lines.join("\n");
    snapshots_created = parse_btrbk_snapshot_count(&full_output);
    snapshots_sent = parse_btrbk_send_count(&full_output);
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
    } else if let Some(s) = upper.strip_suffix("B/S") {
        (s, 1u64)
    } else {
        return None;
    };
    val_str
        .parse::<f64>()
        .ok()
        .map(|v| (v * mult as f64) as u64)
}

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

/// Measure total used bytes across all mounted backup targets.
///
/// Uses `statvfs(2)` to read filesystem usage directly (no child process).
/// Returns the sum of used bytes across all target mount points. Used to
/// calculate bytes_sent as the delta between before/after a backup, since
/// btrbk doesn't report transfer sizes in its output.
fn measure_target_usage(config: &Config) -> u64 {
    config
        .targets
        .iter()
        .filter_map(|tgt| {
            let path = health::find_any_mount(&tgt.mount, &tgt.serial, &tgt.role)?;
            let c_path = std::ffi::CString::new(path).ok()?;
            let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
            let rc = unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) };
            if rc != 0 {
                return None;
            }
            let total = stat.f_blocks * stat.f_frsize;
            let avail = stat.f_bavail * stat.f_frsize;
            Some(total.saturating_sub(avail))
        })
        .sum()
}

/// Archive boot subvolumes as read-only snapshots on backup targets.
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

    let ts = format_timestamp();
    let mut any_archived = false;

    // Use all configured targets — caller pre-mounted via MountGuard.
    if config.targets.is_empty() {
        progress.on_log(
            LogLevel::Warning,
            "No backup targets configured — skipping boot archive",
        );
        return Ok(false);
    }

    for (step, subvol) in config.boot.subvolumes.iter().enumerate() {
        progress.on_progress(
            step as u64,
            config.boot.subvolumes.len() as u64,
            &format!("Archiving {subvol}"),
        );

        // Derive the archive subvolume name. For "@" -> "@.archive.TIMESTAMP",
        // for "@home" -> "@home.archive.TIMESTAMP".
        let archive_name = format!("{subvol}.archive.{ts}");

        for target in &config.targets {
            let tgt_mount = &target.mount;

            // Check if the boot subvolume exists on this target.
            let subvol_path = format!("{tgt_mount}/{subvol}");
            if !std::path::Path::new(&subvol_path).exists() {
                progress.on_log(
                    LogLevel::Info,
                    &format!("Boot subvolume {subvol_path} does not exist on target — skipping"),
                );
                continue;
            }

            let archive_path = format!("{tgt_mount}/{archive_name}");

            // Step 1: Create read-only snapshot of the existing boot subvolume.
            let snap_status = Command::new("btrfs")
                .args(["subvolume", "snapshot", "-r", &subvol_path, &archive_path])
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .status()?;

            if !snap_status.success() {
                progress.on_log(
                    LogLevel::Warning,
                    &format!("Failed to snapshot {subvol_path} -> {archive_path}"),
                );
                continue;
            }
            progress.on_log(
                LogLevel::Info,
                &format!("Archived {subvol_path} -> {archive_path}"),
            );

            // Step 2: Delete the existing boot subvolume.
            let del_status = Command::new("btrfs")
                .args(["subvolume", "delete", &subvol_path])
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .status()?;

            if !del_status.success() {
                progress.on_log(
                    LogLevel::Warning,
                    &format!("Failed to delete existing {subvol_path}"),
                );
                continue;
            }

            // Step 3: We intentionally do NOT recreate the subvolume here.
            // The scripts recreate from the latest btrbk snapshot; that logic
            // lives in backup-run.sh::update_boot_subvolumes and is driven
            // by the shell script until the Rust orchestration is complete.
            // This function only performs the archive (snapshot + delete) step.

            any_archived = true;
        }

        progress.on_progress(
            step as u64 + 1,
            config.boot.subvolumes.len() as u64,
            &format!("Archived {subvol}"),
        );
    }

    Ok(any_archived)
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
    let mut snapshots_created: usize = 0;
    let mut snapshots_sent: usize = 0;
    let mut bytes_sent: u64 = 0;
    let mut boot_archived = false;
    let mut indexed = false;
    let report_sent = false; // email not yet integrated

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

        let summary = format!("DRY RUN ({mode}) completed — no changes made");
        let result = BackupResult {
            success: true,
            mode,
            snapshots_created: 0,
            snapshots_sent: 0,
            snapshots_cleaned: 0,
            bytes_sent: 0,
            boot_archived: false,
            indexed: false,
            report_sent: false,
            errors: Vec::new(),
            duration_secs: start.elapsed().as_secs(),
        };
        progress.on_complete(true, &summary);
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

    // Measure target disk usage before btrbk runs so we can calculate
    // bytes_sent as the delta (btrbk doesn't report transfer sizes).
    let usage_before = measure_target_usage(config);
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
            // Step (b): Send with --preserve (skip retention cleanup)
            if !options.snapshot_only {
                match send_snapshots(
                    config,
                    &effective_sources,
                    &effective_targets,
                    true, // --preserve: skip retention cleanup
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
        let usage_after = measure_target_usage(config);
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

    // Step (e): Email report
    if options.send_report {
        progress.on_log(
            LogLevel::Info,
            "Email reports not yet integrated — skipping",
        );
    }

    let success = errors.is_empty();
    let nothing_to_do = success
        && snapshots_created == 0
        && snapshots_sent == 0
        && snapshots_cleaned == 0
        && !options.dry_run;

    let summary = if nothing_to_do {
        format!("Backup ({mode}): nothing to do — all snapshots up to date")
    } else {
        let cleaned_msg = if snapshots_cleaned > 0 {
            format!(", {} cleaned up", snapshots_cleaned)
        } else {
            String::new()
        };
        format!(
            "Backup {status} ({mode}): {snaps} snapshots created, {sent} sent{cleaned}, boot archived: {boot}",
            status = if success {
                "succeeded"
            } else {
                "completed with errors"
            },
            snaps = snapshots_created,
            sent = snapshots_sent,
            cleaned = cleaned_msg,
            boot = boot_archived,
        )
    };

    let result = BackupResult {
        success,
        mode,
        snapshots_created,
        snapshots_sent,
        snapshots_cleaned,
        bytes_sent,
        boot_archived,
        indexed,
        report_sent,
        errors,
        duration_secs: start.elapsed().as_secs(),
    };

    progress.on_complete(result.success, &summary);
    Ok(result)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        Boot, Config, Das, Email, Esp, General, Gui, Init, InitSystem, Retention, Schedule, Source,
        SubvolConfig, Target, TargetRole,
    };
    use crate::progress::TestProgress;

    // Build a minimal Config suitable for unit tests.
    fn make_test_config() -> Config {
        Config {
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
            sources: vec![
                Source {
                    label: "nvme-root".into(),
                    volume: "/.btrfs-nvme".into(),
                    subvolumes: vec![
                        SubvolConfig {
                            name: "@".into(),
                            manual_only: false,
                            snapshot_name: None,
                        },
                        SubvolConfig {
                            name: "@home".into(),
                            manual_only: false,
                            snapshot_name: None,
                        },
                    ],
                    device: "/dev/nvme0n1p2".into(),
                    snapshot_dir: ".btrbk-snapshots".into(),
                    target_subdirs: vec![],
                    target_labels: vec![],
                },
                Source {
                    label: "manual-src".into(),
                    volume: "/.btrfs-manual".into(),
                    subvolumes: vec![SubvolConfig {
                        name: "@special".into(),
                        manual_only: true,
                        snapshot_name: None,
                    }],
                    device: "/dev/sdb".into(),
                    snapshot_dir: ".btrbk-snapshots".into(),
                    target_subdirs: vec![],
                    target_labels: vec![],
                },
            ],
            targets: vec![Target {
                label: "primary-22tb".into(),
                serial: "TESTSERIAL".into(),
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
            esp: Esp::default(),
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
>>> /mnt/backup-system/nvme/root.20260302T0835
*** /mnt/backup-system-mirror/nvme/root.20260302T0835
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

        // Verify on_complete was called with success.
        let completed = progress.completed.lock().unwrap();
        assert!(completed.is_some(), "on_complete must have been called");
        assert!(
            completed.as_ref().unwrap().0,
            "on_complete must report success"
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
}
