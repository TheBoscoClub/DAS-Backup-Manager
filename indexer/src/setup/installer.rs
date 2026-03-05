// Installer module — install, uninstall, upgrade, and check modes.
// Orchestrates config saving, template generation, file writing, and manifest tracking.

#![allow(dead_code)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use crate::setup::config::Config;
use crate::setup::templates::GeneratedFiles;

const CONFIG_DIR: &str = "/etc/das-backup";
const CONFIG_FILE: &str = "/etc/das-backup/config.toml";
const MANIFEST_FILE: &str = "/etc/das-backup/.manifest";

/// Install using system defaults (/etc, /).
pub fn install(config: &Config) -> Result<(), Box<dyn std::error::Error>> {
    let config_path = PathBuf::from(CONFIG_FILE);
    let manifest_path = PathBuf::from(MANIFEST_FILE);
    let root = PathBuf::from("/");
    install_to_prefix(config, &root, &config_path, &manifest_path)?;

    // Enable systemd timers (only in real installs, not in install_to_prefix tests)
    if config.init.system == crate::setup::config::InitSystem::Systemd {
        let _ = std::process::Command::new("systemctl")
            .args(["daemon-reload"])
            .status();
        let _ = std::process::Command::new("systemctl")
            .args(["enable", "--now", "das-backup.timer"])
            .status();
        let _ = std::process::Command::new("systemctl")
            .args(["enable", "--now", "das-backup-full.timer"])
            .status();
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

    // Generate all files
    let generated = GeneratedFiles::generate(config);
    let mut manifest_entries = vec![config_path.to_string_lossy().to_string()];

    for (rel_path, content) in &generated.files {
        let full_path = if rel_path.starts_with('/') {
            root.join(rel_path.strip_prefix('/').unwrap_or(rel_path.as_ref()))
        } else {
            root.join(rel_path)
        };

        if let Some(parent) = full_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        std::fs::write(&full_path, content)?;

        // Make scripts executable
        if full_path.extension().and_then(|e| e.to_str()) == Some("sh") {
            let mut perms = std::fs::metadata(&full_path)?.permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&full_path, perms)?;
        }

        // Email config gets restricted permissions
        if full_path.to_string_lossy().contains("email.conf") {
            let mut perms = std::fs::metadata(&full_path)?.permissions();
            perms.set_mode(0o600);
            std::fs::set_permissions(&full_path, perms)?;
        }

        manifest_entries.push(full_path.to_string_lossy().to_string());
    }

    // Write manifest
    if let Some(parent) = manifest_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(manifest_path, manifest_entries.join("\n"))?;

    // Create DB directory
    if let Some(parent) = Path::new(&config.general.db_path).parent() {
        let _ = std::fs::create_dir_all(parent);
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

    let db_path = Config::load(&PathBuf::from(CONFIG_FILE))
        .ok()
        .map(|c| c.general.db_path);

    let _ = std::process::Command::new("systemctl")
        .args(["disable", "--now", "das-backup.timer"])
        .status();
    let _ = std::process::Command::new("systemctl")
        .args(["disable", "--now", "das-backup-full.timer"])
        .status();

    let removed = uninstall_from_manifest(&manifest_path);
    println!("Removed {} files.", removed);

    let _ = std::fs::remove_file(&manifest_path);
    let _ = std::fs::remove_dir(CONFIG_DIR);

    if remove_db
        && let Some(db) = db_path
        && Path::new(&db).exists()
    {
        std::fs::remove_file(&db)?;
        println!("Removed database: {}", db);
    }

    let _ = std::process::Command::new("systemctl")
        .args(["daemon-reload"])
        .status();

    println!("Uninstall complete.");
    Ok(())
}

/// Remove all files listed in a manifest. Returns the count of files removed.
pub fn uninstall_from_manifest(manifest_path: &Path) -> usize {
    let content = match std::fs::read_to_string(manifest_path) {
        Ok(c) => c,
        Err(_) => return 0,
    };

    let mut removed = 0;
    for line in content.lines() {
        let path = Path::new(line.trim());
        if path.exists() && std::fs::remove_file(path).is_ok() {
            removed += 1;
        }
    }
    removed
}

/// Upgrade: reload existing config and regenerate all files.
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
    if old_version != config.general.version {
        println!(
            "Updating config version: {} -> {}",
            old_version, config.general.version
        );
        config.save(&config_path)?;
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

    let deps = crate::setup::detect::check_dependencies(config.email.enabled, config.esp.mirror);
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
        // FFI
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
        p("lib/das-backup/config/das-backup-email.conf.example"),
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
    let _ = std::process::Command::new("systemctl")
        .args(["disable", "--now", "btrdasd-helper.service"])
        .status();

    // Phase 3: determine install prefix from config (default /usr)
    let prefix = Config::load(&PathBuf::from(CONFIG_FILE))
        .ok()
        .map(|c| c.general.install_prefix.clone())
        .unwrap_or_else(|| "/usr".to_string());

    let paths = cmake_installed_paths(&prefix);
    let removed = remove_paths(&paths);
    println!("Removed {} cmake-installed files.", removed);

    // Phase 4: clean up empty directories
    let _ = std::fs::remove_dir_all(format!("{prefix}/lib/das-backup"));
    let _ = std::fs::remove_dir("/var/lib/das-backup");

    let _ = std::process::Command::new("systemctl")
        .args(["daemon-reload"])
        .status();

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
            }],
            device: "/dev/sda".to_string(),
            snapshot_dir: ".btrbk-snapshots".into(),
            target_subdirs: vec![],
        });
        config.targets.push(Target {
            label: "tgt".to_string(),
            serial: "ABC123".to_string(),
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

        let removed = uninstall_from_manifest(&manifest);
        assert_eq!(removed, 2);
        assert!(!file1.exists());
        assert!(!file2.exists());
    }
}
