// Types are defined here and consumed by the wizard (Task 3+).
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
            },
            init: Init {
                system: InitSystem::Systemd,
            },
            schedule: Schedule {
                incremental: "03:00".into(),
                full: "Sun 04:00".into(),
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
    }

    #[test]
    fn roundtrip_full_config() {
        let mut cfg = Config::default();
        cfg.sources.push(Source {
            label: "nvme-root".into(),
            volume: "/.btrfs-nvme".into(),
            subvolumes: vec!["@".into(), "@home".into()],
            device: "/dev/nvme0n1p2".into(),
        });
        cfg.targets.push(Target {
            label: "primary-22tb".into(),
            serial: "ZXA0LMAE".into(),
            mount: "/mnt/backup-22tb".into(),
            role: TargetRole::Primary,
            retention: Retention {
                weekly: 4,
                monthly: 2,
            },
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
        assert_eq!(parsed.sources[0].subvolumes, vec!["@", "@home"]);
        assert_eq!(parsed.targets.len(), 1);
        assert_eq!(parsed.targets[0].serial, "ZXA0LMAE");
        assert_eq!(parsed.targets[0].role, TargetRole::Primary);
        assert_eq!(parsed.targets[0].retention.weekly, 4);
        assert!(parsed.esp.enabled);
        assert!(parsed.esp.mirror);
        assert_eq!(parsed.esp.hooks.hook_type, HookType::Pacman);
        assert!(parsed.email.enabled);
        assert_eq!(parsed.email.smtp_port, 1025);
        assert_eq!(parsed.email.auth, AuthMethod::Plain);
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
            subvolumes: vec!["@".into()],
            device: "/dev/sda".into(),
        });
        let errors = cfg.validate();
        assert!(
            errors.iter().any(|e| e.to_lowercase().contains("target")),
            "expected validation error about targets, got: {errors:?}",
        );
    }
}
