// Installer module — install, uninstall, upgrade, and check modes.
// Orchestrates config saving, template generation, file writing, and manifest tracking.

#![allow(dead_code)]

use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::time::Duration;

use buttered_dasd::maintenance::MaintenanceHeld;
use buttered_dasd::scrub::FileLock;

use crate::setup::config::Config;
use crate::setup::templates::GeneratedFiles;

const CONFIG_FILE: &str = "/etc/das-backup/config.toml";
const MANIFEST_FILE: &str = "/etc/das-backup/.manifest";

const SYSTEMCTL: &str = "systemctl";

/// How the installer reaches the host's service manager: one `systemctl`
/// invocation per call, given its arguments. A parameter rather than a direct
/// call so the decisions built on it — which units, in which order, and what a
/// failure does to the result — can be exercised without touching the host.
type UnitRunner<'a> = &'a dyn Fn(&[&str]) -> Result<(), String>;

/// Whether the configured mail relay answers. See `relay_reachable`.
type RelayProbe<'a> = &'a dyn Fn(&Config) -> bool;

/// Regenerate and install every managed file for a config, under setup's
/// locks — `install` on the host, a recorder in tests.
pub type Installer<'a> = &'a dyn Fn(&Config, &SetupLocks) -> Result<(), Box<dyn std::error::Error>>;

/// Run a command for its exit status, returning a description of the failure
/// instead of discarding it.
///
/// Every `systemctl` call site used to be
/// `let _ = Command::new("systemctl")...status()`, so `install()` returned
/// `Ok(())` whether or not a single timer had been enabled. Installing the
/// schedule is the entire purpose of the command: a masked unit, a malformed
/// generated unit, or systemctl being unavailable produced a clean "install
/// complete" and **no scheduled backup, no scheduled scrub, and no drift check
/// ever running**, with nothing to surface it until someone noticed the
/// absence of the 03:00 report.
/// bd DAS-Backup-Manager-nsp (finding #5).
fn command_status(program: &str, args: &[&str]) -> Result<(), String> {
    match std::process::Command::new(program).args(args).status() {
        Ok(st) if st.success() => Ok(()),
        Ok(st) => Err(format!("{program} {} exited with {}", args.join(" "), st)),
        Err(e) => Err(format!(
            "{program} {} could not be run: {e}",
            args.join(" ")
        )),
    }
}

/// `command_status`, given at most `limit` to return. A command still running
/// then is killed and reaped, and reported as not having returned. Only the
/// client is killed: work it asked a service to do — systemd's restart job —
/// goes on without it.
fn command_status_within(program: &str, args: &[&str], limit: Duration) -> Result<(), String> {
    let command = std::iter::once(program)
        .chain(args.iter().copied())
        .collect::<Vec<_>>()
        .join(" ");
    let mut child = std::process::Command::new(program)
        .args(args)
        .spawn()
        .map_err(|e| format!("{command} could not be run: {e}"))?;
    let deadline = std::time::Instant::now() + limit;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(status)) => return Err(format!("{command} exited with {status}")),
            Ok(None) if std::time::Instant::now() >= deadline => {
                // Already returning an error; a kill that fails means it exited.
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "{command} did not return within {} s",
                    limit.as_secs()
                ));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(e) => return Err(format!("{command}: cannot wait for it: {e}")),
        }
    }
}

/// Run one `udevadm` verb, returning a description of the failure.
fn run_udevadm(args: &[&str]) -> Result<(), String> {
    command_status("udevadm", args)
}

/// Make the freshly written udisks-ignore rule take effect on devices that are
/// already attached. Not fatal — the rule file is in place and applies on the
/// next attach or boot — but never silent: until it applies, a desktop login
/// can still automount the targets.
fn apply_udev_rules() {
    let steps: [&[&str]; 2] = [
        &["control", "--reload"],
        &["trigger", "--action=change", "--subsystem-match=block"],
    ];
    for args in steps {
        if let Err(e) = run_udevadm(args) {
            eprintln!(
                "Warning: {e} — the udisks-ignore rule is installed but not yet \
                 applied to attached drives; it takes effect on the next boot"
            );
            return;
        }
    }
}

/// How one backup target appears to udisks2, per `udevadm info --export-db`.
#[derive(Debug, PartialEq, Eq)]
pub struct TargetExposure {
    pub label: String,
    /// Member devices carrying `UDISKS_IGNORE=1`.
    pub hidden: Vec<String>,
    /// Member devices udisks2 can see, and a desktop session can automount.
    pub exposed: Vec<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ExposureVerdict {
    Hidden,
    Exposed,
    /// No attached device matched. Says nothing about whether the rule works.
    NotAttached,
}

impl TargetExposure {
    pub fn verdict(&self) -> ExposureVerdict {
        if !self.exposed.is_empty() {
            ExposureVerdict::Exposed
        } else if self.hidden.is_empty() {
            ExposureVerdict::NotAttached
        } else {
            ExposureVerdict::Hidden
        }
    }
}

/// Sort every attached member device of every configured target into hidden
/// or exposed. A device belongs to a target when it holds a btrfs filesystem
/// and either its drive serial is one of the target's, or its filesystem UUID
/// is the target's `mount_uuid` — the same two identities the generated rule
/// matches on, read back from what udev actually applied.
pub fn udisks_exposure(export_db: &str, config: &Config) -> Vec<TargetExposure> {
    let mut found: Vec<TargetExposure> = config
        .targets
        .iter()
        .map(|t| TargetExposure {
            label: t.label.clone(),
            hidden: Vec::new(),
            exposed: Vec::new(),
        })
        .collect();

    for record in export_db.split("\n\n") {
        let prop = |key: &str| {
            record
                .lines()
                .filter_map(|l| l.strip_prefix("E: "))
                .find_map(|kv| kv.strip_prefix(key).and_then(|v| v.strip_prefix('=')))
        };
        if prop("SUBSYSTEM") != Some("block") || prop("ID_FS_TYPE") != Some("btrfs") {
            continue;
        }
        let Some(devname) = prop("DEVNAME") else {
            continue;
        };
        let serial = prop("ID_SERIAL_SHORT");
        let uuid = prop("ID_FS_UUID");
        let ignored = prop("UDISKS_IGNORE") == Some("1");

        for (target, slot) in config.targets.iter().zip(found.iter_mut()) {
            let by_serial =
                serial.is_some_and(|s| target.effective_serials().iter().any(|t| t == s));
            let by_uuid = uuid.is_some() && uuid == target.mount_uuid.as_deref();
            if by_serial || by_uuid {
                if ignored {
                    slot.hidden.push(devname.to_string());
                } else {
                    slot.exposed.push(devname.to_string());
                }
            }
        }
    }
    for slot in &mut found {
        slot.hidden.sort();
        slot.exposed.sort();
    }
    found
}

/// The BTRFS filesystem UUIDs udev reports on the drives carrying any of
/// `serials` — the identity a target without `mount_uuid` would be given.
/// Read from udev's database for the serials the config names; nothing is
/// enumerated or acted on. Only BTRFS is considered, so a recovery drive's
/// own ESP can never be offered.
fn btrfs_uuids_for_serials(export_db: &str, serials: &[String]) -> Vec<String> {
    let mut uuids: Vec<String> = Vec::new();
    for record in export_db.split("\n\n") {
        let prop = |key: &str| {
            record
                .lines()
                .filter_map(|l| l.strip_prefix("E: "))
                .find_map(|kv| kv.strip_prefix(key).and_then(|v| v.strip_prefix('=')))
        };
        if prop("SUBSYSTEM") != Some("block") || prop("ID_FS_TYPE") != Some("btrfs") {
            continue;
        }
        let ours = prop("ID_SERIAL_SHORT").is_some_and(|s| serials.iter().any(|t| t == s));
        if let (true, Some(uuid)) = (ours, prop("ID_FS_UUID"))
            && !uuids.iter().any(|u| u == uuid)
        {
            uuids.push(uuid.to_string());
        }
    }
    uuids
}

/// The lines `setup --check` prints about targets that have no `mount_uuid`
/// (bd DAS-Backup-Manager-9v2, -arx). Without one, the CLI and the GUI find
/// the target by drive serial only and verify only that its mount path is a
/// mount point, not which filesystem is mounted there. Where udev knows the
/// target's filesystem, the line to add is printed; `setup` never writes it
/// into an existing config by itself.
pub fn mount_uuid_report(export_db: &Result<String, String>, config: &Config) -> Vec<String> {
    let mut lines = Vec::new();
    for target in config
        .targets
        .iter()
        .filter(|t| t.mount_uuid.as_deref().is_none_or(str::is_empty))
    {
        lines.push(format!(
            "Target {} has no mount_uuid — found by drive serial only, and verified only \
             as a mount point, not as its filesystem",
            target.label
        ));
        let serials = target.effective_serials();
        let hint = match export_db {
            Err(e) => format!("  Its filesystem UUID could not be looked up: {e}"),
            Ok(db) => match btrfs_uuids_for_serials(db, &serials).as_slice() {
                [] => "  Its drive is not attached (or holds no BTRFS filesystem) — attach it \
                       and run setup --check again to read its UUID"
                    .to_string(),
                [uuid] => format!(
                    "  Its filesystem has UUID {uuid} — add  mount_uuid = \"{uuid}\"  to its \
                     [[target]] in config.toml"
                ),
                many => format!(
                    "  Its drives carry more than one BTRFS filesystem ({}) — set mount_uuid \
                     by hand",
                    many.join(", ")
                ),
            },
        };
        lines.push(hint);
    }
    lines
}

/// Run a command and return its stdout, or a description of why there is none.
/// A command that ran and failed is an error, never an empty answer.
fn command_stdout(program: &str, args: &[&str]) -> Result<String, String> {
    match std::process::Command::new(program).args(args).output() {
        Ok(out) if out.status.success() => Ok(String::from_utf8_lossy(&out.stdout).into_owned()),
        Ok(out) => Err(format!(
            "{program} {} exited with {}",
            args.join(" "),
            out.status
        )),
        Err(e) => Err(format!("{program} could not be run: {e}")),
    }
}

/// The lines `setup --check` prints about udisks visibility, given the result
/// of reading udev's database. A database that could not be read is reported
/// as not checked — it must never read as "nothing exposed".
pub fn udisks_report(export_db: Result<String, String>, config: &Config) -> Vec<String> {
    let db = match export_db {
        Ok(db) => db,
        Err(e) => return vec![format!("udisks visibility NOT checked: {e}")],
    };
    let mut lines = Vec::new();
    for t in udisks_exposure(&db, config) {
        match t.verdict() {
            ExposureVerdict::Hidden => lines.push(format!(
                "Target {} hidden from udisks ({})",
                t.label,
                t.hidden.join(", ")
            )),
            ExposureVerdict::Exposed => {
                lines.push(format!(
                    "Target {} EXPOSED to udisks — a desktop login can automount {}",
                    t.label,
                    t.exposed.join(", ")
                ));
                lines.push("  Fix with: sudo btrdasd setup --upgrade".to_string());
            }
            ExposureVerdict::NotAttached => lines.push(format!(
                "Target {} not attached — udisks visibility could not be checked",
                t.label
            )),
        }
    }
    lines
}

/// Install using system defaults (/etc, /). `_held`: the caller holds
/// setup's locks ([`SetupLocks`]) — this replaces the scripts and `btrbk.conf`
/// a running backup uses.
pub fn install(config: &Config, _held: &SetupLocks) -> Result<(), Box<dyn std::error::Error>> {
    install_to_prefix(
        config,
        Path::new("/"),
        Path::new(CONFIG_FILE),
        Path::new(MANIFEST_FILE),
    )?;
    apply_udev_rules();
    // Only in real installs — `install_to_prefix` never touches the host's units.
    install_schedule(config, &|args| command_status(SYSTEMCTL, args))
}

/// The `systemctl` invocations that put the schedule in place for `config`, in
/// the order they must run. Empty on any init system but systemd: there the
/// schedule is the generated cron entry, and there is no unit to enable.
fn schedule_unit_operations(config: &Config) -> Vec<Vec<&'static str>> {
    if config.init.system == crate::setup::config::InitSystem::Systemd {
        // das-scrub.service/.timer are always generated and installed (see
        // GeneratedFiles::generate), but the timer is only *enabled* when
        // `[scrub].enabled = true`. The scrub engine itself ignores
        // `enabled` for manual `btrdasd scrub run` invocations (warn only)
        // — the timer is the sole enforcement point for the schedule
        // (bd DAS-Backup-Manager-atq). Explicitly disable on the false path
        // too (not just "skip enabling") so a later `enabled = true -> false`
        // edit followed by `setup --upgrade` actually turns the timer off —
        // `enable --now`/`disable --now` are both idempotent no-ops if the
        // unit is already in the target state.
        let scrub_verb = if config.scrub.enabled {
            "enable"
        } else {
            "disable"
        };

        // das-backup-doctor.timer is always generated and always enabled —
        // unlike das-scrub, there is no `[doctor].enabled` config toggle
        // (bd DAS-Backup-Manager-01u). Rationale: the drift check is a fast,
        // read-mostly scan (mount + `btrfs subvolume list` + compare), not a
        // resource-intensive operation like a multi-hour scrub pass, so there
        // is no meaningful cost an operator would want to opt out of — and a
        // drift detector that's off by default defeats its own purpose (the
        // whole feature exists because the 2026-05-17 audit found ~30
        // subvolumes silently unbacked-up for months; an opt-in check would
        // have caught none of them any sooner than a human remembering to
        // look). If a future need for disabling it emerges, add
        // `[doctor].enabled` and gate this the same way scrub is gated above.
        vec![
            vec!["daemon-reload"],
            vec!["enable", "--now", "das-backup.timer"],
            vec!["enable", "--now", "das-backup-full.timer"],
            vec![scrub_verb, "--now", "das-scrub.timer"],
            vec!["enable", "--now", "das-backup-doctor.timer"],
        ]
    } else {
        Vec::new()
    }
}

/// Enable (or, for a disabled scrub, disable) the timers `config` calls for.
///
/// Every operation is attempted even after one fails, and the failures are
/// collected rather than discarded — see `command_status`.
fn install_schedule(
    config: &Config,
    systemctl: UnitRunner,
) -> Result<(), Box<dyn std::error::Error>> {
    let unit_errors: Vec<String> = schedule_unit_operations(config)
        .iter()
        .filter_map(|op| systemctl(op).err())
        .collect();

    // A schedule that was not installed is not an install that succeeded.
    // This is the entire point of finding #5: the command's purpose is to
    // put the timers in place, so failing to do so must not be reported as
    // success. Listed individually because "one timer failed" and "systemd
    // is unreachable" need different operator responses.
    if !unit_errors.is_empty() {
        for e in &unit_errors {
            eprintln!("ERROR: {e}");
        }
        return Err(format!(
            "{} systemd unit operation(s) failed — the backup schedule is NOT fully installed",
            unit_errors.len()
        )
        .into());
    }

    Ok(())
}

/// Install with a custom root prefix — the core of [`install`], which runs it
/// on `/` under setup's locks; tests run it on a scratch tree. Private, so
/// nothing outside this module writes the installed files without the proof
/// of the locks.
fn install_to_prefix(
    config: &Config,
    root: &Path,
    config_path: &Path,
    manifest_path: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    // Save config
    config.save(config_path)?;

    // What the PREVIOUS install put on disk. Needed to spot files that have
    // dropped out of the generated set (bd DAS-Backup-Manager-e23).
    let previous: Vec<String> = std::fs::read_to_string(manifest_path)
        .map(|t| {
            t.lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();

    // mtime of the running binary, for the staleness check below.
    let exe_mtime = std::env::current_exe()
        .ok()
        .and_then(|p| std::fs::metadata(p).ok())
        .and_then(|m| m.modified().ok());

    // Generate all files
    let generated = GeneratedFiles::generate(config);
    let mut manifest_entries = vec![config_path.to_string_lossy().to_string()];
    let mut skipped_newer: Vec<String> = Vec::new();

    for (rel_path, content) in &generated.files {
        let full_path = if rel_path.starts_with('/') {
            root.join(rel_path.strip_prefix('/').unwrap_or(rel_path.as_ref()))
        } else {
            root.join(rel_path)
        };

        if let Some(parent) = full_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        // Never overwrite an on-disk SCRIPT that is NEWER than the binary carrying
        // the embedded copy (bd DAS-Backup-Manager-2lj). Scripts are compiled in
        // via include_str!, so a btrdasd built before a script edit holds a stale
        // copy; `setup --upgrade` would then silently downgrade the file that
        // `cmake --install` had just refreshed. Skipping is safe in the normal
        // direction too: after a rebuild the binary is newer than the file, so a
        // genuine upgrade still writes.
        //
        // The guard is scoped to the embedded scripts and MUST NOT be widened
        // (bd DAS-Backup-Manager-bwt). Config-derived files — btrbk.conf, the
        // systemd units, the cron entry — are rendered from `config.toml`, so a
        // binary older than the file says nothing about whether the file is
        // current. Applying the mtime test to them inverted its polarity: every
        // successful upgrade stamps them with `now`, so the next upgrade refused
        // to rewrite them, and it refused exactly when a real config change was
        // waiting to be applied — while still printing "Upgrade complete".
        let is_stale_overwrite = super::templates::is_embedded_script(rel_path)
            && match (exe_mtime, std::fs::metadata(&full_path)) {
                (Some(exe), Ok(meta)) => meta
                    .modified()
                    .ok()
                    .filter(|disk| *disk > exe)
                    .map(|_| {
                        std::fs::read(&full_path)
                            .map(|d| d != content.as_bytes())
                            .unwrap_or(false)
                    })
                    .unwrap_or(false),
                _ => false,
            };
        if is_stale_overwrite {
            skipped_newer.push(full_path.to_string_lossy().to_string());
            manifest_entries.push(full_path.to_string_lossy().to_string());
            continue;
        }

        // A new file renamed into place, never a rewrite of the old one: a
        // backup that has a script or btrbk.conf open keeps reading the old
        // file whole. Scripts are made executable; any other file keeps the
        // mode it had, as `write_atomic` keeps it for every caller.
        let mode = (full_path.extension().and_then(|e| e.to_str()) == Some("sh")).then_some(0o755);
        buttered_dasd::fsutil::write_atomic_mode(&full_path, content.as_bytes(), mode)?;

        manifest_entries.push(full_path.to_string_lossy().to_string());
    }

    // Write manifest
    if let Some(parent) = manifest_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    buttered_dasd::fsutil::write_atomic(manifest_path, &manifest_entries.join("\n"))?;

    // Create DB directory. Not fatal — `db_path` is absolute, so a prefixed
    // (packaging or test) install legitimately cannot create it — but never
    // silent either: `let _ =` here meant a read-only or full /var produced a
    // clean "Installation complete" and the first indexer run then died on
    // SQLITE_CANTOPEN with nothing in the install log to point at
    // (bd DAS-Backup-Manager-8wx).
    if let Err(e) = ensure_db_dir(&config.general.db_path) {
        eprintln!("Warning: {e}");
    }

    // Files the previous install owned that this one no longer generates. Left
    // behind they look installed and supported while nothing maintains them.
    let mut pruned = Vec::new();
    for stale in &previous {
        if manifest_entries.iter().any(|e| e == stale) {
            continue;
        }
        let path = Path::new(stale);
        // Only ever remove something the manifest says WE installed, and never
        // the config itself.
        if path == config_path || !path.exists() {
            continue;
        }
        match std::fs::remove_file(path) {
            Ok(()) => pruned.push(stale.clone()),
            Err(e) => eprintln!("Warning: could not remove stale file {stale}: {e}"),
        }
    }

    for path in &skipped_newer {
        println!(
            "Kept existing {path} — it is newer than this btrdasd binary, whose \
             embedded copy would be a downgrade (rebuild and re-run to update it)"
        );
    }
    for path in &pruned {
        println!("Removed stale file no longer generated: {path}");
    }

    println!("Installation complete.");
    println!("Config: {}", config_path.display());
    println!(
        "Manifest: {} ({} files)",
        manifest_path.display(),
        manifest_entries.len()
    );
    Ok(())
}

/// Uninstall using system defaults. `_held`: the caller holds setup's locks
/// ([`SetupLocks`]) — this removes the scripts, `btrbk.conf` and, if asked,
/// the database a running backup uses.
pub fn uninstall(remove_db: bool, _held: &SetupLocks) -> Result<(), Box<dyn std::error::Error>> {
    uninstall_with(
        Path::new(CONFIG_FILE),
        Path::new(MANIFEST_FILE),
        remove_db,
        &|args| command_status(SYSTEMCTL, args),
    )
}

/// `uninstall` against an explicit config and manifest (for testing).
fn uninstall_with(
    config_path: &Path,
    manifest_path: &Path,
    remove_db: bool,
    systemctl: UnitRunner,
) -> Result<(), Box<dyn std::error::Error>> {
    if !manifest_path.exists() {
        eprintln!(
            "No manifest found at {}. Nothing to uninstall.",
            manifest_path.display()
        );
        return Ok(());
    }

    // A config that will not load means the DB location is unknown. Saying so
    // matters when `--remove-db` was asked for: it used to be `.ok()`, so the
    // request was silently dropped and the operator was told "Uninstall
    // complete" with the database still on disk (bd DAS-Backup-Manager-8wx).
    let db_path = match Config::load(config_path) {
        Ok(c) => Some(c.general.db_path),
        Err(e) => {
            eprintln!(
                "Warning: could not read {} ({e}) — the database location is \
                 unknown and it will NOT be removed",
                config_path.display()
            );
            None
        }
    };

    // A timer left enabled after an uninstall keeps firing at 03:00 against
    // files that are no longer there.
    for unit in [
        "das-backup.timer",
        "das-backup-full.timer",
        "das-scrub.timer",
        "das-backup-doctor.timer",
    ] {
        if let Err(e) = systemctl(&["disable", "--now", unit]) {
            eprintln!("Warning: {unit} may still be enabled: {e}");
        }
    }

    let (removed, problems) = uninstall_from_manifest(manifest_path);
    println!("Removed {} files.", removed);
    for p in &problems {
        eprintln!("Warning: {p}");
    }

    if let Err(e) = std::fs::remove_file(manifest_path) {
        eprintln!(
            "Warning: could not remove manifest {}: {e}",
            manifest_path.display()
        );
    }
    // Bare `remove_dir`: deliberately best-effort, because "the directory still
    // has operator files in it" is the normal outcome, not a fault.
    if let Some(config_dir) = config_path.parent() {
        let _ = std::fs::remove_dir(config_dir);
    }

    if remove_db
        && let Some(db) = db_path
        && Path::new(&db).exists()
    {
        std::fs::remove_file(&db)?;
        println!("Removed database: {}", db);
    }

    if let Err(e) = systemctl(&["daemon-reload"]) {
        eprintln!("Warning: {e}");
    }

    println!("Uninstall complete.");
    Ok(())
}

/// Create the parent directory of `db_path`, describing the failure instead of
/// discarding it.
fn ensure_db_dir(db_path: &str) -> Result<(), String> {
    let Some(parent) = Path::new(db_path).parent() else {
        return Err(format!("database path '{db_path}' has no parent directory"));
    };
    std::fs::create_dir_all(parent).map_err(|e| {
        format!(
            "could not create database directory {}: {e} — the indexer will fail to open {db_path}",
            parent.display()
        )
    })
}

/// Remove all files listed in a manifest.
///
/// Returns `(files removed, problems)`. The problems are the point: this used
/// to return a bare count, an unreadable manifest returned `0`, and every
/// `remove_file` error was dropped by `.is_ok()` — so an uninstall that removed
/// nothing, or that left half the tree behind on a read-only `/usr`, printed
/// "Removed 0 files." and "Uninstall complete." and looked identical to one
/// that had nothing left to do (bd DAS-Backup-Manager-8wx). Private, like
/// [`install_to_prefix`]: the removal happens only under setup's locks.
fn uninstall_from_manifest(manifest_path: &Path) -> (usize, Vec<String>) {
    let mut problems: Vec<String> = Vec::new();
    let content = match std::fs::read_to_string(manifest_path) {
        Ok(c) => c,
        Err(e) => {
            problems.push(format!(
                "could not read manifest {}: {e} — NOTHING was removed",
                manifest_path.display()
            ));
            return (0, problems);
        }
    };

    let mut removed = 0;
    for line in content.lines() {
        let path = Path::new(line.trim());
        if !path.exists() {
            continue;
        }
        match std::fs::remove_file(path) {
            Ok(()) => removed += 1,
            Err(e) => problems.push(format!("could not remove {}: {e}", path.display())),
        }
    }
    (removed, problems)
}

/// Can we open a TCP connection to the configured mail relay?
///
/// Used only to warn during `--upgrade`/`--check`. A connect is the whole test:
/// it proves something is listening, which is the failure this catches (relay
/// not installed, not running, or a config pointing at the wrong port). It
/// deliberately does not speak SMTP — a real send is the only proof of
/// deliverability, and that belongs to a backup run, not the installer.
fn relay_reachable(config: &Config) -> bool {
    let addr = format!("{}:{}", config.email.smtp_host, config.email.smtp_port);
    let Ok(mut addrs) = addr.to_socket_addrs() else {
        return false;
    };
    addrs.any(|sa| TcpStream::connect_timeout(&sa, Duration::from_secs(2)).is_ok())
}

/// Rewrite settings that a new binary would otherwise misread from an
/// old-but-valid `config.toml`.
///
/// The live config is preserved across upgrades, so a compiled default change
/// alone never reaches an existing host — it only affects fresh installs. Any
/// setting whose *meaning* changes between releases has to be migrated here or
/// the upgraded host silently keeps the old behaviour.
///
/// Returns the human-readable list of changes applied (empty when nothing
/// needed changing). Idempotent: running it twice changes nothing the second
/// time, which is what makes it safe on every `--upgrade`.
fn migrate_config(config: &mut Config) -> Vec<String> {
    let mut changes = Vec::new();

    // 2026-08-06 — Protonmail Bridge to local mail relay.
    //
    // Port 1025 is Bridge's loopback submission port. A host still carrying it
    // would have the new, credential-free sender talking to Bridge, which
    // demands authentication — so every report would fail rather than fail
    // over. Only the exact Bridge port is rewritten: an operator who has
    // deliberately set some other port keeps it.
    if config.email.smtp_port == 1025 {
        config.email.smtp_port = 25;
        changes.push(
            "[email] smtp_port 1025 -> 25 (Protonmail Bridge -> local mail relay)".to_string(),
        );
    }

    changes
}

/// `--upgrade` on the host: reload the existing config, apply migrations,
/// regenerate every file and restart the D-Bus helper onto the binary just
/// installed — all of it under setup's two locks at `site` ([`SetupLocks`]) —
/// the helper's unit reached through the real `systemctl`. A refusal is said
/// on `warn`; everything else on `say`.
pub fn upgrade(
    site: &SetupLockSite,
    config_path: &Path,
    say: &mut dyn FnMut(String),
    warn: &mut dyn FnMut(String),
) -> Result<SetupOutcome, Box<dyn std::error::Error>> {
    let active_state = || {
        command_stdout(
            SYSTEMCTL,
            &["show", "--property=ActiveState", "--value", HELPER_UNIT],
        )
    };
    let systemctl = |args: &[&str]| command_status_within(SYSTEMCTL, args, HELPER_RESTART_LIMIT);
    upgrade_with(
        site,
        config_path,
        &relay_reachable,
        &install,
        &HelperHost {
            active_state: &active_state,
            systemctl: &systemctl,
        },
        say,
        warn,
    )
}

/// Leave with `code`, once everything said has reached stdout.
pub fn exit_with(code: i32) -> ! {
    use std::io::Write;
    // Best effort: every line was already written by println!.
    let _ = std::io::stdout().flush();
    std::process::exit(code)
}

/// `setup --upgrade`'s exit status when the files were upgraded but the
/// helper is the operator's to restart: the init system is not systemd, so
/// this tool can neither tell whether one runs nor restart it. A helper left
/// running keeps the binary it started with — one that cannot read a run with
/// unknown counts — so it is not success.
pub const UPGRADE_RESTART_DEFERRED_EXIT: i32 = 3;

/// `btrdasd setup`'s exit status when a backup, or a job holding the DAS
/// maintenance lock, was running — or, for `--modify`, when `config.toml`
/// changed while the wizard was open: nothing was written or removed — try
/// again later. `EX_TEMPFAIL`, as `walk` and `restore --no-wait` exit when
/// they find that lock held.
pub const SETUP_REFUSED_EXIT: i32 = buttered_dasd::maintenance::DEFERRED_EXIT_CODE;

/// How a `btrdasd setup` mode ended, when it did not fail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetupOutcome {
    /// Done: the files written or removed — for `--upgrade`, the helper
    /// restarted or not running — or, for `--check`, the check made.
    Done,
    /// `--upgrade` regenerated the files; the helper is the operator's to
    /// restart.
    RestartDeferred,
    /// Nothing written or removed: a job held one of setup's locks, or
    /// `config.toml` changed under `--modify`.
    Refused,
}

/// The exit status of a setup run that ended `outcome` — the one place a
/// refusal becomes [`SETUP_REFUSED_EXIT`], for every mode.
pub fn exit_status(outcome: SetupOutcome) -> i32 {
    match outcome {
        SetupOutcome::Done => 0,
        SetupOutcome::RestartDeferred => UPGRADE_RESTART_DEFERRED_EXIT,
        SetupOutcome::Refused => SETUP_REFUSED_EXIT,
    }
}

/// What `setup --upgrade` records as the maintenance lock's holder.
const UPGRADE_JOB: &str = "btrdasd setup --upgrade";

/// `upgrade` with setup's lock files at `site`, the config at `config_path`,
/// and the host bound by the caller: `install` regenerates the files, `helper`
/// reaches the D-Bus helper's unit. Everything from the config's rewrite to the
/// helper's restart happens under the locks, which are let go before the last
/// line. Progress goes to `say`, one line per call; a refusal to `warn`.
fn upgrade_with(
    site: &SetupLockSite,
    config_path: &Path,
    relay_up: RelayProbe,
    install: Installer,
    helper: &HelperHost,
    say: &mut dyn FnMut(String),
    warn: &mut dyn FnMut(String),
) -> Result<SetupOutcome, Box<dyn std::error::Error>> {
    let ran = under_setup_locks(site, UPGRADE_JOB, |held| {
        let config = prepare_upgrade(config_path, relay_up, say)?;
        say(format!(
            "Regenerating files from {}...",
            config_path.display()
        ));
        // The helper is restarted even when regenerating failed: what it runs
        // is the binary `cmake --install` or the package already put in place,
        // not anything written here, and left alone it keeps the old one.
        let installed = install(&config, held);
        let restarted = restart_helper(&config, helper, held, say);
        installed?;
        Ok(restarted?)
    })?;
    let restarted = match ran {
        Locked::Ran(restarted) => restarted,
        Locked::Refused(why) => {
            warn(why);
            return Ok(SetupOutcome::Refused);
        }
    };
    Ok(match restarted {
        HelperRestart::Done => {
            say("Upgrade complete.".to_string());
            SetupOutcome::Done
        }
        HelperRestart::Deferred => {
            say(format!(
                "Files upgraded; the {HELPER_UNIT} restart is deferred (exit \
                 {UPGRADE_RESTART_DEFERRED_EXIT})."
            ));
            SetupOutcome::RestartDeferred
        }
    })
}

/// Where the two locks setup takes are: the host's in production, scratch
/// files in tests.
pub struct SetupLockSite {
    /// The backup singleton — `/run/das-backup.lock`.
    pub backup: PathBuf,
    /// The DAS maintenance lock — `/run/das-maintenance.lock`.
    pub maintenance: PathBuf,
}

impl SetupLockSite {
    /// The host's: the files `backup-run.sh`, `btrdasd backup`, the scrub
    /// engine and every job that mounts a backup target take.
    pub fn production() -> Self {
        Self {
            backup: PathBuf::from(buttered_dasd::backup::BACKUP_LOCK_PATH),
            maintenance: PathBuf::from(buttered_dasd::scrub::MAINTENANCE_LOCK_PATH),
        }
    }
}

/// Proof that this process holds the backup singleton and the DAS
/// maintenance lock, for as long as the value lives. [`take_setup_locks`] is
/// the only way to get one, and everything setup does to the installed files
/// needs one — `install`, `uninstall`, `uninstall_all` and the helper restart
/// take it as an argument — so a mode that skips the locks does not compile.
///
/// Why both (`.claude/rules/backup.md` §Never Run `setup --upgrade` …): setup's
/// writes are atomic — a new file renamed into place, so a backup already
/// reading `backup-run.sh` keeps reading the old one whole — but a run must
/// still see one version of the files from its start to its end: the scripts
/// it calls later, the `btrbk.conf` btrbk reads, the config sync reads again.
/// And an uninstall removes them. A backup holds the singleton from its start,
/// also while it still waits for the maintenance lock. The maintenance lock is
/// held by every job that mounts a backup target, and none of them may be
/// mounting while the helper restart kills the helper's jobs.
pub struct SetupLocks {
    // Dropped in declaration order: the maintenance lock — its record emptied
    // first — then the singleton, the reverse of taking them.
    _maintenance: MaintenanceHeld,
    _backup: FileLock,
}

/// How setup found its two locks.
enum SetupLockAttempt {
    /// Both taken, and held until the value is dropped.
    Taken(SetupLocks),
    /// Another holds one of them: what setup says, naming it. Nothing is held.
    Busy(String),
}

/// Take setup's two locks without waiting, in the project's order — the
/// backup singleton, then the maintenance lock, recording `job` as its holder
/// — or say which one is held, and by whom, holding neither. Not waiting is
/// the point: setup is run by hand, and a backup can run for hours. A lock
/// file that cannot be opened is an error, never a free lock.
fn take_setup_locks(site: &SetupLockSite, job: &str) -> Result<SetupLockAttempt, String> {
    let Some(backup) = FileLock::try_acquire(&site.backup).map_err(|e| e.to_string())? else {
        return Ok(SetupLockAttempt::Busy(backup_held_line(&site.backup)));
    };
    let Some(maintenance) =
        MaintenanceHeld::try_acquire_at(&site.maintenance, job).map_err(|e| e.to_string())?
    else {
        // The singleton is let go as this returns.
        return Ok(SetupLockAttempt::Busy(maintenance_held_line(
            &site.maintenance,
            &buttered_dasd::maintenance::holder_of(&site.maintenance),
        )));
    };
    Ok(SetupLockAttempt::Taken(SetupLocks {
        _maintenance: maintenance,
        _backup: backup,
    }))
}

/// What setup says, and stops at, when the backup singleton at `path` is held.
/// The singleton carries no record of its holder — `backup-run.sh` opens it
/// with `>`, which would empty one — so this names what takes it.
fn backup_held_line(path: &Path) -> String {
    format!(
        "Refused, nothing written or removed: {} is held, so a backup is running — \
         backup-run.sh, or btrdasd backup from the CLI or the GUI, which hold it from the \
         start, while they wait for the DAS maintenance lock too — or another btrdasd setup \
         is. A running backup reads the scripts and btrbk.conf that setup rewrites. Run setup \
         again once it has finished (exit {SETUP_REFUSED_EXIT}).",
        path.display()
    )
}

/// What setup says, and stops at, when the DAS maintenance lock at `path` is
/// held by `holder`, as its record names it.
fn maintenance_held_line(path: &Path, holder: &str) -> String {
    format!(
        "Refused, nothing written or removed: the DAS maintenance lock {} is held by {holder}. \
         Run setup again once that job has finished (exit {SETUP_REFUSED_EXIT}).",
        path.display()
    )
}

/// What became of a change setup wanted to make under its locks.
#[derive(Debug, PartialEq, Eq)]
pub enum Locked<T> {
    /// Both locks were taken, the change made — this is what it returned — and
    /// the locks let go again.
    Ran(T),
    /// A job holds one of them, as this says: the change was not made.
    Refused(String),
}

/// Run `change` — the part of a setup mode that writes or removes installed
/// files — holding setup's two locks from before its first write until it
/// returns, and let them go then, whether it succeeded or failed. While a job
/// holds either lock `change` is not run, and the result says which lock, and
/// by whom.
pub fn under_setup_locks<T>(
    site: &SetupLockSite,
    job: &str,
    change: impl FnOnce(&SetupLocks) -> Result<T, Box<dyn std::error::Error>>,
) -> Result<Locked<T>, Box<dyn std::error::Error>> {
    match take_setup_locks(site, job)? {
        SetupLockAttempt::Busy(why) => Ok(Locked::Refused(why)),
        // `held` is dropped as the arm ends: after `change`, on either path.
        SetupLockAttempt::Taken(held) => change(&held).map(Locked::Ran),
    }
}

/// How the helper step of an upgrade ended, when it did not fail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HelperRestart {
    /// Restarted onto the new binary, or not running at all.
    Done,
    /// Left for the operator, who was told so: the init system is not systemd.
    Deferred,
}

/// How long one `systemctl try-restart` may take, under setup's locks, before
/// it is reported as failed and the locks let go as the upgrade ends.
///
/// It must outlast systemd's own handling of the restart, or the maintenance
/// lock could be let go while the old helper — and a GUI job in it — is still
/// alive and could mount. systemd waits up to `TimeoutStopSec` for the old
/// helper to stop on SIGTERM, then sends SIGKILL and waits for the processes to
/// go, and gives the new one `TimeoutStartSec` to claim its bus name. The unit
/// sets neither, so they are the manager's defaults: 90 s each upstream, which
/// is 270 s even allowing a second stop timeout for the SIGKILL wait; 300 s
/// covers it. On the author's host they are 10 s and 15 s, and this is a
/// backstop that never fires.
const HELPER_RESTART_LIMIT: Duration = Duration::from_secs(300);

/// The D-Bus helper's unit.
const HELPER_UNIT: &str = "btrdasd-helper.service";

/// The command that restarts the helper once nothing needs it.
const HELPER_RESTART_LATER: &str = "sudo systemctl try-restart btrdasd-helper.service";

/// What restarting the D-Bus helper asks of the host.
struct HelperHost<'a> {
    /// The unit's `ActiveState`, as `systemctl show --value` prints it.
    active_state: &'a dyn Fn() -> Result<String, String>,
    /// `systemctl`, one invocation per call — bounded by
    /// [`HELPER_RESTART_LIMIT`] on the host.
    systemctl: UnitRunner<'a>,
}

/// Restart the D-Bus helper so it runs the binary just installed. `_held` is
/// the proof that the caller holds setup's locks, from before the first file
/// was written until after this returns.
///
/// `btrdasd-helper` runs as root for as long as the system does, and nothing
/// else restarts it: an upgrade that leaves it running leaves the GUI on the
/// old binary. An old one cannot read a run whose snapshot counts are unknown
/// — NULL since schema 4 (bd DAS-Backup-Manager-6wt) — so the GUI history
/// fails with an error and shows nothing, the very symptom the upgrade fixes.
///
/// Restarting kills whatever job the helper runs, and a job killed mid-mount
/// leaves the target mounted (bd DAS-Backup-Manager-jgm). Every job that
/// mounts a backup target holds the maintenance lock, and setup holds it now,
/// so none is mounting. A GUI backup asked for meanwhile finds the singleton
/// held and declines; a restore or index job waits for the maintenance lock —
/// in the new helper until setup lets go, or in the old one, which cancels it
/// as it stops: it ends without a `JobFinished` signal (bd
/// DAS-Backup-Manager-hoh). Holding the locks cannot stall the restart: the
/// helper claims its bus name, which is what systemd's start job waits for,
/// before it can take any lock (its `main`, in `src/bin/btrdasd-helper.rs`).
/// Not knowing — the unit's state could not be read, or the restart failed or
/// did not return in time — is an error: the helper may still be the old
/// binary.
fn restart_helper(
    config: &Config,
    host: &HelperHost,
    _held: &SetupLocks,
    say: &mut dyn FnMut(String),
) -> Result<HelperRestart, String> {
    if config.init.system != crate::setup::config::InitSystem::Systemd {
        say(format!(
            "The D-Bus helper is not a systemd unit on this init system: if btrdasd-helper \
             is running, restart it — it keeps running the binary it started with, and one \
             older than {} cannot read a run whose snapshot counts are unknown.",
            env!("CARGO_PKG_VERSION")
        ));
        return Ok(HelperRestart::Deferred);
    }
    let not_restarted = || format!("{HELPER_UNIT} was not restarted — see above");
    let state = match (host.active_state)() {
        Ok(state) => state.trim().to_string(),
        Err(e) => {
            say(format!(
                "ERROR: cannot tell whether {HELPER_UNIT} is running ({e}) — not restarted. \
                 If it is running, restart it when no backup or restore job is: \
                 {HELPER_RESTART_LATER}"
            ));
            return Err(not_restarted());
        }
    };
    if !matches!(state.as_str(), "active" | "activating" | "reloading") {
        say(format!(
            "{HELPER_UNIT} is {state}: nothing to restart — D-Bus starts the helper from the \
             new binary when it is next needed."
        ));
        return Ok(HelperRestart::Done);
    }
    match (host.systemctl)(&["try-restart", HELPER_UNIT]) {
        Ok(()) => {
            say(format!(
                "Restarted {HELPER_UNIT}: the D-Bus helper now runs the binary just installed."
            ));
            Ok(HelperRestart::Done)
        }
        Err(e) => {
            say(format!(
                "ERROR: could not restart {HELPER_UNIT} ({e}). The running helper may be the \
                 old binary, and the GUI history fails on a run whose snapshot counts are \
                 unknown. See systemctl status {HELPER_UNIT} and journalctl -u {HELPER_UNIT}, \
                 then run: sudo systemctl restart {HELPER_UNIT}"
            ));
            Err(not_restarted())
        }
    }
}

/// Everything `upgrade` does before regenerating files: load the config at
/// `config_path`, stamp it with this binary's version, migrate it, and write
/// it back only if either changed it. Returns the config to install from.
/// Progress and warnings go to `say`, one line per call.
fn prepare_upgrade(
    config_path: &Path,
    relay_up: RelayProbe,
    say: &mut dyn FnMut(String),
) -> Result<Config, Box<dyn std::error::Error>> {
    if !config_path.exists() {
        return Err(format!(
            "No config found at {}. Run 'btrdasd setup' first.",
            config_path.display()
        )
        .into());
    }

    let mut config = Config::load(config_path)?;
    let old_version = config.general.version.clone();
    config.general.version = env!("CARGO_PKG_VERSION").to_string();

    let migrations = migrate_config(&mut config);
    for change in &migrations {
        say(format!("Migrating config: {change}"));
    }

    if old_version != config.general.version || !migrations.is_empty() {
        if old_version != config.general.version {
            say(format!(
                "Updating config version: {} -> {}",
                old_version, config.general.version
            ));
        }
        config.save(config_path)?;
    }

    // The relay is a hard dependency of email reporting now. Warn rather than
    // fail: a backup whose report cannot be sent is still a completed backup,
    // and the report is always written to disk regardless.
    if config.email.enabled && !relay_up(&config) {
        say(format!(
            "Warning: email is enabled but nothing is listening on {}:{}",
            config.email.smtp_host, config.email.smtp_port
        ));
        say("  Reports will be saved to disk but not delivered.".to_string());
        say("  Check the local mail relay: systemctl status postfix".to_string());
    }
    Ok(config)
}

/// Check: validate config, verify manifest files, report dependency status.
pub fn check() -> Result<(), Box<dyn std::error::Error>> {
    check_with(
        Path::new(CONFIG_FILE),
        Path::new(MANIFEST_FILE),
        &CheckProbes {
            relay_up: &relay_reachable,
            export_db: &|| command_stdout("udevadm", &["info", "--export-db"]),
            dependencies: &crate::setup::detect::check_dependencies,
        },
        &mut |line| println!("{line}"),
    )
}

/// What `check` has to ask the running host, as opposed to read from the
/// config and manifest it is pointed at.
struct CheckProbes<'a> {
    relay_up: RelayProbe<'a>,
    /// udev's database, as `udevadm info --export-db` prints it.
    export_db: &'a dyn Fn() -> Result<String, String>,
    /// Dependency lookup, given whether email is enabled.
    dependencies: &'a dyn Fn(bool) -> Vec<crate::setup::detect::DepStatus>,
}

/// `check` against an explicit config and manifest, with the host probes
/// supplied by the caller. The report goes to `say`, one line per call.
fn check_with(
    config_path: &Path,
    manifest_path: &Path,
    probes: &CheckProbes,
    say: &mut dyn FnMut(String),
) -> Result<(), Box<dyn std::error::Error>> {
    if !config_path.exists() {
        say(format!("Config not found at {}", config_path.display()));
        say("  Run: sudo btrdasd setup".to_string());
        return Ok(());
    }
    say(format!("Config found: {}", config_path.display()));

    let config = Config::load(config_path)?;
    let errors = config.validate();
    if errors.is_empty() {
        say("Config is valid".to_string());
    } else {
        for err in &errors {
            say(format!("Config error: {}", err));
        }
    }

    // A config that still names the Bridge port is valid but undeliverable —
    // report it here rather than letting the next backup discover it.
    if config.email.enabled {
        let relay = format!("{}:{}", config.email.smtp_host, config.email.smtp_port);
        if (probes.relay_up)(&config) {
            say(format!("Mail relay reachable at {relay}"));
        } else {
            say(format!("Mail relay UNREACHABLE at {relay}"));
            if config.email.smtp_port == 1025 {
                say(
                    "  Port 1025 is Protonmail Bridge, which this version no longer uses."
                        .to_string(),
                );
                say("  Fix with: sudo btrdasd setup --upgrade".to_string());
            } else {
                say("  Reports will be saved to disk but not delivered.".to_string());
            }
        }
    }

    if manifest_path.exists() {
        let content = std::fs::read_to_string(manifest_path)?;
        let total = content.lines().count();
        let missing: Vec<&str> = content
            .lines()
            .filter(|line| !Path::new(line.trim()).exists())
            .collect();
        if missing.is_empty() {
            say(format!("All {} generated files present", total));
        } else {
            say(format!(
                "{} of {} generated files missing:",
                missing.len(),
                total
            ));
            for m in &missing {
                say(format!("    {}", m));
            }
            say("  Fix with: sudo btrdasd setup --upgrade".to_string());
        }
    } else {
        say("No manifest found. Files may be from a manual install.".to_string());
    }

    // Are the targets hidden from udisks2? Read back from udev rather than
    // from the rule file: a rule that exists and matches nothing looks
    // installed and does nothing (bd DAS-Backup-Manager-a10).
    let export_db = (probes.export_db)();
    for line in mount_uuid_report(&export_db, &config) {
        say(line);
    }
    for line in udisks_report(export_db, &config) {
        say(line);
    }

    for dep in &(probes.dependencies)(config.email.enabled) {
        if let Some(path) = &dep.path {
            say(format!("{} ({})", dep.name, path));
        } else if dep.required {
            say(format!("{} (required, not found)", dep.name));
        } else {
            say(format!("{} (optional, not found)", dep.name));
        }
    }

    Ok(())
}

/// Remove a list of file paths, silently skipping any that don't exist.
/// Returns the count of files successfully removed.
fn remove_paths(paths: &[String]) -> usize {
    let mut removed = 0;
    for p in paths {
        let path = Path::new(p);
        if path.exists() && std::fs::remove_file(path).is_ok() {
            removed += 1;
        }
    }
    removed
}

/// Return the list of all files installed by `cmake --install`.
/// The `prefix` is the install prefix (e.g., `/usr` or `/usr/local`).
///
/// Mirrors the `install()` rules in CMakeLists.txt and gui/CMakeLists.txt and
/// must change with them. Every entry is a file this project installs — never
/// a directory, least of all one shared with other software: the list is
/// handed to `remove_file`.
fn cmake_installed_paths(prefix: &str) -> Vec<String> {
    let p = |suffix: &str| format!("{prefix}/{suffix}");
    vec![
        // Binaries
        p("bin/btrdasd"),
        p("bin/btrdasd-gui"),
        p("libexec/btrdasd-helper"),
        // LEGACY FFI artifacts. The C-ABI library was removed in 0.7.22.2
        // (bd DAS-Backup-Manager-5xo) and is no longer built or installed, but
        // hosts installed at 0.7.22.1 or earlier still carry these two files.
        // They stay on the uninstall list so a full uninstall cleans them up;
        // remove these entries only once no supported host can still have them.
        p("lib/libbuttered_dasd_ffi.so"),
        p("include/btrdasd_ffi.h"),
        // D-Bus
        p("share/dbus-1/system.d/org.dasbackup.Helper1.conf"),
        p("share/dbus-1/system-services/org.dasbackup.Helper1.service"),
        // Polkit
        p("share/polkit-1/actions/org.dasbackup.policy"),
        // Man page
        p("share/man/man1/btrdasd.1"),
        // Shell completions
        p("share/bash-completion/completions/btrdasd"),
        p("share/zsh/site-functions/_btrdasd"),
        p("share/fish/vendor_completions.d/btrdasd.fish"),
        // Desktop entry and icon
        p("share/applications/org.theboscoclub.btrdasd-gui.desktop"),
        p("share/icons/hicolor/scalable/apps/btrdasd-gui.svg"),
        // XML GUI
        p("share/kxmlgui5/btrdasd-gui/btrdasd-gui.rc"),
        // Backup scripts (cmake-installed, separate from setup-generated)
        p("lib/das-backup/backup-run.sh"),
        p("lib/das-backup/backup-verify.sh"),
        p("lib/das-backup/boot-archive-cleanup.sh"),
        p("lib/das-backup/das-partition-drives.sh"),
        p("lib/das-backup/install-backup-timer.sh"),
        p("lib/das-backup/config/btrbk.conf"),
        // Systemd units (cmake-installed templates). Under the prefix like
        // everything else: CMakeLists.txt gives them the relative destination
        // `lib/systemd/system`. They were listed at a fixed
        // `/lib/systemd/system`, which is the same place only for `/usr` on a
        // merged-/usr host; for any other prefix it missed the installed units
        // and named files this install never wrote.
        p("lib/systemd/system/das-backup.service"),
        p("lib/systemd/system/das-backup-full.service"),
        p("lib/systemd/system/das-backup.timer"),
        p("lib/systemd/system/das-backup-full.timer"),
        p("lib/systemd/system/btrdasd-helper.service"),
    ]
}

/// Where an absolute path lands under `root`. With the real root, `/`, that
/// is the path itself.
fn under_root(root: &Path, absolute: &str) -> PathBuf {
    root.join(absolute.trim_start_matches('/'))
}

/// Full uninstall: remove generated files (manifest), then cmake-installed
/// files, and stop the D-Bus helper. `_held`: the caller holds setup's locks
/// ([`SetupLocks`]), as for [`uninstall`] — and no job in the helper is
/// mounting when it is stopped.
pub fn uninstall_all(
    remove_db: bool,
    _held: &SetupLocks,
) -> Result<(), Box<dyn std::error::Error>> {
    uninstall_all_with(
        Path::new("/"),
        Path::new(CONFIG_FILE),
        Path::new(MANIFEST_FILE),
        remove_db,
        &|args| command_status(SYSTEMCTL, args),
    )
}

/// `uninstall_all` with the cmake-installed tree looked for under `root`
/// (for testing). The manifest's entries and the database path are absolute
/// and are used as written.
fn uninstall_all_with(
    root: &Path,
    config_path: &Path,
    manifest_path: &Path,
    remove_db: bool,
    systemctl: UnitRunner,
) -> Result<(), Box<dyn std::error::Error>> {
    // Determine the install prefix from config (default /usr) BEFORE anything
    // is removed: the manifest lists config.toml, so phase 1 deletes it. Read
    // afterwards, the load always failed and every full uninstall fell back to
    // /usr — leaving an install under any other prefix in place.
    // An unreadable config here does not mean "/usr" — it means we are guessing,
    // and everything installed under a different prefix will silently survive
    // the "full" uninstall (bd DAS-Backup-Manager-8wx).
    let prefix = match Config::load(config_path) {
        Ok(c) => c.general.install_prefix,
        Err(e) => {
            eprintln!(
                "Warning: could not read {} ({e}) — assuming the default install \
                 prefix /usr; files installed under any other prefix will be LEFT BEHIND",
                config_path.display()
            );
            "/usr".to_string()
        }
    };

    // Phase 1: run the standard uninstall (manifest files, timers, config dir)
    uninstall_with(config_path, manifest_path, remove_db, systemctl)?;

    // Phase 2: stop the helper service
    if let Err(e) = systemctl(&["disable", "--now", "btrdasd-helper.service"]) {
        eprintln!("Warning: btrdasd-helper.service may still be enabled: {e}");
    }

    // Phase 3: remove the cmake-installed files under that prefix
    let paths: Vec<String> = cmake_installed_paths(&prefix)
        .iter()
        .map(|p| under_root(root, p).to_string_lossy().into_owned())
        .collect();
    let removed = remove_paths(&paths);
    println!("Removed {} cmake-installed files.", removed);

    // Phase 4: clean up directories
    let libdir = under_root(root, &format!("{prefix}/lib/das-backup"));
    if libdir.exists()
        && let Err(e) = std::fs::remove_dir_all(&libdir)
    {
        eprintln!("Warning: could not remove {}: {e}", libdir.display());
    }
    // Bare `remove_dir`: best-effort, and failing because the database is still
    // there is the normal outcome.
    let _ = std::fs::remove_dir(under_root(root, "/var/lib/das-backup"));

    if let Err(e) = systemctl(&["daemon-reload"]) {
        eprintln!("Warning: {e}");
    }

    println!("Full uninstall complete.");
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests (TDD — written first, implementation follows)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::setup::config::*;
    use std::os::unix::fs::PermissionsExt;

    /// Install into a throwaway prefix and hand back the paths used.
    fn install_into(dir: &Path) -> (PathBuf, PathBuf) {
        let config_path = dir.join("etc/das-backup/config.toml");
        let manifest_path = dir.join("etc/das-backup/.manifest");
        std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        let config = Config::default();
        install_to_prefix(&config, dir, &config_path, &manifest_path).unwrap();
        (config_path, manifest_path)
    }

    /// Records trimmed from a real `udevadm info --export-db` (2026-10-01):
    /// a recovery drive's ESP and btrfs partitions, both legs of the RAID-1
    /// pair, and an unrelated optical drive.
    fn export_db(sdj2_ignore: bool, sdl1_ignore: bool) -> String {
        let ign = |on: bool| if on { "E: UDISKS_IGNORE=1\n" } else { "" };
        format!(
            "P: /devices/x/block/sdj/sdj1\nN: sdj1\nE: DEVNAME=/dev/sdj1\n\
             E: SUBSYSTEM=block\nE: ID_SERIAL_SHORT=ZK208Q77\n\
             E: ID_FS_UUID=6D15-0632\nE: ID_FS_TYPE=vfat\n\
             \n\
             P: /devices/x/block/sdj/sdj2\nN: sdj2\nE: DEVNAME=/dev/sdj2\n\
             E: SUBSYSTEM=block\nE: ID_SERIAL_SHORT=ZK208Q77\n\
             E: ID_FS_UUID=60b05268-7f8f-47b5-a38a-752576a1172a\nE: ID_FS_TYPE=btrfs\n{}\
             \n\
             P: /devices/x/block/sdi/sdi1\nN: sdi1\nE: DEVNAME=/dev/sdi1\n\
             E: SUBSYSTEM=block\nE: ID_SERIAL_SHORT=ZXA1NYGZ\n\
             E: ID_FS_UUID=b2dbe07d-40b9-422e-8ccf-ef4931c40457\nE: ID_FS_TYPE=btrfs\n\
             E: UDISKS_IGNORE=1\n\
             \n\
             P: /devices/x/block/sdl/sdl1\nN: sdl1\nE: DEVNAME=/dev/sdl1\n\
             E: SUBSYSTEM=block\nE: ID_SERIAL_SHORT=REPLACEMENT\n\
             E: ID_FS_UUID=b2dbe07d-40b9-422e-8ccf-ef4931c40457\nE: ID_FS_TYPE=btrfs\n{}\
             \n\
             P: /devices/x/block/sr0\nN: sr0\nE: DEVNAME=/dev/sr0\n\
             E: SUBSYSTEM=block\nE: ID_SERIAL_SHORT=USB_Storage\n",
            ign(sdj2_ignore),
            ign(sdl1_ignore),
        )
    }

    fn exposure_target(label: &str, serials: &[&str], uuid: Option<&str>) -> Target {
        Target {
            label: label.to_string(),
            serial: serials.first().map(|s| s.to_string()).unwrap_or_default(),
            serials: serials.iter().map(|s| s.to_string()).collect(),
            mount_uuid: uuid.map(str::to_string),
            mount: format!("/mnt/{label}"),
            role: TargetRole::Primary,
            retention: Retention {
                weekly: 0,
                monthly: 0,
                daily: 7,
                yearly: 0,
            },
            display_name: String::new(),
        }
    }

    fn exposure_config() -> Config {
        let mut config = Config::default();
        config
            .targets
            .push(exposure_target("recovery-A", &["ZK208Q77"], None));
        config.targets.push(exposure_target(
            "pair",
            &["ZXA1NYGZ", "ZXA1R71M"],
            Some("b2dbe07d-40b9-422e-8ccf-ef4931c40457"),
        ));
        config
            .targets
            .push(exposure_target("unplugged", &["NOTHERE"], None));
        config
    }

    #[test]
    fn udisks_exposure_reports_every_target_hidden_when_all_carry_the_flag() {
        let found = udisks_exposure(&export_db(true, true), &exposure_config());
        assert_eq!(found.len(), 3);
        assert_eq!(found[0].label, "recovery-A");
        // The ESP on the same drive shares the serial but is not btrfs, so it
        // is not this target's device.
        assert_eq!(found[0].hidden, vec!["/dev/sdj2".to_string()]);
        assert!(found[0].exposed.is_empty());
        // sdl1's serial is not in the config: it is matched by filesystem UUID.
        assert_eq!(
            found[1].hidden,
            vec!["/dev/sdi1".to_string(), "/dev/sdl1".to_string()]
        );
        assert!(found[1].exposed.is_empty());
    }

    #[test]
    fn udisks_exposure_names_a_device_missing_the_flag() {
        let found = udisks_exposure(&export_db(false, false), &exposure_config());
        assert_eq!(found[0].exposed, vec!["/dev/sdj2".to_string()]);
        assert!(found[0].hidden.is_empty());
        assert_eq!(found[1].exposed, vec!["/dev/sdl1".to_string()]);
        assert_eq!(found[1].hidden, vec!["/dev/sdi1".to_string()]);
    }

    #[test]
    fn udisks_exposure_keeps_an_absent_target_distinct_from_a_hidden_one() {
        // "No device matched" must not read as "nothing exposed".
        let found = udisks_exposure(&export_db(true, true), &exposure_config());
        assert_eq!(found[2].label, "unplugged");
        assert!(found[2].hidden.is_empty() && found[2].exposed.is_empty());
        assert_eq!(found[2].verdict(), ExposureVerdict::NotAttached);
        assert_eq!(found[0].verdict(), ExposureVerdict::Hidden);
        let exposed = udisks_exposure(&export_db(false, true), &exposure_config());
        assert_eq!(exposed[0].verdict(), ExposureVerdict::Exposed);
        // A target with one hidden leg and one exposed leg is exposed.
        let half = udisks_exposure(&export_db(true, false), &exposure_config());
        assert_eq!(half[1].verdict(), ExposureVerdict::Exposed);
    }

    #[test]
    fn command_stdout_separates_output_from_a_failed_or_missing_command() {
        assert_eq!(
            command_stdout("sh", &["-c", "printf hello"]),
            Ok("hello".to_string())
        );
        // Output printed before a failure is not an answer.
        let err = command_stdout("sh", &["-c", "printf partial; exit 3"]).unwrap_err();
        assert!(
            err.contains("sh -c printf partial; exit 3 exited with"),
            "{err}"
        );
        let err = command_stdout("/nonexistent/das-no-such-binary", &[]).unwrap_err();
        assert!(err.contains("could not be run"), "{err}");
    }

    #[test]
    fn udisks_report_says_hidden_exposed_or_absent_per_target() {
        let lines = udisks_report(Ok(export_db(false, true)), &exposure_config());
        assert_eq!(
            lines,
            vec![
                "Target recovery-A EXPOSED to udisks — a desktop login can automount /dev/sdj2"
                    .to_string(),
                "  Fix with: sudo btrdasd setup --upgrade".to_string(),
                "Target pair hidden from udisks (/dev/sdi1, /dev/sdl1)".to_string(),
                "Target unplugged not attached — udisks visibility could not be checked"
                    .to_string(),
            ]
        );
    }

    #[test]
    fn mount_uuid_report_names_each_target_without_one_and_the_uuid_to_add() {
        let mut config = exposure_config();
        config
            .targets
            .push(exposure_target("blank", &["ZK208Q77"], Some("")));
        let lines = mount_uuid_report(&Ok(export_db(true, true)), &config);
        assert_eq!(
            lines,
            vec![
                "Target recovery-A has no mount_uuid — found by drive serial only, and \
                 verified only as a mount point, not as its filesystem"
                    .to_string(),
                // The ESP on the same drive (vfat) is never offered.
                "  Its filesystem has UUID 60b05268-7f8f-47b5-a38a-752576a1172a — add  \
                 mount_uuid = \"60b05268-7f8f-47b5-a38a-752576a1172a\"  to its [[target]] in \
                 config.toml"
                    .to_string(),
                "Target unplugged has no mount_uuid — found by drive serial only, and \
                 verified only as a mount point, not as its filesystem"
                    .to_string(),
                "  Its drive is not attached (or holds no BTRFS filesystem) — attach it and \
                 run setup --check again to read its UUID"
                    .to_string(),
                "Target blank has no mount_uuid — found by drive serial only, and verified \
                 only as a mount point, not as its filesystem"
                    .to_string(),
                "  Its filesystem has UUID 60b05268-7f8f-47b5-a38a-752576a1172a — add  \
                 mount_uuid = \"60b05268-7f8f-47b5-a38a-752576a1172a\"  to its [[target]] in \
                 config.toml"
                    .to_string(),
            ],
            "the pair carries a mount_uuid and is not mentioned"
        );
    }

    #[test]
    fn mount_uuid_report_never_guesses_between_two_filesystems() {
        let mut config = Config::default();
        config
            .targets
            .push(exposure_target("mixed", &["ZK208Q77", "ZXA1NYGZ"], None));
        let lines = mount_uuid_report(&Ok(export_db(true, true)), &config);
        assert_eq!(
            lines[1],
            "  Its drives carry more than one BTRFS filesystem \
             (60b05268-7f8f-47b5-a38a-752576a1172a, b2dbe07d-40b9-422e-8ccf-ef4931c40457) — \
             set mount_uuid by hand"
        );
        let lines = mount_uuid_report(&Err("udevadm exited with 1".into()), &config);
        assert_eq!(
            lines[1],
            "  Its filesystem UUID could not be looked up: udevadm exited with 1"
        );
        assert!(mount_uuid_report(&Ok(export_db(true, true)), &Config::default()).is_empty());
    }

    #[test]
    fn btrfs_uuids_for_serials_reads_each_uuid_once() {
        let db = export_db(true, true);
        assert_eq!(
            btrfs_uuids_for_serials(&db, &["ZXA1NYGZ".into(), "REPLACEMENT".into()]),
            vec!["b2dbe07d-40b9-422e-8ccf-ef4931c40457".to_string()],
            "two legs of one RAID-1 are one filesystem"
        );
        assert!(btrfs_uuids_for_serials(&db, &["NOTHERE".into()]).is_empty());
        assert!(btrfs_uuids_for_serials(&db, &[]).is_empty());
    }

    #[test]
    fn udisks_report_never_reads_an_unreadable_database_as_nothing_exposed() {
        let lines = udisks_report(Err("udevadm exited with 1".to_string()), &exposure_config());
        assert_eq!(
            lines,
            vec!["udisks visibility NOT checked: udevadm exited with 1".to_string()]
        );
    }

    #[test]
    fn run_udevadm_reports_success_and_failure_differently() {
        // Exercises the real binary: a verb it accepts, and one it rejects.
        if crate::setup::detect::which("udevadm") {
            assert_eq!(run_udevadm(&["--version"]), Ok(()));
            let err = run_udevadm(&["no-such-verb"]).unwrap_err();
            assert!(err.contains("udevadm no-such-verb exited with"), "{err}");
        }
    }

    #[test]
    fn install_writes_the_udev_rule_into_the_prefix_and_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let (_, manifest_path) = install_into(dir.path());
        let rule = dir
            .path()
            .join("etc/udev/rules.d/99-das-backup-udisks-ignore.rules");
        assert!(rule.is_file());
        let manifest = std::fs::read_to_string(manifest_path).unwrap();
        assert!(manifest.contains(&rule.to_string_lossy().to_string()));
    }

    #[test]
    fn upgrade_prunes_files_that_left_the_generated_set() {
        // bd DAS-Backup-Manager-e23: the manifest was overwritten without diffing,
        // so a file that dropped out of the generated set stayed on disk looking
        // installed and supported while nothing maintained it.
        let dir = tempfile::tempdir().unwrap();
        let (_config_path, manifest_path) = install_into(dir.path());

        // Simulate a previous install that owned an extra file.
        let orphan = dir
            .path()
            .join("usr/lib/das-backup/dropped-by-a-later-release.sh");
        std::fs::create_dir_all(orphan.parent().unwrap()).unwrap();
        std::fs::write(&orphan, "#!/bin/bash\n").unwrap();
        let mut manifest = std::fs::read_to_string(&manifest_path).unwrap();
        manifest.push('\n');
        manifest.push_str(&orphan.to_string_lossy());
        std::fs::write(&manifest_path, manifest).unwrap();
        assert!(orphan.exists());

        // Re-install: the orphan is no longer generated, so it must go.
        install_into(dir.path());

        assert!(!orphan.exists(), "stale file was left on disk");
        let final_manifest = std::fs::read_to_string(&manifest_path).unwrap();
        assert!(!final_manifest.contains("dropped-by-a-later-release"));
    }

    #[test]
    fn upgrade_never_prunes_the_config_itself() {
        let dir = tempfile::tempdir().unwrap();
        let (config_path, _manifest) = install_into(dir.path());
        install_into(dir.path());
        assert!(config_path.exists(), "config must survive a re-install");
    }

    #[test]
    fn upgrade_keeps_a_script_newer_than_the_binary() {
        // bd DAS-Backup-Manager-2lj: scripts are embedded with include_str!, so a
        // btrdasd built BEFORE a script edit carries a stale copy. Re-running
        // `setup --upgrade` would silently overwrite the file cmake had just
        // refreshed. A file newer than the running binary must be left alone.
        let dir = tempfile::tempdir().unwrap();
        let (_c, _m) = install_into(dir.path());

        // Pick any installed .sh and make it look hand-updated after the build.
        let script = walk_installed_scripts(dir.path())
            .into_iter()
            .next()
            .expect("install should have produced at least one script");
        let sentinel = b"#!/bin/bash\n# edited after the binary was built\n";
        std::fs::write(&script, sentinel).unwrap();
        let future = std::time::SystemTime::now() + std::time::Duration::from_secs(3600);
        filetime::set_file_mtime(&script, filetime::FileTime::from_system_time(future)).unwrap();

        install_into(dir.path());

        assert_eq!(
            std::fs::read(&script).unwrap(),
            sentinel,
            "a script newer than the binary must not be overwritten by the embedded copy"
        );
    }

    #[test]
    fn upgrade_does_overwrite_a_script_older_than_the_binary() {
        // The complement: without this, "keep newer" could be implemented as
        // "never write", and upgrades would silently stop working.
        let dir = tempfile::tempdir().unwrap();
        install_into(dir.path());
        let script = walk_installed_scripts(dir.path())
            .into_iter()
            .next()
            .expect("install should have produced at least one script");
        std::fs::write(&script, b"stale contents\n").unwrap();
        let past = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1);
        filetime::set_file_mtime(&script, filetime::FileTime::from_system_time(past)).unwrap();

        install_into(dir.path());

        assert_ne!(
            std::fs::read(&script).unwrap(),
            b"stale contents\n".to_vec(),
            "an older script must still be refreshed"
        );
    }

    #[test]
    fn upgrade_rewrites_a_config_derived_file_newer_than_the_binary() {
        // bd DAS-Backup-Manager-bwt. The 2lj staleness guard was applied to every
        // generated file, but btrbk.conf is rendered from Config, not embedded via
        // include_str!. Since a successful upgrade stamps it with `now` — always
        // newer than the binary — the guard then refused every subsequent rewrite,
        // and refused precisely when a config change was waiting. `setup --upgrade`
        // printed "Upgrade complete" while the edit stayed inert.
        let dir = tempfile::tempdir().unwrap();
        install_into(dir.path());

        let btrbk = dir.path().join("etc/btrbk/btrbk.conf");
        assert!(btrbk.exists(), "install should have produced btrbk.conf");

        // Make it look exactly like a file written by a prior successful upgrade:
        // different content, and an mtime after the running binary's.
        let stale = b"# a previous generation that must not survive\n";
        std::fs::write(&btrbk, stale).unwrap();
        let future = std::time::SystemTime::now() + std::time::Duration::from_secs(3600);
        filetime::set_file_mtime(&btrbk, filetime::FileTime::from_system_time(future)).unwrap();

        install_into(dir.path());

        assert_ne!(
            std::fs::read(&btrbk).unwrap(),
            stale.to_vec(),
            "a config-derived file must be regenerated regardless of its mtime"
        );
        assert!(
            String::from_utf8_lossy(&std::fs::read(&btrbk).unwrap())
                .contains("Generated by btrdasd setup"),
            "btrbk.conf must be the freshly rendered artifact, not the stale text"
        );
    }

    fn walk_installed_scripts(root: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        for entry in walkdir::WalkDir::new(root)
            .into_iter()
            .filter_map(Result::ok)
        {
            if entry.file_type().is_file()
                && entry.path().extension().and_then(|e| e.to_str()) == Some("sh")
            {
                out.push(entry.path().to_path_buf());
            }
        }
        out.sort();
        out
    }

    #[test]
    fn migrate_rewrites_bridge_port_to_relay_port() {
        let mut config = Config::default();
        config.email.enabled = true;
        config.email.smtp_port = 1025;

        let changes = migrate_config(&mut config);

        assert_eq!(config.email.smtp_port, 25);
        assert_eq!(changes.len(), 1, "one migration should have been reported");
        assert!(changes[0].contains("1025 -> 25"), "got: {}", changes[0]);
    }

    #[test]
    fn migrate_is_idempotent() {
        let mut config = Config::default();
        config.email.enabled = true;
        config.email.smtp_port = 1025;

        migrate_config(&mut config);
        // Second pass must be a no-op — `--upgrade` runs on every install.
        let changes = migrate_config(&mut config);

        assert_eq!(config.email.smtp_port, 25);
        assert!(changes.is_empty(), "second pass reported: {changes:?}");
    }

    #[test]
    fn migrate_preserves_a_deliberate_non_bridge_port() {
        // Only the exact Bridge port is rewritten. An operator running their
        // relay on a non-default port keeps it.
        let mut config = Config::default();
        config.email.enabled = true;
        config.email.smtp_port = 2525;

        let changes = migrate_config(&mut config);

        assert_eq!(config.email.smtp_port, 2525);
        assert!(changes.is_empty());
    }

    #[test]
    fn email_defaults_target_the_local_relay() {
        // A fresh install must not inherit the Bridge port from anywhere.
        let config = Config::default();
        assert_eq!(config.email.smtp_host, "127.0.0.1");
        assert_eq!(config.email.smtp_port, 25);
    }

    /// Serialize a valid config, then edit its `[email]` table textually to
    /// reproduce an on-disk file from before this release. Building the fixture
    /// from `Config::default()` rather than hand-writing one keeps it valid as
    /// unrelated sections gain required fields.
    fn config_toml_with_email_table(body: &str) -> String {
        let toml = Config::default()
            .to_toml()
            .expect("serialize default config");
        let start = toml.find("[email]").expect("default config has [email]");
        // The [email] table runs to the next table header or end of file.
        let rest = &toml[start + "[email]".len()..];
        let end = rest
            .find("\n[")
            .map(|i| start + "[email]".len() + i + 1)
            .unwrap_or(toml.len());
        format!("{}[email]\n{}\n{}", &toml[..start], body, &toml[end..])
    }

    #[test]
    fn bridge_era_config_without_new_keys_parses_to_relay_defaults() {
        // A config.toml predating the [email] keys must not deserialize to
        // port 0 / empty host — serde defaults carry it onto the relay.
        let toml = config_toml_with_email_table("enabled = true");

        let config = Config::from_toml(&toml).expect("parse config with a minimal [email] table");

        assert_eq!(config.email.smtp_host, "127.0.0.1");
        assert_eq!(config.email.smtp_port, 25);
        assert_eq!(config.email.from, "backup@localhost");
        assert_eq!(config.email.to, "root@localhost");
        // Scoped to email: the fixture has no sources/targets, so the config as
        // a whole is legitimately invalid for unrelated reasons.
        let email_errors: Vec<_> = config
            .validate()
            .into_iter()
            .filter(|e| e.contains("smtp") || e.contains("Email"))
            .collect();
        assert!(
            email_errors.is_empty(),
            "email defaults must satisfy validation: {email_errors:?}"
        );
    }

    #[test]
    fn bridge_era_auth_key_is_ignored_not_fatal() {
        // The live config carries `auth = "starttls"`. The field is gone; serde
        // must skip it rather than fail the whole load and take backups down.
        let toml = config_toml_with_email_table(
            r#"enabled = true
smtp_host = "127.0.0.1"
smtp_port = 1025
from = "someone@example.com"
to = "someone@example.com"
auth = "starttls""#,
        );

        let mut config =
            Config::from_toml(&toml).expect("an unknown [email] key must not fail the load");

        assert_eq!(config.email.smtp_port, 1025, "fixture should start on 1025");
        migrate_config(&mut config);
        assert_eq!(config.email.smtp_port, 25);
        // The dropped key must not survive a save/load round trip either.
        let round_tripped = Config::from_toml(&config.to_toml().unwrap()).unwrap();
        assert_eq!(round_tripped.email.smtp_port, 25);
        assert!(!config.to_toml().unwrap().contains("auth ="));
    }

    #[test]
    fn install_creates_files_and_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();

        let mut config = Config::default();
        config.general.install_prefix = base.join("usr/local").to_str().unwrap().to_string();
        config.sources.push(Source {
            label: "test".to_string(),
            volume: "/test".to_string(),
            subvolumes: vec![SubvolConfig {
                name: "@".to_string(),
                manual_only: false,
                snapshot_name: None,
                ..Default::default()
            }],
            device: "/dev/sda".to_string(),
            snapshot_dir: ".btrbk-snapshots".into(),
            target_subdirs: vec![],
            target_labels: vec![],
        });
        config.targets.push(Target {
            label: "tgt".to_string(),
            serial: "ABC123".to_string(),
            serials: vec!["ABC123".to_string()],
            mount_uuid: None,
            mount: "/mnt/tgt".to_string(),
            role: TargetRole::Primary,
            retention: Retention {
                weekly: 4,
                monthly: 2,
                daily: 0,
                yearly: 0,
            },
            display_name: String::new(),
        });

        let config_path = base.join("etc/das-backup/config.toml");
        let manifest_path = base.join("etc/das-backup/.manifest");

        let result = install_to_prefix(&config, base, &config_path, &manifest_path);
        assert!(result.is_ok());
        assert!(config_path.exists());
        assert!(manifest_path.exists());

        let manifest = std::fs::read_to_string(&manifest_path).unwrap();
        assert!(manifest.contains("btrbk.conf"));
        assert!(manifest.contains("backup-run.sh"));
    }

    #[test]
    fn uninstall_all_removes_cmake_files() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();

        // Simulate cmake-installed files
        let bin_dir = base.join("usr/bin");
        let libexec_dir = base.join("usr/libexec");
        let lib_dir = base.join("usr/lib");
        std::fs::create_dir_all(&bin_dir).unwrap();
        std::fs::create_dir_all(&libexec_dir).unwrap();
        std::fs::create_dir_all(&lib_dir).unwrap();

        let btrdasd = bin_dir.join("btrdasd");
        let gui = bin_dir.join("btrdasd-gui");
        let helper = libexec_dir.join("btrdasd-helper");
        let ffi = lib_dir.join("libbuttered_dasd_ffi.so");
        std::fs::write(&btrdasd, "bin").unwrap();
        std::fs::write(&gui, "bin").unwrap();
        std::fs::write(&helper, "bin").unwrap();
        std::fs::write(&ffi, "lib").unwrap();

        let paths = vec![
            btrdasd.to_string_lossy().to_string(),
            gui.to_string_lossy().to_string(),
            helper.to_string_lossy().to_string(),
            ffi.to_string_lossy().to_string(),
        ];

        let removed = remove_paths(&paths);
        assert_eq!(removed, 4);
        assert!(!btrdasd.exists());
        assert!(!gui.exists());
        assert!(!helper.exists());
        assert!(!ffi.exists());
    }

    #[test]
    fn uninstall_removes_manifest_files() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();

        let file1 = base.join("test1.txt");
        let file2 = base.join("test2.txt");
        std::fs::write(&file1, "content").unwrap();
        std::fs::write(&file2, "content").unwrap();

        let manifest = base.join(".manifest");
        std::fs::write(
            &manifest,
            format!("{}\n{}", file1.display(), file2.display()),
        )
        .unwrap();

        let (removed, problems) = uninstall_from_manifest(&manifest);
        assert_eq!(removed, 2);
        assert!(
            problems.is_empty(),
            "clean run reported problems: {problems:?}"
        );
        assert!(!file1.exists());
        assert!(!file2.exists());
    }

    /// An uninstall that could not do its job must say so. Both halves used to
    /// be silent: an unreadable manifest returned a bare `0`, and a file that
    /// would not delete was dropped by `.is_ok()` — so "Removed N files." and
    /// "Uninstall complete." were printed over a tree that was still installed.
    #[test]
    fn uninstall_from_manifest_reports_what_it_could_not_remove() {
        let dir = tempfile::TempDir::new().unwrap();
        let base = dir.path();

        // (a) manifest that cannot be read — a directory, so this holds for
        //     root too and needs no permission games.
        let unreadable = base.join("manifest-is-a-dir");
        std::fs::create_dir(&unreadable).unwrap();
        let (removed, problems) = uninstall_from_manifest(&unreadable);
        assert_eq!(removed, 0);
        assert!(
            problems
                .iter()
                .any(|p| p.contains("could not read manifest")),
            "an unreadable manifest must be reported, got: {problems:?}"
        );

        // (b) a listed entry that exists but cannot be removed by remove_file:
        //     a non-empty directory fails EISDIR/ENOTEMPTY for root as well.
        let undeletable = base.join("stubborn-dir");
        std::fs::create_dir(&undeletable).unwrap();
        std::fs::write(undeletable.join("child"), "x").unwrap();
        let good = base.join("ordinary.txt");
        std::fs::write(&good, "x").unwrap();
        let manifest = base.join(".manifest2");
        std::fs::write(
            &manifest,
            format!("{}\n{}", undeletable.display(), good.display()),
        )
        .unwrap();

        let (removed, problems) = uninstall_from_manifest(&manifest);
        // Positive control: the ordinary file WAS removed and counted, so the
        // reporting cannot be passing by declaring everything a problem.
        assert_eq!(removed, 1, "the ordinary file should still be removed");
        assert!(!good.exists());
        assert!(
            problems
                .iter()
                .any(|p| p.contains("could not remove") && p.contains("stubborn-dir")),
            "an undeletable entry must be reported, got: {problems:?}"
        );
        assert!(undeletable.exists(), "it really was not removed");
    }

    /// `ensure_db_dir` describes its failure instead of discarding it.
    #[test]
    fn ensure_db_dir_reports_a_directory_it_cannot_create() {
        let dir = tempfile::TempDir::new().unwrap();

        // Positive control first: a creatable parent succeeds.
        let ok_db = dir.path().join("var/lib/das-backup/backup-index.db");
        ensure_db_dir(&ok_db.to_string_lossy()).expect("a creatable parent must succeed");
        assert!(ok_db.parent().unwrap().is_dir());

        // A FILE where the parent directory needs to be: create_dir_all fails
        // ENOTDIR/EEXIST for root too.
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, "x").unwrap();
        let bad_db = blocker.join("nested/backup-index.db");
        let err = ensure_db_dir(&bad_db.to_string_lossy())
            .expect_err("a parent that cannot be created must be an error");
        assert!(
            err.contains("could not create database directory"),
            "got: {err}"
        );
    }

    // -----------------------------------------------------------------------
    // Host seams: the runner, the schedule, and the cores of the root-only
    // entry points, each driven against a throwaway directory.
    // -----------------------------------------------------------------------

    /// A config that passes `validate()`, installed entirely under `base`:
    /// the database path is absolute, so it has to be pointed there too.
    fn valid_config(base: &Path) -> Config {
        let mut config = Config::default();
        config.general.db_path = base
            .join("var/lib/das-backup/backup-index.db")
            .to_string_lossy()
            .into_owned();
        config.sources.push(Source {
            label: "test".to_string(),
            volume: "/test".to_string(),
            subvolumes: vec![SubvolConfig {
                name: "@".to_string(),
                manual_only: false,
                snapshot_name: None,
                ..Default::default()
            }],
            device: "/dev/sda".to_string(),
            snapshot_dir: ".btrbk-snapshots".into(),
            target_subdirs: vec![],
            target_labels: vec![],
        });
        config
            .targets
            .push(exposure_target("tgt", &["ABC123"], None));
        config
    }

    fn installed_paths(base: &Path) -> (PathBuf, PathBuf) {
        (
            base.join("etc/das-backup/config.toml"),
            base.join("etc/das-backup/.manifest"),
        )
    }

    /// Install `config` under `base`, as `install` does under `/`.
    fn install_config_into(config: &Config, base: &Path) -> (PathBuf, PathBuf) {
        let (config_path, manifest_path) = installed_paths(base);
        install_to_prefix(config, base, &config_path, &manifest_path).unwrap();
        (config_path, manifest_path)
    }

    fn manifest_lines(manifest_path: &Path) -> Vec<String> {
        std::fs::read_to_string(manifest_path)
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// Stands in for `systemctl`: records every invocation, and fails the ones
    /// whose arguments contain a string in `failing`.
    struct FakeSystemctl {
        calls: std::cell::RefCell<Vec<Vec<String>>>,
        failing: Vec<&'static str>,
    }

    impl FakeSystemctl {
        fn new(failing: &[&'static str]) -> Self {
            Self {
                calls: std::cell::RefCell::new(Vec::new()),
                failing: failing.to_vec(),
            }
        }

        fn run(&self, args: &[&str]) -> Result<(), String> {
            self.calls
                .borrow_mut()
                .push(args.iter().map(|a| a.to_string()).collect());
            if args.iter().any(|a| self.failing.contains(a)) {
                Err(format!("systemctl {} exited with 1", args.join(" ")))
            } else {
                Ok(())
            }
        }

        fn calls(&self) -> Vec<String> {
            self.calls.borrow().iter().map(|c| c.join(" ")).collect()
        }
    }

    const UNINSTALL_UNIT_CALLS: [&str; 5] = [
        "disable --now das-backup.timer",
        "disable --now das-backup-full.timer",
        "disable --now das-scrub.timer",
        "disable --now das-backup-doctor.timer",
        "daemon-reload",
    ];

    #[test]
    fn command_status_separates_success_from_a_failed_or_missing_command() {
        assert_eq!(command_status("sh", &["-c", "exit 0"]), Ok(()));
        // A command that ran and failed is the case finding #5 was about: it
        // must not come back as Ok.
        let err = command_status("sh", &["-c", "exit 3"]).unwrap_err();
        assert!(err.contains("sh -c exit 3 exited with"), "{err}");
        let err = command_status("/nonexistent/das-no-such-binary", &["enable"]).unwrap_err();
        assert!(
            err.contains("/nonexistent/das-no-such-binary enable could not be run"),
            "{err}"
        );
    }

    fn schedule_ops(config: &Config) -> Vec<String> {
        schedule_unit_operations(config)
            .iter()
            .map(|op| op.join(" "))
            .collect()
    }

    #[test]
    fn schedule_enables_every_timer_and_the_scrub_timer_only_when_scrub_is_enabled() {
        let mut config = Config::default();
        config.init.system = InitSystem::Systemd;

        config.scrub.enabled = true;
        assert_eq!(
            schedule_ops(&config),
            vec![
                "daemon-reload",
                "enable --now das-backup.timer",
                "enable --now das-backup-full.timer",
                "enable --now das-scrub.timer",
                "enable --now das-backup-doctor.timer",
            ]
        );

        // bd DAS-Backup-Manager-atq: `enabled = false` must turn an already
        // enabled scrub timer OFF, not merely leave it alone — and must not
        // take the backup or doctor timers with it.
        config.scrub.enabled = false;
        assert_eq!(
            schedule_ops(&config),
            vec![
                "daemon-reload",
                "enable --now das-backup.timer",
                "enable --now das-backup-full.timer",
                "disable --now das-scrub.timer",
                "enable --now das-backup-doctor.timer",
            ]
        );
    }

    #[test]
    fn schedule_touches_no_systemd_unit_on_another_init_system() {
        // There the schedule is the generated cron entry; systemctl may not
        // even exist, and calling it would fail the whole install.
        for system in [InitSystem::Openrc, InitSystem::Sysvinit] {
            let mut config = Config::default();
            config.init.system = system;
            config.scrub.enabled = true;
            assert_eq!(schedule_ops(&config), Vec::<String>::new());

            let systemctl = FakeSystemctl::new(&["daemon-reload", "--now"]);
            install_schedule(&config, &|args| systemctl.run(args))
                .expect("nothing to run, so nothing can fail");
            assert_eq!(systemctl.calls(), Vec::<String>::new());
        }
    }

    #[test]
    fn install_schedule_runs_every_operation_and_succeeds_when_all_do() {
        let mut config = Config::default();
        config.init.system = InitSystem::Systemd;
        let systemctl = FakeSystemctl::new(&[]);

        install_schedule(&config, &|args| systemctl.run(args)).expect("every operation succeeded");

        assert_eq!(systemctl.calls(), schedule_ops(&config));
        assert_eq!(systemctl.calls().len(), 5);
    }

    #[test]
    fn install_schedule_fails_when_any_unit_operation_fails_but_still_attempts_the_rest() {
        // Finding #5: a schedule that was not installed is not an install that
        // succeeded. One failed timer must not stop the others being enabled,
        // and must not be reported as success.
        let mut config = Config::default();
        config.init.system = InitSystem::Systemd;

        let systemctl = FakeSystemctl::new(&["das-backup.timer"]);
        let err = install_schedule(&config, &|args| systemctl.run(args))
            .expect_err("a timer that could not be enabled must fail the install");
        assert!(
            err.to_string()
                .starts_with("1 systemd unit operation(s) failed"),
            "{err}"
        );
        assert_eq!(systemctl.calls(), schedule_ops(&config));

        let systemctl = FakeSystemctl::new(&["daemon-reload", "--now"]);
        let err = install_schedule(&config, &|args| systemctl.run(args)).unwrap_err();
        assert!(
            err.to_string()
                .starts_with("5 systemd unit operation(s) failed"),
            "{err}"
        );
        assert!(err.to_string().contains("NOT fully installed"), "{err}");
        assert_eq!(systemctl.calls(), schedule_ops(&config));
    }

    #[test]
    fn install_makes_scripts_executable_and_nothing_else() {
        let dir = tempfile::tempdir().unwrap();
        let (_, manifest_path) = install_config_into(&valid_config(dir.path()), dir.path());

        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        let scripts = walk_installed_scripts(dir.path());
        assert!(!scripts.is_empty());
        for script in &scripts {
            // systemd and cron exec these directly.
            assert_eq!(mode(script), 0o755, "{}", script.display());
        }
        let mut others = 0;
        for entry in manifest_lines(&manifest_path) {
            let path = Path::new(&entry);
            if !scripts.iter().any(|s| s == path) {
                assert_eq!(mode(path) & 0o111, 0, "{entry} must not be executable");
                others += 1;
            }
        }
        assert!(others > 0, "the install also writes non-script files");
    }

    #[test]
    fn upgrade_overwrites_a_script_exactly_as_old_as_the_binary() {
        // The 2lj guard protects a script NEWER than the binary. One with the
        // same mtime is not newer — nothing says the embedded copy is stale —
        // so it is refreshed like any older file.
        let dir = tempfile::tempdir().unwrap();
        install_into(dir.path());
        let script = walk_installed_scripts(dir.path())
            .into_iter()
            .next()
            .expect("install should have produced at least one script");
        let exe_mtime = std::fs::metadata(std::env::current_exe().unwrap())
            .unwrap()
            .modified()
            .unwrap();
        std::fs::write(&script, b"same age as the binary\n").unwrap();
        filetime::set_file_mtime(&script, filetime::FileTime::from_system_time(exe_mtime)).unwrap();
        assert_eq!(
            std::fs::metadata(&script).unwrap().modified().unwrap(),
            exe_mtime,
            "fixture must sit exactly on the boundary"
        );

        install_into(dir.path());

        assert_ne!(
            std::fs::read(&script).unwrap(),
            b"same age as the binary\n".to_vec(),
            "a script no newer than the binary must be refreshed"
        );
    }

    #[test]
    fn upgrade_never_prunes_the_config_under_another_spelling_of_its_path() {
        // The prune must compare paths, not strings: a manifest entry naming
        // the config some other way is still the config, and removing it
        // would leave the next backup with nothing to read.
        let dir = tempfile::tempdir().unwrap();
        let (config_path, manifest_path) = install_into(dir.path());
        let respelled = dir.path().join("etc/das-backup/./config.toml");
        assert_ne!(respelled.to_string_lossy(), config_path.to_string_lossy());
        let mut manifest = std::fs::read_to_string(&manifest_path).unwrap();
        manifest.push('\n');
        manifest.push_str(&respelled.to_string_lossy());
        std::fs::write(&manifest_path, manifest).unwrap();

        install_into(dir.path());

        assert!(
            config_path.exists(),
            "the config was pruned as a stale file"
        );
    }

    #[test]
    fn uninstall_without_a_manifest_touches_nothing() {
        // No manifest means this tool has nothing it can prove it installed.
        let dir = tempfile::tempdir().unwrap();
        let (config_path, manifest_path) = installed_paths(dir.path());
        Config::default().save(&config_path).unwrap();
        let systemctl = FakeSystemctl::new(&[]);

        uninstall_with(&config_path, &manifest_path, true, &|args| {
            systemctl.run(args)
        })
        .unwrap();

        assert_eq!(systemctl.calls(), Vec::<String>::new());
        assert!(config_path.exists());
    }

    #[test]
    fn uninstall_disables_the_timers_and_removes_what_the_manifest_lists() {
        let dir = tempfile::tempdir().unwrap();
        let config = valid_config(dir.path());
        let (config_path, manifest_path) = install_config_into(&config, dir.path());
        let installed = manifest_lines(&manifest_path);
        assert!(installed.len() > 1);
        let db = PathBuf::from(&config.general.db_path);
        std::fs::write(&db, "index").unwrap();
        let systemctl = FakeSystemctl::new(&[]);

        uninstall_with(&config_path, &manifest_path, false, &|args| {
            systemctl.run(args)
        })
        .unwrap();

        // A timer left enabled keeps firing at 03:00 against files that are
        // gone, so all four are disabled, then systemd is told.
        assert_eq!(systemctl.calls(), UNINSTALL_UNIT_CALLS);
        for entry in &installed {
            assert!(!Path::new(entry).exists(), "{entry} was left behind");
        }
        assert!(!manifest_path.exists());
        assert!(
            !config_path.parent().unwrap().exists(),
            "an emptied config directory is removed"
        );
        // Not asked for, so the index survives.
        assert!(db.exists());
    }

    #[test]
    fn uninstall_removes_the_database_only_when_asked_and_keeps_operator_files() {
        let dir = tempfile::tempdir().unwrap();
        let config = valid_config(dir.path());
        let (config_path, manifest_path) = install_config_into(&config, dir.path());
        let db = PathBuf::from(&config.general.db_path);
        std::fs::write(&db, "index").unwrap();
        let operator_file = config_path.parent().unwrap().join("notes.txt");
        std::fs::write(&operator_file, "mine").unwrap();
        let systemctl = FakeSystemctl::new(&[]);

        uninstall_with(&config_path, &manifest_path, true, &|args| {
            systemctl.run(args)
        })
        .unwrap();

        assert!(!db.exists(), "--remove-db was asked for");
        assert!(!config_path.exists());
        assert!(
            operator_file.exists(),
            "a file this tool did not install must survive the uninstall"
        );
    }

    #[test]
    fn uninstall_still_removes_the_files_when_systemctl_fails() {
        // A unit that will not disable is a warning: refusing to remove the
        // files as well would leave the host half-installed with no way out.
        let dir = tempfile::tempdir().unwrap();
        let (config_path, manifest_path) =
            install_config_into(&valid_config(dir.path()), dir.path());
        let installed = manifest_lines(&manifest_path);
        let systemctl = FakeSystemctl::new(&["daemon-reload", "--now"]);

        uninstall_with(&config_path, &manifest_path, false, &|args| {
            systemctl.run(args)
        })
        .unwrap();

        assert_eq!(systemctl.calls(), UNINSTALL_UNIT_CALLS);
        for entry in &installed {
            assert!(!Path::new(entry).exists(), "{entry} was left behind");
        }
    }

    fn relay_config(port: u16) -> Config {
        let mut config = Config::default();
        config.email.enabled = true;
        config.email.smtp_host = "127.0.0.1".to_string();
        config.email.smtp_port = port;
        config
    }

    #[test]
    fn relay_reachable_is_true_only_when_something_is_listening() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(relay_reachable(&relay_config(port)));

        // A port that is certainly closed and stays closed: the local end of
        // an established connection. Nothing listens on it, and while the
        // stream is alive no other process can be handed it to listen on — a
        // port that was merely released can be reused before the probe runs.
        let client = TcpStream::connect(("127.0.0.1", port)).unwrap();
        let closed = client.local_addr().unwrap().port();
        assert_ne!(closed, port);
        assert!(!relay_reachable(&relay_config(closed)));
    }

    /// Run `prepare_upgrade`, returning its result and the lines it reported.
    fn run_prepare_upgrade(
        config_path: &Path,
        relay_up: bool,
    ) -> (Result<Config, String>, Vec<String>) {
        let mut lines = Vec::new();
        let result = prepare_upgrade(config_path, &|_| relay_up, &mut |l| lines.push(l))
            .map_err(|e| e.to_string());
        (result, lines)
    }

    #[test]
    fn upgrade_refuses_to_run_without_an_existing_config() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");

        let (result, lines) = run_prepare_upgrade(&config_path, true);

        assert_eq!(
            result.unwrap_err(),
            format!(
                "No config found at {}. Run 'btrdasd setup' first.",
                config_path.display()
            )
        );
        assert!(lines.is_empty());
        assert!(!config_path.exists(), "upgrade must not invent a config");
    }

    #[test]
    fn upgrade_stamps_an_older_config_with_this_version_and_saves_it() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        let mut config = Config::default();
        config.general.version = "0.0.1".to_string();
        config.email.enabled = false;
        config.save(&config_path).unwrap();
        let this = env!("CARGO_PKG_VERSION");

        let (result, lines) = run_prepare_upgrade(&config_path, false);

        assert_eq!(result.unwrap().general.version, this);
        assert_eq!(
            lines,
            vec![format!("Updating config version: 0.0.1 -> {this}")]
        );
        assert_eq!(
            Config::load(&config_path).unwrap().general.version,
            this,
            "the new version must reach the file, not just the returned config"
        );
    }

    #[test]
    fn upgrade_saves_a_migration_even_when_the_version_is_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        let mut config = relay_config(1025);
        config.email.enabled = false;
        config.save(&config_path).unwrap();

        let (result, lines) = run_prepare_upgrade(&config_path, false);

        assert_eq!(result.unwrap().email.smtp_port, 25);
        // The version did not change, so no version line is reported.
        assert_eq!(
            lines,
            vec![
                "Migrating config: [email] smtp_port 1025 -> 25 \
                 (Protonmail Bridge -> local mail relay)"
                    .to_string()
            ]
        );
        assert_eq!(Config::load(&config_path).unwrap().email.smtp_port, 25);
    }

    #[test]
    fn upgrade_leaves_a_current_config_file_untouched() {
        // Nothing changed, so the operator's file is not rewritten: a save
        // would re-serialize it and drop everything that is not a setting.
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        let mut config = Config::default();
        config.email.enabled = false;
        config.save(&config_path).unwrap();
        let mut on_disk = std::fs::read_to_string(&config_path).unwrap();
        on_disk.push_str("\n# operator note that a rewrite would drop\n");
        std::fs::write(&config_path, &on_disk).unwrap();

        let (result, lines) = run_prepare_upgrade(&config_path, false);

        result.unwrap();
        assert_eq!(lines, Vec::<String>::new());
        assert_eq!(std::fs::read_to_string(&config_path).unwrap(), on_disk);
    }

    #[test]
    fn upgrade_warns_about_the_relay_only_when_email_is_enabled_and_it_is_down() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        let warning = vec![
            "Warning: email is enabled but nothing is listening on 127.0.0.1:2525".to_string(),
            "  Reports will be saved to disk but not delivered.".to_string(),
            "  Check the local mail relay: systemctl status postfix".to_string(),
        ];

        for (enabled, relay_up, expected) in [
            (true, false, warning.clone()),
            (true, true, Vec::new()),
            // Email off: an unreachable relay is nobody's problem.
            (false, false, Vec::new()),
            (false, true, Vec::new()),
        ] {
            let mut config = relay_config(2525);
            config.email.enabled = enabled;
            config.save(&config_path).unwrap();

            let (result, lines) = run_prepare_upgrade(&config_path, relay_up);

            // A warning, never a failure: the upgrade goes ahead either way.
            result.unwrap();
            assert_eq!(lines, expected, "enabled={enabled} relay_up={relay_up}");
        }
    }

    // -----------------------------------------------------------------
    // bd DAS-Backup-Manager-6wt — an upgrade restarts the D-Bus helper
    // -----------------------------------------------------------------

    use crate::setup::config::InitSystem;

    /// What one `restart_helper` run did.
    struct Restarted {
        result: Result<HelperRestart, String>,
        lines: Vec<String>,
        /// Each systemctl call.
        calls: Vec<String>,
    }

    /// Run `restart_helper` under setup's locks, taken on scratch files, with
    /// the unit in `state` (`Err`: unreadable) and systemctl answering
    /// `systemctl`.
    fn run_restart_helper(
        init: InitSystem,
        state: Result<&str, &str>,
        systemctl: Result<(), &str>,
    ) -> Restarted {
        let locks = ScratchLocks::new();
        let held = match take_setup_locks(&locks.site, UPGRADE_JOB).unwrap() {
            SetupLockAttempt::Taken(held) => held,
            SetupLockAttempt::Busy(why) => panic!("both scratch locks are free: {why}"),
        };
        let mut config = Config::default();
        config.init.system = init;
        let calls = std::cell::RefCell::new(Vec::new());
        let active_state = || state.map(str::to_string).map_err(str::to_string);
        let run = |args: &[&str]| {
            calls.borrow_mut().push(args.join(" "));
            systemctl.map_err(str::to_string)
        };
        let mut lines = Vec::new();
        let result = restart_helper(
            &config,
            &HelperHost {
                active_state: &active_state,
                systemctl: &run,
            },
            &held,
            &mut |l| lines.push(l),
        );
        Restarted {
            result,
            lines,
            calls: calls.into_inner(),
        }
    }

    const RESTARTED: &str = "Restarted btrdasd-helper.service: the D-Bus helper now runs the \
                             binary just installed.";

    #[test]
    fn a_running_helper_is_restarted() {
        for state in ["active", "activating", "reloading", "active\n"] {
            let r = run_restart_helper(InitSystem::Systemd, Ok(state), Ok(()));
            assert_eq!(r.result, Ok(HelperRestart::Done), "{state:?}");
            assert_eq!(
                r.calls,
                vec!["try-restart btrdasd-helper.service"],
                "{state:?}"
            );
            assert_eq!(r.lines, vec![RESTARTED.to_string()], "{state:?}");
        }
    }

    #[test]
    fn a_restart_that_fails_or_does_not_return_is_an_error() {
        let not_restarted = Err("btrdasd-helper.service was not restarted — see above".to_string());
        for e in [
            "systemctl try-restart btrdasd-helper.service exited with exit status: 1",
            "systemctl try-restart btrdasd-helper.service did not return within 300 s",
        ] {
            let r = run_restart_helper(InitSystem::Systemd, Ok("active"), Err(e));
            assert_eq!(r.result, not_restarted, "{e}");
            assert_eq!(r.calls, vec!["try-restart btrdasd-helper.service"], "{e}");
            assert_eq!(
                r.lines,
                vec![format!(
                    "ERROR: could not restart btrdasd-helper.service ({e}). The running helper \
                     may be the old binary, and the GUI history fails on a run whose snapshot \
                     counts are unknown. See systemctl status btrdasd-helper.service and \
                     journalctl -u btrdasd-helper.service, then run: sudo systemctl restart \
                     btrdasd-helper.service"
                )]
            );
        }
    }

    #[test]
    fn a_helper_that_is_not_running_is_left_for_dbus_to_start() {
        for state in ["inactive", "failed"] {
            let r = run_restart_helper(InitSystem::Systemd, Ok(state), Ok(()));
            assert_eq!(r.result, Ok(HelperRestart::Done), "{state}");
            assert_eq!(r.calls, Vec::<String>::new(), "{state}");
            assert_eq!(
                r.lines,
                vec![format!(
                    "btrdasd-helper.service is {state}: nothing to restart — D-Bus starts the \
                     helper from the new binary when it is next needed."
                )]
            );
        }
    }

    #[test]
    fn not_knowing_whether_the_helper_runs_is_an_error_and_restarts_nothing() {
        let r = run_restart_helper(
            InitSystem::Systemd,
            Err("systemctl could not be run: No such file or directory"),
            Ok(()),
        );
        assert_eq!(
            r.result,
            Err("btrdasd-helper.service was not restarted — see above".to_string())
        );
        assert_eq!(r.calls, Vec::<String>::new());
        assert_eq!(
            r.lines,
            vec![
                "ERROR: cannot tell whether btrdasd-helper.service is running (systemctl could \
                 not be run: No such file or directory) — not restarted. If it is running, \
                 restart it when no backup or restore job is: sudo systemctl try-restart \
                 btrdasd-helper.service"
                    .to_string()
            ]
        );
    }

    #[test]
    fn without_systemd_the_restart_is_deferred_to_the_operator() {
        for init in [InitSystem::Sysvinit, InitSystem::Openrc] {
            let r = run_restart_helper(init.clone(), Ok("active"), Ok(()));
            assert_eq!(r.result, Ok(HelperRestart::Deferred), "{init:?}");
            assert_eq!(r.calls, Vec::<String>::new(), "{init:?}");
            assert_eq!(
                r.lines,
                vec![format!(
                    "The D-Bus helper is not a systemd unit on this init system: if \
                     btrdasd-helper is running, restart it — it keeps running the binary it \
                     started with, and one older than {} cannot read a run whose snapshot \
                     counts are unknown.",
                    env!("CARGO_PKG_VERSION")
                )]
            );
        }
    }

    #[test]
    fn the_exit_status_says_done_restart_left_to_the_operator_or_refused() {
        assert_eq!(exit_status(SetupOutcome::Done), 0);
        assert_eq!(exit_status(SetupOutcome::RestartDeferred), 3);
        assert_eq!(exit_status(SetupOutcome::Refused), 75);
        assert_eq!(UPGRADE_RESTART_DEFERRED_EXIT, 3);
        // EX_TEMPFAIL, as `walk` and `restore --no-wait` exit on a held lock.
        assert_eq!(SETUP_REFUSED_EXIT, 75);
        assert_eq!(
            SETUP_REFUSED_EXIT,
            buttered_dasd::maintenance::DEFERRED_EXIT_CODE
        );
    }

    /// The command lines of this process's own live children, NUL-joined as
    /// `/proc` keeps them. Only its own: another test run on the host — a
    /// parallel mutation run, a second CI job — may be running the very same
    /// command at that moment.
    fn process_command_lines() -> Vec<String> {
        let me = std::process::id();
        std::fs::read_dir("/proc")
            .unwrap()
            .filter_map(|e| {
                let dir = e.ok()?.path();
                // The parent pid is the field after the state, read after the
                // last `)`: the command name may hold spaces and parentheses.
                let stat = std::fs::read_to_string(dir.join("stat")).ok()?;
                let ppid: u32 = stat
                    .rsplit_once(')')?
                    .1
                    .split_whitespace()
                    .nth(1)?
                    .parse()
                    .ok()?;
                (ppid == me).then(|| std::fs::read(dir.join("cmdline")).ok())?
            })
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .collect()
    }

    #[test]
    fn a_bounded_command_reports_its_status_or_that_it_did_not_return() {
        assert_eq!(
            command_status_within("true", &[], Duration::from_secs(5)),
            Ok(())
        );
        assert_eq!(
            command_status_within("sh", &["-c", "exit 3"], Duration::from_secs(5)),
            Err("sh -c exit 3 exited with exit status: 3".to_string())
        );
        let err = command_status_within("/nonexistent/6wt-command", &[], Duration::from_secs(5))
            .unwrap_err();
        assert!(
            err.starts_with("/nonexistent/6wt-command could not be run: "),
            "{err}"
        );

        // Still running at the limit: killed and reaped, reported as not
        // having returned — within moments of the limit, not when it would
        // have ended. A distinctive duration finds it in /proc.
        let started = std::time::Instant::now();
        assert_eq!(
            command_status_within("sleep", &["6.0613"], Duration::from_secs(1)),
            Err("sleep 6.0613 did not return within 1 s".to_string())
        );
        let took = started.elapsed();
        assert!(
            took >= Duration::from_secs(1),
            "returned before the limit: {took:?}"
        );
        assert!(
            took < Duration::from_secs(4),
            "waited past the limit: {took:?}"
        );
        assert!(
            !process_command_lines()
                .iter()
                .any(|c| c == "sleep\u{0}6.0613\u{0}"),
            "the command must not be left running"
        );
    }

    // -----------------------------------------------------------------
    // bd DAS-Backup-Manager-6wt fix round 3 — setup writes nothing while a
    // backup or a maintenance job runs: it takes both locks before its
    // first write, holds them to the end, and lets them go on every path
    // -----------------------------------------------------------------

    use std::collections::{BTreeMap, BTreeSet};
    use std::time::SystemTime;

    /// Scratch stand-ins for the two locks setup takes, in a directory of
    /// their own, so they are never part of a tree a test compares.
    struct ScratchLocks {
        _dir: tempfile::TempDir,
        site: SetupLockSite,
    }

    impl ScratchLocks {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let site = SetupLockSite {
                backup: dir.path().join("das-backup.lock"),
                maintenance: dir.path().join("das-maintenance.lock"),
            };
            Self { _dir: dir, site }
        }

        /// The backup singleton, held as `backup-run.sh` holds it: no record.
        fn hold_backup(&self) -> FileLock {
            FileLock::try_acquire(&self.site.backup)
                .unwrap()
                .expect("the scratch singleton is free")
        }

        /// The maintenance lock, held as a scrub holds it, record and all.
        fn hold_maintenance(&self) -> MaintenanceHeld {
            MaintenanceHeld::try_acquire_at(&self.site.maintenance, "btrdasd scrub run")
                .unwrap()
                .expect("the scratch maintenance lock is free")
        }

        /// What a job asking for each lock at this moment would find.
        fn state(&self) -> String {
            let found = |path: &Path| match FileLock::try_acquire(path).unwrap() {
                Some(_) => "free",
                None => "held",
            };
            format!(
                "backup {}, maintenance {}",
                found(&self.site.backup),
                found(&self.site.maintenance)
            )
        }

        /// The maintenance lock's record, as a job that finds it held reads it.
        fn record(&self) -> String {
            std::fs::read_to_string(&self.site.maintenance).unwrap_or_default()
        }
    }

    /// Every file under a directory, with its bytes and modification time.
    type Tree = BTreeMap<PathBuf, (Vec<u8>, SystemTime)>;

    fn tree(dir: &Path) -> Tree {
        walkdir::WalkDir::new(dir)
            .into_iter()
            .map(Result::unwrap)
            .filter(|entry| entry.file_type().is_file())
            .map(|entry| {
                let modified = entry.metadata().unwrap().modified().unwrap();
                let path = entry.into_path();
                let bytes = std::fs::read(&path).unwrap();
                (path, (bytes, modified))
            })
            .collect()
    }

    /// 2001-09-09: a file still dated so has not been rewritten since.
    fn long_ago() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000_000)
    }

    /// Date every file under `dir` [`long_ago`], so that a rewrite shows even
    /// when it puts the same bytes back.
    fn date_back(dir: &Path) {
        for path in tree(dir).keys() {
            std::fs::File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(long_ago())
                .unwrap();
        }
    }

    /// Every path whose bytes or modification time differ between the two
    /// trees, or that is in only one of them.
    fn changed(before: &Tree, after: &Tree) -> Vec<PathBuf> {
        before
            .keys()
            .chain(after.keys())
            .filter(|path| before.get(*path) != after.get(*path))
            .cloned()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    /// Who, besides setup, holds a lock as it starts.
    #[derive(Clone, Copy, Debug, Default, PartialEq)]
    enum Holding {
        #[default]
        Nobody,
        /// A backup: the singleton, as `backup-run.sh` holds it.
        Backup,
        /// A scrub: the maintenance lock, with its record.
        Maintenance,
    }

    /// How [`upgrade_installed_tree`] sets the host up.
    #[derive(Default)]
    struct UpgradeCase<'a> {
        holding: Holding,
        /// `None`: systemd.
        init: Option<InitSystem>,
        /// Regenerating fails, after writing, as a failed timer enable does.
        install_fails: bool,
        /// Every `systemctl` call fails with this.
        systemctl_fails: Option<&'a str>,
        /// Lock paths to use instead of the scratch ones.
        site: Option<SetupLockSite>,
    }

    /// What one `upgrade_with` run on an installed tree did.
    struct UpgradeRun {
        outcome: Result<SetupOutcome, String>,
        /// What it said on stdout, and on stderr.
        lines: Vec<String>,
        warnings: Vec<String>,
        /// Each step that writes or restarts, in order, with the locks as a
        /// job asking for them at that moment would have found them. The
        /// relay probe runs right after the config is written.
        steps: Vec<String>,
        /// Every file the run rewrote, created or removed.
        changed: Vec<PathBuf>,
        /// The installed config, once it returned.
        config: Config,
        /// Whether the maintenance lock file exists once it returned: setup
        /// creates it the moment it opens it.
        maintenance_opened: bool,
        /// The locks, and the maintenance lock's record, once it returned —
        /// the other holder, if any, still holding its own.
        locks_after: String,
        record_after: String,
        maintenance_lock: PathBuf,
        backup_lock: PathBuf,
    }

    /// Run `upgrade_with` on the tree an older release installed — the config
    /// stamped 0.0.1, so the upgrade has something to write, and every file
    /// dated [`long_ago`] — with email on, the helper running, and the host
    /// as `case` says.
    fn upgrade_installed_tree(case: UpgradeCase) -> UpgradeRun {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();
        let mut config = valid_config(base);
        config.general.version = "0.0.1".to_string();
        config.email.enabled = true;
        config.init.system = case.init.clone().unwrap_or(InitSystem::Systemd);
        let (config_path, manifest_path) = install_config_into(&config, base);
        date_back(base);
        let before = tree(base);

        let locks = ScratchLocks::new();
        let site = case.site.as_ref().unwrap_or(&locks.site);
        let _backup = (case.holding == Holding::Backup).then(|| locks.hold_backup());
        let _scrub = (case.holding == Holding::Maintenance).then(|| locks.hold_maintenance());

        // Each step notes the locks and the maintenance lock's record, then
        // leaves its own mark in that record. The next step finding the mark
        // proves the lock was not let go and taken again in between: letting
        // go empties the record, and taking it writes setup's own.
        let steps = std::cell::RefCell::new(Vec::new());
        let step = |name: &str| {
            steps.borrow_mut().push(format!(
                "{name} ({}; record: {})",
                locks.state(),
                locks.record().trim_end()
            ));
            if locks.site.maintenance.exists() {
                std::fs::write(&locks.site.maintenance, format!("mark of {name}\n")).unwrap();
            }
        };
        let relay_up = |_: &Config| {
            step("relay probe");
            true
        };
        let install = |config: &Config, _: &SetupLocks| -> Result<(), Box<dyn std::error::Error>> {
            step("install");
            install_to_prefix(config, base, &config_path, &manifest_path)?;
            if case.install_fails {
                return Err("1 systemd unit operation(s) failed".into());
            }
            Ok(())
        };
        let active_state = || Ok("active".to_string());
        let systemctl = |args: &[&str]| {
            step(&format!("systemctl {}", args.join(" ")));
            case.systemctl_fails.map_or(Ok(()), |e| Err(e.to_string()))
        };
        let mut lines = Vec::new();
        let mut warnings = Vec::new();
        let outcome = upgrade_with(
            site,
            &config_path,
            &relay_up,
            &install,
            &HelperHost {
                active_state: &active_state,
                systemctl: &systemctl,
            },
            &mut |line| lines.push(line),
            &mut |line| warnings.push(line),
        )
        .map_err(|e| e.to_string());
        let maintenance_opened = locks.site.maintenance.exists();
        UpgradeRun {
            outcome,
            lines,
            warnings,
            steps: steps.into_inner(),
            changed: changed(&before, &tree(base)),
            config: Config::load(&config_path).unwrap(),
            maintenance_opened,
            locks_after: locks.state(),
            record_after: locks.record(),
            maintenance_lock: locks.site.maintenance.clone(),
            backup_lock: locks.site.backup.clone(),
        }
    }

    const BOTH_HELD: &str = "backup held, maintenance held";
    const BOTH_FREE: &str = "backup free, maintenance free";

    /// The steps of an upgrade that wrote, and restarted when `restart`, all
    /// under one unbroken hold of both locks: the first finds setup's record,
    /// each later one the mark the step before it left.
    fn steps_under_both_locks(restart: bool) -> Vec<String> {
        let mut steps = vec![
            format!(
                "relay probe ({BOTH_HELD}; record: btrdasd setup --upgrade pid {})",
                std::process::id()
            ),
            format!("install ({BOTH_HELD}; record: mark of relay probe)"),
        ];
        if restart {
            steps.push(format!(
                "systemctl try-restart btrdasd-helper.service ({BOTH_HELD}; record: mark of \
                 install)"
            ));
        }
        steps
    }

    #[test]
    fn an_upgrade_writes_nothing_and_exits_75_while_a_backup_holds_its_lock() {
        let run = upgrade_installed_tree(UpgradeCase {
            holding: Holding::Backup,
            ..Default::default()
        });

        assert_eq!(run.outcome, Ok(SetupOutcome::Refused));
        assert_eq!(exit_status(SetupOutcome::Refused), 75);
        assert_eq!(
            run.changed,
            Vec::<PathBuf>::new(),
            "not one file's bytes or modification time may change"
        );
        assert_eq!(run.config.general.version, "0.0.1");
        assert_eq!(
            run.steps,
            Vec::<String>::new(),
            "nothing written or restarted"
        );
        assert!(
            !run.maintenance_opened,
            "the maintenance lock is not even opened while the singleton is held"
        );
        assert_eq!(
            run.locks_after, "backup held, maintenance free",
            "the backup keeps its lock, and setup holds nothing"
        );
        assert_eq!(run.lines, Vec::<String>::new(), "a refusal goes to stderr");
        assert_eq!(
            run.warnings,
            vec![format!(
                "Refused, nothing written or removed: {} is held, so a backup is running — \
                 backup-run.sh, or btrdasd backup from the CLI or the GUI, which hold it from \
                 the start, while they wait for the DAS maintenance lock too — or another \
                 btrdasd setup is. A running backup reads the scripts and btrbk.conf that \
                 setup rewrites. Run setup again once it has finished (exit 75).",
                run.backup_lock.display()
            )]
        );
    }

    #[test]
    fn an_upgrade_writes_nothing_and_exits_75_while_a_job_holds_the_maintenance_lock() {
        let run = upgrade_installed_tree(UpgradeCase {
            holding: Holding::Maintenance,
            ..Default::default()
        });

        assert_eq!(run.outcome, Ok(SetupOutcome::Refused));
        assert_eq!(exit_status(SetupOutcome::Refused), 75);
        assert_eq!(run.changed, Vec::<PathBuf>::new());
        assert_eq!(run.config.general.version, "0.0.1");
        assert_eq!(run.steps, Vec::<String>::new());
        assert_eq!(
            run.locks_after, "backup free, maintenance held",
            "setup lets the singleton go again; the scrub keeps its lock"
        );
        let pid = std::process::id();
        assert_eq!(
            run.record_after,
            format!("btrdasd scrub run pid {pid}\n"),
            "the holder's record is left as it was"
        );
        assert_eq!(run.lines, Vec::<String>::new(), "a refusal goes to stderr");
        assert_eq!(
            run.warnings,
            vec![format!(
                "Refused, nothing written or removed: the DAS maintenance lock {} is held by \
                 btrdasd scrub run pid {pid}. Run setup again once that job has finished \
                 (exit 75).",
                run.maintenance_lock.display()
            )]
        );
    }

    #[test]
    fn an_upgrade_holds_both_locks_through_every_write_and_the_restart_then_lets_go() {
        let run = upgrade_installed_tree(UpgradeCase::default());

        assert_eq!(run.outcome, Ok(SetupOutcome::Done));
        assert_eq!(exit_status(SetupOutcome::Done), 0);
        assert_eq!(run.warnings, Vec::<String>::new());
        assert_eq!(run.steps, steps_under_both_locks(true));
        assert_eq!(run.locks_after, BOTH_FREE);
        assert_eq!(
            run.record_after, "",
            "the record is emptied before letting go"
        );
        // Written: the config is stamped with this version, and the files
        // the install owns were rewritten.
        let this = env!("CARGO_PKG_VERSION");
        assert_eq!(run.config.general.version, this);
        for written in ["etc/das-backup/config.toml", "lib/das-backup/backup-run.sh"] {
            assert!(
                run.changed.iter().any(|p| p.ends_with(written)),
                "{written} not rewritten: {:?}",
                run.changed
            );
        }
        assert_eq!(
            run.lines.first(),
            Some(&format!("Updating config version: 0.0.1 -> {this}"))
        );
        assert!(
            run.lines.contains(&RESTARTED.to_string()),
            "{:?}",
            run.lines
        );
        assert_eq!(run.lines.last(), Some(&"Upgrade complete.".to_string()));
    }

    #[test]
    fn every_way_an_upgrade_fails_after_taking_the_locks_lets_both_go() {
        let not_restarted = "btrdasd-helper.service was not restarted — see above";
        for (install_fails, systemctl_fails, error) in [
            // The helper is restarted even when regenerating failed.
            (true, None, "1 systemd unit operation(s) failed"),
            (
                false,
                Some("systemctl try-restart btrdasd-helper.service exited with exit status: 1"),
                not_restarted,
            ),
            (
                false,
                Some("systemctl try-restart btrdasd-helper.service did not return within 300 s"),
                not_restarted,
            ),
        ] {
            let run = upgrade_installed_tree(UpgradeCase {
                install_fails,
                systemctl_fails,
                ..Default::default()
            });

            assert_eq!(run.outcome, Err(error.to_string()));
            assert_eq!(run.steps, steps_under_both_locks(true), "{error}");
            assert_eq!(run.locks_after, BOTH_FREE, "{error}");
            assert_eq!(run.record_after, "", "{error}");
            assert!(
                !run.lines.contains(&"Upgrade complete.".to_string()),
                "{error}: {:?}",
                run.lines
            );
        }
    }

    #[test]
    fn without_systemd_the_files_are_upgraded_under_both_locks_and_the_restart_is_left_to_the_operator()
     {
        for init in [InitSystem::Sysvinit, InitSystem::Openrc] {
            let run = upgrade_installed_tree(UpgradeCase {
                init: Some(init.clone()),
                ..Default::default()
            });

            assert_eq!(run.outcome, Ok(SetupOutcome::RestartDeferred), "{init:?}");
            assert_eq!(exit_status(SetupOutcome::RestartDeferred), 3);
            assert_eq!(run.steps, steps_under_both_locks(false), "{init:?}");
            assert_eq!(run.locks_after, BOTH_FREE, "{init:?}");
            assert_eq!(run.config.general.version, env!("CARGO_PKG_VERSION"));
            assert_eq!(
                run.lines[run.lines.len() - 2..],
                [
                    format!(
                        "The D-Bus helper is not a systemd unit on this init system: if \
                         btrdasd-helper is running, restart it — it keeps running the binary it \
                         started with, and one older than {} cannot read a run whose snapshot \
                         counts are unknown.",
                        env!("CARGO_PKG_VERSION")
                    ),
                    "Files upgraded; the btrdasd-helper.service restart is deferred (exit 3)."
                        .to_string(),
                ],
                "{init:?}"
            );
        }
    }

    #[test]
    fn an_upgrade_whose_locks_cannot_be_opened_writes_nothing() {
        let unopenable = || Path::new("/dev/null/das.lock").to_path_buf();
        let scratch = tempfile::tempdir().unwrap();
        for site in [
            SetupLockSite {
                backup: unopenable(),
                maintenance: scratch.path().join("das-maintenance.lock"),
            },
            SetupLockSite {
                backup: scratch.path().join("das-backup.lock"),
                maintenance: unopenable(),
            },
        ] {
            let backup = site.backup.clone();
            let run = upgrade_installed_tree(UpgradeCase {
                site: Some(site),
                ..Default::default()
            });

            let error = run.outcome.expect_err("an unopenable lock is never free");
            assert!(error.contains("/dev/null/das.lock"), "{error}");
            assert_eq!(run.changed, Vec::<PathBuf>::new(), "{error}");
            assert_eq!(run.steps, Vec::<String>::new(), "{error}");
            if backup != unopenable() {
                assert!(
                    FileLock::try_acquire(&backup).unwrap().is_some(),
                    "the singleton taken before the failure is let go"
                );
            }
        }
    }

    #[test]
    fn an_upgrade_that_cannot_read_its_config_writes_nothing_and_lets_both_locks_go() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        std::fs::write(&config_path, "this is not = = valid toml [[[\n").unwrap();
        date_back(dir.path());
        let before = tree(dir.path());
        let locks = ScratchLocks::new();
        let reached = std::cell::Cell::new(false);
        let install = |_: &Config, _: &SetupLocks| -> Result<(), Box<dyn std::error::Error>> {
            reached.set(true);
            Ok(())
        };
        let active_state = || Ok("active".to_string());
        let systemctl = |_: &[&str]| -> Result<(), String> {
            reached.set(true);
            Ok(())
        };

        let outcome = upgrade_with(
            &locks.site,
            &config_path,
            &|_| true,
            &install,
            &HelperHost {
                active_state: &active_state,
                systemctl: &systemctl,
            },
            &mut |_| {},
            &mut |_| {},
        );

        assert!(outcome.is_err());
        assert!(!reached.get(), "nothing regenerated or restarted");
        assert_eq!(changed(&before, &tree(dir.path())), Vec::<PathBuf>::new());
        assert_eq!(locks.state(), BOTH_FREE);
        assert_eq!(locks.record(), "");
    }

    // The two locks themselves.

    #[test]
    fn setup_takes_the_singleton_then_the_maintenance_lock_and_holds_both_until_dropped() {
        let locks = ScratchLocks::new();

        let taken = match take_setup_locks(&locks.site, "btrdasd setup --upgrade").unwrap() {
            SetupLockAttempt::Taken(taken) => taken,
            SetupLockAttempt::Busy(why) => panic!("both scratch locks are free: {why}"),
        };
        assert_eq!(locks.state(), BOTH_HELD);
        assert_eq!(
            locks.record(),
            format!("btrdasd setup --upgrade pid {}\n", std::process::id()),
            "a job that finds the maintenance lock held can say it is setup"
        );

        drop(taken);
        assert_eq!(locks.state(), BOTH_FREE);
        assert_eq!(locks.record(), "");
    }

    #[test]
    fn setup_takes_neither_lock_while_a_backup_holds_the_singleton() {
        let locks = ScratchLocks::new();
        let backup = locks.hold_backup();

        match take_setup_locks(&locks.site, "btrdasd setup").unwrap() {
            SetupLockAttempt::Busy(why) => assert_eq!(why, backup_held_line(&locks.site.backup)),
            SetupLockAttempt::Taken(_) => panic!("the singleton is held"),
        }
        assert!(
            !locks.site.maintenance.exists(),
            "the maintenance lock must not be opened once the singleton is held"
        );
        assert_eq!(locks.state(), "backup held, maintenance free");
        drop(backup);
        assert_eq!(locks.state(), BOTH_FREE);
    }

    #[test]
    fn setup_lets_the_singleton_go_while_a_job_holds_the_maintenance_lock() {
        let locks = ScratchLocks::new();
        let scrub = locks.hold_maintenance();

        match take_setup_locks(&locks.site, "btrdasd setup").unwrap() {
            SetupLockAttempt::Busy(why) => assert_eq!(
                why,
                maintenance_held_line(
                    &locks.site.maintenance,
                    &format!("btrdasd scrub run pid {}", std::process::id())
                )
            ),
            SetupLockAttempt::Taken(_) => panic!("the maintenance lock is held"),
        }
        assert_eq!(locks.state(), "backup free, maintenance held");
        drop(scrub);
    }

    #[test]
    fn a_lock_setup_cannot_open_is_an_error_never_free() {
        let scratch = tempfile::tempdir().unwrap();
        let free_backup = scratch.path().join("das-backup.lock");
        for (site, named) in [
            (
                SetupLockSite {
                    backup: PathBuf::from("/dev/null/das-backup.lock"),
                    maintenance: scratch.path().join("das-maintenance.lock"),
                },
                "/dev/null/das-backup.lock",
            ),
            (
                SetupLockSite {
                    backup: free_backup.clone(),
                    maintenance: PathBuf::from("/dev/null/das-maintenance.lock"),
                },
                "/dev/null/das-maintenance.lock",
            ),
        ] {
            match take_setup_locks(&site, "btrdasd setup") {
                Err(e) => assert!(e.contains(named), "{e}"),
                Ok(_) => panic!("{named} cannot be opened, so it is neither taken nor busy"),
            }
        }
        assert!(
            FileLock::try_acquire(&free_backup).unwrap().is_some(),
            "the singleton taken before the failure is let go"
        );
    }

    #[test]
    fn setup_takes_the_very_lock_files_a_backup_takes() {
        let site = SetupLockSite::production();
        assert_eq!(site.backup, Path::new("/run/das-backup.lock"));
        assert_eq!(site.maintenance, Path::new("/run/das-maintenance.lock"));
        // backup-run.sh, as this binary installs it.
        let script = include_str!("../../../scripts/backup-run.sh");
        assert!(script.contains("\nLOCKFILE=\"/run/das-backup.lock\"\n"));
        assert!(script.contains("\nMAINTENANCE_LOCKFILE=\"/run/das-maintenance.lock\"\n"));
    }

    // Every other mode that writes or removes installed files — the wizard,
    // `--modify` and `--force` install; `--uninstall`; `--uninstall-all` —
    // runs its core under the same two locks.

    #[derive(Clone, Copy, Debug)]
    enum Mode {
        Install,
        Uninstall,
        UninstallAll,
    }

    /// `mode`'s core on the tree under `base`, as its root-only binding runs
    /// it on `/`; `--uninstall` and `--uninstall-all` asked to remove the
    /// database too.
    fn run_mode(
        mode: Mode,
        base: &Path,
        config: &Config,
        systemctl: &FakeSystemctl,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let (config_path, manifest_path) = installed_paths(base);
        let systemctl = |args: &[&str]| systemctl.run(args);
        match mode {
            Mode::Install => install_to_prefix(config, base, &config_path, &manifest_path),
            Mode::Uninstall => uninstall_with(&config_path, &manifest_path, true, &systemctl),
            Mode::UninstallAll => {
                uninstall_all_with(base, &config_path, &manifest_path, true, &systemctl)
            }
        }
    }

    /// An installed tree under `base`, with a database, every file dated
    /// [`long_ago`].
    fn installed_tree(base: &Path) -> Config {
        let config = valid_config(base);
        install_config_into(&config, base);
        std::fs::write(&config.general.db_path, "index").unwrap();
        date_back(base);
        config
    }

    #[test]
    fn every_setup_mode_changes_nothing_while_either_lock_is_held() {
        for holding in [Holding::Backup, Holding::Maintenance] {
            for mode in [Mode::Install, Mode::Uninstall, Mode::UninstallAll] {
                let dir = tempfile::tempdir().unwrap();
                let config = installed_tree(dir.path());
                let before = tree(dir.path());
                let locks = ScratchLocks::new();
                let _backup = (holding == Holding::Backup).then(|| locks.hold_backup());
                let _scrub = (holding == Holding::Maintenance).then(|| locks.hold_maintenance());
                let systemctl = FakeSystemctl::new(&[]);

                let ran = under_setup_locks(&locks.site, "btrdasd setup", |_| {
                    run_mode(mode, dir.path(), &config, &systemctl)
                })
                .unwrap();

                let case = format!("{holding:?} {mode:?}");
                let Locked::Refused(why) = ran else {
                    panic!("{case}: ran while a lock was held");
                };
                assert!(
                    why.starts_with("Refused, nothing written or removed: "),
                    "{case}: {why}"
                );
                assert_eq!(
                    changed(&before, &tree(dir.path())),
                    Vec::<PathBuf>::new(),
                    "{case}"
                );
                assert_eq!(systemctl.calls(), Vec::<String>::new(), "{case}");
            }
        }
    }

    #[test]
    fn every_setup_mode_runs_under_both_locks_and_lets_them_go() {
        for mode in [Mode::Install, Mode::Uninstall, Mode::UninstallAll] {
            let dir = tempfile::tempdir().unwrap();
            let config = installed_tree(dir.path());
            let before = tree(dir.path());
            let locks = ScratchLocks::new();
            let systemctl = FakeSystemctl::new(&[]);
            let during = std::cell::RefCell::new(String::new());

            let ran = under_setup_locks(&locks.site, "btrdasd setup --uninstall", |_| {
                *during.borrow_mut() = locks.state();
                run_mode(mode, dir.path(), &config, &systemctl)
            })
            .unwrap();

            assert_eq!(ran, Locked::Ran(()), "{mode:?}");
            assert_eq!(during.into_inner(), BOTH_HELD, "{mode:?}");
            assert!(
                !changed(&before, &tree(dir.path())).is_empty(),
                "{mode:?} changed nothing"
            );
            assert_eq!(locks.state(), BOTH_FREE, "{mode:?}");
            assert_eq!(locks.record(), "", "{mode:?}");
        }
    }

    #[test]
    fn a_setup_change_that_fails_still_lets_both_locks_go() {
        let locks = ScratchLocks::new();
        let ran: Result<Locked<()>, _> = under_setup_locks(&locks.site, "btrdasd setup", |_| {
            Err("1 systemd unit operation(s) failed".into())
        });
        assert_eq!(
            ran.unwrap_err().to_string(),
            "1 systemd unit operation(s) failed"
        );
        assert_eq!(locks.state(), BOTH_FREE);
        assert_eq!(locks.record(), "");
    }

    /// Run `check_with` with fixed answers from the host, returning the report.
    fn run_check(
        config_path: &Path,
        manifest_path: &Path,
        relay_up: bool,
        export_db: Result<String, String>,
    ) -> Vec<String> {
        let dep =
            |name: &str, required: bool, path: Option<&str>| crate::setup::detect::DepStatus {
                name: name.to_string(),
                required,
                path: path.map(str::to_string),
            };
        let probes = CheckProbes {
            relay_up: &|_| relay_up,
            export_db: &|| export_db.clone(),
            // `mailx` stands for "asked for only when email is enabled".
            dependencies: &|email_enabled| {
                let mut deps = vec![
                    dep("btrbk", true, Some("/usr/bin/btrbk")),
                    dep("smartctl", true, None),
                    dep("mbuffer", false, None),
                ];
                if email_enabled {
                    deps.push(dep("mailx", true, Some("/usr/bin/mailx")));
                }
                deps
            },
        };
        let mut lines = Vec::new();
        check_with(config_path, manifest_path, &probes, &mut |l| lines.push(l)).unwrap();
        lines
    }

    #[test]
    fn check_points_at_setup_when_there_is_no_config() {
        let dir = tempfile::tempdir().unwrap();
        let (config_path, manifest_path) = installed_paths(dir.path());

        let lines = run_check(&config_path, &manifest_path, true, Ok(String::new()));

        assert_eq!(
            lines,
            vec![
                format!("Config not found at {}", config_path.display()),
                "  Run: sudo btrdasd setup".to_string(),
            ]
        );
    }

    #[test]
    fn check_reports_a_healthy_install_line_by_line() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = valid_config(dir.path());
        config.email.enabled = true;
        assert_eq!(config.validate(), Vec::<String>::new());
        let (config_path, manifest_path) = install_config_into(&config, dir.path());
        let total = manifest_lines(&manifest_path).len();

        let lines = run_check(&config_path, &manifest_path, true, Ok(String::new()));

        assert_eq!(
            lines,
            vec![
                format!("Config found: {}", config_path.display()),
                "Config is valid".to_string(),
                "Mail relay reachable at 127.0.0.1:25".to_string(),
                format!("All {total} generated files present"),
                "Target tgt has no mount_uuid — found by drive serial only, and verified only \
                 as a mount point, not as its filesystem"
                    .to_string(),
                "  Its drive is not attached (or holds no BTRFS filesystem) — attach it and run \
                 setup --check again to read its UUID"
                    .to_string(),
                "Target tgt not attached — udisks visibility could not be checked".to_string(),
                "btrbk (/usr/bin/btrbk)".to_string(),
                "smartctl (required, not found)".to_string(),
                "mbuffer (optional, not found)".to_string(),
                "mailx (/usr/bin/mailx)".to_string(),
            ]
        );
    }

    #[test]
    fn check_reports_config_errors_a_missing_manifest_and_an_unread_udev_database() {
        let dir = tempfile::tempdir().unwrap();
        let (config_path, manifest_path) = installed_paths(dir.path());
        let mut config = Config::default();
        config.email.enabled = false;
        config.save(&config_path).unwrap();
        let errors = config.validate();
        assert!(!errors.is_empty(), "a config with no source or target");

        let lines = run_check(
            &config_path,
            &manifest_path,
            false,
            Err("udevadm info --export-db exited with 1".to_string()),
        );

        let mut expected = vec![format!("Config found: {}", config_path.display())];
        expected.extend(errors.iter().map(|e| format!("Config error: {e}")));
        // Email is off: the relay is not reported on at all, and the
        // email-only dependency is not asked for.
        expected.extend([
            "No manifest found. Files may be from a manual install.".to_string(),
            "udisks visibility NOT checked: udevadm info --export-db exited with 1".to_string(),
            "btrbk (/usr/bin/btrbk)".to_string(),
            "smartctl (required, not found)".to_string(),
            "mbuffer (optional, not found)".to_string(),
        ]);
        assert_eq!(lines, expected);
    }

    #[test]
    fn check_explains_an_unreachable_relay_and_singles_out_the_bridge_port() {
        let dir = tempfile::tempdir().unwrap();
        let (config_path, manifest_path) = installed_paths(dir.path());
        let relay_lines = |port: u16| -> Vec<String> {
            relay_config(port).save(&config_path).unwrap();
            run_check(&config_path, &manifest_path, false, Ok(String::new()))
                .into_iter()
                .skip_while(|l| !l.starts_with("Mail relay"))
                .take_while(|l| !l.starts_with("No manifest"))
                .collect()
        };

        // 1025 is Protonmail Bridge: the config predates the relay, and
        // `--upgrade` migrates it.
        assert_eq!(
            relay_lines(1025),
            vec![
                "Mail relay UNREACHABLE at 127.0.0.1:1025",
                "  Port 1025 is Protonmail Bridge, which this version no longer uses.",
                "  Fix with: sudo btrdasd setup --upgrade",
            ]
        );
        // Any other port: an upgrade would change nothing, so do not offer it.
        assert_eq!(
            relay_lines(25),
            vec![
                "Mail relay UNREACHABLE at 127.0.0.1:25",
                "  Reports will be saved to disk but not delivered.",
            ]
        );
    }

    #[test]
    fn check_names_each_generated_file_that_has_gone_missing() {
        let dir = tempfile::tempdir().unwrap();
        let (config_path, manifest_path) =
            install_config_into(&valid_config(dir.path()), dir.path());
        let installed = manifest_lines(&manifest_path);
        let total = installed.len();
        let gone = dir.path().join("etc/btrbk/btrbk.conf");
        assert!(installed.contains(&gone.to_string_lossy().into_owned()));
        std::fs::remove_file(&gone).unwrap();

        let lines = run_check(&config_path, &manifest_path, true, Ok(String::new()));

        let at = lines
            .iter()
            .position(|l| l.contains("generated files"))
            .expect("the manifest is always reported on");
        assert_eq!(
            lines[at..at + 3],
            [
                format!("1 of {total} generated files missing:"),
                format!("    {}", gone.display()),
                "  Fix with: sudo btrdasd setup --upgrade".to_string(),
            ]
        );
    }

    #[test]
    fn cmake_installed_paths_follow_the_install_prefix() {
        let paths = cmake_installed_paths("/opt/das");
        for expected in [
            "/opt/das/bin/btrdasd",
            "/opt/das/bin/btrdasd-gui",
            "/opt/das/libexec/btrdasd-helper",
            "/opt/das/lib/das-backup/backup-run.sh",
            "/opt/das/share/man/man1/btrdasd.1",
            // Legacy FFI artifacts from 0.7.22.1 and earlier: no longer
            // installed, still cleaned up (bd DAS-Backup-Manager-5xo).
            "/opt/das/lib/libbuttered_dasd_ffi.so",
            "/opt/das/include/btrdasd_ffi.h",
        ] {
            assert!(paths.iter().any(|p| p == expected), "missing {expected}");
        }
        // Each entry is handed to remove_file, so each must be a full path.
        for p in &paths {
            assert!(p.starts_with('/') && p.len() > 1, "not absolute: {p:?}");
        }
        assert!(
            cmake_installed_paths("/usr")
                .iter()
                .any(|p| p == "/usr/bin/btrdasd")
        );
    }

    const UNINSTALL_ALL_UNIT_CALLS: [&str; 7] = [
        "disable --now das-backup.timer",
        "disable --now das-backup-full.timer",
        "disable --now das-scrub.timer",
        "disable --now das-backup-doctor.timer",
        "daemon-reload",
        "disable --now btrdasd-helper.service",
        "daemon-reload",
    ];

    /// A fake cmake-installed tree under `root` for `prefix`: the CLI binary
    /// and the script directory with a file the list does not name.
    fn cmake_tree(root: &Path, prefix: &str) -> (PathBuf, PathBuf) {
        let bin = under_root(root, &format!("{prefix}/bin/btrdasd"));
        let libdir = under_root(root, &format!("{prefix}/lib/das-backup"));
        std::fs::create_dir_all(bin.parent().unwrap()).unwrap();
        std::fs::create_dir_all(&libdir).unwrap();
        std::fs::write(&bin, "bin").unwrap();
        std::fs::write(libdir.join("backup-run.sh"), "#!/bin/bash\n").unwrap();
        std::fs::write(libdir.join("left-by-an-older-release.sh"), "#!/bin/bash\n").unwrap();
        (bin, libdir)
    }

    #[test]
    fn under_root_maps_an_absolute_path_into_the_root() {
        assert_eq!(
            under_root(Path::new("/"), "/usr/bin/btrdasd"),
            PathBuf::from("/usr/bin/btrdasd")
        );
        assert_eq!(
            under_root(Path::new("/tmp/pkg"), "/usr/bin/btrdasd"),
            PathBuf::from("/tmp/pkg/usr/bin/btrdasd")
        );
    }

    #[test]
    fn uninstall_all_removes_the_cmake_tree_under_the_configured_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let (config_path, manifest_path) = installed_paths(root);
        let mut config = valid_config(root);
        config.general.install_prefix = "/opt/das".to_string();
        config.save(&config_path).unwrap();
        let generated = root.join("etc/btrbk/btrbk.conf");
        std::fs::create_dir_all(generated.parent().unwrap()).unwrap();
        std::fs::write(&generated, "generated").unwrap();
        std::fs::write(&manifest_path, generated.to_string_lossy().as_bytes()).unwrap();

        let (bin, libdir) = cmake_tree(root, "/opt/das");
        let (other_prefix_bin, _) = cmake_tree(root, "/usr");
        let state_dir = root.join("var/lib/das-backup");
        std::fs::create_dir_all(&state_dir).unwrap();
        let systemctl = FakeSystemctl::new(&[]);

        uninstall_all_with(root, &config_path, &manifest_path, false, &|args| {
            systemctl.run(args)
        })
        .unwrap();

        // The helper service is stopped as well as the timers.
        assert_eq!(systemctl.calls(), UNINSTALL_ALL_UNIT_CALLS);
        assert!(!generated.exists(), "phase 1 removes the manifest's files");
        assert!(!bin.exists());
        assert!(
            !libdir.exists(),
            "the script directory goes with its contents"
        );
        assert!(!state_dir.exists(), "an empty state directory is removed");
        assert!(
            other_prefix_bin.exists(),
            "only the configured prefix is this install's to remove"
        );
    }

    #[test]
    fn uninstall_all_falls_back_to_usr_without_a_config_and_keeps_the_database() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let (config_path, manifest_path) = installed_paths(root);
        std::fs::create_dir_all(manifest_path.parent().unwrap()).unwrap();
        std::fs::write(&manifest_path, "").unwrap();
        let (bin, libdir) = cmake_tree(root, "/usr");
        let db = root.join("var/lib/das-backup/backup-index.db");
        std::fs::create_dir_all(db.parent().unwrap()).unwrap();
        std::fs::write(&db, "index").unwrap();
        // A helper service that will not stop is a warning, not a reason to
        // leave the files installed.
        let systemctl = FakeSystemctl::new(&["btrdasd-helper.service"]);

        uninstall_all_with(root, &config_path, &manifest_path, true, &|args| {
            systemctl.run(args)
        })
        .unwrap();

        assert_eq!(systemctl.calls(), UNINSTALL_ALL_UNIT_CALLS);
        assert!(!bin.exists());
        assert!(!libdir.exists());
        // With no config the database location is unknown, so `--remove-db`
        // removes nothing, and its directory is left because it is not empty.
        assert!(db.exists());
    }

    /// Every file `cmake --install` writes for prefix `/usr`, read off the
    /// `install()` rules in CMakeLists.txt and gui/CMakeLists.txt, plus the
    /// two FFI artifacts older releases installed.
    const CMAKE_PATHS_USR: [&str; 26] = [
        "/usr/bin/btrdasd",
        "/usr/bin/btrdasd-gui",
        "/usr/libexec/btrdasd-helper",
        "/usr/lib/libbuttered_dasd_ffi.so",
        "/usr/include/btrdasd_ffi.h",
        "/usr/share/dbus-1/system.d/org.dasbackup.Helper1.conf",
        "/usr/share/dbus-1/system-services/org.dasbackup.Helper1.service",
        "/usr/share/polkit-1/actions/org.dasbackup.policy",
        "/usr/share/man/man1/btrdasd.1",
        "/usr/share/bash-completion/completions/btrdasd",
        "/usr/share/zsh/site-functions/_btrdasd",
        "/usr/share/fish/vendor_completions.d/btrdasd.fish",
        "/usr/share/applications/org.theboscoclub.btrdasd-gui.desktop",
        "/usr/share/icons/hicolor/scalable/apps/btrdasd-gui.svg",
        "/usr/share/kxmlgui5/btrdasd-gui/btrdasd-gui.rc",
        "/usr/lib/das-backup/backup-run.sh",
        "/usr/lib/das-backup/backup-verify.sh",
        "/usr/lib/das-backup/boot-archive-cleanup.sh",
        "/usr/lib/das-backup/das-partition-drives.sh",
        "/usr/lib/das-backup/install-backup-timer.sh",
        "/usr/lib/das-backup/config/btrbk.conf",
        "/usr/lib/systemd/system/das-backup.service",
        "/usr/lib/systemd/system/das-backup-full.service",
        "/usr/lib/systemd/system/das-backup.timer",
        "/usr/lib/systemd/system/das-backup-full.timer",
        "/usr/lib/systemd/system/btrdasd-helper.service",
    ];

    #[test]
    fn cmake_installed_paths_are_exactly_what_cmake_installs_for_the_prefix() {
        assert_eq!(cmake_installed_paths("/usr"), CMAKE_PATHS_USR);

        // CMakeLists.txt installs the units with a RELATIVE destination,
        // `lib/systemd/system`, so they move with the prefix like everything
        // else. A list that names them at a fixed `/lib/systemd/system` both
        // misses the real files and reaches for ones this install never wrote.
        let local: Vec<String> = CMAKE_PATHS_USR
            .iter()
            .map(|p| p.replacen("/usr/", "/usr/local/", 1))
            .collect();
        assert_eq!(cmake_installed_paths("/usr/local"), local);
        for p in cmake_installed_paths("/usr/local") {
            assert!(p.starts_with("/usr/local/"), "outside the prefix: {p}");
        }
    }

    /// Every file under `dir`, for asserting a tree was left alone.
    fn files_under(dir: &Path) -> Vec<PathBuf> {
        let mut out: Vec<PathBuf> = walkdir::WalkDir::new(dir)
            .into_iter()
            .map(|e| e.unwrap().path().to_path_buf())
            .collect();
        out.sort();
        out
    }

    #[test]
    fn uninstall_all_after_a_normal_install_uses_the_configured_prefix() {
        // The manifest of a normal install lists config.toml, so phase 1
        // deletes it. The prefix has to be read before that: read afterwards,
        // every full uninstall fell back to /usr, left a /usr/local install in
        // place, and removed whatever sat at the same paths under /usr.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let mut config = valid_config(root);
        config.general.install_prefix = "/usr/local".to_string();
        let (config_path, manifest_path) = install_config_into(&config, root);
        assert!(
            manifest_lines(&manifest_path).contains(&config_path.to_string_lossy().into_owned()),
            "fixture: a normal install's manifest lists the config"
        );

        let (bin, libdir) = cmake_tree(root, "/usr/local");
        let unit = root.join("usr/local/lib/systemd/system/das-backup.service");
        std::fs::create_dir_all(unit.parent().unwrap()).unwrap();
        std::fs::write(&unit, "[Unit]\n").unwrap();

        // The same paths under /usr, and the unit at the fixed location the
        // list used to name: none of it is this install's.
        cmake_tree(root, "/usr");
        for decoy in [
            "usr/lib/systemd/system/das-backup.service",
            "lib/systemd/system/das-backup.service",
        ] {
            let decoy = root.join(decoy);
            std::fs::create_dir_all(decoy.parent().unwrap()).unwrap();
            std::fs::write(&decoy, "[Unit]\n").unwrap();
        }
        let outside = |root: &Path| -> Vec<PathBuf> {
            let mut all = files_under(&root.join("lib"));
            all.extend(
                files_under(&root.join("usr"))
                    .into_iter()
                    .filter(|p| !p.starts_with(root.join("usr/local"))),
            );
            all
        };
        let before = outside(root);
        assert!(before.iter().any(|p| p.ends_with("usr/bin/btrdasd")));
        let systemctl = FakeSystemctl::new(&[]);

        uninstall_all_with(root, &config_path, &manifest_path, false, &|args| {
            systemctl.run(args)
        })
        .unwrap();

        assert!(!config_path.exists(), "fixture: phase 1 removed the config");
        assert!(!bin.exists(), "the /usr/local binary was left behind");
        assert!(!libdir.exists(), "the /usr/local script directory was left");
        assert!(!unit.exists(), "the /usr/local unit was left behind");
        assert_eq!(outside(root), before, "files outside the prefix changed");
    }

    // -----------------------------------------------------------------
    // bd DAS-Backup-Manager-6wt fix round 4 — every file setup writes is
    // replaced whole (a new file renamed into place), keeps its mode, and a
    // write that fails names the file and leaves the old one
    // -----------------------------------------------------------------

    fn inode_of(path: &Path) -> u64 {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata(path).unwrap().ino()
    }

    fn mode_of(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o7777
    }

    /// Where an install under `base` puts the script `name`: beneath the
    /// configured prefix.
    fn installed_script(base: &Path, config: &Config, name: &str) -> PathBuf {
        under_root(
            base,
            &format!("{}/lib/das-backup/{name}", config.general.install_prefix),
        )
    }

    #[test]
    fn install_replaces_each_file_whole_and_a_reader_of_the_old_one_reads_it_to_the_end() {
        use std::io::Read;
        let dir = tempfile::tempdir().unwrap();
        let config = valid_config(dir.path());
        let (config_path, manifest_path) = install_config_into(&config, dir.path());
        let script = installed_script(dir.path(), &config, "backup-run.sh");
        // An older script — dated long ago, so the 2lj guard lets it go — and
        // larger than any read buffer, so a reader is part-way in, as a
        // running bash is.
        let old: Vec<u8> = (0..20_000u32)
            .flat_map(|i| format!("# old backup-run.sh line {i}\n").into_bytes())
            .collect();
        std::fs::write(&script, &old).unwrap();
        date_back(dir.path());
        let before = inode_of(&script);
        let mut reader = std::fs::File::open(&script).unwrap();
        let mut first = vec![0u8; 4096];
        reader.read_exact(&mut first).unwrap();

        install_to_prefix(&config, dir.path(), &config_path, &manifest_path).unwrap();

        let mut rest = Vec::new();
        reader.read_to_end(&mut rest).unwrap();
        assert!(
            [first, rest].concat() == old,
            "the script a run has open must stay the old one, whole"
        );
        assert_ne!(inode_of(&script), before, "a new file, renamed into place");
        assert!(
            std::fs::read_to_string(&script)
                .unwrap()
                .starts_with("#!/bin/bash"),
            "a new open reads the new script"
        );
    }

    #[test]
    fn install_keeps_each_files_mode_and_makes_the_scripts_executable() {
        let dir = tempfile::tempdir().unwrap();
        let config = valid_config(dir.path());
        let (config_path, manifest_path) = install_config_into(&config, dir.path());
        let btrbk = dir.path().join("etc/btrbk/btrbk.conf");
        let unit = dir.path().join("etc/systemd/system/das-backup.service");
        let script = installed_script(dir.path(), &config, "boot-archive-cleanup.sh");
        // The operator made btrbk.conf private and the unit group-writable; a
        // script lost its execute bits.
        std::fs::set_permissions(&btrbk, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::set_permissions(&unit, std::fs::Permissions::from_mode(0o664)).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o644)).unwrap();
        date_back(dir.path());
        let (b, u) = (inode_of(&btrbk), inode_of(&unit));

        install_to_prefix(&config, dir.path(), &config_path, &manifest_path).unwrap();

        assert!(
            inode_of(&btrbk) != b && inode_of(&unit) != u,
            "both replaced"
        );
        assert_eq!(mode_of(&btrbk), 0o600, "btrbk.conf keeps its mode");
        assert_eq!(mode_of(&unit), 0o664, "the unit keeps its mode");
        assert_eq!(mode_of(&script), 0o755, "a script is always executable");
    }

    #[test]
    fn install_over_private_files_leaves_every_one_private() {
        // The reviewer's probe: config.toml, btrbk.conf and a unit, made
        // 0600, then installed over. The manifest too.
        let dir = tempfile::tempdir().unwrap();
        let config = valid_config(dir.path());
        let (config_path, manifest_path) = install_config_into(&config, dir.path());
        let btrbk = dir.path().join("etc/btrbk/btrbk.conf");
        let unit = dir.path().join("etc/systemd/system/das-backup.service");
        let private = [&config_path, &btrbk, &unit, &manifest_path];
        for path in private {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        date_back(dir.path());
        let before: Vec<u64> = private.iter().map(|p| inode_of(p)).collect();

        install_to_prefix(&config, dir.path(), &config_path, &manifest_path).unwrap();

        for (path, inode) in private.iter().zip(before) {
            assert_ne!(inode_of(path), inode, "{} replaced", path.display());
            assert_eq!(mode_of(path), 0o600, "{} stays private", path.display());
        }
    }

    /// Make the file at `path` impossible to replace — for root too — while
    /// it still reads: it moves to the longest name a file can have, which
    /// leaves no room for a temp file's name beside it, and a link at `path`
    /// points there. A copy of the library's `fsutil::testing::unreplaceable`,
    /// which this crate's tests cannot see.
    fn unreplaceable(path: &Path) {
        let real = path.with_file_name("x".repeat(255));
        std::fs::rename(path, &real).unwrap();
        std::os::unix::fs::symlink(&real, path).unwrap();
    }

    #[test]
    fn an_install_that_cannot_write_a_file_names_it_once_and_leaves_the_old_one() {
        let dir = tempfile::tempdir().unwrap();
        let config = valid_config(dir.path());
        let (config_path, manifest_path) = install_config_into(&config, dir.path());
        let btrbk = dir.path().join("etc/btrbk/btrbk.conf");
        std::fs::write(&btrbk, "# the old btrbk.conf\n").unwrap();
        unreplaceable(&btrbk);

        let err = install_to_prefix(&config, dir.path(), &config_path, &manifest_path)
            .unwrap_err()
            .to_string();

        assert!(
            err.starts_with(&format!("cannot write {}: ", btrbk.display())),
            "{err}"
        );
        assert_eq!(
            err.matches(&btrbk.display().to_string()).count(),
            1,
            "{err}"
        );
        assert_eq!(
            std::fs::read_to_string(&btrbk).unwrap(),
            "# the old btrbk.conf\n"
        );
    }
    // -----------------------------------------------------------------
    // bd DAS-Backup-Manager-6wt fix round 4 — `btrdasd setup` end to end
    // through `dispatch`: every writing mode refuses with 75 and writes
    // nothing while either lock is held, asks its questions before it takes
    // them, says a refusal on stderr; `--check` takes no lock at all
    // -----------------------------------------------------------------

    use crate::setup::{SetupArgs, SetupHost, dispatch};

    fn setup_args(flags: &[&str]) -> SetupArgs {
        let mut a = SetupArgs {
            modify: false,
            upgrade: false,
            uninstall: false,
            uninstall_all: false,
            check: false,
            force: false,
        };
        for flag in flags {
            match *flag {
                "modify" => a.modify = true,
                "upgrade" => a.upgrade = true,
                "uninstall" => a.uninstall = true,
                "uninstall_all" => a.uninstall_all = true,
                "check" => a.check = true,
                "force" => a.force = true,
                other => panic!("unknown flag {other}"),
            }
        }
        a
    }

    /// What one `dispatch` run on an installed tree did.
    struct Dispatched {
        outcome: Result<SetupOutcome, String>,
        /// Each step, with the locks as a job asking for them then would find
        /// them.
        steps: Vec<String>,
        /// Lines on stdout, and on stderr.
        said: Vec<String>,
        warned: Vec<String>,
        /// Every file the run changed: since it started, and since the wizard
        /// returned.
        changed: Vec<PathBuf>,
        changed_after_wizard: Vec<PathBuf>,
        locks_after: String,
        record_after: String,
        config_path: PathBuf,
        /// `config.toml` once it returned, if there was one.
        config_after: Option<Config>,
    }

    /// Run `btrdasd setup <flags>` through `dispatch` on the tree an install
    /// left — a database beside it, every file dated long ago — while
    /// `holding` hold their locks, the wizard returning the config it was
    /// given (or a fresh one), the operator answering "yes" to removing the
    /// database. With `edit_during_wizard`, something else rewrites
    /// `config.toml` while the wizard is open.
    fn dispatch_on_installed_tree(
        flags: &[&str],
        holding: &[Holding],
        edit_during_wizard: bool,
    ) -> Dispatched {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();
        let config = valid_config(base);
        let (config_path, manifest_path) = install_config_into(&config, base);
        std::fs::write(&config.general.db_path, "index").unwrap();
        date_back(base);
        let before = tree(base);
        let after_wizard = std::cell::RefCell::new(None);

        let locks = ScratchLocks::new();
        let _backup = holding
            .contains(&Holding::Backup)
            .then(|| locks.hold_backup());
        let _scrub = holding
            .contains(&Holding::Maintenance)
            .then(|| locks.hold_maintenance());

        let steps = std::cell::RefCell::new(Vec::new());
        let step = |name: &str| {
            steps
                .borrow_mut()
                .push(format!("{name} ({})", locks.state()))
        };
        let systemctl = FakeSystemctl::new(&[]);
        let check = || -> Result<(), Box<dyn std::error::Error>> {
            step("check");
            Ok(())
        };
        let install = |config: &Config, _: &SetupLocks| -> Result<(), Box<dyn std::error::Error>> {
            step("install");
            install_to_prefix(config, base, &config_path, &manifest_path)
        };
        let uninstall =
            |remove_db: bool, _: &SetupLocks| -> Result<(), Box<dyn std::error::Error>> {
                step(&format!("uninstall remove_db={remove_db}"));
                uninstall_with(&config_path, &manifest_path, remove_db, &|a| {
                    systemctl.run(a)
                })
            };
        let uninstall_all =
            |remove_db: bool, _: &SetupLocks| -> Result<(), Box<dyn std::error::Error>> {
                step(&format!("uninstall-all remove_db={remove_db}"));
                uninstall_all_with(base, &config_path, &manifest_path, remove_db, &|a| {
                    systemctl.run(a)
                })
            };
        let active_state = || Ok("active".to_string());
        let restart = |args: &[&str]| {
            step(&format!("systemctl {}", args.join(" ")));
            Ok(())
        };
        let upgrade = |site: &SetupLockSite,
                       config_path: &Path,
                       say: &mut dyn FnMut(String),
                       warn: &mut dyn FnMut(String)| {
            upgrade_with(
                site,
                config_path,
                &|_| true,
                &install,
                &HelperHost {
                    active_state: &active_state,
                    systemctl: &restart,
                },
                say,
                warn,
            )
        };
        let wizard = |existing: Option<Config>| -> Result<Config, Box<dyn std::error::Error>> {
            step("wizard");
            if edit_during_wizard {
                // Another writer — the GUI's ConfigSet, a subvol sync — while
                // the wizard is open.
                let mut theirs = Config::load(&config_path)?;
                theirs.schedule.incremental = "04:15".to_string();
                theirs.save(&config_path)?;
            }
            *after_wizard.borrow_mut() = Some(tree(base));
            Ok(existing.unwrap_or_else(|| valid_config(base)))
        };
        let ask_remove_db = || -> Result<bool, Box<dyn std::error::Error>> {
            step("ask: also remove the database?");
            Ok(true)
        };
        let host = SetupHost {
            site: SetupLockSite {
                backup: locks.site.backup.clone(),
                maintenance: locks.site.maintenance.clone(),
            },
            check: &check,
            install: &install,
            uninstall: &uninstall,
            uninstall_all: &uninstall_all,
            upgrade: &upgrade,
            wizard: &wizard,
            ask_remove_db: &ask_remove_db,
        };

        let mut said = Vec::new();
        let mut warned = Vec::new();
        let outcome = dispatch(
            &setup_args(flags),
            &config_path,
            &host,
            &mut |line| said.push(line),
            &mut |line| warned.push(line),
        )
        .map_err(|e| e.to_string());

        let now = tree(base);
        let after_wizard = after_wizard.into_inner().unwrap_or_else(|| before.clone());
        Dispatched {
            outcome,
            steps: steps.into_inner(),
            said,
            warned,
            changed: changed(&before, &now),
            changed_after_wizard: changed(&after_wizard, &now),
            locks_after: locks.state(),
            record_after: locks.record(),
            config_after: Config::load(&config_path).ok(),
            config_path,
        }
    }

    /// Every way to ask `btrdasd setup` to write or remove installed files.
    const WRITING_MODES: [&[&str]; 9] = [
        &[],
        &["modify"],
        &["force"],
        &["upgrade"],
        &["upgrade", "force"],
        &["uninstall"],
        &["uninstall", "force"],
        &["uninstall_all"],
        &["uninstall_all", "force"],
    ];

    /// The questions `flags` asks before it takes any lock.
    fn questions(flags: &[&str]) -> Vec<&'static str> {
        let forced = flags.contains(&"force");
        if flags.contains(&"upgrade") || flags == ["force"] {
            vec![]
        } else if flags.contains(&"uninstall") || flags.contains(&"uninstall_all") {
            if forced {
                vec![]
            } else {
                vec!["ask: also remove the database?"]
            }
        } else {
            vec!["wizard"]
        }
    }

    #[test]
    fn every_writing_mode_refuses_with_75_and_writes_nothing_while_either_lock_is_held() {
        let pid = std::process::id();
        for (holding, state) in [
            (Holding::Backup, "backup held, maintenance free"),
            (Holding::Maintenance, "backup free, maintenance held"),
        ] {
            for flags in WRITING_MODES {
                let case = format!("{flags:?} while {holding:?} holds its lock");
                let run = dispatch_on_installed_tree(flags, &[holding], false);

                assert_eq!(run.outcome, Ok(SetupOutcome::Refused), "{case}");
                assert_eq!(exit_status(SetupOutcome::Refused), 75);
                assert_eq!(run.changed, Vec::<PathBuf>::new(), "{case}");
                // The questions came first, with setup holding nothing; then
                // nothing else ran.
                let asked: Vec<String> = questions(flags)
                    .iter()
                    .map(|q| format!("{q} ({state})"))
                    .collect();
                assert_eq!(run.steps, asked, "{case}");
                assert_eq!(run.warned.len(), 1, "{case}: {:?}", run.warned);
                assert!(
                    run.warned[0].starts_with("Refused, nothing written or removed: "),
                    "{case}: {:?}",
                    run.warned
                );
                assert!(
                    !run.said.iter().any(|l| l.contains("Refused")),
                    "{case}: a refusal goes to stderr, not stdout: {:?}",
                    run.said
                );
                assert_eq!(run.locks_after, state, "{case}: setup holds nothing after");
                if holding == Holding::Maintenance {
                    assert_eq!(
                        run.record_after,
                        format!("btrdasd scrub run pid {pid}\n"),
                        "{case}: the holder's record is untouched"
                    );
                }
            }
        }
    }

    #[test]
    fn every_writing_mode_asks_first_then_writes_under_both_locks_and_lets_them_go() {
        for flags in WRITING_MODES {
            let case = format!("{flags:?}");
            let run = dispatch_on_installed_tree(flags, &[], false);

            assert_eq!(run.outcome, Ok(SetupOutcome::Done), "{case}");
            assert_eq!(exit_status(SetupOutcome::Done), 0);
            let mut expected: Vec<String> = questions(flags)
                .iter()
                .map(|q| format!("{q} ({BOTH_FREE})"))
                .collect();
            let forced = flags.contains(&"force");
            expected.extend(
                match flags.first().copied() {
                    Some("upgrade") => vec![
                        "install".to_string(),
                        "systemctl try-restart btrdasd-helper.service".to_string(),
                    ],
                    Some("uninstall") => vec![format!("uninstall remove_db={}", !forced)],
                    Some("uninstall_all") => vec![format!("uninstall-all remove_db={}", !forced)],
                    _ => vec!["install".to_string()],
                }
                .into_iter()
                .map(|s| format!("{s} ({BOTH_HELD})")),
            );
            assert_eq!(run.steps, expected, "{case}");
            assert!(!run.changed.is_empty(), "{case}: nothing written");
            assert_eq!(run.warned, Vec::<String>::new(), "{case}");
            assert_eq!(run.locks_after, BOTH_FREE, "{case}");
            assert_eq!(run.record_after, "", "{case}");
        }
    }

    #[test]
    fn check_takes_no_lock_and_runs_while_both_are_held() {
        let pid = std::process::id();
        for flags in [&["check"][..], &["check", "upgrade", "force"]] {
            // Free: it runs with both still free — setup took neither.
            let run = dispatch_on_installed_tree(flags, &[], false);
            assert_eq!(run.outcome, Ok(SetupOutcome::Done), "{flags:?}");
            assert_eq!(run.steps, vec![format!("check ({BOTH_FREE})")], "{flags:?}");

            // Both held by others: it still runs, and changes nothing.
            let run =
                dispatch_on_installed_tree(flags, &[Holding::Backup, Holding::Maintenance], false);
            assert_eq!(run.outcome, Ok(SetupOutcome::Done), "{flags:?}");
            assert_eq!(exit_status(SetupOutcome::Done), 0);
            assert_eq!(run.steps, vec![format!("check ({BOTH_HELD})")], "{flags:?}");
            assert_eq!(run.warned, Vec::<String>::new(), "{flags:?}");
            assert_eq!(run.changed, Vec::<PathBuf>::new(), "{flags:?}");
            assert_eq!(
                run.record_after,
                format!("btrdasd scrub run pid {pid}\n"),
                "{flags:?}"
            );
        }
    }

    #[test]
    fn modify_writes_nothing_when_config_toml_changed_while_the_wizard_was_open() {
        let run = dispatch_on_installed_tree(&["modify"], &[], true);

        assert_eq!(run.outcome, Ok(SetupOutcome::Refused));
        assert_eq!(exit_status(SetupOutcome::Refused), 75);
        assert_eq!(
            run.steps,
            vec![format!("wizard ({BOTH_FREE})")],
            "nothing installed over the other writer's change"
        );
        assert_eq!(
            run.changed_after_wizard,
            Vec::<PathBuf>::new(),
            "setup wrote nothing after the wizard"
        );
        assert_eq!(
            run.warned,
            vec![format!(
                "Refused, nothing written or removed: {} changed while the wizard was open — \
                 something else wrote it after --modify read it. Run setup --modify again to \
                 start from it as it is now (exit 75).",
                run.config_path.display()
            )]
        );
        assert_eq!(run.locks_after, BOTH_FREE, "taken to compare, then let go");
        assert_eq!(
            run.config_after.expect("config.toml").schedule.incremental,
            "04:15",
            "the other writer's change stands"
        );
    }
}
