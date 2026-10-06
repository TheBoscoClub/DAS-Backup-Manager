mod setup;

use buttered_dasd::backup::{self, BackupLockAttempt, BackupMode, BackupOptions};
use buttered_dasd::config::Config;
use buttered_dasd::db::Database;
use buttered_dasd::forget;
use buttered_dasd::health::{self, HealthStatus};
use buttered_dasd::indexer;
use buttered_dasd::maintenance::HoldsMaintenance;
use buttered_dasd::mount;
use buttered_dasd::progress::{LogLevel, ProgressCallback};
use buttered_dasd::reconcile;
use buttered_dasd::report;
use buttered_dasd::{doctor, maintenance, restore, schedule, scrub, subvol};
use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::{Shell, generate};
use std::path::{Path, PathBuf};

const DEFAULT_DB: &str = "/var/lib/das-backup/backup-index.db";
const DEFAULT_CONFIG: &str = "/etc/das-backup/config.toml";

// ---------------------------------------------------------------------------
// CLI progress callback — prints to stderr so stdout stays machine-parseable
// ---------------------------------------------------------------------------

struct CliProgress;

impl ProgressCallback for CliProgress {
    fn on_stage(&self, stage: &str, total_steps: u64) {
        eprintln!("=== {stage} ({total_steps} steps) ===");
    }

    fn on_progress(&self, current: u64, total: u64, message: &str) {
        eprintln!("  [{current}/{total}] {message}");
    }

    fn on_throughput(&self, bytes_per_sec: u64) {
        eprintln!("  throughput: {}/s", report::format_bytes(bytes_per_sec));
    }

    fn on_log(&self, level: LogLevel, message: &str) {
        match level {
            LogLevel::Debug => eprintln!("  [DEBUG] {message}"),
            LogLevel::Info => eprintln!("  [INFO]  {message}"),
            LogLevel::Warning => eprintln!("  [WARN]  {message}"),
            LogLevel::Error => eprintln!("  [ERROR] {message}"),
        }
    }

    fn on_complete(&self, success: bool, summary: &str) {
        if success {
            eprintln!("OK: {summary}");
        } else {
            eprintln!("FAILED: {summary}");
        }
    }
}

// ---------------------------------------------------------------------------
// CLI definition
// ---------------------------------------------------------------------------

#[derive(Parser)]
#[command(
    name = "btrdasd",
    version,
    about = "ButteredDASD — DAS backup manager with btrbk integration",
    long_about = "ButteredDASD manages BTRFS backups to Direct-Attached Storage (DAS).\n\n\
        Features: btrbk orchestration, content indexing with FTS5 search,\n\
        health monitoring, schedule management, and backup history tracking.",
    after_help = "Examples:\n  \
        btrdasd backup run              Run a full backup pipeline\n  \
        btrdasd backup run --dry-run    Preview without making changes\n  \
        btrdasd restore browse /mnt/backup/root.20260228T030000\n  \
        btrdasd health                  Show drive health and backup status\n  \
        btrdasd scrub status            Show last scrub result per DAS filesystem\n  \
        btrdasd doctor --check-drift    Find unbacked-up subvolumes (source vs config drift)\n  \
        btrdasd schedule show           Show backup schedule and next run times\n  \
        btrdasd search 'report*'        FTS5 search across all indexed files\n  \
        btrdasd subvol list             List all configured subvolumes"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,

    /// Machine-readable JSON output on all read commands
    #[arg(long, global = true)]
    json: bool,
}

/// Prune index rows for snapshots that have disappeared from mounted targets.
///
/// Callers MUST invoke this while the targets are mounted — `plan_reconcile`
/// refuses to prune anything under a root that is not a verified mountpoint, so
/// calling it with everything unmounted is safe but useless (bd DAS-Backup-Manager-cu8).
/// Shared driver for `forget` and `purge`: mount, plan, report, delete, prune.
///
/// Both commands differ only in how they select snapshots, so selection is the
/// closure and everything dangerous — the interlock, the dry-run gate, deleting
/// via `btrfs subvolume delete`, and pruning the index afterwards — is written
/// once.
fn run_deletion<F>(
    db: &str,
    config: &Path,
    json: bool,
    dry_run: bool,
    verb: &str,
    select: F,
) -> Result<(), Box<dyn std::error::Error>>
where
    F: FnOnce(&[buttered_dasd::db::Snapshot]) -> Result<forget::ForgetPlan, forget::ForgetRefusal>,
{
    let cfg = Config::load(config)?;

    // Deleting subvolumes on the targets is maintenance: same interlock.
    let locks = match reconcile::try_acquire_locks(&format!("btrdasd {verb}"))? {
        reconcile::LockAttempt::Acquired(locks) => locks,
        reconcile::LockAttempt::Deferred(why) => {
            println!("Deferred — {why}");
            return Ok(());
        }
    };

    let progress = CliProgress;
    let mut guard = mount::ensure_targets_mounted(&cfg, &progress, locks.maintenance())?;
    let database = Database::open(db)?;

    let outcome =
        (|| -> Result<(forget::ForgetPlan, usize, Vec<String>), Box<dyn std::error::Error>> {
            let snapshots = database.list_snapshots()?;
            let plan = match select(&snapshots) {
                Ok(p) => p,
                Err(forget::ForgetRefusal::LiveSeries(name)) => {
                    return Err(format!(
                        "refusing: the pattern also matches '{name}', a series btrbk is \
                     still backing up — narrow the pattern"
                    )
                    .into());
                }
                Err(forget::ForgetRefusal::NoMatch) => return Err("nothing matched".into()),
            };
            if dry_run {
                return Ok((plan, 0, Vec::new()));
            }
            let mut deleted = 0usize;
            let mut failures = Vec::new();
            for c in &plan.candidates {
                match forget::delete_subvolume(&c.path) {
                    Ok(()) => deleted += 1,
                    Err(e) => failures.push(format!("{}: {e}", c.path.display())),
                }
            }
            // Prune the index for what actually went, so search stops serving it.
            let gone: Vec<i64> = plan.candidates.iter().map(|c| c.id).collect();
            if deleted > 0 {
                database.prune_snapshots(&gone)?;
            }
            Ok((plan, deleted, failures))
        })();

    let still_mounted = guard.unmount(&progress);
    let (plan, deleted, failures) = outcome?;

    if json {
        println!(
            "{{\"verb\":\"{verb}\",\"dry_run\":{dry_run},\"matched\":{},\"series\":{},\"deleted\":{},\"failed\":{}}}",
            plan.candidates.len(),
            plan.series.len(),
            deleted,
            failures.len()
        );
    } else {
        println!(
            "Matched {} snapshots across {} series:",
            plan.candidates.len(),
            plan.series.len()
        );
        for s in &plan.series {
            println!("  {s}");
        }
        if dry_run {
            println!("\nDry run — nothing was deleted.");
        } else {
            println!("\nDeleted {deleted} snapshots; index rows pruned.");
            for f in &failures {
                eprintln!("  FAILED {f}");
            }
        }
    }
    mount::require_released(&still_mounted)?;
    Ok(())
}

/// Per-target reindex outcome: label, snapshots indexed, snapshots on disk.
type ReindexOutcome = Vec<(String, usize, usize, usize)>;

fn run_reconcile(
    database: &Database,
    cfg: &Config,
    dry_run: bool,
) -> rusqlite::Result<reconcile::PruneStats> {
    let roots: Vec<String> = cfg.targets.iter().map(|t| t.mount.clone()).collect();
    let mounted = reconcile::verified_mounted_roots(&roots);
    let snapshots = database.list_snapshots()?;
    let plan = reconcile::plan_reconcile(&snapshots, &mounted, &roots, reconcile::path_exists);
    if dry_run || plan.is_empty() {
        return Ok(reconcile::PruneStats::default());
    }
    database.prune_snapshots(&plan.doomed)
}

#[derive(Subcommand)]
enum Commands {
    /// Index all new snapshots on a backup target
    ///
    /// Waits for the DAS maintenance lock before mounting the targets
    /// (EXIT CODE 75 with --no-wait while it is held).
    Walk {
        /// Path to backup target mount point
        target: PathBuf,
        /// Path to SQLite database
        #[arg(long, default_value = DEFAULT_DB)]
        db: String,
        /// Path to config.toml (for auto-mounting targets)
        #[arg(long, default_value = DEFAULT_CONFIG)]
        config: PathBuf,
        /// Do not wait for the DAS maintenance lock: while a backup, scrub or
        /// other job holds it, exit 75 at once without mounting anything
        #[arg(long)]
        no_wait: bool,
    },
    /// Full-text search across indexed files
    Search {
        /// FTS5 search query (supports prefix: "report*")
        query: String,
        /// Path to SQLite database
        #[arg(long, default_value = DEFAULT_DB)]
        db: String,
        /// Maximum results to return
        #[arg(long, default_value = "50")]
        limit: i64,
    },
    /// List files in a specific snapshot
    List {
        /// Snapshot path or name.timestamp pattern
        snapshot: String,
        /// Path to SQLite database
        #[arg(long, default_value = DEFAULT_DB)]
        db: String,
    },
    /// Show database statistics
    Info {
        /// Path to SQLite database
        #[arg(long, default_value = DEFAULT_DB)]
        db: String,
    },
    /// Delete obsolete snapshots whose series name matches a pattern
    Forget {
        /// Glob over the snapshot SERIES name, e.g. 'Projects-old-name'
        pattern: String,
        #[arg(long, default_value = DEFAULT_DB)]
        db: String,
        #[arg(long, default_value = DEFAULT_CONFIG)]
        config: PathBuf,
        /// btrbk config, read to learn which series are still live
        #[arg(long, default_value = "/etc/btrbk/btrbk.conf")]
        btrbk_conf: PathBuf,
        /// Show the plan without deleting anything
        #[arg(long)]
        dry_run: bool,
    },
    /// Delete every snapshot containing files matching a path pattern
    Purge {
        /// Glob over the file path WITHIN a snapshot, e.g. '*id_rsa'
        path: String,
        #[arg(long, default_value = DEFAULT_DB)]
        db: String,
        #[arg(long, default_value = DEFAULT_CONFIG)]
        config: PathBuf,
        /// Show the plan without deleting anything
        #[arg(long)]
        dry_run: bool,
    },
    /// Re-index every mounted backup target; --rebuild discards the index first
    Reindex {
        /// Path to SQLite database
        #[arg(long, default_value = DEFAULT_DB)]
        db: String,
        /// Path to config.toml (for target mount points)
        #[arg(long, default_value = DEFAULT_CONFIG)]
        config: PathBuf,
        /// Discard the existing index and rebuild it from scratch. Required to
        /// clear spans referencing missing snapshots and to populate file series
        /// (bd DAS-Backup-Manager-opd, -lc9). Backup history is preserved.
        #[arg(long)]
        rebuild: bool,
    },
    /// Remove index rows for snapshots that no longer exist on disk
    Reconcile {
        /// Path to SQLite database
        #[arg(long, default_value = DEFAULT_DB)]
        db: String,
        /// Path to config.toml (for target mount points)
        #[arg(long, default_value = DEFAULT_CONFIG)]
        config: PathBuf,
        /// Report what would be removed without changing anything
        #[arg(long)]
        dry_run: bool,
        /// Drop all index rows recorded under a mount path that is no longer a
        /// configured target — e.g. one retired by a rename. Such rows can never
        /// be reconciled, because that root will never be mounted again
        /// (bd DAS-Backup-Manager-wl8). Refuses a root that IS configured.
        #[arg(long, value_name = "PATH")]
        forget_root: Option<String>,
        /// First delete spans whose endpoints name snapshots that do not exist.
        /// Required on an index carrying such rows, because foreign keys are
        /// enforced and they cannot be rewritten (bd DAS-Backup-Manager-opd).
        #[arg(long)]
        repair: bool,
    },
    /// Interactive setup wizard — configure backup sources, targets, and scheduling
    Setup(setup::SetupArgs),
    /// Config inspection and management
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
    /// Run backup operations
    Backup {
        #[command(subcommand)]
        action: BackupAction,
    },
    /// Restore files or snapshots from backups
    Restore {
        #[command(subcommand)]
        action: RestoreAction,
    },
    /// Manage backup schedule
    Schedule {
        #[command(subcommand)]
        action: ScheduleAction,
    },
    /// Manage configured subvolumes
    Subvol {
        #[command(subcommand)]
        action: SubvolAction,
    },
    /// Show backup system health — drive status, SMART, disk usage, growth trends
    Health {
        /// Path to config.toml
        #[arg(long, default_value = DEFAULT_CONFIG)]
        config: PathBuf,
    },
    /// The independent operating systems on the `role = "mirror"` recovery
    /// drives: read them (never write) and say when they fall behind the host
    /// or would run btrbk when booted
    ///
    /// EXIT CODE: 0 every inspected OS is current with no WARNING (or none is
    /// mounted), 1 at least one needs attention — STALE, or a WARNING that
    /// btrbk may run when it boots — 2 an OS root, the config or the state
    /// file could not be handled.
    RecoveryOs {
        #[command(subcommand)]
        action: RecoveryOsAction,
    },
    /// Scheduled BTRFS scrub of the DAS backup filesystems
    Scrub {
        #[command(subcommand)]
        action: ScrubAction,
    },
    /// Subvolume drift detector — find source subvolumes that exist on disk
    /// but aren't backed up (or config entries for subvolumes that no longer
    /// exist)
    ///
    /// EXIT CODE: 0 means the check ran and found nothing, or it deferred
    /// because the singleton/maintenance lock was held (a backup or scrub is
    /// in progress, or another doctor run is already checking — never
    /// treated as a failure). 1 means DRIFT WAS FOUND: missing or stale
    /// subvolumes. This is a finding, not a malfunction — the check did its
    /// job — so das-backup-doctor.service carries SuccessExitStatus=1 and
    /// systemd does not mark the unit failed for it. 3 means at least one
    /// configured volume failed to mount/list while others were checked
    /// successfully: those subvolumes went unexamined, which is an
    /// operational fault rather than a finding, so the unit DOES fail on it
    /// (and it outranks 1 when both occur). 2 means the check could not run
    /// at all: config load failure, lock I/O error, or every configured
    /// volume failed to mount or list (nothing was ever examined).
    Doctor {
        /// Run the subvolume drift check. Currently the only check this
        /// command performs — the flag exists so future checks can be
        /// selected individually without a breaking CLI change. Omitting it
        /// still runs the drift check today.
        #[arg(long)]
        check_drift: bool,
        /// Email a report via the configured SMTP settings, but only when
        /// drift or an error was found — a clean run stays silent even with
        /// this flag, so the weekly timer doesn't spam an all-clear every
        /// Sunday.
        #[arg(long)]
        email: bool,
        /// Path to config.toml
        #[arg(long, default_value = DEFAULT_CONFIG)]
        config: PathBuf,
    },
    /// Generate shell completions
    Completions {
        /// Shell to generate completions for
        shell: Shell,
    },
}

#[derive(Subcommand)]
enum ConfigAction {
    /// Print shell-sourceable KEY=VALUE pairs from config
    DumpEnv {
        /// Path to config.toml
        #[arg(long, default_value = DEFAULT_CONFIG)]
        config: PathBuf,
    },
    /// Pretty-print the current config
    Show {
        /// Path to config.toml
        #[arg(long, default_value = DEFAULT_CONFIG)]
        config: PathBuf,
    },
    /// Validate config and report issues
    Validate {
        /// Path to config.toml
        #[arg(long, default_value = DEFAULT_CONFIG)]
        config: PathBuf,
    },
    /// Open config in $EDITOR
    Edit {
        /// Path to config.toml
        #[arg(long, default_value = DEFAULT_CONFIG)]
        config: PathBuf,
    },
}

#[derive(Subcommand)]
enum BackupAction {
    /// Run the full backup pipeline (snapshot → send → boot archive → index → report)
    Run {
        /// Path to config.toml
        #[arg(long, default_value = DEFAULT_CONFIG)]
        config: PathBuf,
        /// Preview only — don't execute any operations
        #[arg(long)]
        dry_run: bool,
        /// Run a full backup instead of incremental
        #[arg(long)]
        full: bool,
        /// Source labels to back up (comma-separated). Default: all non-manual sources
        #[arg(long, value_delimiter = ',')]
        sources: Vec<String>,
        /// Target labels to send to (comma-separated). Default: all mounted targets
        #[arg(long, value_delimiter = ',')]
        targets: Vec<String>,
    },
    /// Create snapshots without sending
    Snapshot {
        /// Path to config.toml
        #[arg(long, default_value = DEFAULT_CONFIG)]
        config: PathBuf,
        /// Source labels (comma-separated). Default: all
        #[arg(long, value_delimiter = ',')]
        sources: Vec<String>,
    },
    /// Send existing snapshots to targets
    Send {
        /// Path to config.toml
        #[arg(long, default_value = DEFAULT_CONFIG)]
        config: PathBuf,
        /// Target labels (comma-separated). Default: all mounted
        #[arg(long, value_delimiter = ',')]
        targets: Vec<String>,
    },
    /// Archive boot subvolumes on backup targets
    BootArchive {
        /// Path to config.toml
        #[arg(long, default_value = DEFAULT_CONFIG)]
        config: PathBuf,
    },
    /// Print the boot subvolumes a full run refreshes, with their btrbk snapshot names and target subdirectories (tab-separated; read by backup-run.sh)
    BootPlan {
        /// Path to config.toml
        #[arg(long, default_value = DEFAULT_CONFIG)]
        config: PathBuf,
    },
    /// Show the last backup report
    Report {
        /// Path to SQLite database
        #[arg(long, default_value = DEFAULT_DB)]
        db: String,
        /// Number of recent runs to show
        #[arg(long, default_value = "10")]
        limit: usize,
    },
    /// Record a completed backup run in the database (for use by backup-run.sh)
    RecordRun {
        /// Path to SQLite database
        #[arg(long, default_value = DEFAULT_DB)]
        db: String,
        /// Whether the backup succeeded
        #[arg(long)]
        success: bool,
        /// Backup mode
        #[arg(long, default_value = "incremental", value_parser = ["incremental", "full"])]
        mode: String,
        /// Number of snapshots created (required unless --counts-unknown)
        #[arg(long, required_unless_present = "counts_unknown")]
        snaps_created: Option<u64>,
        /// Number of snapshots sent to targets (required unless --counts-unknown)
        #[arg(long, required_unless_present = "counts_unknown")]
        snaps_sent: Option<u64>,
        /// The run could not count its snapshots: record both counts as
        /// unknown (NULL), never as a number
        #[arg(long, conflicts_with_all = ["snaps_created", "snaps_sent"])]
        counts_unknown: bool,
        /// Bytes sent to targets
        #[arg(long, default_value = "0")]
        bytes_sent: u64,
        /// Duration in seconds
        #[arg(long, default_value = "0")]
        duration_secs: u64,
        /// Error messages (newline-separated string)
        #[arg(long, default_value = "")]
        errors: String,
    },
}

// Every `restore` command waits for the DAS maintenance lock before it mounts
// the targets (exit 75 with --no-wait while it is held).
#[derive(Subcommand)]
enum RestoreAction {
    /// Restore specific files from a snapshot
    File {
        /// Path to the snapshot directory
        snapshot: PathBuf,
        /// Destination directory for restored files
        dest: PathBuf,
        /// File paths relative to snapshot root
        #[arg(required = true)]
        files: Vec<String>,
        /// Path to config.toml (for auto-mounting targets)
        #[arg(long, default_value = DEFAULT_CONFIG)]
        config: PathBuf,
        /// Do not wait for the DAS maintenance lock: while a backup, scrub or
        /// other job holds it, exit 75 at once without mounting anything
        #[arg(long)]
        no_wait: bool,
    },
    /// Restore an entire snapshot (btrfs send/receive or recursive copy)
    Snapshot {
        /// Path to the snapshot directory
        snapshot: PathBuf,
        /// Destination directory
        dest: PathBuf,
        /// Path to config.toml (for auto-mounting targets)
        #[arg(long, default_value = DEFAULT_CONFIG)]
        config: PathBuf,
        /// Do not wait for the DAS maintenance lock: while a backup, scrub or
        /// other job holds it, exit 75 at once without mounting anything
        #[arg(long)]
        no_wait: bool,
    },
    /// Browse files in a snapshot directory
    Browse {
        /// Path to the snapshot directory
        snapshot: PathBuf,
        /// Optional subdirectory prefix to browse
        #[arg(long)]
        prefix: Option<String>,
        /// Path to config.toml (for auto-mounting targets)
        #[arg(long, default_value = DEFAULT_CONFIG)]
        config: PathBuf,
        /// Do not wait for the DAS maintenance lock: while a backup, scrub or
        /// other job holds it, exit 75 at once without mounting anything
        #[arg(long)]
        no_wait: bool,
    },
}

#[derive(Subcommand)]
enum ScheduleAction {
    /// Show the current backup schedule
    Show {
        /// Path to config.toml
        #[arg(long, default_value = DEFAULT_CONFIG)]
        config: PathBuf,
    },
    /// Update schedule settings (incremental time, full schedule, delay)
    Set {
        /// Path to config.toml
        #[arg(long, default_value = DEFAULT_CONFIG)]
        config: PathBuf,
        /// Incremental backup time (HH:MM)
        #[arg(long)]
        incremental: Option<String>,
        /// Full backup schedule (cron-like, e.g., "Sun *-*-* 04:00:00")
        #[arg(long)]
        full: Option<String>,
        /// Randomized delay in minutes
        #[arg(long)]
        delay: Option<u32>,
    },
    /// Enable scheduled backups
    Enable {
        /// Path to config.toml
        #[arg(long, default_value = DEFAULT_CONFIG)]
        config: PathBuf,
    },
    /// Disable scheduled backups
    Disable {
        /// Path to config.toml
        #[arg(long, default_value = DEFAULT_CONFIG)]
        config: PathBuf,
    },
    /// Show next scheduled backup time
    Next {
        /// Path to config.toml
        #[arg(long, default_value = DEFAULT_CONFIG)]
        config: PathBuf,
    },
}

#[derive(Subcommand)]
enum SubvolAction {
    /// List all configured subvolumes across all sources
    List {
        /// Path to config.toml
        #[arg(long, default_value = DEFAULT_CONFIG)]
        config: PathBuf,
    },
    /// Add a subvolume to a source
    Add {
        /// Source label to add the subvolume to
        source: String,
        /// Subvolume name (e.g., "@home")
        name: String,
        /// Mark as manual-only (excluded from automatic backups)
        #[arg(long)]
        manual_only: bool,
        /// Path to config.toml
        #[arg(long, default_value = DEFAULT_CONFIG)]
        config: PathBuf,
    },
    /// Remove a subvolume from a source
    Remove {
        /// Source label
        source: String,
        /// Subvolume name
        name: String,
        /// Path to config.toml
        #[arg(long, default_value = DEFAULT_CONFIG)]
        config: PathBuf,
    },
    /// Mark a subvolume as manual-only
    SetManual {
        /// Source label
        source: String,
        /// Subvolume name
        name: String,
        /// Path to config.toml
        #[arg(long, default_value = DEFAULT_CONFIG)]
        config: PathBuf,
    },
    /// Mark a subvolume for automatic backups (remove manual-only flag)
    SetAuto {
        /// Source label
        source: String,
        /// Subvolume name
        name: String,
        /// Path to config.toml
        #[arg(long, default_value = DEFAULT_CONFIG)]
        config: PathBuf,
    },
    /// Adopt subvolumes that exist and are not excluded, retire entries
    /// whose subvolume is gone. Expects the source volumes to be mounted.
    Sync {
        /// Path to config.toml
        #[arg(long, default_value = DEFAULT_CONFIG)]
        config: PathBuf,
        /// Print what would change and write nothing
        #[arg(long)]
        dry_run: bool,
        /// With --dry-run: also write the btrbk.conf the real run would leave
        /// into PATH, an existing file (never created; a symlink is refused).
        /// `backup-run.sh --dryrun` points `btrbk dryrun` at it
        #[arg(long, requires = "dry_run", value_name = "PATH")]
        render_btrbk_conf: Option<PathBuf>,
    },
    /// Delete the backups of retired subvolumes that are past their window.
    /// Expects the targets to be mounted.
    Expire {
        /// Path to config.toml
        #[arg(long, default_value = DEFAULT_CONFIG)]
        config: PathBuf,
        /// Path to SQLite database
        #[arg(long, default_value = DEFAULT_DB)]
        db: PathBuf,
        /// Print what would be deleted and delete nothing
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Subcommand)]
enum RecoveryOsAction {
    /// Read one recovery OS root (the drive's `@`) and print its block
    Inspect {
        /// The OS root, e.g. /mnt/backup-system-recovery-A/@
        #[arg(long)]
        root: PathBuf,
        /// Name to print for the drive
        #[arg(long)]
        label: String,
        /// Path to config.toml (for [recovery_os] max_age_days)
        #[arg(long, default_value = DEFAULT_CONFIG)]
        config: PathBuf,
    },
    /// Read every mounted `role = "mirror"` target's OS and print the
    /// RECOVERY OS report section; unmounted ones print `not mounted`
    Status {
        /// Path to config.toml
        #[arg(long, default_value = DEFAULT_CONFIG)]
        config: PathBuf,
        /// Record each inspected drive's reading here (atomically, mode 0644)
        /// for `btrdasd health`; a drive not mounted keeps its earlier record
        #[arg(long, value_name = "PATH")]
        state_file: Option<PathBuf>,
    },
    /// Internal: hold a whole recovery disk open with O_EXCL until SIGTERM,
    /// SIGINT or SIGHUP, so the host cannot mount any partition of it while
    /// the recovery-os-updater VM may have it (started by recovery-os-vm.sh)
    ///
    /// Prints one line, `held <path> pid <pid>`, once the claim is in place.
    /// EXIT CODE: 0 released by one of those signals, 2 refused (not a block
    /// device, in use, any other open error, or `--json`).
    #[command(hide = true)]
    HoldDisk {
        /// The whole disk, e.g. /dev/disk/by-id/ata-<model>_<serial>
        #[arg(long, value_name = "PATH")]
        device: PathBuf,
    },
}

#[derive(Subcommand)]
enum ScrubAction {
    /// Run a full scrub pass now — locks, mount, `btrfs scrub`, unmount,
    /// report — the same path the scheduled systemd timer uses.
    ///
    /// This runs even when [scrub].enabled = false in config.toml: that flag
    /// only gates whether the *scheduled* timer fires, never a direct
    /// invocation of this command — a manual run is exactly the intended use
    /// of a temporarily-disabled schedule (testing, or scrubbing on demand).
    /// A warning is printed when this happens so it is never silent.
    ///
    /// EXIT CODE: 0 means the pass RAN (scrubbing was attempted on at least
    /// one target), regardless of what it found — per-FS errors, aborted
    /// scrubs, and unmount problems are reported via the FAILURE email and
    /// `btrdasd health` Critical escalation, never via this exit code.
    /// Nonzero means the pass could NOT run at all (config load failure,
    /// lock IO error, or every target unresolvable/unmountable before any
    /// scrub was attempted). This split exists so a Sentinel-monitored
    /// systemd unit never retry-loops a real multi-hour scrub over and over
    /// on failing hardware just because it found damage (bd
    /// DAS-Backup-Manager-18p) — only genuine can't-even-start failures,
    /// which fail in seconds, trip the unit into `failed` state.
    Run {
        /// Path to config.toml
        #[arg(long, default_value = DEFAULT_CONFIG)]
        config: PathBuf,
    },
    /// Show the last scrub result for every configured scrub target
    ///
    /// Resolved by filesystem UUID, never by mount path, so this works while
    /// the DAS filesystems are unmounted. Reads the engine's persisted state
    /// (/var/lib/das-backup/scrub-state.json) when available, falling back to
    /// the raw btrfs record (/var/lib/btrfs/scrub.status.<fsuuid>) for a
    /// filesystem that has scrub history predating this CLI. A target with
    /// neither source is reported as "never scrubbed", not an error.
    Status {
        /// Path to config.toml
        #[arg(long, default_value = DEFAULT_CONFIG)]
        config: PathBuf,
    },
    /// Cancel the filesystem currently being scrubbed
    ///
    /// Manual operator action only — the scrub interlock never cancels a
    /// pass automatically. Finds the actively-scrubbing filesystem via the
    /// engine's lock and live kernel state, then issues
    /// `btrfs scrub cancel` against it. A no-op (clean exit) when no scrub
    /// is running.
    Cancel {
        /// Path to config.toml
        #[arg(long, default_value = DEFAULT_CONFIG)]
        config: PathBuf,
    },
}

// ---------------------------------------------------------------------------
// Scrub CLI helpers
// ---------------------------------------------------------------------------

/// Combined view of one configured scrub target, as shown by
/// `btrdasd scrub status`.
///
/// Two sources are consulted, in order: the engine's own
/// `scrub-state.json` (richer — carries `last_success_epoch` and engine-level
/// errors), falling back to the raw `/var/lib/btrfs/scrub.status.<fsuuid>`
/// record for a filesystem whose scrub history predates this CLI or the
/// state file. Everything is resolved by filesystem UUID, never by mount
/// path — see the `scrub` module docs for why that matters.
struct ScrubTargetView {
    label: String,
    fsuuid: Option<String>,
    resolve_error: Option<String>,
    /// "state", "btrfs", "never", "unresolved", or "error".
    source: &'static str,
    outcome: Option<String>,
    ok: Option<bool>,
    last_success_epoch: Option<i64>,
    finished_epoch: Option<i64>,
    duration_secs: Option<u64>,
    bytes_scrubbed: Option<u64>,
    counters_summary: Option<String>,
    /// Extra error detail for the "error" source (a read failure that is
    /// neither "state entry present" nor "no record at all").
    detail: Option<String>,
}

impl ScrubTargetView {
    fn new(label: &str) -> Self {
        Self {
            label: label.to_string(),
            fsuuid: None,
            resolve_error: None,
            source: "unresolved",
            outcome: None,
            ok: None,
            last_success_epoch: None,
            finished_epoch: None,
            duration_secs: None,
            bytes_scrubbed: None,
            counters_summary: None,
            detail: None,
        }
    }

    fn status_word(&self) -> &'static str {
        match self.source {
            "unresolved" => "UNRESOLVED",
            "never" => "NEVER SCRUBBED",
            "error" => "ERROR",
            _ => match (self.ok, self.outcome.as_deref()) {
                (Some(true), _) => "OK",
                (Some(false), Some("aborted")) => "ABORTED",
                (Some(false), Some("canceled")) => "CANCELED",
                (Some(false), Some("finished")) => "ERRORS",
                _ => "FAILED",
            },
        }
    }

    fn age_days(&self, now_epoch: i64) -> Option<i64> {
        self.last_success_epoch
            .map(|t| (now_epoch - t).max(0) / 86_400)
    }
}

fn now_epoch_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Build one target's status view. `state` is the already-loaded engine
/// state (or `None` if it could not be loaded at all — a warning about that
/// is the caller's job, once, not per-target).
fn build_scrub_target_view(
    config: &Config,
    label: &str,
    state: Option<&scrub::ScrubState>,
) -> ScrubTargetView {
    let mut view = ScrubTargetView::new(label);

    let fsuuid = match scrub::resolve_target_fsuuid(config, label) {
        Ok(u) => u,
        Err(e) => {
            view.resolve_error = Some(e);
            return view;
        }
    };
    view.fsuuid = Some(fsuuid.clone());

    if let Some(fs) = state.and_then(|s| s.filesystems.get(&fsuuid)) {
        view.source = "state";
        view.outcome = Some(fs.last_attempt.outcome.clone());
        view.ok = Some(fs.last_attempt.ok);
        view.last_success_epoch = fs.last_success_epoch;
        view.finished_epoch = Some(fs.last_attempt.finished_epoch);
        view.duration_secs = Some(fs.last_attempt.duration_secs);
        view.bytes_scrubbed = Some(fs.last_attempt.bytes_scrubbed);
        view.counters_summary = Some(fs.last_attempt.counters.summary());
        return view;
    }

    // No entry in the engine's state (it may not exist at all yet) — fall
    // back to the raw btrfs record, which can hold real history from before
    // this CLI existed.
    match scrub::read_scrub_status(&fsuuid) {
        Ok(record) => {
            view.source = "btrfs";
            view.outcome = Some(record.outcome().as_str().to_string());
            view.ok = Some(record.is_clean());
            view.finished_epoch = Some(record.finished_epoch());
            if record.is_clean() {
                view.last_success_epoch = Some(record.finished_epoch());
            }
            view.duration_secs = Some(record.duration_secs());
            view.bytes_scrubbed = Some(record.bytes_scrubbed());
            view.counters_summary = Some(record.counters().summary());
        }
        Err(scrub::ScrubError::StatusMissing { .. }) => {
            view.source = "never";
        }
        Err(e) => {
            view.source = "error";
            view.detail = Some(e.to_string());
        }
    }
    view
}

/// Build the status view for every configured scrub target, in config order.
/// Returns a warning string when the engine state file exists but could not
/// be parsed — the per-target views still get built from the btrfs fallback.
fn gather_scrub_status(config: &Config) -> (Vec<ScrubTargetView>, Option<String>) {
    let (state, warning) = match scrub::load_state() {
        Ok(s) => (Some(s), None),
        Err(e) => (None, Some(format!("could not read scrub state: {e}"))),
    };
    let views = config
        .scrub
        .targets
        .iter()
        .map(|label| build_scrub_target_view(config, label, state.as_ref()))
        .collect();
    (views, warning)
}

/// Format a duration in seconds as `HHhMMm` (mirrors `scrub::format_duration`,
/// which is private to that module).
fn format_duration_secs(secs: u64) -> String {
    let hours = secs / 3600;
    let mins = (secs % 3600) / 60;
    if hours > 0 {
        format!("{hours}h {mins}m")
    } else {
        format!("{mins}m {}s", secs % 60)
    }
}

/// Render `btrdasd scrub status` as a human-readable table.
fn format_scrub_status(views: &[ScrubTargetView], config: &Config) -> String {
    let now = now_epoch_secs();
    let thin = "-".repeat(70);
    let mut out = String::new();
    out.push_str(&format!(
        "{:<24} {:<15} {:>6} {:>12} {:>10}\n",
        "Target", "Status", "Age", "Bytes", "Duration"
    ));
    out.push_str(&format!("{thin}\n"));
    for v in views {
        let age = v
            .age_days(now)
            .map(|d| format!("{d}d"))
            .unwrap_or_else(|| "-".to_string());
        let bytes = v
            .bytes_scrubbed
            .map(buttered_dasd::report::format_bytes)
            .unwrap_or_else(|| "-".to_string());
        let duration = v
            .duration_secs
            .map(format_duration_secs)
            .unwrap_or_else(|| "-".to_string());
        out.push_str(&format!(
            "{:<24} {:<15} {:>6} {:>12} {:>10}\n",
            v.label,
            v.status_word(),
            age,
            bytes,
            duration
        ));
        let detail = v
            .resolve_error
            .as_deref()
            .or(v.detail.as_deref())
            .unwrap_or("");
        out.push_str(&format!(
            "    uuid={} source={} outcome={} errors={}{}\n",
            v.fsuuid.as_deref().unwrap_or("<unresolved>"),
            v.source,
            v.outcome.as_deref().unwrap_or("<none>"),
            v.counters_summary.as_deref().unwrap_or("-"),
            if detail.is_empty() {
                String::new()
            } else {
                format!(" ({detail})")
            }
        ));
    }
    out.push_str(&format!(
        "\nwarn_age_days={} fail_age_days={} (age is measured against last_success_epoch)\n",
        config.scrub.warn_age_days, config.scrub.fail_age_days
    ));
    out
}

/// Render one target's status view as a single JSON object.
fn scrub_target_json(v: &ScrubTargetView) -> String {
    let esc = |s: &str| s.replace('\\', "\\\\").replace('"', "\\\"");
    format!(
        "{{\"label\":\"{}\",\"fsuuid\":{},\"source\":\"{}\",\"status\":\"{}\",\"outcome\":{},\"ok\":{},\"last_success_epoch\":{},\"finished_epoch\":{},\"duration_secs\":{},\"bytes_scrubbed\":{},\"resolve_error\":{}}}",
        esc(&v.label),
        v.fsuuid
            .as_deref()
            .map_or("null".to_string(), |u| format!("\"{}\"", esc(u))),
        v.source,
        v.status_word(),
        v.outcome
            .as_deref()
            .map_or("null".to_string(), |o| format!("\"{}\"", esc(o))),
        v.ok.map_or("null".to_string(), |b| b.to_string()),
        v.last_success_epoch
            .map_or("null".to_string(), |e| e.to_string()),
        v.finished_epoch
            .map_or("null".to_string(), |e| e.to_string()),
        v.duration_secs
            .map_or("null".to_string(), |d| d.to_string()),
        v.bytes_scrubbed
            .map_or("null".to_string(), |b| b.to_string()),
        v.resolve_error
            .as_deref()
            .map_or("null".to_string(), |e| format!("\"{}\"", esc(e))),
    )
}

/// Find the configured scrub target whose mount is verified — by filesystem
/// UUID, not just by path — as currently undergoing a live scrub, if any.
///
/// Pure (no locks, no side effects): deliberately separated from
/// `cancel_running_scrub` so the skip-not-cancel behavior for an
/// idle/mismatched target can be unit tested without needing to hold the
/// real, host-wide `/run/das-scrub.lock`.
///
/// Each candidate is checked with `scrub::live_scrub_state(mount, fsuuid)`,
/// which verifies via `findmnt` that the mount point is actually backed by
/// that exact filesystem UUID before it will ever report `Running` — an
/// idle/unmounted target's configured mount point is an ordinary directory
/// that falls through to whatever filesystem its parent belongs to, so
/// without that verification this could observe an unrelated live scrub
/// (e.g. a scheduled `btrfs-scrub@` unit on the host root) and mistake it
/// for the DAS target (`bd DAS-Backup-Manager-0kn` review, 2026-08-01). An
/// unverified target simply resolves to `Unknown` and is skipped.
fn find_actively_scrubbing_target(config: &Config) -> Option<(String, String, String)> {
    for label in &config.scrub.targets {
        let Ok(fsuuid) = scrub::resolve_target_fsuuid(config, label) else {
            continue;
        };
        let Some(target) = config.targets.iter().find(|t| t.label == *label) else {
            continue;
        };
        if scrub::live_scrub_state(&target.mount, &fsuuid) == scrub::LiveScrubState::Running {
            return Some((label.clone(), fsuuid, target.mount.clone()));
        }
    }
    None
}

/// Find and cancel the filesystem currently being scrubbed, if any.
///
/// Manual operator action only — never invoked automatically. The scrub
/// lock (`/run/das-scrub.lock`) tells us *whether* a pass is running but not
/// *which* filesystem; once contention on that lock confirms a pass is
/// live, `find_actively_scrubbing_target` locates it (UUID-verified, never
/// by path alone — see its doc comment).
fn cancel_running_scrub(config: &Config) -> Result<String, String> {
    match scrub::FileLock::try_acquire(scrub::SCRUB_LOCK_PATH) {
        Ok(Some(_lock)) => {
            // Acquired and immediately dropped at end of scope — proof that
            // nothing was scrubbing, not a lock we intend to hold.
            return Ok("No scrub pass is currently running — nothing to cancel.".to_string());
        }
        Ok(None) => {} // held elsewhere: a pass IS running
        Err(e) => return Err(format!("could not check scrub lock: {e}")),
    }

    match find_actively_scrubbing_target(config) {
        Some((label, fsuuid, mount)) => {
            let out = std::process::Command::new("btrfs")
                .args(["scrub", "cancel", &mount])
                .output()
                .map_err(|e| format!("cannot execute 'btrfs scrub cancel': {e}"))?;
            if out.status.success() {
                Ok(format!(
                    "Canceled scrub of '{label}' (uuid={fsuuid}) at {mount}"
                ))
            } else {
                let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
                Err(format!("btrfs scrub cancel {mount} failed: {stderr}"))
            }
        }
        None => Ok(
            "A scrub pass is running (lock held) but no configured target's mount currently \
             shows an active scrub — it may be between filesystems (mounting/unmounting); \
             try again in a few seconds."
                .to_string(),
        ),
    }
}

/// Map a completed (or skipped) scrub pass to `btrdasd scrub run`'s process
/// exit code (`bd DAS-Backup-Manager-18p`).
///
/// This is a CLI-only concern, deliberately kept separate from
/// `ScrubPass::success()` — the engine's own "did everything pass cleanly"
/// bit, unchanged, and still what the FAILURE email and `btrdasd health`
/// Critical escalation key on. The split exists because Sentinel
/// (`cachyos-sentinel`) auto-restarts any systemd unit it observes in
/// `failed` state, and its restart-rate limiter (3 restarts / 600 s) never
/// engages for scrub failures spaced 14–18 h apart (one monthly pass) — so
/// if this exit code mirrored `ScrubPass::success()`, a pass that ran to
/// completion but found real damage on failing hardware would retry-loop
/// the entire multi-hour pass forever. Exit 0 therefore means only "the
/// pass ran" — per-FS outcomes (error counters, aborted scrubs, unmount
/// failures after scrubbing) are irrelevant to this decision and are
/// reported through the email/health channels instead. Exit nonzero means
/// "the pass could not even start" — that fails in seconds, exactly where
/// Sentinel's limiter genuinely brakes a real crash loop.
///
/// - A [`PassStatus::Skipped`] pass (another scrub already running) is
///   always 0 — nothing went wrong, there was simply nothing to do here.
/// - Otherwise: 0 if at least one target's `scrub_launched` is `true` —
///   [`scrub::ScrubFsResult::scrub_launched`] is set only once
///   `resolve_target_fsuuid` and `ensure_mounted` have both succeeded *and*
///   the `btrfs scrub start` child process is confirmed to have launched
///   (`Command::spawn()` returned `Ok`), so this is exactly "a real scrub
///   attempt happened" for that filesystem, regardless of what happened
///   afterward (error counters, aborted, unmount failure post-scrub, or
///   even the scrub process itself later exiting nonzero). Deliberately
///   **not** keyed on `started_epoch != 0`: that field used to be stamped
///   before `spawn()` was even attempted, which meant a spawn failure
///   (binary missing, broken `PATH`) on every target still left every
///   `started_epoch` non-zero and made a fast, systemic setup failure
///   masquerade as "the pass ran" — exactly the inverse of the bug this
///   function exists to prevent (`bd DAS-Backup-Manager-18p` review,
///   2026-08-02). `scrub_launched` is set in the same branch as
///   `started_epoch` now, so the two are always consistent, but this
///   function keys on the explicit flag rather than relying on that
///   consistency.
/// - Otherwise (every target failed before any scrub was ever launched —
///   e.g. all targets unresolvable/unmountable, or the `btrfs` binary
///   itself could not be spawned for any of them): nonzero. This is the
///   one judgment call the brief left explicit: a pass where *some*
///   targets scrubbed and *some* could not be mounted is still 0
///   (scrubbing did occur, and the email/`scrub status` output already
///   surfaces the skips) — only *zero* targets ever reaching a launched
///   scrub counts as "could not run".
///
/// Setup-stage failures before a [`ScrubPass`] value even exists — config
/// load errors, lock-file IO errors, `ScrubError::NoTargets` — never reach
/// this function at all; they already propagate as `Err` through
/// `scrub::run_scrub_pass`'s `?` in the caller and exit nonzero via Rust's
/// default `main()` error handling, which is exactly the desired "could not
/// run" outcome for that class of failure too.
pub fn exit_code_for_pass(pass: &scrub::ScrubPass) -> i32 {
    if pass.status == scrub::PassStatus::Skipped {
        return 0;
    }
    let any_scrub_launched = pass.results.iter().any(|r| r.scrub_launched);
    if any_scrub_launched { 0 } else { 1 }
}

/// Map a `btrdasd doctor --check-drift` outcome to its process exit code
/// (bd DAS-Backup-Manager-01u, refined by `DAS-Backup-Manager-f6p`). Unlike
/// `exit_code_for_pass`, this command deliberately does NOT collapse "ran but
/// found problems" into 0: a drift check is a fast read-mostly scan with no
/// Sentinel-retry-loop hazard, so exit 1 on drift is useful to scripts and CI
/// and is, as `01u` put it, the whole point of the weekly timer.
///
/// What `01u` did not weigh is that systemd cannot tell a *finding* from a
/// *malfunction*: any nonzero exit put `das-backup-doctor.service` into
/// `failed`, cachyos-sentinel then restarted it and notified `"backup service
/// das-backup-doctor.service last run failed: Result=exit-code"`. The operator
/// was told the checker broke at the exact moment it worked and had something
/// to say, and that alert competed with the drift email carrying the real
/// signal. Measured 2026-08-31: 13:47:34 run → drift found → emailed → exit 1
/// → sentinel restart at 13:47:57 → identical failure → notification 13:48:08.
///
/// The CLI contract is therefore kept and the two meanings that shared exit 1
/// are split, so the unit can succeed on the benign one and fail on the other:
///
/// - `Deferred` (either lock held) is always 0 — nothing went wrong, the
///   check simply yielded to a real backup/scrub or another doctor run.
/// - `Ran` with zero volumes examined is 2 — "could not run" (every
///   configured volume failed to mount or list).
/// - `Ran` with at least one volume that failed to mount/list is **3** — those
///   subvolumes went unexamined. That is an operational fault, not a finding,
///   so the generated unit lets it fail. It outranks drift when both occur:
///   an incomplete check cannot assert that its drift list is complete. A
///   volume the check mounted and could not unmount again is the same kind
///   of operational fault (bd DAS-Backup-Manager-5oc) and is 3 as well.
/// - `Ran` cleanly with drift is **1** — a successful check with a result.
///   `render_systemd_doctor_service` pairs this with `SuccessExitStatus=1`.
/// - Otherwise 0.
///
/// `not_clean()` remains the source of truth for `doctor::format_report` and
/// the `--email` trigger, exactly as `ScrubPass::success()` stayed the source
/// of truth for the scrub FAILURE email while `exit_code_for_pass` narrowed
/// (bd DAS-Backup-Manager-18p). What travels by email is unchanged; only how
/// systemd reads the process is.
///
/// An earlier version checked only `has_drift()`: a run where 1 of 4 volumes
/// failed to mount while the other 3 were clean had `volumes_checked > 0` (so
/// "ran") and `has_drift() == false` — exit 0, while the printed report said
/// `DRIFT DETECTED — FAILURE` and `--email` sent a failure email for the same
/// run. The split below keeps that case nonzero; it moves it from 1 to 3.
/// `btrdasd doctor --json`'s one line, built by serde: a deferral's reason
/// carries the lock holder's record, which may hold any character, so it is
/// never pasted into the text by hand. The keys keep their order, status first.
fn doctor_json(outcome: &doctor::DoctorOutcome) -> Result<String, serde_json::Error> {
    #[derive(serde::Serialize)]
    #[serde(tag = "status", rename_all = "lowercase")]
    enum Line<'a> {
        Deferred {
            reason: &'a str,
        },
        Ran {
            volumes_checked: usize,
            volumes_failed: usize,
            missing: usize,
            stale: usize,
        },
    }
    serde_json::to_string(&match outcome {
        doctor::DoctorOutcome::Deferred { reason } => Line::Deferred { reason },
        doctor::DoctorOutcome::Ran(dr) => Line::Ran {
            volumes_checked: dr.volumes_checked,
            volumes_failed: dr.volumes_failed.len(),
            missing: dr.missing.len(),
            stale: dr.stale.len(),
        },
    })
}

pub fn exit_code_for_doctor(outcome: &doctor::DoctorOutcome) -> i32 {
    match outcome {
        doctor::DoctorOutcome::Deferred { .. } => 0,
        doctor::DoctorOutcome::Ran(report) => {
            if !report.ran() {
                2
            } else if !report.volumes_failed.is_empty() || !report.left_mounted.is_empty() {
                3
            } else if report.has_drift() {
                1
            } else {
                0
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

/// Drop the index rows of snapshots `subvol expire` just deleted.
/// Returns how many rows were pruned.
fn prune_deleted_from_index(
    db: &Path,
    deleted: &[PathBuf],
) -> Result<usize, Box<dyn std::error::Error>> {
    let database = Database::open(db)?;
    let ids: Vec<i64> = database
        .list_snapshots()?
        .into_iter()
        .filter(|s| deleted.iter().any(|p| p.to_string_lossy() == s.path))
        .map(|s| s.id)
        .collect();
    database.prune_snapshots(&ids)?;
    Ok(ids.len())
}

/// What `backup run` asks of the job: incremental unless `--full`, the
/// selection its flags name (none given: not specified), everything after the
/// btrbk steps on, and a report.
fn backup_run_options(
    dry_run: bool,
    full: bool,
    sources: Vec<String>,
    targets: Vec<String>,
) -> BackupOptions {
    BackupOptions {
        mode: if full {
            Some(BackupMode::Full)
        } else {
            Some(BackupMode::Incremental)
        },
        sources: flag_selection(sources),
        targets: flag_selection(targets),
        dry_run,
        boot_archive: true,
        index_after: true,
        email_report: true,
        ..Default::default()
    }
}

/// The status `backup run` exits with, if it is not 0 (see
/// `BackupJobOutcome::exit_code`).
fn backup_run_exit(code: i32) -> Option<i32> {
    (code != 0).then_some(code)
}

/// A `--sources` / `--targets` flag as the selection the run is given: no flag
/// is "not specified" (all); a flag that names something is exactly that. A
/// flag that names nothing real (`--targets ''`) arrives as a list holding an
/// empty label and is refused as an unknown one — it never reads as "all".
fn flag_selection(list: Vec<String>) -> Option<Vec<String>> {
    (!list.is_empty()).then_some(list)
}

/// `backup run --json`: the counts are `null` when the run could not take
/// them, never 0 (bd DAS-Backup-Manager-no4).
fn backup_run_json(result: &backup::BackupResult) -> String {
    let count = |n: Option<usize>| serde_json::Value::from(n).to_string();
    format!(
        "{{\"success\":{},\"snapshots_created\":{},\"snapshots_sent\":{},\"bytes_sent\":{},\"duration_secs\":{}}}",
        result.success,
        count(result.snapshots_created),
        count(result.snapshots_sent),
        result.bytes_sent,
        result.duration_secs
    )
}

/// `backup run`'s one-line outcome. A count the run could not take reads
/// `unknown`, never 0.
fn backup_run_line(result: &backup::BackupResult) -> String {
    let count = |n: Option<usize>| report::format_count(n.map(|n| n as u64));
    format!(
        "Backup {}: snapshots created: {}, sent: {}, {} in {}s",
        if result.success {
            "succeeded"
        } else {
            "FAILED"
        },
        count(result.snapshots_created),
        count(result.snapshots_sent),
        report::format_bytes(result.bytes_sent),
        result.duration_secs
    )
}

/// Whether `subvol expire` failed overall: the expiry itself did, or snapshots
/// are gone and the index still lists them. The database is not opened at all
/// when nothing was deleted (it may not exist yet).
fn expire_failed(outcome_failed: bool, deleted: &[PathBuf], db: &Path) -> bool {
    let mut failed = outcome_failed;
    if !deleted.is_empty()
        && let Err(e) = prune_deleted_from_index(db, deleted)
    {
        eprintln!("Warning: deleted snapshots could not be removed from the index: {e}");
        failed = true;
    }
    failed
}

/// `recovery-os hold-disk` prints one plain line for a script to read and has
/// no JSON form, so the global `--json` is refused rather than ignored.
fn hold_disk_json_refusal(json: bool) -> Option<&'static str> {
    json.then_some(
        "recovery-os hold-disk has no JSON output; it prints one line, `held <path> pid <pid>`",
    )
}

/// `btrdasd recovery-os`: prints the section (or JSON) and returns the exit
/// code — 0 current, 1 stale, 2 could not be checked. `hold-disk`: 0 released
/// by a signal, 2 refused.
fn run_recovery_os(action: RecoveryOsAction, json: bool) -> i32 {
    use buttered_dasd::recovery_os as ros;
    let load = |config: &Path| {
        Config::load(config).map_err(|e| {
            eprintln!("Error: cannot read {}: {e}", config.display());
        })
    };
    let today = buttered_dasd::caldate::today();
    match action {
        RecoveryOsAction::HoldDisk { device } => {
            if let Some(why) = hold_disk_json_refusal(json) {
                eprintln!("Error: {why}");
                return 2;
            }
            ros::hold_disk::run(&device)
        }
        RecoveryOsAction::Inspect {
            root,
            label,
            config,
        } => {
            let Ok(cfg) = load(&config) else { return 2 };
            let host = ros::host_versions();
            let max = cfg.recovery_os.max_age_days;
            let entry = ros::inspect_drive(&label, &root, &host, &today, max);
            if json {
                println!("{}", ros::entry_json(&entry));
            } else {
                print!(
                    "{}",
                    ros::format_section(std::slice::from_ref(&entry), &host)
                );
            }
            ros::exit_code(std::slice::from_ref(&entry))
        }
        RecoveryOsAction::Status { config, state_file } => {
            let Ok(cfg) = load(&config) else { return 2 };
            let host = ros::host_versions();
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs() as i64);
            let run = ros::status_run_with(
                &cfg,
                state_file.as_deref(),
                &host,
                &today,
                now,
                &health::is_mountpoint,
                &ros::mounted_uuid,
            );
            if json {
                let all: Vec<_> = run.entries.iter().map(ros::entry_json).collect();
                println!("{}", serde_json::Value::from(all));
            } else {
                print!("{}", ros::format_section(&run.entries, &host));
            }
            if let Some(e) = &run.state_error {
                eprintln!("Error: could not record the result: {e}");
            }
            run.code
        }
    }
}

// ---------------------------------------------------------------------------
// walk and restore: the interactive commands that mount the targets
// ---------------------------------------------------------------------------

/// Where `walk` and the `restore` commands find the DAS maintenance lock, and
/// the hold their caller may have handed down ([`maintenance::DELEGATED_FD_ENV`]).
struct CliLock {
    site: maintenance::LockSite,
    delegated: Option<std::ffi::OsString>,
}

impl CliLock {
    fn production() -> Self {
        Self {
            site: maintenance::LockSite::production(),
            delegated: std::env::var_os(maintenance::DELEGATED_FD_ENV),
        }
    }
}

/// What `walk` and the `restore` commands are run with.
struct Interactive<'a> {
    json: bool,
    /// Defer instead of waiting for the maintenance lock.
    no_wait: bool,
    lock: &'a CliLock,
    progress: &'a dyn ProgressCallback,
}

/// How `walk` or a `restore` command ended, when it did not fail.
#[derive(Debug, PartialEq, Eq)]
enum Ran {
    Done,
    /// `--no-wait` and the lock was held: nothing was mounted.
    Deferred,
}

/// The exit code that is not 0: [`maintenance::DEFERRED_EXIT_CODE`] after a
/// deferral.
fn deferred_exit_code(ran: &Ran) -> Option<i32> {
    match ran {
        Ran::Done => None,
        Ran::Deferred => Some(maintenance::DEFERRED_EXIT_CODE),
    }
}

/// The DAS maintenance lock for `walk` or a `restore` command, before it
/// mounts anything (bd DAS-Backup-Manager-frb): the hold its caller handed
/// down, if one is named; otherwise taken — waiting for it, after saying who
/// holds it, unless `--no-wait`. `None` means deferred: the line is printed,
/// and the command exits [`maintenance::DEFERRED_EXIT_CODE`]. A lock held by
/// the command's own caller that did not hand it down is an error at once:
/// waiting for it would never end.
fn hold_for_cli(
    job: &str,
    run: &Interactive<'_>,
) -> Result<Option<maintenance::MaintenanceHeld>, Box<dyn std::error::Error>> {
    let site = &run.lock.site;
    if let Some(raw) = &run.lock.delegated {
        let fd = raw
            .to_str()
            .and_then(|s| s.trim().parse::<std::os::fd::RawFd>().ok())
            .ok_or_else(|| {
                format!(
                    "{}={raw:?} is not a descriptor number",
                    maintenance::DELEGATED_FD_ENV
                )
            })?;
        return Ok(Some(maintenance::MaintenanceHeld::delegated_at(
            &site.path, fd,
        )?));
    }
    let never = || false;
    let callers = maintenance::Callers::of_this_process();
    match maintenance::wait_for(site, job, run.no_wait, Some(&callers), &never, run.progress)? {
        maintenance::Waited::Held(held) => Ok(Some(held)),
        maintenance::Waited::CallerHolds { pid, record } => {
            Err(maintenance::caller_holds_line(pid, &record).into())
        }
        maintenance::Waited::Deferred { holder } | maintenance::Waited::Cancelled { holder } => {
            if run.json {
                println!(
                    "{}",
                    serde_json::json!({ "deferred": true, "holder": holder })
                );
            } else {
                println!("{}", maintenance::deferred_line(&site.path, &holder));
            }
            Ok(None)
        }
    }
}

/// `btrdasd walk`.
fn cmd_walk(
    target: &Path,
    db: &str,
    config: &Path,
    run: &Interactive<'_>,
) -> Result<Ran, Box<dyn std::error::Error>> {
    let (json, progress) = (run.json, run.progress);
    let cfg = Config::load(config)?;
    let Some(held) = hold_for_cli("btrdasd walk", run)? else {
        return Ok(Ran::Deferred);
    };
    let mut guard = mount::ensure_targets_mounted(&cfg, progress, &held)?;
    let database = Database::open(db)?;
    let result = indexer::walk(target, &database);
    // Reconcile while the targets are still mounted — the prune is only
    // ever safe under a verified mountpoint (bd DAS-Backup-Manager-cu8).
    let reconciled = run_reconcile(&database, &cfg, false);
    let still_mounted = guard.unmount(progress);
    let result = result?;
    let (out, warning) = walk_report(json, &result, &reconciled);
    print!("{out}");
    if let Some(warning) = warning {
        eprintln!("{warning}");
    }
    mount::require_released(&still_mounted)?;
    Ok(Ran::Done)
}

/// What `walk` prints on stdout, and the warning for stderr when the
/// reconcile failed (text output only, as before).
fn walk_report(
    json: bool,
    result: &indexer::WalkResult,
    reconciled: &rusqlite::Result<reconcile::PruneStats>,
) -> (String, Option<String>) {
    if json {
        let out = format!(
            "{{\"discovered\":{},\"indexed\":{},\"skipped\":{}}}\n",
            result.snapshots_discovered, result.snapshots_indexed, result.snapshots_skipped
        );
        return (out, None);
    }
    let mut out = format!(
        "Discovered: {} snapshots\nIndexed:    {} new\nSkipped:    {} already indexed\n",
        result.snapshots_discovered, result.snapshots_indexed, result.snapshots_skipped
    );
    let mut warning = None;
    match reconciled {
        Ok(stats) if stats.snapshots_removed > 0 => out.push_str(&format!(
            "Reconciled: {} stale snapshots pruned ({} spans repaired, {} removed, {} files dropped)\n",
            stats.snapshots_removed, stats.spans_repaired, stats.spans_removed, stats.files_removed
        )),
        Ok(_) => out.push_str("Reconciled: index already consistent\n"),
        Err(e) => warning = Some(format!("Warning: reconcile failed: {e}")),
    }
    for r in &result.results {
        out.push_str(&format!(
            "  {} files ({} new, {} extended, {} changed, {} errors)\n",
            r.files_total, r.files_new, r.files_extended, r.files_changed, r.scan_errors
        ));
    }
    (out, warning)
}

/// What `restore file` and `restore snapshot` print on stdout, and the error
/// lines for stderr (text output only, as before).
fn restored_report(json: bool, result: &restore::RestoreResult) -> (String, Vec<String>) {
    if json {
        let out = format!(
            "{{\"files_restored\":{},\"bytes_restored\":{},\"errors\":{},\"duration_secs\":{}}}\n",
            result.files_restored,
            result.bytes_restored,
            result.errors.len(),
            result.duration_secs
        );
        return (out, Vec::new());
    }
    let out = format!(
        "Restored {} files ({}) in {}s\n",
        result.files_restored,
        report::format_bytes(result.bytes_restored),
        result.duration_secs
    );
    let errors = result
        .errors
        .iter()
        .map(|e| format!("  ERROR: {e}"))
        .collect();
    (out, errors)
}

/// `btrdasd restore file`.
fn cmd_restore_file(
    snapshot: &Path,
    dest: &Path,
    files: &[String],
    config: &Path,
    run: &Interactive<'_>,
) -> Result<Ran, Box<dyn std::error::Error>> {
    let (json, progress) = (run.json, run.progress);
    let cfg = Config::load(config)?;
    let Some(held) = hold_for_cli("btrdasd restore file", run)? else {
        return Ok(Ran::Deferred);
    };
    let mut guard = mount::ensure_targets_mounted(&cfg, progress, &held)?;
    let file_refs: Vec<&str> = files.iter().map(|s| s.as_str()).collect();
    let result = restore::restore_files(
        snapshot,
        &file_refs,
        dest,
        &cfg.restore.allowed_roots,
        &restore::snapshot_source_roots(&cfg),
        progress,
    );
    let still_mounted = guard.unmount(progress);
    let (out, errors) = restored_report(json, &result?);
    print!("{out}");
    for line in errors {
        eprintln!("{line}");
    }
    mount::require_released(&still_mounted)?;
    Ok(Ran::Done)
}

/// `btrdasd restore snapshot`.
fn cmd_restore_snapshot(
    snapshot: &Path,
    dest: &Path,
    config: &Path,
    run: &Interactive<'_>,
) -> Result<Ran, Box<dyn std::error::Error>> {
    let (json, progress) = (run.json, run.progress);
    let cfg = Config::load(config)?;
    let Some(held) = hold_for_cli("btrdasd restore snapshot", run)? else {
        return Ok(Ran::Deferred);
    };
    let mut guard = mount::ensure_targets_mounted(&cfg, progress, &held)?;
    let result = restore::restore_snapshot(
        snapshot,
        dest,
        &cfg.restore.allowed_roots,
        &restore::snapshot_source_roots(&cfg),
        progress,
    );
    let still_mounted = guard.unmount(progress);
    let (out, errors) = restored_report(json, &result?);
    print!("{out}");
    for line in errors {
        eprintln!("{line}");
    }
    mount::require_released(&still_mounted)?;
    Ok(Ran::Done)
}

/// `btrdasd restore browse`.
fn cmd_restore_browse(
    snapshot: &Path,
    prefix: Option<&str>,
    config: &Path,
    run: &Interactive<'_>,
) -> Result<Ran, Box<dyn std::error::Error>> {
    let (json, progress) = (run.json, run.progress);
    let cfg = Config::load(config)?;
    let Some(held) = hold_for_cli("btrdasd restore browse", run)? else {
        return Ok(Ran::Deferred);
    };
    let mut guard = mount::ensure_targets_mounted(&cfg, progress, &held)?;
    let entries = restore::browse_snapshot(snapshot, prefix);
    let still_mounted = guard.unmount(progress);
    print!("{}", browse_listing(json, &entries?));
    mount::require_released(&still_mounted)?;
    Ok(Ran::Done)
}

/// What `restore browse` prints.
fn browse_listing(json: bool, entries: &[restore::BrowseEntry]) -> String {
    if json {
        let mut out = String::from("[");
        for (i, e) in entries.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str(&format!(
                "{{\"path\":\"{}\",\"name\":\"{}\",\"size\":{},\"mtime\":{},\"is_dir\":{}}}",
                e.path.replace('"', "\\\""),
                e.name.replace('"', "\\\""),
                e.size,
                e.mtime,
                e.is_dir
            ));
        }
        out.push_str("]\n");
        return out;
    }
    let mut out = String::new();
    for e in entries {
        let (type_char, size) = if e.is_dir {
            ("d", "-".to_string())
        } else {
            ("-", report::format_bytes(e.size))
        };
        out.push_str(&format!("{type_char} {size:>12} {}\n", e.name));
    }
    out.push_str(&format!("({} entries)\n", entries.len()));
    out
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    let json = cli.json;

    match cli.command {
        // ----- Indexer commands (unchanged) -----
        Commands::Walk {
            target,
            db,
            config,
            no_wait,
        } => {
            let lock = CliLock::production();
            let run = Interactive {
                json,
                no_wait,
                lock: &lock,
                progress: &CliProgress,
            };
            let ran = cmd_walk(&target, &db, &config, &run)?;
            if let Some(code) = deferred_exit_code(&ran) {
                std::process::exit(code);
            }
        }
        Commands::Forget {
            pattern,
            db,
            config,
            btrbk_conf,
            dry_run,
        } => {
            let live = forget::live_snapshot_names(&btrbk_conf).unwrap_or_else(|e| {
                eprintln!("Warning: could not read {}: {e}", btrbk_conf.display());
                eprintln!(
                    "Refusing to continue — without the live series list the \
                           guard against deleting an active chain cannot run."
                );
                std::process::exit(2);
            });
            run_deletion(&db, &config, json, dry_run, "forget", |snaps| {
                forget::plan_forget(snaps, &pattern, &live)
            })?;
        }
        Commands::Purge {
            path,
            db,
            config,
            dry_run,
        } => {
            let database = Database::open(&db)?;
            let ids = database.snapshots_containing(&path)?;
            drop(database);
            println!(
                "Matched {} snapshot(s) containing files like '{path}'.",
                ids.len()
            );
            println!(
                "NOTE: whole snapshots are deleted — a file cannot be removed from \
                 inside one. Affected series re-send in full on the next backup."
            );
            run_deletion(&db, &config, json, dry_run, "purge", |snaps| {
                forget::plan_purge(snaps, &ids)
            })?;
        }
        Commands::Reindex {
            db,
            config,
            rebuild,
        } => {
            let cfg = Config::load(&config)?;

            // Mounts the DAS targets for the duration, so it takes the same
            // non-blocking interlock as reconcile.
            let locks = match reconcile::try_acquire_locks("btrdasd reindex")? {
                reconcile::LockAttempt::Acquired(locks) => locks,
                reconcile::LockAttempt::Deferred(why) => {
                    println!("Deferred — {why}");
                    return Ok(());
                }
            };

            let progress = CliProgress;
            let mut guard = mount::ensure_targets_mounted(&cfg, &progress, locks.maintenance())?;
            let database = Database::open(&db)?;

            let outcome = (|| -> Result<ReindexOutcome, Box<dyn std::error::Error>> {
                if rebuild {
                    println!("Discarding existing index (backup history preserved)...");
                    database.rebuild_content_tables()?;
                }
                let mut per_target = Vec::new();
                for target in &cfg.targets {
                    let root = std::path::Path::new(&target.mount);
                    if !buttered_dasd::health::is_mountpoint(root) {
                        println!("  {} — not mounted, skipping", target.label);
                        continue;
                    }
                    println!("  {} — indexing {}", target.label, target.mount);
                    let r = buttered_dasd::indexer::walk(root, &database)?;
                    per_target.push((
                        target.label.clone(),
                        r.snapshots_indexed,
                        r.snapshots_discovered,
                        r.presence_recorded,
                    ));
                }
                Ok(per_target)
            })();

            let still_mounted = guard.unmount(&progress);
            let per_target = outcome?;

            let total: usize = per_target.iter().map(|(_, i, _, _)| i).sum();
            if json {
                let parts: Vec<String> = per_target
                    .iter()
                    .map(|(l, i, d, p)| {
                        format!(
                            "{{\"target\":\"{l}\",\"indexed\":{i},\"discovered\":{d},\"copies_recorded\":{p}}}"
                        )
                    })
                    .collect();
                println!(
                    "{{\"rebuild\":{},\"indexed\":{},\"targets\":[{}]}}",
                    rebuild,
                    total,
                    parts.join(",")
                );
            } else {
                println!();
                for (label, indexed, discovered, present) in &per_target {
                    // "0 indexed of 237" alone reads as a failure. It is not: the
                    // same logical snapshot is indexed once, under whichever
                    // target is walked first, and its presence here is recorded
                    // rather than ignored (bd DAS-Backup-Manager-gt0).
                    if *indexed == 0 && *present > 0 {
                        println!(
                            "  {label}: {present} copies recorded of {discovered} on disk \
                             (already indexed from another target)"
                        );
                    } else {
                        println!(
                            "  {label}: {indexed} indexed of {discovered} on disk, \
                             {present} copies recorded"
                        );
                    }
                }
                println!("Total newly indexed: {total}");
            }
            mount::require_released(&still_mounted)?;
        }
        Commands::Reconcile {
            db,
            config,
            dry_run,
            repair,
            forget_root,
        } => {
            let cfg = Config::load(&config)?;

            // A standalone pass mounts the DAS targets, so it must respect the
            // maintenance interlock. Non-blocking: defer rather than delay a backup.
            let locks = match reconcile::try_acquire_locks("btrdasd reconcile")? {
                reconcile::LockAttempt::Acquired(locks) => locks,
                reconcile::LockAttempt::Deferred(why) => {
                    if json {
                        // The reason names the holder as it recorded itself:
                        // escaped by serde, never pasted into JSON by hand.
                        println!("{}", serde_json::json!({ "deferred": true, "reason": why }));
                    } else {
                        println!("Deferred — {why}");
                    }
                    return Ok(());
                }
            };

            let progress = CliProgress;
            let mut guard = mount::ensure_targets_mounted(&cfg, &progress, locks.maintenance())?;
            let database = Database::open(&db)?;

            if let Some(root) = &forget_root {
                let configured: Vec<String> = cfg.targets.iter().map(|t| t.mount.clone()).collect();
                if configured
                    .iter()
                    .any(|c| c.trim_end_matches('/') == root.trim_end_matches('/'))
                {
                    eprintln!(
                        "refusing: {root} IS a configured target — reconcile handles it \
                         normally, and dropping its rows would discard a live index"
                    );
                    return Ok(());
                }
                let ids = database.snapshots_under_root(root)?;
                if ids.is_empty() {
                    println!("No index rows under {root}.");
                } else if dry_run {
                    println!("Would drop {} index rows under {root}.", ids.len());
                } else {
                    let stats = database.prune_snapshots(&ids)?;
                    println!(
                        "Retired {root}: dropped {} snapshots, {} spans, {} files",
                        stats.snapshots_removed, stats.spans_removed, stats.files_removed
                    );
                }
            }

            if repair {
                if dry_run {
                    println!("(--repair has no effect with --dry-run)");
                } else {
                    match database.delete_dangling_spans() {
                        Ok((spans, files)) => println!(
                            "Repaired: removed {spans} dangling spans, {files} orphaned files"
                        ),
                        Err(e) => eprintln!("Repair failed: {e}"),
                    }
                }
            }

            let roots: Vec<String> = cfg.targets.iter().map(|t| t.mount.clone()).collect();
            let mounted = reconcile::verified_mounted_roots(&roots);
            let snapshots = database.list_snapshots();
            let outcome = snapshots.map(|snaps| {
                let plan =
                    reconcile::plan_reconcile(&snaps, &mounted, &roots, reconcile::path_exists);
                let stats = if dry_run || plan.is_empty() {
                    Ok(reconcile::PruneStats::default())
                } else {
                    database.prune_snapshots(&plan.doomed)
                };
                (plan, stats)
            });
            let still_mounted = guard.unmount(&progress);

            let (plan, stats) = outcome?;
            let stats = stats?;

            if json {
                println!(
                    "{{\"dry_run\":{},\"mounted_roots\":{},\"doomed\":{},\"present\":{},\"skipped_unmounted\":{},\"skipped_unknown_root\":{},\"snapshots_removed\":{},\"spans_repaired\":{},\"spans_removed\":{},\"files_removed\":{}}}",
                    dry_run,
                    mounted.len(),
                    plan.doomed.len(),
                    plan.confirmed_present,
                    plan.skipped_unmounted,
                    plan.skipped_unknown_root,
                    stats.snapshots_removed,
                    stats.spans_repaired,
                    stats.spans_removed,
                    stats.files_removed
                );
            } else {
                println!("Mounted target roots: {}", mounted.len());
                println!("Confirmed present:    {}", plan.confirmed_present);
                println!("Skipped (unmounted):  {}", plan.skipped_unmounted);
                if plan.skipped_unknown_root > 0 {
                    println!(
                        "Skipped (retired root): {} — under a mount path no longer in \
                         config; unreachable by any reconcile, clear with \
                         `btrdasd reindex --rebuild`",
                        plan.skipped_unknown_root
                    );
                }
                println!("Stale (absent):       {}", plan.doomed.len());
                if dry_run {
                    println!("\nDry run — nothing was changed.");
                } else if plan.is_empty() {
                    println!("\nIndex already consistent.");
                } else {
                    println!(
                        "\nPruned {} snapshots, repaired {} spans, removed {} spans, dropped {} files.",
                        stats.snapshots_removed,
                        stats.spans_repaired,
                        stats.spans_removed,
                        stats.files_removed
                    );
                }
            }
            mount::require_released(&still_mounted)?;
        }
        Commands::Search { query, db, limit } => {
            let database = Database::open(&db)?;
            let results = database.search(&query, limit)?;
            if json {
                print!("[");
                for (i, r) in results.iter().enumerate() {
                    if i > 0 {
                        print!(",");
                    }
                    print!(
                        "{{\"path\":\"{}\",\"size\":{},\"mtime\":{},\"first_snap\":\"{}\",\"last_snap\":\"{}\"}}",
                        r.path.replace('"', "\\\""),
                        r.size,
                        r.mtime,
                        r.first_snap.replace('"', "\\\""),
                        r.last_snap.replace('"', "\\\"")
                    );
                }
                println!("]");
            } else if results.is_empty() {
                println!("No matches for '{}'", query);
            } else {
                for r in &results {
                    println!(
                        "{}\t{}\t{}\t{}\t{}",
                        r.path, r.size, r.mtime, r.first_snap, r.last_snap
                    );
                }
                println!("({} results)", results.len());
            }
        }
        Commands::List { snapshot, db } => {
            let database = Database::open(&db)?;
            let files = database.list_files_in_snapshot(&snapshot)?;
            if json {
                print!("[");
                for (i, f) in files.iter().enumerate() {
                    if i > 0 {
                        print!(",");
                    }
                    print!("\"{}\"", f.path.replace('"', "\\\""));
                }
                println!("]");
            } else {
                for f in &files {
                    println!("{}", f.path);
                }
                println!("({} files)", files.len());
            }
        }
        Commands::Info { db } => {
            let database = Database::open(&db)?;
            let stats = database.get_stats()?;
            if json {
                println!(
                    "{{\"snapshots\":{},\"files\":{},\"spans\":{},\"db_size\":{}}}",
                    stats.snapshot_count, stats.file_count, stats.span_count, stats.db_size
                );
            } else {
                println!("Snapshots:  {}", stats.snapshot_count);
                println!("Files:      {}", stats.file_count);
                println!("Spans:      {}", stats.span_count);
                println!("DB size:    {} bytes", stats.db_size);
            }
        }
        Commands::Setup(args) => {
            setup::run(args)?;
        }

        // ----- Config commands -----
        Commands::Config { action } => match action {
            ConfigAction::DumpEnv { config } => {
                let cfg = Config::load(&config)?;
                print!("{}", setup::env_export::dump_env(&cfg));
            }
            ConfigAction::Show { config } => {
                let cfg = Config::load(&config)?;
                println!("{}", cfg.to_toml()?);
            }
            ConfigAction::Validate { config } => {
                let cfg = Config::load(&config)?;
                let errors = cfg.validate();
                if errors.is_empty() {
                    println!("Config is valid.");
                } else {
                    for e in &errors {
                        eprintln!("  - {e}");
                    }
                    std::process::exit(1);
                }
            }
            ConfigAction::Edit { config } => {
                let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".to_string());
                let status = std::process::Command::new(&editor).arg(&config).status()?;
                if !status.success() {
                    eprintln!("Editor exited with non-zero status");
                    std::process::exit(1);
                }
            }
        },

        // ----- Backup commands -----
        Commands::Backup { action } => match action {
            BackupAction::Run {
                config,
                dry_run,
                full,
                sources,
                targets,
            } => {
                let cfg = Config::load(&config)?;
                let options = backup_run_options(dry_run, full, sources, targets);
                let progress = CliProgress;
                // The same job the GUI runs (`backup::run_backup_job`): the
                // interlock (bd DAS-Backup-Manager-pe6), subvolume sync, mounts,
                // btrbk, unmount, report, record. The sync report goes to the
                // progress log (stderr), so stdout stays what it was.
                let outcome = backup::run_backup_job(
                    &backup::SystemBackupHost::new(&config, "btrdasd backup run"),
                    cfg,
                    options,
                    &progress,
                );
                // 0 ran clean (or declined), 3 began and something failed or
                // aborted, 1 could not start: the doctor's rule (bd `vzsu`).
                let exit_code = outcome.exit_code();
                let result = match outcome {
                    backup::BackupJobOutcome::Declined => {
                        println!("A backup is already running — declining.");
                        return Ok(());
                    }
                    // Could not start (1) or began and stopped on a target's or
                    // a source's state (3, recorded as a failed run): the
                    // reason on stderr, the doctor's exit rule.
                    stopped @ (backup::BackupJobOutcome::CouldNotStart(_)
                    | backup::BackupJobOutcome::Aborted(_)) => {
                        let (_, why) = stopped.finish_line(dry_run);
                        eprintln!("Error: {why}");
                        std::process::exit(exit_code);
                    }
                    backup::BackupJobOutcome::Ran(result) => result,
                };
                progress.on_complete(result.success, &backup::backup_summary(&result, dry_run));

                if json {
                    println!("{}", backup_run_json(&result));
                } else {
                    println!("{}", backup_run_line(&result));
                    for e in &result.errors {
                        eprintln!("  ERROR: {e}");
                    }
                }
                // A failed sync or a target left mounted did not stop the run;
                // it is in the result, so the command fails (3) once the run
                // has finished and been recorded.
                if let Some(code) = backup_run_exit(exit_code) {
                    std::process::exit(code);
                }
            }
            BackupAction::Snapshot { config, sources } => {
                let cfg = Config::load(&config)?;
                let progress = CliProgress;
                // Manual backups mount and unmount the DAS filesystems, so they
                // join the same interlock as the scheduled path (bd DAS-Backup-Manager-pe6).
                let _locks =
                    match backup::acquire_manual_locks("btrdasd backup snapshot", &progress)? {
                        BackupLockAttempt::Acquired(locks) => locks,
                        BackupLockAttempt::AlreadyRunning => {
                            println!("A backup is already running — declining.");
                            return Ok(());
                        }
                    };
                let mut source_guard = mount::ensure_sources_mounted(&cfg, &progress);
                // As `backup run` does, with the sources mounted: btrbk.conf is
                // brought into line first (a failed sync stops the step).
                let counted = backup::sync_for_manual_step(&config, &cfg, &progress)
                    .map_err(Box::<dyn std::error::Error>::from)
                    .and_then(|cfg| {
                        buttered_dasd::backup::create_snapshots(
                            &cfg,
                            flag_selection(sources).as_deref(),
                            &progress,
                        )
                    });
                let sources_still_mounted = source_guard.unmount(&progress);
                let count = counted?;
                println!("Created {count} snapshots");
                mount::require_released(&sources_still_mounted)?;
            }
            BackupAction::Send { config, targets } => {
                let cfg = Config::load(&config)?;
                let progress = CliProgress;
                // Manual backups mount and unmount the DAS filesystems, so they
                // join the same interlock as the scheduled path (bd DAS-Backup-Manager-pe6).
                let locks = match backup::acquire_manual_locks("btrdasd backup send", &progress)? {
                    BackupLockAttempt::Acquired(locks) => locks,
                    BackupLockAttempt::AlreadyRunning => {
                        println!("A backup is already running — declining.");
                        return Ok(());
                    }
                };
                let mut source_guard = mount::ensure_sources_mounted(&cfg, &progress);
                // As `backup run` does, with the sources mounted and before the
                // targets are: btrbk.conf is brought into line first, because
                // a send of everything passes btrbk no filter. A failed sync
                // stops the step, with the sources given back.
                let cfg = match backup::sync_for_manual_step(&config, &cfg, &progress) {
                    Ok(cfg) => cfg,
                    Err(why) => {
                        let still = source_guard.unmount(&progress);
                        mount::require_released(&still)?;
                        return Err(why.into());
                    }
                };
                let mut guard =
                    mount::ensure_targets_mounted(&cfg, &progress, locks.maintenance())?;
                let result =
                    buttered_dasd::backup::send_snapshots(&cfg, None, &targets, false, &progress);
                let mut still_mounted = guard.unmount(&progress);
                still_mounted.extend(source_guard.unmount(&progress));
                let (sent, bytes) = result?;
                println!("Sent {sent} snapshots ({})", report::format_bytes(bytes));
                mount::require_released(&still_mounted)?;
            }
            BackupAction::BootArchive { config } => {
                let cfg = Config::load(&config)?;
                let progress = CliProgress;
                // Manual backups mount and unmount the DAS filesystems, so they
                // join the same interlock as the scheduled path (bd DAS-Backup-Manager-pe6).
                let locks =
                    match backup::acquire_manual_locks("btrdasd backup boot-archive", &progress)? {
                        BackupLockAttempt::Acquired(locks) => locks,
                        BackupLockAttempt::AlreadyRunning => {
                            println!("A backup is already running — declining.");
                            return Ok(());
                        }
                    };
                let mut guard =
                    mount::ensure_targets_mounted(&cfg, &progress, locks.maintenance())?;
                let step = buttered_dasd::backup::archive_boot(&cfg, &progress);
                let still_mounted = guard.unmount(&progress);
                println!("Boot subvolumes: {}", step.row());
                let failed = step.failed();
                if let backup::BootStep::Ran(o) = &step {
                    for f in &o.failures {
                        println!("  FAIL  {f}");
                    }
                    for w in &o.warnings {
                        println!("  WARN  {w}");
                    }
                }
                mount::require_released(&still_mounted)?;
                // The script's and the doctor's rule: 3 = it began and
                // something failed; 1 stays "could not start".
                if failed {
                    std::process::exit(3);
                }
            }
            BackupAction::BootPlan { config } => {
                let cfg = Config::load(&config).unwrap_or_else(|e| {
                    eprintln!("Error: cannot read {}: {e}", config.display());
                    std::process::exit(2);
                });
                let plan = match backup::boot_plan(&cfg) {
                    Ok(plan) => plan,
                    Err(why) => {
                        eprintln!("{why}");
                        std::process::exit(2);
                    }
                };
                let mut lines = Vec::new();
                for item in &plan {
                    let name = item.snapshot_name.as_deref().unwrap_or("-");
                    let dirs = if item.subdirs.is_empty() {
                        "-".to_string()
                    } else {
                        item.subdirs.join(",")
                    };
                    let unsafe_field = [item.subvol.as_str(), name, dirs.as_str()]
                        .iter()
                        .any(|f| f.contains(['\t', '\n']))
                        || item.subdirs.iter().any(|d| d.contains(','));
                    if unsafe_field {
                        eprintln!(
                            "boot subvolume {:?}: a tab, newline or comma in a field cannot be passed to backup-run.sh",
                            item.subvol
                        );
                        std::process::exit(2);
                    }
                    lines.push(format!("{}\t{name}\t{dirs}", item.subvol));
                }
                for line in lines {
                    println!("{line}");
                }
            }
            BackupAction::Report { db, limit } => {
                let database = Database::open(&db)?;
                let runs = database.get_backup_history(limit)?;
                if json {
                    print!("[");
                    for (i, run) in runs.iter().enumerate() {
                        if i > 0 {
                            print!(",");
                        }
                        print!(
                            "{{\"id\":{},\"timestamp\":\"{}\",\"mode\":\"{}\",\"success\":{},\"duration_secs\":{},\"snaps_created\":{},\"snaps_sent\":{},\"bytes_sent\":{}}}",
                            run.id,
                            run.timestamp,
                            run.mode,
                            run.success,
                            run.duration_secs,
                            serde_json::Value::from(run.snaps_created),
                            serde_json::Value::from(run.snaps_sent),
                            run.bytes_sent
                        );
                    }
                    println!("]");
                } else if runs.is_empty() {
                    println!("No backup history found.");
                } else {
                    println!(
                        "{:<20} {:<12} {:<8} {:<10} {:<8} {:<8}",
                        "Timestamp", "Mode", "Status", "Duration", "Created", "Sent"
                    );
                    println!("{}", "-".repeat(70));
                    for run in &runs {
                        println!(
                            "{:<20} {:<12} {:<8} {:<10} {:<8} {:<8}",
                            run.timestamp,
                            run.mode,
                            if run.success { "OK" } else { "FAIL" },
                            format!("{}s", run.duration_secs),
                            report::format_count(run.snaps_created),
                            report::format_count(run.snaps_sent)
                        );
                    }
                }
            }
            BackupAction::RecordRun {
                db,
                success,
                mode,
                snaps_created,
                snaps_sent,
                // Said, never implied: clap takes either both counts or this
                // flag and never both, so the counts are None exactly when it
                // was given — the unknown the row then stores as NULL.
                counts_unknown: _,
                bytes_sent,
                duration_secs,
                errors,
            } => {
                use buttered_dasd::db::NewBackupRun;
                let database = Database::open(&db)?;
                let timestamp = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)?
                    .as_secs() as i64;
                let error_list: Vec<String> = if errors.is_empty() {
                    Vec::new()
                } else {
                    errors.split('\n').map(|s| s.to_string()).collect()
                };
                let id = database.insert_backup_run(&NewBackupRun {
                    timestamp,
                    success,
                    mode: &mode,
                    snaps_created,
                    snaps_sent,
                    bytes_sent,
                    duration_secs,
                    errors: &error_list,
                })?;
                if json {
                    println!("{{\"id\":{id},\"timestamp\":{timestamp}}}");
                } else {
                    println!("Recorded backup run (id={id})");
                }
            }
        },

        // ----- Restore commands -----
        Commands::Restore { action } => {
            let lock = CliLock::production();
            let interactive = |no_wait| Interactive {
                json,
                no_wait,
                lock: &lock,
                progress: &CliProgress,
            };
            let ran = match action {
                RestoreAction::File {
                    snapshot,
                    dest,
                    files,
                    config,
                    no_wait,
                } => cmd_restore_file(&snapshot, &dest, &files, &config, &interactive(no_wait))?,
                RestoreAction::Snapshot {
                    snapshot,
                    dest,
                    config,
                    no_wait,
                } => cmd_restore_snapshot(&snapshot, &dest, &config, &interactive(no_wait))?,
                RestoreAction::Browse {
                    snapshot,
                    prefix,
                    config,
                    no_wait,
                } => cmd_restore_browse(
                    &snapshot,
                    prefix.as_deref(),
                    &config,
                    &interactive(no_wait),
                )?,
            };
            if let Some(code) = deferred_exit_code(&ran) {
                std::process::exit(code);
            }
        }

        // ----- Schedule commands -----
        Commands::Schedule { action } => match action {
            ScheduleAction::Show { config } => {
                let cfg = Config::load(&config)?;
                let info = schedule::get_schedule(&cfg)?;
                if json {
                    println!(
                        "{{\"incremental_time\":\"{}\",\"full_schedule\":\"{}\",\"delay_min\":{},\"enabled\":{},\"next_incremental\":{},\"next_full\":{}}}",
                        info.incremental_time,
                        info.full_schedule,
                        info.delay_min,
                        info.enabled,
                        info.next_incremental
                            .as_ref()
                            .map_or("null".to_string(), |s| format!("\"{s}\"")),
                        info.next_full
                            .as_ref()
                            .map_or("null".to_string(), |s| format!("\"{s}\""))
                    );
                } else {
                    println!("Incremental: {} (daily)", info.incremental_time);
                    println!("Full:        {}", info.full_schedule);
                    println!("Delay:       {} min randomized", info.delay_min);
                    println!(
                        "Status:      {}",
                        if info.enabled { "enabled" } else { "disabled" }
                    );
                    if let Some(next) = &info.next_incremental {
                        println!("Next incr:   {next}");
                    }
                    if let Some(next) = &info.next_full {
                        println!("Next full:   {next}");
                    }
                }
            }
            ScheduleAction::Set {
                config,
                incremental,
                full,
                delay,
            } => {
                let mut cfg = Config::load(&config)?;
                schedule::set_schedule(&mut cfg, incremental.as_deref(), full.as_deref(), delay)?;
                let toml = cfg.to_toml()?;
                std::fs::write(&config, toml)?;
                println!("Schedule updated. Config written to {}", config.display());
            }
            ScheduleAction::Enable { config } => {
                let cfg = Config::load(&config)?;
                schedule::set_enabled(&cfg, true)?;
                println!("Scheduled backups enabled.");
            }
            ScheduleAction::Disable { config } => {
                let cfg = Config::load(&config)?;
                schedule::set_enabled(&cfg, false)?;
                println!("Scheduled backups disabled.");
            }
            ScheduleAction::Next { config } => {
                let cfg = Config::load(&config)?;
                let info = schedule::get_schedule(&cfg)?;
                if json {
                    println!(
                        "{{\"next_incremental\":{},\"next_full\":{}}}",
                        info.next_incremental
                            .as_ref()
                            .map_or("null".to_string(), |s| format!("\"{s}\"")),
                        info.next_full
                            .as_ref()
                            .map_or("null".to_string(), |s| format!("\"{s}\""))
                    );
                } else {
                    match &info.next_incremental {
                        Some(next) => println!("Next incremental: {next}"),
                        None => println!("Next incremental: not scheduled"),
                    }
                    match &info.next_full {
                        Some(next) => println!("Next full:        {next}"),
                        None => println!("Next full:        not scheduled"),
                    }
                }
            }
        },

        // ----- Subvol commands -----
        Commands::Subvol { action } => match action {
            SubvolAction::List { config } => {
                let cfg = Config::load(&config)?;
                let subs = subvol::list_subvolumes(&cfg);
                if json {
                    print!("[");
                    for (i, sv) in subs.iter().enumerate() {
                        if i > 0 {
                            print!(",");
                        }
                        print!(
                            "{{\"source\":\"{}\",\"name\":\"{}\",\"manual_only\":{}}}",
                            sv.source_label, sv.name, sv.manual_only
                        );
                    }
                    println!("]");
                } else {
                    println!("{:<16} {:<16} Schedule", "Source", "Subvolume");
                    println!("{}", "-".repeat(48));
                    for sv in &subs {
                        println!(
                            "{:<16} {:<16} {}",
                            sv.source_label,
                            sv.name,
                            if sv.manual_only { "manual" } else { "auto" }
                        );
                    }
                }
            }
            SubvolAction::Add {
                source,
                name,
                manual_only,
                config,
            } => {
                let mut cfg = Config::load(&config)?;
                subvol::add_subvolume(&mut cfg, &source, &name, manual_only)?;
                buttered_dasd::btrbk_conf::save_config_and_btrbk_conf(&cfg, &config)?;
                println!("Added subvolume '{name}' to source '{source}'.");
            }
            SubvolAction::Remove {
                source,
                name,
                config,
            } => {
                let mut cfg = Config::load(&config)?;
                subvol::remove_subvolume(&mut cfg, &source, &name)?;
                buttered_dasd::btrbk_conf::save_config_and_btrbk_conf(&cfg, &config)?;
                println!("Removed subvolume '{name}' from source '{source}'.");
            }
            SubvolAction::SetManual {
                source,
                name,
                config,
            } => {
                let mut cfg = Config::load(&config)?;
                subvol::set_manual(&mut cfg, &source, &name, true)?;
                buttered_dasd::btrbk_conf::save_config_and_btrbk_conf(&cfg, &config)?;
                println!("Subvolume '{name}' in source '{source}' set to manual-only.");
            }
            SubvolAction::SetAuto {
                source,
                name,
                config,
            } => {
                let mut cfg = Config::load(&config)?;
                subvol::set_manual(&mut cfg, &source, &name, false)?;
                buttered_dasd::btrbk_conf::save_config_and_btrbk_conf(&cfg, &config)?;
                println!("Subvolume '{name}' in source '{source}' set to automatic.");
            }
            SubvolAction::Sync {
                config,
                dry_run,
                render_btrbk_conf,
            } => {
                let outcome = match buttered_dasd::adopt::sync_subvolumes(
                    &config,
                    dry_run,
                    &buttered_dasd::caldate::today(),
                    &buttered_dasd::fsutil::SystemRunner,
                    &buttered_dasd::health::is_mountpoint,
                ) {
                    Ok(outcome) => outcome,
                    Err(e) => {
                        eprintln!("Error: {e}");
                        std::process::exit(2);
                    }
                };
                print!(
                    "{}",
                    buttered_dasd::adopt::format_sync_report(&outcome, dry_run)
                );
                let mut failed = outcome.failed();
                if let Some(path) = &render_btrbk_conf
                    && let Err(e) = buttered_dasd::adopt::write_planned_btrbk_conf(&outcome, path)
                {
                    eprintln!("Error: {e}");
                    failed = true;
                }
                if failed {
                    std::process::exit(1);
                }
            }
            SubvolAction::Expire {
                config,
                db,
                dry_run,
            } => {
                let outcome = match buttered_dasd::expire::expire_retired(
                    &config,
                    dry_run,
                    &buttered_dasd::caldate::today(),
                    &buttered_dasd::fsutil::SystemRunner,
                    &buttered_dasd::health::is_mountpoint,
                ) {
                    Ok(outcome) => outcome,
                    Err(e) => {
                        eprintln!("Error: {e}");
                        std::process::exit(2);
                    }
                };
                print!(
                    "{}",
                    buttered_dasd::expire::format_expire_report(&outcome, dry_run)
                );
                if expire_failed(outcome.failed(), &outcome.deleted_paths(), &db) {
                    std::process::exit(1);
                }
            }
        },

        // ----- Health command -----
        Commands::Health { config } => {
            let cfg = Config::load(&config)?;
            let report = buttered_dasd::health::get_health(&cfg)?;
            if json {
                print!("{{\"status\":\"");
                match report.status {
                    HealthStatus::Healthy => print!("healthy"),
                    HealthStatus::Warning => print!("warning"),
                    HealthStatus::Critical => print!("critical"),
                }
                print!("\",\"last_backup\":");
                match &report.last_backup {
                    Some(lb) => print!("\"{lb}\""),
                    None => print!("null"),
                }
                print!(",\"targets\":[");
                for (i, t) in report.targets.iter().enumerate() {
                    if i > 0 {
                        print!(",");
                    }
                    let scrub_status = match t.scrub.status {
                        health::ScrubHealthStatus::NotApplicable => "not_applicable",
                        health::ScrubHealthStatus::NeverScrubbed => "never_scrubbed",
                        health::ScrubHealthStatus::Unresolved => "unresolved",
                        health::ScrubHealthStatus::Ok => "ok",
                        health::ScrubHealthStatus::Warn => "warn",
                        health::ScrubHealthStatus::Fail => "fail",
                    };
                    print!(
                        "{{\"label\":\"{}\",\"serial\":\"{}\",\"mounted\":{},\"total_bytes\":{},\"used_bytes\":{},\"snapshot_count\":{},\"smart_status\":{},\"scrub\":{{\"status\":\"{}\",\"age_days\":{},\"last_outcome\":{},\"error_total\":{}}}}}",
                        t.label,
                        t.serial,
                        t.mounted,
                        t.total_bytes,
                        t.used_bytes,
                        t.snapshot_count,
                        t.smart_status
                            .as_ref()
                            .map_or("null".to_string(), |s| format!("\"{s}\"")),
                        scrub_status,
                        t.scrub
                            .age_days
                            .map_or("null".to_string(), |a| a.to_string()),
                        t.scrub
                            .last_outcome
                            .as_ref()
                            .map_or("null".to_string(), |o| format!("\"{o}\"")),
                        t.scrub
                            .error_total
                            .map_or("null".to_string(), |e| e.to_string()),
                    );
                }
                print!(
                    "],\"recovery_os\":{}",
                    serde_json::Value::from(report.recovery_os.clone())
                );
                print!(",\"warnings\":[");
                for (i, w) in report.warnings.iter().enumerate() {
                    if i > 0 {
                        print!(",");
                    }
                    print!("\"{}\"", w.replace('"', "\\\""));
                }
                println!("]}}");
            } else {
                let status_str = match report.status {
                    HealthStatus::Healthy => "HEALTHY",
                    HealthStatus::Warning => "WARNING",
                    HealthStatus::Critical => "CRITICAL",
                };
                println!("Backup System Health: {status_str}");
                println!();
                if let Some(lb) = &report.last_backup {
                    println!("Last backup: {lb}");
                }
                println!();
                println!(
                    "{:<16} {:<12} {:>10} {:>10} {:>6} {:<10} {:<15} {:>5}",
                    "Target", "Serial", "Used", "Total", "Use%", "SMART", "Scrub", "Age"
                );
                println!("{}", "-".repeat(90));
                for t in &report.targets {
                    let scrub_word = match t.scrub.status {
                        health::ScrubHealthStatus::NotApplicable => "-",
                        health::ScrubHealthStatus::NeverScrubbed => "NEVER SCRUBBED",
                        health::ScrubHealthStatus::Unresolved => "UNRESOLVED",
                        health::ScrubHealthStatus::Ok => "OK",
                        health::ScrubHealthStatus::Warn => "WARN",
                        health::ScrubHealthStatus::Fail => "FAIL",
                    };
                    let scrub_age = t
                        .scrub
                        .age_days
                        .map(|a| format!("{a}d"))
                        .unwrap_or_else(|| "-".to_string());
                    if !t.mounted {
                        println!(
                            "{:<16} {:<12} {:>10} {:>10} {:>6} {:<10} {:<15} {:>5}",
                            t.label, t.serial, "-", "-", "-", "not mounted", scrub_word, scrub_age
                        );
                        continue;
                    }
                    println!(
                        "{:<16} {:<12} {:>10} {:>10} {:>5.1}% {:<10} {:<15} {:>5}",
                        t.label,
                        t.serial,
                        buttered_dasd::report::format_bytes(t.used_bytes),
                        buttered_dasd::report::format_bytes(t.total_bytes),
                        t.usage_percent(),
                        t.smart_status.as_deref().unwrap_or("N/A"),
                        scrub_word,
                        scrub_age
                    );
                }
                if !report.recovery_os.is_empty() {
                    println!();
                    println!("Recovery OS:");
                    for line in &report.recovery_os {
                        println!("  {line}");
                    }
                }
                if !report.warnings.is_empty() {
                    println!();
                    println!("Warnings:");
                    for w in &report.warnings {
                        println!("  - {w}");
                    }
                }
            }
        }

        // ----- Recovery OS commands -----
        Commands::RecoveryOs { action } => {
            std::process::exit(run_recovery_os(action, json));
        }

        // ----- Scrub commands -----
        Commands::Scrub { action } => match action {
            ScrubAction::Run { config } => {
                let cfg = Config::load(&config)?;
                if !cfg.scrub.enabled {
                    eprintln!(
                        "NOTE: [scrub].enabled = false in {} — proceeding anyway. \
                         'btrdasd scrub run' is a manual/forced invocation; the enabled \
                         flag only gates the scheduled systemd timer, never a direct run.",
                        config.display()
                    );
                }
                let progress = CliProgress;
                let pass = scrub::run_scrub_pass(&cfg, &progress)?;
                let exit_code = exit_code_for_pass(&pass);
                let status_word = match pass.status {
                    scrub::PassStatus::Completed => "completed",
                    scrub::PassStatus::Skipped => "skipped",
                };
                if json {
                    println!(
                        "{{\"status\":\"{}\",\"success\":{},\"targets_attempted\":{},\"targets_failed\":{}}}",
                        status_word,
                        pass.success(),
                        pass.results.len(),
                        pass.failed_count(),
                    );
                } else {
                    println!(
                        "Scrub pass {status_word}: {} of {} filesystems clean",
                        pass.results.len() - pass.failed_count(),
                        pass.results.len()
                    );
                    if !pass.success() && exit_code == 0 {
                        // The pass ran (exit 0) despite failures — make sure a
                        // console reader isn't misled by the exit code alone.
                        // When exit_code is nonzero instead (nothing was ever
                        // resolvable), the exit code itself already signals
                        // the problem, so this note would be redundant there.
                        println!(
                            "NOTE: failures were detected in the pass above (per-FS errors, \
                             aborted scrubs, or unmount problems) — this is reported via the \
                             FAILURE email and health Critical escalation, not via this \
                             command's exit code. See 'btrdasd scrub status' or 'btrdasd health'."
                        );
                    }
                }
                std::process::exit(exit_code);
            }
            ScrubAction::Status { config } => {
                let cfg = Config::load(&config)?;
                let (views, warning) = gather_scrub_status(&cfg);
                if let Some(w) = &warning {
                    eprintln!("  [WARN]  {w}");
                }
                if json {
                    print!("[");
                    for (i, v) in views.iter().enumerate() {
                        if i > 0 {
                            print!(",");
                        }
                        print!("{}", scrub_target_json(v));
                    }
                    println!("]");
                } else {
                    print!("{}", format_scrub_status(&views, &cfg));
                }
            }
            ScrubAction::Cancel { config } => {
                let cfg = Config::load(&config)?;
                match cancel_running_scrub(&cfg) {
                    Ok(msg) => {
                        if json {
                            println!("{{\"message\":\"{}\"}}", msg.replace('"', "\\\""));
                        } else {
                            println!("{msg}");
                        }
                    }
                    Err(e) => {
                        eprintln!("Error: {e}");
                        std::process::exit(1);
                    }
                }
            }
        },

        // ----- Doctor command -----
        Commands::Doctor {
            check_drift: _,
            email,
            config,
        } => {
            let cfg = match Config::load(&config) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("Error: could not load config {}: {e}", config.display());
                    std::process::exit(2);
                }
            };
            let progress = CliProgress;
            let outcome = match doctor::run_drift_check(&cfg, &progress) {
                Ok(o) => o,
                Err(e) => {
                    eprintln!("Error: {e}");
                    if email {
                        let failure_text = format!(
                            "═══════════════════════════════════════════════════════════\n  \
                             DAS Subvolume Drift Report\n  Status: COULD NOT RUN — FAILURE\n\
                             ═══════════════════════════════════════════════════════════\n\n\
                             The drift check could not run: {e}\n"
                        );
                        match report::send_email_report_with_kind(&failure_text, &cfg, "Doctor") {
                            Ok(()) => eprintln!("Doctor failure report emailed"),
                            Err(mail_err) => {
                                eprintln!("Could not send doctor failure report email: {mail_err}")
                            }
                        }
                    }
                    std::process::exit(2);
                }
            };

            let exit_code = exit_code_for_doctor(&outcome);

            match &outcome {
                doctor::DoctorOutcome::Deferred { reason } => {
                    if json {
                        println!("{}", doctor_json(&outcome)?);
                    } else {
                        println!("{reason}");
                    }
                }
                doctor::DoctorOutcome::Ran(dr) => {
                    if json {
                        println!("{}", doctor_json(&outcome)?);
                    } else {
                        print!("{}", doctor::format_report(dr));
                    }

                    if email && (!dr.ran() || dr.not_clean()) {
                        let email_text = doctor::format_report(dr);
                        match report::send_email_report_with_kind(&email_text, &cfg, "Doctor") {
                            Ok(()) => eprintln!("Doctor report emailed"),
                            Err(e) => {
                                eprintln!("Could not send doctor report email: {e}")
                            }
                        }
                    }
                }
            }

            std::process::exit(exit_code);
        }

        // ----- Completions command -----
        Commands::Completions { shell } => {
            let mut cmd = Cli::command();
            generate(shell, &mut cmd, "btrdasd", &mut std::io::stdout());
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use buttered_dasd::config::{Retention, Target, TargetRole};
    use std::sync::Mutex;

    /// Serializes tests that mutate process-wide env vars
    /// (`DAS_SCRUB_STATE` / `DAS_BTRFS_STATUS_DIR`) — `cargo test` runs test
    /// functions in parallel by default within one binary.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Standard clap sanity check: catches conflicting arg definitions,
    /// missing help text, and other structural mistakes in the `Cli` tree
    /// (including the new `Scrub`/`ScrubAction` variants) without needing to
    /// actually invoke the binary.
    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn backup_run_asks_the_job_for_what_its_flags_say() {
        let full = backup_run_options(true, true, vec!["a".into()], vec!["t1".into(), "t2".into()]);
        assert_eq!(full.mode, Some(BackupMode::Full));
        assert_eq!(full.sources, Some(vec!["a".to_string()]));
        assert_eq!(full.targets, Some(vec!["t1".to_string(), "t2".to_string()]));
        assert!(full.dry_run && full.boot_archive && full.index_after && full.email_report);
        // No flags: incremental, nothing specified, not a dry run.
        let plain = backup_run_options(false, false, vec![], vec![]);
        assert_eq!(plain.mode, Some(BackupMode::Incremental));
        assert_eq!((plain.sources, plain.targets), (None, None));
        assert!(!plain.dry_run);
        assert!(plain.boot_archive && plain.index_after && plain.email_report);
    }

    #[test]
    fn backup_run_exits_with_the_jobs_status_unless_it_is_0() {
        assert_eq!(backup_run_exit(0), None);
        assert_eq!(backup_run_exit(3), Some(3));
        assert_eq!(backup_run_exit(1), Some(1));
    }

    #[test]
    fn backup_run_prints_unknown_counts_as_null_and_unknown_never_as_zero() {
        let result = |created, sent| backup::BackupResult {
            success: false,
            mode: BackupMode::Incremental,
            snapshots_created: created,
            snapshots_sent: sent,
            snapshots_cleaned: 0,
            bytes_sent: 0,
            boot: backup::BootStep::NotSelected,
            indexed: false,
            report_sent: false,
            errors: Vec::new(),
            duration_secs: 7,
        };
        assert_eq!(
            backup_run_json(&result(None, Some(0))),
            "{\"success\":false,\"snapshots_created\":null,\"snapshots_sent\":0,\"bytes_sent\":0,\"duration_secs\":7}"
        );
        assert_eq!(
            backup_run_line(&result(None, Some(0))),
            "Backup FAILED: snapshots created: unknown, sent: 0, 0 B in 7s"
        );
        // The counter-case: measured counts print as numbers.
        let ok = backup::BackupResult {
            success: true,
            ..result(Some(2), Some(3))
        };
        assert_eq!(
            backup_run_json(&ok),
            "{\"success\":true,\"snapshots_created\":2,\"snapshots_sent\":3,\"bytes_sent\":0,\"duration_secs\":7}"
        );
        assert_eq!(
            backup_run_line(&ok),
            "Backup succeeded: snapshots created: 2, sent: 3, 0 B in 7s"
        );
    }

    /// `backup run --sources/--targets` and what the run is handed (bd
    /// DAS-Backup-Manager-7tx): no flag is "all"; a flag is exactly what it
    /// names; a flag naming nothing real is a label nobody has, never "all".
    #[test]
    fn backup_run_flags_become_a_selection_and_never_widen_to_all() {
        let selection = |extra: &[&str]| {
            let mut argv = vec!["btrdasd", "backup", "run"];
            argv.extend_from_slice(extra);
            match Cli::try_parse_from(argv).unwrap().command {
                Commands::Backup {
                    action:
                        BackupAction::Run {
                            sources, targets, ..
                        },
                } => (flag_selection(sources), flag_selection(targets)),
                _ => panic!("not backup run"),
            }
        };
        assert_eq!(selection(&[]), (None, None), "no flag: not specified");
        assert_eq!(
            selection(&["--sources", "a,b", "--targets", "t"]),
            (
                Some(vec!["a".to_string(), "b".to_string()]),
                Some(vec!["t".to_string()])
            )
        );
        // An empty value is not "no flag": it is a label that matches nothing.
        let (sources, targets) = selection(&["--sources", "", "--targets", ""]);
        assert_eq!(sources, Some(vec![String::new()]));
        assert_eq!(targets, Some(vec![String::new()]));
    }

    /// `backup record-run` takes both counts, or `--counts-unknown` — never
    /// both, never neither, never a negative number. `--snaps-created -1` was
    /// refused as an unknown option, so a run whose counts were unknown was
    /// not recorded at all (bd DAS-Backup-Manager-6wt).
    #[test]
    fn record_run_takes_both_counts_or_counts_unknown() {
        use clap::error::ErrorKind;
        let parse = |extra: &[&str]| {
            let mut argv = vec!["btrdasd", "backup", "record-run"];
            argv.extend_from_slice(extra);
            Cli::try_parse_from(argv)
        };
        let counts = |cli: Cli| match cli.command {
            Commands::Backup {
                action:
                    BackupAction::RecordRun {
                        snaps_created,
                        snaps_sent,
                        counts_unknown,
                        ..
                    },
            } => (snaps_created, snaps_sent, counts_unknown),
            _ => panic!("not record-run"),
        };
        let kind = |extra: &[&str]| parse(extra).err().map(|e| e.kind());

        assert_eq!(
            counts(parse(&["--snaps-created", "53", "--snaps-sent", "94"]).unwrap()),
            (Some(53), Some(94), false)
        );
        assert_eq!(
            counts(parse(&["--snaps-created", "0", "--snaps-sent", "0"]).unwrap()),
            (Some(0), Some(0), false),
            "a measured zero is a count"
        );
        assert_eq!(
            counts(parse(&["--counts-unknown"]).unwrap()),
            (None, None, true)
        );
        assert_eq!(
            kind(&["--counts-unknown", "--snaps-created", "1"]),
            Some(ErrorKind::ArgumentConflict)
        );
        assert_eq!(
            kind(&["--counts-unknown", "--snaps-sent", "1"]),
            Some(ErrorKind::ArgumentConflict)
        );
        assert_eq!(kind(&[]), Some(ErrorKind::MissingRequiredArgument));
        assert_eq!(
            kind(&["--snaps-created", "1"]),
            Some(ErrorKind::MissingRequiredArgument)
        );
        assert_eq!(
            kind(&["--snaps-sent", "1"]),
            Some(ErrorKind::MissingRequiredArgument)
        );
        assert!(kind(&["--snaps-created", "-1", "--snaps-sent", "-1"]).is_some());
        assert!(kind(&["--snaps-created=-1", "--snaps-sent=0"]).is_some());
    }

    #[test]
    fn scrub_subcommands_parse() {
        let run = Cli::try_parse_from(["btrdasd", "scrub", "run"]).unwrap();
        assert!(matches!(
            run.command,
            Commands::Scrub {
                action: ScrubAction::Run { .. }
            }
        ));

        let status = Cli::try_parse_from(["btrdasd", "scrub", "status"]).unwrap();
        assert!(matches!(
            status.command,
            Commands::Scrub {
                action: ScrubAction::Status { .. }
            }
        ));

        let cancel = Cli::try_parse_from(["btrdasd", "scrub", "cancel"]).unwrap();
        assert!(matches!(
            cancel.command,
            Commands::Scrub {
                action: ScrubAction::Cancel { .. }
            }
        ));

        // A bare "scrub" with no action must fail, not silently no-op.
        assert!(Cli::try_parse_from(["btrdasd", "scrub"]).is_err());
    }

    // ---- recovery-os hold-disk (bd DAS-Backup-Manager-7wb) ----

    #[test]
    fn recovery_os_hold_disk_parses_and_needs_a_device() {
        let disk = "/dev/disk/by-id/ata-ST2000DM008-2FR102_ZK208Q77";
        let cli =
            Cli::try_parse_from(["btrdasd", "recovery-os", "hold-disk", "--device", disk]).unwrap();
        match cli.command {
            Commands::RecoveryOs {
                action: RecoveryOsAction::HoldDisk { device },
            } => assert_eq!(device, PathBuf::from(disk)),
            _ => panic!("parsed as something other than recovery-os hold-disk"),
        }
        assert!(Cli::try_parse_from(["btrdasd", "recovery-os", "hold-disk"]).is_err());
    }

    /// An internal helper: documented in the man page, not offered in help.
    #[test]
    fn recovery_os_hold_disk_is_not_offered_in_the_help() {
        let mut cli = Cli::command();
        let help = cli
            .find_subcommand_mut("recovery-os")
            .unwrap()
            .render_help()
            .to_string();
        assert!(
            help.contains("inspect") && help.contains("status"),
            "{help}"
        );
        assert!(!help.contains("hold-disk"), "{help}");
    }

    #[test]
    fn recovery_os_hold_disk_refuses_json_and_anything_but_a_disk() {
        assert!(hold_disk_json_refusal(true).is_some());
        assert_eq!(hold_disk_json_refusal(false), None);
        // In a thread of its own: a hold blocks the termination signals in
        // the calling thread.
        std::thread::spawn(|| {
            let dir = tempfile::tempdir().unwrap();
            let device = dir.path().join("disk.img");
            std::fs::write(&device, b"x").unwrap();
            let hold = |json| {
                run_recovery_os(
                    RecoveryOsAction::HoldDisk {
                        device: device.clone(),
                    },
                    json,
                )
            };
            assert_eq!(hold(true), 2);
            assert_eq!(hold(false), 2);
        })
        .join()
        .unwrap();
    }

    fn set_env(key: &str, value: &std::path::Path) {
        // SAFETY: callers hold `ENV_LOCK` for the duration of the mutation
        // and any code that reads the var, so no other thread observes a
        // torn value.
        unsafe { std::env::set_var(key, value) };
    }

    fn clear_env(key: &str) {
        // SAFETY: see `set_env`.
        unsafe { std::env::remove_var(key) };
    }

    fn test_target(label: &str, mount_uuid: &str, mount: &str) -> Target {
        Target {
            label: label.to_string(),
            serial: String::new(),
            serials: Vec::new(),
            mount_uuid: Some(mount_uuid.to_string()),
            mount: mount.to_string(),
            role: TargetRole::Primary,
            retention: Retention::default(),
            display_name: label.to_string(),
        }
    }

    fn test_config(labels_and_uuids: &[(&str, &str)]) -> Config {
        let mut config = Config::default();
        config.scrub.targets = labels_and_uuids
            .iter()
            .map(|(l, _)| l.to_string())
            .collect();
        config.targets = labels_and_uuids
            .iter()
            .map(|(label, uuid)| test_target(label, uuid, &format!("/mnt/{label}")))
            .collect();
        config
    }

    /// A target with no scrub-state entry and no btrfs record at all must
    /// report "never scrubbed" — not an error, not a crash. This is the
    /// exact shape `system-recovery-B-2tb` was in before its first scrub
    /// (bd DAS-Backup-Manager-0kn acceptance criterion).
    #[test]
    fn never_scrubbed_target_is_graceful() {
        let _guard = ENV_LOCK.lock().unwrap();
        let tmp =
            std::env::temp_dir().join(format!("btrdasd-scrub-test-never-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let state_path = tmp.join("scrub-state.json");
        let btrfs_dir = tmp.join("btrfs-status");
        std::fs::create_dir_all(&btrfs_dir).unwrap();

        set_env("DAS_SCRUB_STATE", &state_path);
        set_env("DAS_BTRFS_STATUS_DIR", &btrfs_dir);

        let config = test_config(&[("never-target", "11111111-1111-1111-1111-111111111111")]);
        let (views, warning) = gather_scrub_status(&config);

        clear_env("DAS_SCRUB_STATE");
        clear_env("DAS_BTRFS_STATUS_DIR");
        let _ = std::fs::remove_dir_all(&tmp);

        assert!(warning.is_none(), "a missing state file is not an error");
        assert_eq!(views.len(), 1);
        let v = &views[0];
        assert_eq!(v.source, "never");
        assert_eq!(v.status_word(), "NEVER SCRUBBED");
        assert_eq!(
            v.fsuuid.as_deref(),
            Some("11111111-1111-1111-1111-111111111111")
        );
        assert!(v.last_success_epoch.is_none());
        assert!(v.bytes_scrubbed.is_none());

        // Must also render and JSON-encode without panicking.
        let table = format_scrub_status(&views, &config);
        assert!(table.contains("NEVER SCRUBBED"));
        let json = scrub_target_json(v);
        assert!(json.contains("\"status\":\"NEVER SCRUBBED\""));
        assert!(json.contains("\"last_success_epoch\":null"));
    }

    /// A target with a raw btrfs record but no entry in the engine's own
    /// state file (the real shape of all three DAS filesystems today — see
    /// bd DAS-Backup-Manager-0kn) must be reported from that record, not
    /// treated as "never scrubbed".
    #[test]
    fn btrfs_record_fallback_when_state_has_no_entry() {
        let _guard = ENV_LOCK.lock().unwrap();
        let tmp = std::env::temp_dir().join(format!(
            "btrdasd-scrub-test-fallback-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        let state_path = tmp.join("scrub-state.json");
        let btrfs_dir = tmp.join("btrfs-status");
        std::fs::create_dir_all(&btrfs_dir).unwrap();

        let fsuuid = "22222222-2222-2222-2222-222222222222";
        let record = format!(
            "scrub status:1\n{fsuuid}:1|data_extents_scrubbed:10|tree_extents_scrubbed:1|\
data_bytes_scrubbed:1048576|tree_bytes_scrubbed:4096|read_errors:0|csum_errors:0|\
verify_errors:0|no_csum:0|csum_discards:0|super_errors:0|malloc_errors:0|\
uncorrectable_errors:0|corrected_errors:0|last_physical:1048576|t_start:1785000000|\
t_resumed:0|duration:120|canceled:0|finished:1\n"
        );
        std::fs::write(btrfs_dir.join(format!("scrub.status.{fsuuid}")), record).unwrap();

        set_env("DAS_SCRUB_STATE", &state_path);
        set_env("DAS_BTRFS_STATUS_DIR", &btrfs_dir);

        let config = test_config(&[("fallback-target", fsuuid)]);
        let (views, warning) = gather_scrub_status(&config);

        clear_env("DAS_SCRUB_STATE");
        clear_env("DAS_BTRFS_STATUS_DIR");
        let _ = std::fs::remove_dir_all(&tmp);

        assert!(warning.is_none());
        assert_eq!(views.len(), 1);
        let v = &views[0];
        assert_eq!(v.source, "btrfs");
        assert_eq!(v.status_word(), "OK");
        assert_eq!(v.outcome.as_deref(), Some("finished"));
        assert_eq!(v.ok, Some(true));
        assert_eq!(v.last_success_epoch, Some(1785000000 + 120));
        assert_eq!(v.bytes_scrubbed, Some(1048576 + 4096));
    }

    /// An unresolvable target (no matching `[[target]]`, no serial, no
    /// `mount_uuid`) must surface a clear resolve error, never panic.
    #[test]
    fn unresolvable_target_reports_error_not_panic() {
        let mut config = Config::default();
        config.scrub.targets = vec!["ghost-target".to_string()];
        // Deliberately no matching [[target]] entry.
        config.targets = Vec::new();

        let (views, _warning) = gather_scrub_status(&config);
        assert_eq!(views.len(), 1);
        let v = &views[0];
        assert_eq!(v.source, "unresolved");
        assert_eq!(v.status_word(), "UNRESOLVED");
        assert!(v.resolve_error.is_some());
        assert!(v.fsuuid.is_none());

        let table = format_scrub_status(&views, &config);
        assert!(table.contains("UNRESOLVED"));
        let json = scrub_target_json(v);
        assert!(json.contains("\"fsuuid\":null"));
    }

    /// The critical negative-path proof (`bd DAS-Backup-Manager-0kn`
    /// review, 2026-08-01): idle DAS targets — the real, everyday state
    /// between backups — have configured mount points that are bare,
    /// unmounted directories. `find_actively_scrubbing_target` must never
    /// mistake that for "this filesystem is scrubbing", no matter what is
    /// actually mounted underneath that bare path. `test_target()` sets
    /// `mount = "/mnt/<label>"`, which does not exist as a mountpoint on a
    /// dev box — exactly the idle-target shape.
    #[test]
    fn find_actively_scrubbing_target_skips_unmounted_targets() {
        let config = test_config(&[
            ("bogus-a", "11111111-1111-1111-1111-111111111111"),
            ("bogus-b", "22222222-2222-2222-2222-222222222222"),
        ]);
        assert!(
            find_actively_scrubbing_target(&config).is_none(),
            "an unmounted target's bare directory must never read as an active scrub"
        );
    }

    /// The sharper half of the same proof: point a target's mount at a
    /// path that IS a real, live mountpoint ("/") but tag it with a UUID
    /// that can never match. This is exactly the exploit the reviewer
    /// demonstrated — a bare DAS mount point falling through to a live
    /// filesystem that happens to be mid-scrub for unrelated reasons (a
    /// scheduled `btrfs-scrub@` unit) must still be skipped, proving the
    /// guard is genuinely UUID-driven and not merely "path doesn't exist".
    #[test]
    fn find_actively_scrubbing_target_skips_real_mount_with_mismatched_uuid() {
        let mut config = test_config(&[("root-mismatch", "00000000-0000-0000-0000-000000000000")]);
        config.targets[0].mount = "/".to_string();
        assert!(
            find_actively_scrubbing_target(&config).is_none(),
            "a real mount backed by the WRONG filesystem must never be trusted"
        );
    }

    /// `cancel_running_scrub` probes the real, host-wide
    /// `/run/das-scrub.lock` (it is not overridable — the engine's own
    /// tests take the same real path, single-threaded, for the same
    /// reason: cancel semantics must exercise the actual production lock).
    /// This test therefore cannot assume a particular host state — it may
    /// run unprivileged (EACCES opening the lock), as root with no DAS
    /// scrub running (the ordinary "nothing to cancel" case), or, in
    /// principle, while a real scrub happens to be in progress. What it
    /// asserts is only that every one of those paths returns cleanly
    /// (no panic) with a non-empty, recognizable message — never silence,
    /// never a crash.
    #[test]
    fn cancel_running_scrub_never_panics() {
        let config = test_config(&[]);
        match cancel_running_scrub(&config) {
            Ok(msg) => assert!(
                msg.contains("nothing to cancel")
                    || msg.contains("No scrub pass")
                    || msg.contains("lock held"),
                "unexpected message: {msg}"
            ),
            Err(e) => {
                // Only acceptable failure is a permissions error opening the
                // real /run/das-scrub.lock path when not running as root.
                assert!(
                    e.contains("this command must run as root")
                        || e.contains("could not check scrub lock"),
                    "unexpected error: {e}"
                );
            }
        }
    }

    #[test]
    fn format_duration_secs_formats_hours_and_minutes() {
        assert_eq!(format_duration_secs(59), "0m 59s");
        assert_eq!(format_duration_secs(60), "1m 0s");
        assert_eq!(format_duration_secs(3661), "1h 1m");
        assert_eq!(format_duration_secs(6274), "1h 44m");
    }

    // --- exit_code_for_pass (bd DAS-Backup-Manager-18p) --------------------
    //
    // "Setup failure -> nonzero" (config load errors, lock IO errors,
    // ScrubError::NoTargets) is deliberately NOT tested here: those never
    // produce a `ScrubPass` value at all, so `exit_code_for_pass` is never
    // called for them — they already exit nonzero via `?` propagation out
    // of `scrub::run_scrub_pass` in `main()`. This was exercised live
    // during the original `scrub run` wiring verification (a config with
    // `[scrub].targets = []` correctly produced `Error: NoTargets` and
    // process exit 1, and a disabled/unresolvable-target config correctly
    // propagated a nonzero exit through the same path) — see this task's
    // report file for the transcript.

    /// `scrub_launched` mirrors the real engine invariant: it is only ever
    /// `true` once `Command::spawn()` for `btrfs scrub start` has been
    /// confirmed to succeed, and `started_epoch` is stamped in the same
    /// branch — see `scrub::ScrubFsResult::scrub_launched`.
    fn fake_result(scrub_launched: bool) -> scrub::ScrubFsResult {
        scrub::ScrubFsResult {
            target_label: "t".to_string(),
            fsuuid: "11111111-1111-1111-1111-111111111111".to_string(),
            mountpoint: "/mnt/t".to_string(),
            outcome: None,
            counters: scrub::ScrubCounters::default(),
            bytes_scrubbed: 0,
            started_epoch: if scrub_launched { 1_700_000_100 } else { 0 },
            finished_epoch: 0,
            duration_secs: 0,
            errors: Vec::new(),
            mounted_by_engine: false,
            scrub_launched,
        }
    }

    fn fake_pass(
        status: scrub::PassStatus,
        results: Vec<scrub::ScrubFsResult>,
    ) -> scrub::ScrubPass {
        scrub::ScrubPass {
            status,
            started_epoch: 1_700_000_000,
            finished_epoch: 1_700_003_600,
            results,
            errors: Vec::new(),
        }
    }

    #[test]
    fn exit_code_for_pass_all_clean_is_zero() {
        let mut result = fake_result(true);
        result.outcome = Some(scrub::ScrubOutcome::Finished);
        let pass = fake_pass(scrub::PassStatus::Completed, vec![result]);
        assert_eq!(exit_code_for_pass(&pass), 0);
    }

    #[test]
    fn exit_code_for_pass_ran_with_errors_is_zero() {
        // Scrubbing was attempted and completed, but found real damage
        // (nonzero error counters). The whole point of bd 18p: this must
        // NOT fail the process exit code, or Sentinel would retry-loop a
        // multi-hour pass on hardware that is genuinely failing.
        let mut result = fake_result(true);
        result.outcome = Some(scrub::ScrubOutcome::Finished);
        result.counters.csum_errors = 3;
        result.errors.push("errors found: csum=3".to_string());
        let pass = fake_pass(scrub::PassStatus::Completed, vec![result]);
        assert_eq!(
            exit_code_for_pass(&pass),
            0,
            "per-FS damage must not fail the process exit code"
        );
    }

    #[test]
    fn exit_code_for_pass_aborted_mid_pass_is_zero() {
        // A scrub that launched (scrub_launched set) but died mid-run
        // (Aborted) still counts as "ran" for exit-code purposes.
        let mut result = fake_result(true);
        result.outcome = Some(scrub::ScrubOutcome::Aborted);
        result
            .errors
            .push("scrub aborted (did not complete)".to_string());
        let pass = fake_pass(scrub::PassStatus::Completed, vec![result]);
        assert_eq!(exit_code_for_pass(&pass), 0);
    }

    #[test]
    fn exit_code_for_pass_nothing_resolvable_is_nonzero() {
        // Every target failed before any scrub was ever attempted --
        // scrub_launched stays false because resolve_target_fsuuid/
        // ensure_mounted never succeeded for anything. This is the one case
        // that must be treated as "could not run".
        let mut a = fake_result(false);
        a.errors
            .push("no [[target]] with label 'a' in config".to_string());
        let mut b = fake_result(false);
        b.errors
            .push("cannot mount /mnt/b: exit status 32".to_string());
        let pass = fake_pass(scrub::PassStatus::Completed, vec![a, b]);
        assert_eq!(exit_code_for_pass(&pass), 1);
    }

    /// The exact regression this task's review caught: `Command::spawn()`
    /// for `btrfs scrub start` failing on every target (binary missing,
    /// broken `PATH`, exec format error) is a fast, systemic setup failure —
    /// no scrub was ever issued anywhere. Constructed the way the OLD buggy
    /// code would have produced it (`started_epoch` stamped as if a scrub
    /// began, because that used to happen *before* `spawn()` was even
    /// attempted) to prove `exit_code_for_pass` now ignores `started_epoch`
    /// entirely and keys only on `scrub_launched`.
    #[test]
    fn exit_code_for_pass_spawn_failure_on_all_targets_is_nonzero() {
        let mut spawn_failed = fake_result(false);
        // What the pre-fix bug would have left behind: a non-zero
        // started_epoch despite the child process never launching.
        spawn_failed.started_epoch = 1_700_000_100;
        spawn_failed
            .errors
            .push("cannot start btrfs scrub on /mnt/a: No such file or directory".to_string());
        let pass = fake_pass(scrub::PassStatus::Completed, vec![spawn_failed]);
        assert_eq!(
            exit_code_for_pass(&pass),
            1,
            "a spawn failure on every target must exit nonzero even if started_epoch is \
             (wrongly) non-zero — scrub_launched is the authoritative signal"
        );
    }

    #[test]
    fn exit_code_for_pass_partially_resolvable_is_zero() {
        // The explicit judgment call from the brief: some targets scrubbed,
        // some could not be mounted. Recommendation taken: still 0, since
        // scrubbing genuinely occurred and the skips are surfaced via email
        // / `scrub status` rather than the exit code.
        let mut unresolved = fake_result(false);
        unresolved
            .errors
            .push("no [[target]] with label 'a' in config".to_string());
        let mut scrubbed = fake_result(true);
        scrubbed.outcome = Some(scrub::ScrubOutcome::Finished);
        let pass = fake_pass(scrub::PassStatus::Completed, vec![unresolved, scrubbed]);
        assert_eq!(exit_code_for_pass(&pass), 0);
    }

    #[test]
    fn exit_code_for_pass_singleton_skip_is_zero() {
        let pass = fake_pass(scrub::PassStatus::Skipped, Vec::new());
        assert_eq!(exit_code_for_pass(&pass), 0);
    }

    // -- exit_code_for_doctor (bd DAS-Backup-Manager-01u) --------------------
    //
    // Mirrors the exit_code_for_pass suite above. The `partial_volume_failure`
    // case is the exact regression the reviewer caught: with only `has_drift()`
    // consulted, 1-of-4 volumes failing while the rest are clean produced exit
    // 0 alongside a `DRIFT DETECTED — FAILURE` report and a failure email.

    #[test]
    fn exit_code_for_doctor_clean_is_zero() {
        let report = doctor::DriftReport {
            volumes_checked: 4,
            ..Default::default()
        };
        let outcome = doctor::DoctorOutcome::Ran(report);
        assert_eq!(exit_code_for_doctor(&outcome), 0);
    }

    #[test]
    fn exit_code_for_doctor_drift_is_one() {
        let mut report = doctor::DriftReport {
            volumes_checked: 4,
            ..Default::default()
        };
        report.missing.push(doctor::MissingSubvolume {
            volume: "/.btrfs-hdd".into(),
            source_labels: vec!["hdd-projects".into()],
            name: "ClaudeCodeProjects/new-project".into(),
        });
        let outcome = doctor::DoctorOutcome::Ran(report);
        assert_eq!(exit_code_for_doctor(&outcome), 1);
    }

    #[test]
    fn exit_code_for_doctor_partial_volume_failure_is_three() {
        // 3 of 4 volumes examined cleanly (no missing/stale among them), 1
        // failed to mount/list. `ran()` is true (volumes_checked > 0) and
        // `has_drift()` is false, but this must still be NONZERO — a volume
        // that went unchecked is exactly the kind of gap this tool exists to
        // surface, and the report/email already call it a failure.
        //
        // It is 3 rather than 1 (bd DAS-Backup-Manager-f6p) so the generated
        // unit can carry `SuccessExitStatus=1` for the drift case without also
        // swallowing this one. Drift is a finding; an unexamined volume is an
        // operational fault, and only the latter should fail the unit.
        let report = doctor::DriftReport {
            volumes_checked: 3,
            volumes_failed: vec![("/.btrfs-ssd".into(), "not mounted".into())],
            ..Default::default()
        };
        assert!(report.ran());
        assert!(!report.has_drift());
        assert!(report.not_clean());
        let outcome = doctor::DoctorOutcome::Ran(report);
        assert_eq!(exit_code_for_doctor(&outcome), 3);
    }

    /// `forget` and `purge` fail when their config cannot be read, before
    /// any lock, mount or deletion — never a silent success.
    #[test]
    fn run_deletion_fails_on_a_config_it_cannot_load() {
        let dir = tempfile::tempdir().unwrap();
        let err = run_deletion(
            "/nonexistent/index.db",
            &dir.path().join("absent.toml"),
            false,
            true,
            "forget",
            |_| -> Result<forget::ForgetPlan, forget::ForgetRefusal> {
                panic!("nothing may be selected without a config")
            },
        )
        .unwrap_err();
        assert!(!err.to_string().is_empty());
    }

    #[test]
    fn exit_code_for_doctor_volume_left_mounted_is_an_operational_fault() {
        let report = doctor::DriftReport {
            volumes_checked: 3,
            left_mounted: vec!["/.btrfs-nvme".into()],
            missing: vec![doctor::MissingSubvolume {
                volume: "/.btrfs-nvme".into(),
                source_labels: vec!["nvme".into()],
                name: "@new".into(),
            }],
            ..Default::default()
        };
        assert_eq!(
            exit_code_for_doctor(&doctor::DoctorOutcome::Ran(report)),
            3,
            "left mounted outranks drift, like an unexamined volume"
        );
    }

    #[test]
    fn exit_code_for_doctor_volume_failure_outranks_drift() {
        // Both at once. The fault wins: a check that could not examine every
        // volume cannot assert that the drift list it produced is complete, so
        // reporting only "drift found" would overstate what the run knows.
        let mut report = doctor::DriftReport {
            volumes_checked: 3,
            volumes_failed: vec![("/.btrfs-ssd".into(), "not mounted".into())],
            ..Default::default()
        };
        report.missing.push(doctor::MissingSubvolume {
            volume: "/.btrfs-hdd".into(),
            source_labels: vec!["hdd-projects".into()],
            name: "ClaudeCodeProjects/powershell-scripts".into(),
        });
        assert!(report.has_drift());
        let outcome = doctor::DoctorOutcome::Ran(report);
        assert_eq!(
            exit_code_for_doctor(&outcome),
            3,
            "an incomplete check must not report as a mere finding"
        );
    }

    #[test]
    fn doctor_unit_does_not_fail_systemd_on_a_drift_finding() {
        // The other half of the f6p fix, and the half that actually silences
        // sentinel's "last run failed" notification. Without this line systemd
        // marks the unit failed for exit 1, which is a finding, not a fault.
        let config = Config::default();
        let unit = setup::templates::render_systemd_doctor_service(&config);
        // Match the DIRECTIVE, not the string: the explanatory comment in this
        // unit also contains the words "SuccessExitStatus=1", so a bare
        // `contains()` passes on the documentation even when the directive is
        // gone — caught by sabotaging the template and watching this stay green.
        assert!(
            unit.lines().any(|l| l.trim() == "SuccessExitStatus=1"),
            "doctor unit must not treat exit 1 (drift found) as a unit failure:\n{unit}"
        );
    }

    #[test]
    fn exit_code_for_doctor_total_failure_is_two() {
        // Every configured volume failed — nothing was ever examined.
        let report = doctor::DriftReport {
            volumes_checked: 0,
            volumes_failed: vec![
                ("/.btrfs-hdd".into(), "not mounted".into()),
                ("/.btrfs-nvme".into(), "mount failed".into()),
            ],
            ..Default::default()
        };
        assert!(!report.ran());
        let outcome = doctor::DoctorOutcome::Ran(report);
        assert_eq!(exit_code_for_doctor(&outcome), 2);
    }

    /// A deferral's reason carries the lock holder's record, which may hold a
    /// backslash and quotes: the line must still be JSON, and give the reason
    /// back exactly.
    #[test]
    fn doctor_json_gives_back_any_holder_record() {
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("das-maintenance.lock");
        for record in [
            format!(
                r#"recovery-os VM session C:\vm "A" pid {}"#,
                std::process::id()
            ),
            r#"odd \ "record" \"#.to_string(),
        ] {
            std::fs::write(&lock, format!("{record}\n")).unwrap();
            let holder = buttered_dasd::maintenance::holder_of(&lock);
            assert!(holder.contains('\\') && holder.contains('"'), "{holder}");
            let reason = format!("DAS maintenance lock held by {holder} — skipping drift check");
            let line = doctor_json(&doctor::DoctorOutcome::Deferred {
                reason: reason.clone(),
            })
            .unwrap();
            let parsed: serde_json::Value =
                serde_json::from_str(&line).unwrap_or_else(|e| panic!("not JSON ({e}): {line}"));
            assert_eq!(parsed["status"], "deferred", "{line}");
            assert_eq!(parsed["reason"], reason.as_str(), "{line}");
            assert!(
                line.starts_with(r#"{"status":"deferred","reason":"#),
                "{line}"
            );
        }
    }

    /// The line for a run that happened is what it was before the deferral
    /// went through serde: same keys, same order.
    #[test]
    fn doctor_json_ran_line_is_unchanged() {
        let mut report = doctor::DriftReport {
            volumes_checked: 4,
            volumes_failed: vec![("/.btrfs-hdd".into(), "not mounted".into())],
            ..Default::default()
        };
        for name in ["a", "b"] {
            report.missing.push(doctor::MissingSubvolume {
                volume: "/.btrfs-nvme".into(),
                source_labels: vec!["nvme".into()],
                name: name.into(),
            });
        }
        assert_eq!(
            doctor_json(&doctor::DoctorOutcome::Ran(report)).unwrap(),
            r#"{"status":"ran","volumes_checked":4,"volumes_failed":1,"missing":2,"stale":0}"#
        );
    }

    #[test]
    fn exit_code_for_doctor_deferred_is_zero() {
        let outcome = doctor::DoctorOutcome::Deferred {
            reason: "maintenance lock held".into(),
        };
        assert_eq!(exit_code_for_doctor(&outcome), 0);
    }

    // ---- subvol expire: the index follows the deletes ----

    #[test]
    fn expire_failed_does_not_open_the_database_when_nothing_was_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("index.db");
        assert!(!expire_failed(false, &[], &db));
        assert!(!db.exists(), "the database must not be created");
        assert!(
            expire_failed(true, &[], &db),
            "an expiry failure stays a failure"
        );
    }

    #[test]
    fn expire_failed_is_true_when_deleted_snapshots_cannot_be_pruned_from_the_index() {
        let dir = tempfile::tempdir().unwrap();
        // A directory is not a database.
        let deleted = [PathBuf::from("/mnt/t/gone.20260101")];
        assert!(expire_failed(false, &deleted, dir.path()));
    }

    #[test]
    fn expire_prunes_exactly_the_deleted_snapshots_from_the_index() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("index.db");
        {
            let db = Database::open(&db_path).unwrap();
            db.insert_snapshot("gone", "20260101", "s", "/mnt/t/gone.20260101")
                .unwrap();
            db.insert_snapshot("kept", "20260101", "s", "/mnt/t/kept.20260101")
                .unwrap();
        }
        let deleted = [PathBuf::from("/mnt/t/gone.20260101")];
        assert!(!expire_failed(false, &deleted, &db_path));

        let left = Database::open(&db_path).unwrap().list_snapshots().unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].path, "/mnt/t/kept.20260101");
    }

    // --- walk and restore wait for the maintenance lock (bd DAS-Backup-Manager-frb)
    //
    // Each command runs against a scratch lock and a config with no targets,
    // so its mount step mounts nothing. `restore file` and `restore snapshot`
    // then stop at their source policy ("no backup targets are configured") —
    // before anything is copied or `btrfs` is run — which is how a test sees
    // that they got past the lock and the mount step.

    use buttered_dasd::maintenance::{LockSite, MaintenanceHeld};
    use std::sync::Arc;
    use std::sync::mpsc::{self, RecvTimeoutError};
    use std::time::Duration;

    /// Long enough to be sure a command is blocked, not merely slow to start.
    const STILL_WAITING: Duration = Duration::from_millis(300);
    /// Ample for a command looking every 10 ms to notice a release.
    const NOTICES: Duration = Duration::from_secs(1);

    /// Every log line a command wrote.
    #[derive(Default)]
    struct Logged(Mutex<Vec<String>>);

    impl ProgressCallback for Logged {
        fn on_stage(&self, _: &str, _: u64) {}
        fn on_progress(&self, _: u64, _: u64, _: &str) {}
        fn on_throughput(&self, _: u64) {}
        fn on_log(&self, _: LogLevel, message: &str) {
            self.0.lock().unwrap().push(message.to_string());
        }
        fn on_complete(&self, _: bool, _: &str) {}
    }

    impl Logged {
        fn waiting_lines(&self) -> Vec<String> {
            self.0
                .lock()
                .unwrap()
                .iter()
                .filter(|l| l.starts_with("Waiting for the DAS maintenance lock"))
                .cloned()
                .collect()
        }
    }

    /// A temp dir holding the lock file, a config with no targets that allows
    /// restores into the dir, a snapshot with one file, and the index.
    struct LockRig {
        dir: tempfile::TempDir,
    }

    impl LockRig {
        fn new() -> Arc<Self> {
            let dir = tempfile::tempdir().unwrap();
            let mut cfg = Config::default();
            cfg.restore.allowed_roots = vec![dir.path().to_string_lossy().into_owned()];
            cfg.save(&dir.path().join("config.toml")).unwrap();
            std::fs::create_dir(dir.path().join("snap")).unwrap();
            std::fs::write(dir.path().join("snap/hello.txt"), "hi").unwrap();
            Arc::new(Self { dir })
        }

        fn path(&self, name: &str) -> PathBuf {
            self.dir.path().join(name)
        }

        fn lock(&self, delegated: Option<&str>) -> CliLock {
            CliLock {
                site: LockSite {
                    path: self.path("das-maintenance.lock"),
                    poll: Duration::from_millis(10),
                },
                delegated: delegated.map(Into::into),
            }
        }

        /// Another job takes the lock and records itself.
        fn hold(&self) -> MaintenanceHeld {
            MaintenanceHeld::try_acquire_at(&self.path("das-maintenance.lock"), "test holder")
                .unwrap()
                .expect("the scratch lock is free")
        }
    }

    #[derive(Clone, Copy, Debug)]
    enum Cmd {
        Walk,
        RestoreFile,
        RestoreSnapshot,
        RestoreBrowse,
    }

    /// Run one command on the rig; errors as text.
    fn run_cmd(
        rig: &LockRig,
        cmd: Cmd,
        no_wait: bool,
        delegated: Option<&str>,
        logs: &Logged,
    ) -> Result<Ran, String> {
        let lock = rig.lock(delegated);
        let run = Interactive {
            json: false,
            no_wait,
            lock: &lock,
            progress: logs,
        };
        let config = rig.path("config.toml");
        let (snap, dest) = (rig.path("snap"), rig.path("restored"));
        match cmd {
            Cmd::Walk => cmd_walk(
                rig.dir.path(),
                &rig.path("index.db").to_string_lossy(),
                &config,
                &run,
            ),
            Cmd::RestoreFile => {
                cmd_restore_file(&snap, &dest, &["hello.txt".to_string()], &config, &run)
            }
            Cmd::RestoreSnapshot => cmd_restore_snapshot(&snap, &dest, &config, &run),
            Cmd::RestoreBrowse => cmd_restore_browse(&snap, None, &config, &run),
        }
        .map_err(|e| e.to_string())
    }

    /// [`run_cmd`] on a thread: what it returned within `limit`, or `None`
    /// while it is still running. A command that should not wait, but does,
    /// then fails its test instead of hanging it.
    fn run_cmd_within(
        rig: &Arc<LockRig>,
        cmd: Cmd,
        no_wait: bool,
        delegated: Option<String>,
        limit: Duration,
    ) -> (Option<Result<Ran, String>>, Arc<Logged>) {
        let logs = Arc::new(Logged::default());
        let (tx, rx) = mpsc::channel();
        let (cmd_rig, cmd_logs) = (rig.clone(), logs.clone());
        std::thread::spawn(move || {
            let _ = tx.send(run_cmd(
                &cmd_rig,
                cmd,
                no_wait,
                delegated.as_deref(),
                &cmd_logs,
            ));
        });
        (rx.recv_timeout(limit).ok(), logs)
    }

    /// Whether `out` is what the command does once it is past the lock.
    #[track_caller]
    fn assert_proceeded(cmd: Cmd, out: &Result<Ran, String>) {
        match cmd {
            Cmd::Walk | Cmd::RestoreBrowse => assert_eq!(out, &Ok(Ran::Done), "{cmd:?}"),
            Cmd::RestoreFile | Cmd::RestoreSnapshot => assert!(
                matches!(out, Err(e) if e.contains("no backup targets are configured")),
                "{cmd:?} must reach its source policy, got {out:?}"
            ),
        }
    }

    fn proceeds_at_once_when_the_lock_is_free(cmd: Cmd) {
        let rig = LockRig::new();
        let (out, logs) = run_cmd_within(&rig, cmd, false, None, STILL_WAITING);
        let out = out.unwrap_or_else(|| panic!("{cmd:?} waited for a free lock"));
        assert_proceeded(cmd, &out);
        assert!(logs.waiting_lines().is_empty(), "nothing to wait for");
        // It held the lock while it ran, and let it go when done.
        assert!(rig.hold().path().exists());
    }

    fn waits_while_the_lock_is_held_then_proceeds(cmd: Cmd) {
        let rig = LockRig::new();
        let holder = rig.hold();
        let logs = Arc::new(Logged::default());
        let (tx, rx) = mpsc::channel();
        let (cmd_rig, cmd_logs) = (rig.clone(), logs.clone());
        let worker = std::thread::spawn(move || {
            tx.send(run_cmd(&cmd_rig, cmd, false, None, &cmd_logs))
                .unwrap();
        });

        assert!(
            matches!(
                rx.recv_timeout(STILL_WAITING),
                Err(RecvTimeoutError::Timeout)
            ),
            "{cmd:?} must wait while another job holds the lock"
        );
        assert!(
            !rig.path("index.db").exists() && !rig.path("restored").exists(),
            "{cmd:?} did work before it had the lock"
        );
        let waiting = logs.waiting_lines();
        assert_eq!(waiting.len(), 1, "{cmd:?}: {waiting:?}");
        assert!(
            waiting[0].contains(&format!("held by test holder pid {}", std::process::id())),
            "{}",
            waiting[0]
        );

        drop(holder);
        let out = rx
            .recv_timeout(NOTICES)
            .unwrap_or_else(|_| panic!("{cmd:?} must proceed once the lock is released"));
        assert_proceeded(cmd, &out);
        worker.join().unwrap();
    }

    fn with_no_wait_defers_while_the_lock_is_held(cmd: Cmd) {
        let rig = LockRig::new();
        let holder = rig.hold();
        let (out, logs) = run_cmd_within(&rig, cmd, true, None, STILL_WAITING);
        assert_eq!(out, Some(Ok(Ran::Deferred)), "{cmd:?} must defer at once");
        assert!(
            logs.waiting_lines().is_empty(),
            "deferring announces no wait"
        );
        assert!(
            !rig.path("index.db").exists() && !rig.path("restored").exists(),
            "{cmd:?} did work although it deferred"
        );
        drop(holder);
    }

    #[test]
    fn walk_proceeds_at_once_when_the_lock_is_free() {
        proceeds_at_once_when_the_lock_is_free(Cmd::Walk);
    }

    #[test]
    fn walk_waits_while_the_lock_is_held_then_proceeds() {
        waits_while_the_lock_is_held_then_proceeds(Cmd::Walk);
    }

    #[test]
    fn walk_with_no_wait_defers_while_the_lock_is_held() {
        with_no_wait_defers_while_the_lock_is_held(Cmd::Walk);
    }

    #[test]
    fn restore_file_proceeds_at_once_when_the_lock_is_free() {
        proceeds_at_once_when_the_lock_is_free(Cmd::RestoreFile);
    }

    #[test]
    fn restore_file_waits_while_the_lock_is_held_then_proceeds() {
        waits_while_the_lock_is_held_then_proceeds(Cmd::RestoreFile);
    }

    #[test]
    fn restore_file_with_no_wait_defers_while_the_lock_is_held() {
        with_no_wait_defers_while_the_lock_is_held(Cmd::RestoreFile);
    }

    #[test]
    fn restore_snapshot_proceeds_at_once_when_the_lock_is_free() {
        proceeds_at_once_when_the_lock_is_free(Cmd::RestoreSnapshot);
    }

    #[test]
    fn restore_snapshot_waits_while_the_lock_is_held_then_proceeds() {
        waits_while_the_lock_is_held_then_proceeds(Cmd::RestoreSnapshot);
    }

    #[test]
    fn restore_snapshot_with_no_wait_defers_while_the_lock_is_held() {
        with_no_wait_defers_while_the_lock_is_held(Cmd::RestoreSnapshot);
    }

    #[test]
    fn restore_browse_proceeds_at_once_when_the_lock_is_free() {
        proceeds_at_once_when_the_lock_is_free(Cmd::RestoreBrowse);
    }

    #[test]
    fn restore_browse_waits_while_the_lock_is_held_then_proceeds() {
        waits_while_the_lock_is_held_then_proceeds(Cmd::RestoreBrowse);
    }

    #[test]
    fn restore_browse_with_no_wait_defers_while_the_lock_is_held() {
        with_no_wait_defers_while_the_lock_is_held(Cmd::RestoreBrowse);
    }

    // --- what walk and restore print ------------------------------------------

    fn walked(removed: usize) -> (indexer::WalkResult, rusqlite::Result<reconcile::PruneStats>) {
        let result = indexer::WalkResult {
            snapshots_discovered: 5,
            snapshots_indexed: 2,
            snapshots_skipped: 3,
            presence_recorded: 3,
            results: vec![indexer::IndexResult {
                snapshot_id: 1,
                files_total: 10,
                files_new: 4,
                files_extended: 3,
                files_changed: 2,
                scan_errors: 1,
            }],
        };
        let stats = reconcile::PruneStats {
            snapshots_removed: removed,
            spans_repaired: 6,
            spans_removed: 7,
            files_removed: 8,
        };
        (result, Ok(stats))
    }

    #[test]
    fn walk_prints_its_counts_the_reconcile_and_each_snapshot() {
        let counts = "Discovered: 5 snapshots\nIndexed:    2 new\nSkipped:    3 already indexed\n";
        let files = "  10 files (4 new, 3 extended, 2 changed, 1 errors)\n";
        let (result, nothing_pruned) = walked(0);
        assert_eq!(
            walk_report(false, &result, &nothing_pruned),
            (
                format!("{counts}Reconciled: index already consistent\n{files}"),
                None
            )
        );
        let (result, one_pruned) = walked(1);
        assert_eq!(
            walk_report(false, &result, &one_pruned),
            (
                format!(
                    "{counts}Reconciled: 1 stale snapshots pruned (6 spans repaired, 7 removed, \
                     8 files dropped)\n{files}"
                ),
                None
            )
        );
    }

    #[test]
    fn walk_warns_when_the_reconcile_failed_and_prints_json_alone() {
        let (result, _) = walked(0);
        let failed: rusqlite::Result<reconcile::PruneStats> =
            Err(rusqlite::Error::QueryReturnedNoRows);
        let (out, warning) = walk_report(false, &result, &failed);
        assert!(!out.contains("Reconciled"), "{out}");
        assert_eq!(
            warning.as_deref(),
            Some("Warning: reconcile failed: Query returned no rows")
        );
        assert_eq!(
            walk_report(true, &result, &failed),
            (
                "{\"discovered\":5,\"indexed\":2,\"skipped\":3}\n".to_string(),
                None
            )
        );
    }

    #[test]
    fn restore_prints_a_summary_and_each_error() {
        let result = restore::RestoreResult {
            files_restored: 2,
            bytes_restored: 2048,
            errors: vec!["a: gone".into(), "b: denied".into()],
            duration_secs: 3,
        };
        assert_eq!(
            restored_report(false, &result),
            (
                format!("Restored 2 files ({}) in 3s\n", report::format_bytes(2048)),
                vec![
                    "  ERROR: a: gone".to_string(),
                    "  ERROR: b: denied".to_string()
                ]
            )
        );
        assert_eq!(
            restored_report(true, &result),
            (
                "{\"files_restored\":2,\"bytes_restored\":2048,\"errors\":2,\"duration_secs\":3}\n"
                    .to_string(),
                Vec::<String>::new()
            )
        );
    }

    #[test]
    fn browse_lists_the_entries_as_text_and_as_json() {
        let entry = |name: &str, size: u64, mtime: i64, is_dir: bool| restore::BrowseEntry {
            path: name.to_string(),
            name: name.to_string(),
            size,
            mtime,
            is_dir,
        };
        let entries = vec![
            entry("etc", 0, 1, true),
            entry("a\"b", 5, 2, false),
            entry("c", 7, 3, false),
        ];
        assert_eq!(
            browse_listing(false, &entries),
            format!(
                "d {:>12} etc\n- {:>12} a\"b\n- {:>12} c\n(3 entries)\n",
                "-",
                report::format_bytes(5),
                report::format_bytes(7)
            )
        );
        assert_eq!(
            browse_listing(true, &entries),
            r#"[{"path":"etc","name":"etc","size":0,"mtime":1,"is_dir":true},{"path":"a\"b","name":"a\"b","size":5,"mtime":2,"is_dir":false},{"path":"c","name":"c","size":7,"mtime":3,"is_dir":false}]"#
                .to_string()
                + "\n"
        );
        assert_eq!(browse_listing(true, &[]), "[]\n");
        assert_eq!(browse_listing(false, &[]), "(0 entries)\n");
    }

    #[test]
    fn forget_and_purge_fail_on_a_config_they_cannot_read() {
        let out = run_deletion(
            "/nonexistent-frb/index.db",
            Path::new("/nonexistent-frb/config.toml"),
            false,
            true,
            "forget",
            |_| Err(forget::ForgetRefusal::NoMatch),
        );
        assert!(out.is_err(), "nothing can be done without the config");
    }

    #[test]
    fn a_deferral_exits_75_and_a_finished_run_does_not() {
        assert_eq!(deferred_exit_code(&Ran::Deferred), Some(75));
        assert_eq!(deferred_exit_code(&Ran::Done), None);
    }

    /// `backup-run.sh` runs `walk` while it holds the lock on fd 8 and names
    /// that descriptor; waiting there would wait for the caller forever.
    #[test]
    fn a_hold_handed_down_by_the_caller_is_used_instead_of_waiting() {
        use std::os::fd::AsRawFd;
        let rig = LockRig::new();
        let parent = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(rig.path("das-maintenance.lock"))
            .unwrap();
        // SAFETY: flock on a descriptor `parent` owns.
        let rc = unsafe { libc::flock(parent.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        assert_eq!(rc, 0, "the caller holds the lock");

        let fd = parent.as_raw_fd().to_string();
        let (out, logs) = run_cmd_within(&rig, Cmd::Walk, false, Some(fd), STILL_WAITING);
        assert_eq!(
            out,
            Some(Ok(Ran::Done)),
            "the handed-down hold is used at once"
        );
        assert!(logs.waiting_lines().is_empty());
        // The caller's hold survives the command.
        assert!(
            MaintenanceHeld::try_acquire_at(&rig.path("das-maintenance.lock"), "x")
                .unwrap()
                .is_none()
        );
    }

    /// The caller holds the lock (and recorded itself) but runs the command
    /// without handing the hold down: waiting would wait for the caller, which
    /// waits for the command. It fails at once instead, with or without
    /// `--no-wait` — this test process's real parent stands in for the caller.
    #[test]
    fn a_command_whose_own_caller_holds_the_lock_fails_instead_of_waiting() {
        let rig = LockRig::new();
        let caller = std::os::unix::process::parent_id();
        let lock = scrub::FileLock::try_acquire(rig.path("das-maintenance.lock"))
            .unwrap()
            .expect("the scratch lock is free");
        std::fs::write(
            rig.path("das-maintenance.lock"),
            format!("backup-run.sh pid {caller}\n"),
        )
        .unwrap();
        for no_wait in [false, true] {
            let (out, logs) = run_cmd_within(&rig, Cmd::Walk, no_wait, None, STILL_WAITING);
            let err = out
                .expect("fails at once, never waits for its caller")
                .expect_err("a lock its own caller holds must fail the command");
            assert!(
                err.contains(&format!(
                    "held by my own caller (pid {caller}, backup-run.sh pid {caller})"
                )),
                "{err}"
            );
            assert!(err.contains("DAS_MAINTENANCE_LOCK_FD"), "{err}");
            assert!(logs.waiting_lines().is_empty(), "no wait was announced");
            assert!(!rig.path("index.db").exists(), "nothing ran");
        }
        drop(lock);
    }

    #[test]
    fn a_hold_handed_down_that_does_not_hold_the_lock_fails_instead_of_waiting() {
        use std::os::fd::AsRawFd;
        let rig = LockRig::new();
        let holder = rig.hold();
        let bystander = std::fs::File::open(rig.path("das-maintenance.lock")).unwrap();
        let fd = bystander.as_raw_fd().to_string();
        for named in [fd.as_str(), "eight"] {
            let (out, _) =
                run_cmd_within(&rig, Cmd::Walk, false, Some(named.into()), STILL_WAITING);
            let err = out
                .expect("a hold that cannot be proven fails at once, never waits")
                .expect_err("a hold that cannot be proven must fail");
            assert!(err.contains("DAS_MAINTENANCE_LOCK_FD"), "{err}");
            assert!(!rig.path("index.db").exists(), "nothing ran");
        }
        drop(holder);
    }
}
