mod setup;

use buttered_dasd::backup::{BackupMode, BackupOptions};
use buttered_dasd::config::Config;
use buttered_dasd::db::Database;
use buttered_dasd::health::HealthStatus;
use buttered_dasd::indexer;
use buttered_dasd::mount;
use buttered_dasd::progress::{LogLevel, ProgressCallback};
use buttered_dasd::report;
use buttered_dasd::{restore, schedule, scrub, subvol};
use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::{Shell, generate};
use std::path::PathBuf;

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

#[derive(Subcommand)]
enum Commands {
    /// Index all new snapshots on a backup target
    Walk {
        /// Path to backup target mount point
        target: PathBuf,
        /// Path to SQLite database
        #[arg(long, default_value = DEFAULT_DB)]
        db: String,
        /// Path to config.toml (for auto-mounting targets)
        #[arg(long, default_value = DEFAULT_CONFIG)]
        config: PathBuf,
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
    /// Scheduled BTRFS scrub of the DAS backup filesystems
    Scrub {
        #[command(subcommand)]
        action: ScrubAction,
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
        /// Number of snapshots created
        #[arg(long, default_value = "0")]
        snaps_created: usize,
        /// Number of snapshots sent to targets
        #[arg(long, default_value = "0")]
        snaps_sent: usize,
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

/// Find and cancel the filesystem currently being scrubbed, if any.
///
/// Manual operator action only — never invoked automatically. The scrub
/// lock (`/run/das-scrub.lock`) tells us *whether* a pass is running but not
/// *which* filesystem; once contention on that lock confirms a pass is live,
/// each configured target's mount point is probed with
/// `scrub::live_scrub_state`. That call is safe against an unmounted path
/// here for the same reason the engine's own doc comment gives: only a
/// filesystem the engine itself mounted for scrubbing can ever report
/// `Running`, so an idle/unmounted target simply resolves to `Unknown` or
/// `NotRunning` and is skipped.
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

    for label in &config.scrub.targets {
        let Ok(fsuuid) = scrub::resolve_target_fsuuid(config, label) else {
            continue;
        };
        let Some(target) = config.targets.iter().find(|t| t.label == *label) else {
            continue;
        };
        if scrub::live_scrub_state(&target.mount) == scrub::LiveScrubState::Running {
            let out = std::process::Command::new("btrfs")
                .args(["scrub", "cancel", &target.mount])
                .output()
                .map_err(|e| format!("cannot execute 'btrfs scrub cancel': {e}"))?;
            return if out.status.success() {
                Ok(format!(
                    "Canceled scrub of '{label}' (uuid={fsuuid}) at {}",
                    target.mount
                ))
            } else {
                let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
                Err(format!(
                    "btrfs scrub cancel {} failed: {stderr}",
                    target.mount
                ))
            };
        }
    }

    Ok(
        "A scrub pass is running (lock held) but no configured target's mount currently \
        shows an active scrub — it may be between filesystems (mounting/unmounting); \
        try again in a few seconds."
            .to_string(),
    )
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    let json = cli.json;

    match cli.command {
        // ----- Indexer commands (unchanged) -----
        Commands::Walk { target, db, config } => {
            let cfg = Config::load(&config)?;
            let progress = CliProgress;
            let mut guard = mount::ensure_targets_mounted(&cfg, &progress)?;
            let database = Database::open(&db)?;
            let result = indexer::walk(&target, &database);
            guard.unmount(&progress);
            let result = result?;
            if json {
                println!(
                    "{{\"discovered\":{},\"indexed\":{},\"skipped\":{}}}",
                    result.snapshots_discovered, result.snapshots_indexed, result.snapshots_skipped
                );
            } else {
                println!("Discovered: {} snapshots", result.snapshots_discovered);
                println!("Indexed:    {} new", result.snapshots_indexed);
                println!("Skipped:    {} already indexed", result.snapshots_skipped);
                for r in &result.results {
                    println!(
                        "  {} files ({} new, {} extended, {} changed, {} errors)",
                        r.files_total,
                        r.files_new,
                        r.files_extended,
                        r.files_changed,
                        r.scan_errors
                    );
                }
            }
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
                let options = BackupOptions {
                    mode: if full {
                        Some(BackupMode::Full)
                    } else {
                        Some(BackupMode::Incremental)
                    },
                    sources,
                    targets,
                    dry_run,
                    boot_archive: true,
                    index_after: true,
                    send_report: true,
                    ..Default::default()
                };
                let progress = CliProgress;
                let mut source_guard = mount::ensure_sources_mounted(&cfg, &progress);
                let mut guard = mount::ensure_targets_mounted(&cfg, &progress)?;
                let result = buttered_dasd::backup::run_backup(&cfg, &options, &progress);
                guard.unmount(&progress);
                source_guard.unmount(&progress);
                let result = result?;

                // Record the backup run in the database (skip dry runs)
                if !options.dry_run {
                    match Database::open(&cfg.general.db_path) {
                        Ok(db) => {
                            if let Err(e) = report::record_backup_run(&db, &result) {
                                eprintln!("  [WARN]  Failed to record backup history: {e}");
                            }
                        }
                        Err(e) => {
                            eprintln!("  [WARN]  Failed to open DB for history: {e}");
                        }
                    }
                }

                if json {
                    println!(
                        "{{\"success\":{},\"snapshots_created\":{},\"snapshots_sent\":{},\"bytes_sent\":{},\"duration_secs\":{}}}",
                        result.success,
                        result.snapshots_created,
                        result.snapshots_sent,
                        result.bytes_sent,
                        result.duration_secs
                    );
                } else {
                    println!(
                        "Backup {}: {} snapshots created, {} sent, {} in {}s",
                        if result.success {
                            "succeeded"
                        } else {
                            "FAILED"
                        },
                        result.snapshots_created,
                        result.snapshots_sent,
                        report::format_bytes(result.bytes_sent),
                        result.duration_secs
                    );
                    for e in &result.errors {
                        eprintln!("  ERROR: {e}");
                    }
                }
                if !result.success {
                    std::process::exit(1);
                }
            }
            BackupAction::Snapshot { config, sources } => {
                let cfg = Config::load(&config)?;
                let progress = CliProgress;
                let mut source_guard = mount::ensure_sources_mounted(&cfg, &progress);
                let count = buttered_dasd::backup::create_snapshots(&cfg, &sources, &progress)?;
                source_guard.unmount(&progress);
                println!("Created {count} snapshots");
            }
            BackupAction::Send { config, targets } => {
                let cfg = Config::load(&config)?;
                let progress = CliProgress;
                let mut source_guard = mount::ensure_sources_mounted(&cfg, &progress);
                let mut guard = mount::ensure_targets_mounted(&cfg, &progress)?;
                let result =
                    buttered_dasd::backup::send_snapshots(&cfg, &[], &targets, false, &progress);
                guard.unmount(&progress);
                source_guard.unmount(&progress);
                let (sent, bytes) = result?;
                println!("Sent {sent} snapshots ({})", report::format_bytes(bytes));
            }
            BackupAction::BootArchive { config } => {
                let cfg = Config::load(&config)?;
                let progress = CliProgress;
                let mut guard = mount::ensure_targets_mounted(&cfg, &progress)?;
                let result = buttered_dasd::backup::archive_boot(&cfg, &progress);
                guard.unmount(&progress);
                let archived = result?;
                if archived {
                    println!("Boot subvolumes archived successfully");
                } else {
                    println!(
                        "No boot subvolumes to archive (boot archival disabled or no targets mounted)"
                    );
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
                            run.snaps_created,
                            run.snaps_sent,
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
                            run.snaps_created,
                            run.snaps_sent
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
        Commands::Restore { action } => match action {
            RestoreAction::File {
                snapshot,
                dest,
                files,
                config,
            } => {
                let cfg = Config::load(&config)?;
                let progress = CliProgress;
                let mut guard = mount::ensure_targets_mounted(&cfg, &progress)?;
                let file_refs: Vec<&str> = files.iter().map(|s| s.as_str()).collect();
                let result = restore::restore_files(&snapshot, &file_refs, &dest, &progress);
                guard.unmount(&progress);
                let result = result?;
                if json {
                    println!(
                        "{{\"files_restored\":{},\"bytes_restored\":{},\"errors\":{},\"duration_secs\":{}}}",
                        result.files_restored,
                        result.bytes_restored,
                        result.errors.len(),
                        result.duration_secs
                    );
                } else {
                    println!(
                        "Restored {} files ({}) in {}s",
                        result.files_restored,
                        report::format_bytes(result.bytes_restored),
                        result.duration_secs
                    );
                    for e in &result.errors {
                        eprintln!("  ERROR: {e}");
                    }
                }
            }
            RestoreAction::Snapshot {
                snapshot,
                dest,
                config,
            } => {
                let cfg = Config::load(&config)?;
                let progress = CliProgress;
                let mut guard = mount::ensure_targets_mounted(&cfg, &progress)?;
                let result = restore::restore_snapshot(&snapshot, &dest, &progress);
                guard.unmount(&progress);
                let result = result?;
                if json {
                    println!(
                        "{{\"files_restored\":{},\"bytes_restored\":{},\"errors\":{},\"duration_secs\":{}}}",
                        result.files_restored,
                        result.bytes_restored,
                        result.errors.len(),
                        result.duration_secs
                    );
                } else {
                    println!(
                        "Restored {} files ({}) in {}s",
                        result.files_restored,
                        report::format_bytes(result.bytes_restored),
                        result.duration_secs
                    );
                    for e in &result.errors {
                        eprintln!("  ERROR: {e}");
                    }
                }
            }
            RestoreAction::Browse {
                snapshot,
                prefix,
                config,
            } => {
                let cfg = Config::load(&config)?;
                let progress = CliProgress;
                let mut guard = mount::ensure_targets_mounted(&cfg, &progress)?;
                let entries = restore::browse_snapshot(&snapshot, prefix.as_deref());
                guard.unmount(&progress);
                let entries = entries?;
                if json {
                    print!("[");
                    for (i, e) in entries.iter().enumerate() {
                        if i > 0 {
                            print!(",");
                        }
                        print!(
                            "{{\"path\":\"{}\",\"name\":\"{}\",\"size\":{},\"mtime\":{},\"is_dir\":{}}}",
                            e.path.replace('"', "\\\""),
                            e.name.replace('"', "\\\""),
                            e.size,
                            e.mtime,
                            e.is_dir
                        );
                    }
                    println!("]");
                } else {
                    for e in &entries {
                        let type_char = if e.is_dir { "d" } else { "-" };
                        println!(
                            "{} {:>12} {}",
                            type_char,
                            if e.is_dir {
                                "-".to_string()
                            } else {
                                report::format_bytes(e.size)
                            },
                            e.name
                        );
                    }
                    println!("({} entries)", entries.len());
                }
            }
        },

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
                let toml = cfg.to_toml()?;
                std::fs::write(&config, toml)?;
                println!("Added subvolume '{name}' to source '{source}'.");
            }
            SubvolAction::Remove {
                source,
                name,
                config,
            } => {
                let mut cfg = Config::load(&config)?;
                subvol::remove_subvolume(&mut cfg, &source, &name)?;
                let toml = cfg.to_toml()?;
                std::fs::write(&config, toml)?;
                println!("Removed subvolume '{name}' from source '{source}'.");
            }
            SubvolAction::SetManual {
                source,
                name,
                config,
            } => {
                let mut cfg = Config::load(&config)?;
                subvol::set_manual(&mut cfg, &source, &name, true)?;
                let toml = cfg.to_toml()?;
                std::fs::write(&config, toml)?;
                println!("Subvolume '{name}' in source '{source}' set to manual-only.");
            }
            SubvolAction::SetAuto {
                source,
                name,
                config,
            } => {
                let mut cfg = Config::load(&config)?;
                subvol::set_manual(&mut cfg, &source, &name, false)?;
                let toml = cfg.to_toml()?;
                std::fs::write(&config, toml)?;
                println!("Subvolume '{name}' in source '{source}' set to automatic.");
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
                    print!(
                        "{{\"label\":\"{}\",\"serial\":\"{}\",\"mounted\":{},\"total_bytes\":{},\"used_bytes\":{},\"snapshot_count\":{},\"smart_status\":{}}}",
                        t.label,
                        t.serial,
                        t.mounted,
                        t.total_bytes,
                        t.used_bytes,
                        t.snapshot_count,
                        t.smart_status
                            .as_ref()
                            .map_or("null".to_string(), |s| format!("\"{s}\""))
                    );
                }
                print!("],\"warnings\":[");
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
                    "{:<16} {:<12} {:>10} {:>10} {:>6} {:<10}",
                    "Target", "Serial", "Used", "Total", "Use%", "SMART"
                );
                println!("{}", "-".repeat(70));
                for t in &report.targets {
                    if !t.mounted {
                        println!(
                            "{:<16} {:<12} {:>10} {:>10} {:>6} {:<10}",
                            t.label, t.serial, "-", "-", "-", "not mounted"
                        );
                        continue;
                    }
                    println!(
                        "{:<16} {:<12} {:>10} {:>10} {:>5.1}% {:<10}",
                        t.label,
                        t.serial,
                        buttered_dasd::report::format_bytes(t.used_bytes),
                        buttered_dasd::report::format_bytes(t.total_bytes),
                        t.usage_percent(),
                        t.smart_status.as_deref().unwrap_or("N/A")
                    );
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
                }
                if !pass.success() {
                    std::process::exit(1);
                }
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
                    e.contains("scrub must run as root")
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
}
