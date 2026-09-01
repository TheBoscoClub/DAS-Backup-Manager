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
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

use zbus::connection::Builder;
use zbus::fdo;
use zbus::object_server::SignalEmitter;
use zbus::{Connection, interface};

use buttered_dasd::backup::{self, BackupLockAttempt, BackupMode, BackupOptions};
use buttered_dasd::config::Config;
use buttered_dasd::db::Database;
use buttered_dasd::health;
use buttered_dasd::indexer;
use buttered_dasd::mount;
use buttered_dasd::progress::{LogLevel, ProgressCallback};
use buttered_dasd::report;
use buttered_dasd::restore;
use buttered_dasd::schedule;
use buttered_dasd::subvol;

// ---------------------------------------------------------------------------
// Cancellation token (simple AtomicBool-based, avoids tokio-util dependency)
// ---------------------------------------------------------------------------

/// A simple cancellation flag shared between the job spawner and the worker.
#[derive(Clone)]
struct CancelFlag(Arc<AtomicBool>);

impl CancelFlag {
    fn new() -> Self {
        Self(Arc::new(AtomicBool::new(false)))
    }

    fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

// ---------------------------------------------------------------------------
// Job tracking
// ---------------------------------------------------------------------------

/// Live jobs, keyed by id: `(task handle, cancel flag, owning D-Bus sender)`.
///
/// The sender is what makes `job_cancel` authorizable. Without it the map held
/// no notion of ownership at all, so a polkit check for
/// `org.dasbackup.backup` — a question about the CALLER, not about the JOB —
/// was the only gate, and any authorized caller could abort anyone's in-flight
/// backup or restore (bd DAS-Backup-Manager-h2s).
type JobEntry = (JoinHandle<()>, CancelFlag, String);
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

/// A `ProgressCallback` implementation that emits D-Bus signals for each
/// progress event.  Holds a connection and job_id so it can send signals
/// without access to the interface object.
struct DbusProgress {
    conn: Connection,
    job_id: String,
    cancel: CancelFlag,
}

impl DbusProgress {
    fn new(conn: Connection, job_id: String, cancel: CancelFlag) -> Self {
        Self {
            conn,
            job_id,
            cancel,
        }
    }
}

impl ProgressCallback for DbusProgress {
    fn on_stage(&self, stage: &str, _total_steps: u64) {
        if self.cancel.is_cancelled() {
            return;
        }
        let conn = self.conn.clone();
        let job_id = self.job_id.clone();
        let stage = stage.to_owned();
        tokio::spawn(async move {
            let iface_ref = conn
                .object_server()
                .interface::<_, HelperInterface>("/org/dasbackup/Helper1")
                .await;
            if let Ok(iface) = iface_ref {
                let ctxt = iface.signal_emitter();
                let _ = HelperInterface::job_progress(ctxt, &job_id, &stage, 0, "").await;
            }
        });
    }

    fn on_progress(&self, current: u64, total: u64, message: &str) {
        if self.cancel.is_cancelled() {
            return;
        }
        let percent = (current * 100)
            .checked_div(total)
            .map(|p| p.min(100) as i32)
            .unwrap_or(0);
        let conn = self.conn.clone();
        let job_id = self.job_id.clone();
        let msg = message.to_owned();
        tokio::spawn(async move {
            let iface_ref = conn
                .object_server()
                .interface::<_, HelperInterface>("/org/dasbackup/Helper1")
                .await;
            if let Ok(iface) = iface_ref {
                let ctxt = iface.signal_emitter();
                let _ =
                    HelperInterface::job_progress(ctxt, &job_id, "progress", percent, &msg).await;
            }
        });
    }

    fn on_throughput(&self, _bytes_per_sec: u64) {
        // Throughput is informational; folded into progress messages if needed.
    }

    fn on_log(&self, level: LogLevel, message: &str) {
        if self.cancel.is_cancelled() {
            return;
        }
        let level_str = match level {
            LogLevel::Debug => "debug",
            LogLevel::Info => "info",
            LogLevel::Warning => "warn",
            LogLevel::Error => "error",
        };
        // Also log to stderr (journald) for post-mortem debugging.
        eprintln!("[{level_str}] {message}");
        let conn = self.conn.clone();
        let job_id = self.job_id.clone();
        let lvl = level_str.to_owned();
        let msg = message.to_owned();
        tokio::spawn(async move {
            let iface_ref = conn
                .object_server()
                .interface::<_, HelperInterface>("/org/dasbackup/Helper1")
                .await;
            if let Ok(iface) = iface_ref {
                let ctxt = iface.signal_emitter();
                let _ = HelperInterface::job_log(ctxt, &job_id, &lvl, &msg).await;
            }
        });
    }

    fn on_complete(&self, success: bool, summary: &str) {
        let conn = self.conn.clone();
        let job_id = self.job_id.clone();
        let summ = summary.to_owned();
        tokio::spawn(async move {
            let iface_ref = conn
                .object_server()
                .interface::<_, HelperInterface>("/org/dasbackup/Helper1")
                .await;
            if let Ok(iface) = iface_ref {
                let ctxt = iface.signal_emitter();
                let _ = HelperInterface::job_finished(ctxt, &job_id, success, &summ).await;
            }
        });
    }
}

// ---------------------------------------------------------------------------
// Polkit authorization
// ---------------------------------------------------------------------------

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

fn save_config(config: &Config) -> Result<(), fdo::Error> {
    config
        .save(Path::new(CANONICAL_CONFIG))
        .map_err(|e| fdo::Error::Failed(format!("Failed to save config '{CANONICAL_CONFIG}': {e}")))
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
// D-Bus interface
// ---------------------------------------------------------------------------

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

    /// Run a full backup pipeline.
    async fn backup_run(
        &self,
        #[zbus(header)] header: zbus::message::Header<'_>,
        mode: &str,
        sources: Vec<String>,
        targets: Vec<String>,
        dry_run: bool,
    ) -> fdo::Result<String> {
        let sender = sender_from_header(&header)?;
        check_polkit(&self.conn, &sender, "org.dasbackup.backup").await?;

        let config = load_config()?;
        let backup_mode = match mode.to_lowercase().as_str() {
            "full" => Some(BackupMode::Full),
            "incremental" => Some(BackupMode::Incremental),
            _ => None,
        };
        let options = BackupOptions {
            mode: backup_mode,
            sources,
            targets,
            dry_run,
            boot_archive: config.boot.enabled,
            index_after: true,
            send_report: config.email.enabled,
            ..Default::default()
        };

        let job_id = new_job_id();
        let cancel = CancelFlag::new();
        let progress = DbusProgress::new(self.conn.clone(), job_id.clone(), cancel.clone());
        let jobs = self.jobs.clone();
        let jid = job_id.clone();
        let conn = self.conn.clone();

        let handle = tokio::spawn(async move {
            let result: Result<(bool, String), String> = tokio::task::spawn_blocking(move || {
                // Join the same two-lock interlock as the scheduled path and the
                // CLI: singleton (non-blocking — a second backup is redundant,
                // not late) then the shared maintenance lock (blocking — a scrub
                // is a peer operation and this should wait for it). Without it
                // a GUI backup could run concurrently with the 03:00 timer or a
                // live scrub, and MountGuard::Drop could unmount a target out
                // from under a running `btrfs receive`. bd DAS-Backup-Manager-pe6
                // fixed this for main.rs in 0.7.15.0 and never reached the
                // daemon the GUI actually calls (bd DAS-Backup-Manager-dca).
                let _locks = match backup::acquire_manual_locks(&progress) {
                    Ok(BackupLockAttempt::Acquired(locks)) => locks,
                    Ok(BackupLockAttempt::AlreadyRunning) => {
                        return Err("A backup is already running — declined".to_string());
                    }
                    Err(e) => return Err(format!("Could not acquire backup locks: {e}")),
                };
                let mut source_guard = mount::ensure_sources_mounted(&config, &progress);
                let mut guard = mount::ensure_targets_mounted(&config, &progress)
                    .map_err(|e| format!("Mount failed: {e}"))?;

                let res = match backup::run_backup(&config, &options, &progress) {
                    Ok(r) => {
                        // Record the backup run in the database for history (skip dry runs).
                        if !options.dry_run {
                            match Database::open(&config.general.db_path) {
                                Ok(db) => {
                                    if let Err(e) = report::record_backup_run(&db, &r) {
                                        progress.on_log(
                                            LogLevel::Warning,
                                            &format!("Failed to record backup history: {e}"),
                                        );
                                    }
                                }
                                Err(e) => {
                                    progress.on_log(
                                        LogLevel::Warning,
                                        &format!("Failed to open DB for history: {e}"),
                                    );
                                }
                            }
                        }
                        Ok((
                            r.success,
                            format!(
                                "Backup complete: {} snapshots created, {} sent",
                                r.snapshots_created, r.snapshots_sent
                            ),
                        ))
                    }
                    Err(e) => Err(format!("Backup failed: {e}")),
                };

                guard.unmount(&progress);
                source_guard.unmount(&progress);
                res
            })
            .await
            .unwrap_or_else(|e| Err(format!("Backup task panicked: {e}")));

            let (success, summary) = match result {
                Ok((s, msg)) => (s, msg),
                Err(msg) => (false, msg),
            };

            emit_job_finished(&conn, &jid, success, &summary).await;
            jobs.lock().await.remove(&jid);
        });

        self.jobs
            .lock()
            .await
            .insert(job_id.clone(), (handle, cancel, sender.clone()));
        Ok(job_id)
    }

    /// Create snapshots only.
    async fn backup_snapshot(
        &self,
        #[zbus(header)] header: zbus::message::Header<'_>,
        sources: Vec<String>,
    ) -> fdo::Result<String> {
        let sender = sender_from_header(&header)?;
        check_polkit(&self.conn, &sender, "org.dasbackup.backup").await?;

        let config = load_config()?;
        let job_id = new_job_id();
        let cancel = CancelFlag::new();
        let progress = DbusProgress::new(self.conn.clone(), job_id.clone(), cancel.clone());
        let jobs = self.jobs.clone();
        let jid = job_id.clone();
        let conn = self.conn.clone();

        let handle = tokio::spawn(async move {
            let result: Result<String, String> = tokio::task::spawn_blocking(move || {
                // Join the same two-lock interlock as the scheduled path and the
                // CLI: singleton (non-blocking — a second backup is redundant,
                // not late) then the shared maintenance lock (blocking — a scrub
                // is a peer operation and this should wait for it). Without it
                // a GUI backup could run concurrently with the 03:00 timer or a
                // live scrub, and MountGuard::Drop could unmount a target out
                // from under a running `btrfs receive`. bd DAS-Backup-Manager-pe6
                // fixed this for main.rs in 0.7.15.0 and never reached the
                // daemon the GUI actually calls (bd DAS-Backup-Manager-dca).
                let _locks = match backup::acquire_manual_locks(&progress) {
                    Ok(BackupLockAttempt::Acquired(locks)) => locks,
                    Ok(BackupLockAttempt::AlreadyRunning) => {
                        return Err("A backup is already running — declined".to_string());
                    }
                    Err(e) => return Err(format!("Could not acquire backup locks: {e}")),
                };
                let mut source_guard = mount::ensure_sources_mounted(&config, &progress);
                let res = match backup::create_snapshots(&config, &sources, &progress) {
                    Ok(n) => Ok(format!("{n} snapshots created")),
                    Err(e) => Err(format!("Snapshot failed: {e}")),
                };
                source_guard.unmount(&progress);
                res
            })
            .await
            .unwrap_or_else(|e| Err(format!("Snapshot task panicked: {e}")));

            let (success, summary) = match result {
                Ok(msg) => (true, msg),
                Err(msg) => (false, msg),
            };

            emit_job_finished(&conn, &jid, success, &summary).await;
            jobs.lock().await.remove(&jid);
        });

        self.jobs
            .lock()
            .await
            .insert(job_id.clone(), (handle, cancel, sender.clone()));
        Ok(job_id)
    }

    /// Send existing snapshots to targets.
    async fn backup_send(
        &self,
        #[zbus(header)] header: zbus::message::Header<'_>,
        targets: Vec<String>,
    ) -> fdo::Result<String> {
        let sender = sender_from_header(&header)?;
        check_polkit(&self.conn, &sender, "org.dasbackup.backup").await?;

        let config = load_config()?;
        let job_id = new_job_id();
        let cancel = CancelFlag::new();
        let progress = DbusProgress::new(self.conn.clone(), job_id.clone(), cancel.clone());
        let jobs = self.jobs.clone();
        let jid = job_id.clone();
        let conn = self.conn.clone();
        // Send from all sources to the specified targets.
        let sources: Vec<String> = Vec::new();

        let handle = tokio::spawn(async move {
            let result: Result<String, String> = tokio::task::spawn_blocking(move || {
                // Join the same two-lock interlock as the scheduled path and the
                // CLI: singleton (non-blocking — a second backup is redundant,
                // not late) then the shared maintenance lock (blocking — a scrub
                // is a peer operation and this should wait for it). Without it
                // a GUI backup could run concurrently with the 03:00 timer or a
                // live scrub, and MountGuard::Drop could unmount a target out
                // from under a running `btrfs receive`. bd DAS-Backup-Manager-pe6
                // fixed this for main.rs in 0.7.15.0 and never reached the
                // daemon the GUI actually calls (bd DAS-Backup-Manager-dca).
                let _locks = match backup::acquire_manual_locks(&progress) {
                    Ok(BackupLockAttempt::Acquired(locks)) => locks,
                    Ok(BackupLockAttempt::AlreadyRunning) => {
                        return Err("A backup is already running — declined".to_string());
                    }
                    Err(e) => return Err(format!("Could not acquire backup locks: {e}")),
                };
                let mut source_guard = mount::ensure_sources_mounted(&config, &progress);
                let mut guard = mount::ensure_targets_mounted(&config, &progress)
                    .map_err(|e| format!("Mount failed: {e}"))?;

                let res =
                    match backup::send_snapshots(&config, &sources, &targets, false, &progress) {
                        Ok((sent, bytes)) => Ok(format!("{sent} snapshots sent ({bytes} bytes)")),
                        Err(e) => Err(format!("Send failed: {e}")),
                    };

                guard.unmount(&progress);
                source_guard.unmount(&progress);
                res
            })
            .await
            .unwrap_or_else(|e| Err(format!("Send task panicked: {e}")));

            let (success, summary) = match result {
                Ok(msg) => (true, msg),
                Err(msg) => (false, msg),
            };

            emit_job_finished(&conn, &jid, success, &summary).await;
            jobs.lock().await.remove(&jid);
        });

        self.jobs
            .lock()
            .await
            .insert(job_id.clone(), (handle, cancel, sender.clone()));
        Ok(job_id)
    }

    /// Archive boot subvolumes.
    async fn backup_boot_archive(
        &self,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> fdo::Result<String> {
        let sender = sender_from_header(&header)?;
        check_polkit(&self.conn, &sender, "org.dasbackup.backup").await?;

        let config = load_config()?;
        let job_id = new_job_id();
        let cancel = CancelFlag::new();
        let progress = DbusProgress::new(self.conn.clone(), job_id.clone(), cancel.clone());
        let jobs = self.jobs.clone();
        let jid = job_id.clone();
        let conn = self.conn.clone();

        let handle = tokio::spawn(async move {
            let result: Result<String, String> = tokio::task::spawn_blocking(move || {
                // Join the same two-lock interlock as the scheduled path and the
                // CLI: singleton (non-blocking — a second backup is redundant,
                // not late) then the shared maintenance lock (blocking — a scrub
                // is a peer operation and this should wait for it). Without it
                // a GUI backup could run concurrently with the 03:00 timer or a
                // live scrub, and MountGuard::Drop could unmount a target out
                // from under a running `btrfs receive`. bd DAS-Backup-Manager-pe6
                // fixed this for main.rs in 0.7.15.0 and never reached the
                // daemon the GUI actually calls (bd DAS-Backup-Manager-dca).
                let _locks = match backup::acquire_manual_locks(&progress) {
                    Ok(BackupLockAttempt::Acquired(locks)) => locks,
                    Ok(BackupLockAttempt::AlreadyRunning) => {
                        return Err("A backup is already running — declined".to_string());
                    }
                    Err(e) => return Err(format!("Could not acquire backup locks: {e}")),
                };
                let mut guard = mount::ensure_targets_mounted(&config, &progress)
                    .map_err(|e| format!("Mount failed: {e}"))?;

                let res = match backup::archive_boot(&config, &progress) {
                    Ok(archived) => {
                        let msg = if archived {
                            "Boot subvolumes archived"
                        } else {
                            "No boot subvolumes to archive"
                        };
                        Ok(msg.to_string())
                    }
                    Err(e) => Err(format!("Boot archive failed: {e}")),
                };

                guard.unmount(&progress);
                res
            })
            .await
            .unwrap_or_else(|e| Err(format!("Boot archive task panicked: {e}")));

            let (success, summary) = match result {
                Ok(msg) => (true, msg),
                Err(msg) => (false, msg),
            };

            emit_job_finished(&conn, &jid, success, &summary).await;
            jobs.lock().await.remove(&jid);
        });

        self.jobs
            .lock()
            .await
            .insert(job_id.clone(), (handle, cancel, sender.clone()));
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
        let cancel = CancelFlag::new();
        let progress = DbusProgress::new(self.conn.clone(), job_id.clone(), cancel.clone());
        let jobs = self.jobs.clone();
        let jid = job_id.clone();
        let conn = self.conn.clone();
        let target_path = target_path.to_owned();
        let db_path = config.general.db_path.clone();

        let handle = tokio::spawn(async move {
            let result: Result<String, String> = tokio::task::spawn_blocking(move || {
                let mut guard = mount::ensure_targets_mounted(&config, &progress)
                    .map_err(|e| format!("Mount failed: {e}"))?;

                let db = Database::open(&db_path).map_err(|e| format!("DB open failed: {e}"))?;

                // Collect target paths to walk (detect udisks2 mounts too)
                let paths: Vec<String> = if target_path.is_empty() {
                    config
                        .targets
                        .iter()
                        .filter_map(|t| {
                            health::find_any_mount(&t.mount, &t.serial, &t.role)
                        })
                        .collect()
                } else {
                    vec![target_path]
                };

                let mut total_discovered = 0usize;
                let mut total_indexed = 0usize;
                let mut total_skipped = 0usize;
                let mut errors = Vec::new();

                progress.on_stage("Indexing targets", paths.len() as u64);
                for (i, path) in paths.iter().enumerate() {
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

                guard.unmount(&progress);

                if !errors.is_empty() && total_indexed == 0 {
                    Err(format!("Indexing failed: {}", errors.join("; ")))
                } else {
                    let mut msg = format!(
                        "Indexed {total_indexed} new snapshots ({total_discovered} discovered, {total_skipped} skipped)"
                    );
                    if !errors.is_empty() {
                        msg.push_str(&format!(" [warnings: {}]", errors.join("; ")));
                    }
                    Ok(msg)
                }
            })
            .await
            .unwrap_or_else(|e| Err(format!("Indexing task panicked: {e}")));

            let (success, summary) = match result {
                Ok(msg) => (true, msg),
                Err(msg) => (false, msg),
            };

            emit_job_finished(&conn, &jid, success, &summary).await;
            jobs.lock().await.remove(&jid);
        });

        self.jobs
            .lock()
            .await
            .insert(job_id.clone(), (handle, cancel, sender.clone()));
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
            let arr: Vec<serde_json::Value> = runs
                .iter()
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
                        "errors": &r.errors,
                    })
                })
                .collect();
            Ok(serde_json::to_string(&arr).unwrap_or_else(|_| "[]".to_string()))
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
        let cancel = CancelFlag::new();
        let progress = DbusProgress::new(self.conn.clone(), job_id.clone(), cancel.clone());
        let jobs = self.jobs.clone();
        let jid = job_id.clone();
        let conn = self.conn.clone();
        let snapshot = snapshot.to_owned();
        let dest = dest.to_owned();

        let handle = tokio::spawn(async move {
            let result: Result<(bool, String), String> = tokio::task::spawn_blocking(move || {
                let mut guard = mount::ensure_targets_mounted(&config, &progress)
                    .map_err(|e| format!("Mount failed: {e}"))?;

                let file_refs: Vec<&str> = files.iter().map(|s| s.as_str()).collect();
                let res = match restore::restore_files(
                    Path::new(&snapshot),
                    &file_refs,
                    Path::new(&dest),
                    &config.restore.allowed_roots,
                    &restore::snapshot_source_roots(&config),
                    &progress,
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
                };

                guard.unmount(&progress);
                res
            })
            .await
            .unwrap_or_else(|e| Err(format!("Restore task panicked: {e}")));

            let (success, summary) = match result {
                Ok((s, msg)) => (s, msg),
                Err(msg) => (false, msg),
            };

            emit_job_finished(&conn, &jid, success, &summary).await;
            jobs.lock().await.remove(&jid);
        });

        self.jobs
            .lock()
            .await
            .insert(job_id.clone(), (handle, cancel, sender.clone()));
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
        let cancel = CancelFlag::new();
        let progress = DbusProgress::new(self.conn.clone(), job_id.clone(), cancel.clone());
        let jobs = self.jobs.clone();
        let jid = job_id.clone();
        let conn = self.conn.clone();
        let snapshot = snapshot.to_owned();
        let dest = dest.to_owned();

        let handle = tokio::spawn(async move {
            let result: Result<(bool, String), String> = tokio::task::spawn_blocking(move || {
                let mut guard = mount::ensure_targets_mounted(&config, &progress)
                    .map_err(|e| format!("Mount failed: {e}"))?;

                let res = match restore::restore_snapshot(
                    Path::new(&snapshot),
                    Path::new(&dest),
                    &config.restore.allowed_roots,
                    &restore::snapshot_source_roots(&config),
                    &progress,
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
                };

                guard.unmount(&progress);
                res
            })
            .await
            .unwrap_or_else(|e| Err(format!("Snapshot restore task panicked: {e}")));

            let (success, summary) = match result {
                Ok((s, msg)) => (s, msg),
                Err(msg) => (false, msg),
            };

            emit_job_finished(&conn, &jid, success, &summary).await;
            jobs.lock().await.remove(&jid);
        });

        self.jobs
            .lock()
            .await
            .insert(job_id.clone(), (handle, cancel, sender.clone()));
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

        save_config(&config)
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

        let mut config = load_config()?;

        let inc = if incremental.is_empty() {
            None
        } else {
            Some(incremental)
        };
        let f = if full.is_empty() { None } else { Some(full) };
        let d = if delay == 0 { None } else { Some(delay) };

        schedule::set_schedule(&mut config, inc, f, d)
            .map_err(|e| fdo::Error::Failed(format!("Failed to set schedule: {e}")))?;

        save_config(&config)
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

        let mut config = load_config()?;
        subvol::add_subvolume(&mut config, source, name, false).map_err(fdo::Error::Failed)?;
        save_config(&config)
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

        let mut config = load_config()?;
        subvol::remove_subvolume(&mut config, source, name).map_err(fdo::Error::Failed)?;
        save_config(&config)
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

        let mut config = load_config()?;
        subvol::set_manual(&mut config, source, name, manual).map_err(fdo::Error::Failed)?;
        save_config(&config)
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

    /// Cancel a running job.
    async fn job_cancel(
        &self,
        #[zbus(header)] header: zbus::message::Header<'_>,
        job_id: &str,
    ) -> fdo::Result<bool> {
        let sender = sender_from_header(&header)?;
        check_polkit(&self.conn, &sender, "org.dasbackup.backup").await?;

        let mut jobs = self.jobs.lock().await;
        // Ownership is a property of the JOB, and polkit only answered a
        // question about the caller. Check both.
        match jobs.get(job_id) {
            None => Ok(false),
            Some((_, _, owner)) if *owner != sender => Err(fdo::Error::AccessDenied(format!(
                "Job '{job_id}' belongs to another client"
            ))),
            Some(_) => {
                let (handle, cancel, _) = jobs.remove(job_id).expect("checked present above");
                cancel.cancel();
                handle.abort();
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
            let _ = tokio::task::spawn_blocking(move || -> Result<(), String> {
                let db = Database::open(&path_inner).map_err(|e| format!("open: {e}"))?;
                let stats = db.get_stats().map_err(|e| format!("stats: {e}"))?;
                let meta = std::fs::metadata(&path_inner).map_err(|e| format!("stat: {e}"))?;
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
                cache_inner.blocking_lock().insert(
                    path_inner.clone(),
                    StatsCacheEntry {
                        db_mtime_nanos: mtime_nanos,
                        db_size_bytes,
                        json,
                    },
                );
                Ok(())
            })
            .await;
            in_flight.lock().await.remove(&db_path);
        });
    }
}

// ---------------------------------------------------------------------------
// Helper functions
// ---------------------------------------------------------------------------

/// Extract the sender bus name from a D-Bus message header.
fn sender_from_header(header: &zbus::message::Header<'_>) -> Result<String, fdo::Error> {
    header
        .sender()
        .map(|s| s.to_string())
        .ok_or_else(|| fdo::Error::Failed("Missing sender in D-Bus message header".to_string()))
}

/// Emit a JobFinished signal from outside the interface method context.
async fn emit_job_finished(conn: &Connection, job_id: &str, success: bool, summary: &str) {
    let iface_ref = conn
        .object_server()
        .interface::<_, HelperInterface>("/org/dasbackup/Helper1")
        .await;
    if let Ok(iface) = iface_ref {
        let ctxt = iface.signal_emitter();
        let _ = HelperInterface::job_finished(ctxt, job_id, success, summary).await;
    }
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let jobs: JobMap = Arc::new(Mutex::new(HashMap::new()));

    // Build the system D-Bus connection and serve the interface.
    let conn = Builder::system()?
        .name("org.dasbackup.Helper1")?
        .build()
        .await?;

    let iface = HelperInterface {
        jobs: jobs.clone(),
        conn: conn.clone(),
        stats_cache: Arc::new(Mutex::new(HashMap::new())),
        stats_refresh_in_flight: Arc::new(Mutex::new(std::collections::HashSet::new())),
    };

    conn.object_server()
        .at("/org/dasbackup/Helper1", iface)
        .await?;

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
        for (id, (handle, cancel, _owner)) in entries {
            eprintln!("btrdasd-helper: cancelling job {id}");
            cancel.cancel();
            handle.abort();
        }
    }

    eprintln!("btrdasd-helper: shutdown complete");
    Ok(())
}
