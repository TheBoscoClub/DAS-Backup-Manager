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
    #[serde(default)]
    pub subvolumes: Subvolumes,
    #[serde(default)]
    pub restore: Restore,
    #[serde(default)]
    pub recovery_os: RecoveryOs,
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

/// Where a restore is permitted to write.
///
/// `restore_files`/`restore_snapshot` run as root under the D-Bus helper and
/// used to accept ANY destination, so a caller could land a file in
/// `/etc/systemd/system` or `/etc/pacman.d/hooks` — the same class of write that
/// caused the 2026-03-05 ESP wipe (`.claude/rules/esp-safety.md`). Destinations
/// are now checked against this allowlist, and against a built-in denylist that
/// no configuration can override (bd DAS-Backup-Manager-s05).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Restore {
    /// Roots a restore may write beneath. A destination must canonicalize to a
    /// path under one of these.
    #[serde(default = "default_restore_roots")]
    pub allowed_roots: Vec<String>,
}

fn default_restore_roots() -> Vec<String> {
    vec!["/home".into(), "/tmp".into()]
}

impl Default for Restore {
    fn default() -> Self {
        Self {
            allowed_roots: default_restore_roots(),
        }
    }
}

/// The independent operating systems on the `role = "mirror"` targets
/// (`btrdasd recovery-os`, bd DAS-Backup-Manager-xd3). An absent section
/// parses to the defaults.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecoveryOs {
    /// Days since a recovery OS's last full system upgrade before the backup
    /// report marks it STALE. A kernel series behind the host's, or an upgrade
    /// date that cannot be read, is stale regardless of this.
    #[serde(default = "default_recovery_os_max_age_days")]
    pub max_age_days: u32,
}

fn default_recovery_os_max_age_days() -> u32 {
    60
}

impl Default for RecoveryOs {
    fn default() -> Self {
        Self {
            max_age_days: default_recovery_os_max_age_days(),
        }
    }
}

/// Roots a restore may NEVER write beneath, regardless of `allowed_roots`.
///
/// Listing one of these in `allowed_roots` does not enable it — the denylist is
/// checked first and wins. These are the paths where a restored file becomes
/// executable code or changes system identity.
///
/// `/srv/http` and `/srv/ftp` are here for the same reason rather than a
/// different one: they are the distro-default document roots, so a file
/// restored into either is *served* — published to whoever can reach the
/// listener. That is the same "restored content becomes live" property the rest
/// of this list guards, arriving by a network path instead of an exec path.
/// They were added when `/srv/VirtualMachines` was granted to `allowed_roots`
/// so that a VM image could be restored in place (bd DAS-Backup-Manager-tku):
/// the grant is deliberately a *subdirectory* of `/srv` and never `/srv`
/// itself, and these entries make sure that stays true even if someone later
/// widens the allow-list to the parent. Comparison is component-wise, so
/// `/srv/http-archive` is unaffected.
pub const RESTORE_DENIED_ROOTS: &[&str] = &[
    "/bin",
    "/boot",
    "/dev",
    "/etc",
    "/lib",
    "/lib64",
    "/proc",
    "/root",
    "/sbin",
    "/srv/ftp",
    "/srv/http",
    "/sys",
    "/usr",
    "/var/lib",
    "/var/spool",
];

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
    // 03:05 on the 1st — five minutes AFTER the 03:00 daily backup fires, so
    // the backup already holds /run/das-maintenance.lock and the scrub
    // engine's blocking acquire makes the scrub start the moment the backup
    // finishes ("scrub follows the backup", user decision 2026-08-02). The
    // same blocking flock also guarantees the inverse on overrun: a scrub
    // still running when the NEXT day's 03:00 backup fires HOLDS that backup
    // (deferral, never a skip) and releases it the moment the scrub
    // completes.
    "*-*-01 03:05:00".into()
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
#[derive(Debug, Clone, Default, Serialize)]
pub struct SubvolConfig {
    pub name: String,
    pub manual_only: bool,
    /// Override the btrbk snapshot_name (default: algorithmic from subvol name).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snapshot_name: Option<String>,
    /// Date (`YYYY-MM-DD`, UTC) the backup run added this entry by itself.
    /// Absent on hand-written entries.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub adopted: Option<String>,
    /// Date the subvolume was found to be gone. A retired entry is not sent
    /// to btrbk; its existing backups expire (`expire.rs`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retired: Option<String>,
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
                #[serde(default)]
                adopted: Option<String>,
                #[serde(default)]
                retired: Option<String>,
            },
        }

        match SubvolEntry::deserialize(deserializer)? {
            SubvolEntry::Simple(name) => Ok(SubvolConfig {
                name,
                ..Default::default()
            }),
            SubvolEntry::Full {
                name,
                manual_only,
                snapshot_name,
                adopted,
                retired,
            } => Ok(SubvolConfig {
                name,
                manual_only,
                snapshot_name,
                adopted,
                retired,
            }),
        }
    }
}

/// What may keep a subvolume out of the backup, apart from snapshot trees.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Subvolumes {
    /// Glob patterns (`*`, `?`) matched against the on-disk subvolume path.
    /// A pattern also excludes everything nested under what it matches.
    #[serde(default)]
    pub exclude: Vec<String>,
}

/// Always excluded: btrbk would snapshot scratch space on every run.
const DEFAULT_EXCLUDES: &[&str] = &["@tmp", "@var-tmp"];

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
// See .claude/rules/esp-safety.md for the full postmortem.
// Old config.toml files with [esp] sections are silently ignored by serde.

/// Report email settings.
///
/// Submission is **unauthenticated plaintext to a local mail relay** — the
/// relay (Postfix null-client on `127.0.0.1:25`) holds the upstream credential
/// and authenticates to the provider by envelope sender. Nothing in this
/// project reads, stores, or transmits a mail credential; see
/// `.claude/rules/backup.md` §Email Reports.
///
/// Every field here is read at send time by [`crate::report`] and exported to
/// the shell orchestrator by `setup::env_export` as `DAS_EMAIL_*`. Prior to
/// 2026-08-06 these keys were parsed and then ignored — both senders read
/// Protonmail Bridge's own credentials file instead — so a host could
/// set `smtp_host` to anything with no effect whatsoever.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Email {
    #[serde(default)]
    pub enabled: bool,
    /// Relay host. Loopback in every sane configuration — mail leaves the host
    /// over the relay's authenticated, certificate-verified uplink, not this hop.
    #[serde(default = "default_smtp_host")]
    pub smtp_host: String,
    /// Relay port. 25 is the local submission port of the null-client relay,
    /// NOT outbound port 25 (which is blocked at this host's uplink anyway).
    #[serde(default = "default_smtp_port")]
    pub smtp_port: u16,
    /// Envelope sender AND `From:` address. The relay selects the upstream API
    /// key by this value, so it must be an address the relay has a key for;
    /// anything unknown is rewritten by the relay's canonical-sender fallback
    /// and delivered under a different identity.
    #[serde(default = "default_email_from")]
    pub from: String,
    #[serde(default = "default_email_to")]
    pub to: String,
}

fn default_smtp_host() -> String {
    "127.0.0.1".to_string()
}

/// Local relay submission port. Deliberately not 1025 (Protonmail Bridge) or
/// 587 (direct provider submission) — both imply a credential this project no
/// longer holds.
fn default_smtp_port() -> u16 {
    25
}

fn default_email_from() -> String {
    "backup@localhost".to_string()
}

fn default_email_to() -> String {
    "root@localhost".to_string()
}

impl Default for Email {
    fn default() -> Self {
        Self {
            enabled: false,
            smtp_host: default_smtp_host(),
            smtp_port: default_smtp_port(),
            from: default_email_from(),
            to: default_email_to(),
        }
    }
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
            restore: Restore::default(),
            recovery_os: RecoveryOs::default(),
            schedule: Schedule {
                incremental: "03:00".into(),
                full: "Sun 04:00".into(),
                randomized_delay_min: 30,
            },
            das: Das::default(),
            boot: Boot::default(),
            scrub: Scrub::default(),
            doctor: Doctor::default(),
            subvolumes: Subvolumes::default(),
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

    /// Save this config to a TOML file, creating parent directories as
    /// needed. The file is replaced whole and keeps its mode (and, saved by
    /// root, its owner and group) — see [`crate::fsutil::write_atomic`]. Every
    /// error names `path`, once.
    pub fn save(&self, path: &Path) -> Result<(), Box<dyn Error>> {
        let named = |e: &dyn std::fmt::Display| format!("cannot write {}: {e}", path.display());
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| named(&e))?;
        }
        let header = "# Generated by btrdasd setup — do not edit.\n\
                       # Modify this file and run: sudo btrdasd setup --upgrade\n\n";
        let body = self.to_toml().map_err(|e| named(&e))?;
        crate::fsutil::write_atomic(path, &format!("{header}{body}"))?;
        Ok(())
    }

    /// Every pattern that excludes a subvolume: the built-in defaults, then
    /// `[subvolumes].exclude`, then the older `[doctor].exclude`, without
    /// repeats. `[doctor].exclude` is still read so existing configs keep
    /// excluding what they excluded.
    pub fn exclude_patterns(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let all = DEFAULT_EXCLUDES
            .iter()
            .map(|s| s.to_string())
            .chain(self.subvolumes.exclude.iter().cloned())
            .chain(self.doctor.exclude.iter().cloned());
        for pattern in all {
            if !out.contains(&pattern) {
                out.push(pattern);
            }
        }
        out
    }

    /// Validate the config and return a list of human-readable error messages.
    /// An empty vec means the config is valid.
    pub fn validate(&self) -> Vec<String> {
        let mut errors = Vec::new();

        // A malformed schedule used to be absorbed by silent defaults in the
        // cron generator (03:00, Sunday). Rejecting it here means `setup` and
        // the D-Bus `config_set` both refuse it, on every init system
        // (bd DAS-Backup-Manager-06p).
        errors.extend(schedule_errors(
            &self.schedule.incremental,
            &self.schedule.full,
        ));

        if self.recovery_os.max_age_days == 0 {
            errors.push(
                "recovery_os.max_age_days must be at least 1 (0 would mark every recovery OS stale)"
                    .into(),
            );
        }

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

        // The boot step keys a target's state by its label, and a label names a
        // mount in every log line: two targets sharing one would overwrite each
        // other's entry (bd azvo).
        let mut labels = std::collections::HashSet::new();
        for tgt in &self.targets {
            if !labels.insert(tgt.label.as_str()) {
                errors.push(format!(
                    "Target label '{}' is used by more than one [[target]] — labels must be unique",
                    tgt.label
                ));
            }
        }

        // A boot subvolume listed twice is archived twice in one run, to the
        // same `<subvol>.archive.<TS>` path (bd tens).
        let mut boot_seen = std::collections::HashSet::new();
        for sv in &self.boot.subvolumes {
            if !boot_seen.insert(sv.as_str()) {
                errors.push(format!(
                    "[boot].subvolumes lists '{sv}' more than once — each entry must be unique"
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

        if self.email.enabled {
            // These are read at send time now (they were inert before 2026-08-06),
            // so an empty value is a real misconfiguration rather than a cosmetic
            // one. An empty recipient in particular produces a mailx invocation
            // with no destination, which fails at submission rather than silently.
            if self.email.smtp_host.is_empty() {
                errors.push("Email is enabled but smtp_host is empty".into());
            }
            if self.email.smtp_port == 0 {
                errors.push("Email is enabled but smtp_port is 0".into());
            }
            if self.email.from.is_empty() {
                errors.push("Email is enabled but from is empty".into());
            }
            if self.email.to.is_empty() {
                errors.push("Email is enabled but to is empty".into());
            }
        }

        // Two entries sharing a snapshot directory and a snapshot name would
        // overwrite each other's series; btrbk refuses the whole config.
        let mut seen: std::collections::HashMap<(String, String, String), String> =
            std::collections::HashMap::new();
        for src in &self.sources {
            let names = crate::btrbk_conf::resolve_snapshot_names(&src.subvolumes);
            for (sv, snap) in src.subvolumes.iter().zip(names) {
                if sv.retired.is_some() {
                    continue;
                }
                let key = (src.volume.clone(), src.snapshot_dir.clone(), snap.clone());
                if let Some(first) = seen.get(&key) {
                    errors.push(format!(
                        "Subvolumes '{first}' and '{}' on volume '{}' both resolve to \
                         snapshot name '{snap}' — give one an explicit snapshot_name",
                        sv.name, src.volume
                    ));
                } else {
                    seen.insert(key, sv.name.clone());
                }
            }
        }

        errors
    }
}

// ---------------------------------------------------------------------------
// Schedule parsing (moved here from setup/templates.rs — `setup` is
// binary-only, and `Config` owns these fields, so validation must live with
// the type rather than with one of its consumers).
// ---------------------------------------------------------------------------

/// Parse `"HH:MM"`.
///
/// Returns `Err` rather than defaulting. The previous version fell back to
/// `(3, 0)` on any parse failure, so `"3 AM"`, `"03:15am"` and `"0300"` all
/// silently produced an 03:00 cron entry with no warning at generation time, no
/// warning at cron-load time, and nothing in the journal — discoverable only by
/// noticing backups at the wrong hour (bd DAS-Backup-Manager-06p).
pub fn parse_time(time_str: &str) -> Result<(u32, u32), String> {
    let parts: Vec<&str> = time_str.split(':').collect();
    if parts.len() != 2 {
        return Err(format!("expected HH:MM, got '{time_str}'"));
    }
    let hour: u32 = parts[0]
        .trim()
        .parse()
        .map_err(|_| format!("hour is not a number in '{time_str}'"))?;
    let min: u32 = parts[1]
        .trim()
        .parse()
        .map_err(|_| format!("minute is not a number in '{time_str}'"))?;
    if hour > 23 {
        return Err(format!("hour {hour} out of range in '{time_str}'"));
    }
    if min > 59 {
        return Err(format!("minute {min} out of range in '{time_str}'"));
    }
    Ok((hour, min))
}

/// Parse a schedule string like "Sun 04:00" into (day_of_week, hour, minute).
/// Day of week: Sun=0, Mon=1, ..., Sat=6.
pub fn parse_schedule_with_day(schedule: &str) -> Result<(u32, u32, u32), String> {
    let parts: Vec<&str> = schedule.split_whitespace().collect();
    match parts.len() {
        2 => {
            // An unrecognised token used to fall through to `_ => 0`, so
            // "Friday 04:00" (full name instead of "Fri") silently scheduled
            // SUNDAY backups. Locale day names collapsed the same way.
            let dow = match parts[0].to_lowercase().as_str() {
                "sun" => 0,
                "mon" => 1,
                "tue" => 2,
                "wed" => 3,
                "thu" => 4,
                "fri" => 5,
                "sat" => 6,
                other => {
                    return Err(format!(
                        "unrecognised weekday '{other}' in '{schedule}' \
                         (expected one of sun mon tue wed thu fri sat)"
                    ));
                }
            };
            let (hour, min) = parse_time(parts[1])?;
            Ok((dow, hour, min))
        }
        1 => {
            let (hour, min) = parse_time(parts[0])?;
            Ok((0, hour, min)) // bare time means Sunday
        }
        n => Err(format!(
            "expected \"HH:MM\" or \"Day HH:MM\", got {n} fields in '{schedule}'"
        )),
    }
}

/// Reasons `schedule.incremental` / `schedule.full` cannot be turned into a
/// schedule. Consumed by [`Config::validate`] so a malformed
/// value is refused at config load on EVERY init system — not just where a cron
/// entry happens to be generated.
pub fn schedule_errors(incremental: &str, full: &str) -> Vec<String> {
    let mut errors = Vec::new();
    if let Err(e) = parse_time(incremental) {
        errors.push(format!("schedule.incremental: {e}"));
    }
    if let Err(e) = parse_schedule_with_day(full) {
        errors.push(format!("schedule.full: {e}"));
    }
    errors
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {

    fn source_with(label: &str, volume: &str, subvols: &[(&str, Option<&str>)]) -> String {
        let mut s = format!(
            "[[source]]\nlabel = \"{label}\"\nvolume = \"{volume}\"\ndevice = \"/dev/sda\"\n"
        );
        for (name, snap) in subvols {
            s.push_str(&format!("[[source.subvolumes]]\nname = \"{name}\"\n"));
            if let Some(snap) = snap {
                s.push_str(&format!("snapshot_name = \"{snap}\"\n"));
            }
        }
        s
    }

    const ONE_TARGET: &str = "[[target]]\nlabel = \"t\"\nserial = \"X\"\nmount = \"/mnt/t\"\nrole = \"primary\"\n[target.retention]\ndaily = 7\n";

    #[test]
    fn adopted_and_retired_round_trip_and_are_absent_when_unset() {
        let extra = format!(
            "{}[[source.subvolumes]]\nname = \"b\"\nadopted = \"2026-10-02\"\nretired = \"2026-11-14\"\n{ONE_TARGET}",
            source_with("s", "/vol", &[("a", None)])
        );
        let cfg = Config::from_toml(&minimal_toml(&extra)).unwrap();
        let a = &cfg.sources[0].subvolumes[0];
        let b = &cfg.sources[0].subvolumes[1];
        assert_eq!((a.adopted.as_deref(), a.retired.as_deref()), (None, None));
        assert_eq!(b.adopted.as_deref(), Some("2026-10-02"));
        assert_eq!(b.retired.as_deref(), Some("2026-11-14"));

        let text = cfg.to_toml().unwrap();
        assert_eq!(text.matches("adopted = ").count(), 1, "{text}");
        assert_eq!(text.matches("retired = ").count(), 1, "{text}");
        let again = Config::from_toml(&text).unwrap();
        assert_eq!(
            again.sources[0].subvolumes[1].retired.as_deref(),
            Some("2026-11-14")
        );
    }

    #[test]
    fn exclude_patterns_merge_the_old_doctor_key_and_the_defaults() {
        let extra = "[doctor]\nexclude = [\"@cache\", \"coredumps\"]\n\
                     [subvolumes]\nexclude = [\"scratch\", \"@cache\"]\n";
        let cfg = Config::from_toml(&minimal_toml(extra)).unwrap();
        // Order: built-in defaults, then [subvolumes], then [doctor]; no repeats.
        assert_eq!(
            cfg.exclude_patterns(),
            ["@tmp", "@var-tmp", "scratch", "@cache", "coredumps"]
        );
    }

    #[test]
    fn validate_rejects_two_entries_that_resolve_to_one_snapshot_name() {
        let extra = format!(
            "{}{ONE_TARGET}",
            // Explicit names: `resolve_snapshot_names` already separates two
            // unnamed entries that collide, so only an explicit clash survives it.
            source_with("s", "/vol", &[("a/b", Some("a-b")), ("a-b", Some("a-b"))])
        );
        let cfg = Config::from_toml(&minimal_toml(&extra)).unwrap();
        let errors = cfg.validate();
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(
            errors[0].contains("snapshot name 'a-b'")
                && errors[0].contains("'a/b'")
                && errors[0].contains("'a-b'"),
            "{errors:?}"
        );
    }

    #[test]
    fn validate_ignores_a_retired_entry_when_checking_snapshot_names() {
        let extra = format!(
            "{}[[source.subvolumes]]\nname = \"a-b\"\nretired = \"2026-01-01\"\n{ONE_TARGET}",
            source_with("s", "/vol", &[("a/b", None)])
        );
        let cfg = Config::from_toml(&minimal_toml(&extra)).unwrap();
        assert!(cfg.validate().is_empty(), "{:?}", cfg.validate());
    }

    #[test]
    fn validate_checks_names_across_sources_sharing_a_snapshot_dir() {
        let extra = format!(
            "{}{}{ONE_TARGET}",
            source_with("one", "/vol", &[("x", Some("same"))]),
            source_with("two", "/vol", &[("y", Some("same"))]),
        );
        let cfg = Config::from_toml(&minimal_toml(&extra)).unwrap();
        assert_eq!(cfg.validate().len(), 1, "{:?}", cfg.validate());
        // A different volume is a different snapshot directory.
        let extra = format!(
            "{}{}{ONE_TARGET}",
            source_with("one", "/vol", &[("x", Some("same"))]),
            source_with("two", "/other", &[("y", Some("same"))]),
        );
        let cfg = Config::from_toml(&minimal_toml(&extra)).unwrap();
        assert!(cfg.validate().is_empty(), "{:?}", cfg.validate());
    }

    #[test]
    fn validate_rejects_a_boot_subvolume_listed_twice_and_names_it() {
        let extra = format!("{}{ONE_TARGET}", source_with("s", "/vol", &[("a", None)]));
        let mut cfg = Config::from_toml(&minimal_toml(&extra)).unwrap();
        cfg.boot.subvolumes = vec!["@".into(), "@home".into(), "@".into()];
        let errors = cfg.validate();
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(
            errors[0].contains("[boot].subvolumes") && errors[0].contains("'@'"),
            "{errors:?}"
        );
        // The control: the same list without the repeat is accepted.
        cfg.boot.subvolumes = vec!["@".into(), "@home".into()];
        assert!(cfg.validate().is_empty(), "{:?}", cfg.validate());
    }

    #[test]
    fn validate_rejects_two_targets_sharing_a_label_and_names_it() {
        let second = ONE_TARGET.replace("serial = \"X\"", "serial = \"Y\"");
        let extra = format!(
            "{}{ONE_TARGET}{second}",
            source_with("s", "/vol", &[("a", None)])
        );
        let cfg = Config::from_toml(&minimal_toml(&extra)).unwrap();
        let errors = cfg.validate();
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(
            errors[0].contains("label 't'") && errors[0].contains("unique"),
            "{errors:?}"
        );
        // The control: distinct labels pass.
        let second = second.replace("label = \"t\"", "label = \"u\"");
        let extra = format!(
            "{}{ONE_TARGET}{second}",
            source_with("s", "/vol", &[("a", None)])
        );
        let cfg = Config::from_toml(&minimal_toml(&extra)).unwrap();
        assert!(cfg.validate().is_empty(), "{:?}", cfg.validate());
    }

    #[test]
    fn save_replaces_the_file_whole_and_leaves_no_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "old contents that must not survive").unwrap();
        let cfg = Config::from_toml(&minimal_toml("")).unwrap();
        cfg.save(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("# Generated by btrdasd setup"));
        assert!(Config::from_toml(&text).is_ok());
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(names, ["config.toml"]);
    }

    #[test]
    fn save_keeps_the_files_mode_and_a_failure_names_the_file_once() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config::from_toml(&minimal_toml("")).unwrap();
        // The reviewer's probe: a private config.toml stays private.
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "old").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        cfg.save(&path).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o7777,
            0o600
        );
        // A file where its directory should be: the directory cannot be made.
        std::fs::write(dir.path().join("etc"), "a file").unwrap();
        let path = dir.path().join("etc/das-backup/config.toml");
        let err = cfg.save(&path).unwrap_err().to_string();
        assert!(
            err.starts_with(&format!("cannot write {}: ", path.display())),
            "{err}"
        );
        assert_eq!(err.matches(&path.display().to_string()).count(), 1, "{err}");
    }
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
        assert_eq!(parsed.scrub.on_calendar, "*-*-01 03:05:00");
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
    fn recovery_os_defaults_to_60_days_and_round_trips() {
        let cfg = Config::from_toml(&minimal_toml("")).unwrap();
        assert_eq!(cfg.recovery_os.max_age_days, 60, "absent section");
        let cfg = Config::from_toml(&minimal_toml("[recovery_os]\n")).unwrap();
        assert_eq!(cfg.recovery_os.max_age_days, 60, "empty section");
        let cfg = Config::from_toml(&minimal_toml("[recovery_os]\nmax_age_days = 30\n")).unwrap();
        assert_eq!(cfg.recovery_os.max_age_days, 30);
        let text = cfg.to_toml().unwrap();
        assert!(
            text.contains("[recovery_os]\nmax_age_days = 30\n"),
            "{text}"
        );
        assert_eq!(
            Config::from_toml(&text).unwrap().recovery_os.max_age_days,
            30
        );
        assert!(Config::from_toml(&minimal_toml("[recovery_os]\nmax_age_days = -1\n")).is_err());
    }

    #[test]
    fn validate_requires_a_recovery_os_age_of_at_least_one_day() {
        let msg =
            "recovery_os.max_age_days must be at least 1 (0 would mark every recovery OS stale)";
        let mut cfg =
            Config::from_toml(&minimal_toml("[recovery_os]\nmax_age_days = 0\n")).unwrap();
        assert!(
            cfg.validate().iter().any(|e| e == msg),
            "{:?}",
            cfg.validate()
        );
        cfg.recovery_os.max_age_days = 1;
        assert!(
            !cfg.validate().iter().any(|e| e.contains("max_age_days")),
            "{:?}",
            cfg.validate()
        );
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
        assert_eq!(scrub.on_calendar, "*-*-01 03:05:00");
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
                    ..Default::default()
                },
                SubvolConfig {
                    name: "@home".into(),
                    manual_only: false,
                    snapshot_name: None,
                    ..Default::default()
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
        cfg.email.smtp_port = 25;
        cfg.email.from = "backup@example.com".into();
        cfg.email.to = "user@example.com".into();

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
        assert_eq!(parsed.email.smtp_port, 25);
        assert_eq!(parsed.email.from, "backup@example.com");
        assert_eq!(parsed.email.to, "user@example.com");
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
        assert_eq!(cfg.scrub.on_calendar, "*-*-01 03:05:00");
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
                ..Default::default()
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
                ..Default::default()
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
                ..Default::default()
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

    /// Minimal config carrying only the mandatory sections, plus whatever
    /// optional sections a test appends. Parsing real TOML (rather than calling
    /// the `default_*` fns) is the point: it exercises the `#[serde(default)]`
    /// wiring an on-disk config.toml actually goes through.
    fn minimal_toml(extra: &str) -> String {
        format!(
            r#"
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

[email]
enabled = false

[gui]
enabled = false

{extra}
"#
        )
    }

    #[test]
    fn restore_roots_default_when_section_absent() {
        // The allow-list is a security policy (`.claude/rules/backup.md`
        // §Restore Destination Policy): a config predating `[restore]` must
        // permit exactly /home and /tmp. An empty list would refuse every
        // restore; anything wider would let the root D-Bus helper write there.
        let cfg = Config::from_toml(&minimal_toml("")).expect("parse without [restore]");
        assert_eq!(cfg.restore.allowed_roots, vec!["/home", "/tmp"]);
    }

    #[test]
    fn restore_roots_default_when_section_present_but_empty() {
        // A bare `[restore]` header takes the field-level serde default rather
        // than `Restore::default()`; both routes must agree on the policy.
        let cfg = Config::from_toml(&minimal_toml("[restore]")).expect("parse bare [restore]");
        assert_eq!(cfg.restore.allowed_roots, vec!["/home", "/tmp"]);
    }

    #[test]
    fn restore_roots_explicit_value_replaces_default() {
        let cfg = Config::from_toml(&minimal_toml(
            "[restore]\nallowed_roots = [\"/home\", \"/tmp\", \"/srv/VirtualMachines\"]",
        ))
        .expect("parse explicit [restore]");
        assert_eq!(
            cfg.restore.allowed_roots,
            vec!["/home", "/tmp", "/srv/VirtualMachines"]
        );
        // An explicit empty list is honoured as "nothing allowed", never
        // silently widened back to the defaults.
        let cfg = Config::from_toml(&minimal_toml("[restore]\nallowed_roots = []"))
            .expect("parse empty allowed_roots");
        assert!(cfg.restore.allowed_roots.is_empty());
    }

    #[test]
    fn general_path_defaults_when_keys_absent() {
        // backup-run.sh receives these via the env export and writes to them
        // unconditionally: the report is saved to `last_report` BEFORE any send
        // (`backup.md` §Email Reports) and `btrdasd health` reads `growth_log`.
        // An empty default would turn both into writes to "".
        let cfg = Config::from_toml(&minimal_toml("")).expect("parse minimal config");
        assert_eq!(cfg.general.growth_log, "/var/lib/das-backup/growth.log");
        assert_eq!(
            cfg.general.last_report,
            "/var/lib/das-backup/last-report.txt"
        );
    }

    #[test]
    fn boot_partial_section_fills_missing_keys_with_defaults() {
        // A `[boot]` header with only some keys goes through the per-field
        // serde defaults, not `Boot::default()`. Boot archival is on by default
        // and archives are pruned after 60 days (`backup.md` §Targets and
        // Retention); a retention of 0 or 1 would have the pruner delete
        // archives almost as soon as they are made.
        let cfg = Config::from_toml(&minimal_toml("[boot]\nsubvolumes = [\"@\"]"))
            .expect("parse partial [boot]");
        assert!(cfg.boot.enabled);
        assert_eq!(cfg.boot.archive_retention_days, 60);
        assert_eq!(cfg.boot.subvolumes, vec!["@"]);

        let cfg = Config::from_toml(&minimal_toml("[boot]\nenabled = false"))
            .expect("parse [boot] enabled=false");
        assert!(!cfg.boot.enabled);
        assert_eq!(cfg.boot.archive_retention_days, 60);
        assert_eq!(cfg.boot.subvolumes, vec!["@", "@home"]);
    }

    #[test]
    fn scrub_partial_section_stays_enabled() {
        // Tuning one scrub key must not switch the monthly scrub off.
        let cfg = Config::from_toml(&minimal_toml("[scrub]\nwarn_age_days = 30"))
            .expect("parse partial [scrub]");
        assert!(cfg.scrub.enabled);
        assert_eq!(cfg.scrub.warn_age_days, 30);
        assert_eq!(cfg.scrub.fail_age_days, 75);
    }

    fn target_with(serial_lines: &str) -> Target {
        let toml = format!(
            "label = \"primary\"\n{serial_lines}\nmount = \"/mnt/backup\"\nrole = \"primary\"\n[retention]\ndaily = 7\n"
        );
        toml::from_str(&toml).expect("target should parse")
    }

    #[test]
    fn target_serial_precedence_table() {
        // (toml lines, expected serials). `serials` wins when it names at least
        // one drive; an EMPTY `serials` must not mask a legacy `serial`, and an
        // empty `serial` must never become a one-element list holding "" —
        // that would satisfy `validate()`'s "has an anchor" check and send
        // consumers looking for a drive whose serial is the empty string.
        let cases: &[(&str, &[&str])] = &[
            ("serial = \"LEGACY\"", &["LEGACY"]),
            ("serials = [\"A\", \"B\"]", &["A", "B"]),
            ("serial = \"LEGACY\"\nserials = [\"A\", \"B\"]", &["A", "B"]),
            ("serial = \"LEGACY\"\nserials = []", &["LEGACY"]),
            ("serial = \"\"\nserials = [\"A\"]", &["A"]),
            ("serial = \"\"\nserials = []", &[]),
            ("serial = \"\"", &[]),
            ("serials = []", &[]),
            ("", &[]),
        ];
        for (lines, want) in cases {
            let t = target_with(lines);
            let want: Vec<String> = want.iter().map(|s| s.to_string()).collect();
            assert_eq!(t.serials, want, "serials for {lines:?}");
            assert_eq!(
                t.serial,
                want.first().cloned().unwrap_or_default(),
                "legacy serial for {lines:?}"
            );
            assert_eq!(t.effective_serials(), want, "effective for {lines:?}");
        }
    }

    #[test]
    fn target_with_only_empty_serials_fails_validation() {
        // End to end: a target whose only anchors are empty must be refused.
        let cfg = Config::from_toml(&minimal_toml(
            "[[target]]\nlabel = \"naked\"\nserial = \"\"\nserials = []\nmount = \"/mnt/backup\"\nrole = \"primary\"\n[target.retention]\ndaily = 7",
        ))
        .expect("parse target with empty anchors");
        assert!(
            cfg.validate()
                .iter()
                .any(|e| e.contains("naked") && e.contains("mount_uuid")),
            "empty serial + empty serials must not count as an anchor"
        );
    }

    #[test]
    fn parse_time_range_boundaries() {
        // Valid clock range is 00:00..=23:59, inclusive at both ends.
        assert_eq!(parse_time("00:00"), Ok((0, 0)));
        assert_eq!(parse_time("22:58"), Ok((22, 58)));
        assert_eq!(parse_time("23:59"), Ok((23, 59)));
        assert_eq!(parse_time("23:00"), Ok((23, 0)));
        assert_eq!(parse_time("00:59"), Ok((0, 59)));

        let hour_err = parse_time("24:00").expect_err("hour 24 is out of range");
        assert!(hour_err.contains("hour 24 out of range"), "{hour_err}");
        assert!(parse_time("25:00").is_err());

        let min_err = parse_time("00:60").expect_err("minute 60 is out of range");
        assert!(min_err.contains("minute 60 out of range"), "{min_err}");
        assert!(parse_time("00:61").is_err());
    }

    /// The GUI's config editor shows `Config::to_toml()` (the helper's
    /// `config_get`) and sends the edited text back whole (`config_set`,
    /// `Config::from_toml`). Every field sync writes must survive that round
    /// trip untouched, or a GUI save would put a retired entry back into
    /// btrbk.conf (bd DAS-Backup-Manager-h4t item 5).
    #[test]
    fn the_gui_round_trip_keeps_retired_adopted_snapshot_name_and_excludes() {
        let toml = r#"
[general]
version = "0.7.22"
install_prefix = "/usr"
db_path = "/tmp/test.db"
[init]
system = "systemd"
[schedule]
incremental = "03:00"
full = "Sun 04:00"
randomized_delay_min = 30
[subvolumes]
exclude = ["@cache", "srv/scratch*"]
[doctor]
exclude = ["@old"]
[[source]]
label = "nvme"
volume = "/.btrfs-nvme"
device = "/dev/nvme0n1p2"
subvolumes = [
  "@",
  { name = "@srv", snapshot_name = "srv-a", adopted = "2026-10-01" },
  { name = "@gone", snapshot_name = "gone", retired = "2026-10-02", manual_only = true },
]
[[target]]
label = "t"
serials = ["X"]
mount_uuid = "u-1"
mount = "/mnt/t"
role = "primary"
[target.retention]
weekly = 4
[email]
enabled = false
[gui]
enabled = false
"#;
        let before = Config::from_toml(toml).unwrap();
        let shown = before.to_toml().unwrap();
        let after = Config::from_toml(&shown).unwrap();
        type Entry = (String, bool, Option<String>, Option<String>, Option<String>);
        let entries = |c: &Config| -> Vec<Entry> {
            c.sources[0]
                .subvolumes
                .iter()
                .map(|e| {
                    (
                        e.name.clone(),
                        e.manual_only,
                        e.snapshot_name.clone(),
                        e.adopted.clone(),
                        e.retired.clone(),
                    )
                })
                .collect()
        };
        assert_eq!(entries(&after), entries(&before));
        assert_eq!(
            entries(&after)[2],
            (
                "@gone".to_string(),
                true,
                Some("gone".to_string()),
                None,
                Some("2026-10-02".to_string())
            )
        );
        assert_eq!(after.subvolumes.exclude, vec!["@cache", "srv/scratch*"]);
        assert_eq!(after.doctor.exclude, vec!["@old"]);
        assert_eq!(after.targets[0].mount_uuid.as_deref(), Some("u-1"));
        assert_eq!(after.targets[0].serials, vec!["X"]);
        assert_eq!(
            after.to_toml().unwrap(),
            shown,
            "a second round trip changes nothing"
        );
    }
}
