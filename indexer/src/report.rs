use crate::backup::BackupResult;
use crate::config::Config;
use crate::db::{Database, NewBackupRun};

use std::process::{Command, Stdio};

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

/// Generate a backup report for the Rust (manual `btrdasd backup run`) path.
///
/// Sections: Header, Backup Operations, Throughput, Disk Capacity, SMART Status,
/// Latest Snapshots, Footer. The Growth Analysis section requires historical data
/// from the growth log — if available, it is included.
///
/// # This deliberately does NOT match `scripts/backup-run.sh` row-for-row
///
/// The comment here used to claim parity with the shell script's format
/// (`bd DAS-Backup-Manager-39w`). It never had it, and chasing it would be worse
/// than the drift: the shell path's "Archive cleanup" row reports
/// `boot-archive-cleanup.sh`, which this path does not run at all — it calls
/// `backup::archive_boot` and nothing prunes afterward — and its "Unmount
/// targets" row reports the script's own `unmount_all`, whereas here unmounting
/// is `MountGuard`'s business and its failures surface through the guard.
///
/// Emitting those rows here would report on work that did not happen. The two
/// reports describe two different pipelines and are allowed to differ; what is
/// not allowed is a comment claiming otherwise.
///
/// Nothing parses either report's text — verified by grep across the tree: no
/// consumer reads `LAST_REPORT` or matches these section strings — so the
/// divergence is a documentation question, not a compatibility one.
pub fn format_report(result: &BackupResult, config: &Config) -> String {
    let sep = "═".repeat(63);
    let thin = "─".repeat(63);

    // Timestamp + hostname via libc (no extra deps).
    let (timestamp, hostname) = {
        let mut t: libc::time_t = 0;
        unsafe { libc::time(&mut t) };
        let mut tm: libc::tm = unsafe { std::mem::zeroed() };
        unsafe { libc::localtime_r(&t, &mut tm) };
        let ts = format!(
            "{:04}-{:02}-{:02} {:02}:{:02}",
            tm.tm_year + 1900,
            tm.tm_mon + 1,
            tm.tm_mday,
            tm.tm_hour,
            tm.tm_min
        );
        let mut buf = [0u8; 256];
        let rc = unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) };
        let hn = if rc == 0 {
            let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
            String::from_utf8_lossy(&buf[..len]).to_string()
        } else {
            "unknown".to_string()
        };
        (ts, hn)
    };

    let overall = if result.success {
        "ALL OPERATIONS SUCCESSFUL"
    } else {
        "FAILURES DETECTED"
    };

    let elapsed_min = result.duration_secs / 60;
    let elapsed_sec = result.duration_secs % 60;

    let mut r = String::with_capacity(4096);

    // Header
    r.push_str(&format!(
        "{sep}\n  DAS Backup Report — {timestamp}\n  Host: {hostname}\n  Status: {overall}\n{sep}\n\n"
    ));

    // Backup Operations
    let btrbk_status = if result.errors.iter().any(|e| e.contains("btrbk")) {
        "FAIL"
    } else {
        "OK"
    };
    let boot_status = if result.boot_archived { "OK" } else { "N/A" };
    let index_status = if result.indexed { "OK" } else { "N/A" };

    r.push_str(&format!("BACKUP OPERATIONS\n{thin}\n"));
    r.push_str(&format!(
        "  btrbk send/receive    {btrbk_status}  ({elapsed_min}m {elapsed_sec}s)\n"
    ));
    r.push_str(&format!("  Boot subvolumes       {boot_status}\n"));
    r.push_str(&format!("  Content indexer       {index_status}\n"));

    // Throughput — simple summary from result data.
    r.push_str(&format!("\nTHROUGHPUT\n{thin}\n"));
    if result.bytes_sent > 0 && result.duration_secs > 0 {
        let rate = result.bytes_sent as f64 / result.duration_secs as f64;
        r.push_str(&format!(
            "  Total                    {} @ {}/s\n",
            format_bytes(result.bytes_sent),
            format_bytes(rate as u64),
        ));
    } else {
        r.push_str("  (no data transferred)\n");
    }

    // Disk Capacity — query live health data.
    r.push_str(&format!("\nDISK CAPACITY\n{thin}\n"));
    r.push_str("  Target                   Used       Avail      Use%\n");
    if let Ok(health) = crate::health::get_health(config) {
        for th in &health.targets {
            if !th.mounted {
                continue;
            }
            let avail = th.total_bytes.saturating_sub(th.used_bytes);
            r.push_str(&format!(
                "  {:<25}{:<11}{:<11}{:.0}%\n",
                th.label,
                format_bytes(th.used_bytes),
                format_bytes(avail),
                th.usage_percent(),
            ));
        }

        // SMART Status
        r.push_str(&format!("\nSMART STATUS\n{thin}\n"));
        for th in &health.targets {
            let smart = th.smart_status.as_deref().unwrap_or("N/A");
            let temp = th
                .temperature_c
                .map(|t| format!("{t}°C"))
                .unwrap_or_else(|| "N/A".to_string());
            let hours = th
                .power_on_hours
                .map(|h| format!("{h}h"))
                .unwrap_or_else(|| "N/A".to_string());
            r.push_str(&format!(
                "  {:<25}{:<11}{:<8}{:<8}{}\n",
                th.label, th.serial, smart, temp, hours
            ));
        }
    } else {
        r.push_str("  (health data unavailable)\n");
    }

    // Latest Snapshots — run btrbk list latest.
    r.push_str(&format!("\nLATEST SNAPSHOTS\n{thin}\n"));
    if let Ok(output) = Command::new("btrbk")
        .args(["-c", &config.general.btrbk_conf, "list", "latest"])
        .output()
        .and_then(|o| {
            if o.status.success() {
                Ok(o)
            } else {
                Err(std::io::Error::other("btrbk failed"))
            }
        })
    {
        let stdout = String::from_utf8_lossy(&output.stdout);
        for (i, line) in stdout.lines().enumerate() {
            if i == 0 {
                continue; // skip header
            }
            r.push_str(&format!("  {line}\n"));
        }
    }

    // Errors
    if !result.errors.is_empty() {
        r.push_str(&format!("\nERRORS\n{thin}\n"));
        for e in &result.errors {
            r.push_str(&format!("  - {e}\n"));
        }
    }

    // Footer
    let version = env!("CARGO_PKG_VERSION");
    // Try to get next scheduled time from systemd.
    let next_scheduled = Command::new("systemctl")
        .args([
            "show",
            "das-backup.timer",
            "--property=NextElapseUSecRealtime",
        ])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.split('=').nth(1).map(|v| v.trim().to_string()))
        .unwrap_or_else(|| "unknown".to_string());

    r.push_str(&format!(
        "\n{sep}\n  btrdasd v{version}\n  Next scheduled: {next_scheduled}\n{sep}\n"
    ));

    r
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
    let mode_str = result.mode.to_string();
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs() as i64;
    let id = db.insert_backup_run(&NewBackupRun {
        timestamp,
        success: result.success,
        mode: &mode_str,
        snaps_created: result.snapshots_created,
        snaps_sent: result.snapshots_sent,
        bytes_sent: result.bytes_sent,
        duration_secs: result.duration_secs,
        errors: &result.errors,
    })?;
    Ok(id)
}

/// Send a backup report via email using s-nail (mailx).
///
/// Thin wrapper over [`send_email_report_with_kind`] with the `Backup` subject kind.
pub fn send_email_report(report: &str, config: &Config) -> Result<(), Box<dyn std::error::Error>> {
    send_email_report_with_kind(report, config, "Backup")
}

/// Send a report via email using s-nail (mailx).
///
/// Submits **unauthenticated plaintext** to the local mail relay named by
/// `[email].smtp_host`/`smtp_port` (`127.0.0.1:25`). This process holds no mail
/// credential of any kind: the relay authenticates upstream on its own, keyed by
/// the envelope sender, using a key readable only by root under `/etc`. Before
/// 2026-08-06 this function parsed Protonmail Bridge's credentials file and
/// passed the password to a child process — visible in `/proc/<pid>/cmdline` to
/// any local process for the duration of the send.
///
/// Sender and recipient come from `[email].from`/`to`; `DAS_REPORT_FROM` /
/// `DAS_REPORT_TO` override them for testing without editing the live config.
///
/// `kind` names the report in the subject line (`[DAS <kind>] …`) so backup and
/// scrub reports are distinguishable in the inbox; the status word is derived
/// from the body containing `FAILURE`.
pub fn send_email_report_with_kind(
    report: &str,
    config: &Config,
    kind: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    if !config.email.enabled {
        return Err("Email is not enabled in config".into());
    }

    // Hostname is needed both for the subject line and the From display name —
    // compute it once up front. libc::gethostname returns whatever the kernel has
    // set; split on '.' to get the short form for the From display name even when
    // the kernel hostname is FQDN-like.
    let hostname = {
        let mut buf = [0u8; 256];
        let rc = unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) };
        if rc == 0 {
            let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
            String::from_utf8_lossy(&buf[..len]).to_string()
        } else {
            "unknown".to_string()
        }
    };
    let short_hostname = hostname.split('.').next().unwrap_or(&hostname).to_string();

    // Sender and recipient come from config; env vars override for testing.
    // The configured `from` is wrapped in a display name "DAS <kind> (<host>)"
    // so reports stand out in the inbox From column — s-nail extracts the
    // bracketed address for the SMTP envelope, which is what the relay keys its
    // upstream credential lookup on.
    let to = std::env::var("DAS_REPORT_TO").unwrap_or_else(|_| config.email.to.clone());
    let from_addr = std::env::var("DAS_REPORT_FROM").unwrap_or_else(|_| config.email.from.clone());

    if to.is_empty() {
        return Err("No email recipient configured ([email].to or DAS_REPORT_TO)".into());
    }
    if from_addr.is_empty() {
        return Err("No email sender configured ([email].from or DAS_REPORT_FROM)".into());
    }

    // A DAS_REPORT_FROM that already carries its own display name is used
    // verbatim; a bare address gets the standard one.
    let from = if from_addr.contains('<') {
        from_addr
    } else {
        format!("DAS {kind} ({short_hostname}) <{from_addr}>")
    };

    let smtp_url = format!(
        "smtp://{}:{}",
        config.email.smtp_host, config.email.smtp_port
    );
    let status_word = if report.contains("FAILURE") {
        "FAILURE"
    } else {
        "SUCCESS"
    };
    // Format current local time as YYYY-MM-DD HH:MM using libc.
    let now = {
        let mut t: libc::time_t = 0;
        unsafe { libc::time(&mut t) };
        let mut tm: libc::tm = unsafe { std::mem::zeroed() };
        unsafe { libc::localtime_r(&t, &mut tm) };
        format!(
            "{:04}-{:02}-{:02} {:02}:{:02}",
            tm.tm_year + 1900,
            tm.tm_mon + 1,
            tm.tm_mday,
            tm.tm_hour,
            tm.tm_min
        )
    };
    let subject = format!("[DAS {kind}] {hostname} — {status_word} — {now}");

    // Submit to the relay. No credential and no TLS setting: the hop is
    // loopback-only plaintext and the relay owns the authenticated,
    // certificate-verified leg to the provider. `v15-compat` stays because it
    // is what makes s-nail honour the `mta=` URL form.
    //
    // `smtp-auth=none` is NOT optional and NOT symmetry with the shell path:
    // s-nail demands a password for any `smtp://` mta and aborts with
    // "A password is necessary for smtp authentication" (exit 4) without it.
    // `nosave` stops a failed send from dropping the body into /root/dead.letter,
    // which nothing reads or prunes — the caller has the report either way.
    let mut child = Command::new("mailx")
        .args([
            "-s",
            &subject,
            "-r",
            &from,
            "-S",
            "v15-compat",
            "-S",
            &format!("mta={smtp_url}"),
            "-S",
            "smtp-auth=none",
            "-S",
            "nosave",
            &to,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()?;

    // Write report body to stdin.
    if let Some(mut stdin) = child.stdin.take() {
        use std::io::Write;
        stdin.write_all(report.as_bytes())?;
    }

    let output = child.wait_with_output()?;
    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(format!("mailx failed (exit {}): {}", output.status, stderr.trim()).into())
    }
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
    use crate::backup::BackupMode;

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
            mode: BackupMode::Full,
            snapshots_created: 5,
            snapshots_sent: 5,
            snapshots_cleaned: 2,
            bytes_sent: 1_073_741_824,
            boot_archived: true,
            indexed: true,
            report_sent: false,
            errors: vec![],
            duration_secs: 3600,
        };
        let cfg = Config::default();
        let report = format_report(&result, &cfg);
        assert!(report.contains("ALL OPERATIONS SUCCESSFUL"));
        assert!(report.contains("60m 0s"));
        assert!(report.contains("1.00 GiB"));
        assert!(report.contains("BACKUP OPERATIONS"));
        assert!(report.contains("THROUGHPUT"));
        assert!(report.contains("DISK CAPACITY"));
        assert!(report.contains("SMART STATUS"));
        assert!(!report.contains("ERRORS"));
    }

    #[test]
    fn record_and_retrieve_backup_run() {
        let db = Database::open(":memory:").unwrap();
        let result = BackupResult {
            success: true,
            mode: BackupMode::Incremental,
            snapshots_created: 3,
            snapshots_sent: 3,
            snapshots_cleaned: 0,
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
        assert_eq!(history[0].mode, "incremental");
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
            mode: BackupMode::Full,
            snapshots_created: 2,
            snapshots_sent: 0,
            snapshots_cleaned: 0,
            bytes_sent: 0,
            boot_archived: false,
            indexed: false,
            report_sent: false,
            errors: vec!["btrbk failed".into(), "target not mounted".into()],
            duration_secs: 60,
        };
        let cfg = Config::default();
        let report = format_report(&result, &cfg);
        assert!(report.contains("FAILURES DETECTED"));
        assert!(report.contains("ERRORS"));
        assert!(report.contains("btrbk failed"));
        assert!(report.contains("target not mounted"));
    }
}
