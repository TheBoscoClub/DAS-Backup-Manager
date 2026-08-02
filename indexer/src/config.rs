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
    #[serde(default)]
    pub scrub: Scrub,
    #[serde(default)]
    pub doctor: Doctor,
    #[serde(default, rename = "source")]
    pub sources: Vec<Source>,
    #[serde(default, rename = "target")]
    pub targets: Vec<Target>,
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
    "/etc/btrbk/btrbk.conf".into()
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
    60
}

impl Default for Boot {
    fn default() -> Self {
        Self {
            enabled: true,
            subvolumes: default_boot_subvolumes(),
            archive_retention_days: 60,
        }
    }
}

/// Scheduled BTRFS scrub of the DAS backup filesystems. Consumed by the
/// scrub engine (bd DAS-Backup-Manager-212), the systemd timer template
/// (bd DAS-Backup-Manager-atq), and the health checks (bd DAS-Backup-Manager-5kb).
/// Design decisions (2026-07-27, user-confirmed): unbounded monthly pass,
/// sequential, per-filesystem — three scrubs cover the four DAS drives
/// because the 22tb target is a single RAID-1 filesystem.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Scrub {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// systemd `OnCalendar=` expression consumed verbatim by the timer
    /// template. Kept as a plain string (rather than structured
    /// day/hour/minute fields) because the timer template only needs to
    /// substitute it directly — see bd DAS-Backup-Manager-ikn.
    #[serde(default = "default_scrub_on_calendar")]
    pub on_calendar: String,
    /// Config target labels, scrubbed sequentially in list order. Entries
    /// MUST match `[[target]].label` values elsewhere in this same config
    /// (e.g. `primary-22tb`, not the underlying BTRFS filesystem label
    /// `das-backup-22tb`) — UUIDs are resolved from the existing
    /// `[[target]]` blocks at use time by joining on this field, never
    /// duplicated here.
    #[serde(default = "default_scrub_targets")]
    pub targets: Vec<String>,
    /// Days since the last completed scrub of a target before health
    /// checks report a warning.
    #[serde(default = "default_scrub_warn_age_days")]
    pub warn_age_days: u32,
    /// Days since the last completed scrub of a target before health
    /// checks report a failure.
    #[serde(default = "default_scrub_fail_age_days")]
    pub fail_age_days: u32,
}

fn default_scrub_on_calendar() -> String {
    "*-*-01 05:30:00".into()
}
fn default_scrub_targets() -> Vec<String> {
    // Config target labels (`[[target]].label`), not BTRFS filesystem
    // labels — see the doc comment on `Scrub::targets`.
    vec![
        "primary-22tb".into(),
        "system-recovery-A-2tb".into(),
        "system-recovery-B-2tb".into(),
    ]
}
fn default_scrub_warn_age_days() -> u32 {
    45
}
fn default_scrub_fail_age_days() -> u32 {
    75
}

impl Default for Scrub {
    fn default() -> Self {
        Self {
            enabled: true,
            on_calendar: default_scrub_on_calendar(),
            targets: default_scrub_targets(),
            warn_age_days: default_scrub_warn_age_days(),
            fail_age_days: default_scrub_fail_age_days(),
        }
    }
}

/// Subvolume drift detector (`btrdasd doctor --check-drift`, bd DAS-Backup-Manager-01u).
/// Entirely optional — an absent `[doctor]` section (every config predating this
/// feature) parses to an empty exclude list via `#[serde(default)]`, identical in
/// effect to an explicit `exclude = []`. The built-in exclusions (`.snapshots/`,
/// `.btrbk-snapshots/`, `@tmp`, `@var-tmp`) are never configurable — this list only
/// adds patterns on top of them.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Doctor {
    /// Additional glob patterns (`*`/`?` wildcards, matched case-sensitively
    /// against the full on-disk subvolume path) excluded from drift reporting,
    /// on top of the built-in exclusions. See `doctor::glob_match`.
    #[serde(default)]
    pub exclude: Vec<String>,
}

/// A subvolume within a source, with optional scheduling flags and snapshot name override.
/// Accepts bare strings ("@"), structs with name+manual_only, or full structs with snapshot_name.
#[derive(Debug, Clone, Serialize)]
pub struct SubvolConfig {
    pub name: String,
    #[serde(default)]
    pub manual_only: bool,
    /// Override the btrbk snapshot_name (default: algorithmic from subvol name).
    #[serde(default)]
    pub snapshot_name: Option<String>,
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
                #[serde(default)]
                snapshot_name: Option<String>,
            },
        }

        match SubvolEntry::deserialize(deserializer)? {
            SubvolEntry::Simple(name) => Ok(SubvolConfig {
                name,
                manual_only: false,
                snapshot_name: None,
            }),
            SubvolEntry::Full {
                name,
                manual_only,
                snapshot_name,
            } => Ok(SubvolConfig {
                name,
                manual_only,
                snapshot_name,
            }),
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
    /// Which target labels this source sends to (empty = all targets).
    #[serde(default)]
    pub target_labels: Vec<String>,
}

fn default_snapshot_dir() -> String {
    ".btrbk-snapshots".into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(from = "TargetRaw")]
pub struct Target {
    pub label: String,
    // Legacy single-anchor serial. Populated from `serials[0]` so existing
    // call sites (health/SMART/GUI/mount) keep working unchanged. Skipped on
    // serialization — `serials` is the canonical written form.
    #[serde(skip_serializing)]
    pub serial: String,
    // Expected RAID-1 member serials. Length 1 = single-drive target,
    // length 2 = RAID-1 pair. Operator advisory only: backup-run.sh warns
    // when a serial is absent but does NOT abort, because a degraded BTRFS
    // RAID-1 array remains mountable from any present leg.
    #[serde(default)]
    pub serials: Vec<String>,
    // BTRFS filesystem UUID for mount-by-UUID. When set, backup-run.sh mounts
    // via `UUID=<mount_uuid>` instead of resolving a device from `serials`.
    // BTRFS auto-discovers remaining members from any present leg's
    // superblock, making backups tolerant of single-member loss.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mount_uuid: Option<String>,
    pub mount: String,
    pub role: TargetRole,
    pub retention: Retention,
    #[serde(default)]
    pub display_name: String,
}

impl Target {
    /// Effective list of expected serials, falling back to the legacy
    /// `serial` field when `serials` is empty (in-code construction sites
    /// not yet migrated to the new field).
    pub fn effective_serials(&self) -> Vec<String> {
        if !self.serials.is_empty() {
            self.serials.clone()
        } else if !self.serial.is_empty() {
            vec![self.serial.clone()]
        } else {
            Vec::new()
        }
    }
}

// TOML deserialization shim. Accepts both legacy `serial = "X"` and new
// `serials = ["X", "Y"]` forms; normalizes both into the canonical Target.
#[derive(Debug, Deserialize)]
struct TargetRaw {
    label: String,
    #[serde(default)]
    serial: Option<String>,
    #[serde(default)]
    serials: Option<Vec<String>>,
    #[serde(default)]
    mount_uuid: Option<String>,
    mount: String,
    role: TargetRole,
    retention: Retention,
    #[serde(default)]
    display_name: String,
}

impl From<TargetRaw> for Target {
    fn from(raw: TargetRaw) -> Self {
        // Prefer new-form `serials`; fall back to legacy `serial`.
        let serials = match (raw.serials, raw.serial.as_ref()) {
            (Some(list), _) if !list.is_empty() => list,
            (_, Some(s)) if !s.is_empty() => vec![s.clone()],
            (Some(list), _) => list,
            (None, None) => Vec::new(),
            (None, Some(_)) => Vec::new(),
        };
        let serial = serials.first().cloned().unwrap_or_default();
        Target {
            label: raw.label,
            serial,
            serials,
            mount_uuid: raw.mount_uuid,
            mount: raw.mount,
            role: raw.role,
            retention: raw.retention,
            display_name: raw.display_name,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TargetRole {
    Primary,
    Mirror,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
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

// ESP struct, EspHooks, HookType, and sync_esp() all removed 2026-04-12.
// The template-generated pacman hook was the root cause of the 2026-03-05
// DAS ESP wipe incident; remaining ESP code (struct, sync function, wizard
// step, env export, shell script references) was a latent risk vector and
// caused the daily backup to stop running on 2026-04-09 when dump-env
// stopped emitting DAS_ESP_MOUNT_POINTS.
// See .claude/rules/das-esp-safety.md for the full postmortem.
// Old config.toml files with [esp] sections are silently ignored by serde.

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
            scrub: Scrub::default(),
            doctor: Doctor::default(),
            sources: Vec::new(),
            targets: Vec::new(),
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
            // A target needs at least one way to find its filesystem:
            // legacy `serial`, new `serials`, or `mount_uuid`. UUID-only is
            // valid and preferred for RAID-1 because the FS is mountable
            // from any present member.
            if tgt.serial.is_empty() && tgt.serials.is_empty() && tgt.mount_uuid.is_none() {
                errors.push(format!(
                    "Target '{}' (index {i}) needs at least one of: `serial`, `serials`, or `mount_uuid`",
                    tgt.label
                ));
            }
        }

        if self.email.enabled && self.email.smtp_host.is_empty() {
            errors.push("Email is enabled but smtp_host is empty".into());
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
        assert_eq!(parsed.general.btrbk_conf, "/etc/btrbk/btrbk.conf");
        assert_eq!(parsed.das.model_pattern, "TDAS");
        assert_eq!(parsed.das.io_scheduler, "mq-deadline");
        assert!(parsed.boot.enabled);
        assert_eq!(parsed.boot.subvolumes, vec!["@", "@home"]);
        assert_eq!(parsed.boot.archive_retention_days, 60);
        assert!(parsed.scrub.enabled);
        assert_eq!(parsed.scrub.on_calendar, "*-*-01 05:30:00");
        assert_eq!(
            parsed.scrub.targets,
            vec![
                "primary-22tb",
                "system-recovery-A-2tb",
                "system-recovery-B-2tb",
            ]
        );
        assert_eq!(parsed.scrub.warn_age_days, 45);
        assert_eq!(parsed.scrub.fail_age_days, 75);
        // [doctor] is optional — default config has no exclude patterns
        assert!(parsed.doctor.exclude.is_empty());
    }

    #[test]
    fn doctor_round_trip_custom_exclude() {
        let mut cfg = Config::default();
        cfg.doctor.exclude = vec!["*.cache".into(), "Downloads/*".into()];
        let toml_str = cfg
            .to_toml()
            .expect("serialize config with custom doctor exclude");
        assert!(toml_str.contains("[doctor]"));
        let parsed =
            Config::from_toml(&toml_str).expect("deserialize config with custom doctor exclude");
        assert_eq!(parsed.doctor.exclude, vec!["*.cache", "Downloads/*"]);
    }

    #[test]
    fn scrub_defaults() {
        let scrub = Scrub::default();
        assert!(scrub.enabled);
        assert_eq!(scrub.on_calendar, "*-*-01 05:30:00");
        assert_eq!(
            scrub.targets,
            vec![
                "primary-22tb",
                "system-recovery-A-2tb",
                "system-recovery-B-2tb",
            ]
        );
        assert_eq!(scrub.warn_age_days, 45);
        assert_eq!(scrub.fail_age_days, 75);
    }

    #[test]
    fn scrub_round_trip_custom_values() {
        let mut cfg = Config::default();
        cfg.scrub.enabled = false;
        cfg.scrub.on_calendar = "*-*-15 02:00:00".into();
        cfg.scrub.targets = vec!["custom-target".into()];
        cfg.scrub.warn_age_days = 30;
        cfg.scrub.fail_age_days = 60;

        let toml_str = cfg.to_toml().expect("serialize config with custom scrub");
        assert!(toml_str.contains("[scrub]"));
        let parsed = Config::from_toml(&toml_str).expect("deserialize config with custom scrub");

        assert!(!parsed.scrub.enabled);
        assert_eq!(parsed.scrub.on_calendar, "*-*-15 02:00:00");
        assert_eq!(parsed.scrub.targets, vec!["custom-target"]);
        assert_eq!(parsed.scrub.warn_age_days, 30);
        assert_eq!(parsed.scrub.fail_age_days, 60);
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
                    snapshot_name: None,
                },
                SubvolConfig {
                    name: "@home".into(),
                    manual_only: false,
                    snapshot_name: None,
                },
            ],
            device: "/dev/nvme0n1p2".into(),
            snapshot_dir: ".snapshots".into(),
            target_subdirs: vec!["nvme".into()],
            target_labels: vec![],
        });
        cfg.targets.push(Target {
            label: "primary-22tb".into(),
            serial: "ZXA0LMAE".into(),
            serials: vec!["ZXA0LMAE".into()],
            mount_uuid: None,
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
        assert_eq!(cfg.general.btrbk_conf, "/etc/btrbk/btrbk.conf");
        assert_eq!(cfg.das.model_pattern, "TDAS");
        assert_eq!(cfg.das.io_scheduler, "mq-deadline");
        assert!(cfg.boot.enabled);
        assert_eq!(cfg.boot.archive_retention_days, 60);
        assert_eq!(cfg.sources[0].snapshot_dir, ".btrbk-snapshots");
        assert!(cfg.sources[0].target_subdirs.is_empty());
        assert_eq!(cfg.targets[0].retention.daily, 0);
        assert_eq!(cfg.targets[0].retention.yearly, 0);
        assert!(cfg.targets[0].display_name.is_empty());
        // [scrub] absent entirely from this old config — must parse to defaults
        assert!(cfg.scrub.enabled);
        assert_eq!(cfg.scrub.on_calendar, "*-*-01 05:30:00");
        assert_eq!(
            cfg.scrub.targets,
            vec![
                "primary-22tb",
                "system-recovery-A-2tb",
                "system-recovery-B-2tb",
            ]
        );
        assert_eq!(cfg.scrub.warn_age_days, 45);
        assert_eq!(cfg.scrub.fail_age_days, 75);
        // [doctor] absent entirely from this old config — must parse to defaults
        assert!(cfg.doctor.exclude.is_empty());
    }

    #[test]
    fn target_parses_legacy_single_serial() {
        let toml = r#"
label = "primary"
serial = "ZXA0LMAE"
mount = "/mnt/backup"
role = "primary"
[retention]
daily = 7
"#;
        let t: Target = toml::from_str(toml).expect("legacy form should parse");
        assert_eq!(t.serial, "ZXA0LMAE");
        assert_eq!(t.serials, vec!["ZXA0LMAE"]);
        assert!(t.mount_uuid.is_none());
        assert_eq!(t.effective_serials(), vec!["ZXA0LMAE"]);
    }

    #[test]
    fn target_parses_new_serials_list() {
        let toml = r#"
label = "primary"
serials = ["ZXA0LMAE", "ZXA1NYGZ"]
mount_uuid = "46ffbd7c-dfd9-4ba5-82ae-0afffde99bb1"
mount = "/mnt/backup"
role = "primary"
[retention]
daily = 7
"#;
        let t: Target = toml::from_str(toml).expect("new form should parse");
        assert_eq!(t.serials, vec!["ZXA0LMAE", "ZXA1NYGZ"]);
        // legacy `serial` is auto-populated from serials[0] for back-compat
        assert_eq!(t.serial, "ZXA0LMAE");
        assert_eq!(
            t.mount_uuid.as_deref(),
            Some("46ffbd7c-dfd9-4ba5-82ae-0afffde99bb1")
        );
    }

    #[test]
    fn target_parses_uuid_only() {
        // Valid: UUID-only target with no serial — degraded RAID-1 with both
        // legs unidentified by serial but the FS is mountable.
        let toml = r#"
label = "primary"
mount_uuid = "46ffbd7c-dfd9-4ba5-82ae-0afffde99bb1"
mount = "/mnt/backup"
role = "primary"
[retention]
daily = 7
"#;
        let t: Target = toml::from_str(toml).expect("uuid-only form should parse");
        assert!(t.serial.is_empty());
        assert!(t.serials.is_empty());
        assert_eq!(
            t.mount_uuid.as_deref(),
            Some("46ffbd7c-dfd9-4ba5-82ae-0afffde99bb1")
        );
    }

    #[test]
    fn target_parses_mixed_prefers_serials() {
        // Both forms present → `serials` wins, `serial` is ignored.
        let toml = r#"
label = "primary"
serial = "OLDSERIAL"
serials = ["NEW1", "NEW2"]
mount = "/mnt/backup"
role = "primary"
[retention]
daily = 7
"#;
        let t: Target = toml::from_str(toml).expect("mixed form should parse");
        assert_eq!(t.serials, vec!["NEW1", "NEW2"]);
        assert_eq!(t.serial, "NEW1");
    }

    #[test]
    fn target_round_trips_new_form_only() {
        // After serialize → deserialize, the legacy `serial` field is dropped
        // from output and re-derived from `serials[0]`.
        let original = Target {
            label: "primary-22tb".into(),
            serial: "ZXA1NYGZ".into(),
            serials: vec!["ZXA1NYGZ".into(), "NEWLEG".into()],
            mount_uuid: Some("46ffbd7c-dfd9-4ba5-82ae-0afffde99bb1".into()),
            mount: "/mnt/backup-22tb".into(),
            role: TargetRole::Primary,
            retention: Retention {
                weekly: 4,
                monthly: 12,
                daily: 7,
                yearly: 1,
            },
            display_name: "22TB RAID-1 Primary".into(),
        };
        let s = toml::to_string(&original).expect("serialize");
        // Legacy `serial = ` line MUST NOT appear in canonical output
        assert!(!s.contains("\nserial = "), "found legacy serial in: {s}");
        assert!(s.contains("serials = "));
        assert!(s.contains("mount_uuid = "));
        let parsed: Target = toml::from_str(&s).expect("round-trip parse");
        assert_eq!(parsed.serials, original.serials);
        assert_eq!(parsed.mount_uuid, original.mount_uuid);
        assert_eq!(parsed.serial, "ZXA1NYGZ"); // re-derived from serials[0]
    }

    #[test]
    fn target_validate_uuid_only_passes() {
        let mut cfg = Config::default();
        // Make a config that's otherwise valid except the target uses UUID-only.
        cfg.sources.push(Source {
            label: "src".into(),
            volume: "/mnt/src".into(),
            subvolumes: vec![SubvolConfig {
                name: "@".into(),
                manual_only: false,
                snapshot_name: None,
            }],
            device: "/dev/nvme0n1p2".into(),
            snapshot_dir: ".btrbk-snapshots".into(),
            target_subdirs: vec![],
            target_labels: vec![],
        });
        // Replace the default target with a UUID-only one.
        cfg.targets.clear();
        cfg.targets.push(Target {
            label: "primary".into(),
            serial: String::new(),
            serials: Vec::new(),
            mount_uuid: Some("46ffbd7c-dfd9-4ba5-82ae-0afffde99bb1".into()),
            mount: "/mnt/backup".into(),
            role: TargetRole::Primary,
            retention: Retention::default(),
            display_name: String::new(),
        });
        let errors = cfg.validate();
        assert!(
            !errors.iter().any(|e| e.contains("primary")),
            "UUID-only target should validate, got: {errors:?}"
        );
    }

    #[test]
    fn target_validate_no_anchor_fails() {
        let mut cfg = Config::default();
        cfg.sources.push(Source {
            label: "src".into(),
            volume: "/mnt/src".into(),
            subvolumes: vec![SubvolConfig {
                name: "@".into(),
                manual_only: false,
                snapshot_name: None,
            }],
            device: "/dev/nvme0n1p2".into(),
            snapshot_dir: ".btrbk-snapshots".into(),
            target_subdirs: vec![],
            target_labels: vec![],
        });
        cfg.targets.clear();
        cfg.targets.push(Target {
            label: "naked".into(),
            serial: String::new(),
            serials: Vec::new(),
            mount_uuid: None,
            mount: "/mnt/backup".into(),
            role: TargetRole::Primary,
            retention: Retention::default(),
            display_name: String::new(),
        });
        let errors = cfg.validate();
        assert!(
            errors
                .iter()
                .any(|e| e.contains("naked") && (e.contains("serial") || e.contains("mount_uuid"))),
            "no-anchor target should fail validation, got: {errors:?}"
        );
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
                snapshot_name: None,
            }],
            device: "/dev/sda".into(),
            snapshot_dir: ".btrbk-snapshots".into(),
            target_subdirs: vec![],
            target_labels: vec![],
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
