// Installer module — install, uninstall, upgrade, and check modes.
// Orchestrates config saving, template generation, file writing, and manifest tracking.

#![allow(dead_code)]

use std::net::{TcpStream, ToSocketAddrs};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::setup::config::Config;
use crate::setup::templates::GeneratedFiles;

const CONFIG_DIR: &str = "/etc/das-backup";
const CONFIG_FILE: &str = "/etc/das-backup/config.toml";
const MANIFEST_FILE: &str = "/etc/das-backup/.manifest";

/// Install using system defaults (/etc, /).
/// Run one `systemctl` verb, returning a description of the failure instead of
/// discarding it.
///
/// Every call site used to be `let _ = Command::new("systemctl")...status()`,
/// so `install()` returned `Ok(())` whether or not a single timer had been
/// enabled. Installing the schedule is the entire purpose of the command: a
/// masked unit, a malformed generated unit, or systemctl being unavailable
/// produced a clean "install complete" and **no scheduled backup, no scheduled
/// scrub, and no drift check ever running**, with nothing to surface it until
/// someone noticed the absence of the 03:00 report.
/// bd DAS-Backup-Manager-nsp (finding #5).
fn run_systemctl(args: &[&str]) -> Result<(), String> {
    match std::process::Command::new("systemctl").args(args).status() {
        Ok(st) if st.success() => Ok(()),
        Ok(st) => Err(format!("systemctl {} exited with {}", args.join(" "), st)),
        Err(e) => Err(format!(
            "systemctl {} could not be run: {e}",
            args.join(" ")
        )),
    }
}

pub fn install(config: &Config) -> Result<(), Box<dyn std::error::Error>> {
    let config_path = PathBuf::from(CONFIG_FILE);
    let manifest_path = PathBuf::from(MANIFEST_FILE);
    let root = PathBuf::from("/");
    install_to_prefix(config, &root, &config_path, &manifest_path)?;

    // Enable systemd timers (only in real installs, not in install_to_prefix tests)
    if config.init.system == crate::setup::config::InitSystem::Systemd {
        // Collected rather than discarded — see run_systemctl. A schedule that
        // was not installed must not be reported as an install that succeeded.
        let mut unit_errors: Vec<String> = Vec::new();
        if let Err(e) = run_systemctl(&["daemon-reload"]) {
            unit_errors.push(e);
        }
        if let Err(e) = run_systemctl(&["enable", "--now", "das-backup.timer"]) {
            unit_errors.push(e);
        }
        if let Err(e) = run_systemctl(&["enable", "--now", "das-backup-full.timer"]) {
            unit_errors.push(e);
        }

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
        if config.scrub.enabled {
            if let Err(e) = run_systemctl(&["enable", "--now", "das-scrub.timer"]) {
                unit_errors.push(e);
            }
        } else {
            if let Err(e) = run_systemctl(&["disable", "--now", "das-scrub.timer"]) {
                unit_errors.push(e);
            }
        }

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
        if let Err(e) = run_systemctl(&["enable", "--now", "das-backup-doctor.timer"]) {
            unit_errors.push(e);
        }

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
    }

    Ok(())
}

/// Install with a custom root prefix (for testing and packaging).
pub fn install_to_prefix(
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

        std::fs::write(&full_path, content)?;

        // Make scripts executable
        if full_path.extension().and_then(|e| e.to_str()) == Some("sh") {
            let mut perms = std::fs::metadata(&full_path)?.permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&full_path, perms)?;
        }

        manifest_entries.push(full_path.to_string_lossy().to_string());
    }

    // Write manifest
    if let Some(parent) = manifest_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(manifest_path, manifest_entries.join("\n"))?;

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

/// Uninstall using system defaults.
pub fn uninstall(remove_db: bool) -> Result<(), Box<dyn std::error::Error>> {
    let manifest_path = PathBuf::from(MANIFEST_FILE);
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
    let db_path = match Config::load(&PathBuf::from(CONFIG_FILE)) {
        Ok(c) => Some(c.general.db_path),
        Err(e) => {
            eprintln!(
                "Warning: could not read {CONFIG_FILE} ({e}) — the database location is \
                 unknown and it will NOT be removed"
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
        if let Err(e) = run_systemctl(&["disable", "--now", unit]) {
            eprintln!("Warning: {unit} may still be enabled: {e}");
        }
    }

    let (removed, problems) = uninstall_from_manifest(&manifest_path);
    println!("Removed {} files.", removed);
    for p in &problems {
        eprintln!("Warning: {p}");
    }

    if let Err(e) = std::fs::remove_file(&manifest_path) {
        eprintln!(
            "Warning: could not remove manifest {}: {e}",
            manifest_path.display()
        );
    }
    // Bare `remove_dir`: deliberately best-effort, because "the directory still
    // has operator files in it" is the normal outcome, not a fault.
    let _ = std::fs::remove_dir(CONFIG_DIR);

    if remove_db
        && let Some(db) = db_path
        && Path::new(&db).exists()
    {
        std::fs::remove_file(&db)?;
        println!("Removed database: {}", db);
    }

    if let Err(e) = run_systemctl(&["daemon-reload"]) {
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
/// that had nothing left to do (bd DAS-Backup-Manager-8wx).
pub fn uninstall_from_manifest(manifest_path: &Path) -> (usize, Vec<String>) {
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

/// Upgrade: reload existing config, apply migrations, and regenerate all files.
pub fn upgrade() -> Result<(), Box<dyn std::error::Error>> {
    let config_path = PathBuf::from(CONFIG_FILE);
    if !config_path.exists() {
        return Err(format!(
            "No config found at {}. Run 'btrdasd setup' first.",
            config_path.display()
        )
        .into());
    }

    let mut config = Config::load(&config_path)?;
    let old_version = config.general.version.clone();
    config.general.version = env!("CARGO_PKG_VERSION").to_string();

    let migrations = migrate_config(&mut config);
    for change in &migrations {
        println!("Migrating config: {change}");
    }

    if old_version != config.general.version || !migrations.is_empty() {
        if old_version != config.general.version {
            println!(
                "Updating config version: {} -> {}",
                old_version, config.general.version
            );
        }
        config.save(&config_path)?;
    }

    // The relay is a hard dependency of email reporting now. Warn rather than
    // fail: a backup whose report cannot be sent is still a completed backup,
    // and the report is always written to disk regardless.
    if config.email.enabled && !relay_reachable(&config) {
        println!(
            "Warning: email is enabled but nothing is listening on {}:{}",
            config.email.smtp_host, config.email.smtp_port
        );
        println!("  Reports will be saved to disk but not delivered.");
        println!("  Check the local mail relay: systemctl status postfix");
    }
    println!("Regenerating files from {}...", config_path.display());
    install(&config)?;
    println!("Upgrade complete.");
    Ok(())
}

/// Check: validate config, verify manifest files, report dependency status.
pub fn check() -> Result<(), Box<dyn std::error::Error>> {
    let config_path = PathBuf::from(CONFIG_FILE);

    if !config_path.exists() {
        println!("Config not found at {}", config_path.display());
        println!("  Run: sudo btrdasd setup");
        return Ok(());
    }
    println!("Config found: {}", config_path.display());

    let config = Config::load(&config_path)?;
    let errors = config.validate();
    if errors.is_empty() {
        println!("Config is valid");
    } else {
        for err in &errors {
            println!("Config error: {}", err);
        }
    }

    // A config that still names the Bridge port is valid but undeliverable —
    // report it here rather than letting the next backup discover it.
    if config.email.enabled {
        let relay = format!("{}:{}", config.email.smtp_host, config.email.smtp_port);
        if relay_reachable(&config) {
            println!("Mail relay reachable at {relay}");
        } else {
            println!("Mail relay UNREACHABLE at {relay}");
            if config.email.smtp_port == 1025 {
                println!("  Port 1025 is Protonmail Bridge, which this version no longer uses.");
                println!("  Fix with: sudo btrdasd setup --upgrade");
            } else {
                println!("  Reports will be saved to disk but not delivered.");
            }
        }
    }

    let manifest_path = PathBuf::from(MANIFEST_FILE);
    if manifest_path.exists() {
        let content = std::fs::read_to_string(&manifest_path)?;
        let total = content.lines().count();
        let missing: Vec<&str> = content
            .lines()
            .filter(|line| !Path::new(line.trim()).exists())
            .collect();
        if missing.is_empty() {
            println!("All {} generated files present", total);
        } else {
            println!("{} of {} generated files missing:", missing.len(), total);
            for m in &missing {
                println!("    {}", m);
            }
            println!("  Fix with: sudo btrdasd setup --upgrade");
        }
    } else {
        println!("No manifest found. Files may be from a manual install.");
    }

    let deps = crate::setup::detect::check_dependencies(config.email.enabled);
    for dep in &deps {
        if let Some(path) = &dep.path {
            println!("{} ({})", dep.name, path);
        } else if dep.required {
            println!("{} (required, not found)", dep.name);
        } else {
            println!("{} (optional, not found)", dep.name);
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
        // Systemd units (cmake-installed templates)
        "/lib/systemd/system/das-backup.service".to_string(),
        "/lib/systemd/system/das-backup-full.service".to_string(),
        "/lib/systemd/system/das-backup.timer".to_string(),
        "/lib/systemd/system/das-backup-full.timer".to_string(),
        "/lib/systemd/system/btrdasd-helper.service".to_string(),
    ]
}

/// Full uninstall: remove generated files (manifest), then cmake-installed files.
pub fn uninstall_all(remove_db: bool) -> Result<(), Box<dyn std::error::Error>> {
    // Phase 1: run the standard uninstall (manifest files, timers, config dir)
    uninstall(remove_db)?;

    // Phase 2: stop the helper service
    if let Err(e) = run_systemctl(&["disable", "--now", "btrdasd-helper.service"]) {
        eprintln!("Warning: btrdasd-helper.service may still be enabled: {e}");
    }

    // Phase 3: determine install prefix from config (default /usr).
    // An unreadable config here does not mean "/usr" — it means we are guessing,
    // and everything installed under a different prefix will silently survive
    // the "full" uninstall (bd DAS-Backup-Manager-8wx).
    let prefix = match Config::load(&PathBuf::from(CONFIG_FILE)) {
        Ok(c) => c.general.install_prefix.clone(),
        Err(e) => {
            eprintln!(
                "Warning: could not read {CONFIG_FILE} ({e}) — assuming the default install \
                 prefix /usr; files installed under any other prefix will be LEFT BEHIND"
            );
            "/usr".to_string()
        }
    };

    let paths = cmake_installed_paths(&prefix);
    let removed = remove_paths(&paths);
    println!("Removed {} cmake-installed files.", removed);

    // Phase 4: clean up directories
    let libdir = format!("{prefix}/lib/das-backup");
    if Path::new(&libdir).exists()
        && let Err(e) = std::fs::remove_dir_all(&libdir)
    {
        eprintln!("Warning: could not remove {libdir}: {e}");
    }
    // Bare `remove_dir`: best-effort, and failing because the database is still
    // there is the normal outcome.
    let _ = std::fs::remove_dir("/var/lib/das-backup");

    if let Err(e) = run_systemctl(&["daemon-reload"]) {
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

    /// Install into a throwaway prefix and hand back the paths used.
    fn install_into(dir: &Path) -> (PathBuf, PathBuf) {
        let config_path = dir.join("etc/das-backup/config.toml");
        let manifest_path = dir.join("etc/das-backup/.manifest");
        std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        let config = Config::default();
        install_to_prefix(&config, dir, &config_path, &manifest_path).unwrap();
        (config_path, manifest_path)
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
}
