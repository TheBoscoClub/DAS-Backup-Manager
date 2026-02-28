use crate::backup::BackupResult;
use crate::config::Config;
use crate::db::{Database, NewBackupRun};

/// A historical backup run record (stored in DB).
#[derive(Debug, Clone)]
pub struct BackupRun {
    pub id: i64,
    pub timestamp: i64,
    pub success: bool,
    pub mode: String,
    pub snapshots_created: usize,
    pub snapshots_sent: usize,
    pub bytes_sent: u64,
    pub duration_secs: u64,
    pub errors: Vec<String>,
}

/// Generate a human-readable backup report string.
pub fn format_report(result: &BackupResult, _config: &Config) -> String {
    let status = if result.success { "SUCCESS" } else { "FAILURE" };
    let mut report = format!(
        "Backup Report: {status}\n\
         Duration: {}s\n\
         Snapshots created: {}\n\
         Snapshots sent: {}\n\
         Bytes sent: {}\n",
        result.duration_secs,
        result.snapshots_created,
        result.snapshots_sent,
        format_bytes(result.bytes_sent),
    );

    if result.boot_archived {
        report.push_str("Boot archived: yes\n");
    }
    if result.indexed {
        report.push_str("Indexed: yes\n");
    }

    if !result.errors.is_empty() {
        report.push_str("\nErrors:\n");
        for e in &result.errors {
            report.push_str(&format!("  - {e}\n"));
        }
    }

    report
}

/// Format bytes into human-readable form (KiB, MiB, GiB, TiB).
pub fn format_bytes(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * KIB;
    const GIB: u64 = 1024 * MIB;
    const TIB: u64 = 1024 * GIB;

    if bytes >= TIB {
        format!("{:.2} TiB", bytes as f64 / TIB as f64)
    } else if bytes >= GIB {
        format!("{:.2} GiB", bytes as f64 / GIB as f64)
    } else if bytes >= MIB {
        format!("{:.2} MiB", bytes as f64 / MIB as f64)
    } else if bytes >= KIB {
        format!("{:.2} KiB", bytes as f64 / KIB as f64)
    } else {
        format!("{bytes} B")
    }
}

/// Record a completed backup run in the database.
pub fn record_backup_run(
    db: &Database,
    result: &BackupResult,
) -> Result<i64, Box<dyn std::error::Error>> {
    let mode = if result.snapshots_created == 0 && result.snapshots_sent == 0 {
        "none"
    } else {
        "incremental"
    };
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs() as i64;
    let id = db.insert_backup_run(&NewBackupRun {
        timestamp,
        success: result.success,
        mode,
        snaps_created: result.snapshots_created,
        snaps_sent: result.snapshots_sent,
        bytes_sent: result.bytes_sent,
        duration_secs: result.duration_secs,
        errors: &result.errors,
    })?;
    Ok(id)
}

/// Get the last N backup runs from the database.
pub fn get_backup_history(
    db: &Database,
    limit: usize,
) -> Result<Vec<BackupRun>, Box<dyn std::error::Error>> {
    let records = db.get_backup_history(limit)?;
    let runs = records
        .into_iter()
        .map(|r| BackupRun {
            id: r.id,
            timestamp: r.timestamp,
            success: r.success,
            mode: r.mode,
            snapshots_created: r.snaps_created,
            snapshots_sent: r.snaps_sent,
            bytes_sent: r.bytes_sent,
            duration_secs: r.duration_secs,
            errors: r.errors,
        })
        .collect();
    Ok(runs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_bytes_units() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(1024), "1.00 KiB");
        assert_eq!(format_bytes(1_048_576), "1.00 MiB");
        assert_eq!(format_bytes(1_073_741_824), "1.00 GiB");
        assert_eq!(format_bytes(1_099_511_627_776), "1.00 TiB");
    }

    #[test]
    fn format_bytes_fractional() {
        assert_eq!(format_bytes(1_610_612_736), "1.50 GiB");
        assert_eq!(format_bytes(2_684_354_560), "2.50 GiB");
    }

    #[test]
    fn format_report_success() {
        let result = BackupResult {
            success: true,
            snapshots_created: 5,
            snapshots_sent: 5,
            bytes_sent: 1_073_741_824,
            boot_archived: true,
            indexed: true,
            report_sent: false,
            errors: vec![],
            duration_secs: 3600,
        };
        let cfg = Config::default();
        let report = format_report(&result, &cfg);
        assert!(report.contains("SUCCESS"));
        assert!(report.contains("3600s"));
        assert!(report.contains("1.00 GiB"));
        assert!(report.contains("Boot archived: yes"));
        assert!(report.contains("Indexed: yes"));
        assert!(!report.contains("Errors"));
    }

    #[test]
    fn record_and_retrieve_backup_run() {
        let db = Database::open(":memory:").unwrap();
        let result = BackupResult {
            success: true,
            snapshots_created: 3,
            snapshots_sent: 3,
            bytes_sent: 500_000,
            boot_archived: false,
            indexed: true,
            report_sent: false,
            errors: vec![],
            duration_secs: 120,
        };
        let id = record_backup_run(&db, &result).unwrap();
        assert!(id > 0);

        let history = get_backup_history(&db, 10).unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].id, id);
        assert!(history[0].success);
        assert_eq!(history[0].snapshots_created, 3);
        assert_eq!(history[0].snapshots_sent, 3);
        assert_eq!(history[0].bytes_sent, 500_000);
        assert_eq!(history[0].duration_secs, 120);
        assert!(history[0].errors.is_empty());
    }

    #[test]
    fn format_report_with_errors() {
        let result = BackupResult {
            success: false,
            snapshots_created: 2,
            snapshots_sent: 0,
            bytes_sent: 0,
            boot_archived: false,
            indexed: false,
            report_sent: false,
            errors: vec!["btrbk failed".into(), "target not mounted".into()],
            duration_secs: 60,
        };
        let cfg = Config::default();
        let report = format_report(&result, &cfg);
        assert!(report.contains("FAILURE"));
        assert!(report.contains("Errors:"));
        assert!(report.contains("btrbk failed"));
        assert!(report.contains("target not mounted"));
    }
}
