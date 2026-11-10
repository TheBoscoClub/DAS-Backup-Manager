use crate::backup::{BackupResult, SyncSection};
use crate::config::Config;
use crate::db::{BackupRunRecord, Database, NewBackupRun};

use std::process::{Command, Stdio};

/// A historical backup run record (stored in DB).
#[derive(Debug, Clone)]
pub struct BackupRun {
    pub id: i64,
    pub timestamp: i64,
    pub success: bool,
    pub mode: String,
    /// `None`: the run could not count them.
    pub snapshots_created: Option<u64>,
    /// `None`: the run could not count them.
    pub snapshots_sent: Option<u64>,
    /// `None`: the run could not measure it.
    pub bytes_sent: Option<u64>,
    pub duration_secs: u64,
    pub errors: Vec<String>,
}

/// A run count as the history shows it: the number, or `unknown` when the run
/// could not count — never 0, which would read as nothing done
/// (bd DAS-Backup-Manager-6wt).
pub fn format_count(count: Option<u64>) -> String {
    count.map_or_else(|| "unknown".to_string(), |n| n.to_string())
}

/// A byte count as a report shows it: [`format_bytes`], or `unknown` when the
/// run could not measure it — never 0 B (bd DAS-Backup-Manager-3lpr).
pub fn format_size(bytes: Option<u64>) -> String {
    bytes.map_or_else(|| "unknown".to_string(), format_bytes)
}

/// The run history as the GUI reads it from the D-Bus helper
/// (`IndexBackupHistory`): one object per run, in the order given. A count or
/// size the run could not take is `null`, never 0 or -1; the GUI shows it as
/// "unknown".
pub fn backup_history_json(runs: &[BackupRunRecord]) -> serde_json::Value {
    runs.iter()
        .map(|r| {
            serde_json::json!({
                "id": r.id,
                "timestamp": r.timestamp,
                "mode": r.mode,
                "success": r.success,
                "duration_secs": r.duration_secs,
                "snaps_created": r.snaps_created,
                "snaps_sent": r.snaps_sent,
                "bytes_sent": r.bytes_sent,
                "errors": r.errors,
            })
        })
        .collect()
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
    format_report_with_sync(result, config, None)
}

/// `format_report` for a run that began with a subvolume sync: its status
/// joins the operations list and its section follows it, where
/// `backup-run.sh` puts them.
pub fn format_report_with_sync(
    result: &BackupResult,
    config: &Config,
    sync: Option<&SyncSection>,
) -> String {
    format_report_from(result, sync, &capture_report_data(config, &[]))
}

/// The report sections that read the mounted targets — disk capacity, SMART,
/// `btrbk list latest` — rendered while the targets are still mounted. A
/// backup job captures this BEFORE it unmounts and builds the report after,
/// so an unmount failure is in the report and the capacity table is not
/// empty — what `capture_report_data` + `unmount_all` do in `backup-run.sh`
/// (bd DAS-Backup-Manager-ecg, -h4t).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportData {
    /// `DISK CAPACITY` (and `SMART STATUS`) sections.
    pub capacity_and_smart: String,
    /// `LATEST SNAPSHOTS` section.
    pub latest_snapshots: String,
}

/// Capture [`ReportData`] from the live system. Call while mounted. Only
/// the two queries live here; what they become is decided by
/// [`render_capacity_and_smart`] and [`render_latest_snapshots`].
pub fn capture_report_data(config: &Config, filters: &[String]) -> ReportData {
    let health = crate::health::get_health(config).ok();
    let latest = latest_listing(
        Command::new("btrbk")
            .args(latest_listing_args(config, filters))
            .output(),
    );
    ReportData {
        capacity_and_smart: render_capacity_and_smart(health.as_ref()),
        latest_snapshots: render_latest_snapshots(latest.as_deref().map_err(String::as_str)),
    }
}

/// The arguments of the `btrbk list latest` the report reads. `filters` are the
/// run's own (`backup::btrbk_filters`): btrbk reads only the targets they name,
/// so an unselected, unmounted target does not make the listing fail
/// (bd DAS-Backup-Manager-s5jv.6); none: every target, as before.
fn latest_listing_args(config: &Config, filters: &[String]) -> Vec<String> {
    let mut args = vec![
        "-c".to_string(),
        config.general.btrbk_conf.clone(),
        "list".to_string(),
        "latest".to_string(),
    ];
    args.extend(filters.iter().cloned());
    args
}

/// What `btrbk list latest` answered: its output, or why there is none.
fn latest_listing(output: std::io::Result<std::process::Output>) -> Result<String, String> {
    match output {
        Ok(o) if o.status.success() => Ok(String::from_utf8_lossy(&o.stdout).into_owned()),
        Ok(o) => Err(format!(
            "btrbk list latest exited with {}: {}",
            o.status,
            String::from_utf8_lossy(&o.stderr)
                .lines()
                .next()
                .unwrap_or("")
        )),
        Err(e) => Err(format!("btrbk could not be run: {e}")),
    }
}

/// `DISK CAPACITY` (mounted targets only) and `SMART STATUS` (every target),
/// or a note that health data was unavailable.
pub fn render_capacity_and_smart(health: Option<&crate::health::HealthReport>) -> String {
    let thin = "─".repeat(63);
    let mut r = String::new();
    r.push_str(&format!("\nDISK CAPACITY\n{thin}\n"));
    r.push_str("  Target                   Used       Avail      Use%\n");
    r.push_str("  (raw device space as df reports it; a RAID-1 target holds half as much data)\n");
    let Some(health) = health else {
        r.push_str("  (health data unavailable)\n");
        return r;
    };
    for th in health.targets.iter().filter(|t| t.mounted) {
        // A usage that could not be measured is "unknown", never 0 B.
        if !th.usage_known {
            r.push_str(&format!(
                "  {:<25}{:<11}{:<11}{}\n",
                th.label, "unknown", "unknown", "unknown"
            ));
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
    r.push_str(&format!("\nSMART STATUS\n{thin}\n"));
    for th in &health.targets {
        // One row per configured drive, an absent one saying ABSENT: a RAID-1
        // target used to show only its first drive (bd DAS-Backup-Manager-uhi).
        // A health report built without members keeps the one-row form.
        if th.members.is_empty() {
            smart_row(
                &mut r,
                &th.label,
                &th.serial,
                th.smart_status.as_deref(),
                th.temperature_c,
                th.power_on_hours,
            );
            continue;
        }
        for m in &th.members {
            if m.present {
                smart_row(
                    &mut r,
                    &th.label,
                    &m.serial,
                    m.smart_status.as_deref(),
                    m.temperature_c,
                    m.power_on_hours,
                );
            } else {
                r.push_str(&format!("  {:<25}{:<11}ABSENT\n", th.label, m.serial));
            }
        }
    }
    r
}

fn smart_row(
    r: &mut String,
    label: &str,
    serial: &str,
    smart: Option<&str>,
    temperature_c: Option<i32>,
    power_on_hours: Option<u64>,
) {
    let temp = temperature_c
        .map(|t| format!("{t}°C"))
        .unwrap_or_else(|| "N/A".to_string());
    let hours = power_on_hours
        .map(|h| format!("{h}h"))
        .unwrap_or_else(|| "N/A".to_string());
    r.push_str(&format!(
        "  {:<25}{:<11}{:<8}{:<8}{}\n",
        label,
        serial,
        smart.unwrap_or("N/A"),
        temp,
        hours
    ));
}

/// `LATEST SNAPSHOTS` from `btrbk list latest` output, its header line
/// dropped; when btrbk could not answer, one line saying so and why — an
/// empty section would read as "no snapshots".
pub fn render_latest_snapshots(btrbk_list_latest: Result<&str, &str>) -> String {
    let thin = "─".repeat(63);
    let mut r = format!("\nLATEST SNAPSHOTS\n{thin}\n");
    match btrbk_list_latest {
        Ok(out) => {
            for line in out.lines().skip(1) {
                r.push_str(&format!("  {line}\n"));
            }
        }
        Err(why) => r.push_str(&format!("  (unavailable: {why})\n")),
    }
    r
}

/// The report for `result`, with the mount-dependent sections from `data`.
pub fn format_report_from(
    result: &BackupResult,
    sync: Option<&SyncSection>,
    data: &ReportData,
) -> String {
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

    let overall = if !result.success {
        "FAILURES DETECTED"
    } else if result.boot.has_warnings()
        || result.recovery_os.has_warnings()
        || result.usb_link.has_warnings()
        || sync.is_some_and(|s| crate::coverage::has_warnings_or_missing(s.coverage.as_ref()))
    {
        "COMPLETED WITH WARNINGS"
    } else {
        "ALL OPERATIONS SUCCESSFUL"
    };

    let elapsed_min = result.duration_secs / 60;
    let elapsed_sec = result.duration_secs % 60;

    let mut r = String::with_capacity(4096);

    // Header
    r.push_str(&format!(
        "{sep}\n  DAS Backup Report — {timestamp}\n  Host: {hostname}\n  Status: {overall}\n{sep}\n\n"
    ));

    // Backup Operations
    // A boot-step failure is the boot row's, even when its text names btrbk
    // (an unreadable btrbk.conf): it is not a send/receive failure.
    let btrbk_status = if result
        .errors
        .iter()
        .filter(|e| {
            !e.starts_with(crate::backup::BOOT_ERROR_PREFIX)
                && !e.starts_with(crate::backup::RECOVERY_OS_ERROR_PREFIX)
                && !e.starts_with(crate::run_steps::USB_LINK_ERROR_PREFIX)
                && !e.starts_with(crate::run_steps::EXPIRY_ERROR_PREFIX)
        })
        .any(|e| e.contains("btrbk") || crate::backup::is_btrbk_step_failure(e))
    {
        "FAIL"
    } else {
        "OK"
    };
    let index_status = if result.indexed { "OK" } else { "N/A" };

    r.push_str(&format!("BACKUP OPERATIONS\n{thin}\n"));
    // First, where backup-run.sh's `Maintenance lock` row (which this path has
    // no counterpart for) is followed by it.
    r.push_str(&format!(
        "  USB link              {}\n",
        result.usb_link.row()
    ));
    r.push_str(&format!(
        "  btrbk send/receive    {btrbk_status}  ({elapsed_min}m {elapsed_sec}s)\n"
    ));
    r.push_str(&format!("  Boot subvolumes       {}\n", result.boot.row()));
    r.push_str(&format!(
        "  Retired expiry        {}\n",
        result.expiry.row()
    ));
    r.push_str(&format!(
        "  Recovery OS           {}\n",
        result.recovery_os.row()
    ));
    r.push_str(&format!("  Content indexer       {index_status}\n"));
    if let Some(sync) = sync {
        let status = if sync.failed { "FAIL" } else { "OK" };
        r.push_str(&format!("  Subvolume sync        {status}\n"));
        r.push_str(&format!(
            "  Subvolume coverage    {}\n",
            crate::coverage::row_or_not_run(sync.coverage.as_ref())
        ));
        r.push('\n');
        r.push_str(&sync.report);
    }
    // What the expiry of retired subvolumes did, after the sync's section.
    if let crate::run_steps::ExpiryStep::Ran { section, .. } = &result.expiry
        && !section.is_empty()
    {
        r.push('\n');
        r.push_str(section);
    }
    if let crate::backup::BootStep::Ran(o) = &result.boot
        && (!o.warnings.is_empty() || !o.failures.is_empty())
    {
        r.push_str(&format!("\nBOOT SUBVOLUMES\n{thin}\n"));
        for f in &o.failures {
            r.push_str(&format!("  FAIL  {f}\n"));
        }
        for w in &o.warnings {
            r.push_str(&format!("  WARN  {w}\n"));
        }
    }
    // The section the recovery-OS step printed, where backup-run.sh puts it:
    // after the operations (bd DAS-Backup-Manager-58c8).
    if let crate::backup::RecoveryOsStep::Ran(rec) = &result.recovery_os {
        r.push('\n');
        r.push_str(&rec.section);
    }

    // Throughput — simple summary from result data.
    r.push_str(&format!("\nTHROUGHPUT\n{thin}\n"));
    if let Some(bytes) = result.bytes_sent.filter(|&b| b > 0) {
        // Whole seconds: a send under one has no rate to give, but it sent.
        if result.duration_secs > 0 {
            let rate = bytes as f64 / result.duration_secs as f64;
            r.push_str(&format!(
                "  Total                    {} @ {}/s\n",
                format_bytes(bytes),
                format_bytes(rate as u64),
            ));
        } else {
            r.push_str(&format!(
                "  Total                    {} in under a second\n",
                format_bytes(bytes)
            ));
        }
    } else if result.bytes_sent.is_none() {
        // A target read before the steps and not after (bd DAS-Backup-Manager-3lpr).
        r.push_str("  (not measured — a target could not be read after the steps)\n");
    } else if result.snapshots_created.is_none() || result.snapshots_sent.is_none() {
        // A step failed before its counts were taken (bd DAS-Backup-Manager-no4):
        // nothing is known about what was sent, so none of it is said.
        r.push_str("  (not measured — a step failed, so what was sent is unknown)\n");
    } else {
        r.push_str("  (no data transferred)\n");
    }

    // Capacity, SMART and latest snapshots, as captured while the targets
    // were still mounted.
    r.push_str(&data.capacity_and_smart);
    r.push_str(&data.latest_snapshots);

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
        snaps_created: result.snapshots_created.map(|n| n as u64),
        snaps_sent: result.snapshots_sent.map(|n| n as u64),
        bytes_sent: result.bytes_sent,
        duration_secs: result.duration_secs,
        errors: &result.errors,
    })?;
    Ok(id)
}

/// One report as the relay is to receive it: everything `mailx` is told.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MailMessage {
    /// `[DAS <kind>] <host> — <SUCCESS|SUCCESS WITH WARNINGS|FAILURE> — <YYYY-MM-DD HH:MM>`.
    pub subject: String,
    /// `DAS <kind> (<short host>) <address>`, or a `DAS_REPORT_FROM` that
    /// carries its own display name. s-nail takes the bracketed address as
    /// the envelope sender, which is what the relay keys its upstream
    /// credential on.
    pub from: String,
    /// The recipient: `[email].to`, or `DAS_REPORT_TO`.
    pub to: String,
    /// The relay, `smtp://<smtp_host>:<smtp_port>`.
    pub mta: String,
    /// The report itself.
    pub body: String,
}

/// Every report send — backup, scrub, doctor — goes through this
/// (bd DAS-Backup-Manager-1x76). Production sends through [`MAILX`]; tests
/// hand the send a recording mailer and assert on the exact message.
pub trait Mailer: Send + Sync + std::any::Any {
    /// Hand `message` to the relay. The error is why it was not handed
    /// over, as the caller logs it.
    fn send(&self, message: &MailMessage) -> Result<(), String>;
    /// What the message takes from where it is sent: the host's name, the
    /// minute, the `DAS_REPORT_TO` / `DAS_REPORT_FROM` overrides. [`Mailx`]
    /// reads this machine and its environment ([`MailContext::current`]); a
    /// test's mailer answers a fixed one, so no test depends on the
    /// environment it runs in.
    fn context(&self) -> MailContext;
}

/// s-nail's `mailx`, submitting to the local relay: the production [`Mailer`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mailx {
    program: &'static str,
}

/// The mailer every production entry point hands down: `mailx` from `PATH`.
pub const MAILX: Mailx = Mailx { program: "mailx" };

/// The bound on one send, `backup-run.sh`'s `MAIL_TIMEOUT_SECS` and
/// `MAIL_KILL_AFTER_SECS`: TERM after 60 s, KILL 10 s later.
const MAIL_TIMEOUT_SECS: u64 = 60;
const MAIL_KILL_AFTER_SECS: u64 = 10;

/// True in every test build: the `test-overrides` feature, which only this
/// crate's dev-dependency on itself turns on, and which every target `cargo
/// test` builds therefore has — this crate's unit tests (`cargo test --lib`
/// included), integration tests, the binaries' unit tests and the binaries
/// they run. There, [`Mailx`] refuses before it spawns anything, so no test,
/// and no mutant a test build runs, can hand a report to a real relay (bd
/// DAS-Backup-Manager-1x76: on 2026-10-06 a local mutation run did, four
/// times). A shipped build never has the feature, and `tests/test_report_mail.sh`
/// proves the built `btrdasd` mails.
const SPAWN_REFUSED: bool = cfg!(feature = "test-overrides");

impl Mailx {
    /// A `mailx` at `program`. Production uses [`MAILX`]; the test that
    /// proves a test build never spawns one points this at a recording stub.
    pub const fn new(program: &'static str) -> Self {
        Self { program }
    }

    /// The program this mailer runs.
    pub fn program(&self) -> &'static str {
        self.program
    }

    /// What [`Mailer::send`] runs: `mailx` under `timeout -k 10 60`, as
    /// `backup-run.sh` bounds its send. s-nail gives up by itself after about
    /// 45 s of silence, but not on a relay that keeps trickling bytes; that
    /// one used to hold a backup, scrub or doctor run at its report step for
    /// as long as it trickled (bd DAS-Backup-Manager-sp78.2).
    pub fn command(&self, message: &MailMessage) -> (&'static str, Vec<String>) {
        let mut args = vec![
            "-k".to_string(),
            MAIL_KILL_AFTER_SECS.to_string(),
            MAIL_TIMEOUT_SECS.to_string(),
            self.program.to_string(),
        ];
        args.extend(Self::args(message));
        ("timeout", args)
    }

    /// `mailx`'s arguments for `message`, in order. No credential and no TLS
    /// setting: the hop is loopback-only plaintext and the relay owns the
    /// authenticated, certificate-verified leg to the provider. `v15-compat`
    /// stays because it is what makes s-nail honour the `mta=` URL form.
    ///
    /// `smtp-auth=none` is NOT optional and NOT symmetry with the shell path:
    /// s-nail demands a password for any `smtp://` mta and aborts with
    /// "A password is necessary for smtp authentication" (exit 4) without it.
    /// `nosave` stops a failed send from dropping the body into
    /// /root/dead.letter, which nothing reads or prunes — the caller has the
    /// report either way.
    pub fn args(message: &MailMessage) -> Vec<String> {
        vec![
            "-s".into(),
            message.subject.clone(),
            "-r".into(),
            message.from.clone(),
            "-S".into(),
            "v15-compat".into(),
            "-S".into(),
            format!("mta={}", message.mta),
            "-S".into(),
            "smtp-auth=none".into(),
            "-S".into(),
            "nosave".into(),
            message.to.clone(),
        ]
    }
}

impl Mailer for Mailx {
    /// The body goes to `mailx`'s stdin; its stdout is discarded and its
    /// stderr is kept for the error, never sent to /dev/null. The run's own
    /// lock files are opened close-on-exec (std's default), so `mailx` never
    /// holds them.
    fn send(&self, message: &MailMessage) -> Result<(), String> {
        if SPAWN_REFUSED {
            return Err(format!(
                "{} not run: a test build never mails (bd DAS-Backup-Manager-1x76)",
                self.program
            ));
        }
        let (program, args) = self.command(message);
        let output = spawn_with_stdin(program, &args, &message.body).map_err(|e| e.to_string())?;
        mailx_outcome(&output)
    }

    /// This machine, now.
    fn context(&self) -> MailContext {
        MailContext::current()
    }
}

/// Run `program` with `args`, `body` on its stdin, stdout discarded, stderr
/// captured. In non-test code only [`Mailx::send`] calls it, and only outside
/// a test build; its unit test drives it on a stub program.
fn spawn_with_stdin(
    program: &str,
    args: &[String],
    body: &str,
) -> std::io::Result<std::process::Output> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        use std::io::Write;
        stdin.write_all(body.as_bytes())?;
    }
    child.wait_with_output()
}

/// What a finished `mailx` means: success, or why not with its stderr.
fn mailx_outcome(output: &std::process::Output) -> Result<(), String> {
    if output.status.success() {
        Ok(())
    } else if output.status.code() == Some(124) || output.status.code() == Some(137) {
        // timeout(1): 124 when TERM ended it, 137 when KILL had to.
        Err(mailx_timed_out(&output.status))
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(format!(
            "mailx failed ({}): {}",
            output.status,
            stderr.trim()
        ))
    }
}

/// The error for a send `timeout(1)` ended. Its own function, so
/// `mailx_outcome` keeps the one text `tests/test_report_mail.sh` looks for in
/// the shipped binaries.
fn mailx_timed_out(status: &std::process::ExitStatus) -> String {
    format!(
        "mailx timed out after {MAIL_TIMEOUT_SECS} s ({status}): the relay did not take the report in time"
    )
}

/// What a report's message takes from the machine and the environment: the
/// host's name, the local time, and the `DAS_REPORT_TO` / `DAS_REPORT_FROM`
/// overrides (for testing without editing the live config).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MailContext {
    pub hostname: String,
    /// `YYYY-MM-DD HH:MM`, local time.
    pub now: String,
    pub to_override: Option<String>,
    pub from_override: Option<String>,
}

impl MailContext {
    /// This machine, now, with the overrides from this process's environment.
    pub fn current() -> Self {
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    /// This machine, now, with the overrides `lookup` gives for
    /// `DAS_REPORT_TO` and `DAS_REPORT_FROM` ([`MailContext::current`] looks
    /// in the environment; a test hands it a closure, so it never has to set
    /// a variable).
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Self {
        Self {
            hostname: kernel_hostname(),
            now: format_minute(&local_now()),
            to_override: lookup("DAS_REPORT_TO"),
            from_override: lookup("DAS_REPORT_FROM"),
        }
    }
}

/// Whatever the kernel has set as the host's name (`gethostname`), or
/// `unknown`.
fn kernel_hostname() -> String {
    let mut buf = [0u8; 256];
    // SAFETY: `gethostname` writes at most `buf.len()` bytes into `buf`.
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) };
    if rc == 0 {
        let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        String::from_utf8_lossy(&buf[..len]).to_string()
    } else {
        "unknown".to_string()
    }
}

/// The local time now, broken down.
fn local_now() -> libc::tm {
    let mut t: libc::time_t = 0;
    // SAFETY: `time` writes one `time_t`; `localtime_r` fills the `tm` it is
    // given and keeps no pointer to either.
    unsafe { libc::time(&mut t) };
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&t, &mut tm) };
    tm
}

/// `tm` as `YYYY-MM-DD HH:MM`.
fn format_minute(tm: &libc::tm) -> String {
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min
    )
}

/// The message that mails `report`. `kind` names the report in the subject
/// line (`[DAS <kind>] …`) so backup, scrub and doctor reports are
/// distinguishable in the inbox; the status word is derived from the body
/// containing `FAILURE`. Sender and recipient come from `[email].from`/`to`,
/// or the context's overrides; the configured `from` is wrapped in a display
/// name `DAS <kind> (<short host>)` so reports stand out in the inbox From
/// column. The short host is the name up to its first `.`, even when the
/// kernel's name is FQDN-like.
pub fn report_message(
    report: &str,
    config: &Config,
    kind: &str,
    context: &MailContext,
) -> Result<MailMessage, String> {
    let to = context
        .to_override
        .clone()
        .unwrap_or_else(|| config.email.to.clone());
    let from_addr = context
        .from_override
        .clone()
        .unwrap_or_else(|| config.email.from.clone());

    if to.is_empty() {
        return Err("No email recipient configured ([email].to or DAS_REPORT_TO)".into());
    }
    if from_addr.is_empty() {
        return Err("No email sender configured ([email].from or DAS_REPORT_FROM)".into());
    }

    let hostname = &context.hostname;
    let short_hostname = hostname.split('.').next().unwrap_or(hostname);
    // A DAS_REPORT_FROM that already carries its own display name is used
    // verbatim; a bare address gets the standard one.
    let from = if from_addr.contains('<') {
        from_addr
    } else {
        format!("DAS {kind} ({short_hostname}) <{from_addr}>")
    };
    let status_word = subject_status(report);
    Ok(MailMessage {
        subject: format!("[DAS {kind}] {hostname} — {status_word} — {}", context.now),
        from,
        to,
        mta: format!(
            "smtp://{}:{}",
            config.email.smtp_host, config.email.smtp_port
        ),
        body: report.to_string(),
    })
}

/// The subject's status word for `report`: the script's three states
/// (`subject_status` in backup-run.sh, bd DAS-Backup-Manager-qras).
fn subject_status(report: &str) -> &'static str {
    if report.contains("FAILURE") {
        "FAILURE"
    } else if report.contains("Status: COMPLETED WITH WARNINGS") {
        "SUCCESS WITH WARNINGS"
    } else {
        "SUCCESS"
    }
}

/// Send a backup report: [`send_email_report_with_kind`] with the `Backup`
/// subject kind.
pub fn send_email_report(
    report: &str,
    config: &Config,
    mailer: &dyn Mailer,
) -> Result<(), Box<dyn std::error::Error>> {
    send_email_report_with_kind(report, config, "Backup", mailer)
}

/// Send a report through `mailer` ([`MAILX`] in production).
///
/// Submits **unauthenticated plaintext** to the local mail relay named by
/// `[email].smtp_host`/`smtp_port` (`127.0.0.1:25`). This process holds no mail
/// credential of any kind: the relay authenticates upstream on its own, keyed by
/// the envelope sender, using a key readable only by root under `/etc`. Before
/// 2026-08-06 this function parsed Protonmail Bridge's credentials file and
/// passed the password to a child process — visible in `/proc/<pid>/cmdline` to
/// any local process for the duration of the send.
///
/// Nothing is handed to `mailer` unless `[email].enabled`; the message is
/// [`report_message`] in the mailer's [`Mailer::context`].
pub fn send_email_report_with_kind(
    report: &str,
    config: &Config,
    kind: &str,
    mailer: &dyn Mailer,
) -> Result<(), Box<dyn std::error::Error>> {
    if !config.email.enabled {
        return Err("Email is not enabled in config".into());
    }
    let message = report_message(report, config, kind, &mailer.context())?;
    mailer.send(&message)?;
    Ok(())
}

/// A mailer that keeps every message it is given and answers as told: what
/// this crate's tests hand every report send instead of [`MAILX`].
#[cfg(test)]
pub(crate) struct RecordingMailer {
    sent: std::sync::Mutex<Vec<MailMessage>>,
    answer: Result<(), String>,
}

#[cfg(test)]
impl RecordingMailer {
    /// Takes every message.
    pub(crate) fn accepting() -> Self {
        Self {
            sent: std::sync::Mutex::new(Vec::new()),
            answer: Ok(()),
        }
    }

    /// Keeps every message, and answers each with `why`.
    pub(crate) fn refusing(why: &str) -> Self {
        Self {
            sent: std::sync::Mutex::new(Vec::new()),
            answer: Err(why.to_string()),
        }
    }

    /// What it was given, in order.
    pub(crate) fn sent(&self) -> Vec<MailMessage> {
        self.sent.lock().unwrap().clone()
    }
}

#[cfg(test)]
impl Mailer for RecordingMailer {
    fn send(&self, message: &MailMessage) -> Result<(), String> {
        self.sent.lock().unwrap().push(message.clone());
        self.answer.clone()
    }

    fn context(&self) -> MailContext {
        test_mail_context()
    }
}

/// The context every test mailer answers: host `nas.example.org`, minute
/// `2026-11-01 01:02`, no overrides — whatever the environment holds.
#[cfg(test)]
pub(crate) fn test_mail_context() -> MailContext {
    MailContext {
        hostname: "nas.example.org".into(),
        now: "2026-11-01 01:02".into(),
        to_override: None,
        from_override: None,
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

    /// bd DAS-Backup-Manager-sp78.14: the rate is bytes over seconds, and a
    /// send that took under a second still says what it sent.
    #[test]
    fn the_throughput_line_says_the_rate_and_never_hides_a_quick_send() {
        let with = |bytes: u64, secs: u64| {
            let result = BackupResult {
                success: true,
                mode: BackupMode::Incremental,
                snapshots_created: Some(1),
                snapshots_sent: Some(1),
                snapshots_cleaned: 0,
                bytes_sent: Some(bytes),
                boot: crate::backup::BootStep::NotSelected,
                recovery_os: crate::backup::RecoveryOsStep::NotSelected,
                usb_link: crate::run_steps::UsbLinkStep::NotChecked,
                expiry: crate::run_steps::ExpiryStep::NotRun,
                indexed: false,
                report_sent: false,
                errors: vec![],
                duration_secs: secs,
                coverage_message: String::new(),
            };
            format_report(&result, &Config::default())
        };
        let gib = 1_073_741_824;
        let report = with(4 * gib, 4);
        assert!(
            report.contains("  Total                    4.00 GiB @ 1.00 GiB/s\n"),
            "{report}"
        );
        let report = with(512 * 1024, 0);
        assert!(
            report.contains("  Total                    512.00 KiB in under a second\n"),
            "{report}"
        );
        assert!(!report.contains("no data transferred"), "{report}");
    }

    #[test]
    fn format_report_success() {
        let result = BackupResult {
            success: true,
            mode: BackupMode::Full,
            snapshots_created: Some(5),
            snapshots_sent: Some(5),
            snapshots_cleaned: 2,
            bytes_sent: Some(1_073_741_824),
            boot: crate::backup::BootStep::NotSelected,
            recovery_os: crate::backup::RecoveryOsStep::NotSelected,
            usb_link: crate::run_steps::UsbLinkStep::NotChecked,
            expiry: crate::run_steps::ExpiryStep::NotRun,
            indexed: true,
            report_sent: false,
            errors: vec![],
            duration_secs: 3600,
            coverage_message: String::new(),
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

    /// A boot-step outcome in a finished run's report.
    fn boot_report(outcome: crate::backup::BootOutcome, success: bool) -> String {
        let mut result = result_with(Some(0), Some(0), 0, &[]);
        result.success = success;
        result.boot = crate::backup::BootStep::Ran(outcome);
        format_report(&result, &Config::default())
    }

    #[test]
    fn a_boot_warning_completes_with_warnings_and_a_boot_failure_fails_the_report() {
        let warned = boot_report(
            crate::backup::BootOutcome {
                warnings: vec!["[t] no snapshot".into()],
                ..Default::default()
            },
            true,
        );
        assert!(
            warned.contains("Status: COMPLETED WITH WARNINGS"),
            "{warned}"
        );
        assert!(
            warned.contains("Boot subvolumes       WARN  (0 updated, 0 skipped, 1 warnings)"),
            "{warned}"
        );
        assert!(warned.contains("BOOT SUBVOLUMES"), "{warned}");
        assert!(warned.contains("WARN  [t] no snapshot"), "{warned}");

        let failed = boot_report(
            crate::backup::BootOutcome {
                failures: vec!["[t] could not list".into()],
                ..Default::default()
            },
            false,
        );
        assert!(failed.contains("Status: FAILURES DETECTED"), "{failed}");
        assert!(
            failed.contains("Boot subvolumes       FAIL  (0 updated, 1 failed)"),
            "{failed}"
        );
        assert!(failed.contains("FAIL  [t] could not list"), "{failed}");

        // The counter-case: a clean boot step flags nothing.
        let clean = boot_report(
            crate::backup::BootOutcome {
                updated: 2,
                ..Default::default()
            },
            true,
        );
        assert!(
            clean.contains("Status: ALL OPERATIONS SUCCESSFUL"),
            "{clean}"
        );
        assert!(!clean.contains("BOOT SUBVOLUMES"), "{clean}");
        assert!(clean.contains("Boot subvolumes       OK  (2 updated, 0 skipped)"));
    }

    /// The report's rows, in the order the script prints them.
    fn rows(report: &str) -> Vec<&str> {
        report
            .lines()
            .skip_while(|l| !l.starts_with("BACKUP OPERATIONS"))
            .skip(2)
            .take_while(|l| !l.is_empty())
            .collect()
    }

    #[test]
    fn the_usb_link_row_leads_the_operations_and_a_failed_link_says_why() {
        let mut result = result_with(
            Some(0),
            Some(0),
            0,
            &["USB link: negotiated 5000 Mbit/s, expected 10000"],
        );
        result.usb_link = crate::run_steps::UsbLinkStep::checked_for_test(
            crate::recovery_os::RecordStatus::Fail,
            "negotiated 5000 Mbit/s, expected 10000",
        );
        let report = format_report(&result, &Config::default());
        assert!(report.contains("Status: FAILURES DETECTED"), "{report}");
        let ops = rows(&report);
        assert_eq!(
            ops[0], "  USB link              FAIL  (negotiated 5000 Mbit/s, expected 10000)",
            "{report}"
        );
        assert!(ops[1].starts_with("  btrbk send/receive    OK"), "{report}");

        // Never checked: the row says so, and flags nothing.
        let quiet = format_report(&result_with(Some(0), Some(0), 0, &[]), &Config::default());
        assert_eq!(
            rows(&quiet)[0],
            "  USB link              N/A  (not checked)"
        );
        assert!(
            quiet.contains("Status: ALL OPERATIONS SUCCESSFUL"),
            "{quiet}"
        );

        // An unread link only warns.
        let mut warned = result_with(Some(0), Some(0), 0, &[]);
        warned.usb_link = crate::run_steps::UsbLinkStep::checked_for_test(
            crate::recovery_os::RecordStatus::Warn,
            "no USB device's product contains 'X'",
        );
        let warned = format_report(&warned, &Config::default());
        assert!(
            warned.contains("Status: COMPLETED WITH WARNINGS"),
            "{warned}"
        );
    }

    #[test]
    fn the_retired_expiry_row_and_section_are_in_the_report() {
        let mut result = result_with(Some(0), Some(0), 0, &[]);
        result.expiry = crate::run_steps::ExpiryStep::from_run(
            crate::run_steps::ExpireRun::Performed {
                section: "RETIRED SUBVOLUMES\n  nvme-root @gone: 3 snapshots deleted\n".into(),
                failure: None,
            },
            false,
        );
        let report = format_report(&result, &Config::default());
        let ops = rows(&report);
        let at = |prefix: &str| ops.iter().position(|r| r.starts_with(prefix)).unwrap();
        assert_eq!(
            ops[at("  Retired expiry")],
            "  Retired expiry        OK  (n/a)"
        );
        assert!(at("  Retired expiry") < at("  Recovery OS"), "{report}");
        assert!(
            report.contains("RETIRED SUBVOLUMES\n  nvme-root @gone: 3 snapshots deleted"),
            "{report}"
        );

        // A selection run says it did not expire; never reached has no row text.
        let mut narrow = result_with(Some(0), Some(0), 0, &[]);
        narrow.expiry = crate::run_steps::ExpiryStep::skipped_for_selection();
        let narrow = format_report(&narrow, &Config::default());
        assert!(
            narrow.contains("Retired expiry        SKIP  (not performed: a run of a selection"),
            "{narrow}"
        );
        assert!(
            narrow.contains("Status: ALL OPERATIONS SUCCESSFUL"),
            "{narrow}"
        );

        // A failed expiry is not a btrbk send/receive failure.
        let mut failed = result_with(
            Some(0),
            Some(0),
            0,
            &["Retired expiry: exit code 1 — see RETIRED SUBVOLUMES in the report"],
        );
        failed.expiry = crate::run_steps::ExpiryStep::from_run(
            crate::run_steps::ExpireRun::Performed {
                section: String::new(),
                failure: Some("exit code 1".into()),
            },
            false,
        );
        let failed = format_report(&failed, &Config::default());
        assert!(failed.contains("Status: FAILURES DETECTED"), "{failed}");
        assert!(
            failed.contains(
                "Retired expiry        FAIL  (exit code 1 — see RETIRED SUBVOLUMES in the report)"
            ),
            "{failed}"
        );
    }

    #[test]
    fn a_boot_failure_that_names_btrbk_is_not_a_btrbk_send_receive_failure() {
        let mut result = result_with(Some(0), Some(0), 0, &[]);
        result.success = false;
        result.errors = vec![format!(
            "{}Cannot read /etc/btrbk/btrbk.conf (denied)",
            crate::backup::BOOT_ERROR_PREFIX
        )];
        result.boot = crate::backup::BootStep::Ran(crate::backup::BootOutcome {
            failures: vec!["Cannot read /etc/btrbk/btrbk.conf (denied)".into()],
            ..Default::default()
        });
        let report = format_report(&result, &Config::default());
        assert!(report.contains("btrbk send/receive    OK"), "{report}");
    }

    fn result_with(
        created: Option<usize>,
        sent: Option<usize>,
        bytes: u64,
        errors: &[&str],
    ) -> BackupResult {
        BackupResult {
            success: errors.is_empty(),
            mode: BackupMode::Incremental,
            snapshots_created: created,
            snapshots_sent: sent,
            snapshots_cleaned: 0,
            bytes_sent: Some(bytes),
            boot: crate::backup::BootStep::NotSelected,
            recovery_os: crate::backup::RecoveryOsStep::NotSelected,
            usb_link: crate::run_steps::UsbLinkStep::NotChecked,
            expiry: crate::run_steps::ExpiryStep::NotRun,
            indexed: false,
            report_sent: false,
            errors: errors.iter().map(|e| e.to_string()).collect(),
            duration_secs: 60,
            coverage_message: String::new(),
        }
    }

    /// Review M7: a failed step's count is unknown, and the report must not
    /// turn that into "no data transferred".
    #[test]
    fn the_throughput_section_says_not_measured_when_a_count_is_unknown_never_no_data() {
        let cfg = Config::default();
        let unknown = [
            result_with(None, Some(0), 0, &["Snapshot step failed: x"]),
            result_with(Some(0), None, 0, &["Send step failed: x"]),
            result_with(None, None, 0, &["Full backup pipeline failed: x"]),
        ];
        for result in &unknown {
            let report = format_report(result, &cfg);
            assert!(report.contains("(not measured"), "{report}");
            assert!(!report.contains("no data transferred"), "{report}");
        }
        // The controls: counts that are real zeros do say it, and a real
        // measurement says neither.
        // Counted, and a target not read after the steps: the size is unknown
        // (bd DAS-Backup-Manager-3lpr), never "no data transferred".
        let mut unmeasured = result_with(Some(1), Some(1), 0, &[]);
        unmeasured.bytes_sent = None;
        let report = format_report(&unmeasured, &cfg);
        assert!(
            report.contains("(not measured — a target could not be read"),
            "{report}"
        );
        assert!(!report.contains("no data transferred"), "{report}");
        let report = format_report(&result_with(Some(0), Some(0), 0, &[]), &cfg);
        assert!(report.contains("(no data transferred)"), "{report}");
        assert!(!report.contains("not measured"), "{report}");
        let report = format_report(&result_with(Some(1), Some(1), 1_073_741_824, &[]), &cfg);
        assert!(report.contains("1.00 GiB"), "{report}");
        assert!(!report.contains("not measured"), "{report}");
    }

    /// Review M7: the btrbk line follows the step errors, whatever their text.
    #[test]
    fn the_btrbk_line_is_fail_for_every_failed_step_even_one_that_never_names_btrbk() {
        let cfg = Config::default();
        for error in [
            "Send step failed: none of the selected sources sends to a selected target",
            "Snapshot step failed: No source selected",
            "Full backup pipeline failed: Refusing to run on /mnt/x",
        ] {
            let report = format_report(&result_with(None, None, 0, &[error]), &cfg);
            assert!(
                report.contains("btrbk send/receive    FAIL"),
                "{error}: {report}"
            );
        }
        // The controls: no error, and an error of another step, are OK.
        let report = format_report(&result_with(Some(1), Some(1), 1, &[]), &cfg);
        assert!(report.contains("btrbk send/receive    OK"), "{report}");
        let report = format_report(
            &result_with(Some(1), Some(1), 1, &["Boot archive step failed: x"]),
            &cfg,
        );
        assert!(report.contains("btrbk send/receive    OK"), "{report}");
    }

    #[test]
    fn the_report_carries_the_subvolume_sync_section_and_its_status() {
        let result = BackupResult {
            success: false,
            mode: BackupMode::Incremental,
            snapshots_created: Some(1),
            snapshots_sent: Some(1),
            snapshots_cleaned: 0,
            bytes_sent: Some(0),
            boot: crate::backup::BootStep::NotSelected,
            recovery_os: crate::backup::RecoveryOsStep::NotSelected,
            usb_link: crate::run_steps::UsbLinkStep::NotChecked,
            expiry: crate::run_steps::ExpiryStep::NotRun,
            indexed: false,
            report_sent: false,
            errors: vec!["Subvolume sync failed — see SUBVOLUME SYNC in the report".into()],
            duration_secs: 1,
            coverage_message: String::new(),
        };
        let sync = crate::backup::SyncSection {
            report: "SUBVOLUME SYNC\n  VOLUMES NOT READ (nothing adopted or retired there):\n    /ssd: not mounted\n".into(),
            failed: true,
            coverage: None,
        };
        let cfg = Config::default();
        let report = format_report_with_sync(&result, &cfg, Some(&sync));
        assert!(report.contains("FAILURES DETECTED"), "{report}");
        assert!(
            report.contains("  Subvolume sync        FAIL\n"),
            "{report}"
        );
        // The section sits between the operations and THROUGHPUT, as in the
        // report backup-run.sh writes.
        let ops = report.find("BACKUP OPERATIONS").unwrap();
        let section = report.find("\nSUBVOLUME SYNC\n").expect("section present");
        let throughput = report.find("\nTHROUGHPUT\n").unwrap();
        assert!(ops < section && section < throughput, "{report}");
        assert!(report.contains("    /ssd: not mounted\n"), "{report}");

        let ok = crate::backup::SyncSection {
            report: "SUBVOLUME SYNC\n  No new, vanished or returning subvolumes.\n".into(),
            failed: false,
            coverage: None,
        };
        let report = format_report_with_sync(&result, &cfg, Some(&ok));
        assert!(report.contains("  Subvolume sync        OK\n"), "{report}");
        // Without a sync (other callers) neither the line nor the section.
        let result = BackupResult {
            errors: vec![],
            ..result
        };
        let report = format_report(&result, &cfg);
        assert!(!report.contains("Subvolume sync"), "{report}");
        assert!(!report.contains("SUBVOLUME SYNC"), "{report}");
    }

    #[test]
    fn record_and_retrieve_backup_run() {
        let db = Database::open(":memory:").unwrap();
        let result = BackupResult {
            success: true,
            mode: BackupMode::Incremental,
            snapshots_created: Some(3),
            snapshots_sent: Some(3),
            snapshots_cleaned: 0,
            bytes_sent: Some(500_000),
            boot: crate::backup::BootStep::NotSelected,
            recovery_os: crate::backup::RecoveryOsStep::NotSelected,
            usb_link: crate::run_steps::UsbLinkStep::NotChecked,
            expiry: crate::run_steps::ExpiryStep::NotRun,
            indexed: true,
            report_sent: false,
            errors: vec![],
            duration_secs: 120,
            coverage_message: String::new(),
        };
        let id = record_backup_run(&db, &result).unwrap();
        assert!(id > 0);

        let history = get_backup_history(&db, 10).unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].id, id);
        assert!(history[0].success);
        assert_eq!(history[0].mode, "incremental");
        assert_eq!(history[0].snapshots_created, Some(3));
        assert_eq!(history[0].snapshots_sent, Some(3));
        assert_eq!(history[0].bytes_sent, Some(500_000));
        assert_eq!(history[0].duration_secs, 120);
        assert!(history[0].errors.is_empty());
    }

    #[test]
    fn a_count_shows_as_its_number_or_unknown_never_as_zero() {
        assert_eq!(format_count(Some(0)), "0");
        assert_eq!(format_count(Some(53)), "53");
        assert_eq!(format_count(None), "unknown");
    }

    #[test]
    fn history_json_has_null_for_an_unknown_count_and_the_number_otherwise() {
        let runs = [
            BackupRunRecord {
                id: 290,
                timestamp: 1_791_000_000,
                success: false,
                mode: "full".into(),
                snaps_created: None,
                snaps_sent: None,
                bytes_sent: Some(0),
                duration_secs: 516,
                errors: vec![
                    "btrbk: exit code 10".into(),
                    "btrbk_counters: btrbk list latest failed; counts unknown".into(),
                ],
            },
            BackupRunRecord {
                id: 289,
                timestamp: 1_790_900_000,
                success: true,
                mode: "incremental".into(),
                snaps_created: Some(0),
                snaps_sent: Some(41),
                bytes_sent: Some(4096),
                duration_secs: 300,
                errors: vec![],
            },
        ];
        assert_eq!(
            backup_history_json(&runs),
            serde_json::json!([
                {
                    "id": 290, "timestamp": 1_791_000_000_i64, "mode": "full",
                    "success": false, "duration_secs": 516, "snaps_created": null,
                    "snaps_sent": null, "bytes_sent": 0,
                    "errors": [
                        "btrbk: exit code 10",
                        "btrbk_counters: btrbk list latest failed; counts unknown"
                    ]
                },
                {
                    "id": 289, "timestamp": 1_790_900_000_i64, "mode": "incremental",
                    "success": true, "duration_secs": 300, "snaps_created": 0,
                    "snaps_sent": 41, "bytes_sent": 4096, "errors": []
                }
            ])
        );
        assert_eq!(backup_history_json(&[]), serde_json::json!([]));
    }

    #[test]
    fn format_report_with_errors() {
        let result = BackupResult {
            success: false,
            mode: BackupMode::Full,
            snapshots_created: Some(2),
            snapshots_sent: Some(0),
            snapshots_cleaned: 0,
            bytes_sent: Some(0),
            boot: crate::backup::BootStep::NotSelected,
            recovery_os: crate::backup::RecoveryOsStep::NotSelected,
            usb_link: crate::run_steps::UsbLinkStep::NotChecked,
            expiry: crate::run_steps::ExpiryStep::NotRun,
            indexed: false,
            report_sent: false,
            errors: vec!["btrbk failed".into(), "target not mounted".into()],
            duration_secs: 60,
            coverage_message: String::new(),
        };
        let cfg = Config::default();
        let report = format_report(&result, &cfg);
        assert!(report.contains("FAILURES DETECTED"));
        assert!(report.contains("ERRORS"));
        assert!(report.contains("btrbk failed"));
        assert!(report.contains("target not mounted"));
    }

    fn target_health(label: &str, mounted: bool) -> crate::health::TargetHealth {
        crate::health::TargetHealth {
            label: label.into(),
            serial: format!("SER-{label}"),
            mounted,
            total_bytes: 4 * 1024 * 1024 * 1024,
            used_bytes: 1024 * 1024 * 1024,
            usage_known: true,
            snapshot_count: 3,
            smart_status: Some("PASSED".into()),
            temperature_c: Some(31),
            power_on_hours: None,
            errors: None,
            scrub: crate::health::ScrubHealth::not_applicable(),
            members: Vec::new(),
        }
    }

    #[test]
    fn smart_has_a_row_per_drive_and_an_absent_leg_says_absent() {
        // bd DAS-Backup-Manager-uhi: a RAID-1 target listed only its first
        // drive, so a missing leg had no row to miss.
        let mut pair = target_health("pair", true);
        pair.members = vec![
            crate::health::MemberHealth {
                serial: "SERIAL11".into(),
                present: true,
                smart_status: Some("PASSED".into()),
                temperature_c: Some(30),
                power_on_hours: Some(1200),
            },
            crate::health::MemberHealth {
                serial: "SERIAL12".into(),
                ..Default::default()
            },
        ];
        let health = crate::health::HealthReport {
            status: crate::health::HealthStatus::Healthy,
            targets: vec![pair],
            last_backup: None,
            growth_points: Vec::new(),
            warnings: Vec::new(),
            recovery_os: Vec::new(),
        };
        let text = render_capacity_and_smart(Some(&health));
        let smart = text.split_once("SMART STATUS").unwrap().1;
        assert!(
            smart.contains("  pair                     SERIAL11   PASSED  30°C    1200h\n"),
            "{text}"
        );
        assert!(
            smart.contains("  pair                     SERIAL12   ABSENT\n"),
            "{text}"
        );
        assert!(!smart.contains("SER-pair"), "{text}");
    }

    #[test]
    fn capacity_lists_mounted_targets_and_smart_lists_all() {
        let health = crate::health::HealthReport {
            status: crate::health::HealthStatus::Healthy,
            targets: vec![target_health("up", true), target_health("down", false)],
            last_backup: None,
            growth_points: Vec::new(),
            warnings: Vec::new(),
            recovery_os: Vec::new(),
        };
        let text = render_capacity_and_smart(Some(&health));
        let (capacity, smart) = text.split_once("SMART STATUS").unwrap();
        assert!(
            capacity.contains("raw device space"),
            "the capacity table says its sizes are raw: {text}"
        );
        assert!(
            capacity.contains("  up                       1.00 GiB   3.00 GiB   25%\n"),
            "{text}"
        );
        assert!(
            !capacity.contains("down"),
            "an unmounted target has no capacity row: {text}"
        );
        assert!(
            smart.contains("  up                       SER-up     PASSED  31°C    N/A\n"),
            "{text}"
        );
        assert!(smart.contains("  down "), "{text}");
        // Measured, but genuinely empty: still a measurement.
        let mut empty = target_health("empty", true);
        empty.used_bytes = 0;
        let mut unknown = target_health("unmeasured", true);
        unknown.total_bytes = 0;
        unknown.used_bytes = 0;
        unknown.usage_known = false;
        let health2 = crate::health::HealthReport {
            targets: vec![empty, unknown],
            ..health
        };
        let text = render_capacity_and_smart(Some(&health2));
        assert!(
            text.contains("  empty                    0 B        4.00 GiB   0%\n"),
            "{text}"
        );
        assert!(
            text.contains("  unmeasured               unknown    unknown    unknown\n"),
            "{text}"
        );
        assert!(!text.contains("unmeasured               0 B"), "{text}");
        let none = render_capacity_and_smart(None);
        assert!(none.contains("(health data unavailable)"), "{none}");
        assert!(!none.contains("SMART STATUS"), "{none}");
    }

    #[test]
    fn latest_snapshots_drop_the_header_line() {
        let out = "SOURCE_SUBVOLUME  SNAPSHOT  STATUS  TARGET\n/v/@  /v/.s/root-.1  -  /t/nvme/root-.1\n/v/@d  /v/.s/d.1  -  /t/nvme/d.1\n";
        let text = render_latest_snapshots(Ok(out));
        assert!(text.starts_with("\nLATEST SNAPSHOTS\n"), "{text}");
        assert!(!text.contains("SOURCE_SUBVOLUME"), "{text}");
        assert!(
            text.contains("  /v/@  /v/.s/root-.1  -  /t/nvme/root-.1\n"),
            "{text}"
        );
        assert!(text.contains("  /v/@d  /v/.s/d.1"), "{text}");
        assert!(!text.contains("unavailable"), "{text}");
        // btrbk answered, nothing listed: an empty section, no note.
        assert!(render_latest_snapshots(Ok("HEADER\n")).ends_with("─\n"));
    }

    #[test]
    fn the_latest_listing_is_confined_to_the_filters_it_is_given() {
        let mut config = Config::default();
        config.general.btrbk_conf = "/c/btrbk.conf".into();
        let none: &[String] = &[];
        assert_eq!(
            latest_listing_args(&config, none),
            ["-c", "/c/btrbk.conf", "list", "latest"]
        );
        let filters = ["/t1/root".to_string(), "/t1/home".to_string()];
        assert_eq!(
            latest_listing_args(&config, &filters),
            [
                "-c",
                "/c/btrbk.conf",
                "list",
                "latest",
                "/t1/root",
                "/t1/home"
            ]
        );
    }

    #[test]
    fn latest_snapshots_say_when_btrbk_could_not_answer() {
        let text = render_latest_snapshots(Err(
            "btrbk list latest exited with exit status: 2: ERROR: lock",
        ));
        assert!(
            text.ends_with(
                "─\n  (unavailable: btrbk list latest exited with exit status: 2: ERROR: lock)\n"
            ),
            "{text}"
        );
    }

    #[test]
    fn latest_listing_keeps_output_only_from_a_successful_run() {
        use std::os::unix::process::ExitStatusExt;
        let out = |code: i32, stdout: &str, stderr: &str| {
            Ok(std::process::Output {
                status: std::process::ExitStatus::from_raw(code << 8),
                stdout: stdout.as_bytes().to_vec(),
                stderr: stderr.as_bytes().to_vec(),
            })
        };
        assert_eq!(
            latest_listing(out(0, "H\nrow\n", "")),
            Ok("H\nrow\n".to_string())
        );
        let err = latest_listing(out(2, "H\nstale\n", "ERROR: lock\nmore")).unwrap_err();
        assert!(err.starts_with("btrbk list latest exited with"), "{err}");
        assert!(err.ends_with(": ERROR: lock"), "{err}");
        let err = latest_listing(Err(std::io::Error::other("no such file"))).unwrap_err();
        assert_eq!(err, "btrbk could not be run: no such file");
    }

    // --- The mailer seam (bd DAS-Backup-Manager-1x76) ----------------------

    fn mail_config() -> Config {
        let mut config = Config::default();
        config.email.enabled = true;
        config.email.to = "ops@example.org".into();
        config.email.from = "das@example.org".into();
        config.email.smtp_host = "127.0.0.1".into();
        // Unreachable: a test never names the live relay's port, so not even
        // a broken guard could hand it a report (bd DAS-Backup-Manager-1x76).
        config.email.smtp_port = 1;
        config
    }

    fn context(hostname: &str) -> MailContext {
        MailContext {
            hostname: hostname.into(),
            now: "2026-11-01 01:02".into(),
            to_override: None,
            from_override: None,
        }
    }

    fn message(subject: &str, from: &str, to: &str, mta: &str, body: &str) -> MailMessage {
        MailMessage {
            subject: subject.into(),
            from: from.into(),
            to: to.into(),
            mta: mta.into(),
            body: body.into(),
        }
    }

    #[test]
    fn a_report_message_is_exactly_what_the_relay_is_to_receive() {
        let config = mail_config();
        let ok = report_message("all fine\n", &config, "Scrub", &context("nas.example.org"));
        assert_eq!(
            ok,
            Ok(message(
                "[DAS Scrub] nas.example.org — SUCCESS — 2026-11-01 01:02",
                "DAS Scrub (nas) <das@example.org>",
                "ops@example.org",
                "smtp://127.0.0.1:1",
                "all fine\n",
            ))
        );
        let failed = report_message("Status: FAILURE\n", &config, "Backup", &context("nas"));
        assert_eq!(
            failed,
            Ok(message(
                "[DAS Backup] nas — FAILURE — 2026-11-01 01:02",
                "DAS Backup (nas) <das@example.org>",
                "ops@example.org",
                "smtp://127.0.0.1:1",
                "Status: FAILURE\n",
            ))
        );
        // A run that worked with warnings says so in the subject, as
        // backup-run.sh's does (bd DAS-Backup-Manager-qras).
        let warned = report_message(
            "  Status: COMPLETED WITH WARNINGS\n",
            &config,
            "Backup",
            &context("nas"),
        );
        assert_eq!(
            warned.unwrap().subject,
            "[DAS Backup] nas — SUCCESS WITH WARNINGS — 2026-11-01 01:02"
        );
        let mut elsewhere = mail_config();
        elsewhere.email.smtp_host = "relay.example.org".into();
        elsewhere.email.smtp_port = 2525;
        assert_eq!(
            report_message("x", &elsewhere, "Doctor", &context("h"))
                .unwrap()
                .mta,
            "smtp://relay.example.org:2525"
        );
    }

    #[test]
    fn the_overrides_replace_sender_and_recipient_and_a_named_sender_is_kept_verbatim() {
        let config = mail_config();
        let mut ctx = context("nas.example.org");
        ctx.to_override = Some("test@example.org".into());
        ctx.from_override = Some("probe@example.org".into());
        let m = report_message("x", &config, "Backup", &ctx).unwrap();
        assert_eq!(m.to, "test@example.org");
        assert_eq!(m.from, "DAS Backup (nas) <probe@example.org>");
        ctx.from_override = Some("Probe <probe@example.org>".into());
        let m = report_message("x", &config, "Backup", &ctx).unwrap();
        assert_eq!(m.from, "Probe <probe@example.org>", "its own display name");
    }

    #[test]
    fn no_recipient_or_no_sender_is_refused_before_any_message_exists() {
        let mut config = mail_config();
        config.email.to = String::new();
        assert_eq!(
            report_message("x", &config, "Backup", &context("h")),
            Err("No email recipient configured ([email].to or DAS_REPORT_TO)".into())
        );
        let mut config = mail_config();
        config.email.from = String::new();
        assert_eq!(
            report_message("x", &config, "Backup", &context("h")),
            Err("No email sender configured ([email].from or DAS_REPORT_FROM)".into())
        );
        // An empty override is an empty address, not "use the config".
        let mut ctx = context("h");
        ctx.to_override = Some(String::new());
        assert!(report_message("x", &mail_config(), "Backup", &ctx).is_err());
    }

    #[test]
    fn mailx_is_told_exactly_this() {
        let m = message(
            "[DAS Backup] nas — SUCCESS — 2026-11-01 01:02",
            "DAS Backup (nas) <das@example.org>",
            "ops@example.org",
            "smtp://127.0.0.1:1",
            "body",
        );
        assert_eq!(
            Mailx::args(&m),
            [
                "-s",
                "[DAS Backup] nas — SUCCESS — 2026-11-01 01:02",
                "-r",
                "DAS Backup (nas) <das@example.org>",
                "-S",
                "v15-compat",
                "-S",
                "mta=smtp://127.0.0.1:1",
                "-S",
                "smtp-auth=none",
                "-S",
                "nosave",
                "ops@example.org",
            ]
        );
        assert_eq!(MAILX.program(), "mailx", "production runs mailx from PATH");
    }

    /// The structural guard: in a test build the production mailer refuses
    /// before it spawns anything. The stub stands where `mailx` would: it
    /// records any run of itself, and is seen to do so when run directly,
    /// so a spawn would show.
    #[test]
    fn a_test_build_never_spawns_mailx() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("ran");
        let stub = dir.path().join("mailx-stub");
        // Written by `install`, never by this process: a write descriptor on
        // the stub inherited by another test's fork fails the probe's exec
        // with ETXTBSY (seen in CI on 4d1071ae, bd DAS-Backup-Manager-1x76).
        crate::recovery_os::session::tests::write_executable(
            &stub,
            &format!(
                "#!/bin/bash\nprintf '%s\\n' \"$@\" >>'{}'\ncat >/dev/null\nexit 0\n",
                marker.display()
            ),
        );
        // The stub records when it is run.
        let probe = Command::new(&stub)
            .arg("probe")
            .stdin(Stdio::null())
            .status()
            .unwrap();
        assert!(probe.success());
        assert_eq!(std::fs::read_to_string(&marker).unwrap(), "probe\n");
        std::fs::remove_file(&marker).unwrap();

        let program: &'static str = Box::leak(stub.to_string_lossy().into_owned().into_boxed_str());
        let m = report_message("x", &mail_config(), "Backup", &context("h")).unwrap();
        let err = Mailx::new(program).send(&m).unwrap_err();
        assert_eq!(
            err,
            format!("{program} not run: a test build never mails (bd DAS-Backup-Manager-1x76)")
        );
        assert!(!marker.exists(), "nothing was spawned");
    }

    #[test]
    fn a_finished_mailx_is_success_only_on_exit_0_and_its_stderr_says_why_not() {
        use std::os::unix::process::ExitStatusExt;
        let done = |code: i32, stderr: &str| std::process::Output {
            status: std::process::ExitStatus::from_raw(code << 8),
            stdout: Vec::new(),
            stderr: stderr.as_bytes().to_vec(),
        };
        assert_eq!(mailx_outcome(&done(0, "")), Ok(()));
        assert_eq!(
            mailx_outcome(&done(4, "  A password is necessary\n")),
            Err("mailx failed (exit status: 4): A password is necessary".into())
        );
    }

    #[test]
    fn the_subject_time_is_the_local_minute() {
        // SAFETY: `tm` is plain data; all-zero is a valid value.
        let mut tm: libc::tm = unsafe { std::mem::zeroed() };
        tm.tm_year = 126;
        tm.tm_mon = 10;
        tm.tm_mday = 1;
        tm.tm_hour = 1;
        tm.tm_min = 2;
        assert_eq!(format_minute(&tm), "2026-11-01 01:02");
        let now = MailContext::current().now;
        let year: i32 = now[..4].parse().unwrap();
        assert!((2026..2200).contains(&year), "{now}");
        assert_eq!(now.len(), "2026-11-01 01:02".len(), "{now}");
    }

    #[test]
    fn the_host_name_is_the_kernels() {
        let kernel = std::fs::read_to_string("/proc/sys/kernel/hostname").unwrap();
        assert_eq!(kernel_hostname(), kernel.trim_end());
        assert_eq!(MailContext::current().hostname, kernel.trim_end());
    }

    /// The production mailer's context is this machine, now.
    #[test]
    fn the_production_mailers_context_is_this_machine_now() {
        let kernel = std::fs::read_to_string("/proc/sys/kernel/hostname").unwrap();
        let context = MAILX.context();
        assert_eq!(context.hostname, kernel.trim_end());
        assert_eq!(
            context.now.len(),
            "2026-11-01 01:02".len(),
            "{}",
            context.now
        );
    }

    /// The overrides are what the lookup gives for exactly these two names,
    /// and nothing when it gives nothing — driven by a closure, never by
    /// setting a variable. (That `current` looks in the environment, and that
    /// the built binary honours them, is `tests/test_report_mail.sh`.)
    #[test]
    fn the_overrides_are_what_the_lookup_gives_for_das_report_to_and_from() {
        let asked = std::sync::Mutex::new(Vec::new());
        let context = MailContext::from_lookup(|name| {
            asked.lock().unwrap().push(name.to_string());
            match name {
                "DAS_REPORT_TO" => Some("probe-to@example.org".into()),
                "DAS_REPORT_FROM" => Some("probe-from@example.org".into()),
                _ => Some("unexpected@example.org".into()),
            }
        });
        assert_eq!(context.to_override.as_deref(), Some("probe-to@example.org"));
        assert_eq!(
            context.from_override.as_deref(),
            Some("probe-from@example.org")
        );
        let mut asked = asked.into_inner().unwrap();
        asked.sort();
        assert_eq!(asked, ["DAS_REPORT_FROM", "DAS_REPORT_TO"]);

        let none = MailContext::from_lookup(|_| None);
        assert_eq!((none.to_override, none.from_override), (None, None));
        let kernel = std::fs::read_to_string("/proc/sys/kernel/hostname").unwrap();
        assert_eq!(none.hostname, kernel.trim_end());
    }

    /// The spawn itself (bd DAS-Backup-Manager-1x76 review): the arguments
    /// as given, the body on stdin, stdout discarded, stderr kept — and a
    /// failing program's stderr is what the error says.
    #[test]
    fn the_spawn_passes_the_arguments_and_the_body_and_keeps_stderr() {
        let dir = tempfile::tempdir().unwrap();
        let args_file = dir.path().join("args");
        let body_file = dir.path().join("body");
        let stub = dir.path().join("program");
        // By `install`, so no write descriptor on it can reach another
        // test's fork (ETXTBSY; see `a_test_build_never_spawns_mailx`).
        crate::recovery_os::session::tests::write_executable(
            &stub,
            &format!(
                "#!/bin/bash\nprintf '%s\\n' \"$@\" >'{args}'\ncat >'{body}'\n\
                 echo 'to stdout'\n\
                 if [[ \"$1\" == fail ]]; then echo '  relay said no  ' >&2; exit 4; fi\n\
                 exit 0\n",
                args = args_file.display(),
                body = body_file.display()
            ),
        );
        let program = stub.to_str().unwrap();

        let args = vec![
            "-s".to_string(),
            "a subject — with spaces".to_string(),
            "to@example.org".to_string(),
        ];
        let out = spawn_with_stdin(program, &args, "line 1\nline 2\n").unwrap();
        assert!(out.status.success());
        assert_eq!(
            std::fs::read_to_string(&args_file).unwrap(),
            "-s\na subject — with spaces\nto@example.org\n",
            "each argument as given"
        );
        assert_eq!(
            std::fs::read_to_string(&body_file).unwrap(),
            "line 1\nline 2\n",
            "the body arrives on stdin"
        );
        assert!(out.stdout.is_empty(), "stdout is discarded");
        assert_eq!(mailx_outcome(&out), Ok(()));

        let out = spawn_with_stdin(program, &["fail".to_string()], "body").unwrap();
        assert_eq!(std::fs::read_to_string(&body_file).unwrap(), "body");
        assert_eq!(
            mailx_outcome(&out),
            Err("mailx failed (exit status: 4): relay said no".into()),
            "the failing program's stderr reaches the error"
        );
        let absent = dir.path().join("absent");
        assert!(spawn_with_stdin(absent.to_str().unwrap(), &[], "").is_err());
    }

    /// bd DAS-Backup-Manager-sp78.2: the send runs under `timeout -k 10 60`,
    /// as `backup-run.sh`'s does, so a relay that keeps trickling bytes costs
    /// the report, not the run.
    #[test]
    fn the_production_send_is_bounded_as_the_script_bounds_it() {
        let msg = message(
            "s",
            "f@example.org",
            "t@example.org",
            "smtp://127.0.0.1:25",
            "b",
        );
        let (program, args) = MAILX.command(&msg);
        assert_eq!(program, "timeout");
        assert_eq!(args[..4], ["-k", "10", "60", "mailx"]);
        assert_eq!(args[4..], Mailx::args(&msg)[..]);
    }

    #[test]
    fn a_send_that_never_finishes_is_ended_at_the_bound() {
        let start = std::time::Instant::now();
        let args: Vec<String> = ["-k", "1", "1", "sleep", "30"].map(String::from).to_vec();
        let out = spawn_with_stdin("timeout", &args, "body").unwrap();
        assert!(
            start.elapsed() < std::time::Duration::from_secs(10),
            "ended at the bound, not when sleep finished: {:?}",
            start.elapsed()
        );
        let err = mailx_outcome(&out).unwrap_err();
        assert!(err.contains("timed out after 60 s"), "{err}");
    }

    #[test]
    fn a_report_reaches_the_mailer_only_when_email_is_enabled() {
        let mailer = RecordingMailer::accepting();
        let mut config = mail_config();
        config.email.enabled = false;
        let err = send_email_report_with_kind("x", &config, "Scrub", &mailer).unwrap_err();
        assert_eq!(err.to_string(), "Email is not enabled in config");
        assert!(mailer.sent().is_empty());

        let config = mail_config();
        send_email_report_with_kind("Status: FAILURE\n", &config, "Scrub", &mailer).unwrap();
        send_email_report("fine\n", &config, &mailer).unwrap();
        // The message is made in the mailer's context, not this machine's:
        // the test holds whatever DAS_REPORT_TO / DAS_REPORT_FROM say.
        assert_eq!(
            mailer.sent(),
            [
                message(
                    "[DAS Scrub] nas.example.org — FAILURE — 2026-11-01 01:02",
                    "DAS Scrub (nas) <das@example.org>",
                    "ops@example.org",
                    "smtp://127.0.0.1:1",
                    "Status: FAILURE\n",
                ),
                message(
                    "[DAS Backup] nas.example.org — SUCCESS — 2026-11-01 01:02",
                    "DAS Backup (nas) <das@example.org>",
                    "ops@example.org",
                    "smtp://127.0.0.1:1",
                    "fine\n",
                ),
            ]
        );

        // What the mailer answers is what the caller hears; a message that
        // cannot be made is never handed over.
        let refusing = RecordingMailer::refusing("relay down");
        let err = send_email_report("x", &config, &refusing).unwrap_err();
        assert_eq!(err.to_string(), "relay down");
        let mut nobody = mail_config();
        nobody.email.to = String::new();
        assert!(send_email_report("x", &nobody, &mailer).is_err());
        assert_eq!(mailer.sent().len(), 2, "no recipient: nothing handed over");
    }
}
