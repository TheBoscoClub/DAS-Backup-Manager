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

use buttered_dasd::backup::{self, BackupMode, BackupOptions};
use buttered_dasd::config::Config;
use buttered_dasd::db::Database;
use buttered_dasd::health;
use buttered_dasd::indexer;
use buttered_dasd::mount;
use buttered_dasd::progress::{LogLevel, ProgressCallback};
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

type JobMap = Arc<Mutex<HashMap<String, (JoinHandle<()>, CancelFlag)>>>;

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
        let percent = if total > 0 {
            ((current * 100) / total).min(100) as i32
        } else {
            0i32
        };
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

fn load_config(config_path: &str) -> Result<Config, fdo::Error> {
    Config::load(Path::new(config_path))
        .map_err(|e| fdo::Error::Failed(format!("Failed to load config '{config_path}': {e}")))
}

fn save_config(config: &Config, path: &str) -> Result<(), fdo::Error> {
    config
        .save(Path::new(path))
        .map_err(|e| fdo::Error::Failed(format!("Failed to save config '{path}': {e}")))
}

// ---------------------------------------------------------------------------
// D-Bus interface
// ---------------------------------------------------------------------------

struct HelperInterface {
    jobs: JobMap,
    conn: Connection,
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
        config_path: &str,
        mode: &str,
        sources: Vec<String>,
        targets: Vec<String>,
        dry_run: bool,
    ) -> fdo::Result<String> {
        let sender = sender_from_header(&header)?;
        check_polkit(&self.conn, &sender, "org.dasbackup.backup").await?;

        let config = load_config(config_path)?;
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
                let mut guard = mount::ensure_targets_mounted(&config, &progress)
                    .map_err(|e| format!("Mount failed: {e}"))?;

                let res = match backup::run_backup(&config, &options, &progress) {
                    Ok(r) => Ok((
                        r.success,
                        format!(
                            "Backup complete: {} snapshots created, {} sent",
                            r.snapshots_created, r.snapshots_sent
                        ),
                    )),
                    Err(e) => Err(format!("Backup failed: {e}")),
                };

                guard.unmount(&progress);
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
            .insert(job_id.clone(), (handle, cancel));
        Ok(job_id)
    }

    /// Create snapshots only.
    async fn backup_snapshot(
        &self,
        #[zbus(header)] header: zbus::message::Header<'_>,
        config_path: &str,
        sources: Vec<String>,
    ) -> fdo::Result<String> {
        let sender = sender_from_header(&header)?;
        check_polkit(&self.conn, &sender, "org.dasbackup.backup").await?;

        let config = load_config(config_path)?;
        let job_id = new_job_id();
        let cancel = CancelFlag::new();
        let progress = DbusProgress::new(self.conn.clone(), job_id.clone(), cancel.clone());
        let jobs = self.jobs.clone();
        let jid = job_id.clone();
        let conn = self.conn.clone();

        let handle = tokio::spawn(async move {
            let result: Result<String, String> = tokio::task::spawn_blocking(move || {
                match backup::create_snapshots(&config, &sources, &progress) {
                    Ok(n) => Ok(format!("{n} snapshots created")),
                    Err(e) => Err(format!("Snapshot failed: {e}")),
                }
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
            .insert(job_id.clone(), (handle, cancel));
        Ok(job_id)
    }

    /// Send existing snapshots to targets.
    async fn backup_send(
        &self,
        #[zbus(header)] header: zbus::message::Header<'_>,
        config_path: &str,
        targets: Vec<String>,
    ) -> fdo::Result<String> {
        let sender = sender_from_header(&header)?;
        check_polkit(&self.conn, &sender, "org.dasbackup.backup").await?;

        let config = load_config(config_path)?;
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
                let mut guard = mount::ensure_targets_mounted(&config, &progress)
                    .map_err(|e| format!("Mount failed: {e}"))?;

                let res = match backup::send_snapshots(&config, &sources, &targets, &progress) {
                    Ok((sent, bytes)) => Ok(format!("{sent} snapshots sent ({bytes} bytes)")),
                    Err(e) => Err(format!("Send failed: {e}")),
                };

                guard.unmount(&progress);
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
            .insert(job_id.clone(), (handle, cancel));
        Ok(job_id)
    }

    /// Archive boot subvolumes.
    async fn backup_boot_archive(
        &self,
        #[zbus(header)] header: zbus::message::Header<'_>,
        config_path: &str,
    ) -> fdo::Result<String> {
        let sender = sender_from_header(&header)?;
        check_polkit(&self.conn, &sender, "org.dasbackup.backup").await?;

        let config = load_config(config_path)?;
        let job_id = new_job_id();
        let cancel = CancelFlag::new();
        let progress = DbusProgress::new(self.conn.clone(), job_id.clone(), cancel.clone());
        let jobs = self.jobs.clone();
        let jid = job_id.clone();
        let conn = self.conn.clone();

        let handle = tokio::spawn(async move {
            let result: Result<String, String> = tokio::task::spawn_blocking(move || {
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
            .insert(job_id.clone(), (handle, cancel));
        Ok(job_id)
    }

    /// Walk backup targets and index new snapshots.
    ///
    /// If `target_path` is empty, walks ALL mounted config targets.
    /// Otherwise walks just the specified path (backwards compat).
    async fn index_walk(
        &self,
        #[zbus(header)] header: zbus::message::Header<'_>,
        config_path: &str,
        target_path: &str,
        db_path: &str,
    ) -> fdo::Result<String> {
        let sender = sender_from_header(&header)?;
        check_polkit(&self.conn, &sender, "org.dasbackup.index").await?;

        let config = load_config(config_path)?;
        let job_id = new_job_id();
        let cancel = CancelFlag::new();
        let progress = DbusProgress::new(self.conn.clone(), job_id.clone(), cancel.clone());
        let jobs = self.jobs.clone();
        let jid = job_id.clone();
        let conn = self.conn.clone();
        let target_path = target_path.to_owned();
        let db_path = db_path.to_owned();

        let handle = tokio::spawn(async move {
            let result: Result<String, String> = tokio::task::spawn_blocking(move || {
                let mut guard = mount::ensure_targets_mounted(&config, &progress)
                    .map_err(|e| format!("Mount failed: {e}"))?;

                let db = Database::open(&db_path).map_err(|e| format!("DB open failed: {e}"))?;

                // Collect target paths to walk
                let paths: Vec<String> = if target_path.is_empty() {
                    config
                        .targets
                        .iter()
                        .filter(|t| health::is_mountpoint(Path::new(&t.mount)))
                        .map(|t| t.mount.clone())
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
            .insert(job_id.clone(), (handle, cancel));
        Ok(job_id)
    }

    // ---- Index read methods (synchronous, polkit: org.dasbackup.index.read) ----

    /// Return JSON stats: {snapshots, files, spans, db_size_bytes}.
    async fn index_stats(
        &self,
        #[zbus(header)] header: zbus::message::Header<'_>,
        db_path: &str,
    ) -> fdo::Result<String> {
        let sender = sender_from_header(&header)?;
        check_polkit(&self.conn, &sender, "org.dasbackup.index.read").await?;

        let db = Database::open(db_path)
            .map_err(|e| fdo::Error::Failed(format!("DB open failed: {e}")))?;
        let stats = db
            .get_stats()
            .map_err(|e| fdo::Error::Failed(format!("Stats query failed: {e}")))?;
        let db_size_bytes = std::fs::metadata(db_path).map(|m| m.len()).unwrap_or(0);
        let json = serde_json::json!({
            "snapshots": stats.snapshot_count,
            "files": stats.file_count,
            "spans": stats.span_count,
            "db_size_bytes": db_size_bytes,
        });
        Ok(json.to_string())
    }

    /// Return JSON array of all snapshots.
    async fn index_list_snapshots(
        &self,
        #[zbus(header)] header: zbus::message::Header<'_>,
        db_path: &str,
    ) -> fdo::Result<String> {
        let sender = sender_from_header(&header)?;
        check_polkit(&self.conn, &sender, "org.dasbackup.index.read").await?;

        let db = Database::open(db_path)
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
    }

    /// Return JSON array of files in a given snapshot.
    async fn index_list_files(
        &self,
        #[zbus(header)] header: zbus::message::Header<'_>,
        db_path: &str,
        snapshot_id: i64,
    ) -> fdo::Result<String> {
        let sender = sender_from_header(&header)?;
        check_polkit(&self.conn, &sender, "org.dasbackup.index.read").await?;

        let db = Database::open(db_path)
            .map_err(|e| fdo::Error::Failed(format!("DB open failed: {e}")))?;
        let files = db
            .get_files_in_snapshot(snapshot_id)
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
        Ok(serde_json::to_string(&arr).unwrap_or_else(|_| "[]".to_string()))
    }

    /// FTS5 search returning JSON array of matches.
    async fn index_search(
        &self,
        #[zbus(header)] header: zbus::message::Header<'_>,
        db_path: &str,
        query: &str,
        limit: i64,
    ) -> fdo::Result<String> {
        let sender = sender_from_header(&header)?;
        check_polkit(&self.conn, &sender, "org.dasbackup.index.read").await?;

        let db = Database::open(db_path)
            .map_err(|e| fdo::Error::Failed(format!("DB open failed: {e}")))?;
        let results = db
            .search(query, limit)
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
    }

    /// Return JSON array of recent backup history.
    async fn index_backup_history(
        &self,
        #[zbus(header)] header: zbus::message::Header<'_>,
        db_path: &str,
        limit: i64,
    ) -> fdo::Result<String> {
        let sender = sender_from_header(&header)?;
        check_polkit(&self.conn, &sender, "org.dasbackup.index.read").await?;

        let db = Database::open(db_path)
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
    }

    /// Return the filesystem path for a snapshot by ID.
    async fn index_snapshot_path(
        &self,
        #[zbus(header)] header: zbus::message::Header<'_>,
        db_path: &str,
        snapshot_id: i64,
    ) -> fdo::Result<String> {
        let sender = sender_from_header(&header)?;
        check_polkit(&self.conn, &sender, "org.dasbackup.index.read").await?;

        let db = Database::open(db_path)
            .map_err(|e| fdo::Error::Failed(format!("DB open failed: {e}")))?;
        let path = db
            .snapshot_path_by_id(snapshot_id)
            .map_err(|e| fdo::Error::Failed(format!("Path query failed: {e}")))?
            .ok_or_else(|| fdo::Error::Failed(format!("No snapshot with id {snapshot_id}")))?;
        Ok(path)
    }

    /// Restore specific files from a snapshot.
    async fn restore_files(
        &self,
        #[zbus(header)] header: zbus::message::Header<'_>,
        config_path: &str,
        snapshot: &str,
        dest: &str,
        files: Vec<String>,
    ) -> fdo::Result<String> {
        let sender = sender_from_header(&header)?;
        check_polkit(&self.conn, &sender, "org.dasbackup.restore").await?;

        let config = load_config(config_path)?;
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
            .insert(job_id.clone(), (handle, cancel));
        Ok(job_id)
    }

    /// Restore an entire snapshot to a destination.
    async fn restore_snapshot(
        &self,
        #[zbus(header)] header: zbus::message::Header<'_>,
        config_path: &str,
        snapshot: &str,
        dest: &str,
    ) -> fdo::Result<String> {
        let sender = sender_from_header(&header)?;
        check_polkit(&self.conn, &sender, "org.dasbackup.restore").await?;

        let config = load_config(config_path)?;
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
            .insert(job_id.clone(), (handle, cancel));
        Ok(job_id)
    }

    // ---- Synchronous methods ----

    /// Get the raw TOML config as a string.
    async fn config_get(
        &self,
        #[zbus(header)] header: zbus::message::Header<'_>,
        config_path: &str,
    ) -> fdo::Result<String> {
        let sender = sender_from_header(&header)?;
        check_polkit(&self.conn, &sender, "org.dasbackup.config").await?;

        let config = load_config(config_path)?;
        config
            .to_toml()
            .map_err(|e| fdo::Error::Failed(format!("Failed to serialize config: {e}")))
    }

    /// Write a TOML config string to disk (validates first).
    async fn config_set(
        &self,
        #[zbus(header)] header: zbus::message::Header<'_>,
        config_path: &str,
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

        save_config(&config, config_path)
    }

    /// Get the current backup schedule as JSON.
    async fn schedule_get(
        &self,
        #[zbus(header)] header: zbus::message::Header<'_>,
        config_path: &str,
    ) -> fdo::Result<String> {
        let sender = sender_from_header(&header)?;
        check_polkit(&self.conn, &sender, "org.dasbackup.config").await?;

        let config = load_config(config_path)?;
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
        config_path: &str,
        incremental: &str,
        full: &str,
        delay: u32,
    ) -> fdo::Result<()> {
        let sender = sender_from_header(&header)?;
        check_polkit(&self.conn, &sender, "org.dasbackup.config").await?;

        let mut config = load_config(config_path)?;

        let inc = if incremental.is_empty() {
            None
        } else {
            Some(incremental)
        };
        let f = if full.is_empty() { None } else { Some(full) };
        let d = if delay == 0 { None } else { Some(delay) };

        schedule::set_schedule(&mut config, inc, f, d)
            .map_err(|e| fdo::Error::Failed(format!("Failed to set schedule: {e}")))?;

        save_config(&config, config_path)
    }

    /// Enable or disable scheduled backups.
    async fn schedule_enable(
        &self,
        #[zbus(header)] header: zbus::message::Header<'_>,
        config_path: &str,
        enabled: bool,
    ) -> fdo::Result<()> {
        let sender = sender_from_header(&header)?;
        check_polkit(&self.conn, &sender, "org.dasbackup.config").await?;

        let config = load_config(config_path)?;
        schedule::set_enabled(&config, enabled)
            .map_err(|e| fdo::Error::Failed(format!("Failed to set schedule enabled: {e}")))
    }

    /// Add a subvolume to a source.
    async fn subvol_add(
        &self,
        #[zbus(header)] header: zbus::message::Header<'_>,
        config_path: &str,
        source: &str,
        name: &str,
    ) -> fdo::Result<()> {
        let sender = sender_from_header(&header)?;
        check_polkit(&self.conn, &sender, "org.dasbackup.config").await?;

        let mut config = load_config(config_path)?;
        subvol::add_subvolume(&mut config, source, name, false).map_err(fdo::Error::Failed)?;
        save_config(&config, config_path)
    }

    /// Remove a subvolume from a source.
    async fn subvol_remove(
        &self,
        #[zbus(header)] header: zbus::message::Header<'_>,
        config_path: &str,
        source: &str,
        name: &str,
    ) -> fdo::Result<()> {
        let sender = sender_from_header(&header)?;
        check_polkit(&self.conn, &sender, "org.dasbackup.config").await?;

        let mut config = load_config(config_path)?;
        subvol::remove_subvolume(&mut config, source, name).map_err(fdo::Error::Failed)?;
        save_config(&config, config_path)
    }

    /// Set the manual_only flag on a subvolume.
    async fn subvol_set_manual(
        &self,
        #[zbus(header)] header: zbus::message::Header<'_>,
        config_path: &str,
        source: &str,
        name: &str,
        manual: bool,
    ) -> fdo::Result<()> {
        let sender = sender_from_header(&header)?;
        check_polkit(&self.conn, &sender, "org.dasbackup.config").await?;

        let mut config = load_config(config_path)?;
        subvol::set_manual(&mut config, source, name, manual).map_err(fdo::Error::Failed)?;
        save_config(&config, config_path)
    }

    /// Query system health and return a JSON report.
    ///
    /// Auto-mounts targets first so disk space, SMART, and snapshot data are
    /// available, then unmounts any targets this call mounted.
    async fn health_query(
        &self,
        #[zbus(header)] header: zbus::message::Header<'_>,
        config_path: &str,
    ) -> fdo::Result<String> {
        let sender = sender_from_header(&header)?;
        check_polkit(&self.conn, &sender, "org.dasbackup.health").await?;

        let config = load_config(config_path)?;

        // Run the entire health query (blocking I/O: smartctl, btrfs, mount)
        // inside spawn_blocking with auto-mount.
        let json_str = tokio::task::spawn_blocking(move || -> Result<String, String> {
            let progress = buttered_dasd::progress::NullProgress;

            // Auto-mount targets (only mounts what isn't already mounted)
            let mut guard = mount::ensure_targets_mounted(&config, &progress)
                .map_err(|e| format!("Mount failed: {e}"))?;

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
                    })
                })
                .collect();

            // Build growth data grouped by target label
            let mut growth_map: std::collections::BTreeMap<String, Vec<serde_json::Value>> =
                std::collections::BTreeMap::new();
            for gp in &report.growth_points {
                let (y, m, d) = health::days_to_ymd(gp.timestamp / 86400);
                let date_str = format!("{y:04}-{m:02}-{d:02}");
                growth_map
                    .entry(gp.target_label.clone())
                    .or_default()
                    .push(serde_json::json!({
                        "date": date_str,
                        "used_bytes": gp.used_bytes,
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
                "services": {
                    "btrbk_available": btrbk_available,
                    "timer_enabled": timer_enabled,
                    "timer_next": timer_next,
                    "last_backup": report.last_backup,
                    "last_backup_age_secs": last_backup_age_secs,
                    "drives_mounted": drives_mounted,
                },
            });

            // Unmount targets this call mounted
            guard.unmount(&progress);

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
        if let Some((handle, cancel)) = jobs.remove(job_id) {
            cancel.cancel();
            handle.abort();
            Ok(true)
        } else {
            Ok(false)
        }
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
    };

    conn.object_server()
        .at("/org/dasbackup/Helper1", iface)
        .await?;

    eprintln!("btrdasd-helper: listening on system bus as org.dasbackup.Helper1");

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
        let entries: Vec<(String, (JoinHandle<()>, CancelFlag))> = active_jobs.drain().collect();
        for (id, (handle, cancel)) in entries {
            eprintln!("btrdasd-helper: cancelling job {id}");
            cancel.cancel();
            handle.abort();
        }
    }

    eprintln!("btrdasd-helper: shutdown complete");
    Ok(())
}
