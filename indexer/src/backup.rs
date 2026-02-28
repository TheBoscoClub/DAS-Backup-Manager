use crate::config::Config;
use crate::progress::ProgressCallback;

/// Whether to run an incremental or full backup.
#[derive(Debug, Clone, PartialEq)]
pub enum BackupMode {
    Incremental,
    Full,
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
    pub snapshots_created: usize,
    pub snapshots_sent: usize,
    pub bytes_sent: u64,
    pub boot_archived: bool,
    pub indexed: bool,
    pub report_sent: bool,
    pub errors: Vec<String>,
    pub duration_secs: u64,
}

/// Run a backup with the given options. Calls btrbk under the hood.
/// The caller must ensure this runs with appropriate privileges (root).
pub fn run_backup(
    _config: &Config,
    _options: &BackupOptions,
    _progress: &dyn ProgressCallback,
) -> Result<BackupResult, Box<dyn std::error::Error>> {
    todo!("Full implementation in Phase 2")
}

/// Create btrbk snapshots for specified sources.
pub fn create_snapshots(
    _config: &Config,
    _sources: &[String],
    _progress: &dyn ProgressCallback,
) -> Result<usize, Box<dyn std::error::Error>> {
    todo!("Full implementation in Phase 2")
}

/// Send snapshots to specified targets via btrbk.
/// Returns (snapshots_sent, bytes_sent).
pub fn send_snapshots(
    _config: &Config,
    _sources: &[String],
    _targets: &[String],
    _progress: &dyn ProgressCallback,
) -> Result<(usize, u64), Box<dyn std::error::Error>> {
    todo!("Full implementation in Phase 2")
}

/// Archive boot subvolumes as read-only snapshots on backup targets.
pub fn archive_boot(
    _config: &Config,
    _progress: &dyn ProgressCallback,
) -> Result<bool, Box<dyn std::error::Error>> {
    todo!("Full implementation in Phase 2")
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
