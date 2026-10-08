//! D-Bus helper daemon for the DAS Backup Manager.
//!
//! Provides a system D-Bus service at `org.dasbackup.Helper1` that the KDE
//! Plasma GUI (and other unprivileged clients) can call to perform privileged
//! backup operations.  Polkit authorization is checked before each method
//! invocation.
//!
//! Build: `cargo build --release --features dbus`
//! Run:   activated on-demand by D-Bus (see `org.dasbackup.Helper1.service`)

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

use zbus::connection::Builder;
use zbus::fdo;
use zbus::object_server::SignalEmitter;
use zbus::{Connection, interface};

use buttered_dasd::backup::{self, BackupMode, BackupOptions};
use buttered_dasd::btrbk_conf;
use buttered_dasd::config::Config;
use buttered_dasd::db::Database;
use buttered_dasd::health;
use buttered_dasd::indexer;
use buttered_dasd::maintenance::{self, LockSite};
use buttered_dasd::mount;
use buttered_dasd::progress::{
    self, LogLevel, OrderedProgress, ProgressCallback, ProgressEvent, ProgressSink,
};
use buttered_dasd::recovery_os::{self, panel, session};
use buttered_dasd::restore;
use buttered_dasd::schedule;
use buttered_dasd::subvol;

// ---------------------------------------------------------------------------
// Job tracking
// ---------------------------------------------------------------------------

/// Live jobs, keyed by id: `(task handle, the job's progress queue, owning
/// D-Bus sender, kind)`. The queue is also how a job is cancelled
/// (`OrderedProgress::cancel`). The kind (`"backup"`, `"index"`,
/// `"restore"`, [`SESSION_KIND`]) is how a second recovery-OS session is
/// refused while one runs (`session_busy`).
///
/// The sender is what makes `job_cancel` authorizable. Without it the map held
/// no notion of ownership at all, so a polkit check for
/// `org.dasbackup.backup` — a question about the CALLER, not about the JOB —
/// was the only gate, and any authorized caller could abort anyone's in-flight
/// backup or restore (bd DAS-Backup-Manager-h2s).
type JobEntry = (JoinHandle<()>, Arc<OrderedProgress>, String, &'static str);
type JobMap = Arc<Mutex<HashMap<String, JobEntry>>>;

/// Cache of IndexStats JSON keyed by DB path.  Cold COUNT(*) on a 13.7M-row
/// files table + 68M-row spans table is ~30-60s on HDD, which trips the
/// GUI's 25s D-Bus call timeout.  Stats only change when the indexer bumps
/// the DB mtime.
///
/// Strategy is **stale-while-revalidate**: index_stats always returns the
/// cached value if anything is cached, even if mtime no longer matches.  A
/// mtime mismatch triggers a background refresh that updates the cache for
/// the next call.  This way the GUI never blocks on a cold COUNT — at worst
/// it sees stats one indexer run out of date for a few seconds.
///
/// `in_flight` deduplicates concurrent background refreshes so we don't
/// run multiple expensive computes against the same DB path.
///
/// See DAS-Backup-Manager-aem.
#[derive(Clone)]
struct StatsCacheEntry {
    db_mtime_nanos: i128,
    db_size_bytes: u64,
    json: String,
}
type StatsCache = Arc<Mutex<HashMap<String, StatsCacheEntry>>>;
type StatsRefreshSet = Arc<Mutex<std::collections::HashSet<String>>>;

/// Generate a unique job ID.
fn new_job_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before UNIX epoch")
        .as_nanos();
    format!("job-{ts}")
}

// ---------------------------------------------------------------------------
// D-Bus progress bridge
// ---------------------------------------------------------------------------

/// Delivers a job's progress events as D-Bus signals. Only the emission is
/// here: the ordering — one queue per job, drained by one thread, ending with
/// exactly one `JobFinished` — is `progress::OrderedProgress`, in the library
/// where it is tested (bd DAS-Backup-Manager-6bp).
struct DbusSink {
    conn: Connection,
    job_id: String,
    runtime: tokio::runtime::Handle,
}

impl ProgressSink for DbusSink {
    /// Every log line, cancelled job or not: stderr is the journal, the
    /// record of what the work did after its client stopped listening.
    fn journal(&mut self, level: LogLevel, message: &str) {
        eprintln!("[{}] {message}", level.word());
    }

    fn emit(&mut self, event: ProgressEvent) {
        let conn = &self.conn;
        let job_id = self.job_id.as_str();
        let sent = self.runtime.block_on(async move {
            let iface = conn
                .object_server()
                .interface::<_, HelperInterface>("/org/dasbackup/Helper1")
                .await?;
            let ctxt = iface.signal_emitter();
            match event {
                ProgressEvent::Stage { stage, .. } => {
                    HelperInterface::job_progress(ctxt, job_id, &stage, 0, "").await
                }
                ProgressEvent::Progress {
                    current,
                    total,
                    message,
                } => {
                    let percent = ProgressEvent::percent(current, total);
                    HelperInterface::job_progress(ctxt, job_id, "progress", percent, &message).await
                }
                ProgressEvent::Log { level, message } => {
                    HelperInterface::job_log(ctxt, job_id, level.word(), &message).await
                }
                ProgressEvent::Finished { success, summary } => {
                    HelperInterface::job_finished(ctxt, job_id, success, &summary).await
                }
            }
        });
        if let Err(e) = sent {
            eprintln!("btrdasd-helper: job {job_id}: signal not sent: {e}");
        }
    }
}

/// A job's progress: its events reach the GUI in order, through one queue.
fn job_progress(conn: &Connection, job_id: &str) -> Arc<OrderedProgress> {
    Arc::new(OrderedProgress::new(DbusSink {
        conn: conn.clone(),
        job_id: job_id.to_owned(),
        runtime: tokio::runtime::Handle::current(),
    }))
}

/// End a job: `JobFinished` goes out once, after every line the job logged.
async fn finish_job(progress: Arc<OrderedProgress>, success: bool, summary: String) {
    // `finish` waits for the queue to drain; keep that off the async workers.
    if let Err(e) = tokio::task::spawn_blocking(move || progress.finish(success, &summary)).await {
        eprintln!("btrdasd-helper: finishing a job panicked: {e}");
    }
}

// ---------------------------------------------------------------------------
// Polkit authorization
// ---------------------------------------------------------------------------

/// The steps dictionary as (key, value) pairs; a value that is not a
/// boolean becomes `None`, which `RunSteps::from_entries` refuses.
fn step_entries(map: HashMap<String, zbus::zvariant::OwnedValue>) -> Vec<(String, Option<bool>)> {
    map.into_iter()
        .map(|(key, value)| (key, bool::try_from(&value).ok()))
        .collect()
}

/// `mode` as the GUI sends it. Anything else is refused: an unknown mode
/// read as incremental would be a setting accepted and ignored.
fn parse_mode(mode: &str) -> Result<BackupMode, String> {
    match mode.to_lowercase().as_str() {
        "full" => Ok(BackupMode::Full),
        "incremental" => Ok(BackupMode::Incremental),
        other => Err(format!(
            "unknown backup mode {other:?} — refused (full or incremental)"
        )),
    }
}

/// Check Polkit authorization for the caller of a D-Bus method.
///
/// Calls `org.freedesktop.PolicyKit1.Authority.CheckAuthorization` with the
/// caller's bus name as the subject.  Returns `Ok(())` if authorized, or an
/// `fdo::Error::AccessDenied` otherwise.
async fn check_polkit(conn: &Connection, sender: &str, action_id: &str) -> Result<(), fdo::Error> {
    // Subject: ("system-bus-name", { "name" => sender })
    let subject_kind = "system-bus-name";
    let subject_details: HashMap<&str, zbus::zvariant::Value<'_>> =
        HashMap::from([("name", zbus::zvariant::Value::from(sender))]);

    // Empty details dict for the action.
    let details: HashMap<&str, &str> = HashMap::new();

    // flags = 1 -> AllowUserInteraction (show polkit dialog if needed)
    let flags: u32 = 1;
    // cancellation_id: empty string (no cancellation support)
    let cancel_id = "";

    let reply = conn
        .call_method(
            Some("org.freedesktop.PolicyKit1"),
            "/org/freedesktop/PolicyKit1/Authority",
            Some("org.freedesktop.PolicyKit1.Authority"),
            "CheckAuthorization",
            &(
                (subject_kind, subject_details),
                action_id,
                details,
                flags,
                cancel_id,
            ),
        )
        .await
        .map_err(|e| fdo::Error::Failed(format!("Polkit CheckAuthorization call failed: {e}")))?;

    // The reply body is (is_authorized: bool, is_challenge: bool, details: dict).
    let body = reply.body();
    let (is_authorized, _is_challenge, _details): (bool, bool, HashMap<String, String>) = body
        .deserialize()
        .map_err(|e| fdo::Error::Failed(format!("Cannot parse polkit reply: {e}")))?;

    if is_authorized {
        Ok(())
    } else {
        Err(fdo::Error::AccessDenied(format!(
            "Polkit denied action '{action_id}' for caller '{sender}'"
        )))
    }
}

// ---------------------------------------------------------------------------
// Helper: load/save config with error mapping
// ---------------------------------------------------------------------------

/// The one configuration file this daemon will read or write.
///
/// Every `#[interface]` method used to take a `config_path: &str` from the
/// caller and pass it straight to `Config::load`/`save` as root. Polkit
/// authorizes the ACTION (`org.dasbackup.config`), never the PATH, so
/// `org.dasbackup.config.read` — which the installed policy grants to any
/// active session with no prompt, so the GUI can list sources on startup —
/// doubled as a root-privileged read of any TOML-parseable file on the system,
/// and the mutating actions doubled as a root-privileged overwrite
/// (bd DAS-Backup-Manager-wd7).
///
/// The parameter was never load-bearing: the only client, the Plasma GUI,
/// hardcoded this exact string at both of its call sites.
const CANONICAL_CONFIG: &str = "/etc/das-backup/config.toml";

fn load_config() -> Result<Config, fdo::Error> {
    Config::load(Path::new(CANONICAL_CONFIG))
        .map_err(|e| fdo::Error::Failed(format!("Failed to load config '{CANONICAL_CONFIG}': {e}")))
}

/// Save the config and regenerate `btrbk.conf` from it, or change neither —
/// the same function the CLI's `subvol` commands use. Saving `config.toml`
/// alone left an added entry out of `btrbk.conf`, so it was not backed up.
fn save_config(config: &Config) -> Result<(), fdo::Error> {
    btrbk_conf::save_config_and_btrbk_conf(config, Path::new(CANONICAL_CONFIG))
        .map_err(|e| fdo::Error::Failed(format!("Failed to save config '{CANONICAL_CONFIG}': {e}")))
}

/// Run a read-modify-write of `config.toml`/`btrbk.conf` under the backup
/// singleton (`/run/das-backup.lock`), as setup and every backup run do, so a
/// GUI edit cannot interleave with them and be lost or lose theirs (bd
/// DAS-Backup-Manager-lxw). Never waits: while a backup or setup holds it the
/// call is refused with a message the GUI shows as it is, and nothing is
/// written. Every method that saves the config goes through here.
fn edit_config<T>(edit: impl FnOnce() -> fdo::Result<T>) -> fdo::Result<T> {
    edit_config_at(Path::new(backup::BACKUP_LOCK_PATH), edit)
}

fn edit_config_at<T>(lock: &Path, edit: impl FnOnce() -> fdo::Result<T>) -> fdo::Result<T> {
    match btrbk_conf::edit_config_under_backup_lock(lock, edit) {
        Ok(btrbk_conf::ConfigEdit::Done(result)) => result,
        Ok(btrbk_conf::ConfigEdit::Busy(why)) => Err(fdo::Error::Failed(why)),
        Err(e) => Err(fdo::Error::Failed(format!(
            "Not saved, nothing changed: cannot take {}: {e}",
            lock.display()
        ))),
    }
}

/// The one index database this daemon will open.
///
/// Every `Index*` method used to take the database path from the caller and
/// hand it to `Database::open` as root. That is not a read: `Connection::open`
/// creates the file when absent, `journal_mode=wal` creates `-wal`/`-shm`
/// sidecars beside it, and `execute_batch(SCHEMA_SQL)` + `migrate()` then write
/// into it. So the six read methods were a root file-create-and-write at a
/// caller-chosen path, and pointed at an existing SQLite database anywhere on
/// the host they would open and MIGRATE it.
///
/// Polkit authorizes the ACTION, never the PATH — and `org.dasbackup.index.read`
/// is `allow_active=yes`, so this needed no authentication prompt at all
/// (bd DAS-Backup-Manager-gko). Same defect as the caller-supplied config path
/// fixed in bd DAS-Backup-Manager-wd7, one indirection further out.
///
/// Resolved from the canonical config rather than a constant so an
/// administrator can still relocate the index — by editing a root-owned file,
/// which is a privilege they already hold.
fn canonical_db_path() -> Result<String, fdo::Error> {
    Ok(load_config()?.general.db_path)
}

// ---------------------------------------------------------------------------
// Recovery-OS sessions (bd DAS-Backup-Manager-8249 stage 2)
// ---------------------------------------------------------------------------

/// The `JobMap` kind of a `RecoveryOsSession` job.
const SESSION_KIND: &str = "recovery-os-session";

/// The scheduled-session services a session must not overlap: one per
/// `role = "mirror"` target, and the one that runs both.
fn update_unit_names(cfg: &Config) -> Vec<String> {
    cfg.targets
        .iter()
        .filter(|t| t.role == buttered_dasd::config::TargetRole::Mirror)
        .map(|t| format!("das-recovery-os-update-{}.service", t.label))
        .chain(std::iter::once(
            "das-recovery-os-update-both.service".to_string(),
        ))
        .collect()
}

/// Why a new session must not start, naming what already runs: a session
/// job (`jobs` are `(id, kind)`), or a scheduled-session service whose
/// `ActiveState` (`units` are `(unit, state)`) is anything but `inactive` or
/// `failed`. `None`: nothing in the way.
fn session_busy(jobs: &[(&str, &str)], units: &[(&str, &str)]) -> Option<String> {
    if let Some((id, _)) = jobs.iter().find(|(_, kind)| *kind == SESSION_KIND) {
        return Some(format!(
            "a recovery-OS session is already running as job {id}"
        ));
    }
    units
        .iter()
        .find(|(_, state)| !matches!(*state, "inactive" | "failed"))
        .map(|(unit, state)| {
            format!("a scheduled recovery-OS session is running: {unit} ({state})")
        })
}

/// Each unit's `ActiveState`, from `systemctl show -P ActiveState`. A
/// systemctl that cannot answer is an error, never "inactive": a session
/// started over a running scheduled one would contend for its drive.
fn read_active_states(units: &[String]) -> Result<Vec<(String, String)>, String> {
    units
        .iter()
        .map(|unit| {
            let out = std::process::Command::new("systemctl")
                .args(["show", "-P", "ActiveState", unit])
                .env("LC_ALL", "C")
                .output()
                .map_err(|e| format!("Cannot ask systemctl for {unit}'s state: {e}"))?;
            let state = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !out.status.success() || state.is_empty() {
                return Err(format!(
                    "Cannot read {unit}'s state from systemctl ({}): {}",
                    out.status,
                    String::from_utf8_lossy(&out.stderr).trim()
                ));
            }
            Ok((unit.clone(), state))
        })
        .collect()
}

/// What a script run that failed said: its stderr, else its stdout, else
/// its exit status.
fn script_failure(out: &std::process::Output) -> String {
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
    if !stderr.is_empty() {
        return stderr;
    }
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if !stdout.is_empty() {
        return stdout;
    }
    format!("{} ended with {}", session::SCRIPT, out.status)
}

/// `recovery-os-vm.sh clean-runs <label>`: its stdout is the count and
/// nothing else. Anything else is an error, never 0.
fn clean_runs_from(out: &std::process::Output) -> Result<u32, String> {
    if !out.status.success() {
        return Err(script_failure(out));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    text.trim()
        .parse::<u32>()
        .map_err(|e| format!("The clean-run count {:?} is not a number: {e}", text.trim()))
}

/// `recovery-os-vm.sh history <label>`: one JSON object per line. A line
/// that does not parse fails the whole read, never a shorter list.
fn history_from(out: &std::process::Output) -> Result<Vec<serde_json::Value>, String> {
    if !out.status.success() {
        return Err(script_failure(out));
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .enumerate()
        .map(|(n, line)| {
            serde_json::from_str(line)
                .map_err(|e| format!("History line {} is not JSON: {e}", n + 1))
        })
        .collect()
}

/// `recovery-os-vm.sh console-socket <label> <uid>`: on success its stdout
/// is exactly one absolute path; on failure its refusal is the error.
fn console_path_from(out: &std::process::Output) -> Result<String, String> {
    if !out.status.success() {
        return Err(script_failure(out));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<&str> = text.lines().collect();
    match lines.as_slice() {
        [path] if path.starts_with('/') => Ok((*path).to_string()),
        _ => Err(format!(
            "The console socket was not named: the script printed {:?}",
            text.trim()
        )),
    }
}

/// The last `n` lines of `text`, in order, joined by `\n`.
fn last_lines(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

/// `recovery-os-vm.sh <args>` with stdout and stderr separate.
fn run_script(script: &Path, args: &[&str]) -> Result<std::process::Output, String> {
    let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
    session::script_command(script, &args)
        .output()
        .map_err(|e| format!("Cannot run {}: {e}", script.display()))
}

/// `recovery-os-vm.sh <args>` with stdout and stderr on one pipe, so its
/// lines keep the order the script wrote them in.
fn run_script_merged(
    script: &Path,
    args: &[String],
) -> Result<(std::process::ExitStatus, String), String> {
    use std::io::Read;
    let cannot = |e: std::io::Error| format!("Cannot run {}: {e}", script.display());
    let (mut reader, writer) = std::io::pipe().map_err(cannot)?;
    let mut child = {
        // The command holds both write ends; it goes out of scope here so
        // the read below sees end-of-file when the script exits.
        let mut cmd = session::script_command(script, args);
        cmd.stdout(writer.try_clone().map_err(cannot)?)
            .stderr(writer);
        cmd.spawn().map_err(cannot)?
    };
    let mut buf = Vec::new();
    let read = reader.read_to_end(&mut buf);
    let status = child.wait().map_err(cannot)?;
    read.map_err(cannot)?;
    Ok((status, String::from_utf8_lossy(&buf).into_owned()))
}

/// The panel's system reads: the installed script, `virsh`, the
/// maintenance lock.
struct SystemPanelReads {
    script: PathBuf,
}

impl panel::PanelReads for SystemPanelReads {
    fn clean_runs(&self, label: &str) -> Result<u32, String> {
        clean_runs_from(&run_script(&self.script, &["clean-runs", label])?)
    }

    fn history(&self, label: &str) -> Result<Vec<serde_json::Value>, String> {
        history_from(&run_script(&self.script, &["history", label])?)
    }

    fn unit(&self, _name: &str) -> Result<panel::UnitFacts, String> {
        // Scheduled sessions arrive with RecoveryOsScheduleSet (Task 6); until
        // then there is nothing to read, and no fabricated facts either.
        Err("not read until schedules exist".into())
    }

    fn domain_state(&self, label: &str) -> Option<String> {
        let out = std::process::Command::new("virsh")
            .args(["domstate", &format!("recovery-os-updater-{label}")])
            .env("LC_ALL", "C")
            .output()
            .ok()?;
        let state = String::from_utf8_lossy(&out.stdout).trim().to_string();
        (out.status.success() && !state.is_empty()).then_some(state)
    }

    fn lock_holder(&self) -> Option<String> {
        let path = Path::new(buttered_dasd::scrub::MAINTENANCE_LOCK_PATH);
        match buttered_dasd::scrub::FileLock::try_acquire(path) {
            // Free: the probe's own hold ends as it drops.
            Ok(Some(_probe)) => None,
            Ok(None) => Some(maintenance::holder_of(path)),
            // Never "free" on a lock that could not be checked.
            Err(e) => Some(format!(
                "unknown: the maintenance lock cannot be checked: {e}"
            )),
        }
    }
}

// ---------------------------------------------------------------------------
// D-Bus interface
// ---------------------------------------------------------------------------

const HELPER_PATH: &str = "/org/dasbackup/Helper1";
const HELPER_NAME: &str = "org.dasbackup.Helper1";

struct HelperInterface {
    jobs: JobMap,
    conn: Connection,
    stats_cache: StatsCache,
    stats_refresh_in_flight: StatsRefreshSet,
}

#[interface(name = "org.dasbackup.Helper1")]
impl HelperInterface {
    // ---- Signals ----

    #[zbus(signal)]
    async fn job_progress(
        emitter: &SignalEmitter<'_>,
        job_id: &str,
        stage: &str,
        percent: i32,
        message: &str,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn job_log(
        emitter: &SignalEmitter<'_>,
        job_id: &str,
        level: &str,
        message: &str,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn job_finished(
        emitter: &SignalEmitter<'_>,
        job_id: &str,
        success: bool,
        summary: &str,
    ) -> zbus::Result<()>;

    // ---- Async (job-returning) methods ----

    /// Run a backup job with the operations the GUI ticked (bd c4x).
    async fn backup_run(
        &self,
        #[zbus(header)] header: zbus::message::Header<'_>,
        mode: &str,
        sources: Vec<String>,
        targets: Vec<String>,
        dry_run: bool,
        steps: HashMap<String, zbus::zvariant::OwnedValue>,
    ) -> fdo::Result<String> {
        let sender = sender_from_header(&header)?;
        check_polkit(&self.conn, &sender, "org.dasbackup.backup").await?;

        let mode = parse_mode(mode).map_err(fdo::Error::InvalidArgs)?;
        let steps =
            backup::RunSteps::from_entries(step_entries(steps)).map_err(fdo::Error::InvalidArgs)?;
        let config = load_config()?;
        let mut options = BackupOptions {
            mode: Some(mode),
            // An `as` argument cannot say "not specified", and the GUI always
            // lists its ticks: an empty list is a selection of nothing and
            // the job refuses it (backup::empty_selection) — never "all".
            sources: Some(sources),
            targets: Some(targets),
            dry_run,
            ..Default::default()
        };
        // Snapshot, send, boot archive, index and email: exactly the ticks.
        steps.apply(&mut options);

        let job_id = new_job_id();
        let progress = job_progress(&self.conn, &job_id);
        let cancel = progress.clone();
        let finisher = progress.clone();
        let jobs = self.jobs.clone();
        let jid = job_id.clone();

        let handle = tokio::spawn(async move {
            // The same job the CLI runs (`backup::run_backup_job`): the
            // two-lock interlock (bd DAS-Backup-Manager-pe6, -dca), subvolume
            // sync (its section reaches the GUI through the job log), mounts,
            // btrbk, unmount — a target left mounted fails the run
            // (bd DAS-Backup-Manager-5oc) — report and record.
            let (success, summary) = tokio::task::spawn_blocking(move || {
                backup::run_backup_job(
                    &backup::SystemBackupHost::new(
                        Path::new(CANONICAL_CONFIG),
                        "btrdasd-helper BackupRun job",
                    ),
                    config,
                    options,
                    &*progress,
                )
                .finish_line(dry_run)
            })
            .await
            .unwrap_or_else(|e| (false, format!("Backup task panicked: {e}")));

            finish_job(finisher, success, summary).await;
            jobs.lock().await.remove(&jid);
        });

        self.jobs
            .lock()
            .await
            .insert(job_id.clone(), (handle, cancel, sender.clone(), "backup"));
        Ok(job_id)
    }

    /// Walk backup targets and index new snapshots.
    ///
    /// If `target_path` is empty, walks ALL mounted config targets.
    /// Otherwise walks just the specified path (backwards compat).
    async fn index_walk(
        &self,
        #[zbus(header)] header: zbus::message::Header<'_>,
        target_path: &str,
    ) -> fdo::Result<String> {
        let sender = sender_from_header(&header)?;
        check_polkit(&self.conn, &sender, "org.dasbackup.index").await?;

        let config = load_config()?;
        let job_id = new_job_id();
        let progress = job_progress(&self.conn, &job_id);
        let cancel = progress.clone();
        let finisher = progress.clone();
        let jobs = self.jobs.clone();
        let jid = job_id.clone();
        let target_path = target_path.to_owned();

        let handle = tokio::spawn(async move {
            let result = tokio::task::spawn_blocking(move || {
                index_walk_job(&config, &target_path, &LockSite::production(), &progress)
            })
            .await
            .unwrap_or_else(|e| Err(format!("Indexing task panicked: {e}")));

            let (success, summary) = match result {
                Ok(msg) => (true, msg),
                Err(msg) => (false, msg),
            };

            finish_job(finisher, success, summary).await;
            jobs.lock().await.remove(&jid);
        });

        self.jobs
            .lock()
            .await
            .insert(job_id.clone(), (handle, cancel, sender.clone(), "index"));
        Ok(job_id)
    }

    // ---- Index read methods (synchronous, polkit: org.dasbackup.index.read) ----

    /// Return JSON stats: {snapshots, files, spans, db_size_bytes}.
    ///
    /// **Stale-while-revalidate**: returns the cached value immediately if
    /// anything is cached for this DB path, even when the DB file's mtime
    /// has changed since the cache was populated.  A mtime mismatch fires
    /// a background refresh that updates the cache for the next call.
    /// This keeps the GUI's Health Dashboard responsive after a backup
    /// run (which bumps DB mtime via the indexer), at the cost of one
    /// indexer-run's worth of staleness for a few seconds.
    ///
    /// Concurrent refreshes are deduplicated by stats_refresh_in_flight.
    ///
    /// See DAS-Backup-Manager-aem.
    async fn index_stats(
        &self,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> fdo::Result<String> {
        let sender = sender_from_header(&header)?;
        check_polkit(&self.conn, &sender, "org.dasbackup.index.read").await?;
        let db_path = canonical_db_path()?;

        // Read the file's current mtime/size cheaply on a blocking thread.
        let probe_path = db_path.clone();
        let (current_mtime, current_size) = tokio::task::spawn_blocking(move || {
            std::fs::metadata(&probe_path)
                .ok()
                .map(|m| {
                    let mtime_nanos = m
                        .modified()
                        .ok()
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| d.as_nanos() as i128)
                        .unwrap_or(0);
                    (mtime_nanos, m.len())
                })
                .unwrap_or((0, 0))
        })
        .await
        .map_err(|e| fdo::Error::Failed(format!("Stat probe failed: {e}")))?;

        // Examine the cache.  Three branches:
        //   1. Cache hit, mtime/size match    -> return cached, no refresh.
        //   2. Cache hit, mtime/size differ   -> return STALE cached + fire
        //                                        background refresh.
        //   3. Cache miss                     -> slow path (compute now).
        let cached_entry = self.stats_cache.lock().await.get(&db_path).cloned();
        if let Some(entry) = cached_entry {
            if entry.db_mtime_nanos != current_mtime || entry.db_size_bytes != current_size {
                // Stale — schedule background refresh.
                self.spawn_stats_refresh(db_path.clone());
            }
            return Ok(entry.json);
        }

        // Cache miss: run the slow compute synchronously this one time so
        // the caller gets a real answer.  Subsequent callers will hit the
        // cache.  If another caller is already computing for this path,
        // wait briefly and return their result.
        let cache = self.stats_cache.clone();
        let in_flight = self.stats_refresh_in_flight.clone();
        tokio::task::spawn_blocking(move || -> fdo::Result<String> {
            // Mark in-flight; on Drop the guard auto-removes.
            struct InFlightGuard {
                set: StatsRefreshSet,
                key: String,
            }
            impl Drop for InFlightGuard {
                fn drop(&mut self) {
                    self.set.blocking_lock().remove(&self.key);
                }
            }
            let already = !in_flight.blocking_lock().insert(db_path.clone());
            let _guard = InFlightGuard {
                set: in_flight.clone(),
                key: db_path.clone(),
            };
            if already {
                // Another task is computing.  Spin briefly waiting for
                // them to populate the cache, then return.  Bounded to
                // 20 s so we never exceed the GUI's 25 s deadline.
                for _ in 0..400 {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                    if let Some(entry) = cache.blocking_lock().get(&db_path).cloned() {
                        return Ok(entry.json);
                    }
                }
            }

            let db = Database::open(&db_path)
                .map_err(|e| fdo::Error::Failed(format!("DB open failed: {e}")))?;
            let stats = db
                .get_stats()
                .map_err(|e| fdo::Error::Failed(format!("Stats query failed: {e}")))?;
            let meta = std::fs::metadata(&db_path)
                .map_err(|e| fdo::Error::Failed(format!("DB stat failed: {e}")))?;
            let db_size_bytes = meta.len();
            let mtime_nanos = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_nanos() as i128)
                .unwrap_or(0);
            let json = serde_json::json!({
                "snapshots": stats.snapshot_count,
                "files": stats.file_count,
                "spans": stats.span_count,
                "db_size_bytes": db_size_bytes,
            })
            .to_string();
            cache.blocking_lock().insert(
                db_path.clone(),
                StatsCacheEntry {
                    db_mtime_nanos: mtime_nanos,
                    db_size_bytes,
                    json: json.clone(),
                },
            );
            Ok(json)
        })
        .await
        .map_err(|e| fdo::Error::Failed(format!("Stats task join failed: {e}")))?
    }

    /// Return JSON array of all snapshots.
    async fn index_list_snapshots(
        &self,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> fdo::Result<String> {
        let sender = sender_from_header(&header)?;
        check_polkit(&self.conn, &sender, "org.dasbackup.index.read").await?;
        let db_path = canonical_db_path()?;
        tokio::task::spawn_blocking(move || -> fdo::Result<String> {
            let db = Database::open(&db_path)
                .map_err(|e| fdo::Error::Failed(format!("DB open failed: {e}")))?;
            let snapshots = db
                .list_snapshots()
                .map_err(|e| fdo::Error::Failed(format!("List snapshots failed: {e}")))?;
            let arr: Vec<serde_json::Value> = snapshots
                .iter()
                .map(|s| {
                    serde_json::json!({
                        "id": s.id,
                        "name": s.name,
                        "ts": s.ts,
                        "source": s.source,
                        "path": s.path,
                        "indexed_at": s.indexed_at,
                    })
                })
                .collect();
            Ok(serde_json::to_string(&arr).unwrap_or_else(|_| "[]".to_string()))
        })
        .await
        .map_err(|e| fdo::Error::Failed(format!("List snapshots task join failed: {e}")))?
    }

    /// Return paginated JSON of files in a given snapshot.
    ///
    /// Returns a JSON object: `{"files": [...], "total": N, "limit": L, "offset": O}`
    /// Use limit=0 to return all files (not recommended for large snapshots).
    async fn index_list_files(
        &self,
        #[zbus(header)] header: zbus::message::Header<'_>,
        snapshot_id: i64,
        limit: i64,
        offset: i64,
    ) -> fdo::Result<String> {
        let sender = sender_from_header(&header)?;
        check_polkit(&self.conn, &sender, "org.dasbackup.index.read").await?;
        let db_path = canonical_db_path()?;
        tokio::task::spawn_blocking(move || -> fdo::Result<String> {
            let db = Database::open(&db_path)
                .map_err(|e| fdo::Error::Failed(format!("DB open failed: {e}")))?;

            let total = db
                .count_files_in_snapshot(snapshot_id)
                .map_err(|e| fdo::Error::Failed(format!("Count files failed: {e}")))?;

            // Default to 10000 if limit is 0 or negative (prevents giant responses)
            let effective_limit = if limit <= 0 { 10_000 } else { limit };

            let files = db
                .get_files_in_snapshot_paged(snapshot_id, effective_limit, offset)
                .map_err(|e| fdo::Error::Failed(format!("List files failed: {e}")))?;
            let arr: Vec<serde_json::Value> = files
                .iter()
                .map(|f| {
                    serde_json::json!({
                        "id": f.id,
                        "path": f.path,
                        "name": f.name,
                        "size": f.size,
                        "mtime": f.mtime,
                        "type": f.file_type,
                    })
                })
                .collect();

            let result = serde_json::json!({
                "files": arr,
                "total": total,
                "limit": effective_limit,
                "offset": offset,
            });
            Ok(serde_json::to_string(&result)
                .unwrap_or_else(|_| r#"{"files":[],"total":0,"limit":0,"offset":0}"#.to_string()))
        })
        .await
        .map_err(|e| fdo::Error::Failed(format!("List files task join failed: {e}")))?
    }

    /// FTS5 search returning JSON array of matches.
    async fn index_search(
        &self,
        #[zbus(header)] header: zbus::message::Header<'_>,
        query: &str,
        limit: i64,
    ) -> fdo::Result<String> {
        let sender = sender_from_header(&header)?;
        check_polkit(&self.conn, &sender, "org.dasbackup.index.read").await?;
        let db_path = canonical_db_path()?;
        let query = query.to_owned();
        tokio::task::spawn_blocking(move || -> fdo::Result<String> {
            let db = Database::open(&db_path)
                .map_err(|e| fdo::Error::Failed(format!("DB open failed: {e}")))?;
            let results = db
                .search(&query, limit)
                .map_err(|e| fdo::Error::Failed(format!("Search failed: {e}")))?;
            let arr: Vec<serde_json::Value> = results
                .iter()
                .map(|r| {
                    serde_json::json!({
                        "path": r.path,
                        "name": r.name,
                        "size": r.size,
                        "mtime": r.mtime,
                        "first_snap": r.first_snap,
                        "last_snap": r.last_snap,
                    })
                })
                .collect();
            Ok(serde_json::to_string(&arr).unwrap_or_else(|_| "[]".to_string()))
        })
        .await
        .map_err(|e| fdo::Error::Failed(format!("Search task join failed: {e}")))?
    }

    /// Return JSON array of recent backup history.
    async fn index_backup_history(
        &self,
        #[zbus(header)] header: zbus::message::Header<'_>,
        limit: i64,
    ) -> fdo::Result<String> {
        let sender = sender_from_header(&header)?;
        check_polkit(&self.conn, &sender, "org.dasbackup.index.read").await?;
        let db_path = canonical_db_path()?;
        tokio::task::spawn_blocking(move || -> fdo::Result<String> {
            let db = Database::open(&db_path)
                .map_err(|e| fdo::Error::Failed(format!("DB open failed: {e}")))?;
            let runs = db
                .get_backup_history(limit as usize)
                .map_err(|e| fdo::Error::Failed(format!("History query failed: {e}")))?;
            Ok(buttered_dasd::report::backup_history_json(&runs).to_string())
        })
        .await
        .map_err(|e| fdo::Error::Failed(format!("History task join failed: {e}")))?
    }

    /// Return the filesystem path for a snapshot by ID.
    async fn index_snapshot_path(
        &self,
        #[zbus(header)] header: zbus::message::Header<'_>,
        snapshot_id: i64,
    ) -> fdo::Result<String> {
        let sender = sender_from_header(&header)?;
        check_polkit(&self.conn, &sender, "org.dasbackup.index.read").await?;
        let db_path = canonical_db_path()?;
        tokio::task::spawn_blocking(move || -> fdo::Result<String> {
            let db = Database::open(&db_path)
                .map_err(|e| fdo::Error::Failed(format!("DB open failed: {e}")))?;
            let path = db
                .snapshot_path_by_id(snapshot_id)
                .map_err(|e| fdo::Error::Failed(format!("Path query failed: {e}")))?
                .ok_or_else(|| fdo::Error::Failed(format!("No snapshot with id {snapshot_id}")))?;
            Ok(path)
        })
        .await
        .map_err(|e| fdo::Error::Failed(format!("Snapshot path task join failed: {e}")))?
    }

    /// Restore specific files from a snapshot.
    async fn restore_files(
        &self,
        #[zbus(header)] header: zbus::message::Header<'_>,
        snapshot: &str,
        dest: &str,
        files: Vec<String>,
    ) -> fdo::Result<String> {
        let sender = sender_from_header(&header)?;
        check_polkit(&self.conn, &sender, "org.dasbackup.restore").await?;

        let config = load_config()?;
        let job_id = new_job_id();
        let progress = job_progress(&self.conn, &job_id);
        let cancel = progress.clone();
        let finisher = progress.clone();
        let jobs = self.jobs.clone();
        let jid = job_id.clone();
        let snapshot = snapshot.to_owned();
        let dest = dest.to_owned();

        let handle = tokio::spawn(async move {
            let result = tokio::task::spawn_blocking(move || {
                restore_files_job(
                    &config,
                    &snapshot,
                    &dest,
                    &files,
                    &LockSite::production(),
                    &progress,
                )
            })
            .await
            .unwrap_or_else(|e| Err(format!("Restore task panicked: {e}")));

            let (success, summary) = match result {
                Ok((s, msg)) => (s, msg),
                Err(msg) => (false, msg),
            };

            finish_job(finisher, success, summary).await;
            jobs.lock().await.remove(&jid);
        });

        self.jobs
            .lock()
            .await
            .insert(job_id.clone(), (handle, cancel, sender.clone(), "restore"));
        Ok(job_id)
    }

    /// Restore an entire snapshot to a destination.
    async fn restore_snapshot(
        &self,
        #[zbus(header)] header: zbus::message::Header<'_>,
        snapshot: &str,
        dest: &str,
    ) -> fdo::Result<String> {
        let sender = sender_from_header(&header)?;
        check_polkit(&self.conn, &sender, "org.dasbackup.restore").await?;

        let config = load_config()?;
        let job_id = new_job_id();
        let progress = job_progress(&self.conn, &job_id);
        let cancel = progress.clone();
        let finisher = progress.clone();
        let jobs = self.jobs.clone();
        let jid = job_id.clone();
        let snapshot = snapshot.to_owned();
        let dest = dest.to_owned();

        let handle = tokio::spawn(async move {
            let result = tokio::task::spawn_blocking(move || {
                restore_snapshot_job(
                    &config,
                    &snapshot,
                    &dest,
                    &LockSite::production(),
                    &progress,
                )
            })
            .await
            .unwrap_or_else(|e| Err(format!("Snapshot restore task panicked: {e}")));

            let (success, summary) = match result {
                Ok((s, msg)) => (s, msg),
                Err(msg) => (false, msg),
            };

            finish_job(finisher, success, summary).await;
            jobs.lock().await.remove(&jid);
        });

        self.jobs
            .lock()
            .await
            .insert(job_id.clone(), (handle, cancel, sender.clone(), "restore"));
        Ok(job_id)
    }

    // ---- Synchronous methods ----

    /// Get the raw TOML config as a string.
    /// Uses config.read polkit action (allow_active=yes) so the GUI can load
    /// sources/targets on startup without prompting for admin credentials.
    async fn config_get(
        &self,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> fdo::Result<String> {
        let sender = sender_from_header(&header)?;
        check_polkit(&self.conn, &sender, "org.dasbackup.config.read").await?;

        let config = load_config()?;
        config
            .to_toml()
            .map_err(|e| fdo::Error::Failed(format!("Failed to serialize config: {e}")))
    }

    /// Write a TOML config string to disk (validates first).
    async fn config_set(
        &self,
        #[zbus(header)] header: zbus::message::Header<'_>,
        toml_content: &str,
    ) -> fdo::Result<()> {
        let sender = sender_from_header(&header)?;
        check_polkit(&self.conn, &sender, "org.dasbackup.config").await?;

        let config = Config::from_toml(toml_content)
            .map_err(|e| fdo::Error::Failed(format!("Invalid TOML: {e}")))?;

        let errors = config.validate();
        if !errors.is_empty() {
            return Err(fdo::Error::Failed(format!(
                "Config validation failed: {}",
                errors.join("; ")
            )));
        }
        edit_config(|| {
            // Saving writes btrbk.conf as root at the path the config names, and
            // polkit authorizes the action, never the path (see CANONICAL_CONFIG).
            // A current config that does not load is compared with the default
            // path instead, so the GUI can repair it.
            btrbk_conf::refuse_moving_btrbk_conf_from(
                Config::load(Path::new(CANONICAL_CONFIG)),
                &config,
            )
            .map_err(fdo::Error::Failed)?;

            save_config(&config)
        })
    }

    /// Get the current backup schedule as JSON.
    /// Uses config.read polkit action (read-only, no admin auth for active sessions).
    async fn schedule_get(
        &self,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> fdo::Result<String> {
        let sender = sender_from_header(&header)?;
        check_polkit(&self.conn, &sender, "org.dasbackup.config.read").await?;

        let config = load_config()?;
        let info = schedule::get_schedule(&config)
            .map_err(|e| fdo::Error::Failed(format!("Failed to get schedule: {e}")))?;

        // Serialize to JSON manually since ScheduleInfo doesn't derive Serialize.
        let json = serde_json::json!({
            "incremental_time": info.incremental_time,
            "full_schedule": info.full_schedule,
            "delay_min": info.delay_min,
            "enabled": info.enabled,
            "next_incremental": info.next_incremental,
            "next_full": info.next_full,
        });

        Ok(json.to_string())
    }

    /// Set the backup schedule parameters.
    async fn schedule_set(
        &self,
        #[zbus(header)] header: zbus::message::Header<'_>,
        incremental: &str,
        full: &str,
        delay: u32,
    ) -> fdo::Result<()> {
        let sender = sender_from_header(&header)?;
        check_polkit(&self.conn, &sender, "org.dasbackup.config").await?;

        let inc = if incremental.is_empty() {
            None
        } else {
            Some(incremental)
        };
        let f = if full.is_empty() { None } else { Some(full) };
        let d = if delay == 0 { None } else { Some(delay) };

        edit_config(|| {
            let mut config = load_config()?;
            schedule::set_schedule(&mut config, inc, f, d)
                .map_err(|e| fdo::Error::Failed(format!("Failed to set schedule: {e}")))?;
            save_config(&config)
        })
    }

    /// Enable or disable scheduled backups.
    async fn schedule_enable(
        &self,
        #[zbus(header)] header: zbus::message::Header<'_>,
        enabled: bool,
    ) -> fdo::Result<()> {
        let sender = sender_from_header(&header)?;
        check_polkit(&self.conn, &sender, "org.dasbackup.config").await?;

        let config = load_config()?;
        schedule::set_enabled(&config, enabled)
            .map_err(|e| fdo::Error::Failed(format!("Failed to set schedule enabled: {e}")))
    }

    /// Add a subvolume to a source.
    async fn subvol_add(
        &self,
        #[zbus(header)] header: zbus::message::Header<'_>,
        source: &str,
        name: &str,
    ) -> fdo::Result<()> {
        let sender = sender_from_header(&header)?;
        check_polkit(&self.conn, &sender, "org.dasbackup.config").await?;

        edit_config(|| {
            let mut config = load_config()?;
            subvol::add_subvolume(&mut config, source, name, false).map_err(fdo::Error::Failed)?;
            save_config(&config)
        })
    }

    /// Remove a subvolume from a source.
    async fn subvol_remove(
        &self,
        #[zbus(header)] header: zbus::message::Header<'_>,
        source: &str,
        name: &str,
    ) -> fdo::Result<()> {
        let sender = sender_from_header(&header)?;
        check_polkit(&self.conn, &sender, "org.dasbackup.config").await?;

        edit_config(|| {
            let mut config = load_config()?;
            subvol::remove_subvolume(&mut config, source, name).map_err(fdo::Error::Failed)?;
            save_config(&config)
        })
    }

    /// Set the manual_only flag on a subvolume.
    async fn subvol_set_manual(
        &self,
        #[zbus(header)] header: zbus::message::Header<'_>,
        source: &str,
        name: &str,
        manual: bool,
    ) -> fdo::Result<()> {
        let sender = sender_from_header(&header)?;
        check_polkit(&self.conn, &sender, "org.dasbackup.config").await?;

        edit_config(|| {
            let mut config = load_config()?;
            subvol::set_manual(&mut config, source, name, manual).map_err(fdo::Error::Failed)?;
            save_config(&config)
        })
    }

    /// Query system health and return a JSON report.
    ///
    /// Auto-mounts targets first so disk space, SMART, and snapshot data are
    /// available, then unmounts any targets this call mounted.
    async fn health_query(
        &self,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> fdo::Result<String> {
        let sender = sender_from_header(&header)?;
        check_polkit(&self.conn, &sender, "org.dasbackup.health").await?;

        let config = load_config()?;

        // Run the entire health query (blocking I/O: smartctl, btrfs, mount)
        // inside spawn_blocking.  Do NOT auto-mount — the health report should
        // reflect the actual mount state so the user sees which targets are
        // available vs disconnected.
        let json_str = tokio::task::spawn_blocking(move || -> Result<String, String> {
            let report =
                health::get_health(&config).map_err(|e| format!("Health query failed: {e}"))?;

            let status_str = match report.status {
                health::HealthStatus::Healthy => "healthy",
                health::HealthStatus::Warning => "warning",
                health::HealthStatus::Critical => "critical",
            };

            let targets_json: Vec<serde_json::Value> = report
                .targets
                .iter()
                .map(|t| {
                    let scrub_status_str = match t.scrub.status {
                        health::ScrubHealthStatus::NotApplicable => "not_applicable",
                        health::ScrubHealthStatus::NeverScrubbed => "never_scrubbed",
                        health::ScrubHealthStatus::Unresolved => "unresolved",
                        health::ScrubHealthStatus::Ok => "ok",
                        health::ScrubHealthStatus::Warn => "warn",
                        health::ScrubHealthStatus::Fail => "fail",
                    };
                    serde_json::json!({
                        "label": t.label,
                        "serial": t.serial,
                        "mounted": t.mounted,
                        "total_bytes": t.total_bytes,
                        "used_bytes": t.used_bytes,
                        "usage_percent": t.usage_percent(),
                        "snapshot_count": t.snapshot_count,
                        "smart_status": t.smart_status,
                        "temperature_c": t.temperature_c,
                        "power_on_hours": t.power_on_hours,
                        "errors": t.errors,
                        "scrub": {
                            "status": scrub_status_str,
                            "age_days": t.scrub.age_days,
                            "last_outcome": t.scrub.last_outcome,
                            "last_ok": t.scrub.last_ok,
                            "error_total": t.scrub.error_total,
                            "last_success_epoch": t.scrub.last_success_epoch,
                        },
                    })
                })
                .collect();

            // Build total_bytes lookup per target label
            let target_totals: std::collections::HashMap<&str, u64> = report
                .targets
                .iter()
                .map(|t| (t.label.as_str(), t.total_bytes))
                .collect();

            // Build growth data grouped by target label
            let mut growth_map: std::collections::BTreeMap<String, Vec<serde_json::Value>> =
                std::collections::BTreeMap::new();
            for gp in &report.growth_points {
                let (y, m, d) = health::days_to_ymd(gp.timestamp / 86400);
                let date_str = format!("{y:04}-{m:02}-{d:02}");
                let total = target_totals
                    .get(gp.target_label.as_str())
                    .copied()
                    .unwrap_or(0);
                growth_map
                    .entry(gp.target_label.clone())
                    .or_default()
                    .push(serde_json::json!({
                        "date": date_str,
                        "used_bytes": gp.used_bytes,
                        "total_bytes": total,
                    }));
            }
            let growth_json: Vec<serde_json::Value> = growth_map
                .into_iter()
                .map(|(label, entries)| serde_json::json!({"label": label, "entries": entries}))
                .collect();

            // Service status
            let btrbk_available = std::process::Command::new("which")
                .arg("btrbk")
                .output()
                .is_ok_and(|o| o.status.success());
            let timer_output = std::process::Command::new("systemctl")
                .args([
                    "show",
                    "das-backup.timer",
                    "--property=ActiveState,NextElapseUSecRealtime",
                ])
                .output()
                .ok()
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .unwrap_or_default();
            let timer_enabled = timer_output.contains("ActiveState=active");
            let timer_next = timer_output
                .lines()
                .find(|l| l.starts_with("NextElapseUSecRealtime="))
                .and_then(|l| l.strip_prefix("NextElapseUSecRealtime="))
                .filter(|v| !v.is_empty() && *v != "n/a")
                .map(String::from);
            let drives_mounted = report.targets.iter().filter(|t| t.mounted).count();

            // Compute last_backup_age_secs from report.last_backup
            let last_backup_age_secs: Option<i64> = report.last_backup.as_ref().and_then(|lb| {
                use std::time::{SystemTime, UNIX_EPOCH};
                let parts: Vec<&str> = lb.split_whitespace().collect();
                if parts.len() != 2 {
                    return None;
                }
                let date_parts: Vec<&str> = parts[0].split('-').collect();
                let time_parts: Vec<&str> = parts[1].split(':').collect();
                if date_parts.len() != 3 || time_parts.len() != 2 {
                    return None;
                }
                let year: i32 = date_parts[0].parse().ok()?;
                let month: u32 = date_parts[1].parse().ok()?;
                let day: u32 = date_parts[2].parse().ok()?;
                let hour: u64 = time_parts[0].parse().ok()?;
                let minute: u64 = time_parts[1].parse().ok()?;

                let y = if month <= 2 { year - 1 } else { year } as i64;
                let m = if month <= 2 { month + 9 } else { month - 3 } as i64;
                let era = if y >= 0 { y } else { y - 399 } / 400;
                let yoe = y - era * 400;
                let doy = (153 * m + 2) / 5 + day as i64 - 1;
                let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
                let days = era * 146_097 + doe - 719_468;
                let backup_secs = days * 86400 + hour as i64 * 3600 + minute as i64 * 60;

                let now_secs = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0);
                Some(now_secs - backup_secs)
            });

            let json = serde_json::json!({
                "status": status_str,
                "targets": targets_json,
                "last_backup": report.last_backup,
                "warnings": report.warnings,
                "growth": growth_json,
                "scrub_thresholds": {
                    "enabled": config.scrub.enabled,
                    "warn_age_days": config.scrub.warn_age_days,
                    "fail_age_days": config.scrub.fail_age_days,
                },
                "services": {
                    "btrbk_available": btrbk_available,
                    "timer_enabled": timer_enabled,
                    "timer_next": timer_next,
                    "last_backup": report.last_backup,
                    "last_backup_age_secs": last_backup_age_secs,
                    "drives_mounted": drives_mounted,
                },
            });

            Ok(json.to_string())
        })
        .await
        .unwrap_or_else(|e| Err(format!("Health query task panicked: {e}")));

        json_str.map_err(fdo::Error::Failed)
    }

    /// The recovery-drive panel's document (bd DAS-Backup-Manager-8249).
    async fn recovery_os_status(
        &self,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> fdo::Result<String> {
        let sender = sender_from_header(&header)?;
        check_polkit(&self.conn, &sender, "org.dasbackup.health").await?;
        let config = load_config()?;
        tokio::task::spawn_blocking(move || {
            let state = recovery_os::load_state(&recovery_os::state_path());
            let host = recovery_os::host_versions();
            let today = buttered_dasd::caldate::today();
            let reads = SystemPanelReads {
                script: PathBuf::from(session::SCRIPT),
            };
            panel::status_json(&config, &state, &host, &today, &reads).to_string()
        })
        .await
        .map_err(|e| fdo::Error::Failed(format!("The recovery-OS status task panicked: {e}")))
    }

    /// Start a recovery-OS session (`recovery-os-vm.sh session …`) as a job;
    /// returns its id. `mode` is `sequential`, `parallel`, or empty for none
    /// (one drive, or the script's choice for two). Refused before the script
    /// starts while another session job or a scheduled-session service runs,
    /// naming it. `JobCancel` is the only way to stop it: it sends SIGINT to
    /// the script's process group once. In `--mode parallel` that does not
    /// yet stop the drive sessions themselves — a script defect, bd
    /// DAS-Backup-Manager-c8lf — so a cancelled parallel run goes on to its
    /// own end; parallel is not refused for it.
    async fn recovery_os_session(
        &self,
        #[zbus(header)] header: zbus::message::Header<'_>,
        labels: Vec<String>,
        unattended: bool,
        mode: &str,
        accept_boot_record_risk: bool,
    ) -> fdo::Result<String> {
        let sender = sender_from_header(&header)?;
        check_polkit(&self.conn, &sender, "org.dasbackup.recovery-os").await?;

        let config = load_config()?;
        // An `as` argument cannot say "not specified": an empty list is a
        // selection of nothing, and validate refuses it.
        let req = session::SessionRequest {
            labels,
            unattended,
            mode: (!mode.is_empty()).then(|| mode.to_string()),
            accept_boot_record_risk,
        };
        req.validate(&config).map_err(fdo::Error::InvalidArgs)?;

        let units = update_unit_names(&config);
        let states = tokio::task::spawn_blocking(move || read_active_states(&units))
            .await
            .map_err(|e| fdo::Error::Failed(format!("The unit-state task panicked: {e}")))?
            .map_err(fdo::Error::Failed)?;

        // Held from the check to the insert, so two calls cannot both pass.
        let mut jobs_guard = self.jobs.lock().await;
        let running: Vec<(&str, &str)> = jobs_guard
            .iter()
            .map(|(id, (_, _, _, kind))| (id.as_str(), *kind))
            .collect();
        let unit_states: Vec<(&str, &str)> = states
            .iter()
            .map(|(u, s)| (u.as_str(), s.as_str()))
            .collect();
        if let Some(why) = session_busy(&running, &unit_states) {
            return Err(fdo::Error::Failed(why));
        }

        let job_id = new_job_id();
        let progress = job_progress(&self.conn, &job_id);
        let cancel = progress.clone();
        let finisher = progress.clone();
        let jobs = self.jobs.clone();
        let jid = job_id.clone();

        let handle = tokio::spawn(async move {
            let result = tokio::task::spawn_blocking(move || {
                session::run_session(
                    &session::SystemSpawner,
                    Path::new(session::SCRIPT),
                    &config,
                    &req,
                    &*progress,
                )
            })
            .await;
            let (success, summary) = match result {
                // Exit 0 only; exit 5 is `false`, "warnings: …".
                Ok(Ok(out)) => (out.success(), out.summary()),
                Ok(Err(e)) => (false, e),
                Err(e) => (false, format!("The recovery-OS session task panicked: {e}")),
            };
            finish_job(finisher, success, summary).await;
            jobs.lock().await.remove(&jid);
        });

        jobs_guard.insert(
            job_id.clone(),
            (handle, cancel, sender.clone(), SESSION_KIND),
        );
        Ok(job_id)
    }

    /// End a drive's session (`recovery-os-vm.sh session-end <label>`):
    /// `(exit 0, the script's last lines)`.
    async fn recovery_os_session_end(
        &self,
        #[zbus(header)] header: zbus::message::Header<'_>,
        label: &str,
    ) -> fdo::Result<(bool, String)> {
        let sender = sender_from_header(&header)?;
        check_polkit(&self.conn, &sender, "org.dasbackup.recovery-os").await?;

        let label = configured_mirror_label(label)?;
        let (status, text) = tokio::task::spawn_blocking(move || {
            run_script_merged(
                Path::new(session::SCRIPT),
                &["session-end".to_string(), label],
            )
        })
        .await
        .map_err(|e| fdo::Error::Failed(format!("The session-end task panicked: {e}")))?
        .map_err(fdo::Error::Failed)?;
        Ok((status.success(), last_lines(&text, 12)))
    }

    /// The VNC socket of a drive's running session, made reachable by the
    /// CALLER's uid (`recovery-os-vm.sh console-socket <label> <uid>`).
    async fn recovery_os_console(
        &self,
        #[zbus(header)] header: zbus::message::Header<'_>,
        label: &str,
    ) -> fdo::Result<String> {
        let sender = sender_from_header(&header)?;
        check_polkit(&self.conn, &sender, "org.dasbackup.recovery-os").await?;

        let label = configured_mirror_label(label)?;
        // The caller's uid, from the bus; a failed lookup is an error, never
        // uid 0.
        let no_uid = |e: String| fdo::Error::Failed(format!("Cannot name the caller's uid: {e}"));
        let bus_name =
            zbus::names::BusName::try_from(sender.as_str()).map_err(|e| no_uid(e.to_string()))?;
        let uid = fdo::DBusProxy::new(&self.conn)
            .await
            .map_err(|e| no_uid(e.to_string()))?
            .get_connection_unix_user(bus_name)
            .await
            .map_err(|e| no_uid(e.to_string()))?;

        tokio::task::spawn_blocking(move || {
            let uid = uid.to_string();
            run_script(
                Path::new(session::SCRIPT),
                &["console-socket", &label, &uid],
            )
            .and_then(|out| console_path_from(&out))
        })
        .await
        .map_err(|e| fdo::Error::Failed(format!("The console task panicked: {e}")))?
        .map_err(fdo::Error::Failed)
    }

    /// Cancel a running job.
    async fn job_cancel(
        &self,
        #[zbus(header)] header: zbus::message::Header<'_>,
        job_id: &str,
    ) -> fdo::Result<bool> {
        let sender = sender_from_header(&header)?;
        check_polkit(&self.conn, &sender, "org.dasbackup.backup").await?;

        let jobs = self.jobs.lock().await;
        // Ownership is a property of the JOB, and polkit only answered a
        // question about the caller. Check both.
        match jobs.get(job_id) {
            None => Ok(false),
            Some((_, _, owner, _)) if *owner != sender => Err(fdo::Error::AccessDenied(format!(
                "Job '{job_id}' belongs to another client"
            ))),
            // The task is NOT aborted: that only stopped the job from ever
            // sending JobFinished while its blocking work went on as root,
            // holding the backup locks. The job is marked cancelled — its
            // progress is no longer sent — and it stops at its next safe
            // boundary: a lock wait at once, otherwise once the stage in
            // progress (a btrbk send, a btrfs operation, a write, an
            // unmount) has finished, never in the middle of one. It still
            // unmounts what it mounted, lets go of its locks and records
            // itself, then ends with exactly one JobFinished: failed,
            // "cancelled …", when it stopped early; its real outcome, saying
            // the cancel came too late, when nothing was left to stop
            // (bd DAS-Backup-Manager-yq2).
            Some((_, progress, _, _)) => {
                progress.cancel();
                Ok(true)
            }
        }
    }
}

impl HelperInterface {
    /// Spawn a background task that recomputes IndexStats for `db_path`
    /// and writes the fresh entry into the cache.  Deduplicated by the
    /// stats_refresh_in_flight set so concurrent refresh requests for the
    /// same path collapse to a single compute.  Called from index_stats
    /// on the stale-while-revalidate path; never blocks the caller.
    fn spawn_stats_refresh(&self, db_path: String) {
        let cache = self.stats_cache.clone();
        let in_flight = self.stats_refresh_in_flight.clone();
        tokio::spawn(async move {
            // Claim the in-flight slot.  If another task already holds it,
            // we drop out — they'll populate the cache for us.
            {
                let mut guard = in_flight.lock().await;
                if !guard.insert(db_path.clone()) {
                    return;
                }
            }
            let cache_inner = cache.clone();
            let path_inner = db_path.clone();
            let outcome = tokio::task::spawn_blocking(move || -> Result<(), String> {
                let entry = compute_stats_entry(&path_inner)?;
                cache_inner
                    .blocking_lock()
                    .insert(path_inner.clone(), entry);
                Ok(())
            })
            .await;
            // `let _ = ... .await` here discarded BOTH the task error and the
            // refresh error. A refresh that cannot open the DB leaves the stale
            // entry in place, and index_stats then re-fires a refresh on every
            // single call — failing every time, in total silence, while the GUI
            // Health Dashboard serves an index snapshot from before the failure
            // and dates it with nothing (bd DAS-Backup-Manager-8wx).
            match outcome {
                Ok(Ok(())) => {}
                Ok(Err(e)) => eprintln!(
                    "btrdasd-helper: IndexStats refresh for {db_path} FAILED ({e}) — \
                     the cached value is now stale and will keep being served"
                ),
                Err(e) => eprintln!(
                    "btrdasd-helper: IndexStats refresh task for {db_path} panicked ({e}) — \
                     the cached value is now stale and will keep being served"
                ),
            }
            in_flight.lock().await.remove(&db_path);
        });
    }
}

// ---------------------------------------------------------------------------
// The jobs that mount the backup targets
//
// Each waits for the DAS maintenance lock first (`maintenance::hold_for_job`,
// which logs who holds it and stops waiting if the job is cancelled), so a
// GUI index or restore never mounts a target another job is using — a backup,
// a scrub, or a recovery drive lent to a VM (bd DAS-Backup-Manager-frb).
// ---------------------------------------------------------------------------

/// `IndexWalk`: index every mounted target, or only `target_path`.
fn index_walk_job(
    config: &Config,
    target_path: &str,
    site: &LockSite,
    progress: &OrderedProgress,
) -> Result<String, String> {
    let held = maintenance::hold_for_job(site, "btrdasd-helper IndexWalk job", progress)?;
    let mut guard = mount::ensure_targets_mounted(config, progress, &held)
        .map_err(|e| format!("Mount failed: {e}"))?;

    let db = Database::open(&config.general.db_path).map_err(|e| format!("DB open failed: {e}"))?;

    // Collect target paths to walk (detect udisks2 mounts too)
    let paths: Vec<String> = if target_path.is_empty() {
        config
            .targets
            .iter()
            .filter_map(|t| health::find_any_mount(&t.mount, &t.serial, &t.role))
            .collect()
    } else {
        vec![target_path.to_owned()]
    };

    let mut total_discovered = 0usize;
    let mut total_indexed = 0usize;
    let mut total_skipped = 0usize;
    let mut errors = Vec::new();

    progress.on_stage("Indexing targets", paths.len() as u64);
    let mut cancelled = None;
    for (i, path) in paths.iter().enumerate() {
        // A boundary before each target: a walk in progress is never cut.
        if progress::stop_requested(progress) {
            let done = if i == 0 {
                "mounting the targets".to_string()
            } else {
                format!("indexing {i} of {} targets", paths.len())
            };
            cancelled = Some(backup::cancelled_line(&done, &paths[i..].join(", ")));
            break;
        }
        progress.on_progress(
            (i + 1) as u64,
            paths.len() as u64,
            &format!("Walking {path}"),
        );
        match indexer::walk(Path::new(path), &db) {
            Ok(r) => {
                total_discovered += r.snapshots_discovered;
                total_indexed += r.snapshots_indexed;
                total_skipped += r.snapshots_skipped;
            }
            Err(e) => {
                errors.push(format!("{path}: {e}"));
            }
        }
    }

    let still_mounted = guard.unmount(progress);

    let res = if let Some(cancelled) = cancelled {
        Err(format!(
            "{cancelled} — indexed {total_indexed} new snapshots \
             ({total_discovered} discovered, {total_skipped} skipped)"
        ))
    } else if !errors.is_empty() && total_indexed == 0 {
        Err(format!("Indexing failed: {}", errors.join("; ")))
    } else {
        let mut msg = format!(
            "Indexed {total_indexed} new snapshots ({total_discovered} discovered, {total_skipped} skipped)"
        );
        if !errors.is_empty() {
            msg.push_str(&format!(" [warnings: {}]", errors.join("; ")));
        }
        Ok(msg)
    };
    mount::fail_if_still_mounted(res, &still_mounted)
}

/// `RestoreFiles`: restore `files` from `snapshot` into `dest`.
fn restore_files_job(
    config: &Config,
    snapshot: &str,
    dest: &str,
    files: &[String],
    site: &LockSite,
    progress: &OrderedProgress,
) -> Result<(bool, String), String> {
    let held = maintenance::hold_for_job(site, "btrdasd-helper RestoreFiles job", progress)?;
    let mut guard = mount::ensure_targets_mounted(config, progress, &held)
        .map_err(|e| format!("Mount failed: {e}"))?;

    let file_refs: Vec<&str> = files.iter().map(|s| s.as_str()).collect();
    // A boundary: mounted, nothing restored yet.
    let res = if progress::stop_requested(progress) {
        Err(backup::cancelled_line("mounting the targets", "restore"))
    } else {
        match restore::restore_files(
            Path::new(snapshot),
            &file_refs,
            Path::new(dest),
            &config.restore.allowed_roots,
            &restore::snapshot_source_roots(config),
            progress,
        ) {
            Ok(r) => Ok((
                r.errors.is_empty(),
                format!(
                    "Restored {} files ({} bytes), {} errors",
                    r.files_restored,
                    r.bytes_restored,
                    r.errors.len()
                ),
            )),
            Err(e) => Err(format!("Restore failed: {e}")),
        }
    };

    let still_mounted = guard.unmount(progress);
    mount::fail_if_still_mounted(res, &still_mounted)
}

/// `RestoreSnapshot`: restore all of `snapshot` into `dest`.
fn restore_snapshot_job(
    config: &Config,
    snapshot: &str,
    dest: &str,
    site: &LockSite,
    progress: &OrderedProgress,
) -> Result<(bool, String), String> {
    let held = maintenance::hold_for_job(site, "btrdasd-helper RestoreSnapshot job", progress)?;
    let mut guard = mount::ensure_targets_mounted(config, progress, &held)
        .map_err(|e| format!("Mount failed: {e}"))?;

    // A boundary: mounted, nothing restored yet.
    let res = if progress::stop_requested(progress) {
        Err(backup::cancelled_line("mounting the targets", "restore"))
    } else {
        match restore::restore_snapshot(
            Path::new(snapshot),
            Path::new(dest),
            &config.restore.allowed_roots,
            &restore::snapshot_source_roots(config),
            progress,
        ) {
            Ok(r) => Ok((
                r.errors.is_empty(),
                format!(
                    "Snapshot restored: {} files ({} bytes), {} errors",
                    r.files_restored,
                    r.bytes_restored,
                    r.errors.len()
                ),
            )),
            Err(e) => Err(format!("Snapshot restore failed: {e}")),
        }
    };

    let still_mounted = guard.unmount(progress);
    mount::fail_if_still_mounted(res, &still_mounted)
}

// ---------------------------------------------------------------------------
// Helper functions
// ---------------------------------------------------------------------------

/// `label` if the configuration lists it as a `role = "mirror"` target, by
/// the session request's own rule; refused (`InvalidArgs`) otherwise, before
/// anything reaches the script.
fn configured_mirror_label(label: &str) -> fdo::Result<String> {
    let req = session::SessionRequest {
        labels: vec![label.to_string()],
        unattended: false,
        mode: None,
        accept_boot_record_risk: false,
    };
    req.validate(&load_config()?)
        .map_err(fdo::Error::InvalidArgs)?;
    Ok(label.to_string())
}

/// Extract the sender bus name from a D-Bus message header.
fn sender_from_header(header: &zbus::message::Header<'_>) -> Result<String, fdo::Error> {
    header
        .sender()
        .map(|s| s.to_string())
        .ok_or_else(|| fdo::Error::Failed("Missing sender in D-Bus message header".to_string()))
}

/// Recompute the IndexStats cache entry for `db_path`.
///
/// Returns the failure instead of a fabricated entry, so a caller can say WHY
/// the number on screen stopped moving. Extracted from `spawn_stats_refresh`,
/// where its errors used to be discarded wholesale.
fn compute_stats_entry(db_path: &str) -> Result<StatsCacheEntry, String> {
    let db = Database::open(db_path).map_err(|e| format!("open: {e}"))?;
    let stats = db.get_stats().map_err(|e| format!("stats: {e}"))?;
    let meta = std::fs::metadata(db_path).map_err(|e| format!("stat: {e}"))?;
    let db_size_bytes = meta.len();
    let mtime_nanos = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as i128)
        .unwrap_or(0);
    let json = serde_json::json!({
        "snapshots": stats.snapshot_count,
        "files": stats.file_count,
        "spans": stats.span_count,
        "db_size_bytes": db_size_bytes,
    })
    .to_string();
    Ok(StatsCacheEntry {
        db_mtime_nanos: mtime_nanos,
        db_size_bytes,
        json,
    })
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

/// Serves `iface` on `conn`, and only then requests `name`.
///
/// The order is the whole point (bd 3i1): the bus delivers the message that
/// activated the helper the moment the name is owned, so nothing may be
/// unserved by then.
async fn serve_then_claim_name(
    conn: &Connection,
    iface: HelperInterface,
    name: &str,
) -> zbus::Result<()> {
    conn.object_server().at(HELPER_PATH, iface).await?;
    await_object_server_ready(conn).await?;
    conn.request_name(name).await?;
    Ok(())
}

/// Returns only once the object server is receiving method calls.
///
/// `ObjectServer::at` returns while the dispatch task is still subscribing to
/// the connection's method-call stream (zbus starts it lazily and does not wait
/// for it), so a call that reaches the connection in that gap is dropped and its
/// caller times out. `Builder::serve_at` waits for the task, but cannot be used
/// here: the interface holds a clone of the connection it is served on. So the
/// helper calls itself: an `Introspect` of its own object only gets an answer
/// from a running dispatch task.
async fn await_object_server_ready(conn: &Connection) -> zbus::Result<()> {
    let own_name = conn
        .unique_name()
        .ok_or_else(|| zbus::Error::Failure("no unique name on the bus connection".into()))?
        .to_owned();
    conn.call_method(
        Some(own_name),
        HELPER_PATH,
        Some("org.freedesktop.DBus.Introspectable"),
        "Introspect",
        &(),
    )
    .await?;
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let jobs: JobMap = Arc::new(Mutex::new(HashMap::new()));

    // Build the system D-Bus connection and serve the interface.
    let conn = Builder::system()?.build().await?;

    let iface = HelperInterface {
        jobs: jobs.clone(),
        conn: conn.clone(),
        stats_cache: Arc::new(Mutex::new(HashMap::new())),
        stats_refresh_in_flight: Arc::new(Mutex::new(std::collections::HashSet::new())),
    };

    serve_then_claim_name(&conn, iface, HELPER_NAME).await?;

    eprintln!("btrdasd-helper: listening on system bus as org.dasbackup.Helper1");

    // Pre-warm the IndexStats cache at startup so the first GUI Health
    // Dashboard click hits a populated cache instead of waiting 30-60 s for
    // a cold COUNT(*) on a multi-GB index — that wait trips the GUI's 25 s
    // D-Bus call timeout on cold systems.  See DAS-Backup-Manager-aem.
    {
        let conn_for_warm = conn.clone();
        tokio::spawn(async move {
            let result = tokio::task::spawn_blocking(move || -> Result<String, String> {
                let path = "/var/lib/das-backup/backup-index.db";
                if !Path::new(path).exists() {
                    return Err(format!("DB not present at {path}"));
                }
                let db = Database::open(path).map_err(|e| format!("open: {e}"))?;
                let stats = db.get_stats().map_err(|e| format!("stats: {e}"))?;
                let meta = std::fs::metadata(path).map_err(|e| format!("stat: {e}"))?;
                let db_size_bytes = meta.len();
                let mtime_nanos = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_nanos() as i128)
                    .unwrap_or(0);
                let json = serde_json::json!({
                    "snapshots": stats.snapshot_count,
                    "files": stats.file_count,
                    "spans": stats.span_count,
                    "db_size_bytes": db_size_bytes,
                })
                .to_string();

                // Reach into the registered interface to populate its cache.
                // We block on a runtime handle since we're on a blocking thread.
                let handle = tokio::runtime::Handle::current();
                handle.block_on(async {
                    if let Ok(iface_ref) = conn_for_warm
                        .object_server()
                        .interface::<_, HelperInterface>("/org/dasbackup/Helper1")
                        .await
                    {
                        let cache = iface_ref.get().await.stats_cache.clone();
                        cache.lock().await.insert(
                            path.to_string(),
                            StatsCacheEntry {
                                db_mtime_nanos: mtime_nanos,
                                db_size_bytes,
                                json: json.clone(),
                            },
                        );
                    }
                });
                Ok(json)
            })
            .await;
            match result {
                Ok(Ok(_)) => eprintln!("btrdasd-helper: pre-warmed IndexStats cache"),
                Ok(Err(e)) => eprintln!("btrdasd-helper: pre-warm skipped ({e})"),
                Err(e) => eprintln!("btrdasd-helper: pre-warm task panicked ({e})"),
            }
        });
    }

    // Wait for SIGTERM or SIGINT for graceful shutdown.
    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigint = signal(SignalKind::interrupt())?;

    tokio::select! {
        _ = sigterm.recv() => {
            eprintln!("btrdasd-helper: received SIGTERM, shutting down");
        }
        _ = sigint.recv() => {
            eprintln!("btrdasd-helper: received SIGINT, shutting down");
        }
    }

    // Cancel all running jobs.
    {
        let mut active_jobs = jobs.lock().await;
        let entries: Vec<(String, JobEntry)> = active_jobs.drain().collect();
        for (id, (handle, progress, _owner, _kind)) in entries {
            eprintln!("btrdasd-helper: cancelling job {id}");
            progress.cancel();
            handle.abort();
        }
    }

    eprintln!("btrdasd-helper: shutdown complete");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- recovery-OS methods (bd DAS-Backup-Manager-8249 stage 2)

    #[test]
    fn the_recovery_os_methods_check_polkit_with_the_recovery_os_action() {
        let src = include_str!("btrdasd-helper.rs");
        let body = &src[..src.find("#[cfg(test)]").unwrap()];
        for m in [
            "async fn recovery_os_session(",
            "async fn recovery_os_session_end(",
            "async fn recovery_os_console(",
        ] {
            let at = body.find(m).unwrap_or_else(|| panic!("{m} missing"));
            let after = &body[at..at + 1200];
            assert!(
                after.contains("check_polkit(&self.conn, &sender, \"org.dasbackup.recovery-os\")"),
                "{m} does not check org.dasbackup.recovery-os"
            );
        }
        let at = body.find("async fn recovery_os_status(").unwrap();
        assert!(body[at..at + 600].contains("\"org.dasbackup.health\""));
    }

    #[test]
    fn a_second_session_job_is_refused_naming_the_first() {
        assert_eq!(session_busy(&[("job-1", "backup")], &[]), None);
        assert_eq!(
            session_busy(&[("job-2", "recovery-os-session")], &[]).unwrap(),
            "a recovery-OS session is already running as job job-2"
        );
        assert_eq!(
            session_busy(
                &[],
                &[("das-recovery-os-update-both.service", "activating")]
            )
            .unwrap(),
            "a scheduled recovery-OS session is running: das-recovery-os-update-both.service (activating)"
        );
        assert_eq!(
            session_busy(
                &[],
                &[(
                    "das-recovery-os-update-system-recovery-A-2tb.service",
                    "inactive"
                )]
            ),
            None
        );
    }

    #[test]
    fn the_polkit_policy_declares_the_recovery_os_action_as_auth_admin_keep() {
        let policy = include_str!("../../../polkit/org.dasbackup.policy");
        let at = policy
            .find("action id=\"org.dasbackup.recovery-os\"")
            .expect("action declared");
        let block = &policy[at..policy[at..].find("</action>").unwrap() + at];
        assert!(
            block.contains("<allow_any>no</allow_any>")
                && block.contains("<allow_active>auth_admin_keep</allow_active>"),
            "{block}"
        );
    }

    fn output(code: i32, stdout: &str, stderr: &str) -> std::process::Output {
        use std::os::unix::process::ExitStatusExt;
        std::process::Output {
            status: std::process::ExitStatus::from_raw(code << 8),
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
        }
    }

    #[test]
    fn clean_runs_is_a_count_or_an_error_never_zero_by_default() {
        assert_eq!(clean_runs_from(&output(0, "3\n", "")), Ok(3));
        for bad in [
            output(0, "", ""),
            output(0, "three\n", ""),
            output(0, "-1\n", ""),
            output(2, "0\n", "no such drive"),
        ] {
            assert!(clean_runs_from(&bad).is_err(), "{bad:?}");
        }
        assert_eq!(
            clean_runs_from(&output(2, "", "no such drive\n")),
            Err("no such drive".to_string())
        );
    }

    #[test]
    fn history_is_every_line_as_json_or_an_error_never_a_partial_list() {
        let got = history_from(&output(0, "{\"a\":1}\n{\"b\":2}\n", "")).unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(history_from(&output(0, "", "")), Ok(vec![]));
        assert!(history_from(&output(0, "{\"a\":1}\nnot json\n", "")).is_err());
        assert!(history_from(&output(1, "{\"a\":1}\n", "cannot read")).is_err());
    }

    #[test]
    fn the_console_socket_is_one_path_or_the_scripts_refusal() {
        assert_eq!(
            console_path_from(&output(0, "/run/das-recovery-os/A/vnc.sock\n", "")),
            Ok("/run/das-recovery-os/A/vnc.sock".to_string())
        );
        assert_eq!(
            console_path_from(&output(3, "", "no session is running for A\n")),
            Err("no session is running for A".to_string())
        );
        for bad in [
            output(0, "", ""),
            output(0, "/a\n/b\n", ""),
            output(0, "relative\n", ""),
        ] {
            assert!(console_path_from(&bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn last_lines_keeps_the_tail_in_order() {
        let text: String = (1..=15).map(|n| format!("line {n}\n")).collect();
        let got = last_lines(&text, 12);
        assert!(
            got.starts_with("line 4\n") && got.ends_with("line 15"),
            "{got}"
        );
        assert_eq!(last_lines("", 12), "");
    }

    #[test]
    fn a_step_value_that_is_not_a_boolean_reaches_the_library_as_none() {
        use zbus::zvariant::{OwnedValue, Value};
        let mut map = std::collections::HashMap::new();
        map.insert(
            "index".to_string(),
            OwnedValue::try_from(Value::from(true)).unwrap(),
        );
        map.insert(
            "email".to_string(),
            OwnedValue::try_from(Value::from("yes")).unwrap(),
        );
        let mut got = step_entries(map);
        got.sort();
        assert_eq!(
            got,
            [
                ("email".to_string(), None),
                ("index".to_string(), Some(true))
            ]
        );
    }

    #[test]
    fn an_unknown_or_empty_mode_is_refused_never_read_as_incremental() {
        assert_eq!(parse_mode("full"), Ok(BackupMode::Full));
        assert_eq!(parse_mode("Incremental"), Ok(BackupMode::Incremental));
        for bad in ["", "weekly", "snapshot"] {
            assert!(parse_mode(bad).unwrap_err().contains("mode"), "{bad:?}");
        }
    }

    // --- config writes take the backup singleton (bd DAS-Backup-Manager-lxw)

    #[test]
    fn a_config_write_is_refused_while_a_backup_or_setup_holds_the_singleton() {
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("das-backup.lock");
        let _backup = buttered_dasd::scrub::FileLock::try_acquire(&lock)
            .unwrap()
            .unwrap();
        let mut ran = false;
        let err = edit_config_at(&lock, || {
            ran = true;
            Ok(())
        })
        .unwrap_err();
        assert!(!ran, "the write ran while the singleton was held");
        assert_eq!(
            err,
            fdo::Error::Failed(btrbk_conf::config_busy_line(&lock)),
            "the GUI shows this message as it is"
        );
    }

    #[test]
    fn a_config_write_runs_and_returns_its_own_result_when_the_singleton_is_free() {
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("das-backup.lock");
        assert_eq!(edit_config_at(&lock, || Ok(7)), Ok(7));
        let failed = fdo::Error::Failed("Invalid source".into());
        assert_eq!(
            edit_config_at(&lock, || Err::<(), _>(failed.clone())),
            Err(failed)
        );
        let err = edit_config_at(Path::new("/dev/null/das-backup.lock"), || Ok(())).unwrap_err();
        assert!(matches!(err, fdo::Error::Failed(m) if m.contains("/dev/null/das-backup.lock")));
    }

    #[test]
    fn every_method_that_saves_the_config_does_so_under_the_singleton() {
        let src = include_str!("btrdasd-helper.rs");
        let body = &src[..src.find("#[cfg(test)]").unwrap()];
        // Built at run time so this test does not count itself.
        let save = format!("{}(&config)", "save_config");
        let guarded = format!("{}(|| {{", "edit_config");
        assert_eq!(body.matches(&save).count(), 5, "the five writing methods");
        assert_eq!(body.matches(&guarded).count(), 5);
        // Each save sits inside the edit_config closure that precedes it.
        for (at, _) in body.match_indices(&save) {
            let opened = body[..at]
                .rfind(&guarded)
                .expect("a save outside edit_config");
            let between = &body[opened..at];
            assert!(
                !between.contains("async fn "),
                "a save outside edit_config: {}",
                &body[at.saturating_sub(200)..at]
            );
        }
    }

    #[test]
    fn the_half_selection_methods_are_gone_from_the_interface() {
        let src = include_str!("btrdasd-helper.rs");
        for name in ["backup_snapshot", "backup_send", "backup_boot_archive"] {
            let needle = format!("async fn {name}(");
            // The needle is built at run time so this test does not match itself.
            assert!(
                !src.contains(&needle),
                "{name} is still an interface method"
            );
        }
    }

    /// The refresh must produce a NAMED failure, not nothing. Its result used
    /// to be dropped by `let _ = ..`, which is why a DB the helper could no
    /// longer open showed up as an index that had simply stopped changing.
    #[test]
    fn compute_stats_entry_reports_a_database_it_cannot_open() {
        let err = match compute_stats_entry("/nonexistent-8wx-dir/backup-index.db") {
            Err(e) => e,
            Ok(_) => panic!("a database that cannot be opened must be an error"),
        };
        assert!(
            err.starts_with("open:") || err.starts_with("stat:"),
            "the error must name the step that failed, got: {err}"
        );

        // Positive control: a real database still computes, so the check cannot
        // be passing by failing on everything.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("backup-index.db");
        drop(Database::open(&path).expect("create a real index"));
        let entry = compute_stats_entry(&path.to_string_lossy())
            .expect("a real database must compute an entry");
        assert!(entry.json.contains("\"snapshots\""), "json: {}", entry.json);
        assert!(entry.db_size_bytes > 0, "an opened DB has a nonzero size");
    }

    // --- the jobs that mount wait for the maintenance lock (bd DAS-Backup-Manager-frb)
    //
    // Each runs against a scratch lock and a config with no targets, so
    // nothing is mounted. The restore jobs then stop at their source policy
    // ("no backup targets are configured") — before anything is copied or
    // `btrfs` is run — which is how a test sees that a job got past the lock.

    use buttered_dasd::maintenance::MaintenanceHeld;
    use buttered_dasd::progress::{OrderedProgress, ProgressEvent, ProgressSink};
    use std::sync::mpsc::{self, RecvTimeoutError};
    use std::time::{Duration, Instant};

    /// Long enough to be sure a job is blocked, not merely slow to start.
    const STILL_WAITING: Duration = Duration::from_millis(300);
    /// Ample for a job looking every 10 ms to notice a release or a cancel.
    const NOTICES: Duration = Duration::from_secs(1);

    type Sent = Arc<std::sync::Mutex<Vec<ProgressEvent>>>;

    /// Keeps what the GUI would have been sent.
    struct Gui(Sent);

    impl ProgressSink for Gui {
        fn journal(&mut self, _: LogLevel, _: &str) {}
        fn emit(&mut self, event: ProgressEvent) {
            self.0.lock().unwrap().push(event);
        }
    }

    fn gui_progress() -> (Arc<OrderedProgress>, Sent) {
        let sent: Sent = Arc::default();
        (Arc::new(OrderedProgress::new(Gui(sent.clone()))), sent)
    }

    /// The waiting line, once the GUI has been sent it (within [`NOTICES`]).
    fn waiting_line_sent(sent: &Sent) -> Option<String> {
        let deadline = Instant::now() + NOTICES;
        loop {
            let line = sent.lock().unwrap().iter().find_map(|e| match e {
                ProgressEvent::Log { message, .. }
                    if message.starts_with("Waiting for the DAS maintenance lock") =>
                {
                    Some(message.clone())
                }
                _ => None,
            });
            if line.is_some() || Instant::now() > deadline {
                return line;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// A temp dir holding the lock file, the index, and a snapshot with one
    /// file; a config with no targets that allows restores into the dir.
    struct JobRig {
        dir: tempfile::TempDir,
        config: Config,
    }

    impl JobRig {
        fn new() -> Arc<Self> {
            let dir = tempfile::tempdir().unwrap();
            let mut config = Config::default();
            config.general.db_path = dir.path().join("index.db").to_string_lossy().into_owned();
            config.restore.allowed_roots = vec![dir.path().to_string_lossy().into_owned()];
            std::fs::create_dir(dir.path().join("snap")).unwrap();
            std::fs::write(dir.path().join("snap/hello.txt"), "hi").unwrap();
            Arc::new(Self { dir, config })
        }

        fn path(&self, name: &str) -> std::path::PathBuf {
            self.dir.path().join(name)
        }

        fn site(&self) -> LockSite {
            LockSite {
                path: self.path("das-maintenance.lock"),
                poll: Duration::from_millis(10),
            }
        }

        /// Another job takes the lock and records itself.
        fn hold(&self) -> MaintenanceHeld {
            MaintenanceHeld::try_acquire_at(&self.site().path, "test holder")
                .unwrap()
                .expect("the scratch lock is free")
        }

        fn nothing_done(&self) -> bool {
            !self.path("index.db").exists() && !self.path("restored").exists()
        }
    }

    #[derive(Clone, Copy, Debug)]
    enum Job {
        IndexWalk,
        RestoreFiles,
        RestoreSnapshot,
    }

    /// Run one job on the rig: its summary, either way.
    fn run_job(rig: &JobRig, job: Job, progress: &OrderedProgress) -> Result<String, String> {
        let snap = rig.path("snap").to_string_lossy().into_owned();
        let dest = rig.path("restored").to_string_lossy().into_owned();
        let site = rig.site();
        match job {
            Job::IndexWalk => index_walk_job(&rig.config, "", &site, progress),
            Job::RestoreFiles => restore_files_job(
                &rig.config,
                &snap,
                &dest,
                &["hello.txt".to_string()],
                &site,
                progress,
            )
            .map(|(_, summary)| summary),
            Job::RestoreSnapshot => {
                restore_snapshot_job(&rig.config, &snap, &dest, &site, progress)
                    .map(|(_, summary)| summary)
            }
        }
    }

    /// Whether `out` is what the job does once it is past the lock.
    #[track_caller]
    fn assert_proceeded(job: Job, out: &Result<String, String>) {
        match job {
            Job::IndexWalk => assert_eq!(
                out.as_deref(),
                Ok("Indexed 0 new snapshots (0 discovered, 0 skipped)")
            ),
            Job::RestoreFiles | Job::RestoreSnapshot => assert!(
                matches!(out, Err(e) if e.contains("no backup targets are configured")),
                "{job:?} must reach its source policy, got {out:?}"
            ),
        }
    }

    fn takes_a_free_lock_at_once(job: Job) {
        let rig = JobRig::new();
        let (progress, sent) = gui_progress();
        let started = Instant::now();
        let out = run_job(&rig, job, &progress);
        assert!(started.elapsed() < STILL_WAITING, "{:?}", started.elapsed());
        assert_proceeded(job, &out);
        progress.finish(true, "done");
        assert!(
            !sent
                .lock()
                .unwrap()
                .iter()
                .any(|e| matches!(e, ProgressEvent::Log { message, .. }
                if message.starts_with("Waiting for"))),
            "{job:?} announced a wait with nothing to wait for"
        );
        drop(rig.hold()); // released when done
    }

    fn waits_while_the_lock_is_held_then_proceeds(job: Job) {
        let rig = JobRig::new();
        let holder = rig.hold();
        let (progress, sent) = gui_progress();
        let (tx, rx) = mpsc::channel();
        let (job_rig, job_progress) = (rig.clone(), progress.clone());
        let worker = std::thread::spawn(move || {
            tx.send(run_job(&job_rig, job, &job_progress)).unwrap();
        });

        assert!(
            matches!(
                rx.recv_timeout(STILL_WAITING),
                Err(RecvTimeoutError::Timeout)
            ),
            "{job:?} must wait while another job holds the lock"
        );
        assert!(
            rig.nothing_done(),
            "{job:?} did work before it had the lock"
        );
        let line = waiting_line_sent(&sent).expect("the GUI is told what the job waits for");
        assert!(
            line.contains(&format!("held by test holder pid {}", std::process::id())),
            "{line}"
        );

        drop(holder);
        let out = rx
            .recv_timeout(NOTICES)
            .unwrap_or_else(|_| panic!("{job:?} must proceed once the lock is released"));
        assert_proceeded(job, &out);
        worker.join().unwrap();
    }

    fn stops_waiting_when_cancelled(job: Job) {
        let rig = JobRig::new();
        let holder = rig.hold();
        let (progress, sent) = gui_progress();
        let (tx, rx) = mpsc::channel();
        let (job_rig, job_progress) = (rig.clone(), progress.clone());
        let worker = std::thread::spawn(move || {
            tx.send(run_job(&job_rig, job, &job_progress)).unwrap();
        });
        waiting_line_sent(&sent).expect("the job is waiting");

        progress.cancel();
        let summary = rx
            .recv_timeout(NOTICES)
            .unwrap_or_else(|_| panic!("{job:?}: a cancel must stop the wait"))
            .expect_err("a job cancelled while waiting did nothing");
        worker.join().unwrap();
        assert_eq!(
            summary,
            format!(
                "stopped waiting for the DAS maintenance lock, held by test holder pid {}; \
                 nothing was mounted",
                std::process::id()
            )
        );
        assert!(rig.nothing_done(), "{job:?}");

        // How the D-Bus method ends it: one JobFinished, failed, "cancelled".
        progress.finish(false, &summary);
        match sent.lock().unwrap().last() {
            Some(ProgressEvent::Finished { success, summary }) => {
                assert!(!success);
                assert!(summary.starts_with("cancelled — "), "{summary}");
            }
            other => panic!("{job:?} must end with JobFinished, got {other:?}"),
        }
        drop(holder);
    }

    #[test]
    fn index_walk_takes_a_free_lock_at_once() {
        takes_a_free_lock_at_once(Job::IndexWalk);
    }

    #[test]
    fn index_walk_waits_while_the_lock_is_held_then_proceeds() {
        waits_while_the_lock_is_held_then_proceeds(Job::IndexWalk);
    }

    #[test]
    fn index_walk_stops_waiting_when_cancelled() {
        stops_waiting_when_cancelled(Job::IndexWalk);
    }

    #[test]
    fn restore_files_takes_a_free_lock_at_once() {
        takes_a_free_lock_at_once(Job::RestoreFiles);
    }

    #[test]
    fn restore_files_waits_while_the_lock_is_held_then_proceeds() {
        waits_while_the_lock_is_held_then_proceeds(Job::RestoreFiles);
    }

    #[test]
    fn restore_files_stops_waiting_when_cancelled() {
        stops_waiting_when_cancelled(Job::RestoreFiles);
    }

    #[test]
    fn restore_snapshot_takes_a_free_lock_at_once() {
        takes_a_free_lock_at_once(Job::RestoreSnapshot);
    }

    #[test]
    fn restore_snapshot_waits_while_the_lock_is_held_then_proceeds() {
        waits_while_the_lock_is_held_then_proceeds(Job::RestoreSnapshot);
    }

    #[test]
    fn restore_snapshot_stops_waiting_when_cancelled() {
        stops_waiting_when_cancelled(Job::RestoreSnapshot);
    }

    /// Cancelled with the lock free: the job mounts, then stops at the
    /// boundary before the restore, lets go of the lock, restores nothing
    /// and ends failed, "cancelled after …" (bd DAS-Backup-Manager-yq2).
    fn a_cancelled_restore_stops_before_restoring(job: Job) {
        let rig = JobRig::new();
        let (progress, sent) = gui_progress();
        progress.cancel();
        let out = run_job(&rig, job, &progress);
        assert_eq!(
            out,
            Err("cancelled after mounting the targets; not done: restore".into()),
            "{job:?}"
        );
        assert!(rig.nothing_done(), "{job:?} restored nothing");
        drop(rig.hold()); // the lock was let go
        progress.finish(false, &out.unwrap_err());
        match sent.lock().unwrap().last() {
            Some(ProgressEvent::Finished { success, summary }) => {
                assert!(!success);
                assert_eq!(
                    summary,
                    "cancelled after mounting the targets; not done: restore"
                );
            }
            other => panic!("{job:?} must end with Finished, got {other:?}"),
        }
    }

    #[test]
    fn restore_files_stops_before_restoring_when_cancelled() {
        a_cancelled_restore_stops_before_restoring(Job::RestoreFiles);
    }

    #[test]
    fn restore_snapshot_stops_before_restoring_when_cancelled() {
        a_cancelled_restore_stops_before_restoring(Job::RestoreSnapshot);
    }

    /// Nothing is left to stop in an index walk with no target: the cancel
    /// came too late, and the job ends with its real outcome.
    #[test]
    fn a_cancel_with_nothing_left_to_stop_ends_with_the_real_outcome() {
        let rig = JobRig::new();
        let (progress, sent) = gui_progress();
        progress.cancel();
        let out = run_job(&rig, Job::IndexWalk, &progress);
        assert_proceeded(Job::IndexWalk, &out);
        progress.finish(true, &out.unwrap());
        match sent.lock().unwrap().last() {
            Some(ProgressEvent::Finished { success, summary }) => {
                assert!(success);
                assert!(summary.contains("cancel requested too late"), "{summary}");
            }
            other => panic!("must end with Finished, got {other:?}"),
        }
    }
}
