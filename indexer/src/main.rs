use buttered_dasd::db::Database;
use buttered_dasd::indexer;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

const DEFAULT_DB: &str = "/var/lib/das-backup/backup-index.db";

#[derive(Parser)]
#[command(
    name = "btrdasd",
    about = "ButteredDASD — content indexer for DAS backup snapshots"
)]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
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
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Walk { target, db } => {
            let database = Database::open(&db)?;
            let result = indexer::walk(&target, &database)?;
            println!("Discovered: {} snapshots", result.snapshots_discovered);
            println!("Indexed:    {} new", result.snapshots_indexed);
            println!("Skipped:    {} already indexed", result.snapshots_skipped);
            for r in &result.results {
                println!(
                    "  {} files ({} new, {} extended, {} changed, {} errors)",
                    r.files_total, r.files_new, r.files_extended, r.files_changed, r.scan_errors
                );
            }
        }
        Commands::Search { query, db, limit } => {
            let database = Database::open(&db)?;
            let results = database.search(&query, limit)?;
            if results.is_empty() {
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
            for f in &files {
                println!("{}", f.path);
            }
            println!("({} files)", files.len());
        }
        Commands::Info { db } => {
            let database = Database::open(&db)?;
            let stats = database.get_stats()?;
            println!("Snapshots:  {}", stats.snapshot_count);
            println!("Files:      {}", stats.file_count);
            println!("Spans:      {}", stats.span_count);
            println!("DB size:    {} bytes", stats.db_size);
        }
    }

    Ok(())
}
