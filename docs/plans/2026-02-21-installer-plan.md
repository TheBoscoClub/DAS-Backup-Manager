# ButteredDASD Installer (`btrdasd setup`) Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Add a `btrdasd setup` subcommand that interactively configures the backup system, generating all config files and scripts from a single TOML source of truth.

**Architecture:** The setup wizard detects the system (subvolumes, devices, ESP, init system, package manager), walks the user through configuration via dialoguer prompts, writes `/etc/das-backup/config.toml`, then renders all operational files (scripts, btrbk.conf, systemd/cron units) from embedded templates. Modes: fresh install, modify, upgrade, uninstall, check.

**Tech Stack:** Rust (edition 2024), clap (CLI), dialoguer + console (TUI), toml + serde (config), include_str! (templates), cargo test

---

### Task 1: Add new dependencies and `setup` subcommand skeleton

**Files:**
- Modify: `indexer/Cargo.toml`
- Modify: `indexer/src/main.rs`
- Create: `indexer/src/setup/mod.rs`

**Step 1: Add dependencies to Cargo.toml**

Add to `[dependencies]` in `indexer/Cargo.toml`:
```toml
serde = { version = "1", features = ["derive"] }
toml = "0.8"
dialoguer = { version = "0.11", features = ["fuzzy-select"] }
console = "0.15"
```

**Step 2: Create setup module skeleton**

Create `indexer/src/setup/mod.rs`:
```rust
pub mod config;

use clap::Args;

#[derive(Args)]
pub struct SetupArgs {
    /// Re-open wizard with current config pre-filled
    #[arg(long)]
    pub modify: bool,

    /// Regenerate files from existing config (after binary update)
    #[arg(long)]
    pub upgrade: bool,

    /// Remove all generated files, disable timers, optionally remove DB
    #[arg(long)]
    pub uninstall: bool,

    /// Validate config + deps, report issues, change nothing
    #[arg(long)]
    pub check: bool,
}

pub fn run(args: SetupArgs) -> Result<(), Box<dyn std::error::Error>> {
    // Root check
    if !nix_is_root() {
        eprintln!("Error: btrdasd setup requires root privileges.");
        eprintln!("Run: sudo btrdasd setup");
        std::process::exit(1);
    }

    if args.check {
        println!("setup --check: not yet implemented");
    } else if args.uninstall {
        println!("setup --uninstall: not yet implemented");
    } else if args.upgrade {
        println!("setup --upgrade: not yet implemented");
    } else {
        // Fresh install or --modify
        println!("setup wizard: not yet implemented");
    }
    Ok(())
}

fn nix_is_root() -> bool {
    unsafe { libc::geteuid() == 0 }
}
```

**Step 3: Add `libc` dependency for root check**

Add to `[dependencies]` in `indexer/Cargo.toml`:
```toml
libc = "0.2"
```

**Step 4: Wire into main.rs**

In `indexer/src/main.rs`, add `mod setup;` at the top (NOT in lib.rs — setup is binary-only, not library).

Actually, since `main.rs` uses `buttered_dasd::` imports from lib.rs, we need setup as a separate top-level module. Add to `main.rs`:

```rust
mod setup;
```

Add `Setup` variant to `Commands` enum:
```rust
    /// Interactive setup wizard — configure backup sources, targets, and scheduling
    Setup(setup::SetupArgs),
```

Add match arm in `main()`:
```rust
        Commands::Setup(args) => {
            setup::run(args)?;
        }
```

**Step 5: Create empty config submodule**

Create `indexer/src/setup/config.rs`:
```rust
// Config types — implemented in Task 2
```

**Step 6: Build and verify**

Run: `cd /hddRaid1/ClaudeCodeProjects/DAS-Backup-Manager/indexer && cargo build 2>&1`
Expected: Compiles with zero errors.

Run: `cargo run -- setup --help`
Expected: Shows setup subcommand help with `--modify`, `--upgrade`, `--uninstall`, `--check` flags.

**Step 7: Commit**

```bash
git add indexer/Cargo.toml indexer/src/main.rs indexer/src/setup/mod.rs indexer/src/setup/config.rs
git commit -m "feat(setup): add setup subcommand skeleton with clap args"
```

---

### Task 2: Config types and TOML serialization

**Files:**
- Modify: `indexer/src/setup/config.rs`

**Step 1: Write the failing test**

Add to end of `indexer/src/setup/config.rs`:
```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_default_config() {
        let config = Config::default();
        let toml_str = config.to_toml().unwrap();
        let parsed = Config::from_toml(&toml_str).unwrap();
        assert_eq!(parsed.general.version, config.general.version);
        assert_eq!(parsed.init.system, InitSystem::Systemd);
        assert_eq!(parsed.schedule.incremental, "03:00");
    }

    #[test]
    fn roundtrip_full_config() {
        let mut config = Config::default();
        config.sources.push(Source {
            label: "nvme-root".to_string(),
            volume: "/.btrfs-nvme".to_string(),
            subvolumes: vec!["@".to_string(), "@home".to_string()],
            device: "/dev/nvme0n1p2".to_string(),
        });
        config.targets.push(Target {
            label: "primary-22tb".to_string(),
            serial: "ZXA0LMAE".to_string(),
            mount: "/mnt/backup-22tb".to_string(),
            role: TargetRole::Primary,
            retention: Retention { weekly: 4, monthly: 2 },
        });
        config.esp.enabled = true;
        config.esp.mirror = true;
        config.esp.partitions = vec!["/dev/nvme0n1p1".to_string()];
        config.esp.mount_points = vec!["/efi".to_string()];
        config.email.enabled = true;
        config.email.smtp_host = "127.0.0.1".to_string();
        config.email.smtp_port = 1025;

        let toml_str = config.to_toml().unwrap();
        let parsed = Config::from_toml(&toml_str).unwrap();

        assert_eq!(parsed.sources.len(), 1);
        assert_eq!(parsed.sources[0].label, "nvme-root");
        assert_eq!(parsed.sources[0].subvolumes, vec!["@", "@home"]);
        assert_eq!(parsed.targets.len(), 1);
        assert_eq!(parsed.targets[0].serial, "ZXA0LMAE");
        assert_eq!(parsed.targets[0].role, TargetRole::Primary);
        assert!(parsed.esp.enabled);
        assert!(parsed.email.enabled);
        assert_eq!(parsed.email.smtp_port, 1025);
    }

    #[test]
    fn config_validates_no_sources() {
        let config = Config::default();
        let errors = config.validate();
        assert!(errors.iter().any(|e| e.contains("source")));
    }

    #[test]
    fn config_validates_no_targets() {
        let mut config = Config::default();
        config.sources.push(Source {
            label: "test".to_string(),
            volume: "/test".to_string(),
            subvolumes: vec!["@".to_string()],
            device: "/dev/sda".to_string(),
        });
        let errors = config.validate();
        assert!(errors.iter().any(|e| e.contains("target")));
    }
}
```

**Step 2: Run test to verify it fails**

Run: `cd /hddRaid1/ClaudeCodeProjects/DAS-Backup-Manager/indexer && cargo test setup::config::tests -- --nocapture 2>&1`
Expected: FAIL — `Config` type not defined.

**Step 3: Write minimal implementation**

Replace contents of `indexer/src/setup/config.rs`:
```rust
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub general: General,
    pub init: Init,
    pub schedule: Schedule,
    #[serde(default, rename = "source")]
    pub sources: Vec<Source>,
    #[serde(default, rename = "target")]
    pub targets: Vec<Target>,
    #[serde(default)]
    pub esp: Esp,
    #[serde(default)]
    pub email: Email,
    #[serde(default)]
    pub gui: Gui,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct General {
    pub version: String,
    pub install_prefix: String,
    pub db_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Init {
    pub system: InitSystem,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InitSystem {
    Systemd,
    Sysvinit,
    Openrc,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Schedule {
    pub incremental: String,
    pub full: String,
    pub randomized_delay_min: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Source {
    pub label: String,
    pub volume: String,
    pub subvolumes: Vec<String>,
    pub device: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Target {
    pub label: String,
    pub serial: String,
    pub mount: String,
    pub role: TargetRole,
    pub retention: Retention,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TargetRole {
    Primary,
    Mirror,
    EspSync,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Retention {
    pub weekly: u32,
    pub monthly: u32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Esp {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub mirror: bool,
    #[serde(default)]
    pub partitions: Vec<String>,
    #[serde(default)]
    pub mount_points: Vec<String>,
    #[serde(default)]
    pub hooks: EspHooks,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EspHooks {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default, rename = "type")]
    pub hook_type: HookType,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HookType {
    Pacman,
    Apt,
    Dnf,
    #[default]
    None,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Email {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub smtp_host: String,
    #[serde(default)]
    pub smtp_port: u16,
    #[serde(default)]
    pub from: String,
    #[serde(default)]
    pub to: String,
    #[serde(default)]
    pub auth: AuthMethod,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AuthMethod {
    Plain,
    Starttls,
    #[default]
    None,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Gui {
    #[serde(default)]
    pub enabled: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            general: General {
                version: env!("CARGO_PKG_VERSION").to_string(),
                install_prefix: "/usr/local".to_string(),
                db_path: "/var/lib/das-backup/backup-index.db".to_string(),
            },
            init: Init {
                system: InitSystem::Systemd,
            },
            schedule: Schedule {
                incremental: "03:00".to_string(),
                full: "Sun 04:00".to_string(),
                randomized_delay_min: 30,
            },
            sources: Vec::new(),
            targets: Vec::new(),
            esp: Esp::default(),
            email: Email::default(),
            gui: Gui::default(),
        }
    }
}

impl Config {
    pub fn to_toml(&self) -> Result<String, toml::ser::Error> {
        toml::to_string_pretty(self)
    }

    pub fn from_toml(s: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(s)
    }

    pub fn load(path: &std::path::Path) -> Result<Self, Box<dyn std::error::Error>> {
        let content = std::fs::read_to_string(path)?;
        Ok(Self::from_toml(&content)?)
    }

    pub fn save(&self, path: &std::path::Path) -> Result<(), Box<dyn std::error::Error>> {
        let dir = path.parent().ok_or("invalid config path")?;
        std::fs::create_dir_all(dir)?;
        let content = format!(
            "# DAS-Backup-Manager configuration\n\
             # Generated by btrdasd setup — edit via: sudo btrdasd setup --modify\n\
             # Regenerate files: sudo btrdasd setup --upgrade\n\n{}",
            self.to_toml()?
        );
        std::fs::write(path, content)?;
        Ok(())
    }

    pub fn validate(&self) -> Vec<String> {
        let mut errors = Vec::new();
        if self.sources.is_empty() {
            errors.push("No backup sources configured. Add at least one source volume.".to_string());
        }
        if self.targets.is_empty() {
            errors.push("No backup targets configured. Add at least one target device.".to_string());
        }
        for (i, src) in self.sources.iter().enumerate() {
            if src.subvolumes.is_empty() {
                errors.push(format!("Source '{}' (index {}) has no subvolumes selected.", src.label, i));
            }
            if src.device.is_empty() {
                errors.push(format!("Source '{}' (index {}) has no device path.", src.label, i));
            }
        }
        for (i, tgt) in self.targets.iter().enumerate() {
            if tgt.serial.is_empty() {
                errors.push(format!("Target '{}' (index {}) has no serial number.", tgt.label, i));
            }
        }
        if self.email.enabled && self.email.smtp_host.is_empty() {
            errors.push("Email enabled but no SMTP host configured.".to_string());
        }
        if self.esp.mirror && self.esp.partitions.len() < 2 {
            errors.push("ESP mirroring enabled but fewer than 2 partitions configured.".to_string());
        }
        errors
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_default_config() {
        let config = Config::default();
        let toml_str = config.to_toml().unwrap();
        let parsed = Config::from_toml(&toml_str).unwrap();
        assert_eq!(parsed.general.version, config.general.version);
        assert_eq!(parsed.init.system, InitSystem::Systemd);
        assert_eq!(parsed.schedule.incremental, "03:00");
    }

    #[test]
    fn roundtrip_full_config() {
        let mut config = Config::default();
        config.sources.push(Source {
            label: "nvme-root".to_string(),
            volume: "/.btrfs-nvme".to_string(),
            subvolumes: vec!["@".to_string(), "@home".to_string()],
            device: "/dev/nvme0n1p2".to_string(),
        });
        config.targets.push(Target {
            label: "primary-22tb".to_string(),
            serial: "ZXA0LMAE".to_string(),
            mount: "/mnt/backup-22tb".to_string(),
            role: TargetRole::Primary,
            retention: Retention { weekly: 4, monthly: 2 },
        });
        config.esp.enabled = true;
        config.esp.mirror = true;
        config.esp.partitions = vec!["/dev/nvme0n1p1".to_string()];
        config.esp.mount_points = vec!["/efi".to_string()];
        config.email.enabled = true;
        config.email.smtp_host = "127.0.0.1".to_string();
        config.email.smtp_port = 1025;

        let toml_str = config.to_toml().unwrap();
        let parsed = Config::from_toml(&toml_str).unwrap();

        assert_eq!(parsed.sources.len(), 1);
        assert_eq!(parsed.sources[0].label, "nvme-root");
        assert_eq!(parsed.sources[0].subvolumes, vec!["@", "@home"]);
        assert_eq!(parsed.targets.len(), 1);
        assert_eq!(parsed.targets[0].serial, "ZXA0LMAE");
        assert_eq!(parsed.targets[0].role, TargetRole::Primary);
        assert!(parsed.esp.enabled);
        assert!(parsed.email.enabled);
        assert_eq!(parsed.email.smtp_port, 1025);
    }

    #[test]
    fn config_validates_no_sources() {
        let config = Config::default();
        let errors = config.validate();
        assert!(errors.iter().any(|e| e.contains("source")));
    }

    #[test]
    fn config_validates_no_targets() {
        let mut config = Config::default();
        config.sources.push(Source {
            label: "test".to_string(),
            volume: "/test".to_string(),
            subvolumes: vec!["@".to_string()],
            device: "/dev/sda".to_string(),
        });
        let errors = config.validate();
        assert!(errors.iter().any(|e| e.contains("target")));
    }
}
```

**Step 4: Run tests to verify they pass**

Run: `cd /hddRaid1/ClaudeCodeProjects/DAS-Backup-Manager/indexer && cargo test setup::config::tests -- --nocapture 2>&1`
Expected: 4 tests PASS.

**Step 5: Commit**

```bash
git add indexer/src/setup/config.rs
git commit -m "feat(setup): config types with TOML serde and validation"
```

---

### Task 3: System detection module

**Files:**
- Create: `indexer/src/setup/detect.rs`
- Modify: `indexer/src/setup/mod.rs` (add `pub mod detect;`)

**Step 1: Write the failing test**

Create `indexer/src/setup/detect.rs` with test at the bottom:
```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_lsblk_json() {
        let json = r#"{"blockdevices":[
            {"name":"sda","size":"22T","fstype":"btrfs","serial":"ZXA0LMAE","model":"Exos X22","tran":"usb"},
            {"name":"nvme0n1p1","size":"512M","fstype":"vfat","serial":null,"model":null,"tran":"nvme"},
            {"name":"nvme0n1p2","size":"500G","fstype":"btrfs","serial":null,"model":null,"tran":"nvme"}
        ]}"#;
        let devices = parse_lsblk_output(json).unwrap();
        assert_eq!(devices.len(), 3);
        assert_eq!(devices[0].serial, Some("ZXA0LMAE".to_string()));
        assert_eq!(devices[0].tran, Some("usb".to_string()));
        assert!(devices[0].is_usb());
        assert!(devices[1].is_esp_candidate());
        assert!(!devices[2].is_esp_candidate());
    }

    #[test]
    fn parse_subvolume_list() {
        let output = "ID 256 gen 12345 top level 5 path @\n\
                       ID 257 gen 12340 top level 5 path @home\n\
                       ID 258 gen 12300 top level 5 path @log\n\
                       ID 259 gen 12200 top level 256 path @home/.snapshots\n";
        let subvols = parse_subvolume_output(output);
        assert_eq!(subvols.len(), 4);
        assert_eq!(subvols[0].name, "@");
        assert_eq!(subvols[0].id, 256);
        assert_eq!(subvols[1].name, "@home");
    }

    #[test]
    fn detect_init_system_from_paths() {
        // This tests the logic, not actual system state
        assert_eq!(
            detect_init_from_binaries(true, false, false),
            InitSystemDetected::Systemd
        );
        assert_eq!(
            detect_init_from_binaries(false, false, true),
            InitSystemDetected::Openrc
        );
        assert_eq!(
            detect_init_from_binaries(false, false, false),
            InitSystemDetected::Sysvinit
        );
    }

    #[test]
    fn detect_package_manager_from_binaries() {
        assert_eq!(
            detect_pkgmgr_from_binaries(true, false, false, false, false),
            PackageManager::Pacman
        );
        assert_eq!(
            detect_pkgmgr_from_binaries(false, true, false, false, false),
            PackageManager::Apt
        );
    }
}
```

**Step 2: Run test to verify it fails**

Run: `cd /hddRaid1/ClaudeCodeProjects/DAS-Backup-Manager/indexer && cargo test setup::detect::tests -- --nocapture 2>&1`
Expected: FAIL — types not defined.

**Step 3: Write minimal implementation**

Write the full `indexer/src/setup/detect.rs`:
```rust
use serde::Deserialize;
use std::path::Path;
use std::process::Command;

// ── Block device detection ──────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct BlockDevice {
    pub name: String,
    pub size: String,
    pub fstype: Option<String>,
    pub serial: Option<String>,
    pub model: Option<String>,
    pub tran: Option<String>,
}

impl BlockDevice {
    pub fn is_usb(&self) -> bool {
        self.tran.as_deref() == Some("usb")
    }

    pub fn is_esp_candidate(&self) -> bool {
        self.fstype.as_deref() == Some("vfat") && self.size_bytes() < 2_000_000_000
    }

    fn size_bytes(&self) -> u64 {
        // Parse human-readable sizes like "512M", "22T"
        let s = self.size.trim();
        let (num_str, suffix) = s.split_at(s.len().saturating_sub(1));
        let num: f64 = num_str.parse().unwrap_or(0.0);
        match suffix {
            "K" => (num * 1024.0) as u64,
            "M" => (num * 1024.0 * 1024.0) as u64,
            "G" => (num * 1024.0 * 1024.0 * 1024.0) as u64,
            "T" => (num * 1024.0 * 1024.0 * 1024.0 * 1024.0) as u64,
            _ => num as u64,
        }
    }
}

#[derive(Deserialize)]
struct LsblkOutput {
    blockdevices: Vec<LsblkDevice>,
}

#[derive(Deserialize)]
struct LsblkDevice {
    name: String,
    size: String,
    fstype: Option<String>,
    serial: Option<String>,
    model: Option<String>,
    tran: Option<String>,
}

pub fn parse_lsblk_output(json: &str) -> Result<Vec<BlockDevice>, serde_json::Error> {
    let output: LsblkOutput = serde_json::from_str(json)?;
    Ok(output
        .blockdevices
        .into_iter()
        .map(|d| BlockDevice {
            name: d.name,
            size: d.size,
            fstype: d.fstype,
            serial: d.serial,
            model: d.model,
            tran: d.tran,
        })
        .collect())
}

pub fn detect_block_devices() -> Vec<BlockDevice> {
    let output = Command::new("lsblk")
        .args(["--json", "-o", "NAME,SIZE,FSTYPE,SERIAL,MODEL,TRAN"])
        .output();

    match output {
        Ok(o) if o.status.success() => {
            let json = String::from_utf8_lossy(&o.stdout);
            parse_lsblk_output(&json).unwrap_or_default()
        }
        _ => Vec::new(),
    }
}

// ── BTRFS subvolume detection ───────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct SubvolumeInfo {
    pub id: u64,
    pub name: String,
    pub top_level: u64,
}

pub fn parse_subvolume_output(output: &str) -> Vec<SubvolumeInfo> {
    output
        .lines()
        .filter_map(|line| {
            // Format: ID <id> gen <gen> top level <top> path <name>
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 9 && parts[0] == "ID" {
                Some(SubvolumeInfo {
                    id: parts[1].parse().ok()?,
                    top_level: parts[6].parse().ok()?,
                    name: parts[8..].join(" "),
                })
            } else {
                None
            }
        })
        .collect()
}

pub fn detect_subvolumes() -> Vec<SubvolumeInfo> {
    let output = Command::new("btrfs")
        .args(["subvolume", "list", "/"])
        .output();

    match output {
        Ok(o) if o.status.success() => {
            let text = String::from_utf8_lossy(&o.stdout);
            parse_subvolume_output(&text)
        }
        _ => Vec::new(),
    }
}

// ── Init system detection ───────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub enum InitSystemDetected {
    Systemd,
    Openrc,
    Sysvinit,
}

pub fn detect_init_from_binaries(
    has_systemctl: bool,
    has_initd: bool,
    has_rc_service: bool,
) -> InitSystemDetected {
    if has_systemctl {
        InitSystemDetected::Systemd
    } else if has_rc_service {
        InitSystemDetected::Openrc
    } else {
        InitSystemDetected::Sysvinit
    }
}

pub fn detect_init_system() -> InitSystemDetected {
    let has_systemctl = which("systemctl");
    let has_initd = Path::new("/etc/init.d").is_dir();
    let has_rc_service = which("rc-service");
    detect_init_from_binaries(has_systemctl, has_initd, has_rc_service)
}

// ── Package manager detection ───────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub enum PackageManager {
    Pacman,
    Apt,
    Dnf,
    Zypper,
    Apk,
    Unknown,
}

impl PackageManager {
    pub fn install_cmd(&self, packages: &[&str]) -> String {
        let pkgs = packages.join(" ");
        match self {
            Self::Pacman => format!("pacman -S --noconfirm {pkgs}"),
            Self::Apt => format!("apt-get install -y {pkgs}"),
            Self::Dnf => format!("dnf install -y {pkgs}"),
            Self::Zypper => format!("zypper install -y {pkgs}"),
            Self::Apk => format!("apk add {pkgs}"),
            Self::Unknown => format!("# Install manually: {pkgs}"),
        }
    }
}

pub fn detect_pkgmgr_from_binaries(
    pacman: bool,
    apt: bool,
    dnf: bool,
    zypper: bool,
    apk: bool,
) -> PackageManager {
    if pacman {
        PackageManager::Pacman
    } else if apt {
        PackageManager::Apt
    } else if dnf {
        PackageManager::Dnf
    } else if zypper {
        PackageManager::Zypper
    } else if apk {
        PackageManager::Apk
    } else {
        PackageManager::Unknown
    }
}

pub fn detect_package_manager() -> PackageManager {
    detect_pkgmgr_from_binaries(
        which("pacman"),
        which("apt-get"),
        which("dnf"),
        which("zypper"),
        which("apk"),
    )
}

// ── Dependency checking ─────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct DepStatus {
    pub name: String,
    pub required: bool,
    pub path: Option<String>,
}

pub fn check_dependencies(email_enabled: bool, esp_mirror: bool) -> Vec<DepStatus> {
    let mut deps = vec![
        DepStatus { name: "btrbk".to_string(), required: true, path: which_path("btrbk") },
        DepStatus { name: "btrfs".to_string(), required: true, path: which_path("btrfs") },
        DepStatus { name: "smartctl".to_string(), required: true, path: which_path("smartctl") },
        DepStatus { name: "lsblk".to_string(), required: true, path: which_path("lsblk") },
        DepStatus { name: "mbuffer".to_string(), required: false, path: which_path("mbuffer") },
    ];
    if email_enabled {
        deps.push(DepStatus { name: "msmtp".to_string(), required: true, path: which_path("msmtp") });
    }
    if esp_mirror {
        deps.push(DepStatus { name: "rsync".to_string(), required: true, path: which_path("rsync") });
    }
    deps
}

// ── Aggregate system info ───────────────────────────────────────────────

#[derive(Debug)]
pub struct SystemInfo {
    pub devices: Vec<BlockDevice>,
    pub subvolumes: Vec<SubvolumeInfo>,
    pub init_system: InitSystemDetected,
    pub package_manager: PackageManager,
    pub deps: Vec<DepStatus>,
}

impl SystemInfo {
    pub fn detect() -> Self {
        let init_system = detect_init_system();
        let package_manager = detect_package_manager();
        Self {
            devices: detect_block_devices(),
            subvolumes: detect_subvolumes(),
            init_system,
            package_manager,
            deps: check_dependencies(false, false),
        }
    }
}

// ── Helpers ─────────────────────────────────────────────────────────────

fn which(binary: &str) -> bool {
    Command::new("which")
        .arg(binary)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn which_path(binary: &str) -> Option<String> {
    Command::new("which")
        .arg(binary)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_lsblk_json() {
        let json = r#"{"blockdevices":[
            {"name":"sda","size":"22T","fstype":"btrfs","serial":"ZXA0LMAE","model":"Exos X22","tran":"usb"},
            {"name":"nvme0n1p1","size":"512M","fstype":"vfat","serial":null,"model":null,"tran":"nvme"},
            {"name":"nvme0n1p2","size":"500G","fstype":"btrfs","serial":null,"model":null,"tran":"nvme"}
        ]}"#;
        let devices = parse_lsblk_output(json).unwrap();
        assert_eq!(devices.len(), 3);
        assert_eq!(devices[0].serial, Some("ZXA0LMAE".to_string()));
        assert_eq!(devices[0].tran, Some("usb".to_string()));
        assert!(devices[0].is_usb());
        assert!(devices[1].is_esp_candidate());
        assert!(!devices[2].is_esp_candidate());
    }

    #[test]
    fn parse_subvolume_list() {
        let output = "ID 256 gen 12345 top level 5 path @\n\
                       ID 257 gen 12340 top level 5 path @home\n\
                       ID 258 gen 12300 top level 5 path @log\n\
                       ID 259 gen 12200 top level 256 path @home/.snapshots\n";
        let subvols = parse_subvolume_output(output);
        assert_eq!(subvols.len(), 4);
        assert_eq!(subvols[0].name, "@");
        assert_eq!(subvols[0].id, 256);
        assert_eq!(subvols[1].name, "@home");
    }

    #[test]
    fn detect_init_system_from_paths() {
        assert_eq!(
            detect_init_from_binaries(true, false, false),
            InitSystemDetected::Systemd
        );
        assert_eq!(
            detect_init_from_binaries(false, false, true),
            InitSystemDetected::Openrc
        );
        assert_eq!(
            detect_init_from_binaries(false, false, false),
            InitSystemDetected::Sysvinit
        );
    }

    #[test]
    fn detect_package_manager_from_binaries() {
        assert_eq!(
            detect_pkgmgr_from_binaries(true, false, false, false, false),
            PackageManager::Pacman
        );
        assert_eq!(
            detect_pkgmgr_from_binaries(false, true, false, false, false),
            PackageManager::Apt
        );
    }
}
```

**Step 4: Add `serde_json` dependency**

Add to `[dependencies]` in `indexer/Cargo.toml`:
```toml
serde_json = "1"
```

**Step 5: Add `pub mod detect;` to `indexer/src/setup/mod.rs`**

**Step 6: Run tests to verify they pass**

Run: `cd /hddRaid1/ClaudeCodeProjects/DAS-Backup-Manager/indexer && cargo test setup::detect::tests -- --nocapture 2>&1`
Expected: 4 tests PASS.

**Step 7: Commit**

```bash
git add indexer/Cargo.toml indexer/src/setup/detect.rs indexer/src/setup/mod.rs
git commit -m "feat(setup): system detection for devices, subvolumes, init, and package manager"
```

---

### Task 4: Template engine

**Files:**
- Create: `indexer/src/setup/templates.rs`
- Create: `indexer/templates/` directory with template files
- Modify: `indexer/src/setup/mod.rs` (add `pub mod templates;`)

**Step 1: Write the failing test**

Add tests at bottom of new `indexer/src/setup/templates.rs`:
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::setup::config::*;

    fn test_config() -> Config {
        let mut config = Config::default();
        config.general.install_prefix = "/usr/local".to_string();
        config.sources.push(Source {
            label: "nvme-root".to_string(),
            volume: "/.btrfs-nvme".to_string(),
            subvolumes: vec!["@".to_string(), "@home".to_string()],
            device: "/dev/nvme0n1p2".to_string(),
        });
        config.targets.push(Target {
            label: "primary-22tb".to_string(),
            serial: "ZXA0LMAE".to_string(),
            mount: "/mnt/backup-22tb".to_string(),
            role: TargetRole::Primary,
            retention: Retention { weekly: 4, monthly: 2 },
        });
        config
    }

    #[test]
    fn render_btrbk_conf() {
        let config = test_config();
        let result = render_btrbk_conf(&config);
        assert!(result.contains("volume /.btrfs-nvme"));
        assert!(result.contains("subvolume @"));
        assert!(result.contains("subvolume @home"));
        assert!(result.contains("target /mnt/backup-22tb"));
        assert!(result.contains("target_preserve         4w 2m"));
        assert!(result.contains("# Generated by btrdasd setup"));
    }

    #[test]
    fn render_systemd_service() {
        let config = test_config();
        let result = render_systemd_service(&config, false);
        assert!(result.contains("ExecStart=/usr/local/lib/das-backup/backup-run.sh"));
        assert!(!result.contains("/hddRaid1/"));
        assert!(result.contains("# Generated by btrdasd setup"));
    }

    #[test]
    fn render_systemd_timer() {
        let config = test_config();
        let result = render_systemd_timer(&config, false);
        assert!(result.contains("OnCalendar=*-*-* 03:00:00"));
        assert!(result.contains("RandomizedDelaySec=1800"));
    }

    #[test]
    fn render_cron_entry() {
        let config = test_config();
        let result = render_cron_entry(&config);
        assert!(result.contains("0 3 * * *"));
        assert!(result.contains("/usr/local/lib/das-backup/backup-run.sh"));
    }

    #[test]
    fn render_backup_run_script() {
        let config = test_config();
        let result = render_backup_run(&config);
        assert!(result.starts_with("#!/usr/bin/env bash"));
        assert!(result.contains("ZXA0LMAE"));
        assert!(result.contains("/.btrfs-nvme"));
        assert!(result.contains("/mnt/backup-22tb"));
        assert!(result.contains("# Generated by btrdasd setup"));
    }
}
```

**Step 2: Run test to verify it fails**

Run: `cd /hddRaid1/ClaudeCodeProjects/DAS-Backup-Manager/indexer && cargo test setup::templates::tests -- --nocapture 2>&1`
Expected: FAIL — functions not defined.

**Step 3: Write minimal implementation**

Create `indexer/src/setup/templates.rs` with render functions. Each function builds output strings programmatically from the config — no external template files needed at this stage. The `include_str!` approach from the design can be adopted later if templates get complex, but for now direct string building is simpler and fully testable:

```rust
use crate::setup::config::*;

const GENERATED_HEADER: &str = "# Generated by btrdasd setup — do not edit.\n\
    # Modify /etc/das-backup/config.toml and run: sudo btrdasd setup --upgrade\n";

pub fn render_btrbk_conf(config: &Config) -> String {
    let mut out = String::from(GENERATED_HEADER);
    out.push('\n');

    // Global settings
    out.push_str("# Global settings\n");
    out.push_str("transaction_log         /var/log/btrbk.log\n");
    out.push_str("stream_buffer           256m\n");
    out.push_str("stream_compress         zstd\n");
    out.push_str("lockfile                /var/lock/btrbk.lock\n\n");

    // Source retention
    out.push_str("# Source snapshots — minimal, for send reference\n");
    out.push_str("snapshot_preserve_min   latest\n");
    out.push_str("snapshot_preserve       2d\n\n");

    // Per-source volume blocks
    for source in &config.sources {
        // Find targets that are primary or mirror (not esp-sync)
        let targets: Vec<&Target> = config.targets.iter()
            .filter(|t| t.role == TargetRole::Primary || t.role == TargetRole::Mirror)
            .collect();

        for target in &targets {
            out.push_str(&format!("# {} -> {}\n", source.label, target.label));
            out.push_str(&format!(
                "target_preserve_min     latest\n\
                 target_preserve         {}w {}m\n\n",
                target.retention.weekly, target.retention.monthly
            ));
            out.push_str(&format!("volume {}\n", source.volume));
            out.push_str("  snapshot_dir          .btrbk-snapshots\n");
            out.push_str(&format!("  target                {}/{}\n\n", target.mount, source.label));

            for subvol in &source.subvolumes {
                out.push_str(&format!("  subvolume             {}\n", subvol));
                // Generate a safe snapshot name from subvolume
                let snap_name = subvol.replace('@', "").replace('/', "-");
                let snap_name = if snap_name.is_empty() { "root" } else { &snap_name };
                out.push_str(&format!("    snapshot_name       {}\n\n", snap_name));
            }
        }
    }

    out
}

pub fn render_systemd_service(config: &Config, full: bool) -> String {
    let script_dir = format!("{}/lib/das-backup", config.general.install_prefix);
    let desc = if full {
        "DAS Backup - Full BTRFS backup"
    } else {
        "DAS Backup - Incremental BTRFS backup"
    };
    let exec_args = if full { " --full" } else { "" };

    format!(
        "{GENERATED_HEADER}\
        [Unit]\n\
        Description={desc}\n\
        After=local-fs.target\n\
        \n\
        [Service]\n\
        Type=oneshot\n\
        ExecStart={script_dir}/backup-run.sh{exec_args}\n\
        StandardOutput=journal\n\
        StandardError=journal\n\
        Nice=19\n\
        IOSchedulingClass=idle\n\
        TimeoutStartSec=21600\n\
        \n\
        [Install]\n\
        WantedBy=multi-user.target\n"
    )
}

pub fn render_systemd_timer(config: &Config, full: bool) -> String {
    let (desc, calendar) = if full {
        ("DAS Backup Timer - Weekly full backup", &config.schedule.full)
    } else {
        ("DAS Backup Timer - Daily incremental backup", &config.schedule.incremental)
    };

    // Convert schedule to OnCalendar format
    let on_calendar = if full {
        // "Sun 04:00" -> "Sun *-*-* 04:00:00"
        let parts: Vec<&str> = calendar.split_whitespace().collect();
        if parts.len() == 2 {
            format!("{} *-*-* {}:00", parts[0], parts[1])
        } else {
            format!("*-*-* {}:00", calendar)
        }
    } else {
        // "03:00" -> "*-*-* 03:00:00"
        format!("*-*-* {}:00", calendar)
    };

    let delay_secs = config.schedule.randomized_delay_min * 60;

    format!(
        "{GENERATED_HEADER}\
        [Unit]\n\
        Description={desc}\n\
        \n\
        [Timer]\n\
        OnCalendar={on_calendar}\n\
        RandomizedDelaySec={delay_secs}\n\
        Persistent=true\n\
        WakeSystem=false\n\
        \n\
        [Install]\n\
        WantedBy=timers.target\n"
    )
}

pub fn render_cron_entry(config: &Config) -> String {
    let script_dir = format!("{}/lib/das-backup", config.general.install_prefix);

    // Parse "03:00" -> hour=3, minute=0
    let (inc_hour, inc_min) = parse_time(&config.schedule.incremental);

    // Parse "Sun 04:00" -> dow=0, hour=4, minute=0
    let (full_dow, full_hour, full_min) = parse_schedule_with_day(&config.schedule.full);

    format!(
        "{GENERATED_HEADER}\
        # Incremental backup (daily)\n\
        {inc_min} {inc_hour} * * * root {script_dir}/backup-run.sh >> /var/log/das-backup.log 2>&1\n\
        \n\
        # Full backup (weekly)\n\
        {full_min} {full_hour} * * {full_dow} root {script_dir}/backup-run.sh --full >> /var/log/das-backup.log 2>&1\n"
    )
}

pub fn render_backup_run(config: &Config) -> String {
    let mut out = String::from("#!/usr/bin/env bash\n");
    out.push_str(GENERATED_HEADER);
    out.push_str("set -euo pipefail\n\n");

    // Configuration section
    out.push_str("# ============================================================================\n");
    out.push_str("# CONFIGURATION (from /etc/das-backup/config.toml)\n");
    out.push_str("# ============================================================================\n\n");

    // DAS drive serials
    out.push_str("declare -A DAS_SERIALS=(\n");
    for target in &config.targets {
        out.push_str(&format!("    [\"{}\"]=\"{}\"\n", target.label, target.serial));
    }
    out.push_str(")\n\n");

    // Source mount points
    for source in &config.sources {
        let var_name = source.label.to_uppercase().replace('-', "_");
        out.push_str(&format!("MOUNT_{}=\"{}\"\n", var_name, source.volume));
    }
    out.push('\n');

    // Target mount points
    for target in &config.targets {
        let var_name = target.label.to_uppercase().replace('-', "_");
        out.push_str(&format!("MOUNT_{}=\"{}\"\n", var_name, target.mount));
    }
    out.push('\n');

    // Source devices
    for source in &config.sources {
        let var_name = source.label.to_uppercase().replace('-', "_");
        out.push_str(&format!("DEV_{}=\"{}\"\n", var_name, source.device));
    }
    out.push('\n');

    // Logging
    out.push_str("LOG_FILE=\"/var/log/das-backup.log\"\n\n");

    // Email config reference
    if config.email.enabled {
        out.push_str("EMAIL_CONF=\"/etc/das-backup/email.conf\"\n");
    }
    out.push('\n');

    // BTRDASD binary path
    out.push_str(&format!(
        "BTRDASD_BIN=\"${{BTRDASD_BIN:-{}/bin/btrdasd}}\"\n",
        config.general.install_prefix
    ));
    out.push_str(&format!("DB_PATH=\"{}\"\n\n", config.general.db_path));

    // Core functions (log, cleanup, etc.) — kept minimal
    out.push_str("# ============================================================================\n");
    out.push_str("# FUNCTIONS\n");
    out.push_str("# ============================================================================\n\n");

    out.push_str(r#"log() {
    local level="$1" msg="$2"
    local ts
    ts=$(date '+%Y-%m-%d %H:%M:%S')
    echo "[$ts] [$level] $msg" >> "$LOG_FILE"
    echo "[$level] $msg"
}

die() { log "ERROR" "$1"; exit 1; }
"#);

    out.push('\n');
    out.push_str("# ============================================================================\n");
    out.push_str("# MAIN\n");
    out.push_str("# ============================================================================\n\n");

    out.push_str("log \"INFO\" \"DAS backup starting\"\n\n");

    // Mount source volumes
    out.push_str("# Mount source top-level volumes\n");
    for source in &config.sources {
        let var_name = source.label.to_uppercase().replace('-', "_");
        out.push_str(&format!(
            "mkdir -p \"$MOUNT_{var_name}\"\n\
             mountpoint -q \"$MOUNT_{var_name}\" || mount -o subvolid=5 \"$DEV_{var_name}\" \"$MOUNT_{var_name}\"\n"
        ));
    }
    out.push('\n');

    // Run btrbk
    out.push_str("# Run btrbk\n");
    out.push_str("log \"INFO\" \"Running btrbk\"\n");
    out.push_str("btrbk -c /etc/das-backup/btrbk.conf run\n\n");

    // Run indexer
    out.push_str("# Index new snapshots\n");
    for target in &config.targets {
        if target.role == TargetRole::Primary || target.role == TargetRole::Mirror {
            out.push_str(&format!(
                "\"$BTRDASD_BIN\" walk \"{}\" --db \"$DB_PATH\" || log \"WARN\" \"Indexing {} failed (non-fatal)\"\n",
                target.mount, target.label
            ));
        }
    }
    out.push('\n');

    out.push_str("log \"INFO\" \"DAS backup complete\"\n");

    out
}

pub fn render_email_conf(config: &Config) -> String {
    if !config.email.enabled {
        return String::new();
    }

    let auth = match config.email.auth {
        AuthMethod::Plain => "on",
        AuthMethod::Starttls => "on",
        AuthMethod::None => "off",
    };
    let tls = match config.email.auth {
        AuthMethod::Starttls => "on",
        _ => "off",
    };

    format!(
        "{GENERATED_HEADER}\
        SMTP_HOST=\"{}\"\n\
        SMTP_PORT=\"{}\"\n\
        EMAIL_FROM=\"{}\"\n\
        EMAIL_TO=\"{}\"\n\
        SMTP_AUTH=\"{}\"\n\
        SMTP_TLS=\"{}\"\n",
        config.email.smtp_host, config.email.smtp_port,
        config.email.from, config.email.to,
        auth, tls
    )
}

pub fn render_esp_hook(config: &Config) -> Option<(String, String)> {
    if !config.esp.hooks.enabled {
        return None;
    }

    let script_dir = format!("{}/lib/das-backup", config.general.install_prefix);

    match config.esp.hooks.hook_type {
        HookType::Pacman => {
            let content = format!(
                "[Trigger]\n\
                 Type = Path\n\
                 Operation = Install\n\
                 Operation = Upgrade\n\
                 Target = usr/lib/modules/*/vmlinuz\n\
                 Target = boot/*\n\
                 \n\
                 [Action]\n\
                 Description = Syncing ESP mirrors after kernel update...\n\
                 When = PostTransaction\n\
                 Exec = {script_dir}/esp-sync.sh\n"
            );
            Some(("/etc/pacman.d/hooks/das-esp-sync.hook".to_string(), content))
        }
        HookType::Apt => {
            let content = format!(
                "DPkg::Post-Invoke {{\"{script_dir}/esp-sync.sh\";}};;\n"
            );
            Some(("/etc/apt/apt.conf.d/99-das-esp-sync".to_string(), content))
        }
        HookType::Dnf => {
            let content = format!(
                "#!/usr/bin/env bash\n\
                 {GENERATED_HEADER}\
                 # DNF plugin hook — called after kernel/bootloader transactions\n\
                 {script_dir}/esp-sync.sh\n"
            );
            Some(("/etc/dnf/plugins/das-esp-sync".to_string(), content))
        }
        HookType::None => None,
    }
}

// ── Manifest ────────────────────────────────────────────────────────────

pub struct GeneratedFiles {
    pub files: Vec<(String, String)>, // (path, content)
}

impl GeneratedFiles {
    pub fn generate(config: &Config) -> Self {
        let script_dir = format!("{}/lib/das-backup", config.general.install_prefix);
        let mut files = Vec::new();

        // Config file itself is written separately by Config::save()

        // btrbk.conf
        files.push(("/etc/das-backup/btrbk.conf".to_string(), render_btrbk_conf(config)));

        // backup-run.sh (needs +x)
        files.push((format!("{}/backup-run.sh", script_dir), render_backup_run(config)));

        // Scheduling
        match config.init.system {
            InitSystem::Systemd => {
                files.push(("/etc/systemd/system/das-backup.service".to_string(),
                    render_systemd_service(config, false)));
                files.push(("/etc/systemd/system/das-backup.timer".to_string(),
                    render_systemd_timer(config, false)));
                files.push(("/etc/systemd/system/das-backup-full.service".to_string(),
                    render_systemd_service(config, true)));
                files.push(("/etc/systemd/system/das-backup-full.timer".to_string(),
                    render_systemd_timer(config, true)));
            }
            InitSystem::Sysvinit | InitSystem::Openrc => {
                files.push(("/etc/cron.d/das-backup".to_string(), render_cron_entry(config)));
            }
        }

        // Email config
        if config.email.enabled {
            files.push(("/etc/das-backup/email.conf".to_string(), render_email_conf(config)));
        }

        // ESP hook
        if let Some((path, content)) = render_esp_hook(config) {
            files.push((path, content));
        }

        Self { files }
    }

    pub fn manifest_content(&self) -> String {
        self.files.iter().map(|(p, _)| p.as_str()).collect::<Vec<_>>().join("\n")
    }
}

// ── Helpers ─────────────────────────────────────────────────────────────

fn parse_time(time_str: &str) -> (u32, u32) {
    let parts: Vec<&str> = time_str.split(':').collect();
    let hour = parts.first().and_then(|s| s.parse().ok()).unwrap_or(3);
    let min = parts.get(1).and_then(|s| s.parse().ok()).unwrap_or(0);
    (hour, min)
}

fn parse_schedule_with_day(schedule: &str) -> (u32, u32, u32) {
    let parts: Vec<&str> = schedule.split_whitespace().collect();
    if parts.len() == 2 {
        let dow = match parts[0].to_lowercase().as_str() {
            "sun" => 0,
            "mon" => 1,
            "tue" => 2,
            "wed" => 3,
            "thu" => 4,
            "fri" => 5,
            "sat" => 6,
            _ => 0,
        };
        let (hour, min) = parse_time(parts[1]);
        (dow, hour, min)
    } else {
        let (hour, min) = parse_time(parts[0]);
        (0, hour, min) // Default to Sunday
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::setup::config::*;

    fn test_config() -> Config {
        let mut config = Config::default();
        config.general.install_prefix = "/usr/local".to_string();
        config.sources.push(Source {
            label: "nvme-root".to_string(),
            volume: "/.btrfs-nvme".to_string(),
            subvolumes: vec!["@".to_string(), "@home".to_string()],
            device: "/dev/nvme0n1p2".to_string(),
        });
        config.targets.push(Target {
            label: "primary-22tb".to_string(),
            serial: "ZXA0LMAE".to_string(),
            mount: "/mnt/backup-22tb".to_string(),
            role: TargetRole::Primary,
            retention: Retention { weekly: 4, monthly: 2 },
        });
        config
    }

    #[test]
    fn render_btrbk_conf_test() {
        let config = test_config();
        let result = render_btrbk_conf(&config);
        assert!(result.contains("volume /.btrfs-nvme"));
        assert!(result.contains("subvolume @"));
        assert!(result.contains("subvolume @home"));
        assert!(result.contains("target /mnt/backup-22tb"));
        assert!(result.contains("4w 2m"));
        assert!(result.contains("# Generated by btrdasd setup"));
    }

    #[test]
    fn render_systemd_service_test() {
        let config = test_config();
        let result = render_systemd_service(&config, false);
        assert!(result.contains("ExecStart=/usr/local/lib/das-backup/backup-run.sh"));
        assert!(!result.contains("/hddRaid1/"));
        assert!(result.contains("# Generated by btrdasd setup"));
    }

    #[test]
    fn render_systemd_timer_test() {
        let config = test_config();
        let result = render_systemd_timer(&config, false);
        assert!(result.contains("OnCalendar=*-*-* 03:00:00"));
        assert!(result.contains("RandomizedDelaySec=1800"));
    }

    #[test]
    fn render_cron_entry_test() {
        let config = test_config();
        let result = render_cron_entry(&config);
        assert!(result.contains("0 3 * * *"));
        assert!(result.contains("/usr/local/lib/das-backup/backup-run.sh"));
    }

    #[test]
    fn render_backup_run_test() {
        let config = test_config();
        let result = render_backup_run(&config);
        assert!(result.starts_with("#!/usr/bin/env bash"));
        assert!(result.contains("ZXA0LMAE"));
        assert!(result.contains("/.btrfs-nvme"));
        assert!(result.contains("/mnt/backup-22tb"));
        assert!(result.contains("# Generated by btrdasd setup"));
    }

    #[test]
    fn generated_files_manifest() {
        let config = test_config();
        let generated = GeneratedFiles::generate(&config);
        let manifest = generated.manifest_content();
        assert!(manifest.contains("/etc/das-backup/btrbk.conf"));
        assert!(manifest.contains("backup-run.sh"));
        assert!(manifest.contains("das-backup.service"));
        assert!(manifest.contains("das-backup.timer"));
    }
}
```

**Step 4: Add `pub mod templates;` to `indexer/src/setup/mod.rs`**

**Step 5: Run tests to verify they pass**

Run: `cd /hddRaid1/ClaudeCodeProjects/DAS-Backup-Manager/indexer && cargo test setup::templates::tests -- --nocapture 2>&1`
Expected: 6 tests PASS.

**Step 6: Commit**

```bash
git add indexer/src/setup/templates.rs indexer/src/setup/mod.rs
git commit -m "feat(setup): template engine for btrbk.conf, systemd, cron, and backup script"
```

---

### Task 5: Interactive wizard module

**Files:**
- Create: `indexer/src/setup/wizard.rs`
- Modify: `indexer/src/setup/mod.rs` (add `pub mod wizard;`)

**Step 1: Create wizard module**

This module uses `dialoguer` for interactive prompts. It takes `SystemInfo` and optionally an existing `Config` (for `--modify`) and returns a completed `Config`.

Create `indexer/src/setup/wizard.rs`:
```rust
use console::style;
use dialoguer::{Confirm, Input, MultiSelect, Select};

use crate::setup::config::*;
use crate::setup::detect::*;

pub fn run_wizard(sys: &SystemInfo, existing: Option<Config>) -> Result<Config, Box<dyn std::error::Error>> {
    let mut config = existing.unwrap_or_default();

    println!("\n{}", style("═══ ButteredDASD Setup Wizard ═══").bold().cyan());
    println!("{}\n", style("Configure backup sources, targets, and scheduling.").dim());

    // Step 1: Dependencies
    step_dependencies(sys)?;

    // Step 2: Subvolumes
    step_subvolumes(sys, &mut config)?;

    // Step 3: Targets
    step_targets(sys, &mut config)?;

    // Step 4: ESP
    step_esp(sys, &mut config)?;

    // Step 5: Retention
    step_retention(&mut config)?;

    // Step 6: Scheduling
    step_scheduling(sys, &mut config)?;

    // Step 7: Email
    step_email(&mut config)?;

    // Step 8: Install location
    step_install_location(&mut config)?;

    // Step 9: GUI
    step_gui(&mut config)?;

    // Step 10: Review
    step_review(&config)?;

    Ok(config)
}

fn step_dependencies(sys: &SystemInfo) -> Result<(), Box<dyn std::error::Error>> {
    println!("{}", style("Step 1: Dependencies").bold().underlined());
    println!();

    let missing: Vec<&DepStatus> = sys.deps.iter().filter(|d| d.path.is_none()).collect();
    let installed: Vec<&DepStatus> = sys.deps.iter().filter(|d| d.path.is_some()).collect();

    for dep in &installed {
        println!("  {} {} ({})", style("✓").green(), dep.name,
            dep.path.as_deref().unwrap_or("?"));
    }
    for dep in &missing {
        let marker = if dep.required {
            style("✗").red()
        } else {
            style("○").yellow()
        };
        println!("  {} {} (not found)", marker, dep.name);
    }

    let required_missing: Vec<&&DepStatus> = missing.iter().filter(|d| d.required).collect();
    if !required_missing.is_empty() {
        println!();
        let pkg_names: Vec<&str> = required_missing.iter().map(|d| d.name.as_str()).collect();
        let install_cmd = sys.package_manager.install_cmd(&pkg_names);

        let choices = vec![
            "Install all now (single sudo)",
            "Install one at a time",
            "Skip (print commands for later)",
        ];
        let selection = Select::new()
            .with_prompt("Missing required dependencies")
            .items(&choices)
            .default(0)
            .interact()?;

        match selection {
            0 => {
                println!("Running: sudo {}", install_cmd);
                let status = std::process::Command::new("sudo")
                    .args(install_cmd.split_whitespace())
                    .status()?;
                if !status.success() {
                    eprintln!("Some dependencies failed to install. Continue anyway.");
                }
            }
            1 => {
                for dep in &required_missing {
                    let cmd = sys.package_manager.install_cmd(&[dep.name.as_str()]);
                    if Confirm::new()
                        .with_prompt(format!("Install {}?", dep.name))
                        .default(true)
                        .interact()?
                    {
                        let _ = std::process::Command::new("sudo")
                            .args(cmd.split_whitespace())
                            .status();
                    }
                }
            }
            _ => {
                println!("\nInstall later with:");
                println!("  sudo {}", install_cmd);
            }
        }
    }

    println!();
    Ok(())
}

fn step_subvolumes(sys: &SystemInfo, config: &mut Config) -> Result<(), Box<dyn std::error::Error>> {
    println!("{}", style("Step 2: Select subvolumes to back up").bold().underlined());
    println!();

    if sys.subvolumes.is_empty() {
        println!("  No BTRFS subvolumes detected. You'll need to add sources manually.");
        let volume: String = Input::new()
            .with_prompt("Source volume mount point")
            .interact_text()?;
        let subvol: String = Input::new()
            .with_prompt("Subvolume name")
            .interact_text()?;
        let device: String = Input::new()
            .with_prompt("Device path")
            .interact_text()?;
        let label: String = Input::new()
            .with_prompt("Label for this source")
            .default("manual".to_string())
            .interact_text()?;

        config.sources.push(Source {
            label,
            volume,
            subvolumes: vec![subvol],
            device,
        });
    } else {
        // Build display list with Select All / Deselect All
        let mut items: Vec<String> = vec![
            "── Select All ──".to_string(),
            "── Deselect All ──".to_string(),
        ];
        for sv in &sys.subvolumes {
            items.push(format!("{} (ID {})", sv.name, sv.id));
        }

        // Default: all subvolumes selected
        let defaults: Vec<bool> = items.iter().enumerate().map(|(i, _)| i >= 2).collect();

        let selected = MultiSelect::new()
            .with_prompt("Select subvolumes")
            .items(&items)
            .defaults(&defaults)
            .interact()?;

        // Handle select/deselect all
        let mut chosen_indices: Vec<usize> = if selected.contains(&0) {
            // Select All
            (2..items.len()).collect()
        } else if selected.contains(&1) {
            // Deselect All — empty
            Vec::new()
        } else {
            selected.into_iter().filter(|&i| i >= 2).collect()
        };

        if chosen_indices.is_empty() {
            println!("  Warning: no subvolumes selected.");
        }

        // Group subvolumes by top_level (rough volume grouping)
        // For simplicity, ask for device per top-level group
        let chosen_subvols: Vec<&SubvolumeInfo> = chosen_indices.iter()
            .filter_map(|&i| sys.subvolumes.get(i - 2))
            .collect();

        if !chosen_subvols.is_empty() {
            let volume: String = Input::new()
                .with_prompt("Source volume mount point (top-level BTRFS)")
                .default("/.btrfs-root".to_string())
                .interact_text()?;
            let device: String = Input::new()
                .with_prompt("Device path for this volume")
                .interact_text()?;
            let label: String = Input::new()
                .with_prompt("Label for this source group")
                .default("system".to_string())
                .interact_text()?;

            config.sources.push(Source {
                label,
                volume,
                subvolumes: chosen_subvols.iter().map(|sv| sv.name.clone()).collect(),
                device,
            });
        }
    }

    println!();
    Ok(())
}

fn step_targets(sys: &SystemInfo, config: &mut Config) -> Result<(), Box<dyn std::error::Error>> {
    println!("{}", style("Step 3: Select backup target devices").bold().underlined());
    println!();

    let usb_devices: Vec<&BlockDevice> = sys.devices.iter().filter(|d| d.is_usb()).collect();

    if !usb_devices.is_empty() {
        println!("  Detected USB/DAS devices:");
        for (i, dev) in usb_devices.iter().enumerate() {
            println!("  {} {} — {} {} (serial: {})",
                style(format!("[{}]", i)).cyan(),
                dev.name, dev.size,
                dev.model.as_deref().unwrap_or("Unknown"),
                dev.serial.as_deref().unwrap_or("none"));
        }
        println!();
    }

    loop {
        if !Confirm::new()
            .with_prompt("Add a backup target?")
            .default(config.targets.is_empty())
            .interact()?
        {
            break;
        }

        let label: String = Input::new()
            .with_prompt("Target label")
            .interact_text()?;
        let serial: String = Input::new()
            .with_prompt("Drive serial number (from smartctl or lsblk)")
            .interact_text()?;
        let mount: String = Input::new()
            .with_prompt("Mount point")
            .default(format!("/mnt/backup-{}", label))
            .interact_text()?;

        let roles = vec!["primary", "mirror", "esp-sync"];
        let role_idx = Select::new()
            .with_prompt("Role")
            .items(&roles)
            .default(0)
            .interact()?;
        let role = match role_idx {
            0 => TargetRole::Primary,
            1 => TargetRole::Mirror,
            _ => TargetRole::EspSync,
        };

        let weekly: u32 = Input::new()
            .with_prompt("Retention: weeks to keep")
            .default(4u32)
            .interact_text()?;
        let monthly: u32 = Input::new()
            .with_prompt("Retention: months to keep")
            .default(2u32)
            .interact_text()?;

        config.targets.push(Target {
            label,
            serial,
            mount,
            role,
            retention: Retention { weekly, monthly },
        });
    }

    println!();
    Ok(())
}

fn step_esp(sys: &SystemInfo, config: &mut Config) -> Result<(), Box<dyn std::error::Error>> {
    println!("{}", style("Step 4: ESP Configuration").bold().underlined());
    println!();

    let esp_candidates: Vec<&BlockDevice> = sys.devices.iter()
        .filter(|d| d.is_esp_candidate())
        .collect();

    if esp_candidates.is_empty() {
        println!("  No ESP partitions detected.");
        config.esp.enabled = false;
    } else {
        println!("  Detected ESP partitions:");
        for dev in &esp_candidates {
            println!("    /dev/{} ({})", dev.name, dev.size);
        }

        config.esp.enabled = Confirm::new()
            .with_prompt("Back up ESP partitions?")
            .default(true)
            .interact()?;

        if config.esp.enabled {
            config.esp.partitions = esp_candidates.iter()
                .map(|d| format!("/dev/{}", d.name))
                .collect();

            if esp_candidates.len() > 1 {
                config.esp.mirror = Confirm::new()
                    .with_prompt("Mirror ESPs to each other?")
                    .default(true)
                    .interact()?;
            }

            config.esp.hooks.enabled = Confirm::new()
                .with_prompt("Auto-sync ESP after kernel/bootloader updates?")
                .default(true)
                .interact()?;

            if config.esp.hooks.enabled {
                config.esp.hooks.hook_type = match &sys.package_manager {
                    PackageManager::Pacman => HookType::Pacman,
                    PackageManager::Apt => HookType::Apt,
                    PackageManager::Dnf => HookType::Dnf,
                    _ => HookType::None,
                };
            }
        }
    }

    println!();
    Ok(())
}

fn step_retention(config: &mut Config) -> Result<(), Box<dyn std::error::Error>> {
    println!("{}", style("Step 5: Retention Policy").bold().underlined());
    println!();

    println!("  Current defaults: source=2 daily, target=4 weekly + 2 monthly");

    if Confirm::new()
        .with_prompt("Customize retention? (No = keep defaults)")
        .default(false)
        .interact()?
    {
        for target in &mut config.targets {
            println!("  Target: {}", target.label);
            target.retention.weekly = Input::new()
                .with_prompt("  Weeks to keep")
                .default(target.retention.weekly)
                .interact_text()?;
            target.retention.monthly = Input::new()
                .with_prompt("  Months to keep")
                .default(target.retention.monthly)
                .interact_text()?;
        }
    }

    println!();
    Ok(())
}

fn step_scheduling(sys: &SystemInfo, config: &mut Config) -> Result<(), Box<dyn std::error::Error>> {
    println!("{}", style("Step 6: Scheduling").bold().underlined());
    println!();

    // Set init system from detection
    config.init.system = match sys.init_system {
        InitSystemDetected::Systemd => InitSystem::Systemd,
        InitSystemDetected::Openrc => InitSystem::Openrc,
        InitSystemDetected::Sysvinit => InitSystem::Sysvinit,
    };

    println!("  Detected init system: {:?}", config.init.system);
    println!("  Default: incremental daily at 03:00, full weekly Sunday 04:00");

    if Confirm::new()
        .with_prompt("Customize schedule?")
        .default(false)
        .interact()?
    {
        config.schedule.incremental = Input::new()
            .with_prompt("Daily incremental time (HH:MM)")
            .default(config.schedule.incremental.clone())
            .interact_text()?;
        config.schedule.full = Input::new()
            .with_prompt("Weekly full schedule (e.g. 'Sun 04:00')")
            .default(config.schedule.full.clone())
            .interact_text()?;
    }

    println!();
    Ok(())
}

fn step_email(config: &mut Config) -> Result<(), Box<dyn std::error::Error>> {
    println!("{}", style("Step 7: Email Reports (optional)").bold().underlined());
    println!();

    config.email.enabled = Confirm::new()
        .with_prompt("Enable email backup reports?")
        .default(false)
        .interact()?;

    if config.email.enabled {
        config.email.smtp_host = Input::new()
            .with_prompt("SMTP host")
            .default(config.email.smtp_host.clone())
            .interact_text()?;
        config.email.smtp_port = Input::new()
            .with_prompt("SMTP port")
            .default(config.email.smtp_port)
            .interact_text()?;
        config.email.from = Input::new()
            .with_prompt("From address")
            .interact_text()?;
        config.email.to = Input::new()
            .with_prompt("To address")
            .interact_text()?;

        let auth_choices = vec!["plain", "starttls", "none"];
        let auth_idx = Select::new()
            .with_prompt("Authentication method")
            .items(&auth_choices)
            .default(0)
            .interact()?;
        config.email.auth = match auth_idx {
            0 => AuthMethod::Plain,
            1 => AuthMethod::Starttls,
            _ => AuthMethod::None,
        };
    }

    println!();
    Ok(())
}

fn step_install_location(config: &mut Config) -> Result<(), Box<dyn std::error::Error>> {
    println!("{}", style("Step 8: Install Locations").bold().underlined());
    println!();

    println!("  Binary:   {}/bin/btrdasd", config.general.install_prefix);
    println!("  Scripts:  {}/lib/das-backup/", config.general.install_prefix);
    println!("  Config:   /etc/das-backup/");
    println!("  Database: {}", config.general.db_path);

    if Confirm::new()
        .with_prompt("Customize install paths?")
        .default(false)
        .interact()?
    {
        config.general.install_prefix = Input::new()
            .with_prompt("Install prefix")
            .default(config.general.install_prefix.clone())
            .interact_text()?;
        config.general.db_path = Input::new()
            .with_prompt("Database path")
            .default(config.general.db_path.clone())
            .interact_text()?;
    }

    println!();
    Ok(())
}

fn step_gui(config: &mut Config) -> Result<(), Box<dyn std::error::Error>> {
    println!("{}", style("Step 9: GUI (optional)").bold().underlined());
    println!();

    config.gui.enabled = Confirm::new()
        .with_prompt("Install KDE Plasma GUI? (requires Qt6/KF6)")
        .default(false)
        .interact()?;

    println!();
    Ok(())
}

fn step_review(config: &Config) -> Result<(), Box<dyn std::error::Error>> {
    println!("{}", style("Step 10: Review Configuration").bold().underlined());
    println!();

    println!("  Sources:");
    for src in &config.sources {
        println!("    {} — {} [{}]", src.label, src.volume, src.subvolumes.join(", "));
    }
    println!("  Targets:");
    for tgt in &config.targets {
        println!("    {} — {} (serial: {}, role: {:?})", tgt.label, tgt.mount, tgt.serial, tgt.role);
    }
    println!("  ESP: {}{}", if config.esp.enabled { "enabled" } else { "disabled" },
        if config.esp.mirror { " (mirrored)" } else { "" });
    println!("  Schedule: incremental={}, full={}", config.schedule.incremental, config.schedule.full);
    println!("  Init: {:?}", config.init.system);
    println!("  Email: {}", if config.email.enabled { &config.email.to } else { "disabled" });
    println!("  GUI: {}", if config.gui.enabled { "yes" } else { "no" });
    println!();

    let errors = config.validate();
    if !errors.is_empty() {
        println!("  {}", style("Validation warnings:").yellow());
        for err in &errors {
            println!("    {} {}", style("⚠").yellow(), err);
        }
        println!();
    }

    if !Confirm::new()
        .with_prompt("Proceed with installation?")
        .default(true)
        .interact()?
    {
        return Err("Setup cancelled by user.".into());
    }

    Ok(())
}
```

**Step 2: Add `pub mod wizard;` to `indexer/src/setup/mod.rs`**

**Step 3: Build to verify compilation**

Run: `cd /hddRaid1/ClaudeCodeProjects/DAS-Backup-Manager/indexer && cargo build 2>&1`
Expected: Compiles. (Wizard is interactive — no unit tests for this module, tested via manual `btrdasd setup` run.)

**Step 4: Commit**

```bash
git add indexer/src/setup/wizard.rs indexer/src/setup/mod.rs
git commit -m "feat(setup): interactive wizard with 10-step dialoguer flow"
```

---

### Task 6: Install, uninstall, upgrade, and check modes

**Files:**
- Create: `indexer/src/setup/installer.rs`
- Modify: `indexer/src/setup/mod.rs` (add `pub mod installer;`, wire into `run()`)

**Step 1: Write the failing test**

Add tests to new `indexer/src/setup/installer.rs`:
```rust
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
            subvolumes: vec!["@".to_string()],
            device: "/dev/sda".to_string(),
        });
        config.targets.push(Target {
            label: "tgt".to_string(),
            serial: "ABC123".to_string(),
            mount: "/mnt/tgt".to_string(),
            role: TargetRole::Primary,
            retention: Retention { weekly: 4, monthly: 2 },
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
    fn uninstall_removes_manifest_files() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();

        // Create some files to "uninstall"
        let file1 = base.join("test1.txt");
        let file2 = base.join("test2.txt");
        std::fs::write(&file1, "content").unwrap();
        std::fs::write(&file2, "content").unwrap();

        let manifest = base.join(".manifest");
        std::fs::write(&manifest, format!("{}\n{}", file1.display(), file2.display())).unwrap();

        let removed = uninstall_from_manifest(&manifest);
        assert_eq!(removed, 2);
        assert!(!file1.exists());
        assert!(!file2.exists());
    }
}
```

**Step 2: Run test to verify it fails**

Run: `cd /hddRaid1/ClaudeCodeProjects/DAS-Backup-Manager/indexer && cargo test setup::installer::tests -- --nocapture 2>&1`
Expected: FAIL.

**Step 3: Write minimal implementation**

Create `indexer/src/setup/installer.rs`:
```rust
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use crate::setup::config::Config;
use crate::setup::templates::GeneratedFiles;

const CONFIG_DIR: &str = "/etc/das-backup";
const CONFIG_FILE: &str = "/etc/das-backup/config.toml";
const MANIFEST_FILE: &str = "/etc/das-backup/.manifest";

pub fn install(config: &Config) -> Result<(), Box<dyn std::error::Error>> {
    let config_path = PathBuf::from(CONFIG_FILE);
    let manifest_path = PathBuf::from(MANIFEST_FILE);
    let root = PathBuf::from("/");
    install_to_prefix(config, &root, &config_path, &manifest_path)
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

        // Create parent directory
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

    // Enable systemd timers if applicable
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

    // Create DB directory
    if let Some(parent) = Path::new(&config.general.db_path).parent() {
        std::fs::create_dir_all(parent)?;
    }

    println!("Installation complete.");
    println!("Config: {}", config_path.display());
    println!("Manifest: {} ({} files)", manifest_path.display(), manifest_entries.len());
    Ok(())
}

pub fn uninstall(remove_db: bool) -> Result<(), Box<dyn std::error::Error>> {
    let manifest_path = PathBuf::from(MANIFEST_FILE);
    if !manifest_path.exists() {
        eprintln!("No manifest found at {}. Nothing to uninstall.", manifest_path.display());
        return Ok(());
    }

    // Load config for db path before removing
    let db_path = Config::load(&PathBuf::from(CONFIG_FILE))
        .ok()
        .map(|c| c.general.db_path);

    // Disable timers first
    let _ = std::process::Command::new("systemctl")
        .args(["disable", "--now", "das-backup.timer"])
        .status();
    let _ = std::process::Command::new("systemctl")
        .args(["disable", "--now", "das-backup-full.timer"])
        .status();

    let removed = uninstall_from_manifest(&manifest_path);
    println!("Removed {} files.", removed);

    // Remove manifest itself
    let _ = std::fs::remove_file(&manifest_path);

    // Remove config dir if empty
    let _ = std::fs::remove_dir(CONFIG_DIR);

    // Optionally remove database
    if remove_db {
        if let Some(db) = db_path {
            if Path::new(&db).exists() {
                std::fs::remove_file(&db)?;
                println!("Removed database: {}", db);
            }
        }
    }

    let _ = std::process::Command::new("systemctl")
        .args(["daemon-reload"])
        .status();

    println!("Uninstall complete.");
    Ok(())
}

pub fn uninstall_from_manifest(manifest_path: &Path) -> usize {
    let content = match std::fs::read_to_string(manifest_path) {
        Ok(c) => c,
        Err(_) => return 0,
    };

    let mut removed = 0;
    for line in content.lines() {
        let path = Path::new(line.trim());
        if path.exists() {
            if std::fs::remove_file(path).is_ok() {
                removed += 1;
            }
        }
    }
    removed
}

pub fn upgrade() -> Result<(), Box<dyn std::error::Error>> {
    let config_path = PathBuf::from(CONFIG_FILE);
    if !config_path.exists() {
        return Err(format!("No config found at {}. Run 'btrdasd setup' first.", config_path.display()).into());
    }

    let config = Config::load(&config_path)?;
    println!("Regenerating files from {}...", config_path.display());
    install(&config)?;
    println!("Upgrade complete.");
    Ok(())
}

pub fn check() -> Result<(), Box<dyn std::error::Error>> {
    let config_path = PathBuf::from(CONFIG_FILE);

    // Check config exists
    if !config_path.exists() {
        println!("✗ Config not found at {}", config_path.display());
        println!("  Run: sudo btrdasd setup");
        return Ok(());
    }
    println!("✓ Config found: {}", config_path.display());

    // Load and validate
    let config = Config::load(&config_path)?;
    let errors = config.validate();
    if errors.is_empty() {
        println!("✓ Config is valid");
    } else {
        for err in &errors {
            println!("✗ {}", err);
        }
    }

    // Check manifest
    let manifest_path = PathBuf::from(MANIFEST_FILE);
    if manifest_path.exists() {
        let content = std::fs::read_to_string(&manifest_path)?;
        let total = content.lines().count();
        let missing: Vec<&str> = content.lines()
            .filter(|line| !Path::new(line.trim()).exists())
            .collect();
        if missing.is_empty() {
            println!("✓ All {} generated files present", total);
        } else {
            println!("✗ {} of {} generated files missing:", missing.len(), total);
            for m in &missing {
                println!("    {}", m);
            }
            println!("  Fix with: sudo btrdasd setup --upgrade");
        }
    } else {
        println!("✗ No manifest found. Files may be from a manual install.");
    }

    // Check dependencies
    let deps = crate::setup::detect::check_dependencies(
        config.email.enabled,
        config.esp.mirror,
    );
    for dep in &deps {
        if let Some(path) = &dep.path {
            println!("✓ {} ({})", dep.name, path);
        } else if dep.required {
            println!("✗ {} (required, not found)", dep.name);
        } else {
            println!("○ {} (optional, not found)", dep.name);
        }
    }

    Ok(())
}

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
            subvolumes: vec!["@".to_string()],
            device: "/dev/sda".to_string(),
        });
        config.targets.push(Target {
            label: "tgt".to_string(),
            serial: "ABC123".to_string(),
            mount: "/mnt/tgt".to_string(),
            role: TargetRole::Primary,
            retention: Retention { weekly: 4, monthly: 2 },
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
    fn uninstall_removes_manifest_files() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();

        let file1 = base.join("test1.txt");
        let file2 = base.join("test2.txt");
        std::fs::write(&file1, "content").unwrap();
        std::fs::write(&file2, "content").unwrap();

        let manifest = base.join(".manifest");
        std::fs::write(&manifest, format!("{}\n{}", file1.display(), file2.display())).unwrap();

        let removed = uninstall_from_manifest(&manifest);
        assert_eq!(removed, 2);
        assert!(!file1.exists());
        assert!(!file2.exists());
    }
}
```

**Step 4: Add `pub mod installer;` to `indexer/src/setup/mod.rs` and wire the modes**

Update the `run()` function in `indexer/src/setup/mod.rs`:
```rust
pub mod config;
pub mod detect;
pub mod installer;
pub mod templates;
pub mod wizard;

use clap::Args;

#[derive(Args)]
pub struct SetupArgs {
    #[arg(long)]
    pub modify: bool,
    #[arg(long)]
    pub upgrade: bool,
    #[arg(long)]
    pub uninstall: bool,
    #[arg(long)]
    pub check: bool,
}

pub fn run(args: SetupArgs) -> Result<(), Box<dyn std::error::Error>> {
    if !nix_is_root() {
        eprintln!("Error: btrdasd setup requires root privileges.");
        eprintln!("Run: sudo btrdasd setup");
        std::process::exit(1);
    }

    if args.check {
        installer::check()?;
    } else if args.uninstall {
        let remove_db = dialoguer::Confirm::new()
            .with_prompt("Also remove the backup database?")
            .default(false)
            .interact()?;
        installer::uninstall(remove_db)?;
    } else if args.upgrade {
        installer::upgrade()?;
    } else {
        // Fresh install or --modify
        let existing = if args.modify {
            config::Config::load(&std::path::PathBuf::from("/etc/das-backup/config.toml")).ok()
        } else {
            None
        };

        let sys = detect::SystemInfo::detect();
        let config = wizard::run_wizard(&sys, existing)?;
        installer::install(&config)?;
    }

    Ok(())
}

fn nix_is_root() -> bool {
    unsafe { libc::geteuid() == 0 }
}
```

**Step 5: Run tests**

Run: `cd /hddRaid1/ClaudeCodeProjects/DAS-Backup-Manager/indexer && cargo test setup:: -- --nocapture 2>&1`
Expected: All tests PASS (config: 4, detect: 4, templates: 6, installer: 2 = 16 total).

**Step 6: Commit**

```bash
git add indexer/src/setup/installer.rs indexer/src/setup/mod.rs
git commit -m "feat(setup): install, uninstall, upgrade, and check modes"
```

---

### Task 7: CMake integration — BUILD_GUI option and Rust build

**Files:**
- Modify: `CMakeLists.txt` (root)

**Step 1: Read the current CMakeLists.txt**

Read: `CMakeLists.txt` (root, already read above)

**Step 2: Update root CMakeLists.txt**

Replace the entire file with:
```cmake
cmake_minimum_required(VERSION 3.25)
project(DAS-Backup-Manager
    VERSION 0.4.0
    DESCRIPTION "DAS backup manager with btrbk integration, SQLite FTS5 content indexing, and KDE Plasma GUI"
    LANGUAGES CXX
)

set(CMAKE_CXX_STANDARD 20)
set(CMAKE_CXX_STANDARD_REQUIRED ON)

# =============================================================================
# Build options
# =============================================================================

option(BUILD_GUI "Build KDE Plasma GUI (requires Qt6/KF6)" ON)
option(BUILD_INDEXER "Build btrdasd Rust binary via cargo" ON)

# =============================================================================
# Rust indexer (btrdasd binary)
# =============================================================================

if(BUILD_INDEXER)
    include(ExternalProject)

    set(CARGO_BUILD_TYPE "$<IF:$<CONFIG:Debug>,debug,release>")
    set(CARGO_TARGET_DIR "${CMAKE_BINARY_DIR}/cargo-target")

    ExternalProject_Add(btrdasd_rust
        SOURCE_DIR "${CMAKE_SOURCE_DIR}/indexer"
        CONFIGURE_COMMAND ""
        BUILD_COMMAND cargo build --release --manifest-path "${CMAKE_SOURCE_DIR}/indexer/Cargo.toml" --target-dir "${CARGO_TARGET_DIR}"
        INSTALL_COMMAND ""
        BUILD_IN_SOURCE FALSE
        BUILD_ALWAYS TRUE
    )

    install(PROGRAMS "${CARGO_TARGET_DIR}/release/btrdasd"
        DESTINATION "${CMAKE_INSTALL_PREFIX}/bin")
endif()

# =============================================================================
# Install targets for reference scripts and config
# =============================================================================

set(DAS_SCRIPT_DIR "${CMAKE_INSTALL_PREFIX}/lib/das-backup")

install(PROGRAMS
    scripts/backup-run.sh
    scripts/backup-verify.sh
    scripts/boot-archive-cleanup.sh
    scripts/das-partition-drives.sh
    scripts/install-backup-timer.sh
    DESTINATION "${DAS_SCRIPT_DIR}"
)

install(FILES
    config/btrbk.conf
    config/das-backup-email.conf.example
    DESTINATION "${DAS_SCRIPT_DIR}/config"
)

# =============================================================================
# KDE Plasma GUI (optional)
# =============================================================================

if(BUILD_GUI)
    add_subdirectory(gui)
endif()
```

**Step 3: Build and verify**

Run: `cd /hddRaid1/ClaudeCodeProjects/DAS-Backup-Manager && cmake -B build -DCMAKE_BUILD_TYPE=RelWithDebInfo -DBUILD_TESTING=ON 2>&1`
Expected: CMake configures successfully.

Run: `cmake --build build 2>&1`
Expected: Both btrdasd (Rust) and btrdasd-gui (C++) build.

Run: `cd build && ctest --test-dir gui --output-on-failure 2>&1`
Expected: 5/5 GUI tests pass.

**Step 4: Verify GUI-disabled build**

Run: `cmake -B build-cli -DBUILD_GUI=OFF -DCMAKE_BUILD_TYPE=Release 2>&1`
Expected: Configures without Qt6/KF6 requirements.

**Step 5: Commit**

```bash
git add CMakeLists.txt
git commit -m "feat: BUILD_GUI/BUILD_INDEXER CMake options with Rust ExternalProject"
```

---

### Task 8: Dockerfile for cross-platform CLI

**Files:**
- Create: `Dockerfile`

**Step 1: Create Dockerfile**

```dockerfile
# Stage 1: Build btrdasd
FROM rust:1.85-bookworm AS builder

WORKDIR /src
COPY indexer/ indexer/
RUN cargo build --release --manifest-path indexer/Cargo.toml

# Stage 2: Runtime
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    btrfs-progs \
    smartmontools \
    bash \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /src/target/release/btrdasd /usr/local/bin/btrdasd

ENTRYPOINT ["btrdasd"]
```

**Step 2: Verify Docker build**

Run: `cd /hddRaid1/ClaudeCodeProjects/DAS-Backup-Manager && docker build -t das-backup-manager:latest . 2>&1`
Expected: Build succeeds.

Run: `docker run --rm das-backup-manager:latest --version`
Expected: Prints version.

**Step 3: Commit**

```bash
git add Dockerfile
git commit -m "feat: Dockerfile for headless btrdasd CLI"
```

---

### Task 9: Run full test suite and final verification

**Files:** None new — verification only.

**Step 1: Run all Rust tests**

Run: `cd /hddRaid1/ClaudeCodeProjects/DAS-Backup-Manager/indexer && cargo test 2>&1`
Expected: All tests pass (existing indexer tests + new setup tests: config, detect, templates, installer).

**Step 2: Run cargo clippy**

Run: `cd /hddRaid1/ClaudeCodeProjects/DAS-Backup-Manager/indexer && cargo clippy -- -W clippy::all 2>&1`
Expected: No warnings.

**Step 3: Run cargo fmt check**

Run: `cd /hddRaid1/ClaudeCodeProjects/DAS-Backup-Manager/indexer && cargo fmt -- --check 2>&1`
Expected: Already formatted.

**Step 4: Build GUI and run GUI tests**

Run: `cd /hddRaid1/ClaudeCodeProjects/DAS-Backup-Manager && cmake --build build && cd build && ctest --test-dir gui --output-on-failure 2>&1`
Expected: 5/5 GUI tests pass.

**Step 5: Verify btrdasd setup --help**

Run: `./build/cargo-target/release/btrdasd setup --help 2>&1`
Expected: Shows all setup flags.

**Step 6: Commit any fixes, then final commit**

```bash
git add -A
git commit -m "chore: final verification — all tests pass, clippy clean"
```

---

## Summary

| Task | Component | Files | Tests |
|------|-----------|-------|-------|
| 1 | Setup subcommand skeleton | 4 create/modify | Build test |
| 2 | Config types + TOML serde | 1 create | 4 cargo test |
| 3 | System detection | 1 create, 1 modify | 4 cargo test |
| 4 | Template engine | 1 create, 1 modify | 6 cargo test |
| 5 | Interactive wizard | 1 create, 1 modify | Build test (interactive) |
| 6 | Installer modes | 1 create, 1 modify | 2 cargo test |
| 7 | CMake BUILD_GUI + Rust build | 1 modify | Build + existing 5 QTest |
| 8 | Dockerfile | 1 create | Docker build test |
| 9 | Full verification | 0 | All suites |
