mod setup;

use buttered_dasd::backup::{BackupMode, BackupOptions};
use buttered_dasd::config::Config;
use buttered_dasd::db::Database;
use buttered_dasd::health::HealthStatus;
use buttered_dasd::indexer;
use buttered_dasd::progress::{LogLevel, ProgressCallback};
use buttered_dasd::report;
use buttered_dasd::{restore, schedule, subvol};
use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::{generate, Shell};
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
    },
    /// Restore an entire snapshot (btrfs send/receive or recursive copy)
    Snapshot {
        /// Path to the snapshot directory
        snapshot: PathBuf,
        /// Destination directory
        dest: PathBuf,
    },
    /// Browse files in a snapshot directory
    Browse {
        /// Path to the snapshot directory
        snapshot: PathBuf,
        /// Optional subdirectory prefix to browse
        #[arg(long)]
        prefix: Option<String>,
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

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    let json = cli.json;

    match cli.command {
        // ----- Indexer commands (unchanged) -----
        Commands::Walk { target, db } => {
            let database = Database::open(&db)?;
            let result = indexer::walk(&target, &database)?;
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
                let result = buttered_dasd::backup::run_backup(&cfg, &options, &progress)?;
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
                let count = buttered_dasd::backup::create_snapshots(&cfg, &sources, &progress)?;
                println!("Created {count} snapshots");
            }
            BackupAction::Send { config, targets } => {
                let cfg = Config::load(&config)?;
                let progress = CliProgress;
                let (sent, bytes) =
                    buttered_dasd::backup::send_snapshots(&cfg, &[], &targets, &progress)?;
                println!("Sent {sent} snapshots ({})", report::format_bytes(bytes));
            }
            BackupAction::BootArchive { config } => {
                let cfg = Config::load(&config)?;
                let progress = CliProgress;
                let archived = buttered_dasd::backup::archive_boot(&cfg, &progress)?;
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
                            "{{\"id\":{},\"timestamp\":\"{}\",\"mode\":\"{}\",\"success\":{},\"duration_secs\":{},\"snapshots_created\":{},\"snapshots_sent\":{},\"bytes_sent\":{}}}",
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
        },

        // ----- Restore commands -----
        Commands::Restore { action } => match action {
            RestoreAction::File {
                snapshot,
                dest,
                files,
            } => {
                let progress = CliProgress;
                let file_refs: Vec<&str> = files.iter().map(|s| s.as_str()).collect();
                let result = restore::restore_files(&snapshot, &file_refs, &dest, &progress)?;
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
            RestoreAction::Snapshot { snapshot, dest } => {
                let progress = CliProgress;
                let result = restore::restore_snapshot(&snapshot, &dest, &progress)?;
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
            RestoreAction::Browse { snapshot, prefix } => {
                let entries = restore::browse_snapshot(&snapshot, prefix.as_deref())?;
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

        // ----- Completions command -----
        Commands::Completions { shell } => {
            let mut cmd = Cli::command();
            generate(shell, &mut cmd, "btrdasd", &mut std::io::stdout());
        }
    }

    Ok(())
}
