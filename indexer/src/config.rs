#![allow(dead_code)]

use serde::{Deserialize, Serialize};
use std::error::Error;
use std::fs;
use std::path::Path;

// ---------------------------------------------------------------------------
// Top-level config
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub general: General,
    pub init: Init,
    pub schedule: Schedule,
    #[serde(default)]
    pub das: Das,
    #[serde(default)]
    pub boot: Boot,
    #[serde(default, rename = "source")]
    pub sources: Vec<Source>,
    #[serde(default, rename = "target")]
    pub targets: Vec<Target>,
    pub esp: Esp,
    pub email: Email,
    pub gui: Gui,
}

// ---------------------------------------------------------------------------
// Section structs
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct General {
    pub version: String,
    pub install_prefix: String,
    pub db_path: String,
    #[serde(default = "default_log_file")]
    pub log_file: String,
    #[serde(default = "default_growth_log")]
    pub growth_log: String,
    #[serde(default = "default_last_report")]
    pub last_report: String,
    #[serde(default = "default_btrbk_conf")]
    pub btrbk_conf: String,
}

fn default_log_file() -> String {
    "/var/log/das-backup.log".into()
}
fn default_growth_log() -> String {
    "/var/lib/das-backup/growth.log".into()
}
fn default_last_report() -> String {
    "/var/lib/das-backup/last-report.txt".into()
}
fn default_btrbk_conf() -> String {
    "/etc/das-backup/btrbk.conf".into()
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
pub struct Das {
    #[serde(default = "default_model_pattern")]
    pub model_pattern: String,
    #[serde(default = "default_io_scheduler")]
    pub io_scheduler: String,
    #[serde(default)]
    pub mount_opts: String,
}

impl Default for Das {
    fn default() -> Self {
        Self {
            model_pattern: default_model_pattern(),
            io_scheduler: default_io_scheduler(),
            mount_opts: String::new(),
        }
    }
}

fn default_model_pattern() -> String {
    "TDAS".into()
}
fn default_io_scheduler() -> String {
    "mq-deadline".into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Boot {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_boot_subvolumes")]
    pub subvolumes: Vec<String>,
    #[serde(default = "default_archive_retention_days")]
    pub archive_retention_days: u32,
}

fn default_true() -> bool {
    true
}
fn default_boot_subvolumes() -> Vec<String> {
    vec!["@".into(), "@home".into()]
}
fn default_archive_retention_days() -> u32 {
    365
}

impl Default for Boot {
    fn default() -> Self {
        Self {
            enabled: true,
            subvolumes: default_boot_subvolumes(),
            archive_retention_days: 365,
        }
    }
}

/// A subvolume within a source, with optional scheduling flags.
/// Accepts both bare strings ("@") and full structs ({name = "@", manual_only = true})
/// in TOML for backward compatibility.
#[derive(Debug, Clone, Serialize)]
pub struct SubvolConfig {
    pub name: String,
    #[serde(default)]
    pub manual_only: bool,
}

impl<'de> Deserialize<'de> for SubvolConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum SubvolEntry {
            Simple(String),
            Full {
                name: String,
                #[serde(default)]
                manual_only: bool,
            },
        }

        match SubvolEntry::deserialize(deserializer)? {
            SubvolEntry::Simple(name) => Ok(SubvolConfig {
                name,
                manual_only: false,
            }),
            SubvolEntry::Full { name, manual_only } => Ok(SubvolConfig { name, manual_only }),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Source {
    pub label: String,
    pub volume: String,
    pub subvolumes: Vec<SubvolConfig>,
    pub device: String,
    #[serde(default = "default_snapshot_dir")]
    pub snapshot_dir: String,
    #[serde(default)]
    pub target_subdirs: Vec<String>,
}

fn default_snapshot_dir() -> String {
    ".btrbk-snapshots".into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Target {
    pub label: String,
    pub serial: String,
    pub mount: String,
    pub role: TargetRole,
    pub retention: Retention,
    #[serde(default)]
    pub display_name: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TargetRole {
    Primary,
    Mirror,
    EspSync,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Retention {
    #[serde(default)]
    pub weekly: u32,
    #[serde(default)]
    pub monthly: u32,
    #[serde(default)]
    pub daily: u32,
    #[serde(default)]
    pub yearly: u32,
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

// ---------------------------------------------------------------------------
// Default impl for Config
// ---------------------------------------------------------------------------

impl Default for Config {
    fn default() -> Self {
        Self {
            general: General {
                version: env!("CARGO_PKG_VERSION").to_string(),
                install_prefix: "/usr/local".into(),
                db_path: "/var/lib/das-backup/backup-index.db".into(),
                log_file: default_log_file(),
                growth_log: default_growth_log(),
                last_report: default_last_report(),
                btrbk_conf: default_btrbk_conf(),
            },
            init: Init {
                system: InitSystem::Systemd,
            },
            schedule: Schedule {
                incremental: "03:00".into(),
                full: "Sun 04:00".into(),
                randomized_delay_min: 30,
            },
            das: Das::default(),
            boot: Boot::default(),
            sources: Vec::new(),
            targets: Vec::new(),
            esp: Esp::default(),
            email: Email::default(),
            gui: Gui::default(),
        }
    }
}

// ---------------------------------------------------------------------------
// Config methods
// ---------------------------------------------------------------------------

impl Config {
    /// Serialize this config to a pretty-printed TOML string.
    pub fn to_toml(&self) -> Result<String, toml::ser::Error> {
        toml::to_string_pretty(self)
    }

    /// Deserialize a config from a TOML string.
    pub fn from_toml(s: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(s)
    }

    /// Load a config from a TOML file on disk.
    pub fn load(path: &Path) -> Result<Self, Box<dyn Error>> {
        let contents = fs::read_to_string(path)?;
        let cfg = Self::from_toml(&contents)?;
        Ok(cfg)
    }

    /// Save this config to a TOML file, creating parent directories as needed.
    pub fn save(&self, path: &Path) -> Result<(), Box<dyn Error>> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let header = "# Generated by btrdasd setup — do not edit.\n\
                       # Modify this file and run: sudo btrdasd setup --upgrade\n\n";
        let body = self.to_toml()?;
        fs::write(path, format!("{header}{body}"))?;
        Ok(())
    }

    /// Validate the config and return a list of human-readable error messages.
    /// An empty vec means the config is valid.
    pub fn validate(&self) -> Vec<String> {
        let mut errors = Vec::new();

        if self.sources.is_empty() {
            errors.push("No backup sources defined — add at least one [[source]]".into());
        }

        if self.targets.is_empty() {
            errors.push("No backup targets defined — add at least one [[target]]".into());
        }

        for (i, src) in self.sources.iter().enumerate() {
            if src.subvolumes.is_empty() {
                errors.push(format!(
                    "Source '{}' (index {i}) has no subvolumes",
                    src.label
                ));
            }
            if src.device.is_empty() {
                errors.push(format!(
                    "Source '{}' (index {i}) has an empty device path",
                    src.label
                ));
            }
        }

        for (i, tgt) in self.targets.iter().enumerate() {
            if tgt.serial.is_empty() {
                errors.push(format!(
                    "Target '{}' (index {i}) has an empty serial number",
                    tgt.label
                ));
            }
        }

        if self.email.enabled && self.email.smtp_host.is_empty() {
            errors.push("Email is enabled but smtp_host is empty".into());
        }

        if self.esp.mirror && self.esp.partitions.len() < 2 {
            errors.push("ESP mirror is enabled but fewer than 2 partitions are configured".into());
        }

        errors
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_default_config() {
        let cfg = Config::default();
        let toml_str = cfg.to_toml().expect("serialize default config");
        let parsed: Config = Config::from_toml(&toml_str).expect("deserialize default config");
        assert_eq!(parsed.general.version, env!("CARGO_PKG_VERSION"));
        assert_eq!(parsed.init.system, InitSystem::Systemd);
        assert_eq!(parsed.schedule.incremental, "03:00");
        assert_eq!(parsed.schedule.full, "Sun 04:00");
        assert_eq!(parsed.schedule.randomized_delay_min, 30);
        // New fields have defaults
        assert_eq!(parsed.general.log_file, "/var/log/das-backup.log");
        assert_eq!(parsed.general.btrbk_conf, "/etc/das-backup/btrbk.conf");
        assert_eq!(parsed.das.model_pattern, "TDAS");
        assert_eq!(parsed.das.io_scheduler, "mq-deadline");
        assert!(parsed.boot.enabled);
        assert_eq!(parsed.boot.subvolumes, vec!["@", "@home"]);
        assert_eq!(parsed.boot.archive_retention_days, 365);
    }

    #[test]
    fn roundtrip_full_config() {
        let mut cfg = Config::default();
        cfg.das.model_pattern = "MyDAS".into();
        cfg.das.io_scheduler = "none".into();
        cfg.das.mount_opts = "noatime,compress=zstd".into();
        cfg.boot.archive_retention_days = 180;
        cfg.boot.subvolumes = vec!["@".into(), "@home".into(), "@log".into()];
        cfg.sources.push(Source {
            label: "nvme-root".into(),
            volume: "/.btrfs-nvme".into(),
            subvolumes: vec![
                SubvolConfig {
                    name: "@".into(),
                    manual_only: false,
                },
                SubvolConfig {
                    name: "@home".into(),
                    manual_only: false,
                },
            ],
            device: "/dev/nvme0n1p2".into(),
            snapshot_dir: ".snapshots".into(),
            target_subdirs: vec!["nvme".into()],
        });
        cfg.targets.push(Target {
            label: "primary-22tb".into(),
            serial: "ZXA0LMAE".into(),
            mount: "/mnt/backup-22tb".into(),
            role: TargetRole::Primary,
            retention: Retention {
                weekly: 4,
                monthly: 2,
                daily: 365,
                yearly: 4,
            },
            display_name: "22TB Primary (Bay 2)".into(),
        });
        cfg.esp.enabled = true;
        cfg.esp.mirror = true;
        cfg.esp.partitions = vec!["/dev/nvme0n1p1".into()];
        cfg.esp.mount_points = vec!["/efi".into()];
        cfg.esp.hooks.enabled = true;
        cfg.esp.hooks.hook_type = HookType::Pacman;
        cfg.email.enabled = true;
        cfg.email.smtp_host = "127.0.0.1".into();
        cfg.email.smtp_port = 1025;
        cfg.email.from = "backup@example.com".into();
        cfg.email.to = "user@example.com".into();
        cfg.email.auth = AuthMethod::Plain;

        let toml_str = cfg.to_toml().expect("serialize full config");
        let parsed = Config::from_toml(&toml_str).expect("deserialize full config");

        assert_eq!(parsed.sources.len(), 1);
        assert_eq!(parsed.sources[0].label, "nvme-root");
        assert_eq!(parsed.sources[0].subvolumes.len(), 2);
        assert_eq!(parsed.sources[0].subvolumes[0].name, "@");
        assert_eq!(parsed.sources[0].subvolumes[1].name, "@home");
        assert!(!parsed.sources[0].subvolumes[0].manual_only);
        assert_eq!(parsed.sources[0].snapshot_dir, ".snapshots");
        assert_eq!(parsed.sources[0].target_subdirs, vec!["nvme"]);
        assert_eq!(parsed.targets.len(), 1);
        assert_eq!(parsed.targets[0].serial, "ZXA0LMAE");
        assert_eq!(parsed.targets[0].role, TargetRole::Primary);
        assert_eq!(parsed.targets[0].retention.weekly, 4);
        assert_eq!(parsed.targets[0].retention.daily, 365);
        assert_eq!(parsed.targets[0].retention.yearly, 4);
        assert_eq!(parsed.targets[0].display_name, "22TB Primary (Bay 2)");
        assert_eq!(parsed.das.model_pattern, "MyDAS");
        assert_eq!(parsed.das.io_scheduler, "none");
        assert_eq!(parsed.das.mount_opts, "noatime,compress=zstd");
        assert_eq!(parsed.boot.archive_retention_days, 180);
        assert_eq!(parsed.boot.subvolumes, vec!["@", "@home", "@log"]);
        assert!(parsed.esp.enabled);
        assert!(parsed.esp.mirror);
        assert_eq!(parsed.esp.hooks.hook_type, HookType::Pacman);
        assert!(parsed.email.enabled);
        assert_eq!(parsed.email.smtp_port, 1025);
        assert_eq!(parsed.email.auth, AuthMethod::Plain);
    }

    #[test]
    fn backward_compat_old_config_without_new_fields() {
        // A config.toml from v0.4.0 that lacks das, boot, snapshot_dir, etc.
        let old_toml = r#"
[general]
version = "0.4.0"
install_prefix = "/usr/local"
db_path = "/var/lib/das-backup/backup-index.db"

[init]
system = "systemd"

[schedule]
incremental = "03:00"
full = "Sun 04:00"
randomized_delay_min = 30

[[source]]
label = "nvme"
volume = "/.btrfs-nvme"
subvolumes = ["@", "@home"]
device = "/dev/nvme0n1p2"

[[target]]
label = "primary"
serial = "ABC123"
mount = "/mnt/backup"
role = "primary"

[target.retention]
weekly = 4
monthly = 2

[esp]
enabled = false

[email]
enabled = false

[gui]
enabled = false
"#;
        let cfg = Config::from_toml(old_toml).expect("old config should parse with defaults");
        // New fields get sane defaults
        assert_eq!(cfg.general.log_file, "/var/log/das-backup.log");
        assert_eq!(cfg.general.btrbk_conf, "/etc/das-backup/btrbk.conf");
        assert_eq!(cfg.das.model_pattern, "TDAS");
        assert_eq!(cfg.das.io_scheduler, "mq-deadline");
        assert!(cfg.boot.enabled);
        assert_eq!(cfg.boot.archive_retention_days, 365);
        assert_eq!(cfg.sources[0].snapshot_dir, ".btrbk-snapshots");
        assert!(cfg.sources[0].target_subdirs.is_empty());
        assert_eq!(cfg.targets[0].retention.daily, 0);
        assert_eq!(cfg.targets[0].retention.yearly, 0);
        assert!(cfg.targets[0].display_name.is_empty());
    }

    #[test]
    fn config_validates_no_sources() {
        let cfg = Config::default();
        let errors = cfg.validate();
        assert!(
            errors.iter().any(|e| e.to_lowercase().contains("source")),
            "expected validation error about sources, got: {errors:?}",
        );
    }

    #[test]
    fn config_validates_no_targets() {
        let mut cfg = Config::default();
        cfg.sources.push(Source {
            label: "test".into(),
            volume: "/vol".into(),
            subvolumes: vec![SubvolConfig {
                name: "@".into(),
                manual_only: false,
            }],
            device: "/dev/sda".into(),
            snapshot_dir: ".btrbk-snapshots".into(),
            target_subdirs: vec![],
        });
        let errors = cfg.validate();
        assert!(
            errors.iter().any(|e| e.to_lowercase().contains("target")),
            "expected validation error about targets, got: {errors:?}",
        );
    }

    #[test]
    fn subvol_config_from_string() {
        let toml = r#"
[general]
version = "0.6.0"
install_prefix = "/usr"
db_path = "/tmp/test.db"
[init]
system = "systemd"
[schedule]
incremental = "03:00"
full = "Sun 04:00"
randomized_delay_min = 30
[[source]]
label = "test"
volume = "/vol"
device = "/dev/sda"
subvolumes = ["@", "@home"]
[[target]]
label = "t"
serial = "X"
mount = "/mnt/t"
role = "primary"
[target.retention]
weekly = 4
[esp]
enabled = false
[email]
enabled = false
[gui]
enabled = false
"#;
        let cfg = Config::from_toml(toml).unwrap();
        assert_eq!(cfg.sources[0].subvolumes[0].name, "@");
        assert_eq!(cfg.sources[0].subvolumes[1].name, "@home");
        assert!(!cfg.sources[0].subvolumes[0].manual_only);
        assert!(!cfg.sources[0].subvolumes[1].manual_only);
    }

    #[test]
    fn subvol_config_full_format() {
        let toml = r#"
[general]
version = "0.6.0"
install_prefix = "/usr"
db_path = "/tmp/test.db"
[init]
system = "systemd"
[schedule]
incremental = "03:00"
full = "Sun 04:00"
randomized_delay_min = 30
[[source]]
label = "test"
volume = "/vol"
device = "/dev/sda"
[[source.subvolumes]]
name = "@"
[[source.subvolumes]]
name = "@root"
manual_only = true
[[target]]
label = "t"
serial = "X"
mount = "/mnt/t"
role = "primary"
[target.retention]
weekly = 4
[esp]
enabled = false
[email]
enabled = false
[gui]
enabled = false
"#;
        let cfg = Config::from_toml(toml).unwrap();
        assert_eq!(cfg.sources[0].subvolumes.len(), 2);
        assert_eq!(cfg.sources[0].subvolumes[0].name, "@");
        assert!(!cfg.sources[0].subvolumes[0].manual_only);
        assert_eq!(cfg.sources[0].subvolumes[1].name, "@root");
        assert!(cfg.sources[0].subvolumes[1].manual_only);
    }
}
