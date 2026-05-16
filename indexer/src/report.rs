use crate::backup::BackupResult;
use crate::config::Config;
use crate::db::{Database, NewBackupRun};

use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Stdio};

/// Canonical Protonmail Bridge credentials file. Root services hardcode the
/// user's home path because Bridge is a per-user concept and root has no $HOME
/// of its own. Override via the `PBRIDGE_CONF` env var for testing.
const PBRIDGE_CONF_DEFAULT: &str = "/home/bosco/.config/pbridge.conf";

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

/// Generate a comprehensive backup report matching the original shell script format.
///
/// Sections: Header, Backup Operations, Throughput, Disk Capacity, SMART Status,
/// Latest Snapshots, Footer.  The Growth Analysis section requires historical data
/// from the growth log — if available, it is included.
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

/// Percent-encode a string for use in the userinfo section of a URL (RFC 3986).
/// Only unreserved characters (A-Z, a-z, 0-9, `-`, `.`, `_`, `~`) pass through;
/// everything else is encoded as %XX.
fn percent_encode_userinfo(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char);
            }
            _ => {
                out.push_str(&format!("%{b:02X}"));
            }
        }
    }
    out
}

/// SMTP credentials parsed from the `SMTP:` block of `~/.config/pbridge.conf`.
#[derive(Debug, Clone)]
struct PbridgeSmtp {
    host: String,
    port: String,
    username: String,
    password: String,
}

/// Parse the `SMTP:` block of `~/.config/pbridge.conf` (the single canonical
/// source for Protonmail Bridge credentials per `~/.claude/rules/infrastructure.md`).
///
/// The file consists of top-level `IMAP:` and `SMTP:` blocks, each followed by
/// indented `Key: Value` lines (Hostname, Port, Username, Password, Security).
/// This parser tracks the current block by line prefix: a line starting with
/// `SMTP:` enters the block; any other top-level `<Word>:` line exits it.
fn parse_pbridge_smtp_block(
    path: &str,
) -> Result<PbridgeSmtp, Box<dyn std::error::Error>> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read {path}: {e}"))?;

    let mut host = String::new();
    let mut port = String::new();
    let mut username = String::new();
    let mut password = String::new();
    let mut in_smtp_block = false;

    for raw in content.lines() {
        // Detect block transitions on top-level (unindented) `<Word>:` lines.
        if !raw.starts_with(|c: char| c.is_whitespace()) {
            let trimmed = raw.trim_end();
            if trimmed == "SMTP:" {
                in_smtp_block = true;
                continue;
            }
            // Any other unindented "Word:" line exits the SMTP block.
            if trimmed.ends_with(':')
                && trimmed[..trimmed.len() - 1]
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric())
            {
                in_smtp_block = false;
                continue;
            }
        }
        if !in_smtp_block {
            continue;
        }
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some((key, val)) = trimmed.split_once(':') {
            let key = key.trim();
            let val = val.trim();
            if val.is_empty() {
                continue;
            }
            match key {
                "Hostname" => host = val.to_string(),
                "Port" => port = val.to_string(),
                "Username" => username = val.to_string(),
                "Password" => password = val.to_string(),
                _ => {}
            }
        }
    }

    if host.is_empty() || port.is_empty() || username.is_empty() || password.is_empty() {
        return Err(format!(
            "incomplete SMTP credentials in {path} (need Hostname, Port, Username, Password)"
        )
        .into());
    }

    Ok(PbridgeSmtp {
        host,
        port,
        username,
        password,
    })
}

/// Send a backup report via email using s-nail (mailx).
///
/// Reads SMTP credentials from `~/.config/pbridge.conf` (the single canonical
/// source for Protonmail Bridge credentials). The recipient and sender default
/// to the Bridge account holder (the `Username` field); override via
/// `DAS_REPORT_TO` / `DAS_REPORT_FROM` env vars if a different address is needed.
pub fn send_email_report(report: &str, config: &Config) -> Result<(), Box<dyn std::error::Error>> {
    if !config.email.enabled {
        return Err("Email is not enabled in config".into());
    }

    let pbridge_conf = std::env::var("PBRIDGE_CONF")
        .unwrap_or_else(|_| PBRIDGE_CONF_DEFAULT.to_string());

    // Warn (do not fatal) if the credentials file is world/group-readable. The
    // bash side issues the same warning — pbridge.conf contains a Bridge auth
    // token and should be 0600.
    if let Ok(meta) = std::fs::metadata(&pbridge_conf) {
        let mode = meta.permissions().mode() & 0o777;
        if mode != 0o600 {
            eprintln!(
                "warning: {pbridge_conf} has permissions {mode:o} — should be 600 \
                 (contains Bridge auth token)"
            );
        }
    }

    let smtp = parse_pbridge_smtp_block(&pbridge_conf)?;

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

    // Recipient defaults to the Bridge account holder. Sender uses the Bridge
    // account address (Bridge enforces SMTP MAIL FROM to match the authenticated
    // user) but renders a friendly display name "DAS Backup (<host>)" so the
    // email is visually distinct from ordinary self-to-self mail in the inbox.
    // Override either via DAS_REPORT_TO/FROM env vars.
    let to = std::env::var("DAS_REPORT_TO").unwrap_or_else(|_| smtp.username.clone());
    let from = std::env::var("DAS_REPORT_FROM").unwrap_or_else(|_| {
        format!("DAS Backup ({short_hostname}) <{}>", smtp.username)
    });

    if to.is_empty() {
        return Err("No email recipient configured (DAS_REPORT_TO or pbridge.conf Username)".into());
    }

    let smtp_url = format!("smtp://{}:{}", smtp.host, smtp.port);
    let smtp_user = smtp.username.clone();
    let smtp_pass = smtp.password.clone();
    // Bridge listens on loopback with a self-signed certificate — mirror the
    // bash side's `ssl-verify=ignore` rather than failing the cert check.
    let ssl_verify = "ignore";
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
    let subject = format!("[DAS Backup] {hostname} — {status_word} — {now}");

    // Build the mta URL with credentials embedded (s-nail v14.9+ / v15-compat).
    // Percent-encode all non-unreserved characters per RFC 3986 userinfo rules.
    let encoded_user = percent_encode_userinfo(&smtp_user);
    let encoded_pass = percent_encode_userinfo(&smtp_pass);
    let mta_url = smtp_url.replacen("://", &format!("://{encoded_user}:{encoded_pass}@"), 1);

    // s-nail v14.9+ renamed ssl-verify → tls-verify.
    let tls_verify_key = "tls-verify";

    // Send via mailx (s-nail) using v15-compat mode for new-style URL credentials.
    let mut child = Command::new("mailx")
        .args([
            "-s",
            &subject,
            "-r",
            &from,
            "-S",
            "v15-compat",
            "-S",
            &format!("mta={mta_url}"),
            "-S",
            "smtp-auth=login",
            "-S",
            &format!("{tls_verify_key}={ssl_verify}"),
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
